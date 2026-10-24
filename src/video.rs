//! Video decoding with Windows Media Foundation (no bundled codecs: whatever
//! Windows can play, e.g. H.264/MP4, works).

use std::path::Path;
use std::time::Duration;

use image::{DynamicImage, RgbaImage};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
    ID3D11Device, ID3D11Multithread,
};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFDXGIDeviceManager, IMFSourceReader, MF_MT_DEFAULT_STRIDE, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    IMFMediaType, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_INTERLACE_MODE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_PD_DURATION, MF_SOURCE_READER_D3D_MANAGER,
    MF_SOURCE_READER_MEDIASOURCE, MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL,
    MFVideoFormat_H264, MFVideoInterlace_Progressive, eAVEncH264VProfile_Base, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MFCreateDXGIDeviceManager, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video, MFSTARTUP_NOSOCKET, MFShutdown, MFStartup,
    MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{HSTRING, Interface};

use crate::render::SIZE;

pub const EXTENSIONS: [&str; 7] = ["mp4", "m4v", "mov", "wmv", "avi", "mkv", "webm"];

pub fn is_video(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    EXTENSIONS.contains(&ext.as_str())
}

/// Decodes `path`, calling `on_frame(image, delay)` for frames spaced at least
/// `interval` apart, up to `max_len` of video, and `on_progress` with 0.0..=1.0.
/// Returns whether the video was cut short.
pub fn decode(
    path: &Path,
    interval: Duration,
    max_len: Duration,
    mut on_frame: impl FnMut(DynamicImage, Duration),
    mut on_progress: impl FnMut(f32),
) -> Result<bool, String> {
    let _mf = MediaFoundation::start().map_err(|e| format!("Media Foundation: {e}"))?;
    let err = |e: windows::core::Error| format!("{}: {}", path.display(), e.message());
    let reader = open_reader(path).map_err(err)?;
    let (mut width, mut height, mut stride) = frame_layout(&reader).map_err(err)?;

    // Media Foundation timestamps are in 100 ns units.
    let interval_ticks = interval.as_nanos() as i64 / 100;
    let max_ticks = max_len.as_nanos() as i64 / 100;
    let total_ticks = duration_ticks(&reader).unwrap_or(max_ticks).clamp(1, max_ticks);
    // Pick frames on a fixed grid so a 30 fps source yields every other frame.
    // The tolerance absorbs timestamp rounding (e.g. 333333 vs 333334 ticks).
    let tolerance = interval_ticks / 10;
    let mut next_pick = 0i64;
    // The previously picked frame; its delay is known once the next one is picked.
    let mut pending: Option<(DynamicImage, i64)> = None;
    let mut truncated = false;

    loop {
        let (mut flags, mut timestamp) = (0u32, 0i64);
        let mut sample = None;
        unsafe {
            reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )
                .map_err(err)?;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            break;
        }
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
            (width, height, stride) = frame_layout(&reader).map_err(err)?;
        }
        let Some(sample) = sample else { continue };
        if timestamp >= max_ticks {
            truncated = true;
            break;
        }
        if timestamp + tolerance < next_pick {
            continue;
        }
        next_pick = (next_pick + interval_ticks).max(timestamp + interval_ticks - tolerance);

        let image = unsafe {
            let buffer = sample.ConvertToContiguousBuffer().map_err(err)?;
            let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
            buffer.Lock(&mut data, None, Some(&mut len)).map_err(err)?;
            let bytes = std::slice::from_raw_parts(data, len as usize);
            let image = bgrx_to_rgba(bytes, width, height, stride);
            buffer.Unlock().map_err(err)?;
            image
        };
        if let Some((prev, prev_ts)) = pending.replace((image, timestamp)) {
            on_frame(prev, ticks_to_duration(timestamp - prev_ts));
        }
        on_progress((timestamp as f32 / total_ticks as f32).min(1.0));
    }
    if let Some((last, _)) = pending {
        on_frame(last, interval);
    }
    Ok(truncated)
}

/// Container duration in 100 ns ticks, if the file states one.
fn duration_ticks(reader: &IMFSourceReader) -> Option<i64> {
    unsafe {
        let pv = reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION).ok()?;
        Some(pv.Anonymous.Anonymous.Anonymous.uhVal as i64)
    }
}

fn ticks_to_duration(ticks: i64) -> Duration {
    Duration::from_nanos(ticks.max(0) as u64 * 100)
}

/// Opens the first video stream as RGB32 scaled so its short side is the LCD size.
/// Prefers GPU decoding and scaling; falls back to CPU paths when unavailable.
fn open_reader(path: &Path) -> windows::core::Result<IMFSourceReader> {
    let url = HSTRING::from(path.as_os_str());
    let gpu = || -> windows::core::Result<IMFSourceReader> {
        let manager = d3d_manager()?;
        open_with(&url, |attrs| unsafe {
            attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)?;
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
        }, true)
    };
    gpu()
        .or_else(|_| open_with(&url, |attrs| unsafe { attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1) }, true))
        .or_else(|_| open_with(&url, |attrs| unsafe { attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1) }, false))
}

/// Creates a source reader with `configure`d attributes, requesting RGB32 output,
/// downscaled by the reader's video processor when `scale` is set.
fn open_with(
    url: &HSTRING,
    configure: impl FnOnce(&IMFAttributes) -> windows::core::Result<()>,
    scale: bool,
) -> windows::core::Result<IMFSourceReader> {
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 3)?;
        let attrs = attrs.expect("MFCreateAttributes succeeded");
        configure(&attrs)?;
        let reader = MFCreateSourceReaderFromURL(url, &attrs)?;

        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(stream, true)?;
        let wanted = MFCreateMediaType()?;
        wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        wanted.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        if scale {
            let native = reader.GetNativeMediaType(stream, 0)?.GetUINT64(&MF_MT_FRAME_SIZE)?;
            let (w, h) = scaled_size((native >> 32) as u32, native as u32);
            wanted.SetUINT64(&MF_MT_FRAME_SIZE, (w as u64) << 32 | h as u64)?;
        }
        reader.SetCurrentMediaType(stream, None, &wanted)?;
        Ok(reader)
    }
}

/// Keeps the aspect ratio with the short side at the LCD size (even dimensions).
fn scaled_size(w: u32, h: u32) -> (u32, u32) {
    let short = w.min(h).max(1) as u64;
    if short <= SIZE as u64 {
        return (w, h);
    }
    let scale = |v: u32| ((v as u64 * SIZE as u64 / short) as u32 + 1) & !1;
    (scale(w), scale(h))
}

fn d3d_manager() -> windows::core::Result<IMFDXGIDeviceManager> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
        let device = device.expect("D3D11CreateDevice succeeded");
        // Media Foundation uses the device from several threads.
        let _previous = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
        let (mut token, mut manager) = (0u32, None);
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager: IMFDXGIDeviceManager = manager.expect("MFCreateDXGIDeviceManager succeeded");
        manager.ResetDevice(&device, token)?;
        Ok(manager)
    }
}

/// Width, height and row stride in bytes (negative for bottom-up images).
fn frame_layout(reader: &IMFSourceReader) -> windows::core::Result<(u32, u32, i32)> {
    unsafe {
        let ty = reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)?;
        let size = ty.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let (width, height) = ((size >> 32) as u32, size as u32);
        let stride = ty.GetUINT32(&MF_MT_DEFAULT_STRIDE).map_or(width as i32 * 4, |s| s as i32);
        Ok((width, height, stride))
    }
}

/// RGB32 from Media Foundation is B, G, R, X in memory.
fn bgrx_to_rgba(bytes: &[u8], width: u32, height: u32, stride: i32) -> DynamicImage {
    let row_len = width as usize * 4;
    let mut out = RgbaImage::new(width, height);
    for (y, dst) in out.chunks_exact_mut(row_len).enumerate() {
        let src_row = if stride >= 0 { y } else { height as usize - 1 - y };
        let start = src_row * stride.unsigned_abs() as usize;
        let Some(src) = bytes.get(start..start + row_len) else { break };
        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], 255]);
        }
    }
    DynamicImage::ImageRgba8(out)
}

/// COM + Media Foundation lifetime for the current thread.
struct MediaFoundation;

impl MediaFoundation {
    fn start() -> windows::core::Result<MediaFoundation> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) {
                CoUninitialize();
                return Err(e);
            }
        }
        Ok(MediaFoundation)
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

/// Encodes square RGBA frames (with display durations) into an H.264 MP4 the cooler can
/// store and play by itself. Settings follow MasterCTRL's ffmpeg call: ~2 Mbps, yuv420p,
/// `moov` before `mdat`. Baseline profile keeps the device's decoder on safe ground.
pub fn encode_mp4(
    out: &Path,
    size: u32,
    fps: u32,
    frames: impl Iterator<Item = Result<(RgbaImage, Duration), String>>,
) -> Result<(), String> {
    let _mf = MediaFoundation::start().map_err(|e| format!("Media Foundation: {e}"))?;
    let err = |e: windows::core::Error| format!("MP4 encoder: {}", e.message());
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 2).map_err(err)?;
        let attrs = attrs.expect("MFCreateAttributes succeeded");
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1).map_err(err)?;
        let writer = MFCreateSinkWriterFromURL(&HSTRING::from(out.as_os_str()), None, &attrs).map_err(err)?;

        let frame_size = (size as u64) << 32 | size as u64;
        let frame_rate = (fps as u64) << 32 | 1;
        let common = |t: &IMFMediaType| -> windows::core::Result<()> {
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
            t.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
            t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1u64 << 32 | 1)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
        };
        let encoded = MFCreateMediaType().map_err(err)?;
        common(&encoded).map_err(err)?;
        encoded.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(err)?;
        encoded.SetUINT32(&MF_MT_AVG_BITRATE, 2_000_000).map_err(err)?;
        encoded.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32).map_err(err)?;
        let stream = writer.AddStream(&encoded).map_err(err)?;

        let input = MFCreateMediaType().map_err(err)?;
        common(&input).map_err(err)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).map_err(err)?;
        // Positive stride: rows are stored top-down.
        input.SetUINT32(&MF_MT_DEFAULT_STRIDE, size * 4).map_err(err)?;
        writer.SetInputMediaType(stream, &input, None).map_err(err)?;
        writer.BeginWriting().map_err(err)?;

        let len = size * size * 4;
        let mut time = 0i64;
        for frame in frames {
            let (img, delay) = frame?;
            if img.width() != size || img.height() != size {
                return Err(format!("frame is {}x{}, expected {size}x{size}", img.width(), img.height()));
            }
            let buffer = MFCreateMemoryBuffer(len).map_err(err)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None).map_err(err)?;
            let dst = std::slice::from_raw_parts_mut(data, len as usize);
            for (d, s) in dst.chunks_exact_mut(4).zip(img.as_raw().chunks_exact(4)) {
                d.copy_from_slice(&[s[2], s[1], s[0], 255]);
            }
            buffer.Unlock().map_err(err)?;
            buffer.SetCurrentLength(len).map_err(err)?;

            let duration = (delay.as_nanos() / 100).max(1) as i64;
            let sample = MFCreateSample().map_err(err)?;
            sample.AddBuffer(&buffer).map_err(err)?;
            sample.SetSampleTime(time).map_err(err)?;
            sample.SetSampleDuration(duration).map_err(err)?;
            writer.WriteSample(stream, &sample).map_err(err)?;
            time += duration;
        }
        writer.Finalize().map_err(err)?;
    }
    Ok(())
}
