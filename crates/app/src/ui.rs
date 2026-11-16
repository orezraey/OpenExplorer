//! The window: a toolbar, two panes (each a location tree beside a file list, under a
//! navigation bar and over a status bar), and the message line.

use std::borrow::Cow;
use std::path::PathBuf;

use rune::{Align, Color, Cursor, Cx, Drag, DragOutcome, El, GrowthSpec, Icon, KeyEvent, Phase, ScrollSpec, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, Dialog as Modal, Kit, MenuItem, ScrollBar, ScrollBarStyle, Toast, ToastRegion, context_menu};

use crate::model::{Kind, Location, SortCol, Space, crumbs, format_bytes, format_long, format_size, format_time, shown};
use crate::actions::{self, ClipMode, Dialog};
use crate::{State, click, connect, disconnect, drag_item, dragged_ids, dropped, go_back, go_forward, go_up, key, load, navigate, open, pane_key, paths_for, row_key, set_sort};
use crate::icons;
use crate::ops::Stage;
use crate::sysicons::{self, IconKey};
use crate::transfers;
use crate::layout::{self, clamp_col, clamp_tree};

const ROW_H: f32 = crate::ROW_H;

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    let k = Kit::neutral(s.settings.theme.dark(cx.system_dark()));
    // Menus and dialogs draw in a layer above everything.
    let mut layer = Vec::new();
    let menu = context(s, &k, &mut layer);
    let queue_menu = transfer_context(s, &k, &mut layer);
    let address_menu = address_context(s, &k, &mut layer);
    dialog(s, &k, &mut layer);
    // What the last action said, for a few seconds at the bottom right.
    if !s.status.is_empty() && s.now < s.status_until {
        let close = |s: &mut State| s.status_until = 0.0;
        layer.push(ToastRegion::new().toast(Toast::new("notice", s.status.clone()).on_close(close).build(&k)).build(&k));
    }
    let body = col()
        .w_full()
        .h_full()
        .fill(k.c.page)
        .child(toolbar(s, &k))
        .child(panes(s, &k))
        .child(transfers(s, &k))
        .child(menu)
        .child(queue_menu)
        .child(address_menu);
    rune_kit::root(body, layer)
}

// ---------------------------------------------------------------------------------------------
// Opções

/// A label and its control, on one line of Opções.
fn option_row(k: &Kit, label: &str, control: El) -> El {
    row().w_full().h(32.0).gap(12.0).center_y().child(text(label.to_string()).font(k.ty.text_small).color(k.c.text_base).w(190.0)).child(control)
}

/// A number with − and + beside it, as Air Explorer's spin boxes.
fn stepper(k: &Kit, key: &str, value: u8, set: fn(&mut State, i32)) -> El {
    let button = |label: &str, delta: i32, enabled: bool| {
        Button::new(label).variant(ButtonVariant::Neutral).disabled(!enabled).on_press(move |s: &mut State| set(s, delta)).build(k).key(format!("{key}-{delta}"))
    };
    row()
        .key(key.to_string())
        .gap(6.0)
        .center_y()
        .child(button("−", -1, value > 1))
        .child(text(value.to_string()).font(k.ty.text_small_medium).color(k.c.text_base).w(28.0).align(Align::Center))
        .child(button("+", 1, value < crate::settings::MAX_AT_ONCE))
}

fn change_settings(s: &mut State, f: impl FnOnce(&mut crate::settings::Settings)) {
    f(&mut s.settings);
    s.settings.save();
}

fn options_dialog(s: &State, k: &Kit, layer: &mut Vec<El>) {
    use crate::settings::{Theme, at_once};
    let st = s.settings;
    let theme = rune_kit::SegmentedControl::new(&Theme::ALL.map(Theme::label), Theme::ALL.iter().position(|t| *t == st.theme))
        .label("Tema")
        .on_pick(|s: &mut State, i: usize, _: &mut Tx| change_settings(s, |c| c.theme = Theme::ALL[i]))
        .build(k);
    let note = |t: &str| text(t.to_string()).font(k.ty.text_xsmall).color(k.c.text_muted).wrap().w_full();
    let segmented = rune_kit::Checkbox::new(st.segmented_downloads)
        .label("Baixar arquivos grandes em segmentos paralelos")
        .description("Para aumentar a velocidade.")
        .on_toggle(|s: &mut State| change_settings(s, |c| c.segmented_downloads = !c.segmented_downloads))
        .build(k);
    let body = col()
        .key("content")
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(6.0)
        .child(option_row(k, "Tema", theme))
        .child(option_row(k, "Uploads simultâneos", stepper(k, "uploads", st.uploads, |s, d| change_settings(s, |c| c.uploads = at_once(i32::from(c.uploads) + d)))))
        .child(option_row(k, "Downloads simultâneos", stepper(k, "downloads", st.downloads, |s, d| change_settings(s, |c| c.downloads = at_once(i32::from(c.downloads) + d)))))
        .child(el().h(6.0))
        .child(segmented)
        .child(note("Valem a partir da próxima transferência. Os segmentos paralelos são para arquivos de 64 MB ou mais."))
        .child(row().key("actions").w_full().pt(12.0).justify_end().child(Button::new("OK").on_press(close).build(k)));
    Modal::new("options", "Opções").open(true).child(body).width(500.0).fit().on_close(close).build(k, layer);
}

// ---------------------------------------------------------------------------------------------
// Transfer queue

/// The queue's columns, as in Air Explorer.
const QUEUE_LABELS: [&str; 8] = ["Operação", "Origem", "Destino", "Estado", "Tamanho", "Progresso", "Velocidade", "Estimado"];

fn queue_cell(w: f32) -> El {
    row().w(w).h_full().px(6.0).gap(5.0).center_y().clip_soft(0.0)
}

/// Origem or Destino: the disk's (or Drive's) icon and the path.
fn queue_end(s: &State, k: &Kit, w: f32, end: &transfers::End) -> El {
    let glyph = match &end.disk {
        Some(disk) => shell_icon(s, Some(&sysicons::drive_key(disk)), &icons::DISK, 14.0),
        None => icon(&icons::CLOUD, 14.0),
    };
    queue_cell(w).child(glyph).child(text(end.text.clone()).font(k.ty.text_xsmall).color(k.c.text_base).truncate().shrink().min_w(0.0).help_when_truncated(end.text.clone()))
}

/// The bar with its percentage over it, as in Air Explorer.
fn queue_bar(k: &Kit, fraction: Option<f32>) -> El {
    let label = fraction.map(|f| format!("{}%", (f * 100.0).floor())).unwrap_or_else(|| "…".into());
    let fill = el().key("fill").h_full().fill(k.alpha(k.c.text_accent, 0.35));
    row()
        .key("bar")
        .role("progressbar")
        .label(label.clone())
        .w(0.0)
        .grow(1.0)
        .max_w(120.0)
        .h(15.0)
        .border(1.0, k.c.border_base)
        .fill(k.c.bg_base)
        .clip_soft(0.0)
        .child(fill.w_frac(fraction.unwrap_or(0.0)))
        .child(row().pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0)).center().child(text(label).font(k.ty.text_xsmall).color(k.c.text_base)))
}

fn queue_row(s: &State, k: &Kit, t: &transfers::Transfer) -> El {
    use std::sync::atomic::Ordering::Relaxed;
    let p = &t.progress;
    let w = s.layout.queue_cols;
    let muted = |t: String| text(t).font(k.ty.text_xsmall).color(k.c.text_base).truncate().shrink().min_w(0.0);
    let (state, dot) = match p.stage() {
        Stage::Waiting if p.cancelled() => ("Cancelando…", k.c.text_faint),
        Stage::Waiting => ("Aguardando", k.c.text_faint),
        Stage::Running if p.cancelled() => ("Cancelando…", k.c.warning.fg),
        Stage::Running => ("Em andamento", k.c.success.fg),
        Stage::Done => ("Concluído", k.c.text_accent),
        Stage::Cancelled => ("Cancelado", k.c.warning.fg),
        Stage::Failed => ("Erro", k.c.danger.fg),
    };
    // A folder's row counts its items while running: "Em andamento · 120 de 1.946".
    let items = p.items.load(Relaxed);
    let state = if p.stage() == Stage::Running && items > 1 { format!("{state} · {} de {}", ui_count(p.items_done.load(Relaxed) as usize), ui_count(items as usize)) } else { state.to_string() };
    let mut state_cell = queue_cell(w[3]).child(el().size(8.0, 8.0).radius(4.0).fill(dot)).child(muted(state.clone()).help_when_truncated(state));
    if p.stage() == Stage::Failed {
        state_cell = state_cell.help(p.error());
    }
    let sized = p.sized.load(Relaxed);
    let size = if sized { transfers::pair(p.bytes.load(Relaxed), p.total.load(Relaxed)) } else { String::new() };
    let speed = t.speed().map(|v| format!("{}/s", format_size(v as u64))).unwrap_or_default();
    let left = t.left().map(transfers::hms).unwrap_or_default();
    let (op_icon, op) = match t.op {
        transfers::Op::Copy => (&icons::M_COPY, t.op.label()),
        transfers::Op::Move => (&icons::M_CUT, t.op.label()),
        transfers::Op::Upload => (&icons::UPLOAD, t.op.label()),
        transfers::Op::Download => (&icons::DOWNLOAD, t.op.label()),
    };
    row()
        .key(format!("t-{:p}", std::sync::Arc::as_ptr(p)))
        .role("row")
        .label(format!("{op} {}", t.from.text))
        .w_full()
        .h(22.0)
        .center_y()
        .hover(k.alpha(k.c.text_base, 0.045))
        .on_secondary_press({
            let row = p.clone();
            move |s: &mut State, at: rune::PointerPress, _: &mut Tx| s.transfer_menu = Some((Some(row.clone()), at))
        })
        .child(queue_cell(w[0]).child(icon(op_icon, 14.0).color(k.c.icon_base)).child(muted(op.into())))
        .child(queue_end(s, k, w[1], &t.from))
        .child(queue_end(s, k, w[2], &t.to))
        .child(state_cell)
        .child(queue_cell(w[4]).child(muted(size)))
        .child(queue_cell(w[5]).child(queue_bar(k, t.fraction())))
        .child(queue_cell(w[6]).child(muted(speed)))
        .child(queue_cell(w[7]).child(muted(left)))
}

/// The transfer queue under the panes: a row per item being copied or moved. It shows only
/// while there's a row; its top edge drags to change its height, and its columns resize as the
/// file list's do.
/// The transfer queue's column header, stuck to the top of its viewport.
const QUEUE_HEADER_H: f32 = 22.0;

fn transfers(s: &State, k: &Kit) -> El {
    if s.transfers.is_empty() {
        return el().key("queue-wrap");
    }
    let w = s.layout.queue_cols;
    let total: f32 = w.iter().sum();
    let grips = (0..8).map(|i| {
        let edge = w[..=i].iter().sum();
        grip(k, &format!("queue-grip-{i}"), edge, QUEUE_HEADER_H, move |s| &mut s.layout.queue_cols[i], layout::QUEUE_COLS[i], move |v| layout::clamp_queue_col(i, v))
    });
    let header = row()
        .key("queue-header")
        .w_full()
        .h(QUEUE_HEADER_H)
        .fill(k.c.bg_base)
        .sticky_top(0.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .children(QUEUE_LABELS.iter().enumerate().map(|(i, label)| {
            queue_cell(w[i]).border_sides([0.0, 1.0, 0.0, 0.0], k.c.border_weak).child(text(label.to_string()).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().min_w(0.0))
        }))
        .children(grips);
    // Only the rows in view are built: copying a 5,000-file folder makes 5,000 rows.
    let kit = k.clone();
    // As in the file list: the bar runs below the header, beside the rows.
    let bar = bar_style(k).track_offset(QUEUE_HEADER_H, 0.0);
    let rows = col().key("queue-rows").w_full().pr(bar.strip()).growth(GrowthSpec::default()).virtual_rows(
        s.transfers.len(),
        22.0,
        |s: &State, i| Cow::Owned(format!("t-{:p}", std::sync::Arc::as_ptr(&s.transfers[i].progress))),
        move |s: &State, i| queue_row(s, &kit, &s.transfers[i]),
    );
    // One viewport scrolls both ways, as the file list's: sideways when the columns are wider
    // than the window, with the header stuck to the top.
    let table = col().key("queue-table").w_full().min_w(total + bar.strip()).child(header).child(rows);
    let queue = col()
        .key("queue")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(k.c.bg_base)
        .border(1.0, k.c.border_weak)
        .clip_soft(0.0)
        .on_secondary_press(|s: &mut State, at: rune::PointerPress, _: &mut Tx| s.transfer_menu = Some((None, at)))
        .child(col().key("queue-viewport").w_full().grow(1.0).min_h(0.0).scroll_xy().child(table).scroll_bar_styled(bar))
        .child(row().key("queue-foot").w_full().h(22.0).px(8.0).justify_end().center_y().border_sides([1.0, 0.0, 0.0, 0.0], k.c.border_weak).child(queue_totals(s, k)));
    // The strip above the queue drags up and down to change its height. On a window too
    // short for it, the queue gives way rather than the panes.
    let edge = splitter(k, "queue-edge", 6.0, Split::Up, |s: &mut State| &mut s.layout.queue_h, layout::QUEUE_H, layout::clamp_queue_h);
    col().key("queue-wrap").w_full().h(s.layout.queue_h + 6.0).shrink().min_h(layout::QUEUE_H_MIN).px(6.0).child(edge).child(queue)
}

/// The queue's foot while something is under way: "Total: 0,05 / 2,66 GB", a bar, and the
/// speed of everything running.
fn queue_totals(s: &State, k: &Kit) -> El {
    use std::sync::atomic::Ordering::Relaxed;
    let live: Vec<&transfers::Transfer> = s.transfers.iter().filter(|t| !t.progress.over()).collect();
    if live.is_empty() {
        return el().key("totals");
    }
    let done: u64 = live.iter().map(|t| t.progress.bytes.load(Relaxed)).sum();
    let total: u64 = live.iter().map(|t| t.progress.total.load(Relaxed)).sum();
    let speed: f64 = live.iter().filter_map(|t| t.speed()).sum();
    let fraction = if total > 0 { (done as f64 / total as f64).min(1.0) as f32 } else { 0.0 };
    let small = |t: String| text(t).font(k.ty.text_xsmall).color(k.c.text_muted);
    row()
        .key("totals")
        .gap(8.0)
        .center_y()
        .child(small(format!("Total: {}", transfers::pair(done, total))))
        .child(row().w(80.0).h(10.0).border(1.0, k.c.border_base).child(el().h_full().w_frac(fraction).fill(k.alpha(k.c.text_accent, 0.5))))
        .child(small(if speed > 0.0 { format!("{}/s", format_size(speed as u64)) } else { String::new() }))
}

/// The queue's right-click menu: on a row, or on the list as a whole.
fn transfer_context(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let anchor = el().key("queue-menu-anchor").size(0.0, 0.0);
    let Some((row, at)) = s.transfer_menu.clone() else { return anchor };
    let item = |label: &str, enabled: bool, f: Box<dyn Fn(&mut State)>| {
        MenuItem::new(label).disabled(!enabled).on_select_tx(move |s: &mut State, _: &mut Tx| {
            s.transfer_menu = None;
            f(s);
        })
    };
    let mut menu = context_menu("queue-menu", anchor, Some(at)).min_width(200.0);
    if let Some(row) = row {
        let (over, other) = (row.over(), row.clone());
        menu = menu
            .item(item("Cancelar", !over, Box::new(move |_| actions::cancel_transfer(&row))))
            .item(item("Remover da lista", over, Box::new(move |s| actions::remove_transfer(s, &other))))
            .separator();
    }
    let (any_live, any_over) = (s.transfers.iter().any(|t| !t.progress.over()), s.transfers.iter().any(|t| t.progress.over()));
    menu.item(item("Cancelar todas", any_live, Box::new(actions::cancel_all_transfers)))
        .item(item("Limpar finalizadas", any_over, Box::new(actions::clear_transfers)))
        .on_close(|s: &mut State| s.transfer_menu = None)
        .build(k, layer)
}

/// The path bar's right-click menu: copy the address (or the Drive link), paste one and go
/// there, or edit it.
fn address_context(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let anchor = el().key("address-menu-anchor").size(0.0, 0.0);
    let Some((p, at)) = s.address_menu else { return anchor };
    let item = |label: &str, glyph: &'static Icon, keys: &str, f: fn(&mut State, usize, &mut Tx)| {
        MenuItem::new(label).icon(glyph).shortcut(keys).on_select_tx(move |s: &mut State, tx: &mut Tx| {
            s.address_menu = None;
            f(s, p, tx);
        })
    };
    let mut menu = context_menu("address-menu", anchor, Some(at)).min_width(220.0).item(item("Copiar endereço", &icons::M_COPY, "", actions::copy_address));
    if crate::address::drive_link(&s.panes[p].loc).is_some() {
        menu = menu.item(item("Copiar link do Drive", &icons::M_LINK, "", actions::copy_drive_link));
    }
    menu.item(item("Colar e ir", &icons::M_PASTE, "", actions::paste_and_go))
        .separator()
        .item(item("Editar endereço", &icons::M_RENAME, "Ctrl+L", |s, p, _| actions::edit_address(s, p)))
        .on_close(|s: &mut State| s.address_menu = None)
        .build(k, layer)
}

// ---------------------------------------------------------------------------------------------
// Context menu

/// The right-click menu of the pane it was opened in: on the selection, or on the folder
/// when opened over empty space.
fn context(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let anchor = el().key("menu-anchor").size(0.0, 0.0);
    let Some((p, at)) = s.menu else { return anchor };
    let n = s.panes[p].selected.len();
    let editable = actions::editable(&s.panes[p].loc);
    let item = |label: &str, glyph: &'static Icon, keys: &str, enabled: bool, f: fn(&mut State, usize, &mut Tx)| {
        MenuItem::new(label).icon(glyph).shortcut(keys).disabled(!enabled).on_select_tx(move |s: &mut State, tx: &mut Tx| {
            s.menu = None;
            f(s, p, tx);
        })
    };
    let mut menu = context_menu("menu", anchor, Some(at)).min_width(220.0);
    // Lixeira: what can come back, or go for good.
    if actions::in_trash(&s.panes[p].loc) {
        // Wide enough for "Excluir permanentemente" and its key.
        menu = menu.min_width(270.0);
        if n > 0 {
            menu = menu
                .item(item("Abrir", &icons::M_OPEN, "Enter", n == 1, actions::open))
                .separator()
                .item(item("Restaurar", &icons::M_RESTORE, "", true, |s, p, _| actions::restore(s, p)))
                .item(item("Excluir permanentemente", &icons::M_DELETE, "Del", true, |s, p, _| actions::ask_purge(s, p)).danger(true))
                .separator()
                .item(item("Propriedades", &icons::M_PROPERTIES, "Alt+Enter", true, |s, p, _| actions::properties(s, p)))
                .separator();
        }
        menu = menu.item(item("Esvaziar lixeira", &icons::M_EMPTY_TRASH, "", true, |s, p, _| actions::ask_empty_trash(s, p)).danger(true));
        return menu.on_close(|s: &mut State| s.menu = None).build(k, layer);
    }
    let changeable = actions::items_editable(&s.panes[p].loc);
    if n > 0 {
        menu = menu.item(item("Abrir", &icons::M_OPEN, "Enter", n == 1, actions::open));
        if actions::shows_results(&s.panes[p].loc) {
            menu = menu.item(item("Mostrar na pasta", &icons::M_SHOW_IN_FOLDER, "", n == 1, actions::show_in_folder));
        }
        menu = menu
            .separator()
            .item(item("Recortar", &icons::M_CUT, "Ctrl+X", editable, |s, p, _| actions::pick_up(s, p, ClipMode::Cut)))
            .item(item("Copiar", &icons::M_COPY, "Ctrl+C", changeable, |s, p, _| actions::pick_up(s, p, ClipMode::Copy)));
    }
    menu = menu.item(item("Colar", &icons::M_PASTE, "Ctrl+V", actions::can_paste(s, p), |s, p, _| actions::paste(s, p)));
    if n > 0 {
        menu = menu
            .separator()
            .item(item("Excluir", &icons::M_DELETE, "Del", changeable, |s, p, _| actions::ask_delete(s, p)).danger(true))
            .item(item("Renomear", &icons::M_RENAME, "F2", changeable && n == 1, actions::ask_rename));
        if changeable && matches!(s.panes[p].loc, Location::Drive { .. }) {
            let label = if actions::stars_to_add(s, p) { "Adicionar estrela" } else { "Remover estrela" };
            menu = menu.item(item(label, &icons::M_STAR, "", true, |s, p, _| actions::star(s, p)));
        }
    }
    // Sharing, for one Drive item.
    if n == 1 && editable && matches!(s.panes[p].loc, Location::Drive { .. }) {
        menu = menu
            .separator()
            .item(item("Copiar link", &icons::M_LINK, "", true, crate::sharing::copy_link))
            .item(item("Compartilhar…", &icons::M_SHARE, "", true, crate::sharing::ask_share));
    }
    menu = menu.separator().item(item("Nova pasta", &icons::M_NEW_FOLDER, "Ctrl+Shift+N", editable, actions::ask_new_folder));
    if n > 0 {
        menu = menu.separator().item(item("Propriedades", &icons::M_PROPERTIES, "Alt+Enter", true, |s, p, _| actions::properties(s, p)));
    }
    menu.on_close(|s: &mut State| s.menu = None).build(k, layer)
}

// ---------------------------------------------------------------------------------------------
// Dialogs

fn close(s: &mut State) {
    s.dialog = None;
}

/// Buttons along a dialog's bottom right: the cancel, then the action.
fn actions_row(k: &Kit, cancel: &str, act: Button) -> El {
    row()
        .key("actions")
        .w_full()
        .pt(16.0)
        .gap(8.0)
        .justify_end()
        .child(Button::new(cancel).variant(ButtonVariant::Neutral).on_press(close).build(k))
        .child(act.build(k))
}

fn dialog(s: &State, k: &Kit, layer: &mut Vec<El>) {
    let Some(d) = &s.dialog else { return };
    let pad = |e: El| col().key("content").w_full().px(16.0).pb(16.0).child(e);
    match d {
        Dialog::Delete { pane, items } => {
            let local = s.panes[*pane].loc.is_local();
            let title = match items.as_slice() {
                [one] => format!("Excluir \"{}\"?", shown(&one.name)),
                many => format!("Excluir {} itens?", ui_count(many.len())),
            };
            let where_to = if local {
                "Os itens vão para a Lixeira do Windows, de onde podem ser restaurados."
            } else {
                "Os itens vão para a lixeira do Google Drive, onde ficam 30 dias antes de serem apagados de vez."
            };
            // The explanation goes in the body, where it wraps; the dialog's own description
            // line doesn't, and would run under the close button.
            let note = text(where_to).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0);
            let body = col().w_full().gap(12.0).child(note).child(item_list(s, k, *pane, items)).child(actions_row(k, "Cancelar", Button::new("Excluir").variant(ButtonVariant::Danger).on_press(actions::delete)));
            Modal::new("delete", title).open(true).child(pad(body)).width(460.0).fit().on_close(close).build(k, layer);
        }
        Dialog::Purge { pane, items } => {
            let title = match items.as_slice() {
                [one] => format!("Excluir \"{}\" permanentemente?", shown(&one.name)),
                many => format!("Excluir {} itens permanentemente?", ui_count(many.len())),
            };
            let note = if s.panes[*pane].loc.is_local() {
                "Aqui não há Lixeira: os itens serão apagados para sempre. Isso não pode ser desfeito."
            } else {
                "Os itens serão apagados para sempre do Google Drive. Isso não pode ser desfeito."
            };
            let body = col()
                .w_full()
                .gap(12.0)
                .child(text(note).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0))
                .child(item_list(s, k, *pane, items))
                .child(actions_row(k, "Cancelar", Button::new("Excluir permanentemente").variant(ButtonVariant::Danger).on_press(actions::purge)));
            Modal::new("purge", title).open(true).child(pad(body)).width(460.0).fit().on_close(close).build(k, layer);
        }
        Dialog::EmptyTrash { email } => {
            let note = format!(
                "Todos os itens da lixeira do Meu Drive de {email} serão apagados para sempre. Isso não pode ser desfeito. Itens na lixeira de drives compartilhados continuam lá."
            );
            let body = col()
                .w_full()
                .gap(12.0)
                .child(text(note).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0))
                .child(actions_row(k, "Cancelar", Button::new("Esvaziar lixeira").variant(ButtonVariant::Danger).on_press(actions::empty_trash)));
            Modal::new("empty-trash", "Esvaziar lixeira?").open(true).child(pad(body)).width(460.0).fit().on_close(close).build(k, layer);
        }
        Dialog::NewFolder { error, .. } => {
            // Rune's own field, so it can take the dialog's first focus (`autofocus`); Enter
            // confirms.
            let field = rune::text_input(&s.name_edit, k.ty.ui_compact)
                .single_line()
                .key("name-input")
                .label("Nome")
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .color(k.c.text_base)
                .caret(k.c.text_base, k.selection())
                .on_edit(|s: &mut State| &mut s.name_edit)
                .on_submit(|s: &mut State, _: &mut Tx| actions::submit_name(s))
                .autofocus();
            let input = row()
                .key("name")
                .w_full()
                .h(32.0)
                .px(10.0)
                .center_y()
                .radius(k.radius.sm)
                .fill(k.c.bg_base)
                .border(1.0, if error.is_some() { k.c.danger.fg } else { k.c.border_base })
                .child(field);
            let mut body = col().w_full().gap(6.0).child(input);
            if let Some(e) = error {
                body = body.child(text(e.clone()).font(k.ty.text_small).color(k.c.danger.fg));
            }
            let act = Button::new("Criar").variant(ButtonVariant::Submit).on_press(actions::submit_name);
            let body = body.child(actions_row(k, "Cancelar", act));
            Modal::new("name", "Nova pasta").open(true).child(pad(body)).width(420.0).fit().on_close(close).build(k, layer);
        }
        Dialog::Properties(p) => props_dialog(s, k, p, layer),
        Dialog::Options => options_dialog(s, k, layer),
        Dialog::Clash(ask) => clash_dialog(s, k, ask, layer),
        Dialog::Share(sh) => crate::sharing::share_dialog(s, k, sh, layer),
        Dialog::MakePublic(m) => crate::sharing::public_dialog(k, m, layer),
    }
}

/// Names an upload would repeat in its Drive folder, as Air Explorer asks: all at once with
/// Aplicar a todos ticked, else one by one ("2 de 3"). Closing it cancels the upload.
fn clash_dialog(s: &State, k: &Kit, ask: &actions::ClashAsk, layer: &mut Vec<El>) {
    let n = ask.clashes.len();
    let name_of = |i: usize| ask.up.sources[i].file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    let place = crumbs(&ask.up.to).last().map(|c| c.label.clone()).unwrap_or_default();
    let all = ask.all && n > 1;
    let shown: Vec<usize> = if all { ask.clashes.clone() } else { vec![ask.current()] };
    let current = name_of(shown[0]);
    let title = match (n, all) {
        (1, _) => format!("\"{current}\" já existe"),
        (_, true) => format!("{n} itens já existem"),
        (_, false) => format!("\"{current}\" já existe ({} de {n})", ask.chosen.len() + 1),
    };
    // A folder meeting a folder is merged; the choice is for the files in it.
    let merged = |i: usize| ask.up.sources[i].is_dir() && ask.there(&name_of(i)).is_some_and(|e| e.is_dir && e.target.is_none() && e.mime.as_deref().is_none_or(|m| m == openexplorer_gdrive::api::FOLDER_MIME));
    let muted = |t: String| text(t).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0);
    let lead = if shown.len() == 1 { format!("Já existe um item com esse nome em \"{place}\".") } else { format!("Já existem itens com estes nomes em \"{place}\".") };
    let mut list = col().key("items").w_full().gap(4.0);
    for &i in shown.iter().take(6) {
        let name = name_of(i);
        let glyph = ask.there(&name).map_or_else(|| el().w(16.0), |e| entry_icon(s, &ask.up.to, e, 16.0));
        let mut line = row().key(format!("item-{i}")).w_full().gap(6.0).center_y().child(glyph).child(text(name).font(k.ty.text_small).color(k.c.text_base).truncate().shrink().min_w(0.0));
        if merged(i) {
            line = line.child(el().grow(1.0)).child(text("pasta: juntar conteúdo").font(k.ty.text_xsmall).color(k.c.text_muted));
        }
        list = list.child(line);
    }
    if shown.len() > 6 {
        list = list.child(text(format!("e mais {}", ui_count(shown.len() - 6))).font(k.ty.text_small).color(k.c.text_muted));
    }
    let mut body = col().w_full().gap(12.0).child(muted(lead)).child(list);
    if shown.iter().any(|&i| merged(i)) {
        body = body.child(muted("Uma pasta que já existe recebe o conteúdo enviado; a escolha vale para os arquivos dentro dela que já existirem.".into()));
    }
    body = body.child(muted("Substituir envia o arquivo como nova versão do que está no Drive (a anterior fica no histórico de versões). Manter os dois envia com outro nome, como \"nome (2).ext\".".into()));
    if n > 1 {
        body = body.child(rune_kit::Checkbox::new(ask.all).label("Aplicar a todos").on_toggle(actions::toggle_clash_all).build(k));
    }
    let choice = |label: &str, c: crate::uploads::Clash, variant: ButtonVariant| Button::new(label).variant(variant).on_press(move |s: &mut State| actions::answer_clash(s, Some(c))).build(k);
    let buttons = row()
        .key("actions")
        .w_full()
        .pt(4.0)
        .gap(8.0)
        .justify_end()
        .child(Button::new("Cancelar").variant(ButtonVariant::Neutral).on_press(|s: &mut State| actions::answer_clash(s, None)).build(k))
        .child(el().grow(1.0))
        .child(choice("Pular", crate::uploads::Clash::Skip, ButtonVariant::Neutral))
        .child(choice("Manter os dois", crate::uploads::Clash::KeepBoth, ButtonVariant::Neutral))
        .child(choice("Substituir", crate::uploads::Clash::Replace, ButtonVariant::Submit));
    let body = col().key("content").w_full().px(16.0).pb(16.0).child(body.child(buttons));
    Modal::new("clash", title).open(true).child(body).width(520.0).fit().on_close(|s: &mut State| actions::answer_clash(s, None)).build(k, layer);
}

/// The first few items a dialog acts on, with "e mais N" for the rest.
fn item_list(s: &State, k: &Kit, pane: usize, items: &[crate::model::Entry]) -> El {
    let mut list = col().key("items").w_full().gap(4.0);
    for e in items.iter().take(6) {
        list = list.child(
            row().gap(6.0).center_y().child(entry_icon(s, &s.panes[pane].loc, e, 16.0)).child(text(shown(&e.name)).font(k.ty.text_small).color(k.c.text_base).truncate().shrink().min_w(0.0)),
        );
    }
    if items.len() > 6 {
        list = list.child(text(format!("e mais {}", ui_count(items.len() - 6))).font(k.ty.text_small).color(k.c.text_muted));
    }
    list
}

/// The colours and font of the in-place rename field, copied out of the kit so the row builder
/// can hold them.
#[derive(Clone, Copy)]
struct RenameLook {
    font: rune::TextStyle,
    ink: Color,
    fill: Color,
    edge: Color,
    selection: Color,
}

/// The name field of an item renamed in place. It's a light modal: focus stays in it, a click
/// anywhere else renames (as leaving the field does in Explorer), Enter renames, Esc cancels.
fn rename_field(s: &State, look: RenameLook) -> El {
    let field = rune::text_input(&s.name_edit, look.font)
        .single_line()
        .key("rename-input")
        .label("Novo nome")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .color(look.ink)
        .caret(look.ink, look.selection)
        .on_edit(|s: &mut State| &mut s.name_edit)
        .on_submit(|s: &mut State, _: &mut Tx| actions::commit_rename(s))
        // A field's own key hook runs before Esc dismisses the modal around it.
        .on_input_key(|s: &mut State, ev: &KeyEvent, _: &mut Tx| {
            if ev.key == rune::Key::Escape {
                actions::cancel_rename(s);
                true
            } else {
                false
            }
        })
        .autofocus();
    row()
        .key("rename")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h(20.0)
        .px(3.0)
        .center_y()
        .fill(look.fill)
        .border(1.0, look.edge)
        .child(field)
        // A click outside dismisses the modal: that renames. (Esc never gets here: the field
        // cancels first.)
        .modal(true, actions::commit_rename)
}

fn ui_count(n: usize) -> String {
    thousands(n)
}

// ---------------------------------------------------------------------------------------------
// Properties

/// A label and its value, the value selectable so it can be copied.
fn prop_row(k: &Kit, label: &str, value: El) -> El {
    row()
        .w_full()
        .gap(12.0)
        .items_start()
        .child(text(label.to_string()).font(k.ty.text_small).color(k.c.text_muted).w(120.0))
        .child(row().w(0.0).grow(1.0).min_w(0.0).child(value))
}

fn prop_text(k: &Kit, label: &str, value: impl Into<String>) -> El {
    prop_row(k, label, text(value.into()).font(k.ty.text_small).color(k.c.text_base).selectable(true).wrap_anywhere().w_full().min_w(0.0))
}

fn prop_sep(k: &Kit) -> El {
    el().w_full().h(1.0).my(5.0).fill(k.c.border_weak)
}

/// The Propriedades tabs, Geral first.
fn prop_tab(k: &Kit, label: &str, tab: actions::PropTab, on: bool) -> El {
    row()
        .key(format!("tab-{label}"))
        .role("tab")
        .label(label.to_string())
        .checked(on)
        .h(30.0)
        .px(12.0)
        .center_y()
        .cursor(Cursor::Pointer)
        .hover(k.c.hover)
        .border_sides([0.0, 0.0, if on { 2.0 } else { 0.0 }, 0.0], k.c.text_accent)
        .on_press(move |s: &mut State| actions::props_tab(s, tab))
        .child(text(label.to_string()).font(if on { k.ty.text_small_medium } else { k.ty.text_small }).color(if on { k.c.text_base } else { k.c.text_muted }))
}

/// Where an item is, in words: the folder's full path on disk, or its place in Drive.
fn place_of(s: &State, p: &actions::Props) -> String {
    let pane = &s.panes[p.pane];
    match (&pane.loc, p.single()) {
        (loc @ (Location::Local(_) | Location::Found(_)), Some(e)) => loc.local_path(e).map(|p| p.display().to_string()).unwrap_or_default(),
        (Location::Local(dir), None) => dir.display().to_string(),
        (Location::Found(f), None) => format!("Resultados de “{}” em {}", f.text, f.place),
        (loc, single) => {
            let mut parts: Vec<String> = crumbs(loc).into_iter().skip(1).map(|c| c.label).collect();
            if let Some(e) = single {
                parts.push(shown(&e.name));
            }
            parts.join("/")
        }
    }
}

fn props_dialog(s: &State, k: &Kit, p: &actions::Props, layer: &mut Vec<El>) {
    use actions::PropTab;
    let loc = &s.panes[p.pane].loc;
    let drive = match loc {
        Location::Drive { email, .. } => Some(email.clone()),
        _ => None,
    };
    let details = match &p.details {
        Some(Ok(d)) => Some(d),
        _ => None,
    };
    let when = |t: Option<std::time::SystemTime>| t.map(format_long).unwrap_or_else(|| "—".into());
    let muted = |t: &str| text(t.to_string()).font(k.ty.text_small).color(k.c.text_muted);

    // Header: the item's icon and its name, which renames on OK.
    let loc = &s.panes[p.pane].loc;
    let (glyph, title) = match p.single() {
        Some(e) => (entry_icon(s, loc, e, 40.0), shown(&e.name)),
        None if p.items.iter().all(|e| e.is_dir) => (shell_icon(s, Some(&IconKey::Folder), &icons::FOLDER, 40.0), format!("{} itens", ui_count(p.items.len()))),
        None => (icon(&icons::FILE, 40.0), format!("{} itens", ui_count(p.items.len()))),
    };
    let editable_name = p.renamable(s);
    let name: El = if editable_name {
        rune::text_input(&s.name_edit, k.ty.text_small)
            .single_line()
            .key("prop-name")
            .label("Nome")
            .w_full()
            .min_w(0.0)
            .color(k.c.text_base)
            .caret(k.c.text_base, k.selection())
            .on_edit(|s: &mut State| &mut s.name_edit)
            .on_submit(|s: &mut State, _: &mut Tx| actions::props_ok(s))
    } else {
        text(title.clone()).font(k.ty.text_small_medium).color(k.c.text_base).selectable(true).truncate().min_w(0.0)
    };
    // An editable name sits in a field; one that can't change (several items, a shared drive)
    // is plain text, so it doesn't look like it takes typing.
    let name_box = row().w(0.0).grow(1.0).min_w(0.0).h(30.0).center_y();
    let name_box = if editable_name { name_box.px(8.0).radius(k.radius.sm).fill(k.c.bg_base).border(1.0, k.c.border_base) } else { name_box };
    let header = row()
        .key("prop-head")
        .w_full()
        .gap(14.0)
        .center_y()
        .child(glyph)
        .child(name_box.child(name));

    let mut tabs = row().key("prop-tabs").w_full().border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak).child(prop_tab(k, "Geral", PropTab::General, p.tab == PropTab::General)).child(prop_tab(
        k,
        "Detalhes",
        PropTab::Details,
        p.tab == PropTab::Details,
    ));
    if p.has_versions(s) {
        tabs = tabs.child(prop_tab(k, "Histórico de versões", PropTab::Versions, p.tab == PropTab::Versions));
    }

    // A floor on the height, so switching tabs doesn't make the dialog jump.
    let mut body = col().key("prop-body").w_full().min_h(380.0).pt(14.0).gap(7.0);
    match p.tab {
        PropTab::General => {
            body = body.child(header).child(prop_sep(k));
            // What it is and where it lives.
            body = body.child(prop_text(k, "Tipo", p.kind.clone()));
            if let Some(desc) = details.and_then(|d| d.description.clone()) {
                body = body.child(prop_text(k, "Descrição", desc));
            }
            body = body.child(prop_text(k, "Local", if drive.is_some() { "Google Drive" } else { "Este computador" }));
            if let Some(email) = &drive {
                body = body.child(prop_text(k, "Conta", email.clone()));
            }
            body = body.child(prop_sep(k));
            // Where and how big.
            body = body.child(prop_text(k, "Caminho", place_of(s, p)));
            let size = match (p.single(), &p.totals) {
                (_, Some(t)) => format_bytes(t.bytes),
                (Some(e), None) if !e.is_dir => e.size.map(format_bytes).unwrap_or_else(|| "—".into()),
                (_, None) if loc.is_local() => "Calculando…".into(),
                (None, None) => format_bytes(p.listed_bytes),
                _ => "—".into(),
            };
            body = body.child(prop_text(k, "Tamanho", size));
            if let Some(t) = &p.totals
                && (p.items.len() > 1 || p.items.iter().any(|e| e.is_dir))
            {
                let plural = |n: u64, one: &str, many: &str| format!("{} {}", thousands(n as usize), if n == 1 { one } else { many });
                body = body.child(prop_text(k, "Contém", format!("{}, {}", plural(t.files, "arquivo", "arquivos"), plural(t.dirs, "pasta", "pastas"))));
            }
            if let Some(e) = p.single() {
                body = body.child(prop_sep(k));
                // When, and the content's fingerprint.
                let created = p.local.as_ref().and_then(|l| l.created).or_else(|| details.and_then(|d| d.created));
                body = body.child(prop_text(k, "Criado", when(created))).child(prop_text(k, "Modificado", when(e.modified)));
                if let Some(l) = &p.local {
                    body = body.child(prop_text(k, "Acessado", when(l.accessed)));
                } else if let Some(v) = details.and_then(|d| d.viewed) {
                    body = body.child(prop_text(k, "Aberto por você", format_long(v)));
                }
                if !e.is_dir {
                    let mono = |h: &String| text(h.clone()).font(k.ty.mono).color(k.c.text_base).selectable(true);
                    // Drive keeps each file's MD5 and sends it with the listing: nothing to work
                    // out. A local file's is read from disk on request.
                    let drive_md5 = e.md5.as_ref().or_else(|| details.and_then(|d| d.md5.as_ref()));
                    let google_format = e.mime.as_deref().is_some_and(|m| m.starts_with("application/vnd.google-apps."));
                    let hash: El = match (&p.local, &p.md5) {
                        (_, Some(Ok(h))) => mono(h),
                        (_, Some(Err(err))) => text(err.clone()).font(k.ty.text_small).color(k.c.danger.fg),
                        (Some(_), None) if p.md5_running => muted("Calculando…"),
                        (Some(_), None) => Button::new("Calcular").variant(ButtonVariant::Neutral).on_press(actions::props_md5).build(k).key("md5"),
                        (None, None) => match drive_md5 {
                            Some(h) => mono(h),
                            None if google_format => muted("— (formato do Google, sem arquivo binário)"),
                            None if details.is_none() && p.details.is_none() && matches!(s.panes[p.pane].loc, Location::Drive { .. }) => muted("Carregando…"),
                            None => muted("—"),
                        },
                    };
                    body = body.child(prop_row(k, "Hash (MD5)", hash));
                }
            }
        }
        PropTab::Details => {
            match (p.single(), &p.local, drive.is_some()) {
                (Some(e), Some(l), _) => {
                    let attrs = if l.attributes.is_empty() { "Nenhum".to_string() } else { l.attributes.join(", ") };
                    body = body.child(prop_text(k, "Atributos", attrs));
                    let ext = e.name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty() && !e.is_dir).map(|(_, x)| format!(".{x}")).unwrap_or_else(|| "—".into());
                    body = body.child(prop_text(k, "Extensão", ext)).child(prop_text(k, "Caminho completo", l.path.display().to_string()));
                }
                (Some(e), None, true) => {
                    body = body.child(prop_text(k, "ID", e.id.clone()));
                    match &p.details {
                        Some(Ok(d)) => {
                            let or_dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "—".into());
                            body = body
                                .child(prop_text(k, "Tipo MIME", or_dash(&d.mime)))
                                .child(prop_text(k, "Proprietário", or_dash(&d.owner)))
                                .child(prop_text(k, "Modificado por", or_dash(&d.modified_by)))
                                .child(prop_text(k, "Compartilhado", if d.shared { "Sim" } else { "Não" }));
                            let me = drive.as_deref().unwrap_or_default();
                            body = body.child(match &p.sharing {
                                Some(Ok(sh)) => prop_text(k, "Compartilhamento", sh.summary(me)),
                                Some(Err(err)) => prop_row(k, "Compartilhamento", muted(err)),
                                None => prop_row(k, "Compartilhamento", muted("Carregando…")),
                            });
                            if let Some(by) = &d.shared_by {
                                body = body.child(prop_text(k, "Compartilhado por", by.clone()));
                            }
                            body = body.child(prop_text(k, "Com estrela", if d.starred { "Sim" } else { "Não" }));
                            if let Some(url) = d.link.clone() {
                                body = body.child(prop_sep(k)).child(
                                    row().w_full().justify_end().child(
                                        Button::new("Abrir no Google Drive")
                                            .variant(ButtonVariant::Neutral)
                                            .on_press_tx(move |s: &mut State, tx: &mut Tx| {
                                                if let Err(e) = tx.open_url(&url) {
                                                    s.status = format!("Não consegui abrir o navegador: {e:?}");
                                                }
                                            })
                                            .build(k),
                                    ),
                                );
                            }
                        }
                        Some(Err(err)) => body = body.child(prop_text(k, "Detalhes", err.clone())),
                        None => body = body.child(prop_row(k, "Detalhes", muted("Carregando…"))),
                    }
                }
                _ => {
                    // Several items: their names.
                    let mut names = col().w_full().gap(4.0);
                    for e in p.items.iter().take(20) {
                        names = names.child(row().gap(6.0).center_y().child(entry_icon(s, loc, e, 16.0)).child(text(shown(&e.name)).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0)));
                    }
                    if p.items.len() > 20 {
                        names = names.child(muted(&format!("e mais {}", ui_count(p.items.len() - 20))));
                    }
                    body = body.child(prop_row(k, "Itens", names));
                }
            }
        }
        PropTab::Versions => match &p.revisions {
            None => body = body.child(muted("Carregando…")),
            Some(Err(err)) => body = body.child(text(err.clone()).font(k.ty.text_small).color(k.c.danger.fg).wrap().w_full()),
            Some(Ok(list)) if list.is_empty() => body = body.child(muted("Nenhuma versão guardada.")),
            Some(Ok(list)) => {
                let head = |t: &str, w: f32| text(t.to_string()).font(k.ty.text_xsmall).color(k.c.text_muted).w(w);
                body = body.child(row().w_full().pb(6.0).border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak).child(head("Data", 150.0)).child(head("Tamanho", 90.0)).child(head("Por", 150.0)));
                let mut rows = col().w_full();
                // Newest first, as the Drive website lists them.
                for r in list.iter().rev() {
                    let date = r.modified.map(format_time).unwrap_or_else(|| "—".into());
                    let mut by = r.by.clone().unwrap_or_else(|| "—".into());
                    if r.keep_forever {
                        by.push_str(" · mantida para sempre");
                    }
                    rows = rows.child(
                        row()
                            .w_full()
                            .h(26.0)
                            .center_y()
                            .child(text(date).font(k.ty.text_small).color(k.c.text_base).w(150.0))
                            .child(text(r.size.map(format_size).unwrap_or_else(|| "—".into())).font(k.ty.text_small).color(k.c.text_muted).w(90.0))
                            .child(text(by).font(k.ty.text_small).color(k.c.text_muted).truncate().w(0.0).grow(1.0).min_w(0.0)),
                    );
                }
                body = body.child(col().key("versions").w_full().max_h(260.0).scroll(ScrollSpec { stick_bottom: false }).child(rows));
            }
        },
    }

    let buttons = row()
        .key("actions")
        .w_full()
        .pt(18.0)
        .gap(8.0)
        .justify_end()
        .child(Button::new("OK").variant(ButtonVariant::Submit).on_press(actions::props_ok).build(k))
        .child(Button::new("Cancelar").variant(ButtonVariant::Neutral).on_press(actions::close_properties).build(k));
    let content = col().key("content").w_full().px(16.0).pb(16.0).child(tabs).child(body).child(buttons);
    Modal::new("props", format!("Propriedades: {title}")).open(true).child(content).width(520.0).fit().on_close(actions::close_properties).build(k, layer);
}

/// Windows' icon for `key` once it has loaded, else the app's own `fallback`.
fn shell_icon(s: &State, key: Option<&IconKey>, fallback: &'static Icon, size: f32) -> El {
    match key.and_then(|k| s.icons.get(k)).filter(|i| i.is_ready() && i.error().is_none()) {
        Some(image) => el().size(size, size).pixels(image.pixels()),
        None => icon(fallback, size),
    }
}

/// The icon Explorer would show for `e`: its type's program's (VLC's for a video VLC opens).
/// A Drive shortcut to a folder keeps the app's linked-folder icon.
fn entry_icon(s: &State, loc: &Location, e: &crate::model::Entry, size: f32) -> El {
    let kind = e.kind();
    let fallback = kind_icon(kind);
    if kind == Kind::FolderLink {
        return icon(fallback, size);
    }
    shell_icon(s, sysicons::key_for(e, loc).as_ref(), fallback, size)
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
        .child(tool(k, "options", &icons::SETTINGS, "Opções", true, |s: &mut State, _| s.dialog = Some(Dialog::Options)))
        .child(text("Open Explorer").font(k.ty.text_small_medium).color(k.c.text_faint).mx(8.0))
}

// ---------------------------------------------------------------------------------------------
// Splitters

/// The drag that resizes something to `size`: Down keeps the starting size, each move sets it
/// to the start plus the travel (through `clamp`), a cancelled drag puts it back, and a double
/// click sets `default`. Returns whether the layout changed and should be saved: on a double
/// click, and on release when the size ended up different.
/// `step` is how far the pointer moved along the size: `d.dx` for a width, `-d.dy` for a
/// height grown upwards.
fn resize(d: &mut Drag, step: f32, size: &mut f32, default: f32, clamp: impl Fn(f32) -> f32) -> bool {
    match d.phase {
        Phase::Down if d.clicks >= 2 => {
            // The release of this click resizes from the default, so it doesn't undo it.
            *size = default;
            d.data = default;
            true
        }
        Phase::Down => {
            d.data = *size;
            false
        }
        _ if d.cancelled() => {
            *size = d.data;
            false
        }
        Phase::Move => {
            *size = clamp(d.data + step);
            false
        }
        Phase::Up => {
            *size = clamp(d.data + step);
            *size != d.data
        }
    }
}

/// Which way a [`splitter`] drags.
#[derive(Clone, Copy)]
enum Split {
    /// A vertical bar dragged sideways (between a pane's tree and its list): a width.
    Right,
    /// A horizontal strip dragged up (over the transfer queue): a height that grows upwards.
    Up,
}

/// A bar `thick` across to drag, showing a line on hover and while dragged. `size` picks the
/// width or height it changes in the layout; a double click puts it back to `default`.
fn splitter(k: &Kit, key: &str, thick: f32, way: Split, size: impl Fn(&mut State) -> &mut f32 + 'static, default: f32, clamp: fn(f32) -> f32) -> El {
    let line = k.c.border_focus;
    let bar = match way {
        Split::Right => row().w(thick).h_full().cursor(Cursor::ResizeColumn),
        Split::Up => row().w_full().h(thick).cursor(Cursor::ResizeRow),
    };
    bar.key(key.to_string())
        .role("separator")
        .label("Arraste para redimensionar; clique duas vezes para voltar ao padrão")
        .justify_center()
        .hover(k.alpha(line, 0.18))
        .press(k.alpha(line, 0.32))
        .on_drag(move |s: &mut State, d: &mut Drag, _tx: &mut Tx| {
            let step = match way {
                Split::Right => d.dx,
                Split::Up => -d.dy,
            };
            if resize(d, step, size(s), default, clamp) {
                s.layout.save();
            }
        })
}

/// The two panes, side by side in equal halves.
fn panes(s: &State, k: &Kit) -> El {
    row()
        .key("panes")
        .w_full()
        .grow(1.0)
        .min_h(160.0)
        .padding(6.0)
        .child(pane(s, k, 0))
        .child(gutter(s, k))
        .child(pane(s, k, 1))
}

/// The strip between the panes, as in Air Explorer: the window's own background, empty but
/// for two small triangles near the top that copy the selection of one pane into the folder
/// the other shows. A triangle is blue while its pane has something selected, grey otherwise.
fn gutter(s: &State, k: &Kit) -> El {
    let arrow = |key: &str, glyph: &'static Icon, help: &str, from: usize, to: usize| {
        let ready = !s.panes[from].selected.is_empty();
        let ink = if ready { k.alpha(k.c.text_accent, 0.7) } else { k.alpha(k.c.icon_faint, 0.6) };
        let b = el().key(key.to_string()).role("button").label(help.to_string()).help(help.to_string()).size(28.0, 28.0).center().radius(k.radius.sm);
        let b = if ready { b.hover(k.c.hover).press(k.c.pressed).cursor(Cursor::Pointer) } else { b };
        b.on_press(move |s: &mut State| actions::copy_across(s, from, to)).child(icon(glyph, 16.0).color(ink))
    };
    col()
        .key("gutter")
        .w(40.0)
        .h_full()
        .items_center()
        .pt(96.0)
        .gap(6.0)
        .child(arrow("copy-right", &icons::COPY_RIGHT, "Copiar a seleção do painel esquerdo para o direito", 0, 1))
        .child(arrow("copy-left", &icons::COPY_LEFT, "Copiar a seleção do painel direito para o esquerdo", 1, 0))
}

// ---------------------------------------------------------------------------------------------
// Pane

fn pane(s: &State, k: &Kit, p: usize) -> El {
    let pane = &s.panes[p];
    let border = if pane.drop_over {
        k.c.text_accent
    } else if s.active == p {
        // Just a hint of the accent: the active pane shouldn't shout.
        k.alpha(k.c.border_focus, 0.3)
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
        .child(search_bar(s, k, p))
        .child(
            row()
                .key("body")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(tree(s, k, p))
                .child(
                    splitter(k, "tree-split", 5.0, Split::Right, move |s: &mut State| &mut s.layout.tree[p], layout::TREE_W, clamp_tree)
                    .child(el().w(1.0).h_full().fill(k.c.border_weak)),
                )
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

/// The path bar: each step of the path a link back to it, or, while editing, a field with the
/// address (see [`actions::edit_address`]). Its empty part edits on a click; a right click
/// offers copying and pasting the address (not while editing: the menu would take the field's
/// focus, and with it what was typed).
fn path_bar(s: &State, k: &Kit, p: usize) -> El {
    let bar = row().grow(1.0).min_w(0.0).h(26.0).px(4.0).center_y().radius(k.radius.sm).border(1.0, k.c.border_weak);
    if let Some(edit) = &s.address[p] {
        let field = rune::text_input(edit, k.ty.text_small)
            .single_line()
            .key(address_key(p))
            .label("Endereço")
            .grow(1.0)
            .shrink()
            .min_w(0.0)
            .px(3.0)
            .color(k.c.text_base)
            .caret(k.c.text_base, k.selection())
            // Only reached while the field shows; one closed meanwhile comes back empty and
            // unfocused, which `poll` closes again.
            .on_edit(move |s: &mut State| s.address[p].get_or_insert_with(|| rune::TextEdit::new("")))
            .on_submit(move |s: &mut State, tx: &mut Tx| {
                let typed = s.address[p].as_ref().map(|a| a.text.clone()).unwrap_or_default();
                if actions::go_to_address(s, p, &typed) {
                    tx.focus(pane_key(p), false);
                }
            })
            .on_input_key(move |s: &mut State, ev: &KeyEvent, tx: &mut Tx| {
                if ev.key != rune::Key::Escape {
                    return false;
                }
                s.address[p] = None;
                tx.focus(pane_key(p), false);
                true
            });
        return bar.key("address").border(1.0, k.c.border_strong).child(field);
    }
    let mut path = bar.key("crumbs").clip_soft(8.0).on_secondary_press(move |s: &mut State, at: rune::PointerPress, _: &mut Tx| s.address_menu = Some((p, at)));
    let steps = crumbs(&s.panes[p].loc);
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
    // The bar's empty part, after the last step: a click there edits the address.
    path.child(el().key("address-edit").grow(1.0).h_full().min_w(16.0).cursor(Cursor::Text).on_press(move |s: &mut State| actions::edit_address(s, p)))
}

pub(crate) fn address_key(p: usize) -> String {
    format!("address-input-{p}")
}

fn navbar(s: &State, k: &Kit, p: usize) -> El {
    let pane = &s.panes[p];
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
        .child(path_bar(s, k, p).mx(4.0))
        .child(search_box(s, k, p))
        .child(nav_button(k, "reload", &icons::REFRESH, "Atualizar (Ctrl+R)", true, move |s: &mut State| load(s, p)))
}

/// The search field of a Drive pane: Enter searches where the pane is (see
/// [`actions::search`]), Esc empties it; Ctrl+F reaches it from the list. Nothing for the
/// computer's panes yet.
fn search_box(s: &State, k: &Kit, p: usize) -> El {
    let loc = &s.panes[p].loc;
    let place = match loc {
        Location::Local(dir) => crate::finder::place_of(dir),
        Location::Found(f) => f.place.clone(),
        _ => match actions::search_scope(loc) {
            Some((_, place)) => place,
            None => return el().key("search"),
        },
    };
    let field = rune::text_input(&s.search[p], k.ty.text_small)
        .single_line()
        .key(search_key(p))
        .label("Pesquisar")
        .placeholder(format!("Pesquisar em {place}"), k.c.text_faint)
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .color(k.c.text_base)
        .caret(k.c.text_base, k.selection())
        .on_edit(move |s: &mut State| &mut s.search[p])
        .on_submit(move |s: &mut State, _: &mut Tx| actions::search(s, p))
        .on_input_key(move |s: &mut State, ev: &KeyEvent, _: &mut Tx| {
            if ev.key == rune::Key::Escape && !s.search[p].text.is_empty() {
                s.search[p] = rune::TextEdit::new("");
                true
            } else {
                false
            }
        });
    row()
        .key("search")
        .w(210.0)
        .h(26.0)
        .px(7.0)
        .gap(5.0)
        .mr(4.0)
        .center_y()
        .radius(k.radius.sm)
        .border(1.0, k.c.border_weak)
        .child(icon(&icons::SEARCH, 14.0).color(k.c.icon_faint))
        .child(field)
}

pub(crate) fn search_key(p: usize) -> String {
    format!("search-input-{p}")
}

/// Under the path bar of a search of the computer: what was searched where, how many were found,
/// whether the index is still reading a disk, and ways to widen the search or install the
/// service it needs.
fn found_bar(s: &State, k: &Kit, p: usize, f: &crate::model::Found) -> El {
    use crate::finder::Service;
    let pane = &s.panes[p];
    let info = s.found[p].as_ref();
    let mut said = format!("“{}” em {}", f.text, f.place);
    let n = pane.entries.len();
    let results = |n: usize| format!("{} {}", ui_count(n), if n == 1 { "resultado" } else { "resultados" });
    match info {
        // In WSL or on the network (no index): results come in as folders are read.
        None if pane.loading && n > 0 => said.push_str(&format!(" · Pesquisando… {} até agora", results(n))),
        None if !pane.loading && pane.error.is_none() => said.push_str(&format!(" · {}", results(n))),
        _ if pane.loading => said.push_str(" · Pesquisando…"),
        Some(i) if i.service != Service::Running => said.push_str(" · sem o serviço"),
        Some(i) if i.total > i.shown => said.push_str(&format!(" · mostrando {} de {} resultados", ui_count(i.shown), ui_count(i.total))),
        Some(i) => said.push_str(&format!(" · {} {}", ui_count(i.total), if i.total == 1 { "resultado" } else { "resultados" })),
        None => {}
    }
    if let Some(i) = info
        && !i.indexing.is_empty()
    {
        said.push_str(&format!(" · lendo {} (resultados incompletos)", i.indexing.join(", ")));
    }
    let link = |key: &'static str, label: String, act: fn(&mut State, usize)| {
        text(label).key(key).role("link").font(k.ty.text_xsmall).color(k.c.text_accent).cursor(Cursor::Pointer).on_press(move |s: &mut State| act(s, p))
    };
    let mut bar = row()
        .key("search-bar")
        .w_full()
        .h(28.0)
        .px(10.0)
        .gap(10.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(text(said).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().min_w(0.0))
        .child(el().grow(1.0));
    if info.is_some_and(|i| i.service != Service::Running) {
        let label = if s.installing_finder { "Instalando…" } else { "Instalar o serviço de pesquisa" };
        bar = bar.child(link("install-finder", label.into(), |s, _| actions::install_finder(s)));
    }
    if f.within.is_some() {
        bar = bar.child(link("search-everywhere", "Pesquisar no computador todo".into(), actions::search_everywhere));
    }
    bar
}

/// Under the path bar of a search's results: what was searched where, and a way to widen it.
fn search_bar(s: &State, k: &Kit, p: usize) -> El {
    if let Location::Found(f) = &s.panes[p].loc {
        return found_bar(s, k, p, f);
    }
    let Location::Drive { space: Space::Search(q), path, .. } = &s.panes[p].loc else { return el().key("search-bar") };
    if !path.is_empty() {
        return el().key("search-bar");
    }
    let pane = &s.panes[p];
    let found = if pane.loading { "Pesquisando…".to_string() } else { format!("{} {}", ui_count(pane.entries.len()), if pane.entries.len() == 1 { "resultado" } else { "resultados" }) };
    let mut bar = row()
        .key("search-bar")
        .w_full()
        .h(28.0)
        .px(10.0)
        .gap(10.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(text(format!("“{}” em {} · {found}", q.text, q.place)).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().min_w(0.0));
    if q.within != openexplorer_gdrive::search::Within::Everything {
        bar = bar.child(el().grow(1.0)).child(
            text("Pesquisar em todo o Drive")
                .key("search-everywhere")
                .role("link")
                .font(k.ty.text_xsmall)
                .color(k.c.text_accent)
                .cursor(Cursor::Pointer)
                .on_press(move |s: &mut State| actions::search_everywhere(s, p)),
        );
    }
    bar
}

fn can_go_up(loc: &Location) -> bool {
    match loc {
        Location::Local(dir) => crate::network::parent(dir).is_some(),
        Location::Found(_) => false,
        Location::Mock(segs) => !segs.is_empty(),
        Location::Drive { space, path, .. } => !path.is_empty() || matches!(space, Space::Shared { .. }),
    }
}

// ---------------------------------------------------------------------------------------------
// Location tree

/// A row of a pane's tree: a heading, a note under a node being read, or a node with its arrow
/// (which opens and closes it), icon and name (a click opens the folder; a double click opens
/// or closes the node, as in Explorer).
fn tree_line(s: &State, k: &Kit, p: usize, line: &crate::tree::Line) -> El {
    use crate::tree::Line;
    let indent = |depth: usize| 4.0 + depth as f32 * 16.0;
    let (depth, node, open, empty, on) = match line {
        Line::Heading(label) => return heading(k, label).key(format!("t:{}", line.key())),
        Line::Note { depth, text: note, .. } => {
            return row()
                .key(format!("t:{}", line.key()))
                .w_full()
                .h(ROW_H)
                .pl(indent(*depth) + 22.0)
                .center_y()
                .child(text(note.clone()).font(k.ty.text_xsmall).color(k.c.text_faint).truncate().shrink().min_w(0.0).help_when_truncated(note.clone()));
        }
        Line::Node { depth, node, open, empty, on, .. } => (*depth, node, *open, *empty, *on),
    };
    let arrow = if node.opens && !empty {
        let n = node.clone();
        row()
            .key("arrow")
            .size(16.0, 16.0)
            .center()
            .radius(k.radius.sm)
            .hover(k.alpha(k.c.text_base, 0.08))
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| crate::tree::toggle(s, p, &n))
            .child(icon(if open { &icons::CHEVRON_DOWN } else { &icons::CHEVRON }, 12.0).color(k.c.icon_faint))
    } else {
        el().size(16.0, 16.0)
    };
    let (go, flip) = (node.clone(), node.clone());
    row()
        .key(format!("t:{}", line.key()))
        .role("treeitem")
        .label(node.label.clone())
        .checked(on)
        .w_full()
        .h(ROW_H)
        .pl(indent(depth))
        .pr(6.0)
        .gap(4.0)
        .center_y()
        .fill(if on { k.alpha(k.c.text_accent, if k.dark { 0.24 } else { 0.16 }) } else { Color::TRANSPARENT })
        .hover(if on { k.alpha(k.c.text_accent, 0.24) } else { k.alpha(k.c.text_base, 0.05) })
        .cursor(Cursor::Pointer)
        .on_press(move |s: &mut State| match &go.to {
            Some(to) => navigate(s, p, to.clone()),
            None => crate::tree::toggle(s, p, &go),
        })
        .on_double_press(move |s: &mut State| crate::tree::toggle(s, p, &flip))
        .child(arrow)
        .child(row().gap(6.0).center_y().grow(1.0).min_w(0.0).child(shell_icon(s, node.icon.as_ref(), node.fallback, 16.0)).child(
            text(node.label.clone()).font(k.ty.text_small).color(k.c.text_base).truncate().shrink().min_w(0.0).help_when_truncated(node.label.clone()),
        ))
}

fn heading(k: &Kit, label: &str) -> El {
    row().w_full().h(24.0).px(10.0).pt(6.0).center_y().child(text(label.to_string()).font(k.ty.text_xsmall).color(k.c.text_faint))
}

/// The scroll view of pane `p`'s tree, which `poll` scrolls to the folder shown.
pub(crate) fn tree_key(p: usize) -> String {
    format!("tree-scroll-{p}")
}

/// The tree beside pane `p` (see [`crate::tree`]): only the rows in view are built, so a node
/// with thousands of folders opens at once.
fn tree(s: &State, k: &Kit, p: usize) -> El {
    let lines = std::rc::Rc::new(crate::tree::lines(s, p));
    let (keys, kit, n) = (lines.clone(), k.clone(), lines.len());
    let bar = bar_style(k);
    let rows = col()
        .key("tree")
        .w_full()
        .pb(8.0)
        .pr(bar.strip())
        .growth(GrowthSpec::default())
        .virtual_rows(n, ROW_H, move |_: &State, i| Cow::Owned(format!("t:{}", keys[i].key())), move |s: &State, i| tree_line(s, &kit, p, &lines[i]))
        .virtual_rows_version((s.tree.version, s.panes[p].moves, s.accounts.len()));
    // Opening or closing a node lands at once, as in Explorer, rather than sliding its rows.
    col()
        .key(tree_key(p))
        .instant_changes()
        .w(s.layout.tree[p])
        .h_full()
        .fill(k.alpha(k.c.text_base, if k.dark { 0.02 } else { 0.015 }))
        .scroll(ScrollSpec { stick_bottom: false })
        .child(rows)
        .scroll_bar_styled(bar)
}
// ---------------------------------------------------------------------------------------------
// File list

/// The grip on a column's right edge, as in Windows Explorer: it moves with the pointer,
/// resizing that column and carrying the columns after it along. A double click puts the column
/// back to its default width.
fn col_grip(k: &Kit, p: usize, i: usize, edge: f32) -> El {
    grip(k, &format!("grip-{i}"), edge, 24.0, move |s| &mut s.layout.columns[p][i], layout::COLS[i], move |w| clamp_col(i, w))
}

/// A column grip at `edge` over a header `height` tall, changing the width `size` picks.
fn grip(k: &Kit, key: &str, edge: f32, height: f32, size: impl Fn(&mut State) -> &mut f32 + 'static, default: f32, clamp: impl Fn(f32) -> f32 + 'static) -> El {
    el().key(key.to_string())
        .role("separator")
        .label("Arraste para mudar a largura da coluna; clique duas vezes para voltar ao padrão")
        .absolute(edge - 4.0, 0.0)
        .w(8.0)
        .h(height)
        .cursor(Cursor::ResizeColumn)
        .hover(k.alpha(k.c.border_focus, 0.35))
        .on_drag(move |s: &mut State, d: &mut Drag, _tx: &mut Tx| {
            let step = d.dx;
            if resize(d, step, size(s), default, &clamp) {
                s.layout.save();
            }
        })
}

/// The app's scroll bars: a thumb dark enough to see at rest (the kit's is a faint line on a
/// light theme), wider and darker under the pointer. Shown always, the bar sits over the
/// viewport's end edge: what it scrolls leaves [`ScrollBarStyle::strip`] of room there.
fn bar_style(k: &Kit) -> ScrollBarStyle {
    ScrollBarStyle::from_kit(k).thumb(k.alpha(k.c.text_base, 0.3)).thumb_hover(k.alpha(k.c.text_base, 0.5)).width(8.0).hover_width(10.0)
}

/// The file list's column header, stuck to the top of its viewport.
const HEADER_H: f32 = 24.0;

fn list(s: &State, k: &Kit, p: usize) -> El {
    let pane = &s.panes[p];
    let w = s.layout.columns[p];
    let total: f32 = w.iter().sum();
    let arrow = |c: SortCol| if pane.sort.0 == c { if pane.sort.1 { "  ▲" } else { "  ▼" } } else { "" };
    let head = |i: usize, key: &str, label: &str, c: SortCol, end: bool| {
        let mut h = row()
            .key(key.to_string())
            .w(w[i])
            .h_full()
            .px(8.0)
            .center_y()
            .hover(k.c.hover)
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| set_sort(s, p, c))
            .child(text(format!("{label}{}", arrow(c))).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().min_w(0.0))
            .border_sides([0.0, 1.0, 0.0, 0.0], k.c.border_weak);
        if end {
            h = h.justify_end();
        }
        h
    };
    // The grips sit over the header, one at each column's right edge.
    let grips = (0..4).map(|i| col_grip(k, p, i, w[..=i].iter().sum()));
    let header = row()
        .key("header")
        .w_full()
        .h(HEADER_H)
        .fill(k.c.bg_base)
        .sticky_top(0.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(head(0, "h-name", "Nome", SortCol::Name, false))
        .child(head(1, "h-size", "Tamanho", SortCol::Size, true))
        .child(head(2, "h-mod", "Data", SortCol::Modified, false))
        .child(head(3, "h-kind", if matches!(pane.loc, Location::Found(_)) { "Pasta" } else { "Tipo" }, SortCol::Kind, false))
        .children(grips);

    let selected = k.alpha(k.c.text_accent, if k.dark { 0.26 } else { 0.18 });
    let hover = k.alpha(k.c.text_base, 0.045);
    let ring = k.c.border_focus;
    let (text_c, muted_c) = (k.c.text_base, k.c.text_muted);
    let font = k.ty.text_small;
    let rename_look = RenameLook { font, ink: text_c, fill: k.c.bg_base, edge: k.c.border_focus, selection: k.selection() };
    let build = move |s: &State, i: usize| -> El {
        let pane = &s.panes[p];
        let e = &pane.entries[i];
        let is_sel = pane.selected.contains(&e.id);
        let found = matches!(pane.loc, Location::Found(_));
        let mut r = row().key(row_key(&e.id)).role("row").label(shown(&e.name)).focus_selected(is_sel).w_full().h(ROW_H).center_y();
        r = if is_sel { r.fill(selected) } else { r.hover(hover) };
        if s.active == p && pane.cursor.as_deref() == Some(e.id.as_str()) {
            r = r.outline(1.0, -1.0, ring);
        }
        let (id_press, id_open, id_drag, id_start) = (e.id.clone(), e.id.clone(), e.id.clone(), e.id.clone());
        let cell = |i: usize| row().w(w[i]).h_full().px(8.0).center_y().clip_soft(0.0);
        let muted = |t: String| text(t).font(font).color(muted_c).truncate().shrink().min_w(0.0);
        r.child(
            cell(0)
                .px(6.0)
                .gap(6.0)
                .child(entry_icon(s, &pane.loc, e, 16.0))
                .child(match &s.renaming {
                    Some((rp, item)) if *rp == p && item.id == e.id => rename_field(s, rename_look),
                    _ => text(shown(&e.name)).font(font).color(text_c).truncate().shrink().min_w(0.0),
                }),
        )
        .child(cell(1).justify_end().child(muted(e.size.map(format_size).unwrap_or_default()).align(Align::End)))
        .child(cell(2).child(muted(e.modified.map(format_time).unwrap_or_default())))
        // A search's results show the folder each is in where the type goes.
        .child(cell(3).child(muted(if found { crate::finder::folder_of(e) } else { e.kind_label() })))
        .on_press_tx(move |s: &mut State, tx: &mut Tx| {
            click(s, p, &id_press, tx.mods());
            tx.focus(pane_key(p), false);
        })
        .on_secondary_press({
            let id = e.id.clone();
            // As in Explorer: a right click on an unselected row selects just that row first.
            move |s: &mut State, at: rune::PointerPress, tx: &mut Tx| {
                s.active = p;
                let pane = &mut s.panes[p];
                if !pane.selected.contains(&id) {
                    pane.selected = [id.clone()].into();
                    pane.anchor = Some(id.clone());
                }
                pane.cursor = Some(id.clone());
                tx.focus(pane_key(p), false);
                s.menu = Some((p, at));
            }
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
    // The rows end where the scroll bar starts; the header runs on above it.
    let bar = bar_style(k).track_offset(HEADER_H, 0.0);
    let rows = col()
        .key("rows")
        .w_full()
        .pr(bar.strip())
        .growth(GrowthSpec::default())
        .virtual_rows(n, ROW_H, move |s: &State, i| Cow::Owned(row_key(&s.panes[p].entries[i].id)), build)
        .virtual_rows_version((pane.version, w.map(f32::to_bits), s.renaming.as_ref().map(|(rp, e)| (*rp, e.id.clone()))));
    let notice = |msg: String, color| col().key("notice").w_full().grow(1.0).padding(16.0).child(text(msg).font(k.ty.text_small).color(color).wrap());
    let body = if let Some(err) = &pane.error {
        notice(err.clone(), k.c.danger.fg)
    } else if pane.loading && n == 0 {
        let searching = matches!(&pane.loc, Location::Found(_)) || matches!(&pane.loc, Location::Drive { space: Space::Search(_), path, .. } if path.is_empty());
        notice(if searching { "Pesquisando…" } else { "Carregando…" }.into(), k.c.text_muted)
    } else if let (0, Location::Drive { space: Space::Search(q), path, .. }) = (n, &pane.loc)
        && path.is_empty()
    {
        notice(format!("Nada com “{}” no nome em {}.", q.text, q.place), k.c.text_faint)
    } else if let (0, Location::Found(f)) = (n, &pane.loc) {
        notice(format!("Nada com “{}” no nome em {}.", f.text, f.place), k.c.text_faint)
    } else if n == 0 && matches!(pane.loc, Location::Drive { space: Space::SharedDrives, .. }) {
        notice("Esta conta não participa de nenhum drive compartilhado.".into(), k.c.text_muted)
    } else if let (0, Location::Drive { space, path, .. }) = (n, &pane.loc)
        && path.is_empty()
        && space.view().is_some()
    {
        let msg = match space {
            Space::SharedWithMe => "Ninguém compartilhou arquivos com esta conta.",
            Space::Starred => "Nenhum item com estrela. Use Adicionar estrela no menu de um arquivo.",
            Space::Recent => "Nenhum arquivo aberto recentemente.",
            _ => "A lixeira está vazia.",
        };
        notice(msg.into(), k.c.text_faint)
    } else if n == 0 {
        notice("Esta pasta está vazia.".into(), k.c.text_faint)
    } else {
        rows
    };
    // One viewport scrolls both ways: sideways when the columns are wider than the pane,
    // header and rows together, and up and down with the header stuck to the top. Its
    // vertical bar sits on the pane's right edge, wherever the columns end.
    let table = col().key("table").w_full().min_w(total + bar.strip()).child(header).child(body);
    // A right click on empty space opens the folder's menu (Colar, Nova pasta).
    // Its key is the pane's own: the scroll bar keeps its state, and finds the viewport it
    // scrolls, by this key, so with one key for both panes each bar moved the left pane.
    let viewport = col()
        .key(format!("viewport-{p}"))
        .w_full()
        .h_full()
        .scroll_xy()
        .on_secondary_press(move |s: &mut State, at: rune::PointerPress, tx: &mut Tx| {
            s.active = p;
            s.panes[p].selected.clear();
            tx.focus(pane_key(p), false);
            s.menu = Some((p, at));
        })
        .child(table)
        // The bar runs below the header, which stays put, as Explorer's does.
        .scroll_bar_styled(bar);
    col().key("list").w(0.0).grow(1.0).h_full().min_w(0.0).child(viewport)
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
    // A Drive account's storage, beside My Drive's folders (a shared drive's files count
    // against its organisation, not the account).
    let quota = match &pane.loc {
        Location::Drive { email, space: Space::MyDrive, .. } => s.quotas.get(email).copied().flatten(),
        _ => None,
    };
    let right = quota.map(|q| quota_label(&q)).unwrap_or(right);
    let meter = quota.and_then(|q| q.limit.filter(|l| *l > 0).map(|l| (q.usage as f64 / l as f64).min(1.0) as f32)).map(|used| {
        let fill = if used >= 0.9 { k.c.danger.fg } else { k.c.text_accent };
        row()
            .key("quota-meter")
            .w(64.0)
            .h(6.0)
            .mr(8.0)
            .radius(3.0)
            .fill(k.c.border_weak)
            .clip_soft(3.0)
            .child(el().w(64.0 * used).h_full().radius(3.0).fill(fill))
    });
    let help = quota.map(quota_help).unwrap_or_default();
    let mut bar = row()
        .key("statusbar")
        .w_full()
        .h(24.0)
        .px(10.0)
        .center_y()
        .border_sides([1.0, 0.0, 0.0, 0.0], k.c.border_weak)
        .child(text(left).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().min_w(0.0).grow(1.0));
    if let Some(m) = meter {
        bar = bar.child(m);
    }
    let right = text(right).font(k.ty.text_xsmall).color(k.c.text_muted);
    bar.child(if help.is_empty() { right } else { right.key("quota").help(help) })
}

/// The status bar's word on an account's storage: "12,3 GB livres de 15 GB", or what it uses
/// when there's no limit.
pub fn quota_label(q: &openexplorer_gdrive::Quota) -> String {
    match (q.limit, q.free()) {
        (Some(limit), Some(free)) => format!("{} livres de {}", format_size(free), whole_size(limit)),
        _ => format!("{} usados · Ilimitado", format_size(q.usage)),
    }
}

/// The quota's breakdown, on hover.
fn quota_help(q: openexplorer_gdrive::Quota) -> String {
    let mut out = format!("Usado: {}", format_size(q.usage));
    if let Some(l) = q.limit {
        out.push_str(&format!(" de {}", whole_size(l)));
    }
    out.push_str(&format!("
No Drive: {} (lixeira: {})", format_size(q.in_drive), format_size(q.in_trash)));
    out.push_str(&format!("
Gmail e Fotos: {}", format_size(q.usage.saturating_sub(q.in_drive))));
    out
}

/// A plan's size without a needless ",0": "15 GB", "2 TB", but "1,5 TB".
fn whole_size(b: u64) -> String {
    let s = format_size(b);
    match s.split_once(' ') {
        Some((n, unit)) if n.ends_with(",0") => format!("{} {unit}", &n[..n.len() - 2]),
        _ => s,
    }
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
