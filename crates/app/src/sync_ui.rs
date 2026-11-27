//! UI for the Sincronizar tab, Sync Options dialog, Filter dialog, Folder Browser dialog,
//! and Save Sync dialog.

use std::borrow::Cow;
use std::rc::Rc;

use rune::{Color, Cursor, Drag, El, GrowthSpec, Phase, ScrollSpec, Tx, col, el, icon, row, text};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, Checkbox, Dialog as Modal, Kit, Placement, Select, SelectAppearance,
    TextArea, TextInput, popover, scrim,
};

use openexplorer_i18n::{t, tr};

use crate::actions::Dialog;
use crate::icons;
use crate::layout;
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
    let action_select = Pick::new("sync-action-filter", &action_opts)
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
    let status_select = Pick::new("sync-status-filter", &status_opts)
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

/// A choice among a few options: closed, it is the kit's select field; open, the options come
/// in a menu like the app's other menus, under the field.
///
/// The kit's own listbox is not used: on this screen it opened 50 points wide with every
/// option's text cut to nothing (a small empty box), and inside a dialog it opened under the
/// dialog. `build` pushes the menu into `layer`, so a dialog gives it a list of its own and
/// appends that after the dialog itself.
struct Pick {
    key: String,
    options: Vec<String>,
    current: Option<String>,
    width: f32,
    appearance: SelectAppearance,
    disabled: bool,
    open: bool,
    toggle: Option<Pressed>,
    close: Option<Pressed>,
    choose: Option<Chosen>,
}

/// What a press on a [`Pick`]'s field, or outside its menu, does.
type Pressed = Rc<dyn Fn(&mut State)>;
/// What choosing an option of a [`Pick`] does, given the option.
type Chosen = Rc<dyn Fn(&mut State, &str, &mut Tx)>;

impl Pick {
    fn new(key: &str, options: &[&str]) -> Pick {
        Pick {
            key: key.to_string(),
            options: options.iter().map(|o| o.to_string()).collect(),
            current: None,
            width: 280.0,
            appearance: SelectAppearance::Inline,
            disabled: false,
            open: false,
            toggle: None,
            close: None,
            choose: None,
        }
    }
    fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }
    fn appearance(mut self, appearance: SelectAppearance) -> Self {
        self.appearance = appearance;
        self
    }
    fn current(mut self, current: Option<&str>) -> Self {
        self.current = current.map(str::to_string);
        self
    }
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    fn open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }
    fn on_toggle(mut self, f: impl Fn(&mut State) + 'static) -> Self {
        self.toggle = Some(Rc::new(f));
        self
    }
    fn on_close(mut self, f: impl Fn(&mut State) + 'static) -> Self {
        self.close = Some(Rc::new(f));
        self
    }
    fn on_choose(mut self, f: impl Fn(&mut State, &str, &mut Tx) + 'static) -> Self {
        self.choose = Some(Rc::new(f));
        self
    }

    fn build(self, k: &Kit, layer: &mut Vec<El>) -> El {
        let mut field = Select::new(self.key.clone(), &self.options)
            .width(self.width)
            .appearance(self.appearance)
            .current(self.current.as_deref())
            .disabled(self.disabled)
            .open(self.open);
        if let Some(toggle) = self.toggle {
            field = field.on_toggle(move |s: &mut State| toggle(s));
        }
        let field = field.trigger(k);
        if !self.open || self.disabled {
            return field;
        }
        if let Some(close) = self.close {
            layer.push(scrim(format!("{}-scrim", self.key), move |s: &mut State| close(s)));
        }
        // As wide as its longest option, and no narrower than a field it hangs from.
        let least = if self.appearance == SelectAppearance::Field { self.width } else { 180.0 };
        let mut menu = col().key(format!("{}-menu", self.key)).role("listbox").min_w(least).padding(4.0).gap(2.0);
        for (i, option) in self.options.iter().enumerate() {
            let chosen = self.current.as_deref() == Some(option.as_str());
            let mut item = row()
                .key(format!("{}-option{i}", self.key))
                .role("option")
                .label(option.clone())
                .w_full()
                .h(28.0)
                .pl(10.0)
                .pr(6.0)
                .gap(8.0)
                .center_y()
                .radius(k.radius.sm)
                .fill(if chosen { k.alpha(k.c.text_accent, if k.dark { 0.20 } else { 0.11 }) } else { Color::TRANSPARENT })
                .hover(k.c.hover)
                .cursor(Cursor::Pointer)
                .child(text(option.clone()).font(k.ty.text_small).color(k.c.text_base))
                .child(el().grow(1.0))
                .child(el().size(16.0, 16.0).center().child(icon(k.glyph.check_small, 14.0).color(k.c.icon_base).opacity(if chosen { 1.0 } else { 0.0 })));
            if let Some(choose) = &self.choose {
                let (choose, option) = (choose.clone(), option.clone());
                item = item.on_press_tx(move |s: &mut State, tx: &mut Tx| choose(s, &option, tx));
            }
            menu = menu.child(item);
        }
        let surface = menu
            .radius(k.radius.md)
            .fill(if k.dark { rune::oklch(0.22, 0.008, 255.0) } else { k.c.bg_base })
            .border(1.0, k.c.border_weak)
            .box_shadow(rune::Shadow { y: 6.0, blur: 18.0, color: k.dark_a(0.32), ..Default::default() });
        layer.push(popover::anchored(k, surface, &format!("{}-trigger", self.key), Placement::BottomStart, 4.0));
        field
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

const TABLE_ROW_H: f32 = 26.0;
const TABLE_HEADER_H: f32 = 28.0;
/// The room before the table's first column.
const TABLE_PAD: f32 = 8.0;
/// How many rows the table's list takes as one: enough that a million rows are a list short
/// to index again when a filter changes its length, few enough that a screen is a few blocks.
const TABLE_BLOCK: usize = 32;

/// What each of the table's columns is headed, at its place in [`layout::SYNC_COLS`].
const TABLE_LABELS: [(&str, &str); 8] = [
    ("Caminho (esquerda)", "Path (left)"),
    ("Tamanho", "Size"),
    ("Data", "Date"),
    ("Ação", "Action"),
    ("Caminho (direita)", "Path (right)"),
    ("Tamanho", "Size"),
    ("Data", "Date"),
    ("Estado", "Status"),
];

/// The table's columns from left to right: which column stands at each place, and how wide
/// each is. The widths are dragged by the grips between the headers and the order by the
/// headers themselves; both are kept with the window's other sizes.
#[derive(Clone, Copy)]
struct TableCols {
    order: [u8; 8],
    widths: [f32; 8],
}

impl TableCols {
    fn of(s: &State) -> TableCols {
        TableCols { order: s.layout.sync_order, widths: s.layout.sync_cols }
    }

    /// Each place's column and its width, from left to right.
    fn places(&self) -> impl Iterator<Item = (usize, f32)> + '_ {
        self.order.iter().map(|&col| (col as usize, self.widths[col as usize]))
    }

    fn total(&self) -> f32 {
        TABLE_PAD + self.widths.iter().sum::<f32>()
    }

    /// Where the column at `place` starts along the table; with 8, where the last one ends.
    fn start(&self, place: usize) -> f32 {
        TABLE_PAD + self.places().take(place).map(|(_, w)| w).sum::<f32>()
    }

    /// The place the point `x` along the table falls in: the first before them all, the last
    /// after.
    fn place_at(&self, x: f32) -> usize {
        (1..8).take_while(|&place| x >= self.start(place)).count()
    }
}

/// A header dragged sideways moves its column to the place it is let go over.
fn move_col(s: &mut State, d: &mut Drag, place: usize) {
    let cols = TableCols::of(s);
    let to = cols.place_at(cols.start(place) + d.x);
    match d.phase {
        Phase::Down => s.sync.col_drag = None,
        // A press that barely moves is no drag.
        Phase::Move if d.dx.abs() > 4.0 || s.sync.col_drag.is_some() => s.sync.col_drag = Some((place, to)),
        Phase::Move => {}
        Phase::Up => {
            if s.sync.col_drag.take().is_some() && !d.cancelled() && to != place {
                s.layout.sync_order = layout::moved_sync_col(cols.order, place, to);
                s.layout.save();
            }
        }
    }
}

/// The table's header: each column's name, a grip at its right edge to change its width, and
/// the name itself to drag the column to another place. `sticky`: it heads rows that scroll.
fn table_header(s: &State, k: &Kit, cols: TableCols, sticky: bool) -> El {
    let mut header = row()
        .key("sync-table-header")
        .w_full()
        .h(TABLE_HEADER_H)
        .pl(TABLE_PAD)
        .fill(k.c.bg_base)
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak);
    if sticky {
        header = header.sticky_top(0.0);
    }
    let help = t(
        "Arraste para mudar a coluna de lugar; arraste a borda direita para mudar a largura",
        "Drag to move the column; drag its right edge to change its width",
    );
    let mut grips = Vec::new();
    for (place, (col, w)) in cols.places().enumerate() {
        let (pt, en) = TABLE_LABELS[col];
        let moving = s.sync.col_drag.is_some_and(|(from, _)| from == place);
        header = header.child(
            row()
                .key(format!("sync-col-{col}"))
                .role("columnheader")
                .label(t(pt, en))
                .help(help)
                .w(w)
                .h_full()
                .px(6.0)
                .center_y()
                .clip_soft(0.0)
                .fill(if moving { k.c.hover } else { Color::TRANSPARENT })
                .hover(k.c.hover)
                .border_sides([0.0, 1.0, 0.0, 0.0], k.c.border_weak)
                .on_drag(move |s: &mut State, d: &mut Drag, _: &mut Tx| move_col(s, d, place))
                .child(text(t(pt, en)).font(k.ty.caption()).color(k.c.text_muted).truncate().shrink().min_w(0.0)),
        );
        grips.push(crate::ui::grip(
            k,
            &format!("sync-grip-{col}"),
            cols.start(place + 1),
            TABLE_HEADER_H,
            move |s| &mut s.layout.sync_cols[col],
            layout::SYNC_COLS[col],
            move |w| layout::clamp_sync_col(col, w),
        ));
    }
    // Where the column being dragged would land.
    if let Some((from, to)) = s.sync.col_drag
        && from != to
    {
        let at = cols.start(if to < from { to } else { to + 1 });
        header = header.child(el().key("sync-col-mark").absolute(at - 1.0, 0.0).w(2.0).h(TABLE_HEADER_H).fill(k.c.border_focus));
    }
    header.children(grips)
}

fn comparison_table(s: &State, k: &Kit) -> El {
    let cols = TableCols::of(s);
    let shown = s.sync.shown();
    let table = col().key("sync-table").w_full().grow(1.0).min_h(0.0);
    let message = |size: f32, ink: Color, said: String| {
        col().w_full().grow(1.0).center().gap(8.0).child(icon(&icons::TAB_SYNC, size).color(ink)).child(text(said).font(k.ty.text_small).color(k.c.text_muted))
    };

    if s.sync.phase == SyncPhase::Comparing {
        return table.child(table_header(s, k, cols, false)).child(message(26.0, k.c.text_accent, s.sync.progress_text.clone()));
    }
    if shown.rows.is_empty() {
        let said = if s.sync.rows.is_empty() {
            t(
                "Selecione as pastas acima e clique em Comparar para visualizar as diferenças.",
                "Select the folders above and click Compare to preview the differences.",
            )
        } else {
            t("Nenhum item corresponde aos filtros de visualização atuais.", "No items match the current view filters.")
        };
        return table.child(table_header(s, k, cols, false)).child(message(28.0, k.c.text_faint, said.into()));
    }

    // Only the rows in view are built: a comparison has tens of thousands, and building
    // them all took 1.4 GB and half a second a frame at 20,000.
    //
    // The list is of blocks of rows, each named by its place alone, so a filter or a search
    // changes what the blocks hold and not the list: with a list of the rows themselves,
    // every change of filter had the list read and index a key for each row again, 78 ms
    // of a million. The rows past the last whole block follow the list.
    let kit = k.clone();
    let in_blocks = shown.rows.clone();
    let blocks = shown.rows.len() / TABLE_BLOCK;
    let block_h = TABLE_BLOCK as f32 * TABLE_ROW_H;
    let list = col()
        .key("sync-rows")
        .w_full()
        .growth(GrowthSpec::default())
        .virtual_rows(
            blocks,
            block_h,
            |_: &State, i| Cow::Owned(format!("sync-rows-{i}")),
            move |s: &State, i| col().w_full().h(block_h).children(table_rows(s, &kit, &in_blocks[i * TABLE_BLOCK..(i + 1) * TABLE_BLOCK], i * TABLE_BLOCK)),
        )
        .virtual_rows_version(0u8)
        // A viewport kept built either side of the one in view, where the list's own two
        // had a third more rows built again on every frame (each status of a run is one).
        .virtual_overscan(1.0, 0.5);
    let rest = col().key("sync-rows-rest").w_full().children(table_rows(s, k, &shown.rows[blocks * TABLE_BLOCK..], blocks * TABLE_BLOCK));

    // One viewport scrolls both ways, as the file list's: sideways when the columns are wider
    // than the window, with the header stuck to the top and a bar along the bottom then. The
    // bar at the side runs below the header, and the columns leave it its strip.
    let bar = crate::ui::bar_style(k).track_offset(TABLE_HEADER_H, 0.0);
    let content = col().key("sync-table-content").w_full().min_w(cols.total() + bar.strip()).child(table_header(s, k, cols, true)).child(list).child(rest);
    let viewport = col()
        .key(TABLE_VIEW)
        .scroll(ScrollSpec { stick_bottom: s.sync.auto_scroll && s.sync.phase == SyncPhase::Running })
        .scroll_xy()
        .w_full()
        .h_full()
        .child(content);
    table.child(col().w_full().grow(1.0).min_h(0.0).child(crate::ui::with_scroll_bars(k, TABLE_VIEW, viewport, bar, true)))
}

/// The table's scroll view.
const TABLE_VIEW: &str = "sync-table-scroll";

/// The rows `rows` of the comparison, the first of them the `first`th of those shown.
fn table_rows(s: &State, k: &Kit, rows: &[u32], first: usize) -> impl Iterator<Item = El> {
    let cols = TableCols::of(s);
    rows.iter().enumerate().filter_map(move |(i, &row)| Some(table_row(s, k, cols, s.sync.rows.get(row as usize)?, first + i)))
}

/// One row of the comparison, the `i`th of those shown, its cells in the columns' order.
fn table_row(s: &State, k: &Kit, cols: TableCols, r: &sync::CompareRow, i: usize) -> El {
    let row_id = r.id;
    let stripe = if k.dark { k.alpha(k.c.white, 0.015) } else { k.alpha(k.c.black, 0.015) };
    let off = !r.enabled && r.action != SyncAction::None;
    // Left out by the filter: shown faint, with no action to read as "the same".
    let filtered = r.status == RowStatus::Filtered;
    let ink = if off || filtered { k.c.text_faint } else { k.c.text_base };
    let glyph = if r.is_dir { &icons::FOLDER } else { &icons::FILE };

    // Each cell is one element as wide as its column less the room either side of what it
    // shows (only a path, with its icon, is a row of two): a row of the table is built again
    // on every frame it is in view, so it is kept to few elements, and none that clips.
    const ROOM: f32 = 6.0;
    // A side's path (with the pair it is of, when there are several), its size and its date.
    let path = |side: &Option<sync::SideMeta>, w: f32| {
        let shown = match side {
            Some(_) if s.sync.pairs.len() > 1 => format!("[#{}] {}", r.pair_index + 1, r.rel_path),
            Some(_) => r.rel_path.clone(),
            None => String::new(),
        };
        row()
            .w(w)
            .px(ROOM)
            .gap(6.0)
            .center_y()
            .child(if side.is_some() { icon(glyph, 14.0) } else { el().w(14.0) })
            .child(text(shown).font(k.ty.text_small).color(ink).truncate().shrink().min_w(0.0))
    };
    let small = |shown: String, ink: Color, w: f32| text(shown).font(k.ty.caption()).color(ink).truncate().w(w - 2.0 * ROOM).mx(ROOM);
    let size = |side: &Option<sync::SideMeta>, w: f32| small(side.as_ref().filter(|_| !r.is_dir).and_then(|m| m.size.map(format_size)).unwrap_or_default(), k.c.text_muted, w);
    let date = |side: &Option<sync::SideMeta>, w: f32| small(side.as_ref().and_then(|m| m.modified.map(format_time)).unwrap_or_default(), k.c.text_muted, w);

    let action = |w: f32| {
        let (fill, ink) = match r.action {
            _ if off => (k.alpha(k.c.text_base, 0.06), k.c.text_faint),
            SyncAction::CopyRight => (k.alpha(k.c.text_accent, if k.dark { 0.22 } else { 0.12 }), k.c.text_accent),
            SyncAction::CopyLeft => (k.alpha(k.c.success.fg, if k.dark { 0.22 } else { 0.12 }), k.c.success.fg),
            SyncAction::DeleteRight | SyncAction::DeleteLeft => (k.alpha(k.c.danger.fg, if k.dark { 0.22 } else { 0.12 }), k.c.danger.fg),
            SyncAction::None => (Color::TRANSPARENT, k.c.text_faint),
        };
        row()
            .key(format!("sync-act-{row_id}"))
            .w(w - 2.0 * ROOM)
            .mx(ROOM)
            .h(20.0)
            .px(6.0)
            .center()
            .radius(k.radius.sm)
            .fill(fill)
            .cursor(if r.action != SyncAction::None && !s.sync.is_busy() { Cursor::Pointer } else { Cursor::Default })
            .help(r.action.long_label())
            .on_press(move |s: &mut State| sync::toggle_row(s, row_id))
            .child(text(if filtered { "" } else if off { t("Ignorar", "Skip") } else { r.action.label() }).font(k.ty.caption()).color(ink).truncate().shrink().min_w(0.0))
    };
    let status = |w: f32| {
        let ink = match &r.status {
            RowStatus::Pending => k.c.text_muted,
            RowStatus::Running => k.c.text_accent,
            RowStatus::Done => k.c.success.fg,
            RowStatus::Skipped | RowStatus::Idle | RowStatus::Filtered => k.c.text_faint,
            RowStatus::Failed(_) => k.c.danger.fg,
        };
        small(r.status.label(), ink, w)
    };
    const CELLS: [&str; 8] = ["c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7"];

    let mut line = row()
        .key(format!("sync-row-{row_id}"))
        .w_full()
        .h(TABLE_ROW_H)
        .pl(TABLE_PAD)
        .center_y()
        .fill(if i % 2 == 1 { stripe } else { Color::TRANSPARENT })
        .hover(k.c.hover);
    for (col, w) in cols.places() {
        let cell = match col {
            0 => path(&r.left, w),
            1 => size(&r.left, w),
            2 => date(&r.left, w),
            3 => action(w),
            4 => path(&r.right, w),
            5 => size(&r.right, w),
            6 => date(&r.right, w),
            _ => status(w),
        };
        line = line.child(cell.key(CELLS[col]));
    }
    line
}

fn sync_footer(s: &State, k: &Kit, surface_bg: Color) -> El {
    let shown = s.sync.counts();
    let (copy_r, copy_l, same) = (shown.copy_right, shown.copy_left, shown.same);

    let summary = tr!(
        "Copiar →: {copy_r}   ← Copiar: {copy_l}   Excluir: {}   Iguais: {same}   Filtrados: {}",
        "Copy →: {copy_r}   ← Copy: {copy_l}   Delete: {}   Same: {same}   Filtered: {}",
        shown.delete,
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
    let mut menus = Vec::new();
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
    let right_newer_opts: Vec<&str> = NewerAction::FOR_RIGHT.iter().map(|a| a.label()).collect();

    let rule_row = |label: &str, control: El| {
        row()
            .w_full()
            .h(36.0)
            .gap(12.0)
            .center_y()
            .child(text(label.to_string()).font(k.ty.text_small).color(k.c.text_base).grow(1.0).min_w(0.0))
            .child(control)
    };

    let sel_left_only = Pick::new("sync-rule-left-only", &left_only_opts)
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
        .build(k, &mut menus);

    let sel_right_only = Pick::new("sync-rule-right-only", &right_only_opts)
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
        .build(k, &mut menus);

    let sel_left_newer = Pick::new("sync-rule-left-newer", &newer_opts)
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
        .build(k, &mut menus);

    let sel_right_newer = Pick::new("sync-rule-right-newer", &right_newer_opts)
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
        .build(k, &mut menus);

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

    // A short label and the rest under it, where the text wraps: as one label it ran off the
    // dialog and was cut.
    let hash_cb = Checkbox::new(draft.compare_hash)
        .label(t("Comparar o conteúdo dos arquivos", "Compare the content of files"))
        .description(t(
            "Quando for possível, verifica pelo conteúdo (hash) se os arquivos são os mesmos, mesmo que as datas sejam diferentes. É mais lento: lê os arquivos dos dois lados.",
            "When possible, checks by content (hash) whether the files are the same, even when their dates differ. It is slower: it reads the files of both sides.",
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
                                s.sync.clear_rows();
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
    // The menus of the choices above go over the dialog, so after it.
    layer.append(&mut menus);
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
    let mut menus = Vec::new();
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

    let older_mode_sel = Pick::new("flt-older-mode", &mode_opts)
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
        .build(k, &mut menus);

    let older_val_input = TextInput::new(&f.older_val)
        .width(Some(if f.older_mode == DateFilterMode::Date { 120.0 } else { 76.0 }))
        .placeholder(if f.older_mode == DateFilterMode::Date { "YYYY-MM-DD" } else { "7" })
        .disabled(!f.older_enabled)
        .on_edit(|s: &mut State| match &mut s.dialog {
            Some(Dialog::SyncFilter(fd)) => &mut fd.older_val,
            _ => &mut s.name_edit,
        })
        .build(k);

    let older_unit_sel = Pick::new("flt-older-unit", &date_unit_opts)
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
        .build(k, &mut menus);

    let newer_mode_sel = Pick::new("flt-newer-mode", &mode_opts)
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
        .build(k, &mut menus);

    let newer_val_input = TextInput::new(&f.newer_val)
        .width(Some(if f.newer_mode == DateFilterMode::Date { 120.0 } else { 76.0 }))
        .placeholder(if f.newer_mode == DateFilterMode::Date { "YYYY-MM-DD" } else { "7" })
        .disabled(!f.newer_enabled)
        .on_edit(|s: &mut State| match &mut s.dialog {
            Some(Dialog::SyncFilter(fd)) => &mut fd.newer_val,
            _ => &mut s.name_edit,
        })
        .build(k);

    let newer_unit_sel = Pick::new("flt-newer-unit", &date_unit_opts)
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
        .build(k, &mut menus);

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

    let smaller_unit = Pick::new("flt-smaller-unit", &size_unit_opts)
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
        .build(k, &mut menus);

    let larger_val = TextInput::new(&f.larger_val)
        .width(Some(96.0))
        .placeholder("500")
        .disabled(!f.larger_enabled)
        .on_edit(|s: &mut State| match &mut s.dialog {
            Some(Dialog::SyncFilter(fd)) => &mut fd.larger_val,
            _ => &mut s.name_edit,
        })
        .build(k);

    let larger_unit = Pick::new("flt-larger-unit", &size_unit_opts)
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
        .build(k, &mut menus);

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
    // The menus of the choices above go over the dialog, so after it.
    layer.append(&mut menus);
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant, UNIX_EPOCH};

    use crate::sync::{CompareRow, RowStatus, SideMeta, SyncAction};
    use crate::{MainTab, State};

    /// The memory the process holds now and the most it held, in MB.
    #[cfg(windows)]
    fn memory_mb() -> (f64, f64) {
        #[repr(C)]
        struct Counters {
            cb: u32,
            page_faults: u32,
            peak_working_set: usize,
            working_set: usize,
            rest: [usize; 6],
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> isize;
            fn K32GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
        }
        let mut c = Counters { cb: size_of::<Counters>() as u32, page_faults: 0, peak_working_set: 0, working_set: 0, rest: [0; 6] };
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        (c.working_set as f64 / 1048576.0, c.peak_working_set as f64 / 1048576.0)
    }
    #[cfg(not(windows))]
    fn memory_mb() -> (f64, f64) {
        (0.0, 0.0)
    }

    fn rows(n: usize) -> Vec<CompareRow> {
        let side = |i: usize| SideMeta {
            id: format!("arquivo {:03}.bin", i % 100),
            size: Some(1024 + i as u64),
            modified: Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000 + i as u64)),
            md5: None,
            drive_file: None,
        };
        (0..n)
            .map(|i| {
                let action = [SyncAction::None, SyncAction::CopyRight, SyncAction::DeleteRight, SyncAction::CopyLeft][i % 4];
                CompareRow {
                    id: i,
                    pair_index: 0,
                    rel_path: format!("pasta {:02}/sub {:04}/arquivo {:03}.bin", i / 5000, i / 100, i % 100),
                    is_dir: false,
                    left: (action != SyncAction::CopyLeft).then(|| side(i)),
                    right: (action != SyncAction::CopyRight).then(|| side(i)),
                    action,
                    enabled: action != SyncAction::None,
                    status: RowStatus::Pending,
                }
            })
            .collect()
    }

    /// The menus of the table's filters and of the rules open with every option to read and
    /// to press, the rules' over their dialog; and the long option of that dialog wraps in it.
    #[test]
    fn the_menus_show_their_options() {
        use crate::actions::Dialog;
        use crate::sync::{ActionFilter, LeftOnlyAction, NewerAction, RightOnlyAction, StatusFilter, SyncMode};
        type Rt = rune::Runtime<State>;
        let mut s = crate::init();
        s.main_tab = MainTab::Sync;
        s.layout = crate::layout::Layout::default();
        s.sync.set_rows(rows(200));
        let mut rt = rune::Runtime::new(s, crate::ui::view, (1600.0, 1000.0), rune::Env::new(false, 1.0));
        rt.frame(0.016);
        let place = |rt: &Rt, key: &str| rt.elements().iter().position(|e| !e.exiting && e.key.as_deref() == Some(key)).unwrap_or_else(|| panic!("nothing keyed {key}"));
        let click = |rt: &mut Rt, key: &str| {
            let r = rt.elements()[place(rt, key)].rect;
            let (x, y) = (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0);
            rt.pointer_move(x, y);
            rt.pointer_down(x, y);
            rt.pointer_up(x, y);
            for _ in 0..4 {
                rt.frame(0.016);
            }
        };
        // What a menu offers: each option's text, whole and inside the menu.
        let offers = |rt: &Rt, menu: &str| -> Vec<String> {
            let all = rt.elements();
            let m = place(rt, menu);
            let (left, right) = (all[m].rect[0], all[m].rect[0] + all[m].rect[2]);
            all.iter()
                .filter(|e| e.text.is_some() && e.parent.and_then(|option| all[option].parent) == Some(m))
                .map(|e| {
                    // Wide enough for its letters: the shortest option is "B".
                    assert!(e.rect[2] > 5.0 && !e.text_cut && e.rect[0] >= left && e.rect[0] + e.rect[2] <= right, "{:?} at {:?} in a menu from {left} to {right}", e.text, e.rect);
                    e.text.clone().unwrap()
                })
                .collect()
        };
        let labels = |all: &[&str]| all.iter().map(|l| l.to_string()).collect::<Vec<_>>();

        click(&mut rt, "sync-action-filter-trigger");
        assert_eq!(offers(&rt, "sync-action-filter-menu"), labels(&ActionFilter::ALL.map(|f| f.label())));
        click(&mut rt, "sync-action-filter-option3");
        rt.update(|s, _| assert_eq!((s.sync.action_filter, s.sync.open_menu), (ActionFilter::DeleteRight, None)));
        click(&mut rt, "sync-status-filter-trigger");
        assert_eq!(offers(&rt, "sync-status-filter-menu"), labels(&StatusFilter::ALL.map(|f| f.label())));
        click(&mut rt, "sync-status-filter-option2");
        rt.update(|s, _| assert_eq!((s.sync.status_filter, s.sync.open_menu), (StatusFilter::Done, None)));

        // Personalizado: each rule's menu, over the dialog.
        rt.update(|s, _| {
            crate::sync::open_options(s);
            if let Some(Dialog::SyncOptions(o)) = &mut s.dialog {
                o.draft.mode = SyncMode::Custom;
            }
        });
        for _ in 0..4 {
            rt.frame(0.016);
        }
        let rules = [
            ("sync-rule-left-only", labels(&LeftOnlyAction::ALL.map(|a| a.label()))),
            ("sync-rule-right-only", labels(&RightOnlyAction::ALL.map(|a| a.label()))),
            ("sync-rule-left-newer", labels(&NewerAction::ALL.map(|a| a.label()))),
            ("sync-rule-right-newer", labels(&NewerAction::FOR_RIGHT.map(|a| a.label()))),
        ];
        for (key, wanted) in &rules {
            click(&mut rt, &format!("{key}-trigger"));
            assert_eq!(&offers(&rt, &format!("{key}-menu")), wanted, "{key}");
            assert!(place(&rt, &format!("{key}-menu")) > place(&rt, "sync-options-dialog"), "{key}'s menu is under the dialog");
            click(&mut rt, &format!("{key}-option2"));
        }
        rt.update(|s, _| {
            let Some(Dialog::SyncOptions(o)) = &s.dialog else { panic!("the dialog closed") };
            let r = o.draft.rules;
            assert_eq!((r.left_only, r.right_only, r.left_newer, r.right_newer), (LeftOnlyAction::ALL[2], RightOnlyAction::ALL[2], NewerAction::ALL[2], NewerAction::FOR_RIGHT[2]));
            assert_eq!(o.open_dropdown, None);
        });
        // The order the options come in: what copies first, what deletes last.
        assert_eq!(LeftOnlyAction::ALL, [LeftOnlyAction::CopyToRight, LeftOnlyAction::DoNothing, LeftOnlyAction::DeleteLeft]);
        assert_eq!(RightOnlyAction::ALL, [RightOnlyAction::CopyToLeft, RightOnlyAction::DoNothing, RightOnlyAction::DeleteRight]);

        // The option to compare by content reads whole: its text wraps inside the dialog.
        let all = rt.elements();
        let dialog = all[place(&rt, "sync-options-dialog")].rect;
        let said: Vec<_> = all.iter().filter(|e| e.key.as_deref() == Some("description") && e.text.is_some() && e.rect[0] > dialog[0]).collect();
        assert_eq!(said.len(), 1, "the option's description");
        assert!(!said[0].text_cut && said[0].rect[3] > 20.0 && said[0].rect[0] + said[0].rect[2] <= dialog[0] + dialog[2], "{:?} in {dialog:?}", said[0].rect);

        // Filtro, with every rule on: each of its six menus, over the dialog.
        rt.update(|s, _| {
            crate::sync::open_filter(s);
            let Some(Dialog::SyncFilter(f)) = &mut s.dialog else { panic!("no filter dialog") };
            (f.older_enabled, f.newer_enabled, f.smaller_enabled, f.larger_enabled) = (true, true, true, true);
        });
        for _ in 0..4 {
            rt.frame(0.016);
        }
        // The units before the modes: a rule by date, the mode's second option, has no unit.
        for (key, options) in [("flt-older-unit", 5), ("flt-newer-unit", 5), ("flt-smaller-unit", 5), ("flt-larger-unit", 5), ("flt-older-mode", 2), ("flt-newer-mode", 2)] {
            click(&mut rt, &format!("{key}-trigger"));
            assert_eq!(offers(&rt, &format!("{key}-menu")).len(), options, "{key}");
            assert!(place(&rt, &format!("{key}-menu")) > place(&rt, "sync-filter-dialog"), "{key}'s menu is under the dialog");
            click(&mut rt, &format!("{key}-option1"));
            rt.update(|s, _| {
                let Some(Dialog::SyncFilter(f)) = &s.dialog else { panic!("the filter dialog closed") };
                assert_eq!(f.open_dropdown, None, "{key}");
            });
        }
        rt.update(|s, _| {
            let Some(Dialog::SyncFilter(f)) = &s.dialog else { panic!("the filter dialog closed") };
            assert_eq!((f.older_mode, f.newer_mode), (crate::sync::DateFilterMode::ALL[1], crate::sync::DateFilterMode::ALL[1]));
            assert_eq!((f.smaller_unit, f.larger_unit), (crate::sync::SizeUnit::ALL[1], crate::sync::SizeUnit::ALL[1]));
        });
    }

    /// Texts that show nothing, or less than they say where they aren't meant to be cut short:
    /// what "black" boxes and cut options are made of. `within`: only under that key.
    fn unread(rt: &rune::Runtime<State>, within: &str) -> Vec<String> {
        let all = rt.elements();
        let under = |mut i: usize| loop {
            if all[i].key.as_deref() == Some(within) {
                return true;
            }
            match all[i].parent {
                Some(p) => i = p,
                None => return false,
            }
        };
        all.iter()
            .enumerate()
            .filter(|(i, e)| !e.exiting && e.text.as_deref().is_some_and(|t| !t.trim().is_empty()) && under(*i))
            .filter(|(_, e)| e.rect[2] < 1.0 || e.rect[3] < 1.0 || (e.text_cut && !e.truncate))
            .map(|(_, e)| format!("{:?} at {:?} (cut: {})", e.text, e.rect, e.text_cut))
            .collect()
    }

    /// Every text of the Sincronizar tab and of its dialogs can be read: none with no room at
    /// all, none cut that isn't a path or a cell made to be cut.
    #[test]
    fn the_texts_of_the_tab_and_its_dialogs_can_be_read() {
        use crate::actions::Dialog;
        use crate::sync::{SyncMode, ToolbarMenu};
        let mut s = crate::init();
        s.main_tab = MainTab::Sync;
        s.layout = crate::layout::Layout::default();
        let mut rt = rune::Runtime::new(s, crate::ui::view, (1600.0, 1000.0), rune::Env::new(false, 1.0));
        let settle = |rt: &mut rune::Runtime<State>| {
            for _ in 0..6 {
                rt.frame(0.016);
            }
        };
        let mut bad = Vec::new();
        let mut note = |what: &str, rt: &rune::Runtime<State>, within: &str| bad.extend(unread(rt, within).into_iter().map(|t| format!("{what}: {t}")));

        settle(&mut rt);
        note("the tab, no comparison", &rt, "sync-view");
        rt.update(|s, _| s.sync.set_rows(rows(300)));
        settle(&mut rt);
        note("the tab, compared", &rt, "sync-view");
        rt.update(|s, _| s.sync.open_menu = Some(ToolbarMenu::SavedList));
        settle(&mut rt);
        note("the saved list", &rt, "kit-root");
        rt.update(|s, _| s.sync.open_menu = None);
        for mode in SyncMode::ALL {
            rt.update(|s, _| {
                crate::sync::open_options(s);
                if let Some(Dialog::SyncOptions(o)) = &mut s.dialog {
                    o.draft.mode = mode;
                }
            });
            settle(&mut rt);
            note(&format!("the options, {mode:?}"), &rt, "sync-options-dialog");
        }
        rt.update(|s, _| {
            crate::sync::open_filter(s);
            if let Some(Dialog::SyncFilter(f)) = &mut s.dialog {
                (f.older_enabled, f.newer_enabled, f.smaller_enabled, f.larger_enabled) = (true, true, true, true);
            }
        });
        settle(&mut rt);
        note("the filter", &rt, "sync-filter-dialog");
        rt.update(|s, _| crate::sync::open_save(s));
        settle(&mut rt);
        note("save", &rt, "sync-save-dialog");
        assert!(bad.is_empty(), "{} texts can't be read:\n{}", bad.len(), bad.join("\n"));
    }

    /// Columns wider than the window scroll sideways under a bar, the header with them and
    /// stuck to the top; each column keeps the width it was given.
    #[test]
    fn wide_columns_scroll_sideways() {
        let mut s = crate::init();
        s.main_tab = MainTab::Sync;
        s.layout = crate::layout::Layout::default();
        s.layout.sync_cols[0] = 1500.0;
        s.sync.set_rows(rows(5_000));
        let mut rt = rune::Runtime::new(s, crate::ui::view, (1600.0, 1000.0), rune::Env::new(false, 1.0));
        for _ in 0..4 {
            rt.frame(0.016);
        }
        let rect = |rt: &rune::Runtime<State>, key: &str| rt.elements().iter().find(|e| !e.exiting && e.key.as_deref() == Some(key)).map(|e| e.rect).unwrap_or_else(|| panic!("nothing keyed {key}"));
        // Each header and, under it, the first row's cell: where they start and how wide.
        let columns = |rt: &rune::Runtime<State>| -> Vec<(f32, f32)> {
            let all = rt.elements();
            let row = all.iter().position(|e| !e.exiting && e.key.as_deref().is_some_and(|k| k.starts_with("sync-row-"))).expect("a row");
            let cells: Vec<_> = all.iter().filter(|e| e.parent == Some(row)).map(|e| e.rect).collect();
            (0..8)
                .map(|col| {
                    let head = rect(rt, &format!("sync-col-{col}"));
                    assert!(cells[col][0] >= head[0] - 0.5 && cells[col][0] + cells[col][2] <= head[0] + head[2] + 0.5, "cell {:?} under header {head:?}", cells[col]);
                    (head[0], head[2])
                })
                .collect()
        };
        let table = rect(&rt, super::TABLE_VIEW);
        let at_rest = columns(&rt);
        assert_eq!(at_rest[0], (8.0, 1500.0));
        assert_eq!(at_rest.iter().map(|c| c.1).collect::<Vec<_>>(), [1500.0, 82.0, 124.0, 100.0, 300.0, 82.0, 124.0, 140.0]);
        assert!(at_rest[7].0 > table[0] + table[2], "the last column starts past the window's edge");
        let bar = rect(&rt, &format!("h-bar-{}", super::TABLE_VIEW));
        assert!(bar[2] > 200.0 && bar[1] > table[1] + table[3] - 30.0, "the bar along the bottom: {bar:?} of {table:?}");

        // Sideways by the wheel: every column moves the same way, the header with the rows.
        let (x, y) = (table[0] + table[2] / 2.0, table[1] + table[3] / 2.0);
        for _ in 0..4 {
            rt.wheel_xy(x, y, -400.0, 0.0);
            rt.frame(0.016);
        }
        let scrolled = columns(&rt);
        let moved = at_rest[0].0 - scrolled[0].0;
        assert!(moved > 500.0, "the wheel moved the table {moved} sideways");
        assert!(at_rest.iter().zip(&scrolled).all(|(a, b)| (a.0 - b.0 - moved).abs() < 0.5 && a.1 == b.1));

        // Down to the end: the header stays at the top of the table, the last row at its bottom.
        rt.update(|_, tx| tx.scroll_into_view("sync-rows-rest"));
        for _ in 0..30 {
            rt.frame(0.016);
        }
        assert!((rect(&rt, "sync-table-header")[1] - table[1]).abs() < 0.5, "the header stays at the top");
        let last = rect(&rt, "sync-row-4999");
        assert!(last[1] + last[3] <= table[1] + table[3] + 0.5 && last[1] > table[1] + table[3] - 60.0, "the last row {last:?} in {table:?}");

        // With no rows the header keeps the columns' widths.
        rt.update(|s, _| s.sync.clear_rows());
        for _ in 0..4 {
            rt.frame(0.016);
        }
        assert_eq!(rect(&rt, "sync-col-0")[2], 1500.0);
        assert_eq!(rect(&rt, "sync-col-7")[2], 140.0);
    }


    /// Only the rows in view are built, and what is shown follows the filters, an action
    /// switched off and a status that arrives.
    #[test]
    fn the_table_builds_only_the_rows_in_view() {
        use crate::sync::{self, ActionFilter, Answer, StatusFilter};
        let mut s = crate::init();
        s.main_tab = MainTab::Sync;
        s.layout = crate::layout::Layout::default();
        s.sync.set_rows(rows(5_000));
        let mut rt = rune::Runtime::new(s, crate::ui::view, (1600.0, 1000.0), rune::Env::new(false, 1.0));
        let built = |rt: &mut rune::Runtime<State>| {
            rt.frame(0.016);
            let mut ids: Vec<usize> = rt.elements().iter().filter(|e| !e.exiting).filter_map(|e| e.key.as_deref()?.strip_prefix("sync-row-")?.parse().ok()).collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        };
        let all = built(&mut rt);
        assert!((10..400).contains(&all.len()), "{} rows built", all.len());
        assert_eq!(all[..3], [0, 1, 2]);

        // The list is of blocks of rows, and the rows past the last whole block follow it: at
        // the end, the last rows show, each where the one before ends. 5,000 rows are 156
        // blocks and 8 rows more.
        let placed = |rt: &rune::Runtime<State>| -> Vec<(usize, [f32; 4], [f32; 4])> {
            let mut rows: Vec<_> = rt
                .elements()
                .iter()
                .filter(|e| !e.exiting)
                .filter_map(|e| Some((e.key.as_deref()?.strip_prefix("sync-row-")?.parse().ok()?, e.rect, e.visible)))
                .collect();
            rows.sort_by_key(|r| r.0);
            rows
        };
        let table = rt.elements().iter().find(|e| e.key.as_deref() == Some("sync-table-scroll")).map(|e| e.rect).expect("the table");
        // What shows of the rows fills the table from under its header to its bottom, with no
        // gap between them.
        let fills = |rt: &rune::Runtime<State>, what: &str| {
            let rows = placed(rt);
            let seen: Vec<_> = rows.iter().filter(|r| r.2[3] > 0.5).collect();
            let (top, bottom) = (seen[0].1[1], seen[seen.len() - 1].1[1] + seen[seen.len() - 1].1[3]);
            assert!(top <= table[1] + super::TABLE_HEADER_H + 0.5 && bottom >= table[1] + table[3] - 0.5, "{what}: rows show from {top} to {bottom}, the table is {table:?}");
            for pair in seen.windows(2) {
                assert_eq!(pair[1].0, pair[0].0 + 1, "{what}: a row is missing");
                assert!((pair[1].1[1] - (pair[0].1[1] + pair[0].1[3])).abs() < 0.5, "{what}: row {} doesn't start where row {} ends", pair[1].0, pair[0].0);
            }
            (seen[0].0, seen[seen.len() - 1].0)
        };
        assert_eq!(fills(&rt, "at the top").0, 0);
        let (x, y) = (table[0] + table[2] / 2.0, table[1] + table[3] / 2.0);
        // Flings of the wheel, each a frame: the rows in view are there in that same frame.
        for fling in 0..6 {
            rt.wheel(x, y, -2500.0);
            rt.frame(0.016);
            assert!(fills(&rt, "after a fling").0 > fling * 50);
        }
        rt.update(|_, tx| tx.scroll_into_view("sync-rows-rest"));
        for _ in 0..30 {
            rt.frame(0.016);
        }
        let (first, last) = fills(&rt, "at the end");
        assert!(first < 4_992 && last == 4_999, "rows {first} to {last} show at the end");
        let bottom = placed(&rt).last().map(|r| r.1[1] + r.1[3]).unwrap();
        assert!((bottom - (table[1] + table[3])).abs() < 1.0, "the last row ends at {bottom}, the table at {}", table[1] + table[3]);
        // And back up, by the wheel.
        for _ in 0..80 {
            rt.wheel(x, y, 2500.0);
            rt.frame(0.016);
        }
        assert_eq!(fills(&rt, "back at the top").0, 0);

        // The columns stand in the order kept, each as wide as kept, the header's as the rows':
        // here the left side's date before its size, and the status first.
        let cells = |rt: &rune::Runtime<State>| -> Vec<(String, f32, f32)> {
            let all = rt.elements();
            let row = all.iter().position(|e| !e.exiting && e.key.as_deref() == Some("sync-row-0")).expect("the first row");
            all.iter().filter(|e| e.parent == Some(row)).map(|e| (e.key.clone().unwrap_or_default(), e.rect[0], e.rect[2])).collect()
        };
        let heads = |rt: &rune::Runtime<State>| -> Vec<(String, f32, f32)> {
            rt.elements().iter().filter_map(|e| Some((e.key.as_deref()?.strip_prefix("sync-col-")?.parse::<usize>().ok()?, e.rect[0], e.rect[2]))).map(|(col, x, w)| (format!("c{col}"), x, w)).collect()
        };
        // Each cell of the row stands under its column's header, inside it.
        let lined_up = |rt: &rune::Runtime<State>| {
            let (cells, heads) = (cells(rt), heads(rt));
            assert_eq!(cells.len(), 8);
            for (cell, head) in cells.iter().zip(&heads) {
                assert_eq!(cell.0, head.0);
                assert!(cell.1 >= head.1 - 0.5 && cell.1 - head.1 <= 6.5 && cell.1 + cell.2 <= head.1 + head.2 + 0.5, "{cell:?} under {head:?}");
            }
            heads
        };
        let starting = lined_up(&rt);
        assert_eq!(starting.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7"]);
        assert_eq!((starting[0].1, starting[0].2, starting[1].1), (8.0, 300.0, 308.0));
        rt.update(|s, _| {
            s.layout.sync_order = [7, 0, 2, 1, 3, 4, 5, 6];
            s.layout.sync_cols[0] = 420.0;
            s.layout.sync_cols[7] = 90.0;
        });
        // The headers slide to their new places.
        for _ in 0..120 {
            rt.frame(0.016);
        }
        let moved = lined_up(&rt);
        assert_eq!(moved.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["c7", "c0", "c2", "c1", "c3", "c4", "c5", "c6"]);
        assert_eq!((moved[0].1, moved[0].2, moved[1].1, moved[1].2, moved[2].1), (8.0, 90.0, 98.0, 420.0, 518.0));
        // A header dragged shows where its column would land, and lands there when let go.
        rt.update(|s, _| {
            let cols = super::TableCols::of(s);
            assert_eq!((cols.place_at(0.0), cols.place_at(8.0), cols.place_at(97.0), cols.place_at(98.0), cols.place_at(5000.0)), (0, 0, 0, 1, 7));
            assert_eq!(crate::layout::moved_sync_col(cols.order, 0, cols.place_at(600.0)), [0, 2, 7, 1, 3, 4, 5, 6]);
            s.sync.col_drag = Some((0, 2));
        });
        rt.frame(0.016);
        let mark = rt.elements().iter().find(|e| e.key.as_deref() == Some("sync-col-mark")).map(|e| e.rect[0]);
        assert_eq!(mark, Some(641.0), "the mark stands where place 2 ends");
        rt.update(|s, _| {
            s.sync.col_drag = None;
            s.layout = crate::layout::Layout::default();
        });
        rt.frame(0.016);

        // A status that arrives shows in its row at once, with no filter on: what a run does.
        let texts = |rt: &rune::Runtime<State>, id: usize| {
            let all = rt.elements();
            let key = format!("sync-row-{id}");
            let row = all.iter().position(|e| !e.exiting && e.key.as_deref() == Some(key.as_str()));
            // The texts of the row's cells: a cell is a text, or holds one.
            all.iter().filter(|e| row.is_some() && (e.parent == row || e.parent.and_then(|cell| all[cell].parent) == row)).filter_map(|e| e.text.clone()).collect::<Vec<_>>()
        };
        assert!(texts(&rt, 2).contains(&RowStatus::Pending.label()), "{:?}", texts(&rt, 2));
        rt.update(|s, _| {
            let job = s.sync.job_seq;
            assert!(sync::arrived(s, Answer::RowStatus { job, row_id: 2, status: RowStatus::Done, done: 1, total: 1 }));
        });
        rt.frame(0.016);
        assert!(texts(&rt, 2).contains(&RowStatus::Done.label()), "{:?}", texts(&rt, 2));

        // Every fourth row copies to the right: rows 1, 5, 9…
        rt.update(|s, _| {
            let (shown, counts) = (s.sync.shown(), s.sync.counts());
            assert_eq!((shown.rows.len(), counts.planned(), counts.same), (5_000, 3_750, 1_250));
            s.sync.action_filter = ActionFilter::CopyRight;
        });
        assert_eq!(built(&mut rt)[..3], [1, 5, 9]);

        // An action switched off leaves what Iniciar would run, and shows as ignored.
        rt.update(|s, _| {
            sync::toggle_row(s, 5);
            let (shown, counts) = (s.sync.shown(), s.sync.counts());
            assert_eq!((shown.rows.len(), counts.planned(), counts.copy_right), (1_250, 3_749, 1_249));
            // Switched on again and off again, the totals follow.
            sync::toggle_row(s, 5);
            assert_eq!(s.sync.counts().planned(), 3_750);
            sync::toggle_row(s, 5);
            sync::toggle_row(s, 0);
            assert_eq!(s.sync.counts(), sync::Counts { copy_right: 1_249, copy_left: 1_250, delete: 1_250, same: 1_250 });
            s.sync.action_filter = ActionFilter::All;
            s.sync.status_filter = StatusFilter::Skipped;
        });
        assert_eq!(built(&mut rt), [5]);

        // A status that arrives moves its row to the filter it now belongs to.
        rt.update(|s, _| {
            s.sync.status_filter = StatusFilter::Done;
            assert_eq!(s.sync.shown().rows[..], [2]);
            let job = s.sync.job_seq;
            assert!(sync::arrived(s, Answer::RowStatus { job, row_id: 9, status: RowStatus::Done, done: 2, total: 2 }));
        });
        assert_eq!(built(&mut rt), [2, 9]);

        // The search looks in the path, whatever the case.
        rt.update(|s, _| {
            s.sync.status_filter = StatusFilter::All;
            s.sync.search = rune::TextEdit::new("SUB 0049/arquivo 00");
            assert_eq!(s.sync.shown().rows.len(), 10);
        });
        assert_eq!(built(&mut rt)[..2], [4900, 4901]);

        // New folders empty the table.
        rt.update(|s, _| sync::add_pair(s));
        assert!(built(&mut rt).is_empty());

        // A header dragged sideways: the left side's Tamanho, 200 points to the right, is over
        // Ação (two places on), and a mark shows where it would land. Not let go: letting go
        // saves the layout, and this test must not write the user's.
        let head = rt.elements().iter().find(|e| e.key.as_deref() == Some("sync-col-1")).map(|e| e.rect).expect("the header");
        let (x, y) = (head[0] + head[2] / 2.0, head[1] + head[3] / 2.0);
        rt.pointer_move(x, y);
        rt.pointer_down(x, y);
        rt.pointer_move(x + 2.0, y);
        rt.frame(0.016);
        rt.update(|s, _| assert_eq!(s.sync.col_drag, None, "a press that barely moves is no drag"));
        rt.pointer_move(x + 200.0, y);
        rt.frame(0.016);
        rt.update(|s, _| {
            assert_eq!(s.sync.col_drag, Some((1, 3)));
            assert_eq!(crate::layout::moved_sync_col(s.layout.sync_order, 1, 3), [0, 2, 3, 1, 4, 5, 6, 7]);
            assert_eq!(s.layout.sync_order, crate::layout::SYNC_ORDER, "nothing moves before the header is let go");
        });
        let mark = rt.elements().iter().find(|e| e.key.as_deref() == Some("sync-col-mark")).map(|e| e.rect[0] + 1.0);
        assert_eq!(mark, Some(8.0 + 300.0 + 82.0 + 124.0 + 100.0), "the mark stands after Ação");
    }

    /// What the Sincronizar tab costs with a finished comparison on screen: the first frame,
    /// then frames after a small change of state (what a hover or a status line does), and the
    /// memory held. `OE_SYNC_BENCH_ROWS` sets the row count (100,000 unless given). Run it with
    /// `cargo test --release -p openexplorer -- --ignored --nocapture compare_table_cost`.
    #[test]
    #[ignore = "benchmark"]
    fn compare_table_cost() {
        let n = std::env::var("OE_SYNC_BENCH_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(100_000);
        let mut s = crate::init();
        s.main_tab = MainTab::Sync;
        let before = memory_mb().0;
        s.sync.set_rows(rows(n));
        let held = memory_mb().0;
        let mut rt = rune::Runtime::new(s, crate::ui::view, (1600.0, 1000.0), rune::Env::new(false, 1.0));
        let started = Instant::now();
        rt.frame(0.016);
        let first = started.elapsed().as_secs_f64() * 1e3;
        let frames = if n > 20_000 { 5 } else { 20 };
        let mut times = Vec::new();
        for i in 0..frames {
            let started = Instant::now();
            rt.update_if(|s: &mut State, _| {
                s.sync.progress_text = format!("tick {i}");
                true
            });
            rt.frame(0.016);
            times.push(started.elapsed().as_secs_f64() * 1e3);
        }
        times.sort_by(f64::total_cmp);
        let (now, peak) = memory_mb();
        println!(
            "{n} rows: first frame {first:.1} ms, next frames {:.2} ms median of {frames}; rows hold {:.0} MB, window adds {:.0} MB (peak {peak:.0} MB)",
            times[frames / 2],
            held - before,
            now - held
        );

        // Moving about the comparison: each step is the change and the frame that shows it.
        use crate::sync::{self, ActionFilter, Answer, StatusFilter};
        // The median of 9, and the best of them: on a busy machine the best is the cost itself.
        struct Took(f64, f64);
        impl std::fmt::Display for Took {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{:.2} ms (best {:.2})", self.0, self.1)
            }
        }
        let step = |rt: &mut rune::Runtime<State>, change: &mut dyn FnMut(&mut rune::Runtime<State>, usize)| {
            let mut times: Vec<f64> = (0..9)
                .map(|i| {
                    let started = Instant::now();
                    change(rt, i);
                    rt.frame(0.016);
                    started.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            times.sort_by(f64::total_cmp);
            Took(times[4], times[0])
        };
        let first_built = |rt: &rune::Runtime<State>| {
            rt.elements().iter().filter(|e| !e.exiting).filter_map(|e| e.key.as_deref()?.strip_prefix("sync-row-")?.parse::<usize>().ok()).min().unwrap_or(0)
        };
        let table = rt.elements().iter().find(|e| e.key.as_deref() == Some("sync-table-scroll")).map(|e| e.rect).expect("the table");
        let (x, y) = (table[0] + table[2] / 2.0, table[1] + table[3] / 2.0);
        // A notch of the wheel (a few rows), then a fling of some 75 rows a frame.
        let wheel = step(&mut rt, &mut |rt, _| rt.wheel(x, y, -120.0));
        let fling = step(&mut rt, &mut |rt, _| rt.wheel(x, y, -2000.0));
        let down = first_built(&rt);
        assert!(down > 100, "the wheel didn't move the table: at row {down}");
        println!("  a fling of the wheel, a frame {fling}, down to row {down}");
        // To a row far away, as a drag of the scroll bar does.
        let jump = step(&mut rt, &mut |rt, i| {
            let far = format!("sync-rows-{}", ((i + 1) * n / 10 - 1) / super::TABLE_BLOCK - 1);
            rt.update(|_, tx| tx.scroll_into_view(&far));
        });
        println!("  a jump to a row far away {jump}, then at row {}", first_built(&rt));
        let toggle = step(&mut rt, &mut |rt, i| rt.update(|s, _| sync::toggle_row(s, 1 + 4 * i)));
        let status = step(&mut rt, &mut |rt, i| {
            rt.update(|s, _| {
                let job = s.sync.job_seq;
                sync::arrived(s, Answer::RowStatus { job, row_id: 2 + 4 * i, status: RowStatus::Done, done: i, total: 9 });
            })
        });
        // Of a filter's or a search's step, the part that works out which rows show.
        let picking = std::cell::Cell::new(0.0);
        let pick = |s: &State| {
            let started = Instant::now();
            s.sync.shown();
            picking.set(picking.get() + started.elapsed().as_secs_f64() * 1e3 / 9.0);
        };
        let filter = step(&mut rt, &mut |rt, i| {
            rt.update(|s, _| {
                s.sync.action_filter = [ActionFilter::CopyRight, ActionFilter::All][i % 2];
                pick(s);
            })
        });
        let filter_pick = picking.replace(0.0);
        // Between two filters that show as many rows: the list keeps its length, so this is
        // picking the rows and showing a new page of them, and no more.
        let filter_same = step(&mut rt, &mut |rt, i| rt.update(|s, _| s.sync.action_filter = [ActionFilter::CopyRight, ActionFilter::CopyLeft][i % 2]));
        println!("  the action filter changes, to as many rows {filter_same}");
        // The first letter of a search also puts every path in lowercase, once.
        let before_search = memory_mb().0;
        let started = Instant::now();
        rt.update(|s, _| {
            s.sync.search = rune::TextEdit::new("a");
            s.sync.shown();
        });
        let first_letter = started.elapsed().as_secs_f64() * 1e3;
        let search = step(&mut rt, &mut |rt, i| {
            rt.update(|s, _| {
                s.sync.search = rune::TextEdit::new(&"arquivo 05"[..2 + i]);
                pick(s);
            })
        });
        let search_pick = picking.replace(0.0);
        let searching = memory_mb().0;
        rt.update(|s, _| s.sync.search = rune::TextEdit::new(""));
        rt.frame(0.016);
        println!(
            "  the first letter of a search picks the rows in {first_letter:.2} ms; a search holds {:.0} MB, {:.0} MB once cleared",
            searching - before_search,
            memory_mb().0 - before_search
        );
        println!("  of which picking the rows: the action filter {filter_pick:.2} ms, the search {search_pick:.2} ms (means of 9)");
        rt.update(|s, _| {
            s.sync.search = rune::TextEdit::new("");
            s.sync.status_filter = StatusFilter::Pending;
        });
        let status_filtered = step(&mut rt, &mut |rt, i| {
            rt.update(|s, _| {
                let job = s.sync.job_seq;
                sync::arrived(s, Answer::RowStatus { job, row_id: 42 + 4 * i, status: RowStatus::Done, done: i, total: 9 });
            })
        });
        println!("  a notch of the wheel {wheel}\n  an action switched off {toggle}\n  a status arrives {status}\n  a status arrives, with a status filter {status_filtered}");
        println!("  the action filter changes {filter}\n  a letter typed in the search {search}");
    }
}
