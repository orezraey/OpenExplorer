//! ReNamer-inspired UI for the "Regras (Avançado)" tab: rule stack, preview table with Estado /
//! Nome / Novo Nome / Erro, Adicionar/Editar Regra modal (all 18 rules), Meta Tags picker,
//! Presets manager, and step-by-step Analyzer.

use std::borrow::Cow;
use std::sync::Arc;

use openexplorer_i18n::{t, tr};
use rune::{Anim, Color, Cursor, El, GrowthSpec, ScrollSpec, Spring, TextEdit, Tx, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, Dialog as Modal, MenuItem, ScrollBar, TextArea, TextInput, context_menu};

use crate::batch_rename::{self, BatchRename, Preview, RowPreview};
use crate::icons;
use crate::model::format_bytes;
use crate::rules_meta::{self, META_TAGS, TagCategory};
use crate::rules_rename::{
    self, CYRILLIC_TRANSLIT, CaseMode, DATE_FORMATS, DEFAULT_LATIN_TRANSLIT, DeleteFrom, DeleteUntil, InsertWhere, MetaTagTarget, NumSystem, Occurrences,
    RearrangeSplit, RuleConfig, RuleKind, RulePreset, RulesPopup, Side, StripWhere, TimeUnit, TranslitDir, UserInputMode,
};
use crate::rules_script::{self, SCRIPT_PRESETS};
use crate::{State, ui};

// ---------------------------------------------------------------------------------------------
// Small Reusable Controls (ReNamer-precise, instant response, dark/light aware)

pub(crate) fn instant_checkbox(k: &rune_kit::Kit, accent: Color, key: String, checked: bool, label: &str, on_press: impl Fn(&mut State) + 'static) -> El {
    let mut box_el = row()
        .size(15.0, 15.0)
        .center()
        .radius(3.0)
        .instant_changes()
        .hover_motion(Anim::INSTANT, Anim::INSTANT)
        .transition_motion(Anim::INSTANT, Anim::INSTANT);
    if checked {
        box_el = box_el
            .fill(accent)
            .border(1.5, accent)
            .child(icon(k.glyph.check, 10.0).color(Color::WHITE).instant_changes().transition_motion(Anim::INSTANT, Anim::INSTANT));
    } else {
        let off_fill = if k.dark { k.alpha(Color::WHITE, 0.06) } else { k.c.bg_base };
        let off_border = if k.dark { k.alpha(Color::WHITE, 0.38) } else { k.c.border_strong };
        box_el = box_el.fill(off_fill).border(1.5, off_border);
    }
    let mut r = row()
        .key(key)
        .role("checkbox")
        .shrink()
        .min_w(0.0)
        .gap(7.0)
        .py(1.5)
        .center_y()
        .instant_changes()
        .hover_motion(Anim::INSTANT, Anim::INSTANT)
        .transition_motion(Anim::INSTANT, Anim::INSTANT)
        .cursor(Cursor::Pointer)
        .on_press(on_press)
        .child(box_el);
    if !label.is_empty() {
        r = r.child(
            text(label.to_string())
                .font(k.ty.text_small)
                .color(k.c.text_base)
                .shrink()
                .min_w(0.0)
                .wrap(),
        );
    }
    r
}

fn radio_btn(k: &rune_kit::Kit, accent: Color, key: String, selected: bool, label: &str, on_press: impl Fn(&mut State) + 'static) -> El {
    let border_col = if selected {
        accent
    } else if k.dark {
        k.alpha(Color::WHITE, 0.38)
    } else {
        k.c.border_strong
    };
    let mut circle = row()
        .size(15.0, 15.0)
        .center()
        .radius(7.5)
        .instant_changes()
        .hover_motion(Anim::INSTANT, Anim::INSTANT)
        .transition_motion(Anim::INSTANT, Anim::INSTANT)
        .border(1.5, border_col)
        .fill(if k.dark { k.alpha(Color::WHITE, 0.05) } else { k.c.bg_base });
    if selected {
        circle = circle.child(el().size(7.0, 7.0).radius(3.5).fill(accent));
    }
    let mut r = row()
        .key(key)
        .role("radio")
        .shrink()
        .min_w(0.0)
        .gap(7.0)
        .py(1.5)
        .center_y()
        .instant_changes()
        .hover_motion(Anim::INSTANT, Anim::INSTANT)
        .transition_motion(Anim::INSTANT, Anim::INSTANT)
        .cursor(Cursor::Pointer)
        .on_press(on_press)
        .child(circle);
    if !label.is_empty() {
        r = r.child(
            text(label.to_string())
                .font(k.ty.text_small)
                .color(k.c.text_base)
                .shrink()
                .min_w(0.0)
                .wrap(),
        );
    }
    r
}

pub(crate) fn field_colors(k: &rune_kit::Kit, invalid: bool) -> (Color, Color) {
    let fill = if k.dark { rune::oklch(0.155, 0.0, 0.0) } else { k.c.bg_base };
    let border = if invalid {
        k.c.danger.fg
    } else if k.dark {
        k.alpha(Color::WHITE, 0.22)
    } else {
        k.c.border_strong
    };
    (fill, border)
}

#[allow(clippy::too_many_arguments)]
fn num_stepper(
    k: &rune_kit::Kit,
    key: &'static str,
    slot: usize,
    val: i64,
    min: i64,
    max: i64,
    edit: &TextEdit,
    on_set: impl Fn(&mut State, i64) + Copy + 'static,
) -> El {
    let trimmed = edit.text.trim();
    let invalid = !trimmed.is_empty()
        && !(trimmed == "-" && min < 0)
        && trimmed.parse::<i64>().is_err();
    let (fill, border) = field_colors(k, invalid);
    let btn = |suffix: &'static str, glyph: &'static str, delta: i64| {
        row()
            .key(format!("{key}-{suffix}"))
            .role("button")
            .w(20.0)
            .h(14.0)
            .center()
            .cursor(Cursor::Pointer)
            .hover_fill(k.c.hover)
            .on_press(move |s: &mut State| {
                let e = ed_mut(s);
                let cur = e.nums[slot].text.trim().parse::<i64>().unwrap_or(val);
                let next = (cur + delta).clamp(min, max);
                e.nums[slot] = TextEdit::new(next.to_string());
                on_set(s, next);
            })
            .child(text(glyph).font(k.ty.caption()).color(k.c.text_muted))
    };
    row()
        .key(key)
        .h(28.0)
        .radius(k.radius.sm)
        .fill(fill)
        .border(1.0, border)
        .center_y()
        .child(
            row()
                .w(54.0)
                .center_y()
                .child(
                    TextInput::new(edit)
                        .plain()
                        .width(Some(54.0))
                        .height(26.0)
                        .invalid(invalid)
                        .label(key)
                        .on_edit(move |s: &mut State| &mut ed_mut(s).nums[slot])
                        .on_change(move |s: &mut State, _| {
                            if let Ok(parsed) = ed_mut(s).nums[slot].text.trim().parse::<i64>() {
                                on_set(s, parsed.clamp(min, max));
                            }
                        })
                        .build(k),
                ),
        )
        .child(
            col()
                .border_sides([0.0, 0.0, 0.0, 1.0], k.c.border_weak)
                .child(btn("up", "▲", 1))
                .child(el().w_full().h(1.0).fill(k.c.border_weak))
                .child(btn("dn", "▼", -1)),
        )
}

pub(crate) fn tool_btn(k: &rune_kit::Kit, key: &'static str, label: &str, enabled: bool, on_press: fn(&mut State)) -> El {
    let mut r = row()
        .key(key)
        .role("button")
        .h(28.0)
        .px(10.0)
        .gap(5.0)
        .center_y()
        .radius(k.radius.sm)
        .fill(k.c.surface_raised)
        .border(1.0, k.c.border_weak);
    if enabled {
        r = r
            .hover_fill(k.c.hover)
            .cursor(Cursor::Pointer)
            .on_press(on_press)
            .child(text(label.to_string()).font(k.ty.text_xsmall_medium).color(k.c.text_base));
    } else {
        r = r.child(text(label.to_string()).font(k.ty.text_xsmall_medium).color(k.c.text_faint));
    }
    r
}

fn meta_tag_btn(k: &rune_kit::Kit, target: MetaTagTarget) -> El {
    row()
        .key(format!("meta-btn-{target:?}"))
        .role("button")
        .h(28.0)
        .px(10.0)
        .gap(6.0)
        .center_y()
        .radius(k.radius.sm)
        .fill(k.c.surface_raised)
        .hover_fill(k.c.hover)
        .border(1.0, k.c.border_base)
        .cursor(Cursor::Pointer)
        .on_press(move |s: &mut State| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.editor.sync_edits_to_config();
                b.rules.tag_search = TextEdit::new("");
                b.rules.tag_selected = 0;
                b.rules.tag_cat_open = false;
                b.rules.editor.meta_target = Some(target);
            }
        })
        .child(icon(&icons::PLUS, 12.0).color(k.c.text_accent))
        .child(text(t("Inserir Meta Tag", "Insert Meta Tag")).font(k.ty.text_xsmall_medium).color(k.c.text_base))
}

fn table_col_grip(
    k: &rune_kit::Kit,
    key: String,
    edge: f32,
    height: f32,
    on_drag: impl Fn(&mut State, &mut rune::Drag) + 'static,
) -> El {
    el().key(key)
        .role("separator")
        .label(t(
            "Arraste para mudar a largura da coluna; clique duas vezes para voltar ao padrão",
            "Drag to change column width; double-click to reset default",
        ))
        .absolute(edge - 4.0, 0.0)
        .w(8.0)
        .h(height)
        .cursor(Cursor::ResizeColumn)
        .hover(k.alpha(k.c.border_focus, 0.35))
        .on_drag(move |s: &mut State, d: &mut rune::Drag, _: &mut Tx| on_drag(s, d))
}

fn close_popup(s: &mut State) {
    if let Some(b) = &mut s.batch_rename {
        b.rules.popup = None;
        b.rules.tag_cat_open = false;
    }
}

fn close_window(s: &mut State) {
    s.close_batch_rename();
}

// ---------------------------------------------------------------------------------------------
// Main "Regras (Avançado)" Workspace

#[allow(clippy::too_many_arguments)]
pub fn rules_workspace(
    _s: &State,
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
    let rs = &br.rules;
    let has_sel = rs.selected.is_some_and(|i| i < rs.rules.len());
    let can_up = rs.selected.is_some_and(|i| i > 0 && i < rs.rules.len());
    let can_dn = rs.selected.is_some_and(|i| i + 1 < rs.rules.len());

    // ReNamer-style top rule bar: + Adicionar | - Remover | ↑ Para Cima | ↓ Para Baixo | Predefinições
    let rule_toolbar = row()
        .key("rules-toolbar")
        .w_full()
        .gap(6.0)
        .center_y()
        .child(
            row()
                .key("btn-rule-add")
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
                    s.open_rule_editor(None);
                })
                .child(icon(&icons::PLUS, 12.0).color(k.c.text_accent))
                .child(text(t("Adicionar", "Add")).font(k.ty.text_xsmall_medium).color(k.c.text_accent)),
        )
        .child(tool_btn(k, "btn-rule-rem", t("− Remover", "− Remove"), has_sel, |s| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.remove_selected();
            }
        }))
        .child(tool_btn(k, "btn-rule-up", t("↑ Para Cima", "↑ Move Up"), can_up, |s| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.move_selected(-1);
            }
        }))
        .child(tool_btn(k, "btn-rule-dn", t("↓ Para Baixo", "↓ Move Down"), can_dn, |s| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.move_selected(1);
            }
        }))
        .child(el().grow(1.0))
        .child(tool_btn(k, "btn-rule-presets", t("Predefinições", "Presets"), true, |s| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.popup = Some(RulesPopup::Presets);
            }
        }));

    let all_rules_enabled = !rs.rules.is_empty() && rs.rules.iter().all(|r| r.enabled);
    let cols = rs.col_widths;
    let header_h = 28.0;
    let mut header_row = row()
        .key("rules-stack-header")
        .w_full()
        .h(header_h)
        .sticky_top(0.0)
        .fill(k.c.surface_raised)
        .border_sides([0.0, 0.0, 1.0, 0.0], panel_border)
        .child(
            row()
                .w(cols[0])
                .h_full()
                .pl(10.0)
                .pr(8.0)
                .gap(8.0)
                .center_y()
                .border_sides([0.0, 1.0, 0.0, 0.0], panel_border)
                .child(instant_checkbox(k, accent_bg, "rules-stack-toggle-all".into(), all_rules_enabled, "", move |s| {
                    if let Some(b) = &mut s.batch_rename {
                        b.rules.set_all_enabled(!all_rules_enabled);
                    }
                }))
                .child(text("#").font(k.ty.caption()).color(k.c.text_muted)),
        )
        .child(
            row()
                .w(cols[1])
                .h_full()
                .px(8.0)
                .center_y()
                .border_sides([0.0, 1.0, 0.0, 0.0], panel_border)
                .child(text(t("Regra", "Rule")).font(k.ty.text_xsmall_medium).color(k.c.text_muted).truncate()),
        )
        .child(
            row()
                .w(cols[2])
                .h_full()
                .px(8.0)
                .center_y()
                .border_sides([0.0, 1.0, 0.0, 0.0], panel_border)
                .child(text(t("Instrução", "Instruction")).font(k.ty.text_xsmall_medium).color(k.c.text_muted).truncate()),
        )
        .child(el().grow(1.0).min_w(78.0));

    for i in 0..3 {
        let edge: f32 = cols[..=i].iter().sum();
        header_row = header_row.child(table_col_grip(k, format!("rule-col-grip-{i}"), edge, header_h, move |s, d| {
            if let Some(b) = &mut s.batch_rename {
                let _ = ui::resize(
                    d,
                    d.dx,
                    &mut b.rules.col_widths[i],
                    rules_rename::DEFAULT_RULE_COLS[i],
                    |v| rules_rename::clamp_rule_col(i, v),
                );
            }
        }));
    }

    // Rule Stack area ("Clique aqui para adicionar regra" when empty)
    let stack_rows: El = if rs.rules.is_empty() {
        row()
            .key("rules-empty-click")
            .role("button")
            .w_full()
            .h(112.0)
            .center()
            .cursor(Cursor::Pointer)
            .hover_fill(k.c.hover)
            .on_press(|s: &mut State| {
                s.open_rule_editor(None);
            })
            .on_secondary_press(|s: &mut State, at: rune::PointerPress, _: &mut Tx| {
                if let Some(b) = &mut s.batch_rename {
                    b.rules.stack_menu = Some((None, at));
                }
            })
            .child(text(t("Clique aqui para adicionar regra", "Click here to add a rule")).font(k.ty.body_medium).color(k.c.text_muted))
    } else {
        let mut list = col().key("rules-stack-list").w_full();
        for (i, rule) in rs.rules.iter().enumerate() {
            let active = rs.all_selected || rs.selected == Some(i);
            let instr_el: El = if rule.comment.trim().is_empty() {
                text(rule.summary())
                    .font(k.ty.text_xsmall)
                    .color(if rule.enabled { k.c.text_muted } else { k.c.text_faint })
                    .w_full()
                    .min_w(0.0)
                    .truncate()
            } else {
                row()
                    .w_full()
                    .min_w(0.0)
                    .gap(8.0)
                    .center_y()
                    .child(
                        text(rule.summary())
                            .font(k.ty.text_xsmall)
                            .color(if rule.enabled { k.c.text_muted } else { k.c.text_faint })
                            .shrink()
                            .min_w(0.0)
                            .truncate(),
                    )
                    .child(
                        text(format!("// {}", rule.comment.trim()))
                            .font(k.ty.caption())
                            .color(k.c.text_accent)
                            .truncate(),
                    )
            };
            let mut r = row()
                .key(format!("rule-row-{i}"))
                .w_full()
                .h(30.0)
                .center_y()
                .cursor(Cursor::Pointer)
                .fill(if active { k.alpha(k.c.text_accent, if k.dark { 0.16 } else { 0.09 }) } else { Color::TRANSPARENT })
                .hover_fill(k.c.hover)
                .on_press(move |s: &mut State| {
                    let double = s.batch_rename.as_ref().is_some_and(|b| !b.rules.all_selected && b.rules.selected == Some(i));
                    if double {
                        s.open_rule_editor(Some(i));
                    } else if let Some(b) = &mut s.batch_rename {
                        b.rules.selected = Some(i);
                        b.rules.all_selected = false;
                    }
                })
                .on_secondary_press(move |s: &mut State, at: rune::PointerPress, _: &mut Tx| {
                    if let Some(b) = &mut s.batch_rename {
                        b.rules.selected = Some(i);
                        b.rules.all_selected = false;
                        b.rules.stack_menu = Some((Some(i), at));
                    }
                })
                .child(
                    row()
                        .w(cols[0])
                        .h_full()
                        .pl(10.0)
                        .pr(8.0)
                        .gap(8.0)
                        .center_y()
                        .border_sides([0.0, 1.0, 0.0, 0.0], panel_border)
                        .child(instant_checkbox(k, accent_bg, format!("rule-en-{i}"), rule.enabled, "", move |s| {
                            if let Some(r) = s.batch_rename.as_mut().and_then(|b| b.rules.rules.get_mut(i)) {
                                r.enabled = !r.enabled;
                            }
                        }))
                        .child(text(format!("{}", i + 1)).font(k.ty.caption()).color(k.c.text_faint).truncate()),
                )
                .child(
                    row()
                        .w(cols[1])
                        .h_full()
                        .px(8.0)
                        .center_y()
                        .border_sides([0.0, 1.0, 0.0, 0.0], panel_border)
                        .child(
                            text(rule.config.kind().label())
                                .font(k.ty.text_xsmall_medium)
                                .color(if rule.enabled { k.c.text_base } else { k.c.text_faint })
                                .w_full()
                                .min_w(0.0)
                                .truncate(),
                        ),
                )
                .child(
                    row()
                        .w(cols[2])
                        .h_full()
                        .px(8.0)
                        .center_y()
                        .border_sides([0.0, 1.0, 0.0, 0.0], panel_border)
                        .child(instr_el),
                )
                .child(
                    row()
                        .grow(1.0)
                        .min_w(78.0)
                        .pr(16.0)
                        .gap(4.0)
                        .justify_end()
                        .center_y()
                        .child(
                            row()
                                .key(format!("rule-edit-{i}"))
                                .role("button")
                                .px(7.0)
                                .py(2.0)
                                .radius(4.0)
                                .hover_fill(k.alpha(k.c.text_base, 0.08))
                                .cursor(Cursor::Pointer)
                                .on_press(move |s: &mut State| {
                                    s.open_rule_editor(Some(i));
                                })
                                .child(text(t("Editar", "Edit")).font(k.ty.caption()).color(k.c.text_accent)),
                        )
                        .child(
                            row()
                                .key(format!("rule-del-{i}"))
                                .role("button")
                                .px(6.0)
                                .py(2.0)
                                .radius(4.0)
                                .hover_fill(k.c.danger.bg)
                                .cursor(Cursor::Pointer)
                                .on_press(move |s: &mut State| {
                                    if let Some(b) = &mut s.batch_rename
                                        && i < b.rules.rules.len()
                                    {
                                        b.rules.rules.remove(i);
                                        b.rules.selected = if b.rules.rules.is_empty() { None } else { Some(i.min(b.rules.rules.len() - 1)) };
                                        b.rules.all_selected = false;
                                    }
                                })
                                .child(text("×").font(k.ty.text_small).color(k.c.text_muted)),
                        ),
                );
            if i + 1 < rs.rules.len() {
                r = r.border_sides([0.0, 0.0, 1.0, 0.0], panel_border);
            }
            list = list.child(r);
        }
        list
    };

    let stack_menu_el = render_stack_context_menu(k, br, layer);

    let stack_table = col()
        .key("rules-stack-table")
        .w_full()
        .child(header_row)
        .child(stack_rows);

    let stack_viewport = col()
        .key("rules-stack-scroll")
        .w_full()
        .h(140.0)
        .min_h(0.0)
        .on_secondary_press(|s: &mut State, at: rune::PointerPress, _: &mut Tx| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.stack_menu = Some((None, at));
            }
        })
        .scroll_xy()
        .child(stack_table)
        .scroll_bar_styled(ui::bar_style(k).track_offset(header_h, 0.0));

    let stack_panel = col()
        .key("rules-stack-panel")
        .w_full()
        .radius(k.radius.md)
        .fill(panel_bg)
        .border(1.0, panel_border)
        .clip_soft(0.0)
        .child(stack_viewport)
        .child(stack_menu_el);

    let util_bar = shared_util_bar(
        k,
        br.items.len(),
        "btn-rules-analyze",
        !br.items.is_empty(),
        |s| {
            if let Some(b) = &mut s.batch_rename {
                let sample = b.items.first().map(|e| crate::model::shown(&e.name)).unwrap_or_default();
                b.rules.analyze_input = TextEdit::new(sample);
                b.rules.popup = Some(RulesPopup::Analyze);
            }
        },
        "btn-rules-export",
        "btn-rules-clear",
        t("Limpar regras", "Clear rules"),
        !rs.rules.is_empty(),
        |s| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.clear_all();
            }
        },
    );
    let preview_panel = shared_preview_panel(k, br, preview, all_checked, panel_bg, panel_border, accent_bg);
    let footer = shared_footer(k, br, preview, preview.can_submit(), None);

    if let Some(popup) = rs.popup {
        match popup {
            RulesPopup::Presets => render_presets_modal(k, br, panel_border, layer),
            RulesPopup::Analyze => render_analyze_modal(k, br, panel_border, layer),
            RulesPopup::Comment(idx) => render_comment_modal(k, br, idx, layer),
        }
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
        .child(rule_toolbar)
        .child(stack_panel)
        .child(util_bar)
        .child(preview_panel)
        .child(footer)
}

pub(crate) fn copy_preview_to_clipboard(s: &mut State) {
    if let Some(b) = &s.batch_rename {
        let p = b.preview();
        let txt: String = p.rows.iter().map(|r| format!("{}\t{}", r.current, r.next)).collect::<Vec<_>>().join("\n");
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(txt);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn shared_util_bar(
    k: &rune_kit::Kit,
    item_count: usize,
    analyze_key: &'static str,
    can_analyze: bool,
    on_analyze: fn(&mut State),
    export_key: &'static str,
    clear_key: &'static str,
    clear_label: &str,
    can_clear: bool,
    on_clear: fn(&mut State),
) -> El {
    row()
        .key("shared-util-bar")
        .w_full()
        .gap(8.0)
        .center_y()
        .child(text(tr!("Arquivos ({})", "Files ({})", item_count)).font(k.ty.text_xsmall_medium).color(k.c.text_muted))
        .child(el().grow(1.0))
        .child(tool_btn(k, analyze_key, t("Analisar", "Analyze"), can_analyze, on_analyze))
        .child(tool_btn(k, export_key, t("Copiar prévia", "Copy preview"), item_count > 0, copy_preview_to_clipboard))
        .child(tool_btn(k, clear_key, clear_label, can_clear, on_clear))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn shared_preview_panel(
    k: &rune_kit::Kit,
    br: &BatchRename,
    preview: &Preview,
    all_checked: bool,
    panel_bg: Color,
    panel_border: Color,
    accent_bg: Color,
) -> El {
    let pcols = br.rules.preview_cols;
    let prev_hdr_h = 30.0;
    let mut table_header = row()
        .key("rules-preview-header")
        .w_full()
        .h(prev_hdr_h)
        .sticky_top(0.0)
        .fill(k.c.surface_raised)
        .border_sides([0.0, 0.0, 1.0, 0.0], panel_border)
        .child(
            row()
                .w(pcols[0])
                .h_full()
                .pl(10.0)
                .pr(8.0)
                .gap(8.0)
                .center_y()
                .border_sides([0.0, 1.0, 0.0, 0.0], panel_border)
                .child(instant_checkbox(k, accent_bg, "rules-toggle-all".into(), all_checked, "", move |s| {
                    if let Some(b) = &mut s.batch_rename {
                        let next = !all_checked;
                        b.checked.iter_mut().for_each(|c| *c = next);
                    }
                }))
                .child(text(t("Estado", "State")).font(k.ty.caption()).color(k.c.text_muted).truncate()),
        )
        .child(
            row()
                .w(pcols[1])
                .h_full()
                .px(8.0)
                .center_y()
                .border_sides([0.0, 1.0, 0.0, 0.0], panel_border)
                .child(text(t("Nome", "Name")).font(k.ty.text_xsmall_medium).color(k.c.text_muted).truncate()),
        )
        .child(
            row()
                .w(pcols[2])
                .h_full()
                .px(8.0)
                .center_y()
                .border_sides([0.0, 1.0, 0.0, 0.0], panel_border)
                .child(text(t("Novo Nome", "New Name")).font(k.ty.text_xsmall_medium).color(k.c.text_muted).truncate()),
        )
        .child(
            row()
                .w(pcols[3])
                .h_full()
                .px(8.0)
                .center_y()
                .border_sides([0.0, 1.0, 0.0, 0.0], panel_border)
                .child(text(t("Erro", "Status")).font(k.ty.caption()).color(k.c.text_muted).truncate()),
        )
        .child(el().grow(1.0));

    for i in 0..4 {
        let edge: f32 = pcols[..=i].iter().sum();
        table_header = table_header.child(table_col_grip(k, format!("rule-prev-grip-{i}"), edge, prev_hdr_h, move |s, d| {
            if let Some(b) = &mut s.batch_rename {
                let _ = ui::resize(
                    d,
                    d.dx,
                    &mut b.rules.preview_cols[i],
                    rules_rename::DEFAULT_PREVIEW_COLS[i],
                    |v| rules_rename::clamp_preview_col(i, v),
                );
            }
        }));
    }

    // Only the rows in view are built: a batch can hold tens of thousands of files, and the
    // table is drawn again at every change (building them all took 3 s for 10,000).
    let total = br.items.len();
    let has_regex_err = preview.regex_error.is_some();
    let (kit, rows) = (k.clone(), preview.rows.clone());
    let rows_col = col()
        .key("rules-preview-rows")
        .w_full()
        .growth(GrowthSpec::default())
        .virtual_rows(
            total,
            PREVIEW_ROW_H,
            |s: &State, i| match s.batch_rename.as_ref().and_then(|b| b.items.get(i)) {
                Some(item) => Cow::Owned(preview_row_key(&item.id)),
                None => Cow::Owned(format!("rules-prev-{i}")),
            },
            move |s: &State, i| {
                // Rows are built after the view returns: for the batch these rows preview, or
                // there's nothing to draw.
                let (Some(br), Some(rp)) = (&s.batch_rename, rows.get(i)) else { return el() };
                let Some(item) = br.items.get(i) else { return el() };
                let on = br.checked.get(i).copied().unwrap_or(false);
                preview_row(&kit, rp, (i, &item.id), on, i + 1 < total, has_regex_err, pcols, panel_border, accent_bg)
            },
        )
        // The items of a batch never change: their keys are read once.
        .virtual_rows_version(br.serial);

    let preview_table = col()
        .key("rules-preview-table")
        .w_full()
        .child(table_header)
        .child(rows_col);

    col()
        .key("rules-preview-panel")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .radius(k.radius.md)
        .fill(panel_bg)
        .border(1.0, panel_border)
        .clip_soft(0.0)
        .child(
            col()
                .key("rules-preview-scroll")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .scroll_xy()
                .child(preview_table)
                .scroll_bar_styled(ui::bar_style(k).track_offset(prev_hdr_h, 0.0)),
        )
}

/// The key of the preview table's row for the item of this id: no other key of the window
/// starts as it does.
pub(crate) fn preview_row_key(id: &str) -> String {
    format!("prev:{id}")
}

/// How tall a row of the preview table is.
const PREVIEW_ROW_H: f32 = 28.0;

/// One row of the preview table: the item's checkbox, its name, its new name and what stops
/// it. `item` is where the item sits in the batch and its id; `divider` draws the line under it.
#[allow(clippy::too_many_arguments)]
fn preview_row(k: &rune_kit::Kit, rp: &RowPreview, item: (usize, &str), on: bool, divider: bool, has_regex_err: bool, pcols: [f32; 4], panel_border: Color, accent_bg: Color) -> El {
    let row_err: Option<&str> = rp.error.as_deref().or_else(|| if on && has_regex_err { Some(t("Regex inválida", "Invalid regex")) } else { None });
    let has_err = row_err.is_some();
    let is_changed = on && rp.changed;
    let arrow_col = if has_err {
        k.c.danger.fg
    } else if is_changed {
        k.c.success.fg
    } else {
        k.c.text_faint
    };
    let next_col = if has_err {
        k.c.danger.fg
    } else if is_changed {
        k.c.text_accent
    } else if on {
        k.c.text_base
    } else {
        k.c.text_faint
    };
    // A checked item left alone for a reason that isn't plain says it where an error would be.
    let status_label = row_err.or(rp.note.as_deref().filter(|_| on)).unwrap_or("OK");
    let status_col = if has_err { k.c.danger.fg } else { k.c.text_muted };
    let cell = |w: f32| row().w(w).h_full().px(8.0).center_y().border_sides([0.0, 1.0, 0.0, 0.0], panel_border);
    // The handler finds its item by id: a row can outlive the place it was built at.
    let (at, id) = (item.0, item.1.to_string());
    let toggle = move |s: &mut State| {
        let Some(b) = &mut s.batch_rename else { return };
        let here = b.items.get(at).is_some_and(|e| e.id == id);
        let found = if here { Some(at) } else { b.items.iter().position(|e| e.id == id) };
        if let Some(c) = found.and_then(|i| b.checked.get_mut(i)) {
            *c = !*c;
        }
    };
    let r = row()
        .w_full()
        .h(PREVIEW_ROW_H)
        .center_y()
        .hover_fill(k.c.hover)
        .child(
            row()
                .w(pcols[0])
                .h_full()
                .pl(10.0)
                .pr(8.0)
                .gap(8.0)
                .center_y()
                .border_sides([0.0, 1.0, 0.0, 0.0], panel_border)
                .child(instant_checkbox(k, accent_bg, format!("rules-chk-{}", item.1), on, "", toggle))
                .child(row().grow(1.0).center().child(text("→").font(k.ty.text_small_medium).color(arrow_col))),
        )
        .child(cell(pcols[1]).child(text(rp.current.clone()).font(k.ty.text_small).color(if on { k.c.text_base } else { k.c.text_faint }).w_full().min_w(0.0).truncate()))
        .child(cell(pcols[2]).child(text(rp.next.clone()).font(if is_changed { k.ty.text_small_medium } else { k.ty.text_small }).color(next_col).w_full().min_w(0.0).truncate()))
        .child(cell(pcols[3]).child(text(status_label.to_string()).font(k.ty.caption()).color(status_col).w_full().min_w(0.0).truncate()))
        .child(el().grow(1.0));
    if divider { r.border_sides([0.0, 0.0, 1.0, 0.0], panel_border) } else { r }
}

pub(crate) fn shared_footer(k: &rune_kit::Kit, br: &BatchRename, preview: &Preview, can_submit: bool, extra_error: Option<&str>) -> El {
    let total = br.items.len();
    let total_bytes: u64 = br.items.iter().filter_map(|e| e.size).sum();
    let count_text = if total_bytes > 0 {
        tr!("{} itens ({})", "{} items ({})", total, format_bytes(total_bytes))
    } else {
        tr!("{} itens", "{} items", total)
    };
    let submit_label = if preview.changed_count > 0 && can_submit {
        tr!("Renomear ({})", "Rename ({})", preview.changed_count)
    } else {
        t("Renomear", "Rename").into()
    };

    let warn_text: Option<String> = if let Some(err) = extra_error {
        Some(err.to_string())
    } else if preview.waiting {
        Some(t("Calculando meta tags…", "Computing meta tags…").into())
    } else if preview.error_count > 0 {
        Some(if preview.error_count == 1 {
            t("1 item com conflito ou nome inválido", "1 item has a conflict or invalid name").into()
        } else {
            tr!("{} itens com conflito ou nome inválido", "{} items have conflicts or invalid names", preview.error_count)
        })
    } else {
        None
    };

    let mut status_row = row().gap(8.0).center_y().grow(1.0).min_w(0.0).child(
        text(count_text).font(k.ty.text_xsmall).color(k.c.text_muted).shrink().min_w(0.0),
    );
    if let Some(w) = warn_text {
        status_row = status_row
            .child(text("·").font(k.ty.text_xsmall).color(k.c.text_faint))
            .child(text(w).font(k.ty.text_xsmall).color(k.c.danger.fg).grow(1.0).min_w(0.0).truncate());
    }

    row()
        .key("shared-footer")
        .w_full()
        .pt(2.0)
        .gap(8.0)
        .center_y()
        .child(status_row)
        .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press(close_window).build(k).cursor(Cursor::Pointer))
        .child(
            Button::new(submit_label)
                .variant(ButtonVariant::Submit)
                .disabled(!can_submit)
                .on_press(batch_rename::submit)
                .build(k)
                .default_button()
                .cursor(Cursor::Pointer),
        )
}

// ---------------------------------------------------------------------------------------------
// Independent Window: Adicionar / Editar Regra (resizable, instant switching)

pub fn editor_window_title(edit_index: Option<usize>) -> &'static str {
    if edit_index.is_some() {
        t("Editar Regra", "Edit Rule")
    } else {
        t("Adicionar Regra", "Add Rule")
    }
}

/// Renders the independent, resizable "Adicionar Regra" / "Editar Regra" window.
pub fn rule_editor_window_view(s: &State, cx: &mut rune::Cx<State>) -> El {
    if s.is_placeholder {
        return el();
    }
    // Its hover is instant, as the window's own (`hover_motion_within` below).
    let k = ui::window_kit(s, cx, Anim::INSTANT);
    cx.caption_color(k.c.text_base);

    let window_bg = if k.dark { rune::oklch(0.24, 0.0, 0.0) } else { k.c.page };
    let panel_bg = if k.dark { rune::oklch(0.198, 0.0, 0.0) } else { k.c.bg_base };
    let panel_border = if k.dark { rune::oklch(0.285, 0.0, 0.0) } else { k.c.border_weak };
    let accent = k.c.bg_icon_button_contrast;

    let default_br;
    let br = match &s.batch_rename {
        Some(br) => br.as_ref(),
        None => {
            default_br = BatchRename::new(crate::model::Location::Local(std::path::PathBuf::new()), Vec::new(), Vec::new());
            &default_br
        }
    };
    let ed = &br.rules.editor;
    let title = editor_window_title(ed.edit_index);

    let title_leading = row()
        .gap(8.0)
        .center_y()
        .child(icon(&icons::M_RENAME, 15.0).color(k.c.text_accent))
        .child(text(title).font(k.ty.text_small).color(k.c.text_base));

    let titlebar = rune_kit::WindowTitlebar::from_insets(cx.chrome_insets())
        .key("rule-editor-titlebar")
        .leading_padding(14.0)
        .fill(window_bg)
        .leading(title_leading)
        .build(&k);

    let mut layer: Vec<El> = Vec::new();

    // Left sidebar: 18 rules with right gutter so selection highlight never overlaps the scrollbar.
    let mut sidebar = col().key("rule-kinds-list").w_full().py(3.0).pl(3.0).pr(12.0).gap(1.0);
    for kind in RuleKind::ALL {
        let active = ed.kind == kind;
        let item = row()
            .key(format!("rk-{kind:?}"))
            .role("option")
            .w_full()
            .h(26.0)
            .px(10.0)
            .center_y()
            .radius(k.radius.sm)
            .instant_changes()
            .hover_motion(Anim::INSTANT, Anim::INSTANT)
            .transition_motion(Anim::INSTANT, Anim::INSTANT)
            .cursor(Cursor::Pointer)
            .fill(if active { accent } else { Color::TRANSPARENT })
            .hover_fill(if active { accent } else { k.c.hover })
            .on_press(move |s: &mut State| {
                if let Some(b) = &mut s.batch_rename {
                    b.rules.editor.switch_kind(kind);
                }
            })
            .child(
                text(kind.label())
                    .font(if active { k.ty.text_xsmall_medium } else { k.ty.text_xsmall })
                    .color(if active { Color::WHITE } else { k.c.text_base })
                    .truncate(),
            );
        sidebar = sidebar.child(item);
    }

    let left_col = col()
        .key("rule-kinds-scroll")
        .w(182.0)
        .h_full()
        .min_h(0.0)
        .radius(k.radius.sm)
        .fill(panel_bg)
        .border(1.0, panel_border)
        .scroll(ScrollSpec { stick_bottom: false })
        .child(sidebar)
        .scroll_bar_styled(ui::bar_style(&k));

    // Right configuration panel (keyed by rule kind + Spring::NONE so switching rules never morphs)
    let cfg_body = render_rule_config_panel(&k, br, accent, panel_border, &mut layer);
    let right_col = col()
        .key("rule-config-panel")
        .grow(1.0)
        .w(0.0)
        .h_full()
        .min_w(0.0)
        .min_h(0.0)
        .padding(12.0)
        .gap(8.0)
        .radius(k.radius.sm)
        .fill(panel_bg)
        .border(1.0, panel_border)
        .motion(Spring::NONE)
        .instant_changes()
        .child(text(t("Configuração:", "Configuration:")).font(k.ty.caption()).color(k.c.text_muted))
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec { stick_bottom: false })
                .child(cfg_body)
                .scroll_bar_styled(ui::bar_style(&k)),
        );

    let main_split = row()
        .key("rule-ed-split")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(10.0)
        .child(left_col)
        .child(right_col);

    let submit_label = if ed.edit_index.is_some() {
        t("Salvar Regra", "Save Rule")
    } else {
        t("+ Adicionar Regra", "+ Add Rule")
    };
    let close_ed = |s: &mut State| {
        s.close_rule_editor();
    };
    let actions = row()
        .key("rule-ed-actions")
        .w_full()
        .pt(2.0)
        .gap(8.0)
        .justify_end()
        .child(
            Button::new(submit_label)
                .variant(ButtonVariant::Submit)
                .on_press(|s: &mut State| {
                    if let Some(b) = &mut s.batch_rename {
                        b.rules.commit_editor();
                    }
                    s.close_rule_editor();
                    s.notify_batch_rename();
                })
                .build(&k)
                .default_button()
                .cursor(Cursor::Pointer),
        )
        .child(Button::new(t("Fechar", "Close")).variant(ButtonVariant::Neutral).on_press(close_ed).build(&k).cursor(Cursor::Pointer));

    if let Some(target) = ed.meta_target {
        render_meta_tags_modal(&k, br, target, panel_border, &mut layer);
    }

    let body = col()
        .key("rule-ed-body")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .px(16.0)
        .pt(6.0)
        .pb(14.0)
        .gap(10.0)
        .child(main_split)
        .child(actions);

    let root = col()
        .key("rule-editor-window")
        .w_full()
        .h_full()
        .fill(window_bg)
        .motion(Spring::NONE)
        .instant_changes()
        .hover_motion_within(Anim::INSTANT, Anim::INSTANT)
        .child(titlebar)
        .child(body);

    rune_kit::root(root, layer)
}

fn ed_mut(s: &mut State) -> &mut crate::rules_rename::RuleEditorState {
    match &mut s.batch_rename {
        Some(b) => &mut b.rules.editor,
        None => unreachable!(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn field_box(
    k: &rune_kit::Kit,
    label: &str,
    placeholder: &str,
    leading_icon: Option<&'static rune::Icon>,
    invalid: bool,
    edit: &TextEdit,
    on_edit: impl for<'a> Fn(&'a mut State) -> &'a mut TextEdit + 'static,
    on_submit: Option<fn(&mut State, &mut Tx)>,
) -> El {
    let (fill, border) = field_colors(k, invalid);
    let mut r = row()
        .w_full()
        .h(30.0)
        .center_y()
        .radius(k.radius.sm)
        .fill(fill)
        .border(1.0, border);
    if let Some(ic) = leading_icon {
        r = r.pl(9.0).child(icon(ic, 13.0).color(k.c.text_muted));
    }
    let mut input = TextInput::new(edit)
        .plain()
        .width(None)
        .height(28.0)
        .label(label)
        .placeholder(placeholder)
        .on_edit(on_edit);
    if let Some(sub) = on_submit {
        input = input.on_submit(sub);
    }
    r.child(input.build(k).grow(1.0).w(0.0).min_w(0.0))
}

fn text_box(
    k: &rune_kit::Kit,
    label: &str,
    placeholder: &str,
    edit: &TextEdit,
    on_edit: impl for<'a> Fn(&'a mut State) -> &'a mut TextEdit + 'static,
) -> El {
    let (fill, border) = field_colors(k, false);
    row()
        .w_full()
        .h(28.0)
        .center_y()
        .radius(k.radius.sm)
        .fill(fill)
        .border(1.0, border)
        .child(
            TextInput::new(edit)
                .plain()
                .width(None)
                .height(26.0)
                .label(label)
                .placeholder(placeholder)
                .on_edit(on_edit)
                .build(k)
                .grow(1.0)
                .w(0.0)
                .min_w(0.0),
        )
}

fn input_a(k: &rune_kit::Kit, label: &str, placeholder: &str, edit: &TextEdit) -> El {
    text_box(k, label, placeholder, edit, |s| &mut ed_mut(s).edit_a)
}

fn input_b(k: &rune_kit::Kit, label: &str, placeholder: &str, edit: &TextEdit) -> El {
    text_box(k, label, placeholder, edit, |s| &mut ed_mut(s).edit_b)
}

fn input_c(k: &rune_kit::Kit, label: &str, placeholder: &str, edit: &TextEdit) -> El {
    text_box(k, label, placeholder, edit, |s| &mut ed_mut(s).edit_c)
}

fn render_rule_config_panel(k: &rune_kit::Kit, br: &BatchRename, accent: Color, panel_border: Color, layer: &mut Vec<El>) -> El {
    let ed = &br.rules.editor;
    render_rule_config_panel_inner(k, br, accent, panel_border, layer)
        .key(format!("rule-cfg-{:?}", ed.kind))
        .w_full()
        .pr(12.0)
        .motion(Spring::NONE)
        .instant_changes()
        .transition_motion(Anim::INSTANT, Anim::INSTANT)
}

fn render_rule_config_panel_inner(k: &rune_kit::Kit, br: &BatchRename, accent: Color, panel_border: Color, layer: &mut Vec<El>) -> El {
    let ed = &br.rules.editor;
    let group_fill = if k.dark { rune::oklch(0.225, 0.0, 0.0) } else { k.c.surface_raised };
    let group_box = |title: &str, inner: El| {
        col()
            .w_full()
            .items_start()
            .padding(10.0)
            .gap(6.0)
            .radius(k.radius.sm)
            .fill(group_fill)
            .border(1.0, panel_border)
            .child(text(title.to_string()).font(k.ty.caption()).color(k.c.text_muted))
            .child(inner)
    };

    match &ed.config {
        RuleConfig::Insert { where_mode, position, rtl, skip_ext, .. } => {
            let (wm, pos, rtl_v, se) = (*where_mode, *position, *rtl, *skip_ext);
            col()
                .w_full()
                .gap(8.0)
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .center_y()
                        .child(text(t("Inserir:", "Insert:")).font(k.ty.text_small).color(k.c.text_base).w(68.0))
                        .child(input_a(k, t("Inserir", "Insert"), t("Texto ou :MetaTag:", "Text or :MetaTag:"), &ed.edit_a).grow(1.0).w(0.0).min_w(0.0)),
                )
                .child(row().w_full().pl(76.0).child(meta_tag_btn(k, MetaTagTarget::InsertText)))
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .child(text(t("Onde:", "Where:")).font(k.ty.text_small).color(k.c.text_base).w(68.0).pt(3.0))
                        .child(
                            col()
                                .grow(1.0)
                                .w(0.0)
                                .min_w(0.0)
                                .items_start()
                                .gap(6.0)
                                .child(radio_btn(k, accent, "ins-pre".into(), wm == InsertWhere::Prefix, t("Prefixo", "Prefix"), |s| {
                                    if let RuleConfig::Insert { where_mode, .. } = &mut ed_mut(s).config {
                                        *where_mode = InsertWhere::Prefix;
                                    }
                                }))
                                .child(radio_btn(k, accent, "ins-suf".into(), wm == InsertWhere::Suffix, t("Sufixo", "Suffix"), |s| {
                                    if let RuleConfig::Insert { where_mode, .. } = &mut ed_mut(s).config {
                                        *where_mode = InsertWhere::Suffix;
                                    }
                                }))
                                .child(
                                    row()
                                        .w_full()
                                        .gap(8.0)
                                        .center_y()
                                        .child(radio_btn(k, accent, "ins-pos".into(), wm == InsertWhere::Position, t("Posição:", "Position:"), |s| {
                                            if let RuleConfig::Insert { where_mode, .. } = &mut ed_mut(s).config {
                                                *where_mode = InsertWhere::Position;
                                            }
                                        }))
                                        .child(num_stepper(k, "ins-pos-num", 0, pos as i64, 1, 999_999, &ed.nums[0], |s, v| {
                                            if let RuleConfig::Insert { where_mode, position, .. } = &mut ed_mut(s).config {
                                                *where_mode = InsertWhere::Position;
                                                *position = v as usize;
                                            }
                                        }))
                                        .child(instant_checkbox(k, accent, "ins-rtl".into(), rtl_v, t("Direita-para-esquerda", "Right-to-left"), |s| {
                                            if let RuleConfig::Insert { rtl, .. } = &mut ed_mut(s).config {
                                                *rtl = !*rtl;
                                            }
                                        })),
                                )
                                .child(
                                    row()
                                        .w_full()
                                        .gap(8.0)
                                        .center_y()
                                        .child(row().w(116.0).center_y().child(radio_btn(k, accent, "ins-after".into(), wm == InsertWhere::AfterText, t("Após texto:", "After text:"), |s| {
                                            if let RuleConfig::Insert { where_mode, .. } = &mut ed_mut(s).config {
                                                *where_mode = InsertWhere::AfterText;
                                            }
                                        })))
                                        .child(input_b(k, t("Após", "After"), "", &ed.edit_b).grow(1.0).w(0.0).min_w(0.0)),
                                )
                                .child(
                                    row()
                                        .w_full()
                                        .gap(8.0)
                                        .center_y()
                                        .child(row().w(116.0).center_y().child(radio_btn(k, accent, "ins-before".into(), wm == InsertWhere::BeforeText, t("Antes do texto:", "Before text:"), |s| {
                                            if let RuleConfig::Insert { where_mode, .. } = &mut ed_mut(s).config {
                                                *where_mode = InsertWhere::BeforeText;
                                            }
                                        })))
                                        .child(input_c(k, t("Antes", "Before"), "", &ed.edit_c).grow(1.0).w(0.0).min_w(0.0)),
                                )
                                .child(radio_btn(k, accent, "ins-rep".into(), wm == InsertWhere::ReplaceCurrent, t("Substituir nome atual", "Replace current name"), |s| {
                                    if let RuleConfig::Insert { where_mode, .. } = &mut ed_mut(s).config {
                                        *where_mode = InsertWhere::ReplaceCurrent;
                                    }
                                }))
                                .child(instant_checkbox(k, accent, "ins-skipext".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                                    if let RuleConfig::Insert { skip_ext, .. } = &mut ed_mut(s).config {
                                        *skip_ext = !*skip_ext;
                                    }
                                })),
                        ),
                )
        }
        RuleConfig::Delete { from_mode, from_pos, until_mode, until_count, delete_current, skip_ext, rtl, keep_delimiters, .. } => {
            let (fm, fp, um, uc, dc, se, rtl_v, kd) = (*from_mode, *from_pos, *until_mode, *until_count, *delete_current, *skip_ext, *rtl, *keep_delimiters);
            let from_box = group_box(
                t("De:", "From:"),
                col()
                    .w_full()
                    .items_start()
                    .gap(6.0)
                    .child(
                        row()
                            .w_full()
                            .gap(8.0)
                            .center_y()
                            .child(row().w(92.0).center_y().child(radio_btn(k, accent, "del-fpos".into(), fm == DeleteFrom::Position, t("Posição:", "Position:"), |s| {
                                if let RuleConfig::Delete { from_mode, .. } = &mut ed_mut(s).config {
                                    *from_mode = DeleteFrom::Position;
                                }
                            })))
                            .child(num_stepper(k, "del-fp-n", 0, fp as i64, 1, 999_999, &ed.nums[0], |s, v| {
                                if let RuleConfig::Delete { from_mode, from_pos, .. } = &mut ed_mut(s).config {
                                    *from_mode = DeleteFrom::Position;
                                    *from_pos = v as usize;
                                }
                            })),
                    )
                    .child(
                        row()
                            .w_full()
                            .gap(8.0)
                            .center_y()
                            .child(row().w(92.0).center_y().child(radio_btn(k, accent, "del-fdel".into(), fm == DeleteFrom::Delimiter, t("Delimitador:", "Delimiter:"), |s| {
                                if let RuleConfig::Delete { from_mode, .. } = &mut ed_mut(s).config {
                                    *from_mode = DeleteFrom::Delimiter;
                                }
                            })))
                            .child(input_a(k, t("Delimitador", "Delimiter"), "", &ed.edit_a).grow(1.0).w(0.0).min_w(0.0)),
                    ),
            );
            let until_box = group_box(
                t("Até:", "Until:"),
                col()
                    .w_full()
                    .items_start()
                    .gap(6.0)
                    .child(
                        row()
                            .w_full()
                            .gap(8.0)
                            .center_y()
                            .child(row().w(92.0).center_y().child(radio_btn(k, accent, "del-ucnt".into(), um == DeleteUntil::Count, t("Contagem:", "Count:"), |s| {
                                if let RuleConfig::Delete { until_mode, .. } = &mut ed_mut(s).config {
                                    *until_mode = DeleteUntil::Count;
                                }
                            })))
                            .child(num_stepper(k, "del-uc-n", 1, uc as i64, 1, 999_999, &ed.nums[1], |s, v| {
                                if let RuleConfig::Delete { until_mode, until_count, .. } = &mut ed_mut(s).config {
                                    *until_mode = DeleteUntil::Count;
                                    *until_count = v as usize;
                                }
                            })),
                    )
                    .child(
                        row()
                            .w_full()
                            .gap(8.0)
                            .center_y()
                            .child(row().w(92.0).center_y().child(radio_btn(k, accent, "del-udel".into(), um == DeleteUntil::Delimiter, t("Delimitador:", "Delimiter:"), |s| {
                                if let RuleConfig::Delete { until_mode, .. } = &mut ed_mut(s).config {
                                    *until_mode = DeleteUntil::Delimiter;
                                }
                            })))
                            .child(input_b(k, t("Delimitador", "Delimiter"), "", &ed.edit_b).grow(1.0).w(0.0).min_w(0.0)),
                    )
                    .child(radio_btn(k, accent, "del-uend".into(), um == DeleteUntil::UntilEnd, t("Até o final", "Until the end"), |s| {
                        if let RuleConfig::Delete { until_mode, .. } = &mut ed_mut(s).config {
                            *until_mode = DeleteUntil::UntilEnd;
                        }
                    })),
            );
            col()
                .w_full()
                .items_start()
                .gap(10.0)
                .child(row().w_full().gap(10.0).child(from_box.grow(1.0).w(0.0)).child(until_box.grow(1.0).w(0.0)))
                .child(
                    row()
                        .w_full()
                        .gap(16.0)
                        .child(
                            col()
                                .grow(1.0)
                                .w(0.0)
                                .items_start()
                                .gap(6.0)
                                .child(instant_checkbox(k, accent, "del-cur".into(), dc, t("Excluir nome atual", "Delete current name"), |s| {
                                    if let RuleConfig::Delete { delete_current, .. } = &mut ed_mut(s).config {
                                        *delete_current = !*delete_current;
                                    }
                                }))
                                .child(instant_checkbox(k, accent, "del-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                                    if let RuleConfig::Delete { skip_ext, .. } = &mut ed_mut(s).config {
                                        *skip_ext = !*skip_ext;
                                    }
                                })),
                        )
                        .child(
                            col()
                                .grow(1.0)
                                .w(0.0)
                                .items_start()
                                .gap(6.0)
                                .child(instant_checkbox(k, accent, "del-rtl".into(), rtl_v, t("Direita-para-esquerda", "Right-to-left"), |s| {
                                    if let RuleConfig::Delete { rtl, .. } = &mut ed_mut(s).config {
                                        *rtl = !*rtl;
                                    }
                                }))
                                .child(instant_checkbox(k, accent, "del-kd".into(), kd, t("Não remover delimitadores", "Do not remove delimiters"), |s| {
                                    if let RuleConfig::Delete { keep_delimiters, .. } = &mut ed_mut(s).config {
                                        *keep_delimiters = !*keep_delimiters;
                                    }
                                })),
                        ),
                )
        }
        RuleConfig::Remove { occurrences, case_sensitive, whole_words, skip_ext, wildcards, .. } => {
            let (occ, cs, ww, se, wc) = (*occurrences, *case_sensitive, *whole_words, *skip_ext, *wildcards);
            col()
                .w_full()
                .items_start()
                .gap(10.0)
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .center_y()
                        .child(text(t("Remover:", "Remove:")).font(k.ty.text_small).color(k.c.text_base).w(72.0))
                        .child(input_a(k, t("Remover", "Remove"), "", &ed.edit_a).grow(1.0).w(0.0).min_w(0.0))
                        .child(meta_tag_btn(k, MetaTagTarget::RemoveText)),
                )
                .child(
                    row()
                        .w_full()
                        .gap(16.0)
                        .child(
                            col()
                                .w(140.0)
                                .items_start()
                                .gap(5.0)
                                .child(text(t("Ocorrências:", "Occurrences:")).font(k.ty.caption()).color(k.c.text_muted))
                                .child(radio_btn(k, accent, "rem-all".into(), occ == Occurrences::All, t("Tudo", "All"), |s| {
                                    if let RuleConfig::Remove { occurrences, .. } = &mut ed_mut(s).config {
                                        *occurrences = Occurrences::All;
                                    }
                                }))
                                .child(radio_btn(k, accent, "rem-first".into(), occ == Occurrences::First, t("Primeiro", "First"), |s| {
                                    if let RuleConfig::Remove { occurrences, .. } = &mut ed_mut(s).config {
                                        *occurrences = Occurrences::First;
                                    }
                                }))
                                .child(radio_btn(k, accent, "rem-last".into(), occ == Occurrences::Last, t("Último", "Last"), |s| {
                                    if let RuleConfig::Remove { occurrences, .. } = &mut ed_mut(s).config {
                                        *occurrences = Occurrences::Last;
                                    }
                                })),
                        )
                        .child(
                            col()
                                .grow(1.0)
                                .w(0.0)
                                .items_start()
                                .gap(6.0)
                                .pt(16.0)
                                .child(instant_checkbox(k, accent, "rem-cs".into(), cs, t("Diferenciar maiúsculas de minúsculas", "Case sensitive"), |s| {
                                    if let RuleConfig::Remove { case_sensitive, .. } = &mut ed_mut(s).config {
                                        *case_sensitive = !*case_sensitive;
                                    }
                                }))
                                .child(instant_checkbox(k, accent, "rem-ww".into(), ww, t("Somente palavras inteiras", "Whole words only"), |s| {
                                    if let RuleConfig::Remove { whole_words, .. } = &mut ed_mut(s).config {
                                        *whole_words = !*whole_words;
                                    }
                                }))
                                .child(instant_checkbox(k, accent, "rem-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                                    if let RuleConfig::Remove { skip_ext, .. } = &mut ed_mut(s).config {
                                        *skip_ext = !*skip_ext;
                                    }
                                })),
                        ),
                )
                .child(instant_checkbox(
                    k,
                    accent,
                    "rem-wc".into(),
                    wc,
                    t("Interpretar símbolos '?', '*', '[', ']' como curingas?", "Interpret '?', '*', '[', ']' as wildcards?"),
                    |s| {
                        if let RuleConfig::Remove { wildcards, .. } = &mut ed_mut(s).config {
                            *wildcards = !*wildcards;
                        }
                    },
                ))
        }
        RuleConfig::Replace { occurrences, case_sensitive, whole_words, skip_ext, wildcards, .. } => {
            let (occ, cs, ww, se, wc) = (*occurrences, *case_sensitive, *whole_words, *skip_ext, *wildcards);
            col()
                .w_full()
                .items_start()
                .gap(10.0)
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .center_y()
                        .child(text(t("Localizar:", "Find:")).font(k.ty.text_small).color(k.c.text_base).w(72.0))
                        .child(input_a(k, t("Localizar", "Find"), "", &ed.edit_a).grow(1.0).w(0.0).min_w(0.0)),
                )
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .center_y()
                        .child(text(t("Substituir:", "Replace:")).font(k.ty.text_small).color(k.c.text_base).w(72.0))
                        .child(input_b(k, t("Substituir", "Replace"), "", &ed.edit_b).grow(1.0).w(0.0).min_w(0.0))
                        .child(meta_tag_btn(k, MetaTagTarget::ReplaceWith)),
                )
                .child(
                    row()
                        .w_full()
                        .gap(16.0)
                        .child(
                            col()
                                .w(140.0)
                                .items_start()
                                .gap(5.0)
                                .child(text(t("Ocorrências:", "Occurrences:")).font(k.ty.caption()).color(k.c.text_muted))
                                .child(radio_btn(k, accent, "rep-all".into(), occ == Occurrences::All, t("Tudo", "All"), |s| {
                                    if let RuleConfig::Replace { occurrences, .. } = &mut ed_mut(s).config {
                                        *occurrences = Occurrences::All;
                                    }
                                }))
                                .child(radio_btn(k, accent, "rep-first".into(), occ == Occurrences::First, t("Primeiro", "First"), |s| {
                                    if let RuleConfig::Replace { occurrences, .. } = &mut ed_mut(s).config {
                                        *occurrences = Occurrences::First;
                                    }
                                }))
                                .child(radio_btn(k, accent, "rep-last".into(), occ == Occurrences::Last, t("Último", "Last"), |s| {
                                    if let RuleConfig::Replace { occurrences, .. } = &mut ed_mut(s).config {
                                        *occurrences = Occurrences::Last;
                                    }
                                })),
                        )
                        .child(
                            col()
                                .grow(1.0)
                                .w(0.0)
                                .items_start()
                                .gap(6.0)
                                .pt(16.0)
                                .child(instant_checkbox(k, accent, "rep-cs".into(), cs, t("Diferenciar maiúsculas de minúsculas", "Case sensitive"), |s| {
                                    if let RuleConfig::Replace { case_sensitive, .. } = &mut ed_mut(s).config {
                                        *case_sensitive = !*case_sensitive;
                                    }
                                }))
                                .child(instant_checkbox(k, accent, "rep-ww".into(), ww, t("Somente palavras inteiras", "Whole words only"), |s| {
                                    if let RuleConfig::Replace { whole_words, .. } = &mut ed_mut(s).config {
                                        *whole_words = !*whole_words;
                                    }
                                }))
                                .child(instant_checkbox(k, accent, "rep-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                                    if let RuleConfig::Replace { skip_ext, .. } = &mut ed_mut(s).config {
                                        *skip_ext = !*skip_ext;
                                    }
                                })),
                        ),
                )
                .child(instant_checkbox(
                    k,
                    accent,
                    "rep-wc".into(),
                    wc,
                    t("Interpretar '?', '*', '[', ']' como curingas e '$n' como referências?", "Interpret '?', '*', '[', ']' as wildcards and '$n' as backreferences?"),
                    |s| {
                        if let RuleConfig::Replace { wildcards, .. } = &mut ed_mut(s).config {
                            *wildcards = !*wildcards;
                        }
                    },
                ))
        }
        RuleConfig::Rearrange { split_mode, skip_ext, rtl, .. } => {
            let (sm, se, rtl_v) = (*split_mode, *skip_ext, *rtl);
            col()
                .w_full()
                .items_start()
                .gap(8.0)
                .child(radio_btn(k, accent, "rear-del".into(), sm == RearrangeSplit::Delimiters, t("Delimitadores (separe múltiplos com |)", "Delimiters (separate multiple with |)"), |s| {
                    if let RuleConfig::Rearrange { split_mode, .. } = &mut ed_mut(s).config {
                        *split_mode = RearrangeSplit::Delimiters;
                    }
                }))
                .child(radio_btn(k, accent, "rear-pos".into(), sm == RearrangeSplit::Positions, t("Posições (ex.: 4, 8)", "Positions (e.g. 4, 8)"), |s| {
                    if let RuleConfig::Rearrange { split_mode, .. } = &mut ed_mut(s).config {
                        *split_mode = RearrangeSplit::Positions;
                    }
                }))
                .child(radio_btn(k, accent, "rear-ex".into(), sm == RearrangeSplit::ExactPattern, t("Padrão exato de delimitadores", "Exact pattern of delimiters"), |s| {
                    if let RuleConfig::Rearrange { split_mode, .. } = &mut ed_mut(s).config {
                        *split_mode = RearrangeSplit::ExactPattern;
                    }
                }))
                .child(input_a(k, t("Delimitadores", "Delimiters"), " - ", &ed.edit_a))
                .child(text(t("Novo padrão:", "New pattern:")).font(k.ty.text_small).color(k.c.text_base))
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .child(input_b(k, t("Novo padrão", "New pattern"), "$2 - $1", &ed.edit_b).grow(1.0).w(0.0).min_w(0.0))
                        .child(meta_tag_btn(k, MetaTagTarget::RearrangePattern)),
                )
                .child(
                    text(t(
                        "Dica: Use $1..$N para referenciar partes delimitadas, $-1..$-N a partir do final, $0 para o nome original.",
                        "Hint: Use $1..$N to reference split parts, $-1..$-N from the end, $0 for the original name.",
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
                        .gap(16.0)
                        .child(instant_checkbox(k, accent, "rear-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                            if let RuleConfig::Rearrange { skip_ext, .. } = &mut ed_mut(s).config {
                                *skip_ext = !*skip_ext;
                            }
                        }))
                        .child(instant_checkbox(k, accent, "rear-rtl".into(), rtl_v, t("Direita-para-esquerda", "Right-to-left"), |s| {
                            if let RuleConfig::Rearrange { rtl, .. } = &mut ed_mut(s).config {
                                *rtl = !*rtl;
                            }
                        })),
                )
        }
        RuleConfig::Extension { set_new, append_original, detect_binary, remove_duplicates, dup_case_sensitive, .. } => {
            let (sn, ao, db, rd, dcs) = (*set_new, *append_original, *detect_binary, *remove_duplicates, *dup_case_sensitive);
            col()
                .w_full()
                .items_start()
                .gap(8.0)
                .child(instant_checkbox(k, accent, "ext-new".into(), sn, t("Nova Extensão (sem ponto):", "New Extension (without dot):"), |s| {
                    if let RuleConfig::Extension { set_new, .. } = &mut ed_mut(s).config {
                        *set_new = !*set_new;
                    }
                }))
                .child(input_a(k, t("Extensão", "Extension"), "mp4", &ed.edit_a))
                .child(instant_checkbox(k, accent, "ext-app".into(), ao, t("Anexar nome original do arquivo", "Append to the original filename"), |s| {
                    if let RuleConfig::Extension { append_original, .. } = &mut ed_mut(s).config {
                        *append_original = !*append_original;
                    }
                }))
                .child(instant_checkbox(k, accent, "ext-bin".into(), db, t("Detectar usando \"assinatura binária\"", "Detect using \"binary signature\""), |s| {
                    if let RuleConfig::Extension { detect_binary, .. } = &mut ed_mut(s).config {
                        *detect_binary = !*detect_binary;
                    }
                }))
                .child(
                    text(t(
                        "Nota: Alguns formatos compartilham assinatura (ex.: doc/ppt/xls ou zip/docx). Arquivos não reconhecidos permanecem inalterados.",
                        "Note: Some formats share signatures. Unrecognized files remain unchanged.",
                    ))
                    .font(k.ty.caption())
                    .color(k.c.text_muted)
                    .wrap()
                    .w_full()
                    .min_w(0.0),
                )
                .child(el().w_full().h(1.0).my(4.0).fill(panel_border))
                .child(instant_checkbox(k, accent, "ext-rdup".into(), rd, t("Remover extensões duplicadas", "Remove duplicate extensions"), |s| {
                    if let RuleConfig::Extension { remove_duplicates, .. } = &mut ed_mut(s).config {
                        *remove_duplicates = !*remove_duplicates;
                    }
                }))
                .child(
                    row()
                        .pl(20.0)
                        .child(instant_checkbox(k, accent, "ext-dcs".into(), dcs, t("Diferenciar maiúsculas de minúsculas", "Case sensitive"), |s| {
                            if let RuleConfig::Extension { dup_case_sensitive, .. } = &mut ed_mut(s).config {
                                *dup_case_sensitive = !*dup_case_sensitive;
                            }
                        })),
                )
        }
        RuleConfig::Strip { english, digits, symbols, brackets, custom, unicode_range, where_mode, invert, case_sensitive, skip_ext, .. } => {
            let (en, dg, sy, brk, cu, ur, wm, inv, cs, se) = (*english, *digits, *symbols, *brackets, *custom, *unicode_range, *where_mode, *invert, *case_sensitive, *skip_ext);
            let line = |chk_key: &'static str, on: bool, label: &'static str, edit: &TextEdit, toggle: fn(&mut State), pick: fn(&mut crate::rules_rename::RuleEditorState) -> &mut TextEdit| {
                row()
                    .w_full()
                    .gap(8.0)
                    .center_y()
                    .child(row().w(150.0).center_y().child(instant_checkbox(k, accent, chk_key.into(), on, label, toggle)))
                    .child(text_box(k, label, "", edit, move |s| pick(ed_mut(s))).grow(1.0).w(0.0).min_w(0.0))
            };
            col()
                .w_full()
                .items_start()
                .gap(6.0)
                .child(line("stp-en", en, t("Inglês:", "English:"), &ed.edit_a, |s| {
                    if let RuleConfig::Strip { english, .. } = &mut ed_mut(s).config { *english = !*english; }
                }, |e| &mut e.edit_a))
                .child(line("stp-dg", dg, t("Dígitos:", "Digits:"), &ed.edit_b, |s| {
                    if let RuleConfig::Strip { digits, .. } = &mut ed_mut(s).config { *digits = !*digits; }
                }, |e| &mut e.edit_b))
                .child(line("stp-sy", sy, t("Símbolos:", "Symbols:"), &ed.edit_c, |s| {
                    if let RuleConfig::Strip { symbols, .. } = &mut ed_mut(s).config { *symbols = !*symbols; }
                }, |e| &mut e.edit_c))
                .child(line("stp-bk", brk, t("Colchetes:", "Brackets:"), &ed.edit_d, |s| {
                    if let RuleConfig::Strip { brackets, .. } = &mut ed_mut(s).config { *brackets = !*brackets; }
                }, |e| &mut e.edit_d))
                .child(line("stp-cu", cu, t("Definido pelo usuário:", "User defined:"), &ed.edit_e, |s| {
                    if let RuleConfig::Strip { custom, .. } = &mut ed_mut(s).config { *custom = !*custom; }
                }, |e| &mut e.edit_e))
                .child(line("stp-ur", ur, t("Intervalo unicode:", "Unicode range:"), &ed.edit_f, |s| {
                    if let RuleConfig::Strip { unicode_range, .. } = &mut ed_mut(s).config { *unicode_range = !*unicode_range; }
                }, |e| &mut e.edit_f))
                .child(
                    row()
                        .w_full()
                        .pt(4.0)
                        .gap(16.0)
                        .child(
                            col()
                                .w(130.0)
                                .items_start()
                                .gap(4.0)
                                .child(radio_btn(k, accent, "stp-w-all".into(), wm == StripWhere::Everywhere, t("Em todo lugar", "Everywhere"), |s| {
                                    if let RuleConfig::Strip { where_mode, .. } = &mut ed_mut(s).config { *where_mode = StripWhere::Everywhere; }
                                }))
                                .child(radio_btn(k, accent, "stp-w-main".into(), wm == StripWhere::Main, t("Principal (Início)", "Leading"), |s| {
                                    if let RuleConfig::Strip { where_mode, .. } = &mut ed_mut(s).config { *where_mode = StripWhere::Main; }
                                }))
                                .child(radio_btn(k, accent, "stp-w-r".into(), wm == StripWhere::Right, t("À Direita (Fim)", "Trailing"), |s| {
                                    if let RuleConfig::Strip { where_mode, .. } = &mut ed_mut(s).config { *where_mode = StripWhere::Right; }
                                })),
                        )
                        .child(
                            col()
                                .grow(1.0)
                                .w(0.0)
                                .items_start()
                                .gap(4.0)
                                .child(instant_checkbox(k, accent, "stp-inv".into(), inv, t("Retira todos, menos os selecionados", "Strip all except selected"), |s| {
                                    if let RuleConfig::Strip { invert, .. } = &mut ed_mut(s).config { *invert = !*invert; }
                                }))
                                .child(instant_checkbox(k, accent, "stp-cs".into(), cs, t("Diferenciar maiúsculas de minúsculas", "Case sensitive"), |s| {
                                    if let RuleConfig::Strip { case_sensitive, .. } = &mut ed_mut(s).config { *case_sensitive = !*case_sensitive; }
                                }))
                                .child(instant_checkbox(k, accent, "stp-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                                    if let RuleConfig::Strip { skip_ext, .. } = &mut ed_mut(s).config { *skip_ext = !*skip_ext; }
                                })),
                        ),
                )
        }
        RuleConfig::Case { mode, preserve_case, skip_ext, force_fragments, ext_lower, ext_upper, .. } => {
            let (cm, pc, se, ff, ext_lo, eu) = (*mode, *preserve_case, *skip_ext, *force_fragments, *ext_lower, *ext_upper);
            let left = group_box(
                t("Alterar maiúsculo/minúsculo:", "Change case:"),
                col()
                    .w_full()
                    .items_start()
                    .gap(4.0)
                    .child(radio_btn(k, accent, "cs-title".into(), cm == CaseMode::TitleCase, t("Primeira Letra Da Palavra Em Maiúscula", "Capitalize Every Word"), |s| {
                        if let RuleConfig::Case { mode, .. } = &mut ed_mut(s).config { *mode = CaseMode::TitleCase; }
                    }))
                    .child(radio_btn(k, accent, "cs-low".into(), cm == CaseMode::Lower, t("tudo em minúsculo", "all lower case"), |s| {
                        if let RuleConfig::Case { mode, .. } = &mut ed_mut(s).config { *mode = CaseMode::Lower; }
                    }))
                    .child(radio_btn(k, accent, "cs-up".into(), cm == CaseMode::Upper, t("TUDO EM MAIÚSCULO", "ALL UPPER CASE"), |s| {
                        if let RuleConfig::Case { mode, .. } = &mut ed_mut(s).config { *mode = CaseMode::Upper; }
                    }))
                    .child(radio_btn(k, accent, "cs-inv".into(), cm == CaseMode::Invert, t("iNVERTER mAIÚSCULAS/mINÚSCULAS", "iNVERT cASE"), |s| {
                        if let RuleConfig::Case { mode, .. } = &mut ed_mut(s).config { *mode = CaseMode::Invert; }
                    }))
                    .child(radio_btn(k, accent, "cs-first".into(), cm == CaseMode::FirstLetter, t("Primeira letra maiúscula", "First letter capital"), |s| {
                        if let RuleConfig::Case { mode, .. } = &mut ed_mut(s).config { *mode = CaseMode::FirstLetter; }
                    }))
                    .child(radio_btn(k, accent, "cs-sent".into(), cm == CaseMode::Sentence, t("Iniciar sentença em maiúsculas", "Sentence case"), |s| {
                        if let RuleConfig::Case { mode, .. } = &mut ed_mut(s).config { *mode = CaseMode::Sentence; }
                    }))
                    .child(radio_btn(k, accent, "cs-none".into(), cm == CaseMode::None, t("(nenhum dos anteriores)", "(none of the above)"), |s| {
                        if let RuleConfig::Case { mode, .. } = &mut ed_mut(s).config { *mode = CaseMode::None; }
                    }))
                    .child(el().w_full().h(1.0).my(2.0).fill(panel_border))
                    .child(instant_checkbox(k, accent, "cs-pc".into(), pc, t("Preservar maiúsculas e minúsculas", "Preserve existing case"), |s| {
                        if let RuleConfig::Case { preserve_case, .. } = &mut ed_mut(s).config { *preserve_case = !*preserve_case; }
                    }))
                    .child(instant_checkbox(k, accent, "cs-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                        if let RuleConfig::Case { skip_ext, .. } = &mut ed_mut(s).config { *skip_ext = !*skip_ext; }
                    })),
            );
            let right = col()
                .grow(1.0)
                .w(0.0)
                .min_w(0.0)
                .items_start()
                .gap(8.0)
                .child(instant_checkbox(k, accent, "cs-ff".into(), ff, t("Forçar maiúsculo/minúsculo para fragmentos:", "Force case for fragments:"), |s| {
                    if let RuleConfig::Case { force_fragments, .. } = &mut ed_mut(s).config { *force_fragments = !*force_fragments; }
                }))
                .child(input_a(k, t("Fragmentos", "Fragments"), "CD, DVD, DJ", &ed.edit_a))
                .child(
                    text(t("Dica: fragmentos separados por vírgula serão mantidos exatamente como digitados, ex.: CD, DVD, DJ", "Hint: comma-separated fragments keep exact case, e.g. CD, DVD, DJ"))
                        .font(k.ty.caption())
                        .color(k.c.text_muted)
                        .wrap()
                        .w_full()
                        .min_w(0.0),
                )
                .child(el().grow(1.0))
                .child(instant_checkbox(k, accent, "cs-el".into(), ext_lo, t("Extensão sempre em minúsculo", "Extension always lower case"), |s| {
                    if let RuleConfig::Case { ext_lower, ext_upper, .. } = &mut ed_mut(s).config {
                        *ext_lower = !*ext_lower;
                        if *ext_lower { *ext_upper = false; }
                    }
                }))
                .child(instant_checkbox(k, accent, "cs-eu".into(), eu, t("Extensão sempre em maiúsculo", "Extension always UPPER CASE"), |s| {
                    if let RuleConfig::Case { ext_lower, ext_upper, .. } = &mut ed_mut(s).config {
                        *ext_upper = !*ext_upper;
                        if *ext_upper { *ext_lower = false; }
                    }
                }));
            row().w_full().min_w(0.0).gap(12.0).child(left.grow(1.15).w(0.0).min_w(285.0)).child(right)
        }
        RuleConfig::Serialize { start, repeat, step, reset_every_on, reset_every, reset_folder_change, reset_name_change, pad_zeros, pad_length, num_system, where_mode, position, skip_ext } => {
            let (st, rep, stp, re_on, re_n, rfc, rnc, pz, pl, ns, wm, pos, se) = (*start, *repeat, *step, *reset_every_on, *reset_every, *reset_folder_change, *reset_name_change, *pad_zeros, *pad_length, *num_system, *where_mode, *position, *skip_ext);
            let left = col()
                .grow(1.0)
                .w(0.0)
                .items_start()
                .gap(6.0)
                .child(row().gap(8.0).center_y().child(text(t("Índice inicial:", "Index starts:")).font(k.ty.text_small).color(k.c.text_base).w(134.0)).child(num_stepper(k, "ser-st", 0, st, -999_999, 999_999, &ed.nums[0], |s, v| {
                    if let RuleConfig::Serialize { start, .. } = &mut ed_mut(s).config { *start = v; }
                })))
                .child(row().gap(8.0).center_y().child(text(t("Repetir:", "Repeat:")).font(k.ty.text_small).color(k.c.text_base).w(134.0)).child(num_stepper(k, "ser-rep", 1, rep as i64, 1, 999_999, &ed.nums[1], |s, v| {
                    if let RuleConfig::Serialize { repeat, .. } = &mut ed_mut(s).config { *repeat = v as u32; }
                })))
                .child(row().gap(8.0).center_y().child(text(t("Passo:", "Step:")).font(k.ty.text_small).color(k.c.text_base).w(134.0)).child(num_stepper(k, "ser-stp", 2, stp, -999_999, 999_999, &ed.nums[2], |s, v| {
                    if let RuleConfig::Serialize { step, .. } = &mut ed_mut(s).config { *step = v; }
                })))
                .child(
                    row().gap(8.0).center_y()
                        .child(row().w(134.0).center_y().child(instant_checkbox(k, accent, "ser-re".into(), re_on, t("Redefinir a cada:", "Reset every:"), |s| {
                            if let RuleConfig::Serialize { reset_every_on, .. } = &mut ed_mut(s).config { *reset_every_on = !*reset_every_on; }
                        })))
                        .child(num_stepper(k, "ser-ren", 3, re_n as i64, 1, 999_999, &ed.nums[3], |s, v| {
                            if let RuleConfig::Serialize { reset_every_on, reset_every, .. } = &mut ed_mut(s).config {
                                *reset_every_on = true;
                                *reset_every = v as u32;
                            }
                        })),
                )
                .child(instant_checkbox(k, accent, "ser-rfc".into(), rfc, t("Redefinir se pasta for alterada", "Reset if folder changes"), |s| {
                    if let RuleConfig::Serialize { reset_folder_change, .. } = &mut ed_mut(s).config { *reset_folder_change = !*reset_folder_change; }
                }))
                .child(instant_checkbox(k, accent, "ser-rnc".into(), rnc, t("Redefinir se o nome for alterado", "Reset if filename changes"), |s| {
                    if let RuleConfig::Serialize { reset_name_change, .. } = &mut ed_mut(s).config { *reset_name_change = !*reset_name_change; }
                }))
                .child(
                    row().gap(6.0).center_y()
                        .child(instant_checkbox(k, accent, "ser-pz".into(), pz, t("Preencher zeros compr.:", "Pad with zeros len:"), |s| {
                            if let RuleConfig::Serialize { pad_zeros, .. } = &mut ed_mut(s).config { *pad_zeros = !*pad_zeros; }
                        }))
                        .child(num_stepper(k, "ser-pl", 4, pl as i64, 1, 64, &ed.nums[4], |s, v| {
                            if let RuleConfig::Serialize { pad_zeros, pad_length, .. } = &mut ed_mut(s).config {
                                *pad_zeros = true;
                                *pad_length = v as usize;
                            }
                        })),
                )
                .child(text(t("Sistema de numeração:", "Numbering system:")).font(k.ty.caption()).color(k.c.text_muted).pt(2.0))
                .child(
                    row()
                        .key("ser-num-dd")
                        .role("combobox")
                        .w_full()
                        .h(28.0)
                        .px(8.0)
                        .center_y()
                        .radius(k.radius.sm)
                        .fill(k.c.surface_raised)
                        .hover_fill(k.c.hover)
                        .border(1.0, k.c.border_base)
                        .cursor(Cursor::Pointer)
                        .on_press(|s: &mut State| {
                            let e = ed_mut(s);
                            e.show_num_sys_dd = !e.show_num_sys_dd;
                        })
                        .child(text(ns.label()).font(k.ty.text_xsmall).color(k.c.text_base).grow(1.0).truncate())
                        .child(icon(&icons::CHEVRON_DOWN, 12.0).color(k.c.text_muted)),
                );

            if ed.show_num_sys_dd {
                let mut list = col().w_full().padding(4.0).gap(2.0);
                for sys in NumSystem::ALL {
                    let act = ns == sys;
                    list = list.child(
                        row()
                            .role("option")
                            .w_full()
                            .h(26.0)
                            .px(8.0)
                            .center_y()
                            .radius(4.0)
                            .cursor(Cursor::Pointer)
                            .fill(if act { k.alpha(k.c.text_accent, 0.16) } else { Color::TRANSPARENT })
                            .hover_fill(k.c.hover)
                            .on_press(move |s: &mut State| {
                                let e = ed_mut(s);
                                if let RuleConfig::Serialize { num_system, .. } = &mut e.config { *num_system = sys; }
                                e.show_num_sys_dd = false;
                            })
                            .child(text(sys.label()).font(k.ty.text_xsmall).color(if act { k.c.text_accent } else { k.c.text_base })),
                    );
                }
                let surf = col().w(220.0).radius(6.0).fill(k.c.bg_base).border(1.0, k.c.border_base).child(list).modal(true, |s: &mut State| {
                    ed_mut(s).show_num_sys_dd = false;
                });
                layer.push(rune_kit::popover::anchored(k, surf, "ser-num-dd", rune_kit::Placement::BottomStart, 4.0));
            }

            let right = group_box(
                t("Onde inserir:", "Where to insert:"),
                col()
                    .w_full()
                    .items_start()
                    .gap(6.0)
                    .child(radio_btn(k, accent, "ser-pre".into(), wm == InsertWhere::Prefix, t("Prefixo", "Prefix"), |s| {
                        if let RuleConfig::Serialize { where_mode, .. } = &mut ed_mut(s).config { *where_mode = InsertWhere::Prefix; }
                    }))
                    .child(radio_btn(k, accent, "ser-suf".into(), wm == InsertWhere::Suffix, t("Sufixo", "Suffix"), |s| {
                        if let RuleConfig::Serialize { where_mode, .. } = &mut ed_mut(s).config { *where_mode = InsertWhere::Suffix; }
                    }))
                    .child(
                        row().gap(6.0).center_y()
                            .child(radio_btn(k, accent, "ser-pos".into(), wm == InsertWhere::Position, t("Posição:", "Position:"), |s| {
                                if let RuleConfig::Serialize { where_mode, .. } = &mut ed_mut(s).config { *where_mode = InsertWhere::Position; }
                            }))
                            .child(num_stepper(k, "ser-posn", 5, pos as i64, 1, 999_999, &ed.nums[5], |s, v| {
                                if let RuleConfig::Serialize { where_mode, position, .. } = &mut ed_mut(s).config {
                                    *where_mode = InsertWhere::Position;
                                    *position = v as usize;
                                }
                            })),
                    )
                    .child(radio_btn(k, accent, "ser-rep".into(), wm == InsertWhere::ReplaceCurrent, t("Substituir nome atual", "Replace current name"), |s| {
                        if let RuleConfig::Serialize { where_mode, .. } = &mut ed_mut(s).config { *where_mode = InsertWhere::ReplaceCurrent; }
                    }))
                    .child(instant_checkbox(k, accent, "ser-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                        if let RuleConfig::Serialize { skip_ext, .. } = &mut ed_mut(s).config { *skip_ext = !*skip_ext; }
                    })),
            );
            row().w_full().gap(10.0).child(left).child(right.w(195.0))
        }
        RuleConfig::Random { length, unique, digits, letters, custom, where_mode, position, skip_ext, .. } => {
            let (len, uq, dg, lt, cu, wm, pos, se) = (*length, *unique, *digits, *letters, *custom, *where_mode, *position, *skip_ext);
            let left = group_box(
                t("Caracteres para usar:", "Characters to use:"),
                col()
                    .w_full()
                    .items_start()
                    .gap(6.0)
                    .child(instant_checkbox(k, accent, "rnd-dg".into(), dg, t("Dígitos (0..9)", "Digits (0..9)"), |s| {
                        if let RuleConfig::Random { digits, .. } = &mut ed_mut(s).config { *digits = !*digits; }
                    }))
                    .child(instant_checkbox(k, accent, "rnd-lt".into(), lt, t("Letras (a..z)", "Letters (a..z)"), |s| {
                        if let RuleConfig::Random { letters, .. } = &mut ed_mut(s).config { *letters = !*letters; }
                    }))
                    .child(instant_checkbox(k, accent, "rnd-cu".into(), cu, t("Definido pelo usuário:", "User defined:"), |s| {
                        if let RuleConfig::Random { custom, .. } = &mut ed_mut(s).config { *custom = !*custom; }
                    }))
                    .child(input_a(k, t("Caracteres", "Characters"), "", &ed.edit_a)),
            );
            let right = group_box(
                t("Onde inserir:", "Where to insert:"),
                col()
                    .w_full()
                    .items_start()
                    .gap(6.0)
                    .child(radio_btn(k, accent, "rnd-pre".into(), wm == InsertWhere::Prefix, t("Prefixo", "Prefix"), |s| {
                        if let RuleConfig::Random { where_mode, .. } = &mut ed_mut(s).config { *where_mode = InsertWhere::Prefix; }
                    }))
                    .child(radio_btn(k, accent, "rnd-suf".into(), wm == InsertWhere::Suffix, t("Sufixo", "Suffix"), |s| {
                        if let RuleConfig::Random { where_mode, .. } = &mut ed_mut(s).config { *where_mode = InsertWhere::Suffix; }
                    }))
                    .child(
                        row().gap(6.0).center_y()
                            .child(radio_btn(k, accent, "rnd-pos".into(), wm == InsertWhere::Position, t("Posição:", "Position:"), |s| {
                                if let RuleConfig::Random { where_mode, .. } = &mut ed_mut(s).config { *where_mode = InsertWhere::Position; }
                            }))
                            .child(num_stepper(k, "rnd-posn", 1, pos as i64, 1, 999_999, &ed.nums[1], |s, v| {
                                if let RuleConfig::Random { where_mode, position, .. } = &mut ed_mut(s).config {
                                    *where_mode = InsertWhere::Position;
                                    *position = v as usize;
                                }
                            })),
                    )
                    .child(radio_btn(k, accent, "rnd-rep".into(), wm == InsertWhere::ReplaceCurrent, t("Substituir nome atual", "Replace current name"), |s| {
                        if let RuleConfig::Random { where_mode, .. } = &mut ed_mut(s).config { *where_mode = InsertWhere::ReplaceCurrent; }
                    }))
                    .child(instant_checkbox(k, accent, "rnd-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                        if let RuleConfig::Random { skip_ext, .. } = &mut ed_mut(s).config { *skip_ext = !*skip_ext; }
                    })),
            );
            col()
                .w_full()
                .items_start()
                .gap(8.0)
                .child(
                    row()
                        .w_full()
                        .gap(10.0)
                        .center_y()
                        .child(text(t("Tamanho da sequência aleatória:", "Length of random string:")).font(k.ty.text_small).color(k.c.text_base))
                        .child(num_stepper(k, "rnd-len", 0, len as i64, 1, 64, &ed.nums[0], |s, v| {
                            if let RuleConfig::Random { length, .. } = &mut ed_mut(s).config { *length = v as usize; }
                        })),
                )
                .child(instant_checkbox(k, accent, "rnd-uq".into(), uq, t("Único se possível", "Unique if possible"), |s| {
                    if let RuleConfig::Random { unique, .. } = &mut ed_mut(s).config { *unique = !*unique; }
                }))
                .child(row().w_full().gap(10.0).child(left.grow(1.0).w(0.0)).child(right.grow(1.0).w(0.0)))
        }
        RuleConfig::Padding { pad_numbers, pad_numbers_len, strip_zeros, pad_text, pad_text_len, pad_text_side, skip_ext, .. } => {
            let (pn, pnl, sz, pt, ptl, pts, se) = (*pad_numbers, *pad_numbers_len, *strip_zeros, *pad_text, *pad_text_len, *pad_text_side, *skip_ext);
            col()
                .w_full()
                .items_start()
                .gap(10.0)
                .child(group_box(
                    t("Sequências de números", "Number sequences"),
                    col()
                        .w_full()
                        .items_start()
                        .gap(6.0)
                        .child(
                            row().w_full().gap(8.0).center_y()
                                .child(instant_checkbox(k, accent, "pad-pn".into(), pn, t("Adicionar preenchimento com zero ao comprimento:", "Pad with zeros to length:"), |s| {
                                    if let RuleConfig::Padding { pad_numbers, .. } = &mut ed_mut(s).config { *pad_numbers = !*pad_numbers; }
                                }))
                                .child(num_stepper(k, "pad-pnl", 0, pnl as i64, 1, 64, &ed.nums[0], |s, v| {
                                    if let RuleConfig::Padding { pad_numbers, pad_numbers_len, .. } = &mut ed_mut(s).config {
                                        *pad_numbers = true;
                                        *pad_numbers_len = v as usize;
                                    }
                                })),
                        )
                        .child(instant_checkbox(k, accent, "pad-sz".into(), sz, t("Remover preenchimento com zero", "Strip zero padding"), |s| {
                            if let RuleConfig::Padding { strip_zeros, .. } = &mut ed_mut(s).config { *strip_zeros = !*strip_zeros; }
                        })),
                ))
                .child(group_box(
                    t("Preenchimento de texto", "Text padding"),
                    col()
                        .w_full()
                        .items_start()
                        .gap(6.0)
                        .child(
                            row().w_full().gap(8.0).center_y()
                                .child(instant_checkbox(k, accent, "pad-pt".into(), pt, t("Adicionar preenchimento ao comprimento:", "Pad text to length:"), |s| {
                                    if let RuleConfig::Padding { pad_text, .. } = &mut ed_mut(s).config { *pad_text = !*pad_text; }
                                }))
                                .child(num_stepper(k, "pad-ptl", 1, ptl as i64, 1, 64, &ed.nums[1], |s, v| {
                                    if let RuleConfig::Padding { pad_text, pad_text_len, .. } = &mut ed_mut(s).config {
                                        *pad_text = true;
                                        *pad_text_len = v as usize;
                                    }
                                })),
                        )
                        .child(
                            row().w_full().gap(8.0).center_y()
                                .child(text(t("Caracteres de preenchimento:", "Padding characters:")).font(k.ty.text_small).color(k.c.text_base))
                                .child(input_a(k, t("Caracteres", "Characters"), "0", &ed.edit_a).grow(1.0).w(0.0).min_w(0.0)),
                        )
                        .child(
                            row().w_full().gap(16.0).center_y()
                                .child(text(t("Posição:", "Position:")).font(k.ty.text_small).color(k.c.text_base))
                                .child(radio_btn(k, accent, "pad-sl".into(), pts == Side::Left, t("Esquerda", "Left"), |s| {
                                    if let RuleConfig::Padding { pad_text_side, .. } = &mut ed_mut(s).config { *pad_text_side = Side::Left; }
                                }))
                                .child(radio_btn(k, accent, "pad-sr".into(), pts == Side::Right, t("Direita", "Right"), |s| {
                                    if let RuleConfig::Padding { pad_text_side, .. } = &mut ed_mut(s).config { *pad_text_side = Side::Right; }
                                })),
                        ),
                ))
                .child(instant_checkbox(k, accent, "pad-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                    if let RuleConfig::Padding { skip_ext, .. } = &mut ed_mut(s).config { *skip_ext = !*skip_ext; }
                }))
        }
        RuleConfig::CleanUp { strip_parens, strip_brackets, strip_braces, replace_dot, replace_comma, replace_underscore, replace_plus, replace_dash, replace_pct20, skip_versions, fix_spaces, normalize_unicode_spaces, strip_emoji, strip_accents, space_before_camel, prepare_sharepoint, skip_ext } => {
            let (sp, sb, sbr, rd, rc, ru, rp, rda, r20, sv, fs, nus, sem, sac, sbc, psp, se) = (
                *strip_parens, *strip_brackets, *strip_braces, *replace_dot, *replace_comma, *replace_underscore, *replace_plus, *replace_dash, *replace_pct20, *skip_versions, *fix_spaces, *normalize_unicode_spaces, *strip_emoji, *strip_accents, *space_before_camel, *prepare_sharepoint, *skip_ext,
            );
            col()
                .w_full()
                .items_start()
                .gap(6.0)
                .child(group_box(
                    t("Retirar o conteúdo de colchetes:", "Strip contents of brackets:"),
                    row().w_full().gap(16.0)
                        .child(instant_checkbox(k, accent, "cln-p".into(), sp, "(...)", |s| {
                            if let RuleConfig::CleanUp { strip_parens, .. } = &mut ed_mut(s).config { *strip_parens = !*strip_parens; }
                        }))
                        .child(instant_checkbox(k, accent, "cln-b".into(), sb, "[...]", |s| {
                            if let RuleConfig::CleanUp { strip_brackets, .. } = &mut ed_mut(s).config { *strip_brackets = !*strip_brackets; }
                        }))
                        .child(instant_checkbox(k, accent, "cln-br".into(), sbr, "{...}", |s| {
                            if let RuleConfig::CleanUp { strip_braces, .. } = &mut ed_mut(s).config { *strip_braces = !*strip_braces; }
                        })),
                ))
                .child(group_box(
                    t("Substituir estes caracteres por espaços:", "Replace these characters with spaces:"),
                    col()
                        .w_full()
                        .items_start()
                        .gap(6.0)
                        .child(
                            row().w_full().gap(12.0)
                                .child(instant_checkbox(k, accent, "cln-dot".into(), rd, ". (ponto)", |s| {
                                    if let RuleConfig::CleanUp { replace_dot, .. } = &mut ed_mut(s).config { *replace_dot = !*replace_dot; }
                                }))
                                .child(instant_checkbox(k, accent, "cln-com".into(), rc, ", (vírgula)", |s| {
                                    if let RuleConfig::CleanUp { replace_comma, .. } = &mut ed_mut(s).config { *replace_comma = !*replace_comma; }
                                }))
                                .child(instant_checkbox(k, accent, "cln-und".into(), ru, "_", |s| {
                                    if let RuleConfig::CleanUp { replace_underscore, .. } = &mut ed_mut(s).config { *replace_underscore = !*replace_underscore; }
                                }))
                                .child(instant_checkbox(k, accent, "cln-pls".into(), rp, "+", |s| {
                                    if let RuleConfig::CleanUp { replace_plus, .. } = &mut ed_mut(s).config { *replace_plus = !*replace_plus; }
                                }))
                                .child(instant_checkbox(k, accent, "cln-dsh".into(), rda, "-", |s| {
                                    if let RuleConfig::CleanUp { replace_dash, .. } = &mut ed_mut(s).config { *replace_dash = !*replace_dash; }
                                }))
                                .child(instant_checkbox(k, accent, "cln-20".into(), r20, "%20", |s| {
                                    if let RuleConfig::CleanUp { replace_pct20, .. } = &mut ed_mut(s).config { *replace_pct20 = !*replace_pct20; }
                                })),
                        )
                        .child(instant_checkbox(k, accent, "cln-sv".into(), sv, t("Ignorar sequências numéricas, por exemplo: versão 1.2.3.4", "Skip number sequences, e.g. version 1.2.3.4"), |s| {
                            if let RuleConfig::CleanUp { skip_versions, .. } = &mut ed_mut(s).config { *skip_versions = !*skip_versions; }
                        })),
                ))
                .child(instant_checkbox(k, accent, "cln-fs".into(), fs, t("Corrigir espaços: somente um espaço por vez, sem espaços nas pontas", "Fix spaces: collapse multiple spaces and trim ends"), |s| {
                    if let RuleConfig::CleanUp { fix_spaces, .. } = &mut ed_mut(s).config { *fix_spaces = !*fix_spaces; }
                }))
                .child(instant_checkbox(k, accent, "cln-nus".into(), nus, t("Normalizar espaços unicode", "Normalize unicode spaces"), |s| {
                    if let RuleConfig::CleanUp { normalize_unicode_spaces, .. } = &mut ed_mut(s).config { *normalize_unicode_spaces = !*normalize_unicode_spaces; }
                }))
                .child(instant_checkbox(k, accent, "cln-em".into(), sem, t("Remover faixa unicode emoji", "Strip unicode emoji range"), |s| {
                    if let RuleConfig::CleanUp { strip_emoji, .. } = &mut ed_mut(s).config { *strip_emoji = !*strip_emoji; }
                }))
                .child(instant_checkbox(k, accent, "cln-ac".into(), sac, t("Remover marcas unicode (diacríticos, acentos)", "Remove unicode marks (diacritics, accents)"), |s| {
                    if let RuleConfig::CleanUp { strip_accents, .. } = &mut ed_mut(s).config { *strip_accents = !*strip_accents; }
                }))
                .child(instant_checkbox(k, accent, "cln-cc".into(), sbc, t("Inserir espaço na frente de letras maiúsculas (ex.: CamelCase)", "Insert space in front of capitals (e.g. CamelCase)"), |s| {
                    if let RuleConfig::CleanUp { space_before_camel, .. } = &mut ed_mut(s).config { *space_before_camel = !*space_before_camel; }
                }))
                .child(instant_checkbox(k, accent, "cln-sp".into(), psp, t("Preparar para SharePoint (sempre incl. extensão)", "Prepare for SharePoint (always incl. extension)"), |s| {
                    if let RuleConfig::CleanUp { prepare_sharepoint, .. } = &mut ed_mut(s).config { *prepare_sharepoint = !*prepare_sharepoint; }
                }))
                .child(instant_checkbox(k, accent, "cln-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                    if let RuleConfig::CleanUp { skip_ext, .. } = &mut ed_mut(s).config { *skip_ext = !*skip_ext; }
                }))
        }
        RuleConfig::Translit { direction, auto_case, skip_ext, .. } => {
            let (dir, ac, se) = (*direction, *auto_case, *skip_ext);
            let left = col()
                .w(235.0)
                .gap(6.0)
                .child(
                    row().w_full().gap(6.0)
                        .child(tool_btn(k, "tr-lat", t("Acentos Latinos", "Latin Accents"), true, |s| {
                            ed_mut(s).edit_a = TextEdit::new(DEFAULT_LATIN_TRANSLIT);
                        }))
                        .child(tool_btn(k, "tr-cyr", t("Cirílico", "Cyrillic"), true, |s| {
                            ed_mut(s).edit_a = TextEdit::new(CYRILLIC_TRANSLIT);
                        })),
                )
                .child(
                    TextArea::new(&ed.edit_a)
                        .width(None)
                        .rows(12)
                        .scroll_bar(true)
                        .on_edit(|s: &mut State| &mut ed_mut(s).edit_a)
                        .build(k),
                );
            let right = col()
                .grow(1.0)
                .w(0.0)
                .items_start()
                .gap(8.0)
                .child(text(t("Direção:", "Direction:")).font(k.ty.text_small).color(k.c.text_base))
                .child(radio_btn(k, accent, "tr-fwd".into(), dir == TranslitDir::Forward, t("Avançar", "Forward"), |s| {
                    if let RuleConfig::Translit { direction, .. } = &mut ed_mut(s).config { *direction = TranslitDir::Forward; }
                }))
                .child(radio_btn(k, accent, "tr-bwd".into(), dir == TranslitDir::Backward, t("Retroceder", "Backward"), |s| {
                    if let RuleConfig::Translit { direction, .. } = &mut ed_mut(s).config { *direction = TranslitDir::Backward; }
                }))
                .child(
                    text(t(
                        "Dica: Alfabeto é um conjunto de pares separados por '=' (ex.: á=a, ç=c).",
                        "Hint: Alphabet is a set of pairs separated by '=' (e.g. á=a, ç=c).",
                    ))
                    .font(k.ty.caption())
                    .color(k.c.text_muted)
                    .wrap()
                    .w_full()
                    .min_w(0.0),
                )
                .child(instant_checkbox(k, accent, "tr-ac".into(), ac, t("Ajuste automático de maiúsculas/minúsculas", "Automatic case adjustment"), |s| {
                    if let RuleConfig::Translit { auto_case, .. } = &mut ed_mut(s).config { *auto_case = !*auto_case; }
                }))
                .child(instant_checkbox(k, accent, "tr-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                    if let RuleConfig::Translit { skip_ext, .. } = &mut ed_mut(s).config { *skip_ext = !*skip_ext; }
                }));
            row().w_full().gap(10.0).child(left).child(right)
        }
        RuleConfig::ReformatDate { whole_words, skip_ext, custom_short_months, custom_long_months, adjust_on, adjust_amount, adjust_unit, .. } => {
            let (ww, se, csm, clm, ao, aa, au) = (*whole_words, *skip_ext, *custom_short_months, *custom_long_months, *adjust_on, *adjust_amount, *adjust_unit);
            if ed.show_date_find_dd {
                let mut list = col().w_full().padding(4.0).gap(2.0);
                for fmt in DATE_FORMATS {
                    list = list.child(row().role("option").w_full().h(26.0).px(8.0).center_y().radius(4.0).cursor(Cursor::Pointer).hover_fill(k.c.hover).on_press(move |s: &mut State| {
                        let e = ed_mut(s);
                        e.edit_a = TextEdit::new(*fmt);
                        e.show_date_find_dd = false;
                    }).child(text(*fmt).font(k.ty.text_xsmall).color(k.c.text_base)));
                }
                let surf = col().w(200.0).radius(6.0).fill(k.c.bg_base).border(1.0, k.c.border_base).child(list).modal(true, |s: &mut State| ed_mut(s).show_date_find_dd = false);
                layer.push(rune_kit::popover::anchored(k, surf, "dt-find-dd", rune_kit::Placement::BottomEnd, 4.0));
            }
            if ed.show_date_rep_dd {
                let mut list = col().w_full().padding(4.0).gap(2.0);
                for fmt in DATE_FORMATS {
                    list = list.child(row().role("option").w_full().h(26.0).px(8.0).center_y().radius(4.0).cursor(Cursor::Pointer).hover_fill(k.c.hover).on_press(move |s: &mut State| {
                        let e = ed_mut(s);
                        e.edit_b = TextEdit::new(*fmt);
                        e.show_date_rep_dd = false;
                    }).child(text(*fmt).font(k.ty.text_xsmall).color(k.c.text_base)));
                }
                let surf = col().w(200.0).radius(6.0).fill(k.c.bg_base).border(1.0, k.c.border_base).child(list).modal(true, |s: &mut State| ed_mut(s).show_date_rep_dd = false);
                layer.push(rune_kit::popover::anchored(k, surf, "dt-rep-dd", rune_kit::Placement::BottomEnd, 4.0));
            }
            if ed.show_time_unit_dd {
                let mut list = col().w_full().padding(4.0).gap(2.0);
                for u in TimeUnit::ALL {
                    list = list.child(row().role("option").w_full().h(26.0).px(8.0).center_y().radius(4.0).cursor(Cursor::Pointer).hover_fill(k.c.hover).on_press(move |s: &mut State| {
                        let e = ed_mut(s);
                        if let RuleConfig::ReformatDate { adjust_unit, .. } = &mut e.config { *adjust_unit = u; }
                        e.show_time_unit_dd = false;
                    }).child(text(u.label()).font(k.ty.text_xsmall).color(k.c.text_base)));
                }
                let surf = col().w(140.0).radius(6.0).fill(k.c.bg_base).border(1.0, k.c.border_base).child(list).modal(true, |s: &mut State| ed_mut(s).show_time_unit_dd = false);
                layer.push(rune_kit::popover::anchored(k, surf, "dt-unit-dd", rune_kit::Placement::BottomStart, 4.0));
            }
            col()
                .w_full()
                .items_start()
                .gap(8.0)
                .child(text(t("Localizar formatos de data/hora:", "Find date/time formats:")).font(k.ty.text_small).color(k.c.text_base))
                .child(
                    row().w_full().gap(6.0)
                        .child(input_a(k, t("Localizar formato", "Find format"), "yyyy-mm-dd", &ed.edit_a).grow(1.0).w(0.0).min_w(0.0))
                        .child(tool_btn(k, "dt-find-dd", "▾", true, |s| { let e = ed_mut(s); e.show_date_find_dd = !e.show_date_find_dd; })),
                )
                .child(text(t("Converter para formato de data/hora:", "Convert to date/time format:")).font(k.ty.text_small).color(k.c.text_base))
                .child(
                    row().w_full().gap(6.0)
                        .child(input_b(k, t("Novo formato", "New format"), "dd-mm-yyyy", &ed.edit_b).grow(1.0).w(0.0).min_w(0.0))
                        .child(tool_btn(k, "dt-rep-dd", "▾", true, |s| { let e = ed_mut(s); e.show_date_rep_dd = !e.show_date_rep_dd; })),
                )
                .child(instant_checkbox(k, accent, "dt-ww".into(), ww, t("Corresponder somente palavras inteiras", "Match whole words only"), |s| {
                    if let RuleConfig::ReformatDate { whole_words, .. } = &mut ed_mut(s).config { *whole_words = !*whole_words; }
                }))
                .child(instant_checkbox(k, accent, "dt-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                    if let RuleConfig::ReformatDate { skip_ext, .. } = &mut ed_mut(s).config { *skip_ext = !*skip_ext; }
                }))
                .child(
                    row().w_full().gap(8.0).center_y()
                        .child(row().w(165.0).center_y().child(instant_checkbox(k, accent, "dt-csm".into(), csm, t("Usar meses curtos:", "Short months:"), |s| {
                            if let RuleConfig::ReformatDate { custom_short_months, .. } = &mut ed_mut(s).config { *custom_short_months = !*custom_short_months; }
                        })))
                        .child(input_c(k, t("Meses curtos", "Short months"), "", &ed.edit_c).grow(1.0).w(0.0).min_w(0.0)),
                )
                .child(
                    row().w_full().gap(8.0).center_y()
                        .child(row().w(165.0).center_y().child(instant_checkbox(k, accent, "dt-clm".into(), clm, t("Usar meses longos:", "Long months:"), |s| {
                            if let RuleConfig::ReformatDate { custom_long_months, .. } = &mut ed_mut(s).config { *custom_long_months = !*custom_long_months; }
                        })))
                        .child(text_box(k, "Long months", "", &ed.edit_d, |s| &mut ed_mut(s).edit_d).grow(1.0).w(0.0).min_w(0.0)),
                )
                .child(
                    row().w_full().gap(8.0).center_y()
                        .child(instant_checkbox(k, accent, "dt-adj".into(), ao, t("Ajustar tempo por:", "Adjust time by:"), |s| {
                            if let RuleConfig::ReformatDate { adjust_on, .. } = &mut ed_mut(s).config { *adjust_on = !*adjust_on; }
                        }))
                        .child(num_stepper(k, "dt-adjn", 0, aa, -999_999, 999_999, &ed.nums[0], |s, v| {
                            if let RuleConfig::ReformatDate { adjust_on, adjust_amount, .. } = &mut ed_mut(s).config {
                                *adjust_on = true;
                                *adjust_amount = v;
                            }
                        }))
                        .child(
                            row()
                                .key("dt-unit-dd")
                                .role("combobox")
                                .h(28.0)
                                .px(10.0)
                                .gap(6.0)
                                .center_y()
                                .radius(k.radius.sm)
                                .fill(k.c.surface_raised)
                                .hover_fill(k.c.hover)
                                .border(1.0, k.c.border_base)
                                .cursor(Cursor::Pointer)
                                .on_press(|s: &mut State| { let e = ed_mut(s); e.show_time_unit_dd = !e.show_time_unit_dd; })
                                .child(text(au.label()).font(k.ty.text_xsmall).color(k.c.text_base))
                                .child(icon(&icons::CHEVRON_DOWN, 12.0).color(k.c.text_muted)),
                        ),
                )
        }
        RuleConfig::Regex { case_sensitive, skip_ext, .. } => {
            let (cs, se) = (*case_sensitive, *skip_ext);
            col()
                .w_full()
                .items_start()
                .gap(10.0)
                .child(
                    row().w_full().gap(8.0).center_y()
                        .child(text(t("Expressão:", "Expression:")).font(k.ty.text_small).color(k.c.text_base).w(76.0))
                        .child(input_a(k, t("Expressão", "Expression"), r"^(.+) - (\d+)", &ed.edit_a).grow(1.0).w(0.0).min_w(0.0))
                        .child(meta_tag_btn(k, MetaTagTarget::RegexPattern)),
                )
                .child(
                    row().w_full().gap(8.0).center_y()
                        .child(text(t("Substituir:", "Replace:")).font(k.ty.text_small).color(k.c.text_base).w(76.0))
                        .child(input_b(k, t("Substituir", "Replace"), "$2 - $1", &ed.edit_b).grow(1.0).w(0.0).min_w(0.0)),
                )
                .child(text(t("Dica: use $1..$9 para referenciar subexpressões.", "Hint: use $1..$9 to reference capture groups.")).font(k.ty.caption()).color(k.c.text_muted))
                .child(instant_checkbox(k, accent, "rx-cs".into(), cs, t("Diferenciar maiúsculas-minúsculas", "Case sensitive"), |s| {
                    if let RuleConfig::Regex { case_sensitive, .. } = &mut ed_mut(s).config { *case_sensitive = !*case_sensitive; }
                }))
                .child(instant_checkbox(k, accent, "rx-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                    if let RuleConfig::Regex { skip_ext, .. } = &mut ed_mut(s).config { *skip_ext = !*skip_ext; }
                }))
        }
        RuleConfig::PascalScript { .. } => {
            let line_count = ed.edit_a.text.lines().count().max(1);
            let gutter_text = (1..=line_count).map(|n| format!("{n:>2}")).collect::<Vec<_>>().join("\n");
            if ed.show_scripts_menu {
                let mut list = col().w_full().padding(4.0).gap(2.0);
                for (pt, en, code) in SCRIPT_PRESETS {
                    list = list.child(
                        row().role("option").w_full().h(28.0).px(8.0).center_y().radius(4.0).cursor(Cursor::Pointer).hover_fill(k.c.hover).on_press(move |s: &mut State| {
                            let e = ed_mut(s);
                            e.edit_a = TextEdit::new(*code);
                            e.show_scripts_menu = false;
                            e.script_status = None;
                        }).child(text(t(*pt, *en)).font(k.ty.text_xsmall).color(k.c.text_base)),
                    );
                }
                let surf = col().w(260.0).radius(6.0).fill(k.c.bg_base).border(1.0, k.c.border_base).child(list).modal(true, |s: &mut State| ed_mut(s).show_scripts_menu = false);
                layer.push(rune_kit::popover::anchored(k, surf, "ps-scripts-btn", rune_kit::Placement::TopStart, 4.0));
            }
            let mut body = col()
                .w_full()
                .gap(8.0)
                .child(
                    row()
                        .w_full()
                        .h(230.0)
                        .radius(k.radius.sm)
                        .fill(field_colors(k, false).0)
                        .border(1.0, field_colors(k, false).1)
                        .child(
                            col()
                                .w(32.0)
                                .h_full()
                                .py(8.0)
                                .px(4.0)
                                .border_sides([0.0, 1.0, 0.0, 0.0], panel_border)
                                .child(text(gutter_text).font(k.ty.caption()).color(k.c.text_faint)),
                        )
                        .child(
                            TextArea::new(&ed.edit_a)
                                .plain()
                                .width(None)
                                .rows(11)
                                .scroll_bar(true)
                                .on_edit(|s: &mut State| &mut ed_mut(s).edit_a)
                                .build(k)
                                .grow(1.0)
                                .w(0.0)
                                .h_full(),
                        ),
                )
                .child(
                    row()
                        .w_full()
                        .gap(6.0)
                        .center_y()
                        .child(tool_btn(k, "ps-compile-btn", t("Tentar Compilar", "Try to Compile"), true, |s| {
                            let e = ed_mut(s);
                            e.script_status = Some(match rules_script::compile(&e.edit_a.text) {
                                Ok(_) => Ok(t("Compilado com sucesso.", "Compiled successfully.").into()),
                                Err(err) => Err(err),
                            });
                        }))
                        .child(tool_btn(k, "ps-scripts-btn", t("Scripts ▾", "Scripts ▾"), true, |s| {
                            let e = ed_mut(s);
                            e.show_scripts_menu = !e.show_scripts_menu;
                        }))
                        .child(meta_tag_btn(k, MetaTagTarget::PascalScript))
                        .child(tool_btn(k, "ps-help-btn", t("Ajuda", "Help"), true, |s| {
                            let e = ed_mut(s);
                            e.show_help = !e.show_help;
                        })),
                );
            if let Some(st) = &ed.script_status {
                let (msg, col_fg) = match st {
                    Ok(m) => (m.clone(), k.c.success.fg),
                    Err(e) => (e.clone(), k.c.danger.fg),
                };
                body = body.child(text(msg).font(k.ty.caption()).color(col_fg).wrap().w_full().min_w(0.0));
            }
            if ed.show_help {
                body = body.child(
                    text("Vars: FileName, FilePath, FileIndex · Fns: WideExtractBaseName, WideExtractFileExt, WideReplaceStr, WideUpperCase, WideLowerCase, WideCaseCapitalize, WideCopy, WideDelete, WideInsert, WidePos, WideLength, WideTrim, IntToStr, StrToInt, MetaTag(':Tag:')")
                        .font(k.ty.caption())
                        .color(k.c.text_muted)
                        .wrap()
                        .w_full()
                        .min_w(0.0),
                );
            }
            body
        }
        RuleConfig::UserInput { mode, skip_ext, trim_lines, skip_empty, .. } => {
            let (um, se, tl, sk) = (*mode, *skip_ext, *trim_lines, *skip_empty);
            col()
                .w_full()
                .gap(8.0)
                .child(text(t("Digite aqui os novos nomes de arquivos (um por linha):", "Type the new filenames here (one per line):")).font(k.ty.text_small).color(k.c.text_base))
                .child(
                    TextArea::new(&ed.edit_a)
                        .width(None)
                        .rows(10)
                        .scroll_bar(true)
                        .on_edit(|s: &mut State| &mut ed_mut(s).edit_a)
                        .build(k),
                )
                .child(
                    row()
                        .w_full()
                        .gap(16.0)
                        .child(
                            col()
                                .grow(1.0)
                                .w(0.0)
                                .items_start()
                                .gap(4.0)
                                .child(radio_btn(k, accent, "ui-bef".into(), um == UserInputMode::Before, t("Inserir na frente do nome atual", "Insert in front of current name"), |s| {
                                    if let RuleConfig::UserInput { mode, .. } = &mut ed_mut(s).config { *mode = UserInputMode::Before; }
                                }))
                                .child(radio_btn(k, accent, "ui-aft".into(), um == UserInputMode::After, t("Inserir após nome atual", "Insert after current name"), |s| {
                                    if let RuleConfig::UserInput { mode, .. } = &mut ed_mut(s).config { *mode = UserInputMode::After; }
                                })),
                        )
                        .child(
                            col()
                                .grow(1.0)
                                .w(0.0)
                                .items_start()
                                .gap(4.0)
                                .child(radio_btn(k, accent, "ui-rep".into(), um == UserInputMode::Replace, t("Substituir nome atual", "Replace current name"), |s| {
                                    if let RuleConfig::UserInput { mode, .. } = &mut ed_mut(s).config { *mode = UserInputMode::Replace; }
                                }))
                                .child(instant_checkbox(k, accent, "ui-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                                    if let RuleConfig::UserInput { skip_ext, .. } = &mut ed_mut(s).config { *skip_ext = !*skip_ext; }
                                })),
                        ),
                )
                .child(
                    row()
                        .w_full()
                        .gap(16.0)
                        .child(instant_checkbox(k, accent, "ui-tl".into(), tl, t("Aparar espaços das linhas", "Trim line whitespace"), |s| {
                            if let RuleConfig::UserInput { trim_lines, .. } = &mut ed_mut(s).config { *trim_lines = !*trim_lines; }
                        }))
                        .child(instant_checkbox(k, accent, "ui-sk".into(), sk, t("Pular linhas vazias", "Skip empty lines"), |s| {
                            if let RuleConfig::UserInput { skip_empty, .. } = &mut ed_mut(s).config { *skip_empty = !*skip_empty; }
                        })),
                )
        }
        RuleConfig::Mapping { entries, allow_reuse, partial_match, inverse, case_sensitive, skip_ext } => {
            let (ar, pm, inv, cs, se) = (*allow_reuse, *partial_match, *inverse, *case_sensitive, *skip_ext);
            let mut tbl = col().w_full().pr(12.0);
            for (idx, (k_str, v_str)) in entries.iter().enumerate() {
                tbl = tbl.child(
                    row()
                        .key(format!("map-row-{idx}"))
                        .w_full()
                        .h(26.0)
                        .px(8.0)
                        .gap(8.0)
                        .center_y()
                        .border_sides([0.0, 0.0, 1.0, 0.0], panel_border)
                        .child(text(k_str.clone()).font(k.ty.text_xsmall).color(k.c.text_base).grow(1.0).w(0.0).truncate())
                        .child(text("→").font(k.ty.caption()).color(k.c.text_faint))
                        .child(text(v_str.clone()).font(k.ty.text_xsmall).color(k.c.text_accent).grow(1.0).w(0.0).truncate())
                        .child(
                            row().role("button").px(5.0).cursor(Cursor::Pointer).hover_fill(k.c.danger.bg).on_press(move |s: &mut State| {
                                if let RuleConfig::Mapping { entries, .. } = &mut ed_mut(s).config
                                    && idx < entries.len()
                                {
                                    entries.remove(idx);
                                }
                            }).child(text("×").font(k.ty.caption()).color(k.c.text_muted)),
                        ),
                );
            }
            col()
                .w_full()
                .gap(8.0)
                .child(
                    row()
                        .w_full()
                        .gap(12.0)
                        .child(
                            col()
                                .grow(1.0)
                                .w(0.0)
                                .items_start()
                                .gap(4.0)
                                .child(instant_checkbox(k, accent, "map-ar".into(), ar, t("Permitir reutilização", "Allow reuse"), |s| {
                                    if let RuleConfig::Mapping { allow_reuse, .. } = &mut ed_mut(s).config { *allow_reuse = !*allow_reuse; }
                                }))
                                .child(instant_checkbox(k, accent, "map-pm".into(), pm, t("Correspondência parcial", "Partial match"), |s| {
                                    if let RuleConfig::Mapping { partial_match, .. } = &mut ed_mut(s).config { *partial_match = !*partial_match; }
                                }))
                                .child(instant_checkbox(k, accent, "map-inv".into(), inv, t("Mapeamento inverso", "Inverse mapping"), |s| {
                                    if let RuleConfig::Mapping { inverse, .. } = &mut ed_mut(s).config { *inverse = !*inverse; }
                                })),
                        )
                        .child(
                            col()
                                .grow(1.0)
                                .w(0.0)
                                .items_start()
                                .gap(4.0)
                                .child(instant_checkbox(k, accent, "map-cs".into(), cs, t("Diferenciar maiúsculas de minúsculas", "Case sensitive"), |s| {
                                    if let RuleConfig::Mapping { case_sensitive, .. } = &mut ed_mut(s).config { *case_sensitive = !*case_sensitive; }
                                }))
                                .child(instant_checkbox(k, accent, "map-se".into(), se, t("Ignorar extensão", "Skip extension"), |s| {
                                    if let RuleConfig::Mapping { skip_ext, .. } = &mut ed_mut(s).config { *skip_ext = !*skip_ext; }
                                })),
                        ),
                )
                .child(
                    row()
                        .w_full()
                        .gap(6.0)
                        .center_y()
                        .child(text_box(k, "Corresponder", t("Corresponder", "Match"), &ed.map_key, |s| &mut ed_mut(s).map_key).grow(1.0).w(0.0).min_w(0.0))
                        .child(text_box(k, "Novo Nome", t("Novo Nome", "New Name"), &ed.map_val, |s| &mut ed_mut(s).map_val).grow(1.0).w(0.0).min_w(0.0))
                        .child(tool_btn(k, "map-add-row", "+", true, |s| {
                            let e = ed_mut(s);
                            let a = e.map_key.text.clone();
                            let b = e.map_val.text.clone();
                            if !a.is_empty() || !b.is_empty() {
                                if let RuleConfig::Mapping { entries, .. } = &mut e.config {
                                    entries.push((a, b));
                                }
                                e.map_key = TextEdit::new("");
                                e.map_val = TextEdit::new("");
                            }
                        }))
                        .child(tool_btn(k, "map-clear", t("Limpar", "Clear"), true, |s| {
                            if let RuleConfig::Mapping { entries, .. } = &mut ed_mut(s).config {
                                entries.clear();
                            }
                        })),
                )
                .child(
                    col()
                        .w_full()
                        .h(145.0)
                        .radius(k.radius.sm)
                        .fill(k.c.surface_raised)
                        .border(1.0, panel_border)
                        .child(
                            row().w_full().h(24.0).px(8.0).gap(8.0).center_y().border_sides([0.0, 0.0, 1.0, 0.0], panel_border)
                                .child(text(t("Corresponder", "Match")).font(k.ty.caption()).color(k.c.text_muted).grow(1.0).w(0.0))
                                .child(text(t("Novo Nome", "New Name")).font(k.ty.caption()).color(k.c.text_muted).grow(1.0).w(0.0)),
                        )
                        .child(col().w_full().grow(1.0).min_h(0.0).scroll(ScrollSpec { stick_bottom: false }).child(tbl).scroll_bar_styled(ui::bar_style(k))),
                )
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Modal: Meta Tags Picker

fn render_meta_tags_modal(k: &rune_kit::Kit, br: &BatchRename, target: MetaTagTarget, panel_border: Color, layer: &mut Vec<El>) {
    let rs = &br.rules;
    let q = rs.tag_search.text.trim().to_lowercase();
    let filtered: Vec<&rules_meta::MetaTagDef> = META_TAGS
        .iter()
        .filter(|t| (rs.tag_cat == TagCategory::All || t.category == rs.tag_cat) && (q.is_empty() || t.name.to_lowercase().contains(&q) || t.description().to_lowercase().contains(&q)))
        .collect();
    let active_idx = rs.tag_selected.min(filtered.len().saturating_sub(1));
    let active_tag = filtered.get(active_idx).copied();

    let cat_trigger = row()
        .key("mt-cat-dd")
        .role("combobox")
        .w(150.0)
        .h(30.0)
        .px(10.0)
        .center_y()
        .radius(k.radius.sm)
        .fill(k.c.surface_raised)
        .hover_fill(k.c.hover)
        .border(1.0, k.c.border_base)
        .cursor(Cursor::Pointer)
        .on_press(|s: &mut State| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.tag_cat_open = !b.rules.tag_cat_open;
            }
        })
        .child(text(rs.tag_cat.label()).font(k.ty.text_xsmall).color(k.c.text_base).grow(1.0).truncate())
        .child(icon(&icons::CHEVRON_DOWN, 12.0).color(k.c.text_muted));

    let search_in = TextInput::new(&rs.tag_search)
        .width(None)
        .label(t("Filtrar tags", "Filter tags"))
        .leading_icon(&icons::SEARCH)
        .placeholder(t("Filtrar Meta Tags…", "Filter Meta Tags…"))
        .on_edit(|s: &mut State| match &mut s.batch_rename {
            Some(b) => &mut b.rules.tag_search,
            None => &mut s.name_edit,
        })
        .build(k);

    let mut list = col().w_full().py(3.0).pl(3.0).pr(12.0).gap(1.0);
    for (idx, tag) in filtered.iter().enumerate() {
        let on = idx == active_idx;
        let name = tag.name;
        list = list.child(
            row()
                .key(format!("mt-item-{idx}"))
                .role("option")
                .w_full()
                .h(26.0)
                .px(10.0)
                .center_y()
                .radius(k.radius.sm)
                .instant_changes()
                .hover_motion(Anim::INSTANT, Anim::INSTANT)
                .transition_motion(Anim::INSTANT, Anim::INSTANT)
                .cursor(Cursor::Pointer)
                .fill(if on { k.c.text_accent } else { Color::TRANSPARENT })
                .hover_fill(if on { k.c.text_accent } else { k.c.hover })
                .on_press(move |s: &mut State| {
                    if let Some(b) = &mut s.batch_rename {
                        if b.rules.tag_selected == idx {
                            b.rules.editor.insert_meta_tag(target, name);
                            b.rules.tag_cat_open = false;
                            b.rules.editor.meta_target = None;
                        } else {
                            b.rules.tag_selected = idx;
                        }
                    }
                })
                .child(text(name).font(k.ty.text_xsmall).color(if on { Color::WHITE } else { k.c.text_base }).truncate()),
        );
    }

    let left_list = col()
        .grow(1.0)
        .w(0.0)
        .h(300.0)
        .radius(k.radius.sm)
        .fill(k.c.bg_base)
        .border(1.0, panel_border)
        .scroll(ScrollSpec { stick_bottom: false })
        .child(list)
        .scroll_bar_styled(ui::bar_style(k));

    let desc_box = col()
        .w(210.0)
        .h(300.0)
        .padding(12.0)
        .gap(8.0)
        .radius(k.radius.sm)
        .fill(k.c.bg_base)
        .border(1.0, panel_border)
        .child(text(active_tag.map_or("", |t| t.name)).font(k.ty.text_small_medium).color(k.c.text_accent).wrap().w_full().min_w(0.0))
        .child(text(active_tag.map_or("", |t| t.description())).font(k.ty.text_xsmall).color(k.c.text_base).wrap().w_full().min_w(0.0))
        .child(el().grow(1.0))
        .child(
            text(if active_tag.is_some_and(|t| t.file_only) {
                t("Observação: funciona apenas para arquivos locais, não pastas.", "Note: works for local files only, not folders.")
            } else {
                ""
            })
            .font(k.ty.caption())
            .color(k.c.text_faint)
            .wrap()
            .w_full()
            .min_w(0.0),
        );

    let insert_name: Arc<str> = active_tag.map_or("", |t| t.name).into();
    let back_to_editor = |s: &mut State| {
        if let Some(b) = &mut s.batch_rename {
            b.rules.tag_cat_open = false;
            b.rules.editor.meta_target = None;
        }
    };

    let actions = row()
        .w_full()
        .pt(4.0)
        .gap(8.0)
        .justify_end()
        .child(Button::new(t("Voltar", "Back")).variant(ButtonVariant::Neutral).on_press(back_to_editor).build(k).cursor(Cursor::Pointer))
        .child(
            Button::new(t("Inserir", "Insert"))
                .variant(ButtonVariant::Submit)
                .disabled(active_tag.is_none())
                .on_press(move |s: &mut State| {
                    if let Some(b) = &mut s.batch_rename {
                        b.rules.editor.insert_meta_tag(target, &insert_name);
                        b.rules.tag_cat_open = false;
                        b.rules.editor.meta_target = None;
                    }
                })
                .build(k)
                .default_button()
                .cursor(Cursor::Pointer),
        );

    let body = col()
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(10.0)
        .child(row().w_full().gap(8.0).child(cat_trigger).child(search_in.grow(1.0).w(0.0).min_w(0.0)))
        .child(row().w_full().gap(10.0).child(left_list).child(desc_box))
        .child(actions);

    Modal::new("rules-meta-tags", t("Meta Tags", "Meta Tags"))
        .open(true)
        .child(body)
        .width(560.0)
        .fit()
        .on_close(back_to_editor)
        .build(k, layer);

    if rs.tag_cat_open {
        let mut cats = col().w_full().py(4.0).pl(4.0).pr(12.0).gap(1.0);
        for cat in TagCategory::ALL {
            let act = rs.tag_cat == cat;
            cats = cats.child(
                row()
                    .role("option")
                    .w_full()
                    .h(26.0)
                    .px(8.0)
                    .center_y()
                    .radius(4.0)
                    .cursor(Cursor::Pointer)
                    .fill(if act { k.alpha(k.c.text_accent, 0.16) } else { Color::TRANSPARENT })
                    .hover_fill(k.c.hover)
                    .on_press(move |s: &mut State| {
                        if let Some(b) = &mut s.batch_rename {
                            b.rules.tag_cat = cat;
                            b.rules.tag_selected = 0;
                            b.rules.tag_cat_open = false;
                        }
                    })
                    .child(text(cat.label()).font(k.ty.text_xsmall).color(if act { k.c.text_accent } else { k.c.text_base })),
            );
        }
        let surf = col()
            .w(170.0)
            .max_h(260.0)
            .radius(6.0)
            .fill(k.c.bg_base)
            .border(1.0, k.c.border_base)
            .scroll(ScrollSpec { stick_bottom: false })
            .child(cats)
            .scroll_bar_styled(ui::bar_style(k))
            .modal(true, |s: &mut State| {
                if let Some(b) = &mut s.batch_rename {
                    b.rules.tag_cat_open = false;
                }
            });
        layer.push(rune_kit::popover::anchored(k, surf, "mt-cat-dd", rune_kit::Placement::BottomStart, 4.0));
    }
}

// ---------------------------------------------------------------------------------------------
// Modal: Predefinições (Presets)

fn render_presets_modal(k: &rune_kit::Kit, br: &BatchRename, panel_border: Color, layer: &mut Vec<El>) {
    let rs = &br.rules;
    let builtins = rules_rename::builtin_presets();
    let mut list = col().w_full().pr(12.0).gap(4.0);

    list = list.child(text(t("Predefinições prontas:", "Built-in presets:")).font(k.ty.caption()).color(k.c.text_muted));
    for (idx, p) in builtins.into_iter().enumerate() {
        let rules = p.rules.clone();
        list = list.child(
            row()
                .key(format!("bp-{idx}"))
                .w_full()
                .px(10.0)
                .py(6.0)
                .gap(8.0)
                .center_y()
                .radius(4.0)
                .fill(k.c.bg_base)
                .border(1.0, panel_border)
                .child(text(p.name).font(k.ty.text_xsmall_medium).color(k.c.text_base).grow(1.0).truncate())
                .child(text(tr!("{} regra(s)", "{} rule(s)", rules.len())).font(k.ty.caption()).color(k.c.text_faint))
                .child(
                    Button::new(t("Carregar", "Load"))
                        .variant(ButtonVariant::Neutral)
                        .on_press(move |s: &mut State| {
                            if let Some(b) = &mut s.batch_rename {
                                b.rules.rules = rules.clone();
                                b.rules.selected = (!b.rules.rules.is_empty()).then_some(0);
                                b.rules.popup = None;
                            }
                        })
                        .build(k)
                        .cursor(Cursor::Pointer),
                ),
        );
    }

    if !rs.presets.is_empty() {
        list = list.child(text(t("Salvas por você:", "Saved presets:")).font(k.ty.caption()).color(k.c.text_muted).pt(6.0));
        for (idx, p) in rs.presets.iter().enumerate() {
            let rules = p.rules.clone();
            list = list.child(
                row()
                    .key(format!("up-{idx}"))
                    .w_full()
                    .px(10.0)
                    .py(6.0)
                    .gap(8.0)
                    .center_y()
                    .radius(4.0)
                    .fill(k.c.bg_base)
                    .border(1.0, panel_border)
                    .child(text(p.name.clone()).font(k.ty.text_xsmall_medium).color(k.c.text_base).grow(1.0).truncate())
                    .child(
                        Button::new(t("Carregar", "Load"))
                            .variant(ButtonVariant::Neutral)
                            .on_press(move |s: &mut State| {
                                if let Some(b) = &mut s.batch_rename {
                                    b.rules.rules = rules.clone();
                                    b.rules.selected = (!b.rules.rules.is_empty()).then_some(0);
                                    b.rules.popup = None;
                                }
                            })
                            .build(k)
                            .cursor(Cursor::Pointer),
                    )
                    .child(
                        Button::new("×")
                            .variant(ButtonVariant::Danger)
                            .on_press(move |s: &mut State| {
                                if let Some(b) = &mut s.batch_rename
                                    && idx < b.rules.presets.len()
                                {
                                    b.rules.presets.remove(idx);
                                    rules_rename::save_presets(&b.rules.presets);
                                }
                            })
                            .build(k)
                            .cursor(Cursor::Pointer),
                    ),
            );
        }
    }

    let save_row = row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(
            TextInput::new(&rs.preset_name)
                .width(None)
                .label(t("Nome da predefinição", "Preset name"))
                .placeholder(t("Nome para salvar as regras atuais…", "Name to save current rules…"))
                .on_edit(|s: &mut State| match &mut s.batch_rename {
                    Some(b) => &mut b.rules.preset_name,
                    None => &mut s.name_edit,
                })
                .build(k)
                .grow(1.0)
                .w(0.0),
        )
        .child(
            Button::new(t("Salvar atual", "Save current"))
                .variant(ButtonVariant::Submit)
                .disabled(rs.rules.is_empty() || rs.preset_name.text.trim().is_empty())
                .on_press(|s: &mut State| {
                    if let Some(b) = &mut s.batch_rename {
                        let name = b.rules.preset_name.text.trim().to_string();
                        if !name.is_empty() && !b.rules.rules.is_empty() {
                            b.rules.presets.push(RulePreset { name, rules: b.rules.rules.clone() });
                            rules_rename::save_presets(&b.rules.presets);
                            b.rules.preset_name = TextEdit::new("");
                        }
                    }
                })
                .build(k)
                .cursor(Cursor::Pointer),
        );

    let body = col()
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(10.0)
        .child(col().w_full().max_h(260.0).min_h(0.0).scroll(ScrollSpec { stick_bottom: false }).child(list).scroll_bar_styled(ui::bar_style(k)))
        .child(save_row)
        .child(row().w_full().justify_end().child(Button::new(t("Fechar", "Close")).variant(ButtonVariant::Neutral).on_press(close_popup).build(k).cursor(Cursor::Pointer)));

    Modal::new("rules-presets-modal", t("Predefinições de Regras", "Rule Presets"))
        .open(true)
        .child(body)
        .width(500.0)
        .fit()
        .on_close(close_popup)
        .build(k, layer);
}

// ---------------------------------------------------------------------------------------------
// Modal: Analisar (step-by-step trace shared across Simple, Media, and Rules)

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_steps_analyze_modal(
    k: &rune_kit::Kit,
    modal_key: &'static str,
    title: &'static str,
    edit: &TextEdit,
    on_edit: fn(&mut State) -> &mut TextEdit,
    steps: Vec<(String, String, Option<String>)>,
    on_close: fn(&mut State),
    panel_border: Color,
    layer: &mut Vec<El>,
) {
    let sample_in = field_box(
        k,
        t("Texto de entrada", "Sample input"),
        t("Digite um nome de arquivo para testar…", "Type a filename to test…"),
        None,
        false,
        edit,
        on_edit,
        None,
    );

    let mut steps_col = col().w_full().pr(12.0).gap(4.0);
    for (idx, (label, out, err)) in steps.into_iter().enumerate() {
        let has_err = err.is_some();
        steps_col = steps_col.child(
            row()
                .key(format!("an-step-{idx}"))
                .w_full()
                .px(10.0)
                .py(6.0)
                .gap(8.0)
                .center_y()
                .radius(4.0)
                .fill(k.c.bg_base)
                .border(1.0, panel_border)
                .child(text(format!("{}. {label}", idx + 1)).font(k.ty.text_xsmall_medium).color(k.c.text_muted).w(150.0).truncate())
                .child(
                    text(err.unwrap_or(out))
                        .font(k.ty.text_xsmall)
                        .color(if has_err { k.c.danger.fg } else { k.c.text_base })
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                ),
        );
    }

    let body = col()
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(10.0)
        .child(
            col()
                .w_full()
                .gap(4.0)
                .child(text(t("Texto de entrada:", "Sample input:")).font(k.ty.caption()).color(k.c.text_muted))
                .child(sample_in),
        )
        .child(col().w_full().max_h(260.0).min_h(0.0).scroll(ScrollSpec { stick_bottom: false }).child(steps_col).scroll_bar_styled(ui::bar_style(k)))
        .child(row().w_full().justify_end().child(Button::new(t("Fechar", "Close")).variant(ButtonVariant::Neutral).on_press(on_close).build(k).cursor(Cursor::Pointer)));

    Modal::new(modal_key, title)
        .open(true)
        .child(body)
        .width(580.0)
        .fit()
        .on_close(on_close)
        .build(k, layer);
}

fn render_analyze_modal(k: &rune_kit::Kit, br: &BatchRename, panel_border: Color, layer: &mut Vec<El>) {
    let rs = &br.rules;
    let steps = if let Some(first) = br.items.first() {
        rules_rename::analyze_sample(&rs.rules, &rs.analyze_input.text, first, &br.loc)
    } else {
        Vec::new()
    };
    render_steps_analyze_modal(
        k,
        "rules-analyze-modal",
        t("Analisar Regras", "Analyze Rules"),
        &rs.analyze_input,
        |s| match &mut s.batch_rename {
            Some(b) => &mut b.rules.analyze_input,
            None => &mut s.name_edit,
        },
        steps,
        close_popup,
        panel_border,
        layer,
    );
}

fn render_stack_context_menu(k: &rune_kit::Kit, br: &BatchRename, layer: &mut Vec<El>) -> El {
    let anchor = el().key("rules-stack-menu-anchor").size(0.0, 0.0);
    let Some((clicked_idx, at)) = br.rules.stack_menu else { return anchor };
    let rs = &br.rules;
    let target = clicked_idx.or(rs.selected).filter(|&i| i < rs.rules.len());
    let has_rules = !rs.rules.is_empty();
    let can_up = target.is_some_and(|i| i > 0);
    let can_dn = target.is_some_and(|i| i + 1 < rs.rules.len());

    let act = |label: &str, ic: Option<&'static rune::Icon>, sc: &'static str, enabled: bool, f: Box<dyn Fn(&mut State)>| {
        let mut it = MenuItem::new(label).shortcut(sc).disabled(!enabled).on_select_tx(move |s: &mut State, _: &mut Tx| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.stack_menu = None;
            }
            f(s);
        });
        if let Some(i) = ic {
            it = it.icon(i);
        }
        it
    };

    context_menu("rules-stack-menu", anchor, Some(at))
        .min_width(245.0)
        .item(act(t("Adicionar Regra", "Add Rule"), Some(&icons::PLUS), "Ins", true, Box::new(|s| {
            s.open_rule_editor(None);
        })))
        .item(act(t("Adicionar regra (acima)", "Add rule (above)"), Some(&icons::PLUS), "", target.is_some(), Box::new(move |s| {
            if let Some(i) = target {
                s.open_rule_editor_at(None, Some(i));
            }
        })))
        .item(act(t("Adicionar regra (abaixo)", "Add rule (below)"), Some(&icons::PLUS), "", target.is_some(), Box::new(move |s| {
            if let Some(i) = target {
                s.open_rule_editor_at(None, Some(i + 1));
            }
        })))
        .item(act(t("Editar Regra", "Edit Rule"), Some(&icons::M_RENAME), "Enter", target.is_some(), Box::new(move |s| {
            if let Some(i) = target {
                s.open_rule_editor(Some(i));
            }
        })))
        .item(act(t("Duplicar Regra", "Duplicate Rule"), Some(&icons::M_COPY), "Shift+Ins", target.is_some(), Box::new(move |s| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.selected = target;
                b.rules.duplicate_selected();
            }
        })))
        .item(act(t("Remover Regra", "Remove Rule"), Some(&icons::M_DELETE), "Del", target.is_some(), Box::new(move |s| {
            if let Some(b) = &mut s.batch_rename {
                if !b.rules.all_selected {
                    b.rules.selected = target;
                }
                b.rules.remove_selected();
            }
        })))
        .item(act(t("Remover Todas as Regras", "Remove All Rules"), Some(&icons::M_EMPTY_TRASH), "Shift+Del", has_rules, Box::new(|s| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.clear_all();
            }
        })))
        .separator()
        .item(act(t("Mover Para Cima", "Move Up"), Some(&icons::UP), "Ctrl+Acima", can_up, Box::new(move |s| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.selected = target;
                b.rules.move_selected(-1);
            }
        })))
        .item(act(t("Mover Para Baixo", "Move Down"), Some(&icons::CHEVRON_DOWN), "Ctrl+Abaixo", can_dn, Box::new(move |s| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.selected = target;
                b.rules.move_selected(1);
            }
        })))
        .separator()
        .item(act(t("Selecionar Tudo", "Select All"), None, "Ctrl+A", has_rules, Box::new(|s| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.select_all();
            }
        })))
        .item(
            MenuItem::new(t("Marcar Todos", "Mark All"))
                .checked(true)
                .shortcut("Shift+M")
                .disabled(!has_rules)
                .on_select_tx(|s: &mut State, _: &mut Tx| {
                    if let Some(b) = &mut s.batch_rename {
                        b.rules.stack_menu = None;
                        b.rules.set_all_enabled(true);
                    }
                }),
        )
        .item(
            MenuItem::new(t("Desmarcar Todos", "Unmark All"))
                .checked(false)
                .shortcut("Shift+U")
                .disabled(!has_rules)
                .on_select_tx(|s: &mut State, _: &mut Tx| {
                    if let Some(b) = &mut s.batch_rename {
                        b.rules.stack_menu = None;
                        b.rules.set_all_enabled(false);
                    }
                }),
        )
        .separator()
        .item(act(t("Exportar para Área de Transferência", "Export to Clipboard"), Some(&icons::M_COPY), "", has_rules, Box::new(|s| {
            if let Some(b) = &s.batch_rename {
                let txt = b.rules.export_rules_text();
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    let _ = cb.set_text(txt);
                }
            }
        })))
        .item(act(t("Comentário...", "Comment..."), Some(&icons::M_PROPERTIES), "Shift+Enter", target.is_some(), Box::new(move |s| {
            if let Some(b) = &mut s.batch_rename
                && let Some(i) = target
            {
                b.rules.open_comment(i);
            }
        })))
        .on_close(|s: &mut State| {
            if let Some(b) = &mut s.batch_rename {
                b.rules.stack_menu = None;
            }
        })
        .build(k, layer)
}

fn render_comment_modal(k: &rune_kit::Kit, br: &BatchRename, idx: usize, layer: &mut Vec<El>) {
    let rs = &br.rules;
    let body = col()
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(12.0)
        .child(text_box(k, t("Comentário", "Comment"), t("Nota opcional sobre esta regra…", "Optional note about this rule…"), &rs.comment_edit, |s| {
            match &mut s.batch_rename {
                Some(b) => &mut b.rules.comment_edit,
                None => &mut s.name_edit,
            }
        }))
        .child(
            row()
                .w_full()
                .gap(8.0)
                .justify_end()
                .child(Button::new(t("Cancelar", "Cancel")).variant(ButtonVariant::Neutral).on_press(close_popup).build(k).cursor(Cursor::Pointer))
                .child(
                    Button::new(t("Salvar", "Save"))
                        .variant(ButtonVariant::Submit)
                        .on_press(move |s: &mut State| {
                            if let Some(b) = &mut s.batch_rename {
                                if let Some(rule) = b.rules.rules.get_mut(idx) {
                                    rule.comment = b.rules.comment_edit.text.trim().to_string();
                                }
                                b.rules.popup = None;
                            }
                        })
                        .build(k)
                        .default_button()
                        .cursor(Cursor::Pointer),
                ),
        );

    Modal::new("rules-comment-modal", t("Comentário da Regra", "Rule Comment"))
        .open(true)
        .child(body)
        .width(420.0)
        .fit()
        .on_close(close_popup)
        .build(k, layer);
}
