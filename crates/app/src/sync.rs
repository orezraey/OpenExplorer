//! Folder synchronization: configure 2 or more folder pairs (local or cloud/server),
//! choose a sync preset or custom rules, filter by name/date/size/subfolders, compare
//! differences with optional MD5 hash verification, and execute the sync plan.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    pub const ALL: [LeftOnlyAction; 3] = [LeftOnlyAction::CopyToRight, LeftOnlyAction::DeleteLeft, LeftOnlyAction::DoNothing];

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
    pub const ALL: [RightOnlyAction; 3] = [RightOnlyAction::CopyToLeft, RightOnlyAction::DeleteRight, RightOnlyAction::DoNothing];

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
    pub const ALL: [NewerAction; 3] = [NewerAction::CopyToRight, NewerAction::CopyToLeft, NewerAction::DoNothing];

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
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_p, mut star_t) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
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
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn pattern_matches_dir(raw_pat: &str, rel_dir: &str) -> bool {
    let pat = raw_pat.replace('\\', "/");
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
    let pat = raw_pat.replace('\\', "/");
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
    #[default]
    Pending,
    Running,
    Done,
    Skipped,
    Failed(String),
}

impl RowStatus {
    pub fn label(&self) -> String {
        match self {
            RowStatus::Pending => t("Pendente", "Pending").into(),
            RowStatus::Running => t("Sincronizando…", "Syncing…").into(),
            RowStatus::Done => t("Concluído", "Done").into(),
            RowStatus::Skipped => t("Ignorado", "Skipped").into(),
            RowStatus::Failed(e) => tr!("Erro: {e}", "Error: {e}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SideMeta {
    pub id: String,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub md5: Option<String>,
    pub drive_file: Option<DriveFile>,
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
}

impl StatusFilter {
    pub const ALL: [StatusFilter; 5] = [
        StatusFilter::All,
        StatusFilter::Pending,
        StatusFilter::Done,
        StatusFilter::Failed,
        StatusFilter::Skipped,
    ];

    pub fn label(self) -> &'static str {
        match self {
            StatusFilter::All => t("Todos os estados", "All states"),
            StatusFilter::Pending => t("Pendente", "Pending"),
            StatusFilter::Done => t("Concluído", "Done"),
            StatusFilter::Failed => t("Erro", "Error"),
            StatusFilter::Skipped => t("Ignorado", "Skipped"),
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
}

impl SyncState {
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
        }
    }

    pub fn is_busy(&self) -> bool {
        self.phase != SyncPhase::Idle
    }

    pub fn has_valid_pair(&self) -> bool {
        self.pairs.iter().any(|p| p.left.is_some() && p.right.is_some())
    }

    pub fn planned_action_count(&self) -> usize {
        self.rows.iter().filter(|r| r.enabled && r.action != SyncAction::None).count()
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
    SyncFinished {
        job: u64,
        ok: usize,
        failed: usize,
        cancelled: bool,
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
                    s.sync.rows = rows;
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
            if let Some(row) = s.sync.rows.iter_mut().find(|r| r.id == row_id) {
                row.status = status;
            }
            s.sync.done_count = done;
            s.sync.total_count = total;
            s.sync.progress_text = tr!(
                "Sincronizando: {done} de {total}…",
                "Syncing: {done} of {total}…"
            );
            true
        }
        Answer::SyncFinished { job, ok, failed, cancelled } => {
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
    s.sync.rows.clear();
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
        s.sync.rows.clear();
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
    s.sync.rows.clear();
}

pub fn remove_pair(s: &mut State, id: u64) {
    if s.sync.is_busy() || s.sync.pairs.len() <= 1 {
        return;
    }
    s.sync.pairs.retain(|p| p.id != id);
    s.sync.rows.clear();
}

pub fn swap_pair(s: &mut State, id: u64) {
    if s.sync.is_busy() {
        return;
    }
    if let Some(pair) = s.sync.pairs.iter_mut().find(|p| p.id == id) {
        std::mem::swap(&mut pair.left, &mut pair.right);
        s.sync.rows.clear();
    }
}

pub fn set_pair_location(s: &mut State, pair_id: u64, side: Side, loc: SyncLocation) {
    if let Some(pair) = s.sync.pairs.iter_mut().find(|p| p.id == pair_id) {
        match side {
            Side::Left => pair.left = Some(loc),
            Side::Right => pair.right = Some(loc),
        }
        s.sync.rows.clear();
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
    s.sync.rows.clear();
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
    if let Some(row) = s.sync.rows.iter_mut().find(|r| r.id == row_id)
        && row.action != SyncAction::None
    {
        row.enabled = !row.enabled;
        row.status = if row.enabled { RowStatus::Pending } else { RowStatus::Skipped };
    }
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
    s.sync.rows.clear();
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
                emit(Msg::Sync(Answer::CompareProgress {
                    job,
                    text: tr!("Lendo esquerda: {}…", "Reading left: {}…", left_loc.short_label()),
                }));
                let left_map = scan_location(left_loc, &options.filter, show_hidden, &pool, &servers_list, &drives, &cancel)?;

                if cancel.load(Ordering::Relaxed) {
                    return Err(t("Comparação cancelada.", "Comparison cancelled.").into());
                }
                emit(Msg::Sync(Answer::CompareProgress {
                    job,
                    text: tr!("Lendo direita: {}…", "Reading right: {}…", right_loc.short_label()),
                }));
                let right_map = scan_location(right_loc, &options.filter, show_hidden, &pool, &servers_list, &drives, &cancel)?;

                emit(Msg::Sync(Answer::CompareProgress {
                    job,
                    text: t("Comparando diferenças…", "Comparing differences…").into(),
                }));
                let (mut rows, filtered_out) =
                    compare_maps(*pair_index, left_loc, right_loc, left_map, right_map, &options, now, &cancel);
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
) -> Result<BTreeMap<String, ScannedItem>, String> {
    let mut out = BTreeMap::new();
    match root {
        SyncLocation::Local(dir) => {
            if !dir.is_dir() {
                return Err(tr!("Pasta não encontrada: {}", "Folder not found: {}", dir.display()));
            }
            scan_local_dir(dir, "", 0, filter, show_hidden, cancel, &mut out)?;
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
            scan_drive_dir(drive, folder_id, shared_drive_id.as_deref(), "", 0, filter, cancel, &mut out)?;
        }
        SyncLocation::Server { server_id, path, .. } => {
            let server = servers_list
                .iter()
                .find(|sv| sv.id == *server_id)
                .ok_or_else(|| t("Servidor não encontrado.", "Server not found.").to_string())?;
            let base = server.path(path);
            servers::with(pool, server, |conn| {
                scan_server_dir(conn, &base, "", 0, filter, show_hidden, cancel, &mut out)
            })?;
        }
    }
    Ok(out)
}

fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn scan_local_dir(
    dir: &Path,
    prefix: &str,
    depth: usize,
    filter: &SyncFilter,
    show_hidden: bool,
    cancel: &AtomicBool,
    out: &mut BTreeMap<String, ScannedItem>,
) -> Result<(), String> {
    if depth > MAX_DEPTH || cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    let entries = crate::model::read_local(dir, show_hidden).map_err(|e| e.to_string())?;
    for e in entries {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let rel = join_rel(prefix, &e.name);
        if e.is_dir {
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
                        meta: SideMeta { id: e.id, size: None, modified: e.modified, md5: None, drive_file: None },
                    },
                );
            }
            scan_local_dir(&dir.join(&e.name), &rel, depth + 1, filter, show_hidden, cancel, out)?;
        } else {
            out.insert(
                rel,
                ScannedItem {
                    is_dir: false,
                    meta: SideMeta { id: e.id, size: e.size, modified: e.modified, md5: None, drive_file: None },
                },
            );
        }
    }
    Ok(())
}

fn scan_drive_dir(
    drive: &Drive,
    folder_id: &str,
    shared_drive: Option<&str>,
    prefix: &str,
    depth: usize,
    filter: &SyncFilter,
    cancel: &AtomicBool,
    out: &mut BTreeMap<String, ScannedItem>,
) -> Result<(), String> {
    if depth > MAX_DEPTH || cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    let mut items = Vec::new();
    drive.list(folder_id, shared_drive, |page, _| {
        items.extend(page);
        !cancel.load(Ordering::Relaxed)
    })?;
    for f in items {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let rel = join_rel(prefix, &f.name);
        if f.is_dir {
            if filter.root_only {
                continue;
            }
            if filter.skip_filtered_dirs && filter.is_dir_excluded(&rel) {
                continue;
            }
            let child_id = f.target.clone().unwrap_or_else(|| f.id.clone());
            if !filter.is_dir_excluded(&rel) {
                out.insert(
                    rel.clone(),
                    ScannedItem {
                        is_dir: true,
                        meta: SideMeta {
                            id: child_id.clone(),
                            size: None,
                            modified: f.modified,
                            md5: None,
                            drive_file: Some(f),
                        },
                    },
                );
            }
            scan_drive_dir(drive, &child_id, shared_drive, &rel, depth + 1, filter, cancel, out)?;
        } else {
            out.insert(
                rel,
                ScannedItem {
                    is_dir: false,
                    meta: SideMeta {
                        id: f.id.clone(),
                        size: f.size,
                        modified: f.modified,
                        md5: f.md5.clone(),
                        drive_file: Some(f),
                    },
                },
            );
        }
    }
    Ok(())
}

fn scan_server_dir(
    conn: &mut dyn openexplorer_remote::Conn,
    remote_dir: &str,
    prefix: &str,
    depth: usize,
    filter: &SyncFilter,
    show_hidden: bool,
    cancel: &AtomicBool,
    out: &mut BTreeMap<String, ScannedItem>,
) -> Result<(), String> {
    if depth > MAX_DEPTH || cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    let items = conn.list(remote_dir)?;
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
            scan_server_dir(conn, &child_remote, &rel, depth + 1, filter, show_hidden, cancel, out)?;
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

fn compute_local_md5(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Md5::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

fn resolve_md5(loc: &SyncLocation, rel_path: &str, meta: &SideMeta) -> Option<String> {
    if let Some(h) = &meta.md5 {
        return Some(h.to_ascii_lowercase());
    }
    if let SyncLocation::Local(root) = loc {
        return compute_local_md5(&local_join_rel(root, rel_path));
    }
    None
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
    mut right: BTreeMap<String, ScannedItem>,
    options: &SyncOptions,
    now: SystemTime,
    cancel: &AtomicBool,
) -> (Vec<CompareRow>, usize) {
    let rules = options.effective_rules();
    let mut rows = Vec::new();
    let mut filtered_out = 0usize;

    // Collect union of keys in deterministic order
    let mut merged: BTreeMap<String, (Option<ScannedItem>, Option<ScannedItem>)> = BTreeMap::new();
    for (k, l) in left {
        let r = right.remove(&k);
        merged.insert(k, (Some(l), r));
    }
    for (k, r) in right {
        merged.insert(k, (None, Some(r)));
    }

    for (rel_path, (l_opt, r_opt)) in merged {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let is_dir = l_opt.as_ref().map(|x| x.is_dir).or_else(|| r_opt.as_ref().map(|x| x.is_dir)).unwrap_or(false);

        if is_dir {
            if options.filter.is_dir_excluded(&rel_path) {
                filtered_out += 1;
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
                status: RowStatus::Pending,
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
            continue;
        }

        let action = match (&l_opt, &r_opt) {
            (Some(_), None) => rules.left_only.to_sync_action(),
            (None, Some(_)) => rules.right_only.to_sync_action(),
            (Some(l), Some(r)) => decide_both_exist(left_loc, right_loc, &rel_path, &l.meta, &r.meta, &rules, options.compare_hash),
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
            status: RowStatus::Pending,
        });
    }

    (rows, filtered_out)
}

fn decide_both_exist(
    left_loc: &SyncLocation,
    right_loc: &SyncLocation,
    rel_path: &str,
    l: &SideMeta,
    r: &SideMeta,
    rules: &SyncRules,
    compare_hash: bool,
) -> SyncAction {
    // Optional hash comparison: if both sides provide MD5 and they match, files are identical.
    if compare_hash
        && l.size == r.size
        && let (Some(hl), Some(hr)) = (resolve_md5(left_loc, rel_path, l), resolve_md5(right_loc, rel_path, r))
        && hl == hr
    {
        return SyncAction::None;
    }

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
                    emit(Msg::Sync(Answer::SyncFinished { job, ok: 0, failed: planned.len(), cancelled: false }));
                    return;
                }
            }
        }

        // Order operations safely:
        // 1. Create directories (shallowest first)
        // 2. Copy files
        // 3. Delete files
        // 4. Delete directories (deepest first)
        let mut ordered = planned;
        ordered.sort_by(|a, b| {
            let phase = |r: &CompareRow| -> u8 {
                match (r.action, r.is_dir) {
                    (SyncAction::CopyRight | SyncAction::CopyLeft, true) => 0,
                    (SyncAction::CopyRight | SyncAction::CopyLeft, false) => 1,
                    (SyncAction::DeleteRight | SyncAction::DeleteLeft, false) => 2,
                    (SyncAction::DeleteRight | SyncAction::DeleteLeft, true) => 3,
                    _ => 4,
                }
            };
            let pa = phase(a);
            let pb = phase(b);
            pa.cmp(&pb).then_with(|| {
                let da = a.rel_path.matches('/').count();
                let db = b.rel_path.matches('/').count();
                if pa == 3 { db.cmp(&da) } else { da.cmp(&db) }
            })
        });

        let total = ordered.len();
        let mut ok = 0usize;
        let mut failed = 0usize;
        let mut done = 0usize;

        for row in &ordered {
            if cancel.load(Ordering::Relaxed) {
                emit(Msg::Sync(Answer::SyncFinished { job, ok, failed, cancelled: true }));
                return;
            }
            emit(Msg::Sync(Answer::RowStatus {
                job,
                row_id: row.id,
                status: RowStatus::Running,
                done,
                total,
            }));

            let res = catch_panic(t("sincronizar item", "sync item"), || {
                let pair = pairs
                    .get(row.pair_index)
                    .ok_or_else(|| t("Par de pastas inválido.", "Invalid folder pair.").to_string())?;
                let (Some(left), Some(right)) = (&pair.left, &pair.right) else {
                    return Err(t("Par de pastas incompleto.", "Incomplete folder pair.").into());
                };
                execute_row(row, left, right, skip_recycle_bin, segmented, &pool, &servers_list, &drives, &cancel)
            });

            done += 1;
            let status = match res {
                Ok(()) => {
                    ok += 1;
                    RowStatus::Done
                }
                Err(e) => {
                    failed += 1;
                    RowStatus::Failed(e)
                }
            };
            emit(Msg::Sync(Answer::RowStatus { job, row_id: row.id, status, done, total }));
        }

        emit(Msg::Sync(Answer::SyncFinished { job, ok, failed, cancelled: false }));
    });
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
) -> Result<(), String> {
    if is_dir {
        return ensure_dir(dst_root, rel_path, pool, servers_list, drives);
    }

    // Fast path: Local -> Local
    if let (SyncLocation::Local(src_base), SyncLocation::Local(dst_base)) = (src_root, dst_root) {
        let from = local_join_rel(src_base, rel_path);
        let to = local_join_rel(dst_base, rel_path);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::copy(&from, &to).map_err(|e| tr!("Falha ao copiar {}: {e}", "Failed to copy {}: {e}", rel_path))?;
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
            ensure_remote_parents(conn, &server.path(path), rel_path)?;
            conn.upload_file(&dst_remote, &from, &mut |_| !cancel.load(Ordering::Relaxed))
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
            conn.download(&src_remote, &mut f, &mut |_| !cancel.load(Ordering::Relaxed))
        });
    }

    // Fast path: Local -> Drive
    if let (SyncLocation::Local(src_base), SyncLocation::Drive { email, shared_drive_id, path, .. }) = (src_root, dst_root) {
        let from = local_join_rel(src_base, rel_path);
        let drive = drives
            .get(email)
            .ok_or_else(|| tr!("Conta não conectada: {email}", "Account not connected: {email}"))?;
        if let Some(existing) = dst_meta {
            drive.upload_version(&existing.id, &from, &mut |_| !cancel.load(Ordering::Relaxed))?;
        } else {
            let parent_id = ensure_drive_parents(drive, shared_drive_id.as_deref(), path, rel_path)?;
            let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
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
            .and_then(|m| m.drive_file.as_ref())
            .ok_or_else(|| t("Metadados do arquivo no Drive ausentes.", "Missing Drive file metadata.").to_string())?;
        drive.download(df, &to, segmented, &mut |_| !cancel.load(Ordering::Relaxed))?;
        return Ok(());
    }

    // General relay through a temporary file (Cloud/Server <-> Cloud/Server)
    let tmp_dir = std::env::temp_dir().join("openexplorer-sync");
    std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp_file = tmp_dir.join(format!("relay-{stamp}.tmp"));
    let _cleanup = TempGuard(tmp_file.clone());

    // Download source -> tmp_file
    match src_root {
        SyncLocation::Drive { email, .. } => {
            let drive = drives
                .get(email)
                .ok_or_else(|| tr!("Conta não conectada: {email}", "Account not connected: {email}"))?;
            let df = src_meta
                .and_then(|m| m.drive_file.as_ref())
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
                conn.download(&src_remote, &mut f, &mut |_| !cancel.load(Ordering::Relaxed))
            })?;
        }
        SyncLocation::Local(_) => unreachable!(),
    }

    // Upload tmp_file -> destination
    match dst_root {
        SyncLocation::Drive { email, shared_drive_id, path, .. } => {
            let drive = drives
                .get(email)
                .ok_or_else(|| tr!("Conta não conectada: {email}", "Account not connected: {email}"))?;
            if let Some(existing) = dst_meta {
                drive.upload_version(&existing.id, &tmp_file, &mut |_| !cancel.load(Ordering::Relaxed))?;
            } else {
                let parent_id = ensure_drive_parents(drive, shared_drive_id.as_deref(), path, rel_path)?;
                let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
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
                ensure_remote_parents(conn, &server.path(path), rel_path)?;
                conn.upload_file(&dst_remote, &tmp_file, &mut |_| !cancel.load(Ordering::Relaxed))
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

fn ensure_dir(
    root: &SyncLocation,
    rel_path: &str,
    pool: &Pool,
    servers_list: &[Server],
    drives: &HashMap<String, Arc<Drive>>,
) -> Result<(), String> {
    match root {
        SyncLocation::Local(base) => {
            std::fs::create_dir_all(local_join_rel(base, rel_path)).map_err(|e| e.to_string())
        }
        SyncLocation::Drive { email, shared_drive_id, path, .. } => {
            let drive = drives
                .get(email)
                .ok_or_else(|| tr!("Conta não conectada: {email}", "Account not connected: {email}"))?;
            let mut cur_id = path
                .last()
                .map(|(id, _)| id.clone())
                .or_else(|| shared_drive_id.clone())
                .unwrap_or_else(|| "root".into());
            for seg in rel_path.split('/').filter(|s| !s.is_empty()) {
                cur_id = find_or_create_drive_folder(drive, &cur_id, shared_drive_id.as_deref(), seg)?;
            }
            Ok(())
        }
        SyncLocation::Server { server_id, path, .. } => {
            let server = servers_list
                .iter()
                .find(|sv| sv.id == *server_id)
                .ok_or_else(|| t("Servidor não encontrado.", "Server not found.").to_string())?;
            let base = server.path(path);
            servers::with(pool, server, |conn| {
                let mut cur = base;
                for seg in rel_path.split('/').filter(|s| !s.is_empty()) {
                    cur = servers::child(&cur, seg);
                    let _ = conn.mkdir(&cur);
                }
                Ok(())
            })
        }
    }
}

fn ensure_remote_parents(conn: &mut dyn openexplorer_remote::Conn, base: &str, rel_path: &str) -> Result<(), String> {
    let segs: Vec<&str> = rel_path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() <= 1 {
        return Ok(());
    }
    let mut cur = base.to_string();
    for seg in &segs[..segs.len() - 1] {
        cur = servers::child(&cur, seg);
        let _ = conn.mkdir(&cur);
    }
    Ok(())
}

fn remote_join_rel(server: &Server, base_path: &[String], rel_path: &str) -> String {
    let mut segs = base_path.to_vec();
    segs.extend(rel_path.split('/').filter(|s| !s.is_empty()).map(str::to_string));
    server.path(&segs)
}

fn ensure_drive_parents(drive: &Drive, shared_drive: Option<&str>, base_path: &[(String, String)], rel_path: &str) -> Result<String, String> {
    let mut cur_id = base_path
        .last()
        .map(|(id, _)| id.clone())
        .or_else(|| shared_drive.map(str::to_string))
        .unwrap_or_else(|| "root".into());
    let segs: Vec<&str> = rel_path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() <= 1 {
        return Ok(cur_id);
    }
    for seg in &segs[..segs.len() - 1] {
        cur_id = find_or_create_drive_folder(drive, &cur_id, shared_drive, seg)?;
    }
    Ok(cur_id)
}

fn find_or_create_drive_folder(drive: &Drive, parent_id: &str, shared_drive: Option<&str>, name: &str) -> Result<String, String> {
    let mut found = None;
    drive.list(parent_id, shared_drive, |page, _| {
        for f in page {
            if f.is_dir && f.name == name {
                found = Some(f.target.unwrap_or(f.id));
                return false;
            }
        }
        true
    })?;
    if let Some(id) = found {
        Ok(id)
    } else {
        Ok(drive.create_folder(name, parent_id)?.id)
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

        let lmap = scan_location(&left_loc, &opts.filter, true, &pool, &[], &drives, &cancel).unwrap();
        let rmap = scan_location(&right_loc, &opts.filter, true, &pool, &[], &drives, &cancel).unwrap();

        // skipped dir should not be scanned at all
        assert!(!lmap.contains_key("ignored_dir/x.txt"));

        let (rows, filtered_out) = compare_maps(0, &left_loc, &right_loc, lmap, rmap, &opts, SystemTime::now(), &cancel);
        assert_eq!(filtered_out, 1); // skip.ini

        let by_path: HashMap<&str, SyncAction> = rows.iter().map(|r| (r.rel_path.as_str(), r.action)).collect();
        assert_eq!(by_path.get("a.txt"), Some(&SyncAction::CopyRight));
        assert_eq!(by_path.get("sub/b.txt"), Some(&SyncAction::CopyRight));
        assert_eq!(by_path.get("extra.txt"), Some(&SyncAction::DeleteRight));
        assert!(!by_path.contains_key("skip.ini"));

        // Execute the sync
        for row in &rows {
            if row.enabled && row.action != SyncAction::None {
                execute_row(row, &left_loc, &right_loc, true, false, &pool, &[], &drives, &cancel).unwrap();
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
        let lmap = scan_location(&SyncLocation::Local(left), &filter, true, &pool, &[], &drives, &cancel).unwrap();
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

        std::fs::write(left.join("same.txt"), b"identical-content").unwrap();
        std::fs::write(right.join("same.txt"), b"identical-content").unwrap();

        let left_loc = SyncLocation::Local(left);
        let right_loc = SyncLocation::Local(right);
        let t1 = SystemTime::now();
        let t2 = t1 - Duration::from_secs(60);
        let l_meta = SideMeta { id: "same.txt".into(), size: Some(17), modified: Some(t1), md5: None, drive_file: None };
        let r_meta = SideMeta { id: "same.txt".into(), size: Some(17), modified: Some(t2), md5: None, drive_file: None };
        let rules = SyncMode::Mirror.preset_rules().unwrap();

        assert_eq!(decide_both_exist(&left_loc, &right_loc, "same.txt", &l_meta, &r_meta, &rules, false), SyncAction::CopyRight);
        assert_eq!(decide_both_exist(&left_loc, &right_loc, "same.txt", &l_meta, &r_meta, &rules, true), SyncAction::None);

        let _ = std::fs::remove_dir_all(&root);
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
