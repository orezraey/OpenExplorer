//! The window: a toolbar, two panes (each a location tree beside a file list, under a
//! navigation bar and over a status bar), and the message line.

use std::borrow::Cow;
use std::path::PathBuf;

use rune::{Align, Color, Cursor, Cx, Drag, DragOutcome, El, GrowthSpec, Icon, KeyEvent, Phase, ScrollSpec, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, Dialog as Modal, Kit, MenuItem, ScrollBar, context_menu};

use crate::model::{Kind, Location, SortCol, Space, crumbs, format_bytes, format_long, format_size, format_time};
use crate::actions::{self, ClipMode, Dialog};
use crate::{State, click, connect, disconnect, drag_item, dragged_ids, dropped, go_back, go_forward, go_up, key, load, navigate, open, pane_key, paths_for, row_key, set_sort};
use crate::icons;
use crate::sysicons::{self, IconKey};
use crate::layout::{self, clamp_col, clamp_tree};

const ROW_H: f32 = crate::ROW_H;

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    let k = Kit::neutral(cx.system_dark());
    let message = text(s.status.clone()).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().min_w(0.0).help_when_truncated(s.status.clone());
    // Menus and dialogs draw in a layer above everything.
    let mut layer = Vec::new();
    let menu = context(s, &k, &mut layer);
    dialog(s, &k, &mut layer);
    let body = col()
        .w_full()
        .h_full()
        .fill(k.c.page)
        .child(toolbar(s, &k))
        .child(panes(s, &k))
        .child(row().key("message").w_full().h(22.0).px(10.0).center_y().child(message))
        .child(menu);
    rune_kit::root(body, layer)
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
    if n > 0 {
        menu = menu
            .item(item("Abrir", &icons::M_OPEN, "Enter", n == 1, actions::open))
            .separator()
            .item(item("Recortar", &icons::M_CUT, "Ctrl+X", editable, |s, p, _| actions::pick_up(s, p, ClipMode::Cut)))
            .item(item("Copiar", &icons::M_COPY, "Ctrl+C", editable, |s, p, _| actions::pick_up(s, p, ClipMode::Copy)));
    }
    menu = menu.item(item("Colar", &icons::M_PASTE, "Ctrl+V", actions::can_paste(s, p), |s, p, _| actions::paste(s, p)));
    if n > 0 {
        menu = menu
            .separator()
            .item(item("Excluir", &icons::M_DELETE, "Del", editable, |s, p, _| actions::ask_delete(s, p)).danger(true))
            .item(item("Renomear", &icons::M_RENAME, "F2", editable && n == 1, actions::ask_rename));
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
            let local = matches!(s.panes[*pane].loc, Location::Local(_));
            let title = match items.as_slice() {
                [one] => format!("Excluir \"{}\"?", one.name),
                many => format!("Excluir {} itens?", ui_count(many.len())),
            };
            let where_to = if local {
                "Os itens vão para a Lixeira do Windows, de onde podem ser restaurados."
            } else {
                "Os itens vão para a lixeira do Google Drive, onde ficam 30 dias antes de serem apagados de vez."
            };
            let mut list = col().key("items").w_full().gap(4.0);
            for e in items.iter().take(6) {
                list = list.child(
                    row().gap(6.0).center_y().child(entry_icon(s, &s.panes[*pane].loc, e, 16.0)).child(text(e.name.clone()).font(k.ty.text_small).color(k.c.text_base).truncate().shrink().min_w(0.0)),
                );
            }
            if items.len() > 6 {
                list = list.child(text(format!("e mais {}", ui_count(items.len() - 6))).font(k.ty.text_small).color(k.c.text_muted));
            }
            // The explanation goes in the body, where it wraps; the dialog's own description
            // line doesn't, and would run under the close button.
            let note = text(where_to).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0);
            let body = col().w_full().gap(12.0).child(note).child(list).child(actions_row(k, "Cancelar", Button::new("Excluir").variant(ButtonVariant::Danger).on_press(actions::delete)));
            Modal::new("delete", title).open(true).child(pad(body)).width(460.0).fit().on_close(close).build(k, layer);
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
    }
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
        (Location::Local(dir), Some(e)) => dir.join(&e.name).display().to_string(),
        (Location::Local(dir), None) => dir.display().to_string(),
        (loc, single) => {
            let mut parts: Vec<String> = crumbs(loc).into_iter().skip(1).map(|c| c.label).collect();
            if let Some(e) = single {
                parts.push(e.name.clone());
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
        Some(e) => (entry_icon(s, loc, e, 40.0), e.name.clone()),
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
                (_, None) if matches!(loc, Location::Local(_)) => "Calculando…".into(),
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
                        names = names.child(row().gap(6.0).center_y().child(entry_icon(s, loc, e, 16.0)).child(text(e.name.clone()).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0)));
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
        .child(text("Open Explorer").font(k.ty.text_small_medium).color(k.c.text_faint).mx(8.0))
}

// ---------------------------------------------------------------------------------------------
// Splitters

/// The drag that resizes something to `size`: Down keeps the starting size, each move sets it
/// to the start plus the travel (through `clamp`), a cancelled drag puts it back, and a double
/// click sets `default`. Returns whether the layout changed and should be saved: on a double
/// click, and on release when the size ended up different.
fn resize(d: &mut Drag, size: &mut f32, default: f32, clamp: impl Fn(f32) -> f32) -> bool {
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
            *size = clamp(d.data + d.dx);
            false
        }
        Phase::Up => {
            *size = clamp(d.data + d.dx);
            *size != d.data
        }
    }
}

/// A vertical bar to drag sideways (between a pane's tree and its list), `width` wide, showing
/// a line on hover and while dragged. `size` picks the width it changes in the layout.
fn splitter(k: &Kit, key: &str, width: f32, size: impl Fn(&mut State) -> &mut f32 + 'static, default: f32, clamp: fn(f32) -> f32) -> El {
    let line = k.c.border_focus;
    row()
        .key(key.to_string())
        .role("separator")
        .label("Arraste para redimensionar; clique duas vezes para voltar ao padrão")
        .w(width)
        .h_full()
        .justify_center()
        .cursor(Cursor::ResizeColumn)
        .hover(k.alpha(line, 0.18))
        .press(k.alpha(line, 0.32))
        .on_drag(move |s: &mut State, d: &mut Drag, _tx: &mut Tx| {
            if resize(d, size(s), default, clamp) {
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
        .min_h(0.0)
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
        .child(
            row()
                .key("body")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(tree(s, k, p))
                .child(
                    splitter(k, "tree-split", 5.0, move |s: &mut State| &mut s.layout.tree[p], layout::TREE_W, clamp_tree)
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
fn node(k: &Kit, p: usize, id: String, depth: usize, glyph: El, label: &str, current: &str, to: Location) -> El {
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
        .child(glyph)
        .child(text(label.to_string()).font(k.ty.text_small).color(k.c.text_base).truncate().shrink().min_w(0.0).help_when_truncated(label.to_string()))
}

fn heading(k: &Kit, label: &str) -> El {
    row().w_full().h(24.0).px(10.0).pt(6.0).center_y().child(text(label.to_string()).font(k.ty.text_xsmall).color(k.c.text_faint))
}

fn tree(s: &State, k: &Kit, p: usize) -> El {
    let current = node_of(&s.panes[p].loc);
    let mut t = col().key("tree").w_full().pb(8.0);
    t = t.child(heading(k, "Este computador")).child(node(k, p, "home".into(), 0, shell_icon(s, sysicons::home_key().as_ref(), &icons::HOME, 16.0), "Pasta pessoal", &current, Location::home()));
    for (d, kind) in &s.local_drives {
        let letter = d.display().to_string().trim_end_matches('\\').to_string();
        t = t.child(node(k, p, format!("disk:{letter}"), 0, shell_icon(s, Some(&sysicons::drive_key(d)), &icons::DISK, 16.0), &kind.label(&letter), &current, Location::Local(d.clone())));
    }
    if !s.accounts.is_empty() {
        t = t.child(heading(k, "Google Drive"));
    }
    for a in &s.accounts {
        let e = &a.email;
        t = t
            .child(node(k, p, format!("acc:{e}"), 0, icon(&icons::CLOUD, 16.0), e, &current, Location::drive(e.clone(), Space::MyDrive)))
            .child(node(k, p, format!("acc:{e}:my"), 1, shell_icon(s, Some(&IconKey::Folder), &icons::FOLDER, 16.0), "Meu Drive", &current, Location::drive(e.clone(), Space::MyDrive)))
            .child(node(k, p, format!("acc:{e}:sd"), 1, icon(&icons::SHARED, 16.0), "Drives compartilhados", &current, Location::drive(e.clone(), Space::SharedDrives)));
        // The shared drives, once their list has been loaded.
        if let Some(drives) = s.cache.shared_drives(e) {
            for d in drives.iter() {
                let space = Space::Shared { id: d.id.clone(), name: d.name.clone() };
                t = t.child(node(k, p, format!("acc:{e}:sd:{}", d.id), 2, icon(&icons::SHARED, 16.0), &d.name, &current, Location::drive(e.clone(), space)));
            }
        }
    }
    t = t.child(heading(k, "Testes")).child(node(k, p, "mock".into(), 0, icon(&icons::LAB, 16.0), "Drive simulado", &current, Location::Mock(Vec::new())));
    col()
        .key("tree-scroll")
        .w(s.layout.tree[p])
        .h_full()
        
        .fill(k.alpha(k.c.text_base, if k.dark { 0.02 } else { 0.015 }))
        .scroll(ScrollSpec { stick_bottom: false })
        .child(t)
}

// ---------------------------------------------------------------------------------------------
// File list

/// The grip on a column's right edge, as in Windows Explorer: it moves with the pointer,
/// resizing that column and carrying the columns after it along. A double click puts the column
/// back to its default width.
fn col_grip(k: &Kit, p: usize, i: usize, width: f32) -> El {
    el().key(format!("grip-{i}"))
        .role("separator")
        .label("Arraste para mudar a largura da coluna; clique duas vezes para voltar ao padrão")
        .absolute(width - 4.0, 0.0)
        .w(8.0)
        .h(24.0)
        .cursor(Cursor::ResizeColumn)
        .hover(k.alpha(k.c.border_focus, 0.35))
        .on_drag(move |s: &mut State, d: &mut Drag, _tx: &mut Tx| {
            if resize(d, &mut s.layout.columns[p][i], layout::COLS[i], |w| clamp_col(i, w)) {
                s.layout.save();
            }
        })
}

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
        .h(24.0)
        .fill(k.c.bg_base)
        .sticky_top(0.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(head(0, "h-name", "Nome", SortCol::Name, false))
        .child(head(1, "h-size", "Tamanho", SortCol::Size, true))
        .child(head(2, "h-mod", "Data", SortCol::Modified, false))
        .child(head(3, "h-kind", "Tipo", SortCol::Kind, false))
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
        let mut r = row().key(row_key(&e.id)).role("row").label(e.name.clone()).focus_selected(is_sel).w_full().h(ROW_H).center_y();
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
                    _ => text(e.name.clone()).font(font).color(text_c).truncate().shrink().min_w(0.0),
                }),
        )
        .child(cell(1).justify_end().child(muted(e.size.map(format_size).unwrap_or_default()).align(Align::End)))
        .child(cell(2).child(muted(e.modified.map(format_time).unwrap_or_default())))
        .child(cell(3).child(muted(e.kind_label())))
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
    let rows = col()
        .key("rows")
        .w_full()
        .growth(GrowthSpec::default())
        .virtual_rows(n, ROW_H, move |s: &State, i| Cow::Owned(row_key(&s.panes[p].entries[i].id)), build)
        .virtual_rows_version((pane.version, w.map(f32::to_bits), s.renaming.as_ref().map(|(rp, e)| (*rp, e.id.clone()))));
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
        rows
    };
    // One viewport scrolls both ways: sideways when the columns are wider than the pane,
    // header and rows together, and up and down with the header stuck to the top. Its
    // vertical bar sits on the pane's right edge, wherever the columns end.
    let table = col().key("table").w_full().min_w(total).child(header).child(body);
    // A right click on empty space opens the folder's menu (Colar, Nova pasta).
    let viewport = col()
        .key("viewport")
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
        .scroll_bar(k);
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
