//! The Opções the user picks: the theme and how many transfers run at once. Kept in
//! `settings.json` under the config folder, saved on each change.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    Light,
    Dark,
    /// As Windows is set; also what an unknown value reads as, so it doesn't reset the rest
    /// (serde wants that variant last).
    #[default]
    #[serde(other)]
    Auto,
}

impl Theme {
    pub const ALL: [Theme; 3] = [Theme::Auto, Theme::Light, Theme::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Theme::Auto => "Auto",
            Theme::Light => "Claro",
            Theme::Dark => "Escuro",
        }
    }

    /// Whether to draw dark, given whether Windows is.
    pub fn dark(self, system_dark: bool) -> bool {
        match self {
            Theme::Auto => system_dark,
            Theme::Light => false,
            Theme::Dark => true,
        }
    }
}

/// Transfers at once: at least one, at most this many.
pub const MAX_AT_ONCE: u8 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    /// Files sent to the cloud at the same time.
    pub uploads: u8,
    /// Files fetched from the cloud at the same time (for downloads, still to come).
    pub downloads: u8,
    /// Fetch a big file in several parallel pieces (for downloads, still to come).
    pub segmented_downloads: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        // Four uploads at once: measured, one file tops out near 35 MB/s per connection to
        // Google, four together reached 55 MB/s.
        Settings { theme: Theme::Auto, uploads: 4, downloads: 4, segmented_downloads: false }
    }
}

/// A count of transfers at once kept between 1 and [`MAX_AT_ONCE`].
pub fn at_once(n: i32) -> u8 {
    n.clamp(1, MAX_AT_ONCE as i32) as u8
}

fn file() -> std::path::PathBuf {
    openexplorer_gdrive::config_dir().join("settings.json")
}

impl Settings {
    pub fn load() -> Settings {
        let s: Settings = std::fs::read(file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Settings { uploads: at_once(s.uploads.into()), downloads: at_once(s.downloads.into()), ..s }
    }

    pub fn save(&self) {
        let Ok(text) = serde_json::to_vec_pretty(self) else { return };
        let _ = std::fs::create_dir_all(openexplorer_gdrive::config_dir());
        let _ = std::fs::write(file(), text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_read_back_and_stay_in_bounds() {
        assert_eq!(serde_json::from_str::<Settings>("{}").unwrap(), Settings::default());
        let s: Settings = serde_json::from_str(r#"{"theme":"Dark","uploads":7}"#).unwrap();
        assert_eq!((s.theme, s.uploads, s.downloads), (Theme::Dark, 7, 4));
        let s: Settings = serde_json::from_str(r#"{"theme":"HighContrast","uploads":7}"#).unwrap();
        assert_eq!((s.theme, s.uploads), (Theme::Auto, 7));
        assert_eq!((at_once(0), at_once(50), at_once(3)), (1, MAX_AT_ONCE, 3));
        assert!(Theme::Auto.dark(true) && !Theme::Light.dark(true) && Theme::Dark.dark(false));
    }
}
