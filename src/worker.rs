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
use crate::media::{self, Animation, Media};
use crate::render::{self, TextRenderer};
use crate::scenes;
use crate::stats::Stats;

/// How often a still frame is resent; also restores the picture after the LCD resets.
const RESEND_INTERVAL: Duration = Duration::from_millis(500);
const LIVE_REDRAW: Duration = Duration::from_secs(1);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Streaming; optionally with a note about the content (e.g. a cut video).
    Connected(Option<String>),
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
    Animation { media: Arc<Media>, index: usize },
}

impl Playing {
    fn animation(&self) -> Option<(&Animation, usize)> {
        match self {
            Playing::Animation { media, index } => match media.as_ref() {
                Media::Animated(a) => Some((a, *index)),
                Media::Still(_) => None,
            },
            _ => None,
        }
    }

    fn current(&self) -> Option<&[u8]> {
        match self {
            Playing::Still(f) => Some(f),
            _ => self.animation().map(|(a, i)| a.frames[i].wire.as_slice()),
        }
    }

    /// When the current frame should be sent again or replaced.
    fn interval(&self) -> Duration {
        self.animation().map_or(RESEND_INTERVAL, |(a, i)| a.frames[i].delay)
    }

    fn advance(&mut self) {
        let len = self.animation().map(|(a, _)| a.frames.len());
        if let (Playing::Animation { index, .. }, Some(len)) = (self, len) {
            *index = (*index + 1) % len;
        }
    }
}

/// A loaded file depends on its path and on the rotation baked into animation frames.
type MediaKey = (PathBuf, u16);

enum Msg {
    Config(Config),
    Loaded(MediaKey, Result<Media, String>),
    Quit,
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
            let mut media: Option<(MediaKey, Arc<Media>)> = None;
            let mut loading: Option<MediaKey> = None;
            // Problem or note about what is shown, reported instead of a plain "connected".
            let mut content_error: Option<String> = None;
            let mut content_note: Option<String> = None;
            // Settings that must be pushed to the device (on change or reconnect).
            let mut dirty = true;

            loop {
                let mut wake = next_draw.min(next_send);
                wake = if dev.is_some() { wake.min(next_keepalive) } else { wake.min(next_connect) };
                match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
                    Ok(Msg::Config(new)) => {
                        config = new;
                        dirty = true;
                        next_draw = Instant::now();
                    }
                    Ok(Msg::Loaded(key, result)) => {
                        // Ignore results for a file that is no longer selected.
                        if loading.as_ref() == Some(&key) {
                            loading = None;
                            match result {
                                Ok(m) => media = Some((key, Arc::new(m))),
                                Err(e) => content_error = Some(e),
                            }
                            next_draw = Instant::now();
                        }
                    }
                    Ok(Msg::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                let now = Instant::now();

                if now >= next_draw {
                    // Files don't change by themselves; they wait for the next config or load result.
                    next_draw = now + Duration::from_secs(3600);
                    if config.screen != "image" {
                        let canvas = renderer.draw(&config);
                        (hooks.frame)(&canvas);
                        playing = Playing::Still(render::to_jpeg(&render::rotate(&canvas, config.split_rotation().1)));
                        (content_error, content_note) = (None, None);
                        next_draw = now + LIVE_REDRAW;
                        next_send = now;
                    } else if let Some(path) = &config.image {
                        let key = (path.clone(), config.rotation);
                        match &media {
                            Some((k, m)) if *k == key => {
                                (content_error, content_note) = (None, None);
                                match m.as_ref() {
                                    Media::Still(canvas) => {
                                        (hooks.frame)(canvas);
                                        let rotated = render::rotate(canvas, config.split_rotation().1);
                                        playing = Playing::Still(render::to_jpeg(&rotated));
                                    }
                                    Media::Animated(a) => {
                                        if a.truncated {
                                            content_note = Some("영상이 길어 앞 1분만 재생".into());
                                        }
                                        let same = matches!(&playing, Playing::Animation { media, .. } if Arc::ptr_eq(media, m));
                                        if !same {
                                            (hooks.frame)(&a.poster);
                                            playing = Playing::Animation { media: m.clone(), index: 0 };
                                        }
                                    }
                                }
                                next_send = now;
                            }
                            _ if loading.as_ref() != Some(&key) => {
                                // Decoding can take seconds; keep streaming the old content meanwhile.
                                loading = Some(key.clone());
                                content_error = None;
                                let tx = loader_tx.clone();
                                let rotation = config.split_rotation().1;
                                thread::spawn(move || {
                                    let result = media::load(&key.0, rotation);
                                    let _ = tx.send(Msg::Loaded(key, result));
                                });
                            }
                            _ => {}
                        }
                    } else {
                        content_error = Some("사진이나 동영상을 선택하세요".into());
                    }
                }

                if dev.is_none() && now >= next_connect {
                    next_connect = now + RECONNECT_INTERVAL;
                    match Atmos::open() {
                        Ok(d) => {
                            dev = Some(d);
                            dirty = true;
                            next_keepalive = now;
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
                    if dirty {
                        d.set_brightness(config.brightness)?;
                        d.set_rotation(config.split_rotation().0)?;
                        dirty = false;
                    }
                    if now >= next_keepalive {
                        d.keep_realtime_alive()?;
                        next_keepalive = now + KEEPALIVE_INTERVAL;
                    }
                    if send_due && let Some(f) = playing.current() {
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
                        (None, None) => Status::Connected(content_note.clone()),
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
