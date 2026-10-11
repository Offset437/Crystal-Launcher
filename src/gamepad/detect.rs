//! Connected controllers: gilrs (Windows.Gaming.Input) on a worker thread,
//! classified by USB vendor/product id, with live button/axis state.
use crate::i18n::{t, tf};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Logical controls in SDL's positional naming (South is the bottom face
/// button: A on Xbox, Cross on PlayStation, B on Nintendo).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Btn {
    South,
    East,
    West,
    North,
    LB,
    RB,
    LT,
    RT,
    Back,
    Start,
    LS,
    RS,
    DUp,
    DDown,
    DLeft,
    DRight,
    Guide,
    Touchpad,
    Mute,
}

impl Btn {
    pub(crate) const ALL: [Btn; 19] = [
        Btn::South,
        Btn::East,
        Btn::West,
        Btn::North,
        Btn::LB,
        Btn::RB,
        Btn::LT,
        Btn::RT,
        Btn::Back,
        Btn::Start,
        Btn::LS,
        Btn::RS,
        Btn::DUp,
        Btn::DDown,
        Btn::DLeft,
        Btn::DRight,
        Btn::Guide,
        Btn::Touchpad,
        Btn::Mute,
    ];
    pub(crate) fn bit(self) -> u32 {
        1 << (self as u32)
    }
    pub(crate) fn is_dpad(self) -> bool {
        matches!(self, Btn::DUp | Btn::DDown | Btn::DLeft | Btn::DRight)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    Xbox,
    DualSense,
    DualSenseEdge,
    DualShock4,
    SwitchPro,
    Generic,
}

impl Family {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Family::Xbox => "Xbox",
            Family::DualSense => "PlayStation DualSense",
            Family::DualSenseEdge => "PlayStation DualSense Edge",
            Family::DualShock4 => "PlayStation DualShock 4",
            Family::SwitchPro => "Nintendo Switch Pro",
            Family::Generic => t("Generic controller"),
        }
    }
    pub(crate) fn is_playstation(self) -> bool {
        matches!(self, Family::DualSense | Family::DualSenseEdge | Family::DualShock4)
    }
}

/// Classifies by USB ids, falling back to the device name.
pub(crate) fn classify(vid: Option<u16>, pid: Option<u16>, name: &str) -> Family {
    match (vid, pid) {
        (Some(0x045E), _) => return Family::Xbox,
        (Some(0x054C), Some(0x0CE6)) => return Family::DualSense,
        (Some(0x054C), Some(0x0DF2)) => return Family::DualSenseEdge,
        (Some(0x054C), _) => return Family::DualShock4,
        (Some(0x057E), Some(0x2009)) => return Family::SwitchPro,
        _ => (),
    }
    let n = name.to_ascii_lowercase();
    if n.contains("xbox") || n.contains("xinput") {
        Family::Xbox
    } else if n.contains("dualsense edge") {
        Family::DualSenseEdge
    } else if n.contains("dualsense") {
        Family::DualSense
    } else if n.contains("dualshock") || n.contains("playstation") {
        Family::DualShock4
    } else if n.contains("pro controller") {
        Family::SwitchPro
    } else {
        Family::Generic
    }
}

/// Live input, quantised so tiny stick noise does not cause redraws.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Input {
    pub buttons: u32,
    pub lt: f32,
    pub rt: f32,
    pub lx: f32,
    pub ly: f32,
    pub rx: f32,
    pub ry: f32,
}

impl Input {
    pub(crate) fn pressed(&self, b: Btn) -> bool {
        self.buttons & b.bit() != 0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pad {
    pub id: usize,
    pub name: String,
    pub family: Family,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    /// `Some(true)` wireless, `Some(false)` cable, `None` unknown.
    pub wireless: Option<bool>,
    pub battery: Option<u8>,
    pub rumble: bool,
    pub input: Input,
}

impl Pad {
    pub(crate) fn connection(&self) -> String {
        match (self.wireless, self.battery) {
            (Some(false), _) => t("Wired (USB)").into(),
            (Some(true), Some(b)) => tf("Wireless · battery {percent}%", &[("percent", &b)]),
            (Some(true), None) => t("Wireless").into(),
            (None, _) => t("Connected").into(),
        }
    }
}

enum Command {
    Rumble(usize),
}

fn commands() -> &'static Mutex<Option<mpsc::Sender<Command>>> {
    static C: OnceLock<Mutex<Option<mpsc::Sender<Command>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// Asks the worker to buzz a controller briefly.
pub(crate) fn rumble(id: usize) {
    if let Ok(guard) = commands().lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(Command::Rumble(id));
        }
    }
}

fn q(v: f32) -> f32 {
    (v * 50.0).round() / 50.0
}

/// WGI raw button indices follow the Sony HID descriptor. gilrs' standard
/// Button enum has no touchpad or mic control, but its cache retains these
/// native codes even when the SDL mapping leaves them unnamed.
/// DualSense USB declares 15 buttons; Bluetooth declares only 14, so the
/// mic button requires USB for both capture and ordinary FFXI DirectInput.
#[cfg(any(windows, test))]
fn sony_extra_buttons(family: Family, buttons: impl IntoIterator<Item = (u32, bool)>) -> u32 {
    buttons.into_iter().fold(0, |mask, (code, pressed)| {
        let button = match (family, code) {
            (Family::DualSense | Family::DualSenseEdge | Family::DualShock4, 13) => Some(Btn::Touchpad),
            (Family::DualSense | Family::DualSenseEdge, 14) => Some(Btn::Mute),
            _ => None,
        };
        mask | button.filter(|_| pressed).map_or(0, Btn::bit)
    })
}

fn read_pad(id: gilrs::GamepadId, g: &gilrs::Gamepad) -> Pad {
    use gilrs::{Axis, Button, PowerInfo};
    const MAP: [(Button, Btn); 17] = [
        (Button::South, Btn::South),
        (Button::East, Btn::East),
        (Button::West, Btn::West),
        (Button::North, Btn::North),
        (Button::LeftTrigger, Btn::LB),
        (Button::RightTrigger, Btn::RB),
        (Button::LeftTrigger2, Btn::LT),
        (Button::RightTrigger2, Btn::RT),
        (Button::Select, Btn::Back),
        (Button::Start, Btn::Start),
        (Button::LeftThumb, Btn::LS),
        (Button::RightThumb, Btn::RS),
        (Button::DPadUp, Btn::DUp),
        (Button::DPadDown, Btn::DDown),
        (Button::DPadLeft, Btn::DLeft),
        (Button::DPadRight, Btn::DRight),
        (Button::Mode, Btn::Guide),
    ];
    let (vid, pid) = (g.vendor_id(), g.product_id());
    let family = classify(vid, pid, g.name());
    let mut input = Input::default();
    for (gb, b) in MAP {
        if g.is_pressed(gb) {
            input.buttons |= b.bit();
        }
    }
    #[cfg(windows)]
    {
        input.buttons |= sony_extra_buttons(
            family,
            g.state().buttons().map(|(code, data)| (code.into_u32(), data.is_pressed())),
        );
    }
    let trigger = |b: Button| g.button_data(b).map_or(0.0, |d| d.value());
    input.lt = q(trigger(Button::LeftTrigger2));
    input.rt = q(trigger(Button::RightTrigger2));
    if input.lt > 0.5 {
        input.buttons |= Btn::LT.bit();
    }
    if input.rt > 0.5 {
        input.buttons |= Btn::RT.bit();
    }
    input.lx = q(g.value(Axis::LeftStickX));
    input.ly = q(g.value(Axis::LeftStickY));
    input.rx = q(g.value(Axis::RightStickX));
    input.ry = q(g.value(Axis::RightStickY));
    let (wireless, battery) = match g.power_info() {
        PowerInfo::Wired => (Some(false), None),
        PowerInfo::Discharging(p) | PowerInfo::Charging(p) => (Some(true), Some(p)),
        PowerInfo::Charged => (Some(true), Some(100)),
        PowerInfo::Unknown => (None, None),
    };
    Pad {
        id: usize::from(id),
        name: g.name().to_string(),
        family,
        vid,
        pid,
        wireless,
        battery,
        rumble: g.is_ff_supported(),
        input,
    }
}

fn snapshot(gilrs: &gilrs::Gilrs) -> Vec<Pad> {
    gilrs
        .gamepads()
        .filter(|(_, g)| g.is_connected())
        .map(|(id, g)| read_pad(id, &g))
        .collect()
}

fn play_rumble(gilrs: &mut gilrs::Gilrs, id: usize) -> Option<gilrs::ff::Effect> {
    use gilrs::ff::{BaseEffect, BaseEffectType, EffectBuilder, Repeat, Replay, Ticks};
    let gid = gilrs.gamepads().find(|(g, _)| usize::from(*g) == id)?.0;
    let replay = Replay {
        play_for: Ticks::from_ms(260),
        ..Default::default()
    };
    let effect = EffectBuilder::new()
        .add_effect(BaseEffect {
            kind: BaseEffectType::Strong { magnitude: 42_000 },
            scheduling: replay,
            ..Default::default()
        })
        .add_effect(BaseEffect {
            kind: BaseEffectType::Weak { magnitude: 52_000 },
            scheduling: replay,
            ..Default::default()
        })
        .repeat(Repeat::For(Ticks::from_ms(260)))
        .gamepads(&[gid])
        .finish(gilrs)
        .ok()?;
    effect.play().ok()?;
    Some(effect)
}

/// Runs the controller thread, calling `emit` with the full list whenever a
/// controller connects, disconnects or changes input. Returns when `emit`
/// reports the receiver is gone.
pub(crate) fn run(mut emit: impl FnMut(Vec<Pad>) -> bool) {
    let mut gilrs = match gilrs::Gilrs::new() {
        Ok(g) => g,
        Err(gilrs::Error::NotImplemented(g)) => g,
        Err(_) => {
            emit(Vec::new());
            return;
        }
    };
    let (tx, rx) = mpsc::channel();
    if let Ok(mut guard) = commands().lock() {
        *guard = Some(tx);
    }
    let mut last: Option<Vec<Pad>> = None;
    let mut effects: Vec<(Instant, gilrs::ff::Effect)> = Vec::new();
    let mut last_power = Instant::now();
    loop {
        while gilrs.next_event().is_some() {}
        while let Ok(Command::Rumble(id)) = rx.try_recv() {
            if let Some(e) = play_rumble(&mut gilrs, id) {
                effects.push((Instant::now(), e));
            }
        }
        effects.retain(|(t, _)| t.elapsed() < Duration::from_secs(1));
        let mut now = snapshot(&gilrs);
        // Battery level changes slowly; refresh it every few seconds only, so
        // it never costs a redraw on its own.
        if let Some(prev) = &last {
            if last_power.elapsed() < Duration::from_secs(5) {
                for p in &mut now {
                    if let Some(old) = prev.iter().find(|o| o.id == p.id) {
                        p.battery = old.battery;
                        p.wireless = old.wireless;
                    }
                }
            } else {
                last_power = Instant::now();
            }
        }
        if last.as_ref() != Some(&now) {
            if !emit(now.clone()) {
                break;
            }
            last = Some(now);
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    if let Ok(mut guard) = commands().lock() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamepad_classifies_by_usb_ids() {
        assert_eq!(classify(Some(0x045E), Some(0x0B12), "Controller"), Family::Xbox);
        assert_eq!(classify(Some(0x054C), Some(0x0CE6), "Wireless Controller"), Family::DualSense);
        assert_eq!(classify(Some(0x054C), Some(0x0DF2), ""), Family::DualSenseEdge);
        assert_eq!(classify(Some(0x054C), Some(0x09CC), ""), Family::DualShock4);
        assert_eq!(classify(Some(0x057E), Some(0x2009), ""), Family::SwitchPro);
        assert_eq!(classify(None, None, "Xbox One For Windows"), Family::Xbox);
        assert_eq!(classify(Some(0x0F0D), Some(0x0001), "Arcade Stick"), Family::Generic);
    }

    #[test]
    fn gamepad_sony_extra_buttons_capture_press_and_release() {
        for family in [Family::DualSense, Family::DualSenseEdge] {
            assert_eq!(sony_extra_buttons(family, [(13, true), (14, true)]), Btn::Touchpad.bit() | Btn::Mute.bit());
            assert_eq!(sony_extra_buttons(family, [(13, false), (14, true)]), Btn::Mute.bit());
            assert_eq!(sony_extra_buttons(family, [(13, false), (14, false)]), 0);
        }
        assert_eq!(sony_extra_buttons(Family::DualShock4, [(13, true), (14, true)]), Btn::Touchpad.bit());
    }

    #[test]
    fn gamepad_sony_extra_buttons_ignore_other_devices_axes_and_hats() {
        for family in [Family::Xbox, Family::SwitchPro, Family::Generic] {
            assert_eq!(sony_extra_buttons(family, [(13, true), (14, true)]), 0);
        }
        // WGI encodes the element kind in the high 16 bits. Axis 13/14
        // and synthesized D-pad events must never become extra buttons.
        assert_eq!(sony_extra_buttons(Family::DualSense, [(0x1000d, true), (0x2000e, true), (u32::MAX, true)]), 0);
        assert_eq!(sony_extra_buttons(Family::DualSense, []), 0);
    }

    /// Lists the controllers connected to this machine:
    /// `cargo test gamepad_list_pads -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn gamepad_list_pads() {
        let mut gilrs = gilrs::Gilrs::new().expect("gilrs");
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(1500) {
            while gilrs.next_event().is_some() {}
            std::thread::sleep(Duration::from_millis(10));
        }
        let pads = snapshot(&gilrs);
        println!("{} controller(s)", pads.len());
        for p in &pads {
            println!(
                "{} | {} | vid {:04X?} pid {:04X?} | {} | rumble {} | buttons {:#x}",
                p.name,
                p.family.label(),
                p.vid,
                p.pid,
                p.connection(),
                p.rumble,
                p.input.buttons
            );
        }
    }
}
