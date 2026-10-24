// No console window when started by double-click or at login; CLI use attaches to the caller's console.
#![windows_subsystem = "windows"]

mod app;
mod autostart;
mod config;
mod device;
mod gallery;
mod hid;
mod media;
mod render;
mod scenes;
mod stats;
mod video;
mod worker;

use std::process::ExitCode;

use config::Config;
use device::Atmos;
use media::Media;
use worker::{Hooks, Renderer, Worker};

const USAGE: &str = "\
usage: atmos-lcd [command]
  (no command)                  start the tray app
  param                         print current device parameters
  brightness <0-100>
  rotate <degrees>              e.g. 60 for a hexagonal pump; saved to config
  run <screen>                  show a screen in the foreground until Ctrl+C
  preview <screen> <out.jpg>    render a screen to a file without the device
  gallery                       list gallery items
  import <file>                 add a picture, GIF/WebP or video to the gallery
screens:
  clock | system | gallery <id> | file <path>   (file: preview only)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = if args.is_empty() {
        if !single_instance() {
            return ExitCode::SUCCESS;
        }
        app::run()
    } else {
        attach_console();
        cli(&args)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Applies `<screen> [arg]` to the saved config and returns the remaining args.
fn with_screen<'a>(args: &'a [String], mut config: Config) -> Result<(Config, &'a [String]), &'static str> {
    match args.first().map(String::as_str) {
        Some(s @ ("clock" | "system")) => {
            config.screen = s.into();
            Ok((config, &args[1..]))
        }
        Some("gallery") => {
            config.screen = "gallery".into();
            config.item = Some(args.get(1).ok_or(USAGE)?.clone());
            Ok((config, &args[2..]))
        }
        _ => Err(USAGE),
    }
}

/// Child-process protocol used by `gallery::run_child`.
fn child_result(result: Result<String, String>) -> Result<(), Box<dyn std::error::Error>> {
    match result {
        Ok(v) => println!("done {v}"),
        Err(e) => println!("error {}", e.replace('\n', " ")),
    }
    Ok(())
}

fn cli(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let arg = |i: usize| args.get(i).map(String::as_str).ok_or(USAGE);
    match arg(0)? {
        "param" => println!("{}", Atmos::open()?.params()?),
        "brightness" => println!("{}", Atmos::open()?.set_brightness(arg(1)?.parse()?)?),
        "rotate" => {
            let mut config = Config::load();
            config.rotation = arg(1)?.parse::<u16>()? % 360;
            config.save()?;
            Atmos::open()?.set_rotation(config.split_rotation().0)?;
            println!("rotation {} saved", config.rotation);
        }
        "run" => {
            let (config, _) = with_screen(&args[1..], Config::load())?;
            let hooks = Hooks { frame: Box::new(|_| {}), status: Box::new(|s| eprintln!("{s:?}")) };
            let _worker = Worker::spawn(config, hooks);
            loop {
                std::thread::park();
            }
        }
        "preview" => {
            let rotation = Config::load().split_rotation().1;
            let (jpeg, out) = if args.get(1).map(String::as_str) == Some("file") {
                let path = std::path::Path::new(arg(2)?);
                let jpeg = match media::load(path, rotation)? {
                    Media::Still(canvas) => render::to_jpeg(&render::rotate(&canvas, rotation)),
                    Media::Animated(anim) => {
                        let total: std::time::Duration = anim.frames.iter().map(|f| f.delay).sum();
                        let cut = if anim.truncated { " (truncated)" } else { "" };
                        println!("animation: {} frames, {:.1} s per loop{cut}", anim.frames.len(), total.as_secs_f32());
                        anim.frames[0].wire.clone()
                    }
                };
                (jpeg, arg(3)?)
            } else {
                let (config, rest) = with_screen(&args[1..], Config::load())?;
                let mut renderer = Renderer::new();
                // CPU load needs two samples to measure an interval.
                renderer.draw(&config);
                std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
                (render::to_jpeg(&render::rotate(&renderer.draw(&config), rotation)), rest.first().ok_or(USAGE)?.as_str())
            };
            std::fs::write(out, jpeg)?;
        }
        "gallery" => {
            for item in gallery::list() {
                println!(
                    "{}  {:>4} frames  {:>5.1} s  {:>6.1} MB  {}",
                    item.id,
                    item.frames,
                    item.duration.as_secs_f32(),
                    item.bytes as f32 / 1e6,
                    item.name
                );
            }
        }
        "import" => child_result(gallery::import_in_process(std::path::Path::new(arg(1)?)))?,
        "rotate-item" => child_result(gallery::rotate_in_process(arg(1)?, arg(2)?.parse()?))?,
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

/// Lets `println!` reach the terminal that launched this GUI-subsystem exe.
fn attach_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// False if another tray instance is already running (two would fight over the LCD).
fn single_instance() -> bool {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = "Local\\AtmosLcdTray".encode_utf16().chain(Some(0)).collect();
    // The handle is intentionally leaked; the mutex lives as long as the process.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    !handle.is_null() && unsafe { GetLastError() } != ERROR_ALREADY_EXISTS
}
