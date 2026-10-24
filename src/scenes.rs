//! The screens the app can show. Each returns a finished 480x480 canvas.

use cosmic_text::Weight;
use tiny_skia::{ColorU8, Pixmap};
use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

use crate::render::{self, SIZE, TextRenderer, rgb, TextStyle, stroke_arc};
use crate::stats::Sample;

const BG: ColorU8 = rgb(10, 12, 20);
const TRACK: ColorU8 = rgb(40, 44, 60);
const DIM: ColorU8 = rgb(140, 146, 170);
const CPU_COLOR: ColorU8 = rgb(80, 170, 255);
const GPU_COLOR: ColorU8 = rgb(120, 220, 120);
const HOT: ColorU8 = rgb(255, 90, 70);

const C: f32 = SIZE as f32 / 2.0;
/// Gauges sweep 270 degrees with the gap at the bottom.
const GAUGE_START: f32 = -135.0;
const GAUGE_SWEEP: f32 = 270.0;

pub fn clock(text: &mut TextRenderer) -> Pixmap {
    let now = local_time();
    let mut canvas = render::new_canvas(BG);

    // Seconds ring.
    stroke_arc(&mut canvas, C, C, 200.0, 0.0, 360.0, 6.0, TRACK);
    stroke_arc(&mut canvas, C, C, 200.0, 0.0, now.wSecond as f32 * 6.0, 6.0, CPU_COLOR);

    let time = format!("{:02}:{:02}", now.wHour, now.wMinute);
    text.draw_centered(&mut canvas, &time, C, C - 10.0, &TextStyle { size: 120.0, weight: Weight::BOLD, ..TextStyle::default() });
    const DAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
    let date = format!("{}.{:02} {}", now.wMonth, now.wDay, DAYS[now.wDayOfWeek as usize % 7]);
    text.draw_centered(&mut canvas, &date, C, C + 80.0, &TextStyle { size: 36.0, color: DIM, ..TextStyle::default() });
    canvas
}

/// Outer ring: CPU load. Inner ring: GPU temperature (0-100 °C).
pub fn system(text: &mut TextRenderer, s: &Sample) -> Pixmap {
    let mut canvas = render::new_canvas(BG);

    let cpu = s.cpu_load.clamp(0.0, 100.0);
    stroke_arc(&mut canvas, C, C, 205.0, GAUGE_START, GAUGE_SWEEP, 22.0, TRACK);
    stroke_arc(&mut canvas, C, C, 205.0, GAUGE_START, GAUGE_SWEEP * cpu / 100.0, 22.0, CPU_COLOR);

    if let Some(temp) = s.gpu_temp {
        let t = (temp as f32).clamp(0.0, 100.0);
        let color = if temp >= 80 { HOT } else { GPU_COLOR };
        stroke_arc(&mut canvas, C, C, 172.0, GAUGE_START, GAUGE_SWEEP, 14.0, TRACK);
        stroke_arc(&mut canvas, C, C, 172.0, GAUGE_START, GAUGE_SWEEP * t / 100.0, 14.0, color);
    }

    let label = TextStyle { size: 30.0, weight: Weight::SEMIBOLD, color: DIM, ..TextStyle::default() };
    let value = TextStyle { size: 96.0, weight: Weight::BOLD, ..TextStyle::default() };
    text.draw_centered(&mut canvas, "CPU", C, C - 95.0, &TextStyle { color: CPU_COLOR, ..label });
    text.draw_centered(&mut canvas, &format!("{:.0}%", cpu), C, C - 25.0, &value);

    let gpu = match (s.gpu_temp, s.gpu_load) {
        (Some(t), Some(l)) => format!("{t}°C · {l}%"),
        (Some(t), None) => format!("{t}°C"),
        _ => "--".to_string(),
    };
    text.draw_centered(&mut canvas, "GPU", C, C + 55.0, &TextStyle { color: GPU_COLOR, ..label });
    text.draw_centered(&mut canvas, &gpu, C, C + 100.0, &TextStyle { size: 40.0, weight: Weight::SEMIBOLD, ..TextStyle::default() });
    canvas
}

fn local_time() -> SYSTEMTIME {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    t
}
