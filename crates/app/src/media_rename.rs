//! Advanced media batch rename (FileBot-style) powered by TMDB v3 and TVDB v4 APIs:
//! filename parsing (series, anime, movies), online search with poster covers, episode ordering
//! (Airdate / DVD / Absolute), language selection, Opportunistic / Strict match modes, presets
//! and customizable format templates.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use openexplorer_i18n::{t, tr};
use regex::Regex;
use rune::{Image, ImageCache, TextEdit};
use serde::{Deserialize, Serialize};

use crate::model::{Entry, Location, shown};
use crate::{Msg, State, background, catch_panic, ops};

// ---------------------------------------------------------------------------------------------
// API keys, kept in the system's credential store like the accounts' credentials

const KEY_SERVICE: &str = "OpenExplorer Media";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaKeys {
    pub tmdb: String,
    pub tvdb: String,
}

/// The two keys: their name in the credential store and the environment variable that can give
/// one instead.
const KEY_SLOTS: [(&str, &str); 2] = [("tmdb", "TMDB_API_KEY"), ("tvdb", "TVDB_API_KEY")];

fn key_entry(name: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEY_SERVICE, name).map_err(|e| tr!("Cofre de credenciais indisponível: {e}", "Credential store unavailable: {e}"))
}

fn env_key(var: &str) -> String {
    std::env::var(var).map(|v| v.trim().to_string()).unwrap_or_default()
}

/// What to keep in the store for a key typed as `typed`: nothing when it is empty or is the
/// key the environment already gives, so a key from the environment is never written down.
fn key_to_store<'a>(typed: &'a str, from_env: &str) -> Option<&'a str> {
    let typed = typed.trim();
    (!typed.is_empty() && typed != from_env).then_some(typed)
}

impl MediaKeys {
    /// The file older versions kept the keys in, as plain text.
    fn old_path() -> PathBuf {
        openexplorer_gdrive::config_dir().join("media-apis.json")
    }

    pub fn load() -> Self {
        Self::move_old_file();
        let read = |(name, var): (&str, &str)| {
            let stored = key_entry(name).and_then(|e| e.get_password().map_err(|e| e.to_string())).unwrap_or_default();
            if stored.trim().is_empty() { env_key(var) } else { stored.trim().to_string() }
        };
        Self { tmdb: read(KEY_SLOTS[0]), tvdb: read(KEY_SLOTS[1]) }
    }

    /// Moves the keys of the old plain-text file into the credential store, and deletes the file
    /// once both are kept there.
    fn move_old_file() {
        let Ok(text) = std::fs::read_to_string(Self::old_path()) else { return };
        let old: Self = serde_json::from_str(&text).unwrap_or_default();
        if old.save().is_ok() {
            let _ = std::fs::remove_file(Self::old_path());
        }
    }

    pub fn save(&self) -> Result<(), String> {
        for ((name, var), typed) in KEY_SLOTS.into_iter().zip([&self.tmdb, &self.tvdb]) {
            let entry = key_entry(name)?;
            match key_to_store(typed, &env_key(var)) {
                Some(key) => entry
                    .set_password(key)
                    .map_err(|e| tr!("Não consegui guardar a chave no cofre do sistema: {e}", "Couldn't keep the key in the system's credential store: {e}"))?,
                None => {
                    let _ = entry.delete_credential();
                }
            }
        }
        Ok(())
    }

    pub fn for_source(&self, source: DataSource) -> &str {
        match source {
            DataSource::Tmdb => self.tmdb.trim(),
            DataSource::Tvdb => self.tvdb.trim(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Options: Data Source, Media Kind, Episode Order, Language, Match Mode, Presets

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DataSource {
    #[default]
    Tmdb,
    Tvdb,
}

impl DataSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tmdb => "TMDB",
            Self::Tvdb => "TVDB",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MediaKind {
    #[default]
    Series,
    Movie,
}

impl MediaKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Series => t("Série / Anime", "Series / Anime"),
            Self::Movie => t("Filme", "Movie"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EpisodeOrder {
    #[default]
    Airdate,
    Dvd,
    Absolute,
}

impl EpisodeOrder {
    pub const ALL: [Self; 3] = [Self::Airdate, Self::Dvd, Self::Absolute];

    pub fn label(self) -> &'static str {
        match self {
            Self::Airdate => t("Exibição", "Airdate"),
            Self::Dvd => "DVD",
            Self::Absolute => t("Absoluto", "Absolute"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MediaLang {
    #[default]
    PtBr,
    PtPt,
    EnUs,
    EnGb,
    EsEs,
    EsMx,
    FrFr,
    DeDe,
    ItIt,
    JaJp,
    KoKr,
    ZhCn,
    ZhTw,
    RuRu,
    PlPl,
    NlNl,
    TrTr,
    SvSe,
}

impl MediaLang {
    pub const ALL: [Self; 18] = [
        Self::PtBr,
        Self::PtPt,
        Self::EnUs,
        Self::EnGb,
        Self::EsEs,
        Self::EsMx,
        Self::FrFr,
        Self::DeDe,
        Self::ItIt,
        Self::JaJp,
        Self::KoKr,
        Self::ZhCn,
        Self::ZhTw,
        Self::RuRu,
        Self::PlPl,
        Self::NlNl,
        Self::TrTr,
        Self::SvSe,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::PtBr => "Português (Brasil)",
            Self::PtPt => "Português (Portugal)",
            Self::EnUs => "English (US)",
            Self::EnGb => "English (UK)",
            Self::EsEs => "Español (España)",
            Self::EsMx => "Español (Latinoamérica)",
            Self::FrFr => "Français",
            Self::DeDe => "Deutsch",
            Self::ItIt => "Italiano",
            Self::JaJp => "日本語 (Japanese)",
            Self::KoKr => "한국어 (Korean)",
            Self::ZhCn => "中文 (简体)",
            Self::ZhTw => "中文 (繁體)",
            Self::RuRu => "Русский",
            Self::PlPl => "Polski",
            Self::NlNl => "Nederlands",
            Self::TrTr => "Türkçe",
            Self::SvSe => "Svenska",
        }
    }

    /// Official ISO 3166-1 4x3 SVG flag bytes (from `lipis/flag-icons`).
    pub fn flag_svg(self) -> &'static [u8] {
        match self {
            Self::PtBr => include_bytes!("../assets/flags/br.svg"),
            Self::PtPt => include_bytes!("../assets/flags/pt.svg"),
            Self::EnUs => include_bytes!("../assets/flags/us.svg"),
            Self::EnGb => include_bytes!("../assets/flags/gb.svg"),
            Self::EsEs => include_bytes!("../assets/flags/es.svg"),
            Self::EsMx => include_bytes!("../assets/flags/mx.svg"),
            Self::FrFr => include_bytes!("../assets/flags/fr.svg"),
            Self::DeDe => include_bytes!("../assets/flags/de.svg"),
            Self::ItIt => include_bytes!("../assets/flags/it.svg"),
            Self::JaJp => include_bytes!("../assets/flags/jp.svg"),
            Self::KoKr => include_bytes!("../assets/flags/kr.svg"),
            Self::ZhCn => include_bytes!("../assets/flags/cn.svg"),
            Self::ZhTw => include_bytes!("../assets/flags/tw.svg"),
            Self::RuRu => include_bytes!("../assets/flags/ru.svg"),
            Self::PlPl => include_bytes!("../assets/flags/pl.svg"),
            Self::NlNl => include_bytes!("../assets/flags/nl.svg"),
            Self::TrTr => include_bytes!("../assets/flags/tr.svg"),
            Self::SvSe => include_bytes!("../assets/flags/se.svg"),
        }
    }

    pub fn tmdb_code(self) -> &'static str {
        match self {
            Self::PtBr => "pt-BR",
            Self::PtPt => "pt-PT",
            Self::EnUs => "en-US",
            Self::EnGb => "en-GB",
            Self::EsEs => "es-ES",
            Self::EsMx => "es-MX",
            Self::FrFr => "fr-FR",
            Self::DeDe => "de-DE",
            Self::ItIt => "it-IT",
            Self::JaJp => "ja-JP",
            Self::KoKr => "ko-KR",
            Self::ZhCn => "zh-CN",
            Self::ZhTw => "zh-TW",
            Self::RuRu => "ru-RU",
            Self::PlPl => "pl-PL",
            Self::NlNl => "nl-NL",
            Self::TrTr => "tr-TR",
            Self::SvSe => "sv-SE",
        }
    }

    pub fn tvdb_code(self) -> &'static str {
        match self {
            Self::PtBr | Self::PtPt => "pt",
            Self::EnUs | Self::EnGb => "eng",
            Self::EsEs | Self::EsMx => "spa",
            Self::FrFr => "fra",
            Self::DeDe => "deu",
            Self::ItIt => "ita",
            Self::JaJp => "jpn",
            Self::KoKr => "kor",
            Self::ZhCn => "zho",
            Self::ZhTw => "zhtw",
            Self::RuRu => "rus",
            Self::PlPl => "pol",
            Self::NlNl => "nld",
            Self::TrTr => "tur",
            Self::SvSe => "swe",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MatchMode {
    #[default]
    Opportunistic,
    Strict,
}

impl MatchMode {
    pub const ALL: [Self; 2] = [Self::Opportunistic, Self::Strict];

    pub fn label(self) -> &'static str {
        match self {
            Self::Opportunistic => t("Oportunista", "Opportunistic"),
            Self::Strict => t("Estrito", "Strict"),
        }
    }
}

pub struct FormatPreset {
    pub label_pt: &'static str,
    pub label_en: &'static str,
    pub template: &'static str,
}

impl FormatPreset {
    pub fn label(&self) -> &'static str {
        t(self.label_pt, self.label_en)
    }
}

pub const SERIES_PRESETS: &[FormatPreset] = &[
    FormatPreset { label_pt: "Organizar Séries (Plex)", label_en: "Organize Episodes for Plex", template: "{n} - {s00e00} - {t}" },
    FormatPreset { label_pt: "Organizar Séries (1x01)", label_en: "Organize Episodes (1x01)", template: "{n} - {sxe} - {t}" },
    FormatPreset { label_pt: "Organizar Séries (Compacto)", label_en: "Organize Episodes (Compact)", template: "{n} {s00e00}" },
    FormatPreset { label_pt: "Organizar Séries (Com Ano)", label_en: "Organize Episodes (With Year)", template: "{n} ({y}) - {s00e00} - {t}" },
];

pub const ANIME_PRESETS: &[FormatPreset] = &[
    FormatPreset { label_pt: "Anime · Número Absoluto", label_en: "Anime · Absolute Number", template: "{n} - {e00} - {t}" },
    FormatPreset { label_pt: "Anime · Absoluto Compacto", label_en: "Anime · Absolute Compact", template: "{n} - {abs}" },
    FormatPreset { label_pt: "Anime · Temporada (S01E01)", label_en: "Anime · Season (S01E01)", template: "{n} - {s00e00} - {t}" },
];

pub const MOVIE_PRESETS: &[FormatPreset] = &[
    FormatPreset { label_pt: "Organizar Filmes (Plex / Jellyfin)", label_en: "Organize Movies for Plex / Jellyfin", template: "{n} ({y})" },
    FormatPreset { label_pt: "Apenas Nome", label_en: "Title Only", template: "{n}" },
    FormatPreset { label_pt: "Ano Primeiro", label_en: "Year First", template: "[{y}] {n}" },
];

// ---------------------------------------------------------------------------------------------
// Filename Parser

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedFile {
    pub stem: String,
    pub ext: String,
    /// Empty when the name holds none (`S01E01.mkv`).
    pub title: String,
    pub kind: MediaKind,
    pub season: Option<u32>,
    pub episodes: Vec<u32>,
    pub absolute: Option<u32>,
    pub year: Option<u32>,
}

static RE_SXXEXX: LazyLock<Regex> =
    // A second episode has its own `E` (`S01E01E02`, `S01E01-E02`) or a hyphen (`S01E01-02`): a
    // bare number after a dot is something else, as the 5.1 of the sound.
    LazyLock::new(|| Regex::new(r"(?i)(?:^|[\s._\-\(\[])S(\d{1,2})[\s._\-]?E(\d{1,3})(?:[\s._\-]?E(\d{1,3})|-(\d{1,3}))?(?:$|[\s._\-\)\]])").unwrap());
static RE_NXNN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:^|[\s._\-\(\[])(\d{1,2})x(\d{1,3})(?:[\-x](\d{1,3}))?(?:$|[\s._\-\)\]])").unwrap());
static RE_EP_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:^|[\s._\-])(?:ep(?:isode|isódio)?|e)[\s._\-]?(\d{1,4})(?:v\d)?(?:$|[\s._\-\)\]])").unwrap());
static RE_ANIME_DASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s+-\s+(\d{1,4})(?:v\d)?(?:\s|$|\(|\[)").unwrap());
static RE_YEAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[\s._\-\(\[])((?:19|20)\d{2})(?:$|[\s._\-\)\]])").unwrap());
static RE_BRACKETS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[^\]]*\]|\([^)]*(?:1080p|720p|2160p|480p|x264|x265|hevc|web|bluray|aac|flac|dual|10bit)[^)]*\)").unwrap());
/// The words that only a release name has (its picture, source and codec): the title ends where
/// the first one starts.
static RE_RELEASE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[\s._\-])(?:2160p|1080p|720p|480p|web[\s._\-]?dl|webrip|bluray|bdrip|brrip|hdtv|dvdrip|remux|x264|x265|h\.?264|h\.?265|hevc|xvid)(?:$|[\s._\-\)\]]).*$").unwrap()
});
/// A word a release name adds that a title can hold too ("Dual Survival", "The Extended
/// Family"): it goes only from the end of the title, where nothing of the title follows it.
static RE_RELEASE_TAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[\s._\-]+)(?:4k|uhd|hdr10?|dv|amzn|nf|dsnp|hmax|atvp|av1|aac\d*(?:\.\d)?|ddp?\d*(?:\.\d)?|ac3|dts|atmos|truehd|flac|dual|dublado|legendado|multi|repack|proper|extended|unrated|directors[\s._\-]?cut|10bit|8bit)[\s._\-]*$")
        .unwrap()
});

/// `raw` without the release words after the title. A title made only of such words ("Dual")
/// keeps them.
fn without_release_words(raw: &str) -> String {
    let cut = RE_RELEASE.replace(raw, "").into_owned();
    let mut title = cut.as_str();
    while let Some(m) = RE_RELEASE_TAIL.find(title) {
        title = &title[..m.start()];
    }
    let named = |s: &str| s.chars().any(char::is_alphanumeric);
    if named(title) { title.to_string() } else { cut }
}

/// Whether a number after a dash reads as a year ("Blade Runner - 2049") rather than as an
/// episode: no show has that many.
fn is_year(number: &str) -> bool {
    number.len() == 4 && matches!(number.parse::<u32>(), Ok(1900..=2099))
}

/// Parses a media filename into its detected title, media kind (Series vs Movie), season/episode
/// numbers, and release year.
pub fn parse_filename(name: &str, is_dir: bool) -> ParsedFile {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && !is_dir => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.to_string(), String::new()),
    };

    let mut season: Option<u32> = None;
    let mut episodes: Vec<u32> = Vec::new();
    let mut absolute: Option<u32> = None;
    let mut cut_at: Option<usize> = None;

    if let Some(cap) = RE_SXXEXX.captures(&stem) {
        let m = cap.get(0).unwrap();
        cut_at = Some(m.start());
        season = cap.get(1).and_then(|c| c.as_str().parse().ok());
        if let Some(e1) = cap.get(2).and_then(|c| c.as_str().parse::<u32>().ok()) {
            episodes.push(e1);
            if let Some(e2) = cap.get(3).or_else(|| cap.get(4)).and_then(|c| c.as_str().parse::<u32>().ok())
                && e2 > e1
                && e2 - e1 <= 4
            {
                for e in (e1 + 1)..=e2 {
                    episodes.push(e);
                }
            }
        }
    } else if let Some(cap) = RE_NXNN.captures(&stem) {
        let m = cap.get(0).unwrap();
        cut_at = Some(m.start());
        season = cap.get(1).and_then(|c| c.as_str().parse().ok());
        if let Some(e1) = cap.get(2).and_then(|c| c.as_str().parse::<u32>().ok()) {
            episodes.push(e1);
            if let Some(e2) = cap.get(3).and_then(|c| c.as_str().parse::<u32>().ok())
                && e2 > e1
                && e2 - e1 <= 4
            {
                for e in (e1 + 1)..=e2 {
                    episodes.push(e);
                }
            }
        }
    } else if let Some(cap) = RE_ANIME_DASH.captures(&stem).filter(|cap| !is_year(&cap[1])) {
        let m = cap.get(0).unwrap();
        cut_at = Some(m.start());
        if let Some(ep) = cap.get(1).and_then(|c| c.as_str().parse::<u32>().ok()) {
            absolute = Some(ep);
            episodes.push(ep);
        }
    } else if let Some(cap) = RE_EP_WORD.captures(&stem) {
        let m = cap.get(0).unwrap();
        cut_at = Some(m.start());
        if let Some(ep) = cap.get(1).and_then(|c| c.as_str().parse::<u32>().ok()) {
            absolute = Some(ep);
            episodes.push(ep);
        }
    }

    let kind = if !episodes.is_empty() || season.is_some() { MediaKind::Series } else { MediaKind::Movie };

    let title_src = match cut_at {
        Some(pos) => &stem[..pos],
        None => stem.as_str(),
    };

    // Detect year (avoiding matching a year that is the entire title, like "1917" or "2001").
    let mut year: Option<u32> = None;
    let mut title_cut = title_src.len();
    for cap in RE_YEAR.captures_iter(title_src) {
        let m = cap.get(0).unwrap();
        if m.start() > 0 {
            if let Some(y) = cap.get(1).and_then(|c| c.as_str().parse::<u32>().ok()) {
                year = Some(y);
                title_cut = m.start();
            }
        }
    }
    if year.is_none() && cut_at.is_some() {
        // Also check after episode token just in case (rare).
        if let Some(cap) = RE_YEAR.captures(&stem[cut_at.unwrap()..]) {
            year = cap.get(1).and_then(|c| c.as_str().parse::<u32>().ok());
        }
    }

    let raw_title = &title_src[..title_cut];
    let stripped = RE_BRACKETS.replace_all(raw_title, " ");
    let stripped = without_release_words(&stripped);
    let cleaned: String = stripped
        .chars()
        .map(|c| match c {
            '.' | '_' => ' ',
            _ => c,
        })
        .collect();
    let title = cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| matches!(c, '-' | '–' | '—' | '(' | ')' | '[' | ']' | ' '))
        .to_string();

    ParsedFile {
        stem,
        ext,
        title,
        kind,
        season,
        episodes,
        absolute,
        year,
    }
}

/// Picks the best consensus title, media kind and year across a batch of parsed files.
pub fn detect_batch_query(parsed: &[ParsedFile]) -> (String, MediaKind, Option<u32>) {
    let series_count = parsed.iter().filter(|p| p.kind == MediaKind::Series).count();
    let kind = if series_count > 0 { MediaKind::Series } else { MediaKind::Movie };

    // The title most of the files have; among those tied, the first, so the same batch always
    // searches the same title. Files that name none don't count.
    let mut counts: HashMap<String, (usize, usize)> = HashMap::new();
    for (i, p) in parsed.iter().enumerate().filter(|(_, p)| !p.title.is_empty()) {
        counts.entry(p.title.to_lowercase()).or_insert((0, i)).0 += 1;
    }
    let query = counts.into_values().max_by_key(|&(count, first)| (count, std::cmp::Reverse(first))).map(|(_, first)| parsed[first].title.clone()).unwrap_or_default();

    let year = parsed.iter().find_map(|p| p.year);
    (query, kind, year)
}

// ---------------------------------------------------------------------------------------------
// API Types & Background Messages

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub source: DataSource,
    pub kind: MediaKind,
    pub title: String,
    pub original_title: Option<String>,
    pub year: Option<u32>,
    pub overview: String,
    pub poster_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpisodeInfo {
    pub season: u32,
    pub episode: u32,
    pub absolute: Option<u32>,
    pub title: String,
}

/// Messages returned from background TMDB/TVDB/poster threads into `State`.
pub enum Answer {
    Search {
        seq: u64,
        result: Result<Vec<Candidate>, String>,
    },
    Episodes {
        seq: u64,
        candidate_id: String,
        result: Result<Vec<EpisodeInfo>, String>,
    },
    Poster {
        url: String,
        bytes: Option<Vec<u8>>,
    },
}

// ---------------------------------------------------------------------------------------------
// Poster Cache (decodes remote JPEG/PNG covers asynchronously via Rune's ImageCache)

#[derive(Default)]
pub struct PosterStore {
    cache: Option<ImageCache>,
    images: HashMap<String, Option<Image>>,
    flags: HashMap<MediaLang, Image>,
}

impl PosterStore {
    pub fn start(&mut self, cache: ImageCache) {
        self.flags.clear();
        for lang in MediaLang::ALL {
            let img = cache.svg(lang.flag_svg(), 20.0, 2.0);
            self.flags.insert(lang, img);
        }
        self.cache = Some(cache);
    }

    pub fn flag(&self, lang: MediaLang) -> Option<&Image> {
        self.flags.get(&lang).filter(|img| img.is_ready() && img.error().is_none())
    }

    pub fn get(&self, url: &str) -> Option<&Image> {
        self.images.get(url).and_then(|o| o.as_ref()).filter(|img| img.is_ready() && img.error().is_none())
    }

    pub fn want(&mut self, url: &str) -> bool {
        if self.images.contains_key(url) {
            return false;
        }
        self.images.insert(url.to_string(), None);
        true
    }

    pub fn arrived(&mut self, url: String, bytes: Option<Vec<u8>>) {
        let Some(cache) = &self.cache else { return };
        if let Some(data) = bytes {
            let img = cache.load(data);
            self.images.insert(url, Some(img));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Media Rename State (embedded inside BatchRename)

/// Which dropdown is currently expanded inside `MediaPopup::Preferences`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrefDropdown {
    Datasource,
    EpisodeOrder,
    Language,
    MatchMode,
}

/// Floating submenu or modal open inside the "Mídia" tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaPopup {
    /// Submenu from the "Match" button: pick TMDB or TVDB (Series / Movie).
    MatchMenu,
    /// Submenu from the "Presets" button: pick a FileBot-style preset or edit custom format.
    PresetsMenu,
    /// Compact modal to edit the custom format template.
    EditFormat,
    /// Compact modal for Datasource, Episode Order, Language, Match Mode, and TMDB/TVDB API Keys.
    Preferences,
    /// FileBot-style Match Picker modal with poster covers, title, year and manual search.
    SelectMatch,
    /// Step-by-step analyzer modal for testing media filename detection and formatting.
    Analyze,
}

pub struct MediaState {
    pub source: DataSource,
    pub kind: MediaKind,
    pub order: EpisodeOrder,
    pub lang: MediaLang,
    pub match_mode: MatchMode,
    pub query: TextEdit,
    pub format: TextEdit,
    pub parsed: Vec<ParsedFile>,
    /// The title most of the files name, as [`norm_title`] writes it: the one the batch
    /// searches by default. A file that names another is of another show or movie.
    pub batch_title: String,
    pub detected_year: Option<u32>,
    pub candidates: Vec<Candidate>,
    pub selected: Option<usize>,
    pub episodes: Vec<EpisodeInfo>,
    /// The id of the candidate `episodes` were loaded for: the picker highlights another before
    /// its own arrive, and names must never mix one show's title with another's episodes.
    pub episodes_for: Option<String>,
    pub searching: bool,
    pub loading_episodes: bool,
    pub error: Option<String>,
    pub search_seq: u64,
    pub episodes_seq: u64,
    pub keys: MediaKeys,
    pub tmdb_key_edit: TextEdit,
    pub tvdb_key_edit: TextEdit,
    pub analyze_input: TextEdit,
    pub popup: Option<MediaPopup>,
    pub open_dropdown: Option<PrefDropdown>,
}

impl MediaState {
    pub fn new(items: &[Entry]) -> Self {
        let parsed: Vec<ParsedFile> = items.iter().map(|e| parse_filename(&shown(&e.name), e.is_dir)).collect();
        let (query_text, kind, detected_year) = detect_batch_query(&parsed);
        let default_format = match kind {
            MediaKind::Series => SERIES_PRESETS[0].template,
            MediaKind::Movie => MOVIE_PRESETS[0].template,
        };
        let keys = MediaKeys::load();
        let source = if !keys.tmdb.is_empty() || keys.tvdb.is_empty() { DataSource::Tmdb } else { DataSource::Tvdb };
        Self {
            source,
            kind,
            order: EpisodeOrder::Airdate,
            lang: MediaLang::PtBr,
            match_mode: MatchMode::Opportunistic,
            batch_title: norm_title(&query_text),
            query: TextEdit::new(query_text),
            format: TextEdit::new(default_format),
            parsed,
            detected_year,
            candidates: Vec::new(),
            selected: None,
            episodes: Vec::new(),
            episodes_for: None,
            searching: false,
            loading_episodes: false,
            error: None,
            search_seq: 0,
            episodes_seq: 0,
            tmdb_key_edit: TextEdit::new(keys.tmdb.clone()),
            tvdb_key_edit: TextEdit::new(keys.tvdb.clone()),
            analyze_input: TextEdit::new(""),
            keys,
            popup: None,
            open_dropdown: None,
        }
    }

    pub fn clear_match(&mut self) {
        self.candidates.clear();
        self.selected = None;
        self.episodes.clear();
        self.episodes_for = None;
        self.searching = false;
        self.loading_episodes = false;
        self.error = None;
    }

    pub fn analyze_sample(&self, sample: &str, loc: &Location) -> Vec<(String, String, Option<String>)> {
        let parsed = parse_filename(sample, false);
        let mut parsed_parts = vec![format!("\"{}\"", parsed.title), parsed.kind.label().to_string()];
        if let Some(s) = parsed.season {
            if let Some(&e) = parsed.episodes.first() {
                parsed_parts.push(format!("S{s:02}E{e:02}"));
            } else {
                parsed_parts.push(format!("S{s:02}"));
            }
        } else if let Some(abs) = parsed.absolute.or_else(|| parsed.episodes.first().copied()) {
            parsed_parts.push(format!("#{abs:02}"));
        }
        if let Some(y) = parsed.year {
            parsed_parts.push(format!("({y})"));
        }

        let mut steps = vec![(
            t("Detecção local", "Local parsing").into(),
            parsed_parts.join(" · "),
            None,
        )];

        match self.active_candidate() {
            None => {
                steps.push((
                    t("Correspondência", "Active match").into(),
                    String::new(),
                    Some(t("Nenhuma correspondência ativa (clique em Match)", "No active match (click Match)").into()),
                ));
                steps.push((t("Formato", "Format").into(), self.format.text.clone(), None));
                steps.push((t("Resultado", "Result").into(), sample.to_string(), None));
            }
            Some(cand) => {
                let cand_desc = match cand.year {
                    Some(y) => format!("{} · {} ({y})", cand.source.label(), cand.title),
                    None => format!("{} · {}", cand.source.label(), cand.title),
                };
                match cand.kind {
                    MediaKind::Movie => {
                        let strict_mismatch = self.match_mode == MatchMode::Strict
                            && parsed.year.zip(cand.year).is_some_and(|(fy, cy)| fy != cy);
                        if strict_mismatch {
                            steps.push((
                                t("Correspondência", "Active match").into(),
                                cand_desc,
                                Some(t("Ano diverge no modo Estrito", "Year mismatch in Strict mode").into()),
                            ));
                            steps.push((t("Formato", "Format").into(), self.format.text.clone(), None));
                            steps.push((t("Resultado", "Result").into(), sample.to_string(), None));
                        } else {
                            let formatted = render_template(&self.format.text, cand, None, &parsed);
                            let clean = sanitize_filename(&formatted, loc);
                            let out = if clean.is_empty() { sample.to_string() } else { format!("{clean}{}", parsed.ext) };
                            steps.push((t("Correspondência", "Active match").into(), cand_desc, None));
                            steps.push((t("Formato", "Format").into(), self.format.text.clone(), None));
                            steps.push((t("Resultado", "Result").into(), out, None));
                        }
                    }
                    MediaKind::Series => match self.resolve_episode(0, &parsed) {
                        Some(ep) => {
                            let ep_label = if ep.title.is_empty() {
                                format!("{cand_desc} · S{:02}E{:02}", ep.season, ep.episode)
                            } else {
                                format!("{cand_desc} · S{:02}E{:02} \"{}\"", ep.season, ep.episode, ep.title)
                            };
                            let formatted = render_template(&self.format.text, cand, Some(&ep), &parsed);
                            let clean = sanitize_filename(&formatted, loc);
                            let out = if clean.is_empty() { sample.to_string() } else { format!("{clean}{}", parsed.ext) };
                            steps.push((t("Correspondência", "Active match").into(), ep_label, None));
                            steps.push((t("Formato", "Format").into(), self.format.text.clone(), None));
                            steps.push((t("Resultado", "Result").into(), out, None));
                        }
                        None => {
                            steps.push((
                                t("Correspondência", "Active match").into(),
                                cand_desc,
                                Some(t("Episódio não encontrado para este arquivo", "Episode not found for this file").into()),
                            ));
                            steps.push((t("Formato", "Format").into(), self.format.text.clone(), None));
                            steps.push((t("Resultado", "Result").into(), sample.to_string(), None));
                        }
                    },
                }
            }
        }
        steps
    }

    pub fn active_candidate(&self) -> Option<&Candidate> {
        self.selected.and_then(|i| self.candidates.get(i))
    }

    /// The loaded episodes, if they belong to the active candidate; none otherwise.
    fn active_episodes(&self) -> &[EpisodeInfo] {
        match (self.active_candidate(), &self.episodes_for) {
            (Some(cand), Some(id)) if cand.id == *id => &self.episodes,
            _ => &[],
        }
    }

    /// Closes the open popup. Leaving the picker without confirming goes back to the candidate
    /// whose episodes are loaded, instead of keeping the one only highlighted there.
    pub fn close_popup(&mut self) {
        if self.popup == Some(MediaPopup::SelectMatch)
            && let Some(id) = &self.episodes_for
            && let Some(i) = self.candidates.iter().position(|c| c.id == *id)
        {
            self.selected = Some(i);
        }
        self.popup = None;
    }

    pub fn set_kind(&mut self, kind: MediaKind) {
        if self.kind == kind {
            return;
        }
        self.kind = kind;
        let current = self.format.text.trim();
        let is_default = SERIES_PRESETS.iter().any(|p| p.template == current)
            || ANIME_PRESETS.iter().any(|p| p.template == current)
            || MOVIE_PRESETS.iter().any(|p| p.template == current)
            || current.is_empty();
        if is_default {
            let next = match kind {
                MediaKind::Series => SERIES_PRESETS[0].template,
                MediaKind::Movie => MOVIE_PRESETS[0].template,
            };
            self.format = TextEdit::new(next);
        }
    }

    /// Computes the target filename for item `idx` using the selected match, fetched episodes,
    /// match mode, and format template. Returns `None` if no match applies to this item.
    pub fn format_item(&self, idx: usize, loc: &Location) -> Option<String> {
        let cand = self.active_candidate()?;
        let parsed = self.parsed.get(idx)?;
        if self.names_another_title(parsed) {
            return None;
        }

        match cand.kind {
            MediaKind::Movie => {
                if self.match_mode == MatchMode::Strict
                    && let (Some(fy), Some(cy)) = (parsed.year, cand.year)
                    && fy != cy
                {
                    return None;
                }
                let formatted = render_template(&self.format.text, cand, None, parsed);
                let clean = sanitize_filename(&formatted, loc);
                if clean.is_empty() {
                    return None;
                }
                Some(format!("{clean}{}", parsed.ext))
            }
            MediaKind::Series => {
                let ep = self.resolve_episode(idx, parsed)?;
                let formatted = render_template(&self.format.text, cand, Some(&ep), parsed);
                let clean = sanitize_filename(&formatted, loc);
                if clean.is_empty() {
                    return None;
                }
                Some(format!("{clean}{}", parsed.ext))
            }
        }
    }

    /// Whether `parsed` names another show or movie than the rest of the batch. One match names
    /// every file, so a file of another title would take this one's name and episodes: it is
    /// left out, to be matched in a batch of its own. A title that only adds words to the
    /// batch's, or drops some ("Show" and "Show US"), is the same; a file that names no title
    /// goes with the others.
    fn names_another_title(&self, parsed: &ParsedFile) -> bool {
        let (title, batch) = (norm_title(&parsed.title), &self.batch_title);
        let starts = |long: &str, short: &str| long.strip_prefix(short).is_some_and(|rest| rest.is_empty() || rest.starts_with(' '));
        !title.is_empty() && !batch.is_empty() && !starts(&title, batch) && !starts(batch, &title)
    }

    /// Why the match leaves item `idx` as it is, for its row to say: only when that isn't plain.
    pub fn skipped_note(&self, idx: usize) -> Option<String> {
        let parsed = self.parsed.get(idx)?;
        (self.active_candidate().is_some() && self.names_another_title(parsed)).then(|| t("Outro título: renomeie à parte", "Another title: rename it apart").into())
    }

    fn resolve_episode(&self, idx: usize, parsed: &ParsedFile) -> Option<EpisodeInfo> {
        let episodes = self.active_episodes();
        // 1. Absolute order: match by absolute number (or parsed episode).
        if self.order == EpisodeOrder::Absolute {
            let target_abs = parsed.absolute.or_else(|| parsed.episodes.first().copied());
            if let Some(num) = target_abs {
                if let Some(found) = episodes.iter().find(|e| e.absolute == Some(num) || e.episode == num) {
                    return Some(found.clone());
                }
                if self.match_mode == MatchMode::Opportunistic {
                    return Some(EpisodeInfo { season: parsed.season.unwrap_or(1), episode: num, absolute: Some(num), title: String::new() });
                }
                return None;
            }
        }

        // 2. Season + Episode match.
        if let (Some(s), Some(&e)) = (parsed.season, parsed.episodes.first()) {
            if let Some(found) = episodes.iter().find(|ep| ep.season == s && ep.episode == e) {
                return Some(found.clone());
            }
            if self.match_mode == MatchMode::Opportunistic {
                return Some(EpisodeInfo { season: s, episode: e, absolute: None, title: String::new() });
            }
            return None;
        }

        // 3. Episode-only (e.g. anime `- 05`) when in Airdate/DVD order.
        if let Some(&e) = parsed.episodes.first() {
            if let Some(found) = episodes.iter().find(|ep| ep.absolute == Some(e) || (ep.season == 1 && ep.episode == e)) {
                return Some(found.clone());
            }
            if self.match_mode == MatchMode::Opportunistic {
                return Some(EpisodeInfo { season: 1, episode: e, absolute: Some(e), title: String::new() });
            }
            return None;
        }

        // 4. Opportunistic sequential fallback, only when no file of the batch has an episode
        // number: then the files are taken in order, folders left out. Mixed with numbered
        // files, an extra or a trailer would take some episode's name.
        if self.match_mode == MatchMode::Opportunistic
            && self.parsed.iter().all(|p| p.episodes.is_empty() && p.absolute.is_none())
        {
            let nth = self.parsed[..idx].iter().filter(|p| !p.ext.is_empty()).count();
            if !parsed.ext.is_empty()
                && let Some(found) = episodes.get(nth)
            {
                return Some(found.clone());
            }
        }

        None
    }

    /// Which seasons are referenced by the selected files (used to fetch TMDB season endpoints).
    fn needed_seasons(&self) -> Vec<u32> {
        let mut set: HashSet<u32> = HashSet::new();
        for p in &self.parsed {
            set.insert(p.season.unwrap_or(1));
        }
        let mut out: Vec<u32> = set.into_iter().collect();
        out.sort_unstable();
        if out.is_empty() {
            out.push(1);
        }
        out
    }
}

/// Expands FileBot-style tokens (`{n}`, `{y}`, `{ny}`, `{s}`, `{s00}`, `{e}`, `{e00}`, `{s00e00}`,
/// `{sxe}`, `{t}`, `{abs}`) into a clean filename stem.
pub fn render_template(template: &str, cand: &Candidate, ep: Option<&EpisodeInfo>, parsed: &ParsedFile) -> String {
    let year = cand.year.map(|y| y.to_string()).unwrap_or_default();
    let ny = match cand.year {
        Some(y) => format!("{} ({y})", cand.title),
        None => cand.title.clone(),
    };

    let (s_str, s00_str, e_str, e00_str, s00e00_str, sxe_str, t_str, abs_str) = if let Some(ep) = ep {
        let s = ep.season;
        let multi: Vec<u32> = if parsed.episodes.len() > 1 && parsed.episodes[0] == ep.episode {
            parsed.episodes.clone()
        } else {
            vec![ep.episode]
        };
        let e_plain = if multi.len() > 1 {
            format!("{}-{}", multi[0], multi[multi.len() - 1])
        } else {
            format!("{}", multi[0])
        };
        let e_pad = if multi.len() > 1 {
            format!("{:02}-{:02}", multi[0], multi[multi.len() - 1])
        } else {
            format!("{:02}", multi[0])
        };
        let s00e00 = if multi.len() > 1 {
            format!("S{:02}E{:02}-E{:02}", s, multi[0], multi[multi.len() - 1])
        } else {
            format!("S{:02}E{:02}", s, multi[0])
        };
        let sxe = if multi.len() > 1 {
            format!("{s}x{:02}-{:02}", multi[0], multi[multi.len() - 1])
        } else {
            format!("{s}x{:02}", multi[0])
        };
        let abs = ep.absolute.or(parsed.absolute).map(|a| format!("{a:02}")).unwrap_or_else(|| e_pad.clone());
        (format!("{s}"), format!("{s:02}"), e_plain, e_pad, s00e00, sxe, ep.title.clone(), abs)
    } else {
        (String::new(), String::new(), String::new(), String::new(), String::new(), String::new(), String::new(), String::new())
    };

    let mut out = template.to_string();
    for (tok, val) in [
        ("{s00e00}", s00e00_str.as_str()),
        ("{sxe}", sxe_str.as_str()),
        ("{s00}", s00_str.as_str()),
        ("{e00}", e00_str.as_str()),
        ("{abs}", abs_str.as_str()),
        ("{ny}", ny.as_str()),
        ("{n}", cand.title.as_str()),
        ("{y}", year.as_str()),
        ("{s}", s_str.as_str()),
        ("{e}", e_str.as_str()),
        ("{t}", t_str.as_str()),
    ] {
        out = out.replace(tok, val);
    }

    // Clean up empty wrappers when optional tokens (like `{y}` or `{t}`) are absent.
    out = out.replace("()", "").replace("[]", "");
    let mut collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    while let Some(trimmed) = collapsed.strip_suffix(" -").or_else(|| collapsed.strip_suffix('-')) {
        collapsed = trimmed.trim().to_string();
    }
    collapsed
}

fn sanitize_filename(name: &str, loc: &Location) -> String {
    // Replace colons nicely ("Avatar: Seven Havens" -> "Avatar - Seven Havens") before sanitizing.
    let nice = name.replace(": ", " - ").replace(':', " - ");
    let cleaned = if matches!(loc, Location::Local(_) | Location::Found(_)) {
        let mut s: String = nice
            .chars()
            .filter(|c| !matches!(c, '<' | '>' | '"' | '/' | '\\' | '|' | '?' | '*') && !c.is_control())
            .collect();
        s = s.split_whitespace().collect::<Vec<_>>().join(" ");
        let trimmed = s.trim_end_matches(['.', ' ']);
        if trimmed.is_empty() {
            String::new()
        } else {
            ops::windows_name(trimmed)
        }
    } else {
        nice.replace('/', " - ").split_whitespace().collect::<Vec<_>>().join(" ")
    };
    cleaned
}

// ---------------------------------------------------------------------------------------------
// Actions triggered from the UI

/// Normalizes a title for confidence comparison (lowercase alphanumeric tokens only).
fn norm_title(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns `true` when the top search candidate is a clear, unambiguous match (so we can apply
/// it immediately without interrupting the user with a picker modal, just like FileBot).
pub fn is_confident_match(query: &str, detected_year: Option<u32>, cands: &[Candidate]) -> bool {
    let Some(first) = cands.first() else { return false };
    if cands.len() == 1 {
        return true;
    }
    let q = norm_title(query);
    if q.is_empty() {
        return false;
    }
    let exact = |c: &Candidate| {
        norm_title(&c.title) == q || c.original_title.as_deref().is_some_and(|o| norm_title(o) == q)
    };
    if !exact(first) {
        return false;
    }
    // If a year was detected in the filename, check if the top match agrees.
    if let (Some(dy), Some(cy)) = (detected_year, first.year)
        && dy != cy
    {
        return false;
    }
    // If a second candidate also has the exact same title and we have no year to disambiguate, ask the user.
    if detected_year.is_none() && cands[1..].iter().any(exact) {
        return false;
    }
    true
}

pub fn save_keys(s: &mut State) {
    let Some(br) = &mut s.batch_rename else { return };
    let m = &mut br.media;
    m.keys.tmdb = m.tmdb_key_edit.text.trim().to_string();
    m.keys.tvdb = m.tvdb_key_edit.text.trim().to_string();
    m.popup = None;
    m.error = m.keys.save().err();
}

pub fn match_with(s: &mut State, source: DataSource, kind: MediaKind) {
    if let Some(br) = &mut s.batch_rename {
        br.media.source = source;
        br.media.order = EpisodeOrder::Airdate;
        br.media.set_kind(kind);
        if kind == MediaKind::Series {
            let cur = br.media.format.text.trim();
            if ANIME_PRESETS.iter().any(|p| p.template == cur) {
                br.media.format = TextEdit::new(SERIES_PRESETS[0].template);
            }
        }
        br.media.popup = None;
    }
    start_search(s);
}

/// Matches as Anime using Absolute episode ordering and the default Anime preset (if on a preset).
pub fn match_anime(s: &mut State, source: DataSource) {
    if let Some(br) = &mut s.batch_rename {
        br.media.source = source;
        br.media.kind = MediaKind::Series;
        br.media.order = EpisodeOrder::Absolute;
        let cur = br.media.format.text.trim();
        let is_default = SERIES_PRESETS.iter().any(|p| p.template == cur)
            || MOVIE_PRESETS.iter().any(|p| p.template == cur)
            || cur.is_empty();
        if is_default {
            br.media.format = TextEdit::new(ANIME_PRESETS[0].template);
        }
        br.media.popup = None;
    }
    start_search(s);
}

/// Smart Mode -> Automatic: inspects parsed filenames to pick Series, Anime (absolute numbering)
/// or Movie automatically, then searches the active data source.
pub fn match_auto(s: &mut State) {
    let plan = s.batch_rename.as_ref().map(|br| {
        let src = br.media.source;
        let has_absolute = br.media.parsed.iter().any(|p| p.absolute.is_some() && p.season.is_none());
        let has_series = br.media.parsed.iter().any(|p| p.kind == MediaKind::Series);
        (src, has_absolute, has_series)
    });
    let Some((src, has_absolute, has_series)) = plan else { return };
    if has_absolute {
        match_anime(s, src);
    } else if has_series {
        match_with(s, src, MediaKind::Series);
    } else {
        match_with(s, src, MediaKind::Movie);
    }
}

pub fn start_search(s: &mut State) {
    let Some(br) = &mut s.batch_rename else { return };
    let m = &mut br.media;
    let query = m.query.text.trim().to_string();
    if query.is_empty() {
        m.popup = Some(MediaPopup::SelectMatch);
        m.error = Some(t("Digite o nome da série, anime ou filme para buscar.", "Type the series, anime or movie title to search.").into());
        return;
    }
    let api_key = m.keys.for_source(m.source).to_string();
    if api_key.is_empty() {
        m.popup = Some(MediaPopup::Preferences);
        m.error = Some(match m.source {
            DataSource::Tmdb => t("Configure sua API Key do TMDB nas Preferências para buscar.", "Configure your TMDB API Key in Preferences to search.").into(),
            DataSource::Tvdb => t("Configure sua API Key do TVDB nas Preferências para buscar.", "Configure your TVDB API Key in Preferences to search.").into(),
        });
        return;
    }

    m.search_seq += 1;
    let seq = m.search_seq;
    let source = m.source;
    let kind = m.kind;
    let lang = m.lang;
    let year = if m.match_mode == MatchMode::Strict { m.detected_year } else { None };
    m.searching = true;
    m.error = None;

    background(s, move |emit| {
        let result = catch_panic(t("buscar mídia", "search media"), || match source {
            DataSource::Tmdb => tmdb_search(&api_key, &query, kind, lang, year),
            DataSource::Tvdb => tvdb_search(&api_key, &query, kind, lang),
        });
        emit(Msg::Media(Answer::Search { seq, result }));
    });
}

/// Highlights candidate `idx` inside the Match Picker without closing it yet.
pub fn preview_candidate(s: &mut State, idx: usize) {
    let Some(br) = &mut s.batch_rename else { return };
    if idx < br.media.candidates.len() {
        br.media.selected = Some(idx);
    }
}

/// Confirms the currently highlighted candidate, closes the picker, and fetches episodes.
pub fn confirm_selected_candidate(s: &mut State) {
    let idx = s.batch_rename.as_ref().and_then(|b| b.media.selected);
    if let Some(i) = idx {
        select_candidate(s, i);
    }
}

pub fn select_candidate(s: &mut State, idx: usize) {
    let Some(br) = &mut s.batch_rename else { return };
    if idx >= br.media.candidates.len() {
        return;
    }
    br.media.selected = Some(idx);
    br.media.popup = None;
    br.media.episodes.clear();
    br.media.episodes_for = None;
    br.media.error = None;
    fetch_selected_episodes(s);
}

pub fn fetch_selected_episodes(s: &mut State) {
    let Some(br) = &mut s.batch_rename else { return };
    let m = &mut br.media;
    let Some(cand) = m.active_candidate().cloned() else { return };
    if cand.kind != MediaKind::Series {
        m.loading_episodes = false;
        return;
    }
    let api_key = m.keys.for_source(cand.source).to_string();
    if api_key.is_empty() {
        return;
    }
    m.episodes_seq += 1;
    let seq = m.episodes_seq;
    let order = m.order;
    let lang = m.lang;
    let seasons = m.needed_seasons();
    let cand_id = cand.id.clone();
    m.loading_episodes = true;

    background(s, move |emit| {
        let result = catch_panic(t("buscar episódios", "fetch episodes"), || match cand.source {
            DataSource::Tmdb => tmdb_episodes(&api_key, &cand.id, &seasons, order, lang),
            DataSource::Tvdb => tvdb_episodes(&api_key, &cand.id, order, lang),
        });
        emit(Msg::Media(Answer::Episodes { seq, candidate_id: cand_id, result }));
    });
}

/// Handles a background [`Answer`] delivered in `poll`.
pub fn arrived(s: &mut State, ans: Answer) {
    match ans {
        Answer::Poster { url, bytes } => {
            s.posters.arrived(url, bytes);
            s.notify_batch_rename();
        }
        Answer::Search { seq, result } => {
            let mut urls_to_fetch: Vec<String> = Vec::new();
            let mut should_fetch_eps = false;
            if let Some(br) = &mut s.batch_rename
                && br.media.search_seq == seq
            {
                br.media.searching = false;
                match result {
                    Ok(cands) => {
                        for c in cands.iter().take(10) {
                            if let Some(url) = &c.poster_url {
                                urls_to_fetch.push(url.clone());
                            }
                        }
                        br.media.candidates = cands;
                        br.media.episodes.clear();
                        br.media.episodes_for = None;
                        if br.media.candidates.is_empty() {
                            br.media.selected = None;
                            br.media.popup = Some(MediaPopup::SelectMatch);
                            br.media.error = Some(t("Nenhum resultado encontrado para esta busca.", "No results found for this search.").into());
                        } else if br.media.popup != Some(MediaPopup::SelectMatch)
                            && is_confident_match(&br.media.query.text, br.media.detected_year, &br.media.candidates)
                        {
                            // High confidence: auto-apply immediately without interrupting the user.
                            br.media.selected = Some(0);
                            br.media.error = None;
                            should_fetch_eps = br.media.candidates[0].kind == MediaKind::Series;
                        } else {
                            // Ambiguous (or user explicitly opened the picker): let them choose with posters.
                            br.media.selected = Some(0);
                            br.media.popup = Some(MediaPopup::SelectMatch);
                            br.media.error = None;
                            should_fetch_eps = br.media.candidates[0].kind == MediaKind::Series;
                        }
                    }
                    Err(e) => {
                        br.media.error = Some(e);
                    }
                }
            }
            for url in urls_to_fetch {
                if s.posters.want(&url) {
                    background(s, move |emit| {
                        let bytes = fetch_poster_bytes(&url).ok();
                        emit(Msg::Media(Answer::Poster { url, bytes }));
                    });
                }
            }
            if should_fetch_eps {
                fetch_selected_episodes(s);
            }
            s.notify_batch_rename();
        }
        Answer::Episodes { seq, candidate_id, result } => {
            if let Some(br) = &mut s.batch_rename
                && br.media.episodes_seq == seq
                && br.media.active_candidate().is_some_and(|c| c.id == candidate_id)
            {
                br.media.loading_episodes = false;
                match result {
                    Ok(eps) => {
                        br.media.episodes = eps;
                        br.media.episodes_for = Some(candidate_id);
                        br.media.error = None;
                    }
                    Err(e) => {
                        br.media.error = Some(e);
                    }
                }
            }
            s.notify_batch_rename();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// HTTP Clients: TMDB v3 & TVDB v4

/// One agent for every request: it keeps its connections open, so the requests of a search (a
/// show's seasons, one after another) don't each open one and greet the server again.
fn http_agent() -> ureq::Agent {
    static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).build().into());
    AGENT.clone()
}

fn fetch_poster_bytes(url: &str) -> Result<Vec<u8>, String> {
    let agent = http_agent();
    let mut resp = agent.get(url).call().map_err(|e| e.to_string())?;
    resp.body_mut().read_to_vec().map_err(|e| e.to_string())
}

fn tmdb_get(api_key: &str, path: &str, params: &[(&str, &str)]) -> Result<serde_json::Value, String> {
    let agent = http_agent();
    let url = format!("https://api.themoviedb.org/3{path}");
    let is_bearer = api_key.starts_with("eyJ") || api_key.len() > 40;
    let mut req = agent.get(&url).header("Accept", "application/json");
    if is_bearer {
        req = req.header("Authorization", &format!("Bearer {api_key}"));
    } else {
        req = req.query("api_key", api_key);
    }
    for (k, v) in params {
        req = req.query(k, v);
    }
    let mut resp = req.call().map_err(|e| format_http_err("TMDB", e))?;
    resp.body_mut().read_json::<serde_json::Value>().map_err(|e| tr!("Resposta inválida do TMDB: {e}", "Invalid TMDB response: {e}"))
}

fn tmdb_search(api_key: &str, query: &str, kind: MediaKind, lang: MediaLang, year: Option<u32>) -> Result<Vec<Candidate>, String> {
    let path = match kind {
        MediaKind::Series => "/search/tv",
        MediaKind::Movie => "/search/movie",
    };
    let year_str = year.map(|y| y.to_string());
    let mut params = vec![("query", query), ("language", lang.tmdb_code()), ("include_adult", "false")];
    if let Some(y) = year_str.as_deref() {
        match kind {
            MediaKind::Series => params.push(("first_air_date_year", y)),
            MediaKind::Movie => params.push(("year", y)),
        }
    }
    let json = tmdb_get(api_key, path, &params)?;
    let results = json.get("results").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for item in results.into_iter().take(12) {
        let Some(id) = item.get("id").and_then(|v| v.as_u64()) else { continue };
        let title = match kind {
            MediaKind::Series => item.get("name").and_then(|v| v.as_str()).unwrap_or_default(),
            MediaKind::Movie => item.get("title").and_then(|v| v.as_str()).unwrap_or_default(),
        }
        .to_string();
        if title.is_empty() {
            continue;
        }
        let original_title = match kind {
            MediaKind::Series => item.get("original_name").and_then(|v| v.as_str()),
            MediaKind::Movie => item.get("original_title").and_then(|v| v.as_str()),
        }
        .map(str::to_string)
        .filter(|o| o != &title);
        let date = match kind {
            MediaKind::Series => item.get("first_air_date").and_then(|v| v.as_str()),
            MediaKind::Movie => item.get("release_date").and_then(|v| v.as_str()),
        };
        let year = date.and_then(|d| d.get(..4)).and_then(|y| y.parse::<u32>().ok());
        let overview = item.get("overview").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let poster_url = item
            .get("poster_path")
            .and_then(|v| v.as_str())
            .filter(|p| !p.is_empty())
            .map(|p| format!("https://image.tmdb.org/t/p/w185{p}"));
        out.push(Candidate {
            id: id.to_string(),
            source: DataSource::Tmdb,
            kind,
            title,
            original_title,
            year,
            overview,
            poster_url,
        });
    }
    Ok(out)
}

fn tmdb_episodes(api_key: &str, id: &str, seasons: &[u32], order: EpisodeOrder, lang: MediaLang) -> Result<Vec<EpisodeInfo>, String> {
    // For DVD or Absolute order, try TMDB episode groups first (type 1 = Original air date, 2 = Absolute, 3 = DVD).
    if matches!(order, EpisodeOrder::Dvd | EpisodeOrder::Absolute) {
        let wanted_type = if order == EpisodeOrder::Absolute { 2 } else { 3 };
        if let Ok(groups_json) = tmdb_get(api_key, &format!("/tv/{id}/episode_groups"), &[])
            && let Some(groups) = groups_json.get("results").and_then(|v| v.as_array())
            && let Some(group_id) = groups
                .iter()
                .find(|g| g.get("type").and_then(|t| t.as_u64()) == Some(wanted_type))
                .and_then(|g| g.get("id").and_then(|v| v.as_str()))
            && let Ok(detail) = tmdb_get(api_key, &format!("/tv/episode_group/{group_id}"), &[("language", lang.tmdb_code())])
        {
            let mut eps = Vec::new();
            let mut abs_counter = 0u32;
            if let Some(subgroups) = detail.get("groups").and_then(|v| v.as_array()) {
                for grp in subgroups {
                    let s_num = grp.get("order").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                    if let Some(list) = grp.get("episodes").and_then(|v| v.as_array()) {
                        for (idx, ep) in list.iter().enumerate() {
                            abs_counter += 1;
                            let e_num = ep.get("order").and_then(|v| v.as_u64()).map(|n| n as u32 + 1).unwrap_or(idx as u32 + 1);
                            let title = ep.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                            eps.push(EpisodeInfo {
                                season: s_num,
                                episode: e_num,
                                absolute: Some(abs_counter),
                                title,
                            });
                        }
                    }
                }
            }
            if !eps.is_empty() {
                return Ok(eps);
            }
        }
    }

    // Standard Airdate season fetch. Absolute numbers count from the first episode of the show,
    // so absolute order reads every season (anime files rarely name one): with only season 1,
    // episode 30 of a show whose season 1 has 24 would never be found.
    let all_seasons: Vec<u32>;
    let seasons = if order == EpisodeOrder::Absolute {
        let count = tmdb_get(api_key, &format!("/tv/{id}"), &[("language", lang.tmdb_code())])?
            .get("number_of_seasons")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .clamp(1, 100) as u32;
        all_seasons = (1..=count).collect();
        &all_seasons[..]
    } else {
        seasons
    };
    let mut eps = Vec::new();
    let mut abs_counter = 0u32;
    for &season in seasons {
        let path = format!("/tv/{id}/season/{season}");
        let json = match tmdb_get(api_key, &path, &[("language", lang.tmdb_code())]) {
            Ok(j) => j,
            Err(_) if seasons.len() > 1 => continue,
            Err(e) => return Err(e),
        };
        if let Some(list) = json.get("episodes").and_then(|v| v.as_array()) {
            for ep in list {
                let s_num = ep.get("season_number").and_then(|v| v.as_u64()).unwrap_or(season as u64) as u32;
                let e_num = ep.get("episode_number").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                if e_num == 0 {
                    continue;
                }
                abs_counter += 1;
                let title = ep.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                eps.push(EpisodeInfo {
                    season: s_num,
                    episode: e_num,
                    absolute: Some(abs_counter),
                    title,
                });
            }
        }
    }
    Ok(eps)
}

// --- TVDB v4 ---

struct TvdbToken {
    apikey: String,
    token: String,
    obtained: Instant,
}

static TVDB_TOKEN: LazyLock<Mutex<Option<TvdbToken>>> = LazyLock::new(|| Mutex::new(None));

fn tvdb_token(api_key: &str) -> Result<String, String> {
    {
        let guard = TVDB_TOKEN.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.as_ref()
            && cached.apikey == api_key
            && cached.obtained.elapsed() < Duration::from_secs(60 * 60 * 12)
        {
            return Ok(cached.token.clone());
        }
    }
    let agent = http_agent();
    let body = serde_json::json!({ "apikey": api_key });
    let mut resp = agent
        .post("https://api4.thetvdb.com/v4/login")
        .header("Accept", "application/json")
        .send_json(body)
        .map_err(|e| format_http_err("TVDB", e))?;
    let json: serde_json::Value = resp.body_mut().read_json().map_err(|e| tr!("Resposta inválida do TVDB: {e}", "Invalid TVDB response: {e}"))?;
    let token = json
        .pointer("/data/token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| t("TVDB não retornou um token de acesso. Verifique sua API Key.", "TVDB did not return an access token. Check your API Key.").to_string())?
        .to_string();
    let mut guard = TVDB_TOKEN.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(TvdbToken { apikey: api_key.to_string(), token: token.clone(), obtained: Instant::now() });
    Ok(token)
}

fn tvdb_get(api_key: &str, path: &str, params: &[(&str, &str)]) -> Result<serde_json::Value, String> {
    let token = tvdb_token(api_key)?;
    let agent = http_agent();
    let url = format!("https://api4.thetvdb.com/v4{path}");
    let mut req = agent.get(&url).header("Accept", "application/json").header("Authorization", &format!("Bearer {token}"));
    for (k, v) in params {
        req = req.query(k, v);
    }
    let mut resp = req.call().map_err(|e| format_http_err("TVDB", e))?;
    resp.body_mut().read_json::<serde_json::Value>().map_err(|e| tr!("Resposta inválida do TVDB: {e}", "Invalid TVDB response: {e}"))
}

fn tvdb_search(api_key: &str, query: &str, kind: MediaKind, lang: MediaLang) -> Result<Vec<Candidate>, String> {
    let type_param = match kind {
        MediaKind::Series => "series",
        MediaKind::Movie => "movie",
    };
    let json = tvdb_get(api_key, "/search", &[("query", query), ("type", type_param)])?;
    let results = json.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let lang_key = lang.tvdb_code();
    let mut out = Vec::new();
    for item in results.into_iter().take(12) {
        let id = item
            .get("tvdb_id")
            .and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_u64().map(|n| n.to_string())))
            .or_else(|| item.get("id").and_then(|v| v.as_str()).map(|s| s.rsplit('-').next().unwrap_or(s).to_string()));
        let Some(id) = id else { continue };

        let translated = item.get("translations").and_then(|tr| tr.get(lang_key)).and_then(|v| v.as_str());
        let base_name = item.get("name").and_then(|v| v.as_str()).unwrap_or_default();
        let title = translated.filter(|s| !s.is_empty()).unwrap_or(base_name).to_string();
        if title.is_empty() {
            continue;
        }
        let year = item
            .get("year")
            .and_then(|v| v.as_str().and_then(|s| s.parse::<u32>().ok()).or_else(|| v.as_u64().map(|n| n as u32)));
        let overview = item
            .get("overviews")
            .and_then(|ov| ov.get(lang_key))
            .and_then(|v| v.as_str())
            .or_else(|| item.get("overview").and_then(|v| v.as_str()))
            .unwrap_or_default()
            .to_string();
        let poster_url = item
            .get("image_url")
            .or_else(|| item.get("thumbnail"))
            .and_then(|v| v.as_str())
            .filter(|u| u.starts_with("http"))
            .map(str::to_string);
        let original_title = (base_name != title && !base_name.is_empty()).then(|| base_name.to_string());
        out.push(Candidate {
            id,
            source: DataSource::Tvdb,
            kind,
            title,
            original_title,
            year,
            overview,
            poster_url,
        });
    }
    Ok(out)
}

fn tvdb_episodes(api_key: &str, id: &str, order: EpisodeOrder, lang: MediaLang) -> Result<Vec<EpisodeInfo>, String> {
    let order_str = match order {
        EpisodeOrder::Airdate => "default",
        EpisodeOrder::Dvd => "dvd",
        EpisodeOrder::Absolute => "absolute",
    };
    let lang_code = lang.tvdb_code();
    // TVDB pages long series (`links.next`): read every page, not just the first.
    let mut list: Vec<serde_json::Value> = Vec::new();
    let mut translated = lang != MediaLang::EnUs;
    for page in 0..100u32 {
        let page_str = page.to_string();
        let params = [("page", page_str.as_str())];
        let json = if translated {
            match tvdb_get(api_key, &format!("/series/{id}/episodes/{order_str}/{lang_code}"), &params) {
                Ok(json) => json,
                // No translation: the rest in the original language, from this page on.
                Err(_) => {
                    translated = false;
                    tvdb_get(api_key, &format!("/series/{id}/episodes/{order_str}"), &params)?
                }
            }
        } else {
            tvdb_get(api_key, &format!("/series/{id}/episodes/{order_str}"), &params)?
        };
        let episodes = json.pointer("/data/episodes").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let more = json.pointer("/links/next").is_some_and(|v| !v.is_null()) && !episodes.is_empty();
        list.extend(episodes);
        if !more {
            break;
        }
    }

    let mut eps = Vec::new();
    for (idx, ep) in list.into_iter().enumerate() {
        let season = ep.get("seasonNumber").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
        let episode = ep.get("number").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        if episode == 0 {
            continue;
        }
        let absolute = ep.get("absoluteNumber").and_then(|v| v.as_u64()).map(|n| n as u32).or(Some(idx as u32 + 1));
        let title = ep.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        eps.push(EpisodeInfo { season, episode, absolute, title });
    }
    Ok(eps)
}

fn format_http_err(provider: &str, err: ureq::Error) -> String {
    match err {
        ureq::Error::StatusCode(401) => tr!(
            "{provider}: API Key inválida ou não autorizada (401).",
            "{provider}: Invalid or unauthorized API Key (401)."
        ),
        ureq::Error::StatusCode(404) => tr!("{provider}: Recurso não encontrado (404).", "{provider}: Resource not found (404)."),
        ureq::Error::StatusCode(429) => tr!("{provider}: Limite de requisições atingido (429). Tente novamente em alguns segundos.", "{provider}: Rate limit exceeded (429). Try again in a few seconds."),
        other => tr!("Erro ao consultar {provider}: {other}", "Error querying {provider}: {other}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Unit Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_from_the_environment_is_never_stored() {
        assert_eq!(key_to_store("  abc  ", ""), Some("abc"));
        assert_eq!(key_to_store("abc", "abc"), None);
        assert_eq!(key_to_store("xyz", "abc"), Some("xyz"));
        assert_eq!(key_to_store("   ", "abc"), None);
    }

    #[test]
    fn parses_standard_tv_scene_filenames() {
        let p = parse_filename("Avatar.Seven.Havens.S01E05.1080p.AMZN.WEB-DL.DDP5.1.H.264.DUAL-BiOMA.mkv", false);
        assert_eq!(p.kind, MediaKind::Series);
        assert_eq!(p.title, "Avatar Seven Havens");
        assert_eq!(p.season, Some(1));
        assert_eq!(p.episodes, vec![5]);
        assert_eq!(p.ext, ".mkv");
    }

    #[test]
    fn parses_multi_episode_and_nxnn_formats() {
        let p = parse_filename("Breaking.Bad.S02E03E04.720p.BluRay.mkv", false);
        assert_eq!(p.title, "Breaking Bad");
        assert_eq!(p.season, Some(2));
        assert_eq!(p.episodes, vec![3, 4]);

        let p2 = parse_filename("The Office 3x12 - Traveling Salesmen.mp4", false);
        assert_eq!(p2.title, "The Office");
        assert_eq!(p2.season, Some(3));
        assert_eq!(p2.episodes, vec![12]);
    }

    #[test]
    fn parses_anime_absolute_and_movies() {
        let a = parse_filename("[SubsPlease] Solo Leveling - 08 (1080p) [A1B2C3].mkv", false);
        assert_eq!(a.kind, MediaKind::Series);
        assert_eq!(a.title, "Solo Leveling");
        assert_eq!(a.absolute, Some(8));
        assert_eq!(a.episodes, vec![8]);

        let m = parse_filename("Inception.2010.1080p.BluRay.x264.mkv", false);
        assert_eq!(m.kind, MediaKind::Movie);
        assert_eq!(m.title, "Inception");
        assert_eq!(m.year, Some(2010));
    }

    #[test]
    fn renders_filebot_templates_and_sanitizes_windows_names() {
        let cand = Candidate {
            id: "1".into(),
            source: DataSource::Tmdb,
            kind: MediaKind::Series,
            title: "Avatar: Seven Havens".into(),
            original_title: None,
            year: Some(2026),
            overview: String::new(),
            poster_url: None,
        };
        let ep = EpisodeInfo { season: 1, episode: 1, absolute: Some(1), title: "A New Era?".into() };
        let parsed = parse_filename("Avatar.Seven.Havens.S01E01.mkv", false);
        let stem = render_template("{n} - {s00e00} - {t}", &cand, Some(&ep), &parsed);
        let safe = sanitize_filename(&stem, &Location::Local(PathBuf::from("C:/Videos")));
        assert_eq!(safe, "Avatar - Seven Havens - S01E01 - A New Era");

        // Movie template
        let movie = Candidate {
            id: "2".into(),
            source: DataSource::Tmdb,
            kind: MediaKind::Movie,
            title: "Dune: Part Two".into(),
            original_title: None,
            year: Some(2024),
            overview: String::new(),
            poster_url: None,
        };
        let mp = parse_filename("Dune.Part.Two.2024.2160p.mkv", false);
        let mstem = render_template("{n} ({y})", &movie, None, &mp);
        let msafe = sanitize_filename(&mstem, &Location::Local(PathBuf::from("C:/Movies")));
        assert_eq!(msafe, "Dune - Part Two (2024)");
    }

    #[test]
    fn a_title_word_that_looks_like_a_release_word_stays() {
        for (name, title) in [
            ("Property.Brothers.S01E01.720p.mkv", "Property Brothers"),
            ("Flack.S02E03.1080p.WEB.mkv", "Flack"),
            ("Dual.Survival.S01E01.mkv", "Dual Survival"),
            ("The.Extended.Family.S01E02.mkv", "The Extended Family"),
            ("Multiverse.of.Madness.2022.1080p.mkv", "Multiverse of Madness"),
            ("Atmosphere.2019.mkv", "Atmosphere"),
            ("Dual.2022.1080p.WEB-DL.mkv", "Dual"),
            // Release words still end the title, with or without a year before them.
            ("Breaking.Bad.S01E01.Pilot.1080p.BluRay.x264.mkv", "Breaking Bad"),
            ("Some.Movie.Extended.1080p.BluRay.x264-GRP.mkv", "Some Movie"),
            ("Some.Movie.NF.WEB-DL.DDP5.1.Atmos.mkv", "Some Movie"),
            ("Some.Movie.DUAL.mkv", "Some Movie"),
        ] {
            assert_eq!(parse_filename(name, false).title, title, "{name}");
        }
    }

    #[test]
    fn a_year_after_a_dash_is_not_an_episode() {
        for name in ["Blade Runner - 2049.mkv", "Movie Name - 2019.mkv"] {
            let p = parse_filename(name, false);
            assert_eq!((p.kind, p.absolute, p.episodes.as_slice()), (MediaKind::Movie, None, &[][..]), "{name}");
        }
        assert_eq!(parse_filename("Blade Runner - 2049.mkv", false).title, "Blade Runner");
        assert_eq!(parse_filename("Movie Name - 2019.mkv", false).year, Some(2019));
        // A long anime's episode past 1,000 still reads as one.
        assert_eq!(parse_filename("[Sub] One Piece - 1071 (1080p).mkv", false).absolute, Some(1071));
    }

    #[test]
    fn a_number_after_the_episode_is_no_second_episode() {
        // The 5.1 of the sound.
        assert_eq!(parse_filename("Show.S01E02.5.1.mkv", false).episodes, [2]);
        assert_eq!(parse_filename("Show.S01E02.720p.mkv", false).episodes, [2]);
        for name in ["Show.S01E02E03.mkv", "Show.S01E02-E03.mkv", "Show.S01E02.E03.mkv", "Show.S01E02-03.mkv"] {
            assert_eq!(parse_filename(name, false).episodes, [2, 3], "{name}");
        }
    }

    #[test]
    fn the_batch_title_is_the_most_common_then_the_first() {
        let parsed = |names: &[&str]| names.iter().map(|n| parse_filename(n, false)).collect::<Vec<_>>();
        assert_eq!(detect_batch_query(&parsed(&["Beta.S01E01.mkv", "Alpha.S01E01.mkv", "Alpha.S01E02.mkv"])).0, "Alpha");
        // A tie: the first title, the same at every run.
        for _ in 0..20 {
            assert_eq!(detect_batch_query(&parsed(&["Gamma.S01E01.mkv", "Beta.S01E01.mkv", "Alpha.S01E01.mkv"])).0, "Gamma");
        }
        // Files that name no title leave the search to the user.
        assert_eq!(detect_batch_query(&parsed(&["S01E01.mkv", "S01E02.mkv"])).0, "");
    }
}
