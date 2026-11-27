//! Batch rename ("Renomear em massa"): independent resizable window for search & replace
//! (literal or regex, first or all occurrences, case-sensitive or case-insensitive) with live
//! before/after comparison, per-item selection, conflict validation and single-step undo across
//! local disk, Google Drive and remote servers.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use openexplorer_i18n::{t, tr};
use regex::{NoExpand, Regex, RegexBuilder};
use rune::{Anim, Color, Cursor, Cx, El, ScrollSpec, Spring, TextEdit, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, Dialog as Modal, ScrollBar};

use crate::actions;
use crate::icons;
use crate::media_rename::{self, ANIME_PRESETS, DataSource, EpisodeOrder, MOVIE_PRESETS, MatchMode, MediaKind, MediaLang, MediaPopup, MediaState, PrefDropdown, SERIES_PRESETS};
use crate::model::{Entry, Location, shown, typed};
use crate::undo::{self, Step};
use crate::{State, ops, servers, ui};

/// Active tab inside the "Renomear em massa" window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Simple,
    Media,
    Rules,
}

/// State of the independent "Renomear em massa" window.
pub struct BatchRename {
    pub mode: Mode,
    pub loc: Location,
    pub items: Vec<Entry>,
    /// Whether each item in `items` is included in the batch rename.
    pub checked: Vec<bool>,
    /// Names of sibling items in the same folder that are not part of `items`, used to detect
    /// collisions in the live preview.
    pub other_names: Vec<String>,
    pub search: TextEdit,
    pub replace: TextEdit,
    pub replace_all: bool,
    pub case_insensitive: bool,
    pub use_regex: bool,
    pub simple_analyze_open: bool,
    pub simple_analyze_input: TextEdit,
    /// State for the advanced "Mídia · TMDB / TVDB" tab.
    pub media: MediaState,
    /// State for the composable ReNamer-style "Regras (Avançado)" tab.
    pub rules: crate::rules_rename::RulesState,
}

/// Live preview of a single row in the batch rename table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowPreview {
    pub current: String,
    pub next: String,
    pub changed: bool,
    pub error: Option<String>,
}

/// Computed preview for the entire batch rename window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub regex_error: Option<String>,
    pub rows: Vec<RowPreview>,
    pub changed_count: usize,
    pub error_count: usize,
    /// Meta tags (hashes) still being computed in the background: the names aren't final yet.
    pub waiting: bool,
}

impl Preview {
    pub fn can_submit(&self) -> bool {
        self.regex_error.is_none() && self.changed_count > 0 && self.error_count == 0 && !self.waiting
    }
}

enum Matcher {
    Literal,
    LiteralCi(Regex),
    Regex(Regex),
}

impl BatchRename {
    pub fn new(loc: Location, items: Vec<Entry>, other_names: Vec<String>) -> Self {
        crate::rules_meta::clear_cache();
        let n = items.len();
        let media = MediaState::new(&items);
        let mut search = TextEdit::new("");
        search.focused = true;
        Self {
            mode: Mode::Simple,
            loc,
            items,
            checked: vec![true; n],
            other_names,
            search,
            replace: TextEdit::new(""),
            replace_all: false,
            case_insensitive: false,
            use_regex: false,
            simple_analyze_open: false,
            simple_analyze_input: TextEdit::new(""),
            media,
            rules: crate::rules_rename::RulesState::new(),
        }
    }

    pub fn analyze_simple_sample(&self, sample: &str) -> Vec<(String, String, Option<String>)> {
        let mode_str = format!(
            "{} · {} · {}",
            if self.use_regex { t("Expressão Regular", "Regular Expression") } else { t("Texto literal", "Literal text") },
            if self.replace_all { t("Todas as ocorrências", "All occurrences") } else { t("1ª ocorrência", "1st occurrence") },
            if self.case_insensitive { t("Ignorar maiúsc./minúsc.", "Case insensitive") } else { t("Sensível a maiúsc.", "Case sensitive") },
        );

        let search = self.search.text.as_str();
        let replace = self.replace.text.as_str();
        let mut steps = vec![(t("Configuração", "Configuration").into(), mode_str, None)];

        if search.is_empty() {
            steps.push((
                t("Busca", "Search").into(),
                t("Termo vazio (nenhuma alteração)", "Empty search term (no change)").into(),
                None,
            ));
            steps.push((t("Resultado", "Result").into(), sample.to_string(), None));
            return steps;
        }

        let (matcher, count) = if self.use_regex {
            match RegexBuilder::new(search).case_insensitive(self.case_insensitive).build() {
                Ok(re) => {
                    let c = re.find_iter(sample).count();
                    (Matcher::Regex(re), c)
                }
                Err(e) => {
                    steps.push((t("Busca", "Search").into(), String::new(), Some(format_regex_error(&e))));
                    steps.push((t("Resultado", "Result").into(), sample.to_string(), None));
                    return steps;
                }
            }
        } else if self.case_insensitive {
            match RegexBuilder::new(&regex::escape(search)).case_insensitive(true).build() {
                Ok(re) => {
                    let c = re.find_iter(sample).count();
                    (Matcher::LiteralCi(re), c)
                }
                Err(_) => (Matcher::Literal, sample.matches(search).count()),
            }
        } else {
            (Matcher::Literal, sample.matches(search).count())
        };

        steps.push((
            t("Busca", "Search").into(),
            tr!(
                "\"{search}\" → \"{replace}\" ({count} ocorrência(s))",
                "\"{search}\" → \"{replace}\" ({count} match(es))"
            ),
            None,
        ));

        let next = apply_replace(sample, search, replace, self.replace_all, &matcher);
        let name_err = actions::name_problem(&self.loc, next.trim());
        steps.push((t("Resultado", "Result").into(), next, name_err));
        steps
    }

    /// Computes the live before/after preview and validates invalid names and collisions.
    pub fn preview(&self) -> Preview {
        crate::rules_meta::begin_pass();
        let mut rows: Vec<RowPreview> = match self.mode {
            Mode::Simple => {
                let search = self.search.text.as_str();
                let replace = self.replace.text.as_str();

                let matcher = if search.is_empty() {
                    Matcher::Literal
                } else if self.use_regex {
                    match RegexBuilder::new(search).case_insensitive(self.case_insensitive).build() {
                        Ok(re) => Matcher::Regex(re),
                        Err(e) => {
                            let rows = self
                                .items
                                .iter()
                                .map(|item| {
                                    let current = shown(&item.name);
                                    RowPreview { next: current.clone(), current, changed: false, error: None }
                                })
                                .collect();
                            return Preview { regex_error: Some(format_regex_error(&e)), rows, changed_count: 0, error_count: 0, waiting: false };
                        }
                    }
                } else if self.case_insensitive {
                    match RegexBuilder::new(&regex::escape(search)).case_insensitive(true).build() {
                        Ok(re) => Matcher::LiteralCi(re),
                        Err(_) => Matcher::Literal,
                    }
                } else {
                    Matcher::Literal
                };

                self.items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let current = shown(&item.name);
                        let included = self.checked.get(i).copied().unwrap_or(false);
                        if !included || search.is_empty() {
                            return RowPreview { next: current.clone(), current, changed: false, error: None };
                        }
                        let next = apply_replace(&current, search, replace, self.replace_all, &matcher);
                        let changed = next != current;
                        RowPreview { current, next, changed, error: None }
                    })
                    .collect()
            }
            Mode::Media => self
                .items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let current = shown(&item.name);
                    let included = self.checked.get(i).copied().unwrap_or(false);
                    if !included {
                        return RowPreview { next: current.clone(), current, changed: false, error: None };
                    }
                    match self.media.format_item(i, &self.loc) {
                        Some(next) => {
                            let changed = next != current;
                            RowPreview { current, next, changed, error: None }
                        }
                        None => RowPreview { next: current.clone(), current, changed: false, error: None },
                    }
                })
                .collect(),
            Mode::Rules => crate::rules_rename::evaluate_rules(&self.rules.rules, &self.items, &self.checked, &self.loc)
                .into_iter()
                .enumerate()
                .map(|(i, (next, error))| {
                    let current = shown(&self.items[i].name);
                    let changed = next != current;
                    RowPreview { current, next, changed, error }
                })
                .collect(),
        };

        // Hashes still on their way: the names will change, so no conflict is judged yet.
        if crate::rules_meta::waited() {
            let changed_count = rows.iter().enumerate().filter(|(i, r)| self.checked.get(*i).copied().unwrap_or(false) && r.changed).count();
            return Preview { regex_error: None, rows, changed_count, error_count: 0, waiting: true };
        }

        // Collision & validity checks.
        let case_insensitive_fs = matches!(self.loc, Location::Local(_) | Location::Found(_));
        let norm = |name: &str| -> String {
            let trimmed = name.trim();
            if case_insensitive_fs {
                trimmed.to_lowercase()
            } else if matches!(self.loc, Location::Drive { .. } | Location::DriveSearch(_)) {
                typed(trimmed)
            } else {
                trimmed.to_string()
            }
        };

        let group_of = |item: &Entry| -> Option<PathBuf> {
            if matches!(self.loc, Location::Found(_)) {
                self.loc.local_path(item).and_then(|p| p.parent().map(|d| d.to_path_buf()))
            } else {
                None
            }
        };

        // Names that will remain occupied by items NOT being renamed (siblings + unchecked or unchanged items).
        let mut occupied: HashSet<(Option<PathBuf>, String)> = HashSet::new();
        if !matches!(self.loc, Location::Found(_)) {
            for name in &self.other_names {
                occupied.insert((None, norm(name)));
            }
        }
        for (i, item) in self.items.iter().enumerate() {
            let included = self.checked.get(i).copied().unwrap_or(false);
            if !included || !rows[i].changed {
                occupied.insert((group_of(item), norm(&rows[i].current)));
            }
        }

        // Count target names among active renames to catch duplicates within the batch.
        let mut target_counts: HashMap<(Option<PathBuf>, String), usize> = HashMap::new();
        for (i, item) in self.items.iter().enumerate() {
            let included = self.checked.get(i).copied().unwrap_or(false);
            if included && rows[i].changed {
                let key = (group_of(item), norm(&rows[i].next));
                *target_counts.entry(key).or_insert(0) += 1;
            }
        }

        // Servers rename one item at a time and refuse a name that's still taken, so a swap or a
        // chain (`a` → `b` while `b` → `c`) would stop halfway: the names the batch frees count
        // as taken there.
        let server = matches!(self.loc, Location::Server { .. });
        let freed: HashSet<(Option<PathBuf>, String)> = if server {
            self.items
                .iter()
                .enumerate()
                .filter(|(i, _)| self.checked.get(*i).copied().unwrap_or(false) && rows[*i].changed)
                .map(|(i, item)| (group_of(item), norm(&rows[i].current)))
                .collect()
        } else {
            HashSet::new()
        };

        let mut changed_count = 0;
        let mut error_count = 0;
        for (i, item) in self.items.iter().enumerate() {
            let included = self.checked.get(i).copied().unwrap_or(false);
            if included && rows[i].error.is_some() {
                error_count += 1;
                continue;
            }
            if !included || !rows[i].changed {
                continue;
            }
            changed_count += 1;
            let trimmed = rows[i].next.trim();
            let err = if let Some(why) = actions::name_problem(&self.loc, trimmed) {
                Some(why)
            } else {
                let key = (group_of(item), norm(trimmed));
                if target_counts.get(&key).copied().unwrap_or(0) > 1 {
                    Some(t("Nome duplicado", "Duplicate name").into())
                } else if occupied.contains(&key) {
                    Some(t("Já existe", "Already exists").into())
                } else if freed.contains(&key) && key.1 != norm(&rows[i].current) {
                    Some(t("Troca de nomes: faça em duas etapas neste servidor", "Name swap: do it in two steps on this server").into())
                } else {
                    None
                }
            };
            if err.is_some() {
                error_count += 1;
                rows[i].error = err;
            }
        }

        Preview { regex_error: None, rows, changed_count, error_count, waiting: false }
    }
}

fn apply_replace(input: &str, search: &str, replace: &str, replace_all: bool, matcher: &Matcher) -> String {
    match matcher {
        Matcher::Regex(re) => {
            if replace_all {
                re.replace_all(input, replace).into_owned()
            } else {
                re.replace(input, replace).into_owned()
            }
        }
        Matcher::LiteralCi(re) => {
            if replace_all {
                re.replace_all(input, NoExpand(replace)).into_owned()
            } else {
                re.replace(input, NoExpand(replace)).into_owned()
            }
        }
        Matcher::Literal => {
            if replace_all {
                input.replace(search, replace)
            } else {
                input.replacen(search, replace, 1)
            }
        }
    }
}

fn format_regex_error(err: &regex::Error) -> String {
    let raw = err.to_string();
    let concise = raw.lines().last().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&raw);
    tr!("Regex inválida: {concise}", "Invalid regex: {concise}")
}

/// Opens the independent batch rename window for `items` in pane `p`.
pub fn open(s: &mut State, p: usize, items: Vec<Entry>) {
    let loc = s.panes[p].loc.clone();
    let selected_ids: HashSet<&str> = items.iter().map(|e| e.id.as_str()).collect();
    let other_names: Vec<String> = if matches!(loc, Location::Found(_) | Location::DriveSearch(_)) {
        Vec::new()
    } else {
        s.panes[p].entries.iter().filter(|e| !selected_ids.contains(e.id.as_str())).map(|e| shown(&e.name)).collect()
    };
    s.open_batch_rename(BatchRename::new(loc, items, other_names));
}

/// Applies all checked, changed renames in a single background job with undo support.
pub fn submit(s: &mut State) {
    let Some(br) = &s.batch_rename else { return };
    let preview = br.preview();
    if !preview.can_submit() {
        return;
    }
    let Some(br) = s.batch_rename.take() else { return };
    s.close_batch_rename();

    let pairs: Vec<(Entry, String)> = br
        .items
        .into_iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let included = br.checked.get(i).copied().unwrap_or(false);
            let row = &preview.rows[i];
            if included && row.changed && row.error.is_none() {
                Some((item, row.next.trim().to_string()))
            } else {
                None
            }
        })
        .collect();

    if pairs.is_empty() {
        return;
    }

    let loc = br.loc;

    // Follow renamed folders if any open tab or pane is inside them.
    if !matches!(loc, Location::Mock(_) | Location::Found(_) | Location::Computer | Location::DriveSearch(_)) {
        for (item, shown_name) in &pairs {
            if item.is_dir {
                let raw = if matches!(loc, Location::Drive { .. }) { typed(shown_name) } else { shown_name.clone() };
                let from = crate::folder_in(&loc, item);
                if crate::tabs::any_inside(s, &from) {
                    let to = crate::folder_in(&loc, &Entry { name: raw, ..item.clone() });
                    s.renamed.push((loc.clone(), from, to));
                }
            }
        }
    }

    match &loc {
        Location::Local(_) | Location::Found(_) => {
            let jobs: Vec<(PathBuf, String)> = pairs
                .into_iter()
                .filter_map(|(item, name)| loc.local_path(&item).map(|from| (from, name)))
                .collect();
            if jobs.is_empty() {
                return;
            }
            undo::local_job(
                s,
                vec![loc.clone()],
                move |n| tr!("renomear em massa ({})", "batch rename ({})", undo::items(n)),
                move |rec| run_local_batch(jobs, rec),
            );
        }
        Location::Drive { email, .. } => {
            let who = email.clone();
            let reload = actions::account_panes_and(s, email, &loc);
            undo::drive_job(
                s,
                email,
                reload,
                move |n| tr!("renomear em massa ({})", "batch rename ({})", undo::items(n)),
                move |d, rec| {
                    let mut done = 0usize;
                    for (item, shown_name) in pairs {
                        let raw = typed(&shown_name);
                        let file = d.rename(&item.id, &raw).map_err(|e| batch_err(done, &shown(&item.name), &e))?;
                        rec.add([Step::DriveRename { email: who.clone(), id: item.id, from: file.name, to: item.name }]);
                        done += 1;
                    }
                    Ok(batch_done_msg(done))
                },
            );
        }
        Location::Server { server, .. } => {
            let at = openexplorer_remote::Server::clone(server);
            let moves: Vec<(String, String, String)> = pairs
                .into_iter()
                .filter_map(|(item, name)| {
                    let from = servers::path_of(&loc, &item)?;
                    let dir = from.rsplit_once('/').map_or("", |(d, _)| d);
                    let to = servers::child(dir, &name);
                    Some((item.name, from, to))
                })
                .collect();
            if moves.is_empty() {
                return;
            }
            undo::server_job(
                s,
                server,
                vec![loc.clone()],
                move |n| tr!("renomear em massa ({})", "batch rename ({})", undo::items(n)),
                move |c, rec| {
                    let req: Vec<(String, String)> = moves.iter().map(|(_, from, to)| (from.clone(), to.clone())).collect();
                    let results = c.rename_many(&req);
                    let mut done = 0usize;
                    let mut first_err: Option<(String, String)> = None;
                    for ((label, from, to), res) in moves.into_iter().zip(results) {
                        match res {
                            Ok(()) => {
                                rec.add([Step::ServerMove { server: at.clone(), from: to, to: from }]);
                                done += 1;
                            }
                            Err(e) => {
                                if first_err.is_none() {
                                    first_err = Some((label, e));
                                }
                            }
                        }
                    }
                    if let Some((label, e)) = first_err {
                        return Err(batch_err(done, &label, &e));
                    }
                    Ok(batch_done_msg(done))
                },
            );
        }
        Location::DriveSearch(_) => {
            let mut by_account: Vec<(String, Vec<(Entry, String)>)> = Vec::new();
            for (item, shown_name) in pairs {
                let Some(email) = actions::item_account(&loc, &item).map(str::to_string) else { continue };
                if let Some((_, group)) = by_account.iter_mut().find(|(e, _)| *e == email) {
                    group.push((item, shown_name));
                } else {
                    by_account.push((email, vec![(item, shown_name)]));
                }
            }
            for (email, group) in by_account {
                let who = email.clone();
                let reload = actions::account_panes_and(s, &email, &loc);
                undo::drive_job(
                    s,
                    &email,
                    reload,
                    move |n| tr!("renomear em massa ({})", "batch rename ({})", undo::items(n)),
                    move |d, rec| {
                        let mut done = 0usize;
                        for (item, shown_name) in group {
                            let raw = typed(&shown_name);
                            let file = d.rename(&item.id, &raw).map_err(|e| batch_err(done, &shown(&item.name), &e))?;
                            rec.add([Step::DriveRename { email: who.clone(), id: item.id, from: file.name, to: item.name }]);
                            done += 1;
                        }
                        Ok(batch_done_msg(done))
                    },
                );
            }
        }
        Location::Mock(_) | Location::Computer => {
            s.status = t("Aqui não dá para renomear itens.", "Items can't be renamed here.").into();
        }
    }
    s.notify_main();
}

/// Executes local disk batch renames safely, using a two-phase temporary rename whenever a target
/// path overlaps another source path in the same batch (e.g. `a -> b` and `b -> c`, or swaps).
fn run_local_batch(jobs: Vec<(PathBuf, String)>, rec: &undo::Rec) -> Result<String, String> {
    let source_keys: HashSet<String> = jobs.iter().map(|(from, _)| from.to_string_lossy().to_lowercase()).collect();
    let needs_staging = jobs.iter().any(|(from, name)| {
        let to = from.with_file_name(name.trim());
        let from_key = from.to_string_lossy().to_lowercase();
        let to_key = to.to_string_lossy().to_lowercase();
        from_key != to_key && source_keys.contains(&to_key)
    });

    if !needs_staging {
        let mut done = 0usize;
        for (from, name) in jobs {
            let label = from.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            let to = ops::rename(&from, &name).map_err(|e| batch_err(done, &label, &e))?;
            rec.add([Step::Move { from: to, to: from }]);
            done += 1;
        }
        return Ok(batch_done_msg(done));
    }

    // Swaps and chains go together through temporary names; a failure puts every item back.
    let n = jobs.len();
    let moves: Vec<(PathBuf, PathBuf)> = jobs.into_iter().map(|(from, name)| {
        let to = from.with_file_name(name.trim());
        (from, to)
    }).collect();
    ops::rename_together(&moves)?;
    // One step takes them all back, as separate moves would each find their name still taken.
    rec.add_items([Step::MoveMany { moves: moves.into_iter().map(|(from, to)| (to, from)).collect() }], n);
    Ok(batch_done_msg(n))
}

fn batch_done_msg(done: usize) -> String {
    if done == 1 {
        t("Renomeado: 1 item.", "Renamed: 1 item.").into()
    } else {
        tr!("Renomeados: {done} itens.", "Renamed: {done} items.")
    }
}

fn batch_err(done: usize, item: &str, err: &str) -> String {
    if done == 0 {
        tr!("Não consegui renomear \"{item}\": {err}", "Couldn't rename \"{item}\": {err}")
    } else {
        tr!(
            "Não consegui renomear \"{item}\": {err} (já renomeados: {done})",
            "Couldn't rename \"{item}\": {err} (already renamed: {done})"
        )
    }
}

// ---------------------------------------------------------------------------------------------
// Independent Window UI

pub fn window_title() -> &'static str {
    t("Renomear em massa", "Batch rename")
}

/// Renders the independent, movable and resizable "Renomear em massa" native window.
pub fn batch_rename_window_view(s: &State, cx: &mut Cx<State>) -> El {
    if s.is_placeholder {
        return el();
    }
    // Its hover is instant, as the window's own (`hover_motion_within` below).
    let k = ui::window_kit(s, cx, Anim::INSTANT);
    cx.caption_color(k.c.text_base);

    let window_bg = if k.dark { rune::oklch(0.24, 0.0, 0.0) } else { k.c.page };
    let panel_bg = if k.dark { rune::oklch(0.198, 0.0, 0.0) } else { k.c.bg_base };
    let panel_border = if k.dark { rune::oklch(0.285, 0.0, 0.0) } else { k.c.border_weak };
    let accent_bg = k.c.bg_icon_button_contrast;

    let title_leading = row()
        .gap(8.0)
        .center_y()
        .child(icon(&icons::M_RENAME, 15.0).color(k.c.text_accent))
        .child(text(window_title()).font(k.ty.text_small).color(k.c.text_base));

    let titlebar = rune_kit::WindowTitlebar::from_insets(cx.chrome_insets())
        .key("batch-rename-titlebar")
        .leading_padding(14.0)
        .fill(window_bg)
        .leading(title_leading)
        .build(&k);

    let default_br;
    let br = match &s.batch_rename {
        Some(br) => br.as_ref(),
        None => {
            default_br = BatchRename::new(Location::Local(PathBuf::new()), Vec::new(), Vec::new());
            &default_br
        }
    };

    let preview = br.preview();
    let total = br.items.len();
    let checked_count = br.checked.iter().filter(|c| **c).count();
    let all_checked = total > 0 && checked_count == total;

    // Mode switcher tabs ("Simples" | "Mídia · TMDB / TVDB" | "Regras (Avançado)")
    let tab_btn = |key: &'static str, label: &'static str, target: Mode| -> El {
        let active = br.mode == target;
        row()
            .key(key)
            .role("tab")
            .h(28.0)
            .px(12.0)
            .center_y()
            .radius(k.radius.sm)
            .cursor(Cursor::Pointer)
            .fill(if active { k.alpha(k.c.text_accent, if k.dark { 0.22 } else { 0.12 }) } else { Color::TRANSPARENT })
            .hover_fill(if active { k.alpha(k.c.text_accent, if k.dark { 0.28 } else { 0.16 }) } else { k.c.hover })
            .border(1.0, if active { k.alpha(k.c.text_accent, 0.45) } else { Color::TRANSPARENT })
            .on_press(move |s: &mut State| {
                if let Some(b) = &mut s.batch_rename {
                    b.mode = target;
                }
            })
            .child(
                text(label)
                    .font(if active { k.ty.text_xsmall_medium } else { k.ty.text_xsmall })
                    .color(if active { k.c.text_accent } else { k.c.text_muted }),
            )
    };

    let mode_tabs = row()
        .key("mode-tabs")
        .w_full()
        .gap(6.0)
        .center_y()
        .child(tab_btn("tab-simple", t("Simples (Procurar e substituir)", "Simple (Search & replace)"), Mode::Simple))
        .child(tab_btn("tab-media", t("Mídia · TMDB / TVDB", "Media · TMDB / TVDB"), Mode::Media))
        .child(tab_btn("tab-rules", t("Regras (Avançado)", "Rules (Advanced)"), Mode::Rules));

    let mut layer: Vec<El> = Vec::new();

    let content = match br.mode {
        Mode::Simple => {
            let search_placeholder = if br.use_regex {
                t("Ex.: ^PAD-(.+)", "e.g. ^PAD-(.+)")
            } else {
                t("Texto a procurar…", "Text to search…")
            };
            let replace_placeholder = if br.use_regex {
                t("Ex.: $1 (vazio remove)", "e.g. $1 (empty removes)")
            } else {
                t("Substituir por (vazio remove)…", "Replace with (empty removes)…")
            };

            let search_input = crate::rules_ui::field_box(
                &k,
                t("Procurar por", "Search for"),
                search_placeholder,
                Some(&icons::SEARCH),
                preview.regex_error.is_some(),
                &br.search,
                |s: &mut State| match &mut s.batch_rename {
                    Some(br) => &mut br.search,
                    None => &mut s.name_edit,
                },
                Some(|s: &mut State, _: &mut Tx| submit(s)),
            );

            let replace_input = crate::rules_ui::field_box(
                &k,
                t("Substituir por", "Replace with"),
                replace_placeholder,
                None,
                false,
                &br.replace,
                |s: &mut State| match &mut s.batch_rename {
                    Some(br) => &mut br.replace,
                    None => &mut s.name_edit,
                },
                Some(|s: &mut State, _: &mut Tx| submit(s)),
            );

            let inputs_row = row()
                .key("criteria-inputs")
                .w_full()
                .gap(10.0)
                .child(
                    col()
                        .key("batch-search-wrap")
                        .grow(1.0)
                        .w(0.0)
                        .min_w(0.0)
                        .gap(4.0)
                        .child(text(t("Procurar por", "Search for")).font(k.ty.text_xsmall_medium).color(k.c.text_base))
                        .child(search_input),
                )
                .child(
                    col()
                        .key("batch-replace-wrap")
                        .grow(1.0)
                        .w(0.0)
                        .min_w(0.0)
                        .gap(4.0)
                        .child(text(t("Substituir por", "Replace with")).font(k.ty.text_xsmall_medium).color(k.c.text_base))
                        .child(replace_input),
                );

            let options_row = row()
                .key("criteria-options")
                .w_full()
                .gap(18.0)
                .center_y()
                .child(crate::rules_ui::instant_checkbox(
                    &k,
                    accent_bg,
                    "opt-replace-all".into(),
                    br.replace_all,
                    t("Substituir todas as ocorrências", "Replace all occurrences"),
                    |s| {
                        if let Some(br) = &mut s.batch_rename {
                            br.replace_all = !br.replace_all;
                        }
                    },
                ))
                .child(crate::rules_ui::instant_checkbox(
                    &k,
                    accent_bg,
                    "opt-case-insensitive".into(),
                    br.case_insensitive,
                    t("Ignorar maiúsculas/minúsculas", "Case insensitive"),
                    |s| {
                        if let Some(br) = &mut s.batch_rename {
                            br.case_insensitive = !br.case_insensitive;
                        }
                    },
                ))
                .child(crate::rules_ui::instant_checkbox(
                    &k,
                    accent_bg,
                    "opt-regex".into(),
                    br.use_regex,
                    t("Usar Regex", "Use Regex"),
                    |s| {
                        if let Some(br) = &mut s.batch_rename {
                            br.use_regex = !br.use_regex;
                        }
                    },
                ));

            let mut criteria_card = col()
                .key("criteria-card")
                .w_full()
                .padding(12.0)
                .gap(10.0)
                .radius(k.radius.md)
                .fill(panel_bg)
                .border(1.0, panel_border)
                .child(inputs_row)
                .child(options_row);

            if let Some(err) = &preview.regex_error {
                criteria_card = criteria_card.child(
                    text(err.clone())
                        .key("regex-error")
                        .font(k.ty.text_xsmall)
                        .color(k.c.danger.fg)
                        .wrap()
                        .w_full()
                        .min_w(0.0),
                );
            }

            let has_fields = !br.search.text.is_empty() || !br.replace.text.is_empty();
            let util_bar = crate::rules_ui::shared_util_bar(
                &k,
                total,
                "btn-simple-analyze",
                total > 0,
                |s| {
                    if let Some(b) = &mut s.batch_rename {
                        let sample = b.items.first().map(|e| shown(&e.name)).unwrap_or_default();
                        b.simple_analyze_input = TextEdit::new(sample);
                        b.simple_analyze_open = true;
                    }
                },
                "btn-simple-export",
                "btn-simple-clear",
                t("Limpar campos", "Clear fields"),
                has_fields,
                |s| {
                    if let Some(b) = &mut s.batch_rename {
                        let mut search = TextEdit::new("");
                        search.focused = true;
                        b.search = search;
                        b.replace = TextEdit::new("");
                    }
                },
            );

            let preview_panel = crate::rules_ui::shared_preview_panel(&k, br, &preview, all_checked, panel_bg, panel_border, accent_bg);
            let footer = crate::rules_ui::shared_footer(
                &k,
                br,
                &preview,
                preview.can_submit(),
                preview.regex_error.as_deref(),
            );

            if br.simple_analyze_open {
                let steps = br.analyze_simple_sample(&br.simple_analyze_input.text);
                crate::rules_ui::render_steps_analyze_modal(
                    &k,
                    "simple-analyze-modal",
                    t("Analisar Substituição", "Analyze Replace"),
                    &br.simple_analyze_input,
                    |s| match &mut s.batch_rename {
                        Some(b) => &mut b.simple_analyze_input,
                        None => &mut s.name_edit,
                    },
                    steps,
                    |s| {
                        if let Some(b) = &mut s.batch_rename {
                            b.simple_analyze_open = false;
                        }
                    },
                    panel_border,
                    &mut layer,
                );
            }

            col()
                .key("content")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .px(16.0)
                .pt(6.0)
                .pb(14.0)
                .gap(8.0)
                .motion(Spring::NONE)
                .instant_changes()
                .child(mode_tabs)
                .child(criteria_card)
                .child(util_bar)
                .child(preview_panel)
                .child(footer)
        }
        Mode::Media => media_workspace(s, &k, br, &preview, all_checked, panel_bg, panel_border, accent_bg, mode_tabs, &mut layer),
        Mode::Rules => crate::rules_ui::rules_workspace(s, &k, br, &preview, all_checked, panel_bg, panel_border, accent_bg, mode_tabs, &mut layer),
    };

    let root = col()
        .key("batch-rename-window")
        .w_full()
        .h_full()
        .fill(window_bg)
        .child(titlebar)
        .child(content)
        .hover_motion_within(Anim::INSTANT, Anim::INSTANT);

    rune_kit::root(root, layer)
}

/// Renders the ReNamer-consistent "Mídia · TMDB / TVDB" workspace:
/// Top action toolbar (Match, Buscar título, Presets, Editar formato, Preferências),
/// compact Match & Format summary panel, shared utility bar (Analisar, Copiar prévia, Limpar match),
/// resizable 4-column preview grid (Estado | Nome | Novo Nome | Erro), and unified footer.
#[allow(clippy::too_many_arguments)]
fn media_workspace(
    s: &State,
    k: &rune_kit::Kit,
    br: &BatchRename,
    preview: &Preview,
    all_checked: bool,
    panel_bg: Color,
    panel_border: Color,
    accent_bg: Color,
    mode_tabs: El,
    layer: &mut Vec<El>,
) -> El {
    let m = &br.media;
    let total = br.items.len();
    let busy = m.searching || m.loading_episodes;
    let can_submit = preview.can_submit() && !busy;
    let has_any_key = !m.keys.tmdb.is_empty() || !m.keys.tvdb.is_empty();

    let match_btn_label = if m.searching {
        t("Buscando…", "Matching…")
    } else if m.loading_episodes {
        t("Episódios…", "Episodes…")
    } else {
        "Match"
    };

    let media_toolbar = row()
        .key("media-toolbar")
        .w_full()
        .gap(6.0)
        .center_y()
        .child(
            row()
                .key("btn-media-match")
                .role("button")
                .h(28.0)
                .px(10.0)
                .gap(5.0)
                .center_y()
                .radius(k.radius.sm)
                .fill(k.alpha(k.c.text_accent, if k.dark { 0.20 } else { 0.12 }))
                .hover_fill(k.alpha(k.c.text_accent, if k.dark { 0.28 } else { 0.18 }))
                .border(1.0, k.alpha(k.c.text_accent, 0.45))
                .cursor(Cursor::Pointer)
                .on_press(|s: &mut State| {
                    if let Some(b) = &mut s.batch_rename {
                        b.media.popup = Some(MediaPopup::MatchMenu);
                    }
                })
                .child(icon(&icons::SEARCH, 12.0).color(k.c.text_accent))
                .child(text(match_btn_label).font(k.ty.text_xsmall_medium).color(k.c.text_accent)),
        )
        .child(crate::rules_ui::tool_btn(k, "btn-media-pick", t("Buscar título…", "Pick title…"), !busy, |s| {
            if let Some(b) = &mut s.batch_rename {
                b.media.popup = Some(MediaPopup::SelectMatch);
            }
        }))
        .child(crate::rules_ui::tool_btn(k, "btn-media-presets", t("Presets", "Presets"), true, |s| {
            if let Some(b) = &mut s.batch_rename {
                b.media.popup = Some(MediaPopup::PresetsMenu);
            }
        }))
        .child(crate::rules_ui::tool_btn(k, "btn-media-format", t("Editar formato", "Edit format"), true, |s| {
            if let Some(b) = &mut s.batch_rename {
                b.media.popup = Some(MediaPopup::EditFormat);
            }
        }))
        .child(el().grow(1.0))
        .child(
            row()
                .key("media-prefs-btn")
                .role("button")
                .h(28.0)
                .px(10.0)
                .gap(6.0)
                .center_y()
                .radius(k.radius.sm)
                .fill(k.c.surface_raised)
                .hover_fill(k.c.hover)
                .border(1.0, k.c.border_weak)
                .cursor(Cursor::Pointer)
                .on_press(|s: &mut State| {
                    if let Some(b) = &mut s.batch_rename {
                        b.media.popup = Some(MediaPopup::Preferences);
                    }
                })
                .child(el().size(6.0, 6.0).radius(3.0).fill(if has_any_key { k.c.success.fg } else { k.c.warning.fg }))
                .child(text(t("Preferências", "Preferences")).font(k.ty.text_xsmall_medium).color(k.c.text_base)),
        );

    let cfg_summary = format!(
        "{} · {} · {} · {} · {}",
        m.source.label(),
        m.kind.label(),
        m.order.label(),
        m.lang.label(),
        m.match_mode.label(),
    );

    let panel_header = row()
        .key("media-panel-header")
        .w_full()
        .h(28.0)
        .px(10.0)
        .gap(8.0)
        .center_y()
        .fill(k.c.surface_raised)
        .border_sides([0.0, 0.0, 1.0, 0.0], panel_border)
        .child(text(t("Correspondência & Formato", "Match & Format")).font(k.ty.text_xsmall_medium).color(k.c.text_muted))
        .child(el().grow(1.0))
        .child(text(cfg_summary).font(k.ty.caption()).color(k.c.text_faint).truncate());

    let panel_body: El = if let Some(cand) = m.active_candidate() {
        let thumb: El = match cand.poster_url.as_deref().and_then(|u| s.posters.get(u)) {
            Some(img) => el().size(32.0, 48.0).radius(3.0).pixels(img.pixels()),
            None => row()
                .size(32.0, 48.0)
                .radius(3.0)
                .center()
                .fill(k.c.surface_raised)
                .border(1.0, k.c.border_weak)
                .child(icon(&icons::VIDEO, 15.0).color(k.c.text_accent)),
        };
        let title_str = match cand.year {
            Some(y) => format!("{} ({y})", cand.title),
            None => cand.title.clone(),
        };
        let ep_info = if m.loading_episodes {
            t("Carregando lista de episódios…", "Loading episode list…").to_string()
        } else if cand.kind == MediaKind::Series {
            tr!(
                "Formato: {} · {} episódios carregados",
                "Format: {} · {} episodes loaded",
                m.format.text,
                m.episodes.len()
            )
        } else {
            tr!("Formato: {}", "Format: {}", m.format.text)
        };
        let mut info_col = col()
            .grow(1.0)
            .w(0.0)
            .min_w(0.0)
            .gap(3.0)
            .child(text(title_str).font(k.ty.text_small_medium).color(k.c.text_accent).w_full().min_w(0.0).truncate())
            .child(text(ep_info).font(k.ty.caption()).color(k.c.text_muted).w_full().min_w(0.0).truncate());
        if let Some(err) = &m.error {
            info_col = info_col.child(text(err.clone()).font(k.ty.caption()).color(k.c.danger.fg).w_full().min_w(0.0).truncate());
        }
        row()
            .key("media-active-match")
            .w_full()
            .min_h(72.0)
            .px(12.0)
            .py(8.0)
            .gap(12.0)
            .center_y()
            .child(thumb)
            .child(info_col)
            .child(crate::rules_ui::tool_btn(k, "btn-media-change", t("Trocar…", "Change…"), true, |s| {
                if let Some(b) = &mut s.batch_rename {
                    b.media.popup = Some(MediaPopup::SelectMatch);
                }
            }))
    } else {
        let mut inner = col()
            .gap(3.0)
            .items_center()
            .child(
                row()
                    .gap(6.0)
                    .center_y()
                    .child(icon(&icons::SEARCH, 14.0).color(k.c.text_accent))
                    .child(
                        text(t(
                            "Clique aqui ou em Match para identificar no TMDB ou TVDB",
                            "Click here or Match to identify via TMDB or TVDB",
                        ))
                        .font(k.ty.body_medium)
                        .color(k.c.text_muted),
                    ),
            )
            .child(
                text(tr!(
                    "Detectado: \"{}\" · Formato: {}",
                    "Detected: \"{}\" · Format: {}",
                    m.query.text,
                    m.format.text
                ))
                .font(k.ty.caption())
                .color(k.c.text_faint),
            );
        if let Some(err) = &m.error {
            inner = inner.child(text(err.clone()).font(k.ty.caption()).color(k.c.danger.fg));
        }
        row()
            .key("media-empty-click")
            .role("button")
            .w_full()
            .min_h(72.0)
            .px(14.0)
            .py(8.0)
            .center()
            .cursor(Cursor::Pointer)
            .hover_fill(k.c.hover)
            .on_press(|s: &mut State| {
                if let Some(b) = &mut s.batch_rename {
                    b.media.popup = Some(MediaPopup::MatchMenu);
                }
            })
            .child(inner)
    };

    let media_panel = col()
        .key("media-summary-panel")
        .w_full()
        .radius(k.radius.md)
        .fill(panel_bg)
        .border(1.0, panel_border)
        .clip_soft(0.0)
        .child(panel_header)
        .child(panel_body);

    let can_clear = m.selected.is_some() || !m.candidates.is_empty() || m.error.is_some();
    let util_bar = crate::rules_ui::shared_util_bar(
        k,
        total,
        "btn-media-analyze",
        total > 0,
        |s| {
            if let Some(b) = &mut s.batch_rename {
                let sample = b.items.first().map(|e| shown(&e.name)).unwrap_or_default();
                b.media.analyze_input = TextEdit::new(sample);
                b.media.popup = Some(MediaPopup::Analyze);
            }
        },
        "btn-media-export",
        "btn-media-clear",
        t("Limpar match", "Clear match"),
        can_clear,
        |s| {
            if let Some(b) = &mut s.batch_rename {
                b.media.clear_match();
            }
        },
    );

    let preview_panel = crate::rules_ui::shared_preview_panel(k, br, preview, all_checked, panel_bg, panel_border, accent_bg);
    let footer = crate::rules_ui::shared_footer(k, br, preview, can_submit, m.error.as_deref());

    // Render any open submenu / modal in the overlay layer.
    if let Some(popup) = m.popup {
        render_media_popup(s, k, br, popup, panel_border, layer);
    }

    col()
        .key("content")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .px(16.0)
        .pt(6.0)
        .pb(14.0)
        .gap(8.0)
        .motion(Spring::NONE)
        .instant_changes()
        .child(mode_tabs)
        .child(media_toolbar)
        .child(media_panel)
        .child(util_bar)
        .child(preview_panel)
        .child(footer)
}

fn close_popup(s: &mut State) {
    if let Some(b) = &mut s.batch_rename {
        b.media.close_popup();
        b.media.open_dropdown = None;
    }
}

fn render_media_popup(s: &State, k: &rune_kit::Kit, br: &BatchRename, popup: MediaPopup, panel_border: Color, layer: &mut Vec<El>) {
    let m = &br.media;
    let pad = |e: El| col().key("popup-content").w_full().px(16.0).pb(16.0).child(e);

    let menu_item = |key: String, title: String, subtitle: Option<String>, active: bool, on_press: Box<dyn Fn(&mut State) + 'static>| -> El {
        let mut r = row()
            .key(key)
            .role("button")
            .w_full()
            .min_h(34.0)
            .px(10.0)
            .py(6.0)
            .gap(8.0)
            .center_y()
            .radius(k.radius.sm)
            .cursor(Cursor::Pointer)
            .fill(if active { k.alpha(k.c.text_accent, if k.dark { 0.18 } else { 0.10 }) } else { Color::TRANSPARENT })
            .hover_fill(k.c.hover)
            .on_press(move |s: &mut State| on_press(s))
            .child(
                text(title)
                    .font(if active { k.ty.text_small_medium } else { k.ty.text_small })
                    .color(if active { k.c.text_accent } else { k.c.text_base })
                    .grow(1.0)
                    .min_w(0.0)
                    .truncate(),
            );
        if let Some(sub) = subtitle {
            r = r.child(text(sub).font(k.ty.caption()).color(k.c.text_faint));
        }
        r
    };

    let section_title = |label: &'static str| -> El {
        text(label).font(k.ty.caption()).color(k.c.text_faint).px(10.0).pt(4.0).pb(2.0)
    };
    let divider = || -> El { el().w_full().h(1.0).my(3.0).fill(panel_border) };

    match popup {
        MediaPopup::MatchMenu => {
            let is_anime = m.kind == MediaKind::Series && m.order == EpisodeOrder::Absolute;
            let is_series = m.kind == MediaKind::Series && m.order != EpisodeOrder::Absolute;
            let is_movie = m.kind == MediaKind::Movie;
            let body = col()
                .w_full()
                .gap(2.0)
                // Episode Mode (Series)
                .child(section_title(t("Modo Episódio (Séries):", "Episode Mode (Series):")))
                .child(menu_item(
                    "match-tmdb-tv".into(),
                    "TheMovieDB".into(),
                    Some("TMDB".into()),
                    is_series && m.source == DataSource::Tmdb,
                    Box::new(|s| media_rename::match_with(s, DataSource::Tmdb, MediaKind::Series)),
                ))
                .child(menu_item(
                    "match-tvdb-tv".into(),
                    "TheTVDB".into(),
                    Some("TVDB".into()),
                    is_series && m.source == DataSource::Tvdb,
                    Box::new(|s| media_rename::match_with(s, DataSource::Tvdb, MediaKind::Series)),
                ))
                .child(divider())
                // Anime Mode (Absolute numbering)
                .child(section_title(t("Modo Anime:", "Anime Mode:")))
                .child(menu_item(
                    "match-tmdb-anime".into(),
                    "TheMovieDB".into(),
                    Some(t("Absoluto", "Absolute").into()),
                    is_anime && m.source == DataSource::Tmdb,
                    Box::new(|s| media_rename::match_anime(s, DataSource::Tmdb)),
                ))
                .child(menu_item(
                    "match-tvdb-anime".into(),
                    "TheTVDB".into(),
                    Some(t("Absoluto", "Absolute").into()),
                    is_anime && m.source == DataSource::Tvdb,
                    Box::new(|s| media_rename::match_anime(s, DataSource::Tvdb)),
                ))
                .child(divider())
                // Movie Mode
                .child(section_title(t("Modo Filme:", "Movie Mode:")))
                .child(menu_item(
                    "match-tmdb-mv".into(),
                    "TheMovieDB".into(),
                    Some("TMDB".into()),
                    is_movie && m.source == DataSource::Tmdb,
                    Box::new(|s| media_rename::match_with(s, DataSource::Tmdb, MediaKind::Movie)),
                ))
                .child(menu_item(
                    "match-tvdb-mv".into(),
                    "TheTVDB".into(),
                    Some("TVDB".into()),
                    is_movie && m.source == DataSource::Tvdb,
                    Box::new(|s| media_rename::match_with(s, DataSource::Tvdb, MediaKind::Movie)),
                ))
                .child(divider())
                // Smart Mode
                .child(section_title(t("Modo Inteligente:", "Smart Mode:")))
                .child(menu_item(
                    "match-auto".into(),
                    t("Automático", "Automatic").into(),
                    Some(t("Detecta série, anime ou filme", "Auto-detects series, anime or movie").into()),
                    false,
                    Box::new(media_rename::match_auto),
                ))
                .child(menu_item(
                    "match-manual".into(),
                    t("Escolher título manualmente…", "Pick title manually…").into(),
                    None,
                    false,
                    Box::new(|s| {
                        if let Some(b) = &mut s.batch_rename {
                            b.media.popup = Some(MediaPopup::SelectMatch);
                        }
                    }),
                ))
                .child(divider())
                // Options
                .child(section_title(t("Opções:", "Options:")))
                .child(menu_item(
                    "match-edit-format".into(),
                    t("Editar formato…", "Edit Format…").into(),
                    None,
                    false,
                    Box::new(|s| {
                        if let Some(b) = &mut s.batch_rename {
                            b.media.popup = Some(MediaPopup::EditFormat);
                        }
                    }),
                ))
                .child(menu_item(
                    "match-preferences".into(),
                    t("Preferências…", "Preferences…").into(),
                    None,
                    false,
                    Box::new(|s| {
                        if let Some(b) = &mut s.batch_rename {
                            b.media.popup = Some(MediaPopup::Preferences);
                        }
                    }),
                ));
            Modal::new("media-match-menu", t("Buscar e corresponder dados", "Fetch & Match Data"))
                .open(true)
                .child(pad(body))
                .width(340.0)
                .fit()
                .on_close(close_popup)
                .build(k, layer);
        }
        MediaPopup::PresetsMenu => {
            let mut body = col().w_full().gap(2.0);
            body = body.child(section_title(t("Séries:", "Series:")));
            for (idx, p) in SERIES_PRESETS.iter().enumerate() {
                let tpl = p.template;
                let active = m.format.text.trim() == tpl;
                body = body.child(menu_item(
                    format!("preset-s-{idx}"),
                    p.label().into(),
                    Some(tpl.into()),
                    active,
                    Box::new(move |s| {
                        if let Some(b) = &mut s.batch_rename {
                            b.media.format = TextEdit::new(tpl);
                            b.media.popup = None;
                        }
                    }),
                ));
            }
            body = body.child(divider()).child(section_title(t("Animes:", "Anime:")));
            for (idx, p) in ANIME_PRESETS.iter().enumerate() {
                let tpl = p.template;
                let active = m.format.text.trim() == tpl;
                body = body.child(menu_item(
                    format!("preset-a-{idx}"),
                    p.label().into(),
                    Some(tpl.into()),
                    active,
                    Box::new(move |s| {
                        if let Some(b) = &mut s.batch_rename {
                            b.media.format = TextEdit::new(tpl);
                            b.media.order = EpisodeOrder::Absolute;
                            b.media.popup = None;
                        }
                    }),
                ));
            }
            body = body.child(divider()).child(section_title(t("Filmes:", "Movies:")));
            for (idx, p) in MOVIE_PRESETS.iter().enumerate() {
                let tpl = p.template;
                let active = m.format.text.trim() == tpl;
                body = body.child(menu_item(
                    format!("preset-m-{idx}"),
                    p.label().into(),
                    Some(tpl.into()),
                    active,
                    Box::new(move |s| {
                        if let Some(b) = &mut s.batch_rename {
                            b.media.format = TextEdit::new(tpl);
                            b.media.popup = None;
                        }
                    }),
                ));
            }
            body = body.child(divider()).child(menu_item(
                "preset-edit".into(),
                t("Editar formato personalizado…", "Edit custom format…").into(),
                None,
                false,
                Box::new(|s| {
                    if let Some(b) = &mut s.batch_rename {
                        b.media.popup = Some(MediaPopup::EditFormat);
                    }
                }),
            ));
            Modal::new("media-presets-menu", t("Presets", "Presets"))
                .open(true)
                .child(pad(body))
                .width(440.0)
                .fit()
                .on_close(close_popup)
                .build(k, layer);
        }
        MediaPopup::EditFormat => {
            let input = crate::rules_ui::field_box(
                k,
                t("Formato", "Format"),
                "{n} - {s00e00} - {t}",
                None,
                false,
                &m.format,
                |s: &mut State| match &mut s.batch_rename {
                    Some(b) => &mut b.media.format,
                    None => &mut s.name_edit,
                },
                Some(|s: &mut State, _: &mut Tx| close_popup(s)),
            );

            let tokens = text(t(
                "Tokens: {n} título · {y} ano · {ny} título (ano) · {s00e00} S01E01 · {sxe} 1x01 · {s} temp · {e00} ep · {t} título do episódio · {abs} absoluto",
                "Tokens: {n} title · {y} year · {ny} title (year) · {s00e00} S01E01 · {sxe} 1x01 · {s} season · {e00} ep · {t} episode title · {abs} absolute",
            ))
            .font(k.ty.caption())
            .color(k.c.text_muted)
            .wrap()
            .w_full()
            .min_w(0.0);

            let actions = row()
                .w_full()
                .pt(8.0)
                .justify_end()
                .child(Button::new(t("Concluído", "Done")).variant(ButtonVariant::Submit).on_press(close_popup).build(k).default_button().cursor(Cursor::Pointer));

            let body = col().w_full().gap(10.0).child(input).child(tokens).child(actions);
            Modal::new("media-edit-format", t("Editar formato", "Edit Format"))
                .open(true)
                .child(pad(body))
                .width(480.0)
                .fit()
                .on_close(close_popup)
                .build(k, layer);
        }
        MediaPopup::Preferences => {
            // FileBot-inspired 2-column dropdown options grid with real anchored popovers + API keys card.
            let card_box = |key: &'static str| col().key(key).w_full().radius(k.radius.lg).fill(k.c.bg_base).border(1.0, k.c.border_weak);
            let hair = || el().w_full().h(1.0).fill(k.c.border_weak);

            let flag_el = |lang: MediaLang| -> El {
                match s.posters.flag(lang) {
                    Some(img) => el().size(18.0, 13.5).radius(2.0).pixels(img.pixels()),
                    None => el().size(18.0, 13.5).radius(2.0).fill(k.c.surface_raised),
                }
            };

            let combo_field = |key: &'static str,
                               label: &'static str,
                               leading: Option<El>,
                               value: String,
                               dd: PrefDropdown|
             -> El {
                let open = m.open_dropdown == Some(dd);
                let mut trigger = row()
                    .key(key)
                    .role("combobox")
                    .grow(1.0)
                    .w(0.0)
                    .min_w(0.0)
                    .h(30.0)
                    .px(9.0)
                    .gap(7.0)
                    .center_y()
                    .radius(k.radius.sm)
                    .cursor(Cursor::Pointer)
                    .fill(k.c.surface_raised)
                    .hover_fill(k.c.hover)
                    .border(1.0, if open { k.c.text_accent } else { k.c.border_base })
                    .on_press(move |s: &mut State| {
                        if let Some(b) = &mut s.batch_rename {
                            b.media.open_dropdown = if b.media.open_dropdown == Some(dd) { None } else { Some(dd) };
                        }
                    });
                if let Some(lead) = leading {
                    trigger = trigger.child(lead);
                }
                trigger = trigger
                    .child(text(value).font(k.ty.text_small).color(k.c.text_base).truncate().grow(1.0).w(0.0).min_w(0.0))
                    .child(icon(&icons::CHEVRON_DOWN, 12.0).color(k.c.text_muted));

                row()
                    .grow(1.0)
                    .w(0.0)
                    .min_w(0.0)
                    .gap(8.0)
                    .center_y()
                    .child(text(label).font(k.ty.text_xsmall_medium).color(k.c.text_muted).w(104.0))
                    .child(trigger)
            };

            let ds_label = match (m.source, m.kind) {
                (DataSource::Tmdb, MediaKind::Series) => "TheMovieDB · TV",
                (DataSource::Tmdb, MediaKind::Movie) => "TheMovieDB · Filme",
                (DataSource::Tvdb, MediaKind::Series) => "TheTVDB · TV",
                (DataSource::Tvdb, MediaKind::Movie) => "TheTVDB · Filme",
            };

            let options_card = card_box("pref-options-card")
                .padding(14.0)
                .gap(10.0)
                .child(
                    row()
                        .w_full()
                        .gap(16.0)
                        .center_y()
                        .child(combo_field(
                            "dd-datasource",
                            t("Fonte:", "Datasource:"),
                            None,
                            ds_label.to_string(),
                            PrefDropdown::Datasource,
                        ))
                        .child(combo_field(
                            "dd-order",
                            t("Ordem:", "Episode Order:"),
                            None,
                            m.order.label().to_string(),
                            PrefDropdown::EpisodeOrder,
                        )),
                )
                .child(
                    row()
                        .w_full()
                        .gap(16.0)
                        .center_y()
                        .child(combo_field(
                            "dd-language",
                            t("Idioma:", "Language:"),
                            Some(flag_el(m.lang)),
                            m.lang.label().to_string(),
                            PrefDropdown::Language,
                        ))
                        .child(combo_field(
                            "dd-match-mode",
                            t("Match:", "Match Mode:"),
                            None,
                            m.match_mode.label().to_string(),
                            PrefDropdown::MatchMode,
                        )),
                );

            let key_status = |filled: bool| -> El {
                row()
                    .gap(5.0)
                    .center_y()
                    .child(el().size(6.0, 6.0).radius(3.0).fill(if filled { k.c.success.fg } else { k.c.text_faint }))
                    .child(
                        text(if filled { t("Configurada", "Configured") } else { t("Não configurada", "Not set") })
                            .font(k.ty.caption())
                            .color(if filled { k.c.success.fg } else { k.c.text_faint }),
                    )
            };

            let tmdb_in = crate::rules_ui::field_box(
                k,
                "TMDB API Key",
                t("API Key v3 ou Read Access Token…", "v3 API Key or Read Access Token…"),
                None,
                false,
                &m.tmdb_key_edit,
                |s: &mut State| match &mut s.batch_rename {
                    Some(b) => &mut b.media.tmdb_key_edit,
                    None => &mut s.name_edit,
                },
                Some(|s: &mut State, _: &mut Tx| media_rename::save_keys(s)),
            );

            let tvdb_in = crate::rules_ui::field_box(
                k,
                "TVDB API Key",
                t("TVDB v4 API Key…", "TVDB v4 API Key…"),
                None,
                false,
                &m.tvdb_key_edit,
                |s: &mut State| match &mut s.batch_rename {
                    Some(b) => &mut b.media.tvdb_key_edit,
                    None => &mut s.name_edit,
                },
                Some(|s: &mut State, _: &mut Tx| media_rename::save_keys(s)),
            );

            let keys_card = card_box("pref-keys-card")
                .child(
                    col()
                        .w_full()
                        .px(14.0)
                        .py(11.0)
                        .gap(6.0)
                        .child(
                            row()
                                .w_full()
                                .center_y()
                                .child(text("TheMovieDB (TMDB)").font(k.ty.text_small_medium).color(k.c.text_base).grow(1.0))
                                .child(key_status(!m.tmdb_key_edit.text.trim().is_empty())),
                        )
                        .child(tmdb_in),
                )
                .child(hair())
                .child(
                    col()
                        .w_full()
                        .px(14.0)
                        .py(11.0)
                        .gap(6.0)
                        .child(
                            row()
                                .w_full()
                                .center_y()
                                .child(text("TheTVDB (v4)").font(k.ty.text_small_medium).color(k.c.text_base).grow(1.0))
                                .child(key_status(!m.tvdb_key_edit.text.trim().is_empty())),
                        )
                        .child(tvdb_in),
                );

            let mut body = col()
                .w_full()
                .gap(12.0)
                .child(options_card)
                .child(keys_card);

            if let Some(err) = &m.error {
                body = body.child(text(err.clone()).font(k.ty.text_xsmall).color(k.c.danger.fg).wrap().w_full().min_w(0.0));
            }

            let actions = row()
                .w_full()
                .pt(8.0)
                .gap(8.0)
                .justify_end()
                .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press(close_popup).build(k).cursor(Cursor::Pointer))
                .child(Button::new(t("Salvar", "Save")).variant(ButtonVariant::Submit).on_press(media_rename::save_keys).build(k).default_button().cursor(Cursor::Pointer));

            Modal::new("media-preferences", t("Preferências de Mídia", "Media Preferences"))
                .open(true)
                .child(pad(body.child(actions)))
                .width(620.0)
                .fit()
                .on_close(close_popup)
                .build(k, layer);

            // Real anchored floating dropdown attached directly beneath the clicked combobox!
            if let Some(dd) = m.open_dropdown {
                let dd_item = |key: String, leading: Option<El>, label: String, active: bool, on_press: Box<dyn Fn(&mut State) + 'static>| -> El {
                    let mut r = row()
                        .key(key)
                        .role("option")
                        .w_full()
                        .h(30.0)
                        .px(10.0)
                        .gap(8.0)
                        .center_y()
                        .radius(5.0)
                        .cursor(Cursor::Pointer)
                        .fill(if active { k.alpha(k.c.text_accent, if k.dark { 0.20 } else { 0.11 }) } else { Color::TRANSPARENT })
                        .hover_fill(k.c.hover)
                        .on_press(move |s: &mut State| on_press(s));
                    if let Some(lead) = leading {
                        r = r.child(lead);
                    }
                    r = r.child(
                        text(label)
                            .font(if active { k.ty.text_small_medium } else { k.ty.text_small })
                            .color(if active { k.c.text_accent } else { k.c.text_base })
                            .grow(1.0)
                            .min_w(0.0)
                            .truncate(),
                    );
                    if active {
                        r = r.child(icon(k.glyph.check, 12.0).color(k.c.text_accent));
                    }
                    r
                };

                let mut list = col().key("pref-dd-list").w_full().padding(4.0).gap(2.0);
                let (anchor_key, menu_w) = match dd {
                    PrefDropdown::Datasource => {
                        for (idx, (src, kind, label)) in [
                            (DataSource::Tmdb, MediaKind::Series, "TheMovieDB · TV"),
                            (DataSource::Tmdb, MediaKind::Movie, "TheMovieDB · Filme"),
                            (DataSource::Tvdb, MediaKind::Series, "TheTVDB · TV"),
                            (DataSource::Tvdb, MediaKind::Movie, "TheTVDB · Filme"),
                        ]
                        .into_iter()
                        .enumerate()
                        {
                            let active = m.source == src && m.kind == kind;
                            list = list.child(dd_item(
                                format!("opt-ds-{idx}"),
                                None,
                                label.into(),
                                active,
                                Box::new(move |s| {
                                    if let Some(b) = &mut s.batch_rename {
                                        b.media.source = src;
                                        b.media.set_kind(kind);
                                        b.media.open_dropdown = None;
                                    }
                                }),
                            ));
                        }
                        ("dd-datasource", 210.0)
                    }
                    PrefDropdown::EpisodeOrder => {
                        for ord in EpisodeOrder::ALL {
                            let active = m.order == ord;
                            list = list.child(dd_item(
                                format!("opt-ord-{ord:?}"),
                                None,
                                ord.label().into(),
                                active,
                                Box::new(move |s| {
                                    if let Some(b) = &mut s.batch_rename {
                                        b.media.order = ord;
                                        b.media.open_dropdown = None;
                                    }
                                    media_rename::fetch_selected_episodes(s);
                                }),
                            ));
                        }
                        ("dd-order", 180.0)
                    }
                    PrefDropdown::Language => {
                        for lang in MediaLang::ALL {
                            let active = m.lang == lang;
                            list = list.child(dd_item(
                                format!("opt-lang-{lang:?}"),
                                Some(flag_el(lang)),
                                lang.label().into(),
                                active,
                                Box::new(move |s| {
                                    if let Some(b) = &mut s.batch_rename {
                                        b.media.lang = lang;
                                        b.media.open_dropdown = None;
                                    }
                                }),
                            ));
                        }
                        ("dd-language", 240.0)
                    }
                    PrefDropdown::MatchMode => {
                        for mm in MatchMode::ALL {
                            let active = m.match_mode == mm;
                            list = list.child(dd_item(
                                format!("opt-mm-{mm:?}"),
                                None,
                                mm.label().into(),
                                active,
                                Box::new(move |s| {
                                    if let Some(b) = &mut s.batch_rename {
                                        b.media.match_mode = mm;
                                        b.media.open_dropdown = None;
                                    }
                                }),
                            ));
                        }
                        ("dd-match-mode", 180.0)
                    }
                };

                let surface = col()
                    .key("pref-dd-surface")
                    .w(menu_w)
                    .max_h(240.0)
                    .min_h(0.0)
                    .radius(8.0)
                    .fill(k.c.bg_base)
                    .border(1.0, k.c.border_base)
                    .box_shadow(rune::Shadow { y: 8.0, blur: 24.0, color: k.dark_a(0.35), ..Default::default() })
                    .scroll(ScrollSpec { stick_bottom: false })
                    .child(list)
                    .scroll_bar_styled(ui::bar_style(k))
                    .modal(true, |s: &mut State| {
                        if let Some(b) = &mut s.batch_rename {
                            b.media.open_dropdown = None;
                        }
                    });

                layer.push(rune_kit::popover::anchored(k, surface, anchor_key, rune_kit::Placement::BottomStart, 4.0));
            }
        }
        MediaPopup::SelectMatch => {
            let query_input = crate::rules_ui::field_box(
                k,
                t("Título", "Title"),
                t("Digite o nome da série, anime ou filme…", "Type the series, anime or movie title…"),
                Some(&icons::SEARCH),
                false,
                &m.query,
                |s: &mut State| match &mut s.batch_rename {
                    Some(b) => &mut b.media.query,
                    None => &mut s.name_edit,
                },
                Some(|s: &mut State, _: &mut Tx| media_rename::start_search(s)),
            );

            let search_bar = row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(query_input.grow(1.0).w(0.0).min_w(0.0))
                .child(
                    Button::new(if m.searching { t("Buscando…", "Searching…") } else { t("Buscar", "Search") })
                        .variant(ButtonVariant::Submit)
                        .disabled(m.searching)
                        .on_press(media_rename::start_search)
                        .build(k)
                        .cursor(Cursor::Pointer),
                );

            let mut body = col().w_full().gap(10.0).child(search_bar);
            if let Some(err) = &m.error {
                body = body.child(text(err.clone()).font(k.ty.text_xsmall).color(k.c.danger.fg).wrap().w_full().min_w(0.0));
            }

            if !m.candidates.is_empty() {
                // Left: clean hairline-divided list (no heavy nested boxes).
                let mut list = col().key("select-match-list").w_full();
                for (i, cand) in m.candidates.iter().enumerate() {
                    let active = m.selected == Some(i);
                    let thumb: El = match cand.poster_url.as_deref().and_then(|u| s.posters.get(u)) {
                        Some(img) => el().size(34.0, 50.0).radius(3.0).pixels(img.pixels()),
                        None => row()
                            .size(34.0, 50.0)
                            .radius(3.0)
                            .center()
                            .fill(k.c.surface_raised)
                            .border(1.0, k.c.border_weak)
                            .child(icon(&icons::VIDEO, 15.0).color(k.c.text_faint)),
                    };
                    let meta = match cand.year {
                        Some(y) => format!("{y} · {}", cand.kind.label()),
                        None => cand.kind.label().to_string(),
                    };
                    let mut text_col = col()
                        .grow(1.0)
                        .w(0.0)
                        .min_w(0.0)
                        .gap(2.0)
                        .child(
                            text(cand.title.clone())
                                .font(if active { k.ty.text_small_medium } else { k.ty.text_small })
                                .color(if active { k.c.text_accent } else { k.c.text_base })
                                .truncate()
                                .w_full()
                                .min_w(0.0),
                        )
                        .child(text(meta).font(k.ty.caption()).color(k.c.text_muted));
                    if let Some(orig) = &cand.original_title {
                        text_col = text_col.child(text(orig.clone()).font(k.ty.caption()).color(k.c.text_faint).truncate().w_full().min_w(0.0));
                    }

                    let mut r = row()
                        .key(format!("pick-cand-{i}"))
                        .role("button")
                        .w_full()
                        .px(10.0)
                        .py(7.0)
                        .gap(10.0)
                        .center_y()
                        .cursor(Cursor::Pointer)
                        .fill(if active { k.alpha(k.c.text_accent, if k.dark { 0.18 } else { 0.09 }) } else { Color::TRANSPARENT })
                        .hover_fill(if active { k.alpha(k.c.text_accent, if k.dark { 0.22 } else { 0.12 }) } else { k.c.hover })
                        .on_press(move |s: &mut State| media_rename::preview_candidate(s, i))
                        .child(
                            el()
                                .w(3.0)
                                .h(34.0)
                                .radius(1.5)
                                .fill(if active { k.c.text_accent } else { Color::TRANSPARENT }),
                        )
                        .child(thumb)
                        .child(text_col);
                    if i + 1 < m.candidates.len() {
                        r = r.border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak);
                    }
                    list = list.child(r);
                }

                let left_list = col()
                    .key("select-match-scroll")
                    .grow(1.0)
                    .w(0.0)
                    .h(380.0)
                    .min_w(0.0)
                    .radius(k.radius.md)
                    .fill(k.c.bg_base)
                    .border(1.0, k.c.border_weak)
                    .scroll(ScrollSpec { stick_bottom: false })
                    .child(list)
                    .scroll_bar_styled(ui::bar_style(k));

                // Right: large poster artwork & full scrollable synopsis preview.
                let detail_pane: El = if let Some(cand) = m.active_candidate() {
                    let big_poster: El = match cand.poster_url.as_deref().and_then(|u| s.posters.get(u)) {
                        Some(img) => el().size(116.0, 172.0).radius(6.0).pixels(img.pixels()),
                        None => row()
                            .size(116.0, 172.0)
                            .radius(6.0)
                            .center()
                            .fill(k.c.surface_raised)
                            .border(1.0, k.c.border_weak)
                            .child(icon(&icons::VIDEO, 28.0).color(k.c.text_faint)),
                    };
                    let meta = match cand.year {
                        Some(y) => format!("{y} · {} · {}", cand.kind.label(), cand.source.label()),
                        None => format!("{} · {}", cand.kind.label(), cand.source.label()),
                    };
                    let mut inner = col()
                        .w_full()
                        .padding(12.0)
                        .gap(8.0)
                        .child(row().w_full().center().child(big_poster))
                        .child(text(cand.title.clone()).font(k.ty.text_small_medium).color(k.c.text_base).wrap().w_full().min_w(0.0))
                        .child(text(meta).font(k.ty.caption()).color(k.c.text_muted));
                    if !cand.overview.is_empty() {
                        inner = inner.child(text(cand.overview.clone()).font(k.ty.caption()).color(k.c.text_muted).wrap().w_full().min_w(0.0));
                    }
                    col()
                        .key("select-match-detail")
                        .w(248.0)
                        .h(380.0)
                        .radius(k.radius.md)
                        .fill(k.c.bg_base)
                        .border(1.0, k.c.border_weak)
                        .scroll(ScrollSpec { stick_bottom: false })
                        .child(inner)
                        .scroll_bar_styled(ui::bar_style(k))
                } else {
                    el().w(248.0)
                };

                body = body.child(row().w_full().gap(10.0).child(left_list).child(detail_pane));
            } else if !m.searching && m.error.is_none() {
                body = body.child(
                    row()
                        .w_full()
                        .h(120.0)
                        .center()
                        .radius(k.radius.md)
                        .fill(k.c.bg_base)
                        .border(1.0, k.c.border_weak)
                        .child(text(t("Digite um título acima e clique em Buscar.", "Type a title above and click Search.")).font(k.ty.text_small).color(k.c.text_faint)),
                );
            }

            let can_confirm = m.active_candidate().is_some();
            let actions = row()
                .w_full()
                .pt(6.0)
                .gap(8.0)
                .justify_end()
                .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press(close_popup).build(k).cursor(Cursor::Pointer))
                .child(
                    Button::new(t("Selecionar", "Select"))
                        .variant(ButtonVariant::Submit)
                        .disabled(!can_confirm)
                        .on_press(media_rename::confirm_selected_candidate)
                        .build(k)
                        .default_button()
                        .cursor(Cursor::Pointer),
                );

            Modal::new("media-select-match", t("Selecionar correspondência", "Select Match"))
                .open(true)
                .child(pad(body.child(actions)))
                .width(680.0)
                .fit()
                .on_close(close_popup)
                .build(k, layer);
        }
        MediaPopup::Analyze => {
            let steps = m.analyze_sample(&m.analyze_input.text, &br.loc);
            crate::rules_ui::render_steps_analyze_modal(
                k,
                "media-analyze-modal",
                t("Analisar Mídia", "Analyze Media"),
                &m.analyze_input,
                |s| match &mut s.batch_rename {
                    Some(b) => &mut b.media.analyze_input,
                    None => &mut s.name_edit,
                },
                steps,
                close_popup,
                panel_border,
                layer,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> Entry {
        Entry {
            id: name.into(),
            name: name.into(),
            is_dir: false,
            size: Some(1024),
            modified: None,
            mime: None,
            target: None,
            starred: false,
            md5: None,
            drive_origin: None,
        }
    }

    fn local_br(names: &[&str], others: &[&str]) -> BatchRename {
        let items = names.iter().map(|n| file(n)).collect();
        let other_names = others.iter().map(|n| (*n).to_string()).collect();
        BatchRename::new(Location::Local(PathBuf::from("C:/tmp")), items, other_names)
    }

    #[test]
    fn literal_replace_first_and_all_occurrences() {
        let mut br = local_br(&["PAD-Genivaldo-Parte I.pdf", "PAD-Genivaldo-Parte II.pdf"], &[]);
        br.search = TextEdit::new("-");
        br.replace = TextEdit::new(" ");
        br.replace_all = false;

        let p = br.preview();
        assert!(p.can_submit());
        assert_eq!(p.changed_count, 2);
        assert_eq!(p.rows[0].next, "PAD Genivaldo-Parte I.pdf");
        assert_eq!(p.rows[1].next, "PAD Genivaldo-Parte II.pdf");

        br.replace_all = true;
        let p = br.preview();
        assert!(p.can_submit());
        assert_eq!(p.rows[0].next, "PAD Genivaldo Parte I.pdf");
        assert_eq!(p.rows[1].next, "PAD Genivaldo Parte II.pdf");
    }

    #[test]
    fn case_insensitive_works_for_literal_and_regex() {
        let mut br = local_br(&["PAD-Relatório-pad.pdf", "pad-RELATÓRIO.pdf"], &[]);
        br.search = TextEdit::new("pad");
        br.replace = TextEdit::new("$1_DOC");
        br.replace_all = true;

        // Case-sensitive: only lowercase "pad" matches; literal "$1" is not expanded.
        br.case_insensitive = false;
        let p = br.preview();
        assert_eq!(p.rows[0].next, "PAD-Relatório-$1_DOC.pdf");
        assert_eq!(p.rows[1].next, "$1_DOC-RELATÓRIO.pdf");

        // Case-insensitive literal: matches both "PAD" and "pad", still no "$1" expansion.
        br.case_insensitive = true;
        let p = br.preview();
        assert_eq!(p.rows[0].next, "$1_DOC-Relatório-$1_DOC.pdf");
        assert_eq!(p.rows[1].next, "$1_DOC-RELATÓRIO.pdf");

        // Case-insensitive literal with Unicode accents ("relatório" matches "Relatório" and "RELATÓRIO").
        br.search = TextEdit::new("relatório");
        br.replace = TextEdit::new("Informe");
        let p = br.preview();
        assert_eq!(p.rows[0].next, "PAD-Informe-pad.pdf");
        assert_eq!(p.rows[1].next, "pad-Informe.pdf");

        // Case-insensitive regex:
        br.use_regex = true;
        br.search = TextEdit::new(r"^pad-(.+)");
        br.replace = TextEdit::new("DOC-$1");
        let p = br.preview();
        assert_eq!(p.rows[0].next, "DOC-Relatório-pad.pdf");
        assert_eq!(p.rows[1].next, "DOC-RELATÓRIO.pdf");
    }

    #[test]
    fn regex_replace_with_capture_groups_and_invalid_pattern() {
        let mut br = local_br(&["PAD-Genivaldo-Parte I.pdf", "PAD-Genivaldo-Parte II.pdf"], &[]);
        br.use_regex = true;
        br.search = TextEdit::new(r"^PAD-([^-]+)-Parte (.+)\.pdf$");
        br.replace = TextEdit::new("$1 ($2).pdf");

        let p = br.preview();
        assert!(p.can_submit());
        assert_eq!(p.rows[0].next, "Genivaldo (I).pdf");
        assert_eq!(p.rows[1].next, "Genivaldo (II).pdf");

        // Invalid regex disables submit and reports error.
        br.search = TextEdit::new("[unclosed");
        let p = br.preview();
        assert!(p.regex_error.is_some());
        assert!(!p.can_submit());
    }

    #[test]
    fn unchecked_items_are_skipped_and_protected_from_collision() {
        let mut br = local_br(&["a.pdf", "b.pdf"], &[]);
        br.checked[0] = false;
        br.search = TextEdit::new("b");
        br.replace = TextEdit::new("a");

        let p = br.preview();
        assert!(!p.can_submit());
        assert_eq!(p.changed_count, 1);
        assert_eq!(p.error_count, 1);
        assert!(p.rows[1].error.is_some());
    }

    #[test]
    fn detects_duplicate_targets_and_sibling_collisions() {
        let mut br = local_br(&["doc1.pdf", "doc2.pdf"], &["final.pdf"]);
        br.use_regex = true;
        br.search = TextEdit::new(r"doc\d");
        br.replace = TextEdit::new("same");

        let p = br.preview();
        assert!(!p.can_submit());
        assert_eq!(p.error_count, 2);

        br.search = TextEdit::new("doc1");
        br.replace = TextEdit::new("FINAL");
        br.use_regex = false;
        let p = br.preview();
        assert!(!p.can_submit());
        assert_eq!(p.error_count, 1);
        assert!(p.rows[0].error.is_some());
    }

    #[test]
    fn local_batch_executes_swaps_and_chains_cleanly() {
        let dir = std::env::temp_dir().join(format!("oe-batch-rename-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, b"from-a").unwrap();
        std::fs::write(&b, b"from-b").unwrap();

        let rec = undo::Rec::default();
        let res = run_local_batch(vec![(a.clone(), "b.txt".into()), (b.clone(), "a.txt".into())], &rec);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(std::fs::read(&a).unwrap(), b"from-b");
        assert_eq!(std::fs::read(&b).unwrap(), b"from-a");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_swap_batch_puts_every_item_back() {
        let dir = std::env::temp_dir().join(format!("oe-batch-rename-rollback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["a.txt", "b.txt", "c.txt", "x.txt"] {
            std::fs::write(dir.join(n), n).unwrap();
        }
        // c → x can't go: x is taken by a file outside the batch, as a hidden one would be.
        let rec = undo::Rec::default();
        let res = run_local_batch(vec![(dir.join("a.txt"), "b.txt".into()), (dir.join("b.txt"), "a.txt".into()), (dir.join("c.txt"), "x.txt".into())], &rec);
        assert!(res.is_err());
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["a.txt", "b.txt", "c.txt", "x.txt"]);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn server_swaps_are_refused_in_the_preview() {
        let server = openexplorer_remote::Server {
            id: "s".into(),
            name: "s".into(),
            protocol: openexplorer_remote::Protocol::Sftp,
            brand: None,
            host: "h".into(),
            port: 22,
            user: "u".into(),
            root: "/".into(),
            key: None,
            host_key: None,
            cert: None,
            connections: 1,
            implicit: false,
        };
        let loc = Location::Server { server: Box::new(server), path: vec![], find: None };
        let mut br = BatchRename::new(loc, vec![file("a.txt"), file("b.txt")], vec![]);
        br.mode = Mode::Rules;
        br.rules.rules = vec![crate::rules_rename::Rule::new(crate::rules_rename::RuleConfig::Mapping {
            entries: vec![("a".into(), "b".into()), ("b".into(), "a".into())],
            allow_reuse: true,
            partial_match: false,
            inverse: false,
            case_sensitive: true,
            skip_ext: true,
        })];
        let p = br.preview();
        assert_eq!(p.rows[0].next, "b.txt");
        assert_eq!(p.rows[1].next, "a.txt");
        assert!(!p.can_submit(), "{p:?}");
        assert!(p.rows.iter().all(|r| r.error.is_some()));
    }

    #[test]
    fn media_names_never_mix_two_shows() {
        use crate::media_rename::{Candidate, DataSource, EpisodeInfo, MediaKind, MediaPopup};
        let mut br = BatchRename::new(Location::Local(PathBuf::from("C:/tmp")), vec![file("Show S01E01.mkv"), file("Behind the scenes.mkv")], vec![]);
        let cand = |id: &str, title: &str| Candidate { id: id.into(), source: DataSource::Tmdb, kind: MediaKind::Series, title: title.into(), original_title: None, year: None, overview: String::new(), poster_url: None };
        br.media.candidates = vec![cand("1", "Alpha"), cand("2", "Beta")];
        br.media.selected = Some(0);
        br.media.episodes = vec![
            EpisodeInfo { season: 1, episode: 1, absolute: Some(1), title: "Alpha Pilot".into() },
            EpisodeInfo { season: 1, episode: 2, absolute: Some(2), title: "Alpha Two".into() },
        ];
        br.media.episodes_for = Some("1".into());
        assert_eq!(br.media.format_item(0, &br.loc).as_deref(), Some("Alpha - S01E01 - Alpha Pilot.mkv"));
        // An extra without a number, among numbered files, is left alone.
        assert_eq!(br.media.format_item(1, &br.loc), None);
        // Beta only highlighted in the picker: it never takes Alpha's episode titles.
        br.media.selected = Some(1);
        br.media.popup = Some(MediaPopup::SelectMatch);
        assert_eq!(br.media.format_item(0, &br.loc).as_deref(), Some("Beta - S01E01.mkv"));
        // Leaving the picker without confirming goes back to Alpha.
        br.media.close_popup();
        assert_eq!(br.media.selected, Some(0));
    }

    #[test]
    fn hash_tags_wait_in_the_background() {
        let dir = std::env::temp_dir().join(format!("oe-batch-rename-hash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "abc").unwrap();
        let mut br = BatchRename::new(Location::Local(dir.clone()), vec![file("a.txt")], vec![]);
        br.mode = Mode::Rules;
        let mut cfg = crate::rules_rename::RuleConfig::default_for(crate::rules_rename::RuleKind::Insert);
        if let crate::rules_rename::RuleConfig::Insert { text, .. } = &mut cfg {
            *text = ":Hash_CRC32: ".into();
        }
        br.rules.rules = vec![crate::rules_rename::Rule::new(cfg)];
        let first = br.preview();
        assert!(first.waiting && !first.can_submit(), "{first:?}");
        let mut done = None;
        for _ in 0..500 {
            let p = br.preview();
            if !p.waiting {
                done = Some(p);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let p = done.expect("the hash arrives");
        assert_eq!(p.rows[0].next, "352441C2 a.txt");
        assert!(p.can_submit());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn simple_and_media_analyzers_produce_trace_steps() {
        let mut br = local_br(&["Avatar - S01E01 - No horizonte.mkv"], &[]);
        br.search = TextEdit::new("S01");
        br.replace = TextEdit::new("Temporada 1 ");
        let steps = br.analyze_simple_sample("Avatar - S01E01 - No horizonte.mkv");
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[2].1, "Avatar - Temporada 1 E01 - No horizonte.mkv");
        assert!(steps[2].2.is_none());

        let m_steps = br.media.analyze_sample("Avatar - S01E01 - No horizonte.mkv", &br.loc);
        assert_eq!(m_steps.len(), 4);
        assert!(m_steps[0].1.contains("S01E01"));

        br.media.error = Some("erro".into());
        br.media.clear_match();
        assert!(br.media.error.is_none());
        assert!(br.media.selected.is_none());
    }
}
