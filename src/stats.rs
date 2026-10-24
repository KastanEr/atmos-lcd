//! System readings shown on the LCD.
//!
//! Read straight from Windows, the same sources Task Manager uses: CPU times from
//! `GetSystemTimes`, GPU engine busy time and temperature from the graphics kernel
//! (D3DKMT). Libraries like NVML cost ~20 MB of memory for two numbers.

use std::time::Instant;

use windows::Wdk::Graphics::Direct3D::*;
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1};
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Threading::GetSystemTimes;

#[derive(Clone, Copy, Default)]
pub struct Sample {
    pub cpu_load: f32,
    pub gpu_temp: Option<u32>,
    pub gpu_load: Option<u32>,
}

pub struct Stats {
    /// Idle and total (kernel + user) CPU time at the previous sample, in 100 ns.
    cpu: Option<(u64, u64)>,
    gpu: Option<Gpu>,
}

impl Stats {
    pub fn new() -> Stats {
        let mut stats = Stats { cpu: None, gpu: Gpu::open() };
        stats.sample();
        stats
    }

    /// Loads are measured since the previous call, so call this at a steady interval.
    pub fn sample(&mut self) -> Sample {
        let (gpu_temp, gpu_load) = self.gpu.as_mut().map_or((None, None), |g| (g.temperature(), g.load()));
        Sample { cpu_load: self.cpu_load(), gpu_temp, gpu_load }
    }

    fn cpu_load(&mut self) -> f32 {
        let mut idle = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut kernel, mut user) = (idle, idle);
        if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
            return 0.0;
        }
        let ticks = |t: FILETIME| (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64;
        // Kernel time includes the idle time.
        let now = (ticks(idle), ticks(kernel) + ticks(user));
        let load = match self.cpu.replace(now) {
            Some((idle0, total0)) if now.1 > total0 => {
                let total = (now.1 - total0) as f32;
                (total - now.0.saturating_sub(idle0) as f32) / total * 100.0
            }
            _ => 0.0,
        };
        load.clamp(0.0, 100.0)
    }
}

/// The discrete (largest) GPU, through the graphics kernel.
struct Gpu {
    luid: LUID,
    adapter: u32,
    nodes: u32,
    /// Running time of each engine at the previous sample, in 100 ns, and when it was taken.
    busy: Option<(Vec<i64>, Instant)>,
}

impl Gpu {
    fn open() -> Option<Gpu> {
        let luid = unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            let mut best: Option<(usize, LUID)> = None;
            let mut i = 0;
            while let Ok(adapter) = factory.EnumAdapters1(i) {
                i += 1;
                let Ok(desc) = adapter.GetDesc1() else { continue };
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    continue;
                }
                if best.is_none_or(|(vram, _)| desc.DedicatedVideoMemory > vram) {
                    best = Some((desc.DedicatedVideoMemory, desc.AdapterLuid));
                }
            }
            best?.1
        };
        let mut open = D3DKMT_OPENADAPTERFROMLUID { AdapterLuid: luid, hAdapter: 0 };
        if unsafe { D3DKMTOpenAdapterFromLuid(&mut open) }.is_err() {
            return None;
        }
        let mut query = D3DKMT_QUERYSTATISTICS { Type: D3DKMT_QUERYSTATISTICS_ADAPTER, AdapterLuid: luid, ..Default::default() };
        let nodes = if unsafe { D3DKMTQueryStatistics(&mut query) }.is_ok() {
            unsafe { query.QueryResult.AdapterInformation.NodeCount }
        } else {
            0
        };
        let mut gpu = Gpu { luid, adapter: open.hAdapter, nodes, busy: None };
        gpu.load();
        Some(gpu)
    }

    /// Degrees Celsius, if the driver reports it (WDDM 2.7+).
    fn temperature(&self) -> Option<u32> {
        let mut perf = D3DKMT_ADAPTER_PERFDATA::default();
        let mut query = D3DKMT_QUERYADAPTERINFO {
            hAdapter: self.adapter,
            Type: KMTQAITYPE_ADAPTERPERFDATA,
            pPrivateDriverData: &mut perf as *mut _ as *mut _,
            PrivateDriverDataSize: size_of::<D3DKMT_ADAPTER_PERFDATA>() as u32,
        };
        unsafe { D3DKMTQueryAdapterInfo(&mut query) }.ok().ok()?;
        // Reported in tenths of a degree; 0 means no sensor.
        (perf.Temperature > 0).then(|| (perf.Temperature + 5) / 10)
    }

    /// Busiest engine's share of the time since the previous call, in percent (as Task Manager).
    fn load(&mut self) -> Option<u32> {
        let mut running = Vec::with_capacity(self.nodes as usize);
        for node in 0..self.nodes {
            let mut query = D3DKMT_QUERYSTATISTICS { Type: D3DKMT_QUERYSTATISTICS_NODE, AdapterLuid: self.luid, ..Default::default() };
            query.Anonymous.QueryNode.NodeId = node;
            let ok = unsafe { D3DKMTQueryStatistics(&mut query) }.is_ok();
            running.push(if ok { unsafe { query.QueryResult.NodeInformation.GlobalInformation.RunningTime } } else { 0 });
        }
        let now = Instant::now();
        let (before, then) = self.busy.replace((running.clone(), now))?;
        let elapsed = (now - then).as_nanos() as f64 / 100.0;
        if elapsed <= 0.0 {
            return None;
        }
        let busiest = running.iter().zip(&before).map(|(a, b)| (a - b).max(0)).max().unwrap_or(0);
        Some((busiest as f64 / elapsed * 100.0).round().min(100.0) as u32)
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        let close = D3DKMT_CLOSEADAPTER { hAdapter: self.adapter };
        unsafe {
            let _ = D3DKMTCloseAdapter(&close);
        }
    }
}
