//! Files shown on the LCD: still images, animated GIF/WebP and videos.
//! Animations are pre-encoded once so playback costs almost nothing.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use image::codecs::gif::GifDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, Frames};
use tiny_skia::Pixmap;

use crate::{render, video};

/// MasterCTRL's capability list allows 15 fps; faster frames are merged.
const MIN_FRAME: Duration = Duration::from_millis(66);
/// Browsers treat GIF delays of 0-10 ms as "unspecified"; this is their usual substitute.
const DEFAULT_DELAY: Duration = Duration::from_millis(100);
/// Longer videos are cut to bound memory (about 0.6 MB per second at 15 fps).
const MAX_VIDEO: Duration = Duration::from_secs(60);

pub struct Frame {
    /// Rotated JPEG as sent to the device.
    pub wire: Vec<u8>,
    pub delay: Duration,
}

pub struct Animation {
    pub frames: Vec<Frame>,
    /// Unrotated first frame, for the preview.
    pub poster: Pixmap,
    /// The source was longer than what was kept.
    pub truncated: bool,
}

pub enum Media {
    Still(Pixmap),
    Animated(Animation),
}

/// Loads any supported file. Slow for videos; call off the streaming thread.
pub fn load(path: &Path, extra_rotation: f32) -> Result<Media, String> {
    load_with_progress(path, extra_rotation, |_| {})
}

/// Like `load`, reporting video decoding progress as 0.0..=1.0.
pub fn load_with_progress(path: &Path, extra_rotation: f32, progress: impl FnMut(f32)) -> Result<Media, String> {
    if video::is_video(path) {
        let mut builder = Builder::new(extra_rotation);
        let truncated = video::decode(path, MIN_FRAME, MAX_VIDEO, |img, delay| builder.push(img, delay), progress)?;
        return builder
            .finish(truncated)
            .map(Media::Animated)
            .ok_or_else(|| format!("{}: no video frames", path.display()));
    }
    if let Some(anim) = load_animation(path, extra_rotation)? {
        return Ok(Media::Animated(anim));
    }
    render::load_image(path).map(Media::Still).map_err(|e| format!("{}: {e}", path.display()))
}

/// Accumulates frames, merging runs of very short ones. Scaling and JPEG encoding
/// run on a pool of threads; that dominates load time for videos.
struct Builder {
    delays: Vec<Duration>,
    jobs: Option<mpsc::SyncSender<(usize, DynamicImage)>>,
    results: mpsc::Receiver<(usize, Vec<u8>, Option<Pixmap>)>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl Builder {
    fn new(extra_rotation: f32) -> Builder {
        let threads = thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
        // Bounded so a fast decoder cannot queue up hundreds of full-size frames.
        let (jobs, job_rx) = mpsc::sync_channel::<(usize, DynamicImage)>(threads);
        let job_rx = Arc::new(Mutex::new(job_rx));
        let (result_tx, results) = mpsc::channel();
        let workers = (0..threads)
            .map(|_| {
                let job_rx = job_rx.clone();
                let result_tx = result_tx.clone();
                thread::spawn(move || {
                    loop {
                        let job = job_rx.lock().expect("job queue").recv();
                        let Ok((index, img)) = job else { break };
                        // Triangle is much faster than Lanczos and fine for motion.
                        let canvas = render::fit_to_canvas(img, FilterType::Triangle);
                        let wire = render::to_jpeg(&render::rotate(&canvas, extra_rotation));
                        let poster = (index == 0).then_some(canvas);
                        if result_tx.send((index, wire, poster)).is_err() {
                            break;
                        }
                    }
                })
            })
            .collect();
        Builder { delays: Vec::new(), jobs: Some(jobs), results, workers }
    }

    fn push(&mut self, img: DynamicImage, delay: Duration) {
        // Runs of very short frames would exceed the device's frame rate: drop the
        // later ones but keep their time, so the loop length stays the same.
        if delay < MIN_FRAME
            && let Some(last) = self.delays.last_mut().filter(|d| **d < MIN_FRAME)
        {
            *last += delay;
            return;
        }
        let index = self.delays.len();
        self.delays.push(delay);
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send((index, img));
        }
    }

    /// `None` unless there are at least two frames.
    fn finish(mut self, truncated: bool) -> Option<Animation> {
        drop(self.jobs.take());
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        let mut wires: Vec<Option<Vec<u8>>> = vec![None; self.delays.len()];
        let mut poster = None;
        for (index, wire, p) in self.results.try_iter() {
            wires[index] = Some(wire);
            poster = poster.or(p);
        }
        let frames: Vec<Frame> = wires
            .into_iter()
            .zip(&self.delays)
            .map(|(wire, &delay)| Frame { wire: wire.expect("every frame encoded"), delay })
            .collect();
        match (frames.len(), poster) {
            (n, Some(poster)) if n > 1 => Some(Animation { frames, poster, truncated }),
            _ => None,
        }
    }
}

/// Decodes an animated GIF/WebP. Returns `Ok(None)` for files that are not animated.
pub fn load_animation(path: &Path, extra_rotation: f32) -> Result<Option<Animation>, String> {
    let err = |e: image::ImageError| format!("{}: {e}", path.display());
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let open = || File::open(path).map(BufReader::new).map_err(|e| format!("{}: {e}", path.display()));
    let frames: Frames = match ext.as_str() {
        "gif" => GifDecoder::new(open()?).map_err(err)?.into_frames(),
        "webp" => {
            let decoder = WebPDecoder::new(open()?).map_err(err)?;
            if !decoder.has_animation() {
                return Ok(None);
            }
            decoder.into_frames()
        }
        _ => return Ok(None),
    };

    let mut builder = Builder::new(extra_rotation);
    for frame in frames {
        let frame = frame.map_err(err)?;
        let (num, den) = frame.delay().numer_denom_ms();
        let delay = match Duration::from_micros(num as u64 * 1000 / den.max(1) as u64) {
            d if d <= Duration::from_millis(10) => DEFAULT_DELAY,
            d => d,
        };
        builder.push(DynamicImage::ImageRgba8(frame.into_buffer()), delay);
    }
    Ok(builder.finish(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Rgba, RgbaImage};

    fn write_gif(path: &Path, delays_ms: &[u32]) {
        let mut enc = GifEncoder::new(File::create(path).unwrap());
        for (i, &ms) in delays_ms.iter().enumerate() {
            let img = RgbaImage::from_pixel(64, 64, Rgba([(i * 60) as u8, 0, 0, 255]));
            let frame = image::Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(ms, 1));
            enc.encode_frame(frame).unwrap();
        }
    }

    #[test]
    fn gif_frames_are_paced_and_merged() {
        let path = std::env::temp_dir().join("atmos-lcd-test.gif");
        // Two normal frames, two 20 ms frames that must merge, one "unspecified" delay.
        write_gif(&path, &[100, 100, 20, 20, 0]);
        let anim = load_animation(&path, 0.0).unwrap().expect("animated");
        let delays: Vec<u128> = anim.frames.iter().map(|f| f.delay.as_millis()).collect();
        assert_eq!(delays, [100, 100, 40, 100]);
        assert_eq!(&anim.frames[0].wire[..2], &[0xFF, 0xD8]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn single_frame_gif_is_not_an_animation() {
        let path = std::env::temp_dir().join("atmos-lcd-still.gif");
        write_gif(&path, &[100]);
        assert!(load_animation(&path, 0.0).unwrap().is_none());
        let _ = std::fs::remove_file(path);
    }
}
