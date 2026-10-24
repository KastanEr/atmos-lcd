// No console window when started by double-click or at login; CLI use attaches to the caller's console.
#![windows_subsystem = "windows"]

mod app;
mod autostart;
mod config;
mod device;
mod hid;
mod media;
mod render;
mod scenes;
mod stats;
mod worker;

use std::process::ExitCode;

use config::Config;
use device::Atmos;
use worker::{Hooks, Renderer, Scene, Worker};

const USAGE: &str = "\
usage: atmos-lcd [command]
  (no command)                  start the tray app
  param                         print current device parameters
  brightness <0-100>
  rotate <degrees>              e.g. 60 for a hexagonal pump; saved to config
  run <screen>                  show a screen in the foreground until Ctrl+C
  preview <screen> <out.jpg>    render a screen to a file without the device
screens:
  clock | system | image <file>";

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

/// Applies `<screen> [file]` to the saved config and returns the remaining args.
fn with_screen<'a>(args: &'a [String], mut config: Config) -> Result<(Config, &'a [String]), &'static str> {
    match args.first().map(String::as_str) {
        Some(s @ ("clock" | "system")) => {
            config.screen = s.into();
            Ok((config, &args[1..]))
        }
        Some("image") => {
            config.screen = "image".into();
            config.image = Some(args.get(1).ok_or(USAGE)?.into());
            Ok((config, &args[2..]))
        }
        _ => Err(USAGE),
    }
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
            let (config, rest) = with_screen(&args[1..], Config::load())?;
            let out = rest.first().ok_or(USAGE)?;
            let mut renderer = Renderer::new();
            if config.screen == "system" {
                // CPU load needs two samples to measure an interval.
                renderer.draw(&config)?;
                std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
            }
            let jpeg = match renderer.draw(&config)? {
                Scene::Still(canvas) => render::to_jpeg(&render::rotate(&canvas, config.split_rotation().1)),
                Scene::Animated(anim) => {
                    let total: std::time::Duration = anim.frames.iter().map(|f| f.delay).sum();
                    println!("animation: {} frames, {:.1} s per loop", anim.frames.len(), total.as_secs_f32());
                    anim.frames[0].wire.clone()
                }
            };
            std::fs::write(out, jpeg)?;
        }
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
