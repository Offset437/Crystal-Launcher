//! FINAL FANTASY XI game options: what Square Enix's "FINAL FANTASY XI Config"
//! tool edits, built into the Settings page.
//!
//! Values are read when the launcher starts, edited in memory, and written
//! only on Apply - after the values being replaced are backed up. See
//! `store` for where each launch target reads them.
mod display;
mod model;
mod page;
mod store;

use crate::i18n::{t, tf};
use crate::{App, Message};
use iced::{Element, Subscription, Task};
use model::{Data, Values};
use std::path::PathBuf;
use store::{BackupEntry, Paths, Region, Target};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Section {
    Graphics,
    Sound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Info,
    Success,
    Warning,
    Error,
}

pub(crate) struct State {
    preview: bool,
    requested: bool,
    loaded: bool,
    paths: Paths,
    target: Target,
    /// Regions whose registry key holds game settings.
    regions: Vec<Region>,
    writable: bool,
    /// As stored, when last read.
    original: Values,
    /// As edited; equal to `original` when nothing is pending.
    edits: Values,
    menu_selected: bool,
    menu_retained: bool,
    /// What an entry missing from storage shows as.
    fallback: Values,
    modes: Vec<(u32, u32)>,
    desktop: (u32, u32),
    backups: Vec<BackupEntry>,
    /// Targets backed up this session (a backup precedes the first write).
    backed_up: Vec<Target>,
    notice: Option<(Tone, String)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            preview: std::env::args().any(|a| a.starts_with("--preview")),
            requested: false,
            loaded: false,
            paths: Paths::from_config(&crate::backend::Config::default()),
            target: Target::Retail(Region::Us),
            regions: Vec::new(),
            writable: true,
            original: Values::new(),
            edits: Values::new(),
            menu_selected: false,
            menu_retained: false,
            fallback: model::defaults((1920, 1080)),
            modes: Vec::new(),
            desktop: (1920, 1080),
            backups: Vec::new(),
            backed_up: Vec::new(),
            notice: None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Msg {
    Load,
    Target(Target),
    Set(&'static str, u32),
    Window(u32, u32),
    Menu(u32, u32),
    Background(u32, u32),
    Gamma(f32),
    ScreenshotDir(String),
    Apply,
    Discard,
    Defaults,
    LoadBackup(BackupEntry),
    DismissNotice,
}

impl State {
    fn pending(&self) -> Vec<&'static str> {
        let mut keys = model::changed_keys(&self.original, &self.edits);
        if self.menu_selected {
            for key in [model::MENU_W, model::MENU_H] {
                if !keys.contains(&key) { keys.push(key); }
            }
        }
        keys
    }
    fn get(&self, key: &str) -> Option<&Data> {
        self.edits.get(key).or_else(|| self.fallback.get(key))
    }
    fn dword(&self, key: &str) -> u32 {
        match self.get(key) {
            Some(Data::Dword(v)) => *v,
            _ => 0,
        }
    }
    fn menu_size(&self) -> (u32, u32) {
        let size = (self.dword(model::MENU_W), self.dword(model::MENU_H));
        if size == (0, 0) {
            (self.dword(model::WINDOW_W), self.dword(model::WINDOW_H))
        } else {
            size
        }
    }
    fn text(&self, key: &str) -> &str {
        match self.get(key) {
            Some(Data::Text(v)) => v,
            _ => "",
        }
    }
    fn changed(&self, keys: &[&str]) -> bool {
        keys.iter().any(|k| self.original.get(*k) != self.edits.get(*k)
            || self.menu_selected && matches!(*k, model::MENU_W | model::MENU_H))
    }
    /// Sets an entry; one missing from storage stays missing while it shows
    /// its fallback, so merely looking never makes a change pending.
    fn set(&mut self, key: &str, data: Data) {
        if self.original.get(key).is_none() && self.fallback.get(key) == Some(&data) {
            self.edits.remove(key);
        } else {
            self.edits.insert(key.into(), data);
        }
    }
    fn set_dword(&mut self, key: &str, value: u32) {
        if self.target == Target::Native && !model::native_supports(key, value) {
            return;
        }
        self.set(key, Data::Dword(value));
    }
    /// For the native client, replaces edited values it cannot start with
    /// (see `model::NATIVE_FONT_TYPE`); true when something changed.
    fn guard_native(&mut self) -> bool {
        self.target == Target::Native && !model::guard_native(&mut self.edits).is_empty()
    }

    fn load(&mut self) {
        self.requested = true;
        if self.preview {
            self.load_preview();
            return;
        }
        let (modes, current) = display::modes();
        self.desktop = current.or_else(|| modes.first().copied()).unwrap_or((1920, 1080));
        self.modes = modes;
        self.fallback = model::defaults(self.desktop);
        self.regions = store::retail_regions();
        if !self.regions.iter().any(|region| self.target == Target::Retail(*region)) {
            self.target = Target::Retail(self.regions.first().copied().unwrap_or(Region::Us));
        }
        self.read_target();
    }

    fn read_target(&mut self) {
        self.loaded = true;
        self.menu_selected = false;
        self.menu_retained = match self.target {
            Target::Retail(region) => store::menu_override(&self.paths, region).ok().flatten().is_some(),
            Target::Native => false,
        };
        self.writable = store::writable(self.target);
        self.backups = store::list_backups(&self.paths.backups(), self.target);
        match store::read(&self.paths, self.target) {
            Ok(values) => {
                self.original = values;
                self.edits = self.original.clone();
                if self.guard_native() {
                    self.notice = Some((
                        Tone::Warning,
                        t("The saved font type stops the native client at startup, so it was changed to Compressed. Apply to save the fix.").into(),
                    ));
                }
            }
            Err(e) => {
                self.original = Values::new();
                self.edits = Values::new();
                self.notice = Some((Tone::Error, e));
            }
        }
    }

    /// Realistic synthetic values; nothing on the machine is read.
    fn load_preview(&mut self) {
        let (modes, current) = display::synthetic();
        self.modes = modes;
        self.desktop = current.unwrap_or((3840, 2160));
        self.fallback = model::defaults(self.desktop);
        self.regions = vec![Region::Us];
        self.loaded = true;
        self.writable = true;
        let mut v = self.fallback.clone();
        for (k, x) in [
            (model::WINDOW_W, 2560),
            (model::WINDOW_H, 1440),
            (model::MENU_W, 1708),
            (model::MENU_H, 960),
            (model::WINDOW_MODE, 3),
            (model::TEXTURE_COMPRESSION, 2),
            (model::ENV_ANIMATION, 1),
            (model::MAX_SOUNDS, 16),
            (model::FONT_TYPE, model::NATIVE_FONT_TYPE),
            (model::OPENING_MOVIE, 0),
            (model::SCREENSHOT_SCREEN_RES, 1),
            (model::GAMMA, model::gamma_bits(0.1)),
        ] {
            v.insert(k.into(), Data::Dword(x));
        }
        v.insert(model::SCREENSHOT_DIR.into(), Data::Text("D:\\Pictures\\Vana'diel".into()));
        if std::env::args().any(|a| a == "--preview-menu-auto") {
            v.insert(model::MENU_W.into(), Data::Dword(0));
            v.insert(model::MENU_H.into(), Data::Dword(0));
        }
        v.insert("0031".into(), Data::Dword(0x3bc4_9ba6));
        self.original = v;
        self.edits = self.original.clone();
        // Two pending edits, so the preview shows the unsaved-changes state.
        self.set_dword(model::ENV_ANIMATION, 2);
        self.set_dword(model::MAX_SOUNDS, 20);
        self.backups = vec![
            BackupEntry {
                path: PathBuf::from("preview-1"),
                label: "Oct 2, 2026  21:14".into(),
            },
            BackupEntry {
                path: PathBuf::from("preview-2"),
                label: "Sep 28, 2026  09:40".into(),
            },
        ];
    }

    fn apply(&mut self) {
        let pending = self.pending();
        if pending.is_empty() {
            return;
        }
        if self.preview {
            self.notice = Some((Tone::Info, t("Design preview: nothing was written.").into()));
            return;
        }
        let mut proposed = self.edits.clone();
        // The native file gets every managed entry, so fill in fallbacks.
        if self.target == Target::Native {
            for key in model::MANAGED {
                if !proposed.contains_key(key) {
                    if let Some(d) = self.fallback.get(key) {
                        proposed.insert(key.into(), d.clone());
                    }
                }
            }
            model::guard_native(&mut proposed);
        }
        if let Err(e) = model::validate(&proposed, self.target == Target::Native) {
            self.notice = Some((Tone::Error, e));
            return;
        }
        // Re-read: something else (the game, the official Config tool) may
        // have changed the values since they were loaded.
        let current = match store::read(&self.paths, self.target) {
            Ok(v) => v,
            Err(e) => {
                self.notice = Some((Tone::Error, e));
                return;
            }
        };
        if !model::changed_keys(&self.original, &current).is_empty() {
            self.original = current;
            self.notice = Some((
                Tone::Warning,
                t("These settings were changed outside the launcher since they were loaded. The page now compares against the new values; review your changes and apply again.").into(),
            ));
            return;
        }
        if !store::writable(self.target) {
            self.writable = false;
            self.notice = Some((
                Tone::Error,
                t("Windows refused the change: saving FINAL FANTASY XI's settings needs Crystal Launcher to run as administrator. Nothing was changed.").into(),
            ));
            return;
        }
        if !self.backed_up.contains(&self.target) {
            let location = self.paths.location(self.target);
            match store::save_backup(&self.paths.backups(), self.target, &location, &current, chrono::Local::now().naive_local()) {
                Ok(_) => self.backed_up.push(self.target),
                Err(e) => {
                    self.notice = Some((Tone::Error, tf("{e} Nothing was changed.", &[("e", &e)])));
                    return;
                }
            }
        }
        match store::write(&self.paths, self.target, &self.original, &proposed, self.menu_selected) {
            Ok(()) => {
                let count = pending.len();
                self.read_target();
                self.notice = Some((
                    Tone::Success,
                    if count == 1 {
                        t("Saved 1 change. FINAL FANTASY XI uses it the next time it starts.").into()
                    } else {
                        tf("Saved {count} changes. FINAL FANTASY XI uses them the next time it starts.", &[("count", &count)])
                    },
                ));
            }
            Err(e) => {
                self.notice = Some((Tone::Error, e));
                // Show what is actually stored now; the edits stay pending.
                if let Ok(v) = store::read(&self.paths, self.target) {
                    self.original = v;
                }
            }
        }
    }

    fn load_values(&mut self, values: &Values) {
        for key in model::MANAGED {
            if let Some(d) = values.get(key) {
                self.set(key, d.clone());
            }
        }
        self.guard_native();
    }
}

pub(crate) fn update(state: &mut State, msg: Msg) -> Task<Msg> {
    match msg {
        Msg::Load => {
            if !state.requested {
                state.load();
            }
        }
        Msg::Target(target) => {
            if target != Target::Native && target != state.target && state.pending().is_empty() {
                state.target = target;
                state.notice = None;
                if state.preview {
                    state.writable = true;
                } else {
                    state.read_target();
                }
            }
        }
        Msg::Set(key, value) => state.set_dword(key, value),
        Msg::Window(w, h) => {
            // Keep the interface's magnification as the window changes.
            let scale = model::scale_of(state.dword(model::WINDOW_H), state.menu_size().1).max(1.0);
            state.set_dword(model::WINDOW_W, w);
            state.set_dword(model::WINDOW_H, h);
            let (mw, mh) = model::menu_for(w, h, scale);
            state.set_dword(model::MENU_W, mw.max(model::MIN_MENU.0));
            state.set_dword(model::MENU_H, mh.max(model::MIN_MENU.1));
        }
        Msg::Menu(w, h) => {
            // An explicit choice must be stored even when a missing registry
            // value happens to display this fallback. Otherwise Apply would
            // treat the missing pair as automatic and remove the preference.
            state.edits.insert(model::MENU_W.into(), Data::Dword(w));
            state.edits.insert(model::MENU_H.into(), Data::Dword(h));
            // Selecting the currently displayed registry size still establishes
            // a preference when no Crystal menu size has been saved yet.
            state.menu_selected = matches!(state.target, Target::Retail(_)) && !state.menu_retained && (w, h) != (0, 0);
        }
        Msg::Background(w, h) => {
            state.set_dword(model::BACKGROUND_W, w);
            state.set_dword(model::BACKGROUND_H, h);
        }
        Msg::Gamma(g) => state.set_dword(model::GAMMA, model::gamma_bits(g)),
        Msg::ScreenshotDir(dir) => state.set(model::SCREENSHOT_DIR, Data::Text(dir)),
        Msg::Apply => state.apply(),
        Msg::Discard => {
            state.edits = state.original.clone();
            state.menu_selected = false;
            state.notice = None;
            // A stored value the native client cannot start with stays corrected.
            state.guard_native();
        }
        Msg::Defaults => {
            let defaults = state.fallback.clone();
            state.load_values(&defaults);
            state.notice = Some((
                Tone::Info,
                t("Default settings loaded. Review them, then Apply to save.").into(),
            ));
        }
        Msg::LoadBackup(entry) => {
            if state.preview {
                state.notice = Some((Tone::Info, t("Design preview: no backups were read.").into()));
            } else {
                match store::load_backup(&entry.path) {
                    Ok(backup) => {
                        state.load_values(&backup.values);
                        state.notice = Some((
                            Tone::Info,
                            tf("Loaded the backup from {taken}. Review it, then Apply to save.", &[("taken", &backup.taken)]),
                        ));
                    }
                    Err(e) => state.notice = Some((Tone::Error, e)),
                }
            }
        }
        Msg::DismissNotice => state.notice = None,
    }
    Task::none()
}

pub(crate) fn subscription(state: &State) -> Subscription<Msg> {
    // Read once, as soon as the app runs (a few registry values: cheap).
    if state.requested {
        Subscription::none()
    } else {
        iced::time::every(std::time::Duration::from_millis(10)).map(|_| Msg::Load)
    }
}

pub(crate) fn view(app: &App, section: Section) -> Element<'_, Message> {
    page::view(app, section)
}

pub(crate) fn retail_menu(config: &crate::backend::Config) -> Result<Option<(u32, u32)>, String> {
    store::menu_override(&Paths::from_config(config), Region::Us)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded() -> State {
        let mut s = State {
            preview: true,
            ..State::default()
        };
        s.load();
        s.edits = s.original.clone();
        s
    }

    #[test]
    fn automatic_retail_menu_uses_the_window_without_pending_edits() {
        let mut s = loaded();
        s.original.remove(model::SCREENSHOT_DIR);
        for key in [model::MENU_W, model::MENU_H] {
            s.original.insert(key.into(), Data::Dword(0));
        }
        s.edits = s.original.clone();
        assert_eq!(s.menu_size(), (2560, 1440));
        assert!(s.pending().is_empty());
        assert!(model::validate(&s.edits, false).is_ok());
        assert!(model::validate(&s.edits, true).is_err());
        let _ = update(&mut s, Msg::Set(model::MAX_SOUNDS, 20));
        assert_eq!(s.pending(), vec![model::MAX_SOUNDS]);
        let _ = update(&mut s, Msg::Window(1920, 1080));
        assert_eq!(s.menu_size(), (1920, 1080));
    }

    #[test]
    fn choosing_the_current_menu_can_retain_it_across_profile_starts() {
        let mut s = loaded();
        let size = s.menu_size();
        let _ = update(&mut s, Msg::Menu(size.0, size.1));
        assert_eq!(s.pending(), vec![model::MENU_W, model::MENU_H]);
        let _ = update(&mut s, Msg::Discard);
        assert!(s.pending().is_empty());
        s.menu_retained = true;
        let _ = update(&mut s, Msg::Menu(size.0, size.1));
        assert!(s.pending().is_empty());
    }

    #[test]
    fn choosing_a_fallback_menu_stores_both_dimensions() {
        let mut s = loaded();
        for key in [model::MENU_W, model::MENU_H] {
            s.original.remove(key);
            s.edits.remove(key);
        }
        let size = s.menu_size();
        assert!(s.pending().is_empty());
        let _ = update(&mut s, Msg::Menu(size.0, size.1));
        assert_eq!(model::dword(&s.edits, model::MENU_W), Some(size.0));
        assert_eq!(model::dword(&s.edits, model::MENU_H), Some(size.1));
        assert_eq!(s.pending(), vec![model::MENU_W, model::MENU_H]);
        let _ = update(&mut s, Msg::Discard);
        assert!(s.pending().is_empty());
        assert!(!s.edits.contains_key(model::MENU_W));
    }

    #[test]
    fn game_config_edits_stay_in_memory_until_apply() {
        let mut s = loaded();
        assert!(s.pending().is_empty());
        let _ = update(&mut s, Msg::Set(model::MIP_MAPPING, 2));
        assert_eq!(s.pending(), vec![model::MIP_MAPPING]);
        // Setting the stored value again clears the change.
        let _ = update(&mut s, Msg::Set(model::MIP_MAPPING, 6));
        assert!(s.pending().is_empty());
        let _ = update(&mut s, Msg::Set(model::BUMP_MAPPING, 0));
        let _ = update(&mut s, Msg::Discard);
        assert!(s.pending().is_empty());
    }

    #[test]
    fn game_config_preview_never_writes() {
        let mut s = loaded();
        let _ = update(&mut s, Msg::Set(model::SOUND, 0));
        let _ = update(&mut s, Msg::Apply);
        assert_eq!(s.notice.as_ref().map(|n| n.0), Some(Tone::Info));
        assert_eq!(s.pending(), vec![model::SOUND]);
        assert!(s.backed_up.is_empty());
    }

    #[test]
    fn game_config_window_change_keeps_interface_scale() {
        let mut s = loaded(); // 2560 x 1440 with a 1708 x 960 menu: 1.5x
        let _ = update(&mut s, Msg::Window(3840, 2160));
        assert_eq!((s.dword(model::MENU_W), s.dword(model::MENU_H)), (2560, 1440));
        let _ = update(&mut s, Msg::Window(1280, 720));
        assert_eq!((s.dword(model::MENU_W), s.dword(model::MENU_H)), (854, 480));
    }

    #[test]
    fn game_config_missing_entries_show_fallback_without_pending() {
        let mut s = loaded();
        s.original.remove(model::KEEP_ASPECT);
        s.edits.remove(model::KEEP_ASPECT);
        assert_eq!(s.dword(model::KEEP_ASPECT), 1);
        let _ = update(&mut s, Msg::Set(model::KEEP_ASPECT, 1));
        assert!(s.pending().is_empty());
        let _ = update(&mut s, Msg::Set(model::KEEP_ASPECT, 0));
        assert_eq!(s.pending(), vec![model::KEEP_ASPECT]);
    }

    #[test]
    fn game_config_defaults_load_as_pending_edits() {
        let mut s = loaded();
        let _ = update(&mut s, Msg::Defaults);
        assert!(!s.pending().is_empty());
        assert_eq!(s.dword(model::WINDOW_W), 3840);
        // Unmanaged values are untouched.
        assert_eq!(s.edits.get("0031"), Some(&Data::Dword(0x3bc4_9ba6)));
    }

    #[test]
    fn game_config_native_never_offers_a_crashing_font_type() {
        let mut s = loaded();
        s.target = Target::Native; // Legacy-file compatibility, not a UI target.
        assert_eq!(s.target, Target::Native);
        let _ = update(&mut s, Msg::Set(model::FONT_TYPE, 2));
        let _ = update(&mut s, Msg::Set(model::FONT_TYPE, 1));
        assert!(s.pending().is_empty());
        assert_eq!(s.dword(model::FONT_TYPE), model::NATIVE_FONT_TYPE);
        // A backup or defaults holding High quality load as Compressed.
        let mut backup = s.original.clone();
        backup.insert(model::FONT_TYPE.into(), Data::Dword(2));
        s.load_values(&backup);
        assert_eq!(s.dword(model::FONT_TYPE), model::NATIVE_FONT_TYPE);
        // The registry targets keep every font type.
        let _ = update(&mut s, Msg::Discard);
        let _ = update(&mut s, Msg::Target(Target::Retail(Region::Us)));
        let _ = update(&mut s, Msg::Set(model::FONT_TYPE, 2));
        assert_eq!(s.dword(model::FONT_TYPE), 2);
    }

    #[test]
    fn game_config_native_read_corrects_a_stored_crashing_font_type() {
        let root = std::env::temp_dir().join(format!("game-config-test-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let launcher = root.join("iced-launcher");
        std::fs::create_dir_all(&launcher).unwrap();
        std::fs::write(
            launcher.join("game-settings.reg"),
            "REGEDIT4\r\n\r\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\PlayOnlineUS\\SquareEnix\\FinalFantasyXI]\r\n\"0001\"=dword:00000f00\r\n\"0036\"=dword:00000002\r\n",
        )
        .unwrap();
        let mut s = State {
            target: Target::Native,
            paths: store::Paths {
                base_reg: root.join("repo/ffxi.reg"),
                accounts: root.join("test-launcher/data"),
                launcher,
            },
            ..State::default()
        };
        s.read_target();
        assert_eq!(model::dword(&s.original, model::FONT_TYPE), Some(2));
        assert_eq!(s.dword(model::FONT_TYPE), model::NATIVE_FONT_TYPE);
        assert_eq!(s.pending(), vec![model::FONT_TYPE]);
        assert_eq!(s.notice.as_ref().map(|n| n.0), Some(Tone::Warning));
        // Discarding keeps the correction pending; it is never offered back.
        let _ = update(&mut s, Msg::Discard);
        assert_eq!(s.pending(), vec![model::FONT_TYPE]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn game_config_target_switch_waits_for_pending_changes() {
        let mut s = loaded();
        let _ = update(&mut s, Msg::Set(model::SOUND, 0));
        let _ = update(&mut s, Msg::Target(Target::Retail(Region::Eu)));
        assert_eq!(s.target, Target::Retail(Region::Us));
    }

    #[test]
    fn game_config_does_not_offer_the_disabled_native_target() {
        let mut s = loaded();
        let _ = update(&mut s, Msg::Target(Target::Native));
        assert_eq!(s.target, Target::Retail(Region::Us));
    }
}
