//! Animated images (GIF, animated WebP), pre-encoded for streaming.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::time::Duration;

use image::codecs::gif::GifDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, Frames};
use tiny_skia::Pixmap;

use crate::render;

/// MasterCTRL's capability list allows 15 fps; faster frames are merged.
const MIN_FRAME: Duration = Duration::from_millis(66);
/// Browsers treat GIF delays of 0-10 ms as "unspecified"; this is their usual substitute.
const DEFAULT_DELAY: Duration = Duration::from_millis(100);

pub struct Frame {
    /// Rotated JPEG as sent to the device.
    pub wire: Vec<u8>,
    pub delay: Duration,
}

pub struct Animation {
    pub frames: Vec<Frame>,
    /// Unrotated first frame, for the preview.
    pub poster: Pixmap,
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

    let mut out: Vec<Frame> = Vec::new();
    let mut poster = None;
    for frame in frames {
        let frame = frame.map_err(err)?;
        let (num, den) = frame.delay().numer_denom_ms();
        let delay = match Duration::from_micros(num as u64 * 1000 / den.max(1) as u64) {
            d if d <= Duration::from_millis(10) => DEFAULT_DELAY,
            d => d,
        };
        // Runs of very short frames would exceed the device's frame rate: drop the
        // later ones but keep their time, so the loop length stays the same.
        if delay < MIN_FRAME
            && let Some(last) = out.last_mut().filter(|l| l.delay < MIN_FRAME)
        {
            last.delay += delay;
            continue;
        }
        // Triangle is much faster than Lanczos and fine for motion.
        let canvas = render::fit_to_canvas(DynamicImage::ImageRgba8(frame.into_buffer()), FilterType::Triangle);
        let wire = render::to_jpeg(&render::rotate(&canvas, extra_rotation));
        poster.get_or_insert(canvas);
        out.push(Frame { wire, delay });
    }
    match (out.len(), poster) {
        (n, Some(poster)) if n > 1 => Ok(Some(Animation { frames: out, poster })),
        _ => Ok(None),
    }
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
