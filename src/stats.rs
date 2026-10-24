//! System readings shown on the LCD.

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
use sysinfo::{CpuRefreshKind, RefreshKind, System};

#[derive(Clone, Copy, Default)]
pub struct Sample {
    pub cpu_load: f32,
    pub gpu_temp: Option<u32>,
    pub gpu_load: Option<u32>,
}

pub struct Stats {
    sys: System,
    nvml: Option<Nvml>,
}

impl Stats {
    pub fn new() -> Stats {
        let sys = System::new_with_specifics(RefreshKind::nothing().with_cpu(CpuRefreshKind::nothing().with_cpu_usage()));
        // Absent on machines without an NVIDIA driver; GPU readings are then omitted.
        let nvml = Nvml::init().ok();
        Stats { sys, nvml }
    }

    /// CPU load is measured since the previous call, so call this at a steady interval.
    pub fn sample(&mut self) -> Sample {
        self.sys.refresh_cpu_usage();
        let gpu = self.nvml.as_ref().and_then(|n| n.device_by_index(0).ok());
        Sample {
            cpu_load: self.sys.global_cpu_usage(),
            gpu_temp: gpu.as_ref().and_then(|d| d.temperature(TemperatureSensor::Gpu).ok()),
            gpu_load: gpu.as_ref().and_then(|d| d.utilization_rates().ok()).map(|u| u.gpu),
        }
    }
}
