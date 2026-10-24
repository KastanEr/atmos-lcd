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
use crate::media::{self, Animation};
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
    NotFound,
    Error(String),
}

/// What the selected screen looks like right now.
pub enum Scene {
    Still(Pixmap),
    Animated(Arc<Animation>),
}

/// Renders screens; keeps fonts, CPU counters and the decoded image between frames.
pub struct Renderer {
    text: TextRenderer,
    stats: Stats,
    /// Decoded image file, keyed by path and the rotation baked into animations.
    image: Option<(PathBuf, u16, Arc<Animation>)>,
}

impl Renderer {
    pub fn new() -> Renderer {
        Renderer { text: TextRenderer::new(), stats: Stats::new(), image: None }
    }

    /// The screen selected in `config`. Stills are unrotated; animations are pre-rotated.
    pub fn draw(&mut self, config: &Config) -> Result<Scene, String> {
        match config.screen.as_str() {
            "clock" => Ok(Scene::Still(scenes::clock(&mut self.text))),
            "image" => {
                let path = config.image.as_ref().ok_or("no image selected")?;
                if let Some((p, r, anim)) = &self.image
                    && p == path
                    && *r == config.rotation
                {
                    return Ok(Scene::Animated(anim.clone()));
                }
                match media::load_animation(path, config.split_rotation().1)? {
                    Some(anim) => {
                        let anim = Arc::new(anim);
                        self.image = Some((path.clone(), config.rotation, anim.clone()));
                        Ok(Scene::Animated(anim))
                    }
                    None => {
                        self.image = None;
                        render::load_image(path).map(Scene::Still).map_err(|e| format!("{}: {e}", path.display()))
                    }
                }
            }
            _ => Ok(Scene::Still(scenes::system(&mut self.text, &self.stats.sample()))),
        }
    }

    /// Whether the screen changes over time and must be redrawn every second.
    pub fn is_live(config: &Config) -> bool {
        config.screen != "image"
    }
}

/// The frames currently being streamed.
enum Playing {
    Nothing,
    Still(Vec<u8>),
    Animation { anim: Arc<Animation>, index: usize },
}

impl Playing {
    fn current(&self) -> Option<&[u8]> {
        match self {
            Playing::Nothing => None,
            Playing::Still(f) => Some(f),
            Playing::Animation { anim, index } => Some(&anim.frames[*index].wire),
        }
    }

    /// When the current frame should be sent again or replaced.
    fn interval(&self) -> Duration {
        match self {
            Playing::Animation { anim, index } => anim.frames[*index].delay,
            _ => RESEND_INTERVAL,
        }
    }

    fn advance(&mut self) {
        if let Playing::Animation { anim, index } = self {
            *index = (*index + 1) % anim.frames.len();
        }
    }
}

pub struct Worker {
    tx: Sender<Config>,
    thread: Option<JoinHandle<()>>,
}

/// Callbacks run on the worker thread.
pub struct Hooks {
    /// Each newly drawn still (unrotated), or an animation's first frame, for the preview.
    pub frame: Box<dyn Fn(&Pixmap) + Send>,
    pub status: Box<dyn Fn(Status) + Send>,
}

impl Worker {
    pub fn spawn(config: Config, hooks: Hooks) -> Worker {
        let (tx, rx) = mpsc::channel::<Config>();
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
            let mut draw_error: Option<String> = None;
            // Settings that must be pushed to the device (on change or reconnect).
            let mut dirty = true;

            loop {
                let mut wake = next_draw.min(next_send);
                wake = if dev.is_some() { wake.min(next_keepalive) } else { wake.min(next_connect) };
                match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
                    Ok(new) => {
                        // Coalesce bursts (e.g. dragging the brightness slider).
                        config = rx.try_iter().last().unwrap_or(new);
                        dirty = true;
                        next_draw = Instant::now();
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                let now = Instant::now();

                if now >= next_draw {
                    match renderer.draw(&config) {
                        Ok(Scene::Still(canvas)) => {
                            (hooks.frame)(&canvas);
                            playing = Playing::Still(render::to_jpeg(&render::rotate(&canvas, config.split_rotation().1)));
                            draw_error = None;
                        }
                        Ok(Scene::Animated(anim)) => {
                            let same = matches!(&playing, Playing::Animation { anim: a, .. } if Arc::ptr_eq(a, &anim));
                            if !same {
                                (hooks.frame)(&anim.poster);
                                playing = Playing::Animation { anim, index: 0 };
                            }
                            draw_error = None;
                        }
                        Err(e) => {
                            set_status(Status::Error(e.clone()));
                            draw_error = Some(e);
                        }
                    }
                    next_draw = if Renderer::is_live(&config) { now + LIVE_REDRAW } else { now + Duration::from_secs(3600) };
                    next_send = now;
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
                    Ok(()) => set_status(draw_error.clone().map_or(Status::Connected, Status::Error)),
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
        let _ = self.tx.send(config);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Closing the channel ends the loop; the device then falls back to its stored media.
        let (tx, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.tx, tx));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
