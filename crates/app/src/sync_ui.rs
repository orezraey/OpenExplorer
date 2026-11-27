//! UI for the Sincronizar tab, Sync Options dialog, Filter dialog, Folder Browser dialog,
//! and Save Sync dialog.

use rune::{Color, Cursor, El, ScrollSpec, Tx, col, el, icon, row, text};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, Checkbox, Dialog as Modal, Kit, Placement, ScrollBar,
    Select, SelectAppearance, TextArea, TextInput, popover, scrim,
};

use openexplorer_i18n::{t, tr};

use crate::actions::Dialog;
use crate::icons;
use crate::model::{format_size, format_time};
use crate::sync::{
    self, ActionFilter, BrowseDialog, DateFilterMode, DateUnit, FilterDialog, FilterDropdown, LeftOnlyAction,
    NewerAction, OptionsDialog, OptionsDropdown, RightOnlyAction, RowStatus, SaveDialog, Side, SizeUnit, StatusFilter,
    SyncAction, SyncLocation, SyncMode, SyncPhase, ToolbarMenu,
};
use crate::State;

// ---------------------------------------------------------------------------------------------
// Main Sincronizar tab view

pub fn sync_view(s: &State, k: &Kit, backdrop: bool, layer: &mut Vec<El>) -> El {
    let busy = s.sync.is_busy();
    let surface_bg = if backdrop {
        k.alpha(k.c.bg_base, if k.dark { 0.55 } else { 0.72 })
    } else {
        k.c.bg_base
    };

    // 1. Header bar: title, saved profiles selector, + Nova sincronização
    let saved_anchor = "sync-saved-trigger";
    let saved_open = s.sync.open_menu == Some(ToolbarMenu::SavedList);
    let active_title = if s.sync.active_name.is_empty() {
        t("Nova sincronização", "New sync")
    } else {
        s.sync.active_name.as_str()
    };

    let saved_btn = k
        .interactive(
            row()
                .key(saved_anchor)
                .role("button")
                .focusable(true)
                .focus_ring(2.0, 1.0, k.c.border_focus)
                .h(30.0)
                .px(10.0)
                .gap(6.0)
                .center_y()
                .radius(k.radius.md)
                .fill(k.alpha(k.c.text_base, if k.dark { 0.05 } else { 0.04 }))
                .border(1.0, k.c.border_weak)
                .hover(k.c.hover)
                .cursor(Cursor::Pointer)
                .child(icon(&icons::TAB_SYNC, 14.0).color(k.c.text_accent))
                .child(text(active_title.to_string()).font(k.ty.text_small_medium).color(k.c.text_base))
                .child(
                    Badge::new(s.sync.saved.len().to_string())
                        .variant(BadgeVariant::Subtle)
                        .build(k),
                )
                .child(icon(&icons::CHEVRON_DOWN, 12.0).color(k.c.icon_muted)),
        )
        .on_press(|s: &mut State| {
            s.sync.open_menu = if s.sync.open_menu == Some(ToolbarMenu::SavedList) {
                None
            } else {
                Some(ToolbarMenu::SavedList)
            };
        });

    if saved_open {
        layer.push(scrim("sync-saved-scrim", |s: &mut State| s.sync.open_menu = None));
        let dropdown_bg = if k.dark { rune::oklch(0.22, 0.008, 255.0) } else { k.c.bg_base };
        let mut menu = col().key("sync-saved-menu").w(280.0).padding(6.0).gap(3.0);
        if s.sync.saved.is_empty() {
            menu = menu.child(
                row().h(32.0).px(8.0).center_y().child(
                    text(t("Nenhuma sincronização salva ainda.", "No saved syncs yet."))
                        .font(k.ty.text_small)
                        .color(k.c.text_faint),
                ),
            );
        } else {
            for item in &s.sync.saved {
                let id_load = item.id.clone();
                let id_del = item.id.clone();
                let is_active = s.sync.active_saved_id.as_deref() == Some(item.id.as_str());
                let row_item = row()
                    .key(format!("saved-{}", item.id))
                    .w_full()
                    .h(32.0)
                    .px(8.0)
                    .gap(6.0)
                    .center_y()
                    .radius(k.radius.sm)
                    .fill(if is_active {
                        k.alpha(k.c.text_accent, if k.dark { 0.20 } else { 0.11 })
                    } else {
                        Color::TRANSPARENT
                    })
                    .hover(k.c.hover)
                    .child(
                        row()
                            .grow(1.0)
                            .min_w(0.0)
                            .h_full()
                            .center_y()
                            .gap(6.0)
                            .cursor(Cursor::Pointer)
                            .on_press(move |s: &mut State| sync::load_saved(s, &id_load))
                            .child(
                                text(item.name.clone())
                                    .font(if is_active { k.ty.text_small_medium } else { k.ty.text_small })
                                    .color(k.c.text_base)
                                    .truncate()
                                    .min_w(0.0)
                                    .grow(1.0),
                            )
                            .child(
                                text(item.options.mode.label())
                                    .font(k.ty.caption())
                                    .color(k.c.text_faint),
                            ),
                    )
                    .child(
                        k.interactive(
                            row()
                                .key(format!("saved-del-{}", item.id))
                                .size(22.0, 22.0)
                                .center()
                                .radius(k.radius.sm)
                                .hover(k.alpha(k.c.danger.fg, 0.16))
                                .cursor(Cursor::Pointer)
                                .child(text("×").font(k.ty.text_small_medium).color(k.c.text_muted)),
                        )
                        .on_press(move |s: &mut State| sync::delete_saved(s, &id_del)),
                    );
                menu = menu.child(row_item);
            }
        }
        let surface = menu
            .radius(k.radius.md)
            .fill(dropdown_bg)
            .border(1.0, k.c.border_weak)
            .box_shadow(rune::Shadow { y: 6.0, blur: 18.0, color: k.dark_a(0.32), ..Default::default() });
        layer.push(popover::anchored(k, surface, saved_anchor, Placement::BottomStart, 4.0));
    }

    let header = row()
        .key("sync-header")
        .w_full()
        .px(14.0)
        .py(8.0)
        .gap(8.0)
        .center_y()
        .fill(surface_bg)
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(
            Button::new(t("+ Criar nova sincronização", "+ New sync"))
                .variant(ButtonVariant::Submit)
                .disabled(busy)
                .on_press(sync::new_sync)
                .build(k),
        )
        .child(saved_btn)
        .child(el().grow(1.0))
        .child(
            text(tr!(
                "{} par(es) de pastas",
                "{} folder pair(s)",
                s.sync.pairs.len()
            ))
            .font(k.ty.caption())
            .color(k.c.text_faint),
        );

    // 2. Folder pairs section (2 or more folders)
    let mut pairs_col = col()
        .key("sync-pairs")
        .w_full()
        .px(14.0)
        .py(10.0)
        .gap(8.0)
        .fill(surface_bg)
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak);

    let multi = s.sync.pairs.len() > 1;
    for (idx, pair) in s.sync.pairs.iter().enumerate() {
        let pair_id = pair.id;
        let left_box = folder_picker_box(s, k, pair_id, Side::Left, pair.left.as_ref(), busy, layer);
        let right_box = folder_picker_box(s, k, pair_id, Side::Right, pair.right.as_ref(), busy, layer);

        let swap_btn = k
            .interactive(
                row()
                    .key(format!("sync-swap-{pair_id}"))
                    .role("button")
                    .label(t("Inverter pastas", "Swap folders"))
                    .size(30.0, 30.0)
                    .center()
                    .radius(k.radius.md)
                    .hover(k.c.hover)
                    .cursor(Cursor::Pointer)
                    .child(icon(&icons::SYNC_SWAP, 15.0).color(if busy { k.c.text_faint } else { k.c.text_muted })),
            )
            .help(t("Trocar lado esquerdo e direito", "Swap left and right folders"))
            .on_press(move |s: &mut State| sync::swap_pair(s, pair_id));

        let mut pair_row = row()
            .key(format!("sync-pair-{pair_id}"))
            .w_full()
            .gap(8.0)
            .center_y();

        if multi {
            pair_row = pair_row.child(
                text(format!("#{}", idx + 1))
                    .font(k.ty.caption())
                    .color(k.c.text_faint)
                    .w(22.0),
            );
        }

        pair_row = pair_row
            .child(left_box)
            .child(swap_btn)
            .child(right_box);

        if multi {
            let remove_btn = k
                .interactive(
                    row()
                        .key(format!("sync-rm-{pair_id}"))
                        .role("button")
                        .label(t("Remover par", "Remove pair"))
                        .size(28.0, 28.0)
                        .center()
                        .radius(k.radius.md)
                        .hover(k.alpha(k.c.danger.fg, 0.14))
                        .cursor(Cursor::Pointer)
                        .child(text("×").font(k.ty.text_small_medium).color(k.c.text_muted)),
                )
                .help(t("Remover este par de pastas", "Remove this folder pair"))
                .on_press(move |s: &mut State| sync::remove_pair(s, pair_id));
            pair_row = pair_row.child(remove_btn);
        }

        pairs_col = pairs_col.child(pair_row);
    }

    // 3. Action toolbar (matches reference image 1)
    let filter_count = s.sync.options.filter.active_count();
    let config_label = format!("{} - {}", t("Configuração", "Settings"), s.sync.options.mode.label());

    let mut config_btn_inner = row()
        .key("sync-config-btn")
        .role("button")
        .focusable(true)
        .focus_ring(2.0, 1.0, k.c.border_focus)
        .h(30.0)
        .px(10.0)
        .gap(6.0)
        .center_y()
        .radius(k.radius.md)
        .hover(k.c.hover)
        .cursor(Cursor::Pointer)
        .child(icon(&icons::SETTINGS, 14.0).color(k.c.icon_muted))
        .child(text(config_label).font(k.ty.text_small_medium).color(k.c.text_base));
    if filter_count > 0 {
        config_btn_inner = config_btn_inner.child(
            Badge::new(tr!("{filter_count} filtro(s)", "{filter_count} filter(s)"))
                .variant(BadgeVariant::Accent)
                .build(k),
        );
    }
    let config_btn = k.interactive(config_btn_inner).on_press(sync::open_options);

    let add_folders_btn = toolbar_chip(
        k,
        "sync-add-pair",
        &icons::PLUS,
        t("Adicionar pastas", "Add folders"),
        !busy,
        false,
        sync::add_pair,
    );

    let save_btn = toolbar_chip(
        k,
        "sync-save",
        &icons::SYNC_SAVE,
        t("Salvar…", "Save…"),
        !busy,
        false,
        sync::open_save,
    );

    let compare_btn = toolbar_chip(
        k,
        "sync-compare",
        &icons::SYNC_COMPARE,
        t("Comparar", "Compare"),
        !busy && s.sync.has_valid_pair(),
        false,
        sync::start_compare,
    );

    let start_or_cancel = if busy {
        Button::new(t("Cancelar", "Cancel"))
            .variant(ButtonVariant::Danger)
            .on_press(sync::cancel_current)
            .build(k)
    } else {
        let can_start = s.sync.planned_action_count() > 0;
        Button::new(t("Iniciar", "Start"))
            .variant(ButtonVariant::Submit)
            .disabled(!can_start)
            .on_press(sync::start_sync)
            .build(k)
    };

    let auto_scroll_btn = toolbar_chip(
        k,
        "sync-autoscroll",
        &icons::DOWNLOAD,
        t("Rolagem automática", "Auto-scroll"),
        true,
        s.sync.auto_scroll,
        |s: &mut State| s.sync.auto_scroll = !s.sync.auto_scroll,
    );

    // Action filter dropdown
    let action_opts: Vec<&str> = ActionFilter::ALL.iter().map(|f| f.label()).collect();
    let action_select = Select::new("sync-action-filter", &action_opts)
        .current(Some(s.sync.action_filter.label()))
        .appearance(SelectAppearance::Inline)
        .open(s.sync.open_menu == Some(ToolbarMenu::ActionFilter))
        .on_toggle(|s: &mut State| {
            s.sync.open_menu = if s.sync.open_menu == Some(ToolbarMenu::ActionFilter) {
                None
            } else {
                Some(ToolbarMenu::ActionFilter)
            };
        })
        .on_close(|s: &mut State| {
            if s.sync.open_menu == Some(ToolbarMenu::ActionFilter) {
                s.sync.open_menu = None;
            }
        })
        .on_choose(|s: &mut State, v, _| {
            if let Some(f) = ActionFilter::from_label(v) {
                s.sync.action_filter = f;
            }
            s.sync.open_menu = None;
        })
        .build(k, layer);

    // Status filter dropdown
    let status_opts: Vec<&str> = StatusFilter::ALL.iter().map(|f| f.label()).collect();
    let status_select = Select::new("sync-status-filter", &status_opts)
        .current(Some(s.sync.status_filter.label()))
        .appearance(SelectAppearance::Inline)
        .open(s.sync.open_menu == Some(ToolbarMenu::StatusFilter))
        .on_toggle(|s: &mut State| {
            s.sync.open_menu = if s.sync.open_menu == Some(ToolbarMenu::StatusFilter) {
                None
            } else {
                Some(ToolbarMenu::StatusFilter)
            };
        })
        .on_close(|s: &mut State| {
            if s.sync.open_menu == Some(ToolbarMenu::StatusFilter) {
                s.sync.open_menu = None;
            }
        })
        .on_choose(|s: &mut State, v, _| {
            if let Some(f) = StatusFilter::from_label(v) {
                s.sync.status_filter = f;
            }
            s.sync.open_menu = None;
        })
        .build(k, layer);

    let search_input = TextInput::new(&s.sync.search)
        .width(Some(170.0))
        .leading_icon(&icons::SEARCH)
        .placeholder(t("Pesquisar", "Search"))
        .on_edit(|s: &mut State| &mut s.sync.search)
        .build(k);

    let sep = || el().w(1.0).h(18.0).mx(2.0).fill(k.c.border_weak);

    let actions_bar = row()
        .key("sync-toolbar")
        .w_full()
        .px(14.0)
        .py(6.0)
        .gap(6.0)
        .center_y()
        .fill(surface_bg)
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(add_folders_btn)
        .child(config_btn)
        .child(save_btn)
        .child(sep())
        .child(compare_btn)
        .child(start_or_cancel)
        .child(sep())
        .child(auto_scroll_btn)
        .child(action_select)
        .child(status_select)
        .child(el().grow(1.0))
        .child(search_input);

    // 4. Comparison table
    let table = comparison_table(s, k);

    // 5. Footer summary bar
    let footer = sync_footer(s, k, surface_bg);

    col()
        .key("sync-view")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(header)
        .child(pairs_col)
        .child(actions_bar)
        .child(table)
        .child(footer)
}

fn toolbar_chip(
    k: &Kit,
    key: &'static str,
    glyph: &'static rune::Icon,
    label: &str,
    enabled: bool,
    active: bool,
    on_press: fn(&mut State),
) -> El {
    let ink = if !enabled {
        k.c.text_faint
    } else if active {
        k.c.text_accent
    } else {
        k.c.text_base
    };
    let icon_ink = if !enabled {
        k.c.text_faint
    } else if active {
        k.c.text_accent
    } else {
        k.c.icon_muted
    };
    let bg = if active {
        k.alpha(k.c.text_accent, if k.dark { 0.16 } else { 0.10 })
    } else {
        Color::TRANSPARENT
    };
    let base = row()
        .key(key)
        .role("button")
        .label(label.to_string())
        .h(30.0)
        .px(9.0)
        .gap(6.0)
        .center_y()
        .radius(k.radius.md)
        .fill(bg)
        .child(icon(glyph, 14.0).color(icon_ink))
        .child(text(label.to_string()).font(k.ty.text_small).color(ink));
    if enabled {
        k.interactive(base)
            .focusable(true)
            .focus_ring(2.0, 1.0, k.c.border_focus)
            .hover(k.c.hover)
            .cursor(Cursor::Pointer)
            .on_press(on_press)
    } else {
        base.disabled(true)
    }
}

fn folder_picker_box(
    s: &State,
    k: &Kit,
    pair_id: u64,
    side: Side,
    loc: Option<&SyncLocation>,
    busy: bool,
    layer: &mut Vec<El>,
) -> El {
    let side_tag = match side {
        Side::Left => "l",
        Side::Right => "r",
    };
    let label_text = loc
        .map(SyncLocation::display_label)
        .unwrap_or_else(|| t("Selecionar pasta", "Select folder").into());
    let has_loc = loc.is_some();
    let glyph = match loc {
        Some(SyncLocation::Local(_)) => &icons::FOLDER,
        Some(SyncLocation::Drive { .. }) => &icons::CLOUD,
        Some(SyncLocation::Server { .. }) => &icons::GLOBE,
        None => &icons::CLOUD,
    };

    let field = k
        .interactive(
            row()
                .key(format!("sync-loc-{pair_id}-{side_tag}"))
                .role("button")
                .grow(1.0)
                .min_w(0.0)
                .h(32.0)
                .px(10.0)
                .gap(8.0)
                .center_y()
                .radius(k.radius.md)
                .fill(k.alpha(k.c.text_base, if k.dark { 0.05 } else { 0.035 }))
                .border(1.0, k.c.border_weak)
                .hover(k.c.hover)
                .cursor(Cursor::Pointer)
                .child(icon(glyph, 15.0).color(if has_loc { k.c.text_accent } else { k.c.icon_muted }))
                .child(
                    text(label_text)
                        .font(k.ty.text_small)
                        .color(if has_loc { k.c.text_base } else { k.c.text_faint })
                        .truncate()
                        .min_w(0.0)
                        .grow(1.0),
                ),
        )
        .disabled(busy)
        .on_press(move |s: &mut State| sync::open_browse(s, pair_id, side));

    let anchor = format!("sync-browse-btn-{pair_id}-{side_tag}");
    let menu_open = s.sync.open_menu == Some(ToolbarMenu::BrowseQuick(pair_id, side));

    let browse_btn = k
        .interactive(
            row()
                .key(anchor.clone())
                .role("button")
                .h(32.0)
                .px(10.0)
                .gap(5.0)
                .center_y()
                .radius(k.radius.md)
                .fill(k.alpha(k.c.text_base, if k.dark { 0.05 } else { 0.035 }))
                .border(1.0, k.c.border_weak)
                .hover(k.c.hover)
                .cursor(Cursor::Pointer)
                .child(icon(&icons::TAB_EXPLORE, 14.0).color(k.c.icon_muted))
                .child(text(t("Procurar…", "Browse…")).font(k.ty.text_small).color(k.c.text_base))
                .child(icon(&icons::CHEVRON_DOWN, 11.0).color(k.c.icon_muted)),
        )
        .disabled(busy)
        .on_press(move |s: &mut State| {
            let target = ToolbarMenu::BrowseQuick(pair_id, side);
            s.sync.open_menu = if s.sync.open_menu == Some(target) { None } else { Some(target) };
        });

    if menu_open {
        layer.push(scrim(format!("sync-browse-scrim-{pair_id}-{side_tag}"), |s: &mut State| {
            s.sync.open_menu = None;
        }));
        let dropdown_bg = if k.dark { rune::oklch(0.22, 0.008, 255.0) } else { k.c.bg_base };
        let menu_item = |key: String, label: &str, glyph: &'static rune::Icon, on_click: Box<dyn Fn(&mut State) + 'static>| {
            row()
                .key(key)
                .role("menuitem")
                .w_full()
                .h(30.0)
                .px(10.0)
                .gap(8.0)
                .center_y()
                .radius(k.radius.sm)
                .hover(k.c.hover)
                .cursor(Cursor::Pointer)
                .on_press(move |s: &mut State| on_click(s))
                .child(icon(glyph, 14.0).color(k.c.icon_muted))
                .child(text(label.to_string()).font(k.ty.text_small).color(k.c.text_base))
        };

        let mut menu = col().key(format!("sync-browse-menu-{pair_id}-{side_tag}")).w(230.0).padding(4.0).gap(2.0);
        menu = menu.child(menu_item(
            format!("sb-open-{pair_id}-{side_tag}"),
            t("Procurar pasta…", "Browse folder…"),
            &icons::FOLDER,
            Box::new(move |s: &mut State| sync::open_browse(s, pair_id, side)),
        ));
        if !s.tabs[0].is_picker() {
            menu = menu.child(menu_item(
                format!("sb-left-{pair_id}-{side_tag}"),
                t("Usar painel esquerdo", "Use left pane"),
                &icons::OPEN_LEFT_PANE,
                Box::new(move |s: &mut State| sync::use_pane_folder(s, pair_id, side, 0)),
            ));
        }
        if !s.tabs[1].is_picker() {
            menu = menu.child(menu_item(
                format!("sb-right-{pair_id}-{side_tag}"),
                t("Usar painel direito", "Use right pane"),
                &icons::OPEN_RIGHT_PANE,
                Box::new(move |s: &mut State| sync::use_pane_folder(s, pair_id, side, 1)),
            ));
        }
        let surface = menu
            .radius(k.radius.md)
            .fill(dropdown_bg)
            .border(1.0, k.c.border_weak)
            .box_shadow(rune::Shadow { y: 6.0, blur: 18.0, color: k.dark_a(0.32), ..Default::default() });
        layer.push(popover::anchored(k, surface, &anchor, Placement::BottomEnd, 4.0));
    }

    row()
        .key(format!("sync-side-{pair_id}-{side_tag}"))
        .grow(1.0)
        .min_w(0.0)
        .gap(6.0)
        .center_y()
        .child(field)
        .child(browse_btn)
}

fn comparison_table(s: &State, k: &Kit) -> El {
    let header_bg = if k.dark { k.alpha(k.c.white, 0.025) } else { k.alpha(k.c.black, 0.025) };
    let col_hdr = |label: &str| {
        text(label.to_string())
            .font(k.ty.caption())
            .color(k.c.text_muted)
            .truncate()
    };

    let header = row()
        .key("sync-table-header")
        .w_full()
        .h(28.0)
        .px(14.0)
        .gap(8.0)
        .center_y()
        .fill(header_bg)
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(col_hdr(t("Caminho (Esquerda)", "Path (Left)")).grow(1.0).min_w(0.0))
        .child(col_hdr(t("Tamanho", "Size")).w(74.0))
        .child(col_hdr(t("Data", "Date")).w(116.0))
        .child(col_hdr(t("Ação", "Action")).w(88.0))
        .child(col_hdr(t("Caminho (Direita)", "Path (Right)")).grow(1.0).min_w(0.0))
        .child(col_hdr(t("Tamanho", "Size")).w(74.0))
        .child(col_hdr(t("Data", "Date")).w(116.0))
        .child(col_hdr(t("Estado", "Status")).w(120.0));

    let query = s.sync.search.text.trim().to_lowercase();
    let visible: Vec<&sync::CompareRow> = s
        .sync
        .rows
        .iter()
        .filter(|r| {
            s.sync.action_filter.matches(r.action)
                && s.sync.status_filter.matches(&r.status)
                && (query.is_empty() || r.rel_path.to_lowercase().contains(&query))
        })
        .collect();

    let body = if s.sync.phase == SyncPhase::Comparing {
        col()
            .w_full()
            .grow(1.0)
            .center()
            .gap(8.0)
            .child(icon(&icons::TAB_SYNC, 26.0).color(k.c.text_accent))
            .child(text(s.sync.progress_text.clone()).font(k.ty.text_small).color(k.c.text_muted))
    } else if visible.is_empty() {
        let empty_msg = if s.sync.rows.is_empty() {
            t(
                "Selecione as pastas acima e clique em Comparar para visualizar as diferenças.",
                "Select the folders above and click Compare to preview the differences.",
            )
        } else {
            t(
                "Nenhum item corresponde aos filtros de visualização atuais.",
                "No items match the current view filters.",
            )
        };
        col()
            .w_full()
            .grow(1.0)
            .center()
            .gap(8.0)
            .child(icon(&icons::TAB_SYNC, 28.0).color(k.c.text_faint))
            .child(text(empty_msg).font(k.ty.text_small).color(k.c.text_faint))
    } else {
        let mut list = col().key("sync-rows").w_full();
        let stripe_bg = if k.dark { k.alpha(k.c.white, 0.015) } else { k.alpha(k.c.black, 0.015) };
        let show_pair_prefix = s.sync.pairs.len() > 1;

        for (i, r) in visible.iter().enumerate() {
            let row_id = r.id;
            let bg = if i % 2 == 1 { stripe_bg } else { Color::TRANSPARENT };
            let rel_display = if show_pair_prefix {
                format!("[#{}] {}", r.pair_index + 1, r.rel_path)
            } else {
                r.rel_path.clone()
            };
            let glyph = if r.is_dir { &icons::FOLDER } else { &icons::FILE };

            let left_path = r.left.as_ref().map(|_| rel_display.clone()).unwrap_or_default();
            let left_size = r
                .left
                .as_ref()
                .and_then(|m| if r.is_dir { None } else { m.size.map(format_size) })
                .unwrap_or_default();
            let left_date = r
                .left
                .as_ref()
                .and_then(|m| m.modified.map(format_time))
                .unwrap_or_default();

            let right_path = r.right.as_ref().map(|_| rel_display.clone()).unwrap_or_default();
            let right_size = r
                .right
                .as_ref()
                .and_then(|m| if r.is_dir { None } else { m.size.map(format_size) })
                .unwrap_or_default();
            let right_date = r
                .right
                .as_ref()
                .and_then(|m| m.modified.map(format_time))
                .unwrap_or_default();

            let (act_bg, act_fg) = if !r.enabled && r.action != SyncAction::None {
                (k.alpha(k.c.text_base, 0.06), k.c.text_faint)
            } else {
                match r.action {
                    SyncAction::CopyRight => (k.alpha(k.c.text_accent, if k.dark { 0.22 } else { 0.12 }), k.c.text_accent),
                    SyncAction::CopyLeft => (k.alpha(k.c.success.fg, if k.dark { 0.22 } else { 0.12 }), k.c.success.fg),
                    SyncAction::DeleteRight | SyncAction::DeleteLeft => {
                        (k.alpha(k.c.danger.fg, if k.dark { 0.22 } else { 0.12 }), k.c.danger.fg)
                    }
                    SyncAction::None => (Color::TRANSPARENT, k.c.text_faint),
                }
            };

            let action_label = if !r.enabled && r.action != SyncAction::None {
                t("Ignorar", "Skip")
            } else {
                r.action.label()
            };

            let action_pill = row()
                .key(format!("sync-act-{row_id}"))
                .w(88.0)
                .h(20.0)
                .px(6.0)
                .center()
                .radius(k.radius.sm)
                .fill(act_bg)
                .cursor(if r.action != SyncAction::None && !s.sync.is_busy() {
                    Cursor::Pointer
                } else {
                    Cursor::Default
                })
                .help(r.action.long_label())
                .on_press(move |s: &mut State| sync::toggle_row(s, row_id))
                .child(text(action_label).font(k.ty.caption()).color(act_fg).truncate());

            let status_color = match &r.status {
                RowStatus::Pending => k.c.text_muted,
                RowStatus::Running => k.c.text_accent,
                RowStatus::Done => k.c.success.fg,
                RowStatus::Skipped => k.c.text_faint,
                RowStatus::Failed(_) => k.c.danger.fg,
            };

            let left_cell = row()
                .grow(1.0)
                .min_w(0.0)
                .gap(6.0)
                .center_y()
                .child(if r.left.is_some() {
                    icon(glyph, 14.0)
                } else {
                    el().w(14.0)
                })
                .child(
                    text(left_path)
                        .font(k.ty.text_small)
                        .color(if r.enabled || r.action == SyncAction::None { k.c.text_base } else { k.c.text_faint })
                        .truncate()
                        .min_w(0.0)
                        .grow(1.0),
                );

            let right_cell = row()
                .grow(1.0)
                .min_w(0.0)
                .gap(6.0)
                .center_y()
                .child(if r.right.is_some() {
                    icon(glyph, 14.0)
                } else {
                    el().w(14.0)
                })
                .child(
                    text(right_path)
                        .font(k.ty.text_small)
                        .color(if r.enabled || r.action == SyncAction::None { k.c.text_base } else { k.c.text_faint })
                        .truncate()
                        .min_w(0.0)
                        .grow(1.0),
                );

            let item_row = row()
                .key(format!("sync-row-{row_id}"))
                .w_full()
                .h(26.0)
                .px(14.0)
                .gap(8.0)
                .center_y()
                .fill(bg)
                .hover(k.c.hover)
                .child(left_cell)
                .child(text(left_size).font(k.ty.caption()).color(k.c.text_muted).w(74.0).truncate())
                .child(text(left_date).font(k.ty.caption()).color(k.c.text_muted).w(116.0).truncate())
                .child(action_pill)
                .child(right_cell)
                .child(text(right_size).font(k.ty.caption()).color(k.c.text_muted).w(74.0).truncate())
                .child(text(right_date).font(k.ty.caption()).color(k.c.text_muted).w(116.0).truncate())
                .child(text(r.status.label()).font(k.ty.caption()).color(status_color).w(120.0).truncate());

            list = list.child(item_row);
        }

        list.key("sync-table-scroll")
            .scroll(ScrollSpec { stick_bottom: s.sync.auto_scroll && s.sync.phase == SyncPhase::Running })
            .scroll_bar(k)
            .w_full()
            .grow(1.0)
            .min_h(0.0)
    };

    col()
        .key("sync-table")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(header)
        .child(body)
}

fn sync_footer(s: &State, k: &Kit, surface_bg: Color) -> El {
    let mut copy_r = 0usize;
    let mut copy_l = 0usize;
    let mut del_r = 0usize;
    let mut del_l = 0usize;
    let mut same = 0usize;
    for r in &s.sync.rows {
        if !r.enabled && r.action != SyncAction::None {
            continue;
        }
        match r.action {
            SyncAction::CopyRight => copy_r += 1,
            SyncAction::CopyLeft => copy_l += 1,
            SyncAction::DeleteRight => del_r += 1,
            SyncAction::DeleteLeft => del_l += 1,
            SyncAction::None => same += 1,
        }
    }

    let summary = tr!(
        "Copiar →: {copy_r}   ← Copiar: {copy_l}   Excluir: {}   Iguais: {same}   Filtrados: {}",
        "Copy →: {copy_r}   ← Copy: {copy_l}   Delete: {}   Same: {same}   Filtered: {}",
        del_r + del_l,
        s.sync.filtered_out_count
    );

    row()
        .key("sync-footer")
        .w_full()
        .h(30.0)
        .px(14.0)
        .gap(12.0)
        .center_y()
        .fill(surface_bg)
        .border_sides([1.0, 0.0, 0.0, 0.0], k.c.border_weak)
        .child(
            text(if s.sync.progress_text.is_empty() {
                t("Pronto", "Ready").into()
            } else {
                s.sync.progress_text.clone()
            })
            .font(k.ty.caption())
            .color(k.c.text_muted),
        )
        .child(el().grow(1.0))
        .child(text(summary).font(k.ty.caption()).color(k.c.text_faint))
}

// ---------------------------------------------------------------------------------------------
// Sincronizar - Opções dialog (matches reference image 2)

pub fn options_dialog(_s: &State, k: &Kit, opt: &OptionsDialog, layer: &mut Vec<El>) {
    let draft = &opt.draft;
    let is_custom = draft.mode == SyncMode::Custom;
    let rules = draft.effective_rules();

    // Left sidebar with the 5 sync modes
    let sidebar_bg = if k.dark { k.alpha(k.c.white, 0.018) } else { k.alpha(k.c.black, 0.022) };
    let mut nav = col()
        .key("sync-opt-sidebar")
        .w(190.0)
        .h_full()
        .padding(10.0)
        .gap(4.0)
        .fill(sidebar_bg)
        .border_sides([0.0, 1.0, 0.0, 0.0], k.c.border_weak);

    for mode in SyncMode::ALL {
        let on = draft.mode == mode;
        let fill = if on {
            k.alpha(k.c.text_accent, if k.dark { 0.22 } else { 0.12 })
        } else {
            Color::TRANSPARENT
        };
        let border = if on { k.alpha(k.c.text_accent, 0.45) } else { Color::TRANSPARENT };
        let item = k
            .interactive(
                row()
                    .key(format!("sync-mode-{mode:?}"))
                    .role("tab")
                    .checked(on)
                    .focusable(true)
                    .focus_ring(2.0, 1.0, k.c.border_focus)
                    .w_full()
                    .h(36.0)
                    .px(12.0)
                    .center_y()
                    .radius(k.radius.md)
                    .fill(fill)
                    .border(1.0, border)
                    .cursor(Cursor::Pointer)
                    .child(
                        text(mode.label())
                            .font(if on { k.ty.text_small_medium } else { k.ty.text_small })
                            .color(if on { k.c.text_base } else { k.c.text_muted }),
                    ),
            )
            .on_press(move |s: &mut State| {
                if let Some(Dialog::SyncOptions(o)) = &mut s.dialog {
                    o.draft.mode = mode;
                    if let Some(preset) = mode.preset_rules() {
                        o.draft.rules = preset;
                    }
                    o.open_dropdown = None;
                }
            });
        nav = nav.child(item);
    }

    // Right panel: mode title, description, 4 rule rows, Opções (Filtro..., hash compare)
    let card_bg = if k.dark { k.alpha(k.c.white, 0.03) } else { k.alpha(k.c.black, 0.025) };

    let left_only_opts: Vec<&str> = LeftOnlyAction::ALL.iter().map(|a| a.label()).collect();
    let right_only_opts: Vec<&str> = RightOnlyAction::ALL.iter().map(|a| a.label()).collect();
    let newer_opts: Vec<&str> = NewerAction::ALL.iter().map(|a| a.label()).collect();

    let rule_row = |label: &str, control: El| {
        row()
            .w_full()
            .h(36.0)
            .gap(12.0)
            .center_y()
            .child(text(label.to_string()).font(k.ty.text_small).color(k.c.text_base).grow(1.0).min_w(0.0))
            .child(control)
    };

    let sel_left_only = Select::new("sync-rule-left-only", &left_only_opts)
        .width(210.0)
        .appearance(SelectAppearance::Field)
        .current(Some(rules.left_only.label()))
        .disabled(!is_custom)
        .open(opt.open_dropdown == Some(OptionsDropdown::LeftOnly))
        .on_toggle(|s: &mut State| toggle_opt_dd(s, OptionsDropdown::LeftOnly))
        .on_close(|s: &mut State| close_opt_dd(s, OptionsDropdown::LeftOnly))
        .on_choose(|s: &mut State, v, _| {
            if let Some(Dialog::SyncOptions(o)) = &mut s.dialog {
                if let Some(a) = LeftOnlyAction::from_label(v) {
                    o.draft.rules.left_only = a;
                }
                o.open_dropdown = None;
            }
        })
        .build(k, layer);

    let sel_right_only = Select::new("sync-rule-right-only", &right_only_opts)
        .width(210.0)
        .appearance(SelectAppearance::Field)
        .current(Some(rules.right_only.label()))
        .disabled(!is_custom)
        .open(opt.open_dropdown == Some(OptionsDropdown::RightOnly))
        .on_toggle(|s: &mut State| toggle_opt_dd(s, OptionsDropdown::RightOnly))
        .on_close(|s: &mut State| close_opt_dd(s, OptionsDropdown::RightOnly))
        .on_choose(|s: &mut State, v, _| {
            if let Some(Dialog::SyncOptions(o)) = &mut s.dialog {
                if let Some(a) = RightOnlyAction::from_label(v) {
                    o.draft.rules.right_only = a;
                }
                o.open_dropdown = None;
            }
        })
        .build(k, layer);

    let sel_left_newer = Select::new("sync-rule-left-newer", &newer_opts)
        .width(210.0)
        .appearance(SelectAppearance::Field)
        .current(Some(rules.left_newer.label()))
        .disabled(!is_custom)
        .open(opt.open_dropdown == Some(OptionsDropdown::LeftNewer))
        .on_toggle(|s: &mut State| toggle_opt_dd(s, OptionsDropdown::LeftNewer))
        .on_close(|s: &mut State| close_opt_dd(s, OptionsDropdown::LeftNewer))
        .on_choose(|s: &mut State, v, _| {
            if let Some(Dialog::SyncOptions(o)) = &mut s.dialog {
                if let Some(a) = NewerAction::from_label(v) {
                    o.draft.rules.left_newer = a;
                }
                o.open_dropdown = None;
            }
        })
        .build(k, layer);

    let sel_right_newer = Select::new("sync-rule-right-newer", &newer_opts)
        .width(210.0)
        .appearance(SelectAppearance::Field)
        .current(Some(rules.right_newer.label()))
        .disabled(!is_custom)
        .open(opt.open_dropdown == Some(OptionsDropdown::RightNewer))
        .on_toggle(|s: &mut State| toggle_opt_dd(s, OptionsDropdown::RightNewer))
        .on_close(|s: &mut State| close_opt_dd(s, OptionsDropdown::RightNewer))
        .on_choose(|s: &mut State, v, _| {
            if let Some(Dialog::SyncOptions(o)) = &mut s.dialog {
                if let Some(a) = NewerAction::from_label(v) {
                    o.draft.rules.right_newer = a;
                }
                o.open_dropdown = None;
            }
        })
        .build(k, layer);

    let rules_card = col()
        .w_full()
        .padding(14.0)
        .gap(8.0)
        .radius(k.radius.lg)
        .fill(card_bg)
        .border(1.0, k.c.border_weak)
        .child(rule_row(
            t("O elemento existe somente no lado esquerdo", "Item exists only on the left side"),
            sel_left_only,
        ))
        .child(rule_row(
            t("O elemento existe somente no lado direito", "Item exists only on the right side"),
            sel_right_only,
        ))
        .child(rule_row(
            t("O arquivo do lado esquerdo é mais novo", "Left file is newer"),
            sel_left_newer,
        ))
        .child(rule_row(
            t("O arquivo do lado direito é mais novo", "Right file is newer"),
            sel_right_newer,
        ));

    let filter_active = draft.filter.active_count();
    let filter_label = if filter_active > 0 {
        tr!("Filtro… ({filter_active})", "Filter… ({filter_active})")
    } else {
        t("Filtro…", "Filter…").into()
    };

    let hash_cb = Checkbox::new(draft.compare_hash)
        .label(t(
            "Compare o conteúdo do arquivo para verificar se os arquivos são os mesmos, quando for possível (comparação de hash)",
            "Compare file contents to verify if files are identical when possible (hash comparison)",
        ))
        .on_toggle(|s: &mut State| {
            if let Some(Dialog::SyncOptions(o)) = &mut s.dialog {
                o.draft.compare_hash = !o.draft.compare_hash;
            }
        })
        .build(k);

    let right = col()
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .padding(18.0)
        .gap(12.0)
        .child(
            col()
                .gap(3.0)
                .child(text(draft.mode.label()).font(k.ty.ui_medium).color(k.c.text_base))
                .child(text(draft.mode.description()).font(k.ty.text_small).color(k.c.text_muted).wrap()),
        )
        .child(rules_card)
        .child(text(t("Opções", "Options")).font(k.ty.text_small_medium).color(k.c.text_base).mt(4.0))
        .child(
            row()
                .gap(8.0)
                .center_y()
                .child(
                    Button::new(filter_label)
                        .variant(ButtonVariant::Neutral)
                        .on_press(sync::open_filter)
                        .build(k),
                ),
        )
        .child(hash_cb)
        .child(el().grow(1.0))
        .child(
            row()
                .w_full()
                .gap(8.0)
                .justify_end()
                .child(
                    Button::new(t("OK", "OK"))
                        .variant(ButtonVariant::Submit)
                        .on_press(|s: &mut State| {
                            if let Some(Dialog::SyncOptions(o)) = s.dialog.take() {
                                s.sync.options = o.draft;
                                s.sync.rows.clear();
                            }
                        })
                        .build(k),
                )
                .child(
                    Button::new(t("Cancelar", "Cancel"))
                        .variant(ButtonVariant::Neutral)
                        .on_press(|s: &mut State| s.dialog = None)
                        .build(k),
                ),
        );

    let body = row().w_full().h(440.0).child(nav).child(right);

    Modal::new("sync-options", t("Sincronizar - Opções", "Sync - Options"))
        .open(true)
        .child(body)
        .width(740.0)
        .fit()
        .on_close(|s: &mut State| s.dialog = None)
        .build(k, layer);
}

fn toggle_opt_dd(s: &mut State, dd: OptionsDropdown) {
    if let Some(Dialog::SyncOptions(o)) = &mut s.dialog {
        o.open_dropdown = if o.open_dropdown == Some(dd) { None } else { Some(dd) };
    }
}

fn close_opt_dd(s: &mut State, dd: OptionsDropdown) {
    if let Some(Dialog::SyncOptions(o)) = &mut s.dialog
        && o.open_dropdown == Some(dd)
    {
        o.open_dropdown = None;
    }
}

// ---------------------------------------------------------------------------------------------
// Filtro dialog (matches reference image 3)

pub fn filter_dialog(_s: &State, k: &Kit, f: &FilterDialog, layer: &mut Vec<El>) {
    let card_bg = if k.dark { k.alpha(k.c.white, 0.03) } else { k.alpha(k.c.black, 0.025) };

    // 1. Filtro de nome
    let exclude_area = TextArea::new(&f.exclude_edit)
        .width(None)
        .rows(5)
        .placeholder("*.ini\n*/thumbs.db\n/subfolder/*.txt")
        .on_edit(|s: &mut State| match &mut s.dialog {
            Some(Dialog::SyncFilter(fd)) => &mut fd.exclude_edit,
            _ => &mut s.name_edit,
        })
        .build(k);

    let include_area = TextArea::new(&f.include_edit)
        .width(None)
        .rows(5)
        .placeholder("*.jpg\n*.pdf")
        .on_edit(|s: &mut State| match &mut s.dialog {
            Some(Dialog::SyncFilter(fd)) => &mut fd.include_edit,
            _ => &mut s.name_edit,
        })
        .build(k);

    let name_card = col()
        .w_full()
        .padding(12.0)
        .gap(8.0)
        .radius(k.radius.lg)
        .fill(card_bg)
        .border(1.0, k.c.border_weak)
        .child(
            row()
                .w_full()
                .gap(12.0)
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(5.0)
                        .child(
                            text(t("Excluir arquivos com correspondência:", "Exclude files matching:"))
                                .font(k.ty.text_small)
                                .color(k.c.text_base),
                        )
                        .child(exclude_area),
                )
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(5.0)
                        .child(
                            text(t("Incluir somente arquivos que correspondam a:", "Include only files matching:"))
                                .font(k.ty.text_small)
                                .color(k.c.text_base),
                        )
                        .child(include_area),
                ),
        )
        .child(
            text(t(
                "Digite uma condição por linha — Exemplos: *.ini   */thumbs.db   /subfolder/*.txt",
                "Enter one condition per line — Examples: *.ini   */thumbs.db   /subfolder/*.txt",
            ))
            .font(k.ty.caption())
            .color(k.c.text_faint),
        );

    // 2. Filtro de data
    let mode_opts: Vec<&str> = DateFilterMode::ALL.iter().map(|m| m.label()).collect();
    let date_unit_opts: Vec<&str> = DateUnit::ALL.iter().map(|u| u.label()).collect();

    let older_mode_sel = Select::new("flt-older-mode", &mode_opts)
        .width(120.0)
        .appearance(SelectAppearance::Field)
        .current(Some(f.older_mode.label()))
        .disabled(!f.older_enabled)
        .open(f.open_dropdown == Some(FilterDropdown::OlderMode))
        .on_toggle(|s: &mut State| toggle_flt_dd(s, FilterDropdown::OlderMode))
        .on_close(|s: &mut State| close_flt_dd(s, FilterDropdown::OlderMode))
        .on_choose(|s: &mut State, v, _| {
            if let Some(Dialog::SyncFilter(fd)) = &mut s.dialog {
                if let Some(m) = DateFilterMode::from_label(v) {
                    fd.older_mode = m;
                    let txt = match m {
                        DateFilterMode::Period => fd.parent_options.filter.exclude_older.amount.to_string(),
                        DateFilterMode::Date => fd.parent_options.filter.exclude_older.date.clone(),
                    };
                    fd.older_val = rune::TextEdit::new(&txt);
                }
                fd.open_dropdown = None;
            }
        })
        .build(k, layer);

    let older_val_input = TextInput::new(&f.older_val)
        .width(Some(if f.older_mode == DateFilterMode::Date { 120.0 } else { 76.0 }))
        .placeholder(if f.older_mode == DateFilterMode::Date { "YYYY-MM-DD" } else { "7" })
        .disabled(!f.older_enabled)
        .on_edit(|s: &mut State| match &mut s.dialog {
            Some(Dialog::SyncFilter(fd)) => &mut fd.older_val,
            _ => &mut s.name_edit,
        })
        .build(k);

    let older_unit_sel = Select::new("flt-older-unit", &date_unit_opts)
        .width(110.0)
        .appearance(SelectAppearance::Field)
        .current(Some(f.older_unit.label()))
        .disabled(!f.older_enabled)
        .open(f.open_dropdown == Some(FilterDropdown::OlderUnit))
        .on_toggle(|s: &mut State| toggle_flt_dd(s, FilterDropdown::OlderUnit))
        .on_close(|s: &mut State| close_flt_dd(s, FilterDropdown::OlderUnit))
        .on_choose(|s: &mut State, v, _| {
            if let Some(Dialog::SyncFilter(fd)) = &mut s.dialog {
                if let Some(u) = DateUnit::from_label(v) {
                    fd.older_unit = u;
                }
                fd.open_dropdown = None;
            }
        })
        .build(k, layer);

    let newer_mode_sel = Select::new("flt-newer-mode", &mode_opts)
        .width(120.0)
        .appearance(SelectAppearance::Field)
        .current(Some(f.newer_mode.label()))
        .disabled(!f.newer_enabled)
        .open(f.open_dropdown == Some(FilterDropdown::NewerMode))
        .on_toggle(|s: &mut State| toggle_flt_dd(s, FilterDropdown::NewerMode))
        .on_close(|s: &mut State| close_flt_dd(s, FilterDropdown::NewerMode))
        .on_choose(|s: &mut State, v, _| {
            if let Some(Dialog::SyncFilter(fd)) = &mut s.dialog {
                if let Some(m) = DateFilterMode::from_label(v) {
                    fd.newer_mode = m;
                    let txt = match m {
                        DateFilterMode::Period => fd.parent_options.filter.exclude_newer.amount.to_string(),
                        DateFilterMode::Date => fd.parent_options.filter.exclude_newer.date.clone(),
                    };
                    fd.newer_val = rune::TextEdit::new(&txt);
                }
                fd.open_dropdown = None;
            }
        })
        .build(k, layer);

    let newer_val_input = TextInput::new(&f.newer_val)
        .width(Some(if f.newer_mode == DateFilterMode::Date { 120.0 } else { 76.0 }))
        .placeholder(if f.newer_mode == DateFilterMode::Date { "YYYY-MM-DD" } else { "7" })
        .disabled(!f.newer_enabled)
        .on_edit(|s: &mut State| match &mut s.dialog {
            Some(Dialog::SyncFilter(fd)) => &mut fd.newer_val,
            _ => &mut s.name_edit,
        })
        .build(k);

    let newer_unit_sel = Select::new("flt-newer-unit", &date_unit_opts)
        .width(110.0)
        .appearance(SelectAppearance::Field)
        .current(Some(f.newer_unit.label()))
        .disabled(!f.newer_enabled)
        .open(f.open_dropdown == Some(FilterDropdown::NewerUnit))
        .on_toggle(|s: &mut State| toggle_flt_dd(s, FilterDropdown::NewerUnit))
        .on_close(|s: &mut State| close_flt_dd(s, FilterDropdown::NewerUnit))
        .on_choose(|s: &mut State, v, _| {
            if let Some(Dialog::SyncFilter(fd)) = &mut s.dialog {
                if let Some(u) = DateUnit::from_label(v) {
                    fd.newer_unit = u;
                }
                fd.open_dropdown = None;
            }
        })
        .build(k, layer);

    let mut older_row = row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(
            row()
                .w(205.0)
                .child(
                    Checkbox::new(f.older_enabled)
                        .label(t("Excluir arquivos com mais de", "Exclude files older than"))
                        .on_toggle(|s: &mut State| {
                            if let Some(Dialog::SyncFilter(fd)) = &mut s.dialog {
                                fd.older_enabled = !fd.older_enabled;
                            }
                        })
                        .build(k),
                ),
        )
        .child(older_mode_sel)
        .child(older_val_input);
    if f.older_mode == DateFilterMode::Period {
        older_row = older_row.child(older_unit_sel);
    }

    let mut newer_row = row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(
            row()
                .w(205.0)
                .child(
                    Checkbox::new(f.newer_enabled)
                        .label(t("Excluir arquivos mais novos que", "Exclude files newer than"))
                        .on_toggle(|s: &mut State| {
                            if let Some(Dialog::SyncFilter(fd)) = &mut s.dialog {
                                fd.newer_enabled = !fd.newer_enabled;
                            }
                        })
                        .build(k),
                ),
        )
        .child(newer_mode_sel)
        .child(newer_val_input);
    if f.newer_mode == DateFilterMode::Period {
        newer_row = newer_row.child(newer_unit_sel);
    }

    let date_card = col()
        .w_full()
        .padding(12.0)
        .gap(8.0)
        .radius(k.radius.lg)
        .fill(card_bg)
        .border(1.0, k.c.border_weak)
        .child(older_row)
        .child(newer_row);

    // 3. Filtro de tamanho
    let size_unit_opts: Vec<&str> = SizeUnit::ALL.iter().map(|u| u.label()).collect();

    let smaller_val = TextInput::new(&f.smaller_val)
        .width(Some(96.0))
        .placeholder("100")
        .disabled(!f.smaller_enabled)
        .on_edit(|s: &mut State| match &mut s.dialog {
            Some(Dialog::SyncFilter(fd)) => &mut fd.smaller_val,
            _ => &mut s.name_edit,
        })
        .build(k);

    let smaller_unit = Select::new("flt-smaller-unit", &size_unit_opts)
        .width(96.0)
        .appearance(SelectAppearance::Field)
        .current(Some(f.smaller_unit.label()))
        .disabled(!f.smaller_enabled)
        .open(f.open_dropdown == Some(FilterDropdown::SmallerUnit))
        .on_toggle(|s: &mut State| toggle_flt_dd(s, FilterDropdown::SmallerUnit))
        .on_close(|s: &mut State| close_flt_dd(s, FilterDropdown::SmallerUnit))
        .on_choose(|s: &mut State, v, _| {
            if let Some(Dialog::SyncFilter(fd)) = &mut s.dialog {
                if let Some(u) = SizeUnit::from_label(v) {
                    fd.smaller_unit = u;
                }
                fd.open_dropdown = None;
            }
        })
        .build(k, layer);

    let larger_val = TextInput::new(&f.larger_val)
        .width(Some(96.0))
        .placeholder("500")
        .disabled(!f.larger_enabled)
        .on_edit(|s: &mut State| match &mut s.dialog {
            Some(Dialog::SyncFilter(fd)) => &mut fd.larger_val,
            _ => &mut s.name_edit,
        })
        .build(k);

    let larger_unit = Select::new("flt-larger-unit", &size_unit_opts)
        .width(96.0)
        .appearance(SelectAppearance::Field)
        .current(Some(f.larger_unit.label()))
        .disabled(!f.larger_enabled)
        .open(f.open_dropdown == Some(FilterDropdown::LargerUnit))
        .on_toggle(|s: &mut State| toggle_flt_dd(s, FilterDropdown::LargerUnit))
        .on_close(|s: &mut State| close_flt_dd(s, FilterDropdown::LargerUnit))
        .on_choose(|s: &mut State, v, _| {
            if let Some(Dialog::SyncFilter(fd)) = &mut s.dialog {
                if let Some(u) = SizeUnit::from_label(v) {
                    fd.larger_unit = u;
                }
                fd.open_dropdown = None;
            }
        })
        .build(k, layer);

    let size_card = col()
        .w_full()
        .padding(12.0)
        .gap(8.0)
        .radius(k.radius.lg)
        .fill(card_bg)
        .border(1.0, k.c.border_weak)
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(
                    row()
                        .w(205.0)
                        .child(
                            Checkbox::new(f.smaller_enabled)
                                .label(t("Excluir arquivos menores que", "Exclude files smaller than"))
                                .on_toggle(|s: &mut State| {
                                    if let Some(Dialog::SyncFilter(fd)) = &mut s.dialog {
                                        fd.smaller_enabled = !fd.smaller_enabled;
                                    }
                                })
                                .build(k),
                        ),
                )
                .child(smaller_val)
                .child(smaller_unit),
        )
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(
                    row()
                        .w(205.0)
                        .child(
                            Checkbox::new(f.larger_enabled)
                                .label(t("Excluir arquivos maiores que", "Exclude files larger than"))
                                .on_toggle(|s: &mut State| {
                                    if let Some(Dialog::SyncFilter(fd)) = &mut s.dialog {
                                        fd.larger_enabled = !fd.larger_enabled;
                                    }
                                })
                                .build(k),
                        ),
                )
                .child(larger_val)
                .child(larger_unit),
        );

    // 4. Bottom subfolder checkboxes
    let root_only_cb = Checkbox::new(f.root_only)
        .label(t(
            "Não sincronizar subpastas, somente a pasta raiz",
            "Do not synchronize subfolders, only the root folder",
        ))
        .on_toggle(|s: &mut State| {
            if let Some(Dialog::SyncFilter(fd)) = &mut s.dialog {
                fd.root_only = !fd.root_only;
            }
        })
        .build(k);

    let skip_filtered_cb = Checkbox::new(f.skip_filtered_dirs)
        .label(t(
            "Não ler e comparar pastas já filtradas",
            "Do not read and compare already filtered folders",
        ))
        .on_toggle(|s: &mut State| {
            if let Some(Dialog::SyncFilter(fd)) = &mut s.dialog {
                fd.skip_filtered_dirs = !fd.skip_filtered_dirs;
            }
        })
        .build(k);

    let body = col()
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(10.0)
        .child(text(t("Filtro de nome", "Name filter")).font(k.ty.text_small_medium).color(k.c.text_base))
        .child(name_card)
        .child(text(t("Filtro de data", "Date filter")).font(k.ty.text_small_medium).color(k.c.text_base))
        .child(date_card)
        .child(text(t("Filtro de tamanho", "Size filter")).font(k.ty.text_small_medium).color(k.c.text_base))
        .child(size_card)
        .child(col().gap(6.0).mt(2.0).child(root_only_cb).child(skip_filtered_cb))
        .child(
            row()
                .w_full()
                .gap(8.0)
                .mt(6.0)
                .justify_end()
                .child(
                    Button::new(t("OK", "OK"))
                        .variant(ButtonVariant::Submit)
                        .on_press(|s: &mut State| {
                            if let Some(Dialog::SyncFilter(fd)) = s.dialog.take() {
                                let (mut parent, filter) = fd.into_filter();
                                parent.filter = filter;
                                s.dialog = Some(Dialog::SyncOptions(Box::new(OptionsDialog {
                                    draft: parent,
                                    open_dropdown: None,
                                })));
                            }
                        })
                        .build(k),
                )
                .child(
                    Button::new(t("Cancelar", "Cancel"))
                        .variant(ButtonVariant::Neutral)
                        .on_press(|s: &mut State| {
                            if let Some(Dialog::SyncFilter(fd)) = s.dialog.take() {
                                s.dialog = Some(Dialog::SyncOptions(Box::new(OptionsDialog {
                                    draft: fd.parent_options,
                                    open_dropdown: None,
                                })));
                            }
                        })
                        .build(k),
                ),
        );

    Modal::new("sync-filter", t("Filtro", "Filter"))
        .open(true)
        .child(body)
        .width(620.0)
        .fit()
        .on_close(|s: &mut State| {
            if let Some(Dialog::SyncFilter(fd)) = s.dialog.take() {
                s.dialog = Some(Dialog::SyncOptions(Box::new(OptionsDialog {
                    draft: fd.parent_options,
                    open_dropdown: None,
                })));
            }
        })
        .build(k, layer);
}

fn toggle_flt_dd(s: &mut State, dd: FilterDropdown) {
    if let Some(Dialog::SyncFilter(fd)) = &mut s.dialog {
        fd.open_dropdown = if fd.open_dropdown == Some(dd) { None } else { Some(dd) };
    }
}

fn close_flt_dd(s: &mut State, dd: FilterDropdown) {
    if let Some(Dialog::SyncFilter(fd)) = &mut s.dialog
        && fd.open_dropdown == Some(dd)
    {
        fd.open_dropdown = None;
    }
}

// ---------------------------------------------------------------------------------------------
// Folder browser dialog (supports Local disks, Google Drive, and all connected Servers)

pub fn browse_dialog(s: &State, k: &Kit, b: &BrowseDialog, layer: &mut Vec<El>) {
    let sidebar_bg = if k.dark { k.alpha(k.c.white, 0.018) } else { k.alpha(k.c.black, 0.022) };
    let mut places = col()
        .key("sb-places")
        .w(210.0)
        .h_full()
        .padding(8.0)
        .gap(3.0)
        .fill(sidebar_bg)
        .border_sides([0.0, 1.0, 0.0, 0.0], k.c.border_weak)
        .scroll(ScrollSpec { stick_bottom: false });

    let place_btn = |key: String, label: String, glyph: &'static rune::Icon, loc: SyncLocation| {
        row()
            .key(key)
            .role("button")
            .w_full()
            .h(30.0)
            .px(8.0)
            .gap(8.0)
            .center_y()
            .radius(k.radius.sm)
            .hover(k.c.hover)
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| sync::browse_navigate(s, loc.clone()))
            .child(icon(glyph, 15.0))
            .child(text(label).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0).grow(1.0))
    };

    // Local drives
    places = places.child(
        text(t("Este Computador", "This PC"))
            .font(k.ty.caption())
            .color(k.c.text_faint)
            .px(8.0)
            .py(4.0),
    );
    for (idx, (drive_path, _)) in s.local_drives.iter().enumerate() {
        places = places.child(place_btn(
            format!("sb-disk-{idx}"),
            drive_path.display().to_string(),
            &icons::DISK,
            SyncLocation::Local(drive_path.clone()),
        ));
    }

    // Connected Google Drive accounts
    if !s.accounts.is_empty() {
        places = places.child(
            text("Google Drive")
                .font(k.ty.caption())
                .color(k.c.text_faint)
                .px(8.0)
                .pt(8.0)
                .pb(4.0),
        );
        for (idx, acc) in s.accounts.iter().enumerate() {
            places = places.child(place_btn(
                format!("sb-gd-{idx}"),
                acc.email.clone(),
                &icons::CLOUD,
                SyncLocation::Drive {
                    email: acc.email.clone(),
                    shared_drive_id: None,
                    shared_drive_name: None,
                    path: Vec::new(),
                },
            ));
        }
    }

    // Connected Servers / Cloud accounts
    if !s.servers.list.is_empty() {
        places = places.child(
            text(t("Contas e Servidores", "Accounts & Servers"))
                .font(k.ty.caption())
                .color(k.c.text_faint)
                .px(8.0)
                .pt(8.0)
                .pb(4.0),
        );
        for (idx, sv) in s.servers.list.iter().enumerate() {
            places = places.child(place_btn(
                format!("sb-sv-{idx}"),
                sv.name.clone(),
                &icons::GLOBE,
                SyncLocation::Server {
                    server_id: sv.id.clone(),
                    server_name: sv.name.clone(),
                    path: Vec::new(),
                },
            ));
        }
    }

    let path_bar = row()
        .w_full()
        .gap(6.0)
        .center_y()
        .child(
            k.interactive(
                row()
                    .key("sb-up")
                    .role("button")
                    .size(30.0, 30.0)
                    .center()
                    .radius(k.radius.md)
                    .hover(k.c.hover)
                    .cursor(Cursor::Pointer)
                    .child(icon(&icons::UP, 15.0).color(k.c.icon_muted)),
            )
            .on_press(sync::browse_up),
        )
        .child(
            TextInput::new(&b.path_input)
                .width(None)
                .placeholder(t("Digite um caminho local…", "Enter a local path…"))
                .on_edit(|s: &mut State| match &mut s.dialog {
                    Some(Dialog::SyncBrowse(bd)) => &mut bd.path_input,
                    _ => &mut s.name_edit,
                })
                .on_submit(|s: &mut State, _: &mut Tx| sync::browse_go_typed(s))
                .build(k)
                .grow(1.0)
                .min_w(0.0),
        );

    let list_body = if b.loading {
        col()
            .w_full()
            .grow(1.0)
            .center()
            .child(text(t("Carregando pastas…", "Loading folders…")).font(k.ty.text_small).color(k.c.text_muted))
    } else if let Some(err) = &b.error {
        col()
            .w_full()
            .grow(1.0)
            .center()
            .padding(12.0)
            .child(text(err.clone()).font(k.ty.text_small).color(k.c.danger.fg).wrap())
    } else if b.entries.is_empty() {
        col()
            .w_full()
            .grow(1.0)
            .center()
            .child(text(t("Nenhuma subpasta aqui.", "No subfolders here.")).font(k.ty.text_small).color(k.c.text_faint))
    } else {
        let mut list = col().key("sb-list").w_full().gap(2.0);
        for (i, entry) in b.entries.iter().enumerate() {
            let e_clone = entry.clone();
            list = list.child(
                row()
                    .key(format!("sb-ent-{i}"))
                    .role("button")
                    .w_full()
                    .h(28.0)
                    .px(8.0)
                    .gap(8.0)
                    .center_y()
                    .radius(k.radius.sm)
                    .hover(k.c.hover)
                    .cursor(Cursor::Pointer)
                    .on_press(move |s: &mut State| sync::browse_enter(s, &e_clone))
                    .child(icon(&icons::FOLDER, 15.0))
                    .child(text(entry.name.clone()).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0).grow(1.0)),
            );
        }
        list.scroll(ScrollSpec { stick_bottom: false }).w_full().grow(1.0).min_h(0.0)
    };

    let right = col()
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .padding(12.0)
        .gap(10.0)
        .child(path_bar)
        .child(
            el()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .padding(6.0)
                .radius(k.radius.md)
                .border(1.0, k.c.border_weak)
                .child(list_body),
        )
        .child(
            row()
                .w_full()
                .gap(8.0)
                .justify_end()
                .child(
                    Button::new(t("Selecionar esta pasta", "Select this folder"))
                        .variant(ButtonVariant::Submit)
                        .disabled(b.current.is_none() && b.path_input.text.trim().is_empty())
                        .on_press(sync::browse_confirm)
                        .build(k),
                )
                .child(
                    Button::new(t("Cancelar", "Cancel"))
                        .variant(ButtonVariant::Neutral)
                        .on_press(|s: &mut State| s.dialog = None)
                        .build(k),
                ),
        );

    let title = match b.side {
        Side::Left => t("Selecionar pasta (Esquerda)", "Select folder (Left)"),
        Side::Right => t("Selecionar pasta (Direita)", "Select folder (Right)"),
    };

    let body = row().w_full().h(420.0).child(places).child(right);
    Modal::new("sync-browse", title)
        .open(true)
        .child(body)
        .width(680.0)
        .fit()
        .on_close(|s: &mut State| s.dialog = None)
        .build(k, layer);
}

// ---------------------------------------------------------------------------------------------
// Save Sync profile dialog

pub fn save_dialog(_s: &State, k: &Kit, d: &SaveDialog, layer: &mut Vec<El>) {
    let mut body = col()
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(10.0)
        .child(
            text(t("Nome da sincronização:", "Sync name:"))
                .font(k.ty.text_small)
                .color(k.c.text_base),
        )
        .child(
            TextInput::new(&d.name_edit)
                .width(None)
                .placeholder(t("Ex.: Backup Documentos", "e.g. Documents Backup"))
                .on_edit(|s: &mut State| match &mut s.dialog {
                    Some(Dialog::SyncSave(sd)) => &mut sd.name_edit,
                    _ => &mut s.name_edit,
                })
                .on_submit(|s: &mut State, _: &mut Tx| sync::commit_save(s))
                .build(k),
        );

    if let Some(err) = &d.error {
        body = body.child(text(err.clone()).font(k.ty.caption()).color(k.c.danger.fg));
    }

    body = body.child(
        row()
            .w_full()
            .gap(8.0)
            .mt(4.0)
            .justify_end()
            .child(
                Button::new(t("Salvar", "Save"))
                    .variant(ButtonVariant::Submit)
                    .on_press(sync::commit_save)
                    .build(k),
            )
            .child(
                Button::new(t("Cancelar", "Cancel"))
                    .variant(ButtonVariant::Neutral)
                    .on_press(|s: &mut State| s.dialog = None)
                    .build(k),
            ),
    );

    Modal::new("sync-save", t("Salvar sincronização", "Save sync"))
        .open(true)
        .child(body)
        .width(420.0)
        .fit()
        .on_close(|s: &mut State| s.dialog = None)
        .build(k, layer);
}
