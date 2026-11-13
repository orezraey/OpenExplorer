//! The window: a toolbar, two panes (each a location tree beside a file list, under a
//! navigation bar and over a status bar), and the message line.

use std::borrow::Cow;
use std::path::PathBuf;

use rune::{Align, Color, Cursor, Cx, DragOutcome, El, GrowthSpec, Icon, KeyEvent, ScrollSpec, Tx, col, el, icon, row, text};
use rune_kit::Kit;

use crate::model::{Kind, Location, SortCol, Space, crumbs, format_size, format_time};
use crate::{State, click, connect, disconnect, drag_item, dragged_ids, dropped, go_back, go_forward, go_up, key, load, navigate, open, pane_key, paths_for, row_key, set_sort};
use crate::icons;

const ROW_H: f32 = crate::ROW_H;
const TREE_W: f32 = 190.0;
/// Column widths. Tipo gives way when the pane is narrow, Tamanho and Data keep theirs; Nome keeps at least
/// `MIN_NAME`, so names never shrink to an ellipsis first.
const W_SIZE: f32 = 86.0;
const W_DATE: f32 = 132.0;
const W_KIND: f32 = 124.0;
const MIN_NAME: f32 = 150.0;

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    let k = Kit::neutral(cx.system_dark());
    let message = text(s.status.clone()).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().min_w(0.0).help_when_truncated(s.status.clone());
    col()
        .w_full()
        .h_full()
        .fill(k.c.page)
        .child(toolbar(s, &k))
        .child(row().key("panes").w_full().grow(1.0).min_h(0.0).padding(6.0).gap(6.0).child(pane(s, &k, 0)).child(pane(s, &k, 1)))
        .child(row().key("message").w_full().h(22.0).px(10.0).center_y().child(message))
}

fn kind_icon(kind: Kind) -> &'static Icon {
    match kind {
        Kind::Folder => &icons::FOLDER,
        Kind::FolderLink => &icons::FOLDER_LINK,
        Kind::Image => &icons::IMAGE,
        Kind::Video => &icons::VIDEO,
        Kind::Audio => &icons::AUDIO,
        Kind::Pdf => &icons::PDF,
        Kind::Doc => &icons::DOC,
        Kind::Sheet => &icons::SHEET,
        Kind::Slides => &icons::SLIDES,
        Kind::Archive => &icons::ARCHIVE,
        Kind::Code => &icons::CODE,
        Kind::Text => &icons::TEXT,
        Kind::App => &icons::APP,
        Kind::Torrent => &icons::TORRENT,
        Kind::Other => &icons::FILE,
    }
}

// ---------------------------------------------------------------------------------------------
// Toolbar

/// A toolbar action: icon and label side by side, greyed out when it can't run.
fn tool<F: Fn(&mut State, &mut Tx) + 'static>(k: &Kit, key: &str, glyph: &'static Icon, label: &str, enabled: bool, f: F) -> El {
    let ink = if enabled { k.c.text_base } else { k.c.text_faint };
    let mut b = row()
        .key(key.to_string())
        .role("button")
        .label(label.to_string())
        .h(30.0)
        .px(8.0)
        .gap(6.0)
        .center_y()
        .radius(k.radius.sm)
        .child(icon(glyph, 18.0).color(ink).opacity(if enabled { 1.0 } else { 0.45 }))
        .child(text(label.to_string()).font(k.ty.text_small).color(ink));
    if enabled {
        b = b.hover(k.c.hover).press(k.c.pressed).cursor(Cursor::Pointer).on_press_tx(f);
    }
    b
}

fn toolbar(s: &State, k: &Kit) -> El {
    let p = s.active;
    let account = match &s.panes[p].loc {
        Location::Drive { email, .. } => Some(email.clone()),
        _ => None,
    };
    let connecting = s.login.is_some();
    let sep = || el().w(1.0).h(20.0).mx(4.0).fill(k.c.border_weak);
    let signed_in = account.clone();
    row()
        .key("toolbar")
        .w_full()
        .h(40.0)
        .px(6.0)
        .gap(2.0)
        .center_y()
        .fill(k.c.bg_base)
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(tool(k, "connect", &icons::ADD_ACCOUNT, if connecting { "Conectando… (clique para recomeçar)" } else { "Conectar conta" }, true, move |s: &mut State, tx: &mut Tx| {
            connect(s, s.active, tx)
        }))
        .child(
            tool(k, "disconnect", &icons::SIGN_OUT, &account.as_ref().map_or_else(|| "Desconectar".to_string(), |e| format!("Desconectar {e}")), account.is_some(), move |s: &mut State, _| {
                if let Some(e) = &signed_in {
                    disconnect(s, e);
                }
            })
            .help(if account.is_some() { "Remove a conta do painel ativo: o acesso e o cache dela" } else { "O painel ativo não mostra uma conta" }),
        )
        .child(sep())
        .child(tool(k, "reload-all", &icons::REFRESH, "Atualizar", true, |s: &mut State, _| load(s, s.active)))
        .child(el().grow(1.0))
        .child(text("Open Explorer").font(k.ty.text_small_medium).color(k.c.text_faint).mx(8.0))
}

// ---------------------------------------------------------------------------------------------
// Pane

fn pane(s: &State, k: &Kit, p: usize) -> El {
    let pane = &s.panes[p];
    let border = if pane.drop_over {
        k.c.text_accent
    } else if s.active == p {
        k.c.border_focus
    } else {
        k.c.border_weak
    };
    col()
        .key(pane_key(p))
        .focusable(true)
        .label(format!("Painel {}", p + 1))
        .w(0.0)
        .grow(1.0)
        .h_full()
        .min_w(0.0)
        .radius(k.radius.sm)
        .fill(if pane.drop_over { k.alpha(k.c.text_accent, 0.06) } else { k.c.bg_base })
        .border(if pane.drop_over { 2.0 } else { 1.0 }, border)
        .clip_rounded(k.radius.sm)
        .on_key(move |s: &mut State, ev: &KeyEvent, tx: &mut Tx| key(s, p, ev, tx))
        .on_press(move |s: &mut State| s.active = p)
        .on_drop_files(move |s: &mut State, files: &[PathBuf], _| dropped(s, p, files))
        .on_drop_hover(move |s: &mut State, over, _| s.panes[p].drop_over = over)
        .child(navbar(s, k, p))
        .child(
            row()
                .key("body")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(tree(s, k, p))
                .child(el().w(1.0).h_full().fill(k.c.border_weak))
                .child(list(s, k, p)),
        )
        .child(statusbar(s, k, p))
}

/// A square icon button for the navigation bar.
fn nav_button<F: Fn(&mut State) + 'static>(k: &Kit, key: &str, glyph: &'static Icon, help: &str, enabled: bool, f: F) -> El {
    let mut b = row()
        .key(key.to_string())
        .role("button")
        .label(help.to_string())
        .size(26.0, 26.0)
        .center()
        .radius(k.radius.sm)
        .help(help.to_string())
        .child(icon(glyph, 16.0).color(if enabled { k.c.icon_base } else { k.c.icon_faint }));
    if enabled {
        b = b.hover(k.c.hover).press(k.c.pressed).cursor(Cursor::Pointer).on_press(f);
    }
    b
}

fn navbar(s: &State, k: &Kit, p: usize) -> El {
    let pane = &s.panes[p];
    let mut path = row().key("crumbs").grow(1.0).min_w(0.0).h(26.0).px(4.0).center_y().radius(k.radius.sm).border(1.0, k.c.border_weak).clip_soft(8.0);
    let steps = crumbs(&pane.loc);
    let last = steps.len().saturating_sub(1);
    for (i, c) in steps.into_iter().enumerate() {
        if i > 0 {
            path = path.child(icon(&icons::CHEVRON, 12.0).color(k.c.icon_faint));
        }
        let to = c.to;
        path = path.child(
            row()
                .key(format!("crumb-{i}"))
                .role("link")
                .label(c.label.clone())
                .h(22.0)
                .px(5.0)
                .center_y()
                .radius(k.radius.sm)
                .min_w(0.0)
                .shrink()
                .hover(k.c.hover)
                .cursor(Cursor::Pointer)
                .on_press(move |s: &mut State| navigate(s, p, to.clone()))
                .child(
                    text(c.label.clone())
                        .font(if i == last { k.ty.text_small_medium } else { k.ty.text_small })
                        .color(if i == last { k.c.text_base } else { k.c.text_muted })
                        .truncate()
                        .shrink()
                        .min_w(0.0)
                        .help_when_truncated(c.label),
                ),
        );
    }
    row()
        .key("nav")
        .w_full()
        .h(36.0)
        .px(6.0)
        .gap(2.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(nav_button(k, "back", &icons::BACK, "Voltar (Alt+←)", !pane.back.is_empty(), move |s: &mut State| go_back(s, p)))
        .child(nav_button(k, "forward", &icons::FORWARD, "Avançar (Alt+→)", !pane.forward.is_empty(), move |s: &mut State| go_forward(s, p)))
        .child(nav_button(k, "up", &icons::UP, "Acima (Backspace)", can_go_up(&pane.loc), move |s: &mut State| go_up(s, p)))
        .child(path.mx(4.0))
        .child(nav_button(k, "reload", &icons::REFRESH, "Atualizar (Ctrl+R)", true, move |s: &mut State| load(s, p)))
}

fn can_go_up(loc: &Location) -> bool {
    match loc {
        Location::Local(dir) => dir.parent().is_some(),
        Location::Mock(segs) => !segs.is_empty(),
        Location::Drive { space, path, .. } => !path.is_empty() || matches!(space, Space::Shared { .. }),
    }
}

// ---------------------------------------------------------------------------------------------
// Location tree

/// The tree node a location sits under, to highlight it.
fn node_of(loc: &Location) -> String {
    match loc {
        Location::Local(dir) => {
            let home = match Location::home() {
                Location::Local(h) => h,
                _ => PathBuf::new(),
            };
            if dir.starts_with(&home) {
                "home".into()
            } else {
                format!("disk:{}", dir.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()).unwrap_or_default())
            }
        }
        Location::Mock(_) => "mock".into(),
        Location::Drive { email, space, .. } => match space {
            Space::MyDrive => format!("acc:{email}:my"),
            Space::SharedDrives => format!("acc:{email}:sd"),
            Space::Shared { id, .. } => format!("acc:{email}:sd:{id}"),
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn node(k: &Kit, p: usize, id: String, depth: usize, glyph: &'static Icon, label: &str, current: &str, to: Location) -> El {
    let on = id == current;
    row()
        .key(format!("node-{id}"))
        .role("treeitem")
        .label(label.to_string())
        .checked(on)
        .w_full()
        .h(ROW_H)
        .pl(8.0 + depth as f32 * 16.0)
        .pr(6.0)
        .gap(6.0)
        .center_y()
        .fill(if on { k.alpha(k.c.text_accent, if k.dark { 0.24 } else { 0.16 }) } else { Color::TRANSPARENT })
        .hover(if on { k.alpha(k.c.text_accent, 0.24) } else { k.alpha(k.c.text_base, 0.05) })
        .cursor(Cursor::Pointer)
        .on_press(move |s: &mut State| navigate(s, p, to.clone()))
        .child(icon(glyph, 16.0))
        .child(text(label.to_string()).font(k.ty.text_small).color(k.c.text_base).truncate().shrink().min_w(0.0).help_when_truncated(label.to_string()))
}

fn heading(k: &Kit, label: &str) -> El {
    row().w_full().h(24.0).px(10.0).pt(6.0).center_y().child(text(label.to_string()).font(k.ty.text_xsmall).color(k.c.text_faint))
}

fn tree(s: &State, k: &Kit, p: usize) -> El {
    let current = node_of(&s.panes[p].loc);
    let mut t = col().key("tree").w_full().pb(8.0);
    t = t.child(heading(k, "Este computador")).child(node(k, p, "home".into(), 0, &icons::HOME, "Pasta pessoal", &current, Location::home()));
    for (d, kind) in &s.local_drives {
        let letter = d.display().to_string().trim_end_matches('\\').to_string();
        t = t.child(node(k, p, format!("disk:{letter}"), 0, &icons::DISK, &kind.label(&letter), &current, Location::Local(d.clone())));
    }
    if !s.accounts.is_empty() {
        t = t.child(heading(k, "Google Drive"));
    }
    for a in &s.accounts {
        let e = &a.email;
        t = t
            .child(node(k, p, format!("acc:{e}"), 0, &icons::CLOUD, e, &current, Location::drive(e.clone(), Space::MyDrive)))
            .child(node(k, p, format!("acc:{e}:my"), 1, &icons::FOLDER, "Meu Drive", &current, Location::drive(e.clone(), Space::MyDrive)))
            .child(node(k, p, format!("acc:{e}:sd"), 1, &icons::SHARED, "Drives compartilhados", &current, Location::drive(e.clone(), Space::SharedDrives)));
        // The shared drives, once their list has been loaded.
        if let Some(drives) = s.cache.shared_drives(e) {
            for d in drives.iter() {
                let space = Space::Shared { id: d.id.clone(), name: d.name.clone() };
                t = t.child(node(k, p, format!("acc:{e}:sd:{}", d.id), 2, &icons::SHARED, &d.name, &current, Location::drive(e.clone(), space)));
            }
        }
    }
    t = t.child(heading(k, "Testes")).child(node(k, p, "mock".into(), 0, &icons::LAB, "Drive simulado", &current, Location::Mock(Vec::new())));
    col()
        .key("tree-scroll")
        .w(TREE_W)
        .h_full()
        
        .fill(k.alpha(k.c.text_base, if k.dark { 0.02 } else { 0.015 }))
        .scroll(ScrollSpec { stick_bottom: false })
        .child(t)
}

// ---------------------------------------------------------------------------------------------
// File list

fn list(s: &State, k: &Kit, p: usize) -> El {
    let pane = &s.panes[p];
    let arrow = |c: SortCol| if pane.sort.0 == c { if pane.sort.1 { "  ▲" } else { "  ▼" } } else { "" };
    let head = |key: &str, label: &str, c: SortCol, end: bool| {
        let mut h = row()
            .key(key.to_string())
            .h_full()
            .px(8.0)
            .center_y()
            .hover(k.c.hover)
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| set_sort(s, p, c))
            .child(text(format!("{label}{}", arrow(c))).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().min_w(0.0));
        if end {
            h = h.justify_end();
        }
        h.border_sides([0.0, 1.0, 0.0, 0.0], k.c.border_weak)
    };
    let header = row()
        .key("header")
        .w_full()
        .h(24.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(head("h-name", "Nome", SortCol::Name, false).w(0.0).grow(1.0).min_w(MIN_NAME))
        .child(head("h-size", "Tamanho", SortCol::Size, true).w(W_SIZE))
        .child(head("h-mod", "Data", SortCol::Modified, false).w(W_DATE))
        .child(head("h-kind", "Tipo", SortCol::Kind, false).w(W_KIND).shrink());

    let selected = k.alpha(k.c.text_accent, if k.dark { 0.26 } else { 0.18 });
    let hover = k.alpha(k.c.text_base, 0.045);
    let ring = k.c.border_focus;
    let (text_c, muted_c) = (k.c.text_base, k.c.text_muted);
    let font = k.ty.text_small;
    let build = move |s: &State, i: usize| -> El {
        let pane = &s.panes[p];
        let e = &pane.entries[i];
        let is_sel = pane.selected.contains(&e.id);
        let mut r = row().key(row_key(&e.id)).role("row").label(e.name.clone()).focus_selected(is_sel).w_full().h(ROW_H).center_y();
        r = if is_sel { r.fill(selected) } else { r.hover(hover) };
        if s.active == p && pane.cursor.as_deref() == Some(e.id.as_str()) {
            r = r.outline(1.0, -1.0, ring);
        }
        let (id_press, id_open, id_drag, id_start) = (e.id.clone(), e.id.clone(), e.id.clone(), e.id.clone());
        let cell = |w: f32, gives: bool| {
            let c = row().w(w).h_full().px(8.0).center_y().clip_soft(0.0);
            if gives { c.shrink() } else { c }
        };
        r.child(
            row()
                .w(0.0)
                .grow(1.0)
                .min_w(MIN_NAME)
                .h_full()
                .px(6.0)
                .gap(6.0)
                .center_y()
                .child(icon(kind_icon(e.kind()), 16.0))
                .child(text(e.name.clone()).font(font).color(text_c).truncate().shrink().min_w(0.0)),
        )
        .child(cell(W_SIZE, false).justify_end().child(text(e.size.map(format_size).unwrap_or_default()).font(font).color(muted_c).align(Align::End)))
        .child(cell(W_DATE, false).child(text(e.modified.map(format_time).unwrap_or_default()).font(font).color(muted_c).truncate().shrink().min_w(0.0)))
        .child(cell(W_KIND, true).child(text(e.kind_label()).font(font).color(muted_c).truncate().shrink().min_w(0.0)))
        .on_press_tx(move |s: &mut State, tx: &mut Tx| {
            click(s, p, &id_press, tx.mods());
            tx.focus(pane_key(p), false);
        })
        .on_double_press(move |s: &mut State| open(s, p, &id_open))
        .drag_out(move |s: &State| drag_item(s, p, &id_drag))
        .on_drag_start(move |s: &mut State, _| {
            s.active = p;
            let pane = &mut s.panes[p];
            if !pane.selected.contains(&id_start) {
                pane.selected = [id_start.clone()].into();
                pane.anchor = Some(id_start.clone());
                pane.cursor = Some(id_start.clone());
            }
            // `drag_out` already wrote the files; this only names them.
            let paths = paths_for(s, p, &dragged_ids(&s.panes[p], &id_start), false);
            s.last_drag = Some((p, paths));
        })
        .on_drag_end(move |s: &mut State, outcome: DragOutcome, _| {
            s.status = match outcome {
                DragOutcome::Cancelled => "Arrastar cancelado.".into(),
                o => format!("Arrastar terminou: {o:?}"),
            };
        })
    };

    let n = pane.entries.len();
    let rows = col()
        .key("rows")
        .w_full()
        .growth(GrowthSpec::default())
        .virtual_rows(n, ROW_H, move |s: &State, i| Cow::Owned(row_key(&s.panes[p].entries[i].id)), build)
        .virtual_rows_version(pane.version);
    let notice = |msg: String, color| col().key("notice").w_full().grow(1.0).padding(16.0).child(text(msg).font(k.ty.text_small).color(color));
    let body = if let Some(err) = &pane.error {
        notice(err.clone(), k.c.danger.fg)
    } else if pane.loading && n == 0 {
        notice("Carregando…".into(), k.c.text_muted)
    } else if n == 0 && matches!(pane.loc, Location::Drive { space: Space::SharedDrives, .. }) {
        notice("Esta conta não participa de nenhum drive compartilhado.".into(), k.c.text_muted)
    } else if n == 0 {
        notice("Esta pasta está vazia.".into(), k.c.text_faint)
    } else {
        col().key("viewport").w_full().grow(1.0).min_h(0.0).scroll(ScrollSpec { stick_bottom: false }).child(rows)
    };
    col().key("list").w(0.0).grow(1.0).h_full().min_w(0.0).child(header).child(body)
}

// ---------------------------------------------------------------------------------------------
// Status bar

fn statusbar(s: &State, k: &Kit, p: usize) -> El {
    let pane = &s.panes[p];
    let n = pane.entries.len();
    let mut left = format!("{} {}", thousands(n), if n == 1 { "item" } else { "itens" });
    let picked: Vec<_> = if pane.selected.is_empty() { Vec::new() } else { pane.entries.iter().filter(|e| pane.selected.contains(&e.id)).collect() };
    if !picked.is_empty() {
        let bytes: u64 = picked.iter().filter_map(|e| e.size).sum();
        left.push_str(&format!("   ·   {} {}", thousands(picked.len()), if picked.len() == 1 { "selecionado" } else { "selecionados" }));
        if bytes > 0 {
            left.push_str(&format!(" ({})", format_size(bytes)));
        }
    }
    if pane.loading {
        left.push_str("   ·   carregando…");
    }
    let right = pane.free.map(|b| format!("{} livres", format_size(b))).unwrap_or_default();
    row()
        .key("statusbar")
        .w_full()
        .h(24.0)
        .px(10.0)
        .center_y()
        .border_sides([1.0, 0.0, 0.0, 0.0], k.c.border_weak)
        .child(text(left).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().min_w(0.0).grow(1.0))
        .child(text(right).font(k.ty.text_xsmall).color(k.c.text_muted))
}

pub fn thousands(n: usize) -> String {
    let d = n.to_string();
    d.chars().enumerate().fold(String::new(), |mut out, (i, ch)| {
        if i > 0 && (d.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(ch);
        out
    })
}
