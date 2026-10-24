//! What the app uploaded into the cooler's stored-media slots. The device can only
//! report whether a slot is filled, so names and thumbnails are kept here, under
//! `%LOCALAPPDATA%\atmos-lcd\slots\<n>.txt|.jpg`, independent of the gallery.

use std::fs;
use std::path::PathBuf;

use crate::gallery;

/// Slots the device offers (`maxSuspendMediaCount`).
pub const COUNT: usize = 5;

fn dir() -> PathBuf {
    gallery::root().with_file_name("slots")
}

pub fn thumb_path(slot: usize) -> PathBuf {
    dir().join(format!("{slot}.jpg"))
}

/// Name of what the app stored in `slot`, if it was this app.
pub fn name(slot: usize) -> Option<String> {
    fs::read_to_string(dir().join(format!("{slot}.txt"))).ok()
}

/// The gallery item and the rotation baked into what the app stored in `slot`.
pub fn source(slot: usize) -> Option<(String, u16)> {
    let text = fs::read_to_string(dir().join(format!("{slot}.src"))).ok()?;
    let (id, rotation) = text.split_once('\n')?;
    Some((id.to_string(), rotation.trim().parse().ok()?))
}

/// Remembers that gallery item `id`, rotated by `rotation` degrees, now fills `slot`.
pub fn record(slot: usize, id: &str, name: &str, rotation: u16) -> std::io::Result<()> {
    fs::create_dir_all(dir())?;
    fs::write(dir().join(format!("{slot}.txt")), name)?;
    fs::write(dir().join(format!("{slot}.src")), format!("{id}\n{rotation}"))?;
    // A missing thumbnail only costs the picture in the slot view.
    let _ = fs::copy(gallery::thumb_path(id), thumb_path(slot));
    Ok(())
}

pub fn forget(slot: usize) {
    let _ = fs::remove_file(dir().join(format!("{slot}.txt")));
    let _ = fs::remove_file(dir().join(format!("{slot}.src")));
    let _ = fs::remove_file(thumb_path(slot));
}
