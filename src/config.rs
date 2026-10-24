//! Persistent settings in `%APPDATA%\atmos-lcd\config.txt` as `key=value` lines.

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Clockwise screen rotation in degrees, 0..360.
    pub rotation: u16,
    pub brightness: u8,
    /// `clock`, `system` or `gallery`.
    pub screen: String,
    /// Gallery item id shown by the `gallery` screen.
    pub item: Option<String>,
    /// Don't stream; let the cooler play its stored media.
    pub offline: bool,
    /// Keep the LCD showing stored media while the PC sleeps.
    pub display_in_sleep: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { rotation: 0, brightness: 80, screen: "system".into(), item: None, offline: false, display_in_sleep: false }
    }
}

impl Config {
    fn path() -> PathBuf {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| ".".into());
        base.join("atmos-lcd").join("config.txt")
    }

    pub fn exists() -> bool {
        Self::path().exists()
    }

    /// Missing or unreadable files yield defaults; unknown keys are ignored.
    pub fn load() -> Config {
        let mut c = Config::default();
        let Ok(text) = std::fs::read_to_string(Self::path()) else { return c };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            match key.trim() {
                "rotation" => c.rotation = value.parse().map_or(c.rotation, |r: u16| r % 360),
                "brightness" => c.brightness = value.parse().map_or(c.brightness, |b: u8| b.min(100)),
                "screen" => c.screen = value.to_string(),
                "item" if !value.is_empty() => c.item = Some(value.into()),
                "offline" => c.offline = value == "1",
                "display_in_sleep" => c.display_in_sleep = value == "1",
                _ => {}
            }
        }
        c
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = format!(
            "rotation={}\nbrightness={}\nscreen={}\nitem={}\noffline={}\ndisplay_in_sleep={}\n",
            self.rotation,
            self.brightness,
            self.screen,
            self.item.as_deref().unwrap_or(""),
            self.offline as u8,
            self.display_in_sleep as u8
        );
        std::fs::write(path, text)
    }

    /// Splits the rotation into the part the firmware applies (multiples of 90,
    /// stored on the device so offline media follows it) and the remainder the
    /// app applies to each frame.
    pub fn split_rotation(&self) -> (u16, f32) {
        let r = self.rotation % 360;
        (r / 90 * 90, (r % 90) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_split() {
        let at = |rotation| Config { rotation, ..Config::default() }.split_rotation();
        assert_eq!(at(0), (0, 0.0));
        assert_eq!(at(60), (0, 60.0));
        assert_eq!(at(180), (180, 0.0));
        assert_eq!(at(300), (270, 30.0));
    }
}
