//! The window: a toolbar, two panes (each a location tree beside a file list, under a
//! navigation bar and over a status bar), and the message line.

use std::borrow::Cow;
use std::path::PathBuf;

use rune::{Align, Anim, Color, Cursor, Cx, Drag, DragOutcome, El, GrowthSpec, Icon, KeyEvent, LiveRegion, Phase, ScrollSpec, Span, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, Dialog as Modal, Kit, MenuItem, Toast, ToastRegion, context_menu};

use crate::conflict::{self, Choice};
use crate::model::{Kind, Location, SortCol, Space, ViewMode, crumbs, format_bytes, format_long, format_size, format_size_exact, format_time, format_time_secs, shown};
use crate::actions::{self, ClipMode, Dialog};
use crate::{MainTab, State, click, drag_item, dragged_ids, dropped, go_back, go_forward, go_up, key, load, navigate, open, pane_key, paths_for, refresh, row_key, set_sort};
use crate::icons;
use crate::ops::Stage;
use openexplorer_i18n::{t, tr};
use crate::sysicons::{self, IconKey};
use crate::transfers;
use crate::layout::{self, clamp_col, clamp_tree};

const ROW_H: f32 = crate::ROW_H;

/// Open Explorer's themed `Kit`: replaces `Kit::neutral`'s stark black `Submit` button with the
/// app's accent blue and softens `Neutral` buttons in light mode across every window and dialog.
pub(crate) fn kit(dark: bool) -> Kit {
    let mut k = Kit::neutral(dark);
    if dark {
        k.c.bg_icon_button_contrast = rune::oklch(0.60, 0.15, 252.0);
        k.c.submit_hover = k.alpha(Color::WHITE, 0.10);
        k.c.contrast_pressed = k.alpha(Color::BLACK, 0.14);
    } else {
        k.c.bg_icon_button_contrast = rune::oklch(0.54, 0.165, 255.0);
        k.c.submit_hover = k.alpha(Color::BLACK, 0.10);
        k.c.contrast_pressed = k.alpha(Color::BLACK, 0.18);
        k.c.bg_button_neutral = rune::oklch(0.965, 0.0, 0.0);
    }
    k.c.submit_highlight = k.alpha(Color::WHITE, 0.14);
    k.c.submit_fg = Color::WHITE;
    k.c.submit_icon = Color::WHITE;
    k
}

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    if s.is_placeholder {
        return el();
    }
    let k = kit(s.settings.theme.dark(cx.system_dark()));
    cx.caption_color(k.c.text_base);
    let title_leading = row()
        .gap(8.0)
        .center_y()
        .child(icon(&icons::APP_ICON, 18.0))
        .child(text("Open Explorer").font(k.ty.text_small).color(k.c.text_base));
    // Under Mica (Opções, Experimental) the title bar lets the backdrop through, as the page does.
    // Turned off in Opções, the view paints the window's own background at once, and Mica leaves
    // once that has faded in (see `poll`).
    let active = cx.window_backdrop().is_active();
    if active {
        crate::MICA_SEEN.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let backdrop = active && s.settings.mica;
    // The acrylic background (Experimental, off Windows) shows through the same parts.
    let see_through = backdrop || s.acrylic;
    let titlebar = rune_kit::WindowTitlebar::from_insets(cx.chrome_insets())
        .key("main-titlebar")
        .leading_padding(12.0)
        .fill(if see_through { Color::TRANSPARENT } else { k.c.bg_base })
        .leading(title_leading)
        .build(&k);
    // Menus and dialogs draw in a layer above everything.
    let mut layer = Vec::new();
    let menu = context(s, &k, &mut layer);
    let tree_menu = tree_context(s, &k, &mut layer);
    let queue_menu = transfer_context(s, &k, &mut layer);
    let address_menu = address_context(s, &k, &mut layer);
    // The window's height, so Opções and Registrar conta always fit inside short windows from
    // the very first frame they open.
    let window_h = s.dialog.is_some().then(|| cx.layout_with("window", |r| r[3])).flatten();
    dialog(s, &k, window_h, backdrop, &mut layer);
    quit_question(s, &k, &mut layer);
    // Linux: a menu Rune asks for (a text field's Recortar, Copiar, Colar), as the system draws
    // none there.
    if let Some(request) = &s.popup {
        fallback_menu(request, |s| s.popup = None, &k, &mut layer);
    }
    // What the last action said, for a few seconds at the bottom right.
    if !s.status.is_empty() && s.now < s.status_until {
        let close = |s: &mut State| s.status_until = 0.0;
        let mut toast = Toast::new("notice", s.status.clone()).on_close(close);
        // The latest action's notice offers to take it back (or to redo what was just undone).
        if let Some(redo) = s.undo.offers(&s.status) {
            toast = toast.action(&k, if redo { t("Refazer", "Redo") } else { t("Desfazer", "Undo") }, false, move |s: &mut State, _| crate::undo::undo(s, redo));
        }
        // Not a live region of its own: `notice` has each message said once, at once for errors.
        layer.push(ToastRegion::new().toast(toast.build(&k).live_region(LiveRegion::Off)).build(&k));
    }
    let main_content = match s.main_tab {
        MainTab::Explore => panes(s, &k, see_through, &mut layer),
        MainTab::Accounts => accounts_hub(s, &k),
    };
    let toolbar_el = toolbar(s, &k, &mut layer);
    let body = col()
        .key("window")
        .w_full()
        .h_full()
        .fill(if see_through { Color::TRANSPARENT } else { k.c.page })
        .child(titlebar)
        .child(toolbar_el)
        .child(main_content)
        .child(transfers(s, &k))
        .child(menu)
        .child(tree_menu)
        .child(queue_menu)
        .child(address_menu)
        // The mouse's side buttons anywhere else in Explorar: Voltar and Avançar in the active
        // pane (over a pane, its own handler took them first).
        .on_button(|s: &mut State, button, _: &mut Tx| crate::side_button_in_window(s, button))
        // Fluid 80 ms hover transition when hover animations are on; instant otherwise.
        .hover_motion_within(
            if s.settings.hover_animated() { crate::anim::hover_curve() } else { Anim::INSTANT },
            if s.settings.hover_animated() { crate::anim::hover_curve() } else { Anim::INSTANT },
        )
        .on_key(|s: &mut State, ev: &KeyEvent, _: &mut Tx| {
            if ev.key == rune::Key::Escape && (s.view_menu_open || s.view_mode_menu.is_some() || s.search_scope_menu.is_some()) {
                s.view_menu_open = false;
                s.view_mode_menu = None;
                s.search_scope_menu = None;
                return true;
            }
            if ev.key == rune::Key::Function(5) {
                match s.main_tab {
                    MainTab::Explore => {
                        if !s.tabs[s.active].is_picker() {
                            refresh(s, s.active);
                        }
                    }
                    MainTab::Accounts => crate::refresh_all_quotas(s),
                }
                return true;
            }
            false
        });
    // Experimental, off Windows: the system's material behind the page, showing around the
    // panes and through their folder trees. Unfilled, it takes the material's own tint; where
    // the system draws none, Rune paints a near-opaque neutral one.
    let body = if s.acrylic { body.system_material(rune::SystemMaterial::Sidebar) } else { body };
    rune_kit::root(body, layer)
}

// ---------------------------------------------------------------------------------------------
// Opções

/// A number with − and + beside it inside a pill box, modernized from Air Explorer's spin boxes.
fn stepper(k: &Kit, key: &str, value: u8, set: fn(&mut State, i32)) -> El {
    let btn = |label: &str, delta: i32, enabled: bool| {
        let ink = if enabled { k.c.text_base } else { k.c.text_faint };
        let b = row()
            .key(format!("{key}-{delta}"))
            .role("button")
            .label(if delta < 0 { t("Diminuir", "Decrease") } else { t("Aumentar", "Increase") })
            .size(26.0, 26.0)
            .center()
            .radius(k.radius.sm)
            .child(text(label.to_string()).font(k.ty.text_small_medium).color(ink));
        if enabled {
            k.interactive(b).focusable(true).focus_ring(2.0, 1.0, k.c.border_focus).cursor(Cursor::Pointer).on_press(move |s: &mut State| set(s, delta))
        } else {
            b.disabled(true)
        }
    };
    row()
        .key(key.to_string())
        .padding(2.0)
        .gap(2.0)
        .center_y()
        .radius(k.radius.md)
        .fill(k.alpha(k.c.text_base, if k.dark { 0.05 } else { 0.035 }))
        .border(1.0, k.c.border_weak)
        .child(btn("−", -1, value > 1))
        .child(text(value.to_string()).font(k.ty.text_small_medium).color(k.c.text_base).w(30.0).align(Align::Center))
        .child(btn("+", 1, value < crate::settings::MAX_AT_ONCE))
}

fn change_settings(s: &mut State, f: impl FnOnce(&mut crate::settings::Settings)) {
    f(&mut s.settings);
    s.upload_slots.set_limit(usize::from(s.settings.uploads));
    s.download_slots.set_limit(usize::from(s.settings.downloads));
    s.settings.save();
}

fn reload_hidden_views(s: &mut State) {
    crate::tree::refresh_hidden(s);
    for p in 0..layout::MAX_PANES {
        if matches!(s.panes[p].loc, Location::Local(_) | Location::Server { .. } | Location::Found(_)) {
            load(s, p);
        }
    }
}

fn toggle_show_hidden(s: &mut State) {
    change_settings(s, |c| c.show_hidden = !c.show_hidden);
    reload_hidden_views(s);
}

fn toggle_clear_completed(s: &mut State) {
    change_settings(s, |c| c.clear_completed = !c.clear_completed);
    if s.settings.clear_completed {
        s.transfers.retain(|t| !matches!(t.progress.stage(), Stage::Done | Stage::Skipped));
    }
}

fn reset_settings(s: &mut State, tx: &mut Tx) {
    let (was_hidden, had_mica) = (s.settings.show_hidden, s.settings.mica);
    // Where the window was isn't an option: it stays.
    s.settings = crate::settings::Settings { window: s.settings.window, ..Default::default() };
    // The limits on transfers at once go back too, for those running now as well.
    s.upload_slots.set_limit(usize::from(s.settings.uploads));
    s.download_slots.set_limit(usize::from(s.settings.downloads));
    // The window stays transparent until it opens again: the material's tint follows the
    // theme back.
    if had_mica || s.acrylic {
        tx.set_color_scheme(s.settings.theme.color_scheme());
    }
    s.settings.save();
    s.settings.language.apply();
    crate::tray::refresh_menu(s);
    let (sans, mono) = s.settings.font.families();
    tx.set_font_families(sans, mono);
    tx.set_font_smoothing(s.settings.text_render.smoothing());
    if was_hidden != s.settings.show_hidden {
        reload_hidden_views(s);
    }
    if s.settings.clear_completed {
        s.transfers.retain(|t| !matches!(t.progress.stage(), Stage::Done | Stage::Skipped));
    }
    s.status = t("Opções restauradas para os valores padrão.", "Options restored to their defaults.").into();
}

fn set_options_tab(s: &mut State, tab: crate::settings::OptionsTab) {
    if let Some(Dialog::Options(opt)) = &mut s.dialog {
        opt.tab = tab;
        if tab == crate::settings::OptionsTab::Logs {
            opt.refresh_logs();
        }
    }
    if tab == crate::settings::OptionsTab::Cache {
        crate::caches::measure(s);
    }
}

/// The Opções body's height: 530 points, or less in a window too short for that (the dialog
/// doesn't scroll as a whole), keeping the footer's Concluído in view. `window_h` is the
/// window's height once known; the layer around the dialog pads 16 points, its header takes 48.
fn options_height(window_h: Option<f32>) -> f32 {
    window_h.map_or(530.0, |h| (h - 2.0 * 16.0 - 48.0).clamp(240.0, 530.0))
}

fn options_dialog(s: &State, k: &Kit, opt: &crate::settings::OptionsView, window_h: Option<f32>, backdrop: bool, layer: &mut Vec<El>) {
    use crate::settings::{OptionsTab, Settings, TextFont, TextRender, Theme, at_once};
    let st = s.settings;

    // Left sidebar navigation (Geral, Transferências, Logs + Restaurar padrões).
    let sidebar_bg = if k.dark { k.alpha(k.c.white, 0.018) } else { k.alpha(k.c.black, 0.022) };
    let mut nav = col()
        .key("opt-sidebar")
        .w(188.0)
        .h_full()
        .padding(10.0)
        .gap(4.0)
        .fill(sidebar_bg)
        .border_sides([0.0, 1.0, 0.0, 0.0], k.c.border_weak);

    for tab in OptionsTab::ALL {
        let on = opt.tab == tab;
        let fill = if on { k.alpha(k.c.text_accent, if k.dark { 0.20 } else { 0.11 }) } else { Color::TRANSPARENT };
        let border = if on { k.alpha(k.c.text_accent, 0.45) } else { Color::TRANSPARENT };
        let icon_ink = if on { k.c.text_accent } else { k.c.icon_muted };
        let text_ink = if on { k.c.text_base } else { k.c.text_muted };
        let mut item = k
            .interactive(
                row()
                    .key(format!("opt-tab-{tab:?}"))
                    .role("tab")
                    .label(tab.label())
                    .checked(on)
                    .focusable(true)
                    .focus_ring(2.0, 1.0, k.c.border_focus)
                    .w_full()
                    .h(36.0)
                    .px(10.0)
                    .gap(9.0)
                    .center_y()
                    .radius(k.radius.md)
                    .fill(fill)
                    .border(1.0, border)
                    .cursor(Cursor::Pointer),
            )
            .on_press(move |s: &mut State| set_options_tab(s, tab))
            .child(icon(tab.icon(), 15.0).color(icon_ink))
            .child(text(tab.label()).font(if on { k.ty.text_small_medium } else { k.ty.text_small }).color(text_ink).grow(1.0));

        if tab == OptionsTab::Logs && !opt.log_lines.is_empty() {
            item = item.child(
                row()
                    .h(16.0)
                    .px(5.0)
                    .center()
                    .radius(8.0)
                    .fill(if on { k.alpha(k.c.text_accent, 0.20) } else { k.alpha(k.c.text_base, 0.07) })
                    .child(text(opt.log_lines.len().to_string()).font(k.ty.caption()).color(if on { k.c.text_accent } else { k.c.text_faint })),
            );
        }
        nav = nav.child(item);
    }

    let modified = st != Settings::default();
    nav = nav.child(el().grow(1.0)).child(
        Button::new(t("Restaurar padrões", "Restore defaults"))
            .variant(ButtonVariant::Neutral)
            .disabled(!modified)
            .on_press_tx(reset_settings)
            .build(k).cursor(Cursor::Pointer)
            .key("reset-options"),
    );

    // Reusable card & row builders for clean grouped settings.
    let card_box = |key: &'static str| col().key(key).w_full().radius(k.radius.lg).fill(k.c.bg_base).border(1.0, k.c.border_weak);
    let divider = || el().w_full().h(1.0).fill(k.c.border_weak);
    let section_head = |title: &str, sub: &str| {
        col()
            .w_full()
            .gap(2.0)
            .child(text(title.to_string()).font(k.ty.body_medium).color(k.c.text_base))
            .child(text(sub.to_string()).font(k.ty.caption()).color(k.c.text_muted).wrap().w_full().min_w(0.0))
    };
    let switch_row = |key: &'static str, title: &str, desc: &str, badge: Option<(&str, rune_kit::BadgeVariant)>, on: bool, toggle: fn(&mut State)| {
        let mut head = row().gap(6.0).center_y().child(text(title.to_string()).font(k.ty.text_small_medium).color(k.c.text_base));
        if let Some((b_label, b_var)) = badge {
            head = head.child(rune_kit::Badge::new(b_label).variant(b_var).build(k));
        }
        let words = col()
            .grow(1.0)
            .w(0.0)
            .min_w(0.0)
            .gap(2.0)
            .child(head)
            .child(text(desc.to_string()).font(k.ty.caption()).color(k.c.text_muted).wrap().w_full().min_w(0.0));
        // Custom high-contrast toggle switch so the OFF track stands out clearly on bg_base cards
        // in both light and dark themes.
        let track_bg = if on { k.c.text_accent } else { k.alpha(k.c.text_base, if k.dark { 0.22 } else { 0.16 }) };
        let track_border = if on { k.c.text_accent } else { k.alpha(k.c.text_base, 0.22) };
        let knob = el()
            .size(16.0, 16.0)
            .radius(8.0)
            .fill(k.c.white)
            .box_shadow(rune::Shadow { y: 1.0, blur: 2.0, color: k.dark_a(0.25), ..Default::default() });
        let mut sw = row()
            .key(format!("sw-{key}"))
            .role("switch")
            .label(title.to_string())
            .checked(on)
            .focusable(true)
            .focus_ring(2.0, 1.0, k.c.border_focus)
            .w(36.0)
            .h(20.0)
            .px(2.0)
            .center_y()
            .radius(10.0)
            .fill(track_bg)
            .border(1.0, track_border)
            .cursor(Cursor::Pointer)
            .token_motion(k.motion.control)
            .on_press(toggle);
        if on {
            sw = sw.justify_end();
        }
        row()
            .key(key)
            .w_full()
            .px(14.0)
            .py(11.0)
            .gap(14.0)
            .center_y()
            .hover(k.alpha(k.c.text_base, 0.022))
            .cursor(Cursor::Pointer)
            .on_press(toggle)
            .child(words)
            .child(sw.child(knob))
    };

    let seg_track = |key: &'static str| {
        row()
            .key(key)
            .padding(2.0)
            .gap(2.0)
            .center_y()
            .radius(k.radius.md)
            .fill(k.alpha(k.c.text_base, if k.dark { 0.05 } else { 0.035 }))
            .border(1.0, k.c.border_weak)
    };
    let seg_btn = |key: String, label: &str, on: bool| {
        let fill = if on { k.alpha(k.c.text_accent, if k.dark { 0.22 } else { 0.12 }) } else { Color::TRANSPARENT };
        let border = if on { k.alpha(k.c.text_accent, 0.50) } else { Color::TRANSPARENT };
        k.interactive(
            row()
                .key(key)
                .role("radio")
                .label(label.to_string())
                .checked(on)
                .focusable(true)
                .focus_ring(2.0, 1.0, k.c.border_focus)
                .h(28.0)
                .px(9.0)
                .center()
                .radius(k.radius.sm)
                .fill(fill)
                .border(1.0, border)
                .cursor(Cursor::Pointer),
        )
        .child(text(label.to_string()).font(if on { k.ty.text_small_medium } else { k.ty.text_small }).color(if on { k.c.text_base } else { k.c.text_muted }))
    };
    let make_font_pills = |track_key: &'static str, prefix: &'static str| {
        let mut pills = seg_track(track_key);
        for f in TextFont::ALL {
            let on = st.font == f;
            pills = pills.child(seg_btn(format!("{prefix}-{f:?}"), f.label(), on).on_press_tx(move |s: &mut State, tx: &mut Tx| {
                change_settings(s, |c| c.font = f);
                let (sans, mono) = f.families();
                tx.set_font_families(sans, mono);
            }));
        }
        pills
    };
    let make_language_pills = |track_key: &'static str| {
        let mut pills = seg_track(track_key);
        for l in crate::settings::Language::ALL {
            let on = st.language == l;
            pills = pills.child(seg_btn(format!("lang-{l:?}"), l.label(), on).on_press(move |s: &mut State| {
                change_settings(s, |c| c.language = l);
                l.apply();
                crate::tray::refresh_menu(s);
            }));
        }
        pills
    };
    let make_render_pills = |track_key: &'static str, prefix: &'static str| {
        let mut pills = seg_track(track_key);
        for &r in TextRender::available() {
            let on = st.text_render == r;
            pills = pills.child(seg_btn(format!("{prefix}-{r:?}"), r.label(), on).on_press_tx(move |s: &mut State, tx: &mut Tx| {
                change_settings(s, |c| c.text_render = r);
                tx.set_font_smoothing(r.smoothing());
            }));
        }
        pills
    };

    let control_row = |key: &'static str, title: &str, desc: &str, control: El| {
        row()
            .key(key)
            .w_full()
            .px(14.0)
            .py(11.0)
            .gap(12.0)
            .center_y()
            .child(
                col()
                    .grow(1.0)
                    .w(0.0)
                    .min_w(0.0)
                    .gap(2.0)
                    .child(text(title.to_string()).font(k.ty.text_small_medium).color(k.c.text_base))
                    .child(text(desc.to_string()).font(k.ty.caption()).color(k.c.text_muted).wrap().w_full().min_w(0.0)),
            )
            .child(control)
    };

    // Right tab content.
    let page: El = match opt.tab {
        OptionsTab::General => {
            // Theme picker pills with icons.
            let mut theme_pills = row().key("theme-pills").w_full().gap(8.0);
            for th in Theme::ALL {
                let on = st.theme == th;
                let fill = if on { k.alpha(k.c.text_accent, if k.dark { 0.18 } else { 0.10 }) } else { k.alpha(k.c.text_base, 0.025) };
                let border = if on { k.alpha(k.c.text_accent, 0.55) } else { k.c.border_weak };
                let pill = k
                    .interactive(
                        row()
                            .key(format!("th-{th:?}"))
                            .role("radio")
                            .label(th.label())
                            .checked(on)
                            .focusable(true)
                            .focus_ring(2.0, 1.0, k.c.border_focus)
                            .grow(1.0)
                            .w(0.0)
                            .h(34.0)
                            .gap(7.0)
                            .center()
                            .radius(k.radius.md)
                            .fill(fill)
                            .border(1.0, border)
                            .cursor(Cursor::Pointer),
                    )
                    .on_press_tx(move |s: &mut State, tx: &mut Tx| {
                        change_settings(s, |c| c.theme = th);
                        if s.settings.mica || s.acrylic {
                            tx.set_color_scheme(th.color_scheme());
                        }
                    })
                    .child(icon(th.icon(), 15.0).color(if on { k.c.text_accent } else { k.c.icon_muted }))
                    .child(text(th.label()).font(if on { k.ty.text_small_medium } else { k.ty.text_small }).color(if on { k.c.text_base } else { k.c.text_muted }));
                theme_pills = theme_pills.child(pill);
            }

            let font_desc = if cfg!(windows) {
                t("Usa a fonte nativa do Windows (Segoe UI) ou a fonte integrada Inter.", "Uses Windows' own font (Segoe UI) or the built-in Inter font.")
            } else if cfg!(target_os = "macos") {
                t("Usa a fonte nativa do macOS (San Francisco) ou a fonte integrada Inter.", "Uses macOS's own font (San Francisco) or the built-in Inter font.")
            } else {
                t("Usa a fonte nativa do sistema ou a fonte integrada Inter.", "Uses the system's own font or the built-in Inter font.")
            };

            let appearance = card_box("card-theme")
                .child(
                    col()
                        .w_full()
                        .px(14.0)
                        .py(12.0)
                        .gap(9.0)
                        .child(
                            col()
                                .w_full()
                                .gap(2.0)
                                .child(text(t("Tema do aplicativo", "App theme")).font(k.ty.text_small_medium).color(k.c.text_base))
                                .child(text(t("Escolha a aparência das janelas ou acompanhe automaticamente o tema do sistema.", "Choose how the windows look, or follow the system theme automatically.")).font(k.ty.caption()).color(k.c.text_muted).wrap().w_full()),
                        )
                        .child(theme_pills),
                )
                .child(divider())
                .child(control_row("row-font", t("Fonte da interface", "Interface font"), font_desc, make_font_pills("font-pills", "font")))
                .child(divider())
                .child(control_row(
                    "row-language",
                    t("Idioma", "Language"),
                    t("Automático segue o idioma do Windows.", "Automatic follows Windows' language."),
                    make_language_pills("language-pills"),
                ));

            let purge_badge = st.skip_recycle_bin.then_some((t("Permanente", "Permanent"), rune_kit::BadgeVariant::Accent));
            let files_card = card_box("card-files")
                .child(switch_row(
                    "opt-hidden",
                    t("Exibir arquivos ocultos", "Show hidden files"),
                    t("Mostra pastas e arquivos ocultos do sistema ou iniciados por ponto (ex.: .git, .cargo).", "Shows hidden system files and folders, and those starting with a dot (e.g. .git, .cargo)."),
                    None,
                    st.show_hidden,
                    toggle_show_hidden,
                ))
                .child(divider())
                .child(switch_row(
                    "opt-skip-trash",
                    t("Não mover arquivos locais para a Lixeira", "Don't move local files to the Recycle Bin"),
                    t("Exclui itens do computador permanentemente após confirmação, sem passar pela Lixeira.", "Deletes items on this PC permanently after confirmation, skipping the Recycle Bin."),
                    purge_badge,
                    st.skip_recycle_bin,
                    |s| change_settings(s, |c| c.skip_recycle_bin = !c.skip_recycle_bin),
                ));

            let update_desc = if s.updater.checking {
                t("Verificando novas versões no GitHub…", "Checking GitHub for new versions…").to_string()
            } else if let Some(rel) = &s.updater.release {
                if s.updater.is_ready() {
                    tr!("Versão {} baixada e pronta para instalar.", "Version {} downloaded and ready to install.", rel.version)
                } else {
                    tr!("Nova versão {} disponível.", "New version {} available.", rel.version)
                }
            } else if let Some(err) = &s.updater.check_error {
                err.clone()
            } else if s.updater.checked_once {
                t("Você já está na versão mais recente.", "You're already on the latest version.").to_string()
            } else {
                t("Verifica lançamentos oficiais no GitHub.", "Checks official releases on GitHub.").to_string()
            };
            let update_btn = if s.updater.release.is_some() {
                Button::new(if s.updater.is_ready() {
                    t("Reiniciar para atualizar", "Restart to update")
                } else {
                    t("Ver atualização", "View update")
                })
                .variant(ButtonVariant::Submit)
                .on_press(actions::open_update_dialog)
                .build(k)
                .cursor(Cursor::Pointer)
                .key("opt-open-update")
            } else {
                Button::new(if s.updater.checking {
                    t("Verificando…", "Checking…")
                } else {
                    t("Verificar atualizações", "Check for updates")
                })
                .variant(ButtonVariant::Neutral)
                .disabled(s.updater.checking)
                .on_press(|s: &mut State| actions::check_updates(s, true))
                .build(k)
                .cursor(Cursor::Pointer)
                .key("opt-check-updates")
            };
            let version_title = tr!("Open Explorer v{}", "Open Explorer v{}", crate::updater::VERSION);
            let updates_card = card_box("card-updates")
                .child(switch_row(
                    "opt-auto-update",
                    t("Atualizar automaticamente", "Update automatically"),
                    t(
                        "Baixa novas versões em segundo plano e instala na próxima vez que o Open Explorer abrir.",
                        "Downloads new versions in the background and installs them the next time Open Explorer starts.",
                    ),
                    None,
                    st.auto_update,
                    |s| change_settings(s, |c| c.auto_update = !c.auto_update),
                ))
                .child(divider())
                .child(control_row("row-update-check", &version_title, &update_desc, update_btn));

            col()
                .key("page-general")
                .w_full()
                .gap(12.0)
                .child(section_head(t("Geral", "General"), t("Aparência, fonte da interface e comportamento de arquivos.", "Appearance, interface font, and how files behave.")))
                .child(appearance)
                .child(files_card)
                .child(updates_card)
        }
        OptionsTab::Animations => {
            let master_badge = Some(if st.animations {
                (t("Ativado", "On"), rune_kit::BadgeVariant::Accent)
            } else {
                (t("Desativado", "Off"), rune_kit::BadgeVariant::Neutral)
            });
            let master_card = card_box("card-anim-master").child(switch_row(
                "opt-animations",
                t("Ativar animações", "Enable animations"),
                t(
                    "Ativa ou desativa todas as animações fluidas da interface de uma só vez.",
                    "Turns all fluid interface animations on or off at once.",
                ),
                master_badge,
                st.animations,
                |s| change_settings(s, |c| c.animations = !c.animations),
            ));

            let effects_card = card_box("card-anim-effects")
                .opacity(if st.animations { 1.0 } else { 0.55 })
                .child(switch_row(
                    "opt-anim-hover",
                    t("Realce fluido ao passar o mouse", "Fluid hover highlight"),
                    t(
                        "Desliza suavemente o destaque entre pastas, arquivos e controles conforme o ponteiro se move.",
                        "Smoothly glides the highlight across folders, files, and controls as the pointer moves.",
                    ),
                    None,
                    st.animate_hover,
                    |s| change_settings(s, |c| c.animate_hover = !c.animate_hover),
                ))
                .child(divider())
                .child(switch_row(
                    "opt-anim-clipboard",
                    t("Animação ao copiar e recortar (Ctrl+C / Ctrl+X)", "Copy & cut icon animation (Ctrl+C / Ctrl+X)"),
                    t(
                        "Anima os ícones das pastas e arquivos copiados ou recortados até você colar ou pressionar Esc.",
                        "Gently wiggles icons of copied or cut folders and files until you paste or press Esc.",
                    ),
                    None,
                    st.animate_clipboard,
                    |s| change_settings(s, |c| c.animate_clipboard = !c.animate_clipboard),
                ))
                .child(divider())
                .child(switch_row(
                    "opt-anim-zoom",
                    t("Zoom suave na lista (Ctrl + Scroll)", "Smooth list zoom (Ctrl + Scroll)"),
                    t(
                        "Redimensiona ícones, linhas e texto suavemente ao usar Ctrl + roda do mouse, pinça ou Ctrl + / − / 0.",
                        "Fluidly resizes icons, rows, and text when using Ctrl + mouse wheel, pinch, or Ctrl + / − / 0.",
                    ),
                    None,
                    st.animate_zoom,
                    |s| change_settings(s, |c| c.animate_zoom = !c.animate_zoom),
                ))
                .child(divider())
                .child(switch_row(
                    "opt-anim-selection",
                    t("Transições suaves de seleção, ordenação e árvore", "Smooth selection, sort & tree transitions"),
                    t(
                        "Suaviza o destaque de seleção (incluindo setas do teclado e retângulo de seleção), a reordenação ao ordenar colunas e a rotação das setas.",
                        "Smooths selection highlights (including keyboard arrows and marquee fade), row reordering on sort, and chevron rotation.",
                    ),
                    None,
                    st.animate_selection,
                    |s| change_settings(s, |c| c.animate_selection = !c.animate_selection),
                ));

            let visual_card = card_box("card-anim-visual").child(switch_row(
                "opt-zebra-stripes",
                t("Listras alternadas nas linhas (zebra)", "Alternating row stripes (zebra)"),
                t(
                    "Aplica um fundo sutil em linhas alternadas da lista de arquivos para facilitar a leitura entre colunas.",
                    "Applies a subtle alternating background tint across file list rows to make scanning columns easier.",
                ),
                None,
                st.zebra_stripes,
                |s| change_settings(s, |c| c.zebra_stripes = !c.zebra_stripes),
            ));

            col()
                .key("page-animations")
                .w_full()
                .gap(12.0)
                .child(section_head(
                    t("Animações", "Animations"),
                    t(
                        "Efeitos visuais fluidos para navegação, cópia de arquivos, ordenação e zoom. Todos vêm ativados por padrão.",
                        "Fluid visual effects for browsing, copying files, sorting, and zooming. All are enabled by default.",
                    ),
                ))
                .child(master_card)
                .child(effects_card)
                .child(visual_card)
        }
        OptionsTab::Experimental => {
            let banner = row()
                .key("exp-banner")
                .w_full()
                .px(12.0)
                .py(10.0)
                .gap(8.0)
                .center_y()
                .radius(k.radius.md)
                .fill(k.alpha(k.c.warning.fg, if k.dark { 0.10 } else { 0.07 }))
                .border(1.0, k.alpha(k.c.warning.fg, 0.28))
                .child(rune_kit::Badge::new("Experimental").variant(rune_kit::BadgeVariant::Accent).build(k))
                .child(
                    text(t(
                        "Recursos em teste e opções avançadas do motor gráfico. O padrão Suave (GPU) já entrega a melhor qualidade e eficiência.",
                        "Features being tried out, and advanced options for the graphics engine. The default, Smooth (GPU), already gives the best quality and efficiency.",
                    ))
                        .font(k.ty.caption())
                        .color(k.c.text_muted)
                        .wrap()
                        .grow(1.0)
                        .w(0.0)
                        .min_w(0.0),
                );

            let render_desc = if cfg!(windows) {
                t(
                    "Alterna entre o suavizador vetorial na GPU (padrão recomendado), DirectWrite em tons de cinza e subpixels ClearType.",
                    "Switches between vector smoothing on the GPU (recommended default), grayscale DirectWrite and ClearType subpixels.",
                )
            } else {
                t(
                    "Este sistema usa o suavizador vetorial nativo na GPU (DirectWrite e ClearType estão disponíveis apenas no Windows).",
                    "This system uses native vector smoothing on the GPU (DirectWrite and ClearType are only available on Windows).",
                )
            };
            // Whether Mica shows now: Windows takes it only on some systems (Windows 11 22H2 or
            // later, a GPU drawing through DirectComposition), and Rune turns it off quietly when
            // it can't, so the row says so.
            let started_with_mica = crate::STARTED_WITH_MICA.load(std::sync::atomic::Ordering::Relaxed);
            // Turned back on, Mica takes a frame or two to come: not a refusal once Windows has
            // shown it in this window.
            let seen = crate::MICA_SEEN.load(std::sync::atomic::Ordering::Relaxed);
            let (mica_badge, mica_note) = match (st.mica, started_with_mica, backdrop) {
                (true, _, true) => (Some((t("Ativo", "On"), rune_kit::BadgeVariant::Accent)), None),
                (true, false, _) if cfg!(windows) => (Some((t("Reinicie para aplicar", "Restart to apply"), rune_kit::BadgeVariant::Neutral)), None),
                (true, true, false) if !seen => (
                    Some((t("Não aceito pelo Windows", "Not accepted by Windows"), rune_kit::BadgeVariant::Subtle)),
                    Some(t(
                        " Nesta janela o Windows não ativou o efeito: ele pede o Windows 11 22H2 ou mais novo e a placa de vídeo desenhando pelo DirectComposition, e o Open Explorer mostra o fundo de sempre.",
                        " Windows didn't turn the effect on for this window: it needs Windows 11 22H2 or later and the GPU drawing through DirectComposition, so Open Explorer shows its usual background.",
                    )),
                ),
                _ => (None, None),
            };
            let mica_desc = format!(
                "{}{}",
                t(
                    "Um material do Windows 11 aparece em volta dos painéis, na árvore e na barra de status enquanto a janela está em primeiro plano. Desligar ou trocar o efeito vale na hora; ligar, ao reiniciar o Open Explorer.",
                    "A Windows 11 material shows around the panes, in the tree and in the status bar while the window is in front. Turning it off or changing the effect takes effect at once; turning it on, once Open Explorer restarts.",
                ),
                mica_note.unwrap_or_default()
            );
            // Which material, while the background is on: each one says what it shows.
            let material_desc = match st.material {
                crate::settings::Material::Mica => t(
                    "Tinge a janela de leve com as cores do papel de parede, como o Explorador de Arquivos do Windows 11.",
                    "Tints the window lightly with the wallpaper's colours, as Windows 11's File Explorer does.",
                ),
                crate::settings::Material::MicaAlt => t(
                    "Como o Mica, com o tom do papel de parede mais forte, como no Terminal do Windows.",
                    "Like Mica, with the wallpaper's tint stronger, as in Windows Terminal.",
                ),
                crate::settings::Material::Acrylic => t(
                    "Desfoca o que está atrás da janela, outros aplicativos e o papel de parede. Usa mais a placa de vídeo.",
                    "Blurs what lies behind the window, other apps and the wallpaper. Uses the GPU more.",
                ),
            };
            let mut material_pills = seg_track("material-pills");
            for m in crate::settings::Material::ALL {
                material_pills = material_pills.child(seg_btn(format!("material-{m:?}"), m.label(), st.material == m).on_press(move |s: &mut State| {
                    change_settings(s, |c| c.material = m);
                }));
            }
            let material_row = control_row("row-material", t("Efeito", "Effect"), material_desc, material_pills);
            let exp_card = card_box("card-experimental")
                .child(control_row(
                    "row-render",
                    t("Nitidez e rasterização de texto", "Text sharpness and rendering"),
                    render_desc,
                    make_render_pills("render-pills", "render"),
                ))
                .child(divider())
                .child(switch_row(
                    "opt-mica",
                    t("Fundo translúcido do Windows 11", "Windows 11 translucent background"),
                    &mica_desc,
                    mica_badge,
                    st.mica,
                    |s| change_settings(s, |c| c.mica = !c.mica),
                ))
                .child(if st.mica { divider() } else { el() })
                .child(if st.mica { material_row } else { el() })
                .child(divider())
                .child(switch_row(
                    "opt-search-marker",
                    t("Marca-texto nos resultados da busca", "Highlighter on search results"),
                    t(
                        "Marca as letras encontradas nos nomes com um fundo, como um marca-texto, em vez de só mudar a cor delas.",
                        "Marks the matched letters in names with a background, like a highlighter, instead of only changing their colour.",
                    ),
                    None,
                    st.search_marker,
                    |s| change_settings(s, |c| c.search_marker = !c.search_marker),
                ));
            // Off Windows, where the translucent background above is Windows 11's: the system's
            // own material behind the window. On Windows that background's Acrylic effect is it.
            let exp_card = if cfg!(windows) {
                exp_card
            } else {
                exp_card.child(divider()).child(switch_row(
                    "opt-acrylic",
                    t("Fundo em acrílico", "Acrylic background"),
                    t(
                        "O fundo da janela e a árvore de pastas ficam translúcidos e desfocam o que está atrás da janela: no macOS, o material da barra lateral; no Linux, o desfoque do KDE ou do Hyprland. Vale ao abrir o Open Explorer de novo.",
                        "The window's background and the folder tree turn translucent and blur what is behind the window: on macOS, the sidebar material; on Linux, KDE's or Hyprland's blur. Takes effect the next time Open Explorer opens.",
                    ),
                    None,
                    st.acrylic,
                    |s| change_settings(s, |c| c.acrylic = !c.acrylic),
                ))
            };
            // Rune does it only on Windows.
            let exp_card = if cfg!(windows) {
                exp_card.child(divider()).child(switch_row(
                    "opt-resize-sync",
                    t("Redimensionar sem atraso", "Resize without lag"),
                    t(
                        "Ao arrastar a borda esquerda ou de cima da janela, o conteúdo acompanha a borda em vez de tremer. Não vale com o fundo Mica ou em acrílico. Vale ao abrir o Open Explorer de novo.",
                        "Dragging the window's left or top edge, the content keeps up with the edge instead of shaking. Not with the Mica or Acrylic background. Takes effect the next time Open Explorer opens.",
                    ),
                    None,
                    st.resize_sync,
                    |s| change_settings(s, |c| c.resize_sync = !c.resize_sync),
                ))
            } else {
                exp_card
            };

            col()
                .key("page-experimental")
                .w_full()
                .gap(12.0)
                .child(section_head("Experimental", t("Recursos em teste e parâmetros internos de renderização.", "Features being tried out, and internal rendering settings.")))
                .child(banner)
                .child(exp_card)
        }
        OptionsTab::Transfers => {
            // `control_row`, a little roomier around the steppers.
            let count_row = |key: &'static str, title: &str, desc: &str, control: El| control_row(key, title, desc, control).py(12.0).gap(14.0);

            let concurrency = card_box("card-concurrency")
                .child(count_row(
                    "row-uploads",
                    t("Uploads simultâneos", "Simultaneous uploads"),
                    t("Arquivos enviados ao mesmo tempo para o Google Drive ou servidor remoto (1 a 10).", "Files uploaded at the same time to Google Drive or a remote server (1 to 10)."),
                    stepper(k, "uploads", st.uploads, |s, d| change_settings(s, |c| c.uploads = at_once(i32::from(c.uploads) + d))),
                ))
                .child(divider())
                .child(count_row(
                    "row-downloads",
                    t("Downloads simultâneos", "Simultaneous downloads"),
                    t("Arquivos baixados ao mesmo tempo para o computador (1 a 10).", "Files downloaded at the same time to this PC (1 to 10)."),
                    stepper(k, "downloads", st.downloads, |s, d| change_settings(s, |c| c.downloads = at_once(i32::from(c.downloads) + d))),
                ));

            let speed_card = card_box("card-segmented").child(switch_row(
                "opt-segmented",
                t("Baixar arquivos grandes em segmentos paralelos", "Download large files in parallel segments"),
                t("Divide arquivos grandes em múltiplas conexões simultâneas para acelerar o download.", "Splits large files across several simultaneous connections to speed up the download."),
                Some(("≥ 64 MB", rune_kit::BadgeVariant::Subtle)),
                st.segmented_downloads,
                |s| change_settings(s, |c| c.segmented_downloads = !c.segmented_downloads),
            ));

            let queue_card = card_box("card-queue").child(switch_row(
                "opt-clear-done",
                t("Limpar concluídos automaticamente", "Clear finished automatically"),
                t(
                    "Remove da fila inferior as transferências assim que terminam sem erro. Quando desativado, elas permanecem visíveis até você limpá-las manualmente.",
                    "Removes transfers from the queue below as soon as they finish without errors. When off, they stay visible until you clear them manually.",
                ),
                None,
                st.clear_completed,
                toggle_clear_completed,
            ));

            let info_box = row()
                .key("transfer-note")
                .w_full()
                .px(12.0)
                .py(9.0)
                .radius(k.radius.md)
                .fill(k.alpha(k.c.text_accent, if k.dark { 0.08 } else { 0.05 }))
                .border(1.0, k.alpha(k.c.text_accent, 0.22))
                .child(
                    text(t(
                        "As alterações de simultaneidade e segmentos paralelos passam a valer na próxima transferência iniciada. Cada servidor também respeita o próprio limite de conexões configurado na conta.",
                        "Changes to simultaneous transfers and parallel segments take effect on the next transfer started. Each server also keeps to the connection limit set in its account.",
                    ))
                        .font(k.ty.caption())
                        .color(k.c.text_muted)
                        .wrap()
                        .w_full()
                        .min_w(0.0),
                );

            col()
                .key("page-transfers")
                .w_full()
                .gap(12.0)
                .child(section_head(
                    t("Transferências", "Transfers"),
                    t(
                        "Ajuste quantas transferências rodam em paralelo, a aceleração de downloads grandes e a limpeza da fila.",
                        "Set how many transfers run in parallel, how large downloads are sped up, and how the queue is cleared.",
                    ),
                ))
                .child(concurrency)
                .child(speed_card)
                .child(queue_card)
                .child(info_box)
        }
        OptionsTab::Cache => cache_page(
            k,
            &opt.cache,
            card_box("card-cache"),
            section_head(
                "Cache",
                t(
                    "Cópias locais das pastas do Google Drive e das contas MEGA, Dropbox, OneDrive e Backblaze B2, para que abram sem esperar pela rede.",
                    "Local copies of Google Drive folders and MEGA, Dropbox, OneDrive and Backblaze B2 accounts, so they open without waiting for the network.",
                ),
            ),
        ),
        OptionsTab::Logs => {
            let visible = opt.filtered_logs();
            let size_badge = if opt.log_bytes > 0 { format_size(opt.log_bytes) } else { t("Vazio", "Empty").into() };

            let tools = row()
                .key("log-tools")
                .w_full()
                .gap(6.0)
                .center_y()
                .child(text("Logs").font(k.ty.body_medium).color(k.c.text_base))
                .child(rune_kit::Badge::new(size_badge).variant(rune_kit::BadgeVariant::Subtle).standard().build(k))
                .child(el().grow(1.0))
                .child(
                    Button::new(t("Atualizar", "Refresh"))
                        .icon(&icons::REFRESH)
                        .variant(ButtonVariant::Neutral)
                        .on_press(|s: &mut State| {
                            if let Some(Dialog::Options(opt)) = &mut s.dialog {
                                opt.refresh_logs();
                            }
                        })
                        .build(k).cursor(Cursor::Pointer),
                )
                .child(
                    Button::new(t("Copiar", "Copy"))
                        .variant(ButtonVariant::Neutral)
                        .disabled(visible.is_empty())
                        .on_press_tx(|s: &mut State, tx: &mut Tx| {
                            // Joined on the press, not on every frame the tab is drawn.
                            let Some(Dialog::Options(opt)) = &s.dialog else { return };
                            tx.copy(opt.filtered_logs().join("\n"));
                            s.status = t("Log copiado para a área de transferência.", "Log copied to the clipboard.").into();
                        })
                        .build(k).cursor(Cursor::Pointer),
                )
                .child(
                    Button::new(t("Abrir pasta", "Open folder"))
                        .variant(ButtonVariant::Neutral)
                        .on_press(|s: &mut State| {
                            let dir = openexplorer_gdrive::local_dir();
                            let _ = std::fs::create_dir_all(&dir);
                            if let Err(e) = crate::ops::open(&dir) {
                                crate::fail(s, e);
                            }
                        })
                        .build(k).cursor(Cursor::Pointer),
                )
                .child(
                    Button::new(t("Limpar", "Clear"))
                        .variant(ButtonVariant::Danger)
                        .disabled(opt.log_lines.is_empty())
                        .on_press(|s: &mut State| match openexplorer_gdrive::clear_log() {
                            Ok(()) => {
                                if let Some(Dialog::Options(opt)) = &mut s.dialog {
                                    opt.refresh_logs();
                                }
                                s.status = t("Arquivo de log limpo.", "Log file cleared.").into();
                            }
                            Err(e) => crate::fail(s, e),
                        })
                        .build(k).cursor(Cursor::Pointer),
                );

            let mut filter_input = rune_kit::TextInput::new(&opt.log_filter)
                .width(None)
                .label(t("Filtrar log", "Filter log"))
                .leading_icon(&icons::SEARCH)
                .placeholder(t("Filtrar registros do log (ex.: erro, sftp, cota, operação)…", "Filter log entries (e.g. error, sftp, quota, operation)…"))
                .on_edit(|s: &mut State| match &mut s.dialog {
                    Some(Dialog::Options(opt)) => &mut opt.log_filter,
                    _ => &mut s.name_edit,
                });
            if !opt.log_filter.text.is_empty() {
                filter_input = filter_input.clear_button().on_action(|s: &mut State, _: &mut Tx| {
                    if let Some(Dialog::Options(opt)) = &mut s.dialog {
                        opt.log_filter = rune::TextEdit::new("");
                        opt.log_filter.focused = true;
                    }
                });
            }

            let viewer: El = if visible.is_empty() {
                let empty_msg = if opt.log_lines.is_empty() {
                    t("Nenhum evento registrado ainda em openexplorer.log.", "Nothing logged in openexplorer.log yet.")
                } else {
                    t("Nenhuma linha do log corresponde ao filtro digitado.", "No log line matches the filter.")
                };
                col()
                    .key("log-empty")
                    .w_full()
                    .grow(1.0)
                    .center()
                    .padding(20.0)
                    .radius(k.radius.md)
                    .fill(k.c.bg_base)
                    .border(1.0, k.c.border_weak)
                    .child(text(empty_msg).font(k.ty.text_small).color(k.c.text_muted).align(Align::Center))
            } else {
                let mut lines_col = col().key("log-lines").w_full().padding(10.0).pr(18.0).gap(4.0);
                // Newest entries at the top so recent activity is visible immediately.
                for (idx, line) in visible.iter().rev().enumerate() {
                    let lower = line.to_lowercase();
                    let is_err = lower.contains("erro") || lower.contains("falha") || lower.contains("pânico") || lower.contains("error") || lower.contains("failed") || lower.contains("panic") || lower.contains("http 4") || lower.contains("http 5");
                    let (stamp, rest) = match line.split_once(' ') {
                        Some((ts, msg)) if ts.contains(':') => (ts, msg),
                        _ => ("", *line),
                    };
                    let mut entry = row().key(format!("log-{idx}")).w_full().gap(10.0).items_start();
                    if !stamp.is_empty() {
                        entry = entry.child(text(stamp.to_string()).font(k.ty.mono).color(k.c.text_faint).w(90.0));
                    }
                    entry = entry.child(
                        text(rest.to_string())
                            .font(k.ty.mono)
                            .color(if is_err { k.c.danger.fg } else { k.c.text_base })
                            .selectable(true)
                            .selection_color(k.selection())
                            .wrap_anywhere()
                            .grow(1.0)
                            .w(0.0)
                            .min_w(0.0),
                    );
                    lines_col = lines_col.child(entry);
                }
                col()
                    .key("log-box")
                    .w_full()
                    .grow(1.0)
                    .min_h(0.0)
                    .radius(k.radius.md)
                    .fill(k.c.bg_base)
                    .border(1.0, k.c.border_weak)
                    .clip_rounded(k.radius.md)
                    .child(crate::scrollbars::v_bar(k, "log-scroll", col().key("log-scroll").w_full().h_full().scroll(ScrollSpec { stick_bottom: false }).child(lines_col)).w_full().h_full())
            };

            col()
                .key("page-logs")
                .w_full()
                .h_full()
                .gap(10.0)
                .child(tools)
                .child(filter_input.build(k))
                .child(viewer)
        }
    };

    let main_scroll: El = if opt.tab == OptionsTab::Logs {
        col().key("opt-main-body").w_full().grow(1.0).min_h(0.0).padding(16.0).child(page)
    } else {
        let page = col().key("opt-main-scroll").w_full().h_full().scroll(ScrollSpec { stick_bottom: false }).child(col().w_full().padding(16.0).child(page));
        crate::scrollbars::v_bar(k, "opt-main-scroll", page).w_full().grow(1.0)
    };

    let footer = row()
        .key("opt-footer")
        .w_full()
        .h(50.0)
        .px(16.0)
        .gap(8.0)
        .center_y()
        .border_sides([1.0, 0.0, 0.0, 0.0], k.c.border_weak)
        .child(el().size(6.0, 6.0).radius(3.0).fill(k.c.success.fg))
        .child(text(t("Alterações salvas automaticamente", "Changes are saved automatically")).font(k.ty.caption()).color(k.c.text_faint).grow(1.0))
        // Reiniciar, while a setting waits for a new window: Mica turned on in a window made
        // without it.
        .child(if s.settings.mica && cfg!(windows) && !crate::STARTED_WITH_MICA.load(std::sync::atomic::Ordering::Relaxed) {
            Button::new(t("Reiniciar", "Restart")).variant(ButtonVariant::Neutral).on_press_tx(actions::restart).build(k).cursor(Cursor::Pointer)
        } else {
            el()
        })
        .child(Button::new(t("Concluído", "Done")).variant(ButtonVariant::Submit).on_press(close).build(k).cursor(Cursor::Pointer).default_button());

    let right_col = col().key("opt-right").grow(1.0).w(0.0).h_full().min_w(0.0).child(main_scroll).child(footer);

    let body = row()
        .key("opt-split")
        .w_full()
        .h(options_height(window_h))
        .border_sides([1.0, 0.0, 0.0, 0.0], k.c.border_weak)
        .child(nav)
        .child(right_col);

    Modal::new("options", t("Opções", "Options")).open(true).child(body).width(750.0).fit().on_close(close).build(k, layer);
}

/// Opções › Cache: what each cache takes, and Limpar cache with its confirmation and what it
/// couldn't delete. `card` and `head` are the page's card and title, as the other pages draw them.
fn cache_page(k: &Kit, page: &crate::caches::Page, card: El, head: El) -> El {
    let usage = page.usage;
    let size = |n: Option<u64>| match n {
        Some(n) => format_size(n),
        None => t("Calculando…", "Measuring…").into(),
    };
    let ink = |known: bool| if known { k.c.text_base } else { k.c.text_faint };
    let words = |title: &str, desc: String| {
        col()
            .grow(1.0)
            .w(0.0)
            .min_w(0.0)
            .gap(2.0)
            .child(text(title.to_string()).font(k.ty.text_small_medium).color(k.c.text_base))
            .child(text(desc).font(k.ty.caption()).color(k.c.text_muted).wrap_anywhere().w_full().min_w(0.0))
    };
    let line = |key: &'static str, title: &str, desc: String, bytes: Option<u64>| {
        row().key(key).w_full().px(14.0).py(11.0).gap(12.0).center_y().child(words(title, desc)).child(text(size(bytes)).font(k.ty.text_small).color(ink(bytes.is_some())))
    };
    let divider = || el().w_full().h(1.0).fill(k.c.border_weak);

    let total_note = if page.clearing {
        t("Limpando…", "Clearing…")
    } else if page.waiting != 0 && usage.is_some() {
        t("Atualizando…", "Updating…")
    } else {
        t("As pastas abaixo somadas.", "The folders below added up.")
    };
    let total = row()
        .key("cache-total")
        .w_full()
        .px(14.0)
        .py(12.0)
        .gap(12.0)
        .center_y()
        .child(words(t("Espaço usado pelo cache", "Space used by the cache"), total_note.into()))
        .child(text(size(usage.map(crate::caches::Usage::total))).font(k.ty.body_medium).color(ink(usage.is_some())));

    let mut clear = Button::new(if page.clearing { t("Limpando…", "Clearing…") } else { t("Limpar cache", "Clear cache") })
        .variant(ButtonVariant::Danger)
        .disabled(page.clearing || usage.is_some_and(|u| u.total() == 0));
    if !page.clearing {
        clear = clear.on_press(|s: &mut State| crate::caches::confirm(s, true));
    }
    let actions = row()
        .key("cache-actions")
        .w_full()
        .px(14.0)
        .py(11.0)
        .gap(12.0)
        .center_y()
        .child(
            text(t("Contas, sessões e configurações não são apagadas.", "Accounts, sessions and settings aren't deleted."))
                .font(k.ty.caption())
                .color(k.c.text_muted)
                .wrap()
                .grow(1.0)
                .w(0.0)
                .min_w(0.0),
        )
        .child(clear.build(k).cursor(Cursor::Pointer).key("cache-clear"));

    let card = card
        .child(total)
        .child(divider())
        .child(line("cache-drive", t("Listagens do Google Drive", "Google Drive listings"), crate::cache::root().display().to_string(), usage.map(|u| u.drive)))
        .child(divider())
        .child(line("cache-mega", t("Contas MEGA", "MEGA accounts"), openexplorer_mega::cache::dir().map_or_else(String::new, |d| d.display().to_string()), usage.map(|u| u.mega)))
        .child(divider())
        .child(line("cache-dropbox", t("Contas Dropbox", "Dropbox accounts"), openexplorer_dropbox::cache::dir().map_or_else(String::new, |d| d.display().to_string()), usage.map(|u| u.dropbox)))
        .child(line("cache-b2", t("Listagens do Backblaze B2", "Backblaze B2 listings"), openexplorer_b2::cache::dir().map_or_else(String::new, |d| d.display().to_string()), usage.map(|u| u.b2)))
        .child(line("cache-onedrive", t("Contas OneDrive", "OneDrive accounts"), openexplorer_onedrive::cache::dir().map_or_else(String::new, |d| d.display().to_string()), usage.map(|u| u.onedrive)))
        .child(line("cache-telegram", t("Contas do Telegram", "Telegram accounts"), openexplorer_telegram::cache::dir().map_or_else(String::new, |d| d.display().to_string()), usage.map(|u| u.telegram)))
        .child(divider())
        .child(actions);

    let mut out = col().key("page-cache").w_full().gap(12.0).child(head).child(card);

    if page.confirming {
        let ask = col()
            .key("cache-confirm")
            .role("alertdialog")
            .label(t("Limpar o cache?", "Clear the cache?"))
            .w_full()
            .px(14.0)
            .py(12.0)
            .gap(10.0)
            .radius(k.radius.md)
            .fill(k.alpha(k.c.warning.fg, if k.dark { 0.10 } else { 0.07 }))
            .border(1.0, k.alpha(k.c.warning.fg, 0.30))
            .child(text(t("Limpar o cache?", "Clear the cache?")).font(k.ty.text_small_medium).color(k.c.text_base))
            .child(
                text(t(
                    "As pastas do Google Drive e as contas MEGA apenas voltarão a ser lidas pela rede na próxima vez que forem abertas, o que pode demorar um pouco mais. Contas, sessões e configurações continuam como estão.",
                    "Google Drive folders and MEGA accounts will just be read from the network again the next time they're opened, which may take a little longer. Accounts, sessions and settings stay as they are.",
                ))
                .font(k.ty.caption())
                .color(k.c.text_muted)
                .wrap()
                .w_full()
                .min_w(0.0),
            )
            .child(
                row()
                    .w_full()
                    .gap(8.0)
                    .child(el().grow(1.0))
                    .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press(|s: &mut State| crate::caches::confirm(s, false)).build(k).cursor(Cursor::Pointer).key("cache-cancel"))
                    .child(Button::new(t("Limpar cache", "Clear cache")).variant(ButtonVariant::Danger).on_press(crate::caches::clear).build(k).cursor(Cursor::Pointer).key("cache-clear-yes")),
            );
        out = out.child(ask);
    }

    if let Some(e) = &page.error {
        out = out.child(
            row()
                .key("cache-error")
                .role("alert")
                .w_full()
                .px(12.0)
                .py(10.0)
                .radius(k.radius.md)
                .fill(k.alpha(k.c.danger.fg, if k.dark { 0.10 } else { 0.06 }))
                .border(1.0, k.alpha(k.c.danger.fg, 0.30))
                .child(text(e.clone()).font(k.ty.caption()).color(k.c.danger.fg).selectable(true).selection_color(k.selection()).wrap_anywhere().w_full().min_w(0.0)),
        );
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Transfer queue

/// The queue's columns, as in Air Explorer: each in Portuguese and in English.
const QUEUE_LABELS: [(&str, &str); 8] = [
    ("Operação", "Operation"),
    ("Origem", "Source"),
    ("Destino", "Destination"),
    ("Estado", "Status"),
    ("Tamanho", "Size"),
    ("Progresso", "Progress"),
    ("Velocidade", "Speed"),
    ("Estimado", "Time left"),
];

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

/// The bar with its percentage over it, as in Air Explorer: both live values of the row, so
/// a transfer moving on redraws them without building the view again.
fn queue_bar(k: &Kit, shown: &transfers::Shown) -> El {
    let fill = el().key("fill").w_full().h_full().fill(k.alpha(k.c.text_accent, 0.35)).live_rect(&shown.fill);
    row()
        .key("bar")
        .role("progressbar")
        .label(t("Progresso", "Progress"))
        .range_value(0.0, 0.0, 1.0)
        .live_range_value(&shown.fraction)
        .w(0.0)
        .grow(1.0)
        .max_w(transfers::BAR_MAX)
        .h(transfers::BAR_H)
        .border(1.0, k.c.border_base)
        .fill(k.c.bg_base)
        .clip_soft(0.0)
        .child(fill)
        .child(row().pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0)).center().child(text(shown.percent.get().to_string()).w_full().align(Align::Center).font(k.ty.text_xsmall).color(k.c.text_base).live_text(&shown.percent)))
}

fn queue_row(s: &State, k: &Kit, t: &transfers::Transfer) -> El {
    let p = &t.progress;
    let w = s.layout.queue_cols;
    let muted = |t: String| text(t).font(k.ty.text_xsmall).color(k.c.text_base).truncate().shrink().min_w(0.0);
    // A live text paints inside the box it was laid out in: the cell's width, not its words'.
    let live = |value: &rune::LiveText| muted(value.get().to_string()).w(0.0).grow(1.0).live_text(value);
    let dot = match p.stage() {
        Stage::Waiting => k.c.text_faint,
        Stage::Running if p.cancelled() => k.c.warning.fg,
        Stage::Running => k.c.success.fg,
        Stage::Done => k.c.text_accent,
        Stage::Cancelled => k.c.warning.fg,
        Stage::Failed => k.c.danger.fg,
        Stage::Skipped => k.c.text_muted,
    };
    let failed = p.stage() == Stage::Failed;
    let label = live(&t.shown.state);
    // On hover, the words as the row was last built: a failure's stay as they are.
    let label = if failed { label.color(k.c.danger.fg) } else { label.help_when_truncated(t.state_text()) };
    let mut state_cell = queue_cell(w[3]).child(el().size(8.0, 8.0).radius(4.0).fill(dot)).child(label);
    if failed {
        state_cell = state_cell.help(p.error());
    }
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
        .child(queue_cell(w[4]).child(live(&t.shown.size)))
        .child(queue_cell(w[5]).child(queue_bar(k, &t.shown)))
        .child(queue_cell(w[6]).child(live(&t.shown.speed)))
        .child(queue_cell(w[7]).child(live(&t.shown.left)))
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
        .children(QUEUE_LABELS.iter().enumerate().map(|(i, &(pt, en))| {
            queue_cell(w[i]).border_sides([0.0, 1.0, 0.0, 0.0], k.c.border_weak).child(text(t(pt, en)).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().min_w(0.0))
        }))
        .children(grips);
    // Only the rows in view are built: copying a 5,000-file folder makes 5,000 rows.
    let kit = k.clone();
    let rows = col().key("queue-rows").w_full().growth(GrowthSpec::default()).virtual_rows(
        s.transfers.len(),
        22.0,
        |s: &State, i| Cow::Owned(format!("t-{:p}", std::sync::Arc::as_ptr(&s.transfers[i].progress))),
        move |s: &State, i| queue_row(s, &kit, &s.transfers[i]),
    );
    // One viewport scrolls both ways, as the file list's: sideways when the columns are wider
    // than the window, with the header stuck to the top.
    let table = col().key("queue-table").w_full().min_w(total).child(header).child(rows);
    let any_over = s.transfers.iter().any(|t| t.progress.over());
    let clear_btn = if any_over {
        k.interactive(
            row()
                .key("queue-clear")
                .role("button")
                .label(t("Limpar finalizadas", "Clear finished"))
                .focusable(true)
                .focus_ring(2.0, 1.0, k.c.border_focus)
                .h(18.0)
                .px(6.0)
                .center_y()
                .radius(k.radius.sm)
                .hover(k.c.hover)
                .press(k.c.pressed)
                .cursor(Cursor::Pointer),
        )
        .on_press(actions::clear_transfers)
        .child(text(t("Limpar finalizadas", "Clear finished")).font(k.ty.text_xsmall).color(k.c.text_muted))
    } else {
        el().key("queue-clear")
    };
    let queue = col()
        .key("queue")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .fill(k.c.bg_base)
        .border(1.0, k.c.border_weak)
        .clip_soft(0.0)
        .on_secondary_press(|s: &mut State, at: rune::PointerPress, _: &mut Tx| s.transfer_menu = Some((None, at)))
        // As in the file list: the bar runs below the header, beside the rows.
        .child(crate::scrollbars::xy_bars(k, "queue-viewport", col().key("queue-viewport").w_full().h_full().scroll_xy().child(table), QUEUE_HEADER_H, s.waker.clone()).w_full().grow(1.0))
        .child(
            row()
                .key("queue-foot")
                .w_full()
                .h(22.0)
                .px(6.0)
                .center_y()
                .border_sides([1.0, 0.0, 0.0, 0.0], k.c.border_weak)
                .child(clear_btn)
                .child(el().grow(1.0))
                .child(queue_totals(s, k)),
        );
    // The strip above the queue drags up and down to change its height. On a window too
    // short for it, the queue gives way rather than the panes.
    let edge = splitter(k, "queue-edge", 6.0, Split::Up, |s: &mut State| &mut s.layout.queue_h, layout::QUEUE_H, layout::clamp_queue_h);
    col().key("queue-wrap").w_full().h(s.layout.queue_h + 6.0).shrink().min_h(layout::QUEUE_H_MIN).px(6.0).child(edge).child(queue)
}

/// The queue's foot while something is under way: "Total: 0,05 / 2,66 GB", a bar, and the
/// speed of everything running.
fn queue_totals(s: &State, k: &Kit) -> El {
    if s.transfers_live == 0 {
        return el().key("totals");
    }
    let sums = &s.queue_totals;
    // Live texts paint inside the box they were laid out in: wide enough for the most they say.
    let small = |value: &rune::LiveText, w: f32| text(value.get().to_string()).w(w).align(Align::End).font(k.ty.text_xsmall).color(k.c.text_muted).live_text(value);
    let (bar_w, bar_h) = transfers::TOTAL_BAR;
    row()
        .key("totals")
        .gap(8.0)
        .center_y()
        .child(small(&sums.text, 190.0))
        .child(row().w(bar_w).h(bar_h).border(1.0, k.c.border_base).child(el().w_full().h_full().fill(k.alpha(k.c.text_accent, 0.5)).live_rect(&sums.fill)))
        .child(small(&sums.speed, 80.0))
}

/// A menu spaced and edged as Windows 11's: 4 points around its rows and on each side of a
/// separator (the kit's compact menu has 2), and a hairline border around it.
fn spaced(menu: rune_kit::Menu) -> rune_kit::Menu {
    menu.padding(4.0).separator_spacing(4.0, 4.0).inner_border(1.0)
}

/// Linux: the menu Rune asks a window to draw (a text field's Recortar, Copiar, Colar), as the
/// system draws none there. `clear` drops the window's request once it is answered.
fn fallback_menu(request: &rune::PopupMenuRequest, clear: fn(&mut State), k: &Kit, layer: &mut Vec<El>) {
    spaced(rune_kit::Menu::from_popup_menu("popup-fallback", request, move |s: &mut State, picked, tx: &mut Tx| {
        clear(s);
        tx.resolve_popup_menu(picked);
    }))
    .label(t("Menu de contexto", "Context menu"))
    .build(k, layer);
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
    let mut menu = spaced(context_menu("queue-menu", anchor, Some(at))).min_width(200.0);
    if let Some(row) = row {
        let (over, other) = (row.over(), row.clone());
        menu = menu
            .item(item(t("Cancelar", "Cancel"), !over, Box::new(move |_| actions::cancel_transfer(&row))))
            .item(item(t("Remover da lista", "Remove from list"), over, Box::new(move |s| actions::remove_transfer(s, &other))))
            .separator();
    }
    let (any_live, any_over) = (s.transfers.iter().any(|t| !t.progress.over()), s.transfers.iter().any(|t| t.progress.over()));
    menu.item(item(t("Cancelar todas", "Cancel all"), any_live, Box::new(actions::cancel_all_transfers)))
        .item(item(t("Limpar finalizadas", "Clear finished"), any_over, Box::new(actions::clear_transfers)))
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
    let mut menu = spaced(context_menu("address-menu", anchor, Some(at))).min_width(220.0).item(item(t("Copiar endereço", "Copy address"), &icons::M_COPY, "", actions::copy_address));
    if crate::address::drive_link(&s.panes[p].loc).is_some() {
        menu = menu.item(item(t("Copiar link do Drive", "Copy Drive link"), &icons::M_LINK, "", actions::copy_drive_link));
    }
    menu.item(item(t("Colar e ir", "Paste and go"), &icons::M_PASTE, "", actions::paste_and_go))
        .separator()
        .item(item(t("Editar endereço", "Edit address"), &icons::M_RENAME, "Ctrl+L", |s, p, _| actions::edit_address(s, p)))
        .on_close(|s: &mut State| s.address_menu = None)
        .build(k, layer)
}

/// The right-click menu of a node in the navigation tree: Expandir/Recolher, Recortar,
/// Copiar, Colar, Excluir, Renomear, Nova pasta, and Propriedades.
fn tree_context(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let anchor = el().key("tree-menu-anchor").size(0.0, 0.0);
    let Some((p, node, open, empty, at)) = s.tree_menu.clone() else { return anchor };

    let loc = node.to.clone();
    let parent_entry = loc.as_ref().and_then(actions::parent_and_entry);
    let can_expand = node.opens && !empty;
    // Windows' own folders for the user (the desktop, Documentos…) are copied, not moved,
    // deleted or renamed from here, as in Explorer's tree.
    let special = matches!(&loc, Some(Location::Local(path)) if s.places.is_special(path));
    let can_copy = parent_entry.as_ref().is_some_and(|(par, _)| actions::items_editable(par));
    let can_change = can_copy && !special;
    let can_cut = can_change && parent_entry.as_ref().is_some_and(|(par, _)| actions::editable(par));
    let can_paste = loc.as_ref().is_some_and(|to| actions::can_paste_to(s, to));
    let can_delete = can_change;
    let can_rename = can_change;
    let can_new_folder = loc.as_ref().is_some_and(actions::editable);
    let can_props = parent_entry.is_some();

    let toggle_label = if open { t("Recolher", "Collapse") } else { t("Expandir", "Expand") };
    let n_toggle = node.clone();
    let expand_item = MenuItem::new(toggle_label).disabled(!can_expand).on_select(move |s: &mut State| {
        s.tree_menu = None;
        crate::tree::toggle(s, p, &n_toggle);
    });

    let cut_pe = parent_entry.clone();
    let cut_item = MenuItem::new(t("Recortar", "Cut")).icon(&icons::M_CUT).disabled(!can_cut).on_select(move |s: &mut State| {
        s.tree_menu = None;
        if let Some((par, item)) = cut_pe.clone() {
            actions::pick_up_item(s, par, item, ClipMode::Cut);
        }
    });

    let copy_pe = parent_entry.clone();
    let copy_item = MenuItem::new(t("Copiar", "Copy")).icon(&icons::M_COPY).disabled(!can_copy).on_select(move |s: &mut State| {
        s.tree_menu = None;
        if let Some((par, item)) = copy_pe.clone() {
            actions::pick_up_item(s, par, item, ClipMode::Copy);
        }
    });

    let paste_loc = loc.clone();
    let paste_item = MenuItem::new(t("Colar", "Paste")).icon(&icons::M_PASTE).disabled(!can_paste).on_select(move |s: &mut State| {
        s.tree_menu = None;
        if let Some(to) = paste_loc.clone() {
            actions::paste_into(s, p, to);
        }
    });

    let del_pe = parent_entry.clone();
    let del_item = MenuItem::new(t("Excluir", "Delete")).icon(&icons::M_DELETE).disabled(!can_delete).danger(true).on_select(move |s: &mut State| {
        s.tree_menu = None;
        if let Some((par, item)) = del_pe.clone() {
            actions::ask_delete_item(s, p, par, item);
        }
    });

    let ren_pe = parent_entry.clone();
    let ren_item = MenuItem::new(t("Renomear", "Rename")).icon(&icons::M_RENAME).disabled(!can_rename).on_select(move |s: &mut State| {
        s.tree_menu = None;
        if let Some((par, item)) = ren_pe.clone() {
            actions::ask_rename_item(s, p, par, item);
        }
    });

    let nf_loc = loc.clone();
    let new_folder_item = MenuItem::new(t("Nova pasta", "New folder")).icon(&icons::M_NEW_FOLDER).disabled(!can_new_folder).on_select_tx(move |s: &mut State, tx: &mut Tx| {
        s.tree_menu = None;
        if let Some(to) = nf_loc.clone() {
            actions::ask_new_folder_in(s, p, to, tx);
        }
    });

    let props_pe = parent_entry;
    let props_item = MenuItem::new(t("Propriedades", "Properties")).icon(&icons::M_PROPERTIES).disabled(!can_props).on_select(move |s: &mut State| {
        s.tree_menu = None;
        if let Some((par, item)) = props_pe.clone() {
            actions::properties_of(s, p, par, vec![item]);
        }
    });

    spaced(context_menu("tree-menu", anchor, Some(at)))
        .min_width(210.0)
        .item(expand_item)
        .separator()
        .item(cut_item)
        .item(copy_item)
        .item(paste_item)
        .separator()
        .item(del_item)
        .item(ren_item)
        .separator()
        .item(new_folder_item)
        .separator()
        .item(props_item)
        .on_close(|s: &mut State| s.tree_menu = None)
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
    let mut menu = spaced(context_menu("menu", anchor, Some(at))).min_width(220.0);
    // Lixeira: what can come back, or go for good.
    if actions::in_trash(&s.panes[p].loc) {
        // Wide enough for "Excluir permanentemente" and its key.
        menu = menu.min_width(270.0);
        if n > 0 {
            menu = menu
                .item(item(t("Abrir", "Open"), &icons::M_OPEN, "Enter", n == 1, actions::open))
                .separator()
                .item(item(t("Restaurar", "Restore"), &icons::M_RESTORE, "", true, |s, p, _| actions::restore(s, p)))
                .item(item(t("Excluir permanentemente", "Delete permanently"), &icons::M_DELETE, "Del", true, |s, p, _| actions::ask_purge(s, p)).danger(true))
                .separator()
                .item(item(t("Propriedades", "Properties"), &icons::M_PROPERTIES, "Alt+Enter", true, |s, p, _| actions::properties(s, p)))
                .separator();
        }
        menu = menu.item(item(t("Esvaziar lixeira", "Empty trash"), &icons::M_EMPTY_TRASH, "", true, |s, p, _| actions::ask_empty_trash(s, p)).danger(true));
        return menu.on_close(|s: &mut State| s.menu = None).build(k, layer);
    }
    let changeable = actions::items_editable(&s.panes[p].loc);
    if n > 0 {
        menu = menu.item(item(t("Abrir", "Open"), &icons::M_OPEN, "Enter", n == 1, actions::open));
        if actions::shows_results(&s.panes[p].loc) {
            menu = menu.item(item(t("Mostrar na pasta", "Show in folder"), &icons::M_SHOW_IN_FOLDER, "", n == 1, actions::show_in_folder));
        }
        menu = menu
            .separator()
            .item(item(t("Recortar", "Cut"), &icons::M_CUT, "Ctrl+X", editable, |s, p, _| actions::pick_up(s, p, ClipMode::Cut)))
            .item(item(t("Copiar", "Copy"), &icons::M_COPY, "Ctrl+C", changeable, |s, p, _| actions::pick_up(s, p, ClipMode::Copy)));
    }
    menu = menu.item(item(t("Colar", "Paste"), &icons::M_PASTE, "Ctrl+V", actions::can_paste(s, p), |s, p, _| actions::paste(s, p)));
    if n > 0 {
        let rename_label = if n > 1 { t("Renomear em massa…", "Batch rename…") } else { t("Renomear", "Rename") };
        menu = menu
            .separator()
            .item(item(t("Excluir", "Delete"), &icons::M_DELETE, "Del", changeable, |s, p, _| actions::ask_delete(s, p)).danger(true))
            .item(item(rename_label, &icons::M_RENAME, "F2", changeable, actions::ask_rename));
        if changeable && matches!(s.panes[p].loc, Location::Drive { .. } | Location::DriveSearch(_)) {
            let label = if actions::stars_to_add(s, p) { t("Adicionar estrela", "Add star") } else { t("Remover estrela", "Remove star") };
            menu = menu.item(item(label, &icons::M_STAR, "", true, |s, p, _| actions::star(s, p)));
        }
    }
    // Sharing, for one Drive item.
    if n == 1 && editable && matches!(s.panes[p].loc, Location::Drive { .. }) {
        menu = menu
            .separator()
            .item(item(t("Copiar link", "Copy link"), &icons::M_LINK, "", true, crate::sharing::copy_link))
            .item(item(t("Compartilhar…", "Share…"), &icons::M_SHARE, "", true, crate::sharing::ask_share));
    }
    // Public or sharing links, for one item of a cloud account that makes them.
    if n == 1
        && changeable
        && let Some((copy, remove)) = crate::accounts::links(&s.panes[p].loc)
    {
        menu = menu
            .separator()
            .item(item(t("Copiar link", "Copy link"), &icons::M_LINK, "", true, copy))
            .item(item(t("Remover link", "Remove link"), &icons::M_LINK, "", true, remove));
    }
    // Every version of a B2 item, for good.
    if n > 0 && changeable && crate::b2::at(&s.panes[p].loc).is_some() {
        menu = menu.min_width(250.0).item(item(t("Excluir todas as versões", "Delete every version"), &icons::M_DELETE, "", true, crate::b2::ask_purge).danger(true));
    }
    // A B2 bucket's large uploads that stopped midway.
    if crate::b2::menu_bucket(s, p).is_some() {
        menu = menu.separator().item(item(t("Envios inacabados…", "Unfinished uploads…"), &icons::M_PROPERTIES, "", true, crate::b2::ask_unfinished));
    }
    menu = menu.separator().item(item(t("Nova pasta", "New folder"), &icons::M_NEW_FOLDER, "Ctrl+Shift+N", editable, actions::ask_new_folder));
    // What was deleted from a Dropbox folder, to bring back.
    if crate::dropbox::shows_folder(s, p) {
        menu = menu.item(item(t("Itens excluídos…", "Deleted items…"), &icons::M_RESTORE, "", true, crate::dropbox::show_deleted));
    }
    if n > 0 {
        menu = menu.separator().item(item(t("Propriedades", "Properties"), &icons::M_PROPERTIES, "Alt+Enter", true, |s, p, _| actions::properties(s, p)));
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
        .child(Button::new(cancel).variant(ButtonVariant::Neutral).on_press(close).build(k).cursor(Cursor::Pointer))
        .child(act.build(k).default_button().cursor(Cursor::Pointer))
}

/// Fechar o Open Explorer?, while transfers run (see [`actions::close_requested`]): over any
/// dialog open, which stays as it was.
fn quit_question(s: &State, k: &Kit, layer: &mut Vec<El>) {
    if !s.quit_asked {
        return;
    }
    let pad = |e: El| col().key("content").w_full().px(16.0).pb(16.0).child(e);
    // Reiniciar asks the same question in its own words.
    let restart = actions::restarting();
    let note = match (s.transfers.iter().filter(|t| !t.progress.over()).count(), restart) {
        (1, false) => t(
            "Uma transferência ainda está em andamento. Se fechar agora, ela será cancelada.",
            "A transfer is still in progress. If you close now, it will be cancelled.",
        )
        .to_string(),
        (1, true) => t(
            "Uma transferência ainda está em andamento. Se reiniciar agora, ela será cancelada.",
            "A transfer is still in progress. If you restart now, it will be cancelled.",
        )
        .to_string(),
        (n, false) => tr!(
            "{} transferências ainda estão em andamento. Se fechar agora, elas serão canceladas.",
            "{} transfers are still in progress. If you close now, they will be cancelled.",
            ui_count(n)
        ),
        (n, true) => tr!(
            "{} transferências ainda estão em andamento. Se reiniciar agora, elas serão canceladas.",
            "{} transfers are still in progress. If you restart now, they will be cancelled.",
            ui_count(n)
        ),
    };
    let (title, go) = if restart {
        (t("Reiniciar o Open Explorer?", "Restart Open Explorer?"), t("Reiniciar mesmo assim", "Restart anyway"))
    } else {
        (t("Fechar o Open Explorer?", "Close Open Explorer?"), t("Fechar mesmo assim", "Close anyway"))
    };
    let body = col()
        .w_full()
        .gap(12.0)
        .child(text(note).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0))
        .child(
            row()
                .key("actions")
                .w_full()
                .pt(16.0)
                .gap(8.0)
                .justify_end()
                .child(Button::new(t("Continuar transferindo", "Keep transferring")).variant(ButtonVariant::Neutral).on_press(actions::keep_running).build(k).default_button().cursor(Cursor::Pointer))
                .child(Button::new(go).variant(ButtonVariant::Danger).on_press_tx(actions::quit_anyway).build(k).cursor(Cursor::Pointer)),
        );
    Modal::new("quit", title).open(true).child(pad(body)).width(460.0).fit().on_close(actions::keep_running).build(k, layer);
}

/// `backdrop`: Windows shows Mica behind the main window now.
fn dialog(s: &State, k: &Kit, window_h: Option<f32>, backdrop: bool, layer: &mut Vec<El>) {
    let Some(d) = &s.dialog else { return };
    let pad = |e: El| col().key("content").w_full().px(16.0).pb(16.0).child(e);
    match d {
        Dialog::Delete { loc, items, .. } => {
            let local = loc.is_local();
            let title = match items.as_slice() {
                [one] => tr!("Excluir \"{}\"?", "Delete \"{}\"?", shown(&one.name)),
                many => tr!("Excluir {} itens?", "Delete {} items?", ui_count(many.len())),
            };
            let where_to: Cow<str> = if local {
                t("Os itens vão para a Lixeira do Windows, de onde podem ser restaurados.", "The items go to the Windows Recycle Bin, where they can be restored.").into()
            } else if let Some(note) = crate::accounts::delete_note(s, loc) {
                note
            } else {
                t(
                    "Os itens vão para a lixeira do Google Drive, onde ficam 30 dias antes de serem apagados de vez.",
                    "The items go to the Google Drive trash, where they stay for 30 days before being deleted for good.",
                )
                .into()
            };
            // The explanation goes in the body, where it wraps; the dialog's own description
            // line doesn't, and would run under the close button.
            let note = text(where_to.into_owned()).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0);
            let body = col().w_full().gap(12.0).child(note).child(item_list(s, k, loc, items)).child(actions_row(k, t("Cancelar", "Cancel"), Button::new(t("Excluir", "Delete")).variant(ButtonVariant::Danger).on_press(actions::delete)));
            Modal::new("delete", title).open(true).child(pad(body)).width(460.0).fit().on_close(close).build(k, layer);
        }
        Dialog::Purge { loc, items, .. } => {
            let title = match items.as_slice() {
                [one] => tr!("Excluir \"{}\" permanentemente?", "Permanently delete \"{}\"?", shown(&one.name)),
                many => tr!("Excluir {} itens permanentemente?", "Permanently delete {} items?", ui_count(many.len())),
            };
            let remote = items.iter().filter_map(|e| loc.local_path(e)).any(|path| crate::network::is_remote(&path));
            let note: Cow<str> = if loc.is_local() && !remote && s.settings.skip_recycle_bin {
                t(
                    "A exclusão direta está ativada nas Opções: os itens serão apagados para sempre, sem passar pela Lixeira. Isso não pode ser desfeito.",
                    "Permanent deletion is on in Options: the items will be deleted for good, skipping the Recycle Bin. This can't be undone.",
                )
                .into()
            } else if loc.is_local() {
                t("Aqui não há Lixeira: os itens serão apagados para sempre. Isso não pode ser desfeito.", "There's no Recycle Bin here: the items will be deleted for good. This can't be undone.").into()
            } else if let Some(note) = crate::accounts::purge_note(loc) {
                note.into()
            } else if let Location::Server { server, .. } = loc {
                tr!(
                    "Servidores não têm lixeira: os itens serão apagados para sempre de {}. Isso não pode ser desfeito.",
                    "Servers have no trash: the items will be deleted from {} for good. This can't be undone.",
                    server.name
                )
                .into()
            } else {
                t("Os itens serão apagados para sempre do Google Drive. Isso não pode ser desfeito.", "The items will be deleted from Google Drive for good. This can't be undone.").into()
            };
            let body = col()
                .w_full()
                .gap(12.0)
                .child(text(note.into_owned()).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0))
                .child(item_list(s, k, loc, items))
                .child(actions_row(k, t("Cancelar", "Cancel"), Button::new(t("Excluir permanentemente", "Delete permanently")).variant(ButtonVariant::Danger).on_press(actions::purge)));
            Modal::new("purge", title).open(true).child(pad(body)).width(460.0).fit().on_close(close).build(k, layer);
        }
        Dialog::EmptyTrash { email } if email.starts_with("mega:") => {
            let note = tr!(
                "Todos os itens da Lixeira do MEGA de {} serão apagados para sempre. Isso não pode ser desfeito.",
                "Everything in the MEGA Rubbish Bin of {} will be deleted for good. This can't be undone.",
                email.trim_start_matches("mega:")
            );
            let body = col()
                .w_full()
                .gap(12.0)
                .child(text(note).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0))
                .child(actions_row(k, t("Cancelar", "Cancel"), Button::new(t("Esvaziar lixeira", "Empty trash")).variant(ButtonVariant::Danger).on_press(actions::empty_trash)));
            Modal::new("empty-trash", t("Esvaziar a Lixeira do MEGA?", "Empty the MEGA Rubbish Bin?")).open(true).child(pad(body)).width(460.0).fit().on_close(close).build(k, layer);
        }
        Dialog::EmptyTrash { email } if email.starts_with("onedrive:") => {
            let who = s.servers.list.iter().find(|sv| sv.id == *email).map_or(email.as_str(), |sv| sv.name.as_str());
            let note = tr!(
                "Todos os itens que a lixeira do OneDrive de {who} mostra serão apagados para sempre. Isso não pode ser desfeito.",
                "Everything the OneDrive recycle bin of {who} shows will be deleted for good. This can't be undone."
            );
            let body = col()
                .w_full()
                .gap(12.0)
                .child(text(note).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0))
                .child(actions_row(k, t("Cancelar", "Cancel"), Button::new(t("Esvaziar lixeira", "Empty trash")).variant(ButtonVariant::Danger).on_press(actions::empty_trash)));
            Modal::new("empty-trash", t("Esvaziar a lixeira do OneDrive?", "Empty the OneDrive recycle bin?")).open(true).child(pad(body)).width(460.0).fit().on_close(close).build(k, layer);
        }
        Dialog::EmptyTrash { email } => {
            let note = tr!(
                "Todos os itens da lixeira do Meu Drive de {email} serão apagados para sempre. Isso não pode ser desfeito. Itens na lixeira de drives compartilhados continuam lá.",
                "Everything in the My Drive trash of {email} will be deleted for good. This can't be undone. Items in the trash of shared drives stay there."
            );
            let body = col()
                .w_full()
                .gap(12.0)
                .child(text(note).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0))
                .child(actions_row(k, t("Cancelar", "Cancel"), Button::new(t("Esvaziar lixeira", "Empty trash")).variant(ButtonVariant::Danger).on_press(actions::empty_trash)));
            Modal::new("empty-trash", t("Esvaziar lixeira?", "Empty trash?")).open(true).child(pad(body)).width(460.0).fit().on_close(close).build(k, layer);
        }
        Dialog::RemoveAccount { provider, who, .. } => {
            let title = tr!("Remover conta {provider}?", "Remove {provider} account?");
            let note = if who.is_empty() {
                tr!(
                    "Esta conta ({provider}) será desconectada do Open Explorer neste computador. Seus arquivos na nuvem ou no servidor continuam intactos.",
                    "This account ({provider}) will be disconnected from Open Explorer on this computer. Your files in the cloud or on the server stay untouched."
                )
            } else {
                tr!(
                    "A conta {who} ({provider}) será desconectada do Open Explorer neste computador. Seus arquivos na nuvem ou no servidor continuam intactos.",
                    "The account {who} ({provider}) will be disconnected from Open Explorer on this computer. Your files in the cloud or on the server stay untouched."
                )
            };
            let body = col()
                .w_full()
                .gap(12.0)
                .child(text(note).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0))
                .child(actions_row(k, t("Cancelar", "Cancel"), Button::new(t("Remover conta", "Remove account")).variant(ButtonVariant::Danger).on_press(actions::confirm_remove_account)));
            Modal::new("remove-account", title).open(true).child(pad(body)).width(460.0).fit().on_close(close).build(k, layer);
        }
        Dialog::NewFolder { error, .. } | Dialog::Rename { error, .. } => {
            let is_rename = matches!(d, Dialog::Rename { .. });
            // Rune's own field, so it can take the dialog's first focus (`autofocus`); Enter
            // confirms.
            let field = rune::text_input(&s.name_edit, k.ty.ui_compact)
                .single_line()
                .key("name-input")
                .label(t("Nome", "Name"))
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
            let act_label = if is_rename { t("Renomear", "Rename") } else { t("Criar", "Create") };
            let modal_title = if is_rename { t("Renomear", "Rename") } else { t("Nova pasta", "New folder") };
            let act = Button::new(act_label).variant(ButtonVariant::Submit).on_press(actions::submit_name);
            let body = body.child(actions_row(k, t("Cancelar", "Cancel"), act));
            Modal::new("name", modal_title).open(true).child(pad(body)).width(420.0).fit().on_close(close).build(k, layer);
        }
        Dialog::Properties(p) => props_dialog(s, k, p, layer),
        Dialog::Options(opt) => options_dialog(s, k, opt, window_h, backdrop, layer),
        Dialog::Conflict(prompt) => conflict_dialog(k, prompt, layer),
        Dialog::Share(sh) => crate::sharing::share_dialog(s, k, sh, layer),
        Dialog::MakePublic(m) => crate::sharing::public_dialog(k, m, layer),
        Dialog::MegaLink(m) => crate::mega::link_dialog(k, m, layer),
        Dialog::DropboxLink(m) => crate::dropbox::link_dialog(k, m, layer),
        Dialog::DropboxDeleted(v) => crate::dropbox::deleted_dialog(k, v, layer),
        Dialog::OneDriveLink(m) => crate::onedrive::link_dialog(k, m, layer),
        Dialog::B2Unfinished(d) => crate::b2::unfinished_dialog(k, d, layer),
        Dialog::Update => update_dialog(s, k, layer),
    }
}

fn update_dialog(s: &State, k: &Kit, layer: &mut Vec<El>) {
    use crate::updater::{DownloadState, Offer};

    let Some(rel) = &s.updater.release else { return };
    let pad = |e: El| col().key("content").w_full().px(16.0).pb(16.0).child(e);
    let version_line = text(format!("v{} → v{}", crate::updater::VERSION, rel.version))
        .font(k.ty.text_small_medium)
        .color(k.c.text_muted);

    let mut body = col().w_full().gap(12.0).child(version_line);

    if !rel.notes.is_empty() {
        let notes_box = col()
            .key("update-notes")
            .w_full()
            .max_h(150.0)
            .scroll(ScrollSpec { stick_bottom: false })
            .px(12.0)
            .py(10.0)
            .radius(k.radius.md)
            .fill(k.alpha(k.c.text_base, if k.dark { 0.04 } else { 0.03 }))
            .border(1.0, k.c.border_weak)
            .child(text(rel.notes.clone()).font(k.ty.caption()).color(k.c.text_muted).wrap().w_full().min_w(0.0));
        body = body.child(notes_box);
    }

    let mut primary: Option<El> = None;
    match &s.updater.download {
        DownloadState::Downloading { received, total } => {
            let checking = *total > 0 && received >= total;
            let status_label = if checking {
                t("Verificando download…", "Checking download…")
            } else {
                t("Baixando atualização…", "Downloading update…")
            };
            let frac = if *total > 0 { (*received as f32 / *total as f32).clamp(0.0, 1.0) } else { 0.0 };
            let track = row()
                .w_full()
                .h(6.0)
                .radius(3.0)
                .fill(k.alpha(k.c.text_base, if k.dark { 0.14 } else { 0.10 }))
                .child(el().w(frac * 412.0).h(6.0).radius(3.0).fill(k.c.text_accent));
            let mut progress_col = col()
                .w_full()
                .gap(6.0)
                .child(text(status_label).font(k.ty.text_small_medium).color(k.c.text_base))
                .child(track);
            if *total > 0 {
                let rec_mb = format!("{:.1}", *received as f64 / 1_000_000.0);
                let tot_mb = format!("{:.1}", *total as f64 / 1_000_000.0);
                progress_col = progress_col.child(
                    text(tr!("{rec_mb} de {tot_mb} MB", "{rec_mb} of {tot_mb} MB"))
                        .font(k.ty.caption())
                        .color(k.c.text_muted),
                );
            }
            body = body.child(progress_col);
        }
        DownloadState::Ready(_) => {
            let mut ready_col = col()
                .w_full()
                .gap(4.0)
                .child(text(t("Pronto para instalar", "Ready to install")).font(k.ty.text_small_medium).color(k.c.text_base));
            if s.updater.offer(s.settings.auto_update) == Some(Offer::Automatic) {
                ready_col = ready_col.child(
                    text(t(
                        "Se você não reiniciar agora, a atualização será instalada na próxima vez que o Open Explorer abrir.",
                        "If you don't restart now, the update will install the next time Open Explorer starts.",
                    ))
                    .font(k.ty.caption())
                    .color(k.c.text_muted)
                    .wrap()
                    .w_full()
                    .min_w(0.0),
                );
            }
            body = body.child(ready_col);
            primary = Some(
                Button::new(t("Reiniciar para atualizar", "Restart to update"))
                    .variant(ButtonVariant::Submit)
                    .on_press_tx(actions::install_update)
                    .build(k)
                    .cursor(Cursor::Pointer)
                    .key("update-restart"),
            );
        }
        DownloadState::Installing => {
            body = body.child(
                text(t("Preparando para reiniciar…", "Preparing to restart…"))
                    .font(k.ty.text_small)
                    .color(k.c.text_muted),
            );
        }
        DownloadState::Idle | DownloadState::Failed(_) => {
            if let DownloadState::Failed(err) = &s.updater.download {
                body = body.child(
                    text(err.clone())
                        .font(k.ty.text_small)
                        .color(k.c.danger.fg)
                        .wrap()
                        .w_full()
                        .min_w(0.0),
                );
            }
            match &s.updater.support {
                Ok(_) => {
                    let label = if matches!(s.updater.download, DownloadState::Failed(_)) {
                        t("Tentar novamente", "Retry download")
                    } else {
                        t("Baixar atualização", "Download update")
                    };
                    primary = Some(
                        Button::new(label)
                            .variant(ButtonVariant::Submit)
                            .on_press(actions::download_update)
                            .build(k)
                            .cursor(Cursor::Pointer)
                            .key("update-download"),
                    );
                }
                Err(reason) => {
                    body = body.child(
                        text(reason.clone())
                            .font(k.ty.text_small)
                            .color(k.c.text_muted)
                            .wrap()
                            .w_full()
                            .min_w(0.0),
                    );
                    let gh_url = rel.url.clone();
                    primary = Some(
                        Button::new(t("Baixar no GitHub", "Download from GitHub"))
                            .variant(ButtonVariant::Submit)
                            .on_press_tx(move |_: &mut State, tx: &mut Tx| {
                                let _ = tx.open_url(&gh_url);
                            })
                            .build(k)
                            .cursor(Cursor::Pointer)
                            .key("update-open-github"),
                    );
                }
            }
        }
    }

    let notes_url = rel.url.clone();
    let notes_link = k
        .interactive(
            row()
                .key("update-release-notes")
                .role("button")
                .label(t("Notas da versão", "Release notes"))
                .focusable(true)
                .focus_ring(2.0, 1.0, k.c.border_focus)
                .h(28.0)
                .px(8.0)
                .gap(5.0)
                .center_y()
                .radius(k.radius.sm)
                .hover(k.c.hover)
                .cursor(Cursor::Pointer),
        )
        .on_press_tx(move |_: &mut State, tx: &mut Tx| {
            let _ = tx.open_url(&notes_url);
        })
        .child(icon(&icons::EXTERNAL_LINK, 13.0).color(k.c.text_muted))
        .child(text(t("Notas da versão", "Release notes")).font(k.ty.text_small).color(k.c.text_muted));

    let mut footer = row().w_full().center_y().gap(8.0).child(notes_link).child(el().grow(1.0));
    footer = footer.child(
        Button::new(t("Fechar", "Close"))
            .variant(ButtonVariant::Neutral)
            .on_press(close)
            .build(k)
            .cursor(Cursor::Pointer)
            .key("update-close"),
    );
    if let Some(btn) = primary {
        footer = footer.child(btn);
    }
    body = body.child(footer);

    Modal::new("update", t("Atualizar o Open Explorer", "Update Open Explorer"))
        .open(true)
        .child(pad(body))
        .width(444.0)
        .fit()
        .on_close(close)
        .build(k, layer);
}

/// A certificate's fingerprint, which has no spaces to wrap at, as two halves on two lines.
fn fingerprint_lines(fp: &str) -> String {
    let groups: Vec<&str> = fp.split(':').collect();
    let half = groups.len().div_ceil(2);
    format!("{}\n{}", groups[..half].join(":"), groups[half..].join(":"))
}

/// How many accounts or servers of `provider` are already connected.
fn connected_count(s: &State, provider: crate::servers::Provider) -> usize {
    use crate::servers::Provider;
    match provider {
        Provider::GoogleDrive => s.accounts.len(),
        other => s.servers.list.iter().filter(|sv| Provider::of_server(sv) == other).count(),
    }
}

/// Independent "Registrar conta" / "Editar conta" native window:
/// - Custom sleek titlebar with leading icon + title and native minimize/maximize/close caption controls.
/// - `Step::Pick`: category pill tabs (`Todos`, `Pessoal`, `Empresarial`, `NAS`), top-right search bar
///   (`Pesquisar nuvens`), recessed scrollable container with `"Nuvens populares"` (multi-version
///   popover dropdowns on cards with >1 version) and `"Todas as nuvens A-Z · N"`, plus footer hint.
/// - `Step::Configure`: full-window configuration / OAuth / credentials form with `← Voltar`.
pub fn account_window_view(s: &State, cx: &mut Cx<State>) -> El {
    if s.is_placeholder {
        return el();
    }
    let k = kit(s.settings.theme.dark(cx.system_dark()));
    let default_form;
    let f = match &s.account_form {
        Some(f) => f.as_ref(),
        None => {
            default_form = crate::servers::Form::new();
            &default_form
        }
    };

    let window_bg = if k.dark { rune::oklch(0.24, 0.0, 0.0) } else { k.c.page };
    let panel_bg = if k.dark { rune::oklch(0.198, 0.0, 0.0) } else { k.c.bg_base };
    let panel_border = if k.dark { rune::oklch(0.285, 0.0, 0.0) } else { k.c.border_weak };

    cx.caption_color(k.c.text_base);

    let win_title = account_window_title(f.editing.is_some());

    let title_leading = row()
        .gap(8.0)
        .center_y()
        .child(icon(&icons::TITLE_REGISTER, 15.0).color(k.c.text_base))
        .child(text(win_title).font(k.ty.text_small).color(k.c.text_base));

    let titlebar = rune_kit::WindowTitlebar::from_insets(cx.chrome_insets())
        .key("account-titlebar")
        .leading_padding(14.0)
        .fill(window_bg)
        .leading(title_leading)
        .build(&k);

    let win_w = cx.layout_with("account-window", |r| r[2]).unwrap_or(980.0);
    let mut layer = Vec::new();

    let content = if f.step == crate::servers::Step::Pick {
        account_pick_body(s, &k, f, win_w, panel_bg, panel_border, &mut layer)
    } else {
        account_configure_body(s, &k, f, panel_bg, panel_border)
    };
    // Linux: the fields' Recortar, Copiar, Colar (keys, codes, passwords), drawn in the window.
    if let Some(request) = &s.account_popup {
        fallback_menu(request, |s| s.account_popup = None, &k, &mut layer);
    }

    let root = col()
        .key("account-window")
        .w_full()
        .h_full()
        .fill(window_bg)
        .child(titlebar)
        .child(content)
        .hover_motion_within(
            if s.settings.hover_animated() { crate::anim::hover_curve() } else { Anim::INSTANT },
            if s.settings.hover_animated() { crate::anim::hover_curve() } else { Anim::INSTANT },
        );

    rune_kit::root(root, layer)
}

/// The account window's title: Editar conta for an account or server being edited.
pub fn account_window_title(editing: bool) -> &'static str {
    if editing { t("Editar conta", "Edit account") } else { t("Registrar conta", "Add account") }
}

fn close_account_win(s: &mut State, _: &mut Tx) {
    crate::telegram::abort_pending();
    s.close_account_form();
}

fn account_part(s: &mut State, pick: fn(&mut crate::servers::Form) -> &mut rune::TextEdit) -> &mut rune::TextEdit {
    match &mut s.account_form {
        Some(f) => pick(f),
        None => &mut s.name_edit,
    }
}

fn account_pick_body(
    s: &State,
    k: &Kit,
    f: &crate::servers::Form,
    win_w: f32,
    panel_bg: Color,
    panel_border: Color,
    layer: &mut Vec<El>,
) -> El {
    use crate::servers::Category;

    // Top bar: category tabs on the left + "Pesquisar nuvens" search input on the top right.
    let tab_active_bg = if k.dark { rune::oklch(0.31, 0.06, 252.0) } else { k.alpha(k.c.text_accent, 0.13) };
    let tab_hover_bg = if k.dark { rune::oklch(0.285, 0.0, 0.0) } else { k.alpha(k.c.text_base, 0.05) };

    let mut tabs = row().key("cat-tabs").gap(4.0).center_y();
    for cat in Category::ALL {
        let on = f.category == cat;
        let fill = if on { tab_active_bg } else { Color::TRANSPARENT };
        let icon_ink = if on {
            if k.dark { Color::WHITE } else { k.c.text_accent }
        } else {
            k.c.icon_muted
        };
        let text_ink = if on {
            if k.dark { Color::WHITE } else { k.c.text_base }
        } else {
            if k.dark { rune::oklch(0.84, 0.0, 0.0) } else { k.c.text_muted }
        };
        let mut pill = row()
            .key(format!("cat-{cat:?}"))
            .role("tab")
            .label(cat.label())
            .checked(on)
            .focusable(true)
            .focus_ring(2.0, 1.0, k.c.border_focus)
            .h(32.0)
            .px(12.0)
            .gap(7.0)
            .center_y()
            .radius(6.0)
            .fill(fill)
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| crate::servers::set_category(s, cat))
            .child(icon(cat.icon(), 14.0).color(icon_ink))
            .child(text(cat.label()).font(if on { k.ty.text_small_medium } else { k.ty.text_small }).color(text_ink));
        if !on {
            pill = pill.hover(tab_hover_bg);
        }
        tabs = tabs.child(pill);
    }

    // Trailing magnifier (or clear button + magnifier when text is typed), matching the reference's top-right search bar.
    let search_trailing = if f.filter.text.is_empty() {
        row()
            .size(22.0, 22.0)
            .center()
            .child(icon(&icons::SEARCH, 13.5).color(k.c.icon_muted))
    } else {
        row()
            .gap(2.0)
            .center_y()
            .child(
                row()
                    .key("clear-search")
                    .role("button")
                    .label(t("Limpar pesquisa", "Clear search"))
                    .size(20.0, 20.0)
                    .center()
                    .radius(4.0)
                    .hover(k.c.hover)
                    .cursor(Cursor::Pointer)
                    .on_press(crate::servers::clear_filter)
                    .child(icon(k.glyph.close, 12.0).color(k.c.icon_muted)),
            )
            .child(row().size(20.0, 20.0).center().child(icon(&icons::SEARCH, 13.5).color(k.c.icon_muted)))
    };

    let search_w = if win_w < 760.0 { 200.0 } else { 256.0 };
    let search_box = rune_kit::TextInput::new(&f.filter)
        .width(Some(search_w))
        .label(t("Pesquisar nuvens", "Search clouds"))
        .placeholder(t("Pesquisar nuvens", "Search clouds"))
        .trailing(search_trailing)
        .on_edit(|s: &mut State| account_part(s, |f| &mut f.filter))
        .on_change(|s: &mut State, _: &mut Tx| crate::servers::close_group(s))
        .on_submit(|s: &mut State, _: &mut Tx| crate::servers::submit_filter(s))
        .build(k)
        .key("cloud-search");

    let top_bar = row()
        .key("picker-topbar")
        .w_full()
        .px(18.0)
        .pt(4.0)
        .pb(10.0)
        .gap(12.0)
        .center_y()
        .child(tabs)
        .child(el().grow(1.0))
        .child(search_box);

    // Responsive column counts as the independent window is resized.
    let pop_cols = if win_w >= 860.0 {
        4
    } else if win_w >= 640.0 {
        3
    } else {
        2
    };
    let all_cols = if win_w >= 920.0 {
        5
    } else if win_w >= 760.0 {
        4
    } else {
        3
    };

    let popular = f.visible_popular();
    let variants = f.visible_variants();

    let card_bg = if k.dark { rune::oklch(0.285, 0.0, 0.0) } else { k.alpha(k.c.black, 0.025) };
    let card_border = if k.dark { rune::oklch(0.34, 0.0, 0.0) } else { k.c.border_weak };
    let card_hover = if k.dark { rune::oklch(0.32, 0.0, 0.0) } else { k.alpha(k.c.black, 0.05) };
    let card_active_bg = if k.dark { rune::oklch(0.315, 0.048, 250.0) } else { k.alpha(k.c.text_accent, 0.12) };
    let card_active_border = if k.dark { rune::oklch(0.43, 0.065, 250.0) } else { k.alpha(k.c.text_accent, 0.45) };
    let accent_pill = if k.dark { rune::oklch(0.76, 0.135, 240.0) } else { k.c.text_accent };
    let dropdown_bg = if k.dark { rune::oklch(0.275, 0.0, 0.0) } else { k.c.surface_raised };
    let dropdown_border = if k.dark { rune::oklch(0.38, 0.0, 0.0) } else { k.c.border_base };
    let dropdown_hover = if k.dark { rune::oklch(0.33, 0.0, 0.0) } else { k.c.hover };

    let mut scroll_inner = col().key("catalog-sections").w_full().px(22.0).py(18.0).gap(22.0);

    if variants.is_empty() {
        let trimmed = f.filter.text.trim().trim_matches('"');
        let is_url = crate::servers::parse_address(trimmed).is_some();
        let msg = if is_url {
            tr!("Endereço “{trimmed}” reconhecido. Clique abaixo ou pressione Enter para configurar.", "Address “{trimmed}” recognized. Click below or press Enter to set it up.")
        } else {
            tr!("Nenhuma nuvem encontrada para “{}”.", "No cloud found for “{}”.", f.filter.text.trim())
        };
        let act = if is_url {
            Button::new(t("Configurar endereço", "Set up address")).variant(ButtonVariant::Submit).on_press(crate::servers::submit_filter).build(k).cursor(Cursor::Pointer).default_button()
        } else {
            Button::new(t("Limpar pesquisa", "Clear search")).variant(ButtonVariant::Neutral).on_press(crate::servers::clear_filter).build(k).cursor(Cursor::Pointer)
        };
        scroll_inner = scroll_inner.child(
            col()
                .key("empty-grid")
                .w_full()
                .min_h(240.0)
                .padding(24.0)
                .gap(12.0)
                .center()
                .child(text(msg).font(k.ty.text_small).color(k.c.text_muted).align(Align::Center).wrap().w_full())
                .child(act),
        );
    } else {
        // Section 1: "Nuvens populares"
        if !popular.is_empty() {
            let mut pop_grid = col().key("popular-grid").w_full().gap(10.0);
            for (r_idx, chunk) in popular.chunks(pop_cols).enumerate() {
                let mut line = row().key(format!("pop-row-{r_idx}")).w_full().gap(10.0);
                for (provider, versions) in chunk {
                    let provider = *provider;
                    let multi = versions.len() > 1;
                    let is_open = multi && f.open_group == Some(provider);
                    let anchor_key = format!("pop-card-{}", provider.label());
                    let connected = connected_count(s, provider);

                    let labels_col = if multi {
                        let ver_txt = tr!("{} versões", "{} versions", versions.len());
                        col()
                            .grow(1.0)
                            .min_w(0.0)
                            .gap(1.0)
                            .child(text(provider.label()).font(k.ty.text_small_medium).color(k.c.text_base).truncate().min_w(0.0))
                            .child(text(ver_txt).font(k.ty.caption()).color(k.c.text_muted).truncate().min_w(0.0))
                    } else {
                        col()
                            .grow(1.0)
                            .min_w(0.0)
                            .child(text(provider.label()).font(k.ty.text_small_medium).color(k.c.text_base).truncate().min_w(0.0))
                    };

                    let mut card = row()
                        .key(anchor_key.clone())
                        .role("button")
                        .label(provider.label())
                        .expanded(is_open)
                        .focusable(true)
                        .focus_ring(2.0, 1.0, k.c.border_focus)
                        .grow(1.0)
                        .w(0.0)
                        .h(58.0)
                        .pl(16.0)
                        .pr(14.0)
                        .gap(12.0)
                        .center_y()
                        .radius(6.0)
                        .fill(if is_open { card_active_bg } else { card_bg })
                        .border(1.0, if is_open { card_active_border } else { card_border })
                        .cursor(Cursor::Pointer)
                        .on_press(move |s: &mut State| crate::servers::toggle_group(s, provider))
                        .child(icon(provider.icon(), 26.0))
                        .child(labels_col);

                    if !is_open {
                        card = card.hover(card_hover);
                    }

                    if connected > 0 && !multi {
                        card = card.child(
                            el()
                                .size(6.0, 6.0)
                                .radius(3.0)
                                .fill(k.c.success.fg),
                        );
                    }

                    if multi {
                        card = card.child(icon(&icons::CHEVRON_DOWN, 13.0).color(if is_open { k.c.text_base } else { k.c.icon_muted }));
                    }

                    // Active left vertical accent indicator pill (matching the reference's expanded card).
                    if is_open {
                        card = card.child(
                            el()
                                .key("active-bar")
                                .pin(Some(0.0), Some(20.0), None, None)
                                .size(3.0, 18.0)
                                .radius(1.5)
                                .fill(accent_pill),
                        );

                        // Floating dropdown menu anchored beneath the card.
                        layer.push(rune_kit::scrim("group-menu-scrim", crate::servers::close_group));
                        let mut menu_rows = col().key("group-menu-list").w(316.0).padding(6.0).gap(2.0);
                        for &v in versions {
                            let opt = row()
                                .key(format!("opt-{}", v.label()))
                                .role("menuitem")
                                .label(format!("{} — {}", v.label(), v.description()))
                                .focusable(true)
                                .focus_ring(2.0, 1.0, k.c.border_focus)
                                .w_full()
                                .px(12.0)
                                .py(9.0)
                                .gap(12.0)
                                .center_y()
                                .radius(6.0)
                                .hover(dropdown_hover)
                                .cursor(Cursor::Pointer)
                                .on_press(move |s: &mut State| crate::servers::pick_variant(s, v))
                                .child(icon(v.icon(), 25.0))
                                .child(
                                    col()
                                        .grow(1.0)
                                        .min_w(0.0)
                                        .gap(2.0)
                                        .child(text(v.label()).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0))
                                        .child(text(v.description()).font(k.ty.caption()).color(k.c.text_muted).truncate().min_w(0.0)),
                                );
                            menu_rows = menu_rows.child(opt);
                        }
                        let surface = menu_rows
                            .radius(8.0)
                            .fill(dropdown_bg)
                            .border(1.0, dropdown_border)
                            .box_shadow(rune::Shadow { y: 8.0, blur: 24.0, color: k.dark_a(0.45), ..Default::default() });
                        layer.push(rune_kit::popover::anchored(k, surface, &anchor_key, rune_kit::Placement::BottomStart, 4.0));
                    }

                    line = line.child(card);
                }
                for filler in chunk.len()..pop_cols {
                    line = line.child(el().key(format!("pop-filler-{r_idx}-{filler}")).grow(1.0).w(0.0));
                }
                pop_grid = pop_grid.child(line);
            }

            scroll_inner = scroll_inner.child(
                col()
                    .key("sec-popular")
                    .w_full()
                    .gap(11.0)
                    .child(text(t("Nuvens populares", "Popular clouds")).font(k.ty.text_small_medium).color(k.c.text_base))
                    .child(pop_grid),
            );
        }

        // Section 2: "Todas as nuvens A-Z · N"
        let all_hover = if k.dark { rune::oklch(0.255, 0.0, 0.0) } else { k.alpha(k.c.black, 0.045) };
        let mut az_grid = col().key("az-grid").w_full().gap(4.0);
        for (r_idx, chunk) in variants.chunks(all_cols).enumerate() {
            let mut line = row().key(format!("az-row-{r_idx}")).w_full().gap(8.0);
            for &variant in chunk {
                let item = row()
                    .key(format!("az-{}", variant.label()))
                    .role("button")
                    .label(format!("{} — {}", variant.label(), variant.description()))
                    .focusable(true)
                    .focus_ring(2.0, 1.0, k.c.border_focus)
                    .grow(1.0)
                    .w(0.0)
                    .h(38.0)
                    .px(8.0)
                    .gap(10.0)
                    .center_y()
                    .radius(6.0)
                    .hover(all_hover)
                    .cursor(Cursor::Pointer)
                    .on_press(move |s: &mut State| crate::servers::pick_variant(s, variant))
                    .child(icon(variant.icon(), 21.0))
                    .child(text(variant.label()).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0).grow(1.0));
                line = line.child(item);
            }
            for filler in chunk.len()..all_cols {
                line = line.child(el().key(format!("az-filler-{r_idx}-{filler}")).grow(1.0).w(0.0));
            }
            az_grid = az_grid.child(line);
        }

        let az_head = row()
            .gap(6.0)
            .center_y()
            .child(text(t("Todas as nuvens", "All clouds")).font(k.ty.text_small_medium).color(k.c.text_base))
            .child(text(format!("A-Z · {}", variants.len())).font(k.ty.text_small).color(k.c.text_muted));

        scroll_inner = scroll_inner.child(
            col()
                .key("sec-all")
                .w_full()
                .gap(10.0)
                .child(az_head)
                .child(az_grid),
        );
    }

    let scroll_panel = col()
        .key("picker-panel-wrap")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .px(12.0)
        .child(
            col()
                .key("picker-panel")
                .w_full()
                .h_full()
                .min_h(0.0)
                .radius(8.0)
                .fill(panel_bg)
                .border(1.0, panel_border)
                .clip_rounded(8.0)
                .child(
                    crate::scrollbars::v_bar(
                        k,
                        "picker-scroll",
                        col().key("picker-scroll").w_full().h_full().min_h(0.0).scroll(ScrollSpec { stick_bottom: false }).child(scroll_inner),
                    )
                    .w_full()
                    .h_full(),
                ),
        );

    let waiting_oauth = s.login.is_some() || s.servers.onedrive.login.is_some() || s.servers.dropbox_login.is_some();
    let footer: El = if waiting_oauth {
        row()
            .key("picker-footer")
            .w_full()
            .h(34.0)
            .px(18.0)
            .gap(7.0)
            .center_y()
            .child(el().size(7.0, 7.0).radius(3.5).fill(k.c.text_accent))
            .child(text(t("Aguardando autorização no navegador…", "Waiting for authorization in the browser…")).font(k.ty.caption()).color(k.c.text_accent).truncate().min_w(0.0))
    } else {
        row()
            .key("picker-footer")
            .w_full()
            .h(34.0)
            .px(18.0)
            .center_y()
            .child(
                text(t(
                    "Não está na lista? Muitas nuvens funcionam via WebDAV, SFTP ou FTP.",
                    "Not on the list? Many clouds work via WebDAV, SFTP or FTP.",
                ))
                .font(k.ty.caption())
                .color(k.c.text_muted)
                .truncate()
                .min_w(0.0),
            )
    };

    col()
        .key("picker-body")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(top_bar)
        .child(scroll_panel)
        .child(footer)
}

fn account_configure_body(
    s: &State,
    k: &Kit,
    f: &crate::servers::Form,
    panel_bg: Color,
    panel_border: Color,
) -> El {
    use crate::servers::{Form, Provider};
    use openexplorer_remote::Protocol;

    let proto = f.effective_protocol();
    let (summary, badge_text) = match (f.provider, proto) {
        (Provider::GoogleDrive, _) => (t("Autorização segura OAuth 2.0 pelo navegador padrão", "Secure OAuth 2.0 sign-in through the default browser"), "OAuth 2.0"),
        (Provider::OneDrive, _) => (t("Contas pessoais, corporativas ou de estudante pelo navegador", "Personal, work or school accounts through the browser"), "OAuth 2.0"),
        (Provider::Mega, _) => (t("Criptografia de ponta a ponta · os arquivos são decifrados neste computador", "End-to-end encryption · files are decrypted on this computer"), "E2EE"),
        (Provider::Dropbox, _) => (t("Autorização segura OAuth 2.0 (PKCE) pelo navegador padrão", "Secure OAuth 2.0 (PKCE) sign-in through the default browser"), "OAuth 2.0"),
        (Provider::B2, _) => crate::b2::summary(),
        (Provider::Telegram, _) => crate::telegram::summary(),
        (Provider::Nextcloud, Protocol::Davs) => (t("Servidor Nextcloud · conexão criptografada", "Nextcloud server · encrypted connection"), "HTTPS"),
        (Provider::Nextcloud, _) => (t("Servidor Nextcloud · conexão sem criptografia", "Nextcloud server · unencrypted connection"), "HTTP"),
        (Provider::OwnCloud, Protocol::Davs) => (t("Servidor ownCloud · conexão criptografada", "ownCloud server · encrypted connection"), "HTTPS"),
        (Provider::OwnCloud, _) => (t("Servidor ownCloud · conexão sem criptografia", "ownCloud server · unencrypted connection"), "HTTP"),
        (Provider::Sftp, _) => (t("Transferência segura sobre SSH · porta padrão 22", "Secure transfer over SSH · default port 22"), "SSH"),
        (Provider::Ftp, Protocol::Ftps) => (t("FTP explícito ou implícito com criptografia TLS", "Explicit or implicit FTP with TLS encryption"), "FTPS"),
        (Provider::Ftp, _) => (t("FTP padrão · ative SSL/TLS abaixo se o servidor aceitar", "Standard FTP · turn on SSL/TLS below if the server supports it"), "FTP"),
        (Provider::WebDav, Protocol::Davs) => (t("Servidor WebDAV seguro · porta padrão 443", "Secure WebDAV server · default port 443"), "HTTPS"),
        (Provider::WebDav, _) => (t("Servidor WebDAV sem criptografia · porta padrão 80", "Unencrypted WebDAV server · default port 80"), "HTTP"),
        (Provider::Service(b), _) => (crate::services::of(b).map_or("WebDAV", |svc| svc.about()), "HTTPS"),
    };

    let badge_variant = if f.provider != Provider::GoogleDrive && proto.plain() {
        rune_kit::BadgeVariant::Neutral
    } else {
        rune_kit::BadgeVariant::Subtle
    };

    let banner_title = f.variant_label.unwrap_or_else(|| f.provider.label());
    let banner = row()
        .key("provider-banner")
        .w_full()
        .px(14.0)
        .py(11.0)
        .gap(12.0)
        .center_y()
        .radius(k.radius.md)
        .fill(k.c.bg_base)
        .border(1.0, k.c.border_weak)
        .child(icon(f.provider.icon(), 28.0))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(1.0)
                .child(text(banner_title).font(k.ty.text_small_medium).color(k.c.text_base))
                .child(text(summary).font(k.ty.caption()).color(k.c.text_muted).truncate().min_w(0.0)),
        )
        .child(rune_kit::Badge::new(badge_text).variant(badge_variant).standard().build(k));

    let eye_toggle = |key: &'static str, revealed: bool, flip: fn(&mut State)| -> El {
        row()
            .key(key)
            .role("button")
            .label(if revealed { t("Ocultar senha", "Hide password") } else { t("Mostrar senha", "Show password") })
            .preserve_focus_on_press(true)
            .focusable(true)
            .focus_ring(2.0, 1.0, k.c.border_focus)
            .size(22.0, 22.0)
            .center()
            .radius(k.radius.sm)
            .hover(k.c.hover)
            .cursor(Cursor::Pointer)
            .on_press(flip)
            .child(icon(if revealed { k.glyph.eye_off } else { k.glyph.eye }, 15.0).color(k.c.icon_muted))
    };

    let (inner, actions) = match f.provider {
        Provider::OneDrive => crate::onedrive::configure(s, k, f, banner, close_account_win),
        Provider::Dropbox => crate::dropbox::account_step(s, k, f, banner, close_account_win),
        Provider::B2 => {
            let eye = eye_toggle("pass-eye", f.reveal_password, |s| {
                if let Some(f) = &mut s.account_form {
                    f.reveal_password = !f.reveal_password;
                }
            });
            crate::b2::form(k, f, banner, eye, close_account_win)
        }
        Provider::Telegram => {
            let eye = eye_toggle("pass-eye", f.reveal_password, |s| {
                if let Some(f) = &mut s.account_form {
                    f.reveal_password = !f.reveal_password;
                }
            });
            crate::telegram::form(k, f, banner, eye)
        }
        Provider::GoogleDrive => {
            let show_oauth_form = f.custom_oauth;
            let mut inner = col()
                .w_full()
                .gap(12.0)
                .child(banner)
                .child(
                    text(t(
                        "Ao continuar, o navegador abrirá a página do Google para você escolher a conta e autorizar o acesso ao Meu Drive e aos Drives compartilhados.",
                        "When you continue, the browser opens Google's page so you can pick the account and allow access to My Drive and Shared drives.",
                    ))
                    .font(k.ty.text_small)
                    .color(k.c.text_muted)
                    .wrap()
                    .w_full()
                    .min_w(0.0),
                );

            if !s.accounts.is_empty() {
                let emails = s.accounts.iter().map(|a| a.email.as_str()).collect::<Vec<_>>().join(", ");
                inner = inner.child(
                    row()
                        .key("connected-drives")
                        .w_full()
                        .px(10.0)
                        .py(7.0)
                        .gap(6.0)
                        .center_y()
                        .radius(k.radius.sm)
                        .fill(k.alpha(k.c.text_accent, if k.dark { 0.10 } else { 0.06 }))
                        .child(el().size(6.0, 6.0).radius(3.0).fill(k.c.success.fg))
                        .child(text(tr!("Já conectadas: {emails}", "Already connected: {emails}")).font(k.ty.text_xsmall).color(k.c.text_base).truncate().min_w(0.0).grow(1.0)),
                );
            }

            if show_oauth_form {
                let id_input = rune_kit::TextInput::new(&f.client_id)
                    .width(None)
                    .label(t("Client ID (ou JSON completo)", "Client ID (or the full JSON)"))
                    .placeholder(t("123456-abc.apps.googleusercontent.com ou cole o JSON inteiro", "123456-abc.apps.googleusercontent.com or paste the whole JSON"))
                    .on_edit(|s: &mut State| account_part(s, |f| &mut f.client_id))
                    .on_submit(|s: &mut State, tx: &mut Tx| crate::servers::submit_google(s, tx));
                let secret_eye = eye_toggle("secret-eye", f.reveal_secret, |s| {
                    if let Some(f) = &mut s.account_form {
                        f.reveal_secret = !f.reveal_secret;
                    }
                });
                let secret_input = rune_kit::TextInput::new(&f.client_secret)
                    .width(None)
                    .label("Client Secret")
                    .placeholder(t("GOCSPX-… (dispensável se colar o JSON acima)", "GOCSPX-… (not needed if you paste the JSON above)"))
                    .revealed(f.reveal_secret)
                    .trailing(secret_eye)
                    .on_edit(|s: &mut State| account_part(s, |f| &mut f.client_secret))
                    .on_submit(|s: &mut State, tx: &mut Tx| crate::servers::submit_google(s, tx));
                let console_btn = Button::new(t("Abrir Google Cloud Console", "Open Google Cloud Console"))
                    .icon(&icons::EXTERNAL_LINK)
                    .variant(ButtonVariant::Neutral)
                    .on_press_tx(|s: &mut State, tx: &mut Tx| {
                        if let Err(e) = tx.open_url("https://console.cloud.google.com/apis/credentials") {
                            crate::fail(s, tr!("Não consegui abrir o navegador: {e:?}", "Couldn't open the browser: {e:?}"));
                        }
                    })
                    .build(k).cursor(Cursor::Pointer);
                let oauth_box = col()
                    .key("oauth-box")
                    .w_full()
                    .padding(12.0)
                    .gap(8.0)
                    .radius(k.radius.md)
                    .fill(k.c.bg_base)
                    .border(1.0, k.c.border_weak)
                    .child(
                        row()
                            .w_full()
                            .center_y()
                            .child(text(t("Cliente OAuth (App para computador)", "OAuth client (Desktop app)")).font(k.ty.text_small_medium).color(k.c.text_base).grow(1.0))
                            .child(console_btn),
                    )
                    .child(
                        text(t(
                            "Crie uma credencial OAuth do tipo “App para computador” no Google Cloud Console, ou cole o arquivo google-client.json diretamente abaixo:",
                            "Create an OAuth credential of type “Desktop app” in the Google Cloud Console, or paste the google-client.json file right below:",
                        ))
                        .font(k.ty.caption())
                        .color(k.c.text_muted)
                        .wrap()
                        .w_full()
                        .min_w(0.0),
                    )
                    .child(col().w_full().gap(4.0).child(text(t("Client ID ou JSON", "Client ID or JSON")).font(k.ty.text_xsmall_medium).color(k.c.text_base)).child(id_input.build(k)))
                    .child(col().w_full().gap(4.0).child(text("Client Secret").font(k.ty.text_xsmall_medium).color(k.c.text_base)).child(secret_input.build(k)));
                inner = inner.child(oauth_box);
            } else {
                let ready_row = row()
                    .key("oauth-ready")
                    .w_full()
                    .px(12.0)
                    .py(8.0)
                    .gap(8.0)
                    .center_y()
                    .radius(k.radius.md)
                    .fill(k.c.bg_base)
                    .border(1.0, k.c.border_weak)
                    .child(icon(k.glyph.lock, 15.0).color(k.c.success.fg))
                    .child(text(t("Cliente OAuth configurado e pronto para conectar.", "OAuth client set up and ready to connect.")).font(k.ty.text_xsmall).color(k.c.text_muted).grow(1.0).min_w(0.0))
                    .child(
                        Button::new(t("Alterar Client ID", "Change Client ID"))
                            .variant(ButtonVariant::GhostMuted)
                            .on_press(|s: &mut State| {
                                if let Some(f) = &mut s.account_form {
                                    f.custom_oauth = true;
                                    f.client_id.focused = true;
                                }
                            })
                            .build(k).cursor(Cursor::Pointer),
                    );
                inner = inner.child(ready_row);
            }

            if let Some(e) = &f.error {
                inner = inner.child(text(e.clone()).key("error").font(k.ty.text_small).color(k.c.danger.fg).wrap().w_full().min_w(0.0));
            }

            let actions = row()
                .key("actions")
                .w_full()
                .gap(8.0)
                .center_y()
                .child(Button::new(t("Voltar", "Back")).icon(&icons::BACK).variant(ButtonVariant::Neutral).on_press(crate::servers::back_to_pick).build(k).cursor(Cursor::Pointer))
                .child(el().grow(1.0))
                .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press_tx(close_account_win).build(k).cursor(Cursor::Pointer))
                .child(Button::new(t("Conectar no navegador", "Connect in browser")).variant(ButtonVariant::Submit).on_press_tx(crate::servers::submit_google).build(k).cursor(Cursor::Pointer).default_button());
            (inner, actions)
        }
        Provider::Mega => {
            let labeled = |key: &'static str, label: &'static str, input: El| col().key(key).w_full().gap(4.0).child(text(label).font(k.ty.text_small_medium).color(k.c.text_base)).child(input);
            let email_input = rune_kit::TextInput::new(&f.user)
                .width(None)
                .label(t("E-mail", "E-mail"))
                .placeholder(t("voce@exemplo.com", "you@example.com"))
                .on_edit(|s: &mut State| account_part(s, |f| &mut f.user))
                .on_submit(|s: &mut State, _: &mut Tx| crate::mega::submit(s))
                .build(k);
            let pass_eye = eye_toggle("pass-eye", f.reveal_password, |s| {
                if let Some(f) = &mut s.account_form {
                    f.reveal_password = !f.reveal_password;
                }
            });
            let pass_input = rune_kit::TextInput::new(&f.password)
                .width(None)
                .label(t("Senha", "Password"))
                .revealed(f.reveal_password)
                .trailing(pass_eye)
                .on_edit(|s: &mut State| account_part(s, |f| &mut f.password))
                .on_submit(|s: &mut State, _: &mut Tx| crate::mega::submit(s))
                .build(k);
            let mut inner = col()
                .w_full()
                .gap(12.0)
                .child(banner)
                .child(
                    text(t(
                        "O Open Explorer guarda só a sessão, no cofre de credenciais do sistema; a senha não fica salva em lugar nenhum.",
                        "Open Explorer keeps only the session, in the system's credential store; the password isn't saved anywhere.",
                    ))
                    .font(k.ty.text_small)
                    .color(k.c.text_muted)
                    .wrap()
                    .w_full()
                    .min_w(0.0),
                )
                .child(labeled("mega-email", t("E-mail", "E-mail"), email_input))
                .child(labeled("mega-password", t("Senha", "Password"), pass_input));
            if f.needs_code {
                let code_input = rune_kit::TextInput::new(&f.code)
                    .width(None)
                    .label(t("Código de verificação", "Verification code"))
                    .placeholder("123456")
                    .on_edit(|s: &mut State| account_part(s, |f| &mut f.code))
                    .on_submit(|s: &mut State, _: &mut Tx| crate::mega::submit(s))
                    .build(k);
                inner = inner.child(labeled("mega-code", t("Código de verificação (app autenticador)", "Verification code (authenticator app)"), code_input));
            }
            let checking = f.checking.is_some();
            if checking {
                let wait = t(
                    "Conectando… o MEGA pede uma verificação de segurança e a conta é lida inteira, o que leva alguns segundos.",
                    "Connecting… MEGA asks for a security check and the whole account is read, which takes a few seconds.",
                );
                inner = inner.child(text(wait).key("mega-wait").font(k.ty.text_xsmall).color(k.c.text_muted).wrap().w_full().min_w(0.0));
            }
            if let Some(e) = &f.error {
                inner = inner.child(text(e.clone()).key("error").font(k.ty.text_small).color(k.c.danger.fg).wrap().w_full().min_w(0.0));
            }
            let connect = if checking {
                Button::new(t("Conectando…", "Connecting…")).variant(ButtonVariant::Submit).disabled(true).build(k).cursor(Cursor::Pointer)
            } else {
                Button::new(t("Conectar", "Connect")).variant(ButtonVariant::Submit).on_press(crate::mega::submit).build(k).cursor(Cursor::Pointer).default_button()
            };
            let actions = row()
                .key("actions")
                .w_full()
                .gap(8.0)
                .center_y()
                .child(Button::new(t("Voltar", "Back")).icon(&icons::BACK).variant(ButtonVariant::Neutral).disabled(checking).on_press(crate::servers::back_to_pick).build(k).cursor(Cursor::Pointer))
                .child(el().grow(1.0))
                .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press_tx(close_account_win).build(k).cursor(Cursor::Pointer))
                .child(connect);
            (inner, actions)
        }
        Provider::Sftp | Provider::Ftp | Provider::WebDav | Provider::Nextcloud | Provider::OwnCloud | Provider::Service(_) => {
            let service = match f.provider {
                Provider::Service(b) => crate::services::of(b),
                _ => None,
            };
            let field = |key: &'static str, label: &str, edit: &rune::TextEdit, placeholder: &str, pick: fn(&mut Form) -> &mut rune::TextEdit| {
                let input = rune_kit::TextInput::new(edit).width(None).label(label).placeholder(placeholder).on_edit(move |s: &mut State| account_part(s, pick)).on_submit(|s: &mut State, _: &mut Tx| crate::servers::submit(s));
                col().key(key).w_full().gap(4.0).child(text(label.to_string()).font(k.ty.text_small_medium).color(k.c.text_base)).child(input.build(k))
            };

            let pass_eye = eye_toggle("pass-eye", f.reveal_password, |s| {
                if let Some(f) = &mut s.account_form {
                    f.reveal_password = !f.reveal_password;
                }
            });
            let password_label = match (f.editing.is_some(), f.provider) {
                (true, _) => t("Senha (vazio: manter a salva)", "Password (empty: keep the saved one)"),
                (false, Provider::Service(_)) => service.map_or(t("Senha", "Password"), |svc| svc.password_label()),
                (false, Provider::Nextcloud | Provider::OwnCloud) => t("Senha ou senha de app", "Password or app password"),
                (false, _) => t("Senha", "Password"),
            };
            let password_field = col()
                .key("password")
                .w_full()
                .gap(4.0)
                .child(text(password_label).font(k.ty.text_small_medium).color(k.c.text_base))
                .child(
                    rune_kit::TextInput::new(&f.password)
                        .width(None)
                        .label(password_label)
                        .revealed(f.reveal_password)
                        .trailing(pass_eye)
                        .on_edit(|s: &mut State| account_part(s, |f| &mut f.password))
                        .on_submit(|s: &mut State, _: &mut Tx| crate::servers::submit(s))
                        .build(k),
                );

            let muted = |t: String| text(t).font(k.ty.text_xsmall).color(k.c.text_muted).wrap().w_full().min_w(0.0);
            let port_hint = proto.default_port().to_string();
            let half = |a: El, b: El| row().w_full().gap(10.0).child(a.grow(1.0).w(0.0)).child(b.grow(1.0).w(0.0));
            let host_hint = match f.provider {
                Provider::Nextcloud => t("cloud.exemplo.com ou https://cloud.exemplo.com", "cloud.example.com or https://cloud.example.com"),
                Provider::OwnCloud => t("owncloud.exemplo.com ou https://owncloud.exemplo.com", "owncloud.example.com or https://owncloud.example.com"),
                Provider::Sftp => t("nas.local, 192.168.0.10 ou sftp://usuario@host", "nas.local, 192.168.0.10 or sftp://user@host"),
                Provider::Ftp => t("ftp.exemplo.com, 192.168.0.10 ou ftp://usuario@host", "ftp.example.com, 192.168.0.10 or ftp://user@host"),
                Provider::WebDav | Provider::GoogleDrive | Provider::Mega | Provider::Dropbox | Provider::B2 | Provider::OneDrive | Provider::Telegram => t("dav.exemplo.com, 192.168.0.10 ou https://host/dav", "dav.example.com, 192.168.0.10 or https://host/dav"),
                Provider::Service(_) => match service.map(|svc| svc.host) {
                    Some(crate::services::Host::Part { hint, .. }) => t(hint.0, hint.1),
                    Some(crate::services::Host::Pasted { hint }) => hint,
                    _ => "",
                },
            };
            let folder_hint = match f.provider {
                Provider::Nextcloud | Provider::OwnCloud => t("/remote.php/webdav (automático, se vazio)", "/remote.php/webdav (automatic if empty)"),
                Provider::WebDav => t("/dav ou /remote.php/dav/files/usuario (raiz /, se vazio)", "/dav or /remote.php/dav/files/user (root / if empty)"),
                Provider::Service(_) => match service.map(|svc| svc.root) {
                    Some(root) if !root.is_empty() => root,
                    _ => t("a pasta de cima, se vazio", "the top folder if empty"),
                },
                Provider::Sftp => t("a pasta pessoal, se vazio", "the home folder if empty"),
                Provider::Ftp | Provider::GoogleDrive | Provider::Mega | Provider::Dropbox | Provider::B2 | Provider::OneDrive | Provider::Telegram => t("a pasta do login, se vazio", "the login folder if empty"),
            };
            let name_hint = match f.provider {
                Provider::Nextcloud => t("Nextcloud (servidor), se vazio", "Nextcloud (server) if empty"),
                Provider::OwnCloud => t("ownCloud (servidor), se vazio", "ownCloud (server) if empty"),
                Provider::Service(_) => service.map_or("", |svc| svc.name),
                _ => t("o endereço do servidor, se vazio", "the server's address if empty"),
            };
            let user_hint = if matches!(proto, Protocol::Ftp | Protocol::Ftps) { t("anonymous, se vazio", "anonymous if empty") } else { "" };

            let mut inner = col().w_full().gap(10.0).child(banner);
            // A ready-made service needs no port, and its server only when it's the account's own:
            // the part of it to type, under the name the provider gives it.
            inner = match service.map(|svc| svc.host) {
                None => inner.child(
                    row()
                        .w_full()
                        .gap(10.0)
                        .child(field("host", t("Servidor", "Server"), &f.host, host_hint, |f| &mut f.host).grow(3.0).w(0.0))
                        .child(field("port", t("Porta", "Port"), &f.port, &port_hint, |f| &mut f.port).grow(1.0).w(0.0)),
                ),
                Some(crate::services::Host::Part { label, .. }) => inner.child(field("host", t(label.0, label.1), &f.host, host_hint, |f| &mut f.host)),
                Some(crate::services::Host::Pasted { .. }) => inner.child(field("host", t("Endereço WebDAV", "WebDAV address"), &f.host, host_hint, |f| &mut f.host)),
                Some(crate::services::Host::Fixed(_) | crate::services::Host::FromUser(_)) => inner,
            };
            let user_label = service.map_or(t("Usuário", "User name"), |svc| svc.user_label());
            inner = inner.child(half(field("user", user_label, &f.user, user_hint, |f| &mut f.user), password_field));

            match f.provider {
                Provider::Ftp => {
                    inner = inner.child(
                        rune_kit::Checkbox::new(proto == Protocol::Ftps)
                            .label(t("Usar SSL/TLS (FTPS)", "Use SSL/TLS (FTPS)"))
                            .description(t("Criptografa login e arquivos via TLS (porta 21 ou 990).", "Encrypts the login and files with TLS (port 21 or 990)."))
                            .on_toggle(crate::servers::toggle_tls)
                            .build(k)
                            .key("tls-toggle"),
                    );
                }
                Provider::WebDav | Provider::Nextcloud | Provider::OwnCloud => {
                    inner = inner.child(
                        rune_kit::Checkbox::new(proto == Protocol::Davs)
                            .label(t("Usar conexão segura SSL/TLS (HTTPS)", "Use a secure SSL/TLS connection (HTTPS)"))
                            .description(t("Mantém senha e arquivos criptografados na rede.", "Keeps the password and files encrypted on the network."))
                            .on_toggle(crate::servers::toggle_tls)
                            .build(k)
                            .key("tls-toggle"),
                    );
                }
                Provider::Sftp | Provider::GoogleDrive | Provider::Mega | Provider::Dropbox | Provider::B2 | Provider::OneDrive | Provider::Telegram | Provider::Service(_) => {}
            }

            inner = inner.child(field("folder", t("Pasta inicial", "Start folder"), &f.folder, folder_hint, |f| &mut f.folder));

            if proto == Protocol::Sftp {
                inner = inner.child(field(
                    "key",
                    t("Chave SSH (opcional)", "SSH key (optional)"),
                    &f.key,
                    t("C:\\Users\\voce\\.ssh\\id_ed25519 (a senha vira a da chave)", "C:\\Users\\you\\.ssh\\id_ed25519 (the password becomes the key's)"),
                    |f| &mut f.key,
                ));
                inner = inner.child(muted(
                    t(
                        "Sem senha nem chave, as chaves de ~/.ssh são tentadas. Na primeira conexão a chave do servidor é guardada.",
                        "With no password or key, the keys in ~/.ssh are tried. On the first connection the server's key is saved.",
                    )
                    .into(),
                ));
            } else if let Some(svc) = service {
                inner = inner.child(muted(svc.note().into()));
            } else if matches!(f.provider, Provider::Nextcloud | Provider::OwnCloud) {
                inner = inner.child(muted(
                    t(
                        "Dica: se a sua conta usa verificação em duas etapas (2FA), gere uma senha de aplicativo nas configurações de segurança do servidor.",
                        "Tip: if your account uses two-step verification (2FA), create an app password in the server's security settings.",
                    )
                    .into(),
                ));
            }

            let connections_hint = tr!("{} (de 1 a {})", "{} (1 to {})", openexplorer_remote::USUAL_CONNECTIONS, openexplorer_remote::MOST_CONNECTIONS);
            inner = inner.child(half(
                field("name", t("Nome na árvore (opcional)", "Name in the tree (optional)"), &f.name, name_hint, |f| &mut f.name),
                field("connections", t("Conexões simultâneas", "Simultaneous connections"), &f.connections, &connections_hint, |f| &mut f.connections),
            ));

            let active_fp = f.active_trusted();
            if let (Some(fp), None) = (active_fp, &f.untrusted) {
                inner = inner.child(muted(tr!("Certificado confiado (SHA-256): {}", "Trusted certificate (SHA-256): {}", fingerprint_lines(fp))));
            }
            if proto.plain() {
                let secure_alt = if proto == Protocol::Ftp { t("Usar SSL/TLS (FTPS) ou SFTP", "SSL/TLS (FTPS) or SFTP") } else { "HTTPS (SSL/TLS)" };
                inner = inner.child(
                    row()
                        .key("plain-warn")
                        .w_full()
                        .px(10.0)
                        .py(7.0)
                        .radius(k.radius.sm)
                        .fill(k.alpha(k.c.warning.fg, if k.dark { 0.12 } else { 0.08 }))
                        .border(1.0, k.alpha(k.c.warning.fg, 0.38))
                        .child(
                            text(tr!(
                                "{} não usa criptografia: senha e arquivos passam abertos pela rede. Prefira {secure_alt} se o servidor aceitar.",
                                "{} isn't encrypted: the password and files travel in the clear over the network. Prefer {secure_alt} if the server supports it.",
                                proto.label()
                            ))
                            .font(k.ty.text_xsmall)
                            .color(k.c.text_base)
                            .wrap()
                            .w_full()
                            .min_w(0.0),
                        ),
                );
            }
            if let Some(e) = &f.error {
                inner = inner.child(text(e.clone()).key("error").font(k.ty.text_small).color(k.c.danger.fg).wrap().w_full().min_w(0.0));
            }
            if let Some(r) = &f.untrusted {
                let changed = active_fp.is_some_and(|old| old != r.fingerprint);
                let lead = if changed {
                    tr!(
                        "O certificado do servidor mudou desde que você confiou nele: o novo {}. Se você não trocou o certificado do servidor, alguém pode estar se passando por ele.",
                        "The server's certificate has changed since you trusted it: the new one {}. If you didn't change the server's certificate, someone may be impersonating it.",
                        r.why
                    )
                } else {
                    tr!(
                        "O certificado do servidor {}. Isso é comum num NAS ou servidor de casa; confira a impressão digital com a que o servidor mostra antes de confiar.",
                        "The server's certificate {}. This is common on a NAS or home server; check the fingerprint against the one the server shows before trusting it.",
                        r.why
                    )
                };
                let warn = col()
                    .key("untrusted")
                    .w_full()
                    .gap(6.0)
                    .padding(10.0)
                    .radius(k.radius.sm)
                    .border(1.0, k.c.warning.fg)
                    .child(text(lead).font(k.ty.text_small).color(k.c.text_base).wrap().w_full().min_w(0.0))
                    .child(text(format!("SHA-256: {}", fingerprint_lines(&r.fingerprint))).font(k.ty.text_xsmall).color(k.c.text_muted).wrap().w_full().min_w(0.0));
                inner = inner.child(warn);
            }

            let checking = f.checking.is_some();
            let save_label = if f.editing.is_some() { t("Salvar", "Save") } else { t("Conectar", "Connect") };
            let save_el = match (&f.untrusted, checking) {
                (_, true) => Button::new(t("Conectando…", "Connecting…")).variant(ButtonVariant::Submit).disabled(true).build(k).cursor(Cursor::Pointer),
                (Some(_), false) => Button::new(t("Confiar e salvar", "Trust and save")).variant(ButtonVariant::Danger).on_press(crate::servers::trust).build(k).cursor(Cursor::Pointer).default_button(),
                (None, false) => Button::new(save_label).variant(ButtonVariant::Submit).on_press(crate::servers::submit).build(k).cursor(Cursor::Pointer).default_button(),
            };
            let mut actions = row().key("actions").w_full().gap(8.0).center_y();
            if f.editing.is_none() {
                actions = actions.child(Button::new(t("Voltar", "Back")).icon(&icons::BACK).variant(ButtonVariant::Neutral).disabled(checking).on_press(crate::servers::back_to_pick).build(k).cursor(Cursor::Pointer));
            }
            actions = actions
                .child(el().grow(1.0))
                .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press_tx(close_account_win).build(k).cursor(Cursor::Pointer))
                .child(save_el);
            (inner, actions)
        }
    };

    let centered_inner = row()
        .w_full()
        .justify_center()
        .px(24.0)
        .py(22.0)
        .child(col().w_full().max_w(600.0).child(inner));

    let centered_footer = row()
        .key("configure-footer")
        .w_full()
        .justify_center()
        .px(24.0)
        .py(12.0)
        .child(col().w_full().max_w(600.0).child(actions));

    col()
        .key("configure-body")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .pt(6.0)
        .child(
            col()
                .key("configure-panel-wrap")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .px(12.0)
                .child(
                    col()
                        .key("configure-panel")
                        .w_full()
                        .h_full()
                        .min_h(0.0)
                        .radius(8.0)
                        .fill(panel_bg)
                        .border(1.0, panel_border)
                        .clip_rounded(8.0)
                        .child(
                            crate::scrollbars::v_bar(
                                k,
                                "configure-scroll",
                                col().key("configure-scroll").w_full().h_full().min_h(0.0).scroll(ScrollSpec { stick_bottom: false }).child(centered_inner),
                            )
                            .w_full()
                            .h_full(),
                        ),
                ),
        )
        .child(centered_footer)
}

/// Unified Air Explorer-style destination conflict prompt, styled in OpenExplorer's Rune Kit
/// card aesthetic. Clicking any option card resolves the item (or batch); Cancel aborts the batch.
fn conflict_dialog(k: &Kit, prompt: &conflict::Prompt, layer: &mut Vec<El>) {
    let modal_title = match prompt.op {
        transfers::Op::Copy => t("Copiar arquivo", "Copy file"),
        transfers::Op::Move => t("Mover arquivo", "Move file"),
        transfers::Op::Download => t("Baixar arquivo", "Download file"),
        transfers::Op::Upload => t("Enviar arquivo", "Upload file"),
    };
    let (replace_title, skip_title, keep_title, newer_title) = match prompt.op {
        transfers::Op::Copy => (
            t("Copiar e substituir", "Copy and replace"),
            t("Não copiar", "Don't copy"),
            t("Copiar, mas manter ambos os arquivos", "Copy, but keep both files"),
            t("Copiar arquivos mais recentes da origem para o destino", "Copy newer files from origin to destination"),
        ),
        transfers::Op::Move => (
            t("Mover e substituir", "Move and replace"),
            t("Não mover", "Don't move"),
            t("Mover, mas manter ambos os arquivos", "Move, but keep both files"),
            t("Mover arquivos mais recentes da origem para o destino", "Move newer files from origin to destination"),
        ),
        transfers::Op::Download => (
            t("Baixar e substituir", "Download and replace"),
            t("Não baixar", "Don't download"),
            t("Baixar, mas manter ambos os arquivos", "Download, but keep both files"),
            t("Baixar arquivos mais recentes da origem para o destino", "Download newer files from origin to destination"),
        ),
        transfers::Op::Upload => (
            t("Enviar e substituir", "Upload and replace"),
            t("Não enviar", "Don't upload"),
            t("Enviar, mas manter ambos os arquivos", "Upload, but keep both files"),
            t("Enviar arquivos mais recentes da origem para o destino", "Upload newer files from origin to destination"),
        ),
    };

    let lead_title = if prompt.src.is_dir || prompt.dst.is_dir {
        t("Esta pasta já contém um item com o mesmo nome", "This folder already contains an item with the same name")
    } else {
        t("Esta pasta já contém um arquivo com o mesmo nome", "This folder already contains a file with the same name")
    };
    let header = col()
        .key("lead")
        .w_full()
        .gap(2.0)
        .child(text(lead_title).font(k.ty.body_medium).color(k.c.text_base).wrap().w_full().min_w(0.0))
        .child(text(t("Clique no arquivo que deseja manter", "Click on the file you want to keep")).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0));

    let option_card = |key: &'static str, title: &str, choice: Choice, detail: El| {
        let arrow_badge = row()
            .mt(1.0)
            .size(24.0, 24.0)
            .radius(12.0)
            .center()
            .fill(k.alpha(k.c.text_accent, if k.dark { 0.18 } else { 0.12 }))
            .child(icon(&icons::FORWARD, 13.0).color(k.c.text_accent));
        let content = col()
            .grow(1.0)
            .w(0.0)
            .min_w(0.0)
            .gap(4.0)
            .child(text(title.to_string()).font(k.ty.text_small_medium).color(k.c.text_base).wrap().w_full().min_w(0.0))
            .child(detail);
        k.interactive(
            row()
                .key(key)
                .role("button")
                .label(title.to_string())
                .focusable(true)
                .focus_ring(2.0, 1.0, k.c.border_focus)
                .w_full()
                .px(12.0)
                .py(10.0)
                .gap(10.0)
                .radius(k.radius.lg)
                .fill(k.alpha(k.c.text_base, if k.dark { 0.035 } else { 0.025 }))
                .border(1.0, k.c.border_weak)
                .hover(k.alpha(k.c.text_accent, if k.dark { 0.12 } else { 0.07 }))
                .cursor(Cursor::Pointer),
        )
        .on_press(move |s: &mut State| actions::answer_conflict(s, Some(choice)))
        .child(arrow_badge)
        .child(content)
    };

    let file_block = |info: &conflict::FileInfo, newer: bool| {
        let mut block = col().w_full().gap(2.0);
        let name = shown(&info.name);
        let name_row = row()
            .w_full()
            .gap(6.0)
            .center_y()
            .child(icon(kind_icon(info.kind), 15.0))
            .child(text(name.clone()).font(k.ty.text_small).color(k.c.text_base).truncate().shrink().min_w(0.0).help_when_truncated(name));
        block = block.child(name_row);
        if !info.place.is_empty() {
            block = block.child(
                text(info.place.clone())
                    .font(k.ty.caption())
                    .color(k.c.text_muted)
                    .truncate()
                    .w_full()
                    .min_w(0.0)
                    .help_when_truncated(info.place.clone()),
            );
        }
        let mut meta = row().w_full().gap(10.0).center_y();
        let mut has_meta = false;
        if let Some(size) = info.size {
            has_meta = true;
            meta = meta.child(text(tr!("Tamanho: {}", "Size: {}", format_size_exact(size))).font(k.ty.caption()).color(k.c.text_muted));
        }
        if let Some(modified) = info.modified {
            has_meta = true;
            meta = meta.child(text(tr!("Data: {}", "Date: {}", format_time_secs(modified))).font(k.ty.caption()).color(k.c.text_muted));
        }
        if newer {
            has_meta = true;
            meta = meta.child(rune_kit::Badge::new(t("mais recente", "newer")).variant(rune_kit::BadgeVariant::Accent).build(k));
        }
        if has_meta {
            block = block.child(meta);
        }
        block
    };

    let src_newer = conflict::is_newer(prompt.src.modified, prompt.dst.modified);
    let dst_newer = conflict::is_newer(prompt.dst.modified, prompt.src.modified);
    let keep_desc = col()
        .w_full()
        .gap(1.0)
        .child(text(tr!("O arquivo será renomeado para {}", "The file will be renamed to {}", shown(&prompt.renamed))).font(k.ty.caption()).color(k.c.text_muted).wrap().w_full().min_w(0.0));
    let newer_desc = text(t(
        "Arquivos com a mesma data ou mais antigos serão ignorados",
        "Files with the same date or older will be skipped",
    ))
    .font(k.ty.caption())
    .color(k.c.text_muted)
    .wrap()
    .w_full()
    .min_w(0.0);

    let options = col()
        .key("options")
        .w_full()
        .gap(8.0)
        .child(option_card("conflict-replace", replace_title, Choice::Replace, file_block(&prompt.src, src_newer)))
        .child(option_card("conflict-skip", skip_title, Choice::Skip, file_block(&prompt.dst, dst_newer)))
        .child(option_card("conflict-keep-both", keep_title, Choice::KeepBoth, keep_desc))
        .child(option_card("conflict-replace-newer", newer_title, Choice::ReplaceNewer, newer_desc));

    let mut footer = row().key("actions").w_full().pt(4.0).center_y().gap(8.0);
    if prompt.may_have_more {
        footer = footer.child(
            rune_kit::Checkbox::new(prompt.apply_all)
                .label(t("Aplicar a todos os conflitos", "Apply to all conflicts"))
                .on_toggle(actions::toggle_conflict_all)
                .build(k),
        );
    }
    footer = footer.child(el().grow(1.0)).child(
        Button::new(t("Cancelar", "Cancel"))
            .variant(ButtonVariant::Neutral)
            .on_press(|s: &mut State| actions::answer_conflict(s, None))
            .build(k)
            .cursor(Cursor::Pointer)
            .key("conflict-cancel"),
    );

    let body = col().key("content").w_full().px(16.0).pb(16.0).gap(12.0).child(header).child(options).child(footer);
    Modal::new("conflict", modal_title)
        .open(true)
        .child(body)
        .width(540.0)
        .fit()
        .on_close(|s: &mut State| actions::answer_conflict(s, None))
        .build(k, layer);
}

/// The first few items a dialog acts on, with "e mais N" for the rest.
fn item_list(s: &State, k: &Kit, loc: &Location, items: &[crate::model::Entry]) -> El {
    let mut list = col().key("items").w_full().gap(4.0);
    for e in items.iter().take(6) {
        list = list.child(
            row().gap(6.0).center_y().child(entry_icon(s, loc, e, 16.0)).child(text(shown(&e.name)).font(k.ty.text_small).color(k.c.text_base).truncate().shrink().min_w(0.0)),
        );
    }
    if items.len() > 6 {
        list = list.child(text(tr!("e mais {}", "and {} more", ui_count(items.len() - 6))).font(k.ty.text_small).color(k.c.text_muted));
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
        .label(t("Novo nome", "New name"))
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

/// How many a search found: "1 resultado", "12 resultados".
fn results(n: usize) -> String {
    if n == 1 { tr!("{} resultado", "{} result", ui_count(n)) } else { tr!("{} resultados", "{} results", ui_count(n)) }
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
        .key(format!("tab-{tab:?}"))
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
fn place_of(p: &actions::Props) -> String {
    match (&p.loc, p.single()) {
        (loc @ (Location::Local(_) | Location::Found(_)), Some(e)) => loc.local_path(e).map(|p| p.display().to_string()).unwrap_or_default(),
        (Location::Local(dir), None) => dir.display().to_string(),
        (Location::Found(f), None) => tr!("Resultados de “{}” em {}", "Results for “{}” in {}", f.text, f.place),
        (loc @ Location::Server { server, path, .. }, single) => match single {
            Some(e) => format!("{}{}", server.address(), crate::servers::path_of(loc, e).unwrap_or_default()),
            None => format!("{}{}", server.address(), server.path(path)),
        },
        (Location::DriveSearch(_), Some(e)) => match &e.drive_origin {
            Some(o) => format!("{}/{}/{}", o.email, o.label, shown(&e.name)),
            None => shown(&e.name),
        },
        (Location::DriveSearch(q), None) => tr!("Resultados de “{}” em todas as contas", "Results for “{}” in all accounts", q.text),
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
    let loc = &p.loc;
    let drive = match loc {
        Location::Drive { email, .. } => Some(email.clone()),
        Location::DriveSearch(_) => p.single().and_then(|e| e.drive_origin.as_deref().map(|o| o.email.clone())),
        _ => None,
    };
    let details = match &p.details {
        Some(Ok(d)) => Some(d),
        _ => None,
    };
    let when = |t: Option<std::time::SystemTime>| t.map(format_long).unwrap_or_else(|| "—".into());
    let muted = |t: &str| text(t.to_string()).font(k.ty.text_small).color(k.c.text_muted);

    // Header: the item's icon and its name, which renames on OK.
    let (glyph, title) = match p.single() {
        Some(e) => (entry_icon(s, loc, e, 40.0), shown(&e.name)),
        None if p.items.iter().all(|e| e.is_dir) => (shell_icon(s, Some(&IconKey::Folder), &icons::FOLDER, 40.0), tr!("{} itens", "{} items", ui_count(p.items.len()))),
        None => (icon(&icons::FILE, 40.0), tr!("{} itens", "{} items", ui_count(p.items.len()))),
    };
    let editable_name = p.renamable();
    let name: El = if editable_name {
        rune::text_input(&s.name_edit, k.ty.text_small)
            .single_line()
            .key("prop-name")
            .label(t("Nome", "Name"))
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

    let mut tabs = row().key("prop-tabs").w_full().border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak).child(prop_tab(k, t("Geral", "General"), PropTab::General, p.tab == PropTab::General)).child(prop_tab(
        k,
        t("Detalhes", "Details"),
        PropTab::Details,
        p.tab == PropTab::Details,
    ));
    if p.has_versions() {
        tabs = tabs.child(prop_tab(k, t("Histórico de versões", "Version history"), PropTab::Versions, p.tab == PropTab::Versions));
    }

    // A floor on the height, so switching tabs doesn't make the dialog jump.
    let mut body = col().key("prop-body").w_full().min_h(380.0).pt(14.0).gap(7.0);
    match p.tab {
        PropTab::General => {
            body = body.child(header).child(prop_sep(k));
            // What it is and where it lives.
            body = body.child(prop_text(k, t("Tipo", "Type"), p.kind.clone()));
            if let Some(desc) = details.and_then(|d| d.description.clone()) {
                body = body.child(prop_text(k, t("Descrição", "Description"), desc));
            }
            body = body.child(prop_text(k, t("Local", "Location"), if drive.is_some() { "Google Drive" } else { t("Este computador", "This PC") }));
            if let Some(email) = &drive {
                body = body.child(prop_text(k, t("Conta", "Account"), email.clone()));
            }
            body = body.child(prop_sep(k));
            // Where and how big.
            body = body.child(prop_text(k, t("Caminho", "Path"), place_of(p)));
            let size = match (p.single(), &p.totals) {
                (_, Some(t)) => format_bytes(t.bytes),
                // A server folder whose size came with the listing (OneDrive's).
                (Some(e), None) if !e.is_dir || e.size.is_some() => e.size.map(format_bytes).unwrap_or_else(|| "—".into()),
                (_, None) if loc.is_local() => t("Calculando…", "Calculating…").into(),
                (None, None) => format_bytes(p.listed_bytes),
                _ => "—".into(),
            };
            body = body.child(prop_text(k, t("Tamanho", "Size"), size));
            if let Some(totals) = &p.totals
                && (p.items.len() > 1 || p.items.iter().any(|e| e.is_dir))
            {
                let plural = |n: u64, one: &str, many: &str| format!("{} {}", thousands(n as usize), if n == 1 { one } else { many });
                let files = plural(totals.files, t("arquivo", "file"), t("arquivos", "files"));
                let dirs = plural(totals.dirs, t("pasta", "folder"), t("pastas", "folders"));
                body = body.child(prop_text(k, t("Contém", "Contains"), format!("{files}, {dirs}")));
            }
            if let Some(e) = p.single() {
                body = body.child(prop_sep(k));
                // When, and the content's fingerprint.
                let created = p.local.as_ref().and_then(|l| l.created).or_else(|| details.and_then(|d| d.created));
                body = body.child(prop_text(k, t("Criado", "Created"), when(created))).child(prop_text(k, t("Modificado", "Modified"), when(e.modified)));
                if let Some(l) = &p.local {
                    body = body.child(prop_text(k, t("Acessado", "Accessed"), when(l.accessed)));
                } else if let Some(v) = details.and_then(|d| d.viewed) {
                    body = body.child(prop_text(k, t("Aberto por você", "Opened by you"), format_long(v)));
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
                        (Some(_), None) if p.md5_running => muted(t("Calculando…", "Calculating…")),
                        (Some(_), None) => Button::new(t("Calcular", "Calculate")).variant(ButtonVariant::Neutral).on_press(actions::props_md5).build(k).cursor(Cursor::Pointer).key("md5"),
                        (None, None) => match drive_md5 {
                            Some(h) => mono(h),
                            None if google_format => muted(t("— (formato do Google, sem arquivo binário)", "— (Google format, no binary file)")),
                            None if details.is_none() && p.details.is_none() && matches!(p.loc, Location::Drive { .. }) => muted(t("Carregando…", "Loading…")),
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
                    let attrs = if l.attributes.is_empty() { t("Nenhum", "None").to_string() } else { l.attributes.join(", ") };
                    body = body.child(prop_text(k, t("Atributos", "Attributes"), attrs));
                    let ext = e.name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty() && !e.is_dir).map(|(_, x)| format!(".{x}")).unwrap_or_else(|| "—".into());
                    body = body.child(prop_text(k, t("Extensão", "Extension"), ext)).child(prop_text(k, t("Caminho completo", "Full path"), l.path.display().to_string()));
                }
                (Some(e), None, true) => {
                    body = body.child(prop_text(k, "ID", e.id.clone()));
                    match &p.details {
                        Some(Ok(d)) => {
                            let or_dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "—".into());
                            body = body
                                .child(prop_text(k, t("Tipo MIME", "MIME type"), or_dash(&d.mime)))
                                .child(prop_text(k, t("Proprietário", "Owner"), or_dash(&d.owner)))
                                .child(prop_text(k, t("Modificado por", "Modified by"), or_dash(&d.modified_by)))
                                .child(prop_text(k, t("Compartilhado", "Shared"), if d.shared { t("Sim", "Yes") } else { t("Não", "No") }));
                            let me = drive.as_deref().unwrap_or_default();
                            body = body.child(match &p.sharing {
                                Some(Ok(sh)) => prop_text(k, t("Compartilhamento", "Sharing"), sh.summary(me)),
                                Some(Err(err)) => prop_row(k, t("Compartilhamento", "Sharing"), muted(err)),
                                None => prop_row(k, t("Compartilhamento", "Sharing"), muted(t("Carregando…", "Loading…"))),
                            });
                            if let Some(by) = &d.shared_by {
                                body = body.child(prop_text(k, t("Compartilhado por", "Shared by"), by.clone()));
                            }
                            body = body.child(prop_text(k, t("Com estrela", "Starred"), if d.starred { t("Sim", "Yes") } else { t("Não", "No") }));
                            if let Some(url) = d.link.clone() {
                                body = body.child(prop_sep(k)).child(
                                    row().w_full().justify_end().child(
                                        Button::new(t("Abrir no Google Drive", "Open in Google Drive"))
                                            .variant(ButtonVariant::Neutral)
                                            .on_press_tx(move |s: &mut State, tx: &mut Tx| {
                                                if let Err(e) = tx.open_url(&url) {
                                                    crate::fail(s, tr!("Não consegui abrir o navegador: {e:?}", "Couldn't open the browser: {e:?}"));
                                                }
                                            })
                                            .build(k).cursor(Cursor::Pointer),
                                    ),
                                );
                            }
                        }
                        Some(Err(err)) => body = body.child(prop_text(k, t("Detalhes", "Details"), err.clone())),
                        None => body = body.child(prop_row(k, t("Detalhes", "Details"), muted(t("Carregando…", "Loading…")))),
                    }
                }
                _ => {
                    // Several items: their names.
                    let mut names = col().w_full().gap(4.0);
                    for e in p.items.iter().take(20) {
                        names = names.child(row().gap(6.0).center_y().child(entry_icon(s, loc, e, 16.0)).child(text(shown(&e.name)).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0)));
                    }
                    if p.items.len() > 20 {
                        names = names.child(muted(&tr!("e mais {}", "and {} more", ui_count(p.items.len() - 20))));
                    }
                    body = body.child(prop_row(k, t("Itens", "Items"), names));
                }
            }
        }
        PropTab::Versions => match &p.revisions {
            None => body = body.child(muted(t("Carregando…", "Loading…"))),
            Some(Err(err)) => body = body.child(text(err.clone()).font(k.ty.text_small).color(k.c.danger.fg).wrap().w_full()),
            Some(Ok(list)) if list.is_empty() => body = body.child(muted(t("Nenhuma versão guardada.", "No versions kept."))),
            Some(Ok(list)) => {
                let head = |label: &str, w: f32| text(label.to_string()).font(k.ty.text_xsmall).color(k.c.text_muted).w(w);
                body = body.child(
                    row()
                        .w_full()
                        .pb(6.0)
                        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
                        .child(head(t("Data", "Date"), 150.0))
                        .child(head(t("Tamanho", "Size"), 90.0))
                        .child(head(t("Por", "By"), 150.0)),
                );
                let mut rows = col().w_full();
                // Newest first, as the Drive website lists them.
                for r in list.iter().rev() {
                    let date = r.modified.map(format_time).unwrap_or_else(|| "—".into());
                    let mut by = r.by.clone().unwrap_or_else(|| "—".into());
                    if r.keep_forever {
                        by.push_str(t(" · mantida para sempre", " · kept forever"));
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
        .child(Button::new("OK").variant(ButtonVariant::Submit).on_press(actions::props_ok).build(k).cursor(Cursor::Pointer))
        .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press(actions::close_properties).build(k).cursor(Cursor::Pointer));
    let content = col().key("content").w_full().px(16.0).pb(16.0).child(tabs).child(body).child(buttons);
    Modal::new("props", tr!("Propriedades: {title}", "Properties: {title}")).open(true).child(content).width(520.0).fit().on_close(actions::close_properties).build(k, layer);
}

/// Windows' icon for `key` once it has loaded, else the app's own `fallback`.
fn shell_icon(s: &State, key: Option<&IconKey>, fallback: &'static Icon, size: f32) -> El {
    match key.and_then(|k| s.icons.get_sized(k, size)).filter(|i| i.is_ready() && i.error().is_none()) {
        Some(image) => el().size(size, size).pixels(image.pixels()),
        None => icon(fallback, size),
    }
}

/// The icon Explorer would show for `e`: its type's program's (VLC's for a video VLC opens),
/// or a local folder's own Windows icon when it's a special/customized folder (Documentos,
/// Downloads, Imagens, OneDrive…). A Drive shortcut to a folder keeps the app's linked-folder
/// icon.
pub(crate) fn entry_icon(s: &State, loc: &Location, e: &crate::model::Entry, size: f32) -> El {
    let kind = e.kind();
    let fallback = kind_icon(kind);
    if kind == Kind::FolderLink {
        return icon(fallback, size);
    }
    // A disk in Este Computador: its own icon, as the tree shows it.
    if *loc == Location::Computer {
        return shell_icon(s, sysicons::key_for(e, loc).as_ref(), &icons::DISK, size);
    }
    if e.is_dir
        && let Some(path) = loc.local_path(e)
    {
        let key = IconKey::Dir(path);
        if let Some(image) = s.icons.get_sized(&key, size).filter(|i| i.is_ready() && i.error().is_none()) {
            return el().size(size, size).pixels(image.pixels());
        }
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

fn toolbar(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let sep = || el().w(1.0).h(20.0).mx(6.0).fill(k.c.border_weak);
    let waiting_oauth = s.login.is_some() || s.servers.onedrive.login.is_some() || s.servers.dropbox_login.is_some();
    let total_accounts = s.accounts.len() + s.servers.list.len();

    let nav_tab = |key: &'static str, tab: MainTab, glyph: &'static Icon, label: &str, badge: Option<usize>, busy_dot: bool| -> El {
        let on = s.main_tab == tab;
        let ink = if on { k.c.text_accent } else { k.c.text_base };
        let icon_ink = if on { k.c.text_accent } else { k.c.icon_muted };
        let font = if on { k.ty.text_small_medium } else { k.ty.text_small };

        let mut inner = row()
            .h(32.0)
            .px(12.0)
            .gap(7.0)
            .center_y()
            .radius(k.radius.md)
            .child(icon(glyph, 16.0).color(icon_ink))
            .child(text(label.to_string()).font(font).color(ink));

        if busy_dot {
            inner = inner.child(el().size(6.0, 6.0).radius(3.0).fill(k.c.text_accent));
        } else if let Some(count) = badge.filter(|&n| n > 0) {
            let pill_bg = if on {
                k.alpha(k.c.text_accent, if k.dark { 0.22 } else { 0.12 })
            } else {
                k.alpha(k.c.text_base, if k.dark { 0.10 } else { 0.065 })
            };
            let pill_fg = if on { k.c.text_accent } else { k.c.text_muted };
            inner = inner.child(
                row()
                    .h(17.0)
                    .px(5.5)
                    .center()
                    .radius(8.5)
                    .fill(pill_bg)
                    .child(text(count.to_string()).font(k.ty.caption()).color(pill_fg)),
            );
        }

        if on {
            inner = inner.child(
                row()
                    .pin(Some(12.0), None, Some(12.0), Some(0.0))
                    .justify_center()
                    .child(el().w(22.0).h(2.5).radius(1.5).fill(k.c.text_accent)),
            );
        }

        k.interactive(
            inner
                .key(key)
                .role("tab")
                .label(label.to_string())
                .checked(on)
                .focusable(true)
                .focus_ring(2.0, 1.0, k.c.border_focus)
                .hover(if on { k.alpha(k.c.text_accent, 0.08) } else { k.c.hover })
                .press(k.c.pressed)
                .cursor(Cursor::Pointer),
        )
        .on_press(move |s: &mut State| crate::set_main_tab(s, tab))
    };

    let refresh_help = match s.main_tab {
        MainTab::Explore => t("Recarregar a pasta atual (F5)", "Reload the current folder (F5)"),
        MainTab::Accounts => t("Atualizar o espaço utilizado nas contas (F5)", "Refresh storage usage across accounts (F5)"),
    };

    let view_label = t("Visualizar", "View");
    let view_anchor = "toolbar-view";
    let view_open = s.view_menu_open;
    let view_btn = row()
        .key(view_anchor)
        .role("button")
        .label(view_label)
        .help(t("Escolher a disposição dos painéis (1, 2 ou 4)", "Choose the pane layout (1, 2, or 4)"))
        .h(30.0)
        .px(8.0)
        .gap(6.0)
        .center_y()
        .radius(k.radius.sm)
        .fill(if view_open { k.c.hover } else { Color::TRANSPARENT })
        .hover(k.c.hover)
        .press(k.c.pressed)
        .cursor(Cursor::Pointer)
        .on_press(|s: &mut State| {
            let next = !s.view_menu_open;
            crate::tabs::close_picker_filter(s);
            s.menu = None;
            s.tree_menu = None;
            s.address_menu = None;
            s.view_mode_menu = None;
            s.search_scope_menu = None;
            s.view_menu_open = next;
        })
        .child(icon(&icons::VIEW_MONITOR, 16.0).color(k.c.text_base))
        .child(text(view_label).font(k.ty.text_small).color(k.c.text_base))
        .child(icon(&icons::CHEVRON_DOWN, 12.0).color(k.c.icon_muted));

    if view_open {
        layer.push(rune_kit::scrim("view-menu-scrim", |s: &mut State| s.view_menu_open = false));
        let dropdown_bg = if k.dark { rune::oklch(0.22, 0.008, 255.0) } else { k.c.bg_base };
        let mut menu = col().key("view-menu").w(196.0).padding(4.0).gap(2.0);
        for layout in crate::settings::PaneLayout::ALL {
            let selected = s.settings.pane_layout == layout;
            let item = row()
                .key(format!("view-opt-{layout:?}"))
                .role("menuitem")
                .label(layout.label())
                .checked(selected)
                .w_full()
                .h(32.0)
                .px(10.0)
                .gap(10.0)
                .center_y()
                .radius(k.radius.sm)
                .fill(if selected { k.alpha(k.c.text_accent, if k.dark { 0.20 } else { 0.10 }) } else { Color::TRANSPARENT })
                .hover(k.c.hover)
                .cursor(Cursor::Pointer)
                .on_press(move |s: &mut State| crate::set_pane_layout(s, layout))
                .child(icon(layout.icon(), 16.0).color(if selected { k.c.text_accent } else { k.c.icon_muted }))
                .child(text(layout.label()).font(if selected { k.ty.text_small_medium } else { k.ty.text_small }).color(k.c.text_base).grow(1.0));
            menu = menu.child(item);
        }
        let surface = menu
            .radius(k.radius.md)
            .fill(dropdown_bg)
            .border(1.0, k.c.border_weak)
            .box_shadow(rune::Shadow { y: 6.0, blur: 18.0, color: k.dark_a(0.32), ..Default::default() });
        layer.push(rune_kit::popover::anchored(k, surface, view_anchor, rune_kit::Placement::BottomStart, 4.0));
    }

    row()
        .key("toolbar")
        .w_full()
        .h(42.0)
        .px(8.0)
        .gap(3.0)
        .center_y()
        .fill(k.c.bg_base)
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(nav_tab("tab-explore", MainTab::Explore, &icons::TAB_EXPLORE, t("Explorar", "Explore"), None, false))
        .child(nav_tab("tab-accounts", MainTab::Accounts, &icons::TAB_ACCOUNTS, t("Contas", "Accounts"), Some(total_accounts), waiting_oauth))
        .child(sep())
        .child(
            tool(k, "reload-all", &icons::REFRESH, t("Atualizar", "Refresh"), true, |s: &mut State, _| match s.main_tab {
                MainTab::Explore => {
                    if !s.tabs[s.active].is_picker() {
                        refresh(s, s.active);
                    }
                }
                MainTab::Accounts => crate::refresh_all_quotas(s),
            })
            .help(refresh_help),
        )
        .child(view_btn)
        .child(el().grow(1.0))
        .child(update_toolbar_pill(s, k))
        .child(tool(k, "options", &icons::SETTINGS, t("Opções", "Options"), true, |s: &mut State, _| {
            s.dialog = Some(Dialog::Options(Box::new(crate::settings::OptionsView::open(crate::settings::OptionsTab::General))));
        }))
        .child(
            row()
                .gap(6.0)
                .center_y()
                .mx(8.0)
                .child(icon(&icons::APP_ICON, 16.0))
                .child(text("Open Explorer").font(k.ty.text_small_medium).color(k.c.text_faint)),
        )
}

fn update_toolbar_pill(s: &State, k: &Kit) -> El {
    let Some(rel) = &s.updater.release else {
        return el();
    };
    let label = if s.updater.is_ready() {
        t("Reiniciar para atualizar", "Restart to update").to_string()
    } else {
        tr!("Atualizar v{}", "Update v{}", rel.version)
    };
    let fill = k.alpha(k.c.text_accent, if k.dark { 0.22 } else { 0.12 });
    let hover = k.alpha(k.c.text_accent, if k.dark { 0.30 } else { 0.18 });
    let border = k.alpha(k.c.text_accent, 0.45);
    k.interactive(
        row()
            .key("toolbar-update")
            .role("button")
            .label(label.clone())
            .focusable(true)
            .focus_ring(2.0, 1.0, k.c.border_focus)
            .h(26.0)
            .px(10.0)
            .mr(4.0)
            .gap(6.0)
            .center_y()
            .radius(13.0)
            .fill(fill)
            .hover(hover)
            .border(1.0, border)
            .cursor(Cursor::Pointer),
    )
    .on_press(actions::open_update_dialog)
    .child(icon(&icons::DOWNLOAD, 13.0).color(k.c.text_accent))
    .child(text(label).font(k.ty.caption()).color(k.c.text_accent))
}

// ---------------------------------------------------------------------------------------------
// Central de Contas

pub(crate) enum HubQuota {
    Known { total: u64, free: u64, used_frac: f32, help: String },
    UnlimitedUsed { used: u64, help: String },
    Loading,
    Info(String),
}

pub(crate) struct HubCard {
    pub key: String,
    pub provider: crate::servers::Provider,
    pub drive_name: String,
    pub user_name: Option<String>,
    pub email_or_addr: String,
    pub quota: HubQuota,
    pub open_loc: Location,
    pub in_pane: [bool; layout::MAX_PANES],
    pub edit_id: Option<String>,
    pub remove_target: actions::AccountTarget,
}

impl HubCard {
    pub(crate) fn matches_search(&self, q: &str) -> bool {
        let q = q.trim().to_lowercase();
        if q.is_empty() {
            return true;
        }
        q.split_whitespace().all(|w| {
            self.drive_name.to_lowercase().contains(w)
                || self.provider.label().to_lowercase().contains(w)
                || self.user_name.as_deref().is_some_and(|u| u.to_lowercase().contains(w))
                || self.email_or_addr.to_lowercase().contains(w)
        })
    }
}

pub(crate) fn hub_cards(s: &State) -> Vec<HubCard> {
    use crate::servers::Provider;
    use openexplorer_remote::Protocol;

    let mut out = Vec::with_capacity(s.accounts.len() + s.servers.list.len());

    // 1. Google Drive accounts
    for a in &s.accounts {
        let user_name = (!a.name.trim().is_empty() && !a.name.eq_ignore_ascii_case(&a.email)).then(|| a.name.clone());
        let quota = match s.quotas.get(&a.email) {
            Some(Some(q)) => match (q.limit.filter(|&l| l > 0), q.free()) {
                (Some(total), Some(free)) => HubQuota::Known {
                    total,
                    free,
                    used_frac: (q.usage as f64 / total as f64).clamp(0.0, 1.0) as f32,
                    help: quota_help(*q),
                },
                _ => HubQuota::UnlimitedUsed { used: q.usage, help: quota_help(*q) },
            },
            Some(None) | None => HubQuota::Loading,
        };
        let in_pane = |p: usize| matches!(&s.panes[p].loc, Location::Drive { email, .. } if email == &a.email);
        out.push(HubCard {
            key: format!("gdrive:{}", a.email),
            provider: Provider::GoogleDrive,
            drive_name: "GoogleDrive".into(),
            user_name,
            email_or_addr: a.email.clone(),
            quota,
            open_loc: Location::drive(&a.email, Space::MyDrive),
            in_pane: std::array::from_fn(&in_pane),
            edit_id: None,
            remove_target: actions::AccountTarget::GoogleDrive(a.email.clone()),
        });
    }

    // 2. Cloud accounts & remote servers in `s.servers.list`
    for sv in &s.servers.list {
        let provider = Provider::of_server(sv);
        let in_pane = |p: usize| matches!(&s.panes[p].loc, Location::Server { server, .. } if server.id == sv.id);

        match provider {
            Provider::OneDrive => {
                let meta = s.servers.onedrive.accounts.iter().find(|a| a.id == sv.user);
                let business = meta.is_some_and(|m| m.business);
                let email = meta.map(|m| m.email.clone()).unwrap_or_else(|| sv.name.clone());
                let user_name = meta
                    .map(|m| m.name.trim())
                    .filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case(&email))
                    .map(str::to_string);
                let drive_name = if business { "OneDrive For Business" } else { "OneDrive" }.to_string();
                let quota = match s.servers.onedrive.quotas.get(&sv.id) {
                    Some(Some(q)) if q.total > 0 => HubQuota::Known {
                        total: q.total,
                        free: q.remaining,
                        used_frac: (q.used as f64 / q.total as f64).clamp(0.0, 1.0) as f32,
                        help: tr!("Usado: {} de {}", "Used: {} of {}", format_size(q.used), whole_size(q.total)),
                    },
                    Some(Some(q)) => HubQuota::UnlimitedUsed {
                        used: q.used,
                        help: tr!("Usado: {}", "Used: {}", format_size(q.used)),
                    },
                    Some(None) | None => HubQuota::Loading,
                };
                out.push(HubCard {
                    key: format!("srv:{}", sv.id),
                    provider,
                    drive_name,
                    user_name,
                    email_or_addr: email,
                    quota,
                    open_loc: crate::onedrive::files(sv),
                    in_pane: std::array::from_fn(&in_pane),
                    edit_id: None,
                    remove_target: actions::AccountTarget::Server(sv.id.clone()),
                });
            }
            Provider::Dropbox => {
                let meta = s.servers.dropbox_accounts.iter().find(|a| a.email.eq_ignore_ascii_case(&sv.user));
                let user_name = meta
                    .map(|m| m.name.trim())
                    .filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case(&sv.user))
                    .map(str::to_string);
                let quota = match s.servers.dropbox_quotas.get(&sv.id) {
                    Some(Some(q)) if q.total > 0 => {
                        let free = q.total.saturating_sub(q.used);
                        HubQuota::Known {
                            total: q.total,
                            free,
                            used_frac: (q.used as f64 / q.total as f64).clamp(0.0, 1.0) as f32,
                            help: tr!("Usado: {} de {}", "Used: {} of {}", format_size(q.used), whole_size(q.total)),
                        }
                    }
                    Some(Some(q)) => HubQuota::UnlimitedUsed {
                        used: q.used,
                        help: tr!("Usado: {}", "Used: {}", format_size(q.used)),
                    },
                    Some(None) | None => HubQuota::Loading,
                };
                out.push(HubCard {
                    key: format!("srv:{}", sv.id),
                    provider,
                    drive_name: "Dropbox".into(),
                    user_name,
                    email_or_addr: sv.user.clone(),
                    quota,
                    open_loc: crate::dropbox::top(sv),
                    in_pane: std::array::from_fn(&in_pane),
                    edit_id: None,
                    remove_target: actions::AccountTarget::Server(sv.id.clone()),
                });
            }
            Provider::Mega => {
                let quota = match s.servers.mega_quotas.get(&sv.id) {
                    Some(Some(q)) if q.total > 0 => {
                        let free = q.total.saturating_sub(q.used);
                        HubQuota::Known {
                            total: q.total,
                            free,
                            used_frac: (q.used as f64 / q.total as f64).clamp(0.0, 1.0) as f32,
                            help: crate::mega::quota_help(*q),
                        }
                    }
                    Some(Some(q)) => HubQuota::UnlimitedUsed {
                        used: q.used,
                        help: crate::mega::quota_help(*q),
                    },
                    Some(None) | None => HubQuota::Loading,
                };
                out.push(HubCard {
                    key: format!("srv:{}", sv.id),
                    provider,
                    drive_name: "Mega".into(),
                    user_name: None,
                    email_or_addr: sv.user.clone(),
                    quota,
                    open_loc: crate::mega::cloud(sv),
                    in_pane: std::array::from_fn(&in_pane),
                    edit_id: None,
                    remove_target: actions::AccountTarget::Server(sv.id.clone()),
                });
            }
            Provider::B2 => {
                let user_name = (!sv.name.trim().is_empty() && !sv.name.eq_ignore_ascii_case("Backblaze B2") && sv.name != sv.user).then(|| sv.name.clone());
                out.push(HubCard {
                    key: format!("srv:{}", sv.id),
                    provider,
                    drive_name: "Backblaze B2".into(),
                    user_name,
                    email_or_addr: sv.user.clone(),
                    quota: HubQuota::Info(t("Armazenamento sob demanda · Buckets B2", "Pay-as-you-go storage · B2 Buckets").into()),
                    open_loc: crate::b2::top(sv),
                    in_pane: std::array::from_fn(&in_pane),
                    edit_id: None,
                    remove_target: actions::AccountTarget::Server(sv.id.clone()),
                });
            }
            Provider::Telegram => {
                let user_name = (!sv.name.trim().is_empty() && sv.name != sv.user).then(|| sv.name.clone());
                out.push(HubCard {
                    key: format!("srv:{}", sv.id),
                    provider,
                    drive_name: "Telegram".into(),
                    user_name,
                    email_or_addr: format!("ID {}", sv.user),
                    quota: HubQuota::Info(t("Armazenamento ilimitado · Canal privado", "Unlimited storage · Private channel").into()),
                    open_loc: crate::servers::folder(sv, Vec::new()),
                    in_pane: std::array::from_fn(&in_pane),
                    edit_id: None,
                    remove_target: actions::AccountTarget::Server(sv.id.clone()),
                });
            }
            Provider::Nextcloud | Provider::OwnCloud | Provider::Sftp | Provider::Ftp | Provider::WebDav | Provider::GoogleDrive | Provider::Service(_) => {
                let drive_name = match (provider, sv.protocol) {
                    (Provider::Ftp, Protocol::Ftps) => "FTPS",
                    (Provider::WebDav, Protocol::Dav) => "WebDAV (HTTP)",
                    _ => provider.label(),
                }
                .to_string();
                let addr = sv.address();
                let user_or_addr = if !sv.user.is_empty() {
                    format!("{}@{}", sv.user, sv.host)
                } else {
                    sv.host.clone()
                };
                let user_name = (!sv.name.trim().is_empty()
                    && !sv.name.eq_ignore_ascii_case(&drive_name)
                    && !sv.name.eq_ignore_ascii_case(&sv.host)
                    && !sv.name.eq_ignore_ascii_case(&user_or_addr))
                .then(|| sv.name.clone());
                out.push(HubCard {
                    key: format!("srv:{}", sv.id),
                    provider,
                    drive_name,
                    user_name,
                    email_or_addr: user_or_addr,
                    quota: HubQuota::Info(addr),
                    open_loc: crate::servers::folder(sv, Vec::new()),
                    in_pane: std::array::from_fn(&in_pane),
                    edit_id: Some(sv.id.clone()),
                    remove_target: actions::AccountTarget::Server(sv.id.clone()),
                });
            }
        }
    }

    out
}

fn accounts_hub(s: &State, k: &Kit) -> El {
    use crate::servers::Provider;

    let all_cards = hub_cards(s);
    let active_filter = s.accounts_filter.filter(|&p| all_cards.iter().any(|c| c.provider == p));
    let search_query = s.accounts_search.text.as_str();

    // Left sidebar: "Todos" + each connected provider type.
    let sidebar_bg = if k.dark { k.alpha(k.c.white, 0.015) } else { k.alpha(k.c.black, 0.014) };
    let mut sidebar = col()
        .key("accounts-sidebar")
        .w(214.0)
        .h_full()
        .pt(14.0)
        .pb(14.0)
        .px(10.0)
        .gap(3.0)
        .fill(sidebar_bg)
        .border_sides([0.0, 1.0, 0.0, 0.0], k.c.border_weak);

    let sidebar_row = |key: String, label: &str, glyph: El, count: usize, on: bool, pick: Option<Provider>| -> El {
        let fill = if on {
            if k.dark {
                rune::oklch(0.30, 0.055, 252.0)
            } else {
                k.alpha(k.c.text_accent, 0.12)
            }
        } else {
            Color::TRANSPARENT
        };
        let text_ink = if on { k.c.text_base } else { k.c.text_muted };
        let font = if on { k.ty.text_small_medium } else { k.ty.text_small };

        let mut r = row()
            .key(key)
            .role("tab")
            .label(label.to_string())
            .checked(on)
            .focusable(true)
            .focus_ring(2.0, 1.0, k.c.border_focus)
            .w_full()
            .h(34.0)
            .pl(6.0)
            .pr(10.0)
            .gap(8.0)
            .center_y()
            .radius(6.0)
            .fill(fill)
            .hover(if on { fill } else { k.c.hover })
            .cursor(Cursor::Pointer)
            .child(el().w(3.0).h(16.0).radius(1.5).fill(if on { k.c.text_accent } else { Color::TRANSPARENT }))
            .child(glyph)
            .child(text(label.to_string()).font(font).color(text_ink).truncate().min_w(0.0).grow(1.0));

        if count > 0 {
            r = r.child(text(count.to_string()).font(k.ty.caption()).color(if on { k.c.text_accent } else { k.c.text_faint }));
        }

        k.interactive(r).on_press(move |s: &mut State| {
            s.accounts_filter = pick;
        })
    };

    let all_on = active_filter.is_none();
    let globe_icon = icon(&icons::GLOBE, 16.0).color(if all_on { k.c.text_accent } else { k.c.icon_muted });
    sidebar = sidebar.child(sidebar_row("hub-filter-all".into(), t("Todos", "All"), globe_icon, all_cards.len(), all_on, None));

    for provider in Provider::all() {
        let count = all_cards.iter().filter(|c| c.provider == provider).count();
        if count == 0 {
            continue;
        }
        let on = active_filter == Some(provider);
        let label = match provider {
            Provider::GoogleDrive => "GoogleDrive",
            _ => provider.label(),
        };
        sidebar = sidebar.child(sidebar_row(
            format!("hub-filter-{provider:?}"),
            label,
            icon(provider.icon(), 17.0),
            count,
            on,
            Some(provider),
        ));
    }

    // Top bar inside Central de Contas: primary blue "+ Adicionar conta" + "Pesquisar" box.
    let add_bg = if k.dark { rune::oklch(0.53, 0.16, 253.0) } else { rune::oklch(0.50, 0.165, 252.0) };
    let add_hover = if k.dark { rune::oklch(0.57, 0.16, 253.0) } else { rune::oklch(0.46, 0.165, 252.0) };
    let add_press = if k.dark { rune::oklch(0.48, 0.155, 253.0) } else { rune::oklch(0.42, 0.16, 252.0) };
    let add_btn = k
        .interactive(
            row()
                .key("hub-add-account")
                .role("button")
                .label(t("Adicionar conta", "Add account"))
                .focusable(true)
                .focus_ring(2.0, 1.0, k.c.border_focus)
                .h(34.0)
                .px(14.0)
                .gap(8.0)
                .center_y()
                .radius(6.0)
                .fill(add_bg)
                .hover(add_hover)
                .press(add_press)
                .cursor(Cursor::Pointer)
                .child(icon(&icons::PERSON_PLUS, 16.0).color(Color::WHITE))
                .child(text(t("Adicionar conta", "Add account")).font(k.ty.text_small_medium).color(Color::WHITE)),
        )
        .on_press(crate::servers::ask_add);

    let search_trailing: El = if !s.accounts_search.text.is_empty() {
        row()
            .key("hub-search-clear")
            .role("button")
            .label(t("Limpar pesquisa", "Clear search"))
            .preserve_focus_on_press(true)
            .size(20.0, 20.0)
            .center()
            .radius(k.radius.sm)
            .hover(k.c.hover)
            .cursor(Cursor::Pointer)
            .on_press(|s: &mut State| {
                s.accounts_search = rune::TextEdit::new("");
                s.accounts_search.focused = true;
            })
            .child(icon(k.glyph.close, 12.0).color(k.c.icon_muted))
    } else {
        icon(&icons::SEARCH, 13.0).color(k.c.icon_muted)
    };

    let search_box = rune_kit::TextInput::new(&s.accounts_search)
        .width(Some(248.0))
        .label(t("Pesquisar contas", "Search accounts"))
        .placeholder(t("Pesquisar", "Search"))
        .trailing(search_trailing)
        .on_edit(|s: &mut State| &mut s.accounts_search)
        .build(k)
        .key("hub-search");

    let waiting_oauth = s.login.is_some() || s.servers.onedrive.login.is_some() || s.servers.dropbox_login.is_some();
    let mut top_bar = row()
        .key("accounts-topbar")
        .w_full()
        .gap(16.0)
        .center_y()
        .child(add_btn)
        .child(search_box);

    if waiting_oauth {
        top_bar = top_bar.child(
            row()
                .key("hub-oauth-waiting")
                .h(28.0)
                .px(10.0)
                .gap(7.0)
                .center_y()
                .radius(14.0)
                .fill(k.alpha(k.c.text_accent, if k.dark { 0.16 } else { 0.09 }))
                .child(el().size(6.5, 6.5).radius(3.5).fill(k.c.text_accent))
                .child(text(t("Aguardando autorização no navegador…", "Waiting for authorization in the browser…")).font(k.ty.caption()).color(k.c.text_accent)),
        );
    }

    let visible: Vec<&HubCard> = all_cards
        .iter()
        .filter(|c| active_filter.is_none_or(|p| c.provider == p) && c.matches_search(search_query))
        .collect();

    let card_bg = if k.dark { rune::oklch(0.245, 0.0, 0.0) } else { k.c.bg_base };
    let card_border = if k.dark { k.alpha(k.c.white, 0.085) } else { k.c.border_weak };

    let list_content: El = if all_cards.is_empty() {
        col()
            .key("hub-empty")
            .w_full()
            .max_w(540.0)
            .mt(24.0)
            .padding(28.0)
            .gap(10.0)
            .items_start()
            .radius(10.0)
            .fill(card_bg)
            .border(1.0, card_border)
            .child(icon(&icons::CLOUD, 32.0))
            .child(text(t("Nenhuma conta conectada ainda", "No accounts connected yet")).font(k.ty.body_medium).color(k.c.text_base))
            .child(
                text(t(
                    "Clique em “Adicionar conta” acima para conectar Google Drive, OneDrive, Dropbox, MEGA, Backblaze B2, Telegram ou um servidor SFTP, FTP ou WebDAV.",
                    "Click “Add account” above to connect Google Drive, OneDrive, Dropbox, MEGA, Backblaze B2, Telegram or an SFTP, FTP or WebDAV server.",
                ))
                .font(k.ty.text_small)
                .color(k.c.text_muted)
                .wrap()
                .w_full()
                .min_w(0.0),
            )
    } else if visible.is_empty() {
        col()
            .key("hub-no-match")
            .w_full()
            .max_w(520.0)
            .mt(12.0)
            .padding(22.0)
            .gap(10.0)
            .items_start()
            .radius(8.0)
            .fill(card_bg)
            .border(1.0, card_border)
            .child(text(t("Nenhuma conta corresponde à pesquisa.", "No accounts match your search.")).font(k.ty.text_small_medium).color(k.c.text_base))
            .child(
                Button::new(t("Limpar filtros", "Clear filters"))
                    .variant(ButtonVariant::Neutral)
                    .on_press(|s: &mut State| {
                        s.accounts_filter = None;
                        s.accounts_search = rune::TextEdit::new("");
                    })
                    .build(k).cursor(Cursor::Pointer),
            )
    } else {
        let mut cards = col().key("hub-cards").w_full().gap(10.0);
        for c in visible {
            // Identity line: bold Drive name + optional display name + muted email/host.
            let mut head = row()
                .w_full()
                .gap(10.0)
                .center_y()
                .min_w(0.0)
                .child(text(c.drive_name.clone()).font(k.ty.text_small_medium).color(k.c.text_base));

            if let Some(u) = &c.user_name {
                head = head.child(text(u.clone()).font(k.ty.text_small).color(k.c.text_base).truncate().shrink().min_w(0.0));
            }
            if !c.email_or_addr.is_empty() {
                head = head.child(
                    text(c.email_or_addr.clone())
                        .font(k.ty.text_small)
                        .color(k.c.text_muted)
                        .truncate()
                        .shrink()
                        .min_w(0.0)
                        .help_when_truncated(c.email_or_addr.clone()),
                );
            }

            // Storage / quota line: "Tamanho 15 GB  Livre 12,1 GB  [━━━━░░░░]"
            let track_bg = k.alpha(k.c.text_base, if k.dark { 0.14 } else { 0.10 });
            let quota_row: El = match &c.quota {
                HubQuota::Known { total, free, used_frac, help } => {
                    let bar_w = 132.0;
                    let fill_w = if *used_frac > 0.0 { (bar_w * *used_frac).max(3.0) } else { 0.0 };
                    let fill_color = if *used_frac >= 0.90 {
                        k.c.danger.fg
                    } else if *used_frac >= 0.78 {
                        k.c.warning.fg
                    } else {
                        k.c.text_accent
                    };
                    let bar = row()
                        .w(bar_w)
                        .h(5.5)
                        .radius(3.0)
                        .fill(track_bg)
                        .clip_soft(3.0)
                        .child(el().w(fill_w).h_full().radius(3.0).fill(fill_color));
                    row()
                        .gap(8.0)
                        .center_y()
                        .help(help.clone())
                        .child(text(t("Tamanho", "Size")).font(k.ty.text_xsmall).color(k.c.text_muted))
                        .child(text(whole_size(*total)).font(k.ty.text_xsmall_medium).color(k.c.text_base))
                        .child(text(t("Livre", "Free")).font(k.ty.text_xsmall).color(k.c.text_muted).ml(4.0))
                        .child(text(format_size(*free)).font(k.ty.text_xsmall_medium).color(k.c.text_base))
                        .child(bar.ml(4.0))
                }
                HubQuota::UnlimitedUsed { used, help } => row()
                    .gap(8.0)
                    .center_y()
                    .help(help.clone())
                    .child(text(t("Usado", "Used")).font(k.ty.text_xsmall).color(k.c.text_muted))
                    .child(text(format_size(*used)).font(k.ty.text_xsmall_medium).color(k.c.text_base))
                    .child(text("·").font(k.ty.text_xsmall).color(k.c.text_faint))
                    .child(text(t("Ilimitado", "Unlimited")).font(k.ty.text_xsmall).color(k.c.text_muted)),
                HubQuota::Loading => row()
                    .gap(8.0)
                    .center_y()
                    .child(text(t("Tamanho", "Size")).font(k.ty.text_xsmall).color(k.c.text_muted))
                    .child(text("—").font(k.ty.text_xsmall).color(k.c.text_faint))
                    .child(text(t("Livre", "Free")).font(k.ty.text_xsmall).color(k.c.text_muted).ml(4.0))
                    .child(row().w(120.0).h(5.0).radius(2.5).fill(track_bg).ml(2.0)),
                HubQuota::Info(info) => row()
                    .gap(6.0)
                    .center_y()
                    .child(text(info.clone()).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().min_w(0.0)),
            };

            let pane_btn = |side_key: &str, pane: usize, glyph: &'static Icon, short: &str, help_text: &str, active_here: bool, loc: Location| -> El {
                let bg = if active_here {
                    k.alpha(k.c.text_accent, if k.dark { 0.20 } else { 0.10 })
                } else if k.dark {
                    k.alpha(k.c.white, 0.045)
                } else {
                    k.alpha(k.c.black, 0.03)
                };
                let border = if active_here {
                    k.alpha(k.c.text_accent, 0.45)
                } else {
                    k.c.border_weak
                };
                let ink = if active_here { k.c.text_accent } else { k.c.text_base };
                let icon_ink = if active_here { k.c.text_accent } else { k.c.icon_muted };

                k.interactive(
                    row()
                        .key(format!("{}-{side_key}", c.key))
                        .role("button")
                        .label(help_text.to_string())
                        .help(help_text.to_string())
                        .focusable(true)
                        .focus_ring(2.0, 1.0, k.c.border_focus)
                        .h(30.0)
                        .px(9.0)
                        .gap(5.5)
                        .center_y()
                        .radius(k.radius.sm)
                        .fill(bg)
                        .border(1.0, border)
                        .hover(if active_here { k.alpha(k.c.text_accent, if k.dark { 0.28 } else { 0.16 }) } else { k.c.hover })
                        .press(k.c.pressed)
                        .cursor(Cursor::Pointer)
                        .child(icon(glyph, 15.0).color(icon_ink))
                        .child(text(short.to_string()).font(k.ty.text_xsmall_medium).color(ink)),
                )
                .on_press(move |s: &mut State| crate::open_account_in_pane(s, pane, loc.clone()))
            };

            let mut actions_bar = row().gap(6.0).center_y();
            match s.settings.pane_layout {
                crate::settings::PaneLayout::One => {
                    actions_bar = actions_bar.child(pane_btn(
                        "open-left",
                        0,
                        &icons::VIEW_ONE_PANE,
                        t("Abrir", "Open"),
                        t("Abrir este drive no painel", "Open this drive in the pane"),
                        c.in_pane[0],
                        c.open_loc.clone(),
                    ));
                }
                crate::settings::PaneLayout::Two => {
                    actions_bar = actions_bar
                        .child(pane_btn(
                            "open-left",
                            0,
                            &icons::OPEN_LEFT_PANE,
                            t("Esquerda", "Left"),
                            t("Abrir este drive no painel esquerdo", "Open this drive in the left pane"),
                            c.in_pane[0],
                            c.open_loc.clone(),
                        ))
                        .child(pane_btn(
                            "open-right",
                            1,
                            &icons::OPEN_RIGHT_PANE,
                            t("Direita", "Right"),
                            t("Abrir este drive no painel direito", "Open this drive in the right pane"),
                            c.in_pane[1],
                            c.open_loc.clone(),
                        ));
                }
                crate::settings::PaneLayout::Four => {
                    let targets = [
                        ("open-p0", 0, &icons::OPEN_LEFT_PANE, "1", t("Abrir no painel superior esquerdo", "Open in top-left pane")),
                        ("open-p1", 1, &icons::OPEN_RIGHT_PANE, "2", t("Abrir no painel superior direito", "Open in top-right pane")),
                        ("open-p2", 2, &icons::OPEN_LEFT_PANE, "3", t("Abrir no painel inferior esquerdo", "Open in bottom-left pane")),
                        ("open-p3", 3, &icons::OPEN_RIGHT_PANE, "4", t("Abrir no painel inferior direito", "Open in bottom-right pane")),
                    ];
                    for (side_key, idx, glyph, short, help_text) in targets {
                        actions_bar = actions_bar.child(pane_btn(side_key, idx, glyph, short, help_text, c.in_pane[idx], c.open_loc.clone()));
                    }
                }
            }

            if let Some(edit_id) = &c.edit_id {
                let id = edit_id.clone();
                actions_bar = actions_bar.child(
                    k.interactive(
                        row()
                            .key(format!("{}-edit", c.key))
                            .role("button")
                            .label(t("Editar conta", "Edit account"))
                            .help(t("Editar endereço ou credenciais", "Edit address or credentials"))
                            .focusable(true)
                            .focus_ring(2.0, 1.0, k.c.border_focus)
                            .size(30.0, 30.0)
                            .center()
                            .radius(k.radius.sm)
                            .border(1.0, k.c.border_weak)
                            .hover(k.c.hover)
                            .press(k.c.pressed)
                            .cursor(Cursor::Pointer)
                            .child(icon(&icons::M_RENAME, 14.0).color(k.c.icon_muted)),
                    )
                    .on_press(move |s: &mut State| crate::servers::ask_edit(s, &id)),
                );
            }

            let target = c.remove_target.clone();
            let prov_name = c.drive_name.clone();
            let who = c.email_or_addr.clone();
            let del_btn = k
                .interactive(
                    row()
                        .key(format!("{}-delete", c.key))
                        .role("button")
                        .label(t("Remover conta", "Remove account"))
                        .help(t("Remover esta conta do Open Explorer", "Remove this account from Open Explorer"))
                        .focusable(true)
                        .focus_ring(2.0, 1.0, k.c.border_focus)
                        .size(30.0, 30.0)
                        .center()
                        .radius(k.radius.sm)
                        .border(1.0, k.c.border_weak)
                        .hover(k.alpha(k.c.danger.fg, if k.dark { 0.18 } else { 0.10 }))
                        .press(k.alpha(k.c.danger.fg, if k.dark { 0.26 } else { 0.16 }))
                        .cursor(Cursor::Pointer)
                        .child(icon(&icons::M_DELETE, 14.5).color(k.c.danger.fg)),
                )
                .on_press(move |s: &mut State| {
                    s.dialog = Some(Dialog::RemoveAccount {
                        target: target.clone(),
                        provider: prov_name.clone(),
                        who: who.clone(),
                    });
                });
            actions_bar = actions_bar.child(del_btn);

            let card = row()
                .key(format!("hub-card-{}", c.key))
                .w_full()
                .max_w(880.0)
                .px(16.0)
                .py(13.0)
                .gap(14.0)
                .center_y()
                .radius(8.0)
                .fill(card_bg)
                .border(1.0, card_border)
                .child(row().size(38.0, 38.0).center().child(icon(c.provider.icon(), 33.0)))
                .child(col().grow(1.0).w(0.0).min_w(0.0).gap(5.0).child(head).child(quota_row))
                .child(actions_bar);

            cards = cards.child(card);
        }
        cards
    };

    let scroll = col().key("accounts-scroll").w_full().h_full().scroll(ScrollSpec { stick_bottom: false }).child(col().w_full().pb(16.0).child(list_content));
    let scroll = crate::scrollbars::v_bar(k, "accounts-scroll", scroll).w_full().grow(1.0);

    let main_col = col()
        .key("accounts-main")
        .grow(1.0)
        .w(0.0)
        .min_w(0.0)
        .h_full()
        .pt(14.0)
        .px(18.0)
        .gap(14.0)
        .child(top_bar)
        .child(scroll);

    row()
        .key("accounts-hub")
        .w_full()
        .grow(1.0)
        .min_h(160.0)
        .child(sidebar)
        .child(main_col)
}

// ---------------------------------------------------------------------------------------------
// Splitters

/// The drag that resizes something to `size`: Down keeps the starting size, each move sets it
/// to the start plus the travel (through `clamp`), a cancelled drag puts it back, and a double
/// click sets `default`. Returns whether the layout changed and should be saved: on a double
/// click, and on release when the size ended up different.
/// `step` is how far the pointer moved along the size: `d.dx` for a width, `-d.dy` for a
/// height grown upwards.
pub(crate) fn resize(d: &mut Drag, step: f32, size: &mut f32, default: f32, clamp: impl Fn(f32) -> f32) -> bool {
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
        .label(t("Arraste para redimensionar; clique duas vezes para voltar ao padrão", "Drag to resize; double-click to restore the default"))
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

/// The explore panes: one full-width pane, two side by side (default), or four in a 2×2 grid.
/// `backdrop`: Mica shows behind the window (Opções, Experimental).
fn panes(s: &State, k: &Kit, backdrop: bool, layer: &mut Vec<El>) -> El {
    match s.settings.pane_layout {
        crate::settings::PaneLayout::One => row()
            .key("panes")
            .w_full()
            .grow(1.0)
            .min_h(160.0)
            .padding(6.0)
            .child(pane(s, k, 0, backdrop, layer)),
        crate::settings::PaneLayout::Two => row()
            .key("panes")
            .w_full()
            .grow(1.0)
            .min_h(160.0)
            .padding(6.0)
            .child(pane(s, k, 0, backdrop, layer))
            .child(gutter(s, k, 0, 1, false))
            .child(pane(s, k, 1, backdrop, layer)),
        crate::settings::PaneLayout::Four => {
            let pair = |row_key: &'static str, left: usize, right: usize, layer: &mut Vec<El>| {
                row()
                    .key(row_key)
                    .w_full()
                    .grow(1.0)
                    .h(0.0)
                    .min_h(120.0)
                    .child(pane(s, k, left, backdrop, layer))
                    .child(gutter(s, k, left, right, true))
                    .child(pane(s, k, right, backdrop, layer))
            };
            col()
                .key("panes")
                .w_full()
                .grow(1.0)
                .min_h(260.0)
                .padding(6.0)
                .gap(6.0)
                .child(pair("panes-top", 0, 1, layer))
                .child(pair("panes-bottom", 2, 3, layer))
        }
    }
}

/// The strip between two horizontal panes, as in Air Explorer: the window's own background, empty
/// but for two small triangles that copy the selection of one pane into the folder the other
/// shows. A triangle is blue while its pane has something selected, grey otherwise.
fn gutter(s: &State, k: &Kit, left: usize, right: usize, centered: bool) -> El {
    let arrow = |key: String, glyph: &'static Icon, help: &str, from: usize, to: usize| {
        let ready = !s.tabs[from].is_picker() && !s.tabs[to].is_picker() && !s.panes[from].selected.is_empty();
        let ink = if ready { k.alpha(k.c.text_accent, 0.7) } else { k.alpha(k.c.icon_faint, 0.6) };
        let b = el().key(key).role("button").label(help.to_string()).help(help.to_string()).size(28.0, 28.0).center().radius(k.radius.sm);
        let b = if ready { b.hover(k.c.hover).press(k.c.pressed).cursor(Cursor::Pointer) } else { b };
        b.on_press(move |s: &mut State| {
            if !s.tabs[from].is_picker() && !s.tabs[to].is_picker() {
                actions::copy_across(s, from, to);
            }
        })
        .child(icon(glyph, 16.0).color(ink))
    };
    let (gutter_key, right_key, left_key) = if (left, right) == (0, 1) {
        ("gutter".to_string(), "copy-right".to_string(), "copy-left".to_string())
    } else {
        (format!("gutter-{left}-{right}"), format!("copy-right-{left}-{right}"), format!("copy-left-{left}-{right}"))
    };
    let mut g = col().key(gutter_key).w(40.0).h_full().items_center().gap(6.0);
    g = if centered { g.justify_center() } else { g.pt(126.0) };
    g.child(arrow(right_key, &icons::COPY_RIGHT, t("Copiar a seleção do painel esquerdo para o direito", "Copy the selection from the left pane to the right"), left, right))
        .child(arrow(left_key, &icons::COPY_LEFT, t("Copiar a seleção do painel direito para o esquerdo", "Copy the selection from the right pane to the left"), right, left))
}

// ---------------------------------------------------------------------------------------------
// Pane & Tabs

/// The tab strip sitting above pane `p`: one tab per open source (showing its icon, name,
/// and close button) followed by the "+" tab that opens the new-tab / account picker.
/// The active tab merges seamlessly into the pane body below (no bottom line); inactive tabs
/// and the inactive "+" button draw only the continuous top border of the pane and highlight
/// on hover.
fn tab_strip(s: &State, k: &Kit, p: usize, pane_border: Color) -> El {
    let tabs = &s.tabs[p];
    let hover_bg = if k.dark { k.alpha(k.c.white, 0.065) } else { k.alpha(k.c.black, 0.055) };
    let top_corners = [6.0, 6.0, 0.0, 0.0];

    let mut strip = row()
        .key(format!("pane-tabs-{p}"))
        .w_full()
        .h(30.0)
        .items_end()
        .min_w(0.0);

    for (idx, slot) in tabs.items.iter().enumerate() {
        let is_active = tabs.active == Some(idx);
        let loc = tabs.location_of(&s.panes[p], idx);
        let (icon_key, glyph, label) = crate::tabs::tab_header(s, loc);

        let fg = if is_active { k.c.text_base } else { k.c.text_muted };

        let select_area = row()
            .key(format!("tab-sel-{p}-{}", slot.id))
            .role("tab")
            .label(label.clone())
            .checked(is_active)
            .h_full()
            .pl(10.0)
            .pr(4.0)
            .gap(6.0)
            .center_y()
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| crate::tabs::select_tab(s, p, idx))
            .child(shell_icon(s, icon_key.as_ref(), glyph, 15.0))
            .child(text(label).font(k.ty.text_small).color(fg).truncate().max_w(160.0).min_w(0.0));

        let close_btn = row()
            .key(format!("tab-close-{p}-{}", slot.id))
            .role("button")
            .label(t("Fechar aba", "Close tab"))
            .help(t("Fechar aba (Ctrl+W)", "Close tab (Ctrl+W)"))
            .size(18.0, 18.0)
            .mr(5.0)
            .center()
            .radius(k.radius.sm)
            .hover(if k.dark { k.alpha(k.c.white, 0.12) } else { k.alpha(k.c.black, 0.09) })
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| crate::tabs::close_tab(s, p, idx))
            .child(icon(k.glyph.close, 10.0).color(if is_active { k.c.icon_muted } else { k.c.icon_faint }));

        let tab = if is_active {
            row()
                .key(format!("tab-{p}-{}", slot.id))
                .h(30.0)
                .center_y()
                .corners(top_corners)
                .fill(k.c.bg_base)
                .border_sides([1.0, 1.0, 0.0, 1.0], pane_border)
                .child(select_area)
                .child(close_btn)
        } else {
            row()
                .key(format!("tab-{p}-{}", slot.id))
                .h(30.0)
                .center_y()
                .corners(top_corners)
                .fill(Color::TRANSPARENT)
                .hover(hover_bg)
                .border_sides([0.0, 0.0, 1.0, 0.0], pane_border)
                .child(select_area)
                .child(close_btn)
        };

        strip = strip.child(tab);
    }

    // The "+" tab button at the end of the strip: transparent (no box/border) when inactive,
    // highlighting only on hover; merges into the pane body when active.
    let picker_on = tabs.is_picker();
    let plus = if picker_on {
        row()
            .key(format!("tab-plus-{p}"))
            .role("tab")
            .label(t("Nova aba", "New tab"))
            .help(t("Abrir nova aba (Ctrl+T)", "Open new tab (Ctrl+T)"))
            .checked(true)
            .w(48.0)
            .h(30.0)
            .center()
            .corners(top_corners)
            .fill(k.c.bg_base)
            .border_sides([1.0, 1.0, 0.0, 1.0], pane_border)
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| crate::tabs::open_picker(s, p))
            .child(icon(&icons::PLUS, 13.0).color(k.c.text_base))
    } else {
        row()
            .key(format!("tab-plus-{p}"))
            .role("tab")
            .label(t("Nova aba", "New tab"))
            .help(t("Abrir nova aba (Ctrl+T)", "Open new tab (Ctrl+T)"))
            .checked(false)
            .w(36.0)
            .h(30.0)
            .center()
            .corners(top_corners)
            .fill(Color::TRANSPARENT)
            .hover(hover_bg)
            .border_sides([0.0, 0.0, 1.0, 0.0], pane_border)
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| crate::tabs::open_picker(s, p))
            .child(icon(&icons::PLUS, 13.0).color(k.c.icon_muted))
    };

    // Continues the pane's top border cleanly to the right edge.
    let trailing = el()
        .key(format!("tab-trail-{p}"))
        .grow(1.0)
        .h(30.0)
        .border_sides([0.0, 0.0, 1.0, 0.0], pane_border);

    strip.child(plus).child(trailing)
}

/// The "+" new-tab view inside pane `p`: provider filter, search box, "Registrar conta",
/// local quick folders (Este Computador, Documentos, Imagens), and the list of connected
/// cloud accounts & servers.
fn new_tab_picker(s: &State, k: &Kit, p: usize, layer: &mut Vec<El>) -> El {
    use crate::servers::Provider;

    let picker = &s.tabs[p].picker;
    let all_cards = hub_cards(s);
    let active_filter = picker.filter.filter(|&prov| all_cards.iter().any(|c| c.provider == prov));
    let query = picker.search.text.as_str();
    let q_lower = query.trim().to_lowercase();

    // 1. Top row: Provider filter dropdown ("Todos ▾") + Search box ("Pesquisar").
    let anchor_key = format!("tab-picker-filter-{p}");
    let filter_label = match active_filter {
        None => t("Todos", "All"),
        Some(Provider::GoogleDrive) => "GoogleDrive",
        Some(prov) => prov.label(),
    };
    let filter_glyph: El = match active_filter {
        None => icon(&icons::GLOBE, 15.0).color(k.c.icon_muted),
        Some(prov) => icon(prov.icon(), 16.0),
    };

    let filter_btn = k
        .interactive(
            row()
                .key(anchor_key.clone())
                .role("button")
                .label(filter_label.to_string())
                .focusable(true)
                .focus_ring(2.0, 1.0, k.c.border_focus)
                .w(148.0)
                .h(30.0)
                .px(9.0)
                .gap(7.0)
                .center_y()
                .radius(k.radius.sm)
                .fill(k.c.bg_base)
                .border(1.0, if picker.filter_open { k.c.border_focus } else { k.c.border_weak })
                .hover(k.c.hover)
                .cursor(Cursor::Pointer)
                .child(filter_glyph)
                .child(text(filter_label.to_string()).font(k.ty.text_small).color(k.c.text_base).truncate().min_w(0.0).grow(1.0))
                .child(icon(&icons::CHEVRON_DOWN, 12.0).color(k.c.icon_muted)),
        )
        .on_press(move |s: &mut State| {
            let next = !s.tabs[p].picker.filter_open;
            crate::tabs::close_picker_filter(s);
            s.tabs[p].picker.filter_open = next;
        });

    if picker.filter_open {
        layer.push(rune_kit::scrim("tab-picker-filter-scrim", crate::tabs::close_picker_filter));
        let dropdown_bg = if k.dark { rune::oklch(0.22, 0.008, 255.0) } else { k.c.bg_base };
        let mut menu = col().key(format!("tab-picker-filter-menu-{p}")).w(196.0).padding(4.0).gap(2.0);

        let make_opt = |opt_key: String, label: &str, glyph: El, count: usize, selected: bool, pick: Option<Provider>| -> El {
            row()
                .key(opt_key)
                .role("menuitem")
                .label(label.to_string())
                .w_full()
                .h(30.0)
                .px(8.0)
                .gap(8.0)
                .center_y()
                .radius(k.radius.sm)
                .fill(if selected { k.alpha(k.c.text_accent, if k.dark { 0.20 } else { 0.10 }) } else { Color::TRANSPARENT })
                .hover(k.c.hover)
                .cursor(Cursor::Pointer)
                .on_press(move |s: &mut State| {
                    s.tabs[p].picker.filter = pick;
                    s.tabs[p].picker.filter_open = false;
                })
                .child(glyph)
                .child(text(label.to_string()).font(if selected { k.ty.text_small_medium } else { k.ty.text_small }).color(k.c.text_base).truncate().min_w(0.0).grow(1.0))
                .child(text(count.to_string()).font(k.ty.caption()).color(k.c.text_faint))
        };

        menu = menu.child(make_opt(
            format!("tp-opt-all-{p}"),
            t("Todos", "All"),
            icon(&icons::GLOBE, 15.0).color(k.c.icon_muted),
            all_cards.len(),
            active_filter.is_none(),
            None,
        ));

        for prov in Provider::all() {
            let count = all_cards.iter().filter(|c| c.provider == prov).count();
            if count == 0 {
                continue;
            }
            let lbl = match prov {
                Provider::GoogleDrive => "GoogleDrive",
                _ => prov.label(),
            };
            menu = menu.child(make_opt(
                format!("tp-opt-{p}-{prov:?}"),
                lbl,
                icon(prov.icon(), 16.0),
                count,
                active_filter == Some(prov),
                Some(prov),
            ));
        }

        let surface = menu
            .radius(k.radius.md)
            .fill(dropdown_bg)
            .border(1.0, k.c.border_weak)
            .box_shadow(rune::Shadow { y: 6.0, blur: 18.0, color: k.dark_a(0.32), ..Default::default() });
        layer.push(rune_kit::popover::anchored(k, surface, &anchor_key, rune_kit::Placement::BottomStart, 4.0));
    }

    let search_trailing: El = if !picker.search.text.is_empty() {
        row()
            .key(format!("tp-search-clear-{p}"))
            .role("button")
            .label(t("Limpar pesquisa", "Clear search"))
            .preserve_focus_on_press(true)
            .size(20.0, 20.0)
            .center()
            .radius(k.radius.sm)
            .hover(k.c.hover)
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| {
                s.tabs[p].picker.search = rune::TextEdit::new("");
                s.tabs[p].picker.search.focused = true;
            })
            .child(icon(k.glyph.close, 11.0).color(k.c.icon_muted))
    } else {
        icon(&icons::SEARCH, 13.0).color(k.c.icon_muted)
    };

    let search_input = rune_kit::TextInput::new(&picker.search)
        .width(Some(190.0))
        .label(t("Pesquisar contas", "Search accounts"))
        .placeholder(t("Pesquisar", "Search"))
        .trailing(search_trailing)
        .on_edit(move |s: &mut State| &mut s.tabs[p].picker.search)
        .build(k)
        .key(format!("tp-search-{p}"));

    let top_row = row()
        .key(format!("tp-top-{p}"))
        .w_full()
        .gap(8.0)
        .center_y()
        .child(filter_btn)
        .child(search_input);

    // 2. "Registrar conta" action link (opens the same Add Account window).
    let register_row = row()
        .key(format!("tp-register-{p}"))
        .role("button")
        .label(t("Registrar conta", "Register account"))
        .h(30.0)
        .px(6.0)
        .gap(8.0)
        .center_y()
        .radius(k.radius.sm)
        .hover(k.alpha(k.c.text_accent, if k.dark { 0.14 } else { 0.08 }))
        .press(k.alpha(k.c.text_accent, if k.dark { 0.22 } else { 0.14 }))
        .cursor(Cursor::Pointer)
        .on_press(crate::servers::ask_add)
        .child(icon(&icons::PERSON_PLUS, 16.0).color(k.c.text_accent))
        .child(text(t("Registrar conta", "Register account")).font(k.ty.text_small_medium).color(k.c.text_accent));

    // 3. Quick local places row: Este Computador, Documentos, Imagens.
    let mut quick_row = row().key(format!("tp-quick-{p}")).w_full().gap(6.0).center_y();
    let mut has_quick = false;
    let matches_local = |label: &str| q_lower.is_empty() || label.to_lowercase().contains(&q_lower);

    let pc_label = t("Este Computador", "This PC");
    if active_filter.is_none() && matches_local(pc_label) {
        has_quick = true;
        let pc_loc = crate::tabs::computer_location(s);
        quick_row = quick_row.child(
            row()
                .key(format!("tp-quick-pc-{p}"))
                .role("button")
                .label(pc_label.to_string())
                .h(30.0)
                .px(8.0)
                .gap(7.0)
                .center_y()
                .radius(k.radius.sm)
                .hover(k.c.hover)
                .press(k.c.pressed)
                .cursor(Cursor::Pointer)
                .on_press(move |s: &mut State| crate::tabs::open_new_tab(s, p, pc_loc.clone()))
                .child(shell_icon(s, Some(&IconKey::Computer), &icons::COMPUTER, 16.0))
                .child(text(pc_label.to_string()).font(k.ty.text_small).color(k.c.text_base)),
        );
    }

    if active_filter.is_none()
        && let Some((doc_path, doc_name)) = &s.places.documents
        && matches_local(doc_name)
    {
        has_quick = true;
        let loc = Location::Local(doc_path.clone());
        quick_row = quick_row.child(
            row()
                .key(format!("tp-quick-docs-{p}"))
                .role("button")
                .label(doc_name.clone())
                .h(30.0)
                .px(8.0)
                .gap(7.0)
                .center_y()
                .radius(k.radius.sm)
                .hover(k.c.hover)
                .press(k.c.pressed)
                .cursor(Cursor::Pointer)
                .on_press(move |s: &mut State| crate::tabs::open_new_tab(s, p, loc.clone()))
                .child(shell_icon(s, Some(&IconKey::Path(doc_path.clone())), &icons::DOC, 16.0))
                .child(text(doc_name.clone()).font(k.ty.text_small).color(k.c.text_base)),
        );
    }

    if active_filter.is_none()
        && let Some((pic_path, pic_name)) = &s.places.pictures
        && matches_local(pic_name)
    {
        has_quick = true;
        let loc = Location::Local(pic_path.clone());
        quick_row = quick_row.child(
            row()
                .key(format!("tp-quick-pics-{p}"))
                .role("button")
                .label(pic_name.clone())
                .h(30.0)
                .px(8.0)
                .gap(7.0)
                .center_y()
                .radius(k.radius.sm)
                .hover(k.c.hover)
                .press(k.c.pressed)
                .cursor(Cursor::Pointer)
                .on_press(move |s: &mut State| crate::tabs::open_new_tab(s, p, loc.clone()))
                .child(shell_icon(s, Some(&IconKey::Path(pic_path.clone())), &icons::IMAGE, 16.0))
                .child(text(pic_name.clone()).font(k.ty.text_small).color(k.c.text_base)),
        );
    }

    // 4. Collapsible "Contas" section with count badge & edit/manage icon.
    let visible_cards: Vec<&HubCard> = all_cards
        .iter()
        .filter(|c| active_filter.is_none_or(|prov| c.provider == prov))
        .filter(|c| c.matches_search(query))
        .collect();

    let pill_bg = k.alpha(k.c.text_base, if k.dark { 0.11 } else { 0.07 });
    let accounts_toggle = row()
        .key(format!("tp-accounts-toggle-{p}"))
        .role("button")
        .label(t("Contas", "Accounts"))
        .h(26.0)
        .px(4.0)
        .gap(6.0)
        .center_y()
        .radius(k.radius.sm)
        .hover(k.c.hover)
        .cursor(Cursor::Pointer)
        .on_press(move |s: &mut State| {
            s.tabs[p].picker.accounts_open = !s.tabs[p].picker.accounts_open;
        })
        .child(icon(if picker.accounts_open { &icons::CHEVRON_DOWN } else { &icons::CHEVRON }, 12.0).color(k.c.icon_muted))
        .child(text(t("Contas", "Accounts")).font(k.ty.text_small_medium).color(k.c.text_muted))
        .child(
            row()
                .h(17.0)
                .px(6.0)
                .center()
                .radius(8.5)
                .fill(pill_bg)
                .child(text(visible_cards.len().to_string()).font(k.ty.caption()).color(k.c.text_muted)),
        );

    let manage_btn = row()
        .key(format!("tp-accounts-manage-{p}"))
        .role("button")
        .label(t("Gerenciar contas", "Manage accounts"))
        .help(t("Gerenciar contas conectadas", "Manage connected accounts"))
        .size(24.0, 24.0)
        .center()
        .radius(k.radius.sm)
        .hover(k.c.hover)
        .cursor(Cursor::Pointer)
        .on_press(|s: &mut State| crate::set_main_tab(s, MainTab::Accounts))
        .child(icon(&icons::M_RENAME, 13.0).color(k.c.icon_muted));

    let accounts_header = row()
        .key(format!("tp-accounts-head-{p}"))
        .w_full()
        .gap(4.0)
        .center_y()
        .child(accounts_toggle)
        .child(manage_btn);

    let mut content = col()
        .key(format!("tp-body-{p}"))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .padding(14.0)
        .gap(10.0)
        .child(top_row)
        .child(register_row);

    if has_quick {
        content = content.child(quick_row);
    }

    content = content.child(accounts_header);

    if picker.accounts_open {
        let mut accounts_list = col()
            .key(format!("tp-accounts-list-{p}"))
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .pl(4.0)
            .gap(2.0)
            .scroll(ScrollSpec { stick_bottom: false });

        if visible_cards.is_empty() {
            let msg = if all_cards.is_empty() {
                t("Nenhuma conta conectada. Clique em Registrar conta acima.", "No accounts connected. Click Register account above.")
            } else {
                t("Nenhuma conta encontrada.", "No accounts found.")
            };
            accounts_list = accounts_list.child(
                row()
                    .px(8.0)
                    .py(10.0)
                    .child(text(msg).font(k.ty.text_small).color(k.c.text_faint)),
            );
        } else {
            for c in visible_cards {
                let loc = c.open_loc.clone();
                let mut item = row()
                    .key(format!("tp-acc-{p}-{}", c.key))
                    .role("button")
                    .label(format!("{} {}", c.drive_name, c.email_or_addr))
                    .w_full()
                    .h(34.0)
                    .px(8.0)
                    .gap(8.0)
                    .center_y()
                    .radius(k.radius.sm)
                    .hover(k.c.hover)
                    .press(k.c.pressed)
                    .cursor(Cursor::Pointer)
                    .on_press(move |s: &mut State| crate::tabs::open_new_tab(s, p, loc.clone()))
                    .child(icon(c.provider.icon(), 18.0))
                    .child(text(c.drive_name.clone()).font(k.ty.text_small).color(k.c.text_muted));

                if let Some(user_name) = &c.user_name {
                    item = item.child(text(user_name.clone()).font(k.ty.text_small_medium).color(k.c.text_base));
                }

                item = item.child(
                    text(c.email_or_addr.clone())
                        .font(k.ty.text_small)
                        .color(k.c.text_muted)
                        .truncate()
                        .min_w(0.0)
                        .grow(1.0),
                );

                accounts_list = accounts_list.child(item);
            }
        }

        content = content.child(accounts_list);
    }

    content
}

fn pane(s: &State, k: &Kit, p: usize, backdrop: bool, layer: &mut Vec<El>) -> El {
    let pane = &s.panes[p];
    let is_picker = s.tabs[p].is_picker();
    let drop_over = !is_picker && pane.drop_over;
    let border = if drop_over {
        k.c.text_accent
    } else if s.active == p {
        // Just a hint of the accent: the active pane shouldn't shout.
        k.alpha(k.c.border_focus, 0.3)
    } else {
        k.c.border_weak
    };
    // Under Mica the card lets it through, and with it the folder tree and the status bar; the
    // bars on top, the file list and the "+" picker keep the pane's own background.
    let card_fill = if drop_over {
        k.alpha(k.c.text_accent, 0.06)
    } else if backdrop {
        Color::TRANSPARENT
    } else {
        k.c.bg_base
    };
    // Over a drop, those parts carry the card's highlight, which they cover.
    let solid_fill = if drop_over { k.c.bg_base.mix(k.c.text_accent, 0.06) } else { k.c.bg_base };
    let solid = |e: El| if backdrop { e.fill(solid_fill) } else { e };

    let mut card = col()
        .key(pane_key(p))
        .focusable(true)
        .label(tr!("Painel {}", "Pane {}", p + 1))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .min_w(0.0)
        .corners([0.0, 0.0, k.radius.sm, k.radius.sm])
        .fill(card_fill)
        .border_sides(if drop_over { [2.0, 2.0, 2.0, 2.0] } else { [0.0, 1.0, 1.0, 1.0] }, border)
        .clip_rounded(0.0)
        .on_key(move |s: &mut State, ev: &KeyEvent, tx: &mut Tx| key(s, p, ev, tx))
        .on_press(move |s: &mut State| s.active = p)
        // The mouse's side buttons go back and forward in the tab's history; over the "+"
        // picker they do nothing, rather than reach the other pane.
        .on_button(move |s: &mut State, button, _: &mut Tx| crate::side_button(s, p, button))
        // While the folder loads (or a search runs), the arrow with the busy circle, as in
        // Explorer: the pane still takes clicks. Buttons and fields keep their own pointers.
        .cursor(if !is_picker && pane.loading { Cursor::Progress } else { Cursor::Default });

    if is_picker {
        card = card.child(solid(new_tab_picker(s, k, p, layer)));
    } else {
        card = card
            .on_drop_files(move |s: &mut State, files: &[PathBuf], _| {
                crate::autoscroll::left(&mut s.autoscroll, p);
                dropped(s, p, files)
            })
            .on_drop_hover(move |s: &mut State, over, _| {
                s.panes[p].drop_over = over;
                if !over {
                    crate::autoscroll::left(&mut s.autoscroll, p);
                }
            })
            // Held near the top or bottom of the list or the tree, the drag scrolls it.
            .on_drop_move(move |s: &mut State, at: rune::PointerEvent, tx: &mut Tx| {
                crate::autoscroll::moved(&mut s.autoscroll, p, (at.window_x, at.window_y), tx.clock(), tx)
            })
            .child(solid(navbar(s, k, p, layer)))
            .child(solid(pane_actions(s, k, p, layer)))
            .child(solid(search_bar(s, k, p)))
            .child({
                let mut body = row().key("body").instant_changes().w_full().grow(1.0).min_h(0.0);
                if s.show_tree[p] {
                    body = body
                        .child(tree(s, k, p))
                        .child(
                            splitter(k, "tree-split", 5.0, Split::Right, move |s: &mut State| &mut s.layout.tree[p], layout::TREE_W, clamp_tree)
                                .child(el().w(1.0).h_full().fill(k.c.border_weak)),
                        );
                }
                body.child(solid(list(s, k, p)))
            })
            .child(statusbar(s, k, p));
    }

    col()
        .key(format!("pane-wrap-{p}"))
        .w(0.0)
        .grow(1.0)
        .h_full()
        .min_w(0.0)
        .child(tab_strip(s, k, p, border))
        .child(card)
}

/// Action strip directly beneath the navigation bar: split view-mode button (click icon to cycle,
/// click chevron to pick exact mode), toggle folder tree visibility, and create a new folder.
fn pane_actions(s: &State, k: &Kit, p: usize, layer: &mut Vec<El>) -> El {
    let cur_mode = s.panes[p].view_mode;
    let menu_open = s.view_mode_menu == Some(p);
    let split_anchor = format!("view-mode-split-{p}");
    let cycle_help = tr!(
        "Alterar visualização (atual: {})",
        "Switch view mode (current: {})",
        cur_mode.label()
    );
    let pick_help = t("Escolher modo de visualização", "Choose view mode");

    let split_bg = if menu_open {
        k.c.hover
    } else if k.dark {
        k.alpha(k.c.white, 0.04)
    } else {
        k.alpha(k.c.black, 0.03)
    };
    let view_cycle_btn = row()
        .key(format!("view-mode-cycle-{p}"))
        .role("button")
        .label(cycle_help.clone())
        .help(cycle_help)
        .w(24.0)
        .h_full()
        .center()
        .corners([k.radius.sm, 0.0, 0.0, k.radius.sm])
        .hover(k.c.hover)
        .press(k.c.pressed)
        .cursor(Cursor::Pointer)
        .on_press(move |s: &mut State| crate::cycle_view_mode(s, p))
        .child(icon(cur_mode.icon(), 15.0).color(k.c.text_base));

    let view_drop_btn = row()
        .key(format!("view-mode-drop-{p}"))
        .role("button")
        .label(pick_help.to_string())
        .help(pick_help.to_string())
        .w(16.0)
        .h_full()
        .center()
        .corners([0.0, k.radius.sm, k.radius.sm, 0.0])
        .hover(k.c.hover)
        .press(k.c.pressed)
        .cursor(Cursor::Pointer)
        .on_press(move |s: &mut State| {
            let next = if s.view_mode_menu == Some(p) { None } else { Some(p) };
            s.active = p;
            s.view_menu_open = false;
            crate::tabs::close_picker_filter(s);
            s.menu = None;
            s.tree_menu = None;
            s.address_menu = None;
            s.search_scope_menu = None;
            s.view_mode_menu = next;
        })
        .child(icon(&icons::CHEVRON_DOWN, 10.0).color(k.c.icon_muted));

    let view_split = row()
        .key(split_anchor.clone())
        .h(24.0)
        .radius(k.radius.sm)
        .fill(split_bg)
        .border(1.0, k.c.border_weak)
        .child(view_cycle_btn)
        .child(el().w(1.0).h(14.0).fill(k.c.border_weak))
        .child(view_drop_btn);

    if menu_open {
        layer.push(rune_kit::scrim("view-mode-scrim", |s: &mut State| s.view_mode_menu = None));
        let dropdown_bg = if k.dark { rune::oklch(0.22, 0.008, 255.0) } else { k.c.bg_base };
        let mut menu = col().key(format!("view-mode-menu-{p}")).w(210.0).padding(4.0).gap(2.0);
        for mode in ViewMode::ALL {
            if mode == ViewMode::List {
                menu = menu.child(el().w_full().h(1.0).my(3.0).fill(k.c.border_weak));
            }
            let selected = cur_mode == mode;
            let item = row()
                .key(format!("vm-opt-{p}-{mode:?}"))
                .role("menuitem")
                .label(mode.label())
                .checked(selected)
                .w_full()
                .h(30.0)
                .px(10.0)
                .gap(10.0)
                .center_y()
                .radius(k.radius.sm)
                .fill(if selected { k.alpha(k.c.text_accent, if k.dark { 0.20 } else { 0.10 }) } else { Color::TRANSPARENT })
                .hover(k.c.hover)
                .cursor(Cursor::Pointer)
                .on_press(move |s: &mut State| crate::set_view_mode(s, p, mode))
                .child(icon(mode.icon(), 16.0).color(if selected { k.c.text_accent } else { k.c.icon_muted }))
                .child(text(mode.label()).font(if selected { k.ty.text_small_medium } else { k.ty.text_small }).color(k.c.text_base).grow(1.0));
            menu = menu.child(item);
        }
        let surface = menu
            .radius(k.radius.md)
            .fill(dropdown_bg)
            .border(1.0, k.c.border_weak)
            .box_shadow(rune::Shadow { y: 6.0, blur: 18.0, color: k.dark_a(0.32), ..Default::default() });
        layer.push(rune_kit::popover::anchored(k, surface, &split_anchor, rune_kit::Placement::BottomEnd, 4.0));
    }

    let tree_on = s.show_tree[p];
    let tree_help = if tree_on {
        t("Ocultar árvore", "Hide tree")
    } else {
        t("Exibir árvore", "Show tree")
    };
    let tree_bg = if tree_on {
        k.alpha(k.c.text_accent, if k.dark { 0.22 } else { 0.14 })
    } else {
        Color::TRANSPARENT
    };
    let tree_hover = if tree_on {
        k.alpha(k.c.text_accent, if k.dark { 0.30 } else { 0.20 })
    } else {
        k.c.hover
    };
    let tree_btn = row()
        .key(format!("toggle-tree-{p}"))
        .role("button")
        .label(tree_help.to_string())
        .help(tree_help.to_string())
        .checked(tree_on)
        .size(28.0, 24.0)
        .center()
        .radius(k.radius.sm)
        .fill(tree_bg)
        .hover(tree_hover)
        .press(k.c.pressed)
        .cursor(Cursor::Pointer)
        .on_press(move |s: &mut State| {
            s.active = p;
            s.show_tree[p] = !s.show_tree[p];
        })
        .child(icon(&icons::TOGGLE_TREE, 15.0).color(if tree_on { k.c.text_base } else { k.c.icon_muted }));

    let can_new = actions::editable(&s.panes[p].loc);
    let new_help = t("Nova pasta (Ctrl+Shift+N)", "New folder (Ctrl+Shift+N)");
    let mut new_btn = row()
        .key(format!("pane-new-folder-{p}"))
        .role("button")
        .label(new_help.to_string())
        .help(new_help.to_string())
        .size(28.0, 24.0)
        .center()
        .radius(k.radius.sm)
        .child(icon(&icons::M_NEW_FOLDER, 15.0).color(if can_new { k.c.icon_base } else { k.c.icon_faint }));
    if can_new {
        new_btn = new_btn
            .hover(k.c.hover)
            .press(k.c.pressed)
            .cursor(Cursor::Pointer)
            .on_press_tx(move |s: &mut State, tx: &mut Tx| {
                s.active = p;
                actions::ask_new_folder(s, p, tx);
            });
    }

    row()
        .key(format!("pane-actions-{p}"))
        .w_full()
        .h(30.0)
        .px(8.0)
        .gap(6.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(tree_btn)
        .child(new_btn)
        .child(el().grow(1.0))
        .child(view_split)
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
            .label(t("Endereço", "Address"))
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
                    // ↑ and ↓: addresses reached before.
                    return crate::history::address_key(s, p, ev);
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
                        .font(k.ty.text_small)
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

fn navbar(s: &State, k: &Kit, p: usize, layer: &mut Vec<El>) -> El {
    let pane = &s.panes[p];
    row()
        .key("nav")
        .w_full()
        .h(36.0)
        .px(6.0)
        .gap(2.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(nav_button(k, "back", &icons::BACK, t("Voltar (Alt+←)", "Back (Alt+←)"), !pane.back.is_empty(), move |s: &mut State| go_back(s, p)))
        .child(nav_button(k, "forward", &icons::FORWARD, t("Avançar (Alt+→)", "Forward (Alt+→)"), !pane.forward.is_empty(), move |s: &mut State| go_forward(s, p)))
        .child(nav_button(k, "up", &icons::UP, t("Acima (Backspace)", "Up (Backspace)"), can_go_up(s, &pane.loc), move |s: &mut State| go_up(s, p)))
        .child(path_bar(s, k, p).mx(4.0))
        .child(search_box(s, k, p, layer))
        .child(nav_button(k, "reload", &icons::REFRESH, t("Atualizar (Ctrl+R)", "Refresh (Ctrl+R)"), true, move |s: &mut State| refresh(s, p)))
}

fn scope_menu_item(
    k: &Kit,
    key: String,
    glyph: &'static Icon,
    label: String,
    sub: Option<String>,
    selected: bool,
    on_pick: impl Fn(&mut State, &mut Tx) + 'static,
) -> El {
    let mut text_col = col().grow(1.0).min_w(0.0).child(
        text(label.clone())
            .font(if selected { k.ty.text_small_medium } else { k.ty.text_small })
            .color(k.c.text_base)
            .truncate()
            .shrink()
            .min_w(0.0),
    );
    if let Some(sub) = sub {
        text_col = text_col.child(text(sub).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().min_w(0.0));
    }
    row()
        .key(key)
        .role("menuitem")
        .label(label)
        .checked(selected)
        .w_full()
        .px(10.0)
        .py(6.0)
        .gap(8.0)
        .center_y()
        .radius(k.radius.sm)
        .fill(if selected { k.alpha(k.c.text_accent, if k.dark { 0.20 } else { 0.10 }) } else { Color::TRANSPARENT })
        .hover(k.c.hover)
        .cursor(Cursor::Pointer)
        .on_press_tx(on_pick)
        .child(icon(glyph, 15.0).color(if selected { k.c.text_accent } else { k.c.icon_muted }))
        .child(text_col)
}

/// The search field of a pane: Enter searches where the pane is (or the chosen Google Drive
/// scope), Esc empties it; Ctrl+F reaches it from the list.
fn search_box(s: &State, k: &Kit, p: usize, layer: &mut Vec<El>) -> El {
    use crate::model::DriveSearchScope;
    use openexplorer_gdrive::search::Within;
    let loc = &s.panes[p].loc;
    let is_drive = matches!(loc, Location::Drive { .. } | Location::DriveSearch(_));
    let (here_scope, place) = match loc {
        Location::Local(dir) => (None, crate::finder::place_of(dir)),
        Location::Found(f) => (None, f.place.clone()),
        Location::Computer => (None, t("todo o computador", "This PC").to_string()),
        Location::Server { server, path, .. } => (None, path.last().unwrap_or(&server.name).clone()),
        Location::DriveSearch(_) => (None, t("todas as contas", "all accounts").to_string()),
        _ => match actions::search_scope(loc) {
            Some((within, place)) => (Some(within), place),
            None => return el().key("search"),
        },
    };
    let effective_scope = match (loc, s.search_scope[p]) {
        (Location::DriveSearch(_), _) => DriveSearchScope::AllAccounts,
        (_, DriveSearchScope::AllAccounts) if s.accounts.len() >= 2 => DriveSearchScope::AllAccounts,
        (_, DriveSearchScope::AllAccounts) => DriveSearchScope::Account,
        (_, other) => other,
    };
    let placeholder = if !is_drive {
        tr!("Pesquisar em {place}", "Search {place}")
    } else {
        match effective_scope {
            DriveSearchScope::AllAccounts => t("Pesquisar em todas as contas", "Search all accounts").to_string(),
            DriveSearchScope::Account => t("Pesquisar em toda a conta", "Search entire account").to_string(),
            DriveSearchScope::Here => tr!("Pesquisar em {place}", "Search {place}"),
        }
    };
    let field = rune::text_input(&s.search[p], k.ty.text_small)
        .single_line()
        .key(search_key(p))
        .label(t("Pesquisar", "Search"))
        .placeholder(placeholder, k.c.text_faint)
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
                s.history.search[p].reset();
                true
            } else {
                // ↑ and ↓: words searched for before.
                crate::history::search_key(s, p, ev)
            }
        });
    if !is_drive {
        return row()
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
            .child(field);
    }

    let (scope_icon, scope_label, scope_help): (&'static Icon, &str, String) = match effective_scope {
        DriveSearchScope::AllAccounts => (
            &icons::TAB_ACCOUNTS,
            t("Todas", "All"),
            tr!("Pesquisar em todas as {} contas do Google Drive", "Search all {} Google Drive accounts", s.accounts.len()),
        ),
        DriveSearchScope::Account => (
            &icons::PROVIDER_GDRIVE,
            t("Geral", "Account"),
            t("Pesquisar em toda a conta (Meu Drive e drives compartilhados)", "Search entire account (My Drive & shared drives)").to_string(),
        ),
        DriveSearchScope::Here => match &here_scope {
            Some(Within::Everything) | None => (
                &icons::PROVIDER_GDRIVE,
                t("Geral", "Account"),
                t("Pesquisar em toda a conta (Meu Drive e drives compartilhados)", "Search entire account (My Drive & shared drives)").to_string(),
            ),
            Some(Within::SharedDrive(_)) => (
                &icons::SHARED,
                t("Drive", "Drive"),
                tr!("Pesquisar somente no drive “{place}”", "Search only in shared drive “{place}”"),
            ),
            Some(Within::Folder { .. }) => (
                &icons::FOLDER,
                t("Pasta", "Folder"),
                tr!("Pesquisar somente na pasta “{place}”", "Search only in folder “{place}”"),
            ),
            Some(Within::MyDrive) => (
                &icons::FOLDER,
                t("Meu Drive", "My Drive"),
                t("Pesquisar somente em Meu Drive", "Search only in My Drive").to_string(),
            ),
        },
    };
    let scope_open = s.search_scope_menu == Some(p);
    let scope_anchor = format!("search-scope-{p}");
    let scope_bg = if scope_open {
        k.c.hover
    } else if k.dark {
        k.alpha(k.c.white, 0.04)
    } else {
        k.alpha(k.c.black, 0.03)
    };
    let scope_btn = row()
        .key(scope_anchor.clone())
        .role("button")
        .label(scope_label)
        .help(scope_help)
        .h_full()
        .px(7.0)
        .gap(4.0)
        .center_y()
        .corners([k.radius.sm, 0.0, 0.0, k.radius.sm])
        .fill(scope_bg)
        .hover(k.c.hover)
        .press(k.c.pressed)
        .cursor(Cursor::Pointer)
        .on_press(move |s: &mut State| {
            let next = if s.search_scope_menu == Some(p) { None } else { Some(p) };
            s.active = p;
            s.view_menu_open = false;
            s.view_mode_menu = None;
            crate::tabs::close_picker_filter(s);
            s.menu = None;
            s.tree_menu = None;
            s.address_menu = None;
            s.search_scope_menu = next;
        })
        .child(icon(scope_icon, 13.0).color(k.c.text_base))
        .child(text(scope_label).font(k.ty.text_xsmall).color(k.c.text_base))
        .child(icon(&icons::CHEVRON_DOWN, 10.0).color(k.c.icon_muted));

    if scope_open {
        layer.push(rune_kit::scrim("search-scope-scrim", |s: &mut State| s.search_scope_menu = None));
        let dropdown_bg = if k.dark { rune::oklch(0.22, 0.008, 255.0) } else { k.c.bg_base };
        let mut menu = col().key(format!("search-scope-menu-{p}")).w(260.0).padding(4.0).gap(2.0);
        match loc {
            Location::Drive { email, .. } => {
                let has_narrow = here_scope.as_ref().is_some_and(|w| *w != Within::Everything);
                if has_narrow {
                    let here_icon = match &here_scope {
                        Some(Within::SharedDrive(_)) => &icons::SHARED,
                        _ => &icons::FOLDER,
                    };
                    menu = menu.child(scope_menu_item(
                        k,
                        format!("scope-here-{p}"),
                        here_icon,
                        tr!("Só em “{place}”", "Only in “{place}”"),
                        None,
                        effective_scope == DriveSearchScope::Here,
                        move |s: &mut State, tx: &mut Tx| actions::set_search_scope(s, p, DriveSearchScope::Here, tx),
                    ));
                }
                let account_selected = effective_scope == DriveSearchScope::Account || (!has_narrow && effective_scope == DriveSearchScope::Here);
                menu = menu.child(scope_menu_item(
                    k,
                    format!("scope-account-{p}"),
                    &icons::PROVIDER_GDRIVE,
                    t("Geral (toda a conta)", "All of this account").to_string(),
                    Some(email.clone()),
                    account_selected,
                    move |s: &mut State, tx: &mut Tx| actions::set_search_scope(s, p, DriveSearchScope::Account, tx),
                ));
                if s.accounts.len() >= 2 {
                    menu = menu.child(el().w_full().h(1.0).my(3.0).fill(k.c.border_weak));
                    menu = menu.child(scope_menu_item(
                        k,
                        format!("scope-all-{p}"),
                        &icons::TAB_ACCOUNTS,
                        tr!("Todas as contas ({})", "All accounts ({})", s.accounts.len()),
                        Some(t("Meu Drive e drives compartilhados de todas", "My Drive and shared drives of all accounts").to_string()),
                        effective_scope == DriveSearchScope::AllAccounts,
                        move |s: &mut State, tx: &mut Tx| actions::set_search_scope(s, p, DriveSearchScope::AllAccounts, tx),
                    ));
                }
            }
            Location::DriveSearch(_) => {
                menu = menu.child(scope_menu_item(
                    k,
                    format!("scope-all-{p}"),
                    &icons::TAB_ACCOUNTS,
                    tr!("Todas as contas ({})", "All accounts ({})", s.accounts.len()),
                    None,
                    true,
                    |s: &mut State, _| s.search_scope_menu = None,
                ));
                if !s.accounts.is_empty() {
                    menu = menu.child(el().w_full().h(1.0).my(3.0).fill(k.c.border_weak));
                    for (idx, acc) in s.accounts.iter().enumerate() {
                        let email = acc.email.clone();
                        let label = email.clone();
                        menu = menu.child(scope_menu_item(
                            k,
                            format!("scope-acc-{p}-{idx}"),
                            &icons::PROVIDER_GDRIVE,
                            label,
                            Some(t("Geral (toda a conta)", "All of this account").to_string()),
                            false,
                            move |s: &mut State, tx: &mut Tx| actions::search_in_account(s, p, email.clone(), tx),
                        ));
                    }
                }
            }
            _ => {}
        }
        let surface = menu
            .radius(k.radius.md)
            .fill(dropdown_bg)
            .border(1.0, k.c.border_weak)
            .box_shadow(rune::Shadow { y: 6.0, blur: 18.0, color: k.dark_a(0.32), ..Default::default() });
        layer.push(rune_kit::popover::anchored(k, surface, &scope_anchor, rune_kit::Placement::BottomStart, 4.0));
    }

    row()
        .key("search")
        .w(290.0)
        .h(26.0)
        .mr(4.0)
        .center_y()
        .radius(k.radius.sm)
        .border(1.0, k.c.border_weak)
        .child(scope_btn)
        .child(el().w(1.0).h(14.0).fill(k.c.border_weak))
        .child(
            row()
                .grow(1.0)
                .min_w(0.0)
                .h_full()
                .px(7.0)
                .gap(5.0)
                .center_y()
                .child(icon(&icons::SEARCH, 14.0).color(k.c.icon_faint))
                .child(field),
        )
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
    let mut said = tr!("“{}” em {}", "“{}” in {}", f.text, f.place);
    let n = pane.entries.len();
    match info {
        // In WSL or on the network (no index): results come in as folders are read.
        None if pane.loading && n > 0 => said.push_str(&tr!(" · Pesquisando… {} até agora", " · Searching… {} so far", results(n))),
        None if !pane.loading && pane.error.is_none() => said.push_str(&format!(" · {}", results(n))),
        _ if pane.loading => said.push_str(t(" · Pesquisando…", " · Searching…")),
        Some(i) if i.service != Service::Running => said.push_str(t(" · sem o serviço", " · without the service")),
        Some(i) if i.total > i.shown => said.push_str(&tr!(" · mostrando {} de {} resultados", " · showing {} of {} results", ui_count(i.shown), ui_count(i.total))),
        Some(i) => said.push_str(&format!(" · {}", results(i.total))),
        None => {}
    }
    if let Some(i) = info
        && !i.indexing.is_empty()
    {
        said.push_str(&tr!(" · lendo {} (resultados incompletos)", " · reading {} (results incomplete)", i.indexing.join(", ")));
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
        let label = if s.installing_finder { t("Instalando…", "Installing…") } else { t("Instalar o serviço de pesquisa", "Install the search service") };
        bar = bar.child(link("install-finder", label.into(), |s, _| actions::install_finder(s)));
    }
    if f.within.is_some() {
        bar = bar.child(link("search-everywhere", t("Pesquisar no computador todo", "Search the whole computer").into(), actions::search_everywhere));
    }
    bar
}

/// Under the path bar of a search's results: what was searched where, and a way to widen it.
fn search_bar(s: &State, k: &Kit, p: usize) -> El {
    if let Location::Found(f) = &s.panes[p].loc {
        return found_bar(s, k, p, f);
    }
    if let Location::Server { server, path, find: Some(words) } = &s.panes[p].loc {
        let text_ = words;
        let pane = &s.panes[p];
        let n = pane.entries.len();
        let results = results(n);
        let place = path.last().unwrap_or(&server.name);
        let said = match (pane.loading, n) {
            (true, 0) => tr!("“{text_}” em {place} · Pesquisando…", "“{text_}” in {place} · Searching…"),
            (true, _) => tr!("“{text_}” em {place} · Pesquisando… {results} até agora", "“{text_}” in {place} · Searching… {results} so far"),
            _ => tr!("“{text_}” em {place} e nas pastas dentro · {results}", "“{text_}” in {place} and the folders inside · {results}"),
        };
        return row()
            .key("search-bar")
            .w_full()
            .h(28.0)
            .px(10.0)
            .center_y()
            .fill(k.c.bg_base)
            .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
            .child(text(said).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().min_w(0.0));
    }
    if let Location::DriveSearch(q) = &s.panes[p].loc {
        let pane = &s.panes[p];
        let n = pane.entries.len();
        let found = match (pane.loading, n) {
            (true, 0) => t("Pesquisando…", "Searching…").to_string(),
            (true, _) => tr!("Pesquisando… {} até agora", "Searching… {} so far", results(n)),
            _ => results(n),
        };
        let mut said = tr!("“{}” em todas as contas ({}) · {found}", "“{}” in all accounts ({}) · {found}", q.text, s.accounts.len());
        let mut bar = row()
            .key("search-bar")
            .w_full()
            .h(28.0)
            .px(10.0)
            .gap(10.0)
            .center_y()
            .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
            .child(text(said.clone()).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().min_w(0.0));
        if !s.drive_search_warnings[p].is_empty() {
            let failed_emails = s.drive_search_warnings[p].iter().map(|(e, _)| e.as_str()).collect::<Vec<_>>().join(", ");
            let details = s.drive_search_warnings[p].iter().map(|(e, err)| format!("{e}: {err}")).collect::<Vec<_>>().join("\n");
            said = tr!("falha em {failed_emails}", "failed on {failed_emails}");
            bar = bar
                .child(el().grow(1.0))
                .child(text(said).font(k.ty.text_xsmall).color(k.c.danger.fg).help(details).truncate().shrink().min_w(0.0));
        }
        return bar;
    }
    let Location::Drive { space: Space::Search(q), path, .. } = &s.panes[p].loc else { return el().key("search-bar") };
    if !path.is_empty() {
        return el().key("search-bar");
    }
    let pane = &s.panes[p];
    let n = pane.entries.len();
    let found = match (pane.loading, n) {
        (true, 0) => t("Pesquisando…", "Searching…").to_string(),
        (true, _) => tr!("Pesquisando… {} até agora", "Searching… {} so far", results(n)),
        _ => results(n),
    };
    let mut bar = row()
        .key("search-bar")
        .w_full()
        .h(28.0)
        .px(10.0)
        .gap(10.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(text(tr!("“{}” em {} · {found}", "“{}” in {} · {found}", q.text, q.place)).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().min_w(0.0))
        .child(el().grow(1.0));
    if q.within != openexplorer_gdrive::search::Within::Everything {
        bar = bar.child(
            text(t("Pesquisar em toda a conta", "Search all of this account"))
                .key("search-everywhere")
                .role("link")
                .font(k.ty.text_xsmall)
                .color(k.c.text_accent)
                .cursor(Cursor::Pointer)
                .on_press(move |s: &mut State| actions::search_everywhere(s, p)),
        );
    }
    if s.accounts.len() >= 2 {
        bar = bar.child(
            text(t("Pesquisar em todas as contas", "Search all accounts"))
                .key("search-all-accounts")
                .role("link")
                .font(k.ty.text_xsmall)
                .color(k.c.text_accent)
                .cursor(Cursor::Pointer)
                .on_press(move |s: &mut State| actions::search_all_accounts(s, p)),
        );
    }
    bar
}

fn can_go_up(s: &State, loc: &Location) -> bool {
    match loc {
        // A disk's top goes up to Este Computador.
        Location::Local(dir) => crate::network::parent(dir).is_some() || crate::mounts::is_disk_root(dir, &s.local_drives),
        Location::Computer | Location::Found(_) | Location::DriveSearch(_) => false,
        Location::Mock(segs) => !segs.is_empty(),
        Location::Drive { space, path, .. } => !path.is_empty() || matches!(space, Space::Shared { .. }),
        Location::Server { path, find, .. } => !path.is_empty() || find.is_some(),
    }
}

// ---------------------------------------------------------------------------------------------
// Location tree

/// A row of a pane's tree: a heading, a note under a node being read, or a node with its arrow
/// (which opens and closes it), icon and name (a click opens the folder; a double click opens
/// or closes the node, as in Explorer). The row the keyboard is on shows a ring while the tree
/// has focus.
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
        let chevron = if s.settings.selection_animated() {
            icon(&icons::CHEVRON, 12.0)
                .color(k.c.icon_faint)
                .origin(0.5, 0.5)
                .rotate(if open { std::f32::consts::FRAC_PI_2 } else { 0.0 })
                .rotate_motion(crate::anim::ui_spring())
        } else {
            icon(if open { &icons::CHEVRON_DOWN } else { &icons::CHEVRON }, 12.0).color(k.c.icon_faint)
        };
        row()
            .key("arrow")
            .size(16.0, 16.0)
            .center()
            .radius(k.radius.sm)
            .hover(k.alpha(k.c.text_base, 0.08))
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| crate::tree::toggle(s, p, &n))
            .child(chevron)
    } else {
        el().size(16.0, 16.0)
    };
    let (go, flip, here) = (node.clone(), node.clone(), line.key());
    let (ctx_node, ctx_here) = (node.clone(), here.clone());
    let keyboard = s.tree.focused[p] && crate::tree::cursor(s, p) == Some(here.as_str());
    let mut item = row()
        .key(format!("t:{}", line.key()))
        .role("treeitem")
        .label(node.label.clone())
        .level(depth as u32 + 1)
        .checked(on);
    if node.opens && !empty {
        item = item.expanded(open);
    }
    if keyboard {
        item = item.border(1.5, k.c.border_focus).radius(k.radius.sm);
    }
    item
        .w_full()
        .h(ROW_H)
        .pl(indent(depth))
        .pr(6.0)
        .gap(4.0)
        .center_y()
        .fill(if on { k.alpha(k.c.text_accent, if k.dark { 0.24 } else { 0.16 }) } else { Color::TRANSPARENT })
        .hover(if on { k.alpha(k.c.text_accent, 0.24) } else { k.alpha(k.c.text_base, 0.05) })
        .cursor(Cursor::Pointer)
        .on_press(move |s: &mut State| {
            crate::tree::set_cursor(s, p, here.clone());
            match &go.to {
                Some(to) => navigate(s, p, to.clone()),
                None => crate::tree::toggle(s, p, &go),
            }
        })
        .on_double_press(move |s: &mut State| crate::tree::toggle(s, p, &flip))
        .on_secondary_press(move |s: &mut State, at: rune::PointerPress, _: &mut Tx| {
            s.active = p;
            crate::tree::set_cursor(s, p, ctx_here.clone());
            s.tree_menu = Some((p, (*ctx_node).clone(), open, empty, at));
        })
        .child(arrow)
        .child(row().gap(6.0).center_y().child(shell_icon(s, node.icon.as_ref(), node.fallback, 16.0)).child(
            text(node.label.clone()).font(k.ty.text_small).color(k.c.text_base),
        ))
}

fn heading(k: &Kit, label: &str) -> El {
    row().w_full().h(24.0).px(10.0).pt(6.0).center_y().child(text(label.to_string()).font(k.ty.text_xsmall).color(k.c.text_faint))
}

/// The scroll view of pane `p`'s tree, which `poll` scrolls to the folder shown.
pub(crate) fn tree_key(p: usize) -> String {
    format!("tree-scroll-{p}")
}

/// Estimates the horizontal width needed to show every row in `lines` without clipping, so the
/// tree's horizontal scroll bar appears whenever nested folders or long account names overflow.
fn tree_content_width(lines: &[crate::tree::Line]) -> f32 {
    let mut widest: f32 = 0.0;
    for line in lines {
        let w = match line {
            crate::tree::Line::Heading(h) => 20.0 + h.chars().count() as f32 * 7.2,
            crate::tree::Line::Note { depth, text, .. } => {
                4.0 + (*depth as f32) * 16.0 + 22.0 + text.chars().count() as f32 * 7.0 + 16.0
            }
            crate::tree::Line::Node { depth, node, .. } => {
                4.0 + (*depth as f32) * 16.0 + 16.0 + 4.0 + 16.0 + 6.0 + node.label.chars().count() as f32 * 8.0 + 20.0
            }
        };
        if w > widest {
            widest = w;
        }
    }
    widest
}

/// The tree beside pane `p` (see [`crate::tree`]): only the rows in view are built, so a node
/// with thousands of folders opens at once. It's one Tab stop that keeps the keyboard itself
/// (its rows come and go as it scrolls), naming the row the keyboard is on for screen readers.
fn tree(s: &State, k: &Kit, p: usize) -> El {
    let lines = std::rc::Rc::new(crate::tree::lines(s, p));
    let min_w = tree_content_width(&lines);
    let (keys, kit, n) = (lines.clone(), k.clone(), lines.len());
    let rows = col()
        .key("tree")
        .w_full()
        .min_w(min_w)
        .pb(6.0)
        .growth(GrowthSpec::default())
        .virtual_rows(n, ROW_H, move |_: &State, i| Cow::Owned(format!("t:{}", keys[i].key())), move |s: &State, i| tree_line(s, &kit, p, &lines[i]))
        .virtual_rows_version((s.tree.version, s.panes[p].moves, s.accounts.len(), s.servers.list.len()));
    let at = crate::tree::cursor(s, p).map(|c| format!("t:{c}")).unwrap_or_default();
    let tree_bg = k.alpha(k.c.text_base, if k.dark { 0.02 } else { 0.015 });
    // Opening or closing a node lands at once, as in Explorer, rather than sliding its rows.
    let key = tree_key(p);
    let viewport = col()
        .key(key.clone())
        .role("tree")
        .label(t("Pastas", "Folders"))
        .focusable(true)
        // The row the keyboard is on shows focus with its own ring, while the tree has focus
        // however it got it (Tab from the search field, a click). Switching windows keeps it.
        .no_focus_ring()
        .on_focus_change(move |s: &mut State, on, _: &mut Tx| crate::tree::focus(s, p, on))
        .active_descendant(at)
        .on_key(move |s: &mut State, ev: &KeyEvent, tx: &mut Tx| crate::tree::key(s, p, ev, tx))
        .instant_changes()
        .w_full()
        .h_full()
        .scroll_xy()
        .child(rows);
    let viewport = s.edges[p].tree.on(viewport);
    // The bars sit in gutters of their own, so scrolled folder names clip cleanly before them.
    col()
        .key(format!("tree-wrap-{p}"))
        .w(s.layout.tree[p])
        .h_full()
        .fill(tree_bg)
        .clip_rounded(0.0)
        .child(crate::scrollbars::xy_bars(k, &key, viewport, 0.0, s.waker.clone()).w_full().h_full())
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
        .label(t("Arraste para mudar a largura da coluna; clique duas vezes para voltar ao padrão", "Drag to change the column's width; double-click to restore the default"))
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

/// The app's overlay scroll bar style used inside modals and auxiliary windows.
pub(crate) fn bar_style(k: &Kit) -> rune_kit::ScrollBarStyle {
    rune_kit::ScrollBarStyle::from_kit(k).thumb(k.alpha(k.c.text_base, 0.3)).thumb_hover(k.alpha(k.c.text_base, 0.5)).width(8.0).hover_width(10.0)
}
/// The scroll view of pane `p`'s file list.
pub(crate) fn list_key(p: usize) -> String {
    format!("viewport-{p}")
}

/// The file list's column header, stuck to the top of its viewport.
pub(crate) const HEADER_H: f32 = crate::lasso::HEADER_H;

/// The words of the search whose results `loc` lists, folded as the search folds them; none
/// for a folder.
fn searched_words(loc: &Location) -> Vec<Vec<u8>> {
    let text = match loc {
        Location::Found(f) => &f.text,
        Location::Server { find: Some(text), .. } => text,
        Location::Drive { space: Space::Search(q), path, .. } if path.is_empty() => &q.text,
        Location::DriveSearch(q) => &q.text,
        _ => return Vec::new(),
    };
    crate::finder::words(text)
}

/// The rectangle a lasso in pane `p` draws, in the list's content (so it scrolls with the
/// rows), cut to what the viewport shows. Also draws the smooth fade-out on release.
fn lasso_rect(s: &State, k: &Kit, p: usize) -> Option<El> {
    let view = &s.views[p];
    let scroll = view.scroll.get();
    let (rect, alpha) = if let Some(l) = s.lasso.as_ref().filter(|l| l.pane == p && l.moved) {
        (l.rect(scroll), 1.0)
    } else {
        let fade = s.lasso_fade.filter(|f| f.pane == p)?;
        let a = fade.opacity(s.now);
        if a <= 0.01 {
            return None;
        }
        (fade.rect, a)
    };
    let [x0, y0, x1, y1] = rect;
    // Before the first layout says how big the viewport is, nothing is cut.
    let known = |size: f32, minus: f32| if size > 0.0 { (size - minus).max(0.0) } else { f32::MAX / 4.0 };
    let (left, right) = (scroll.x, scroll.x + known(view.rect[2], 0.0));
    let (top, bottom) = (scroll.y, scroll.y + known(scroll.h, HEADER_H));
    let (x0, x1) = (x0.clamp(left, right), x1.clamp(left, right));
    let (y0, y1) = (y0.clamp(top, bottom), y1.clamp(top, bottom));
    Some(
        el().key("lasso")
            .absolute(x0, HEADER_H + y0)
            .size(x1 - x0, y1 - y0)
            .radius(2.0)
            .fill(k.alpha(k.c.text_accent, 0.14 * alpha))
            .border(1.0, k.alpha(k.c.text_accent, 0.7 * alpha)),
    )
}

const GRID_GAP: f32 = 8.0;
const GRID_PAD: f32 = 8.0;
const LIST_COL_W: f32 = 230.0;
const LIST_COL_GAP: f32 = 8.0;
const LIST_HORIZ_MAX: usize = 2000;
/// List's columns built past each side of those in sight, so a scroll of a column or two needs
/// no view.
const LIST_SPARE_COLS: usize = 2;

fn grid_columns_for(viewport_w: f32, tile_w: f32) -> usize {
    let avail = (viewport_w - 2.0 * GRID_PAD + GRID_GAP).max(tile_w + GRID_GAP);
    (avail / (tile_w + GRID_GAP)).floor().max(1.0) as usize
}

fn list_rows_for(viewport_h: f32) -> usize {
    ((viewport_h - 8.0) / ROW_H).floor().max(1.0) as usize
}

fn wire_entry_interactions(el: El, p: usize, id: &str) -> El {
    let (id_press, id_open, id_drag, id_start, id_menu) = (id.to_string(), id.to_string(), id.to_string(), id.to_string(), id.to_string());
    el.on_press_tx(move |s: &mut State, tx: &mut Tx| {
        click(s, p, &id_press, tx.mods());
        tx.focus(pane_key(p), false);
    })
    .on_secondary_press(move |s: &mut State, at: rune::PointerPress, tx: &mut Tx| {
        s.active = p;
        let pane = &mut s.panes[p];
        if !pane.selected.contains(&id_menu) {
            pane.selected = [id_menu.clone()].into();
            pane.anchor = Some(id_menu.clone());
        }
        pane.cursor = Some(id_menu.clone());
        tx.focus(pane_key(p), false);
        s.menu = Some((p, at));
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
        let paths = paths_for(s, p, &dragged_ids(&s.panes[p], &id_start), false);
        s.last_drag = Some((p, paths));
    })
    .on_drag_end(move |s: &mut State, outcome: DragOutcome, _| {
        s.status = match outcome {
            DragOutcome::Cancelled => t("Arrastar cancelado.", "Drag canceled.").into(),
            o => tr!("Arrastar terminou: {o:?}", "Drag ended: {o:?}"),
        };
    })
}

fn list(s: &State, k: &Kit, p: usize) -> El {
    let pane = &s.panes[p];
    let mode = pane.view_mode;
    let w = s.layout.columns[p];
    let total: f32 = w.iter().sum();
    let order = s.layout.col_order[p];

    // Details only: the list zoom (Ctrl+wheel, pinch) and the animations of Opções > Animações.
    let row_h = s.zoom[p].row_height();
    let icon_sz = s.zoom[p].icon_size();
    let hover_glides = s.settings.hover_animated();
    let sel_anim = s.settings.selection_animated();
    let clip_anim = s.settings.clipboard_animated();
    let zebra = s.settings.zebra_stripes;

    let selected = k.alpha(k.c.text_accent, if k.dark { 0.26 } else { 0.18 });
    let hover = k.alpha(k.c.text_base, if k.dark { 0.065 } else { 0.05 });
    let zebra_bg = k.alpha(k.c.text_base, if k.dark { 0.028 } else { 0.024 });
    let ring = k.c.border_focus;
    let kbd_sel_active = sel_anim
        && mode == ViewMode::Details
        && s.kbd_sel[p].is_active()
        && pane.selected.len() == 1
        && pane.entries.get(s.kbd_sel[p].target_row()).is_some_and(|e| pane.selected.contains(&e.id));
    let (text_c, muted_c) = (k.c.text_base, k.c.text_muted);
    let mut font = k.ty.text_small;
    if mode == ViewMode::Details {
        font.size = s.zoom[p].font_size(font.size);
    }
    let tile_font = k.ty.text_xsmall;
    let rename_look = RenameLook { font, ink: text_c, fill: k.c.bg_base, edge: k.c.border_focus, selection: k.selection() };
    // A search's results show what matched each name in the accent colour: a colour-only span,
    // so the name keeps its shape and width. The words are folded once here; each row finds
    // its ranges as it's built, so only the rows on screen pay for them.
    let words = std::rc::Rc::new(searched_words(&pane.loc));
    let accent = k.c.text_accent;
    let marker = k.alpha(k.c.text_accent, 0.28);
    // The items Copiar or Recortar picked up from this folder, looked up once here rather than
    // by every row: a pick-up can hold thousands.
    let clip_ids: std::rc::Rc<std::collections::HashSet<String>> = std::rc::Rc::new(
        s.clip.as_ref().filter(|c| c.from == pane.loc).map(|c| c.items.iter().map(|it| it.id.clone()).collect()).unwrap_or_default(),
    );

    let n = pane.entries.len();
    let show_header = mode == ViewMode::Details;
    let header_h = if show_header { HEADER_H } else { 0.0 };

    // How the items are laid out, for the keyboard and its menu (`lasso::Flow`).
    let flow = &s.views[p].flow;
    let rows = match mode {
        ViewMode::Details => {
            // Rows as tall as the zoom makes them.
            flow.set(crate::lasso::Flow { step: [0.0, row_h], item: [0.0, row_h], ..Default::default() });
            let build = move |s: &State, i: usize| -> El {
                let pane = &s.panes[p];
                let e = &pane.entries[i];
                let is_sel = pane.selected.contains(&e.id);
                let found = pane.loc.finds();
                let clip_match = s.clip.as_ref().filter(|_| clip_ids.contains(&e.id));
                let is_cut = clip_match.is_some_and(|c| c.mode == ClipMode::Cut);
                // As wide as the columns: past them is empty space, where a drag draws a rectangle.
                let mut r = row().key(row_key(&e.id)).role("row").label(shown(&e.name)).focus_selected(is_sel).w(total).h(row_h).center_y();
                if sel_anim {
                    r = r.radius(4.0).animated_changes().motion(crate::anim::ui_spring());
                } else if clip_anim {
                    r = r.animated_changes();
                }
                let stripe = if zebra && i % 2 == 1 { zebra_bg } else { Color::TRANSPARENT };
                r = if is_sel && !kbd_sel_active {
                    r.fill(selected)
                } else if hover_glides {
                    r.fill(stripe)
                } else {
                    r.fill(stripe).hover(hover)
                };
                if clip_anim {
                    r = r.opacity(if is_cut { 0.55 } else { 1.0 }).opacity_motion(crate::anim::hover_curve());
                } else if is_cut {
                    r = r.opacity(0.55);
                }
                if s.row_reorder[p].is_active() {
                    let dy = s.row_reorder[p].offset_px(&e.id, row_h);
                    if dy.abs() > 0.25 {
                        r = r.offset(0.0, dy).offset_motion(Anim::INSTANT);
                    }
                }
                // The keyboard's row is outlined only when it isn't selected (Ctrl+arrows moved it
                // alone): a selected row already shows, and an outline on the end of a Shift range
                // reads as a second kind of selection.
                if s.active == p && !is_sel && pane.cursor.as_deref() == Some(e.id.as_str()) {
                    r = r.outline(1.0, -1.0, ring);
                }
                let cell = |i: usize| row().w(w[i]).h_full().px(8.0).center_y().clip_soft(0.0);
                let muted = |t: String| text(t).font(font).color(muted_c).truncate().shrink().min_w(0.0);
                let mut ico = entry_icon(s, &pane.loc, e, icon_sz);
                if clip_anim
                    && let Some(clip) = clip_match
                {
                    let elapsed = (s.now - clip.picked_at).max(0.0);
                    let angle = crate::anim::wiggle_radians(icon_sz, elapsed, &e.id);
                    ico = ico.origin(0.5, 0.5).rotate(angle).rotate_motion(Anim::INSTANT);
                }
                let mut cells = Vec::with_capacity(4);
                let mut ico_opt = Some(ico);
                for col_id in order {
                    let c = match col_id {
                        0 => cell(0)
                            .px(6.0)
                            .gap(6.0)
                            .child(ico_opt.take().unwrap_or_else(|| entry_icon(s, &pane.loc, e, icon_sz)))
                            .child(match &s.renaming {
                                Some((rp, item)) if *rp == p && item.id == e.id => rename_field(s, rename_look),
                                _ => {
                                    let name = shown(&e.name);
                                    let ranges = crate::finder::match_ranges(&words, &name);
                                    if s.settings.search_marker {
                                        // Experimental: as a highlighter marks them, painted under the
                                        // glyphs without reshaping the name.
                                        let marks = ranges.into_iter().map(|r| rune::TextHighlight::new(r, marker));
                                        text(name).font(font).color(text_c).text_highlights(marks).truncate().shrink().min_w(0.0)
                                    } else {
                                        let spans: Vec<Span> = ranges.into_iter().map(|r| Span::new(r.start, r.end).color(accent)).collect();
                                        text(name).font(font).color(text_c).spans(spans).truncate().shrink().min_w(0.0)
                                    }
                                }
                            }),
                        1 => cell(1).justify_end().child(muted(e.size.map(format_size).unwrap_or_default()).align(Align::End)),
                        2 => cell(2).child(muted(e.modified.map(format_time).unwrap_or_default())),
                        _ => cell(3).child(muted(if found {
                            crate::finder::folder_of(e)
                        } else if let Some(origin) = &e.drive_origin {
                            origin.label.clone()
                        } else {
                            crate::onedrive::origin_of(&pane.loc, e).unwrap_or_else(|| e.kind_label())
                        })),
                    };
                    cells.push(c);
                }
                wire_entry_interactions(r.children(cells), p, &e.id)
            };
            col()
                .key("rows")
                .w_full()
                .pb(2.0 * row_h)
                .growth(GrowthSpec::default())
                .virtual_rows(n, row_h, move |s: &State, i| Cow::Owned(row_key(&s.panes[p].entries[i].id)), build)
                .virtual_rows_version((pane.version, mode, w.map(f32::to_bits), order, row_h.to_bits(), s.renaming.as_ref().map(|(rp, e)| (*rp, e.id.clone()))))
        }
        ViewMode::List => {
            let radius = k.radius.sm;
            let make_item = move |s: &State, i: usize| -> El {
                let pane = &s.panes[p];
                let e = &pane.entries[i];
                let is_sel = pane.selected.contains(&e.id);
                let mut r = row()
                    .key(row_key(&e.id))
                    .role("row")
                    .label(shown(&e.name))
                    .focus_selected(is_sel)
                    .w(LIST_COL_W)
                    .h(ROW_H)
                    .px(6.0)
                    .gap(6.0)
                    .center_y()
                    .radius(radius)
                    .clip_soft(radius);
                r = if is_sel { r.fill(selected) } else { r.hover(hover) };
                if s.active == p && !is_sel && pane.cursor.as_deref() == Some(e.id.as_str()) {
                    r = r.outline(1.0, -1.0, ring);
                }
                let r = r.child(entry_icon(s, &pane.loc, e, 16.0)).child(match &s.renaming {
                    Some((rp, item)) if *rp == p && item.id == e.id => rename_field(s, rename_look),
                    _ => {
                        let name = shown(&e.name);
                        let ranges = crate::finder::match_ranges(&words, &name);
                        if s.settings.search_marker {
                            let marks = ranges.into_iter().map(|r| rune::TextHighlight::new(r, marker));
                            text(name).font(font).color(text_c).text_highlights(marks).truncate().shrink().min_w(0.0)
                        } else {
                            let spans: Vec<Span> = ranges.into_iter().map(|r| Span::new(r.start, r.end).color(accent)).collect();
                            text(name).font(font).color(text_c).spans(spans).truncate().shrink().min_w(0.0)
                        }
                    }
                });
                wire_entry_interactions(r, p, &e.id)
            };
            if n <= LIST_HORIZ_MAX {
                // Windows Explorer List view: flows top-to-bottom per column and left-to-right
                // across columns, scrolling horizontally when there are more columns than fit.
                // The height left once the bottom scroll bar comes or goes in this view.
                let vp_h = s.views[p].rect[3];
                let vp_h = if vp_h > 0.0 { (vp_h + crate::scrollbars::height_change(&list_key(p))).max(1.0) } else { vp_h };
                let rows_per_col = if vp_h > 0.0 { list_rows_for(vp_h) } else { s.list_rows[p].max(1) };
                let num_cols = n.div_ceil(rows_per_col);
                flow.set(crate::lasso::Flow {
                    per_line: rows_per_col,
                    rows: false,
                    origin: [4.0, 4.0],
                    step: [LIST_COL_W + LIST_COL_GAP, ROW_H],
                    item: [LIST_COL_W, ROW_H],
                    header: 0.0,
                    line_key: None,
                });
                // Only the columns in sight, and two on each side, are built: the rest are room.
                // Scrolling past them asks for the view again (`on_scroll_offset`).
                let step = LIST_COL_W + LIST_COL_GAP;
                let scroll = s.views[p].scroll.get();
                let built = crate::lasso::column_window(scroll.x, s.views[p].rect[2], 4.0, step, LIST_SPARE_COLS, num_cols);
                s.views[p].columns.set(built);
                let width = 8.0 + num_cols as f32 * step - LIST_COL_GAP;
                let height = 8.0 + rows_per_col as f32 * ROW_H;
                let mut cols_row = el().key("rows-list").w(width).h(height);
                for c in built.0..built.1 {
                    let start = c * rows_per_col;
                    let end = (start + rows_per_col).min(n);
                    let mut column = col().key(format!("lcol-{c}")).absolute(4.0 + c as f32 * step, 4.0).w(LIST_COL_W);
                    for i in start..end {
                        column = column.child(make_item(s, i));
                    }
                    cols_row = cols_row.child(column);
                }
                cols_row
            } else {
                // Very large folders stay virtualized in multi-column rows.
                let vp_w = s.views[p].rect[2];
                let cols = if vp_w > 0.0 { grid_columns_for(vp_w, LIST_COL_W) } else { s.grid_cols[p].max(1) };
                let vrows = n.div_ceil(cols);
                flow.set(crate::lasso::Flow {
                    per_line: cols,
                    rows: true,
                    origin: [4.0, 4.0],
                    step: [LIST_COL_W + LIST_COL_GAP, ROW_H],
                    item: [LIST_COL_W, ROW_H],
                    header: 0.0,
                    line_key: Some("lr-"),
                });
                let build = move |s: &State, row_idx: usize| -> El {
                    let start = row_idx * cols;
                    let end = (start + cols).min(s.panes[p].entries.len());
                    let mut line = row().key(format!("lr-{row_idx}")).w_full().h(ROW_H).px(4.0).gap(LIST_COL_GAP).center_y();
                    for i in start..end {
                        line = line.child(make_item(s, i));
                    }
                    line
                };
                col()
                    .key("rows-list-virt")
                    .w_full()
                    .pt(4.0)
                        .pb(2.0 * ROW_H)
                    .growth(GrowthSpec::default())
                    .virtual_rows(vrows, ROW_H, move |_, row_idx| Cow::Owned(format!("lr-{row_idx}")), build)
                    .virtual_rows_version((pane.version, mode, cols, s.renaming.as_ref().map(|(rp, e)| (*rp, e.id.clone()))))
            }
        }
        ViewMode::ExtraLargeIcons | ViewMode::LargeIcons | ViewMode::MediumIcons => {
            let (tile_w, tile_h, icon_size) = mode.tile_metrics();
            let vp_w = s.views[p].rect[2];
            let cols = if vp_w > 0.0 { grid_columns_for(vp_w, tile_w) } else { s.grid_cols[p].max(1) };
            let grid_rows = n.div_ceil(cols);
            flow.set(crate::lasso::Flow {
                per_line: cols,
                rows: true,
                origin: [GRID_PAD, GRID_PAD],
                step: [tile_w + GRID_GAP, tile_h + GRID_GAP],
                item: [tile_w, tile_h],
                header: 0.0,
                line_key: Some("gr-"),
            });
            let row_h = tile_h + GRID_GAP;
            let radius = k.radius.sm;
            let build = move |s: &State, row_idx: usize| -> El {
                let pane = &s.panes[p];
                let start = row_idx * cols;
                let end = (start + cols).min(pane.entries.len());
                let mut line = row().key(format!("gr-{row_idx}")).w_full().h(row_h).px(GRID_PAD).gap(GRID_GAP).items_start();
                for i in start..end {
                    let e = &pane.entries[i];
                    let is_sel = pane.selected.contains(&e.id);
                    let mut tile = col()
                        .key(row_key(&e.id))
                        .role("row")
                        .label(shown(&e.name))
                        .focus_selected(is_sel)
                        .w(tile_w)
                        .h(tile_h)
                        .padding(6.0)
                        .gap(5.0)
                        .items_center()
                        .radius(radius)
                        .clip_soft(radius);
                    tile = if is_sel { tile.fill(selected) } else { tile.hover(hover) };
                    if s.active == p && !is_sel && pane.cursor.as_deref() == Some(e.id.as_str()) {
                        tile = tile.outline(1.0, -1.0, ring);
                    }
                    let label_el = match &s.renaming {
                        Some((rp, item)) if *rp == p && item.id == e.id => rename_field(s, rename_look),
                        _ => {
                            let name = shown(&e.name);
                            let ranges = crate::finder::match_ranges(&words, &name);
                            let t_el = if s.settings.search_marker {
                                let marks = ranges.into_iter().map(|r| rune::TextHighlight::new(r, marker));
                                text(name).font(tile_font).color(text_c).text_highlights(marks)
                            } else {
                                let spans: Vec<Span> = ranges.into_iter().map(|r| Span::new(r.start, r.end).color(accent)).collect();
                                text(name).font(tile_font).color(text_c).spans(spans)
                            };
                            t_el.wrap().align(Align::Center).w_full().min_w(0.0)
                        }
                    };
                    let tile = tile
                        .child(entry_icon(s, &pane.loc, e, icon_size))
                        .child(col().w_full().grow(1.0).min_h(0.0).clip_soft(0.0).child(label_el));
                    line = line.child(wire_entry_interactions(tile, p, &e.id));
                }
                line
            };
            col()
                .key("rows-grid")
                .w_full()
                .pt(GRID_PAD)
                .pb(2.0 * GRID_PAD)
                .growth(GrowthSpec::default())
                .virtual_rows(grid_rows, row_h, move |_, row_idx| Cow::Owned(format!("gr-{row_idx}")), build)
                .virtual_rows_version((pane.version, mode, cols, s.renaming.as_ref().map(|(rp, e)| (*rp, e.id.clone()))))
        }
    };

    let notice = |msg: String, color| col().key("notice").w_full().grow(1.0).padding(16.0).child(text(msg).font(k.ty.text_small).color(color).wrap());
    let body = if let Some(consent) = crate::onedrive::bin_notice(s, k, p) {
        consent
    } else if let Some(err) = &pane.error {
        notice(err.clone(), k.c.danger.fg)
    } else if pane.loading && n == 0 {
        let searching = actions::shows_results(&pane.loc);
        notice(if searching { t("Pesquisando…", "Searching…") } else { t("Carregando…", "Loading…") }.into(), k.c.text_muted)
    } else if let (0, Location::Drive { space: Space::Search(q), path, .. }) = (n, &pane.loc)
        && path.is_empty()
    {
        notice(tr!("Nada com “{}” no nome em {}.", "Nothing with “{}” in its name in {}.", q.text, q.place), k.c.text_faint)
    } else if let (0, Location::DriveSearch(q)) = (n, &pane.loc) {
        notice(tr!("Nada com “{}” no nome em nenhuma conta.", "Nothing with “{}” in its name in any account.", q.text), k.c.text_faint)
    } else if let (0, Location::Found(f)) = (n, &pane.loc) {
        notice(tr!("Nada com “{}” no nome em {}.", "Nothing with “{}” in its name in {}.", f.text, f.place), k.c.text_faint)
    } else if let (0, Location::Server { server, path, find: Some(text) }) = (n, &pane.loc) {
        notice(
            tr!(
                "Nada com “{text}” no nome em {} nem nas pastas dentro.",
                "Nothing with “{text}” in its name in {} or the folders inside.",
                path.last().unwrap_or(&server.name)
            ),
            k.c.text_faint,
        )
    } else if n == 0 && matches!(pane.loc, Location::Drive { space: Space::SharedDrives, .. }) {
        notice(t("Esta conta não participa de nenhum drive compartilhado.", "This account isn't a member of any shared drive.").into(), k.c.text_muted)
    } else if let (0, Location::Drive { space, path, .. }) = (n, &pane.loc)
        && path.is_empty()
        && space.view().is_some()
    {
        let msg = match space {
            Space::SharedWithMe => t("Ninguém compartilhou arquivos com esta conta.", "No one has shared files with this account."),
            Space::Starred => t("Nenhum item com estrela. Use Adicionar estrela no menu de um arquivo.", "No starred items. Use Add star in a file's menu."),
            Space::Recent => t("Nenhum arquivo aberto recentemente.", "No recently opened files."),
            _ => t("A lixeira está vazia.", "The trash is empty."),
        };
        notice(msg.into(), k.c.text_faint)
    } else if n == 0 {
        notice(t("Esta pasta está vazia.", "This folder is empty.").into(), k.c.text_faint)
    } else {
        rows
    };

    let table = if show_header {
        let col_meta: [(&'static str, &str, SortCol, bool); 4] = [
            ("h-name", t("Nome", "Name"), SortCol::Name, false),
            ("h-size", t("Tamanho", "Size"), SortCol::Size, true),
            ("h-mod", crate::onedrive::bin_columns(&pane.loc).map_or(t("Data", "Date modified"), |c| c.0), SortCol::Modified, false),
            (
                "h-kind",
                if let Some(c) = crate::onedrive::bin_columns(&pane.loc) {
                    c.1
                } else if pane.loc.finds() {
                    t("Pasta", "Folder")
                } else if pane.loc.is_drive_search() {
                    t("Local", "Location")
                } else {
                    t("Tipo", "Type")
                },
                SortCol::Kind,
                false,
            ),
        ];

        let active_drag = s.col_drag.filter(|d| d.pane == p);
        let mut header_cells = Vec::with_capacity(4);
        let mut slot_x = 0.0_f32;
        for (slot, &col_id) in order.iter().enumerate() {
            let (key, label, c, end) = col_meta[col_id];
            let is_dragged = active_drag.is_some_and(|d| d.col_id == col_id && d.moved);
            let drag_dx = active_drag.filter(|d| d.col_id == col_id && d.moved).map_or(0.0, |d| d.visual_dx);
            let start_x = slot_x;
            slot_x += w[col_id];

            let mut h = row()
                .key(key.to_string())
                .w(w[col_id])
                .h_full()
                .px(8.0)
                .gap(4.0)
                .center_y()
                .hover(k.c.hover)
                .cursor(if is_dragged { Cursor::Grabbing } else { Cursor::Pointer })
                .on_press_tx(move |s: &mut State, tx: &mut Tx| {
                    set_sort(s, p, c);
                    if s.row_reorder[p].is_active() {
                        tx.wake_at(s.now + 1.0 / 60.0);
                    }
                })
                .on_drag(move |s: &mut State, d: &mut Drag, _tx: &mut Tx| match d.phase {
                    Phase::Down => {
                        let cur_order = s.layout.col_order[p];
                        let cur_w = s.layout.columns[p];
                        let mut x = 0.0;
                        for &cid in &cur_order {
                            if cid == col_id {
                                break;
                            }
                            x += cur_w[cid];
                        }
                        s.col_drag = Some(crate::anim::ColDrag {
                            pane: p,
                            col_id,
                            original_order: cur_order,
                            start_x: x,
                            visual_dx: 0.0,
                            moved: false,
                        });
                    }
                    Phase::Move => {
                        let Some(drag) = s.col_drag.as_mut().filter(|cd| cd.pane == p && cd.col_id == col_id) else {
                            return;
                        };
                        if !drag.moved && d.dx.abs() < 5.0 {
                            return;
                        }
                        drag.moved = true;
                        let cur_w = s.layout.columns[p];
                        let mut ord = s.layout.col_order[p];
                        let cur_slot = ord.iter().position(|&id| id == col_id).unwrap_or(slot);
                        let dragged_center = drag.start_x + d.dx + cur_w[col_id] * 0.5;
                        let mut acc = 0.0;
                        let mut target_slot = cur_slot;
                        for (idx, &cid) in ord.iter().enumerate() {
                            let cw = cur_w[cid];
                            let mid = acc + cw * 0.5;
                            if idx < cur_slot && dragged_center < mid {
                                target_slot = idx;
                                break;
                            }
                            if idx > cur_slot && dragged_center > mid {
                                target_slot = idx;
                            }
                            acc += cw;
                        }
                        if target_slot != cur_slot {
                            let mut v = ord.to_vec();
                            v.remove(cur_slot);
                            v.insert(target_slot, col_id);
                            ord = [v[0], v[1], v[2], v[3]];
                            s.layout.col_order[p] = ord;
                        }
                        let mut new_slot_x = 0.0;
                        for &cid in &ord {
                            if cid == col_id {
                                break;
                            }
                            new_slot_x += cur_w[cid];
                        }
                        drag.visual_dx = (drag.start_x + d.dx) - new_slot_x;
                    }
                    Phase::Up => {
                        if let Some(drag) = s.col_drag.take() {
                            if d.cancelled() {
                                s.layout.col_order[p] = drag.original_order;
                            } else if drag.moved {
                                s.layout.save();
                            }
                        }
                    }
                })
                .child(text(label.to_string()).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().min_w(0.0))
                .border_sides([0.0, 1.0, 0.0, 0.0], k.c.border_weak);

            if pane.sort.0 == c {
                let angle = if pane.sort.1 { -std::f32::consts::FRAC_PI_2 } else { std::f32::consts::FRAC_PI_2 };
                let chev = icon(&icons::CHEVRON, 10.0)
                    .key(format!("sort-chev-{c:?}"))
                    .animated_changes()
                    .color(k.c.text_muted)
                    .origin(0.5, 0.5)
                    .rotate(angle)
                    .rotate_motion(if sel_anim { crate::anim::hover_curve() } else { Anim::INSTANT });
                h = h.child(chev);
            }
            if end {
                h = h.justify_end();
            }
            if is_dragged {
                h = h
                    .raise()
                    .fill(k.c.bg_base)
                    .border(1.0, k.alpha(k.c.text_accent, 0.55))
                    .radius(4.0)
                    .box_shadow(rune::Shadow { y: 2.0, blur: 6.0, color: k.dark_a(0.28), ..Default::default() })
                    .offset(drag_dx, 0.0)
                    .offset_motion(Anim::INSTANT);
            } else if sel_anim {
                h = h.animated_changes().motion(crate::anim::ui_spring());
            }
            let _ = start_x;
            header_cells.push(h);
        }

        // The grips sit over the header, one at each column's right edge in visual order.
        let mut grip_edge = 0.0_f32;
        let mut grips = Vec::with_capacity(4);
        for &col_id in &order {
            grip_edge += w[col_id];
            grips.push(col_grip(k, p, col_id, grip_edge));
        }
        let header = row()
            .key("header")
            .w_full()
            .h(HEADER_H)
            .fill(k.c.bg_base)
            .sticky_top(0.0)
            .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
            .children(header_cells)
            .children(grips);
        // Gliding hover highlight pill (80 ms ease-out cubic, matching Explorer GPU mod):
        // sits behind the rows and slides smoothly from row to row without leaving a trail.
        // Where it goes and how much it shows are live values the pointer sets: crossing rows
        // moves it without running the view.
        let hover_pill = (hover_glides && n > 0 && pane.error.is_none()).then(|| {
            let curve = crate::anim::hover_curve();
            let view = &s.views[p];
            el().key("hover-pill")
                .absolute(0.0, 0.0)
                .radius(5.0)
                .fill(hover)
                .live_rect(view.hover_rect.smooth_with(curve))
                .live_opacity(view.hover_alpha.smooth_with(curve))
        });
        // Gliding keyboard selection pill (Cubic Hermite glide on ↑/↓/PageUp/PageDown/Home/End).
        let kbd_sel_pill = kbd_sel_active.then(|| {
            let y = s.kbd_sel[p].presented_row() * row_h;
            el().key("kbd-sel-pill").absolute(0.0, HEADER_H + y).size(total, row_h).radius(4.0).fill(selected)
        });
        // One viewport scrolls both ways: sideways when the columns are wider than the pane,
        // header and rows together, and up and down with the header stuck to the top. Its
        // vertical bar sits on the pane's right edge, wherever the columns end.
        col()
            .key("table")
            .w_full()
            .min_w(total)
            .children(hover_pill)
            .children(kbd_sel_pill)
            .child(header)
            .child(body)
            .children(lasso_rect(s, k, p))
    } else {
        col().key("table").w_full().child(body).children(lasso_rect(s, k, p))
    };

    let scrolled = s.views[p].scroll.clone();
    // List builds only the columns in sight: scrolled past them, the view runs again. The handler
    // can't change state, so it marks the viewport and wakes `poll`.
    let list_columns = (mode == ViewMode::List && n <= LIST_HORIZ_MAX).then(|| {
        let view = &s.views[p];
        let rows_per_col = view.flow.get().per_line.max(1);
        (view.columns.clone(), view.columns_stale.clone(), n.div_ceil(rows_per_col), s.waker.clone())
    });
    let viewport = col()
        .key(list_key(p))
        .instant_changes()
        .w_full()
        .h_full()
        .scroll_xy()
        .on_scroll_offset(move |at| {
            scrolled.set(at.into());
            if let Some((built, stale, count, waker)) = &list_columns
                && !crate::lasso::columns_cover(built.get(), at.offset[0], at.viewport[0], 4.0, LIST_COL_W + LIST_COL_GAP, *count)
                && !stale.replace(true)
                && let Some(wake) = waker
            {
                wake();
            }
        })
        // Track hovered row for the gliding hover highlight; returns live_only when the row
        // doesn't change so pointer motion inside a row triggers zero view rebuilds.
        .on_pointer_move_local(move |s: &mut State, ev: rune::PointerEvent, tx: &mut Tx| {
            if !s.settings.hover_animated() || s.panes[p].view_mode != ViewMode::Details {
                tx.live_only();
                return;
            }
            tx.live_only();
            let view = &s.views[p];
            let scroll = view.scroll.get();
            let content_x = ev.x + scroll.x;
            let content_y = ev.y + scroll.y - HEADER_H;
            let total_w: f32 = s.layout.columns[p].iter().sum();
            let rh = s.zoom[p].row_height().max(1.0);
            let row = (ev.y >= HEADER_H && content_y >= 0.0 && content_x >= 0.0 && content_x < total_w && s.lasso.is_none())
                .then(|| (content_y / rh).floor() as usize)
                .filter(|&i| i < s.panes[p].entries.len());
            match row {
                Some(i) => {
                    view.hover_rect.set([2.0, HEADER_H + i as f32 * rh, (total_w - 4.0).max(0.0), rh]);
                    view.hover_alpha.set(1.0);
                }
                // It fades where it was.
                None => {
                    view.hover_alpha.set(0.0);
                }
            }
        })
        .on_hover_change(move |s: &mut State, inside: bool, tx: &mut Tx| {
            tx.live_only();
            if !inside {
                s.views[p].hover_alpha.set(0.0);
            }
        })
        // Ctrl + mouse wheel smoothly zooms row height, icons, and text.
        .on_wheel(move |s: &mut State, e: rune::WheelEvent, tx: &mut Tx| {
            // The zoom is Details': the other views keep their sizes.
            if !(e.mods.ctrl || e.mods.cmd) || s.panes[p].view_mode != ViewMode::Details {
                return false;
            }
            let delta = if e.dy > 0.0 { 1.0 } else if e.dy < 0.0 { -1.0 } else { 0.0 };
            if delta != 0.0 && s.zoom[p].step_by(delta, s.now, s.settings.zoom_animated()) && s.zoom[p].is_animating() {
                tx.wake_at(s.now + 1.0 / 60.0);
            }
            true
        })
        // Trackpad pinch-to-zoom.
        .on_magnify(move |s: &mut State, e: rune::MagnifyEvent, tx: &mut Tx| {
            if s.panes[p].view_mode != ViewMode::Details {
                return false;
            }
            if s.zoom[p].scale_by(1.0 + e.delta, s.now, s.settings.zoom_animated()) && s.zoom[p].is_animating() {
                tx.wake_at(s.now + 1.0 / 60.0);
            }
            true
        })
        // Where the viewport is, for the lasso, and the rows in view below the header, which
        // Page Up and Page Down move by. Only handlers read them, so the view needn't run again.
        .on_layout_tx(move |s: &mut State, r: [f32; 4], tx: &mut Tx| {
            s.views[p].rect = r;
            let mode = s.panes[p].view_mode;
            if mode.is_grid() {
                let (tile_w, tile_h, _) = mode.tile_metrics();
                let new_cols = grid_columns_for(r[2], tile_w);
                let visible_rows = (r[3] / (tile_h + GRID_GAP)).floor().max(1.0) as usize;
                s.page_rows[p] = (visible_rows * new_cols).max(1);
                if s.grid_cols[p] != new_cols {
                    s.grid_cols[p] = new_cols;
                } else {
                    tx.live_only();
                }
            } else if mode == ViewMode::List {
                let new_rows = list_rows_for(r[3]);
                let new_cols = grid_columns_for(r[2], LIST_COL_W);
                // A page: a column's items, or past LIST_HORIZ_MAX, where the items run in rows,
                // the rows in view.
                s.page_rows[p] = if s.panes[p].entries.len() > LIST_HORIZ_MAX { new_rows * new_cols } else { new_rows };
                let n = s.panes[p].entries.len();
                let shown = n <= LIST_HORIZ_MAX
                    && !crate::lasso::columns_cover(s.views[p].columns.get(), s.views[p].scroll.get().x, r[2], 4.0, LIST_COL_W + LIST_COL_GAP, n.div_ceil(new_rows.max(1)));
                let needs_relayout = s.list_rows[p] != new_rows || shown || (n > LIST_HORIZ_MAX && s.grid_cols[p] != new_cols);
                s.list_rows[p] = new_rows;
                if s.panes[p].entries.len() > LIST_HORIZ_MAX {
                    s.grid_cols[p] = new_cols;
                }
                if !needs_relayout {
                    tx.live_only();
                }
            } else {
                let rh = s.zoom[p].row_height().max(1.0);
                s.page_rows[p] = ((r[3] - HEADER_H) / rh).floor().max(1.0) as usize;
                tx.live_only();
            }
        })
        .on_drag(move |s: &mut State, d: &mut Drag, tx: &mut Tx| crate::lasso::drag(s, p, d, tx))
        .on_secondary_press(move |s: &mut State, at: rune::PointerPress, tx: &mut Tx| {
            s.active = p;
            s.panes[p].selected.clear();
            tx.focus(pane_key(p), false);
            s.menu = Some((p, at));
        })
        .child(table);
    let key = list_key(p);
    let viewport = s.edges[p].list.on(viewport);
    col().key("list").w(0.0).grow(1.0).h_full().min_w(0.0).child(crate::scrollbars::xy_bars(k, &key, viewport, header_h, s.waker.clone()).w_full().h_full())
}

// ---------------------------------------------------------------------------------------------
// Status bar

// ---------------------------------------------------------------------------------------------
// Status bar

fn statusbar(s: &State, k: &Kit, p: usize) -> El {
    let pane = &s.panes[p];
    let n = pane.entries.len();
    let mut left = if n == 1 { tr!("{} item", "{} item", thousands(n)) } else { tr!("{} itens", "{} items", thousands(n)) };
    let picked: Vec<_> = if pane.selected.is_empty() { Vec::new() } else { pane.entries.iter().filter(|e| pane.selected.contains(&e.id)).collect() };
    if !picked.is_empty() {
        let bytes: u64 = picked.iter().filter_map(|e| e.size).sum();
        let count = thousands(picked.len());
        left.push_str(&if picked.len() == 1 { tr!("   ·   {count} selecionado", "   ·   {count} selected") } else { tr!("   ·   {count} selecionados", "   ·   {count} selected") });
        if bytes > 0 {
            left.push_str(&format!(" ({})", format_size(bytes)));
        }
    }
    if pane.loading {
        left.push_str(t("   ·   carregando…", "   ·   loading…"));
    }
    let right = pane.free.map(|b| tr!("{} livres", "{} free", format_size(b))).unwrap_or_default();
    // A Drive account's storage, beside My Drive's folders (a shared drive's files count
    // against its organisation, not the account).
    let quota = match &pane.loc {
        Location::Drive { email, space: Space::MyDrive, .. } => s.quotas.get(email).copied().flatten().map(|q| (quota_label(&q), q.limit.filter(|l| *l > 0).map(|l| q.usage as f64 / l as f64), quota_help(q))),
        // A cloud account's storage, anywhere in it.
        loc => crate::accounts::quota_bar(s, loc),
    };
    let (right, used, help) = match quota {
        Some((label, used, help)) => (label, used, help),
        None => (right, None, String::new()),
    };
    let meter = used.map(|u| u.min(1.0) as f32).map(|used| {
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
    let mut bar = row()
        .key("statusbar")
        .w_full()
        .h(24.0)
        .px(10.0)
        .center_y()
        .border_sides([1.0, 0.0, 0.0, 0.0], k.c.border_weak)
        .child(text(left).font(k.ty.text_xsmall).color(k.c.text_muted).truncate().shrink().min_w(0.0).grow(1.0));
    let zoom_pct = s.zoom[p].percent();
    if zoom_pct != 100 {
        bar = bar.child(
            row()
                .key("zoom-reset")
                .role("button")
                .label(t("Redefinir zoom (Ctrl+0)", "Reset zoom (Ctrl+0)"))
                .help(t("Clique ou pressione Ctrl+0 para voltar a 100%", "Click or press Ctrl+0 to reset to 100%"))
                .h(16.0)
                .px(5.0)
                .mr(8.0)
                .center()
                .radius(4.0)
                .fill(k.alpha(k.c.text_accent, if k.dark { 0.16 } else { 0.10 }))
                .hover(k.alpha(k.c.text_accent, if k.dark { 0.24 } else { 0.16 }))
                .cursor(Cursor::Pointer)
                .on_press_tx(move |s: &mut State, tx: &mut Tx| {
                    if s.zoom[p].reset(s.now, s.settings.zoom_animated()) && s.zoom[p].is_animating() {
                        tx.wake_at(s.now + 1.0 / 60.0);
                    }
                })
                .child(text(format!("{zoom_pct}%")).font(k.ty.caption()).color(k.c.text_accent)),
        );
    }
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
        (Some(limit), Some(free)) => tr!("{} livres de {}", "{} free of {}", format_size(free), whole_size(limit)),
        _ => tr!("{} usados · Ilimitado", "{} used · Unlimited", format_size(q.usage)),
    }
}

/// The quota's breakdown, on hover.
fn quota_help(q: openexplorer_gdrive::Quota) -> String {
    let mut out = match q.limit {
        Some(l) => tr!("Usado: {} de {}", "Used: {} of {}", format_size(q.usage), whole_size(l)),
        None => tr!("Usado: {}", "Used: {}", format_size(q.usage)),
    };
    out.push_str(&tr!("
No Drive: {} (lixeira: {})", "
In Drive: {} (trash: {})", format_size(q.in_drive), format_size(q.in_trash)));
    out.push_str(&tr!("
Gmail e Fotos: {}", "
Gmail and Photos: {}", format_size(q.usage.saturating_sub(q.in_drive))));
    out
}

/// A plan's size without a needless ",0": "15 GB", "2 TB", but "1,5 TB".
pub(crate) fn whole_size(b: u64) -> String {
    let s = format_size(b);
    match s.split_once(' ') {
        Some((n, unit)) if n.ends_with(",0") || n.ends_with(".0") => format!("{} {unit}", &n[..n.len() - 2]),
        _ => s,
    }
}

/// A count with its thousands grouped: "1.946" in Portuguese, "1,946" in English.
pub fn thousands(n: usize) -> String {
    openexplorer_i18n::grouped(n as u64)
}
