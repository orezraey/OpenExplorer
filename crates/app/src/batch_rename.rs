//! Batch rename ("Renomear em massa"): independent resizable window for search & replace
//! (literal or regex, first or all occurrences, case-sensitive or case-insensitive) with live
//! before/after comparison, per-item selection, conflict validation and single-step undo across
//! local disk, Google Drive and remote servers.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use openexplorer_i18n::{t, tr};
use regex::{NoExpand, Regex, RegexBuilder};
use rune::{Anim, Color, Cursor, Cx, El, ScrollSpec, TextEdit, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, Checkbox, ScrollBar};

use crate::actions;
use crate::icons;
use crate::model::{Entry, Location, shown, typed};
use crate::undo::{self, Step};
use crate::{State, ops, servers, ui};

/// State of the independent "Renomear em massa" window.
pub struct BatchRename {
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
}

impl Preview {
    pub fn can_submit(&self) -> bool {
        self.regex_error.is_none() && self.changed_count > 0 && self.error_count == 0
    }
}

enum Matcher {
    Literal,
    LiteralCi(Regex),
    Regex(Regex),
}

impl BatchRename {
    pub fn new(loc: Location, items: Vec<Entry>, other_names: Vec<String>) -> Self {
        let n = items.len();
        let mut search = TextEdit::new("");
        search.focused = true;
        Self {
            loc,
            items,
            checked: vec![true; n],
            other_names,
            search,
            replace: TextEdit::new(""),
            replace_all: false,
            case_insensitive: false,
            use_regex: false,
        }
    }

    /// Computes the live before/after preview and validates invalid names and collisions.
    pub fn preview(&self) -> Preview {
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
                    return Preview { regex_error: Some(format_regex_error(&e)), rows, changed_count: 0, error_count: 0 };
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

        let mut rows: Vec<RowPreview> = self
            .items
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
            .collect();

        // Collision & validity checks.
        let case_insensitive_fs = matches!(self.loc, Location::Local(_) | Location::Found(_));
        let norm = |name: &str| -> String {
            let trimmed = name.trim();
            if case_insensitive_fs {
                trimmed.to_lowercase()
            } else if matches!(self.loc, Location::Drive { .. }) {
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

        let mut changed_count = 0;
        let mut error_count = 0;
        for (i, item) in self.items.iter().enumerate() {
            let included = self.checked.get(i).copied().unwrap_or(false);
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
                } else {
                    None
                }
            };
            if err.is_some() {
                error_count += 1;
                rows[i].error = err;
            }
        }

        Preview { regex_error: None, rows, changed_count, error_count }
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
    let other_names: Vec<String> = if matches!(loc, Location::Found(_)) {
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
    if !matches!(loc, Location::Mock(_) | Location::Found(_)) {
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
        Location::Mock(_) => {
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

    // Two-phase rename for overlapping chains/swaps.
    let pid = std::process::id();
    let mut staged: Vec<(PathBuf, PathBuf, String)> = Vec::with_capacity(jobs.len());
    for (idx, (from, name)) in jobs.into_iter().enumerate() {
        let mut attempt = 0usize;
        let tmp = loop {
            let candidate = from.with_file_name(format!(".oe-batch-tmp-{pid}-{idx}-{attempt}"));
            if !candidate.exists() {
                break candidate;
            }
            attempt += 1;
        };
        if let Err(e) = std::fs::rename(&from, &tmp) {
            // Roll back any items already moved to temporary names.
            for (orig, staged_tmp, _) in staged.into_iter().rev() {
                let _ = std::fs::rename(&staged_tmp, &orig);
            }
            let label = from.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            return Err(batch_err(0, &label, &e.to_string()));
        }
        staged.push((from, tmp, name));
    }

    let mut done = 0usize;
    for (orig_from, tmp, name) in staged {
        let label = orig_from.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let to = ops::rename(&tmp, &name).map_err(|e| batch_err(done, &label, &e))?;
        rec.add([Step::Move { from: to, to: orig_from }]);
        done += 1;
    }
    Ok(batch_done_msg(done))
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

fn close_window(s: &mut State) {
    s.close_batch_rename();
}

/// Renders the independent, movable and resizable "Renomear em massa" native window.
pub fn batch_rename_window_view(s: &State, cx: &mut Cx<State>) -> El {
    if s.is_placeholder {
        return el();
    }
    let k = ui::kit(s.settings.theme.dark(cx.system_dark()));
    cx.caption_color(k.c.text_base);

    let window_bg = if k.dark { rune::oklch(0.24, 0.0, 0.0) } else { k.c.page };
    let panel_bg = if k.dark { rune::oklch(0.198, 0.0, 0.0) } else { k.c.bg_base };
    let panel_border = if k.dark { rune::oklch(0.285, 0.0, 0.0) } else { k.c.border_weak };
    let accent_bg = k.c.bg_icon_button_contrast;
    let accent_hover = k.on(k.c.submit_hover, accent_bg);

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

    // Crisp accent-blue checkbox for table header and rows.
    let check_box = |key: String, checked: bool, on_click: Box<dyn Fn(&mut State) + 'static>| -> El {
        let mut b = row()
            .key(key)
            .role("checkbox")
            .size(16.0, 16.0)
            .center()
            .radius(4.0)
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| on_click(s));
        if checked {
            b = b.fill(accent_bg).hover_fill(accent_hover).child(icon(k.glyph.check, 11.0).color(Color::WHITE));
        } else {
            b = b.fill(k.c.bg_base).hover_fill(k.c.hover).border(1.0, k.c.border_strong);
        }
        b
    };

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

    let search_input = rune_kit::TextInput::new(&br.search)
        .width(None)
        .label(t("Procurar por", "Search for"))
        .leading_icon(&icons::SEARCH)
        .placeholder(search_placeholder)
        .on_edit(|s: &mut State| match &mut s.batch_rename {
            Some(br) => &mut br.search,
            None => &mut s.name_edit,
        })
        .on_submit(|s: &mut State, _: &mut Tx| submit(s))
        .build(&k);

    let replace_input = rune_kit::TextInput::new(&br.replace)
        .width(None)
        .label(t("Substituir por", "Replace with"))
        .placeholder(replace_placeholder)
        .on_edit(|s: &mut State| match &mut s.batch_rename {
            Some(br) => &mut br.replace,
            None => &mut s.name_edit,
        })
        .on_submit(|s: &mut State, _: &mut Tx| submit(s))
        .build(&k);

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
        .gap(16.0)
        .center_y()
        .child(
            Checkbox::new(br.replace_all)
                .label(t("Substituir todas as ocorrências", "Replace all occurrences"))
                .on_toggle(|s: &mut State| {
                    if let Some(br) = &mut s.batch_rename {
                        br.replace_all = !br.replace_all;
                    }
                })
                .build(&k)
                .key("opt-replace-all"),
        )
        .child(
            Checkbox::new(br.case_insensitive)
                .label(t("Ignorar maiúsculas/minúsculas", "Case insensitive"))
                .on_toggle(|s: &mut State| {
                    if let Some(br) = &mut s.batch_rename {
                        br.case_insensitive = !br.case_insensitive;
                    }
                })
                .build(&k)
                .key("opt-case-insensitive"),
        )
        .child(
            Checkbox::new(br.use_regex)
                .label(t("Usar Regex", "Use Regex"))
                .on_toggle(|s: &mut State| {
                    if let Some(br) = &mut s.batch_rename {
                        br.use_regex = !br.use_regex;
                    }
                })
                .build(&k)
                .key("opt-regex"),
        );

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

    // Preview table header
    let table_header = row()
        .key("preview-header")
        .w_full()
        .h(34.0)
        .px(12.0)
        .gap(10.0)
        .center_y()
        .fill(k.c.surface_raised)
        .border_sides([0.0, 0.0, 1.0, 0.0], panel_border)
        .child(check_box(
            "toggle-all".into(),
            all_checked,
            Box::new(move |s: &mut State| {
                if let Some(br) = &mut s.batch_rename {
                    let next = !all_checked;
                    br.checked.iter_mut().for_each(|c| *c = next);
                }
            }),
        ))
        .child(
            text(t("Nome atual", "Current name"))
                .font(k.ty.text_xsmall_medium)
                .color(k.c.text_muted)
                .grow(1.0)
                .w(0.0)
                .min_w(0.0),
        )
        .child(el().w(18.0))
        .child(
            text(t("Novo nome", "New name"))
                .font(k.ty.text_xsmall_medium)
                .color(k.c.text_muted)
                .grow(1.0)
                .w(0.0)
                .min_w(0.0),
        );

    // Preview table rows
    let mut rows_col = col().key("preview-rows").w_full();
    for (i, (item, rp)) in br.items.iter().zip(&preview.rows).enumerate() {
        let on = br.checked.get(i).copied().unwrap_or(false);
        let has_err = rp.error.is_some();
        let is_changed = on && rp.changed;

        let cur_color = if on { k.c.text_base } else { k.c.text_faint };
        let arrow_color = if has_err {
            k.c.danger.fg
        } else if is_changed {
            k.c.text_accent
        } else {
            k.c.text_faint
        };
        let next_color = if has_err {
            k.c.danger.fg
        } else if is_changed {
            k.c.text_accent
        } else {
            k.c.text_faint
        };
        let next_font = if is_changed { k.ty.text_small_medium } else { k.ty.text_small };

        let mut right_cell = row()
            .grow(1.0)
            .w(0.0)
            .min_w(0.0)
            .gap(6.0)
            .center_y()
            .child(text(rp.next.clone()).font(next_font).color(next_color).truncate().shrink().min_w(0.0));

        if let Some(err) = &rp.error {
            right_cell = right_cell.child(
                row()
                    .px(6.0)
                    .py(1.0)
                    .radius(k.radius.sm)
                    .fill(k.c.danger.bg)
                    .child(text(err.clone()).font(k.ty.caption()).color(k.c.danger.fg)),
            );
        }

        let mut r = row()
            .key(format!("preview-row-{i}"))
            .w_full()
            .min_h(32.0)
            .px(12.0)
            .py(5.0)
            .gap(10.0)
            .center_y()
            .hover_fill(k.c.hover)
            .child(check_box(
                format!("row-check-{i}"),
                on,
                Box::new(move |s: &mut State| {
                    if let Some(c) = s.batch_rename.as_mut().and_then(|b| b.checked.get_mut(i)) {
                        *c = !*c;
                    }
                }),
            ))
            .child(
                row()
                    .grow(1.0)
                    .w(0.0)
                    .min_w(0.0)
                    .gap(6.0)
                    .center_y()
                    .child(ui::entry_icon(s, &br.loc, item, 15.0))
                    .child(text(rp.current.clone()).font(k.ty.text_small).color(cur_color).truncate().shrink().min_w(0.0)),
            )
            .child(row().w(18.0).center().child(text("→").font(k.ty.text_small).color(arrow_color)))
            .child(right_cell);

        if i + 1 < total {
            r = r.border_sides([0.0, 0.0, 1.0, 0.0], panel_border);
        }
        rows_col = rows_col.child(r);
    }

    let bar = ui::bar_style(&k);
    let scroll_area = col()
        .key("preview-scroll")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: false })
        .child(rows_col)
        .scroll_bar_styled(bar);

    let preview_card = col()
        .key("preview-card")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .radius(k.radius.md)
        .fill(panel_bg)
        .border(1.0, panel_border)
        .child(table_header)
        .child(scroll_area);

    // Footer status summary + modern accent-blue primary button & subtle secondary button
    let (status_text, status_color) = if preview.error_count > 0 {
        let msg = if preview.error_count == 1 {
            t("1 item com conflito ou nome inválido.", "1 item has a conflict or invalid name.").into()
        } else {
            tr!(
                "{} itens com conflito ou nome inválido.",
                "{} items have conflicts or invalid names.",
                preview.error_count
            )
        };
        (msg, k.c.danger.fg)
    } else if preview.changed_count > 0 {
        let msg = if preview.changed_count == 1 {
            tr!("1 de {total} itens será renomeado.", "1 of {total} items will be renamed.")
        } else {
            tr!("{} de {total} itens serão renomeados.", "{} of {total} items will be renamed.", preview.changed_count)
        };
        (msg, k.c.text_muted)
    } else if br.search.text.is_empty() {
        (t("Digite um termo para visualizar as alterações.", "Type a search term to preview changes.").into(), k.c.text_faint)
    } else {
        (t("Nenhuma ocorrência encontrada.", "No matches found.").into(), k.c.text_muted)
    };

    let can_submit = preview.can_submit();
    let submit_label = if preview.changed_count > 0 && can_submit {
        tr!("Renomear ({})", "Rename ({})", preview.changed_count)
    } else {
        t("Renomear", "Rename").into()
    };

    let footer = row()
        .key("actions")
        .w_full()
        .pt(4.0)
        .gap(8.0)
        .center_y()
        .child(text(status_text).font(k.ty.text_xsmall).color(status_color).grow(1.0).min_w(0.0).truncate())
        .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press(close_window).build(&k).cursor(Cursor::Pointer))
        .child(
            Button::new(submit_label)
                .variant(ButtonVariant::Submit)
                .disabled(!can_submit)
                .on_press(submit)
                .build(&k)
                .default_button()
                .cursor(Cursor::Pointer),
        );

    let content = col()
        .key("content")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .px(16.0)
        .pt(6.0)
        .pb(16.0)
        .gap(12.0)
        .child(criteria_card)
        .child(preview_card)
        .child(footer);

    let root = col()
        .key("batch-rename-window")
        .w_full()
        .h_full()
        .fill(window_bg)
        .child(titlebar)
        .child(content)
        .hover_motion_within(Anim::INSTANT, Anim::INSTANT);

    rune_kit::root(root, Vec::new())
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
}
