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
mod slots;
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
  stats                         print CPU and GPU readings for a few seconds
  gallery                       list gallery items
  import <file>                 add a picture, GIF/WebP or video to the gallery
  bench <id> [fps...]           measure frame transfer speed with a gallery item
  media-upload <name> <type> <file>   store a file on the device (e.g. suspend_1.jpg suspend)
  media-delete <name> <type>
  realtime on|off               off: play the device's stored media
screens:
  clock | system | gallery <id> | file <path>   (file: preview only)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = if args.is_empty() {
        if !single_instance() {
            // Starting the exe again opens the running instance's window.
            app::show_running_instance();
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
        "stats" => {
            let mut stats = stats::Stats::new();
            for _ in 0..5 {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let s = stats.sample();
                println!("cpu {:.1}% gpu {:?}C {:?}%", s.cpu_load, s.gpu_temp, s.gpu_load);
            }
        }
        "run" => {
            let (config, _) = with_screen(&args[1..], Config::load())?;
            let hooks = Hooks {
                frame: Box::new(|_| {}),
                preview: Default::default(),
                status: Box::new(|s| eprintln!("{s:?}")),
                storage: Box::new(|_| {}),
            };
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
                std::thread::sleep(std::time::Duration::from_millis(500));
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
        "bench" => {
            let fps: Vec<u32> = args[2..].iter().map(|a| a.parse()).collect::<Result<_, _>>()?;
            bench(arg(1)?, if fps.is_empty() { &[15, 20, 25, 30] } else { &fps })?;
        }
        // Low-level access to the device's stored media, for experiments.
        "media-upload" => {
            let data = std::fs::read(arg(3)?)?;
            Atmos::open()?.upload(arg(1)?, arg(2)?, &data, &mut |_| {})?;
            println!("uploaded {} bytes", data.len());
        }
        "media-delete" => println!("{}", Atmos::open()?.delete_media(arg(1)?, arg(2)?)?),
        "realtime" => println!("{}", Atmos::open()?.set_realtime(arg(1)? == "on")?),
        "import" => child_result(gallery::import_in_process(std::path::Path::new(arg(1)?)))?,
        "export" => child_result(gallery::export_in_process(arg(1)?, arg(2)?.parse()?, std::path::Path::new(arg(3)?)))?,
        "rotate-item" => child_result(gallery::rotate_in_process(arg(1)?, arg(2)?.parse()?))?,
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

/// Measures how fast frames reach the device, then plays at fixed rates for a visual check.
fn bench(id: &str, rates: &[u32]) -> Result<(), Box<dyn std::error::Error>> {
    use std::time::{Duration, Instant};
    let file = gallery::FrameFile::open(&gallery::frames_path(id, 0))?;
    let frames: Vec<Vec<u8>> = (0..file.len()).map(|i| file.read(i)).collect::<Result<_, _>>()?;
    let mut dev = Atmos::open()?;
    dev.keep_realtime_alive()?;
    let mut last_keepalive = Instant::now();
    let mut keepalive = |dev: &mut Atmos| -> std::io::Result<()> {
        if last_keepalive.elapsed() >= device::KEEPALIVE_INTERVAL {
            dev.keep_realtime_alive()?;
            last_keepalive = Instant::now();
        }
        Ok(())
    };

    let avg_kb = frames.iter().map(Vec::len).sum::<usize>() as f32 / frames.len() as f32 / 1024.0;
    println!("{} frames, average {avg_kb:.1} KB ({:.0} reports)", frames.len(), avg_kb * 1024.0 / 1000.0);
    let n = 120;
    let start = Instant::now();
    for i in 0..n {
        keepalive(&mut dev)?;
        dev.send_frame(&frames[i % frames.len()])?;
    }
    let per_frame = start.elapsed() / n as u32;
    println!(
        "back to back: {:.1} ms per frame -> max {:.1} fps, {:.0} KB/s",
        per_frame.as_secs_f32() * 1000.0,
        1.0 / per_frame.as_secs_f32(),
        avg_kb / per_frame.as_secs_f32()
    );

    let mut text = render::TextRenderer::new();
    for &rate in rates {
        // Announce the phase on the LCD itself so it can be told apart while watching.
        let mut label = render::new_canvas(render::rgb(0, 0, 0));
        let style = render::TextStyle { size: 120.0, weight: cosmic_text::Weight::BOLD, ..Default::default() };
        text.draw_centered(&mut label, &format!("{rate} fps"), 240.0, 240.0, &style);
        let label = render::to_jpeg(&label);
        let shown = Instant::now();
        while shown.elapsed() < Duration::from_secs(2) {
            keepalive(&mut dev)?;
            dev.send_frame(&label)?;
            std::thread::sleep(Duration::from_millis(200));
        }

        println!("playing at {rate} fps for 10 s ...");
        let interval = Duration::from_secs_f32(1.0 / rate as f32);
        let (start, mut next, mut late) = (Instant::now(), Instant::now(), 0);
        let mut i = 0;
        while start.elapsed() < Duration::from_secs(10) {
            keepalive(&mut dev)?;
            dev.send_frame(&frames[i % frames.len()])?;
            i += 1;
            next += interval;
            match next.checked_duration_since(Instant::now()) {
                Some(wait) => std::thread::sleep(wait),
                None => late += 1,
            }
        }
        println!("  sent {i} frames ({:.1} fps actual), {late} late", i as f32 / 10.0);
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
