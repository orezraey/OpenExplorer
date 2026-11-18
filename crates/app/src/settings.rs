//! The Opções the user picks: appearance, hidden files, local deletion, transfer concurrency
//! and the live activity log. Kept in `settings.json` under the config folder, saved on each
//! change.

use rune::{FontSmoothing, Icon, TextEdit};
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

    pub fn icon(self) -> &'static Icon {
        match self {
            Theme::Auto => &crate::icons::THEME_AUTO,
            Theme::Light => &crate::icons::THEME_LIGHT,
            Theme::Dark => &crate::icons::THEME_DARK,
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

/// Which font family powers the UI (`Sans`) while `Mono` always uses the OS monospace stack.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextFont {
    /// Rune's built-in `Inter` typeface.
    Inter,
    /// Native OS UI typeface (`Segoe UI` on Windows, `San Francisco` on macOS, desktop sans on Linux).
    #[default]
    #[serde(other)]
    System,
}

/// Rune resolves `ui-monospace` natively per OS (`Cascadia Mono`/`Consolas` on Windows,
/// `SF Mono`/`Menlo` on macOS, and `fontconfig`'s `monospace` on Linux).
pub const MONO_STACK: &str = "ui-monospace";

impl TextFont {
    pub const ALL: [TextFont; 2] = [TextFont::System, TextFont::Inter];

    pub fn label(self) -> &'static str {
        match self {
            TextFont::System => "Sistema",
            TextFont::Inter => "Inter",
        }
    }

    pub fn families(self) -> (&'static str, &'static str) {
        let sans = match self {
            TextFont::System if cfg!(windows) => "Segoe UI, Inter",
            TextFont::System if cfg!(target_os = "macos") => "System Font, .SF NS, Inter",
            TextFont::System => "Cantarell, Ubuntu, Noto Sans, Liberation Sans, DejaVu Sans, Inter",
            TextFont::Inter => "Inter",
        };
        (sans, MONO_STACK)
    }
}

/// Experimental glyph rasterizer override (kept in the Experimental tab; `Smooth` is default).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextRender {
    /// Native DirectWrite hinted grayscale on Windows.
    DirectWrite,
    /// Native subpixel ClearType via DirectWrite on Windows.
    ClearType,
    /// Recommended default: Windfoil GPU smoothing with vertical hinting on Windows/Linux, CoreText on macOS.
    #[default]
    #[serde(other)]
    Smooth,
}

impl TextRender {
    pub const ALL: [TextRender; 3] = [TextRender::Smooth, TextRender::DirectWrite, TextRender::ClearType];

    pub fn label(self) -> &'static str {
        match self {
            TextRender::Smooth => "Suave (GPU)",
            TextRender::DirectWrite => "DirectWrite",
            TextRender::ClearType => "ClearType",
        }
    }

    pub fn smoothing(self) -> FontSmoothing {
        match self {
            TextRender::Smooth => FontSmoothing::default(),
            TextRender::DirectWrite if cfg!(windows) => FontSmoothing::DirectWrite,
            TextRender::DirectWrite => FontSmoothing::default(),
            TextRender::ClearType if cfg!(windows) => FontSmoothing::ClearType,
            TextRender::ClearType => FontSmoothing::default(),
        }
    }
}

/// Transfers at once: at least one, at most this many.
pub const MAX_AT_ONCE: u8 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    pub font: TextFont,
    pub text_render: TextRender,
    /// Files sent to the cloud or server at the same time.
    pub uploads: u8,
    /// Files fetched from the cloud or server at the same time.
    pub downloads: u8,
    /// Fetch a big file (64 MB or more) in several parallel pieces: measured, 37 MB/s in one
    /// stream against 58 MB/s in four.
    pub segmented_downloads: bool,
    /// Show hidden files and `.` dotfiles in local folders, servers and the navigation tree.
    pub show_hidden: bool,
    /// Remove finished transfers from the queue automatically once done.
    pub clear_completed: bool,
    /// Delete local files permanently (with confirmation) instead of sending them to Recycle Bin.
    pub skip_recycle_bin: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        // Four uploads at once: measured, one file tops out near 35 MB/s per connection to
        // Google, four together reached 55 MB/s.
        Settings {
            theme: Theme::Auto,
            font: TextFont::System,
            text_render: TextRender::Smooth,
            uploads: 4,
            downloads: 4,
            segmented_downloads: false,
            show_hidden: false,
            clear_completed: true,
            skip_recycle_bin: false,
        }
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

/// Sidebar sections in the Opções dialog.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OptionsTab {
    #[default]
    General,
    Transfers,
    Logs,
    Experimental,
}

impl OptionsTab {
    pub const ALL: [OptionsTab; 4] = [OptionsTab::General, OptionsTab::Transfers, OptionsTab::Logs, OptionsTab::Experimental];

    pub fn label(self) -> &'static str {
        match self {
            OptionsTab::General => "Geral",
            OptionsTab::Transfers => "Transferências",
            OptionsTab::Logs => "Logs",
            OptionsTab::Experimental => "Experimental",
        }
    }

    pub fn icon(self) -> &'static Icon {
        match self {
            OptionsTab::General => &crate::icons::SETTINGS,
            OptionsTab::Transfers => &crate::icons::OPT_TRANSFERS,
            OptionsTab::Logs => &crate::icons::OPT_LOGS,
            OptionsTab::Experimental => &crate::icons::OPT_EXPERIMENTAL,
        }
    }
}

/// Ephemeral UI state of the open Opções dialog.
pub struct OptionsView {
    pub tab: OptionsTab,
    pub log_filter: TextEdit,
    pub log_lines: Vec<String>,
    pub log_bytes: u64,
}

pub const MAX_LOG_LINES: usize = 300;

impl OptionsView {
    pub fn open(tab: OptionsTab) -> OptionsView {
        let (log_lines, log_bytes) = openexplorer_gdrive::read_log(MAX_LOG_LINES);
        OptionsView { tab, log_filter: TextEdit::new(""), log_lines, log_bytes }
    }

    pub fn refresh_logs(&mut self) {
        let (lines, bytes) = openexplorer_gdrive::read_log(MAX_LOG_LINES);
        self.log_lines = lines;
        self.log_bytes = bytes;
    }

    pub fn filtered_logs(&self) -> Vec<&str> {
        let q = self.log_filter.text.trim().to_lowercase();
        if q.is_empty() {
            return self.log_lines.iter().map(String::as_str).collect();
        }
        let words: Vec<&str> = q.split_whitespace().collect();
        self.log_lines
            .iter()
            .map(String::as_str)
            .filter(|line| {
                let lower = line.to_lowercase();
                words.iter().all(|w| lower.contains(w))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_read_back_and_stay_in_bounds() {
        assert_eq!(serde_json::from_str::<Settings>("{}").unwrap(), Settings::default());
        let s: Settings = serde_json::from_str(r#"{"theme":"Dark","uploads":7,"show_hidden":true,"clear_completed":false}"#).unwrap();
        assert_eq!((s.theme, s.uploads, s.downloads, s.show_hidden, s.clear_completed, s.skip_recycle_bin), (Theme::Dark, 7, 4, true, false, false));
        let s: Settings = serde_json::from_str(r#"{"theme":"HighContrast","uploads":7}"#).unwrap();
        assert_eq!((s.theme, s.uploads), (Theme::Auto, 7));
        assert_eq!((at_once(0), at_once(50), at_once(3)), (1, MAX_AT_ONCE, 3));
        assert!(Theme::Auto.dark(true) && !Theme::Light.dark(true) && Theme::Dark.dark(false));
        assert_eq!(Settings::default().font, TextFont::System);
        assert_eq!(Settings::default().text_render, TextRender::Smooth);
    }

    #[test]
    fn log_filter_matches_all_words_case_insensitively() {
        let mut view = OptionsView {
            tab: OptionsTab::Logs,
            log_filter: TextEdit::new("erro sftp"),
            log_lines: vec![
                "10:00:01.000 conectado: voce@gmail.com".into(),
                "10:01:02.500 Erro ao conectar SFTP nas.local".into(),
                "10:02:00.000 upload concluído".into(),
            ],
            log_bytes: 120,
        };
        assert_eq!(view.filtered_logs(), ["10:01:02.500 Erro ao conectar SFTP nas.local"]);
        view.log_filter = TextEdit::new("");
        assert_eq!(view.filtered_logs().len(), 3);
    }
}
