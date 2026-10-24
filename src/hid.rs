//! Minimal Windows HID access: find a device interface by path substring,
//! write reports, and receive input reports on a background thread.

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{HidD_GetHidGuid, HidD_SetOutputReport};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::CreateEventW;
use windows_sys::core::GUID;

const GENERIC_READ_WRITE: u32 = 0x8000_0000 | 0x4000_0000;
/// Writes kept in flight by `write_all`. Measured on the Atmos II: 1 in flight
/// gives 16.1 ms per 54 KB frame, 2 or more about 8.8 ms (the device's limit).
const QUEUE_DEPTH: usize = 4;

/// Returns the device interface paths of all present HID devices.
pub fn device_paths() -> Vec<String> {
    let mut paths = Vec::new();
    unsafe {
        let mut guid: GUID = std::mem::zeroed();
        HidD_GetHidGuid(&mut guid);
        let set = SetupDiGetClassDevsW(&guid, null(), null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if set == -1 {
            return paths;
        }
        let mut iface: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
        iface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        let mut index = 0;
        while SetupDiEnumDeviceInterfaces(set, null(), &guid, index, &mut iface) != 0 {
            index += 1;
            let mut required = 0u32;
            SetupDiGetDeviceInterfaceDetailW(set, &iface, null_mut(), 0, &mut required, null_mut());
            // u32 backing keeps the detail struct aligned.
            let mut buf = vec![0u32; (required as usize).div_ceil(4)];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(set, &iface, detail, required, null_mut(), null_mut()) != 0 {
                let start = (&raw const (*detail).DevicePath) as *const u16;
                let max = (required as usize - 4) / 2;
                let wide = std::slice::from_raw_parts(start, max);
                let len = wide.iter().position(|&c| c == 0).unwrap_or(max);
                paths.push(String::from_utf16_lossy(&wide[..len]));
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    paths
}

struct Handle(HANDLE);

// SAFETY: a Win32 file handle may be used from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    fn open(path: &str, flags: u32) -> io::Result<Handle> {
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                flags,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Handle(h))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// An open HID interface. Writes go through one handle; a second handle is
/// read on a background thread so a blocking read never stalls writes
/// (each HID handle gets its own copy of the input report queue).
pub struct HidDevice {
    writer: Handle,
    reader: std::sync::Arc<Handle>,
    report_len: usize,
}

impl HidDevice {
    /// Opens `path`; input reports of `report_len` bytes arrive on the returned receiver.
    pub fn open(path: &str, report_len: usize) -> io::Result<(HidDevice, Receiver<Vec<u8>>)> {
        // Overlapped so several reports can be queued at once (see `write_all`).
        let writer = Handle::open(path, FILE_FLAG_OVERLAPPED)?;
        let reader = std::sync::Arc::new(Handle::open(path, 0)?);
        let (tx, rx) = mpsc::channel();
        let thread_reader = reader.clone();
        thread::spawn(move || {
            loop {
                let mut buf = vec![0u8; report_len];
                let mut n = 0u32;
                let ok = unsafe {
                    ReadFile(thread_reader.0, buf.as_mut_ptr(), report_len as u32, &mut n, null_mut())
                };
                if ok == 0 || tx.send(buf).is_err() {
                    break;
                }
            }
        });
        Ok((HidDevice { writer, reader, report_len }, rx))
    }

    /// Writes one report over the interrupt OUT pipe; `report[0]` is the report ID.
    pub fn write(&self, report: &[u8]) -> io::Result<()> {
        self.write_all(report)
    }

    /// Writes consecutive reports (`data.len()` a multiple of the report length),
    /// keeping up to `QUEUE_DEPTH` writes in flight. Waiting for each write before
    /// issuing the next leaves the pipe idle between reports and roughly halves throughput.
    pub fn write_all(&self, data: &[u8]) -> io::Result<()> {
        debug_assert_eq!(data.len() % self.report_len, 0);
        let events = (0..QUEUE_DEPTH)
            .map(|_| {
                let h = unsafe { CreateEventW(null(), 1, 0, null()) };
                if h.is_null() { Err(io::Error::last_os_error()) } else { Ok(Handle(h)) }
            })
            .collect::<io::Result<Vec<_>>>()?;
        // Fixed allocation: the kernel holds pointers to these until each write completes.
        let mut slots: Vec<OVERLAPPED> = vec![unsafe { std::mem::zeroed() }; QUEUE_DEPTH];
        let mut in_flight = std::collections::VecDeque::with_capacity(QUEUE_DEPTH);
        let mut result = Ok(());

        for (i, report) in data.chunks(self.report_len).enumerate() {
            let slot = i % QUEUE_DEPTH;
            if in_flight.len() == QUEUE_DEPTH {
                let done = in_flight.pop_front().expect("queue is full");
                result = self.finish(&slots[done]);
                if result.is_err() {
                    break;
                }
            }
            slots[slot] = unsafe { std::mem::zeroed() };
            slots[slot].hEvent = events[slot].0;
            let ok = unsafe {
                WriteFile(self.writer.0, report.as_ptr(), report.len() as u32, null_mut(), &mut slots[slot])
            };
            if ok == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
                result = Err(io::Error::last_os_error());
                break;
            }
            in_flight.push_back(slot);
        }
        // Always drain: the pending writes still reference `data` and `slots`.
        for slot in in_flight {
            let r = self.finish(&slots[slot]);
            if result.is_ok() {
                result = r;
            }
        }
        result
    }

    fn finish(&self, ov: &OVERLAPPED) -> io::Result<()> {
        let mut n = 0u32;
        if unsafe { GetOverlappedResult(self.writer.0, ov, &mut n, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Sends an output report over the control pipe (HID SET_REPORT).
    pub fn set_output_report(&self, report: &[u8]) -> io::Result<()> {
        let ok = unsafe { HidD_SetOutputReport(self.writer.0, report.as_ptr() as *const c_void, report.len() as u32) };
        if !ok {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for HidDevice {
    fn drop(&mut self) {
        // Unblock the reader thread's pending ReadFile so it can exit.
        unsafe { CancelIoEx(self.reader.0, null()) };
    }
}
