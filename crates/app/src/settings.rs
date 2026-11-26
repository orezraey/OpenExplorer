//! The Opções the user picks: appearance, hidden files, local deletion, transfer concurrency
//! and the live activity log. Kept in `settings.json` under the config folder, saved on each
//! change.

use openexplorer_i18n::t;
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
            Theme::Light => t("Claro", "Light"),
            Theme::Dark => t("Escuro", "Dark"),
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
            TextFont::System => t("Sistema", "System"),
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

/// Experimental: which Windows 11 material goes behind the window while `Settings::mica` is on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Material {
    /// Mica with the wallpaper's tint stronger, as tabbed apps such as Terminal use.
    MicaAlt,
    /// What lies behind the window, other apps included, blurred.
    Acrylic,
    /// Tinted by the wallpaper, lightly: Windows' material for an app's main window. Last, so
    /// a name it doesn't know reads as Mica rather than losing the whole settings file.
    #[default]
    #[serde(other)]
    Mica,
}

impl Material {
    pub const ALL: [Material; 3] = [Material::Mica, Material::MicaAlt, Material::Acrylic];

    pub fn label(self) -> &'static str {
        match self {
            Material::Mica => "Mica",
            Material::MicaAlt => "Mica Alt",
            Material::Acrylic => "Acrylic",
        }
    }

    pub fn backdrop(self) -> rune::WindowBackdrop {
        match self {
            Material::Mica => rune::WindowBackdrop::Mica,
            Material::MicaAlt => rune::WindowBackdrop::MicaAlt,
            Material::Acrylic => rune::WindowBackdrop::Acrylic,
        }
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

    /// Modes supported on the current OS (`DirectWrite` and `ClearType` are Windows-only).
    pub fn available() -> &'static [TextRender] {
        if cfg!(windows) { &Self::ALL } else { &[TextRender::Smooth] }
    }

    pub fn label(self) -> &'static str {
        match self {
            TextRender::Smooth => t("Suave (GPU)", "Smooth (GPU)"),
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

/// The interface's language: the system's (Portuguese for a Portuguese Windows, English for any
/// other) or one picked in Options.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    Portuguese,
    English,
    #[default]
    #[serde(other)]
    Auto,
}

impl Language {
    pub const ALL: [Language; 3] = [Language::Auto, Language::Portuguese, Language::English];

    /// Its name in the options, each language in its own words.
    pub fn label(self) -> &'static str {
        match self {
            Language::Auto => openexplorer_i18n::t("Automático", "Automatic"),
            Language::Portuguese => "Português",
            Language::English => "English",
        }
    }

    /// Whether it shows the interface in English.
    pub fn english(self) -> bool {
        match self {
            Language::Auto => openexplorer_i18n::system_english(),
            Language::Portuguese => false,
            Language::English => true,
        }
    }

    /// Makes it the interface's language from now on.
    pub fn apply(self) {
        openexplorer_i18n::set_english(self.english());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub language: Language,
    pub theme: Theme,
    pub font: TextFont,
    pub text_render: TextRender,
    /// Files sent to the cloud or server at the same time.
    pub uploads: u8,
    /// Files fetched from the cloud or server at the same time.
    pub downloads: u8,
    /// Fetch a big file (64 MB or more) in parallel pieces: measured on a 500 MiB file, 10.4 s
    /// in one stream against 5.8 s in eight. On by default; kept under a name of its own since
    /// it started off, so everyone's old `segmented_downloads: false`, left from before downloads
    /// existed, doesn't keep it off.
    #[serde(rename = "big_files_in_segments")]
    pub segmented_downloads: bool,
    /// Show hidden files and `.` dotfiles in local folders, servers and the navigation tree.
    pub show_hidden: bool,
    /// Remove finished transfers from the queue automatically once done.
    pub clear_completed: bool,
    /// Delete local files permanently (with confirmation) instead of sending them to Recycle Bin.
    pub skip_recycle_bin: bool,
    /// Where the main window was when it last closed, to open there again.
    pub window: Option<crate::window_place::Saved>,
    /// Experimental: a Windows 11 material (`material`) behind the window. Turned on, it takes
    /// effect from the next start; turned off or changed, at once.
    pub mica: bool,
    pub material: Material,
    /// Experimental: a search's matches in names marked as with a highlighter, rather than
    /// only coloured.
    pub search_marker: bool,
    /// Experimental (Windows): the window's content keeps up with a drag of its left or top
    /// edge (Rune's `ResizeSync::On`), from the next start.
    pub resize_sync: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        // Four uploads at once: measured, one file tops out near 35 MB/s per connection to
        // Google, four together reached 55 MB/s.
        Settings {
            language: Language::Auto,
            theme: Theme::Auto,
            font: TextFont::System,
            text_render: TextRender::Smooth,
            uploads: 4,
            downloads: 4,
            segmented_downloads: true,
            show_hidden: false,
            clear_completed: true,
            skip_recycle_bin: false,
            window: None,
            mica: false,
            material: Material::Mica,
            search_marker: false,
            resize_sync: false,
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

impl Theme {
    /// The window's own light or dark (its title bar, and Mica's tint) for this theme.
    pub fn color_scheme(self) -> rune::ColorScheme {
        match self {
            Theme::Auto => rune::ColorScheme::System,
            Theme::Light => rune::ColorScheme::Light,
            Theme::Dark => rune::ColorScheme::Dark,
        }
    }
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
    Cache,
    Logs,
    Experimental,
}

impl OptionsTab {
    pub const ALL: [OptionsTab; 5] = [OptionsTab::General, OptionsTab::Transfers, OptionsTab::Cache, OptionsTab::Logs, OptionsTab::Experimental];

    pub fn label(self) -> &'static str {
        match self {
            OptionsTab::General => t("Geral", "General"),
            OptionsTab::Transfers => t("Transferências", "Transfers"),
            OptionsTab::Cache => "Cache",
            OptionsTab::Logs => "Logs",
            OptionsTab::Experimental => "Experimental",
        }
    }

    pub fn icon(self) -> &'static Icon {
        match self {
            OptionsTab::General => &crate::icons::SETTINGS,
            OptionsTab::Transfers => &crate::icons::OPT_TRANSFERS,
            OptionsTab::Cache => &crate::icons::OPT_CACHE,
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
    /// The Cache page: what the caches take, and clearing them.
    pub cache: crate::caches::Page,
}

pub const MAX_LOG_LINES: usize = 300;

impl OptionsView {
    pub fn open(tab: OptionsTab) -> OptionsView {
        let (log_lines, log_bytes) = openexplorer_gdrive::read_log(MAX_LOG_LINES);
        OptionsView { tab, log_filter: TextEdit::new(""), log_lines, log_bytes, cache: Default::default() }
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

    /// Settings saved before the choice of material read as Mica, and a material this build
    /// doesn't know reads as Mica too, without losing the rest of the file.
    #[test]
    fn the_material_defaults_to_mica() {
        let old: Settings = serde_json::from_str(r#"{"theme": "Dark", "mica": true}"#).unwrap();
        assert_eq!((old.theme, old.mica, old.material), (Theme::Dark, true, Material::Mica));
        let later: Settings = serde_json::from_str(r#"{"theme": "Dark", "mica": true, "material": "Tabbed"}"#).unwrap();
        assert_eq!((later.theme, later.mica, later.material), (Theme::Dark, true, Material::Mica));
        let back: Settings = serde_json::from_str(&serde_json::to_string(&Settings { material: Material::Acrylic, ..Settings::default() }).unwrap()).unwrap();
        assert_eq!(back.material, Material::Acrylic);
    }

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
        // Big files come in segments, even for settings that kept the old switch off.
        let s: Settings = serde_json::from_str(r#"{"segmented_downloads":false}"#).unwrap();
        assert!(s.segmented_downloads);
        let off: Settings = serde_json::from_str(r#"{"big_files_in_segments":false}"#).unwrap();
        assert!(!off.segmented_downloads);
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
            cache: Default::default(),
        };
        assert_eq!(view.filtered_logs(), ["10:01:02.500 Erro ao conectar SFTP nas.local"]);
        view.log_filter = TextEdit::new("");
        assert_eq!(view.filtered_logs().len(), 3);
    }
}
