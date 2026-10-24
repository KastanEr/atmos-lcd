//! Background thread that owns the device: renders the selected screen,
//! streams it, keeps realtime mode alive and reconnects when the cooler
//! disappears (USB reset, sleep/resume).

use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tiny_skia::Pixmap;

use crate::config::Config;
use crate::device::{Atmos, KEEPALIVE_INTERVAL};
use crate::render::{self, TextRenderer};
use crate::scenes;
use crate::stats::Stats;

const TICK: Duration = Duration::from_millis(500);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connected,
    NotFound,
    Error(String),
}

/// Renders screens; keeps fonts, CPU counters and the decoded image between frames.
pub struct Renderer {
    text: TextRenderer,
    stats: Stats,
    image: Option<(PathBuf, Pixmap)>,
}

impl Renderer {
    pub fn new() -> Renderer {
        Renderer { text: TextRenderer::new(), stats: Stats::new(), image: None }
    }

    /// The screen selected in `config`, before rotation.
    pub fn draw(&mut self, config: &Config) -> Result<Pixmap, String> {
        match config.screen.as_str() {
            "clock" => Ok(scenes::clock(&mut self.text)),
            "image" => {
                let path = config.image.as_ref().ok_or("no image selected")?;
                if self.image.as_ref().is_none_or(|(p, _)| p != path) {
                    let pixmap = render::load_image(path).map_err(|e| format!("{}: {e}", path.display()))?;
                    self.image = Some((path.clone(), pixmap));
                }
                Ok(self.image.as_ref().expect("just loaded").1.clone())
            }
            _ => Ok(scenes::system(&mut self.text, &self.stats.sample())),
        }
    }

    /// Whether the screen changes over time and must be redrawn every second.
    pub fn is_live(config: &Config) -> bool {
        config.screen != "image"
    }
}

pub struct Worker {
    tx: Sender<Config>,
    thread: Option<JoinHandle<()>>,
}

/// Callbacks run on the worker thread.
pub struct Hooks {
    /// Each rendered frame (unrotated), for the settings preview.
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
            let mut next_connect = Instant::now();
            let mut next_keepalive = Instant::now();
            let mut next_draw = Instant::now();
            let mut frame: Option<Vec<u8>> = None;
            let mut draw_error: Option<String> = None;
            // Settings that must be pushed to the device (on change or reconnect).
            let mut dirty = true;

            loop {
                match rx.recv_timeout(TICK) {
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
                        Ok(canvas) => {
                            (hooks.frame)(&canvas);
                            let rotated = render::rotate(&canvas, config.split_rotation().1);
                            frame = Some(render::to_jpeg(&rotated));
                            draw_error = None;
                        }
                        Err(e) => {
                            set_status(Status::Error(e.clone()));
                            draw_error = Some(e);
                        }
                    }
                    next_draw = if Renderer::is_live(&config) { now + Duration::from_secs(1) } else { now + Duration::from_secs(3600) };
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
                let Some(d) = dev.as_mut() else { continue };

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
                    // Resending the last frame each tick also restores the picture
                    // after the LCD resets.
                    if let Some(f) = &frame {
                        d.send_frame(f)?;
                    }
                    Ok(())
                })();
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
