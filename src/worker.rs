//! Background thread that owns the device: renders the selected screen,
//! streams it, keeps realtime mode alive and reconnects when the cooler
//! disappears (USB reset, sleep/resume).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tiny_skia::Pixmap;

use crate::config::Config;
use crate::device::{Atmos, KEEPALIVE_INTERVAL};
use crate::gallery::{self, FrameFile};
use crate::render::{self, TextRenderer};
use crate::scenes;
use crate::stats::Stats;

/// How often a still frame is resent; also restores the picture after the LCD resets.
const RESEND_INTERVAL: Duration = Duration::from_millis(500);
const LIVE_REDRAW: Duration = Duration::from_secs(1);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connected,
    Loading,
    NotFound,
    Error(String),
}

/// Draws the generated screens; keeps fonts and CPU counters between frames.
pub struct Renderer {
    text: TextRenderer,
    stats: Stats,
}

impl Renderer {
    pub fn new() -> Renderer {
        Renderer { text: TextRenderer::new(), stats: Stats::new() }
    }

    /// The clock or system screen, unrotated.
    pub fn draw(&mut self, config: &Config) -> Pixmap {
        match config.screen.as_str() {
            "clock" => scenes::clock(&mut self.text),
            _ => scenes::system(&mut self.text, &self.stats.sample()),
        }
    }
}

/// The frames currently being streamed.
enum Playing {
    Nothing,
    Still(Vec<u8>),
    /// A gallery animation, read from disk one frame at a time.
    Frames { file: Arc<FrameFile>, index: usize, buf: Vec<u8> },
}

impl Playing {
    fn current(&mut self) -> std::io::Result<Option<&[u8]>> {
        match self {
            Playing::Nothing => Ok(None),
            Playing::Still(f) => Ok(Some(f)),
            Playing::Frames { file, index, buf } => {
                file.read_into(*index, buf)?;
                Ok(Some(buf))
            }
        }
    }

    /// When the current frame should be sent again or replaced.
    fn interval(&self) -> Duration {
        match self {
            Playing::Frames { file, index, .. } => file.delay(*index),
            _ => RESEND_INTERVAL,
        }
    }

    fn advance(&mut self) {
        if let Playing::Frames { file, index, .. } = self {
            *index = (*index + 1) % file.len();
        }
    }

    fn is_file(&self, other: &Arc<FrameFile>) -> bool {
        matches!(self, Playing::Frames { file, .. } if Arc::ptr_eq(file, other))
    }
}

/// A gallery item as streamed: its id and the software rotation baked into the frames.
type ItemKey = (String, u16);

enum Msg {
    Config(Config),
    Loaded(ItemKey, Result<FrameFile, String>),
    Storage(StorageOp),
    Quit,
}

/// Changes to the device's stored media (played when no host streams).
pub enum StorageOp {
    /// Replace slot `n` with a JPEG or MP4 file.
    Store(usize, PathBuf),
    Clear(usize),
    /// Re-read which slots are filled.
    Query,
}

pub enum StorageEvent {
    /// Which slots hold a file.
    Slots(Vec<bool>),
    /// Share of an upload into the slot that has been sent.
    Uploading(usize, f32),
    Stored(usize),
    Failed(usize, String),
}

/// Runs one storage operation; returns the slot it touched.
fn run_storage(d: &mut Atmos, op: &StorageOp, report: &dyn Fn(StorageEvent)) -> std::io::Result<Option<usize>> {
    let clear = |d: &mut Atmos, slot: usize| -> std::io::Result<()> {
        // A slot holds either kind; remove both like MasterCTRL does.
        for ext in ["mp4", "jpg"] {
            d.delete_media(&format!("suspend_{slot}.{ext}"), "suspend")?;
            std::thread::sleep(Duration::from_millis(60));
        }
        Ok(())
    };
    match op {
        StorageOp::Store(slot, file) => {
            let data = std::fs::read(file)?;
            let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("jpg").to_ascii_lowercase();
            clear(d, *slot)?;
            let mut last = -1.0f32;
            d.upload(&format!("suspend_{slot}.{ext}"), "suspend", &data, &mut |p| {
                // Whole percents are enough for the UI.
                if p - last >= 0.01 || p >= 1.0 {
                    last = p;
                    report(StorageEvent::Uploading(*slot, p));
                }
            })?;
            Ok(Some(*slot))
        }
        StorageOp::Clear(slot) => clear(d, *slot).map(|_| Some(*slot)),
        StorageOp::Query => Ok(None),
    }
}

/// First frame of the unrotated item, for the preview.
fn item_poster(id: &str) -> Option<Pixmap> {
    let jpeg = FrameFile::open(&gallery::frames_path(id, 0)).and_then(|f| f.read(0)).ok()?;
    gallery::decode_frame(&jpeg).ok()
}

/// Opens the item's frames for `key`, generating the rotated copy first if needed (slow).
fn open_item(key: &ItemKey) -> Result<FrameFile, String> {
    let (id, rotation) = key;
    let path = gallery::frames_path(id, *rotation);
    if !path.exists() && *rotation != 0 {
        gallery::make_rotated(id, *rotation)?;
    }
    FrameFile::open(&path).map_err(|e| format!("갤러리 항목을 열 수 없음: {e}"))
}

pub struct Worker {
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

/// Callbacks run on the worker thread.
pub struct Hooks {
    /// Each newly shown still (unrotated), or an animation's first frame, for the preview.
    pub frame: Box<dyn Fn(&Pixmap) + Send>,
    pub status: Box<dyn Fn(Status) + Send>,
    pub storage: Box<dyn Fn(StorageEvent) + Send>,
}

impl Worker {
    pub fn spawn(config: Config, hooks: Hooks) -> Worker {
        let (tx, rx) = mpsc::channel::<Msg>();
        let loader_tx = tx.clone();
        let thread = thread::spawn(move || {
            let mut config = config;
            let mut renderer = Renderer::new();
            let mut dev: Option<Atmos> = None;
            let mut status = None;
            let mut set_status = |s: Status| {
                if status.as_ref() != Some(&s) {
                    (hooks.status)(s.clone());
                    status = Some(s);
                }
            };
            let now = Instant::now();
            let (mut next_connect, mut next_keepalive, mut next_draw, mut next_send) = (now, now, now, now);
            let mut playing = Playing::Nothing;
            let mut item: Option<(ItemKey, Arc<FrameFile>)> = None;
            let mut loading: Option<ItemKey> = None;
            // Problem with what is shown, reported instead of a plain "connected".
            let mut content_error: Option<String> = None;
            // Settings that must be pushed to the device (on change or reconnect).
            let mut dirty = true;
            let mut storage_ops = std::collections::VecDeque::from([StorageOp::Query]);

            loop {
                let mut wake = next_draw.min(next_send);
                wake = if dev.is_some() { wake.min(next_keepalive) } else { wake.min(next_connect) };
                match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
                    Ok(Msg::Config(new)) => {
                        if new.offline != config.offline {
                            // Hand the screen to the stored media (or take it back) right away.
                            next_keepalive = Instant::now();
                            next_send = Instant::now();
                        }
                        config = new;
                        dirty = true;
                        next_draw = Instant::now();
                    }
                    Ok(Msg::Loaded(key, result)) => {
                        // Ignore results for a file that is no longer selected.
                        if loading.as_ref() == Some(&key) {
                            loading = None;
                            match result {
                                Ok(f) => item = Some((key, Arc::new(f))),
                                Err(e) => content_error = Some(e),
                            }
                            next_draw = Instant::now();
                        }
                    }
                    Ok(Msg::Storage(op)) => {
                        storage_ops.push_back(op);
                        next_send = Instant::now();
                    }
                    Ok(Msg::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                let now = Instant::now();

                if now >= next_draw {
                    // Files don't change by themselves; they wait for the next config or load result.
                    next_draw = now + Duration::from_secs(3600);
                    // Offline only previews gallery items; nothing is streamed.
                    if !config.offline && config.screen != "gallery" {
                        let canvas = renderer.draw(&config);
                        (hooks.frame)(&canvas);
                        playing = Playing::Still(render::to_jpeg(&render::rotate(&canvas, config.split_rotation().1)));
                        content_error = None;
                        next_draw = now + LIVE_REDRAW;
                        next_send = now;
                    } else if let Some(id) = &config.item {
                        let key = (id.clone(), config.split_rotation().1 as u16);
                        match &item {
                            Some((k, file)) if *k == key => {
                                content_error = None;
                                if !playing.is_file(file) {
                                    if let Some(poster) = item_poster(id) {
                                        (hooks.frame)(&poster);
                                    }
                                    playing = if file.len() == 1 {
                                        match file.read(0) {
                                            Ok(jpeg) => Playing::Still(jpeg),
                                            Err(e) => {
                                                content_error = Some(e.to_string());
                                                Playing::Nothing
                                            }
                                        }
                                    } else {
                                        Playing::Frames { file: file.clone(), index: 0, buf: Vec::new() }
                                    };
                                }
                                next_send = now;
                            }
                            _ if loading.as_ref() != Some(&key) => {
                                // Rotated copies take a while to make; keep streaming meanwhile.
                                loading = Some(key.clone());
                                content_error = None;
                                let tx = loader_tx.clone();
                                thread::spawn(move || {
                                    let result = open_item(&key);
                                    let _ = tx.send(Msg::Loaded(key, result));
                                });
                            }
                            _ => {}
                        }
                    } else {
                        content_error = Some("갤러리에서 항목을 선택하세요".into());
                    }
                }

                if dev.is_none() && now >= next_connect {
                    next_connect = now + RECONNECT_INTERVAL;
                    match Atmos::open() {
                        Ok(d) => {
                            dev = Some(d);
                            dirty = true;
                            next_keepalive = now;
                            storage_ops.push_back(StorageOp::Query);
                        }
                        Err(_) => set_status(Status::NotFound),
                    }
                }

                let send_due = now >= next_send;
                if send_due {
                    // Animations keep their timing even while the cooler is unplugged.
                    next_send = now + playing.interval();
                }
                let Some(d) = dev.as_mut() else {
                    if send_due {
                        playing.advance();
                    }
                    continue;
                };

                let result = (|| -> std::io::Result<()> {
                    if !storage_ops.is_empty() {
                        while let Some(op) = storage_ops.pop_front() {
                            match run_storage(d, &op, &*hooks.storage) {
                                Ok(Some(slot)) if matches!(op, StorageOp::Store(..)) => {
                                    (hooks.storage)(StorageEvent::Stored(slot))
                                }
                                Ok(_) => {}
                                Err(e) if e.kind() == std::io::ErrorKind::Other => {
                                    // Refused by the device; the connection itself is fine.
                                    if let StorageOp::Store(slot, _) | StorageOp::Clear(slot) = op {
                                        (hooks.storage)(StorageEvent::Failed(slot, e.to_string()));
                                    }
                                }
                                Err(e) => return Err(e),
                            }
                        }
                        (hooks.storage)(StorageEvent::Slots(d.suspend_slots()?));
                        // Uploads take a while; make sure realtime mode didn't lapse.
                        next_keepalive = Instant::now();
                    }
                    if dirty {
                        d.set_brightness(config.brightness)?;
                        d.set_rotation(config.split_rotation().0)?;
                        if config.offline {
                            d.set_realtime(false)?;
                        }
                        dirty = false;
                    }
                    // Offline: the cooler plays its stored media; only settings are sent.
                    if config.offline {
                        return Ok(());
                    }
                    if now >= next_keepalive {
                        d.keep_realtime_alive()?;
                        next_keepalive = now + KEEPALIVE_INTERVAL;
                    }
                    if send_due && let Some(f) = playing.current()? {
                        d.send_frame(f)?;
                    }
                    Ok(())
                })();
                if send_due {
                    playing.advance();
                }
                match result {
                    Ok(()) => set_status(match (&content_error, &loading) {
                        (Some(e), _) => Status::Error(e.clone()),
                        (None, Some(_)) => Status::Loading,
                        (None, None) => Status::Connected,
                    }),
                    Err(e) => {
                        set_status(Status::Error(e.to_string()));
                        dev = None;
                    }
                }
            }
        });
        Worker { tx, thread: Some(thread) }
    }

    pub fn apply(&self, config: Config) {
        let _ = self.tx.send(Msg::Config(config));
    }

    pub fn storage(&self, op: StorageOp) {
        let _ = self.tx.send(Msg::Storage(op));
    }

}

impl Drop for Worker {
    fn drop(&mut self) {
        // The worker holds a sender for its loaders, so the channel never closes on its own.
        // After it stops, the device falls back to its stored media.
        let _ = self.tx.send(Msg::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
