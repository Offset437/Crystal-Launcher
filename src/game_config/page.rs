//! The Graphics and Sound & interface tabs.
use super::model;
use super::store::Target;
use super::{Msg, Section, State, Tone};
use crate::i18n::{t, tf};
use crate::icons::{icon, Icon};
use crate::ui::{colors, primary, quiet, scale, Colors, BODY, SEMI};
use crate::{App, Message};
use iced::widget::{
    button, column, container, pick_list, row, slider, stack, text, text_input, toggler, Space,
};
use iced::{Alignment, Background, Border, Color, Element, Length, Shadow, Theme, Vector};

type El<'a> = Element<'a, Message>;

fn gc(msg: Msg) -> Message {
    Message::GameConfig(msg)
}

/// Sizes for the current window scale.
#[derive(Clone, Copy)]
struct Ui {
    c: Colors,
    s: f32,
    light: bool,
}

impl Ui {
    fn control_w(self) -> f32 {
        262.0 * self.s
    }
    fn field_h(self) -> f32 {
        36.0 * self.s
    }
}

pub(super) fn view(app: &App, section: Section) -> El<'_> {
    let light = app.settings.light_theme;
    let ui = Ui {
        c: colors(light),
        s: scale(app),
        light,
    };
    let st = &app.game_config;
    let s = ui.s;
    let body: El<'_> = if !st.loaded {
        container(
            text(t("Reading the game's settings…"))
                .font(BODY)
                .size(13.0 * s)
                .color(ui.c.sub),
        )
        .padding(24.0 * s)
        .into()
    } else {
        let (left, right): (Vec<El<'_>>, Vec<El<'_>>) = match section {
            Section::Graphics => (
                vec![display_card(st, ui), performance_card(st, ui)],
                vec![quality_card(st, ui)],
            ),
            Section::Sound => (
                vec![sound_card(st, ui), screenshot_card(st, ui)],
                vec![interface_card(st, ui)],
            ),
        };
        row![
            column(left).spacing(20.0 * s).width(Length::FillPortion(1)),
            column(right).spacing(20.0 * s).width(Length::FillPortion(1)),
        ]
        .spacing(20.0 * s)
        .into()
    };
    let mut page = column![action_bar(st, ui)].spacing(16.0 * s);
    if let Some((tone, message)) = &st.notice {
        page = page.push(notice(*tone, message, ui));
    }
    container(page.push(body))
        .padding(iced::Padding::ZERO.top(4.0 * s).right(16.0 * s).bottom(28.0 * s))
        .width(Length::Fill)
        .into()
}

// ---- the action bar ----

fn action_bar(st: &State, ui: Ui) -> El<'_> {
    let (c, s) = (ui.c, ui.s);
    let pending = st.pending().len();
    let locked = pending > 0;
    // Where the values go: a segmented switch between the launch targets.
    let targets: Vec<_> = st.regions.iter().map(|r| Target::Retail(*r)).collect();
    let mut segments = row![].spacing(2.0 * s);
    for target in targets {
        let active = target == st.target;
        let label = text(target.label())
            .font(if active { SEMI } else { BODY })
            .size(12.5 * s);
        let mut b = crate::controller_nav::button(label)
            .padding([6.0 * s, 12.0 * s])
            .style(move |_: &Theme, status| {
                let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
                button::Style {
                    background: if active {
                        Some(c.card.into())
                    } else if hovered {
                        Some(c.select.into())
                    } else {
                        None
                    },
                    text_color: if active {
                        c.accent
                    } else if status == button::Status::Disabled {
                        Color { a: 0.55, ..c.sub }
                    } else {
                        c.sub
                    },
                    border: Border {
                        color: if active { c.line } else { Color::TRANSPARENT },
                        width: 1.0,
                        radius: (6.0 * s).into(),
                    },
                    shadow: if active {
                        Shadow {
                            color: Color::from_rgba8(0, 0, 0, if ui.light { 0.06 } else { 0.25 }),
                            offset: Vector::new(0.0, 1.0),
                            blur_radius: 3.0,
                        }
                    } else {
                        Shadow::default()
                    },
                    ..Default::default()
                }
            });
        if !active && !locked {
            b = b.on_press(gc(Msg::Target(target)));
        }
        segments = segments.push(b);
    }
    let segmented = container(segments).padding(3.0 * s).style(move |_| container::Style {
        background: Some(c.field.into()),
        border: Border {
            color: c.line,
            width: 1.0,
            radius: (8.0 * s).into(),
        },
        ..Default::default()
    });

    let status: El<'_> = if pending > 0 {
        row![
            dot(c.accent, 8.0 * s),
            text(if pending == 1 {
                t("1 unsaved change").to_owned()
            } else {
                tf("{count} unsaved changes", &[("count", &pending)])
            })
            .font(SEMI)
            .size(13.0 * s)
            .color(c.ink)
        ]
        .spacing(8.0 * s)
        .align_y(Alignment::Center)
        .into()
    } else {
        row![
            icon(Icon::CheckCircle, 15.0 * s, c.sub),
            text(t("No unsaved changes")).font(BODY).size(13.0 * s).color(c.sub)
        ]
        .spacing(7.0 * s)
        .align_y(Alignment::Center)
        .into()
    };

    let small = |label: &'static str, msg: Option<Msg>| {
        let b = crate::controller_nav::button(text(label).font(BODY).size(13.0 * s))
            .padding([8.0 * s, 12.0 * s])
            .style(quiet);
        match msg {
            Some(m) => b.on_press(gc(m)),
            None => b,
        }
    };
    let backup: El<'_> = if st.backups.is_empty() {
        small(t("No backups yet"), None).into()
    } else {
        let picker = crate::controller_nav::pick_list(st.backups.as_slice(), None::<super::BackupEntry>, |b| {
            gc(Msg::LoadBackup(b))
        })
        .placeholder(t("Restore backup"))
        .width(162.0 * s)
        .font(BODY)
        .text_size(13.0 * s)
        .padding(iced::Padding { top: 8.0 * s, right: 26.0 * s, bottom: 8.0 * s, left: 12.0 * s })
        .handle(pick_list::Handle::None)
        .style(move |_, status| {
            let hovered = matches!(status, pick_list::Status::Hovered | pick_list::Status::Opened { .. });
            pick_list::Style {
                text_color: Color::TRANSPARENT,
                placeholder_color: Color::TRANSPARENT,
                handle_color: Color::TRANSPARENT,
                background: if hovered { c.select.into() } else { Background::Color(Color::TRANSPARENT) },
                border: Border {
                    radius: 5.0.into(),
                    ..Default::default()
                },
            }
        })
        .menu_style(menu_style(ui));
        stack![
            picker,
            container(
                row![
                    text(t("Restore backup")).font(BODY).size(13.0 * s).color(c.accent).width(Length::Fill),
                    icon(Icon::ChevronDown, 13.0 * s, c.accent)
                ]
                .align_y(Alignment::Center)
            )
            .padding(iced::Padding::ZERO.left(12.0 * s).right(11.0 * s))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_y(Alignment::Center)
        ]
        .width(162.0 * s)
        .into()
    };
    let apply_enabled = pending > 0 && st.writable;
    let apply = crate::controller_nav::button(
        container(text(t("Apply")).font(SEMI).size(13.5 * s)).center_x(Length::Shrink),
    )
    .padding([8.0 * s, 22.0 * s])
    .style(primary);
    let apply = if apply_enabled {
        apply.on_press(gc(Msg::Apply))
    } else {
        apply
    };

    let location = match st.target {
        Target::Native => tf(
            "Saved to {path} · every account the native client launches uses it",
            &[("path", &st.paths.location(st.target))],
        ),
        Target::Retail(_) => tf(
            "{path} · read by the official client, Windower and Ashita",
            &[("path", &st.paths.location(st.target))],
        ),
    };
    let mut details = column![text(location)
        .font(BODY)
        .size(11.5 * s)
        .color(c.sub)]
    .spacing(4.0 * s);
    if matches!(st.target, Target::Retail(_)) {
        details = details.push(text(t("Menu sizes applied here are kept for Vanilla and Ashita. Windower uses its selected profile's resolution and UI scale."))
            .font(BODY).size(11.5 * s).color(c.sub));
    }
    if locked && st.regions.len() > 0 {
        details = details.push(
            text(t("Apply or discard your changes to switch where settings are saved."))
                .font(BODY)
                .size(11.5 * s)
                .color(c.sub),
        );
    }
    if !st.writable {
        details = details.push(
            row![
                icon(Icon::AlertCircle, 13.0 * s, tone_color(Tone::Warning, ui)),
                text(t("Run Crystal Launcher as administrator to save registry settings."))
                    .font(SEMI)
                    .size(11.5 * s)
                    .color(tone_color(Tone::Warning, ui))
            ]
            .spacing(6.0 * s)
            .align_y(Alignment::Center),
        );
    }

    container(
        column![
            row![
                text(t("Settings for")).font(BODY).size(13.0 * s).color(c.sub),
                Space::new().width(6.0 * s),
                segmented,
                Space::new().width(Length::Fill),
                status,
                Space::new().width(10.0 * s),
                small(t("Restore defaults"), Some(Msg::Defaults)),
                backup,
                small(t("Discard"), (pending > 0).then_some(Msg::Discard)),
                Space::new().width(4.0 * s),
                apply,
            ]
            .spacing(6.0 * s)
            .align_y(Alignment::Center),
            divider(c),
            details,
        ]
        .spacing(13.0 * s),
    )
    .padding([16.0 * s, 20.0 * s])
    .width(Length::Fill)
    .style(move |_| card_style(c, s))
    .into()
}

fn tone_color(tone: Tone, ui: Ui) -> Color {
    let rgb = |v: u32| Color::from_rgb8((v >> 16) as u8, (v >> 8) as u8, v as u8);
    match (tone, ui.light) {
        (Tone::Info, _) => ui.c.accent,
        (Tone::Success, false) => rgb(0x7CC4A6),
        (Tone::Success, true) => rgb(0x2F7A5F),
        (Tone::Warning, false) => rgb(0xE0B466),
        (Tone::Warning, true) => rgb(0x8F6420),
        (Tone::Error, false) => rgb(0xE88A8A),
        (Tone::Error, true) => rgb(0xB03E3E),
    }
}

fn notice<'a>(tone: Tone, message: &'a str, ui: Ui) -> El<'a> {
    let (c, s) = (ui.c, ui.s);
    let color = tone_color(tone, ui);
    let glyph = match tone {
        Tone::Success => Icon::CheckCircle,
        _ => Icon::AlertCircle,
    };
    container(
        row![
            icon(glyph, 16.0 * s, color),
            text(message).font(BODY).size(13.0 * s).color(c.ink).width(Length::Fill),
            crate::controller_nav::button(icon(Icon::Close, 14.0 * s, c.sub))
                .padding(4.0 * s)
                .style(quiet)
                .on_press(gc(Msg::DismissNotice)),
        ]
        .spacing(10.0 * s)
        .align_y(Alignment::Center),
    )
    .padding([10.0 * s, 14.0 * s])
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(Color { a: if ui.light { 0.08 } else { 0.12 }, ..color }.into()),
        border: Border {
            color: Color { a: 0.45, ..color },
            width: 1.0,
            radius: (8.0 * s).into(),
        },
        ..Default::default()
    })
    .into()
}

// ---- cards ----

fn card_style(c: Colors, s: f32) -> container::Style {
    container::Style {
        background: Some(c.panel.into()),
        text_color: Some(c.ink),
        border: Border {
            color: c.line,
            width: 1.0,
            radius: (9.0 * s).into(),
        },
        ..Default::default()
    }
}

fn divider<'a>(c: Colors) -> El<'a> {
    container(Space::new())
        .width(Length::Fill)
        .height(1)
        .style(move |_| container::Style {
            background: Some(c.line.into()),
            ..Default::default()
        })
        .into()
}

fn dot<'a>(color: Color, size: f32) -> El<'a> {
    container(Space::new())
        .width(size)
        .height(size)
        .style(move |_| container::Style {
            background: Some(color.into()),
            border: Border {
                radius: (size / 2.0).into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

fn card<'a>(title: &'a str, subtitle: &'a str, rows: Vec<El<'a>>, ui: Ui) -> El<'a> {
    let (c, s) = (ui.c, ui.s);
    let mut body = column![
        text(title).font(SEMI).size(16.0 * s).color(c.ink),
        Space::new().height(3.0 * s),
        text(subtitle).font(BODY).size(12.5 * s).color(c.sub),
        Space::new().height(10.0 * s),
    ];
    for r in rows {
        body = body.push(divider(c));
        body = body.push(r);
    }
    container(body)
        .padding(iced::Padding::ZERO.top(18.0 * s).left(22.0 * s).right(22.0 * s).bottom(6.0 * s))
        .width(Length::Fill)
        .style(move |_| card_style(c, s))
        .into()
}

/// A setting: its name and a line of help on the left, the control on the
/// right. A changed setting carries an accent dot.
fn setting<'a>(label: &'a str, help: &'a str, changed: bool, control: El<'a>, ui: Ui) -> El<'a> {
    let (c, s) = (ui.c, ui.s);
    let mut name = row![text(label).font(BODY).size(14.0 * s).color(c.ink)]
        .spacing(8.0 * s)
        .align_y(Alignment::Center);
    if changed {
        name = name.push(dot(c.accent, 7.0 * s));
    }
    container(
        row![
            column![name, text(help).font(BODY).size(12.0 * s).color(c.sub)]
                .spacing(3.0 * s)
                .width(Length::Fill),
            container(control)
                .width(ui.control_w())
                .align_x(Alignment::End),
        ]
        .spacing(20.0 * s)
        .align_y(Alignment::Center),
    )
    .padding([13.0 * s, 0.0])
    .into()
}

fn menu_style(ui: Ui) -> impl Fn(&Theme) -> iced::overlay::menu::Style {
    let c = ui.c;
    let light = ui.light;
    move |_| iced::overlay::menu::Style {
        background: c.panel.into(),
        border: Border {
            color: c.line,
            width: 1.0,
            radius: 6.0.into(),
        },
        text_color: c.ink,
        selected_text_color: c.ink,
        selected_background: c.select.into(),
        shadow: Shadow {
            color: Color::from_rgba8(0, 0, 0, if light { 0.12 } else { 0.35 }),
            offset: Vector::new(0.0, 6.0),
            blur_radius: 18.0,
        },
    }
}

/// A labelled choice for a dropdown.
#[derive(Clone, Debug)]
struct Choice<T> {
    value: T,
    label: String,
}

impl<T: PartialEq> PartialEq for Choice<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T> std::fmt::Display for Choice<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

fn dropdown<'a, T: Clone + PartialEq + 'a>(
    options: Vec<Choice<T>>,
    selected: T,
    on_select: impl Fn(T) -> Msg + 'a,
    ui: Ui,
) -> El<'a> {
    let (c, s) = (ui.c, ui.s);
    let current = options.iter().find(|o| o.value == selected).cloned();
    let selected_label = current.as_ref().map(|o| o.label.clone());
    let text_size = 13.5 * s;
    let pad_y = ((ui.field_h() - text_size * 1.3) / 2.0).max(4.0);
    let light = ui.light;
    let picker = crate::controller_nav::pick_list(options, current, move |o: Choice<T>| gc(on_select(o.value)))
        .font(BODY)
        .text_size(text_size)
        .padding(iced::Padding { top: pad_y, right: 32.0 * s, bottom: pad_y, left: 12.0 * s })
        .width(Length::Fill)
        .handle(pick_list::Handle::None)
        .style(move |_, status| {
            let c = colors(light);
            let active = matches!(status, pick_list::Status::Hovered | pick_list::Status::Opened { .. });
            pick_list::Style {
                text_color: Color::TRANSPARENT,
                placeholder_color: Color::TRANSPARENT,
                handle_color: Color::TRANSPARENT,
                background: c.field.into(),
                border: Border {
                    color: if active { c.sub } else { c.line },
                    width: 1.0,
                    radius: (6.0 * s).into(),
                },
            }
        })
        .menu_style(menu_style(ui));
    // The pick_list's own label is hidden and drawn on top instead: tiny-skia does not clip a
    // pick_list's text to an enclosing scrollable, so it would show over the play bar.
    let label = selected_label.unwrap_or_default();
    stack![
        picker,
        container(
            row![
                text(label).font(BODY).size(text_size).color(c.ink).width(Length::Fill),
                icon(Icon::ChevronDown, 14.0 * s, c.sub)
            ]
            .align_y(Alignment::Center)
        )
        .padding(iced::Padding::ZERO.left(12.0 * s).right(11.0 * s))
        .width(Length::Fill)
        .height(ui.field_h())
        .align_y(Alignment::Center)
    ]
    .width(Length::Fill)
    .into()
}

fn enum_dropdown<'a>(st: &State, key: &'static str, table: &[(u32, &str)], ui: Ui) -> El<'a> {
    let current = st.dword(key);
    let mut options: Vec<Choice<u32>> = table
        .iter()
        .map(|(v, l)| Choice {
            value: *v,
            label: (*l).to_owned(),
        })
        .collect();
    if !options.iter().any(|o| o.value == current) {
        options.push(Choice {
            value: current,
            label: tf("Other ({value})", &[("value", &current)]),
        });
    }
    dropdown(options, current, move |v| Msg::Set(key, v), ui)
}

fn switch<'a>(st: &State, key: &'static str, ui: Ui) -> El<'a> {
    let (on, s, light) = (st.dword(key) != 0, ui.s, ui.light);
    crate::controller_nav::toggler(on)
        .on_toggle(move |v| gc(Msg::Set(key, v as u32)))
        .size(21.0 * s)
        .style(move |_, status| {
            let c = colors(light);
            let (toggled, hovered) = match status {
                toggler::Status::Active { is_toggled } => (is_toggled, false),
                toggler::Status::Hovered { is_toggled } => (is_toggled, true),
                toggler::Status::Disabled { is_toggled } => (is_toggled, false),
            };
            toggler::Style {
                background: if toggled { c.play.into() } else { c.field.into() },
                background_border_width: 1.0,
                background_border_color: if toggled {
                    c.play
                } else if hovered {
                    c.sub
                } else {
                    c.line
                },
                foreground: if toggled { c.play_text.into() } else { c.sub.into() },
                foreground_border_width: 0.0,
                foreground_border_color: Color::TRANSPARENT,
                text_color: None,
                border_radius: None,
                padding_ratio: 0.2,
            }
        })
        .into()
}

fn toggle_row<'a>(st: &State, label: &'a str, help: &'a str, key: &'static str, ui: Ui) -> El<'a> {
    setting(label, help, st.changed(&[key]), switch(st, key, ui), ui)
}

fn slider_style(ui: Ui) -> impl Fn(&Theme, slider::Status) -> slider::Style {
    let (s, light) = (ui.s, ui.light);
    move |_, status| {
        let c = colors(light);
        let grabbed = matches!(status, slider::Status::Hovered | slider::Status::Dragged);
        slider::Style {
            rail: slider::Rail {
                backgrounds: (c.play.into(), c.line.into()),
                width: 4.0 * s,
                border: Border {
                    radius: (2.0 * s).into(),
                    ..Default::default()
                },
            },
            handle: slider::Handle {
                shape: slider::HandleShape::Circle {
                    radius: if grabbed { 8.5 * s } else { 7.5 * s },
                },
                background: c.play.into(),
                border_width: 2.5 * s,
                border_color: c.panel,
            },
        }
    }
}

fn value_label<'a>(value: String, ui: Ui) -> El<'a> {
    container(text(value).font(SEMI).size(13.0 * ui.s).color(ui.c.ink))
        .width(46.0 * ui.s)
        .align_x(Alignment::End)
        .into()
}

fn info<'a>(value: &'a str, ui: Ui) -> El<'a> {
    text(value).font(BODY).size(13.0 * ui.s).color(ui.c.sub).into()
}

// ---- resolution lists ----

fn resolution_label(w: u32, h: u32) -> String {
    format!("{w} × {h}")
}

fn window_choices(st: &State) -> Vec<Choice<(u32, u32)>> {
    let current = (st.dword(model::WINDOW_W), st.dword(model::WINDOW_H));
    let mut list: Vec<Choice<(u32, u32)>> = st
        .modes
        .iter()
        .map(|&(w, h)| Choice {
            value: (w, h),
            label: if (w, h) == st.desktop {
                tf("{resolution}   ·  desktop", &[("resolution", &resolution_label(w, h))])
            } else {
                resolution_label(w, h)
            },
        })
        .collect();
    if !list.iter().any(|c| c.value == current) {
        list.insert(
            0,
            Choice {
                value: current,
                label: tf("{resolution}   ·  current", &[("resolution", &resolution_label(current.0, current.1))]),
            },
        );
    }
    list
}

fn menu_choices(st: &State) -> Vec<Choice<(u32, u32)>> {
    let (w, h) = (st.dword(model::WINDOW_W), st.dword(model::WINDOW_H));
    let current = (st.dword(model::MENU_W), st.dword(model::MENU_H));
    let mut list: Vec<Choice<(u32, u32)>> = Vec::new();
    if matches!(st.target, Target::Retail(_)) {
        list.push(Choice { value: (0, 0), label: t("Automatic · use launch profile").into() });
    }
    for scale in model::MENU_SCALES {
        let (mw, mh) = model::menu_for(w, h, scale);
        if mw < model::MIN_MENU.0 || mh < model::MIN_MENU.1 || list.iter().any(|c| c.value == (mw, mh)) {
            continue;
        }
        list.push(Choice {
            value: (mw, mh),
            label: tf(
                "{resolution}  ·  interface {scale}×",
                &[("resolution", &resolution_label(mw, mh)), ("scale", &trim_float(scale))],
            ),
        });
    }
    if !list.iter().any(|c| c.value == current) {
        let scale = model::scale_of(h, current.1);
        list.insert(
            0,
            Choice {
                value: current,
                label: tf(
                    "{resolution}  ·  interface {scale}×",
                    &[
                        ("resolution", &resolution_label(current.0, current.1)),
                        ("scale", &trim_float((scale * 100.0).round() / 100.0)),
                    ],
                ),
            },
        );
    }
    list
}

fn trim_float(v: f32) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn background_choices(st: &State) -> Vec<Choice<(u32, u32)>> {
    let current = (st.dword(model::BACKGROUND_W), st.dword(model::BACKGROUND_H));
    let window = (st.dword(model::WINDOW_W), st.dword(model::WINDOW_H));
    let mut list = vec![
        Choice {
            value: (1024, 1024),
            label: t("1024 × 1024  ·  low").into(),
        },
        Choice {
            value: (2048, 2048),
            label: t("2048 × 2048  ·  classic").into(),
        },
        Choice {
            value: (4096, 4096),
            label: t("4096 × 4096  ·  recommended").into(),
        },
    ];
    if !list.iter().any(|c| c.value == window) {
        list.push(Choice {
            value: window,
            label: tf("{resolution}  ·  match window", &[("resolution", &resolution_label(window.0, window.1))]),
        });
    }
    if !list.iter().any(|c| c.value == current) {
        list.insert(
            0,
            Choice {
                value: current,
                label: tf("{resolution}  ·  current", &[("resolution", &resolution_label(current.0, current.1))]),
            },
        );
    }
    list
}

// ---- Graphics ----

fn display_card(st: &State, ui: Ui) -> El<'_> {
    let window = (st.dword(model::WINDOW_W), st.dword(model::WINDOW_H));
    let menu = (st.dword(model::MENU_W), st.dword(model::MENU_H));
    card(
        t("Display"),
        t("The game window and how large the interface is drawn."),
        vec![
            setting(
                t("Window mode"),
                t("How the game window sits on your desktop."),
                st.changed(&[model::WINDOW_MODE]),
                enum_dropdown(st, model::WINDOW_MODE, &model::window_modes(), ui),
                ui,
            ),
            setting(
                t("Window resolution"),
                t("The size the game renders at. Your monitor's modes are listed."),
                st.changed(&[model::WINDOW_W, model::WINDOW_H]),
                dropdown(window_choices(st), window, |(w, h)| Msg::Window(w, h), ui),
                ui,
            ),
            setting(
                t("Menu resolution"),
                t("The size menus are laid out at. A smaller size makes the interface larger."),
                st.changed(&[model::MENU_W, model::MENU_H]),
                dropdown(menu_choices(st), menu, |(w, h)| Msg::Menu(w, h), ui),
                ui,
            ),
            toggle_row(
                st,
                t("Maintain aspect ratio"),
                t("Keep the picture's proportions instead of stretching it to the window."),
                model::KEEP_ASPECT,
                ui,
            ),
        ],
        ui,
    )
}

fn quality_card(st: &State, ui: Ui) -> El<'_> {
    let background = (st.dword(model::BACKGROUND_W), st.dword(model::BACKGROUND_H));
    card(
        t("Image quality"),
        t("Texture and scenery detail. Higher settings use more video memory."),
        vec![
            setting(
                t("Background resolution"),
                t("Detail of the surrounding scenery. 4096 suits most modern graphics cards."),
                st.changed(&[model::BACKGROUND_W, model::BACKGROUND_H]),
                dropdown(background_choices(st), background, |(w, h)| Msg::Background(w, h), ui),
                ui,
            ),
            setting(
                t("Mip mapping"),
                t("Smooths textures in the distance. Higher levels look cleaner."),
                st.changed(&[model::MIP_MAPPING]),
                enum_dropdown(st, model::MIP_MAPPING, &model::mip_levels(), ui),
                ui,
            ),
            setting(
                t("Texture compression"),
                t("Uncompressed textures are the sharpest."),
                st.changed(&[model::TEXTURE_COMPRESSION]),
                enum_dropdown(st, model::TEXTURE_COMPRESSION, &model::texture_levels(), ui),
                ui,
            ),
            setting(
                t("Map compression"),
                t("Uncompressed maps are easier to read."),
                st.changed(&[model::MAP_COMPRESSION]),
                enum_dropdown(st, model::MAP_COMPRESSION, &model::map_levels(), ui),
                ui,
            ),
            setting(
                t("Environment animation"),
                t("Moving water, foliage and other scenery."),
                st.changed(&[model::ENV_ANIMATION]),
                enum_dropdown(st, model::ENV_ANIMATION, &model::animation_levels(), ui),
                ui,
            ),
            toggle_row(
                st,
                t("Bump mapping"),
                t("Surface relief on terrain, walls and armor."),
                model::BUMP_MAPPING,
                ui,
            ),
        ],
        ui,
    )
}

fn performance_card(st: &State, ui: Ui) -> El<'_> {
    let gamma = model::gamma_of(st.dword(model::GAMMA));
    let s = ui.s;
    let gamma_control: El<'_> = row![
        crate::controller_nav::slider(-1.0..=1.0, gamma, |g| gc(Msg::Gamma(g)))
            .step(0.05f32)
            .default(0.0f32)
            .width(Length::Fill)
            .style(slider_style(ui)),
        value_label(
            if gamma.abs() < 0.005 {
                "0".into()
            } else {
                format!("{gamma:+.2}")
            },
            ui
        ),
    ]
    .spacing(12.0 * s)
    .align_y(Alignment::Center)
    .into();
    let (fps, fps_help) = match st.target {
        Target::Native => (
            "30 fps",
            t("The native client runs the game at 30 frames a second, as retail ships."),
        ),
        Target::Retail(_) => (
            t("Set by add-ons"),
            t("The game has no frame-rate option; Windower and Ashita set it in their own settings."),
        ),
    };
    card(
        t("Brightness & performance"),
        t("Picture brightness and options for smoother play."),
        vec![
            setting(
                t("Gamma"),
                t("Overall brightness. 0 is the game's default; double-click resets."),
                st.changed(&[model::GAMMA]),
                gamma_control,
                ui,
            ),
            toggle_row(
                st,
                t("Graphics stabilization"),
                t("A compatibility option for drivers that flicker or stutter. Usually off."),
                model::STABILIZATION,
                ui,
            ),
            setting(t("Frame rate"), fps_help, false, info(fps, ui), ui),
        ],
        ui,
    )
}

// ---- Sound & interface ----

fn sound_card(st: &State, ui: Ui) -> El<'_> {
    let s = ui.s;
    let max = st
        .dword(model::MAX_SOUNDS)
        .clamp(model::MAX_SOUNDS_RANGE.0, model::MAX_SOUNDS_RANGE.1);
    let max_control: El<'_> = row![
        crate::controller_nav::slider(
            model::MAX_SOUNDS_RANGE.0..=model::MAX_SOUNDS_RANGE.1,
            max,
            |v| gc(Msg::Set(model::MAX_SOUNDS, v))
        )
        .step(1u32)
        .width(Length::Fill)
        .style(slider_style(ui)),
        value_label(max.to_string(), ui),
    ]
    .spacing(12.0 * s)
    .align_y(Alignment::Center)
    .into();
    card(
        t("Sound"),
        t("Music, effects and when the game plays them."),
        vec![
            toggle_row(st, t("Sound"), t("All game music and sound effects."), model::SOUND, ui),
            setting(
                t("Maximum sounds"),
                t("How many effects can play at once, from 12 to 20."),
                st.changed(&[model::MAX_SOUNDS]),
                max_control,
                ui,
            ),
            toggle_row(
                st,
                t("Play sound in the background"),
                t("Keep the audio on while another window is in front."),
                model::SOUND_UNFOCUSED,
                ui,
            ),
        ],
        ui,
    )
}

fn interface_card(st: &State, ui: Ui) -> El<'_> {
    let font = match st.target {
        // Uncompressed and High quality stop the native client at startup.
        Target::Native => setting(
            t("Font type"),
            t("The native client starts only with compressed fonts; Uncompressed and High quality crash it at startup."),
            st.changed(&[model::FONT_TYPE]),
            info(t("Compressed"), ui),
            ui,
        ),
        Target::Retail(_) => setting(
            t("Font type"),
            t("How on-screen text is drawn. High quality is the sharpest."),
            st.changed(&[model::FONT_TYPE]),
            enum_dropdown(st, model::FONT_TYPE, &model::font_types(), ui),
            ui,
        ),
    };
    card(
        t("Interface"),
        t("The cursor, text and what you see as the game starts."),
        vec![
            toggle_row(
                st,
                t("Hardware mouse cursor"),
                t("Use the Windows cursor, which stays smooth at any frame rate."),
                model::HARDWARE_MOUSE,
                ui,
            ),
            font,
            toggle_row(
                st,
                t("Opening movie"),
                t("Play the introduction movie when the game starts."),
                model::OPENING_MOVIE,
                ui,
            ),
            toggle_row(
                st,
                t("Simplified character creation"),
                t("Lighter visuals on the character creation screen."),
                model::SIMPLE_CREATION,
                ui,
            ),
        ],
        ui,
    )
}

fn screenshot_card(st: &State, ui: Ui) -> El<'_> {
    let (c, s, light) = (ui.c, ui.s, ui.light);
    let dir = st.text(model::SCREENSHOT_DIR);
    let input = text_input(t("The game's own screenshot folder"), &crate::privacy::mask_path(dir))
        .on_input_maybe(crate::privacy::editable().then_some(|v| gc(Msg::ScreenshotDir(crate::privacy::unmask_path(v)))))
        .font(BODY)
        .size(13.5 * s)
        .padding([9.0 * s, 12.0 * s])
        .width(Length::Fill)
        .style(move |_, status| {
            let c = colors(light);
            let focused = matches!(status, text_input::Status::Focused { .. });
            let hovered = matches!(status, text_input::Status::Hovered);
            text_input::Style {
                background: c.field.into(),
                border: Border {
                    color: if focused {
                        c.accent
                    } else if hovered {
                        c.sub
                    } else {
                        c.line
                    },
                    width: 1.0,
                    radius: (6.0 * s).into(),
                },
                icon: c.sub,
                placeholder: c.sub,
                value: c.ink,
                selection: c.select,
            }
        });
    let mut field = row![input].spacing(8.0 * s).align_y(Alignment::Center);
    if !dir.is_empty() {
        field = field.push(
            crate::controller_nav::button(text(t("Use default")).font(BODY).size(13.0 * s))
                .padding([8.0 * s, 12.0 * s])
                .style(quiet)
                .on_press(gc(Msg::ScreenshotDir(String::new()))),
        );
    }
    let mut name = row![text(t("Screenshot folder")).font(BODY).size(14.0 * s).color(c.ink)]
        .spacing(8.0 * s)
        .align_y(Alignment::Center);
    if st.changed(&[model::SCREENSHOT_DIR]) {
        name = name.push(dot(c.accent, 7.0 * s));
    }
    let folder: El<'_> = container(
        column![
            row![icon(Icon::Folder, 16.0 * s, c.sub), name]
                .spacing(9.0 * s)
                .align_y(Alignment::Center),
            text(t("Where screenshots are saved. Leave empty to use the game's default."))
                .font(BODY)
                .size(12.0 * s)
                .color(c.sub),
            Space::new().height(4.0 * s),
            field,
        ]
        .spacing(3.0 * s),
    )
    .padding([13.0 * s, 0.0])
    .into();
    card(
        t("Screenshots"),
        t("Where screenshots go and how large they are."),
        vec![
            folder,
            toggle_row(
                st,
                t("Save at display resolution"),
                t("Capture at the window's full size rather than the game's base size."),
                model::SCREENSHOT_SCREEN_RES,
                ui,
            ),
        ],
        ui,
    )
}

