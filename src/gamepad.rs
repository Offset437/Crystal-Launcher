//! Controller detection and FFXI gamepad setup (Settings > Controller).
//!
//! * `detect`: connected controllers through gilrs (Windows.Gaming.Input),
//!   classified by USB vendor/product id, with live input on a worker thread.
//! * `art`: the Xbox / DualSense / generic illustrations and button glyphs.
//! * `ffxi`: FFXI's `padmode000` / `padsin000` values, read from and written
//!   to the Windows registry.
//!
//! Edits save automatically; running clients reload through their scoped host.
//! The design preview (`--preview-settings-controller`) fakes a controller
//! from `XI_PREVIEW_PAD=xbox|dualsense|ds4|ally|ally-x|switch|generic|none` and never
//! writes anything.
mod art;
pub(crate) mod detect;
mod ffxi;

use crate::i18n::{self, t, tf};
use crate::ui::{colors, panel, primary, quiet, scale, Colors, BODY, SEMI};
use crate::{App, Message};
use detect::{Btn, Family, Input, Pad};
use ffxi::{Control, Flag, Layout, PadConfig, Row, Stick, ROWS};
use iced::widget::{button, column, container, row, svg, text, Space};
use iced::{Alignment, Background, Border, Color, Element, Length, Subscription, Task};

struct Target {
    source: String,
    found: bool,
    saved: PadConfig,
    edit: PadConfig,
}

impl Target {
    fn from(loaded: ffxi::Loaded) -> Self {
        Self {
            source: loaded.source,
            found: loaded.found,
            saved: loaded.config.clone(),
            edit: loaded.config,
        }
    }
}

struct Notice {
    error: bool,
    text: String,
}

#[derive(Default)]
struct Saving {
    revision: u64,
    in_flight: Option<u64>,
    live: Option<(u64, std::collections::BTreeMap<String, String>)>,
    applying: Option<u64>,
    persisted: bool,
}

pub(crate) struct State {
    preview: bool,
    pads: Vec<Pad>,
    selected: Option<usize>,
    prev: Input,
    retail: Target,
    capture: Option<Row>,
    notice: Option<Notice>,
    saving: Saving,
}

impl Default for State {
    fn default() -> Self {
        if std::env::args().any(|a| a.starts_with("--preview")) {
            return Self::preview();
        }
        Self {
            preview: false,
            pads: Vec::new(),
            selected: None,
            prev: Input::default(),
            retail: Target::from(ffxi::load_retail()),
            capture: None,
            notice: None,
            saving: Saving::default(),
        }
    }
}

impl State {
    pub(crate) fn controller_pad(&self) -> Option<&Pad> { self.pad() }
    pub(crate) fn capturing(&self) -> bool { self.capture.is_some() }
    /// Synthetic input for the controller highlight and binding chips.
    fn preview() -> Self {
        let kind = std::env::var("XI_PREVIEW_PAD").unwrap_or_else(|_| "xbox".into());
        let ps5 = matches!(kind.to_ascii_lowercase().as_str(), "dualsense" | "ps5");
        let input = Input {
            buttons: Btn::South.bit() | if ps5 { Btn::Touchpad.bit() | Btn::Mute.bit() } else { 0 },
            rt: 0.6,
            lx: 0.55,
            ly: 0.35,
            ..Default::default()
        };
        let pad = |name: &str, vid: u16, pid: u16, wireless: bool| Pad {
            id: 0,
            name: name.into(),
            family: detect::classify(Some(vid), Some(pid), name),
            vid: Some(vid),
            pid: Some(pid),
            wireless: Some(wireless),
            battery: wireless.then_some(80),
            battery_source: wireless.then_some(detect::BatterySource::Controller),
            battery_state: detect::BatteryState::Discharging,
            battery_level: None,
            rumble: true,
            input,
        };
        let pads = match kind.to_ascii_lowercase().as_str() {
            "none" => vec![],
            "dualsense" | "ps5" => vec![pad("DualSense Wireless Controller", 0x054C, 0x0CE6, false)],
            "ds4" | "ps4" => vec![pad("Wireless Controller", 0x054C, 0x09CC, true)],
            "switch" => vec![pad("Pro Controller", 0x057E, 0x2009, true)],
            "generic" => vec![pad("USB Gamepad", 0x0079, 0x0006, false)],
            "ally" | "rog-xbox-ally" | "ally-x" | "rog-xbox-ally-x" => {
                let mut handheld = pad("ROG Xbox Ally", 0x045E, 0x028E, false);
                handheld.family = if kind.to_ascii_lowercase().ends_with("-x") { Family::RogXboxAllyX } else { Family::RogXboxAlly };
                handheld.name = handheld.family.label().into();
                handheld.battery = Some(64);
                handheld.battery_source = Some(detect::BatterySource::Handheld);
                vec![handheld]
            }
            _ => vec![pad("Xbox Wireless Controller", 0x045E, 0x0B13, true)],
        };
        let target = |source: &str| {
            let mut t = Target {
                source: source.into(),
                found: true,
                saved: PadConfig::default(),
                edit: PadConfig::default(),
            };
            // Pending edits show the unsaved-changes bar.
            t.edit.sin[25] = -1;
            if ps5 {
                t.saved.set_flag(Flag::XInput, false, Layout::DInputSony);
                t.edit.set_flag(Flag::XInput, false, Layout::DInputSony);
                t.edit.bind_button(6, 13);
                t.edit.bind_button(8, 14);
            }
            t
        };
        Self {
            preview: true,
            selected: pads.first().map(|p| p.id),
            prev: input,
            pads,
            retail: target(r"HKLM\SOFTWARE\WOW6432Node\PlayOnlineUS\SquareEnix\FinalFantasyXI"),
            capture: std::env::var_os("XI_PREVIEW_CAPTURE").map(|_| Row::Single(4)),
            notice: None,
            saving: Saving::default(),
        }
    }

    pub(crate) fn take_live_update(&mut self) -> Option<(u64, std::collections::BTreeMap<String, String>)> {
        if self.saving.applying.is_some() { return None; }
        let update = self.saving.live.take()?;
        self.saving.applying = Some(update.0);
        Some(update)
    }
    pub(crate) fn take_persisted_change(&mut self) -> bool { std::mem::take(&mut self.saving.persisted) }
    fn pad(&self) -> Option<&Pad> {
        self.selected
            .and_then(|id| self.pads.iter().find(|p| p.id == id))
            .or(self.pads.first())
    }
    /// The controller's DirectInput numbering that FFXI sees when XInput is off.
    fn di_layout(&self) -> Layout {
        match self.pad().map(|p| p.family) {
            Some(f) if f.is_playstation() => Layout::DInputSony,
            _ => Layout::DInputXbox,
        }
    }
    fn layout(&self) -> Layout {
        if self.retail.edit.flag(Flag::XInput) {
            Layout::XInput
        } else {
            self.di_layout()
        }
    }
    fn try_capture(&mut self, input: Input, newly: u32) {
        let Some(row) = self.capture else {
            return;
        };
        let layout = self.layout();
        match row {
            Row::Single(slot) => {
                let Some(b) = Btn::ALL.into_iter().find(|b| newly & b.bit() != 0) else {
                    return;
                };
                match layout.encode_button(b) {
                    Some(id) => {
                        self.retail.edit.bind_button(slot, id);
                        self.capture = None;
                        self.notice = None;
                    }
                    None => {
                        self.notice = Some(Notice {
                            error: true,
                            text: if matches!(b, Btn::Touchpad | Btn::Mute) && layout == Layout::XInput {
                                t("Touchpad click and Mic button need DirectInput. Turn XInput mode off, then press the button again.").into()
                            } else {
                                t("FFXI cannot use that control for a button in this input mode. Try another.").into()
                            },
                        })
                    }
                }
            }
            Row::Group(first) => {
                let dpad = [Btn::DUp, Btn::DDown, Btn::DLeft, Btn::DRight];
                let stick = if dpad.iter().any(|b| newly & b.bit() != 0) {
                    Some(Stick::Dpad)
                } else if input.lx.abs().max(input.ly.abs()) > 0.6
                    && self.prev.lx.abs().max(self.prev.ly.abs()) <= 0.6 {
                    Some(Stick::Left)
                } else if input.rx.abs().max(input.ry.abs()) > 0.6
                    && self.prev.rx.abs().max(self.prev.ry.abs()) <= 0.6 {
                    Some(Stick::Right)
                } else {
                    None
                };
                if let Some(stick) = stick {
                    self.retail.edit.bind_group(first, layout.group(stick));
                    self.capture = None;
                    self.notice = None;
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Msg {
    Pads(Vec<Pad>),
    Select(usize),
    SetFlag(Flag, bool),
    Capture(Row),
    CancelCapture,
    Clear(Row),
    Reset,
    Discard,
    Apply,
    SaveLater(u64),
    Saved(u64, PadConfig, Result<String, String>),
    LiveResult(u64, Result<String, String>),
    Rumble,
}

pub(crate) fn update(state: &mut State, msg: Msg, config: &crate::backend::Config) -> Task<Msg> {
    let before = state.retail.edit.clone();
    match msg {
        Msg::Pads(pads) => {
            state.pads = pads;
            if state.pad().map(|p| p.id) != state.selected {
                state.selected = state.pad().map(|p| p.id);
                state.prev = state.pad().map(|p| p.input).unwrap_or_default();
            }
            let input = state.pad().map(|p| p.input).unwrap_or_default();
            let newly = input.buttons & !state.prev.buttons;
            state.try_capture(input, newly);
            state.prev = input;
        }
        Msg::Select(id) => {
            state.selected = Some(id);
            state.prev = state.pad().map(|p| p.input).unwrap_or_default();
        }
        Msg::SetFlag(flag, on) => {
            let di = state.di_layout();
            state.retail.edit.set_flag(flag, on, di);
        }
        Msg::Capture(row) => {
            state.capture = Some(row);
            state.prev = state.pad().map(|p| p.input).unwrap_or_default();
            state.notice = None;
        }
        Msg::CancelCapture => state.capture = None,
        Msg::Clear(row) => {
            let t = &mut state.retail;
            match row {
                Row::Single(i) => t.edit.sin[i] = -1,
                Row::Group(i) => t.edit.sin[i..i + 4].copy_from_slice(&[-1; 4]),
            }
            state.capture = None;
        }
        Msg::Reset => {
            let di = state.di_layout();
            state.retail.edit.reset(di);
            state.capture = None;
        }
        Msg::Discard => {
            let t = &mut state.retail;
            t.edit = t.saved.clone();
            state.capture = None;
            state.notice = None;
        }
        Msg::Apply => {
            state.capture = None;
            if !state.preview {
                if state.retail.edit == state.retail.saved {
                    state.saving.revision = state.saving.revision.wrapping_add(1);
                    state.saving.live = Some((state.saving.revision, controller_values(&state.retail.edit)));
                } else { return save_later(state.saving.revision); }
            }
        }
        Msg::SaveLater(revision) => {
            if state.preview || revision != state.saving.revision || state.saving.in_flight.is_some()
                || state.retail.edit == state.retail.saved { return Task::none(); }
            state.saving.in_flight = Some(revision);
            let pad = state.retail.edit.clone();
            let config = config.clone();
            return Task::perform(async move {
                let snapshot = pad.clone();
                let result = tokio::task::spawn_blocking(move || ffxi::save_retail(&config, &snapshot)).await
                    .unwrap_or_else(|_| Err(t("Could not save controller settings.").into()));
                Msg::Saved(revision, pad, result)
            }, |result| result);
        }
        Msg::Saved(revision, pad, result) => {
            if state.saving.in_flight != Some(revision) { return Task::none(); }
            state.saving.in_flight = None;
            let failed = result.is_err();
            match result {
                Ok(_) => {
                    state.retail.saved = pad;
                    state.retail.found = true;
                    state.saving.persisted = true;
                    if revision == state.saving.revision {
                        state.saving.live = Some((revision, controller_values(&state.retail.edit)));
                        state.notice = Some(Notice { error: false, text: t("Saved. Applying to running games…").into() });
                    }
                }
                Err(text) => state.notice = Some(Notice { error: true, text }),
            }
            if state.retail.edit != state.retail.saved && (!failed || revision != state.saving.revision) {
                return save_later(state.saving.revision);
            }
        }
        Msg::LiveResult(revision, result) => {
            if state.saving.applying != Some(revision) { return Task::none(); }
            state.saving.applying = None;
            if revision == state.saving.revision && state.retail.edit == state.retail.saved {
                state.notice = Some(match result {
                    Ok(text) => Notice { error: false, text },
                    Err(text) => Notice { error: true, text },
                });
            }
        }
        Msg::Rumble => {
            if let Some(p) = state.pad() {
                detect::rumble(p.id);
            }
        }
    }
    if state.retail.edit != before {
        state.saving.revision = state.saving.revision.wrapping_add(1);
        state.saving.live = None;
        state.notice = None;
        if !state.preview && state.retail.edit != state.retail.saved { return save_later(state.saving.revision); }
    }
    Task::none()
}

fn controller_values(pad: &PadConfig) -> std::collections::BTreeMap<String, String> {
    pad.values().into_iter().map(|(name, value)| (name.to_owned(), value)).collect()
}

fn save_later(revision: u64) -> Task<Msg> {
    Task::perform(async move { tokio::time::sleep(std::time::Duration::from_millis(200)).await; revision }, Msg::SaveLater)
}

fn watch() -> impl iced::futures::Stream<Item = Msg> {
    iced::stream::channel(64, async move |output: iced::futures::channel::mpsc::Sender<Msg>| {
        let mut tx = output.clone();
        let _ = std::thread::Builder::new()
            .name("gamepad".into())
            .spawn(move || {
                detect::run(|pads| {
                    let mut msg = Msg::Pads(pads);
                    loop {
                        match tx.try_send(msg) {
                            Ok(()) => return true,
                            Err(e) if e.is_full() => {
                                msg = e.into_inner();
                                std::thread::sleep(std::time::Duration::from_millis(4));
                            }
                            Err(_) => return false,
                        }
                    }
                })
            });
        std::future::pending::<()>().await;
    })
}

pub(crate) fn subscription(state: &State) -> Subscription<Msg> {
    if state.preview {
        Subscription::none()
    } else {
        Subscription::run(watch)
    }
}

// ---- view -----------------------------------------------------------------------

fn glyph<'a>(doc: String, size: f32) -> Element<'a, Message> {
    svg(svg::Handle::from_memory(doc.into_bytes()))
        .width(size)
        .height(size)
        .into()
}

fn btn_label(family: Family, b: Btn) -> &'static str {
    let ps = family.is_playstation();
    let nin = family == Family::SwitchPro;
    match b {
        Btn::LB if ps => "L1",
        Btn::RB if ps => "R1",
        Btn::LT if ps => "L2",
        Btn::RT if ps => "R2",
        Btn::LS if ps => "L3",
        Btn::RS if ps => "R3",
        Btn::LB if nin => "L",
        Btn::RB if nin => "R",
        Btn::LT if nin => "ZL",
        Btn::RT if nin => "ZR",
        Btn::LB => "LB",
        Btn::RB => "RB",
        Btn::LT => "LT",
        Btn::RT => "RT",
        Btn::LS => "LS",
        Btn::RS => "RS",
        Btn::Back if family == Family::DualShock4 => "Share",
        Btn::Back if ps => "Create",
        Btn::Back if nin => "Minus",
        Btn::Back => "View",
        Btn::Start if ps => "Options",
        Btn::Start if nin => "Plus",
        Btn::Start => "Menu",
        Btn::Guide if ps => "PS",
        Btn::Guide if nin => "Home",
        Btn::Guide => "Guide",
        Btn::Touchpad => t("Touchpad click"),
        Btn::Mute => t("Mic button"),
        Btn::DUp => t("Up"),
        Btn::DDown => t("Down"),
        Btn::DLeft => t("Left"),
        Btn::DRight => t("Right"),
        Btn::South | Btn::East | Btn::West | Btn::North => "",
    }
}

/// A small bordered label, like the keycaps in the bindings list.
fn keycap<'a>(label: String, c: Colors, s: f32, lit: bool) -> Element<'a, Message> {
    container(text(label).font(SEMI).size(12.0 * s).color(if lit { c.play_text } else { c.ink }))
        .padding([2.0 * s, 8.0 * s])
        .style(move |_| container::Style {
            background: Some(if lit { c.accent } else { c.field }.into()),
            border: Border {
                color: if lit { c.accent } else { c.line },
                width: 1.0,
                radius: 5.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// How one binding row reads, in the connected controller's own style.
fn binding_chip<'a>(
    family: Family,
    layout: Layout,
    row: Row,
    pad: &PadConfig,
    input: &Input,
    c: Colors,
    light: bool,
    s: f32,
) -> (Element<'a, Message>, bool) {
    let size = 22.0 * s;
    match row {
        Row::Group(first) => {
            let ids = &pad.sin[first..first + 4];
            match layout.group_stick(ids) {
                Some(Stick::Dpad) => {
                    let lit: Vec<Btn> = [Btn::DUp, Btn::DDown, Btn::DLeft, Btn::DRight]
                        .into_iter()
                        .filter(|b| input.pressed(*b))
                        .collect();
                    let all = [Btn::DUp, Btn::DDown, Btn::DLeft, Btn::DRight];
                    let doc = art::dpad_chip(if lit.is_empty() { &all } else { &lit }, c.sub, if lit.is_empty() { c.sub } else { c.accent });
                    (row![glyph(doc, size), text(t("D-pad")).font(BODY).size(12.5 * s).color(c.ink)].spacing(6.0 * s).align_y(Alignment::Center).into(), !lit.is_empty())
                }
                Some(stick) => {
                    let (x, y, name) = match stick {
                        Stick::Left => (input.lx, input.ly, t("Left stick")),
                        _ => (input.rx, input.ry, t("Right stick")),
                    };
                    let live = x.abs().max(y.abs()) > 0.3;
                    let doc = art::stick_chip(c.sub, if live { c.accent } else { c.sub });
                    (row![glyph(doc, size), text(name).font(BODY).size(12.5 * s).color(c.ink)].spacing(6.0 * s).align_y(Alignment::Center).into(), live)
                }
                None if ids.iter().all(|v| *v == -1) => (text(t("Not set")).font(BODY).size(12.5 * s).color(c.sub).into(), false),
                None => (keycap(t("Custom").into(), c, s, false), false),
            }
        }
        Row::Single(slot) => match layout.decode(pad.sin[slot]) {
            Control::None => (text(t("Not set")).font(BODY).size(12.5 * s).color(c.sub).into(), false),
            Control::Button(b) if matches!(b, Btn::South | Btn::East | Btn::West | Btn::North) => {
                let lit = input.pressed(b);
                if family == Family::Generic {
                    let name = match b {
                        Btn::South => t("Bottom button"),
                        Btn::East => t("Right button"),
                        Btn::West => t("Left button"),
                        _ => t("Top button"),
                    };
                    return (keycap(name.into(), c, s, lit), lit);
                }
                (glyph(art::face_chip(family, b, light, lit), size), lit)
            }
            Control::Button(b) if b.is_dpad() => {
                let lit = input.pressed(b);
                let doc = art::dpad_chip(&[b], c.sub, if lit { c.accent } else { c.ink });
                (row![glyph(doc, size), text(tf("D-pad {direction}", &[("direction", &btn_label(family, b).to_lowercase())])).font(BODY).size(12.5 * s).color(c.ink)].spacing(6.0 * s).align_y(Alignment::Center).into(), lit)
            }
            Control::Button(b) => {
                let lit = input.pressed(b);
                (keycap(btn_label(family, b).into(), c, s, lit), lit)
            }
            Control::Axis(stick, dir, neg) => {
                let name = match stick {
                    Stick::Left => t("Left stick"),
                    Stick::Right => t("Right stick"),
                    Stick::Dpad => t("D-pad"),
                };
                let d = match (dir, neg) {
                    (ffxi::AxisDir::X, false) => "→",
                    (ffxi::AxisDir::X, true) => "←",
                    (ffxi::AxisDir::Y, false) => "↓",
                    (ffxi::AxisDir::Y, true) => "↑",
                };
                (keycap(format!("{name} {d}"), c, s, false), false)
            }
            Control::Raw(id) if id >= 32 || id < -1 => (keycap(tf("Axis {id}", &[("id", &id)]), c, s, false), false),
            Control::Raw(id) => (keycap(tf("Button {number}", &[("number", &(id + 1))]), c, s, false), false),
        },
    }
}

fn card<'a>(content: impl Into<Element<'a, Message>>, s: f32) -> container::Container<'a, Message> {
    container(content).padding([16.0 * s, 18.0 * s]).style(panel)
}

fn heading<'a>(title: &'a str, c: Colors, s: f32) -> Element<'a, Message> {
    text(title).font(SEMI).size(15.0 * s).color(c.ink).into()
}

fn meter<'a>(label: &'a str, value: f32, c: Colors, s: f32) -> Element<'a, Message> {
    let w = 64.0 * s;
    let fill = (value.clamp(0.0, 1.0) * w).max(0.0);
    row![
        text(label).font(SEMI).size(11.5 * s).color(c.sub).width(22.0 * s),
        container(
            container(Space::new())
                .width(fill)
                .height(6.0 * s)
                .style(move |_| container::Style {
                    background: Some(c.accent.into()),
                    border: Border { radius: 3.0.into(), ..Default::default() },
                    ..Default::default()
                })
        )
        .width(w)
        .height(6.0 * s)
        .style(move |_| container::Style {
            background: Some(c.field.into()),
            border: Border { radius: 3.0.into(), color: c.line, width: 1.0 },
            ..Default::default()
        })
    ]
    .spacing(6.0 * s)
    .align_y(Alignment::Center)
    .into()
}

fn stick_readout<'a>(label: &'a str, x: f32, y: f32, c: Colors, s: f32) -> Element<'a, Message> {
    row![
        text(label).font(SEMI).size(11.5 * s).color(c.sub),
        text(format!("{x:+.2}  {y:+.2}")).font(iced::Font::MONOSPACE).size(11.5 * s).color(c.ink),
    ]
    .spacing(6.0 * s)
    .into()
}

fn device_bar<'a>(st: &'a State, c: Colors, light: bool, s: f32) -> Element<'a, Message> {
    let t = &st.retail;
    let changes = t.edit.changes(&t.saved);
    let (badge, title, sub): (Element<'a, Message>, String, String) = match st.pad() {
        Some(p) => {
            let ids = match (p.vid, p.pid) {
                (Some(v), Some(d)) => format!(" · USB {v:04X}:{d:04X}"),
                _ => String::new(),
            };
            let badge = if p.family == Family::Generic {
                glyph(art::stick_chip(c.sub, c.accent), 26.0 * s)
            } else {
                glyph(art::face_chip(p.family, Btn::South, light, false), 26.0 * s)
            };
            (badge, p.name.clone(), format!("{} · {}{}", p.family.label(), p.connection(), ids))
        }
        None => (
            glyph(art::stick_chip(c.sub, c.sub), 26.0 * s),
            i18n::t("No controller connected").into(),
            i18n::t("Plug in an Xbox or PlayStation controller, or pair it over Bluetooth. It appears here straight away.").into(),
        ),
    };
    let badge = container(badge)
        .center(44.0 * s)
        .style(move |_| container::Style {
            background: Some(c.select.into()),
            border: Border { radius: 10.0.into(), ..Default::default() },
            ..Default::default()
        });
    let mut others = row![].spacing(6.0 * s);
    if st.pads.len() > 1 {
        for p in &st.pads {
            let active = Some(p.id) == st.pad().map(|x| x.id);
            others = others.push(
                crate::controller_nav::button(text(p.name.clone()).font(BODY).size(12.0 * s))
                    .padding([5.0 * s, 10.0 * s])
                    .style(move |theme, status| {
                        let mut b = quiet(theme, status);
                        if active {
                            b.background = Some(c.select.into());
                        }
                        b.border = Border { radius: 6.0.into(), color: c.line, width: 1.0 };
                        b
                    })
                    .on_press(Message::Gamepad(Msg::Select(p.id))),
            );
        }
    }
    let status: Element<'a, Message> = if !st.preview {
        let label = if changes > 0 && st.notice.as_ref().is_some_and(|notice| notice.error) { i18n::t("Controller settings could not be saved") }
            else if st.saving.in_flight.is_some() || changes > 0 { i18n::t("Saving controller settings…") }
            else { i18n::t("Controller settings save automatically") };
        let mut status = row![text(label).font(BODY).size(12.5 * s).color(c.sub)].spacing(10.0 * s).align_y(Alignment::Center);
        if st.notice.as_ref().is_some_and(|notice| notice.error) {
            status = status.push(crate::controller_nav::button(text(i18n::t("Retry")).font(BODY).size(13.0 * s))
                .padding([6.0 * s, 12.0 * s]).style(quiet).on_press(Message::Gamepad(Msg::Apply)));
        }
        status.into()
    } else if changes > 0 {
        row![
            container(Space::new()).width(8.0 * s).height(8.0 * s).style(move |_| container::Style {
                background: Some(c.accent.into()),
                border: Border { radius: 4.0.into(), ..Default::default() },
                ..Default::default()
            }),
            text(if changes == 1 {
                tf("{count} unsaved change", &[("count", &changes)])
            } else {
                tf("{count} unsaved changes", &[("count", &changes)])
            })
                .font(SEMI)
                .size(13.0 * s)
                .color(c.ink),
            crate::controller_nav::button(text(i18n::t("Discard")).font(BODY).size(13.0 * s))
                .padding([8.0 * s, 14.0 * s])
                .style(quiet)
                .on_press(Message::Gamepad(Msg::Discard)),
            crate::controller_nav::button(text(i18n::t("Apply")).font(SEMI).size(13.0 * s))
                .padding([8.0 * s, 20.0 * s])
                .style(primary)
                .on_press_maybe((!st.preview).then_some(Message::Gamepad(Msg::Apply))),
        ]
        .spacing(10.0 * s)
        .align_y(Alignment::Center)
        .into()
    } else {
        row![
            crate::icons::icon(crate::icons::Icon::CheckCircle, 15.0 * s, c.sub),
            text(if t.found { i18n::t("Matches the game's saved settings") } else { i18n::t("Using FFXI defaults") })
                .font(BODY)
                .size(12.5 * s)
                .color(c.sub)
        ]
        .spacing(6.0 * s)
        .align_y(Alignment::Center)
        .into()
    };
    card(
        row![
            badge,
            column![
                text(title).font(SEMI).size(16.0 * s).color(c.ink),
                text(sub).font(BODY).size(12.5 * s).color(c.sub),
            ]
            .spacing(2.0 * s),
            others,
            Space::new().width(Length::Fill),
            status,
        ]
        .spacing(14.0 * s)
        .align_y(Alignment::Center),
        s,
    )
    .padding([12.0 * s, 16.0 * s])
    .width(Length::Fill)
    .into()
}

fn live_card<'a>(st: &'a State, c: Colors, light: bool, s: f32) -> Element<'a, Message> {
    let pad = st.pad();
    let family = pad.map_or(Family::Generic, |p| p.family);
    let input = pad.map(|p| p.input).unwrap_or_default();
    let doc = art::controller(family, &input, light, c.accent);
    let picture = svg(svg::Handle::from_memory(doc.into_bytes()))
        .width(Length::Fill)
        .height(318.0 * s)
        .opacity(if pad.is_some() { 1.0_f32 } else { 0.35 });
    let caption = match pad {
        Some(p) if p.family.is_playstation() => t("Press any button, move the sticks or pull L2/R2 to test it."),
        Some(p) if p.family.is_xbox() => t("Press any button, move the sticks or pull LT/RT to test it."),
        Some(_) => t("Press any button or move the sticks to test it."),
        None => t("Waiting for a controller…"),
    };
    let (lt, rt) = match family {
        f if f.is_playstation() => ("L2", "R2"),
        Family::SwitchPro => ("ZL", "ZR"),
        _ => ("LT", "RT"),
    };
    let mut readout = row![
        meter(lt, input.lt, c, s),
        meter(rt, input.rt, c, s),
        stick_readout("L", input.lx, input.ly, c, s),
        stick_readout("R", input.rx, input.ry, c, s),
        Space::new().width(Length::Fill),
    ]
    .spacing(18.0 * s)
    .align_y(Alignment::Center);
    if pad.is_some_and(|p| p.rumble) {
        readout = readout.push(
            crate::controller_nav::button(text(t("Test vibration")).font(BODY).size(12.5 * s))
                .padding([6.0 * s, 10.0 * s])
                .style(quiet)
                .on_press(Message::Gamepad(Msg::Rumble)),
        );
    }
    let mut content = column![
        row![
            heading(t("Live test"), c, s),
            Space::new().width(Length::Fill),
            text(caption).font(BODY).size(12.0 * s).color(c.sub)
        ]
        .align_y(Alignment::Center),
        picture,
        readout,
    ]
    .spacing(10.0 * s);
    if family.is_playstation() {
        let mut extras = row![keycap(btn_label(family, Btn::Touchpad).into(), c, s, input.pressed(Btn::Touchpad))]
            .spacing(10.0 * s);
        if matches!(family, Family::DualSense | Family::DualSenseEdge) {
            extras = extras.push(keycap(btn_label(family, Btn::Mute).into(), c, s, input.pressed(Btn::Mute)));
        }
        content = content.push(extras);
    }
    card(content, s).width(Length::FillPortion(11)).into()
}

fn bindings_card<'a>(st: &'a State, c: Colors, light: bool, s: f32) -> Element<'a, Message> {
    let t = &st.retail;
    let layout = st.layout();
    let pad = st.pad();
    let input = pad.map(|p| p.input).unwrap_or_default();
    let family = pad.map_or(
        if layout == Layout::DInputSony { Family::DualSense } else { Family::Xbox },
        |p| p.family,
    );
    let rows: Vec<Element<'a, Message>> = ROWS
        .iter()
        .map(|r| {
            let r = *r;
            let label = r.label();
            let capturing = st.capture == Some(r);
            let changed = match r {
                Row::Single(i) => t.edit.sin[i] != t.saved.sin[i],
                Row::Group(i) => t.edit.sin[i..i + 4] != t.saved.sin[i..i + 4],
            };
            let (chip, live) = binding_chip(family, layout, r, &t.edit, &input, c, light, s);
            let right: Element<'a, Message> = if capturing {
                row![
                    text(match r {
                        Row::Group(_) => i18n::t("Move a stick or press the D-pad…"),
                        Row::Single(_) => i18n::t("Press a button…"),
                    })
                    .font(SEMI)
                    .size(12.5 * s)
                    .color(c.accent),
                    crate::controller_nav::button(text(i18n::t("Unbind")).font(BODY).size(12.0 * s))
                        .padding([3.0 * s, 8.0 * s])
                        .style(quiet)
                        .on_press(Message::Gamepad(Msg::Clear(r))),
                ]
                .spacing(4.0 * s)
                .align_y(Alignment::Center)
                .into()
            } else {
                chip
            };
            let label_row = row![
                text(label).font(BODY).size(13.0 * s).color(c.ink),
                if changed {
                    Element::from(
                        container(Space::new()).width(6.0 * s).height(6.0 * s).style(move |_| container::Style {
                            background: Some(c.accent.into()),
                            border: Border { radius: 3.0.into(), ..Default::default() },
                            ..Default::default()
                        }),
                    )
                } else {
                    Space::new().width(0).into()
                },
            ]
            .spacing(6.0 * s)
            .align_y(Alignment::Center);
            crate::controller_nav::button(
                row![label_row, Space::new().width(Length::Fill), right]
                    .align_y(Alignment::Center)
                    .height(26.0 * s),
            )
            .width(Length::Fill)
            .padding([5.0 * s, 10.0 * s])
            .style(move |_, status| {
                let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
                button::Style {
                    background: Some(Background::Color(if capturing || live {
                        c.select
                    } else if hovered {
                        c.field
                    } else {
                        Color::TRANSPARENT
                    })),
                    text_color: c.ink,
                    border: Border {
                        color: if capturing { c.accent } else { c.line },
                        width: if capturing { 1.0 } else { 0.0 },
                        radius: 6.0.into(),
                    },
                    ..Default::default()
                }
            })
            .on_press(Message::Gamepad(if capturing { Msg::CancelCapture } else { Msg::Capture(r) }))
            .into()
        })
        .collect();
    let half = rows.len().div_ceil(2);
    let mut left = column![].spacing(2.0 * s).width(Length::Fill);
    let mut right = column![].spacing(2.0 * s).width(Length::Fill);
    for (i, r) in rows.into_iter().enumerate() {
        if i < half {
            left = left.push(r);
        } else {
            right = right.push(r);
        }
    }
    let client_name = i18n::t("Windower / Ashita");
    let mode = match layout {
        Layout::XInput => "XInput",
        _ => "DirectInput",
    };
    card(
        column![
            row![
                column![
                    heading(i18n::t("Button bindings"), c, s),
                    text(tf("FINAL FANTASY XI · {client} · {mode} · click an action, then press the button for it", &[("client", &client_name), ("mode", &mode)]))
                        .font(BODY)
                        .size(12.0 * s)
                        .color(c.sub),
                ]
                .spacing(2.0 * s),
                Space::new().width(Length::Fill),
                crate::controller_nav::button(text(i18n::t("Reset to default")).font(BODY).size(12.5 * s))
                    .padding([6.0 * s, 10.0 * s])
                    .style(quiet)
                    .on_press(Message::Gamepad(Msg::Reset)),
            ]
            .align_y(Alignment::Center),
            row![left, right].spacing(12.0 * s),
        ]
        .spacing(10.0 * s),
        s,
    )
    .width(Length::FillPortion(12))
    .into()
}

fn options_card<'a>(st: &'a State, app: &'a App, c: Colors, s: f32) -> Element<'a, Message> {
    let t = &st.retail;
    let family = st.pad().map(|p| p.family);
    let switch = |label: &'a str, flag: Flag| -> Element<'a, Message> {
        crate::controller_nav::toggler(t.edit.flag(flag))
            .label(label)
            .font(BODY)
            .text_size(13.0 * s)
            .size(18.0 * s)
            .spacing(10.0 * s)
            .on_toggle(move |on| Message::Gamepad(Msg::SetFlag(flag, on)))
            .into()
    };
    let mut notes = column![].spacing(4.0 * s);
    let note = |s_: String, color: Color| text(s_).font(BODY).size(12.0 * s).color(color);
    if st.preview {
        notes = notes.push(note(i18n::t("Design preview: a synthetic controller is shown and nothing is ever written.").into(), c.sub));
    }
    if matches!(family, Some(Family::DualSense | Family::DualSenseEdge)) {
        notes = notes.push(note(
            i18n::t("Touchpad click and Mic button can be bound with XInput mode off. Connect by USB to use the Mic button; Bluetooth does not expose it to FFXI.").into(),
            c.sub,
        ));
    }
    if family.is_some_and(|f| f.is_playstation()) && t.edit.flag(Flag::XInput) {
        notes = notes.push(note(
            i18n::t("FFXI's XInput mode only sees Xbox-compatible controllers. Turn it off to use this PlayStation controller through DirectInput, or map it with Steam Input.").into(),
            crate::ui::theme(app.settings.light_theme).palette().warning,
        ));
    }
    if app.settings.launch_with == crate::ui::LaunchWith::Ashita {
        notes = notes.push(note(
            i18n::t("Ashita's boot controller settings take priority at launch. Changes here also apply to running games.").into(),
            c.sub,
        ));
    }
    notes = notes.push(note(tf("Stored in {source}", &[("source", &t.source)]), c.sub));
    if let Some(n) = st.notice.as_ref() {
        let palette = crate::ui::theme(app.settings.light_theme).palette();
        notes = notes.push(note(n.text.clone(), if n.error { palette.danger } else { palette.success }));
    }
    card(
        column![
            heading(i18n::t("Game options"), c, s),
            row![
                switch(i18n::t("Use a controller in FFXI"), Flag::Enabled),
                switch(i18n::t("Vibration (force feedback)"), Flag::ForceFeedback),
                switch(i18n::t("XInput mode"), Flag::XInput),
                switch(i18n::t("Keep working in the background"), Flag::Background),
            ]
            .spacing(28.0 * s)
            .align_y(Alignment::Center),
            notes,
        ]
        .spacing(12.0 * s),
        s,
    )
    .width(Length::Fill)
    .into()
}

pub(crate) fn view(app: &App) -> Element<'_, Message> {
    let st = &app.gamepad;
    let light = app.settings.light_theme;
    let c = colors(light);
    let s = scale(app);
    column![
        device_bar(st, c, light, s),
        row![live_card(st, c, light, s), bindings_card(st, c, light, s)].spacing(14.0 * s),
        options_card(st, app, c, s),
    ]
    .spacing(14.0 * s)
    // Room for the page's scrollbar on the right.
    .padding(iced::Padding::ZERO.bottom(8.0 * s).right(14.0 * s))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(state: &mut State, message: Msg) -> Task<Msg> {
        super::update(state, message, &crate::backend::Config::default())
    }

    fn ps5_state(xinput: bool) -> State {
        let target = || {
            let mut config = PadConfig::default();
            config.set_flag(Flag::XInput, xinput, Layout::DInputSony);
            Target { source: "test fixture".into(), found: true, saved: config.clone(), edit: config }
        };
        State {
            preview: true,
            pads: vec![Pad {
                id: 0,
                name: "DualSense".into(),
                family: Family::DualSense,
                vid: Some(0x054C),
                pid: Some(0x0CE6),
                wireless: Some(false),
                battery: None,
                battery_source: None,
                battery_state: detect::BatteryState::Unknown,
                battery_level: None,
                rumble: false,
                input: Input::default(),
            }],
            selected: Some(0),
            prev: Input::default(),
            retail: target(),
            capture: None,
            notice: None,
            saving: Saving::default(),
        }
    }

    #[test]
    fn rapid_edits_save_the_latest_snapshot_without_overlapping_writes() {
        let mut state = ps5_state(false);
        state.preview = false;
        let background = !state.retail.edit.flag(Flag::Background);
        let _ = update(&mut state, Msg::SetFlag(Flag::Background, background));
        let first = state.saving.revision;
        let first_pad = state.retail.edit.clone();
        let _ = update(&mut state, Msg::SaveLater(first));
        assert_eq!(state.saving.in_flight, Some(first));
        let _ = update(&mut state, Msg::Clear(Row::Single(6)));
        let latest = state.saving.revision;
        assert_ne!(latest, first);
        let _ = update(&mut state, Msg::SaveLater(latest));
        assert_eq!(state.saving.in_flight, Some(first));
        let _ = update(&mut state, Msg::Saved(first, first_pad, Ok("Saved".into())));
        assert!(state.take_persisted_change());
        assert!(state.take_live_update().is_none(), "an older disk snapshot must not be applied live");
        let _ = update(&mut state, Msg::SaveLater(first));
        assert!(state.saving.in_flight.is_none(), "a stale debounce must not start another write");
        let snapshot = state.retail.edit.clone();
        let _ = update(&mut state, Msg::SaveLater(latest));
        let _ = update(&mut state, Msg::Saved(latest, snapshot.clone(), Ok("Saved".into())));
        let (revision, values) = state.take_live_update().unwrap();
        assert_eq!(revision, latest);
        assert_eq!(values, controller_values(&snapshot));
        assert_eq!(state.retail.saved, snapshot);
    }

    #[test]
    fn live_updates_coalesce_and_a_stale_reply_does_not_replace_current_feedback() {
        let mut state = ps5_state(false);
        state.preview = false;
        let _ = update(&mut state, Msg::Apply);
        let (first, _) = state.take_live_update().unwrap();
        let _ = update(&mut state, Msg::Clear(Row::Single(6)));
        let latest = state.saving.revision;
        let snapshot = state.retail.edit.clone();
        let _ = update(&mut state, Msg::SaveLater(latest));
        let _ = update(&mut state, Msg::Saved(latest, snapshot, Ok("Saved".into())));
        assert!(state.take_live_update().is_none(), "one confirmation wait per launcher");
        let _ = update(&mut state, Msg::LiveResult(first, Err("old failure".into())));
        assert!(!state.notice.as_ref().unwrap().error);
        assert_eq!(state.take_live_update().unwrap().0, latest);
        let _ = update(&mut state, Msg::LiveResult(first, Err("old failure".into())));
        assert_eq!(state.saving.applying, Some(latest));
        let _ = update(&mut state, Msg::LiveResult(latest, Ok("Applied".into())));
        assert_eq!(state.notice.as_ref().unwrap().text, "Applied");
        assert!(state.saving.applying.is_none());
    }

    #[test]
    fn failed_saves_keep_edits_for_retry_and_battery_refreshes_do_not_save() {
        let mut state = ps5_state(false);
        state.preview = false;
        let mut pads = state.pads.clone();
        pads[0].battery = Some(55);
        let _ = update(&mut state, Msg::Pads(pads));
        assert_eq!(state.saving.revision, 0);
        assert!(state.take_live_update().is_none());
        let before = state.retail.saved.clone();
        let _ = update(&mut state, Msg::Clear(Row::Single(6)));
        let revision = state.saving.revision;
        let snapshot = state.retail.edit.clone();
        let _ = update(&mut state, Msg::SaveLater(revision));
        let _ = update(&mut state, Msg::Saved(revision, snapshot.clone(), Err("Access denied".into())));
        assert_eq!(state.retail.saved, before);
        assert_eq!(state.retail.edit, snapshot);
        assert!(state.saving.in_flight.is_none());
        assert!(state.notice.as_ref().unwrap().error);
        assert!(!state.take_persisted_change());
        assert!(state.take_live_update().is_none());
        let _ = update(&mut state, Msg::Apply);
        let _ = update(&mut state, Msg::SaveLater(revision));
        assert_eq!(state.saving.in_flight, Some(revision));
    }

    fn press(state: &mut State, buttons: u32) {
        let mut pads = state.pads.clone();
        pads[0].input.buttons = buttons;
        let _ = update(state, Msg::Pads(pads));
    }

    #[test]
    fn gamepad_ps5_extra_controls_capture_only_after_a_new_press() {
        let mut state = ps5_state(false);
        for (button, slot, expected) in [(Btn::Touchpad, 6, 13), (Btn::Mute, 8, 14)] {
            press(&mut state, button.bit());
            let _ = update(&mut state, Msg::Capture(Row::Single(slot)));
            let original = state.retail.edit.sin[slot];
            press(&mut state, button.bit());
            assert_eq!(state.capture, Some(Row::Single(slot)));
            assert_eq!(state.retail.edit.sin[slot], original);
            press(&mut state, 0);
            press(&mut state, button.bit());
            assert!(state.capture.is_none());
            assert!(state.notice.is_none());
            assert_eq!(state.retail.edit.sin[slot], expected);
        }
        assert_ne!(state.retail.edit, state.retail.saved);
    }

    #[test]
    fn gamepad_group_capture_waits_for_a_new_stick_movement() {
        let mut state = ps5_state(false);
        let mut pads = state.pads.clone();
        pads[0].input.lx = 0.8;
        let _ = update(&mut state, Msg::Pads(pads.clone()));
        let _ = update(&mut state, Msg::Capture(Row::Group(17)));
        let before = state.retail.edit.clone();
        // Another button or battery update must not capture a held stick.
        pads[0].input.buttons = Btn::South.bit();
        let _ = update(&mut state, Msg::Pads(pads.clone()));
        assert_eq!(state.capture, Some(Row::Group(17)));
        assert_eq!(state.retail.edit, before);
        pads[0].input.lx = 0.0;
        let _ = update(&mut state, Msg::Pads(pads.clone()));
        pads[0].input.ly = 0.8;
        let _ = update(&mut state, Msg::Pads(pads));
        assert!(state.capture.is_none());
        assert_eq!(state.retail.edit.sin[17..21], Layout::DInputSony.group(Stick::Left));
    }

    #[test]
    fn back_cancels_gamepad_capture_before_closing_the_active_page() {
        let mut app = crate::tests::app();
        app.page = crate::Page::Settings;
        let _ = app.update(Message::Gamepad(Msg::Capture(Row::Group(17))));
        assert!(app.gamepad.capturing());
        let _ = app.update(Message::ControllerBack);
        assert!(!app.gamepad.capturing());
        assert!(app.page == crate::Page::Settings);
    }

    #[test]
    fn gamepad_ps5_extra_controls_in_xinput_keep_capture_and_explain_directinput() {
        let mut state = ps5_state(true);
        for button in [Btn::Touchpad, Btn::Mute] {
            press(&mut state, 0);
            let _ = update(&mut state, Msg::Capture(Row::Single(6)));
            press(&mut state, button.bit());
            assert_eq!(state.capture, Some(Row::Single(6)));
            assert_eq!(state.retail.edit, state.retail.saved);
            let notice = state.notice.as_ref().unwrap();
            assert!(notice.error);
            assert!(notice.text.contains("Turn XInput mode off"));
        }
        let _ = update(&mut state, Msg::SetFlag(Flag::XInput, false));
        press(&mut state, 0);
        press(&mut state, Btn::Mute.bit());
        assert!(state.capture.is_none());
        assert!(state.notice.is_none());
        assert_eq!(state.retail.edit.sin[6], 14);
    }
}
