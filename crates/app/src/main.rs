//! Open Explorer prototype: two panes side by side, the local disk on the left and a simulated
//! Google Drive on the right. It checks what Rune gives a dual-pane file manager on Windows:
//! virtual lists of 50,000 rows, dragging between panes, dropping files in from Explorer and
//! dragging files out to Explorer.

mod model;

use std::borrow::Cow;
use std::path::PathBuf;
use std::time::SystemTime;

use rune::{Align, AppSpec, Cursor, Cx, DragItem, DragOutcome, El, GrowthSpec, Key, KeyEvent, Mods, ScrollSpec, Tx, col, el, row, text};
use rune_kit::Kit;

use model::{Entry, Location, MockDrive, Pane, SortCol, format_size, format_time};

const ROW_H: f32 = 26.0;

struct State {
    panes: [Pane; 2],
    drive: MockDrive,
    active: usize,
    /// The pane a drag started in and the files it carried, so a drop back on the same pane
    /// is ignored (the drop can arrive after the drag's end).
    last_drag: Option<(usize, Vec<PathBuf>)>,
    status: String,
    temp: PathBuf,
}

fn init() -> State {
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\"));
    let drive = MockDrive::new();
    let mut panes = [Pane::new(Location::Local(home)), Pane::new(Location::Cloud(Vec::new()))];
    for p in &mut panes {
        p.load(&drive);
    }
    State {
        panes,
        drive,
        active: 0,
        last_drag: None,
        status: "Arraste arquivos entre os painéis, do Explorer para cá ou daqui para o Explorer.".into(),
        temp: std::env::temp_dir().join("OpenExplorer-drag"),
    }
}

fn pane_key(p: usize) -> String {
    format!("pane-{p}")
}

fn row_key(name: &str) -> String {
    format!("r:{name}")
}

// ---------------------------------------------------------------------------------------------
// Actions

fn reload(s: &mut State, p: usize) {
    let State { panes, drive, .. } = s;
    panes[p].load(drive);
}

fn open(s: &mut State, p: usize, name: &str) {
    let pane = &mut s.panes[p];
    let Some(e) = pane.entries.iter().find(|e| e.name == name) else { return };
    if !e.is_dir {
        s.status = format!("Abrir \"{name}\" ainda não faz parte do protótipo.");
        return;
    }
    match &mut pane.loc {
        Location::Local(dir) => dir.push(name),
        Location::Cloud(segs) => segs.push(name.to_string()),
    }
    pane.selected.clear();
    pane.cursor = None;
    pane.anchor = None;
    reload(s, p);
}

fn go_up(s: &mut State, p: usize) {
    let pane = &mut s.panes[p];
    let from = match &mut pane.loc {
        Location::Local(dir) => {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
            if !dir.pop() {
                return;
            }
            name
        }
        Location::Cloud(segs) => segs.pop(),
    };
    reload(s, p);
    // Land on the folder we came out of.
    if let Some(name) = from {
        let pane = &mut s.panes[p];
        pane.selected = [name.clone()].into();
        pane.cursor = Some(name.clone());
        pane.anchor = Some(name);
    }
}

fn click(s: &mut State, p: usize, name: &str, mods: Mods) {
    s.active = p;
    let pane = &mut s.panes[p];
    if mods.cmd {
        if !pane.selected.remove(name) {
            pane.selected.insert(name.to_string());
        }
        pane.anchor = Some(name.to_string());
    } else if mods.shift && pane.anchor.is_some() {
        let a = pane.anchor.as_deref().and_then(|a| pane.index_of(a));
        let b = pane.index_of(name);
        if let (Some(a), Some(b)) = (a, b) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            pane.selected = pane.entries[lo..=hi].iter().map(|e| e.name.clone()).collect();
        }
    } else {
        pane.selected = [name.to_string()].into();
        pane.anchor = Some(name.to_string());
    }
    pane.cursor = Some(name.to_string());
}

fn key(s: &mut State, p: usize, ev: &KeyEvent, tx: &mut Tx) -> bool {
    let n = s.panes[p].entries.len();
    let at = s.panes[p].cursor.as_deref().and_then(|c| s.panes[p].index_of(c));
    let step = |d: isize| -> Option<usize> {
        if n == 0 {
            return None;
        }
        Some(match at {
            None => 0,
            Some(i) => (i as isize + d).clamp(0, n as isize - 1) as usize,
        })
    };
    let to = match &ev.key {
        Key::Up => step(-1),
        Key::Down => step(1),
        Key::PageUp => step(-20),
        Key::PageDown => step(20),
        Key::Home => step(-(n as isize)),
        Key::End => step(n as isize),
        Key::Enter => {
            if let Some(c) = s.panes[p].cursor.clone() {
                open(s, p, &c);
            }
            return true;
        }
        Key::Backspace => {
            go_up(s, p);
            return true;
        }
        Key::Tab => {
            s.active = 1 - p;
            tx.focus(pane_key(1 - p), true);
            return true;
        }
        _ if ev.is_command('a') => {
            let pane = &mut s.panes[p];
            pane.selected = pane.entries.iter().map(|e| e.name.clone()).collect();
            return true;
        }
        _ if ev.is_command('r') => {
            reload(s, p);
            return true;
        }
        _ => return false,
    };
    if let Some(i) = to {
        let name = s.panes[p].entries[i].name.clone();
        click(s, p, &name, Mods { shift: ev.mods.shift, ..Mods::default() });
        tx.scroll_into_view_in(pane_key(p), row_key(&name));
    }
    true
}

fn set_sort(s: &mut State, p: usize, col: SortCol) {
    let pane = &mut s.panes[p];
    pane.sort = if pane.sort.0 == col { (col, !pane.sort.1) } else { (col, true) };
    pane.sort_entries();
}

/// What a drag from pane `p` that started on `name` carries: the selection if `name` is in it,
/// else `name` alone. Drive files become small placeholder files in the temp folder first, as a
/// real download would.
fn drag_item(s: &State, p: usize, name: &str) -> Option<DragItem> {
    let pane = &s.panes[p];
    let names: Vec<&str> = if pane.selected.contains(name) {
        pane.entries.iter().filter(|e| pane.selected.contains(&e.name)).map(|e| e.name.as_str()).collect()
    } else {
        vec![name]
    };
    let paths = paths_for(s, p, &names);
    if paths.is_empty() {
        return None;
    }
    Some(paths.into_iter().fold(DragItem::new(), DragItem::file).within_app())
}

fn paths_for(s: &State, p: usize, names: &[&str]) -> Vec<PathBuf> {
    let pane = &s.panes[p];
    match &pane.loc {
        Location::Local(dir) => names.iter().map(|n| dir.join(n)).collect(),
        Location::Cloud(segs) => {
            let dir = s.temp.join(if segs.is_empty() { "Meu Drive".to_string() } else { segs.join("_") });
            if std::fs::create_dir_all(&dir).is_err() {
                return Vec::new();
            }
            names
                .iter()
                .filter_map(|n| {
                    let e = pane.entries.iter().find(|e| e.name == *n)?;
                    let path = dir.join(n);
                    let ok = if e.is_dir {
                        std::fs::create_dir_all(&path).is_ok()
                    } else {
                        path.exists() || std::fs::write(&path, format!("Arquivo simulado do Google Drive: {}/{n}\n", pane.title())).is_ok()
                    };
                    ok.then_some(path)
                })
                .collect()
        }
    }
}

fn dropped(s: &mut State, p: usize, files: &[PathBuf]) {
    if let Some((from, paths)) = &s.last_drag
        && *from == p
        && paths.as_slice() == files
    {
        s.last_drag = None;
        return;
    }
    s.last_drag = None;
    s.active = p;
    let loc = s.panes[p].loc.clone();
    let (mut done, mut skipped, mut failed) = (0, 0, 0);
    match &loc {
        Location::Local(dir) => {
            for f in files {
                let Some(name) = f.file_name() else { continue };
                let to = dir.join(name);
                if to.exists() || f.parent() == Some(dir.as_path()) {
                    skipped += 1;
                } else if f.is_dir() {
                    // Copying folders comes with the real transfer queue.
                    skipped += 1;
                } else if std::fs::copy(f, &to).is_ok() {
                    done += 1;
                } else {
                    failed += 1;
                }
            }
        }
        Location::Cloud(segs) => {
            for f in files {
                let Some(name) = f.file_name() else { continue };
                let meta = std::fs::metadata(f).ok();
                let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
                let entry = Entry {
                    name: name.to_string_lossy().into_owned(),
                    is_dir,
                    size: if is_dir { None } else { meta.as_ref().map(|m| m.len()) },
                    modified: Some(SystemTime::now()),
                };
                if s.drive.add(segs, entry) { done += 1 } else { skipped += 1 }
            }
        }
    }
    let verb = if matches!(loc, Location::Local(_)) { "Copiados" } else { "Enviados (simulado)" };
    s.status = format!("{verb}: {done} · ignorados (já existem ou pastas): {skipped} · falhas: {failed}");
    reload(s, p);
}

// ---------------------------------------------------------------------------------------------
// View

fn view(s: &State, cx: &mut Cx<State>) -> El {
    let k = Kit::neutral(cx.system_dark());
    let status = text(s.status.clone()).font(k.ty.text_small).color(k.c.text_muted).truncate().shrink().min_w(0.0);
    col()
        .w_full()
        .h_full()
        .fill(k.c.page)
        .padding(10.0)
        .gap(8.0)
        .child(row().key("panes").w_full().grow(1.0).min_h(0.0).gap(8.0).child(pane(s, &k, 0)).child(pane(s, &k, 1)))
        .child(row().key("status").w_full().px(4.0).child(status))
}

fn button<F: Fn(&mut State) + 'static>(k: &Kit, key: &str, label: &str, f: F) -> El {
    row()
        .key(key.to_string())
        .role("button")
        .label(label.to_string())
        .h(24.0)
        .px(8.0)
        .center_y()
        .radius(k.radius.sm)
        .border(1.0, k.c.border_weak)
        .hover(k.c.hover)
        .press(k.c.pressed)
        .cursor(Cursor::Pointer)
        .on_press(f)
        .child(text(label.to_string()).font(k.ty.text_small).color(k.c.text_base))
}

fn pane(s: &State, k: &Kit, p: usize) -> El {
    let pane = &s.panes[p];
    let active = s.active == p;
    let (badge, badge_color) = match pane.loc {
        Location::Local(_) => ("Local", k.c.text_accent),
        Location::Cloud(_) => ("Google Drive (simulado)", k.c.success.icon),
    };
    let toolbar = row()
        .key("toolbar")
        .w_full()
        .h(36.0)
        .px(8.0)
        .gap(8.0)
        .center_y()
        .child(text(badge).font(k.ty.text_small_medium).color(badge_color))
        .child(button(k, "up", "↑ Acima", move |s: &mut State| go_up(s, p)))
        .child(button(k, "reload", "Atualizar", move |s: &mut State| reload(s, p)))
        .child(text(pane.title()).font(k.ty.text_small).color(k.c.text_muted).truncate().shrink().min_w(0.0).grow(1.0).help_when_truncated(pane.title()));

    let arrow = |c: SortCol| if pane.sort.0 == c { if pane.sort.1 { " ▲" } else { " ▼" } } else { "" };
    let head = |key: &str, label: &str, c: SortCol| {
        row()
            .key(key.to_string())
            .h_full()
            .px(8.0)
            .center_y()
            .hover(k.c.hover)
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| set_sort(s, p, c))
            .child(text(format!("{label}{}", arrow(c))).font(k.ty.text_small_medium).color(k.c.text_muted).truncate().shrink().min_w(0.0))
    };
    let header = row()
        .key("header")
        .w_full()
        .h(28.0)
        .border(0.0, k.c.border_weak)
        .fill(k.alpha(k.c.text_base, 0.03))
        .child(head("h-name", "Nome", SortCol::Name).w(0.0).grow(1.0).min_w(0.0))
        .child(head("h-size", "Tamanho", SortCol::Size).w(100.0).justify_end())
        .child(head("h-mod", "Modificado", SortCol::Modified).w(140.0));

    let selected = k.alpha(k.c.text_accent, if k.dark { 0.24 } else { 0.2 });
    let hover = k.alpha(k.c.text_base, 0.05);
    let ring = k.c.border_focus;
    let (text_c, muted_c, folder_c, file_c) = (k.c.text_base, k.c.text_muted, rune::oklch(0.82, 0.15, 75.0), k.c.icon_faint);
    let (font, mono) = (k.ty.text_small, k.ty.mono);
    let build = move |s: &State, i: usize| -> El {
        let pane = &s.panes[p];
        let e = &pane.entries[i];
        let is_sel = pane.selected.contains(&e.name);
        let name = e.name.clone();
        let mut r = row().key(row_key(&e.name)).role("row").label(e.name.clone()).focus_selected(is_sel).w_full().h(ROW_H).center_y();
        r = if is_sel { r.fill(selected) } else { r.hover(hover) };
        if s.active == p && pane.cursor.as_deref() == Some(e.name.as_str()) {
            r = r.outline(1.0, -1.0, ring);
        }
        let icon = el().size(14.0, if e.is_dir { 11.0 } else { 14.0 }).radius(2.0).fill(if e.is_dir { folder_c } else { file_c });
        let n_press = name.clone();
        let n_open = name.clone();
        let n_drag = name.clone();
        let n_start = name.clone();
        r.child(
            row().w(0.0).grow(1.0).min_w(0.0).px(8.0).gap(8.0).center_y().child(icon).child(text(e.name.clone()).font(font).color(text_c).truncate().shrink().min_w(0.0)),
        )
        .child(
            row().w(100.0).px(8.0).justify_end().child(text(e.size.map(format_size).unwrap_or_default()).font(mono).color(muted_c).align(Align::End)),
        )
        .child(row().w(140.0).px(8.0).child(text(e.modified.map(format_time).unwrap_or_default()).font(font).color(muted_c).truncate()))
        .on_press_tx(move |s: &mut State, tx: &mut Tx| {
            click(s, p, &n_press, tx.mods());
            tx.focus(pane_key(p), false);
        })
        .on_double_press(move |s: &mut State| open(s, p, &n_open))
        .drag_out(move |s: &State| drag_item(s, p, &n_drag))
        .on_drag_start(move |s: &mut State, _| {
            s.active = p;
            let pane = &mut s.panes[p];
            if !pane.selected.contains(&n_start) {
                pane.selected = [n_start.clone()].into();
                pane.anchor = Some(n_start.clone());
                pane.cursor = Some(n_start.clone());
            }
            let pane = &s.panes[p];
            let names: Vec<&str> = pane.entries.iter().filter(|e| pane.selected.contains(&e.name)).map(|e| e.name.as_str()).collect();
            let paths = paths_for(s, p, &names);
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
        .virtual_rows(n, ROW_H, move |s: &State, i| Cow::Owned(row_key(&s.panes[p].entries[i].name)), build)
        .virtual_rows_version(pane.version);
    let mut body = col().key("viewport").w_full().grow(1.0).min_h(0.0).scroll(ScrollSpec { stick_bottom: false }).child(rows);
    if let Some(err) = &pane.error {
        body = col().key("error").w_full().grow(1.0).padding(16.0).child(text(err.clone()).font(k.ty.text_small).color(k.c.danger.fg));
    }

    let footer = text(format!("{} itens · {} selecionados", thousands(n), thousands(pane.selected.len())))
        .font(k.ty.text_xsmall)
        .color(k.c.text_faint);

    let border = if pane.drop_over {
        k.c.text_accent
    } else if active {
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
        .radius(k.radius.md)
        .fill(if pane.drop_over { k.alpha(k.c.text_accent, 0.08) } else { k.c.bg_base })
        .border(if pane.drop_over { 2.0 } else { 1.0 }, border)
        .clip_rounded(k.radius.md)
        .on_key(move |s: &mut State, ev: &KeyEvent, tx: &mut Tx| key(s, p, ev, tx))
        .on_press(move |s: &mut State| s.active = p)
        .on_drop_files(move |s: &mut State, files: &[PathBuf], _| dropped(s, p, files))
        .on_drop_hover(move |s: &mut State, over, _| s.panes[p].drop_over = over)
        .child(toolbar)
        .child(header)
        .child(body)
        .child(row().key("footer").w_full().h(24.0).px(10.0).center_y().child(footer))
}

fn thousands(n: usize) -> String {
    let d = n.to_string();
    d.chars().enumerate().fold(String::new(), |mut out, (i, ch)| {
        if i > 0 && (d.len() - i) % 3 == 0 {
            out.push('.');
        }
        out.push(ch);
        out
    })
}

fn main() {
    rune::run(AppSpec::<State> {
        min_size: Some((700.0, 400.0)),
        scenes: &[("stress", |s| {
            s.panes[1].loc = Location::Cloud(vec!["Teste 50.000 arquivos".into()]);
            let State { panes, drive, .. } = s;
            panes[1].load(drive);
        })],
        ..AppSpec::new("Open Explorer", (1200.0, 720.0), init, view)
    });
}
