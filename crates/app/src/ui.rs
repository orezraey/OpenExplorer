//! The window: a toolbar, two panes (each a location tree beside a file list, under a
//! navigation bar and over a status bar), and the message line.

use std::borrow::Cow;
use std::path::PathBuf;

use rune::{Align, Color, Cursor, Cx, Drag, DragOutcome, El, GrowthSpec, Icon, KeyEvent, Phase, ScrollSpec, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, Dialog as Modal, Kit, MenuItem, ScrollBar, ScrollBarStyle, Toast, ToastRegion, context_menu};

use crate::model::{Kind, Location, SortCol, Space, crumbs, format_bytes, format_long, format_size, format_time, shown};
use crate::actions::{self, ClipMode, Dialog};
use crate::{State, click, disconnect, drag_item, dragged_ids, dropped, go_back, go_forward, go_up, key, load, navigate, open, pane_key, paths_for, row_key, set_sort};
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
    // The window's height, so Opções and Registrar conta always fit inside short windows from
    // the very first frame they open.
    let window_h = s.dialog.is_some().then(|| cx.layout_with("window", |r| r[3])).flatten();
    dialog(s, &k, window_h, &mut layer);
    // What the last action said, for a few seconds at the bottom right.
    if !s.status.is_empty() && s.now < s.status_until {
        let close = |s: &mut State| s.status_until = 0.0;
        let mut toast = Toast::new("notice", s.status.clone()).on_close(close);
        // The latest action's notice offers to take it back (or to redo what was just undone).
        if let Some(redo) = s.history.offers(&s.status) {
            toast = toast.action(&k, if redo { "Refazer" } else { "Desfazer" }, false, move |s: &mut State, _| crate::undo::undo(s, redo));
        }
        layer.push(ToastRegion::new().toast(toast.build(&k)).build(&k));
    }
    let body = col()
        .key("window")
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

/// A number with − and + beside it inside a pill box, modernized from Air Explorer's spin boxes.
fn stepper(k: &Kit, key: &str, value: u8, set: fn(&mut State, i32)) -> El {
    let btn = |label: &str, delta: i32, enabled: bool| {
        let ink = if enabled { k.c.text_base } else { k.c.text_faint };
        let b = row()
            .key(format!("{key}-{delta}"))
            .role("button")
            .label(if delta < 0 { "Diminuir" } else { "Aumentar" })
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
    s.settings.save();
}

fn reload_hidden_views(s: &mut State) {
    crate::tree::refresh_hidden(s);
    for p in 0..2 {
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
        s.transfers.retain(|t| t.progress.stage() != Stage::Done);
    }
}

fn reset_settings(s: &mut State, tx: &mut Tx) {
    let was_hidden = s.settings.show_hidden;
    s.settings = crate::settings::Settings::default();
    s.settings.save();
    let (sans, mono) = s.settings.font.families();
    tx.set_font_families(sans, mono);
    tx.set_font_smoothing(s.settings.text_render.smoothing());
    if was_hidden != s.settings.show_hidden {
        reload_hidden_views(s);
    }
    if s.settings.clear_completed {
        s.transfers.retain(|t| t.progress.stage() != Stage::Done);
    }
    s.status = "Opções restauradas para os valores padrão.".into();
}

fn set_options_tab(s: &mut State, tab: crate::settings::OptionsTab) {
    if let Some(Dialog::Options(opt)) = &mut s.dialog {
        opt.tab = tab;
        if tab == crate::settings::OptionsTab::Logs {
            opt.refresh_logs();
        }
    }
}

/// The Opções body's height: 530 points, or less in a window too short for that (the dialog
/// doesn't scroll as a whole), keeping the footer's Concluído in view. `window_h` is the
/// window's height once known; the layer around the dialog pads 16 points, its header takes 48.
fn options_height(window_h: Option<f32>) -> f32 {
    window_h.map_or(530.0, |h| (h - 2.0 * 16.0 - 48.0).clamp(240.0, 530.0))
}

fn options_dialog(s: &State, k: &Kit, opt: &crate::settings::OptionsView, window_h: Option<f32>, layer: &mut Vec<El>) {
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
                    .key(format!("opt-tab-{}", tab.label()))
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
        Button::new("Restaurar padrões")
            .variant(ButtonVariant::Neutral)
            .disabled(!modified)
            .on_press_tx(reset_settings)
            .build(k)
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
            pills = pills.child(seg_btn(format!("{prefix}-{}", f.label()), f.label(), on).on_press_tx(move |s: &mut State, tx: &mut Tx| {
                change_settings(s, |c| c.font = f);
                let (sans, mono) = f.families();
                tx.set_font_families(sans, mono);
            }));
        }
        pills
    };
    let make_render_pills = |track_key: &'static str, prefix: &'static str| {
        let mut pills = seg_track(track_key);
        for &r in TextRender::available() {
            let on = st.text_render == r;
            pills = pills.child(seg_btn(format!("{prefix}-{}", r.label()), r.label(), on).on_press_tx(move |s: &mut State, tx: &mut Tx| {
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
                            .key(format!("th-{}", th.label()))
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
                    .on_press(move |s: &mut State| change_settings(s, |c| c.theme = th))
                    .child(icon(th.icon(), 15.0).color(if on { k.c.text_accent } else { k.c.icon_muted }))
                    .child(text(th.label()).font(if on { k.ty.text_small_medium } else { k.ty.text_small }).color(if on { k.c.text_base } else { k.c.text_muted }));
                theme_pills = theme_pills.child(pill);
            }

            let font_desc = if cfg!(windows) {
                "Usa a fonte nativa do Windows (Segoe UI) ou a fonte integrada Inter."
            } else if cfg!(target_os = "macos") {
                "Usa a fonte nativa do macOS (San Francisco) ou a fonte integrada Inter."
            } else {
                "Usa a fonte nativa do sistema ou a fonte integrada Inter."
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
                                .child(text("Tema do aplicativo").font(k.ty.text_small_medium).color(k.c.text_base))
                                .child(text("Escolha a aparência das janelas ou acompanhe automaticamente o tema do sistema.").font(k.ty.caption()).color(k.c.text_muted).wrap().w_full()),
                        )
                        .child(theme_pills),
                )
                .child(divider())
                .child(control_row("row-font", "Fonte da interface", font_desc, make_font_pills("font-pills", "font")));

            let purge_badge = st.skip_recycle_bin.then_some(("Permanente", rune_kit::BadgeVariant::Accent));
            let files_card = card_box("card-files")
                .child(switch_row(
                    "opt-hidden",
                    "Exibir arquivos ocultos",
                    "Mostra pastas e arquivos ocultos do sistema ou iniciados por ponto (ex.: .git, .cargo).",
                    None,
                    st.show_hidden,
                    toggle_show_hidden,
                ))
                .child(divider())
                .child(switch_row(
                    "opt-clear-done",
                    "Limpar concluídos automaticamente",
                    "Remove da fila inferior os envios e downloads assim que terminam sem erro.",
                    None,
                    st.clear_completed,
                    toggle_clear_completed,
                ))
                .child(divider())
                .child(switch_row(
                    "opt-skip-trash",
                    "Não mover arquivos locais para a Lixeira",
                    "Exclui itens do computador permanentemente após confirmação, sem passar pela Lixeira.",
                    purge_badge,
                    st.skip_recycle_bin,
                    |s| change_settings(s, |c| c.skip_recycle_bin = !c.skip_recycle_bin),
                ));

            col()
                .key("page-general")
                .w_full()
                .gap(12.0)
                .child(section_head("Geral", "Aparência, fonte da interface e comportamento de arquivos e transferências."))
                .child(appearance)
                .child(files_card)
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
                    text("Opções avançadas de diagnóstico do motor gráfico. O padrão Suave (GPU) já entrega a melhor qualidade e eficiência.")
                        .font(k.ty.caption())
                        .color(k.c.text_muted)
                        .wrap()
                        .grow(1.0)
                        .w(0.0)
                        .min_w(0.0),
                );

            let render_desc = if cfg!(windows) {
                "Alterna entre o suavizador vetorial na GPU (padrão recomendado), DirectWrite em tons de cinza e subpixels ClearType."
            } else {
                "Este sistema usa o suavizador vetorial nativo na GPU (DirectWrite e ClearType estão disponíveis apenas no Windows)."
            };
            let exp_card = card_box("card-experimental").child(control_row(
                "row-render",
                "Nitidez e rasterização de texto",
                render_desc,
                make_render_pills("render-pills", "render"),
            ));

            col()
                .key("page-experimental")
                .w_full()
                .gap(12.0)
                .child(section_head("Experimental", "Parâmetros internos de renderização para testes e monitores específicos."))
                .child(banner)
                .child(exp_card)
        }
        OptionsTab::Transfers => {
            // `control_row`, a little roomier around the steppers.
            let count_row = |key: &'static str, title: &str, desc: &str, control: El| control_row(key, title, desc, control).py(12.0).gap(14.0);

            let concurrency = card_box("card-concurrency")
                .child(count_row(
                    "row-uploads",
                    "Uploads simultâneos",
                    "Arquivos enviados ao mesmo tempo para o Google Drive ou servidor remoto (1 a 10).",
                    stepper(k, "uploads", st.uploads, |s, d| change_settings(s, |c| c.uploads = at_once(i32::from(c.uploads) + d))),
                ))
                .child(divider())
                .child(count_row(
                    "row-downloads",
                    "Downloads simultâneos",
                    "Arquivos baixados ao mesmo tempo para o computador (1 a 10).",
                    stepper(k, "downloads", st.downloads, |s, d| change_settings(s, |c| c.downloads = at_once(i32::from(c.downloads) + d))),
                ));

            let speed_card = card_box("card-segmented").child(switch_row(
                "opt-segmented",
                "Baixar arquivos grandes em segmentos paralelos",
                "Divide arquivos grandes em múltiplas conexões simultâneas para acelerar o download.",
                Some(("≥ 64 MB", rune_kit::BadgeVariant::Subtle)),
                st.segmented_downloads,
                |s| change_settings(s, |c| c.segmented_downloads = !c.segmented_downloads),
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
                    text("As alterações de simultaneidade e segmentos paralelos passam a valer na próxima transferência iniciada. Cada servidor também respeita o próprio limite de conexões configurado na conta.")
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
                .child(section_head("Transferências", "Ajuste quantas transferências rodam em paralelo e a aceleração de downloads grandes."))
                .child(concurrency)
                .child(speed_card)
                .child(info_box)
        }
        OptionsTab::Logs => {
            let visible = opt.filtered_logs();
            let size_badge = if opt.log_bytes > 0 { format_size(opt.log_bytes) } else { "Vazio".into() };

            let tools = row()
                .key("log-tools")
                .w_full()
                .gap(6.0)
                .center_y()
                .child(text("Logs").font(k.ty.body_medium).color(k.c.text_base))
                .child(rune_kit::Badge::new(size_badge).variant(rune_kit::BadgeVariant::Subtle).standard().build(k))
                .child(el().grow(1.0))
                .child(
                    Button::new("Atualizar")
                        .icon(&icons::REFRESH)
                        .variant(ButtonVariant::Neutral)
                        .on_press(|s: &mut State| {
                            if let Some(Dialog::Options(opt)) = &mut s.dialog {
                                opt.refresh_logs();
                            }
                        })
                        .build(k),
                )
                .child(
                    Button::new("Copiar")
                        .variant(ButtonVariant::Neutral)
                        .disabled(visible.is_empty())
                        .on_press_tx(|s: &mut State, tx: &mut Tx| {
                            // Joined on the press, not on every frame the tab is drawn.
                            let Some(Dialog::Options(opt)) = &s.dialog else { return };
                            tx.copy(opt.filtered_logs().join("\n"));
                            s.status = "Log copiado para a área de transferência.".into();
                        })
                        .build(k),
                )
                .child(
                    Button::new("Abrir pasta")
                        .variant(ButtonVariant::Neutral)
                        .on_press(|s: &mut State| {
                            let dir = openexplorer_gdrive::local_dir();
                            let _ = std::fs::create_dir_all(&dir);
                            if let Err(e) = crate::ops::open(&dir) {
                                s.status = e;
                            }
                        })
                        .build(k),
                )
                .child(
                    Button::new("Limpar")
                        .variant(ButtonVariant::Danger)
                        .disabled(opt.log_lines.is_empty())
                        .on_press(|s: &mut State| match openexplorer_gdrive::clear_log() {
                            Ok(()) => {
                                if let Some(Dialog::Options(opt)) = &mut s.dialog {
                                    opt.refresh_logs();
                                }
                                s.status = "Arquivo de log limpo.".into();
                            }
                            Err(e) => s.status = e,
                        })
                        .build(k),
                );

            let mut filter_input = rune_kit::TextInput::new(&opt.log_filter)
                .width(None)
                .label("Filtrar log")
                .leading_icon(&icons::SEARCH)
                .placeholder("Filtrar registros do log (ex.: erro, sftp, cota, operação)…")
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
                    "Nenhum evento registrado ainda em openexplorer.log."
                } else {
                    "Nenhuma linha do log corresponde ao filtro digitado."
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
                    let is_err = lower.contains("erro") || lower.contains("falha") || lower.contains("pânico") || lower.contains("http 4") || lower.contains("http 5");
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
                let bar = bar_style(k);
                col()
                    .key("log-box")
                    .w_full()
                    .grow(1.0)
                    .min_h(0.0)
                    .radius(k.radius.md)
                    .fill(k.c.bg_base)
                    .border(1.0, k.c.border_weak)
                    .clip_rounded(k.radius.md)
                    .child(col().key("log-scroll").w_full().h_full().scroll(ScrollSpec { stick_bottom: false }).child(lines_col).scroll_bar_styled(bar))
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
        let bar = bar_style(k);
        col()
            .key("opt-main-scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec { stick_bottom: false })
            .child(col().w_full().padding(16.0).pr(20.0).child(page))
            .scroll_bar_styled(bar)
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
        .child(text("Alterações salvas automaticamente").font(k.ty.caption()).color(k.c.text_faint).grow(1.0))
        .child(Button::new("Concluído").variant(ButtonVariant::Submit).on_press(close).build(k).default_button());

    let right_col = col().key("opt-right").grow(1.0).w(0.0).h_full().min_w(0.0).child(main_scroll).child(footer);

    let body = row()
        .key("opt-split")
        .w_full()
        .h(options_height(window_h))
        .border_sides([1.0, 0.0, 0.0, 0.0], k.c.border_weak)
        .child(nav)
        .child(right_col);

    Modal::new("options", "Opções").open(true).child(body).width(750.0).fit().on_close(close).build(k, layer);
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
        .child(act.build(k).default_button())
}

fn dialog(s: &State, k: &Kit, window_h: Option<f32>, layer: &mut Vec<El>) {
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
            let remote = items.iter().filter_map(|e| s.panes[*pane].loc.local_path(e)).any(|path| crate::network::is_remote(&path));
            let note: Cow<str> = if s.panes[*pane].loc.is_local() && !remote && s.settings.skip_recycle_bin {
                "A exclusão direta está ativada nas Opções: os itens serão apagados para sempre, sem passar pela Lixeira. Isso não pode ser desfeito.".into()
            } else if s.panes[*pane].loc.is_local() {
                "Aqui não há Lixeira: os itens serão apagados para sempre. Isso não pode ser desfeito.".into()
            } else if let Location::Server { server, .. } = &s.panes[*pane].loc {
                format!("Servidores não têm lixeira: os itens serão apagados para sempre de {}. Isso não pode ser desfeito.", server.name).into()
            } else {
                "Os itens serão apagados para sempre do Google Drive. Isso não pode ser desfeito.".into()
            };
            let body = col()
                .w_full()
                .gap(12.0)
                .child(text(note.into_owned()).font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0))
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
        Dialog::Options(opt) => options_dialog(s, k, opt, window_h, layer),
        Dialog::Clash(ask) => clash_dialog(s, k, ask, layer),
        Dialog::Share(sh) => crate::sharing::share_dialog(s, k, sh, layer),
        Dialog::MakePublic(m) => crate::sharing::public_dialog(k, m, layer),
        Dialog::Server(f) => account_dialog(s, k, f, window_h, layer),
    }
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

/// Scrollable body region inside `account_dialog`, capped to `window_h` so the dialog header
/// and action buttons remain visible even at the minimum window height (420px) or when long
/// certificate/error messages appear.
fn account_scroll(k: &Kit, key: &'static str, window_h: Option<f32>, content: El) -> El {
    let max_h = window_h.map_or(620.0, |h| (h - 2.0 * 16.0 - 48.0 - 62.0).max(120.0));
    let bar = bar_style(k);
    col()
        .w_full()
        .max_h(max_h)
        .min_h(0.0)
        .shrink()
        .child(
            col()
                .key(key)
                .w_full()
                .shrink()
                .min_h(0.0)
                .scroll(ScrollSpec { stick_bottom: false })
                .child(content)
                .scroll_bar_styled(bar),
        )
}

/// Unified Adicionar conta / Editar conta dialog:
/// - `Step::Pick`: modern categorized provider grid with instant search and URL detection.
/// - `Step::Configure`: tailored OAuth or server credentials screen with `← Voltar`.
fn account_dialog(s: &State, k: &Kit, f: &crate::servers::Form, window_h: Option<f32>, layer: &mut Vec<El>) {
    use crate::servers::{Category, Form, Provider, Step};
    use openexplorer_remote::Protocol;

    fn part(s: &mut State, pick: fn(&mut Form) -> &mut rune::TextEdit) -> &mut rune::TextEdit {
        match &mut s.dialog {
            Some(Dialog::Server(f)) => pick(f),
            _ => &mut s.name_edit,
        }
    }

    if f.step == Step::Pick {
        let mut search = rune_kit::TextInput::new(&f.filter)
            .width(None)
            .large()
            .label("Buscar provedor")
            .leading_icon(&icons::SEARCH)
            .placeholder("Buscar provedor ou colar endereço (ex.: Google Drive, FTP, sftp://nas)…")
            .on_edit(|s: &mut State| part(s, |f| &mut f.filter))
            .on_submit(|s: &mut State, _: &mut Tx| crate::servers::submit_filter(s));
        if !f.filter.text.is_empty() {
            search = search.clear_button().on_action(|s: &mut State, _: &mut Tx| crate::servers::clear_filter(s));
        }

        // Category filter tabs with icons (inspired by Air Explorer, modernized as pill tabs).
        let mut tabs = row().key("cat-tabs").w_full().gap(6.0).center_y();
        for cat in Category::ALL {
            let on = f.category == cat;
            let count = Provider::ALL.into_iter().filter(|p| p.in_category(cat)).count();
            let fill = if on { k.alpha(k.c.text_accent, if k.dark { 0.20 } else { 0.11 }) } else { k.c.bg_base };
            let border = if on { k.alpha(k.c.text_accent, 0.50) } else { k.c.border_weak };
            let icon_ink = if on { k.c.text_accent } else { k.c.icon_muted };
            let text_ink = if on { k.c.text_base } else { k.c.text_muted };
            let pill = k
                .interactive(
                    row()
                        .key(format!("cat-{}", cat.label()))
                        .role("tab")
                        .label(cat.label())
                        .checked(on)
                        .focusable(true)
                        .focus_ring(2.0, 1.0, k.c.border_focus)
                        .h(30.0)
                        .px(11.0)
                        .gap(6.0)
                        .center_y()
                        .radius(k.radius.md)
                        .fill(fill)
                        .border(1.0, border)
                        .cursor(Cursor::Pointer),
                )
                .on_press(move |s: &mut State| crate::servers::set_category(s, cat))
                .child(icon(cat.icon(), 14.0).color(icon_ink))
                .child(text(cat.label()).font(if on { k.ty.text_small_medium } else { k.ty.text_small }).color(text_ink))
                .child(
                    row()
                        .h(16.0)
                        .px(5.0)
                        .center()
                        .radius(8.0)
                        .fill(if on { k.alpha(k.c.text_accent, 0.20) } else { k.alpha(k.c.text_base, 0.06) })
                        .child(text(count.to_string()).font(k.ty.caption()).color(if on { k.c.text_accent } else { k.c.text_faint })),
                );
            tabs = tabs.child(pill);
        }

        // 3-column provider grid.
        let visible = f.visible_providers();
        let grid: El = if visible.is_empty() {
            let trimmed = f.filter.text.trim().trim_matches('"');
            let is_url = crate::servers::parse_address(trimmed).is_some();
            let msg = if is_url {
                format!("Endereço “{trimmed}” reconhecido. Clique abaixo ou pressione Enter para configurar.")
            } else {
                format!("Nenhum provedor encontrado para “{}”.", f.filter.text.trim())
            };
            let act = if is_url {
                Button::new("Configurar endereço").variant(ButtonVariant::Submit).on_press(crate::servers::submit_filter).build(k).default_button()
            } else {
                Button::new("Limpar busca").variant(ButtonVariant::Neutral).on_press(crate::servers::clear_filter).build(k)
            };
            col()
                .key("empty-grid")
                .w_full()
                .min_h(210.0)
                .padding(20.0)
                .gap(12.0)
                .center()
                .radius(k.radius.lg)
                .fill(k.c.bg_base)
                .border(1.0, k.c.border_weak)
                .child(text(msg).font(k.ty.text_small).color(k.c.text_muted).align(Align::Center).wrap().w_full())
                .child(act)
        } else {
            let mut rows = col().key("provider-grid").w_full().gap(10.0);
            for (r_idx, chunk) in visible.chunks(3).enumerate() {
                let mut line = row().key(format!("p-row-{r_idx}")).w_full().gap(10.0);
                for &provider in chunk {
                    let connected = connected_count(s, provider);
                    let pedestal_bg = if k.dark { k.alpha(k.c.white, 0.055) } else { k.alpha(k.c.black, 0.035) };
                    let mut card = k
                        .interactive(
                            col()
                                .key(format!("provider-{}", provider.label()))
                                .role("button")
                                .label(format!("{} — {}", provider.label(), provider.subtitle()))
                                .focusable(true)
                                .focus_ring(2.0, 1.0, k.c.border_focus)
                                .grow(1.0)
                                .w(0.0)
                                .h(118.0)
                                .padding(12.0)
                                .gap(6.0)
                                .items_center()
                                .justify_center()
                                .radius(k.radius.lg)
                                .fill(k.c.bg_base)
                                .border(1.0, k.c.border_weak)
                                .cursor(Cursor::Pointer),
                        )
                        .on_press(move |s: &mut State| crate::servers::pick_provider(s, provider))
                        .child(row().size(48.0, 48.0).center().radius(12.0).fill(pedestal_bg).child(icon(provider.icon(), 34.0)))
                        .child(text(provider.label()).font(k.ty.text_small_medium).color(k.c.text_base).align(Align::Center))
                        .child(text(provider.subtitle()).font(k.ty.caption()).color(k.c.text_muted).align(Align::Center).truncate().w_full());
                    if connected > 0 {
                        let badge_label = if connected == 1 { "1 ativa".to_string() } else { format!("{connected} ativas") };
                        card = card.child(
                            row()
                                .pin(None, Some(8.0), Some(8.0), None)
                                .h(16.0)
                                .px(5.0)
                                .gap(3.0)
                                .center_y()
                                .radius(8.0)
                                .fill(k.alpha(k.c.success.fg, if k.dark { 0.18 } else { 0.12 }))
                                .child(el().size(5.0, 5.0).radius(2.5).fill(k.c.success.fg))
                                .child(text(badge_label).font(k.ty.caption()).color(k.c.success.fg)),
                        );
                    }
                    line = line.child(card);
                }
                // Keep partial rows aligned to a 3-column grid.
                for filler in chunk.len()..3 {
                    line = line.child(el().key(format!("filler-{r_idx}-{filler}")).grow(1.0).w(0.0));
                }
                rows = rows.child(line);
            }
            rows
        };

        let footer_note: El = if s.login.is_some() {
            row()
                .gap(6.0)
                .center_y()
                .grow(1.0)
                .min_w(0.0)
                .child(el().size(7.0, 7.0).radius(3.5).fill(k.c.text_accent))
                .child(text("Aguardando autorização do Google no navegador…").font(k.ty.text_xsmall).color(k.c.text_accent).truncate().min_w(0.0))
        } else {
            row()
                .gap(5.0)
                .center_y()
                .grow(1.0)
                .min_w(0.0)
                .child(icon(k.glyph.lock, 13.0).color(k.c.icon_faint))
                .child(text("Senhas e tokens ficam protegidos no Cofre do sistema.").font(k.ty.caption()).color(k.c.text_faint).truncate().min_w(0.0))
        };

        let picker_inner = col()
            .w_full()
            .px(16.0)
            .pb(4.0)
            .gap(12.0)
            .child(text("Escolha uma nuvem, NAS ou protocolo remoto para conectar ao Open Explorer.").font(k.ty.text_small).color(k.c.text_muted).wrap().w_full().min_w(0.0))
            .child(search.build(k))
            .child(tabs)
            .child(grid);

        let body = col()
            .key("picker-content")
            .w_full()
            .child(account_scroll(k, "picker-scroll", window_h, picker_inner))
            .child(row().key("picker-footer").w_full().px(16.0).pt(12.0).pb(16.0).gap(8.0).center_y().child(footer_note).child(Button::new("Cancelar").variant(ButtonVariant::Neutral).on_press(close).build(k)));

        Modal::new("server", "Registrar conta").open(true).child(body).width(600.0).fit().on_close(close).build(k, layer);
        return;
    }

    // -----------------------------------------------------------------------------------------
    // Step 2: Provider-specific configuration (Google Drive OAuth or server credentials)

    let proto = f.effective_protocol();
    let (summary, badge_text) = match (f.provider, proto) {
        (Provider::GoogleDrive, _) => ("Autorização segura OAuth 2.0 pelo navegador padrão", "OAuth 2.0"),
        (Provider::Nextcloud, Protocol::Davs) => ("Servidor Nextcloud · conexão criptografada", "HTTPS"),
        (Provider::Nextcloud, _) => ("Servidor Nextcloud · conexão sem criptografia", "HTTP"),
        (Provider::OwnCloud, Protocol::Davs) => ("Servidor ownCloud · conexão criptografada", "HTTPS"),
        (Provider::OwnCloud, _) => ("Servidor ownCloud · conexão sem criptografia", "HTTP"),
        (Provider::Sftp, _) => ("Transferência segura sobre SSH · porta padrão 22", "SSH"),
        (Provider::Ftp, Protocol::Ftps) => ("FTP explícito ou implícito com criptografia TLS", "FTPS"),
        (Provider::Ftp, _) => ("FTP padrão · ative SSL/TLS abaixo se o servidor aceitar", "FTP"),
        (Provider::WebDav, Protocol::Davs) => ("Servidor WebDAV seguro · porta padrão 443", "HTTPS"),
        (Provider::WebDav, _) => ("Servidor WebDAV sem criptografia · porta padrão 80", "HTTP"),
    };

    let badge_variant = if f.provider != Provider::GoogleDrive && proto.plain() {
        rune_kit::BadgeVariant::Neutral
    } else {
        rune_kit::BadgeVariant::Subtle
    };

    let banner = row()
        .key("provider-banner")
        .w_full()
        .px(12.0)
        .py(9.0)
        .gap(10.0)
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
                .child(text(f.provider.label()).font(k.ty.text_small_medium).color(k.c.text_base))
                .child(text(summary).font(k.ty.caption()).color(k.c.text_muted).truncate().min_w(0.0)),
        )
        .child(rune_kit::Badge::new(badge_text).variant(badge_variant).standard().build(k));

    let eye_toggle = |key: &'static str, revealed: bool, flip: fn(&mut State)| -> El {
        row()
            .key(key)
            .role("button")
            .label(if revealed { "Ocultar senha" } else { "Mostrar senha" })
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

    // Google Drive step.
    if f.provider == Provider::GoogleDrive {
        // Whether a client is configured was read once, on picking Google Drive (`custom_oauth`),
        // not on every frame.
        let show_oauth_form = f.custom_oauth;
        let mut inner = col()
            .w_full()
            .px(16.0)
            .pb(4.0)
            .gap(12.0)
            .child(banner)
            .child(
                text("Ao continuar, o navegador abrirá a página do Google para você escolher a conta e autorizar o acesso ao Meu Drive e aos Drives compartilhados.")
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
                    .child(text(format!("Já conectadas: {emails}")).font(k.ty.text_xsmall).color(k.c.text_base).truncate().min_w(0.0).grow(1.0)),
            );
        }

        if show_oauth_form {
            let id_input = rune_kit::TextInput::new(&f.client_id)
                .width(None)
                .label("Client ID (ou JSON completo)")
                .placeholder("123456-abc.apps.googleusercontent.com ou cole o JSON inteiro")
                .on_edit(|s: &mut State| part(s, |f| &mut f.client_id))
                .on_submit(|s: &mut State, tx: &mut Tx| crate::servers::submit_google(s, tx));
            let secret_eye = eye_toggle("secret-eye", f.reveal_secret, |s| {
                if let Some(Dialog::Server(f)) = &mut s.dialog {
                    f.reveal_secret = !f.reveal_secret;
                }
            });
            let secret_input = rune_kit::TextInput::new(&f.client_secret)
                .width(None)
                .label("Client Secret")
                .placeholder("GOCSPX-… (dispensável se colar o JSON acima)")
                .revealed(f.reveal_secret)
                .trailing(secret_eye)
                .on_edit(|s: &mut State| part(s, |f| &mut f.client_secret))
                .on_submit(|s: &mut State, tx: &mut Tx| crate::servers::submit_google(s, tx));
            let console_btn = Button::new("Abrir Google Cloud Console")
                .icon(&icons::EXTERNAL_LINK)
                .variant(ButtonVariant::Neutral)
                .on_press_tx(|s: &mut State, tx: &mut Tx| {
                    if let Err(e) = tx.open_url("https://console.cloud.google.com/apis/credentials") {
                        s.status = format!("Não consegui abrir o navegador: {e:?}");
                    }
                })
                .build(k);
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
                        .child(text("Cliente OAuth (App para computador)").font(k.ty.text_small_medium).color(k.c.text_base).grow(1.0))
                        .child(console_btn),
                )
                .child(
                    text("Crie uma credencial OAuth do tipo “App para computador” no Google Cloud Console, ou cole o arquivo google-client.json diretamente abaixo:")
                        .font(k.ty.caption())
                        .color(k.c.text_muted)
                        .wrap()
                        .w_full()
                        .min_w(0.0),
                )
                .child(col().w_full().gap(4.0).child(text("Client ID ou JSON").font(k.ty.text_xsmall_medium).color(k.c.text_base)).child(id_input.build(k)))
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
                .child(text("Cliente OAuth configurado e pronto para conectar.").font(k.ty.text_xsmall).color(k.c.text_muted).grow(1.0).min_w(0.0))
                .child(
                    Button::new("Alterar Client ID")
                        .variant(ButtonVariant::GhostMuted)
                        .on_press(|s: &mut State| {
                            if let Some(Dialog::Server(f)) = &mut s.dialog {
                                f.custom_oauth = true;
                                f.client_id.focused = true;
                            }
                        })
                        .build(k),
                );
            inner = inner.child(ready_row);
        }

        if let Some(e) = &f.error {
            inner = inner.child(text(e.clone()).key("error").font(k.ty.text_small).color(k.c.danger.fg).wrap().w_full().min_w(0.0));
        }

        let actions = row()
            .key("actions")
            .w_full()
            .px(16.0)
            .pt(12.0)
            .pb(16.0)
            .gap(8.0)
            .center_y()
            .child(Button::new("Voltar").icon(&icons::BACK).variant(ButtonVariant::Neutral).on_press(crate::servers::back_to_pick).build(k))
            .child(el().grow(1.0))
            .child(Button::new("Cancelar").variant(ButtonVariant::Neutral).on_press(close).build(k))
            .child(Button::new("Conectar no navegador").variant(ButtonVariant::Submit).on_press_tx(crate::servers::submit_google).build(k).default_button());

        let body = col()
            .key("gdrive-content")
            .w_full()
            .child(account_scroll(k, "gdrive-scroll", window_h, inner))
            .child(actions);

        Modal::new("server", "Conectar Google Drive").open(true).child(body).width(560.0).fit().on_close(close).build(k, layer);
        return;
    }

    // Server credentials step (FTP, SFTP, WebDAV, Nextcloud, ownCloud).
    let field = |key: &'static str, label: &str, edit: &rune::TextEdit, placeholder: &str, pick: fn(&mut Form) -> &mut rune::TextEdit| {
        let input = rune_kit::TextInput::new(edit).width(None).label(label).placeholder(placeholder).on_edit(move |s: &mut State| part(s, pick)).on_submit(|s: &mut State, _: &mut Tx| crate::servers::submit(s));
        col().key(key).w_full().gap(4.0).child(text(label.to_string()).font(k.ty.text_small_medium).color(k.c.text_base)).child(input.build(k))
    };

    let pass_eye = eye_toggle("pass-eye", f.reveal_password, |s| {
        if let Some(Dialog::Server(f)) = &mut s.dialog {
            f.reveal_password = !f.reveal_password;
        }
    });
    let password_label = match (f.editing.is_some(), f.provider) {
        (true, _) => "Senha (vazio: manter a salva)",
        (false, Provider::Nextcloud | Provider::OwnCloud) => "Senha ou senha de app",
        (false, _) => "Senha",
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
                .on_edit(|s: &mut State| part(s, |f| &mut f.password))
                .on_submit(|s: &mut State, _: &mut Tx| crate::servers::submit(s))
                .build(k),
        );

    let muted = |t: String| text(t).font(k.ty.text_xsmall).color(k.c.text_muted).wrap().w_full().min_w(0.0);
    let port_hint = proto.default_port().to_string();
    let half = |a: El, b: El| row().w_full().gap(10.0).child(a.grow(1.0).w(0.0)).child(b.grow(1.0).w(0.0));
    let host_hint = match f.provider {
        Provider::Nextcloud => "cloud.exemplo.com ou https://cloud.exemplo.com",
        Provider::OwnCloud => "owncloud.exemplo.com ou https://owncloud.exemplo.com",
        Provider::Sftp => "nas.local, 192.168.0.10 ou sftp://usuario@host",
        Provider::Ftp => "ftp.exemplo.com, 192.168.0.10 ou ftp://usuario@host",
        Provider::WebDav | Provider::GoogleDrive => "dav.exemplo.com, 192.168.0.10 ou https://host/dav",
    };
    let folder_hint = match f.provider {
        Provider::Nextcloud | Provider::OwnCloud => "/remote.php/webdav (automático, se vazio)",
        Provider::WebDav => "/dav ou /remote.php/dav/files/usuario (raiz /, se vazio)",
        Provider::Sftp => "a pasta pessoal, se vazio",
        Provider::Ftp | Provider::GoogleDrive => "a pasta do login, se vazio",
    };
    let name_hint = match f.provider {
        Provider::Nextcloud => "Nextcloud (servidor), se vazio",
        Provider::OwnCloud => "ownCloud (servidor), se vazio",
        _ => "o endereço do servidor, se vazio",
    };
    let user_hint = if matches!(proto, Protocol::Ftp | Protocol::Ftps) { "anonymous, se vazio" } else { "" };

    let mut inner = col()
        .w_full()
        .px(16.0)
        .pb(4.0)
        .gap(10.0)
        .child(banner)
        .child(
            row()
                .w_full()
                .gap(10.0)
                .child(field("host", "Servidor", &f.host, host_hint, |f| &mut f.host).grow(3.0).w(0.0))
                .child(field("port", "Porta", &f.port, &port_hint, |f| &mut f.port).grow(1.0).w(0.0)),
        )
        .child(half(field("user", "Usuário", &f.user, user_hint, |f| &mut f.user), password_field));

    // Security toggle checkbox (inspired by the reference's "Usar SSL/TLS").
    match f.provider {
        Provider::Ftp => {
            inner = inner.child(
                rune_kit::Checkbox::new(proto == Protocol::Ftps)
                    .label("Usar SSL/TLS (FTPS)")
                    .description("Criptografa login e arquivos via TLS (porta 21 ou 990).")
                    .on_toggle(crate::servers::toggle_tls)
                    .build(k)
                    .key("tls-toggle"),
            );
        }
        Provider::WebDav | Provider::Nextcloud | Provider::OwnCloud => {
            inner = inner.child(
                rune_kit::Checkbox::new(proto == Protocol::Davs)
                    .label("Usar conexão segura SSL/TLS (HTTPS)")
                    .description("Mantém senha e arquivos criptografados na rede.")
                    .on_toggle(crate::servers::toggle_tls)
                    .build(k)
                    .key("tls-toggle"),
            );
        }
        Provider::Sftp | Provider::GoogleDrive => {}
    }

    inner = inner.child(field("folder", "Pasta inicial", &f.folder, folder_hint, |f| &mut f.folder));

    if proto == Protocol::Sftp {
        inner = inner.child(field("key", "Chave SSH (opcional)", &f.key, "C:\\Users\\voce\\.ssh\\id_ed25519 (a senha vira a da chave)", |f| &mut f.key));
        inner = inner.child(muted("Sem senha nem chave, as chaves de ~/.ssh são tentadas. Na primeira conexão a chave do servidor é guardada.".into()));
    } else if matches!(f.provider, Provider::Nextcloud | Provider::OwnCloud) {
        inner = inner.child(muted("Dica: se a sua conta usa verificação em duas etapas (2FA), gere uma senha de aplicativo nas configurações de segurança do servidor.".into()));
    }

    let connections_hint = format!("{} (de 1 a {})", openexplorer_remote::USUAL_CONNECTIONS, openexplorer_remote::MOST_CONNECTIONS);
    inner = inner.child(half(
        field("name", "Nome na árvore (opcional)", &f.name, name_hint, |f| &mut f.name),
        field("connections", "Conexões simultâneas", &f.connections, &connections_hint, |f| &mut f.connections),
    ));

    let active_fp = f.active_trusted();
    if let (Some(fp), None) = (active_fp, &f.untrusted) {
        inner = inner.child(muted(format!("Certificado confiado (SHA-256): {}", fingerprint_lines(fp))));
    }
    if proto.plain() {
        let secure_alt = if proto == Protocol::Ftp { "Usar SSL/TLS (FTPS) ou SFTP" } else { "HTTPS (SSL/TLS)" };
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
                    text(format!("{} não usa criptografia: senha e arquivos passam abertos pela rede. Prefira {secure_alt} se o servidor aceitar.", proto.label()))
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
    // A certificate the usual authorities don't vouch for: shown, and trusted only on a click.
    if let Some(r) = &f.untrusted {
        let changed = active_fp.is_some_and(|old| old != r.fingerprint);
        let lead = if changed {
            format!("O certificado do servidor mudou desde que você confiou nele: o novo {}. Se você não trocou o certificado do servidor, alguém pode estar se passando por ele.", r.why)
        } else {
            format!("O certificado do servidor {}. Isso é comum num NAS ou servidor de casa; confira a impressão digital com a que o servidor mostra antes de confiar.", r.why)
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
    let save_label = if f.editing.is_some() { "Salvar" } else { "Conectar" };
    let save_el = match (&f.untrusted, checking) {
        (_, true) => Button::new("Conectando…").variant(ButtonVariant::Submit).disabled(true).build(k),
        (Some(_), false) => Button::new("Confiar e salvar").variant(ButtonVariant::Danger).on_press(crate::servers::trust).build(k).default_button(),
        (None, false) => Button::new(save_label).variant(ButtonVariant::Submit).on_press(crate::servers::submit).build(k).default_button(),
    };
    let mut actions = row().key("actions").w_full().px(16.0).pt(12.0).pb(16.0).gap(8.0).center_y();
    if f.editing.is_none() {
        actions = actions.child(Button::new("Voltar").icon(&icons::BACK).variant(ButtonVariant::Neutral).disabled(checking).on_press(crate::servers::back_to_pick).build(k));
    }
    actions = actions
        .child(el().grow(1.0))
        .child(Button::new("Cancelar").variant(ButtonVariant::Neutral).on_press(close).build(k))
        .child(save_el);

    let body = col()
        .key("content")
        .w_full()
        .child(account_scroll(k, "server-scroll", window_h, inner))
        .child(actions);

    let title = if f.editing.is_some() {
        format!("Editar {}", f.provider.label())
    } else {
        format!("Credenciais {}", f.provider.label())
    };
    Modal::new("server", title).open(true).child(body).width(560.0).fit().on_close(close).build(k, layer);
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
        (loc @ Location::Server { server, path, .. }, single) => match single {
            Some(e) => format!("{}{}", server.address(), crate::servers::path_of(loc, e).unwrap_or_default()),
            None => format!("{}{}", server.address(), server.path(path)),
        },
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
    let server = match &s.panes[p].loc {
        Location::Server { server, .. } => Some((server.id.clone(), server.name.clone())),
        _ => None,
    };
    let connecting = s.login.is_some();
    let sep = || el().w(1.0).h(20.0).mx(4.0).fill(k.c.border_weak);
    let add_label = if connecting { "Adicionar conta · Conectando…" } else { "Adicionar conta" };
    let bar = row()
        .key("toolbar")
        .w_full()
        .h(40.0)
        .px(6.0)
        .gap(2.0)
        .center_y()
        .fill(k.c.bg_base)
        .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_weak)
        .child(
            tool(k, "add-account", &icons::ADD_ACCOUNT, add_label, true, |s: &mut State, _| crate::servers::ask_add(s))
                .help("Conectar Google Drive, Nextcloud, ownCloud, SFTP, FTP ou WebDAV"),
        );
    let bar = match (account, server) {
        (Some(email), _) => {
            let short: String = if email.chars().count() > 26 {
                format!("{}…", email.chars().take(25).collect::<String>())
            } else {
                email.clone()
            };
            let who = email.clone();
            bar.child(
                tool(k, "disconnect", &icons::SIGN_OUT, &format!("Desconectar {short}"), true, move |s: &mut State, _| disconnect(s, &who))
                    .help(format!("Desconectar {email}: remove o acesso e o cache desta conta")),
            )
        }
        (None, Some((id, name))) => {
            let edit = id.clone();
            bar.child(tool(k, "edit-server", &icons::M_RENAME, "Editar conta", true, move |s: &mut State, _| crate::servers::ask_edit(s, &edit)).help(format!("Muda o endereço, o login ou a pasta de {name}")))
                .child(tool(k, "remove-server", &icons::SIGN_OUT, "Remover conta", true, move |s: &mut State, _| crate::servers::remove(s, &id)).help(format!("Tira {name} da lista e esquece a senha dele")))
        }
        (None, None) => bar,
    };
    bar.child(sep())
        .child(tool(k, "reload-all", &icons::REFRESH, "Atualizar", true, |s: &mut State, _| load(s, s.active)))
        .child(el().grow(1.0))
        .child(tool(k, "options", &icons::SETTINGS, "Opções", true, |s: &mut State, _| {
            s.dialog = Some(Dialog::Options(Box::new(crate::settings::OptionsView::open(crate::settings::OptionsTab::General))));
        }))
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
        Location::Server { server, path, .. } => path.last().unwrap_or(&server.name).clone(),
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
    if let Location::Server { server, path, find: Some(words) } = &s.panes[p].loc {
        let text_ = words;
        let pane = &s.panes[p];
        let n = pane.entries.len();
        let results = format!("{} {}", ui_count(n), if n == 1 { "resultado" } else { "resultados" });
        let place = path.last().unwrap_or(&server.name);
        let said = match (pane.loading, n) {
            (true, 0) => format!("“{text_}” em {place} · Pesquisando…"),
            (true, _) => format!("“{text_}” em {place} · Pesquisando… {results} até agora"),
            _ => format!("“{text_}” em {place} e nas pastas dentro · {results}"),
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
        Location::Server { path, find, .. } => !path.is_empty() || find.is_some(),
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
        .virtual_rows_version((s.tree.version, s.panes[p].moves, s.accounts.len(), s.servers.list.len()));
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
        .child(head(3, "h-kind", if pane.loc.finds() { "Pasta" } else { "Tipo" }, SortCol::Kind, false))
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
        let found = pane.loc.finds();
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
        let searching = actions::shows_results(&pane.loc);
        notice(if searching { "Pesquisando…" } else { "Carregando…" }.into(), k.c.text_muted)
    } else if let (0, Location::Drive { space: Space::Search(q), path, .. }) = (n, &pane.loc)
        && path.is_empty()
    {
        notice(format!("Nada com “{}” no nome em {}.", q.text, q.place), k.c.text_faint)
    } else if let (0, Location::Found(f)) = (n, &pane.loc) {
        notice(format!("Nada com “{}” no nome em {}.", f.text, f.place), k.c.text_faint)
    } else if let (0, Location::Server { server, path, find: Some(text) }) = (n, &pane.loc) {
        notice(format!("Nada com “{text}” no nome em {} nem nas pastas dentro.", path.last().unwrap_or(&server.name)), k.c.text_faint)
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
        // The rows in view below the header, which Page Up and Page Down move by. Only the
        // keys read it, so the view needn't run again.
        .on_layout_tx(move |s: &mut State, r: [f32; 4], tx: &mut Tx| {
            s.page_rows[p] = ((r[3] - HEADER_H) / ROW_H).floor().max(1.0) as usize;
            tx.live_only();
        })
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
