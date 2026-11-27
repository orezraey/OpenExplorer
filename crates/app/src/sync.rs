//! Folder synchronization: configure 2 or more folder pairs (local or cloud/server),
//! choose a sync preset or custom rules, filter by name/date/size/subfolders, compare
//! differences with optional MD5 hash verification, and execute the sync plan.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Datelike, NaiveDate, TimeZone, Utc};
use md5::{Digest, Md5};
use openexplorer_gdrive::{Drive, DriveFile};
use openexplorer_i18n::{t, tr};
use openexplorer_remote::{Pool, Server};
use rune::TextEdit;
use serde::{Deserialize, Serialize};

use crate::model::{Entry, Location, Space};
use crate::servers;
use crate::{AccountRef, Msg, State, background, catch_panic};

/// Tolerance when comparing modification timestamps across filesystems / cloud providers.
const TIME_TOLERANCE: Duration = Duration::from_secs(2);

/// Maximum directory recursion depth to guard against symlink cycles.
const MAX_DEPTH: usize = 64;

// ---------------------------------------------------------------------------------------------
// Sync modes, rules, and filters

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMode {
    #[default]
    Mirror,
    MirrorUpdate,
    Update,
    Bidirectional,
    Custom,
}

impl SyncMode {
    pub const ALL: [SyncMode; 5] = [
        SyncMode::Mirror,
        SyncMode::MirrorUpdate,
        SyncMode::Update,
        SyncMode::Bidirectional,
        SyncMode::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SyncMode::Mirror => t("Espelho", "Mirror"),
            SyncMode::MirrorUpdate => t("Atualização espelho", "Mirror update"),
            SyncMode::Update => t("Atualizar", "Update"),
            SyncMode::Bidirectional => t("Bidirecional", "Bidirectional"),
            SyncMode::Custom => t("Personalizado", "Custom"),
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            SyncMode::Mirror => t("Replicar lado esquerdo no lado direito", "Replicate left side to right side"),
            SyncMode::MirrorUpdate => t(
                "Replicar lado esquerdo no direito sem sobrescrever arquivos mais novos na direita",
                "Replicate left side to right without overwriting newer files on the right",
            ),
            SyncMode::Update => t(
                "Copiar novos e mais recentes da esquerda para a direita sem excluir na direita",
                "Copy new and newer files from left to right without deleting on the right",
            ),
            SyncMode::Bidirectional => t(
                "Manter ambos os lados sincronizados com os arquivos mais recentes",
                "Keep both sides synchronized with the newest files",
            ),
            SyncMode::Custom => t(
                "Definir manualmente a ação para cada condição de comparação",
                "Manually choose the action for each comparison condition",
            ),
        }
    }

    pub fn preset_rules(self) -> Option<SyncRules> {
        Some(match self {
            SyncMode::Mirror => SyncRules {
                left_only: LeftOnlyAction::CopyToRight,
                right_only: RightOnlyAction::DeleteRight,
                left_newer: NewerAction::CopyToRight,
                right_newer: NewerAction::CopyToRight,
            },
            SyncMode::MirrorUpdate => SyncRules {
                left_only: LeftOnlyAction::CopyToRight,
                right_only: RightOnlyAction::DeleteRight,
                left_newer: NewerAction::CopyToRight,
                right_newer: NewerAction::DoNothing,
            },
            SyncMode::Update => SyncRules {
                left_only: LeftOnlyAction::CopyToRight,
                right_only: RightOnlyAction::DoNothing,
                left_newer: NewerAction::CopyToRight,
                right_newer: NewerAction::DoNothing,
            },
            SyncMode::Bidirectional => SyncRules {
                left_only: LeftOnlyAction::CopyToRight,
                right_only: RightOnlyAction::CopyToLeft,
                left_newer: NewerAction::CopyToRight,
                right_newer: NewerAction::CopyToLeft,
            },
            SyncMode::Custom => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeftOnlyAction {
    #[default]
    CopyToRight,
    DeleteLeft,
    DoNothing,
}

impl LeftOnlyAction {
    pub const ALL: [LeftOnlyAction; 3] = [LeftOnlyAction::CopyToRight, LeftOnlyAction::DoNothing, LeftOnlyAction::DeleteLeft];

    pub fn label(self) -> &'static str {
        match self {
            LeftOnlyAction::CopyToRight => t("Copiar para a direita", "Copy to right"),
            LeftOnlyAction::DeleteLeft => t("Excluir da esquerda", "Delete from left"),
            LeftOnlyAction::DoNothing => t("Não fazer nada", "Do nothing"),
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.label() == s)
    }

    pub fn to_sync_action(self) -> SyncAction {
        match self {
            LeftOnlyAction::CopyToRight => SyncAction::CopyRight,
            LeftOnlyAction::DeleteLeft => SyncAction::DeleteLeft,
            LeftOnlyAction::DoNothing => SyncAction::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RightOnlyAction {
    CopyToLeft,
    #[default]
    DeleteRight,
    DoNothing,
}

impl RightOnlyAction {
    pub const ALL: [RightOnlyAction; 3] = [RightOnlyAction::CopyToLeft, RightOnlyAction::DoNothing, RightOnlyAction::DeleteRight];

    pub fn label(self) -> &'static str {
        match self {
            RightOnlyAction::CopyToLeft => t("Copiar para a esquerda", "Copy to left"),
            RightOnlyAction::DeleteRight => t("Excluir da direita", "Delete from right"),
            RightOnlyAction::DoNothing => t("Não fazer nada", "Do nothing"),
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.label() == s)
    }

    pub fn to_sync_action(self) -> SyncAction {
        match self {
            RightOnlyAction::CopyToLeft => SyncAction::CopyLeft,
            RightOnlyAction::DeleteRight => SyncAction::DeleteRight,
            RightOnlyAction::DoNothing => SyncAction::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NewerAction {
    #[default]
    CopyToRight,
    CopyToLeft,
    DoNothing,
}

impl NewerAction {
    pub const ALL: [NewerAction; 3] = [NewerAction::CopyToRight, NewerAction::DoNothing, NewerAction::CopyToLeft];
    /// The same choices in the order they are offered for a file newer on the right side.
    pub const FOR_RIGHT: [NewerAction; 3] = [NewerAction::DoNothing, NewerAction::CopyToRight, NewerAction::CopyToLeft];

    pub fn label(self) -> &'static str {
        match self {
            NewerAction::CopyToRight => t("Copiar para a direita", "Copy to right"),
            NewerAction::CopyToLeft => t("Copiar para a esquerda", "Copy to left"),
            NewerAction::DoNothing => t("Não fazer nada", "Do nothing"),
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.label() == s)
    }

    pub fn to_sync_action(self) -> SyncAction {
        match self {
            NewerAction::CopyToRight => SyncAction::CopyRight,
            NewerAction::CopyToLeft => SyncAction::CopyLeft,
            NewerAction::DoNothing => SyncAction::None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRules {
    pub left_only: LeftOnlyAction,
    pub right_only: RightOnlyAction,
    pub left_newer: NewerAction,
    pub right_newer: NewerAction,
}

impl Default for SyncRules {
    fn default() -> Self {
        SyncMode::Mirror.preset_rules().unwrap()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DateFilterMode {
    #[default]
    Period,
    Date,
}

impl DateFilterMode {
    pub const ALL: [DateFilterMode; 2] = [DateFilterMode::Period, DateFilterMode::Date];

    pub fn label(self) -> &'static str {
        match self {
            DateFilterMode::Period => t("Período", "Period"),
            DateFilterMode::Date => t("Data", "Date"),
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.label() == s)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DateUnit {
    Hours,
    #[default]
    Days,
    Weeks,
    Months,
    Years,
}

impl DateUnit {
    pub const ALL: [DateUnit; 5] = [DateUnit::Hours, DateUnit::Days, DateUnit::Weeks, DateUnit::Months, DateUnit::Years];

    pub fn label(self) -> &'static str {
        match self {
            DateUnit::Hours => t("Hora(s)", "Hour(s)"),
            DateUnit::Days => t("Dia(s)", "Day(s)"),
            DateUnit::Weeks => t("Semana(s)", "Week(s)"),
            DateUnit::Months => t("Mês(es)", "Month(s)"),
            DateUnit::Years => t("Ano(s)", "Year(s)"),
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|u| u.label() == s)
    }

    pub fn to_duration(self, amount: u64) -> Duration {
        let secs = match self {
            DateUnit::Hours => amount.saturating_mul(3_600),
            DateUnit::Days => amount.saturating_mul(86_400),
            DateUnit::Weeks => amount.saturating_mul(7 * 86_400),
            DateUnit::Months => amount.saturating_mul(30 * 86_400),
            DateUnit::Years => amount.saturating_mul(365 * 86_400),
        };
        Duration::from_secs(secs)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizeUnit {
    Bytes,
    KB,
    #[default]
    MB,
    GB,
    TB,
}

impl SizeUnit {
    pub const ALL: [SizeUnit; 5] = [SizeUnit::Bytes, SizeUnit::KB, SizeUnit::MB, SizeUnit::GB, SizeUnit::TB];

    pub fn label(self) -> &'static str {
        match self {
            SizeUnit::Bytes => "B",
            SizeUnit::KB => "KB",
            SizeUnit::MB => "MB",
            SizeUnit::GB => "GB",
            SizeUnit::TB => "TB",
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|u| u.label() == s)
    }

    pub fn to_bytes(self, amount: u64) -> u64 {
        let mult: u64 = match self {
            SizeUnit::Bytes => 1,
            SizeUnit::KB => 1024,
            SizeUnit::MB => 1024 * 1024,
            SizeUnit::GB => 1024 * 1024 * 1024,
            SizeUnit::TB => 1024 * 1024 * 1024 * 1024,
        };
        amount.saturating_mul(mult)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateRule {
    pub enabled: bool,
    pub mode: DateFilterMode,
    pub amount: u64,
    pub unit: DateUnit,
    /// ISO date string (`YYYY-MM-DD`) when `mode == DateFilterMode::Date`.
    pub date: String,
}

impl Default for DateRule {
    fn default() -> Self {
        let today = Utc::now().date_naive();
        Self {
            enabled: false,
            mode: DateFilterMode::Period,
            amount: 7,
            unit: DateUnit::Days,
            date: format!("{:04}-{:02}-{:02}", today.year(), today.month(), today.day()),
        }
    }
}

impl DateRule {
    /// Resolves the cutoff `SystemTime` relative to `now`.
    pub fn cutoff(&self, now: SystemTime) -> Option<SystemTime> {
        match self.mode {
            DateFilterMode::Period => now.checked_sub(self.unit.to_duration(self.amount)),
            DateFilterMode::Date => {
                let d = NaiveDate::parse_from_str(self.date.trim(), "%Y-%m-%d").ok()?;
                let dt = Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?);
                let secs = dt.timestamp();
                if secs >= 0 {
                    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
                } else {
                    None
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeRule {
    pub enabled: bool,
    pub amount: u64,
    pub unit: SizeUnit,
}

impl SizeRule {
    pub fn bytes(&self) -> u64 {
        self.unit.to_bytes(self.amount)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncFilter {
    /// One glob pattern per line to exclude.
    pub exclude_patterns: String,
    /// One glob pattern per line to include (when non-empty, only matching files are included).
    pub include_patterns: String,
    /// Exclude files older than period/date.
    pub exclude_older: DateRule,
    /// Exclude files newer than period/date.
    pub exclude_newer: DateRule,
    /// Exclude files smaller than size.
    pub exclude_smaller: SizeRule,
    /// Exclude files larger than size.
    pub exclude_larger: SizeRule,
    /// Do not synchronize subfolders, only the root folder.
    pub root_only: bool,
    /// Do not read and compare folders that are already excluded by filter.
    pub skip_filtered_dirs: bool,
}

impl Default for SyncFilter {
    fn default() -> Self {
        Self {
            exclude_patterns: String::new(),
            include_patterns: String::new(),
            exclude_older: DateRule::default(),
            exclude_newer: DateRule::default(),
            exclude_smaller: SizeRule { enabled: false, amount: 100, unit: SizeUnit::MB },
            exclude_larger: SizeRule { enabled: false, amount: 500, unit: SizeUnit::MB },
            root_only: false,
            skip_filtered_dirs: false,
        }
    }
}

impl SyncFilter {
    /// Counts how many filter clauses are active, for badge display on the `Filtro...` button.
    pub fn active_count(&self) -> usize {
        let mut n = 0;
        if parse_patterns(&self.exclude_patterns).next().is_some() {
            n += 1;
        }
        if parse_patterns(&self.include_patterns).next().is_some() {
            n += 1;
        }
        if self.exclude_older.enabled {
            n += 1;
        }
        if self.exclude_newer.enabled {
            n += 1;
        }
        if self.exclude_smaller.enabled {
            n += 1;
        }
        if self.exclude_larger.enabled {
            n += 1;
        }
        if self.root_only {
            n += 1;
        }
        if self.skip_filtered_dirs {
            n += 1;
        }
        n
    }

    /// Returns true if a directory at relative path `rel_path` (slash-separated, no leading `/`)
    /// is excluded by `exclude_patterns`.
    pub fn is_dir_excluded(&self, rel_path: &str) -> bool {
        let rel = rel_path.trim_matches('/');
        if rel.is_empty() {
            return false;
        }
        for pat in parse_patterns(&self.exclude_patterns) {
            if pattern_matches_dir(pat, rel) {
                return true;
            }
        }
        false
    }

    /// Returns true if a file with relative path `rel_path`, `size`, and `modified` passes all filters.
    pub fn allows_file(&self, rel_path: &str, size: Option<u64>, modified: Option<SystemTime>, now: SystemTime) -> bool {
        let rel = rel_path.trim_matches('/');

        // 1. Exclude patterns
        for pat in parse_patterns(&self.exclude_patterns) {
            if pattern_matches_file(pat, rel) {
                return false;
            }
        }

        // 2. Include patterns (if any are provided, file must match at least one)
        let mut include = parse_patterns(&self.include_patterns).peekable();
        if include.peek().is_some() {
            let matched = include.any(|pat| pattern_matches_file(pat, rel));
            if !matched {
                return false;
            }
        }

        // 3. Date filters
        if self.exclude_older.enabled
            && let (Some(m), Some(cutoff)) = (modified, self.exclude_older.cutoff(now))
            && m < cutoff
        {
            return false;
        }
        if self.exclude_newer.enabled
            && let (Some(m), Some(cutoff)) = (modified, self.exclude_newer.cutoff(now))
            && m > cutoff
        {
            return false;
        }

        // 4. Size filters
        if self.exclude_smaller.enabled
            && let Some(sz) = size
            && sz < self.exclude_smaller.bytes()
        {
            return false;
        }
        if self.exclude_larger.enabled
            && let Some(sz) = size
            && sz > self.exclude_larger.bytes()
        {
            return false;
        }

        true
    }
}

fn parse_patterns(raw: &str) -> impl Iterator<Item = &str> {
    raw.lines().map(str::trim).filter(|l| !l.is_empty())
}

/// Glob matcher (`*` matches any sequence of chars, `?` matches one char), case-insensitive.
fn glob_match(pattern: &str, text: &str) -> bool {
    // Plain ASCII, as most names and nearly all patterns are, is matched in place: a filter is
    // asked about every file of both sides, and the copies below were most of what it cost.
    if pattern.is_ascii() && text.is_ascii() {
        return glob(pattern.as_bytes(), text.as_bytes(), b'?', b'*', u8::eq_ignore_ascii_case);
    }
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    glob(&p, &t, '?', '*', char::eq)
}

/// [`glob_match`] over letters `same` tells alike: `one` stands for any one, `any` for any run.
fn glob<T: Copy + PartialEq>(p: &[T], t: &[T], one: T, any: T, same: impl Fn(&T, &T) -> bool) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == one || same(&p[pi], &t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == any {
            star_p = Some(pi);
            pi += 1;
            star_t = ti;
        } else if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == any {
        pi += 1;
    }
    pi == p.len()
}

/// A pattern with `/` between its folders, however it was typed.
fn slashes(raw_pat: &str) -> std::borrow::Cow<'_, str> {
    if raw_pat.contains('\\') { raw_pat.replace('\\', "/").into() } else { raw_pat.into() }
}

fn pattern_matches_dir(raw_pat: &str, rel_dir: &str) -> bool {
    let pat = slashes(raw_pat);
    let anchored = pat.starts_with('/');
    let pat_clean = pat.trim_matches('/');
    if pat_clean.is_empty() {
        return false;
    }
    if anchored {
        return glob_match(pat_clean, rel_dir);
    }
    if pat_clean.contains('/') {
        // Match full relative path or any suffix path
        if glob_match(pat_clean, rel_dir) {
            return true;
        }
        // Check if pattern matches a trailing segment sequence
        for (idx, ch) in rel_dir.char_indices() {
            if ch == '/' && glob_match(pat_clean, &rel_dir[idx + 1..]) {
                return true;
            }
        }
        false
    } else {
        // Bare folder name pattern like `node_modules` or `temp*` matches any path component
        rel_dir.split('/').any(|seg| glob_match(pat_clean, seg))
    }
}

fn pattern_matches_file(raw_pat: &str, rel_file: &str) -> bool {
    let pat = slashes(raw_pat);
    let dir_only = pat.ends_with('/');
    let anchored = pat.starts_with('/');
    let pat_clean = pat.trim_matches('/');
    if pat_clean.is_empty() {
        return false;
    }

    // If pattern ends with `/`, it targets a directory and everything inside it.
    if dir_only {
        if let Some((parent, _)) = rel_file.rsplit_once('/') {
            return pattern_matches_dir(&pat, parent);
        }
        return false;
    }

    if anchored {
        return glob_match(pat_clean, rel_file);
    }

    if pat_clean.contains('/') {
        if glob_match(pat_clean, rel_file) {
            return true;
        }
        for (idx, ch) in rel_file.char_indices() {
            if ch == '/' && glob_match(pat_clean, &rel_file[idx + 1..]) {
                return true;
            }
        }
        false
    } else {
        // Filename pattern like `*.ini` or `thumbs.db`
        let file_name = rel_file.rsplit('/').next().unwrap_or(rel_file);
        if glob_match(pat_clean, file_name) {
            return true;
        }
        // Also match if any parent directory matches bare folder name (e.g. `node_modules` or `.git`)
        if let Some((parent, _)) = rel_file.rsplit_once('/') {
            return parent.split('/').any(|seg| glob_match(pat_clean, seg));
        }
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOptions {
    pub mode: SyncMode,
    pub rules: SyncRules,
    pub filter: SyncFilter,
    pub compare_hash: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            mode: SyncMode::Mirror,
            rules: SyncRules::default(),
            filter: SyncFilter::default(),
            compare_hash: false,
        }
    }
}

impl SyncOptions {
    pub fn effective_rules(&self) -> SyncRules {
        self.mode.preset_rules().unwrap_or(self.rules)
    }
}

// ---------------------------------------------------------------------------------------------
// Serializable locations & folder pairs

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncLocation {
    Local(PathBuf),
    Drive {
        email: String,
        shared_drive_id: Option<String>,
        shared_drive_name: Option<String>,
        /// (id, name) segments from root down.
        path: Vec<(String, String)>,
    },
    Server {
        server_id: String,
        server_name: String,
        path: Vec<String>,
    },
}

impl SyncLocation {
    pub fn from_location(loc: &Location) -> Option<Self> {
        match loc {
            Location::Local(p) => Some(SyncLocation::Local(p.clone())),
            Location::Drive { email, space, path } => match space {
                Space::MyDrive => Some(SyncLocation::Drive {
                    email: email.clone(),
                    shared_drive_id: None,
                    shared_drive_name: None,
                    path: path.clone(),
                }),
                Space::Shared { id, name } => Some(SyncLocation::Drive {
                    email: email.clone(),
                    shared_drive_id: Some(id.clone()),
                    shared_drive_name: Some(name.clone()),
                    path: path.clone(),
                }),
                _ => None,
            },
            Location::Server { server, path, find: None } => Some(SyncLocation::Server {
                server_id: server.id.clone(),
                server_name: server.name.clone(),
                path: path.clone(),
            }),
            _ => None,
        }
    }

    pub fn display_label(&self) -> String {
        match self {
            SyncLocation::Local(p) => p.display().to_string(),
            SyncLocation::Drive { email, shared_drive_name, path, .. } => {
                let root = shared_drive_name.as_deref().unwrap_or(t("Meu Drive", "My Drive"));
                if path.is_empty() {
                    format!("Google Drive ({email}) / {root}")
                } else {
                    let segs: Vec<&str> = path.iter().map(|(_, n)| n.as_str()).collect();
                    format!("Google Drive ({email}) / {root}/{}", segs.join("/"))
                }
            }
            SyncLocation::Server { server_name, path, .. } => {
                if path.is_empty() {
                    format!("{server_name} /")
                } else {
                    format!("{server_name} /{}", path.join("/"))
                }
            }
        }
    }

    pub fn short_label(&self) -> String {
        match self {
            SyncLocation::Local(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| p.display().to_string()),
            SyncLocation::Drive { shared_drive_name, path, .. } => path
                .last()
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| shared_drive_name.clone().unwrap_or_else(|| t("Meu Drive", "My Drive").into())),
            SyncLocation::Server { server_name, path, .. } => path.last().cloned().unwrap_or_else(|| server_name.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderPair {
    pub id: u64,
    pub left: Option<SyncLocation>,
    pub right: Option<SyncLocation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedSync {
    pub id: String,
    pub name: String,
    pub pairs: Vec<FolderPair>,
    pub options: SyncOptions,
}

fn syncs_file() -> PathBuf {
    openexplorer_gdrive::config_dir().join("syncs.json")
}

pub fn load_saved_syncs() -> Vec<SavedSync> {
    std::fs::read(syncs_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_syncs_to_disk(list: &[SavedSync]) {
    let Ok(bytes) = serde_json::to_vec_pretty(list) else { return };
    let _ = std::fs::create_dir_all(openexplorer_gdrive::config_dir());
    let _ = std::fs::write(syncs_file(), bytes);
}

// ---------------------------------------------------------------------------------------------
// Comparison result items & filters

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncAction {
    CopyRight,
    CopyLeft,
    DeleteRight,
    DeleteLeft,
    #[default]
    None,
}

impl SyncAction {
    pub fn label(self) -> &'static str {
        match self {
            SyncAction::CopyRight => t("Copiar →", "Copy →"),
            SyncAction::CopyLeft => t("← Copiar", "← Copy"),
            SyncAction::DeleteRight => t("Excluir →", "Delete →"),
            SyncAction::DeleteLeft => t("← Excluir", "← Delete"),
            SyncAction::None => t("Igual", "Same"),
        }
    }

    pub fn long_label(self) -> &'static str {
        match self {
            SyncAction::CopyRight => t("Copiar para a direita", "Copy to right"),
            SyncAction::CopyLeft => t("Copiar para a esquerda", "Copy to left"),
            SyncAction::DeleteRight => t("Excluir da direita", "Delete from right"),
            SyncAction::DeleteLeft => t("Excluir da esquerda", "Delete from left"),
            SyncAction::None => t("Sem ação", "No action"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RowStatus {
    /// Has an action that waits its turn: before Iniciar, and through the run until it is done.
    #[default]
    Pending,
    Running,
    Done,
    /// Its action was switched off in the table.
    Skipped,
    Failed(String),
    /// Nothing to do: the same on both sides, or the rules say to leave it.
    Idle,
    /// Left out by the filter (a name, a date, a size): shown, never synchronized.
    Filtered,
}

impl RowStatus {
    pub fn label(&self) -> String {
        match self {
            RowStatus::Pending => t("Na fila", "Queued").into(),
            RowStatus::Running => t("Sincronizando…", "Syncing…").into(),
            RowStatus::Done => t("Concluído", "Done").into(),
            RowStatus::Skipped => t("Ignorado", "Skipped").into(),
            RowStatus::Failed(e) => tr!("Erro: {e}", "Error: {e}"),
            RowStatus::Idle => String::new(),
            RowStatus::Filtered => t("Filtrado", "Filtered").into(),
        }
    }

    /// What a row starts as: in the queue with something to do, idle with nothing.
    pub fn of(action: SyncAction) -> RowStatus {
        if action == SyncAction::None { RowStatus::Idle } else { RowStatus::Pending }
    }
}

#[derive(Clone, Debug)]
pub struct SideMeta {
    pub id: String,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub md5: Option<String>,
    /// Boxed: most rows are of files on the computer, and it would more than double each one.
    pub drive_file: Option<Box<DriveFile>>,
}

#[derive(Clone, Debug)]
pub struct CompareRow {
    pub id: usize,
    pub pair_index: usize,
    pub rel_path: String,
    pub is_dir: bool,
    pub left: Option<SideMeta>,
    pub right: Option<SideMeta>,
    pub action: SyncAction,
    /// User can click to disable/skip an individual planned action before starting.
    pub enabled: bool,
    pub status: RowStatus,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActionFilter {
    #[default]
    All,
    CopyRight,
    CopyLeft,
    DeleteRight,
    DeleteLeft,
    NoAction,
}

impl ActionFilter {
    pub const ALL: [ActionFilter; 6] = [
        ActionFilter::All,
        ActionFilter::CopyRight,
        ActionFilter::CopyLeft,
        ActionFilter::DeleteRight,
        ActionFilter::DeleteLeft,
        ActionFilter::NoAction,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ActionFilter::All => t("Todas as ações", "All actions"),
            ActionFilter::CopyRight => t("Copiar para a direita", "Copy to right"),
            ActionFilter::CopyLeft => t("Copiar para a esquerda", "Copy to left"),
            ActionFilter::DeleteRight => t("Excluir da direita", "Delete from right"),
            ActionFilter::DeleteLeft => t("Excluir da esquerda", "Delete from left"),
            ActionFilter::NoAction => t("Sem ação", "No action"),
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.label() == s)
    }

    pub fn matches(self, action: SyncAction) -> bool {
        match self {
            ActionFilter::All => true,
            ActionFilter::CopyRight => action == SyncAction::CopyRight,
            ActionFilter::CopyLeft => action == SyncAction::CopyLeft,
            ActionFilter::DeleteRight => action == SyncAction::DeleteRight,
            ActionFilter::DeleteLeft => action == SyncAction::DeleteLeft,
            ActionFilter::NoAction => action == SyncAction::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatusFilter {
    #[default]
    All,
    Pending,
    Done,
    Failed,
    Skipped,
    Filtered,
}

impl StatusFilter {
    pub const ALL: [StatusFilter; 6] = [
        StatusFilter::All,
        StatusFilter::Pending,
        StatusFilter::Done,
        StatusFilter::Failed,
        StatusFilter::Skipped,
        StatusFilter::Filtered,
    ];

    pub fn label(self) -> &'static str {
        match self {
            StatusFilter::All => t("Todos os estados", "All states"),
            StatusFilter::Pending => t("Na fila", "Queued"),
            StatusFilter::Done => t("Concluído", "Done"),
            StatusFilter::Failed => t("Erro", "Error"),
            StatusFilter::Skipped => t("Ignorado", "Skipped"),
            StatusFilter::Filtered => t("Filtrado", "Filtered"),
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.label() == s)
    }

    pub fn matches(self, status: &RowStatus) -> bool {
        match self {
            StatusFilter::All => true,
            StatusFilter::Pending => matches!(status, RowStatus::Pending | RowStatus::Running),
            StatusFilter::Done => matches!(status, RowStatus::Done),
            StatusFilter::Failed => matches!(status, RowStatus::Failed(_)),
            StatusFilter::Skipped => matches!(status, RowStatus::Skipped),
            StatusFilter::Filtered => matches!(status, RowStatus::Filtered),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// UI / Dialog state

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionsDropdown {
    LeftOnly,
    RightOnly,
    LeftNewer,
    RightNewer,
}

pub struct OptionsDialog {
    pub draft: SyncOptions,
    pub open_dropdown: Option<OptionsDropdown>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterDropdown {
    OlderMode,
    OlderUnit,
    NewerMode,
    NewerUnit,
    SmallerUnit,
    LargerUnit,
}

pub struct FilterDialog {
    /// Kept so Cancelar returns to OptionsDialog and OK updates OptionsDialog's draft.
    pub parent_options: SyncOptions,
    pub exclude_edit: TextEdit,
    pub include_edit: TextEdit,
    pub older_enabled: bool,
    pub older_mode: DateFilterMode,
    pub older_val: TextEdit,
    pub older_unit: DateUnit,
    pub newer_enabled: bool,
    pub newer_mode: DateFilterMode,
    pub newer_val: TextEdit,
    pub newer_unit: DateUnit,
    pub smaller_enabled: bool,
    pub smaller_val: TextEdit,
    pub smaller_unit: SizeUnit,
    pub larger_enabled: bool,
    pub larger_val: TextEdit,
    pub larger_unit: SizeUnit,
    pub root_only: bool,
    pub skip_filtered_dirs: bool,
    pub open_dropdown: Option<FilterDropdown>,
}

impl FilterDialog {
    pub fn from_options(parent_options: SyncOptions) -> Self {
        let f = &parent_options.filter;
        let older_str = match f.exclude_older.mode {
            DateFilterMode::Period => f.exclude_older.amount.to_string(),
            DateFilterMode::Date => f.exclude_older.date.clone(),
        };
        let newer_str = match f.exclude_newer.mode {
            DateFilterMode::Period => f.exclude_newer.amount.to_string(),
            DateFilterMode::Date => f.exclude_newer.date.clone(),
        };
        Self {
            exclude_edit: TextEdit::new(&f.exclude_patterns),
            include_edit: TextEdit::new(&f.include_patterns),
            older_enabled: f.exclude_older.enabled,
            older_mode: f.exclude_older.mode,
            older_val: TextEdit::new(&older_str),
            older_unit: f.exclude_older.unit,
            newer_enabled: f.exclude_newer.enabled,
            newer_mode: f.exclude_newer.mode,
            newer_val: TextEdit::new(&newer_str),
            newer_unit: f.exclude_newer.unit,
            smaller_enabled: f.exclude_smaller.enabled,
            smaller_val: TextEdit::new(&f.exclude_smaller.amount.to_string()),
            smaller_unit: f.exclude_smaller.unit,
            larger_enabled: f.exclude_larger.enabled,
            larger_val: TextEdit::new(&f.exclude_larger.amount.to_string()),
            larger_unit: f.exclude_larger.unit,
            root_only: f.root_only,
            skip_filtered_dirs: f.skip_filtered_dirs,
            open_dropdown: None,
            parent_options,
        }
    }

    pub fn into_filter(self) -> (SyncOptions, SyncFilter) {
        let mut older = self.parent_options.filter.exclude_older.clone();
        older.enabled = self.older_enabled;
        older.mode = self.older_mode;
        older.unit = self.older_unit;
        match self.older_mode {
            DateFilterMode::Period => {
                if let Ok(v) = self.older_val.text.trim().parse::<u64>() {
                    older.amount = v;
                }
            }
            DateFilterMode::Date => older.date = self.older_val.text.trim().to_string(),
        }

        let mut newer = self.parent_options.filter.exclude_newer.clone();
        newer.enabled = self.newer_enabled;
        newer.mode = self.newer_mode;
        newer.unit = self.newer_unit;
        match self.newer_mode {
            DateFilterMode::Period => {
                if let Ok(v) = self.newer_val.text.trim().parse::<u64>() {
                    newer.amount = v;
                }
            }
            DateFilterMode::Date => newer.date = self.newer_val.text.trim().to_string(),
        }

        let smaller = SizeRule {
            enabled: self.smaller_enabled,
            amount: self.smaller_val.text.trim().parse::<u64>().unwrap_or(self.parent_options.filter.exclude_smaller.amount),
            unit: self.smaller_unit,
        };
        let larger = SizeRule {
            enabled: self.larger_enabled,
            amount: self.larger_val.text.trim().parse::<u64>().unwrap_or(self.parent_options.filter.exclude_larger.amount),
            unit: self.larger_unit,
        };

        let filter = SyncFilter {
            exclude_patterns: self.exclude_edit.text.clone(),
            include_patterns: self.include_edit.text.clone(),
            exclude_older: older,
            exclude_newer: newer,
            exclude_smaller: smaller,
            exclude_larger: larger,
            root_only: self.root_only,
            skip_filtered_dirs: self.skip_filtered_dirs,
        };
        (self.parent_options, filter)
    }
}

pub struct BrowseDialog {
    pub pair_id: u64,
    pub side: Side,
    pub current: Option<SyncLocation>,
    pub path_input: TextEdit,
    pub entries: Vec<Entry>,
    pub loading: bool,
    pub error: Option<String>,
    pub request: u64,
}

pub struct SaveDialog {
    pub name_edit: TextEdit,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPhase {
    Idle,
    Comparing,
    Running,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolbarMenu {
    ActionFilter,
    StatusFilter,
    SavedList,
    BrowseQuick(u64, Side),
}

pub struct SyncState {
    pub active_saved_id: Option<String>,
    pub active_name: String,
    pub pairs: Vec<FolderPair>,
    pub next_pair_id: u64,
    pub options: SyncOptions,
    pub saved: Vec<SavedSync>,
    pub rows: Vec<CompareRow>,
    pub filtered_out_count: usize,
    pub phase: SyncPhase,
    pub progress_text: String,
    pub done_count: usize,
    pub total_count: usize,
    pub auto_scroll: bool,
    pub action_filter: ActionFilter,
    pub status_filter: StatusFilter,
    pub search: TextEdit,
    pub open_menu: Option<ToolbarMenu>,
    pub cancel: Option<Arc<AtomicBool>>,
    pub job_seq: u64,
    pub browse_seq: u64,
    /// Bumped whenever `rows` are replaced.
    rows_gen: u64,
    /// Bumped whenever a row's status changes.
    status_gen: u64,
    /// What the table shows of `rows`, kept until what it was made from changes.
    shown: RefCell<Option<(ShownKey, Rc<Shown>)>>,
    /// What `rows` add up to, kept as they change: the foot of the table shows it on every frame.
    counts: Counts,
    /// A column of the table being dragged by its header: the place it is at, and the place
    /// it would land at now.
    pub col_drag: Option<(usize, usize)>,
    /// The rows' paths as a search reads them, made at the first search of a comparison.
    paths: RefCell<Option<Rc<Paths>>>,
    /// Each row's action and status as one byte (see [`tag`]), which is all a filter asks of a
    /// row: a megabyte to go through for a million rows, where the rows themselves are 250.
    /// Rows change only through [`SyncState::set_rows`], `toggle` and `set_status`, which keep it.
    tags: Vec<u8>,
}

const TAG_ACTIONS: [SyncAction; 5] = [SyncAction::CopyRight, SyncAction::CopyLeft, SyncAction::DeleteRight, SyncAction::DeleteLeft, SyncAction::None];

/// How many kinds of status a row has.
const STATUSES: usize = 7;

fn tag_statuses() -> [RowStatus; STATUSES] {
    [RowStatus::Pending, RowStatus::Running, RowStatus::Done, RowStatus::Skipped, RowStatus::Failed(String::new()), RowStatus::Idle, RowStatus::Filtered]
}

/// A row's action and status as one byte: their places in [`TAG_ACTIONS`] and [`tag_statuses`].
fn tag(action: SyncAction, status: &RowStatus) -> u8 {
    let action = TAG_ACTIONS.iter().position(|a| *a == action).unwrap_or(0);
    let status = match status {
        RowStatus::Pending => 0,
        RowStatus::Running => 1,
        RowStatus::Done => 2,
        RowStatus::Skipped => 3,
        RowStatus::Failed(_) => 4,
        RowStatus::Idle => 5,
        RowStatus::Filtered => 6,
    };
    (action * STATUSES + status) as u8
}

/// Which tags the filters let through.
fn passing(action: ActionFilter, status: StatusFilter) -> [bool; 5 * STATUSES] {
    let statuses = tag_statuses();
    std::array::from_fn(|tag| action.matches(TAG_ACTIONS[tag / STATUSES]) && status.matches(&statuses[tag % STATUSES]))
}

/// The rows the table shows, after its filters and search. A comparison has tens of thousands
/// of rows, so this is worked out when they or the filters change, not on every frame.
pub struct Shown {
    /// Indexes into [`SyncState::rows`], in order.
    pub rows: Rc<[u32]>,
}

/// How many rows there are of each action, leaving out the actions switched off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub copy_right: usize,
    pub copy_left: usize,
    pub delete: usize,
    pub same: usize,
}

impl Counts {
    /// The actions Iniciar would run.
    pub fn planned(&self) -> usize {
        self.copy_right + self.copy_left + self.delete
    }

    fn of(&mut self, action: SyncAction) -> &mut usize {
        match action {
            SyncAction::CopyRight => &mut self.copy_right,
            SyncAction::CopyLeft => &mut self.copy_left,
            SyncAction::DeleteRight | SyncAction::DeleteLeft => &mut self.delete,
            SyncAction::None => &mut self.same,
        }
    }
}

/// `text` in lowercase, letter by letter: what a search is typed as and looked for in.
fn lower(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    lower_into(&mut out, text);
    // No NUL in what is looked for: [`Paths`] ends each path with one.
    out.retain(|c| c != '\0');
    out
}

fn lower_into(out: &mut String, text: &str) {
    if text.is_ascii() {
        let from = out.len();
        out.push_str(text);
        out[from..].make_ascii_lowercase();
    } else {
        out.extend(text.chars().flat_map(char::to_lowercase));
    }
}

/// Every row's path in lowercase, one after the other with a NUL after each. A search reads
/// this one run of text, in a few parts at once, where making each path lowercase again on
/// every key typed took 40 ms of a million rows.
struct Paths {
    text: String,
    /// Where each row's path ends in `text`, at its NUL.
    ends: Vec<usize>,
}

impl Paths {
    fn of(rows: &[CompareRow]) -> Paths {
        let lowered = |rows: &[CompareRow]| {
            let mut text = String::with_capacity(rows.iter().map(|r| r.rel_path.len() + 1).sum());
            let mut ends = Vec::with_capacity(rows.len());
            for r in rows {
                lower_into(&mut text, &r.rel_path);
                ends.push(text.len());
                text.push('\0');
            }
            (text, ends)
        };
        let parts = if rows.len() < 50_000 { 1 } else { std::thread::available_parallelism().map_or(4, |c| c.get()).min(8) };
        if parts == 1 {
            let (text, ends) = lowered(rows);
            return Paths { text, ends };
        }
        // Many rows are made lowercase in parts at once, then put end to end: the first letter
        // of a search waits for this.
        let made: Vec<(String, Vec<usize>)> = std::thread::scope(|sc| {
            let work: Vec<_> = rows.chunks(rows.len().div_ceil(parts)).map(|part| sc.spawn(move || lowered(part))).collect();
            work.into_iter().map(|w| w.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))).collect()
        });
        let mut text = String::with_capacity(made.iter().map(|(text, _)| text.len()).sum());
        let mut ends = Vec::with_capacity(rows.len());
        for (part, part_ends) in made {
            let from = text.len();
            text.push_str(&part);
            ends.extend(part_ends.into_iter().map(|end| from + end));
        }
        Paths { text, ends }
    }

    /// The rows of `part` whose path holds `query` (in lowercase, with no NUL) and that `fits`
    /// lets through, in order.
    fn holding(&self, query: &str, part: std::ops::Range<usize>, fits: impl Fn(usize) -> bool) -> Vec<u32> {
        let mut out = Vec::new();
        if part.is_empty() {
            return out;
        }
        let first = if part.start == 0 { 0 } else { self.ends[part.start - 1] + 1 };
        // A path's NUL keeps a match from running over two rows.
        let (mut row, mut seen) = (part.start, first);
        for (at, _) in self.text[first..self.ends[part.end - 1]].match_indices(query) {
            let at = first + at;
            // Another match in a row already taken.
            if at < seen {
                continue;
            }
            while self.ends[row] < at {
                row += 1;
            }
            if fits(row) {
                out.push(row as u32);
            }
            seen = self.ends[row] + 1;
        }
        out
    }
}

/// `pick` over `0..n` in a few parts at once when there is much to go through, its answers
/// put together in order: which rows a filter shows, of a million, in a few milliseconds.
fn in_parts(n: usize, pick: impl Fn(std::ops::Range<usize>) -> Vec<u32> + Sync) -> Vec<u32> {
    let parts = if n < 50_000 { 1 } else { std::thread::available_parallelism().map_or(4, |c| c.get()).min(8) };
    if parts == 1 {
        return pick(0..n);
    }
    let each = n.div_ceil(parts);
    std::thread::scope(|sc| {
        let pick = &pick;
        let work: Vec<_> = (0..parts).map(|p| sc.spawn(move || pick((p * each).min(n)..((p + 1) * each).min(n)))).collect();
        work.into_iter().flat_map(|w| w.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))).collect()
    })
}

#[derive(PartialEq)]
struct ShownKey {
    rows_gen: u64,
    /// Only while a status filter is on: nothing else shown depends on a row's status.
    status_gen: u64,
    action: ActionFilter,
    status: StatusFilter,
    query: String,
}

impl SyncState {
    /// Replaces the comparison's rows.
    pub fn set_rows(&mut self, rows: Vec<CompareRow>) {
        self.counts = Counts::default();
        // What the filter left out is no part of the comparison: it has its own total.
        for r in rows.iter().filter(|r| (r.enabled || r.action == SyncAction::None) && r.status != RowStatus::Filtered) {
            *self.counts.of(r.action) += 1;
        }
        self.tags = rows.iter().map(|r| tag(r.action, &r.status)).collect();
        self.rows = rows;
        self.rows_gen += 1;
        *self.paths.borrow_mut() = None;
    }

    /// Gives row `id` a new status; false when there's no such row.
    pub fn set_status(&mut self, id: usize, status: RowStatus) -> bool {
        let Some(row) = self.rows.get_mut(id).filter(|r| r.id == id) else { return false };
        self.tags[id] = tag(row.action, &status);
        row.status = status;
        self.status_gen += 1;
        true
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    fn paths(&self) -> Rc<Paths> {
        self.paths.borrow_mut().get_or_insert_with(|| Rc::new(Paths::of(&self.rows))).clone()
    }

    /// Switches the action of row `id` off, or on again. Nothing for a row with none.
    pub fn toggle(&mut self, id: usize) {
        let Some(row) = self.rows.get_mut(id).filter(|r| r.id == id && r.action != SyncAction::None) else { return };
        row.enabled = !row.enabled;
        row.status = if row.enabled { RowStatus::Pending } else { RowStatus::Skipped };
        let (on, action) = (row.enabled, row.action);
        self.tags[id] = tag(action, &row.status);
        let n = self.counts.of(action);
        *n = if on { *n + 1 } else { n.saturating_sub(1) };
        self.status_gen += 1;
    }

    pub fn clear_rows(&mut self) {
        self.set_rows(Vec::new());
    }


    /// What the table shows now.
    pub fn shown(&self) -> Rc<Shown> {
        let key = ShownKey {
            rows_gen: self.rows_gen,
            status_gen: if self.status_filter == StatusFilter::All { 0 } else { self.status_gen },
            action: self.action_filter,
            status: self.status_filter,
            query: lower(self.search.text.trim()),
        };
        let mut kept = self.shown.borrow_mut();
        if let Some((k, shown)) = &*kept
            && *k == key
        {
            return shown.clone();
        }
        let (tags, passes) = (&self.tags, passing(key.action, key.status));
        let fits = |i: usize| passes[tags[i] as usize];
        let rows: Vec<u32> = if key.query.is_empty() {
            // The paths made for a search are as much text again as the rows hold: not kept
            // once the search is cleared.
            *self.paths.borrow_mut() = None;
            in_parts(tags.len(), |part| part.filter(|&i| fits(i)).map(|i| i as u32).collect())
        } else {
            let paths = self.paths();
            let paths: &Paths = &paths;
            in_parts(tags.len(), |part| paths.holding(&key.query, part, fits))
        };
        let shown = Rc::new(Shown { rows: rows.into() });
        *kept = Some((key, shown.clone()));
        shown
    }

    pub fn load() -> Self {
        let saved = load_saved_syncs();
        Self {
            active_saved_id: None,
            active_name: String::new(),
            pairs: vec![FolderPair { id: 1, left: None, right: None }],
            next_pair_id: 2,
            options: SyncOptions::default(),
            saved,
            rows: Vec::new(),
            filtered_out_count: 0,
            phase: SyncPhase::Idle,
            progress_text: String::new(),
            done_count: 0,
            total_count: 0,
            auto_scroll: true,
            action_filter: ActionFilter::All,
            status_filter: StatusFilter::All,
            search: TextEdit::new(""),
            open_menu: None,
            cancel: None,
            job_seq: 0,
            browse_seq: 0,
            rows_gen: 0,
            status_gen: 0,
            shown: RefCell::new(None),
            counts: Counts::default(),
            col_drag: None,
            paths: RefCell::new(None),
            tags: Vec::new(),
        }
    }

    pub fn is_busy(&self) -> bool {
        self.phase != SyncPhase::Idle
    }

    pub fn has_valid_pair(&self) -> bool {
        self.pairs.iter().any(|p| p.left.is_some() && p.right.is_some())
    }

    pub fn planned_action_count(&self) -> usize {
        self.counts.planned()
    }
}

// ---------------------------------------------------------------------------------------------
// Background messages

pub enum Answer {
    BrowseListed {
        request: u64,
        result: Result<Vec<Entry>, String>,
    },
    CompareProgress {
        job: u64,
        text: String,
    },
    CompareFinished {
        job: u64,
        result: Result<(Vec<CompareRow>, usize), String>,
    },
    RowStatus {
        job: u64,
        row_id: usize,
        status: RowStatus,
        done: usize,
        total: usize,
    },
    /// What a run changed since it last said: several files go at a time, and thousands of
    /// small ones a second would each wake the window.
    RowStatuses {
        job: u64,
        changes: Vec<(usize, RowStatus)>,
        done: usize,
        total: usize,
    },
    SyncFinished {
        job: u64,
        ok: usize,
        failed: usize,
        cancelled: bool,
        /// Files a server dated itself, for want of a way to keep theirs.
        undated: usize,
    },
}

pub fn arrived(s: &mut State, ans: Answer) -> bool {
    match ans {
        Answer::BrowseListed { request, result } => {
            if let Some(crate::actions::Dialog::SyncBrowse(b)) = &mut s.dialog
                && b.request == request
            {
                b.loading = false;
                match result {
                    Ok(mut entries) => {
                        entries.retain(|e| e.is_dir);
                        entries.sort_by(|a, b| rune_kit::natural_cmp(&a.name, &b.name));
                        b.entries = entries;
                        b.error = None;
                    }
                    Err(e) => {
                        b.entries.clear();
                        b.error = Some(e);
                    }
                }
                return true;
            }
            false
        }
        Answer::CompareProgress { job, text } => {
            if s.sync.job_seq == job && s.sync.phase == SyncPhase::Comparing {
                s.sync.progress_text = text;
                return true;
            }
            false
        }
        Answer::CompareFinished { job, result } => {
            if s.sync.job_seq != job {
                return false;
            }
            s.sync.phase = SyncPhase::Idle;
            s.sync.cancel = None;
            match result {
                Ok((rows, filtered_out)) => {
                    let planned = rows.iter().filter(|r| r.enabled && r.action != SyncAction::None).count();
                    let total = rows.len();
                    s.sync.set_rows(rows);
                    s.sync.filtered_out_count = filtered_out;
                    s.sync.progress_text = tr!(
                        "Comparação concluída: {} itens ({} ações).",
                        "Comparison complete: {} items ({} actions).",
                        total,
                        planned
                    );
                    s.status = s.sync.progress_text.clone();
                }
                Err(e) => {
                    s.sync.progress_text = e.clone();
                    crate::fail(s, e);
                }
            }
            true
        }
        Answer::RowStatus { job, row_id, status, done, total } => {
            if s.sync.job_seq != job {
                return false;
            }
            s.sync.set_status(row_id, status);
            s.sync.done_count = done;
            s.sync.total_count = total;
            s.sync.progress_text = tr!(
                "Sincronizando: {done} de {total}…",
                "Syncing: {done} of {total}…"
            );
            true
        }
        Answer::RowStatuses { job, changes, done, total } => {
            if s.sync.job_seq != job {
                return false;
            }
            for (row_id, status) in changes {
                s.sync.set_status(row_id, status);
            }
            s.sync.done_count = done;
            s.sync.total_count = total;
            s.sync.progress_text = tr!("Sincronizando: {done} de {total}…", "Syncing: {done} of {total}…");
            true
        }
        Answer::SyncFinished { job, ok, failed, cancelled, undated } => {
            if s.sync.job_seq != job {
                return false;
            }
            s.sync.phase = SyncPhase::Idle;
            s.sync.cancel = None;
            let msg = if cancelled {
                tr!(
                    "Sincronização cancelada ({ok} concluídos, {failed} erros).",
                    "Sync cancelled ({ok} completed, {failed} errors)."
                )
            } else if failed > 0 {
                tr!(
                    "Sincronização concluída com erros: {ok} concluídos, {failed} falhas.",
                    "Sync finished with errors: {ok} completed, {failed} failed."
                )
            } else {
                tr!(
                    "Sincronização concluída: {ok} itens processados.",
                    "Sync complete: {ok} items processed."
                )
            };
            // A server with no way to keep a file's date gave these the date they arrived:
            // said, because the next comparison will find them newer on that side.
            let msg = if undated > 0 {
                tr!(
                    "{msg} {undated} arquivos ficaram com a data do envio: o servidor não guarda a data original, e a próxima comparação os verá como mais novos.",
                    "{msg} {undated} files took the date they were sent: the server doesn't keep the original date, and the next comparison will see them as newer."
                )
            } else {
                msg
            };
            s.sync.progress_text = msg.clone();
            if failed > 0 {
                crate::fail(s, msg);
            } else {
                s.status = msg;
            }
            // Reload Explorer panes in case synced folders are currently visible.
            for p in 0..2 {
                if !s.tabs[p].is_picker() {
                    crate::refresh(s, p);
                }
            }
            true
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Actions triggered from UI

pub fn new_sync(s: &mut State) {
    if s.sync.is_busy() {
        return;
    }
    s.sync.active_saved_id = None;
    s.sync.active_name.clear();
    s.sync.pairs = vec![FolderPair { id: 1, left: None, right: None }];
    s.sync.next_pair_id = 2;
    s.sync.options = SyncOptions::default();
    s.sync.clear_rows();
    s.sync.filtered_out_count = 0;
    s.sync.progress_text.clear();
    s.sync.open_menu = None;
}

/// Opens the `Sincronizar` tab pre-configured to sync the folder currently open in the left pane
/// with the folder currently open in the right pane.
pub fn open_from_panes(s: &mut State) {
    if !s.sync.is_busy() {
        let left = (!s.tabs[0].is_picker())
            .then(|| SyncLocation::from_location(&s.panes[0].loc))
            .flatten();
        let right = (!s.tabs[1].is_picker())
            .then(|| SyncLocation::from_location(&s.panes[1].loc))
            .flatten();
        s.sync.active_saved_id = None;
        s.sync.active_name.clear();
        s.sync.pairs = vec![FolderPair { id: 1, left, right }];
        s.sync.next_pair_id = 2;
        s.sync.clear_rows();
        s.sync.filtered_out_count = 0;
        s.sync.progress_text.clear();
        s.sync.open_menu = None;
    }
    crate::set_main_tab(s, crate::MainTab::Sync);
}

pub fn add_pair(s: &mut State) {
    if s.sync.is_busy() {
        return;
    }
    let id = s.sync.next_pair_id;
    s.sync.next_pair_id += 1;
    s.sync.pairs.push(FolderPair { id, left: None, right: None });
    s.sync.clear_rows();
}

pub fn remove_pair(s: &mut State, id: u64) {
    if s.sync.is_busy() || s.sync.pairs.len() <= 1 {
        return;
    }
    s.sync.pairs.retain(|p| p.id != id);
    s.sync.clear_rows();
}

pub fn swap_pair(s: &mut State, id: u64) {
    if s.sync.is_busy() {
        return;
    }
    if let Some(pair) = s.sync.pairs.iter_mut().find(|p| p.id == id) {
        std::mem::swap(&mut pair.left, &mut pair.right);
        s.sync.clear_rows();
    }
}

pub fn set_pair_location(s: &mut State, pair_id: u64, side: Side, loc: SyncLocation) {
    if let Some(pair) = s.sync.pairs.iter_mut().find(|p| p.id == pair_id) {
        match side {
            Side::Left => pair.left = Some(loc),
            Side::Right => pair.right = Some(loc),
        }
        s.sync.clear_rows();
    }
    s.sync.open_menu = None;
}

pub fn use_pane_folder(s: &mut State, pair_id: u64, side: Side, pane: usize) {
    if s.tabs[pane].is_picker() {
        s.status = t("Esse painel ainda não tem uma pasta aberta.", "That pane doesn't have an open folder yet.").into();
        s.sync.open_menu = None;
        return;
    }
    if let Some(loc) = SyncLocation::from_location(&s.panes[pane].loc) {
        set_pair_location(s, pair_id, side, loc);
    } else {
        s.status = t("Esta visualização não pode ser sincronizada diretamente.", "This view can't be synchronized directly.").into();
        s.sync.open_menu = None;
    }
}

pub fn open_options(s: &mut State) {
    if s.sync.is_busy() {
        return;
    }
    s.sync.open_menu = None;
    s.dialog = Some(crate::actions::Dialog::SyncOptions(Box::new(OptionsDialog {
        draft: s.sync.options.clone(),
        open_dropdown: None,
    })));
}

pub fn open_filter(s: &mut State) {
    let parent_options = match &s.dialog {
        Some(crate::actions::Dialog::SyncOptions(opt)) => opt.draft.clone(),
        _ => s.sync.options.clone(),
    };
    s.dialog = Some(crate::actions::Dialog::SyncFilter(Box::new(FilterDialog::from_options(parent_options))));
}

pub fn open_save(s: &mut State) {
    s.sync.open_menu = None;
    let initial = if s.sync.active_name.is_empty() {
        match s.sync.pairs.first() {
            Some(FolderPair { left: Some(l), right: Some(r), .. }) => format!("{} ⇄ {}", l.short_label(), r.short_label()),
            _ => t("Minha sincronização", "My sync").into(),
        }
    } else {
        s.sync.active_name.clone()
    };
    s.dialog = Some(crate::actions::Dialog::SyncSave(Box::new(SaveDialog {
        name_edit: TextEdit::new(&initial),
        error: None,
    })));
}

pub fn commit_save(s: &mut State) {
    let name = match &mut s.dialog {
        Some(crate::actions::Dialog::SyncSave(d)) => {
            let trimmed = d.name_edit.text.trim().to_string();
            if trimmed.is_empty() {
                d.error = Some(t("Digite um nome para a sincronização.", "Enter a name for the sync.").into());
                return;
            }
            trimmed
        }
        _ => return,
    };
    let id = s.sync.active_saved_id.clone().unwrap_or_else(|| {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        format!("sync-{now}")
    });
    let item = SavedSync {
        id: id.clone(),
        name: name.clone(),
        pairs: s.sync.pairs.clone(),
        options: s.sync.options.clone(),
    };
    if let Some(existing) = s.sync.saved.iter_mut().find(|x| x.id == id) {
        *existing = item;
    } else {
        s.sync.saved.push(item);
    }
    save_syncs_to_disk(&s.sync.saved);
    s.sync.active_saved_id = Some(id);
    s.sync.active_name = name.clone();
    s.dialog = None;
    s.status = tr!("Sincronização \"{name}\" salva.", "Sync \"{name}\" saved.");
}

pub fn load_saved(s: &mut State, id: &str) {
    if s.sync.is_busy() {
        return;
    }
    let Some(saved) = s.sync.saved.iter().find(|x| x.id == id).cloned() else { return };
    s.sync.active_saved_id = Some(saved.id);
    s.sync.active_name = saved.name;
    s.sync.pairs = if saved.pairs.is_empty() {
        vec![FolderPair { id: 1, left: None, right: None }]
    } else {
        saved.pairs
    };
    s.sync.next_pair_id = s.sync.pairs.iter().map(|p| p.id).max().unwrap_or(0) + 1;
    s.sync.options = saved.options;
    s.sync.clear_rows();
    s.sync.filtered_out_count = 0;
    s.sync.progress_text.clear();
    s.sync.open_menu = None;
}

pub fn delete_saved(s: &mut State, id: &str) {
    s.sync.saved.retain(|x| x.id != id);
    if s.sync.active_saved_id.as_deref() == Some(id) {
        s.sync.active_saved_id = None;
        s.sync.active_name.clear();
    }
    save_syncs_to_disk(&s.sync.saved);
}

pub fn cancel_current(s: &mut State) {
    if let Some(flag) = &s.sync.cancel {
        flag.store(true, Ordering::Relaxed);
        s.sync.progress_text = t("Cancelando…", "Cancelling…").into();
    }
}

pub fn toggle_row(s: &mut State, row_id: usize) {
    if s.sync.is_busy() {
        return;
    }
    s.sync.toggle(row_id);
}

// ---------------------------------------------------------------------------------------------
// Folder browser modal

pub fn open_browse(s: &mut State, pair_id: u64, side: Side) {
    s.sync.open_menu = None;
    let existing = s
        .sync
        .pairs
        .iter()
        .find(|p| p.id == pair_id)
        .and_then(|p| match side {
            Side::Left => p.left.clone(),
            Side::Right => p.right.clone(),
        });
    let initial = existing.or_else(|| {
        let pane_idx = match side {
            Side::Left => 0,
            Side::Right => 1,
        };
        if !s.tabs[pane_idx].is_picker() {
            SyncLocation::from_location(&s.panes[pane_idx].loc)
        } else {
            Some(SyncLocation::Local(match Location::home() {
                Location::Local(p) => p,
                _ => PathBuf::from("C:\\"),
            }))
        }
    });
    let path_text = match &initial {
        Some(SyncLocation::Local(p)) => p.display().to_string(),
        Some(other) => other.display_label(),
        None => String::new(),
    };
    s.dialog = Some(crate::actions::Dialog::SyncBrowse(Box::new(BrowseDialog {
        pair_id,
        side,
        current: initial.clone(),
        path_input: TextEdit::new(&path_text),
        entries: Vec::new(),
        loading: false,
        error: None,
        request: 0,
    })));
    if let Some(loc) = initial {
        browse_navigate(s, loc);
    }
}

pub fn browse_navigate(s: &mut State, loc: SyncLocation) {
    s.sync.browse_seq += 1;
    let request = s.sync.browse_seq;
    let path_text = match &loc {
        SyncLocation::Local(p) => p.display().to_string(),
        other => other.display_label(),
    };
    if let Some(crate::actions::Dialog::SyncBrowse(b)) = &mut s.dialog {
        b.current = Some(loc.clone());
        b.path_input = TextEdit::new(&path_text);
        b.loading = true;
        b.error = None;
        b.entries.clear();
        b.request = request;
    } else {
        return;
    }

    let show_hidden = s.settings.show_hidden;
    let pool = s.servers.pool.clone();
    let servers_list = s.servers.list.clone();
    let drive_ref = match &loc {
        SyncLocation::Drive { email, .. } => Some(AccountRef::of(s, email)),
        _ => None,
    };

    background(s, move |emit| {
        let res = catch_panic(t("listar pasta", "list folder"), || {
            list_sync_location(&loc, show_hidden, &pool, &servers_list, drive_ref, emit)
        });
        emit(Msg::Sync(Answer::BrowseListed { request, result: res }));
    });
}

pub fn browse_up(s: &mut State) {
    let next = match &s.dialog {
        Some(crate::actions::Dialog::SyncBrowse(b)) => match &b.current {
            Some(SyncLocation::Local(p)) => p.parent().map(|par| SyncLocation::Local(par.to_path_buf())),
            Some(SyncLocation::Drive { email, shared_drive_id, shared_drive_name, path }) if !path.is_empty() => {
                let mut np = path.clone();
                np.pop();
                Some(SyncLocation::Drive {
                    email: email.clone(),
                    shared_drive_id: shared_drive_id.clone(),
                    shared_drive_name: shared_drive_name.clone(),
                    path: np,
                })
            }
            Some(SyncLocation::Server { server_id, server_name, path }) if !path.is_empty() => {
                let mut np = path.clone();
                np.pop();
                Some(SyncLocation::Server { server_id: server_id.clone(), server_name: server_name.clone(), path: np })
            }
            _ => None,
        },
        _ => None,
    };
    if let Some(loc) = next {
        browse_navigate(s, loc);
    }
}

pub fn browse_enter(s: &mut State, entry: &Entry) {
    if !entry.is_dir {
        return;
    }
    let next = match &s.dialog {
        Some(crate::actions::Dialog::SyncBrowse(b)) => match &b.current {
            Some(SyncLocation::Local(p)) => Some(SyncLocation::Local(p.join(&entry.name))),
            Some(SyncLocation::Drive { email, shared_drive_id, shared_drive_name, path }) => {
                let mut np = path.clone();
                let id = entry.target.clone().unwrap_or_else(|| entry.id.clone());
                np.push((id, entry.name.clone()));
                Some(SyncLocation::Drive {
                    email: email.clone(),
                    shared_drive_id: shared_drive_id.clone(),
                    shared_drive_name: shared_drive_name.clone(),
                    path: np,
                })
            }
            Some(SyncLocation::Server { server_id, server_name, path }) => {
                let mut np = path.clone();
                np.push(entry.name.clone());
                Some(SyncLocation::Server { server_id: server_id.clone(), server_name: server_name.clone(), path: np })
            }
            None => None,
        },
        _ => None,
    };
    if let Some(loc) = next {
        browse_navigate(s, loc);
    }
}

pub fn browse_go_typed(s: &mut State) {
    let raw = match &s.dialog {
        Some(crate::actions::Dialog::SyncBrowse(b)) => b.path_input.text.trim().to_string(),
        _ => return,
    };
    if raw.is_empty() {
        return;
    }
    let path = PathBuf::from(&raw);
    if path.is_dir() {
        browse_navigate(s, SyncLocation::Local(path));
    } else if let Some(crate::actions::Dialog::SyncBrowse(b)) = &mut s.dialog {
        b.error = Some(t("Pasta local não encontrada.", "Local folder not found.").into());
    }
}

pub fn browse_confirm(s: &mut State) {
    let picked = match &s.dialog {
        Some(crate::actions::Dialog::SyncBrowse(b)) => {
            // If user typed a valid local path directly, prefer it.
            let typed = PathBuf::from(b.path_input.text.trim());
            let loc = if matches!(b.current, Some(SyncLocation::Local(_)) | None) && typed.is_dir() {
                Some(SyncLocation::Local(typed))
            } else {
                b.current.clone()
            };
            loc.map(|l| (b.pair_id, b.side, l))
        }
        _ => None,
    };
    if let Some((pair_id, side, loc)) = picked {
        set_pair_location(s, pair_id, side, loc);
        s.dialog = None;
    }
}

fn list_sync_location(
    loc: &SyncLocation,
    show_hidden: bool,
    pool: &Pool,
    servers_list: &[Server],
    drive_ref: Option<AccountRef>,
    emit: &dyn Fn(Msg),
) -> Result<Vec<Entry>, String> {
    match loc {
        SyncLocation::Local(dir) => crate::model::read_local(dir, show_hidden).map_err(|e| e.to_string()),
        SyncLocation::Drive { shared_drive_id, path, .. } => {
            let drive = drive_ref
                .ok_or_else(|| t("Conta do Google Drive não encontrada.", "Google Drive account not found.").to_string())?
                .open(emit)?;
            let folder_id = path
                .last()
                .map(|(id, _)| id.as_str())
                .or(shared_drive_id.as_deref())
                .unwrap_or("root");
            let mut out = Vec::new();
            drive.list(folder_id, shared_drive_id.as_deref(), |page, _| {
                out.extend(page.into_iter().map(|f| Entry {
                    id: f.id,
                    name: f.name,
                    is_dir: f.is_dir,
                    size: f.size,
                    modified: f.modified,
                    target: f.target,
                    mime: Some(f.mime),
                    md5: f.md5,
                    starred: f.starred,
                }));
                true
            })?;
            Ok(out)
        }
        SyncLocation::Server { server_id, path, .. } => {
            let server = servers_list
                .iter()
                .find(|sv| sv.id == *server_id)
                .ok_or_else(|| t("Servidor não encontrado.", "Server not found.").to_string())?;
            let dir = server.path(path);
            servers::with(pool, server, |c| {
                let items = c.list(&dir)?;
                Ok(items
                    .into_iter()
                    .filter(|i| show_hidden || !i.name.starts_with('.'))
                    .map(servers::entry)
                    .collect())
            })
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Comparison engine

struct ScannedItem {
    is_dir: bool,
    meta: SideMeta,
}

/// What one side of a pair holds, by path from its root.
type Scanned = BTreeMap<String, ScannedItem>;

pub fn start_compare(s: &mut State) {
    if s.sync.is_busy() {
        return;
    }
    let valid_pairs: Vec<(usize, SyncLocation, SyncLocation)> = s
        .sync
        .pairs
        .iter()
        .enumerate()
        .filter_map(|(i, p)| Some((i, p.left.clone()?, p.right.clone()?)))
        .collect();
    if valid_pairs.is_empty() {
        crate::fail(s, t("Selecione pelo menos um par de pastas para comparar.", "Select at least one folder pair to compare.").into());
        return;
    }

    s.sync.job_seq += 1;
    let job = s.sync.job_seq;
    let cancel = Arc::new(AtomicBool::new(false));
    s.sync.cancel = Some(cancel.clone());
    s.sync.phase = SyncPhase::Comparing;
    s.sync.clear_rows();
    s.sync.filtered_out_count = 0;
    s.sync.progress_text = t("Lendo pastas…", "Reading folders…").into();
    s.sync.open_menu = None;

    let options = s.sync.options.clone();
    let show_hidden = s.settings.show_hidden;
    let pool = s.servers.pool.clone();
    let servers_list = s.servers.list.clone();

    // Pre-capture AccountRefs for any Drive accounts involved
    let mut drive_refs: HashMap<String, AccountRef> = HashMap::new();
    for (_, l, r) in &valid_pairs {
        for loc in [l, r] {
            if let SyncLocation::Drive { email, .. } = loc {
                drive_refs.entry(email.clone()).or_insert_with(|| AccountRef::of(s, email));
            }
        }
    }

    background(s, move |emit| {
        let result = catch_panic(t("comparar pastas", "compare folders"), || {
            let mut drives: HashMap<String, Arc<Drive>> = HashMap::new();
            for (email, aref) in drive_refs {
                drives.insert(email, aref.open(emit)?);
            }
            let now = SystemTime::now();
            let mut all_rows = Vec::new();
            let mut total_filtered_out = 0usize;

            for (pair_index, left_loc, right_loc) in &valid_pairs {
                if cancel.load(Ordering::Relaxed) {
                    return Err(t("Comparação cancelada.", "Comparison cancelled.").into());
                }
                let say = |text: String| emit(Msg::Sync(Answer::CompareProgress { job, text }));
                say(t("Lendo pastas…", "Reading folders…").into());
                let reading = |seen: usize| say(tr!("Lendo pastas: {seen} itens…", "Reading folders: {seen} items…"));
                let (left_map, right_map) =
                    scan_both(left_loc, right_loc, &options.filter, show_hidden, &pool, &servers_list, &drives, &cancel, &reading)?;
                if cancel.load(Ordering::Relaxed) {
                    return Err(t("Comparação cancelada.", "Comparison cancelled.").into());
                }

                say(t("Comparando diferenças…", "Comparing differences…").into());
                let checking = |done: usize, total: usize| say(tr!("Comparando o conteúdo: {done} de {total}…", "Comparing content: {done} of {total}…"));
                let (mut rows, filtered_out) =
                    compare_maps(*pair_index, left_loc, right_loc, left_map, right_map, &options, now, &cancel, &checking);
                // A comparison stopped half way says nothing true about what is missing.
                if cancel.load(Ordering::Relaxed) {
                    return Err(t("Comparação cancelada.", "Comparison cancelled.").into());
                }
                total_filtered_out += filtered_out;
                all_rows.append(&mut rows);
            }

            for (idx, row) in all_rows.iter_mut().enumerate() {
                row.id = idx;
            }
            Ok((all_rows, total_filtered_out))
        });
        emit(Msg::Sync(Answer::CompareFinished { job, result }));
    });
}

fn scan_location(
    root: &SyncLocation,
    filter: &SyncFilter,
    show_hidden: bool,
    pool: &Pool,
    servers_list: &[Server],
    drives: &HashMap<String, Arc<Drive>>,
    cancel: &AtomicBool,
    seen: &AtomicUsize,
) -> Result<Scanned, String> {
    let mut out = BTreeMap::new();
    match root {
        SyncLocation::Local(dir) => {
            if !dir.is_dir() {
                return Err(tr!("Pasta não encontrada: {}", "Folder not found: {}", dir.display()));
            }
            return scan_local(dir, filter, show_hidden, cancel, seen);
        }
        SyncLocation::Drive { email, shared_drive_id, path, .. } => {
            let drive = drives
                .get(email)
                .ok_or_else(|| tr!("Conta não conectada: {email}", "Account not connected: {email}"))?;
            let folder_id = path
                .last()
                .map(|(id, _)| id.as_str())
                .or(shared_drive_id.as_deref())
                .unwrap_or("root");
            return scan_drive(drive, folder_id, shared_drive_id.as_deref(), filter, cancel, seen);
        }
        SyncLocation::Server { server_id, path, .. } => {
            let server = servers_list
                .iter()
                .find(|sv| sv.id == *server_id)
                .ok_or_else(|| t("Servidor não encontrado.", "Server not found.").to_string())?;
            let base = server.path(path);
            servers::with(pool, server, |conn| {
                scan_server_dir(conn, &base, "", 0, filter, show_hidden, cancel, seen, &mut out)
            })?;
        }
    }
    Ok(out)
}

/// Reads the two sides of a pair at the same time, each on its own thread, unless both are the
/// same server: that one is asked for one folder at a time. `tell` hears how many items were
/// seen so far, a few times a second.
fn scan_both(
    left: &SyncLocation,
    right: &SyncLocation,
    filter: &SyncFilter,
    show_hidden: bool,
    pool: &Pool,
    servers_list: &[Server],
    drives: &HashMap<String, Arc<Drive>>,
    cancel: &AtomicBool,
    tell: &dyn Fn(usize),
) -> Result<(Scanned, Scanned), String> {
    let seen = AtomicUsize::new(0);
    let scan = |loc: &SyncLocation| scan_location(loc, filter, show_hidden, pool, servers_list, drives, cancel, &seen);
    let reading = || tell(seen.load(Ordering::Relaxed));
    let failed = || Err::<Scanned, String>(t("Erro interno ao ler a pasta.", "Internal error reading the folder.").into());
    let one_server = matches!((left, right), (SyncLocation::Server { server_id: a, .. }, SyncLocation::Server { server_id: b, .. }) if a == b);
    let (l, r) = std::thread::scope(|sc| {
        if one_server {
            let both = sc.spawn(|| (scan(left), scan(right)));
            wait_telling(|| both.is_finished(), reading);
            both.join().unwrap_or_else(|_| (failed(), failed()))
        } else {
            let (l, r) = (sc.spawn(|| scan(left)), sc.spawn(|| scan(right)));
            wait_telling(|| l.is_finished() && r.is_finished(), reading);
            (l.join().unwrap_or_else(|_| failed()), r.join().unwrap_or_else(|_| failed()))
        }
    });
    Ok((l?, r?))
}

/// Waits until `done` says so, calling `tell` a few times a second meanwhile.
fn wait_telling(done: impl Fn() -> bool, tell: impl Fn()) {
    let mut told = std::time::Instant::now();
    while !done() {
        std::thread::sleep(Duration::from_millis(5));
        if told.elapsed() >= Duration::from_millis(150) {
            tell();
            told = std::time::Instant::now();
        }
    }
}

fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// A folder still to be read: where it is (a path, a Drive id), its path from the root of its
/// side, and how deep it sits.
struct Folder<A> {
    at: A,
    rel: String,
    depth: usize,
}

/// What [`walk`] still has to read.
struct Walking<A> {
    queue: Vec<Folder<A>>,
    /// Folders being read now: with none, and an empty queue, the tree is read.
    reading: usize,
    error: Option<String>,
}

/// Reads a tree of folders, `readers` of them at a time: `read` lists one folder, giving the
/// items it holds and the folders inside to read next. Listing a folder is mostly waiting (for
/// the disk, a share on the network, Google), so several at once take a fraction of the time
/// one by one does. The first error ends the reading and is the answer: a side that wasn't all
/// read can't say what is missing from it.
fn walk<A: Send>(
    root: A,
    readers: usize,
    cancel: &AtomicBool,
    read: impl Fn(&Folder<A>, &mut Vec<(String, ScannedItem)>, &mut Vec<Folder<A>>) -> Result<(), String> + Sync,
) -> Result<Scanned, String> {
    let state = Mutex::new(Walking { queue: vec![Folder { at: root, rel: String::new(), depth: 0 }], reading: 0, error: None });
    let woken = Condvar::new();
    let work = || {
        let mut items = Vec::new();
        loop {
            let folder = {
                let mut w = state.lock().unwrap();
                loop {
                    if let Some(next) = w.queue.pop() {
                        w.reading += 1;
                        break next;
                    }
                    if w.reading == 0 {
                        return items;
                    }
                    w = woken.wait(w).unwrap();
                }
            };
            let mut inside = Vec::new();
            // A panic must still count the folder as read, or the other readers wait for ever.
            let done = if cancel.load(Ordering::Relaxed) { Ok(()) } else { catch_panic(t("ler a pasta", "read the folder"), || read(&folder, &mut items, &mut inside)) };
            let mut w = state.lock().unwrap();
            w.reading -= 1;
            match done {
                Ok(()) if w.error.is_none() && !cancel.load(Ordering::Relaxed) => w.queue.extend(inside.into_iter().filter(|f| f.depth <= MAX_DEPTH)),
                Ok(()) => w.queue.clear(),
                Err(e) => {
                    w.error.get_or_insert(e);
                    w.queue.clear();
                }
            }
            drop(w);
            woken.notify_all();
        }
    };
    let found: Vec<_> = std::thread::scope(|sc| {
        let readers: Vec<_> = (0..readers.max(1)).map(|_| sc.spawn(work)).collect();
        readers.into_iter().flat_map(|r| r.join().unwrap_or_default()).collect()
    });
    match state.into_inner().unwrap().error {
        Some(e) => Err(e),
        None => Ok(found.into_iter().collect()),
    }
}

/// Whether the folder at `rel` is a row of the comparison, and whether what it holds is read.
fn folder_part(filter: &SyncFilter, rel: &str) -> (bool, bool) {
    if filter.root_only {
        return (false, false);
    }
    let excluded = filter.is_dir_excluded(rel);
    (!excluded, !(excluded && filter.skip_filtered_dirs))
}

/// How many folders of the computer (or of a share on the network) are read at a time.
fn local_readers() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get()).min(8)
}

/// How many Drive folders are listed at a time, as when items are dragged out of the app.
const DRIVE_READERS: usize = 6;

fn scan_local(root: &Path, filter: &SyncFilter, show_hidden: bool, cancel: &AtomicBool, seen: &AtomicUsize) -> Result<Scanned, String> {
    walk(root.to_path_buf(), local_readers(), cancel, |folder, items, inside| {
        let entries = crate::model::read_local(&folder.at, show_hidden).map_err(|e| e.to_string())?;
        seen.fetch_add(entries.len(), Ordering::Relaxed);
        for e in entries {
            let rel = join_rel(&folder.rel, &e.name);
            let meta = SideMeta { id: e.id, size: e.size, modified: e.modified, md5: None, drive_file: None };
            if e.is_dir {
                let (listed, read) = folder_part(filter, &rel);
                if read {
                    inside.push(Folder { at: folder.at.join(&e.name), rel: rel.clone(), depth: folder.depth + 1 });
                }
                if listed {
                    items.push((rel, ScannedItem { is_dir: true, meta }));
                }
            } else {
                items.push((rel, ScannedItem { is_dir: false, meta }));
            }
        }
        Ok(())
    })
}

fn scan_drive(drive: &Drive, root_id: &str, shared_drive: Option<&str>, filter: &SyncFilter, cancel: &AtomicBool, seen: &AtomicUsize) -> Result<Scanned, String> {
    walk(root_id.to_string(), DRIVE_READERS, cancel, |folder, items, inside| {
        let mut files = Vec::new();
        drive.list(&folder.at, shared_drive, |page, _| {
            seen.fetch_add(page.len(), Ordering::Relaxed);
            files.extend(page);
            !cancel.load(Ordering::Relaxed)
        })?;
        for f in files {
            let rel = join_rel(&folder.rel, &f.name);
            if f.is_dir {
                let (listed, read) = folder_part(filter, &rel);
                let child_id = f.target.clone().unwrap_or_else(|| f.id.clone());
                if read {
                    inside.push(Folder { at: child_id.clone(), rel: rel.clone(), depth: folder.depth + 1 });
                }
                if listed {
                    let meta = SideMeta { id: child_id, size: None, modified: f.modified, md5: None, drive_file: Some(Box::new(f)) };
                    items.push((rel, ScannedItem { is_dir: true, meta }));
                }
            } else {
                let meta = SideMeta { id: f.id.clone(), size: f.size, modified: f.modified, md5: f.md5.clone(), drive_file: Some(Box::new(f)) };
                items.push((rel, ScannedItem { is_dir: false, meta }));
            }
        }
        Ok(())
    })
}

fn scan_server_dir(
    conn: &mut dyn openexplorer_remote::Conn,
    remote_dir: &str,
    prefix: &str,
    depth: usize,
    filter: &SyncFilter,
    show_hidden: bool,
    cancel: &AtomicBool,
    seen: &AtomicUsize,
    out: &mut BTreeMap<String, ScannedItem>,
) -> Result<(), String> {
    if depth > MAX_DEPTH || cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    let items = conn.list(remote_dir)?;
    seen.fetch_add(items.len(), Ordering::Relaxed);
    // A server that lists many folders in one request reads the ones in here ahead.
    if !filter.root_only && items.iter().any(|it| it.is_dir) {
        conn.read_ahead(remote_dir);
    }
    for item in items {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        if !show_hidden && item.name.starts_with('.') {
            continue;
        }
        let rel = join_rel(prefix, &item.name);
        let child_remote = servers::child(remote_dir, &item.name);
        if item.is_dir {
            if filter.root_only {
                continue;
            }
            if filter.skip_filtered_dirs && filter.is_dir_excluded(&rel) {
                continue;
            }
            if !filter.is_dir_excluded(&rel) {
                out.insert(
                    rel.clone(),
                    ScannedItem {
                        is_dir: true,
                        meta: SideMeta { id: child_remote.clone(), size: None, modified: item.modified, md5: None, drive_file: None },
                    },
                );
            }
            scan_server_dir(conn, &child_remote, &rel, depth + 1, filter, show_hidden, cancel, seen, out)?;
        } else {
            out.insert(
                rel,
                ScannedItem {
                    is_dir: false,
                    meta: SideMeta { id: child_remote, size: item.size, modified: item.modified, md5: None, drive_file: None },
                },
            );
        }
    }
    Ok(())
}

/// How much of a file is read at a time to compare its content.
const CONTENT_CHUNK: usize = 1 << 20;

/// Reads until `buf` is full or the file ends.
fn fill(file: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn compute_local_md5(path: &Path, buf: &mut [u8], cancel: &AtomicBool) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Md5::new();
    loop {
        let n = fill(&mut file, buf).ok()?;
        if n == 0 {
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

fn resolve_md5(loc: &SyncLocation, rel_path: &str, meta: &SideMeta, buf: &mut [u8], cancel: &AtomicBool) -> Option<String> {
    if let Some(h) = &meta.md5 {
        return Some(h.to_ascii_lowercase());
    }
    if let SyncLocation::Local(root) = loc {
        return compute_local_md5(&local_join_rel(root, rel_path), buf, cancel);
    }
    None
}

/// Whether two files on the computer hold the same bytes. They are read side by side and the
/// reading stops at the first difference, so no hash is worked out.
fn same_bytes(a: &Path, b: &Path, bufs: &mut (Vec<u8>, Vec<u8>), cancel: &AtomicBool) -> std::io::Result<bool> {
    let (mut a, mut b) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    loop {
        let (n, m) = (fill(&mut a, &mut bufs.0)?, fill(&mut b, &mut bufs.1)?);
        if n != m || bufs.0[..n] != bufs.1[..m] || cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

/// Whether the file at `rel_path` holds the same content on both sides, as far as that can be
/// known: a side with no MD5 and not on the computer (a server) can't say.
fn same_content(
    left_loc: &SyncLocation,
    right_loc: &SyncLocation,
    rel_path: &str,
    l: &SideMeta,
    r: &SideMeta,
    bufs: &mut (Vec<u8>, Vec<u8>),
    cancel: &AtomicBool,
) -> bool {
    if let (SyncLocation::Local(a), SyncLocation::Local(b)) = (left_loc, right_loc) {
        return same_bytes(&local_join_rel(a, rel_path), &local_join_rel(b, rel_path), bufs, cancel).unwrap_or(false);
    }
    let left = resolve_md5(left_loc, rel_path, l, &mut bufs.0, cancel);
    left.is_some() && left == resolve_md5(right_loc, rel_path, r, &mut bufs.0, cancel)
}

/// Which of `rows[which[i]]` hold the same content on both sides, checked on several threads
/// at once: reading the files is nearly all of what a comparison by content costs. `progress`
/// hears how many are done, of how many.
fn same_content_rows(
    rows: &[CompareRow],
    which: &[usize],
    left_loc: &SyncLocation,
    right_loc: &SyncLocation,
    cancel: &AtomicBool,
    progress: &dyn Fn(usize, usize),
) -> Vec<bool> {
    let (next, done) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let check = || {
        let mut bufs = (vec![0u8; CONTENT_CHUNK], vec![0u8; CONTENT_CHUNK]);
        let mut same = Vec::new();
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i >= which.len() || cancel.load(Ordering::Relaxed) {
                return same;
            }
            let row = &rows[which[i]];
            if let (Some(l), Some(r)) = (&row.left, &row.right)
                && same_content(left_loc, right_loc, &row.rel_path, l, r, &mut bufs, cancel)
            {
                same.push(i);
            }
            done.fetch_add(1, Ordering::Relaxed);
        }
    };
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8).min(which.len());
    let mut out = vec![false; which.len()];
    std::thread::scope(|sc| {
        let workers: Vec<_> = (0..threads).map(|_| sc.spawn(check)).collect();
        wait_telling(|| workers.iter().all(|w| w.is_finished()), || progress(done.load(Ordering::Relaxed), which.len()));
        for i in workers.into_iter().flat_map(|w| w.join().unwrap_or_default()) {
            out[i] = true;
        }
    });
    out
}

fn local_join_rel(root: &Path, rel_path: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for seg in rel_path.split('/') {
        if !seg.is_empty() {
            p.push(seg);
        }
    }
    p
}

fn compare_maps(
    pair_index: usize,
    left_loc: &SyncLocation,
    right_loc: &SyncLocation,
    left: BTreeMap<String, ScannedItem>,
    right: BTreeMap<String, ScannedItem>,
    options: &SyncOptions,
    now: SystemTime,
    cancel: &AtomicBool,
    progress: &dyn Fn(usize, usize),
) -> (Vec<CompareRow>, usize) {
    use std::cmp::Ordering::{Greater, Less};
    let rules = options.effective_rules();
    let mut rows = Vec::with_capacity(left.len().max(right.len()));
    let mut filtered_out = 0usize;
    // The rows of files with the same size on both sides, still to be told apart by content.
    let mut by_content = Vec::new();

    // Both sides come sorted by path: one pass takes the next path of either, or of both.
    let (mut left, mut right) = (left.into_iter().peekable(), right.into_iter().peekable());
    loop {
        let side = match (left.peek(), right.peek()) {
            (None, None) => break,
            (Some(_), None) => Less,
            (None, Some(_)) => Greater,
            (Some((l, _)), Some((r, _))) => l.cmp(r),
        };
        let (l_opt, r_opt) = (if side == Greater { None } else { left.next() }, if side == Less { None } else { right.next() });
        let (rel_path, l_opt, r_opt) = match (l_opt, r_opt) {
            (Some((path, l)), r) => (path, Some(l), r.map(|(_, r)| r)),
            (None, Some((path, r))) => (path, None, Some(r)),
            (None, None) => break,
        };
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let is_dir = l_opt.as_ref().map(|x| x.is_dir).or_else(|| r_opt.as_ref().map(|x| x.is_dir)).unwrap_or(false);

        // What the filter leaves out is still a row, for the table to show as filtered: with
        // no action, so nothing ever copies or deletes it.
        let filtered = |rel_path: String, l_opt: Option<ScannedItem>, r_opt: Option<ScannedItem>| CompareRow {
            id: 0,
            pair_index,
            rel_path,
            is_dir,
            left: l_opt.map(|x| x.meta),
            right: r_opt.map(|x| x.meta),
            action: SyncAction::None,
            enabled: false,
            status: RowStatus::Filtered,
        };

        if is_dir {
            if options.filter.is_dir_excluded(&rel_path) {
                filtered_out += 1;
                rows.push(filtered(rel_path, l_opt, r_opt));
                continue;
            }
            let action = match (&l_opt, &r_opt) {
                (Some(_), None) => rules.left_only.to_sync_action(),
                (None, Some(_)) => rules.right_only.to_sync_action(),
                (Some(_), Some(_)) => SyncAction::None,
                (None, None) => SyncAction::None,
            };
            rows.push(CompareRow {
                id: 0,
                pair_index,
                rel_path,
                is_dir: true,
                left: l_opt.map(|x| x.meta),
                right: r_opt.map(|x| x.meta),
                action,
                enabled: action != SyncAction::None,
                status: RowStatus::of(action),
            });
            continue;
        }

        // File filtering: check filter against available side(s)
        let allowed = match (&l_opt, &r_opt) {
            (Some(l), None) => options.filter.allows_file(&rel_path, l.meta.size, l.meta.modified, now),
            (None, Some(r)) => options.filter.allows_file(&rel_path, r.meta.size, r.meta.modified, now),
            (Some(l), Some(r)) => {
                options.filter.allows_file(&rel_path, l.meta.size, l.meta.modified, now)
                    || options.filter.allows_file(&rel_path, r.meta.size, r.meta.modified, now)
            }
            (None, None) => false,
        };
        if !allowed {
            filtered_out += 1;
            rows.push(filtered(rel_path, l_opt, r_opt));
            continue;
        }

        let action = match (&l_opt, &r_opt) {
            (Some(_), None) => rules.left_only.to_sync_action(),
            (None, Some(_)) => rules.right_only.to_sync_action(),
            (Some(l), Some(r)) => {
                if options.compare_hash && l.meta.size == r.meta.size {
                    by_content.push(rows.len());
                }
                decide_by_time(&l.meta, &r.meta, &rules)
            }
            (None, None) => SyncAction::None,
        };

        rows.push(CompareRow {
            id: 0,
            pair_index,
            rel_path,
            is_dir: false,
            left: l_opt.map(|x| x.meta),
            right: r_opt.map(|x| x.meta),
            action,
            enabled: action != SyncAction::None,
            status: RowStatus::of(action),
        });
    }

    // Optional comparison by content: two files that hold the same are equal, whatever their
    // dates say.
    if !by_content.is_empty() {
        let same = same_content_rows(&rows, &by_content, left_loc, right_loc, cancel, progress);
        for (&row, _) in by_content.iter().zip(same).filter(|(_, same)| *same) {
            rows[row].action = SyncAction::None;
            rows[row].enabled = false;
            rows[row].status = RowStatus::Idle;
        }
    }

    (rows, filtered_out)
}

/// What to do with a file both sides have, from its dates and sizes.
fn decide_by_time(l: &SideMeta, r: &SideMeta, rules: &SyncRules) -> SyncAction {
    match (l.modified, r.modified) {
        (Some(lm), Some(rm)) => {
            if lm > rm + TIME_TOLERANCE {
                rules.left_newer.to_sync_action()
            } else if rm > lm + TIME_TOLERANCE {
                rules.right_newer.to_sync_action()
            } else if l.size != r.size {
                // Same timestamp within tolerance, different size: prefer left_newer rule.
                rules.left_newer.to_sync_action()
            } else {
                SyncAction::None
            }
        }
        _ => {
            if l.size != r.size {
                rules.left_newer.to_sync_action()
            } else {
                SyncAction::None
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Execution engine

pub fn start_sync(s: &mut State) {
    if s.sync.is_busy() {
        return;
    }
    let planned: Vec<CompareRow> = s
        .sync
        .rows
        .iter()
        .filter(|r| r.enabled && r.action != SyncAction::None && !matches!(r.status, RowStatus::Done))
        .cloned()
        .collect();
    if planned.is_empty() {
        s.status = t("Nenhuma ação pendente para executar.", "No pending actions to run.").into();
        return;
    }

    s.sync.job_seq += 1;
    let job = s.sync.job_seq;
    let cancel = Arc::new(AtomicBool::new(false));
    s.sync.cancel = Some(cancel.clone());
    s.sync.phase = SyncPhase::Running;
    s.sync.done_count = 0;
    s.sync.total_count = planned.len();
    s.sync.progress_text = tr!("Sincronizando: 0 de {}…", "Syncing: 0 of {}…", planned.len());
    s.sync.open_menu = None;

    let pairs = s.sync.pairs.clone();
    let skip_recycle_bin = s.settings.skip_recycle_bin;
    let segmented = s.settings.segmented_downloads;
    let pool = s.servers.pool.clone();
    let servers_list = s.servers.list.clone();

    let mut drive_refs: HashMap<String, AccountRef> = HashMap::new();
    for p in &pairs {
        for loc in [&p.left, &p.right].into_iter().flatten() {
            if let SyncLocation::Drive { email, .. } = loc {
                drive_refs.entry(email.clone()).or_insert_with(|| AccountRef::of(s, email));
            }
        }
    }
    let known = Known::from_comparison(&s.sync.rows, &pairs);
    let at_once = AtOnce { uploads: s.settings.uploads.into(), downloads: s.settings.downloads.into(), disk: DISK_AT_ONCE };

    background(s, move |emit| {
        let mut drives: HashMap<String, Arc<Drive>> = HashMap::new();
        for (email, aref) in drive_refs {
            match aref.open(emit) {
                Ok(d) => {
                    drives.insert(email, d);
                }
                Err(e) => {
                    for r in &planned {
                        emit(Msg::Sync(Answer::RowStatus {
                            job,
                            row_id: r.id,
                            status: RowStatus::Failed(e.clone()),
                            done: 0,
                            total: planned.len(),
                        }));
                    }
                    emit(Msg::Sync(Answer::SyncFinished { job, ok: 0, failed: planned.len(), cancelled: false, undated: 0 }));
                    return;
                }
            }
        }

        let run = Run {
            pairs: &pairs,
            skip_recycle_bin,
            segmented,
            pool: &pool,
            servers: &servers_list,
            drives: &drives,
            cancel: &cancel,
            known: &known,
            at_once,
        };
        let total = planned.len();
        let (ok, failed, cancelled) = run_plan(&run, planned, &mut |changes, done| emit(Msg::Sync(Answer::RowStatuses { job, changes, done, total })));
        emit(Msg::Sync(Answer::SyncFinished { job, ok, failed, cancelled, undated: known.undated.load(Ordering::Relaxed) }));
    });
}

/// How many files a run copies at the same time, by where they go.
#[derive(Clone, Copy, Debug)]
pub struct AtOnce {
    /// To a cloud or a server: Opções' uploads at once.
    pub uploads: usize,
    /// From a cloud or a server to the computer: Opções' downloads at once.
    pub downloads: usize,
    /// From a folder of the computer to another.
    pub disk: usize,
}

/// Files copied at once between two folders of the computer: many small files go several
/// times faster than one by one, and past this the disk is what holds them back.
const DISK_AT_ONCE: usize = 4;

/// What a run works with, besides its rows.
struct Run<'a> {
    pairs: &'a [FolderPair],
    skip_recycle_bin: bool,
    segmented: bool,
    pool: &'a Pool,
    servers: &'a [Server],
    drives: &'a HashMap<String, Arc<Drive>>,
    cancel: &'a AtomicBool,
    known: &'a Known,
    at_once: AtOnce,
}

impl Run<'_> {
    /// The two sides of the pair `row` is of.
    fn sides(&self, row: &CompareRow) -> Result<(&SyncLocation, &SyncLocation), String> {
        let pair = self.pairs.get(row.pair_index).ok_or_else(|| t("Par de pastas inválido.", "Invalid folder pair.").to_string())?;
        match (&pair.left, &pair.right) {
            (Some(left), Some(right)) => Ok((left, right)),
            _ => Err(t("Par de pastas incompleto.", "Incomplete folder pair.").into()),
        }
    }

    fn one(&self, row: &CompareRow) -> Result<(), String> {
        catch_panic(t("sincronizar item", "sync item"), || {
            let (left, right) = self.sides(row)?;
            execute_row(row, left, right, self.skip_recycle_bin, self.segmented, self.pool, self.servers, self.drives, self.cancel, self.known)
        })
    }

    /// How many of `rows`, all copies of files going `way`, run at once: Opções' count for
    /// that way, and for a server one connection fewer than it allows, so the panes can still
    /// list while a run goes on.
    fn workers(&self, way: Way, rows: &[&CompareRow]) -> usize {
        let wanted = match way {
            Way::Disk => self.at_once.disk,
            Way::Up => self.at_once.uploads,
            Way::Down => self.at_once.downloads,
        };
        let mut most = wanted.max(1);
        for row in rows {
            let Ok((left, right)) = self.sides(row) else { continue };
            for side in [left, right] {
                if let SyncLocation::Server { server_id, .. } = side
                    && let Some(server) = self.servers.iter().find(|sv| sv.id == *server_id)
                {
                    most = most.min(server.limit().saturating_sub(1).max(1));
                }
            }
        }
        most.min(rows.len()).max(1)
    }
}

/// Which way a file is copied, which says how many go at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Way {
    /// Between two folders of the computer.
    Disk,
    /// To a cloud or a server (from the computer, or from another cloud or server).
    Up,
    /// From a cloud or a server to the computer.
    Down,
}

impl Way {
    fn of(from: &SyncLocation, to: &SyncLocation) -> Way {
        match (from, to) {
            (SyncLocation::Local(_), SyncLocation::Local(_)) => Way::Disk,
            (_, SyncLocation::Local(_)) => Way::Down,
            _ => Way::Up,
        }
    }
}

/// How often a run tells the window what changed: often enough to follow, seldom enough that
/// thousands of small files a second don't each wake it.
const TOLD_EVERY: Duration = Duration::from_millis(40);

/// Runs `planned`, telling `told` which rows changed status (a few at a time) and how many are
/// done. Returns how many went well, how many failed, and whether it was cancelled.
///
/// The order is what keeps it safe: folders are made first (the shallowest first), then files
/// are copied, then files are deleted, then folders (the deepest first). Only the copies of
/// files go several at a time, each way by its own count.
fn run_plan(run: &Run, planned: Vec<CompareRow>, told: &mut dyn FnMut(Vec<(usize, RowStatus)>, usize)) -> (usize, usize, bool) {
    let depth = |r: &CompareRow| r.rel_path.matches('/').count();
    let copies = |r: &CompareRow| matches!(r.action, SyncAction::CopyRight | SyncAction::CopyLeft);
    let mut make: Vec<&CompareRow> = planned.iter().filter(|r| copies(r) && r.is_dir).collect();
    let copy: Vec<&CompareRow> = planned.iter().filter(|r| copies(r) && !r.is_dir).collect();
    let delete: Vec<&CompareRow> = planned.iter().filter(|r| !copies(r) && !r.is_dir && r.action != SyncAction::None).collect();
    let mut unmake: Vec<&CompareRow> = planned.iter().filter(|r| !copies(r) && r.is_dir && r.action != SyncAction::None).collect();
    make.sort_by_key(|r| depth(r));
    unmake.sort_by_key(|r| std::cmp::Reverse(depth(r)));

    // Each way of copying is a group of its own, so each keeps to its count.
    let way = |r: &CompareRow| match (run.sides(r), r.action) {
        (Ok((left, right)), SyncAction::CopyRight) => Way::of(left, right),
        (Ok((left, right)), _) => Way::of(right, left),
        // No pair to go by: it fails when it runs, whichever group it is in.
        (Err(_), _) => Way::Disk,
    };
    let mut groups: Vec<(usize, Vec<&CompareRow>)> = vec![(1, make)];
    for of in [Way::Disk, Way::Up, Way::Down] {
        let rows: Vec<&CompareRow> = copy.iter().copied().filter(|r| way(r) == of).collect();
        groups.push((run.workers(of, &rows), rows));
    }
    groups.extend([(1, delete), (1, unmake)]);

    let (mut ok, mut failed, mut done) = (0usize, 0usize, 0usize);
    for (workers, rows) in groups {
        if rows.is_empty() {
            continue;
        }
        if run.cancel.load(Ordering::Relaxed) {
            return (ok, failed, true);
        }
        let next = AtomicUsize::new(0);
        let (send, changes) = std::sync::mpsc::channel::<(usize, RowStatus)>();
        std::thread::scope(|sc| {
            for _ in 0..workers {
                let (send, next, rows) = (send.clone(), &next, &rows);
                sc.spawn(move || {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= rows.len() || run.cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        let row = rows[i];
                        let _ = send.send((row.id, RowStatus::Running));
                        let status = match run.one(row) {
                            Ok(()) => RowStatus::Done,
                            Err(e) => RowStatus::Failed(e),
                        };
                        let _ = send.send((row.id, status));
                    }
                });
            }
            drop(send);
            // The window hears of the changes a few at a time, and at once of one that has
            // nothing after it for a while (a big file starting).
            let mut waiting = Vec::new();
            let mut last = std::time::Instant::now();
            loop {
                let change = changes.recv_timeout(TOLD_EVERY);
                let ended = matches!(change, Err(std::sync::mpsc::RecvTimeoutError::Disconnected));
                if let Ok((id, status)) = change {
                    match &status {
                        RowStatus::Done => (ok, done) = (ok + 1, done + 1),
                        RowStatus::Failed(_) => (failed, done) = (failed + 1, done + 1),
                        _ => {}
                    }
                    waiting.push((id, status));
                }
                if !waiting.is_empty() && (ended || last.elapsed() >= TOLD_EVERY) {
                    told(std::mem::take(&mut waiting), done);
                    last = std::time::Instant::now();
                }
                if ended {
                    break;
                }
            }
        });
    }
    (ok, failed, run.cancel.load(Ordering::Relaxed))
}

fn execute_row(
    row: &CompareRow,
    left: &SyncLocation,
    right: &SyncLocation,
    skip_recycle_bin: bool,
    segmented: bool,
    pool: &Pool,
    servers_list: &[Server],
    drives: &HashMap<String, Arc<Drive>>,
    cancel: &AtomicBool,
    known: &Known,
) -> Result<(), String> {
    match row.action {
        SyncAction::CopyRight => copy_item(
            left,
            right,
            &row.rel_path,
            row.is_dir,
            row.left.as_ref(),
            row.right.as_ref(),
            segmented,
            pool,
            servers_list,
            drives,
            cancel,
            known,
        ),
        SyncAction::CopyLeft => copy_item(
            right,
            left,
            &row.rel_path,
            row.is_dir,
            row.right.as_ref(),
            row.left.as_ref(),
            segmented,
            pool,
            servers_list,
            drives,
            cancel,
            known,
        ),
        SyncAction::DeleteRight => delete_item(right, &row.rel_path, row.is_dir, row.right.as_ref(), skip_recycle_bin, pool, servers_list, drives),
        SyncAction::DeleteLeft => delete_item(left, &row.rel_path, row.is_dir, row.left.as_ref(), skip_recycle_bin, pool, servers_list, drives),
        SyncAction::None => Ok(()),
    }
}

fn copy_item(
    src_root: &SyncLocation,
    dst_root: &SyncLocation,
    rel_path: &str,
    is_dir: bool,
    src_meta: Option<&SideMeta>,
    dst_meta: Option<&SideMeta>,
    segmented: bool,
    pool: &Pool,
    servers_list: &[Server],
    drives: &HashMap<String, Arc<Drive>>,
    cancel: &AtomicBool,
    known: &Known,
) -> Result<(), String> {
    if is_dir {
        return ensure_dir(dst_root, rel_path, pool, servers_list, drives, known);
    }

    // Fast path: Local -> Local
    if let (SyncLocation::Local(src_base), SyncLocation::Local(dst_base)) = (src_root, dst_root) {
        let from = local_join_rel(src_base, rel_path);
        let to = local_join_rel(dst_base, rel_path);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::copy(&from, &to).map_err(|e| tr!("Falha ao copiar {}: {e}", "Failed to copy {}: {e}", rel_path))?;
        // Windows copies a file with its date; elsewhere the copy is dated now, and would read
        // as newer than what it was copied from.
        #[cfg(not(windows))]
        keep_date(&to, std::fs::metadata(&from).and_then(|m| m.modified()).ok());
        return Ok(());
    }

    // Fast path: Local -> Server
    if let (SyncLocation::Local(src_base), SyncLocation::Server { server_id, path, .. }) = (src_root, dst_root) {
        let from = local_join_rel(src_base, rel_path);
        let server = servers_list
            .iter()
            .find(|sv| sv.id == *server_id)
            .ok_or_else(|| t("Servidor não encontrado.", "Server not found.").to_string())?;
        let dst_remote = remote_join_rel(server, path, rel_path);
        return servers::with(pool, server, |conn| {
            ensure_remote_parents(conn, server, &server.path(path), rel_path, known);
            // The date the file has now, not the one the comparison read: it may have changed.
            let modified = std::fs::metadata(&from).and_then(|m| m.modified()).map_err(|e| tr!("Não consegui ler {}: {e}", "Couldn't read {}: {e}", from.display()))?;
            let dated = conn.upload_file_dated(&dst_remote, &from, modified, &mut |_| !cancel.load(Ordering::Relaxed))?;
            known.sent(dated);
            Ok(())
        });
    }

    // Fast path: Server -> Local
    if let (SyncLocation::Server { server_id, path, .. }, SyncLocation::Local(dst_base)) = (src_root, dst_root) {
        let to = local_join_rel(dst_base, rel_path);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let server = servers_list
            .iter()
            .find(|sv| sv.id == *server_id)
            .ok_or_else(|| t("Servidor não encontrado.", "Server not found.").to_string())?;
        let src_remote = remote_join_rel(server, path, rel_path);
        return servers::with(pool, server, |conn| {
            let mut f = std::fs::File::create(&to).map_err(|e| e.to_string())?;
            conn.download(&src_remote, &mut f, &mut |_| !cancel.load(Ordering::Relaxed))?;
            // The date the server lists, which is what the next comparison sets it against.
            if let Some(modified) = src_meta.and_then(|m| m.modified) {
                let _ = f.set_modified(modified);
            }
            Ok(())
        });
    }

    // Fast path: Local -> Drive
    if let (SyncLocation::Local(src_base), SyncLocation::Drive { email, .. }) = (src_root, dst_root) {
        let from = local_join_rel(src_base, rel_path);
        let drive = drives
            .get(email)
            .ok_or_else(|| tr!("Conta não conectada: {email}", "Account not connected: {email}"))?;
        if let Some(existing) = dst_meta {
            drive.upload_version(&existing.id, &from, &mut |_| !cancel.load(Ordering::Relaxed))?;
        } else {
            let (folder, name) = rel_path.rsplit_once('/').unwrap_or(("", rel_path));
            let parent_id = known.drive_folder(drive, dst_root, folder)?;
            drive.upload(&from, &parent_id, name, &mut |_| !cancel.load(Ordering::Relaxed))?;
        }
        return Ok(());
    }

    // Fast path: Drive -> Local
    if let (SyncLocation::Drive { email, .. }, SyncLocation::Local(dst_base)) = (src_root, dst_root) {
        let to = local_join_rel(dst_base, rel_path);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let drive = drives
            .get(email)
            .ok_or_else(|| tr!("Conta não conectada: {email}", "Account not connected: {email}"))?;
        let df = src_meta
            .and_then(|m| m.drive_file.as_deref())
            .ok_or_else(|| t("Metadados do arquivo no Drive ausentes.", "Missing Drive file metadata.").to_string())?;
        drive.download(df, &to, segmented, &mut |_| !cancel.load(Ordering::Relaxed))?;
        return Ok(());
    }

    // General relay through a temporary file (Cloud/Server <-> Cloud/Server)
    let tmp_dir = std::env::temp_dir().join("openexplorer-sync");
    std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;
    // Several files go at a time: each takes a name of its own, which the clock alone can't
    // promise.
    static RELAYED: AtomicUsize = AtomicUsize::new(0);
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp_file = tmp_dir.join(format!("relay-{}-{stamp}-{}.tmp", std::process::id(), RELAYED.fetch_add(1, Ordering::Relaxed)));
    let _cleanup = TempGuard(tmp_file.clone());

    // Download source -> tmp_file
    match src_root {
        SyncLocation::Drive { email, .. } => {
            let drive = drives
                .get(email)
                .ok_or_else(|| tr!("Conta não conectada: {email}", "Account not connected: {email}"))?;
            let df = src_meta
                .and_then(|m| m.drive_file.as_deref())
                .ok_or_else(|| t("Metadados do arquivo no Drive ausentes.", "Missing Drive file metadata.").to_string())?;
            drive.download(df, &tmp_file, segmented, &mut |_| !cancel.load(Ordering::Relaxed))?;
        }
        SyncLocation::Server { server_id, path, .. } => {
            let server = servers_list
                .iter()
                .find(|sv| sv.id == *server_id)
                .ok_or_else(|| t("Servidor não encontrado.", "Server not found.").to_string())?;
            let src_remote = remote_join_rel(server, path, rel_path);
            servers::with(pool, server, |conn| {
                let mut f = std::fs::File::create(&tmp_file).map_err(|e| e.to_string())?;
                conn.download(&src_remote, &mut f, &mut |_| !cancel.load(Ordering::Relaxed))?;
                // What is sent on reads its date off this file.
                if let Some(modified) = src_meta.and_then(|m| m.modified) {
                    let _ = f.set_modified(modified);
                }
                Ok(())
            })?;
        }
        SyncLocation::Local(_) => unreachable!(),
    }

    // Upload tmp_file -> destination
    match dst_root {
        SyncLocation::Drive { email, .. } => {
            let drive = drives
                .get(email)
                .ok_or_else(|| tr!("Conta não conectada: {email}", "Account not connected: {email}"))?;
            if let Some(existing) = dst_meta {
                drive.upload_version(&existing.id, &tmp_file, &mut |_| !cancel.load(Ordering::Relaxed))?;
            } else {
                let (folder, name) = rel_path.rsplit_once('/').unwrap_or(("", rel_path));
                let parent_id = known.drive_folder(drive, dst_root, folder)?;
                drive.upload(&tmp_file, &parent_id, name, &mut |_| !cancel.load(Ordering::Relaxed))?;
            }
        }
        SyncLocation::Server { server_id, path, .. } => {
            let server = servers_list
                .iter()
                .find(|sv| sv.id == *server_id)
                .ok_or_else(|| t("Servidor não encontrado.", "Server not found.").to_string())?;
            let dst_remote = remote_join_rel(server, path, rel_path);
            servers::with(pool, server, |conn| {
                ensure_remote_parents(conn, server, &server.path(path), rel_path, known);
                // The source's date: the temporary file has it (see above), unless the source
                // gave none.
                match src_meta.and_then(|m| m.modified) {
                    Some(modified) => known.sent(conn.upload_file_dated(&dst_remote, &tmp_file, modified, &mut |_| !cancel.load(Ordering::Relaxed))?),
                    None => conn.upload_file(&dst_remote, &tmp_file, &mut |_| !cancel.load(Ordering::Relaxed))?,
                }
                Ok(())
            })?;
        }
        SyncLocation::Local(_) => unreachable!(),
    }
    Ok(())
}

fn delete_item(
    root: &SyncLocation,
    rel_path: &str,
    is_dir: bool,
    meta: Option<&SideMeta>,
    skip_recycle_bin: bool,
    pool: &Pool,
    servers_list: &[Server],
    drives: &HashMap<String, Arc<Drive>>,
) -> Result<(), String> {
    match root {
        SyncLocation::Local(base) => {
            let target = local_join_rel(base, rel_path);
            if !target.exists() {
                return Ok(());
            }
            if !skip_recycle_bin && !crate::network::is_remote(&target) {
                trash::delete(&target).map_err(|e| e.to_string())
            } else if is_dir {
                std::fs::remove_dir_all(&target).map_err(|e| e.to_string())
            } else {
                std::fs::remove_file(&target).map_err(|e| e.to_string())
            }
        }
        SyncLocation::Drive { email, .. } => {
            let drive = drives
                .get(email)
                .ok_or_else(|| tr!("Conta não conectada: {email}", "Account not connected: {email}"))?;
            let id = meta
                .map(|m| m.id.as_str())
                .ok_or_else(|| t("ID do arquivo no Drive ausente.", "Missing Drive file ID.").to_string())?;
            drive.trash(id)?;
            Ok(())
        }
        SyncLocation::Server { server_id, path, .. } => {
            let server = servers_list
                .iter()
                .find(|sv| sv.id == *server_id)
                .ok_or_else(|| t("Servidor não encontrado.", "Server not found.").to_string())?;
            let remote = remote_join_rel(server, path, rel_path);
            servers::with(pool, server, |conn| conn.remove(&remote, is_dir))
        }
    }
}

/// The folders a run knows are there, on Drive and on servers, so a file doesn't ask for its
/// folders again: before, each file sent to Drive listed every folder above it to find the
/// next one's id, and each file sent to a server tried to make every folder above it.
#[derive(Default)]
struct Known {
    /// A Drive folder's id, by the side it's in (see [`drive_side`]) and its path from there.
    drive: Mutex<HashMap<(String, String), String>>,
    /// The folders of a server, by the server's id and their path on it.
    server: Mutex<std::collections::HashSet<(String, String)>>,
    /// Held while a folder not known yet is found or made.
    making: Mutex<()>,
    /// Files sent to a server that couldn't keep their date (FTP without MFMT, plain WebDAV):
    /// the next comparison will read them as newer than what they were copied from.
    undated: AtomicUsize,
}

/// Gives the file at `path` the modification time `at`, when there is one.
#[cfg_attr(windows, allow(dead_code))]
fn keep_date(path: &Path, at: Option<SystemTime>) {
    if let Some(at) = at
        && let Ok(file) = std::fs::File::options().write(true).open(path)
    {
        let _ = file.set_modified(at);
    }
}

/// The account and folder a Drive side starts at, to tell one side's folders from another's,
/// and that folder's id.
fn drive_side(loc: &SyncLocation) -> Option<(String, &str)> {
    let SyncLocation::Drive { email, shared_drive_id, path, .. } = loc else { return None };
    let root = path.last().map(|(id, _)| id.as_str()).or(shared_drive_id.as_deref()).unwrap_or("root");
    Some((format!("{email}\n{root}"), root))
}

impl Known {
    /// Notes a file sent to a server and whether the server kept its date, where that is
    /// known: only a file known to have lost its date is counted.
    fn sent(&self, dated: Option<bool>) {
        if dated == Some(false) {
            self.undated.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The folders the comparison saw: each one's row holds its Drive id or its server path.
    fn from_comparison(rows: &[CompareRow], pairs: &[FolderPair]) -> Known {
        let known = Known::default();
        let (mut drive, mut server) = (known.drive.lock().unwrap(), known.server.lock().unwrap());
        for row in rows.iter().filter(|r| r.is_dir) {
            let Some(pair) = pairs.get(row.pair_index) else { continue };
            for (loc, meta) in [(&pair.left, &row.left), (&pair.right, &row.right)] {
                match (loc, meta) {
                    (Some(loc @ SyncLocation::Drive { .. }), Some(meta)) => {
                        if let Some((side, _)) = drive_side(loc) {
                            drive.insert((side, row.rel_path.clone()), meta.id.clone());
                        }
                    }
                    (Some(SyncLocation::Server { server_id, .. }), Some(meta)) => {
                        server.insert((server_id.clone(), meta.id.clone()));
                    }
                    _ => {}
                }
            }
        }
        drop((drive, server));
        known
    }

    /// The id of the folder at `rel_dir` (`""` for the root) of the side `side`, whose root
    /// folder is `root`: `find` gives the id of the folder named so in the parent of that id,
    /// making it if need be, and is asked once for each folder not known yet.
    fn folder(&self, side: &str, root: &str, rel_dir: &str, find: &mut dyn FnMut(&str, &str) -> Result<String, String>) -> Result<String, String> {
        if rel_dir.is_empty() {
            return Ok(root.to_string());
        }
        if let Some(id) = self.drive.lock().unwrap().get(&(side.to_string(), rel_dir.to_string())) {
            return Ok(id.clone());
        }
        // One at a time from here: files go several at once, and two that miss the same folder
        // would both make it (Google Drive keeps two folders of one name). The second finds
        // what the first made.
        let _one = self.making.lock().unwrap_or_else(|e| e.into_inner());
        self.folder_unknown(side, root, rel_dir, find)
    }

    /// [`Known::folder`] with `making` held.
    fn folder_unknown(&self, side: &str, root: &str, rel_dir: &str, find: &mut dyn FnMut(&str, &str) -> Result<String, String>) -> Result<String, String> {
        if rel_dir.is_empty() {
            return Ok(root.to_string());
        }
        let key = (side.to_string(), rel_dir.to_string());
        if let Some(id) = self.drive.lock().unwrap().get(&key) {
            return Ok(id.clone());
        }
        let (parent, name) = rel_dir.rsplit_once('/').unwrap_or(("", rel_dir));
        let parent = self.folder_unknown(side, root, parent, find)?;
        let id = find(&parent, name)?;
        self.drive.lock().unwrap().insert(key, id.clone());
        Ok(id)
    }

    /// [`Known::folder`] on Google Drive, for the folder at `rel_dir` of the side `root`.
    fn drive_folder(&self, drive: &Drive, root: &SyncLocation, rel_dir: &str) -> Result<String, String> {
        let (SyncLocation::Drive { shared_drive_id, .. }, Some((side, root_id))) = (root, drive_side(root)) else {
            return Err(t("Par de pastas inválido.", "Invalid folder pair.").into());
        };
        self.folder(&side, root_id, rel_dir.trim_matches('/'), &mut |parent, name| find_or_create_drive_folder(drive, parent, shared_drive_id.as_deref(), name))
    }

    /// Has `make` make each folder of `rel_dir` under `base` on the server `server`, those not
    /// known to be there, from the top down.
    fn server_folders(&self, server: &str, base: &str, rel_dir: &str, make: &mut dyn FnMut(&str)) {
        if self.has_server_folders(server, base, rel_dir) {
            return;
        }
        // One at a time, and a folder is known only once it is made: another file going at the
        // same time would otherwise be sent into a folder that isn't there yet.
        let _one = self.making.lock().unwrap_or_else(|e| e.into_inner());
        let mut cur = base.to_string();
        for seg in rel_dir.split('/').filter(|s| !s.is_empty()) {
            cur = servers::child(&cur, seg);
            let key = (server.to_string(), cur.clone());
            if !self.server.lock().unwrap().contains(&key) {
                make(&cur);
                self.server.lock().unwrap().insert(key);
            }
        }
    }

    /// Whether every folder of `rel_dir` under `base` is known to be there on the server.
    fn has_server_folders(&self, server: &str, base: &str, rel_dir: &str) -> bool {
        let known = self.server.lock().unwrap();
        let mut cur = base.to_string();
        rel_dir.split('/').filter(|s| !s.is_empty()).all(|seg| {
            cur = servers::child(&cur, seg);
            known.contains(&(server.to_string(), cur.clone()))
        })
    }
}

fn ensure_dir(
    root: &SyncLocation,
    rel_path: &str,
    pool: &Pool,
    servers_list: &[Server],
    drives: &HashMap<String, Arc<Drive>>,
    known: &Known,
) -> Result<(), String> {
    match root {
        SyncLocation::Local(base) => {
            std::fs::create_dir_all(local_join_rel(base, rel_path)).map_err(|e| e.to_string())
        }
        SyncLocation::Drive { email, .. } => {
            let drive = drives
                .get(email)
                .ok_or_else(|| tr!("Conta não conectada: {email}", "Account not connected: {email}"))?;
            known.drive_folder(drive, root, rel_path).map(|_| ())
        }
        SyncLocation::Server { server_id, path, .. } => {
            let server = servers_list
                .iter()
                .find(|sv| sv.id == *server_id)
                .ok_or_else(|| t("Servidor não encontrado.", "Server not found.").to_string())?;
            let base = server.path(path);
            if known.has_server_folders(&server.id, &base, rel_path) {
                return Ok(());
            }
            servers::with(pool, server, |conn| {
                // A folder that is already there fails to be made, and that is fine.
                known.server_folders(&server.id, &base, rel_path, &mut |path| drop(conn.mkdir(path)));
                Ok(())
            })
        }
    }
}

/// Makes the folders above the file at `rel_path`, those not known to be there.
fn ensure_remote_parents(conn: &mut dyn openexplorer_remote::Conn, server: &Server, base: &str, rel_path: &str, known: &Known) {
    if let Some((folder, _)) = rel_path.rsplit_once('/') {
        known.server_folders(&server.id, base, folder, &mut |path| drop(conn.mkdir(path)));
    }
}

fn remote_join_rel(server: &Server, base_path: &[String], rel_path: &str) -> String {
    let mut segs = base_path.to_vec();
    segs.extend(rel_path.split('/').filter(|s| !s.is_empty()).map(str::to_string));
    server.path(&segs)
}

/// The folder named `name` in `parent_id`, made when there is none. Drive is asked for that
/// name alone, not for everything the folder holds.
fn find_or_create_drive_folder(drive: &Drive, parent_id: &str, shared_drive: Option<&str>, name: &str) -> Result<String, String> {
    match drive.child_named(parent_id, shared_drive, name)? {
        Some(f) if f.is_dir && f.name == name => Ok(f.target.unwrap_or(f.id)),
        _ => Ok(drive.create_folder(name, parent_id)?.id),
    }
}

struct TempGuard(PathBuf);
impl Drop for TempGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------------------------------------
// Unit tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_and_pattern_matching() {
        let mut f = SyncFilter::default();
        f.exclude_patterns = "*.ini\n*/thumbs.db\n/subfolder/*.txt\nnode_modules".into();
        let now = SystemTime::now();

        assert!(!f.allows_file("desktop.ini", Some(10), Some(now), now));
        assert!(!f.allows_file("a/b/config.INI", Some(10), Some(now), now));
        assert!(!f.allows_file("photos/thumbs.db", Some(10), Some(now), now));
        assert!(f.allows_file("thumbs.db", Some(10), Some(now), now)); // `*/thumbs.db` requires a parent segment
        assert!(!f.allows_file("subfolder/notes.txt", Some(10), Some(now), now));
        assert!(f.allows_file("other/subfolder/notes.txt", Some(10), Some(now), now)); // anchored `/subfolder/*.txt`
        assert!(!f.allows_file("app/node_modules/pkg/index.js", Some(10), Some(now), now));
        assert!(f.is_dir_excluded("app/node_modules"));
        assert!(f.allows_file("readme.md", Some(100), Some(now), now));
    }

    /// Plain ASCII is matched in place; it must say what the copy made lowercase says, and names
    /// with accents still match whatever their case.
    #[test]
    fn a_pattern_matches_the_same_in_place() {
        let lowered = |p: &str, t: &str| {
            let (p, t): (Vec<char>, Vec<char>) = (p.to_lowercase().chars().collect(), t.to_lowercase().chars().collect());
            glob(&p, &t, '?', '*', char::eq)
        };
        let patterns = ["", "*", "?", "*.INI", "a*b*c", "*a", "a*", "??.txt", "Thumbs.db", "*.*", "a?c*", "**", "*?*", "node_modules", "A*B"];
        let names = ["", "a", "ab", "abc", "ABC", "desktop.ini", "Desktop.INI", "aXbXc", "aa.txt", "thumbs.DB", "a.b.c", "node_modules", "*", "a*b", "ab?"];
        for p in patterns {
            for n in names {
                assert_eq!(glob_match(p, n), lowered(p, n), "{p:?} on {n:?}");
            }
        }
        assert!(glob_match("*.ini", "Desktop.INI") && !glob_match("*.ini", "desktop.in"));
        assert!(glob_match("relatório*", "RELATÓRIO final.docx") && glob_match("*ção.TXT", "Edição.txt"));
        assert!(!glob_match("relatório*", "relatorio final.docx"));
        assert!(pattern_matches_dir(r"fotos\2024", "Fotos/2024") && pattern_matches_file(r"\cache\*.log", "cache/a.log"));
    }

    #[test]
    fn include_only_filter() {
        let mut f = SyncFilter::default();
        f.include_patterns = "*.rs\n*.toml".into();
        let now = SystemTime::now();

        assert!(f.allows_file("src/main.rs", Some(10), Some(now), now));
        assert!(f.allows_file("Cargo.toml", Some(10), Some(now), now));
        assert!(!f.allows_file("notes.txt", Some(10), Some(now), now));
    }

    #[test]
    fn size_and_date_filters() {
        let now = SystemTime::now();
        let mut f = SyncFilter::default();
        f.exclude_smaller = SizeRule { enabled: true, amount: 10, unit: SizeUnit::KB };
        f.exclude_larger = SizeRule { enabled: true, amount: 1, unit: SizeUnit::MB };
        f.exclude_older = DateRule { enabled: true, mode: DateFilterMode::Period, amount: 7, unit: DateUnit::Days, date: String::new() };
        f.exclude_newer = DateRule { enabled: true, mode: DateFilterMode::Period, amount: 1, unit: DateUnit::Hours, date: String::new() };

        let two_days_ago = now - Duration::from_secs(2 * 86_400);
        let ten_days_ago = now - Duration::from_secs(10 * 86_400);
        let five_mins_ago = now - Duration::from_secs(300);

        assert!(f.allows_file("ok.bin", Some(50 * 1024), Some(two_days_ago), now));
        assert!(!f.allows_file("tiny.bin", Some(100), Some(two_days_ago), now));
        assert!(!f.allows_file("huge.bin", Some(2 * 1024 * 1024), Some(two_days_ago), now));
        assert!(!f.allows_file("old.bin", Some(50 * 1024), Some(ten_days_ago), now));
        assert!(!f.allows_file("fresh.bin", Some(50 * 1024), Some(five_mins_ago), now));
    }

    #[test]
    fn local_mirror_and_filters_end_to_end() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("oe-sync-test-{stamp}"));
        let left = root.join("left");
        let right = root.join("right");
        std::fs::create_dir_all(left.join("sub")).unwrap();
        std::fs::create_dir_all(left.join("ignored_dir")).unwrap();
        std::fs::create_dir_all(&right).unwrap();

        std::fs::write(left.join("a.txt"), b"hello").unwrap();
        std::fs::write(left.join("skip.ini"), b"ignored").unwrap();
        std::fs::write(left.join("sub/b.txt"), b"nested").unwrap();
        std::fs::write(left.join("ignored_dir/x.txt"), b"secret").unwrap();
        std::fs::write(right.join("extra.txt"), b"delete-me").unwrap();

        let mut opts = SyncOptions::default();
        opts.mode = SyncMode::Mirror;
        opts.filter.exclude_patterns = "*.ini\nignored_dir".into();
        opts.filter.skip_filtered_dirs = true;

        let cancel = AtomicBool::new(false);
        let pool = Pool::default();
        let drives = HashMap::new();
        let left_loc = SyncLocation::Local(left.clone());
        let right_loc = SyncLocation::Local(right.clone());

        let (lmap, rmap) = scan_both(&left_loc, &right_loc, &opts.filter, true, &pool, &[], &drives, &cancel, &|_| {}).unwrap();

        // skipped dir should not be scanned at all
        assert!(!lmap.contains_key("ignored_dir/x.txt"));

        let (rows, filtered_out) = compare_maps(0, &left_loc, &right_loc, lmap, rmap, &opts, SystemTime::now(), &cancel, &|_, _| {});
        assert_eq!(filtered_out, 1); // skip.ini

        let by_path: HashMap<&str, SyncAction> = rows.iter().map(|r| (r.rel_path.as_str(), r.action)).collect();
        assert_eq!(by_path.get("a.txt"), Some(&SyncAction::CopyRight));
        assert_eq!(by_path.get("sub/b.txt"), Some(&SyncAction::CopyRight));
        assert_eq!(by_path.get("extra.txt"), Some(&SyncAction::DeleteRight));
        // What the filter leaves out is a row to show, with nothing to do; the folder left
        // unread is no row at all. What has an action is in the queue, the rest idle.
        let state: HashMap<&str, (&RowStatus, bool)> = rows.iter().map(|r| (r.rel_path.as_str(), (&r.status, r.enabled))).collect();
        assert_eq!((by_path.get("skip.ini"), state.get("skip.ini")), (Some(&SyncAction::None), Some(&(&RowStatus::Filtered, false))));
        assert!(!by_path.contains_key("ignored_dir") && !by_path.contains_key("ignored_dir/x.txt"));
        assert_eq!(state.get("a.txt"), Some(&(&RowStatus::Pending, true)));
        assert!(rows.iter().all(|r| (r.status == RowStatus::Pending) == (r.action != SyncAction::None)));
        // The filters: in the queue are the three with an action, filtered the one left out.
        let mut sync = SyncState::load();
        sync.set_rows(rows.clone());
        let shown = |sync: &mut SyncState, status| {
            sync.status_filter = status;
            sync.shown().rows.iter().map(|&i| sync.rows[i as usize].rel_path.clone()).collect::<Vec<_>>()
        };
        assert_eq!(shown(&mut sync, StatusFilter::Pending), ["a.txt", "extra.txt", "sub", "sub/b.txt"]);
        assert_eq!(shown(&mut sync, StatusFilter::Filtered), ["skip.ini"]);
        assert_eq!(shown(&mut sync, StatusFilter::All).len(), rows.len());
        let counts = sync.counts();
        assert_eq!((counts.copy_right, counts.delete, counts.same), (3, 1, 0), "the filtered file is no part of the totals");

        // Execute the sync
        for row in &rows {
            if row.enabled && row.action != SyncAction::None {
                execute_row(row, &left_loc, &right_loc, true, false, &pool, &[], &drives, &cancel, &Known::default()).unwrap();
            }
        }

        assert_eq!(std::fs::read(right.join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(right.join("sub/b.txt")).unwrap(), b"nested");
        assert!(!right.join("extra.txt").exists());
        assert!(!right.join("skip.ini").exists());
        assert!(!right.join("ignored_dir").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn root_only_skips_subfolders() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("oe-sync-rootonly-{stamp}"));
        let left = root.join("left");
        std::fs::create_dir_all(left.join("sub")).unwrap();
        std::fs::write(left.join("top.txt"), b"top").unwrap();
        std::fs::write(left.join("sub/deep.txt"), b"deep").unwrap();

        let mut filter = SyncFilter::default();
        filter.root_only = true;
        let cancel = AtomicBool::new(false);
        let pool = Pool::default();
        let drives = HashMap::new();
        let seen = AtomicUsize::new(0);
        let lmap = scan_location(&SyncLocation::Local(left), &filter, true, &pool, &[], &drives, &cancel, &seen).unwrap();
        assert_eq!(seen.load(Ordering::Relaxed), 2);
        assert!(lmap.contains_key("top.txt"));
        assert!(!lmap.contains_key("sub"));
        assert!(!lmap.contains_key("sub/deep.txt"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hash_comparison_treats_identical_content_as_same() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("oe-sync-hash-{stamp}"));
        let left = root.join("left");
        let right = root.join("right");
        std::fs::create_dir_all(&left).unwrap();
        std::fs::create_dir_all(&right).unwrap();

        // The same content with another date, and other content of the same size.
        std::fs::write(left.join("same.txt"), b"identical-content").unwrap();
        std::fs::write(right.join("same.txt"), b"identical-content").unwrap();
        std::fs::write(left.join("other.txt"), b"identical-content").unwrap();
        std::fs::write(right.join("other.txt"), b"different-content").unwrap();

        let left_loc = SyncLocation::Local(left);
        let right_loc = SyncLocation::Local(right);
        let t1 = SystemTime::now();
        let t2 = t1 - Duration::from_secs(60);
        let side = |name: &str, modified| ScannedItem {
            is_dir: false,
            meta: SideMeta { id: name.into(), size: Some(17), modified: Some(modified), md5: None, drive_file: None },
        };
        let sides = |modified| BTreeMap::from([("other.txt".to_string(), side("other.txt", modified)), ("same.txt".to_string(), side("same.txt", modified))]);
        let cancel = AtomicBool::new(false);
        let actions = |compare_hash: bool| {
            let opts = SyncOptions { compare_hash, ..SyncOptions::default() };
            let (rows, _) = compare_maps(0, &left_loc, &right_loc, sides(t1), sides(t2), &opts, t1, &cancel, &|_, _| {});
            rows.into_iter().map(|r| (r.rel_path, r.action, r.enabled)).collect::<Vec<_>>()
        };

        assert_eq!(actions(false), [("other.txt".into(), SyncAction::CopyRight, true), ("same.txt".into(), SyncAction::CopyRight, true)]);
        assert_eq!(actions(true), [("other.txt".into(), SyncAction::CopyRight, true), ("same.txt".into(), SyncAction::None, false)]);

        // Against a side that gives an MD5 (a cloud), the file on the computer is hashed.
        let mut bufs = (vec![0u8; 64], vec![0u8; 64]);
        let local = SideMeta { id: "same.txt".into(), size: Some(17), modified: Some(t1), md5: None, drive_file: None };
        let hash = compute_local_md5(&local_join_rel(match &left_loc { SyncLocation::Local(p) => p, _ => unreachable!() }, "same.txt"), &mut bufs.0, &cancel);
        let cloud = SyncLocation::Drive { email: "a@b".into(), shared_drive_id: None, shared_drive_name: None, path: Vec::new() };
        let there = |md5: Option<String>| SideMeta { id: "x".into(), size: Some(17), modified: Some(t2), md5, drive_file: None };
        assert!(same_content(&left_loc, &cloud, "same.txt", &local, &there(hash.map(|h| h.to_uppercase())), &mut bufs, &cancel));
        assert!(!same_content(&left_loc, &cloud, "same.txt", &local, &there(Some("00".into())), &mut bufs, &cancel));
        assert!(!same_content(&left_loc, &cloud, "same.txt", &local, &there(None), &mut bufs, &cancel));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A search shows the rows whose path holds what was typed, whatever the case, for a few
    /// rows as for many (which are gone through in parts at once), under the filters too.
    #[test]
    fn a_search_shows_what_looking_at_each_path_would() {
        let names = ["Relatório Final.docx", "relatorio.txt", "FOTOS/Praia 01.JPG", "fotos/praia 02.jpg", "a/ab/aab/b", "ÁRVORE/Ação.TXT", "x"];
        let actions = [SyncAction::None, SyncAction::CopyRight, SyncAction::DeleteRight];
        let rows = |n: usize| -> Vec<CompareRow> {
            (0..n)
                .map(|i| CompareRow {
                    id: i,
                    pair_index: 0,
                    rel_path: format!("pasta {}/{}", i % 97, names[i % names.len()]),
                    is_dir: false,
                    left: None,
                    right: None,
                    action: actions[i % 3],
                    enabled: i % 3 != 0,
                    status: if i % 5 == 0 { RowStatus::Done } else { RowStatus::Pending },
                })
                .collect()
        };
        for n in [0, 1, 6, 1_000, 120_001] {
            let mut sync = SyncState::load();
            sync.set_rows(rows(n));
            for (action, status) in [(ActionFilter::All, StatusFilter::All), (ActionFilter::CopyRight, StatusFilter::All), (ActionFilter::All, StatusFilter::Done), (ActionFilter::DeleteRight, StatusFilter::Pending)] {
                (sync.action_filter, sync.status_filter) = (action, status);
                for typed in ["", "a", "A", "  praia 0 ", "RELAT", "relatório", "ção.txt", "árvore/a", "pasta 9", "pasta 96/x", "ab", "aab/b", "/", "zzz", "final.docx\0", ".jpg"] {
                    sync.search = TextEdit::new(typed);
                    let wanted = typed.trim().trim_end_matches('\0').to_lowercase();
                    let each: Vec<u32> = sync
                        .rows
                        .iter()
                        .filter(|r| action.matches(r.action) && status.matches(&r.status) && r.rel_path.to_lowercase().contains(&wanted))
                        .map(|r| r.id as u32)
                        .collect();
                    assert_eq!(sync.shown().rows[..], each[..], "{typed:?} in {n} rows, {action:?} {status:?}");
                }
            }
        }
    }

    /// A run asks for each folder once, and not at all for those the comparison saw.
    #[test]
    fn a_run_asks_for_each_folder_once() {
        let folder = |id: &str| Some(SideMeta { id: id.into(), size: None, modified: None, md5: None, drive_file: None });
        let row = |pair_index, rel: &str, right| CompareRow {
            id: 0,
            pair_index,
            rel_path: rel.into(),
            is_dir: true,
            left: folder(rel),
            right,
            action: SyncAction::None,
            enabled: false,
            status: RowStatus::Pending,
        };
        let drive = SyncLocation::Drive { email: "a@b".into(), shared_drive_id: None, shared_drive_name: None, path: vec![("top".into(), "Top".into())] };
        let server = SyncLocation::Server { server_id: "s1".into(), server_name: "S".into(), path: Vec::new() };
        let here = || Some(SyncLocation::Local("x".into()));
        let pairs = [FolderPair { id: 1, left: here(), right: Some(drive.clone()) }, FolderPair { id: 2, left: here(), right: Some(server) }];
        let rows = [row(0, "a", folder("id-a")), row(0, "new", None), row(1, "docs", folder("/home/docs")), row(1, "docs/old", folder("/home/docs/old"))];
        let known = Known::from_comparison(&rows, &pairs);

        // Drive: a folder's id comes from the comparison, or is asked for once.
        let (side, root) = drive_side(&drive).unwrap();
        assert_eq!(root, "top");
        let asked = RefCell::new(Vec::new());
        let mut find = |parent: &str, name: &str| {
            asked.borrow_mut().push(format!("{parent}/{name}"));
            Ok(format!("id-{name}"))
        };
        for (rel, id) in [("", "top"), ("a", "id-a"), ("a/b/c", "id-c"), ("a/b/c", "id-c"), ("a/b/d", "id-d"), ("new", "id-new")] {
            assert_eq!(known.folder(&side, root, rel, &mut find).unwrap(), id, "{rel}");
        }
        assert_eq!(*asked.borrow(), ["id-a/b", "id-b/c", "id-b/d", "top/new"]);
        // Another account's folders are not these, and an error is not kept.
        assert_eq!(known.folder("x@y\ntop", root, "a", &mut find).unwrap(), "id-a");
        assert_eq!(asked.borrow().last().unwrap(), "top/a");
        assert!(known.folder(&side, root, "bad", &mut |_, _| Err("no".into())).is_err());
        assert_eq!(known.folder(&side, root, "bad", &mut find).unwrap(), "id-bad");

        // A server: only the folders the comparison didn't see are made, once, from the top.
        let made = RefCell::new(Vec::new());
        let mut make = |path: &str| made.borrow_mut().push(path.to_string());
        assert!(known.has_server_folders("s1", "/home", "docs/old"));
        assert!(!known.has_server_folders("s1", "/home", "docs/2026/jan"));
        known.server_folders("s1", "/home", "docs/old", &mut make);
        known.server_folders("s1", "/home", "docs/2026/jan", &mut make);
        known.server_folders("s1", "/home", "docs/2026/fev", &mut make);
        known.server_folders("s2", "/home", "docs", &mut make);
        assert_eq!(*made.borrow(), ["/home/docs/2026", "/home/docs/2026/jan", "/home/docs/2026/fev", "/home/docs"]);
        assert!(known.has_server_folders("s1", "/home", "docs/2026/jan"));
    }

    /// Files going at the same time that miss the same folder make it once: on Google Drive two
    /// makes would leave two folders of one name, and on a server a file would be sent into a
    /// folder that isn't there yet.
    #[test]
    fn folders_missed_by_several_files_at_once_are_made_once() {
        let known = Known::default();
        let made = Mutex::new(Vec::new());
        let sent_too_soon = AtomicUsize::new(0);
        std::thread::scope(|sc| {
            for worker in 0..8 {
                let (known, made, sent_too_soon) = (&known, &made, &sent_too_soon);
                sc.spawn(move || {
                    for file in 0..40 {
                        let dir = format!("a/b{}/c{}", file % 4, file % 10);
                        // Drive: finding or making a folder takes a while, as a request does.
                        let id = known
                            .folder("side", "root", &dir, &mut |parent, name| {
                                std::thread::sleep(Duration::from_millis(1));
                                made.lock().unwrap().push(format!("drive {parent}/{name}"));
                                Ok(format!("{parent}/{name}"))
                            })
                            .unwrap();
                        assert_eq!(id, format!("root/{}", dir), "worker {worker}");
                        // A server: the folder is there by the time the file is sent into it.
                        known.server_folders("s1", "/home", &dir, &mut |path| {
                            std::thread::sleep(Duration::from_millis(1));
                            made.lock().unwrap().push(format!("server {path}"));
                        });
                        if !made.lock().unwrap().contains(&format!("server /home/{dir}")) {
                            sent_too_soon.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        assert_eq!(sent_too_soon.load(Ordering::Relaxed), 0, "a file was sent into a folder not made yet");
        let mut made = made.into_inner().unwrap();
        let all = made.len();
        made.sort();
        made.dedup();
        // "a", its four "b" and the twenty "c" there are, on each of the two.
        assert_eq!((made.len(), all), (2 * (1 + 4 + 20), 2 * (1 + 4 + 20)), "each folder is made once");
    }

    /// A run between two folders of the computer copies several files at a time: every file
    /// arrives, every row ends with one status, a file that fails doesn't stop the others, and
    /// folders are made before and deleted after.
    #[test]
    fn a_run_copies_several_files_at_a_time() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("oe-sync-run-{stamp}"));
        let (left, right) = (root.join("left"), root.join("right"));
        for d in 0..6 {
            std::fs::create_dir_all(left.join(format!("d{d}/deep"))).unwrap();
            for f in 0..50 {
                std::fs::write(left.join(format!("d{d}/deep/f{f}.txt")), format!("{d}-{f}")).unwrap();
            }
        }
        // On the right only: to be deleted, the files before their folders.
        std::fs::create_dir_all(right.join("old/older")).unwrap();
        std::fs::write(right.join("old/older/gone.txt"), b"x").unwrap();
        std::fs::write(right.join("old/gone too.txt"), b"x").unwrap();

        let (left_loc, right_loc) = (SyncLocation::Local(left.clone()), SyncLocation::Local(right.clone()));
        let (pool, drives, cancel) = (Pool::default(), HashMap::new(), AtomicBool::new(false));
        let opts = SyncOptions::default();
        let compared = || {
            let (l, r) = scan_both(&left_loc, &right_loc, &opts.filter, true, &pool, &[], &drives, &cancel, &|_| {}).unwrap();
            let (mut rows, _) = compare_maps(0, &left_loc, &right_loc, l, r, &opts, SystemTime::now(), &cancel, &|_, _| {});
            rows.iter_mut().enumerate().for_each(|(i, r)| r.id = i);
            rows
        };
        let mut rows = compared();
        let planned: Vec<CompareRow> = rows.iter().filter(|r| r.enabled && r.action != SyncAction::None).cloned().collect();
        assert_eq!(planned.len(), 12 + 300 + 4, "12 folders and 300 files to copy, 2 files and 2 folders to delete");
        // One of the files is gone by the time it is to be copied.
        std::fs::remove_file(left.join("d3/deep/f7.txt")).unwrap();

        let pairs = [FolderPair { id: 1, left: Some(left_loc.clone()), right: Some(right_loc.clone()) }];
        let known = Known::default();
        let at_once = AtOnce { uploads: 1, downloads: 1, disk: 4 };
        let run = Run { pairs: &pairs, skip_recycle_bin: true, segmented: false, pool: &pool, servers: &[], drives: &drives, cancel: &cancel, known: &known, at_once };
        let mut told: Vec<(usize, RowStatus)> = Vec::new();
        let mut counts = Vec::new();
        let (ok, failed, cancelled) = run_plan(&run, planned.clone(), &mut |changes, done| {
            told.extend(changes);
            counts.push(done);
        });
        assert_eq!((ok, failed, cancelled), (315, 1, false));
        assert!(counts.windows(2).all(|w| w[0] <= w[1]) && counts.last() == Some(&316), "the count goes up to the end: {counts:?}");

        // Each row started once and ended once, started before it ended.
        for row in &planned {
            let its: Vec<&RowStatus> = told.iter().filter(|(id, _)| *id == row.id).map(|(_, s)| s).collect();
            assert_eq!(its.len(), 2, "{}: {its:?}", row.rel_path);
            assert_eq!(its[0], &RowStatus::Running, "{}", row.rel_path);
            let failed = row.rel_path == "d3/deep/f7.txt";
            assert_eq!(matches!(its[1], RowStatus::Failed(_)), failed, "{}: {:?}", row.rel_path, its[1]);
            assert!(failed || its[1] == &RowStatus::Done);
        }
        // Folders are made before any file is copied, files are deleted before their folders,
        // and everything is copied before anything is deleted.
        let place = |path: &str, status: &RowStatus| {
            let id = planned.iter().find(|r| r.rel_path == path).unwrap_or_else(|| panic!("no row {path}")).id;
            told.iter().position(|(i, s)| *i == id && s == status).unwrap_or_else(|| panic!("{path} never {status:?}"))
        };
        let first_file = told.iter().position(|(id, _)| planned.iter().any(|r| r.id == *id && !r.is_dir && r.action == SyncAction::CopyRight)).unwrap();
        let last_file = told.iter().rposition(|(id, _)| planned.iter().any(|r| r.id == *id && !r.is_dir && r.action == SyncAction::CopyRight)).unwrap();
        assert!(place("d5/deep", &RowStatus::Done) < first_file && place("d0", &RowStatus::Done) < place("d0/deep", &RowStatus::Running));
        assert!(last_file < place("old/gone too.txt", &RowStatus::Running));
        assert!(place("old/older/gone.txt", &RowStatus::Done) < place("old/older", &RowStatus::Running));
        assert!(place("old/older", &RowStatus::Done) < place("old", &RowStatus::Running));

        // The right side is the left one, and comparing again finds nothing to do but the file
        // that failed (gone from the left, so nothing at all).
        assert_eq!(std::fs::read(right.join("d2/deep/f49.txt")).unwrap(), b"2-49");
        assert!(!right.join("old").exists() && !right.join("d3/deep/f7.txt").exists());
        rows = compared();
        let left_over: Vec<&str> = rows.iter().filter(|r| r.action != SyncAction::None).map(|r| r.rel_path.as_str()).collect();
        assert!(left_over.is_empty(), "comparing again: {left_over:?}");

        // Cancelled before it starts, a run does nothing and says so.
        cancel.store(true, Ordering::Relaxed);
        std::fs::write(left.join("late.txt"), b"late").unwrap();
        let late = compared().into_iter().filter(|r| r.action != SyncAction::None).collect::<Vec<_>>();
        let mut said = 0;
        assert_eq!(run_plan(&run, late, &mut |changes, _| said += changes.len()), (0, 0, true));
        assert!(said == 0 && !right.join("late.txt").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file's date is kept by hand where the system's copy doesn't keep it (everywhere but
    /// Windows, where the call isn't built), and only a file known to have lost its date on a
    /// server is counted.
    #[test]
    fn a_copy_keeps_the_date_and_only_known_losses_count() {
        let path = std::env::temp_dir().join(format!("oe-sync-date-{}", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        let old = UNIX_EPOCH + Duration::from_secs(1_577_934_245);
        keep_date(&path, None);
        assert!(std::fs::metadata(&path).unwrap().modified().unwrap() > old + Duration::from_secs(86_400));
        keep_date(&path, Some(old));
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), old);
        let _ = std::fs::remove_file(&path);

        let known = Known::default();
        for dated in [Some(true), None, Some(false), None, Some(false)] {
            known.sent(dated);
        }
        assert_eq!(known.undated.load(Ordering::Relaxed), 2);
    }

    /// `rclone serve` on a temporary folder, stopped and removed when dropped.
    struct Served(std::process::Child, PathBuf);
    impl Drop for Served {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }

    /// A folder of the computer and a server synchronized both ways, then compared again: what
    /// is left to do, and how many files the server dated itself. Against `rclone serve`, so
    /// ignored: `cargo test --release -p openexplorer -- --ignored a_server_synchronized`.
    fn after_a_run_with(protocol: openexplorer_remote::Protocol, kind: &str, port: u16) -> (Vec<(String, SyncAction)>, usize) {
        let dir = std::env::temp_dir().join(format!("oe-sync-{kind}-{port}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (left, there) = (dir.join("left"), dir.join("served"));
        let old = |days: u64| UNIX_EPOCH + Duration::from_secs(1_600_000_000 + days * 86_400 + 7);
        let put = |path: PathBuf, body: &[u8], modified: SystemTime| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            std::fs::File::options().write(true).open(&path).unwrap().set_modified(modified).unwrap();
        };
        for i in 0..6 {
            put(left.join(format!("Docs/Year {}/report {i}.txt", 2020 + i % 2)), format!("report {i}").as_bytes(), old(i));
        }
        put(left.join("top.bin"), &[5u8; 200_000], old(40));
        // Only on the server, to come down with its date.
        put(there.join("Photos/beach.jpg"), &[8u8; 50_000], old(90));
        put(there.join("notes.txt"), b"from the server", old(91));

        let mut args = vec!["serve".to_string(), kind.into(), there.display().to_string(), "--addr".into(), format!("127.0.0.1:{port}"), "--user".into(), "ana".into(), "--pass".into(), "a pass".into()];
        if kind == "ftp" {
            args.extend(["--passive-port".into(), format!("{}-{}", port + 1, port + 40)]);
        }
        let child = std::process::Command::new("rclone").args(&args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().expect("rclone on the PATH");
        let _served = Served(child, dir.clone());
        let started = std::time::Instant::now();
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(started.elapsed() < Duration::from_secs(20), "rclone serve {kind} didn't start");
            std::thread::sleep(Duration::from_millis(100));
        }

        let mut server = Server { id: format!("sync-{port}"), name: "test".into(), protocol, brand: None, host: "127.0.0.1".into(), port, user: "ana".into(), root: String::new(), key: None, host_key: None, cert: None, connections: 4, implicit: false };
        openexplorer_remote::probe(&mut server, Some("a pass")).unwrap_or_else(|e| panic!("{kind}: {e}"));
        // The run logs in with the password of the credential store, which a test doesn't
        // write to: the connections it will use are opened here and left in the pool.
        let pool = Pool::default();
        let open: Vec<_> = (0..3).map(|_| pool.take(&server, || Some("a pass".into())).unwrap()).collect();
        open.into_iter().for_each(|lease| lease.done());

        let servers = [server.clone()];
        let (left_loc, right_loc) = (SyncLocation::Local(left.clone()), SyncLocation::Server { server_id: server.id.clone(), server_name: server.name.clone(), path: Vec::new() });
        let (drives, cancel) = (HashMap::new(), AtomicBool::new(false));
        let opts = SyncOptions { mode: SyncMode::Bidirectional, ..SyncOptions::default() };
        let compared = || {
            let (l, r) = scan_both(&left_loc, &right_loc, &opts.filter, true, &pool, &servers, &drives, &cancel, &|_| {}).unwrap_or_else(|e| panic!("{kind}: {e}"));
            let (mut rows, _) = compare_maps(0, &left_loc, &right_loc, l, r, &opts, SystemTime::now(), &cancel, &|_, _| {});
            rows.iter_mut().enumerate().for_each(|(i, r)| r.id = i);
            rows
        };
        let rows = compared();
        let planned: Vec<CompareRow> = rows.iter().filter(|r| r.enabled && r.action != SyncAction::None).cloned().collect();
        // 3 folders and 7 files to send, 1 folder and 2 files to fetch.
        assert_eq!(planned.iter().filter(|r| r.action == SyncAction::CopyRight).count(), 10, "{kind}: {planned:?}");
        assert_eq!(planned.iter().filter(|r| r.action == SyncAction::CopyLeft).count(), 3, "{kind}: {planned:?}");

        let pairs = [FolderPair { id: 1, left: Some(left_loc.clone()), right: Some(right_loc.clone()) }];
        let known = Known::from_comparison(&rows, &pairs);
        let run = Run { pairs: &pairs, skip_recycle_bin: true, segmented: false, pool: &pool, servers: &servers, drives: &drives, cancel: &cancel, known: &known, at_once: AtOnce { uploads: 2, downloads: 2, disk: 2 } };
        let mut failures = Vec::new();
        let (ok, failed, _) = run_plan(&run, planned, &mut |changes, _| failures.extend(changes.into_iter().filter(|(_, s)| matches!(s, RowStatus::Failed(_)))));
        assert_eq!((ok, failed), (13, 0), "{kind}: {failures:?}");

        // Everything arrived, and what came down has the date the server listed.
        assert_eq!(std::fs::read(there.join("Docs/Year 2021/report 3.txt")).unwrap(), b"report 3", "{kind}");
        assert_eq!(std::fs::read(left.join("notes.txt")).unwrap(), b"from the server", "{kind}");
        let came = std::fs::metadata(left.join("Photos/beach.jpg")).unwrap().modified().unwrap();
        assert!(came.duration_since(old(90)).is_ok_and(|d| d < Duration::from_secs(60)), "{kind}: beach.jpg came dated {came:?}");

        let left_over = compared().into_iter().filter(|r| r.action != SyncAction::None).map(|r| (r.rel_path, r.action)).collect();
        (left_over, known.undated.load(Ordering::Relaxed))
    }

    /// SFTP keeps a file's date (SETSTAT): after a run, the comparison finds nothing to do.
    #[test]
    #[ignore = "needs rclone"]
    fn a_server_synchronized_by_sftp_compares_as_the_same() {
        assert_eq!(after_a_run_with(openexplorer_remote::Protocol::Sftp, "sftp", 22310), (Vec::new(), 0));
    }

    /// WebDAV keeps it where the server takes `X-OC-Mtime`, as rclone's, ownCloud and Nextcloud do.
    #[test]
    #[ignore = "needs rclone"]
    fn a_server_synchronized_by_webdav_compares_as_the_same() {
        assert_eq!(after_a_run_with(openexplorer_remote::Protocol::Dav, "webdav", 28110), (Vec::new(), 0));
    }

    /// An FTP server with no MFMT (rclone's) dates what it is sent itself: the run says how
    /// many files, and the comparison then finds each newer on the server. What came down from
    /// it compares as the same.
    #[test]
    #[ignore = "needs rclone"]
    fn an_ftp_server_without_mfmt_is_said_to_date_the_files_itself() {
        let (left_over, undated) = after_a_run_with(openexplorer_remote::Protocol::Ftp, "ftp", 21310);
        assert_eq!(undated, 7);
        assert_eq!(left_over.len(), 7, "{left_over:?}");
        assert!(left_over.iter().all(|(path, action)| *action == SyncAction::CopyLeft && (path.starts_with("Docs/") || path == "top.bin")), "{left_over:?}");
    }

    /// What a run between two folders of the computer costs with 1, 2, 4 and 8 files at a time:
    /// 20,000 files of 4 KB in 200 folders, then 40 files of 16 MB. The trees are made under
    /// the temp folder (`oe-sync-run-bench`) and removed at the end. Run it with
    /// `cargo test --release -p openexplorer -- --ignored --nocapture run_cost`.
    #[test]
    #[ignore = "benchmark"]
    fn run_cost() {
        let root = std::env::temp_dir().join("oe-sync-run-bench");
        let _ = std::fs::remove_dir_all(&root);
        let (pool, drives, cancel, opts) = (Pool::default(), HashMap::new(), AtomicBool::new(false), SyncOptions::default());
        for (label, folders, files, size) in [("20,000 files of 4 KB", 200, 100, 4 << 10), ("40 files of 16 MB", 4, 10, 16 << 20)] {
            let (left, right) = (root.join(label).join("left"), root.join(label).join("right"));
            let body = vec![7u8; size];
            for d in 0..folders {
                std::fs::create_dir_all(left.join(format!("folder {d:03}"))).unwrap();
                for f in 0..files {
                    std::fs::write(left.join(format!("folder {d:03}/file {f:03}.bin")), &body).unwrap();
                }
            }
            let (left_loc, right_loc) = (SyncLocation::Local(left), SyncLocation::Local(right.clone()));
            let pairs = [FolderPair { id: 1, left: Some(left_loc.clone()), right: Some(right_loc.clone()) }];
            let mut line = format!("{label}:");
            for disk in [1, 2, 4, 8, 1, 4] {
                let _ = std::fs::remove_dir_all(&right);
                std::fs::create_dir_all(&right).unwrap();
                let (l, r) = scan_both(&left_loc, &right_loc, &opts.filter, true, &pool, &[], &drives, &cancel, &|_| {}).unwrap();
                let (mut rows, _) = compare_maps(0, &left_loc, &right_loc, l, r, &opts, SystemTime::now(), &cancel, &|_, _| {});
                rows.iter_mut().enumerate().for_each(|(i, r)| r.id = i);
                let known = Known::default();
                let run = Run { pairs: &pairs, skip_recycle_bin: true, segmented: false, pool: &pool, servers: &[], drives: &drives, cancel: &cancel, known: &known, at_once: AtOnce { uploads: 1, downloads: 1, disk } };
                let (started, mut told) = (std::time::Instant::now(), 0);
                let (ok, failed, _) = run_plan(&run, rows, &mut |_, _| told += 1);
                assert_eq!((ok, failed), (folders + folders * files, 0));
                line += &format!("  {disk} at a time {:.2} s ({told} tellings)", started.elapsed().as_secs_f64());
            }
            println!("{line}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// How many files go at once follows the way they go, and a server keeps one connection
    /// for the panes.
    #[test]
    fn how_many_go_at_once_follows_the_way_and_the_server() {
        let disk = |p: &str| SyncLocation::Local(PathBuf::from(p));
        let drive = SyncLocation::Drive { email: "a@b".into(), shared_drive_id: None, shared_drive_name: None, path: Vec::new() };
        let on = |id: &str| SyncLocation::Server { server_id: id.into(), server_name: id.into(), path: Vec::new() };
        assert_eq!(Way::of(&disk("a"), &disk("b")), Way::Disk);
        assert_eq!((Way::of(&disk("a"), &drive), Way::of(&disk("a"), &on("s")), Way::of(&drive, &on("s"))), (Way::Up, Way::Up, Way::Up));
        assert_eq!((Way::of(&drive, &disk("a")), Way::of(&on("s"), &disk("a"))), (Way::Down, Way::Down));

        let server = |id: &str, connections| Server { id: id.into(), name: id.into(), protocol: openexplorer_remote::Protocol::Sftp, brand: None, host: "h".into(), port: 22, user: String::new(), root: "/".into(), key: None, host_key: None, cert: None, connections, implicit: false };
        let servers = [server("wide", 8), server("narrow", 2), server("single", 1)];
        let pairs = [
            FolderPair { id: 1, left: Some(disk("a")), right: Some(drive.clone()) },
            FolderPair { id: 2, left: Some(disk("a")), right: Some(on("wide")) },
            FolderPair { id: 3, left: Some(disk("a")), right: Some(on("narrow")) },
            FolderPair { id: 4, left: Some(disk("a")), right: Some(on("single")) },
        ];
        let (pool, drives, cancel, known) = (Pool::default(), HashMap::new(), AtomicBool::new(false), Known::default());
        let run = Run { pairs: &pairs, skip_recycle_bin: true, segmented: false, pool: &pool, servers: &servers, drives: &drives, cancel: &cancel, known: &known, at_once: AtOnce { uploads: 5, downloads: 3, disk: 4 } };
        let rows = |pair_index: usize, n: usize| -> Vec<CompareRow> {
            (0..n).map(|i| CompareRow { id: i, pair_index, rel_path: format!("f{i}"), is_dir: false, left: None, right: None, action: SyncAction::CopyRight, enabled: true, status: RowStatus::Pending }).collect()
        };
        let workers = |way, rows: &[CompareRow]| run.workers(way, &rows.iter().collect::<Vec<_>>());
        assert_eq!((workers(Way::Up, &rows(0, 100)), workers(Way::Down, &rows(0, 100)), workers(Way::Disk, &rows(0, 100))), (5, 3, 4));
        assert_eq!(workers(Way::Up, &rows(0, 2)), 2, "no more than there are files");
        assert_eq!((workers(Way::Up, &rows(1, 100)), workers(Way::Up, &rows(2, 100)), workers(Way::Up, &rows(3, 100))), (5, 1, 1));
        // Rows of several servers together keep to the narrowest.
        let mixed: Vec<CompareRow> = rows(1, 50).into_iter().chain(rows(2, 50)).collect();
        assert_eq!(workers(Way::Up, &mixed), 1);
    }

    /// A tree is read whole whatever the number of readers, and the first error is the answer.
    #[test]
    fn a_tree_is_read_by_several_readers() {
        // Folder n holds a file and, while n < 400, folders 2n+1 and 2n+2.
        let read = |fail: Option<usize>| {
            move |folder: &Folder<usize>, items: &mut Vec<(String, ScannedItem)>, inside: &mut Vec<Folder<usize>>| {
                if fail == Some(folder.at) {
                    return Err(format!("folder {}", folder.at));
                }
                let meta = || SideMeta { id: String::new(), size: Some(folder.at as u64), modified: None, md5: None, drive_file: None };
                items.push((join_rel(&folder.rel, "file"), ScannedItem { is_dir: false, meta: meta() }));
                for n in [2 * folder.at + 1, 2 * folder.at + 2].into_iter().filter(|_| folder.at < 400) {
                    let rel = join_rel(&folder.rel, &n.to_string());
                    items.push((rel.clone(), ScannedItem { is_dir: true, meta: meta() }));
                    inside.push(Folder { at: n, rel, depth: folder.depth + 1 });
                }
                Ok(())
            }
        };
        let cancel = AtomicBool::new(false);
        let one = walk(0usize, 1, &cancel, read(None)).unwrap();
        // The 400 folders with folders inside make 800 more; each of the 801 holds a file.
        assert_eq!(one.len(), 800 + 801);
        assert_eq!(one["1/3/7/file"].meta.size, Some(7));
        for readers in [2, 8, 32] {
            let many = walk(0usize, readers, &cancel, read(None)).unwrap();
            assert!(many.keys().eq(one.keys()), "{readers} readers");
            assert_eq!(walk(0usize, readers, &cancel, read(Some(200))).err().as_deref(), Some("folder 200"));
        }
        // Nothing is read once cancelled.
        cancel.store(true, Ordering::Relaxed);
        assert!(walk(0usize, 4, &cancel, read(None)).unwrap().is_empty());
    }

    /// Two sides merge by path, whichever has each: only the left, only the right, or both.
    #[test]
    fn sides_merge_in_path_order() {
        let now = SystemTime::now();
        let file = || ScannedItem { is_dir: false, meta: SideMeta { id: String::new(), size: Some(1), modified: Some(now), md5: None, drive_file: None } };
        let side = |names: &[&str]| names.iter().map(|n| (n.to_string(), file())).collect::<BTreeMap<_, _>>();
        let (loc, cancel) = (SyncLocation::Local(PathBuf::from("x")), AtomicBool::new(false));
        let (rows, _) = compare_maps(3, &loc, &loc, side(&["a", "b/1", "d"]), side(&["b/1", "c", "e"]), &SyncOptions::default(), now, &cancel, &|_, _| {});
        let got: Vec<_> = rows.iter().map(|r| (r.rel_path.as_str(), r.left.is_some(), r.right.is_some(), r.action)).collect();
        assert_eq!(
            got,
            [
                ("a", true, false, SyncAction::CopyRight),
                ("b/1", true, true, SyncAction::None),
                ("c", false, true, SyncAction::DeleteRight),
                ("d", true, false, SyncAction::CopyRight),
                ("e", false, true, SyncAction::DeleteRight),
            ]
        );
        assert!(rows.iter().all(|r| r.pair_index == 3));
    }

    /// What comparing two folders on the computer costs: 100,000 files of 1 KB a side in
    /// 1,000 folders, read and compared with no filter, with name filters, and by content.
    /// The tree is made once under the temp folder (`oe-sync-bench`) and kept for the next run.
    /// Run it with `cargo test --release -p openexplorer -- --ignored --nocapture compare_cost`.
    #[test]
    #[ignore = "benchmark"]
    fn compare_cost() {
        let root = std::env::temp_dir().join("oe-sync-bench");
        let (left, right) = (root.join("left"), root.join("right"));
        if !root.join("ready").exists() {
            let started = std::time::Instant::now();
            let body = [7u8; 1024];
            for side in [&left, &right] {
                for d in 0..1000 {
                    let dir = side.join(format!("pasta {:02}", d / 50)).join(format!("sub {d:04}"));
                    std::fs::create_dir_all(&dir).unwrap();
                    for f in 0..100 {
                        std::fs::write(dir.join(format!("arquivo {f:03}.bin")), body).unwrap();
                    }
                }
            }
            std::fs::write(root.join("ready"), b"").unwrap();
            println!("tree made in {:.1} s", started.elapsed().as_secs_f64());
        }
        let cancel = AtomicBool::new(false);
        let pool = Pool::default();
        let drives = HashMap::new();
        let (left_loc, right_loc) = (SyncLocation::Local(left), SyncLocation::Local(right));

        let plain = SyncOptions::default();
        let names = SyncFilter { exclude_patterns: "*.tmp\nthumbs.db\nnode_modules\n/cache/*.log".into(), ..SyncFilter::default() };
        let named = SyncOptions { filter: names, ..SyncOptions::default() };
        let hashed = SyncOptions { compare_hash: true, ..SyncOptions::default() };
        for (label, opts) in [("no filter", &plain), ("4 name filters", &named), ("by content", &hashed)] {
            let mut best = [f64::MAX; 3];
            let mut rows_n = 0;
            for _ in 0..3 {
                let t0 = std::time::Instant::now();
                let (lmap, rmap) = scan_both(&left_loc, &right_loc, &opts.filter, true, &pool, &[], &drives, &cancel, &|_| {}).unwrap();
                let t1 = std::time::Instant::now();
                let (rows, _) = compare_maps(0, &left_loc, &right_loc, lmap, rmap, opts, SystemTime::now(), &cancel, &|_, _| {});
                let t2 = std::time::Instant::now();
                rows_n = rows.len();
                let (read, compared) = ((t1 - t0).as_secs_f64(), (t2 - t1).as_secs_f64());
                if read + compared < best[2] {
                    best = [read, compared, read + compared];
                }
            }
            println!("{label}: {rows_n} rows, read {:.2} s, compare {:.2} s, total {:.2} s (best of 3)", best[0], best[1], best[2]);
        }
        // Reading one side after the other, as before the two were read at once.
        let mut best = f64::MAX;
        for _ in 0..3 {
            let (started, seen) = (std::time::Instant::now(), AtomicUsize::new(0));
            for loc in [&left_loc, &right_loc] {
                scan_location(loc, &plain.filter, true, &pool, &[], &drives, &cancel, &seen).unwrap();
            }
            best = best.min(started.elapsed().as_secs_f64());
        }
        println!("read, one side after the other: {best:.2} s (best of 3)");
        println!("a row takes {} bytes before its strings", std::mem::size_of::<CompareRow>());
    }

    #[test]
    fn custom_and_bidirectional_rules() {
        let mut opts = SyncOptions::default();
        opts.mode = SyncMode::Bidirectional;
        let rules = opts.effective_rules();
        assert_eq!(rules.left_only, LeftOnlyAction::CopyToRight);
        assert_eq!(rules.right_only, RightOnlyAction::CopyToLeft);
        assert_eq!(rules.left_newer, NewerAction::CopyToRight);
        assert_eq!(rules.right_newer, NewerAction::CopyToLeft);

        opts.mode = SyncMode::Custom;
        opts.rules = SyncRules {
            left_only: LeftOnlyAction::DeleteLeft,
            right_only: RightOnlyAction::DoNothing,
            left_newer: NewerAction::DoNothing,
            right_newer: NewerAction::CopyToLeft,
        };
        assert_eq!(opts.effective_rules(), opts.rules);
    }
}
