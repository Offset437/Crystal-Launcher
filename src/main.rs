#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backend;
mod account_import;
mod account_import_ui;
mod account_order;
mod discovery;
mod controller_nav;
mod dgvoodoo;
mod reshade;
mod friends;
mod social;
mod game_config;
mod ffxiah;
mod gamepad;
mod i18n;
mod icons;
mod installer;
mod launch_log;
mod launch_timing;
mod launch_tools;
mod prereqs;
mod settings_page;
mod operation;
mod patch;
mod pol_less;
mod privacy;
mod services;
mod self_update;
mod session_worker;
mod ui;
mod updater;
mod viewer;
mod window_chrome;

use backend::{account_key, Account, AccountMode, Config, LaunchResult};
use i18n::{t, tf};
use iced::{window, Font, Subscription, Task};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};
use zeroize::Zeroize;

#[derive(Clone, Copy)]
pub(crate) enum Field {
    Label,
    Server,
    PolId,
    PolPassword,
    SeId,
    SePassword,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Home,
    Accounts,
    Friends,
    Maintenance,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaintenanceKind {
    Check,
    Update,
    Repair,
    Download,
    Extract,
    Setup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MainAction {
    Play,
    Update,
    Maintenance,
    Offline,
    Working,
}
impl MaintenanceKind {
    pub fn blocks_launch(self) -> bool {
        matches!(self, Self::Update | Self::Repair | Self::Setup)
    }
    pub fn cancellable(self) -> bool {
        !matches!(self, Self::Extract | Self::Setup)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Check => t("Checking files"),
            Self::Update => t("Updating PlayOnline and FFXI"),
            Self::Repair => t("Repairing files"),
            Self::Download => t("Downloading official installer"),
            Self::Extract => t("Extracting official installer"),
            Self::Setup => t("Official setup is running"),
        }
    }
}

/// What the Downloads page's "Last result" line reports, kept apart from its
/// (translated) text so the page never has to match words in a language.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) enum ResultTone {
    #[default]
    Done,
    /// Nothing has been checked yet.
    Unverified,
    /// The operation failed or was cancelled.
    Stopped,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Settings {
    pub install_path: String,
    #[serde(default)]
    pub light_theme: bool,
    #[serde(default)]
    pub install_selected: bool,
    #[serde(default = "default_installer_path")]
    pub installer_path: String,
    #[serde(default)]
    pub installer_selected: bool,
    #[serde(default)]
    selected_accounts: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub account_order: Vec<String>,
    // The default launcher (Settings → System → Launch tools), for accounts whose own
    // launch_with is None. Home's Launch with edits the shown account's own choice.
    #[serde(default)]
    pub launch_with: ui::LaunchWith,
    // Settings → System → Launch tools. Empty means detect / default.
    #[serde(default)]
    pub ashita_path: String,
    #[serde(default)]
    pub windower_path: String,
    #[serde(default)]
    pub ashita_boot_ini: String,
    #[serde(default)]
    pub windower_profile: String,
    #[serde(default)]
    pub loader_path: String,
    /// Non-secret per-account installation overrides; the encrypted vault stays compatible.
    #[serde(default)]
    pub account_installs: HashMap<String, String>,
    #[serde(default)]
    pub account_overlays: HashMap<String, Vec<String>>,
    /// Per-account PlayOnline Auto-Away minutes (0 = off); absent = 10.
    #[serde(default)]
    pub account_auto_away: HashMap<String, u8>,
    /// Opt-in background POL sign-in on this GUI's first account load.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub account_startup_signin: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub account_auto_login: HashMap<String, pol_less::AutoLogin>,
    /// The launcher's own UI language (Settings page). Absent = follow the
    /// Windows display language. The game's language is never changed here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<i18n::Lang>,
    /// Screenshot privacy: names, IDs, mail and the user folder are shown
    /// blurred (`privacy.rs`). Display only. Absent = off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub privacy: bool,
    /// Settings → Graphics. On by default (older files without it too): Crystal
    /// installs dgVoodoo and puts it next to the loader at each launch.
    #[serde(default = "default_true")]
    pub dgvoodoo: bool,
    #[serde(default)]
    pub reshade: reshade::Preferences,
    #[serde(default = "default_true")]
    pub launcher_auto_updates: bool,
    #[serde(default = "default_true")]
    pub controller_navigation: bool,
}
fn default_true() -> bool {
    true
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            install_path: String::new(),
            install_selected: false,
            light_theme: false,
            installer_path: default_installer_path(),
            installer_selected: false,
            selected_accounts: None,
            account_order: Vec::new(),
            launch_with: ui::LaunchWith::default(),
            ashita_path: String::new(),
            windower_path: String::new(),
            ashita_boot_ini: String::new(),
            windower_profile: String::new(),
            loader_path: String::new(),
            account_installs: HashMap::new(),
            account_overlays: HashMap::new(),
            account_auto_away: HashMap::new(),
            account_startup_signin: BTreeSet::new(),
            account_auto_login: HashMap::new(),
            language: None,
            privacy: false,
            dgvoodoo: true,
            reshade: reshade::Preferences::default(),
            launcher_auto_updates: true,
            controller_navigation: true,
        }
    }
}

pub(crate) struct AccountRow {
    pub key: String,
    pub account: Account,
    pub selected: bool,
    pub otp: String,
    pub busy: bool,
    /// Background sign-in can accept Play without starting another login.
    preparing_session: bool,
    queued_play: Option<Instant>,
    /// A coordinated Play owns this row even if an older startup sign-in finishes.
    batch_pending: bool,
    /// This window is awaiting its batch callback; ownership may not be published yet.
    batch_requested: bool,
    launch_generation: u64,
    pub status: String,
    pub pid: Option<u32>,
    alive: Option<Arc<AtomicBool>>,
    game_running: Option<Arc<AtomicBool>>,
    /// Launch diagnostics: the client's stderr log, how it ended, and whether that was a problem.
    pub log: Option<PathBuf>,
    outcome: Option<Arc<std::sync::Mutex<Option<launch_log::ExitReport>>>>,
    pub problem: bool,
    /// How long the running launch took, e.g. "Signed in 2.8s · game started 9.4s".
    pub timing: String,
    /// A Play from an already authenticated session, measured separately from sign-in.
    play_timing: Option<launch_timing::Timeline>,
    /// How the last client ended, while `status` still holds its (English) summary.
    exit: Option<launch_log::ExitReport>,
}
impl Drop for AccountRow {
    fn drop(&mut self) {
        self.otp.zeroize();
    }
}
impl AccountRow {
    pub(crate) fn is_playing(&self) -> bool {
        self.pid.is_some() && self.game_running.as_ref().is_none_or(|g| g.load(Ordering::Acquire))
    }
    pub(crate) fn can_play(&self) -> bool {
        !self.batch_pending && !self.is_playing() && self.queued_play.is_none() && (!self.busy || self.preparing_session)
    }
    fn queued_signin_play(&self) -> bool {
        self.busy && self.preparing_session && self.queued_play.is_some() && !self.batch_pending && !self.is_playing()
    }
    pub(crate) fn game_start_pending(&self) -> bool {
        self.batch_pending || self.queued_play.is_some() || self.busy && !self.preparing_session
    }
    /// The status line to show. A client's exit report is worded in the
    /// current language here; any later status replaces it.
    pub(crate) fn status_text(&self) -> std::borrow::Cow<'_, str> {
        match &self.exit {
            Some(report) if report.summary == self.status => report.message().into(),
            _ => self.status.as_str().into(),
        }
    }
    /// Show how the client ended.
    fn set_exit(&mut self, report: launch_log::ExitReport) {
        self.problem = report.is_problem();
        self.status = report.summary.clone();
        self.exit = Some(report);
    }
}
pub(crate) struct Editor {
    pub key: Option<String>,
    pub account: Account,
    pub game_install: String,
    pub dat_overlays: Vec<String>,
    pub startup_signin: bool,
    pub auto_login: pol_less::AutoLogin,
}

/// The Viewer refreshes its last-input stamp from window input (app 10060039)
/// for Auto-Away; Crystal forwards window input at most this often.
const ACTIVITY_INTERVAL: Duration = Duration::from_secs(5);
static LAST_ACTIVITY: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

fn activity_due(last: &mut Option<Instant>, now: Instant) -> bool {
    if last.is_some_and(|at| now.saturating_duration_since(at) < ACTIVITY_INTERVAL) {
        return false;
    }
    *last = Some(now);
    true
}

fn user_input(event: iced::Event, status: iced::event::Status, _: window::Id) -> Option<Message> {
    use iced::{keyboard, mouse};
    if status == iced::event::Status::Ignored && matches!(&event,
        iced::Event::Keyboard(keyboard::Event::KeyPressed { key: keyboard::Key::Named(keyboard::key::Named::Escape), repeat: false, .. })) {
        return Some(Message::ControllerBack);
    }
    let input = matches!(event,
        iced::Event::Keyboard(keyboard::Event::KeyPressed { .. })
            | iced::Event::Mouse(mouse::Event::ButtonPressed(_) | mouse::Event::WheelScrolled { .. } | mouse::Event::CursorMoved { .. }));
    (input && activity_due(&mut LAST_ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()), Instant::now())).then_some(Message::UserInput)
}

#[derive(Clone)]
enum WorkerEvent {
    /// Ignore a startup/reconnect result superseded by a coordinated Play.
    Launch(String, u64, Box<WorkerEvent>),
    Progress(String, String),
    Complete(String, Result<LaunchResult, String>),
    /// A news update, tagged with its request and the language its feeds were read in.
    Services(u64, i18n::Lang, services::Update),
    Friends(u64, String, bool, Option<friends::Snapshot>),
    /// The stock FFXI version check ran before starting the game from a signed-in session:
    /// what it found, then whether the start request went out.
    StartGame(String, patch::PatchReport, Result<String, String>),
    /// A line for the status bar that outlives the account's progress lines.
    Notice(String),
    WindowerProfileProgress(String),
    Viewer(u64, viewer::Status),
}
impl WorkerEvent {
    fn for_launch(self, key: &str, generation: u64) -> Self {
        Self::Launch(key.to_owned(), generation, Box::new(self))
    }
}

#[derive(Clone)]
pub(crate) enum Message {
    Tick,
    ControllerTick,
    ControllerBack,
    ControllerPage(i8),
    ControllerPlay,
    ControllerNavigation(bool),
    SelfUpdate(self_update::Msg),
    WindowResized(iced::Size),
    MinimizeWindow,
    MaximizeWindow,
    // native chrome, title double-click and edge resizing.
    WindowOpened(window::Id),
    NativeWindow(isize),
    WindowMaximized(bool),
    TitlePressed,
    TitleMoved,
    TitleReleased,
    ResizeWindow(window::Direction),
    Navigate(Page),
    FriendsAccount(String),
    FriendsConnect,
    FriendsPlay,
    FriendsSignOut,
    FriendsStatus(session_worker::NativeControl),
    FriendsStatusGroups(bool),
    FriendsAutoAway(u8),
    /// Keyboard or mouse input reached the window (throttled at the source).
    UserInput,
    FriendsQuery(String),
    /// Open a conversation, the Mailbox or the block list (None: nothing).
    FriendsSelect(Option<friends::Pane>),
    /// Fold or unfold the Online (0) / Offline (1) section.
    FriendsSection(usize),
    FriendsStatusOptions,
    FriendsAddSubject(bool),
    FriendsSocial(social::Request),
    FriendsRemove(friends::Friend),
    FriendsRemoveConfirm,
    FriendsSubject(String),
    FriendsMessageEdit(iced::widget::text_editor::Action),
    FriendsSend,
    FriendsDraftClose,
    ToggleDetails,
    ToggleAccount(String, bool),
    AccountDragStart(String),
    AccountDragOver(String),
    AccountDragFinish,
    AccountDragCancel,
    EditWindowerProfiles,
    WindowerProfilesClosed(Result<(), String>),
    Launch(String),
    LaunchSelected,
    MainAction,
    LaunchAll,
    OtpChanged(String, String),
    NewAccount,
    EditAccount(String),
    EditorChanged(Field, String),
    EditorRegion(u8),
    // the editor's Launch section (None = use the default).
    EditorLaunchWith(Option<ui::LaunchWith>),
    EditorStartupSignin(bool),
    EditorAutoLogin(bool),
    EditorCharacterSlot(u8),
    AccountImport(account_import_ui::Msg),
    EditorBootIni(Option<String>),
    EditorProfile(Option<String>),
    SaveAccount,
    RequestDeleteAccount(String),
    CancelDeleteAccount,
    DeleteAccount(String),
    CloseEditor,
    ToggleSettings,
    SetLightTheme(bool),
    SetLanguage(i18n::Lang),
    SetPrivacy(bool),
    ChooseProfile(String),
    SetLaunchWith(ui::LaunchWith),
    WikiQueryChanged(String),
    AhQueryChanged(String),
    SearchWiki,
    SearchAh,
    AhResults(u64, Result<ffxiah::Results, String>),
    CloseAh,
    AhServerChanged(&'static str),
    NewsFilter(u8),
    InstallPathChanged(String),
    SelectInstallPath,
    FindInstallation,
    InstallationFound(u64, bool, Option<PathBuf>),
    InstallerPathChanged(String),
    AccountInstallChanged(String),
    PickedAccountFolder(String, Option<PathBuf>),
    PickedAccountOverlay(String, Option<PathBuf>),
    SelectInstallerPath,
    /// Open the native folder (or loader file) picker for a path field.
    Browse(PickTarget),
    Picked(PickTarget, Option<PathBuf>),
    StartMaintenance(MaintenanceKind),
    MaintenanceFinished(MaintenanceKind, Result<String, String>, Option<patch::PatchReport>),
    CancelMaintenance,
    SaveSettings,
    Refresh,
    OpenUrl(String),
    OpenLog(String),
    OpenLogsFolder(String),
    AccountsLoaded(Result<Vec<Account>, String>),
    AccountsSaved(Result<Vec<Account>, String>),
    /// The finished news refresh, its request generation and language.
    ServicesLoaded(u64, i18n::Lang, Result<services::ServiceSnapshot, String>),
    ServiceStatusLoaded(u64, i18n::Lang, Result<services::ServiceSnapshot, String>),
    PatchLoaded(u64, patch::PatchReport),
    ViewerLoaded(u64, viewer::Status),
    CloseRequested,
    ConfirmClose,
    CancelClose,
    PreviewReady(window::Screenshot),
    // The Settings page features own their state and messages in their modules.
    SettingsTab(settings_page::Tab),
    GameConfig(game_config::Msg),
    Prereqs(prereqs::Msg),
    Gamepad(gamepad::Msg),
    OpenSettingsTab(settings_page::Tab), // banner Fix → Settings tab
    LaunchTools(launch_tools::Msg),
    Dgvoodoo(dgvoodoo::Msg),
    Reshade(reshade::Msg),
}
/// A path field that has a native Browse dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PickTarget {
    GameFolder,
    AccountGameFolder,
    AccountDatFolder,
    InstallerFolder,
    AshitaFolder,
    WindowerFolder,
    /// A file, not a folder: the loader's pol.exe.
    LoaderFile,
}

// iced requires Debug, but input messages can contain account passwords.
impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LauncherMessage (payload redacted)")
    }
}

pub(crate) struct App {
    pub viewport: iced::Size,
    pub maximized: bool,
    pub title_held: bool, // title pressed, drag not yet started
    pub page: Page,
    pub show_details: bool,
    pub accounts: Vec<AccountRow>,
    pub account_drag: account_order::Drag,
    pub editor: Option<Editor>,
    pub account_import: account_import_ui::State,
    pub services: Option<services::ServiceSnapshot>,
    pub news_loading: bool,
    /// The language of the news refresh in flight (meaningful while `news_loading`).
    news_lang: i18n::Lang,
    news_generation: u64,
    pub news_error: String,
    startup_warning: Option<String>,
    pub patch: Option<patch::PatchReport>,
    pub patch_loading: bool,
    pub installation_loading: bool,
    installation_generation: u64,
    pub viewer: Option<viewer::Status>,
    pub viewer_loading: bool,
    viewer_generation: u64,
    pub settings: Settings,
    pub show_settings: bool,
    pub status: String,
    pub vault_ready: bool,
    startup_signin_pending: bool,
    pub confirm_close: bool,
    pub pending_delete: Option<String>,
    pub wiki_query: String,
    pub ah_query: String,
    pub ah_lookup: ffxiah::Lookup,
    pub ah_server: &'static str,
    pub news_filter: u8,
    pub maintenance: Option<MaintenanceKind>,
    pub maintenance_progress: operation::Progress,
    pub maintenance_result: String,
    pub installer_error: Option<String>,
    pub maintenance_tone: ResultTone,
    pub maintenance_cancelled: bool,
    active_operation: Option<operation::Operation>,
    config: Config,
    sender: mpsc::Sender<WorkerEvent>,
    receiver: mpsc::Receiver<WorkerEvent>,
    vault_saving: bool,
    patch_generation: u64,
    refreshed: Instant,
    preview: bool,
    preview_capture_sent: bool,
    pub settings_tab: settings_page::Tab,
    pub game_config: game_config::State,
    pub prereqs: prereqs::State,
    pub gamepad: gamepad::State,
    pub launch_tools: launch_tools::State,
    pub windower_editor_open: bool,
    pub friends: friends::State,
    friends_generation: u64,
    friends_checked: Instant,
    pub dgvoodoo: dgvoodoo::State,
    pub reshade: reshade::State,
    pub self_update: self_update::State,
    pub controller_nav: controller_nav::State,
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let config = Config::default();
        let preview = std::env::args().any(|a| a.starts_with("--preview"));
        let (settings, startup_warning) = if preview {
            (Settings::default(), None)
        } else {
            match load_settings_and_language(&config) {
                Ok(settings) => (settings, None),
                Err(error) => (
                    Settings::default(),
                    Some(tf("{error} The original settings file was kept; defaults are being used.", &[("error", &error)])),
                ),
            }
        };
        // Previews are English unless --preview-ja, whatever Windows is set to.
        i18n::set(if preview {
            if std::env::args().any(|a| a == "--preview-ja") { i18n::Lang::Ja } else { i18n::Lang::En }
        } else {
            settings.language.unwrap_or_else(i18n::system_default)
        });
        privacy::set(if preview { std::env::args().any(|a| a == "--preview-private") } else { settings.privacy });
        let (sender, receiver) = mpsc::channel();
        // With dgVoodoo on and not yet installed, its download starts in the background now,
        // so the first Play doesn't wait for it.
        let dgvoodoo = dgvoodoo::State::new(settings.dgvoodoo);
        let mut app = Self {
            viewport: initial_window_size(),
            maximized: false,
            title_held: false,
            page: Page::Home,
            show_details: false,
            accounts: Vec::new(),
            account_drag: account_order::Drag::default(),
            windower_editor_open: false,
            editor: None,
            account_import: account_import_ui::State::default(),
            services: if preview { None } else { services::cached(&news_cache_path(&config, i18n::current()), i18n::current()) },
            news_loading: false,
            news_lang: i18n::current(),
            news_generation: 0,
            news_error: String::new(),
            startup_warning,
            patch: None,
            patch_loading: false,
            installation_loading: false,
            installation_generation: 0,
            viewer: None,
            viewer_loading: false,
            viewer_generation: 0,
            settings,
            show_settings: false,
            status: t("Loading saved accounts…").into(),
            vault_ready: false,
            startup_signin_pending: !preview,
            confirm_close: false,
            pending_delete: None,
            wiki_query: String::new(),
            ah_query: String::new(),
            ah_lookup: ffxiah::Lookup::default(),
            ah_server: ui::DEFAULT_AH_SERVER,
            news_filter: 0,
            maintenance: None,
            maintenance_progress: operation::Progress::default(),
            maintenance_result: t("Files have not been checked. Version availability alone does not verify file integrity.").into(),
            installer_error: None,
            maintenance_tone: ResultTone::Unverified,
            maintenance_cancelled: false,
            active_operation: None,
            config: config.clone(),
            sender,
            receiver,
            vault_saving: false,
            patch_generation: 0,
            refreshed: Instant::now(),
            preview,
            preview_capture_sent: false,
            settings_tab: settings_page::Tab::default(),
            game_config: game_config::State::default(),
            prereqs: prereqs::State::default(),
            gamepad: gamepad::State::default(),
            launch_tools: launch_tools::State::default(),
            friends: friends::State::default(),
            friends_generation: 0,
            friends_checked: Instant::now(),
            dgvoodoo,
            reshade: reshade::State::default(),
            self_update: self_update::initial(&config),
            controller_nav: controller_nav::State::default(),
        };
        app.launch_tools.refresh(&app.settings);
        if preview {
            app.settings.light_theme = std::env::args().any(|a| a == "--preview-light");
            app.settings.privacy = privacy::on();
            app.load_preview();
            if std::env::args().any(|a| a == "--preview-vanilla") {
                app.settings.launch_with = ui::LaunchWith::Vanilla;
                for row in &mut app.accounts {
                    if row.account.mode == AccountMode::Retail { row.account.launch_with = Some(ui::LaunchWith::Vanilla); }
                }
            }
            if std::env::args().any(|a| a == "--preview-ffxiah") { app.load_ah_preview(); }
            if std::env::args().any(|a| a == "--preview-news-events") { app.news_filter = 2; }
            if std::env::args().any(|a| a == "--preview-news-maintenance") { app.news_filter = 3; }
            if std::env::args().any(|a| a == "--preview-news-empty") { app.news_filter = 1; }
            if std::env::args().any(|a| matches!(a.as_str(), "--preview-friends" | "--preview-friends-ingame" | "--preview-inbox" | "--preview-compose")) {
                let flag = |f: &str| std::env::args().any(|a| a == f);
                app.load_friends_preview(flag("--preview-inbox"), flag("--preview-friends-ingame"), flag("--preview-compose"), flag("--preview-new-mail"));
            }
            // --preview-settings-<tab> opens a Settings tab (graphics, sound, controller, system).
            if let Some(tab) = std::env::args().find_map(|a| {
                a.strip_prefix("--preview-settings-").and_then(settings_page::Tab::from_flag)
            }) {
                app.page = Page::Settings;
                app.settings_tab = tab;
            }
            if std::env::args().any(|a| a == "--preview-accounts") {
                app.page = Page::Accounts;
                // a client that crashed, with its log actions.
                let crashed = launch_log::classify_exit(
                    "[recomp] xinput: pad 0 polled, nothing connected\n\n[recomp] FATAL at 102dbbc4: indirect call/jump to an address with no translation\n",
                    Some(1),
                    Some(Duration::from_secs(8)),
                );
                if let Some(row) = app.accounts.get_mut(0) {
                    row.set_exit(crashed);
                    row.log = Some(PathBuf::from("preview.stderr.log"));
                }
                // show every row state in the accounts preview.
                if let Some(row) = app.accounts.get_mut(1) {
                    row.busy = true;
                    row.status = t("Signing in to PlayOnline…").into();
                }
                if let Some(row) = app.accounts.get_mut(2) {
                    row.account.mode = backend::AccountMode::Private;
                    row.account.server = "play.example-server.net".into();
                    row.selected = false;
                }
                if std::env::args().any(|a| a == "--preview-empty") {
                    app.accounts.clear();
                }
            }
            // The Play button with the launch profile's game running / starting.
            if std::env::args().any(|a| a == "--preview-playing") {
                for row in app.accounts.iter_mut().skip(1) {
                    row.selected = false;
                }
                if let Some(row) = app.accounts.get_mut(0) {
                    row.selected = true;
                    row.pid = Some(4242);
                }
            }
            if std::env::args().any(|a| a == "--preview-starting") {
                for row in app.accounts.iter_mut().skip(1) {
                    row.selected = false;
                }
                if let Some(row) = app.accounts.get_mut(0) {
                    row.selected = true;
                    row.busy = true;
                    row.status = t("Signing in to PlayOnline…").into();
                }
            }
            // close-confirmation preview.
            if std::env::args().any(|a| a == "--preview-close") {
                app.confirm_close = true;
            }
            if std::env::args().any(|a| a == "--preview-delete") {
                app.pending_delete = Some(app.accounts[0].key.clone());
            }
            if std::env::args().any(|a| a == "--preview-maintenance") {
                app.page = Page::Maintenance;
                app.viewer = Some(viewer::Status {
                    folder: Some(PathBuf::from(r"C:\Games\PlayOnlineViewer")),
                    version: patch::PatchReport {
                        state: patch::PatchState::Required,
                        local_version: t("Not initialized").into(),
                        remote_version: "20110829_E".into(),
                        detail: t("PlayOnline has not completed its initial update. Open Downloads and choose Update.").into(),
                        source_url: None, files_verified: None,
                    },
                    core_error: None,
                });
                if std::env::args().any(|a| a == "--preview-fresh") {
                    app.patch = Some(patch::PatchReport {
                        state: patch::PatchState::Required,
                        local_version: t("Not initialized").into(), remote_version: "30260904_1".into(),
                        detail: t("FFXI has not completed its initial update. Choose Update to finish and create its installed-version record.").into(),
                        source_url: None, files_verified: None,
                    });
                }
            }
            // a synthetic mid-download state for the Downloads preview.
            if std::env::args().any(|a| a == "--preview-busy") {
                app.settings.install_selected = true;
                app.settings.installer_selected = true;
                app.maintenance = Some(MaintenanceKind::Download);
                let part = operation::Part {
                    number: 3,
                    name: "FFXIFullSetup_US.part3.rar",
                    state: operation::PartState::Downloading { done: 678_760_000, expected: 1_610_612_736 },
                };
                app.maintenance_progress = operation::Progress {
                    stage: "installer-download".into(),
                    done: 3_900_000_000,
                    total: 7_734_210_712,
                    detail: part.english(),
                    part: Some(part),
                    bytes: true,
                    remaining: Some(Duration::from_secs(115)),
                    overall: None,
                };
            }
            if std::env::args().any(|a| a == "--preview-update") {
                app.page = Page::Maintenance;
                app.settings.install_selected = true;
                app.maintenance = Some(MaintenanceKind::Update);
                app.maintenance_progress = operation::Progress {
                    stage: "Download".into(), done: 42_000_000, total: 120_000_000,
                    detail: "ROM/285/40.DAT".into(), bytes: true,
                    remaining: Some(Duration::from_secs(115)),
                    overall: Some(operation::OverallProgress {
                        fraction: Some(0.28), remaining: Some(Duration::from_secs(265)), complete: false,
                    }),
                    ..operation::Progress::default()
                };
            }
            if std::env::args().any(|a| a == "--preview-controller-nav") {
                app.controller_nav.preview_focus();
            }
            if std::env::args().any(|a| a == "--preview-details") {
                app.show_details = true;
            }
            if std::env::args().any(|a| a == "--preview-server-maintenance") {
                if let Some(snapshot) = &mut app.services {
                    snapshot.availability = services::Availability::Maintenance;
                    snapshot.availability_note = tf("Official scheduled all-world maintenance window is active: {title}. Completion may change.",
                        &[("title", &"All Worlds Maintenance (Preview)")]);
                }
            }
            if std::env::args().any(|a| a == "--preview-servers-offline") {
                if let Some(snapshot) = &mut app.services {
                    snapshot.availability = services::Availability::Outage;
                    snapshot.availability_note = t("Official status page reports all 16 production worlds down.").into();
                }
            }
            if std::env::args().any(|a| a == "--preview-update-ready") {
                if let Some(report) = &mut app.patch { report.state = patch::PatchState::Required; }
            }
            if std::env::args().any(|a| a == "--preview-modal") {
                let mut account = app.accounts[0].account.clone();
                account.launch_with = Some(if std::env::args().any(|a| a == "--preview-vanilla") {
                    ui::LaunchWith::Vanilla
                } else { ui::LaunchWith::Windower });
                app.editor = Some(Editor {
                    key: Some(app.accounts[0].key.clone()),
                    account,
                    game_install: String::new(),
                    dat_overlays: Vec::new(),
                    startup_signin: false, auto_login: pol_less::AutoLogin::default(),
                });
            }
            if std::env::args().any(|a| a == "--preview-live-news") {
                app.news_loading = true;
                app.news_generation += 1;
                let generation = app.news_generation;
                let lang = i18n::current();
                return (
                    app,
                    Task::perform(services::refresh(), move |result| Message::ServicesLoaded(generation, lang, result)),
                );
            }
            return (app, Task::none());
        }
        let accounts = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || backend::load_or_import_accounts(&config))
                    .await
                    .unwrap_or_else(|_| Err(t("Could not load saved accounts.").into()))
            },
            Message::AccountsLoaded,
        );
        let checks = app.refresh();
        let installation = app.find_installation(true);
        let viewer = app.start_viewer_check(false);
        let launcher = if app.settings.launcher_auto_updates && !matches!(app.self_update, self_update::State::Error(_)) { Task::done(Message::SelfUpdate(self_update::Msg::Check)) } else { Task::none() };
        (app, Task::batch([accounts, checks, viewer, launcher, installation]))
    }

    fn subscription(&self) -> Subscription<Message> {
        // Every tick rebuilds and redraws the whole view, so poll fast only while
        // work is in flight and idle along at 1 Hz otherwise.
        let busy = self.preview
            || self.maintenance.is_some()
            || self.active_operation.is_some()
            || self.vault_saving
            || self.news_loading
            || self.patch_loading
            || self.installation_loading
            || self.viewer_loading
            || self.friends.pending.is_some()
            || self.accounts.iter().any(|a| a.busy || a.batch_pending);
        let tick = Duration::from_millis(if busy { 100 } else { 1000 });
        Subscription::batch([
            iced::time::every(tick).map(|_| Message::Tick),
            window::close_requests().map(|_| Message::CloseRequested),
            window::resize_events().map(|(_, size)| Message::WindowResized(size)),
            window::open_events().map(Message::WindowOpened),
            game_config::subscription(&self.game_config).map(Message::GameConfig),
            prereqs::subscription(&self.prereqs).map(Message::Prereqs),
            gamepad::subscription(&self.gamepad).map(Message::Gamepad),
            dgvoodoo::subscription(&self.dgvoodoo),
            iced::event::listen_with(user_input),
            if self.account_drag.source.is_some() { iced::event::listen_raw(account_order::input) } else { Subscription::none() },
            if self.controller_nav.repeating() { iced::time::every(Duration::from_millis(50)).map(|_| Message::ControllerTick) } else { Subscription::none() },
        ])
    }

    pub fn launchable_count(&self) -> usize {
        self.launch_keys(false).len()
    }
    pub fn selected_count(&self) -> usize {
        self.launch_keys(true).len()
    }
    fn launch_keys(&self, selected_only: bool) -> Vec<String> {
        if self.launch_blocked() {
            return Vec::new();
        }
        let mut seen = BTreeSet::new();
        self.accounts
            .iter()
            .filter(|r| r.account.mode == AccountMode::Retail && r.can_play() && (!selected_only || r.selected))
            .filter_map(|r| seen.insert(r.key.clone()).then(|| r.key.clone()))
            .collect()
    }
    fn has_sessions(&self) -> bool {
        self.accounts.iter().any(|r| r.busy || r.batch_pending || r.pid.is_some()
            || !self.preview && session_worker::find(&self.config, &r.account).is_some())
    }

    fn update_sessions(&self) -> Option<Vec<session_worker::BackgroundSession>> {
        let mut sessions = Vec::new();
        for row in &self.accounts {
            if row.batch_pending || row.queued_play.is_some() || row.is_playing()
                || row.busy && !row.preparing_session { return None; }
            let session = (!self.preview).then(|| session_worker::find(&self.config, &row.account)).flatten();
            if let Some(session) = session {
                if !session.can_prepare_update() { return None; }
                sessions.push(session);
            } else if row.pid.is_some() || row.busy { return None; }
        }
        Some(sessions)
    }

    pub(crate) fn sessions_block_update(&self) -> bool { self.update_sessions().is_none() }
    pub fn launch_blocked(&self) -> bool {
        self.installation_loading
            || self.reshade.busy
            || self.maintenance.is_some_and(MaintenanceKind::blocks_launch)
            || matches!(self.self_update, self_update::State::Applying)
    }

    pub fn service_availability(&self) -> services::Availability {
        self.services.as_ref().map_or(services::Availability::Unknown, |snapshot| {
            if self.preview { snapshot.availability } else { snapshot.recent_availability(chrono::Utc::now()) }
        })
    }

    pub fn main_action(&self) -> MainAction {
        if self.maintenance.is_some() || self.reshade.busy || matches!(self.self_update, self_update::State::Applying) { return MainAction::Working; }
        if self.viewer.as_ref().is_some_and(viewer::Status::requires_update)
            || self.patch.as_ref().is_some_and(|report| report.state == patch::PatchState::Required) {
            return MainAction::Update;
        }
        match self.service_availability() {
            services::Availability::Maintenance => MainAction::Maintenance,
            services::Availability::Outage => MainAction::Offline,
            _ => MainAction::Play,
        }
    }

    fn run_main_action(&mut self) -> Task<Message> {
        if self.reshade.busy {
            self.page = Page::Settings;
            self.settings_tab = settings_page::Tab::Graphics;
            return Task::none();
        }
        // Resolve again on click: a status refresh may have changed the action.
        let action = self.main_action();
        match action {
            MainAction::Play => self.start(self.launch_keys(true)),
            MainAction::Update | MainAction::Working => {
                let task = if action == MainAction::Update {
                    self.start_maintenance(MaintenanceKind::Update)
                } else { Task::none() };
                self.page = Page::Maintenance;
                self.show_settings = false;
                self.show_details = false;
                return task;
            }
            MainAction::Maintenance | MainAction::Offline => self.show_details = true,
        }
        Task::none()
    }

    /// Open the native dialog off the UI thread, starting from what the field holds.
    fn browse(&mut self, target: PickTarget) -> Task<Message> {
        let blocked = match target {
            PickTarget::GameFolder | PickTarget::InstallerFolder => self.maintenance.is_some(),
            _ => false,
        };
        if self.preview || blocked {
            return Task::none();
        }
        if target == PickTarget::AccountGameFolder || target == PickTarget::AccountDatFolder {
            let Some(editor) = &self.editor else { return Task::none(); };
            if self.vault_saving { return Task::none(); }
            if target == PickTarget::AccountDatFolder && editor.dat_overlays.len() >= 8 { return Task::none(); }
            let current = if target == PickTarget::AccountDatFolder {
                editor.dat_overlays.last().unwrap_or(&editor.game_install)
            } else { &editor.game_install };
            let start = dialog_start(current, false);
            let key = account_key(&editor.account);
            return Task::perform(pick_path(target, start), move |picked| {
                if target == PickTarget::AccountDatFolder { Message::PickedAccountOverlay(key.clone(), picked) }
                else { Message::PickedAccountFolder(key.clone(), picked) }
            });
        }
        let current = match target {
            PickTarget::GameFolder => &self.settings.install_path,
            PickTarget::InstallerFolder => &self.settings.installer_path,
            PickTarget::AshitaFolder => &self.settings.ashita_path,
            PickTarget::WindowerFolder => &self.settings.windower_path,
            PickTarget::LoaderFile => &self.settings.loader_path,
            PickTarget::AccountGameFolder | PickTarget::AccountDatFolder => unreachable!(),
        };
        let start = dialog_start(current, target == PickTarget::LoaderFile);
        Task::perform(pick_path(target, start), move |picked| {
            Message::Picked(target, picked)
        })
    }

    /// A dialog answer: put the chosen path in its field. The game and installer
    /// folders then go through the same validation and confirmation as before.
    fn picked(&mut self, target: PickTarget, picked: Option<PathBuf>) {
        let Some(picked) = picked else { return };
        let text = backend::plain_windows_path(&picked).to_string_lossy().into_owned();
        match target {
            PickTarget::GameFolder | PickTarget::InstallerFolder => {
                if self.maintenance.is_some() {
                    return;
                }
                let installer = target == PickTarget::InstallerFolder;
                if installer {
                    self.settings.installer_path = text;
                    self.settings.installer_selected = false;
                } else {
                    let _ = self.update(Message::InstallPathChanged(text));
                }
                self.select_path(installer);
            }
            PickTarget::AshitaFolder => {
                let _ = self.update(Message::LaunchTools(launch_tools::Msg::AshitaPath(text)));
            }
            PickTarget::WindowerFolder => {
                let _ = self.update(Message::LaunchTools(launch_tools::Msg::WindowerPath(text)));
            }
            PickTarget::LoaderFile => {
                let _ = self.update(Message::LaunchTools(launch_tools::Msg::LoaderPath(text)));
            }
            PickTarget::AccountGameFolder | PickTarget::AccountDatFolder => {}
        }
    }

    fn select_path(&mut self, installer: bool) {
        if self.preview || self.maintenance.is_some() || self.reshade.busy {
            return;
        }
        if installer { self.installer_error = None; }
        let raw = if installer {
            &self.settings.installer_path
        } else {
            &self.settings.install_path
        };
        let path = match selected_folder(raw, !installer) {
            Ok(path) => path,
            Err(error) => {
                if installer { self.installer_error = Some(error.clone()); }
                self.status = error;
                return;
            }
        };
        // Installer archives have their own validated names and staging files. Their
        // folder may contain (or be inside) the selected game installation.
        let mut settings = self.settings.clone();
        if installer {
            settings.installer_path = path.to_string_lossy().into_owned();
            settings.installer_selected = true;
        } else {
            settings.install_path = path.to_string_lossy().into_owned();
            settings.install_selected = true;
        }
        match save_settings(&self.config, &settings) {
            Ok(()) => {
                self.settings = settings;
                self.status = if installer {
                    tf("Selected installer download folder: {path}", &[("path", &path.display())])
                } else {
                    tf("Selected game installation folder: {path}", &[("path", &path.display())])
                };
                if !installer {
                    self.installation_generation = self.installation_generation.wrapping_add(1);
                    self.installation_loading = false;
                    self.patch = None;
                    self.maintenance_result =
                        t("Selected installation has not been checked. Use Check files to verify it.")
                            .into();
                    self.maintenance_tone = ResultTone::Unverified;
                }
            }
            Err(error) => {
                if installer { self.installer_error = Some(error.clone()); }
                self.status = error;
            }
        }
    }

    fn start_maintenance(&mut self, kind: MaintenanceKind) -> Task<Message> {
        if self.preview || self.maintenance.is_some() {
            return Task::none();
        }
        let game_operation = matches!(
            kind,
            MaintenanceKind::Check | MaintenanceKind::Update | MaintenanceKind::Repair
        );
        if game_operation && self.installation_loading { return Task::none(); }
        if !game_operation {
            // Pressing an installer step selects its current folder. The old
            // extra confirmation left all three buttons disabled on first use.
            self.settings.installer_selected = false;
            self.select_path(true);
            if !self.settings.installer_selected { return Task::none(); }
        } else if game_operation && !self.settings.install_selected {
            self.status =
                t("Select the target folder explicitly before starting this operation.").into();
            return Task::none();
        }
        let idle_sessions = if matches!(kind, MaintenanceKind::Update | MaintenanceKind::Repair) {
            self.update_sessions()
        } else { Some(Vec::new()) };
        if kind.blocks_launch() && (idle_sessions.is_none()
            || !matches!(kind, MaintenanceKind::Update | MaintenanceKind::Repair) && self.has_sessions()) {
            self.status = t("Close all games and sign out PlayOnline sessions before updating, repairing, or running official setup. Existing sessions were kept.").into();
            if !game_operation { self.installer_error = Some(self.status.clone()); }
            return Task::none();
        }
        if kind.blocks_launch() && !self.vault_ready {
            self.status = t("Wait for saved accounts and running sessions to be checked before changing game files.").into();
            if !game_operation { self.installer_error = Some(self.status.clone()); }
            return Task::none();
        }
        let install = PathBuf::from(&self.settings.install_path);
        let dir = PathBuf::from(&self.settings.installer_path);
        let op = operation::Operation::default();
        if matches!(kind, MaintenanceKind::Update | MaintenanceKind::Repair) { op.begin_update(); }
        let idle_sessions = idle_sessions.unwrap_or_default();
        if !idle_sessions.is_empty() {
            op.progress("SignOut", 0, idle_sessions.len() as u64,
                t("Signing out idle PlayOnline sessions before updating"));
        }
        self.active_operation = Some(op.clone());
        self.maintenance = Some(kind);
        self.maintenance_cancelled = false;
        self.maintenance_progress = op.snapshot();
        self.maintenance_result.clear();
        self.maintenance_tone = ResultTone::Done;
        self.status = kind.label().into();
        if game_operation { self.viewer_generation += 1; self.viewer_loading = true; }
        let viewer_generation = self.viewer_generation;
        let previous_viewer_version = self.viewer.as_ref().map(|v| v.version.remote_version.clone())
            .filter(|version| patch::valid_viewer_version(version));
        let sender = self.sender.clone();
        Task::perform(
            async move {
                if !idle_sessions.is_empty() {
                    if let Err(error) = session_worker::sign_out_for_update(idle_sessions, &op).await {
                        return (Err(error), None);
                    }
                }
                let result = match kind {
                MaintenanceKind::Check | MaintenanceKind::Update | MaintenanceKind::Repair => {
                    let mode = match kind {
                        MaintenanceKind::Check => updater::Mode::Check,
                        MaintenanceKind::Update => updater::Mode::Update,
                        _ => updater::Mode::Repair,
                    };
                    // The updater asked the official server for its version; the interface
                    // shows that answer instead of sending a request of its own.
                    let folder = install.clone();
                    let pol_folder = patch::registry_install_folder("1000");
                    let result = match pol_folder.clone() {
                        Some(pol) => updater::run_combined(pol, install, mode, op).await,
                        None => Err(updater::CombinedFailure::from(t("PlayOnline Viewer is not installed. Run official setup and select PlayOnline Viewer.").to_owned())),
                    };
                    // Refresh POL even after a partial failure: its verified
                    // transaction may have completed before the game stopped.
                    if let Some(pol) = pol_folder {
                        let remote = match &result { Ok(report) => Some(report.viewer.version.clone()),
                            Err(error) => error.viewer_version.clone().or(previous_viewer_version) };
                        let _ = tokio::task::spawn_blocking(move || {
                            let _ = sender.send(WorkerEvent::Viewer(viewer_generation, viewer::after_check(pol, remote.as_deref())));
                        }).await;
                    } else {
                        let _ = sender.send(WorkerEvent::Viewer(viewer_generation, viewer::unavailable(
                            t("PlayOnline Viewer is not installed. Run official setup and select PlayOnline Viewer.").into())));
                    }
                    return match result {
                        Ok(report) => {
                            let summary = report.summary();
                            let shown = tokio::task::spawn_blocking(move || patch::report_after_operation(&folder, &report.game, mode)).await.ok();
                            (Ok(summary), shown)
                        }
                        Err(error) => (Err(error.message), None),
                    };
                }
                MaintenanceKind::Download => installer::download(dir, op).await.map(|report| report.summary),
                MaintenanceKind::Extract | MaintenanceKind::Setup => {
                    tokio::task::spawn_blocking(move || {
                        op.check_cancelled()?;
                        // No stage id: the page names the step from `kind`.
                        op.progress("", 0, 0, dir.display().to_string());
                        if kind == MaintenanceKind::Extract {
                            installer::extract(&dir)?;
                            Ok(t("Extractor finished. Choose Run setup. If you extracted elsewhere, browse to that FFXIFullSetup_US folder first.").into())
                        } else {
                            installer::install(&dir)?;
                            Ok(t("Setup closed. Crystal will look for the installed FFXI folder. PlayOnline Viewer installs only when selected in the wizard. Check files before updating.").into())
                        }
                    }).await.unwrap_or_else(|_| Err(t("Installer worker could not complete.").into()))
                }
                };
                (result, None)
            },
            move |(result, shown)| Message::MaintenanceFinished(kind, result, shown),
        )
    }

    /// Starts reading the news feeds in the UI language showing now, unless a
    /// read for that language is already running. A read for the other language
    /// (the user just switched) is left to finish unheard: its updates carry its
    /// language and are dropped.
    fn start_news(&mut self) -> Task<Message> {
        let lang = i18n::current();
        if self.news_loading && self.news_lang == lang {
            return Task::none();
        }
        self.news_loading = true;
        self.news_lang = lang;
        self.news_generation = self.news_generation.wrapping_add(1);
        let generation = self.news_generation;
        self.refreshed = Instant::now();
        let sender = self.sender.clone();
        let cache = news_cache_path(&self.config, lang);
        Task::perform(
            services::refresh_with_progress(lang, Some(cache), move |update| {
                let _ = sender.send(WorkerEvent::Services(generation, lang, update));
            }),
            move |result| Message::ServicesLoaded(generation, lang, result),
        )
    }

    fn poll_service_status(&mut self) -> Task<Message> {
        let downtime = matches!(self.service_availability(), services::Availability::Maintenance | services::Availability::Outage);
        let interval = Duration::from_secs(if downtime { 60 } else { 300 });
        if self.preview || self.news_loading || self.refreshed.elapsed() < interval {
            return Task::none();
        }
        let lang = i18n::current();
        self.news_loading = true;
        self.news_lang = lang;
        self.news_generation = self.news_generation.wrapping_add(1);
        let generation = self.news_generation;
        self.refreshed = Instant::now();
        Task::perform(services::refresh_status(lang), move |result| Message::ServiceStatusLoaded(generation, lang, result))
    }

    fn refresh(&mut self) -> Task<Message> {
        if self.preview {
            return Task::none();
        }
        self.refreshed = Instant::now();
        Task::batch([self.start_news(), self.read_installed_version()])
    }

    fn find_installation(&mut self, automatic: bool) -> Task<Message> {
        if self.preview || self.maintenance.is_some() || self.installation_loading {
            return Task::none();
        }
        self.installation_generation = self.installation_generation.wrapping_add(1);
        let generation = self.installation_generation;
        self.installation_loading = true;
        let preferred = automatic.then(|| PathBuf::from(&self.settings.install_path));
        if !automatic { self.status = t("Finding FINAL FANTASY XI…").into(); }
        Task::perform(async move {
            tokio::task::spawn_blocking(move || patch::find_game_installation(preferred.as_deref()))
                .await.ok().flatten()
        }, move |folder| Message::InstallationFound(generation, automatic, folder))
    }

    fn read_installed_version(&mut self) -> Task<Message> {
        if self.preview { return Task::none(); }
        self.patch_loading = true;
        self.patch_generation += 1;
        let generation = self.patch_generation;
        let install = PathBuf::from(&self.settings.install_path);
        // Reads the installed record only. The server is asked when Play is pressed.
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || patch::local_report(&install))
                    .await
                    .unwrap_or_else(|e| patch::PatchReport {
                        state: patch::PatchState::Unknown,
                        local_version: t("Unavailable").into(),
                        remote_version: t("Not checked").into(),
                        detail: tf("Could not read local patch metadata: {error}", &[("error", &e)]),
                        source_url: None, files_verified: None,
                    })
            },
            move |r| Message::PatchLoaded(generation, r),
        )
    }

    fn cancel_account_drag(&mut self) {
        let drag = std::mem::take(&mut self.account_drag);
        if drag.source.is_some() {
            account_order::apply(&mut self.accounts, &drag.original, |row| row.key.as_str());
        }
    }

    fn finish_account_drag(&mut self) {
        let drag = std::mem::take(&mut self.account_drag);
        if drag.source.is_none() { return; }
        let order: Vec<_> = self.accounts.iter().map(|row| row.key.clone()).collect();
        if order == drag.original { return; }
        let mut settings = self.settings.clone();
        settings.account_order = order;
        if !self.preview {
            if let Err(error) = save_settings(&self.config, &settings) {
                account_order::apply(&mut self.accounts, &drag.original, |row| row.key.as_str());
                self.status = tf("Account order could not be saved: {error}", &[("error", &error)]);
                return;
            }
        }
        self.settings = settings;
        self.status = t("Account order saved.").into();
    }

    fn replace_accounts(&mut self, values: Vec<Account>) {
        self.cancel_account_drag();
        let mut old: HashMap<String, AccountRow> = std::mem::take(&mut self.accounts)
            .into_iter()
            .map(|r| (r.key.clone(), r))
            .collect();
        let mut seen = BTreeSet::new();
        for account in values {
            let key = account_key(&account);
            if !seen.insert(key.clone()) {
                continue;
            }
            let mut row = if let Some(mut row) = old.remove(&key) {
                row.account = account;
                row
            } else {
                AccountRow {
                    selected: self
                        .settings
                        .selected_accounts
                        .as_ref()
                        .map_or(true, |s| s.contains(&key)),
                    key,
                    status: if account.mode == AccountMode::Retail { t("Ready to launch") } else { t("Private-server client temporarily unavailable") }.into(),
                    account,
                    otp: String::new(),
                    busy: false,
                    preparing_session: false,
                    queued_play: None,
                    batch_pending: false,
                    batch_requested: false,
                    launch_generation: 0,
                    pid: None,
                    alive: None,
                    game_running: None,
                    log: None,
                    outcome: None,
                    problem: false,
                    timing: String::new(),
                    play_timing: None,
                    exit: None,
                }
            };
            if !row.batch_requested {
                row.batch_pending = session_worker::batch_pending(&self.config, &row.account);
            }
            let reconnect = if row.pid.is_none() && !row.busy {
                session_worker::find(&self.config, &row.account)
            } else {
                None
            };
            let key = row.key.clone();
            if let Some(session) = reconnect {
                row.busy = true;
                row.preparing_session = session.preparing_social();
                row.launch_generation = row.launch_generation.wrapping_add(1);
                let generation = row.launch_generation;
                row.status = t("Reconnecting to the background session…").into();
                let sender = self.sender.clone();
                let worker_key = key.clone();
                let spawned = std::thread::Builder::new().name("session-reconnect".into()).spawn(move || {
                    let result = session.reconnect(|message| {
                        let _ = sender.send(WorkerEvent::Progress(worker_key.clone(), message.to_owned()).for_launch(&worker_key, generation));
                    });
                    let _ = sender.send(WorkerEvent::Complete(worker_key.clone(), result).for_launch(&worker_key, generation));
                });
                if spawned.is_err() {
                    row.busy = false;
                    row.status = t("Could not reconnect to the background session.").into();
                }
            }
            self.accounts.push(row);
        }
        account_order::apply(&mut self.accounts, &self.settings.account_order, |row| row.key.as_str());
        self.reset_missing_friends_account();
    }

    fn save_accounts(&mut self, values: Vec<Account>) -> Task<Message> {
        if !self.vault_ready || self.vault_saving || self.preview {
            return Task::none();
        }
        self.vault_saving = true;
        self.status = t("Saving account…").into();
        let config = self.config.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    backend::save_accounts(&config, &values)?;
                    Ok(values)
                })
                .await
                .unwrap_or_else(|_| Err(t("Could not save accounts.").into()))
            },
            Message::AccountsSaved,
        )
    }

    fn start(&mut self, keys: Vec<String>) {
        self.start_mode(keys, false);
    }

    fn start_mode(&mut self, keys: Vec<String>, social_only: bool) {
        self.start_mode_at(keys, social_only, Instant::now());
    }

    fn start_viewer_check(&mut self, force: bool) -> Task<Message> {
        if self.preview || self.viewer_loading { return Task::none(); }
        self.viewer_loading = true;
        self.viewer_generation += 1;
        let generation = self.viewer_generation;
        Task::perform(async move {
            tokio::task::spawn_blocking(move || viewer::status(force)).await
                .unwrap_or_else(|e| viewer::unavailable(e.to_string()))
        }, move |report| Message::ViewerLoaded(generation, report))
    }

    fn set_viewer_status(&mut self, generation: u64, report: viewer::Status) {
        if generation == self.viewer_generation { self.viewer_loading = false; self.viewer = Some(report); }
    }

    fn coordinated_keys(&self, keys: &[String]) -> Vec<String> {
        let mut seen = BTreeSet::new();
        keys.iter().filter(|key| seen.insert((*key).clone())).filter(|key| {
            self.accounts.iter().any(|row| row.key == **key && row.can_play()
                && row.account.mode == AccountMode::Retail && self.settings.launch_plan(&row.account).is_some())
        }).cloned().collect()
    }

    fn batch_sessions(&self, keys: Vec<String>) -> Vec<(String, Option<session_worker::BackgroundSession>)> {
        keys.into_iter().map(|key| {
            let session = self.accounts.iter().find(|row| row.key == key).and_then(|row| {
                let config = self.settings.config_for(&self.config, &row.account);
                session_worker::find(&config, &row.account)
            });
            (key, session)
        }).collect()
    }

    fn start_together(&mut self, keys: Vec<(String, Option<session_worker::BackgroundSession>)>, clicked: Instant) {
        let mut jobs = Vec::new();
        let mut generations = HashMap::new();
        for (key, session) in keys {
            let Some(row) = self.accounts.iter_mut().find(|row| row.key == key
                && (row.can_play() || row.queued_signin_play() && row.queued_play == Some(clicked))) else { continue; };
            let Some(via) = self.settings.launch_plan(&row.account) else { continue; };
            let config = self.settings.config_for(&self.config, &row.account);
            let job = match session {
                Some(session) => session_worker::BatchAccount::existing(key.clone(), config, row.account.clone(),
                    std::mem::take(&mut row.otp), via, &session),
                None if row.busy || row.pid.is_some() => {
                    row.queued_play = None;
                    row.problem = true;
                    row.status = t("The background session ended before Play could start. Sign in again.").into();
                    continue;
                }
                None => session_worker::BatchAccount::new(key.clone(), config, row.account.clone(),
                    std::mem::take(&mut row.otp), via),
            };
            jobs.push(job);
            row.launch_generation = row.launch_generation.wrapping_add(1);
            generations.insert(key, row.launch_generation);
            row.batch_pending = true;
            row.batch_requested = true;
            row.busy = true;
            row.preparing_session = false;
            row.queued_play = None;
            row.problem = false;
            row.exit = None;
            row.timing.clear();
            row.play_timing = Some(launch_timing::Timeline::begin_at(clicked));
            row.status = t("Preparing accounts to start FFXI together…").into();
        }
        let count = jobs.len();
        if count == 0 { return; }
        // The batch owner is another headless Crystal process. The interface only
        // receives its progress; its lifetime does not control the game handoff.
        let sender = self.sender.clone();
        let worker_generations = generations.clone();
        let spawned = std::thread::Builder::new().name("xi-launch-together".into()).spawn(move || {
            let mut completed = BTreeSet::new();
            let result = session_worker::launch_together(jobs, clicked, |key, message| {
                if let Some(generation) = worker_generations.get(key) {
                    let _ = sender.send(WorkerEvent::Progress(key.to_owned(), message.to_owned()).for_launch(key, *generation));
                }
            }, |key, result| {
                completed.insert(key.to_owned());
                if let Some(generation) = worker_generations.get(key) {
                    let _ = sender.send(WorkerEvent::Complete(key.to_owned(), result).for_launch(key, *generation));
                }
            });
            if let Err(error) = result {
                for (key, generation) in worker_generations {
                    if !completed.contains(&key) {
                        let _ = sender.send(WorkerEvent::Complete(key.clone(), Err(error.clone())).for_launch(&key, generation));
                    }
                }
            }
        });
        if spawned.is_err() {
            for key in generations.keys() {
                if let Some(row) = self.accounts.iter_mut().find(|row| &row.key == key) {
                    row.batch_pending = false;
                    row.batch_requested = false;
                    row.busy = false;
                    row.play_timing = None;
                    row.problem = true;
                    row.status = t("Could not start the coordinated launch worker.").into();
                }
            }
            self.status = t("Could not start the coordinated launch worker.").into();
        } else {
            self.status = tf("Preparing {count} accounts in parallel; FFXI will start when every account is ready.", &[("count", &count)]);
        }
    }

    fn start_mode_at(&mut self, keys: Vec<String>, social_only: bool, clicked: Instant) {
        if self.launch_blocked() {
            self.status = t("Game launches are paused while files are being changed or official setup is running.").into();
            return;
        }
        if self.preview || !self.vault_ready || self.vault_saving {
            return;
        }
        if !social_only && matches!(self.service_availability(), services::Availability::Maintenance | services::Availability::Outage) {
            self.show_details = true;
            self.status = self.services.as_ref().map(|snapshot| snapshot.availability_note.clone()).unwrap_or_default();
            return;
        }
        if !social_only {
            let together = self.coordinated_keys(&keys);
            if together.len() > 1 {
                let sessions = self.batch_sessions(together.clone());
                let unpublished = sessions.iter().any(|(key, session)| session.is_none()
                    && self.accounts.iter().any(|row| row.key == *key && (row.busy || row.pid.is_some())));
                if !unpublished {
                    self.start_together(sessions, clicked);
                    return;
                }
            }
            if let [key] = together.as_slice() {
                if self.accounts.iter().any(|row| row.key == *key && row.busy && row.preparing_session) {
                    let sessions = self.batch_sessions(together);
                    if sessions[0].1.as_ref().is_some_and(session_worker::BackgroundSession::supports_batch_start) {
                        self.start_together(sessions, clicked);
                        return;
                    }
                }
            }
        }
        let count = keys.len();
        if count == 0 {
            self.status = t("Choose an account that is ready to launch.").into();
            return;
        }
        // Launch timing counts from the Play click, not from when each worker thread gets going.
        // Which tools this batch starts, for the status line.
        let mut tools: Vec<&'static str> = Vec::new();
        let mut all_native = true;
        // One worker per account: every sign-in starts now and runs in parallel; only the
        // Ashita/Windower start inside each worker waits its turn (pol_less::turns).
        for key in keys {
            let Some(row) = self
                .accounts
                .iter_mut()
                .find(|r| r.key == key && r.can_play())
            else {
                continue;
            };
            if row.busy {
                if !social_only {
                    row.queued_play = Some(clicked);
                    row.status = t("FFXI will start when sign-in finishes.").into();
                }
                continue;
            }
            let account = row.account.clone();
            if account.mode != AccountMode::Retail {
                row.status = t("Private-server client temporarily unavailable; saved account kept.").into();
                continue;
            }
            // Signing in again must never turn an existing Friends session into a game launch.
            if social_only && row.pid.is_some() { continue; }
            let config = self.settings.config_for(&self.config, &account);
            if let Some(plan) = self.settings.launch_plan(&account) {
                if let Some(session) = session_worker::find(&config, &account) {
                    if let Err(error) = session.ensure_launch_tool(plan.tool) {
                        row.status = error;
                        row.problem = true;
                        continue;
                    }
                }
            }
            row.launch_generation = row.launch_generation.wrapping_add(1);
            let generation = row.launch_generation;
            if row.pid.is_some() {
                // The stock FFXI version check runs immediately before the game starts, here
                // as well as in a Play that signs in: off the interface thread, then the request.
                let install = match pol_less::install_directory(&config) {
                    Ok(install) => install,
                    Err(error) => { row.status = error; row.problem = true; continue; }
                };
                row.busy = true;
                row.problem = false;
                row.timing.clear();
                row.play_timing = Some(launch_timing::Timeline::begin_at(clicked));
                row.status = t("Checking the game version…").into();
                let sender = self.sender.clone();
                let worker_key = key.clone();
                // the signed-in session's loader starts the game; put the wrapper
                // beside it (or take Crystal's copy away) before the start.
                let dgvoodoo = self.settings.dgvoodoo;
                let loader_setting = self.settings.loader_path.clone();
                let spawned = std::thread::Builder::new().name("xi-start-game".into()).spawn(move || {
                    if let Some(loader) = pol_less::tools::loader_path(&loader_setting) {
                        let _ = dgvoodoo::sync_for_launch(&loader, dgvoodoo);
                    }
                    let check = patch::check_before_launch(&install);
                    let report = match &check {
                        patch::LaunchCheck::Start(r) | patch::LaunchCheck::Update(r) | patch::LaunchCheck::Unverified(r) => r.clone(),
                    };
                    let result = patch::gate(check).and_then(|note| {
                        session_worker::request_native_control(&config, &account, session_worker::NativeControl::StartGame).map(|()| note)
                    });
                    let _ = sender.send(WorkerEvent::StartGame(worker_key.clone(), report, result).for_launch(&worker_key, generation));
                });
                if spawned.is_err() {
                    row.busy = false;
                    row.problem = true;
                    row.play_timing = None;
                    row.status = t("Could not start the game version check.").into();
                }
                continue;
            }
            // the account's own launcher, boot config and profile,
            // over the Launch tools defaults. All retail routes use the same session worker.
            let mut via = self.settings.launch_plan(&account);
            if let Some(plan) = &mut via { plan.social_only = social_only; }
            let tool = via.as_ref().map_or(t("the native client"), |plan| plan.tool.name());
            all_native &= via.is_none();
            if !tools.contains(&tool) {
                tools.push(tool);
            }
            let viewer_generation = self.viewer_generation;
            let otp = std::mem::take(&mut row.otp);
            row.busy = true;
            row.preparing_session = social_only;
            row.problem = false;
            row.timing.clear();
            row.play_timing = None;
            row.status = t("Starting sign-in…").into();
            let sender = self.sender.clone();
            let worker_key = key.clone();
            // installed here first when it isn't yet; the session worker then puts
            // it next to the loader it runs. A failed install never stops the launch.
            let dgvoodoo = self.settings.dgvoodoo && via.is_some();
            let result = std::thread::Builder::new()
                .name("xi-account-login".into())
                .spawn(move || {
                    let viewer = viewer::status(false);
                    let _ = sender.send(WorkerEvent::Viewer(viewer_generation, viewer.clone()));
                    if let Err(error) = viewer::gate(&viewer) {
                        let _ = sender.send(WorkerEvent::Complete(worker_key.clone(), Err(error)).for_launch(&worker_key, generation));
                        return;
                    }
                    if dgvoodoo {
                        let installing = (sender.clone(), worker_key.clone());
                        let installed = dgvoodoo::ensure_installed_blocking(move |line| {
                            let _ = installing.0.send(WorkerEvent::Progress(installing.1.clone(), line).for_launch(&installing.1, generation));
                        });
                        if let Err(error) = installed {
                            let _ = sender.send(WorkerEvent::Notice(tf(
                                "dgVoodoo could not be installed, so this launch uses the game's own DirectX 8. {error}",
                                &[("error", &error)],
                            )));
                        }
                    }
                    let progress_sender = sender.clone();
                    let progress_key = worker_key.clone();
                    let result = session_worker::launch(&config, account, otp, via, clicked, move |s| {
                        let _ = progress_sender
                            .send(WorkerEvent::Progress(progress_key.clone(), s.to_owned()).for_launch(&progress_key, generation));
                    });
                    let _ = sender.send(WorkerEvent::Complete(worker_key.clone(), result).for_launch(&worker_key, generation));
                });
            if result.is_err() {
                row.busy = false;
                row.preparing_session = false;
                row.status = t("Could not start the sign-in worker.").into();
            }
        }
        self.status = if social_only {
            t("Signing in to PlayOnline…").into()
        } else { match (all_native, count == 1) {
            (true, true) => t("Starting 1 account in parallel. Continue in each game window after sign-in.").into(),
            (true, false) => tf("Starting {count} accounts in parallel. Continue in each game window after sign-in.", &[("count", &count)]),
            (false, single) => {
                let tools = join_and(&tools);
                if single {
                    tf("Starting 1 account through {tools}: sign-ins run in parallel, then each game starts as soon as its tool is free.", &[("tools", &tools)])
                } else {
                    tf("Starting {count} accounts through {tools}: sign-ins run in parallel, then each game starts as soon as its tool is free.", &[("count", &count), ("tools", &tools)])
                }
            }
        } };
    }

    /// A capable worker can check the version during the rest of sign-in. Older
    /// loaders keep the signed-in path, with the same version and native idle gates.
    fn start_queued_plays(&mut self) {
        if self.preview || !self.vault_ready || self.vault_saving || self.launch_blocked()
            || matches!(self.service_availability(), services::Availability::Maintenance | services::Availability::Outage) { return; }
        let preparing: Vec<_> = self.accounts.iter().filter_map(|row| {
            if !row.queued_signin_play() || row.account.mode != AccountMode::Retail
                || self.settings.launch_plan(&row.account).is_none() { return None; }
            let config = self.settings.config_for(&self.config, &row.account);
            let session = session_worker::find(&config, &row.account)?;
            if !session.supports_batch_start() { return None; }
            Some((row.key.clone(), row.queued_play?, session))
        }).collect();
        for (key, clicked, session) in preparing { self.start_together(vec![(key, Some(session))], clicked); }
        let queued: Vec<_> = self.accounts.iter_mut().filter_map(|row| {
            if row.busy || row.pid.is_none() || row.is_playing() { return None; }
            row.queued_play.take().map(|clicked| (row.key.clone(), clicked))
        }).collect();
        for (key, clicked) in queued { self.start_mode_at(vec![key], false, clicked); }
    }

    /// Consume the startup action once. Account refreshes and client exits never retry it.
    fn take_startup_signins(&mut self) -> Vec<String> {
        if !self.startup_signin_pending || !self.vault_ready || self.installation_loading { return Vec::new(); }
        self.startup_signin_pending = false;
        if self.preview || self.vault_saving || self.launch_blocked() { return Vec::new(); }
        self.accounts.iter().filter(|row|
            row.account.mode == AccountMode::Retail && row.pid.is_none() && !row.busy && row.can_play()
            && self.settings.account_startup_signin.contains(&row.key)
        ).map(|row| row.key.clone()).collect()
    }

    /// What `account` launches with (its own choice, else the default).
    pub fn launcher_for(&self, account: &Account) -> ui::LaunchWith {
        self.settings.launch_with_for(account)
    }

    /// The play bar's launcher line: one launcher, or "Mixed launchers" when the selected
    /// accounts differ.
    pub fn selected_launcher(&self, current: Option<&AccountRow>) -> String {
        let selected: Vec<ui::LaunchWith> = self
            .accounts
            .iter()
            .filter(|r| r.selected)
            .map(|r| self.launcher_for(&r.account))
            .collect();
        match selected.split_first() {
            Some((first, rest)) if selected.len() > 1 => {
                if rest.iter().all(|l| l == first) {
                    tf("Selected accounts · {launcher}", &[("launcher", first)])
                } else {
                    t("Selected accounts · Mixed launchers").into()
                }
            }
            _ => current
                .map(|row| self.launcher_for(&row.account))
                .unwrap_or(self.settings.launch_with)
                .to_string(),
        }
    }

    /// Set the account's own launcher (Home's Launch with) and save the vault.
    fn set_account_launcher(&mut self, key: &str, choice: ui::LaunchWith) -> Task<Message> {
        let Some(row) = self.accounts.iter_mut().find(|r| r.key == key) else {
            return Task::none();
        };
        if choice == ui::LaunchWith::Native || row.account.mode == AccountMode::Private || row.account.launch_with == Some(choice) {
            return Task::none();
        }
        if self.preview {
            row.account.launch_with = Some(choice);
            return Task::none();
        }
        if !self.vault_ready || self.vault_saving {
            self.status = t("Wait for the accounts to finish saving, then choose again.").into();
            return Task::none();
        }
        let values = self
            .accounts
            .iter()
            .map(|r| {
                let mut account = r.account.clone();
                if r.key == key {
                    account.launch_with = Some(choice);
                }
                account
            })
            .collect();
        self.save_accounts(values)
    }

    /// Open an account's latest log (or its logs folder) in the default app.
    fn open_log(&mut self, key: &str, folder: bool) {
        if self.preview {
            return;
        }
        let Some(row) = self.accounts.iter().find(|r| r.key == key) else {
            return;
        };
        let logs = self.config.logs_directory(&row.account);
        let target = if folder { Some(logs.clone()) } else { row.log.clone() };
        let opened = match target {
            Some(target) => open_log_path(&logs, &target),
            None => Err(t("There is no log for this account yet.").into()),
        };
        if let Err(error) = opened {
            self.status = error;
        }
    }

    fn search_ah(&mut self) -> Task<Message> {
        let generation = self.ah_lookup.begin(&self.ah_query, self.ah_server);
        if self.preview {
            self.ah_lookup.finish(generation, Ok(ffxiah::Results::default()));
            return Task::none();
        }
        let query = self.ah_lookup.query.clone();
        let world = self.ah_lookup.world;
        Task::perform(ffxiah::search(query, world), move |result| Message::AhResults(generation, result))
    }

    fn reset_friends(&mut self, account: Option<String>) {
        self.friends_generation = self.friends_generation.wrapping_add(1);
        self.friends = friends::State { account, ..Default::default() };
    }

    fn reset_missing_friends_account(&mut self) {
        if self.friends.account.as_ref().is_some_and(|key| {
            !self.accounts.iter().any(|row| &row.key == key && row.account.mode == AccountMode::Retail)
        }) {
            self.reset_friends(None);
        }
    }

    fn load_friends(&mut self) {
        self.reset_missing_friends_account();
        if self.preview || self.friends.loading { return; }
        if self.friends.account.is_none() {
            self.friends.account = self.accounts.iter().find(|a| a.pid.is_some() && a.account.mode == AccountMode::Retail)
                .or_else(|| self.accounts.iter().find(|a| a.account.mode == AccountMode::Retail)).map(|a| a.key.clone());
        }
        let Some(row) = self.accounts.iter().find(|a| self.friends.account.as_ref() == Some(&a.key)) else { return };
        let (key, account) = (row.key.clone(), row.account.clone());
        let config = self.settings.config_for(&self.config, &account);
        let sender = self.sender.clone();
        self.friends.loading = true;
        self.friends_generation = self.friends_generation.wrapping_add(1);
        let generation = self.friends_generation;
        self.friends_checked = Instant::now();
        if std::thread::Builder::new().name("friends-view".into()).spawn(move || {
            let (connected, snapshot) = session_worker::friends_snapshot(&config, &account);
            let _ = sender.send(WorkerEvent::Friends(generation, key, connected, snapshot));
        }).is_err() {
            self.friends.loading = false;
            self.friends.error = t("Could not read the friends session.").into();
        }
    }

    /// Ask for the next message the open conversation needs, if any.
    fn drive_sync(&mut self) -> Task<Message> {
        if self.preview {
            return Task::none();
        }
        if let Some(request) = self.friends.next_sync() {
            let task = self.update(Message::FriendsSocial(request));
            if self.friends.pending.is_none() {
                // Refused at once: stop until the user acts, not retry each poll.
                self.friends.mail.stalled = true;
            }
            return task;
        }
        // Once Sent lists the message just sent, it shows from there.
        if self.friends.pending.is_none() && self.friends.outgoing.as_ref().is_some_and(|o| o.sent) {
            self.friends.outgoing = None;
        }
        Task::none()
    }

    fn worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Viewer(generation, report) => self.set_viewer_status(generation, report),
            WorkerEvent::Launch(key, generation, event) => {
                if self.accounts.iter().any(|row| row.key == key && row.launch_generation == generation) {
                    self.worker_event(*event);
                }
            }
            WorkerEvent::Friends(generation, key, connected, snapshot) => {
                if generation == self.friends_generation && self.friends.account.as_ref() == Some(&key) {
                    self.friends.loading = false;
                    self.friends.connected = connected;
                    let mut finished = None;
                    if let Some((id, _)) = &self.friends.pending {
                        let completed = snapshot.as_ref().is_some_and(|s| s.social.id.as_ref() == Some(id) && !s.social.busy);
                        if completed || !connected {
                            finished = completed.then(|| snapshot.clone()).flatten();
                            if !completed {
                                self.friends.mail.reading = None;
                                self.friends.outgoing = None;
                            }
                            self.friends.pending = None;
                        }
                    }
                    self.friends.snapshot = snapshot;
                    if let Some(done) = finished {
                        self.friends.completed(&done);
                    }
                    let _ = self.drive_sync();
                }
            }
            // A language switch can supersede a read even after switching back.
            WorkerEvent::Services(generation, lang, update) => {
                if generation == self.news_generation && lang == i18n::current() {
                    services::apply(&mut self.services, update)
                }
            }
            WorkerEvent::StartGame(key, report, result) => {
                self.patch_generation += 1;
                self.patch_loading = false;
                self.patch = Some(report);
                if let Some(row) = self.accounts.iter_mut().find(|r| r.key == key) {
                    match result {
                        Ok(note) => {
                            row.status = t("Starting FFXI from the signed-in PlayOnline session…").into();
                            self.status = tf("{account}: {message}", &[("account", &row.account.label), ("message", &note)]);
                        }
                        Err(error) => {
                            row.busy = false;
                            row.problem = true;
                            row.play_timing = None;
                            row.status = error.clone();
                            self.status = tf("{account}: {message}", &[("account", &row.account.label), ("message", &error)]);
                        }
                    }
                }
            }
            WorkerEvent::Notice(message) => self.status = message,
            WorkerEvent::WindowerProfileProgress(message) => {
                if self.windower_editor_open { self.status = message; }
            }
            WorkerEvent::Progress(key, status) => {
                if let Some(row) = self.accounts.iter_mut().find(|r| r.key == key) {
                    row.status = if row.queued_play.is_some() {
                        format!("{status} · {}", t("FFXI will start when sign-in finishes."))
                    } else { status };
                }
            }
            WorkerEvent::Complete(key, result) => {
                if let Some(row) = self.accounts.iter_mut().find(|r| r.key == key) {
                    row.busy = false;
                    row.preparing_session = false;
                    row.batch_requested = false;
                    row.batch_pending = session_worker::batch_pending(&self.config, &row.account);
                    match result {
                        Ok(result) => {
                            // The signed-in session stays up; the stock check refused the game start.
                            let refused = patch::parse_update_required(&result.detail);
                            row.pid = Some(result.pid);
                            row.alive = Some(result.alive);
                            row.game_running = Some(result.game_running);
                            row.log = Some(result.log);
                            row.outcome = Some(result.outcome);
                            row.problem = refused.is_some();
                            row.status = if let Some(report) = refused {
                                self.patch_generation += 1;
                                self.patch_loading = false;
                                self.patch = Some(report);
                                result.detail.clone()
                            } else if !row.is_playing() {
                                t("Signed in to PlayOnline · FFXI is closed").into()
                            } else if result.timing.is_empty() {
                                result.detail.clone()
                            } else {
                                t("Client started · continue in game").into()
                            };
                            row.timing = result.timing;
                            self.status = tf("{account}: {message}", &[("account", &row.account.label), ("message", &result.detail)]);
                        }
                        Err(error) => {
                            row.queued_play = None;
                            // The row shows the reason; the log path stays behind View log.
                            let (message, log) = launch_log::split_log(&error);
                            if let Some(report) = patch::parse_update_required(message) {
                                self.patch_generation += 1;
                                self.patch_loading = false;
                                self.patch = Some(report);
                            }
                            row.status = message.to_owned();
                            row.problem = true;
                            if log.is_some() {
                                row.log = log;
                            }
                            self.status = tf("{account}: {message}", &[("account", &row.account.label), ("message", &error)]);
                        }
                    }
                }
                self.start_queued_plays();
            }
        }
    }

    pub(crate) fn can_restart_launcher(&self) -> bool {
        self.maintenance.is_none() && self.active_operation.is_none() && !self.prereqs.busy()
            && !self.dgvoodoo.busy() && !self.reshade.busy && self.friends.pending.is_none()
            && self.vault_ready && !self.startup_signin_pending && !self.vault_saving
            && !self.accounts.iter().any(|a| a.busy || a.batch_pending)
    }

    fn poll_controller(&mut self, capturing: bool) {
        let allowed = self.settings.controller_navigation && window_chrome::is_foreground()
            && !capturing && !self.gamepad.capturing() && !matches!(self.self_update, self_update::State::Applying);
        if self.controller_nav.poll(self.gamepad.controller_pad(), allowed, Instant::now())
            && activity_due(&mut LAST_ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()), Instant::now()) {
            let _ = self.update(Message::UserInput);
        }
    }

    fn controller_modal(&self) -> bool {
        self.confirm_close || self.pending_delete.is_some() || self.editor.is_some()
            || self.account_import.open || self.show_settings || self.show_details || self.ah_lookup.open
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        // The helper is preparing to replace this process. Do not start new
        // writes or work between its readiness check and our exit.
        if matches!(self.self_update, self_update::State::Applying)
            && !matches!(&message, Message::SelfUpdate(self_update::Msg::Applied(_)) | Message::CloseRequested)
        {
            return Task::none();
        }
        match message {
            Message::SelfUpdate(self_update::Msg::AutoCheck(on)) => {
                self.settings.launcher_auto_updates = on;
                if !self.preview { if let Err(error) = save_settings(&self.config, &self.settings) { self.status = error; } }
                if on && !self.preview { return Task::done(Message::SelfUpdate(self_update::Msg::Check)); }
            }
            Message::SelfUpdate(self_update::Msg::Applied(Ok(()))) => return iced::exit(),
            Message::SelfUpdate(msg) => {
                if !self.preview {
                    let can_restart = self.can_restart_launcher();
                    return self_update::update(&mut self.self_update, msg, &self.config, can_restart).map(Message::SelfUpdate);
                }
            }
            Message::ControllerTick => self.poll_controller(false),
            Message::ControllerNavigation(on) => {
                self.settings.controller_navigation = on;
                self.controller_nav.clear();
                if !self.preview { if let Err(error) = save_settings(&self.config, &self.settings) { self.status = error; } }
            }
            Message::ControllerPlay => {
                if !self.controller_modal() { return self.update(Message::MainAction); }
            }
            Message::ControllerPage(step) => {
                if !self.controller_modal() {
                    let pages = [Page::Home, Page::Accounts, Page::Friends, Page::Maintenance, Page::Settings];
                    let index = pages.iter().position(|p| *p == self.page).unwrap_or(0);
                    self.controller_nav.clear();
                    return self.update(Message::Navigate(pages[(index as i32 + step as i32).rem_euclid(5) as usize]));
                }
            }
            Message::ControllerBack => {
                if self.account_drag.source.is_some() { self.cancel_account_drag(); return Task::none(); }
                if self.gamepad.capturing() { return self.update(Message::Gamepad(gamepad::Msg::CancelCapture)); }
                if self.confirm_close { return self.update(Message::CancelClose); }
                if self.pending_delete.is_some() { return self.update(Message::CancelDeleteAccount); }
                if self.editor.is_some() { return self.update(Message::CloseEditor); }
                if self.account_import.open { return self.import_update(account_import_ui::Msg::Close); }
                if self.show_settings { return self.update(Message::ToggleSettings); }
                if self.show_details { return self.update(Message::ToggleDetails); }
                if self.ah_lookup.open { return self.update(Message::CloseAh); }
                if self.page == Page::Friends && self.friends.remove.is_some() { return self.update(Message::FriendsDraftClose); }
                if self.page == Page::Friends && self.friends.pane.is_some() { return self.update(Message::FriendsSelect(None)); }
                if self.page != Page::Home { return self.update(Message::Navigate(Page::Home)); }
            }
            Message::FriendsAccount(key) => {
                self.reset_friends(Some(key));
                self.load_friends();
            }
            Message::FriendsQuery(query) => self.friends.query = query,
            Message::FriendsSelect(pane) => {
                self.friends.select(pane.clone());
                // The Mailbox lists Inbox page 1 when it has nothing current.
                if pane == Some(friends::Pane::Mailbox) && self.friends.pending.is_none() && self.friends.snapshot.as_ref().is_some_and(|s| s.social.ready && !s.social.busy && s.game_running == Some(false) && (s.social.inbox_unread.is_none() || s.social.inbox_changed)) {
                    return self.update(Message::FriendsSocial(social::Request::Inbox { folder:0, page:0 }));
                }
                return self.drive_sync();
            }
            Message::FriendsSection(index) => { if let Some(folded) = self.friends.collapsed.get_mut(index) { *folded = !*folded; } }
            Message::FriendsStatusOptions => self.friends.status_options = !self.friends.status_options,
            Message::FriendsAddSubject(open) => { if let Some(draft) = &mut self.friends.composer { draft.show_subject = open; } }
            Message::FriendsRemove(friend) => self.friends.remove = Some(friend),
            Message::FriendsRemoveConfirm => {
                if let Some(friend) = self.friends.remove.take() {
                    if let Some(uid) = friend.uid { return self.update(Message::FriendsSocial(social::Request::Remove { alternate: friend.alternate, slot: friend.slot, uid })); }
                }
            }
            Message::FriendsDraftClose => self.friends.remove = None,
            Message::FriendsSubject(subject) => { if let Some(draft) = &mut self.friends.composer { draft.subject = subject.chars().take(512).collect(); } }
            Message::FriendsMessageEdit(action) => { if let Some(draft) = &mut self.friends.composer { draft.text.perform(action); } }
            Message::FriendsSend => {
                let ready = self.friends.connected && self.friends.pending.is_none()
                    && self.friends.snapshot.as_ref().is_some_and(|s| s.game_running == Some(false) && s.social.ready && !s.social.busy);
                if let (true, Some(request)) = (ready, self.friends.send_request()) {
                    if let social::Request::Send { uid, subject, message, .. } = &request {
                        self.friends.outgoing = Some(friends::Outgoing { uid: *uid, subject: subject.clone(), message: message.clone(), sent: false });
                    }
                    let task = self.update(Message::FriendsSocial(request));
                    if self.friends.pending.is_none() { self.friends.outgoing = None; }
                    return task;
                }
            }
            Message::FriendsSocial(request) => {
                if !self.preview && self.friends.pending.is_none() {
                    if let Some(row) = self.accounts.iter().find(|a| self.friends.account.as_ref() == Some(&a.key)) {
                        let config = self.settings.config_for(&self.config, &row.account);
                        if matches!(request, social::Request::Check {}) { self.friends.mail.stalled = false; }
                        let reading = if let social::Request::Read { header } = &request { Some(social::message_key(header)) } else { None };
                        match session_worker::request_social(&config, &row.account, request) {
                            Ok(id) => { self.friends.pending = Some((id, Instant::now())); self.friends.mail.reading = reading; self.friends.error.clear(); }
                            Err(error) => self.friends.error = error,
                        }
                        self.load_friends();
                    }
                }
            }
            Message::FriendsConnect => {
                if let Some(key) = self.friends.account.clone() { self.start_mode(vec![key], true); }
            }
            Message::FriendsPlay => {
                if let Some(key) = self.friends.account.clone() { self.start(vec![key]); }
            }
            Message::FriendsSignOut => {
                if !self.preview {
                    if let Some(row) = self.accounts.iter().find(|a| self.friends.account.as_ref() == Some(&a.key)) {
                        let config = self.settings.config_for(&self.config, &row.account);
                        self.friends.error = session_worker::request_native_control(&config, &row.account, session_worker::NativeControl::SignOut).err().unwrap_or_default();
                    }
                }
            }
            Message::FriendsStatus(action) => {
                if !self.preview && matches!(action, session_worker::NativeControl::Visible | session_worker::NativeControl::Away | session_worker::NativeControl::Invisible) {
                    if let Some(row) = self.accounts.iter().find(|a| self.friends.account.as_ref() == Some(&a.key)) {
                        let config = self.settings.config_for(&self.config, &row.account);
                        self.friends.error = session_worker::request_native_status(&config, &row.account, action, !self.friends.skip_groups).err().unwrap_or_default();
                    }
                }
            }
            Message::FriendsStatusGroups(groups) => self.friends.skip_groups = !groups,
            Message::FriendsAutoAway(minutes) => {
                if !self.preview && pol_less::AUTO_AWAY_CHOICES.contains(&minutes) {
                    if let Some(row) = self.accounts.iter().find(|a| self.friends.account.as_ref() == Some(&a.key)) {
                        self.settings.account_auto_away.insert(account_key(&row.account), minutes);
                        if let Err(error) = save_settings(&self.config, &self.settings) { self.friends.error = error; }
                    }
                }
            }
            Message::UserInput => {
                // Only signed-in sessions whose game is closed; the worker
                // forwards at most one native_activity per note.
                if !self.preview {
                    let idle: Vec<_> = self.accounts.iter()
                        .filter(|r| r.account.mode == AccountMode::Retail && r.pid.is_some() && !r.busy && !r.is_playing())
                        .map(|r| (self.settings.config_for(&self.config, &r.account), r.account.clone()))
                        .collect();
                    if !idle.is_empty() {
                        let _ = std::thread::Builder::new().name("native-activity".into()).spawn(move || {
                            for (config, account) in idle { let _ = session_worker::note_native_activity(&config, &account); }
                        });
                    }
                }
            }
            Message::SettingsTab(tab) => {
                self.settings_tab = tab;
                if tab == settings_page::Tab::Graphics { return reshade::update(self, reshade::Msg::Refresh); }
            }
            // open a Settings tab from elsewhere (the components banner).
            Message::OpenSettingsTab(tab) => {
                self.settings_tab = tab;
                return self.update(Message::Navigate(Page::Settings));
            }
            Message::GameConfig(m) => {
                return game_config::update(&mut self.game_config, m).map(Message::GameConfig)
            }
            Message::Prereqs(m) => return prereqs::update(&mut self.prereqs, m).map(Message::Prereqs),
            Message::Dgvoodoo(m) => return dgvoodoo::update(self, m),
            Message::Reshade(m) => return reshade::update(self, m),
            Message::Gamepad(m) => {
                let captured = self.gamepad.capturing();
                let task = gamepad::update(&mut self.gamepad, m).map(Message::Gamepad);
                self.poll_controller(captured);
                return task;
            },
            Message::LaunchTools(m) => {
                if launch_tools::apply(&mut self.settings, m) && !self.preview {
                    if let Err(error) = save_settings(&self.config, &self.settings) {
                        self.status = error;
                    }
                }
                self.launch_tools.refresh(&self.settings);
            }
            Message::EditWindowerProfiles => {
                if self.preview || self.windower_editor_open { return Task::none(); }
                self.windower_editor_open = true;
                self.status = t("Edit profiles in Windower. Saved changes apply on the next fresh game launch.").into();
                let preferred = self.settings.windower_path.clone();
                let sender = self.sender.clone();
                return Task::perform(async move {
                    tokio::task::spawn_blocking(move || {
                        let folder = pol_less::tools::detect(pol_less::Tool::Windower, &preferred)
                            .ok_or_else(|| t("Windower was not found. Set its folder in Settings → System → Launch tools.").to_owned())?;
                        launch_tools::edit_profiles(&folder, |line| {
                            let _ = sender.send(WorkerEvent::WindowerProfileProgress(line.into()));
                        })
                    }).await.unwrap_or_else(|_| Err(t("Could not open Windower's profile editor.").into()))
                }, Message::WindowerProfilesClosed);
            }
            Message::WindowerProfilesClosed(result) => {
                self.windower_editor_open = false;
                if let Err(error) = result { self.status = error; }
                self.launch_tools.refresh(&self.settings);
            }
            Message::Tick => {
                self.launch_tools.poll();
                if self.startup_signin_pending && self.vault_ready && !self.installation_loading {
                    let keys = self.take_startup_signins();
                    if !keys.is_empty() { self.start_mode(keys, true); }
                }
                if self.friends.pending.as_ref().is_some_and(|(_, started)| started.elapsed() >= Duration::from_secs(45)) {
                    self.friends.pending = None;
                    self.friends.mail.stalled = true;
                    self.friends.mail.reading = None;
                    self.friends.outgoing = None;
                    self.friends.error = t("PlayOnline has not reported completion. Check messages or the friend list before repeating this request.").into();
                }
                // A request in flight is followed closely, so a conversation
                // fills message by message; otherwise every two seconds.
                let every = if self.friends.pending.is_some() { Duration::from_millis(250) } else { Duration::from_secs(2) };
                if self.page == Page::Friends && self.friends_checked.elapsed() >= every {
                    self.load_friends();
                }
                if let Some(op) = &self.active_operation {
                    self.maintenance_progress = op.snapshot();
                }
                while let Ok(event) = self.receiver.try_recv() {
                    self.worker_event(event);
                }
                for row in &mut self.accounts {
                    if !row.batch_requested {
                        row.batch_pending = session_worker::batch_pending(&self.config, &row.account);
                    }
                    if row.pid.is_some() && row.alive.as_ref().is_some_and(|a| a.load(Ordering::Acquire)) {
                        if row.is_playing() && !row.batch_pending {
                            if let Some(mut timing) = row.play_timing.take() {
                                timing.stage("game start observed by launcher");
                                timing.mark(launch_timing::Milestone::GameStarted);
                                row.timing = timing.row_summary();
                                row.status = t("Client started · continue in game").into();
                                if let Some(log) = &row.log { timing.append_to(&launch_timing::sibling_path(log)); }
                            }
                        }
                        if row.busy && row.is_playing() && !row.batch_pending { row.busy = false; }
                        // A start the version check refused keeps its message on the row.
                        if !row.busy && !row.batch_pending && row.queued_play.is_none() && !row.is_playing() && !row.problem { row.status = t("Signed in to PlayOnline · FFXI is closed").into(); }
                    }
                    if row
                        .alive
                        .as_ref()
                        .is_some_and(|a| !a.load(Ordering::Acquire))
                    {
                        row.pid = None;
                        row.alive = None;
                        row.game_running = None;
                        row.busy = row.batch_requested;
                        row.preparing_session = false;
                        row.queued_play = None;
                        row.timing.clear();
                        row.play_timing = None;
                        let report = row
                            .outcome
                            .take()
                            .and_then(|slot| slot.lock().ok().and_then(|mut r| r.take()));
                        match report {
                            Some(report) => row.set_exit(report),
                            None => {
                                row.problem = false;
                                row.status = t("Client closed").into();
                            }
                        }
                    }
                }
                self.start_queued_plays();
                if self.preview
                    && !self.preview_capture_sent
                    && !self.news_loading
                    && self.refreshed.elapsed() > Duration::from_secs(2)
                {
                    self.preview_capture_sent = true;
                    return window::oldest().then(|id| {
                        id.map_or(Task::none(), |id| {
                            window::screenshot(id).map(Message::PreviewReady)
                        })
                    });
                }
                return self.poll_service_status();
            }
            Message::AccountDragStart(key) => {
                if self.page == Page::Accounts && self.vault_ready && !self.vault_saving
                    && self.editor.is_none() && !self.account_import.open && !self.confirm_close && self.pending_delete.is_none()
                    && self.accounts.iter().any(|row| row.key == key) {
                    self.cancel_account_drag();
                    self.account_drag.original = self.accounts.iter().map(|row| row.key.clone()).collect();
                    self.account_drag.source = Some(key);
                }
            }
            Message::AccountDragOver(target) => {
                if let Some(source) = self.account_drag.source.as_ref() {
                    if let (Some(from), Some(to)) = (self.accounts.iter().position(|row| &row.key == source),
                        self.accounts.iter().position(|row| row.key == target)) {
                        if from != to {
                            let row = self.accounts.remove(from);
                            self.accounts.insert(to, row);
                        }
                    }
                }
            }
            Message::AccountDragFinish => self.finish_account_drag(),
            Message::AccountDragCancel => self.cancel_account_drag(),
            Message::ToggleAccount(key, value) => {
                if let Some(row) = self.accounts.iter_mut().find(|r| r.key == key) {
                    row.selected = value;
                }
                self.settings.selected_accounts = Some(
                    self.accounts
                        .iter()
                        .filter(|r| r.selected)
                        .map(|r| r.key.clone())
                        .collect(),
                );
                if !self.preview {
                    if let Err(error) = save_settings(&self.config, &self.settings) {
                        self.status = error;
                    }
                }
            }
            Message::WindowResized(size) => {
                self.viewport = size;
                // Maximise/restore arrives as a resize; keep the chrome's
                // restore glyph and resize edges in step with it.
                return window::oldest().then(|id| {
                    id.map_or(Task::none(), |id| {
                        window::is_maximized(id).map(Message::WindowMaximized)
                    })
                });
            }
            Message::WindowMaximized(maximized) => self.maximized = maximized,
            Message::WindowOpened(id) => {
                return window::run(id, |w| window_chrome::native_handle(w))
                    .map(Message::NativeWindow)
            }
            Message::NativeWindow(hwnd) => {
                window_chrome::attach(hwnd, self.settings.light_theme)
            }
            Message::MinimizeWindow => {
                return window::oldest()
                    .then(|id| id.map_or(Task::none(), |id| window::minimize(id, true)))
            }
            Message::MaximizeWindow => {
                return window::oldest().then(|id| id.map_or(Task::none(), window::toggle_maximize))
            }
            Message::TitlePressed => {
                self.title_held = !window_chrome::title_press_is_double();
                if !self.title_held {
                    return window::oldest()
                        .then(|id| id.map_or(Task::none(), window::toggle_maximize));
                }
            }
            Message::TitleMoved => {
                if std::mem::take(&mut self.title_held) {
                    return window::oldest().then(|id| id.map_or(Task::none(), window::drag));
                }
            }
            Message::TitleReleased => self.title_held = false,
            Message::ResizeWindow(direction) => {
                return window::oldest().then(move |id| {
                    id.map_or(Task::none(), |id| window::drag_resize(id, direction))
                })
            }
            Message::Navigate(page) => {
                self.cancel_account_drag();
                self.page = page;
                self.show_settings = false;
                self.show_details = false;
                if page == Page::Friends { self.load_friends(); }
                if page == Page::Settings && self.settings_tab == settings_page::Tab::Graphics {
                    return reshade::update(self, reshade::Msg::Refresh);
                }
                if page == Page::Maintenance && !self.settings.install_selected {
                    return self.find_installation(true);
                }
            }
            Message::ToggleDetails => self.show_details = !self.show_details,
            Message::Launch(key) => self.start(vec![key]),
            Message::LaunchSelected => self.start(self.launch_keys(true)),
            Message::MainAction => return self.run_main_action(),
            Message::LaunchAll => self.start(self.launch_keys(false)),
            Message::OtpChanged(key, value) => {
                if let Some(row) = self.accounts.iter_mut().find(|r| r.key == key && !r.busy) {
                    row.otp.zeroize();
                    row.otp = value;
                }
            }
            Message::NewAccount => {
                self.cancel_account_drag();
                if self.vault_ready && !self.vault_saving {
                    self.status = t("Enter account details, then save.").into();
                    self.editor = Some(Editor {
                        key: None,
                        account: blank_account(),
                        game_install: String::new(),
                        dat_overlays: Vec::new(),
                        startup_signin: false,
                        auto_login: pol_less::AutoLogin::default(),
                    });
                }
            }
            Message::AccountImport(msg) => return self.import_update(msg),
            Message::EditAccount(key) => {
                self.cancel_account_drag();
                if self.vault_saving { return Task::none(); }
                if let Some(row) = self
                    .accounts
                    .iter()
                    .find(|r| r.key == key)
                {
                    self.editor = Some(Editor {
                        game_install: self.settings.account_installs.get(&key).cloned().unwrap_or_default(),
                        dat_overlays: self.settings.account_overlays.get(&key).cloned().unwrap_or_default(),
                        startup_signin: self.settings.account_startup_signin.contains(&key),
                        auto_login: self.settings.account_auto_login.get(&key).copied().unwrap_or_default().normalized(),
                        key: Some(key),
                        account: row.account.clone(),
                    });
                    self.status = t("Edit account details.").into();
                }
            }
            Message::EditorChanged(field, value) => {
                if matches!(field, Field::PolId | Field::Server) && self.editor.as_ref().and_then(|editor| editor.key.as_ref())
                    .is_some_and(|key| self.accounts.iter().any(|row| &row.key == key && (row.busy || row.pid.is_some()))) {
                    return Task::none();
                }
                if !self.vault_saving {
                    if let Some(editor) = &mut self.editor {
                        let destination = match field {
                            Field::Label => &mut editor.account.label,
                            Field::Server => &mut editor.account.server,
                            Field::PolId => &mut editor.account.pol_id,
                            Field::PolPassword => &mut editor.account.pol_password,
                            Field::SeId => &mut editor.account.se_id,
                            Field::SePassword => &mut editor.account.se_password,
                        };
                        destination.zeroize();
                        *destination = value;
                    }
                }
            }
            Message::EditorRegion(region) => {
                if !self.vault_saving {
                    if let Some(editor) = &mut self.editor {
                        editor.account.region = region;
                    }
                }
            }
            Message::EditorAutoLogin(enabled) => {
                if !self.vault_saving {
                    if let Some(editor) = &mut self.editor { editor.auto_login.enabled = enabled; }
                }
            }
            Message::EditorCharacterSlot(slot) => {
                if !self.vault_saving && (1..=16).contains(&slot) {
                    if let Some(editor) = &mut self.editor { editor.auto_login.character_slot = slot; }
                }
            }
            Message::SaveAccount => {
                if let Some(editor) = &self.editor {
                    if self.settings.launch_with_for(&editor.account) == ui::LaunchWith::Native {
                        if editor.dat_overlays.len() > 8 { self.status = t("Choose at most eight DAT overlay folders.").into(); return Task::none(); }
                        for path in &editor.dat_overlays {
                            if let Err(error) = selected_folder(path, true) { self.status = error; return Task::none(); }
                        }
                    }
                    if !editor.game_install.trim().is_empty() {
                        let valid = selected_folder(&editor.game_install, true).and_then(|path| {
                            if path.join("FFXiMain.dll").is_file() && path.join("FFXi.dll").is_file() {
                                Ok(path)
                            } else {
                                Err(t("Choose the game folder containing FFXiMain.dll and FFXi.dll.").into())
                            }
                        });
                        if let Err(error) = valid { self.status = error; return Task::none(); }
                    }
                    let key = account_key(&editor.account);
                    if editor.key.as_ref().is_some_and(|previous| *previous != key
                        && self.accounts.iter().any(|row| &row.key == previous && (row.busy || row.pid.is_some()))) {
                        self.status = t("Keep the account ID unchanged while its session is running.").into();
                        return Task::none();
                    }
                    if self
                        .accounts
                        .iter()
                        .any(|r| r.key == key && Some(&r.key) != editor.key.as_ref())
                    {
                        self.status =
                            t("This login is already saved. Edit its existing account instead.")
                                .into();
                        return Task::none();
                    }
                    let mut replaced = false;
                    let mut values: Vec<Account> = self.accounts.iter().map(|row| {
                        if Some(&row.key) == editor.key.as_ref() {
                            replaced = true;
                            editor.account.clone()
                        } else { row.account.clone() }
                    }).collect();
                    if !replaced { values.push(editor.account.clone()); }
                    return self.save_accounts(values);
                }
            }
            Message::RequestDeleteAccount(key) => {
                if self.vault_ready && !self.vault_saving && self.accounts.iter().any(|row| row.key == key && !row.busy && row.pid.is_none()) {
                    self.pending_delete = Some(key);
                }
            }
            Message::CancelDeleteAccount => self.pending_delete = None,
            Message::DeleteAccount(key) => {
                if self.pending_delete.as_deref() != Some(key.as_str()) {
                    return Task::none();
                }
                self.pending_delete = None;
                if self
                    .accounts
                    .iter()
                    .any(|r| r.key == key && (r.busy || r.pid.is_some()))
                {
                    return Task::none();
                }
                let values = self
                    .accounts
                    .iter()
                    .filter(|r| r.key != key)
                    .map(|r| r.account.clone())
                    .collect();
                return self.save_accounts(values);
            }
            Message::CloseEditor => {
                if !self.vault_saving {
                    self.editor = None;
                    if self.account_import.batch.is_some() { self.account_import.open = true; }
                }
            }
            Message::ToggleSettings => self.show_settings = !self.show_settings,
            Message::SetLightTheme(light) => {
                self.settings.light_theme = light;
                window_chrome::set_theme(light);
                if !self.preview {
                    if let Err(error) = save_settings(&self.config, &self.settings) {
                        self.status = error;
                    }
                }
            }
            // Display only and at once: the next view draws pseudonyms.
            Message::SetPrivacy(on) => {
                self.settings.privacy = on;
                privacy::set(on);
                if !self.preview {
                    if let Err(error) = save_settings(&self.config, &self.settings) {
                        self.status = error;
                    }
                }
            }
            // Applies at once: the next view reads the new language.
            Message::SetLanguage(lang) => {
                let before = i18n::current();
                self.settings.language = Some(lang);
                i18n::set(lang);
                self.retranslate(before);
                if !self.preview {
                    if let Err(error) = save_settings(&self.config, &self.settings) {
                        self.status = error;
                    }
                }
                if lang != before {
                    // The official news is per language: drop the old language's
                    // headlines at once, show the new language's saved ones, and
                    // fetch fresh ones.
                    self.news_error.clear();
                    if !self.preview {
                        self.services = services::cached(&news_cache_path(&self.config, lang), lang);
                        return self.start_news();
                    }
                }
            }
            Message::ChooseProfile(key) => {
                for account in &mut self.accounts {
                    account.selected = account.key == key;
                }
                self.settings.selected_accounts = Some(
                    self.accounts
                        .iter()
                        .filter(|r| r.selected)
                        .map(|r| r.key.clone())
                        .collect(),
                );
                if !self.preview {
                    if let Err(error) = save_settings(&self.config, &self.settings) {
                        self.status = error;
                    }
                }
            }
            // Home's Launch with sets the shown account's own launcher
            // (the one Home's profile card shows: the first selected, else the first).
            Message::SetLaunchWith(choice) => {
                let shown = self
                    .accounts
                    .iter()
                    .find(|r| r.selected)
                    .or_else(|| self.accounts.first())
                    .map(|r| r.key.clone());
                if let Some(key) = shown {
                    return self.set_account_launcher(&key, choice);
                }
            }
            Message::EditorLaunchWith(choice) => {
                if choice != Some(ui::LaunchWith::Native) && !self.vault_saving {
                    if let Some(editor) = &mut self.editor {
                        editor.account.launch_with = choice;
                    }
                }
            }
            Message::EditorStartupSignin(enabled) => {
                if !self.vault_saving {
                    if let Some(editor) = &mut self.editor { editor.startup_signin = enabled; }
                }
            }
            Message::EditorBootIni(choice) => {
                if !self.vault_saving {
                    if let Some(editor) = &mut self.editor {
                        editor.account.ashita_boot_ini = choice;
                    }
                }
            }
            Message::EditorProfile(choice) => {
                if !self.vault_saving {
                    if let Some(editor) = &mut self.editor {
                        editor.account.windower_profile = choice;
                    }
                }
            }
            Message::WikiQueryChanged(value) => self.wiki_query = value,
            Message::AhQueryChanged(value) => self.ah_query = value,
            Message::AhServerChanged(value) => {
                self.ah_server = value;
                if self.ah_lookup.open { return self.search_ah(); }
            }
            Message::AhResults(generation, result) => self.ah_lookup.finish(generation, result),
            Message::CloseAh => self.ah_lookup.open = false,
            Message::NewsFilter(value) => self.news_filter = value,
            Message::SearchAh => return self.search_ah(),
            Message::SearchWiki => {
                let url = ui::wiki_search_url(&self.wiki_query);
                if !self.preview {
                    if let Err(error) = open_url(url.as_str()) {
                        self.status = error;
                    }
                }
            }
            Message::InstallPathChanged(value) => {
                if self.maintenance.is_none() {
                    self.installation_generation = self.installation_generation.wrapping_add(1);
                    self.installation_loading = false;
                    self.settings.install_path = value;
                    self.settings.install_selected = false;
                    self.patch = None;
                    self.patch_generation += 1;
                    self.patch_loading = false;
                    self.maintenance_result =
                        t("Installation path changed. Select this folder, then Check files.").into();
                    self.maintenance_tone = ResultTone::Unverified;
                }
            }
            Message::SelectInstallPath => self.select_path(false),
            Message::FindInstallation => return self.find_installation(false),
            Message::InstallationFound(generation, automatic, folder) => {
                if generation != self.installation_generation { return Task::none(); }
                self.installation_loading = false;
                if self.preview || self.maintenance.is_some() { return Task::none(); }
                let Some(folder) = folder else {
                    if !automatic { self.status = t("No installed FFXI folder was found. Run official setup or use Browse.").into(); }
                    return Task::none();
                };
                let path = folder.to_string_lossy().into_owned();
                if self.settings.install_selected && self.settings.install_path == path {
                    if !automatic { self.status = tf("Found FINAL FANTASY XI: {path}", &[("path", &folder.display())]); }
                    return Task::none();
                }
                let _ = self.update(Message::InstallPathChanged(path));
                // Recovery defaults must not replace an unreadable settings file
                // merely because startup found an installation.
                if automatic && self.startup_warning.is_some() {
                    self.settings.install_selected = true;
                } else {
                    self.select_path(false);
                }
                if self.settings.install_selected {
                    if !automatic { self.status = tf("Found FINAL FANTASY XI: {path}", &[("path", &folder.display())]); }
                    return self.read_installed_version();
                }
            }
            Message::InstallerPathChanged(value) => {
                if self.maintenance.is_none() {
                    self.installer_error = None;
                    self.settings.installer_path = value;
                    self.settings.installer_selected = false;
                }
            }
            Message::SelectInstallerPath => self.select_path(true),
            Message::Browse(target) => return self.browse(target),
            Message::Picked(target, picked) => self.picked(target, picked),
            Message::AccountInstallChanged(value) => {
                if !self.vault_saving {
                    if let Some(editor) = &mut self.editor { editor.game_install = value; }
                }
            }
            Message::PickedAccountFolder(key, picked) => {
                if !self.vault_saving {
                    if let Some(editor) = &mut self.editor {
                        if account_key(&editor.account) == key {
                            if let Some(path) = picked { editor.game_install = backend::plain_windows_path(&path).to_string_lossy().into_owned(); }
                        }
                    }
                }
            }
            Message::PickedAccountOverlay(key, picked) => {
                if !self.vault_saving {
                    if let Some(editor) = &mut self.editor {
                        if account_key(&editor.account) == key && editor.dat_overlays.len() < 8 {
                            if let Some(path) = picked {
                                let path = backend::plain_windows_path(&path).to_string_lossy().into_owned();
                                if !editor.dat_overlays.iter().any(|p| p.eq_ignore_ascii_case(&path)) { editor.dat_overlays.push(path); }
                            }
                        }
                    }
                }
            }
            Message::StartMaintenance(kind) => return self.start_maintenance(kind),
            Message::CancelMaintenance => {
                if self.maintenance.is_some_and(MaintenanceKind::cancellable) {
                    if let Some(op) = &self.active_operation {
                        op.cancel();
                        self.maintenance_cancelled = true;
                        self.status = t("Cancellation requested; waiting for the current file operation to finish safely.").into();
                    }
                }
            }
            Message::MaintenanceFinished(kind, result, shown) => {
                if self.maintenance == Some(kind) {
                    let setup_finished = kind == MaintenanceKind::Setup && result.is_ok();
                    if matches!(kind, MaintenanceKind::Check | MaintenanceKind::Update | MaintenanceKind::Repair) { self.viewer_loading = false; }
                    if matches!(kind, MaintenanceKind::Download | MaintenanceKind::Extract | MaintenanceKind::Setup) {
                        self.installer_error = result.as_ref().err().cloned();
                    }
                    if let Some(op) = self.active_operation.take() {
                        self.maintenance_progress = op.snapshot();
                    }
                    self.maintenance = None;
                    self.maintenance_tone = if result.is_err() { ResultTone::Stopped }
                        else if shown.as_ref().is_some_and(|report| report.files_verified == Some(false) || report.files_verified.is_none() && report.state != patch::PatchState::Current) { ResultTone::Unverified }
                        else { ResultTone::Done };
                    self.maintenance_result =
                        result.unwrap_or_else(|error| tf("Operation stopped: {error}", &[("error", &error)]));
                    self.status = self.maintenance_result.clone();
                    if let Some(report) = shown {
                        self.patch_generation += 1;
                        self.patch_loading = false;
                        self.patch = Some(report);
                    } else if matches!(kind, MaintenanceKind::Update | MaintenanceKind::Repair) {
                        return self.refresh();
                    }
                    if setup_finished {
                        return Task::batch([self.find_installation(false), self.start_viewer_check(true)]);
                    }
                }
            }
            Message::SaveSettings => {
                if self.preview {
                    return Task::none();
                }
                match save_settings(&self.config, &self.settings) {
                    Ok(()) => {
                        self.show_settings = false;
                        self.status = t("Settings saved. Checking the installed version…").into();
                        return self.refresh();
                    }
                    Err(error) => self.status = error,
                }
            }
            Message::Refresh => {
                if !self.news_loading && !self.patch_loading {
                    return self.refresh();
                }
            }
            Message::AccountsLoaded(result) => {
                match result {
                    Ok(values) => {
                        self.replace_accounts(values);
                        self.vault_ready = true;
                        self.status = t("Ready. Select accounts or use Launch all. Games continue when you close Crystal.").into();
                        let keys = self.take_startup_signins();
                        if !keys.is_empty() { self.start_mode(keys, true); }
                    }
                    Err(error) => {
                        self.vault_ready = false;
                        self.status = tf("Saved accounts could not be loaded: {error} The existing vault was kept.", &[("error", &error)]);
                    }
                }
                if let Some(warning) = self.startup_warning.take() {
                    self.status = tf("{warning} {status}", &[("warning", &warning), ("status", &self.status)]);
                }
            }
            Message::AccountsSaved(result) => {
                self.vault_saving = false;
                match result {
                    Ok(values) => {
                        let mut settings = self.settings.clone();
                        let saved: BTreeSet<String> = values.iter().map(account_key).collect();
                        // A login edit changes the account key. Keep its non-editor
                        // preferences with the account before removing obsolete keys.
                        if let Some(editor) = &self.editor {
                            let key = account_key(&editor.account);
                            if let Some(previous) = editor.key.as_ref().filter(|old| **old != key && saved.contains(&key)) {
                                if let Some(minutes) = settings.account_auto_away.remove(previous) {
                                    settings.account_auto_away.insert(key.clone(), minutes);
                                }
                                if let Some(choice) = settings.account_auto_login.remove(previous) {
                                    settings.account_auto_login.insert(key.clone(), choice);
                                }
                                if let Some(selected) = &mut settings.selected_accounts {
                                    if selected.remove(previous) { selected.insert(key); }
                                }
                                for ordered in &mut settings.account_order {
                                    if ordered == previous { *ordered = account_key(&editor.account); }
                                }
                            }
                        }
                        settings.account_installs.retain(|key, _| saved.contains(key));
                        settings.account_overlays.retain(|key, _| saved.contains(key));
                        settings.account_auto_away.retain(|key, _| saved.contains(key));
                        settings.account_auto_login.retain(|key, _| saved.contains(key));
                        settings.account_startup_signin.retain(|key| saved.contains(key));
                        settings.account_order.retain(|key| saved.contains(key));
                        if let Some(selected) = &mut settings.selected_accounts { selected.retain(|key| saved.contains(key)); }
                        if let Some(editor) = &self.editor {
                            let key = account_key(&editor.account);
                            if saved.contains(&key) {
                                let path = editor.game_install.trim();
                                if path.is_empty() { settings.account_installs.remove(&key); }
                                else { settings.account_installs.insert(key.clone(), plain_text_path(path)); }
                                if editor.dat_overlays.is_empty() { settings.account_overlays.remove(&key); }
                                else { settings.account_overlays.insert(key.clone(), editor.dat_overlays.iter().map(|p| plain_text_path(p)).collect()); }
                                if editor.auto_login != pol_less::AutoLogin::default() {
                                    settings.account_auto_login.insert(key.clone(), editor.auto_login.normalized());
                                } else { settings.account_auto_login.remove(&key); }
                                if editor.startup_signin && editor.account.mode == AccountMode::Retail { settings.account_startup_signin.insert(key); }
                                else { settings.account_startup_signin.remove(&key); }
                            }
                        }
                        if (self.editor.is_some() || settings.account_installs != self.settings.account_installs || settings.account_overlays != self.settings.account_overlays
                            || settings.account_auto_away != self.settings.account_auto_away || settings.account_auto_login != self.settings.account_auto_login || settings.selected_accounts != self.settings.selected_accounts
                            || settings.account_order != self.settings.account_order
                            || settings.account_startup_signin != self.settings.account_startup_signin) && !self.preview {
                            if let Err(error) = save_settings(&self.config, &settings) {
                                if let Some(editor) = &mut self.editor {
                                    let key = account_key(&editor.account);
                                    if saved.contains(&key) {
                                        // The vault already saved this identity. A retry
                                        // must edit it instead of reporting a duplicate.
                                        editor.key = Some(key);
                                    } else {
                                        self.editor = None;
                                    }
                                }
                                // Keep the pending preferences for a retry or the next
                                // settings save; account storage is already committed.
                                self.settings = settings;
                                self.replace_accounts(values);
                                self.status = tf("Accounts saved, but launcher settings were not saved: {error}", &[("error", &error)]);
                                return Task::none();
                            }
                        }
                        self.settings = settings;
                        self.replace_accounts(values);
                        self.editor = None;
                        if self.account_import.batch.is_some() { self.account_import.open = true; }
                        self.status = t("Accounts saved.").into();
                    }
                    Err(error) => self.status = error,
                }
            }
            Message::ServicesLoaded(generation, lang, _) | Message::ServiceStatusLoaded(generation, lang, _)
                if generation != self.news_generation || lang != i18n::current() => {
                // The newest request owns `news_loading` and the displayed data.
            }
            Message::ServicesLoaded(_, _, result) => {
                self.news_loading = false;
                // let the live feed paint before a preview capture; a
                // capture taken on the next tick replays the previous frame
                // (stale images, text whose paragraphs were already dropped).
                if self.preview {
                    self.refreshed = Instant::now();
                }
                match result {
                    Ok(snapshot) => {
                        services::apply(&mut self.services, services::Update::Headlines { items: snapshot.news.clone(), note: snapshot.news_note.clone() });
                        services::apply(&mut self.services, services::Update::Status(snapshot));
                        self.news_error.clear();
                    }
                    Err(error) => self.news_error = error,
                }
            }
            Message::ViewerLoaded(generation, report) => self.set_viewer_status(generation, report),
            Message::ServiceStatusLoaded(_, _, result) => {
                self.news_loading = false;
                match result {
                    Ok(snapshot) => services::apply(&mut self.services, services::Update::Status(snapshot)),
                    Err(error) => {
                        if let Some(snapshot) = &mut self.services {
                            snapshot.availability = services::Availability::Unknown;
                            snapshot.availability_note = tf("{note} Official world status unavailable: {error}.",
                                &[("note", &t("Official world status is unavailable. This does not establish a game outage. Check official notices.")), ("error", &error)]);
                        }
                    }
                }
            }
            Message::PatchLoaded(generation, mut result) => {
                if generation == self.patch_generation {
                    self.patch_loading = false;
                    // A version-record read contains no new server or file-check evidence.
                    // Keep the last check while that record is unchanged and readable.
                    let keep_checked = result.state == patch::PatchState::Unknown && result.remote_version == t("Not checked") && self.patch.as_ref().is_some_and(|previous| {
                        previous.local_version == result.local_version &&
                            (previous.state != patch::PatchState::Unknown || previous.files_verified.is_some())
                    });
                    if result.remote_version == t("Not checked") {
                        if let Some(previous) = self.patch.as_ref().filter(|p| p.local_version == result.local_version) {
                            result.remote_version.clone_from(&previous.remote_version);
                            if result.state == patch::PatchState::Required && result.local_version == t("Not initialized") {
                                result.files_verified = previous.files_verified;
                            }
                        }
                    }
                    if !keep_checked { self.patch = Some(result); }
                }
            }
            Message::OpenUrl(url) => {
                if !self.preview {
                    if let Err(error) = open_url(&url) {
                        self.status = error;
                    }
                }
            }
            Message::OpenLog(key) => self.open_log(&key, false),
            Message::OpenLogsFolder(key) => self.open_log(&key, true),
            Message::CloseRequested => {
                if matches!(self.self_update, self_update::State::Applying) {
                    self.status = t("Waiting for the launcher update helper…").into();
                    return Task::none();
                }
                if self.reshade.busy {
                    self.page = Page::Settings;
                    self.settings_tab = settings_page::Tab::Graphics;
                    self.status = t("Finish ReShade setup before closing Crystal.").into();
                    return Task::none();
                }
                if self.prereqs.busy() {
                    self.page = Page::Settings;
                    self.settings_tab = settings_page::Tab::System;
                    self.status = t("Wait for Windows component setup to finish, or cancel its download, before closing.").into();
                    return Task::none();
                }
                if self.maintenance.is_some() {
                    self.page = Page::Maintenance;
                    self.show_settings = false;
                    self.show_details = false;
                    self.status = t("Wait for the operation to finish, or request Cancel, before closing the launcher.").into();
                    return Task::none();
                }
                if self.vault_saving {
                    self.confirm_close = true;
                } else {
                    return iced::exit();
                }
            }
            Message::ConfirmClose => {
                if self.maintenance.is_none() && !self.prereqs.busy() && !self.reshade.busy {
                    return iced::exit();
                }
                self.confirm_close = false;
            }
            Message::CancelClose => self.confirm_close = false,
            Message::PreviewReady(screenshot) => {
                // light/empty/close variants get their own files.
                let light = self.settings.light_theme;
                let filename = if std::env::args().any(|a| a == "--preview-delete") {
                    "preview-delete.png"
                } else if std::env::args().any(|a| a == "--preview-server-maintenance") {
                    "preview-server-maintenance.png"
                } else if std::env::args().any(|a| a == "--preview-servers-offline") {
                    "preview-servers-offline.png"
                } else if std::env::args().any(|a| a == "--preview-update-ready") {
                    "preview-update-ready.png"
                } else if std::env::args().any(|a| a == "--preview-news-events") {
                    "preview-news-events.png"
                } else if std::env::args().any(|a| a == "--preview-news-maintenance") {
                    "preview-news-maintenance.png"
                } else if std::env::args().any(|a| a == "--preview-news-empty") {
                    "preview-news-empty.png"
                } else if std::env::args().any(|a| a == "--preview-inbox") {
                    if light { "preview-inbox-light.png" } else { "preview-inbox.png" }
                } else if std::env::args().any(|a| a == "--preview-compose") {
                    if light { "preview-compose-light.png" } else { "preview-compose.png" }
                } else if std::env::args().any(|a| a == "--preview-friends-ingame") {
                    if light { "preview-friends-ingame-light.png" } else { "preview-friends-ingame.png" }
                } else if std::env::args().any(|a| a == "--preview-friends") {
                    if light { "preview-friends-light.png" } else { "preview-friends.png" }
                } else if std::env::args().any(|a| a == "--preview-close") {
                    if light { "preview-close-light.png" } else { "preview-close.png" }
                } else if std::env::args().any(|a| a == "--preview-modal") {
                    if light { "preview-account-light.png" } else { "preview-account.png" }
                } else if std::env::args().any(|a| a == "--preview-empty") {
                    if light { "preview-accounts-empty-light.png" } else { "preview-accounts-empty.png" }
                } else if std::env::args().any(|a| a == "--preview-accounts") {
                    if light { "preview-accounts-light.png" } else { "preview-accounts.png" }
                } else if std::env::args().any(|a| a == "--preview-update") {
                    if light { "preview-update-light.png" } else { "preview-update.png" }
                } else if std::env::args().any(|a| a == "--preview-maintenance") {
                    // keep theme/busy variants apart.
                    match (self.settings.light_theme, self.maintenance.is_some()) {
                        (false, false) => "preview-maintenance.png",
                        (true, false) => "preview-maintenance-light.png",
                        (false, true) => "preview-maintenance-busy.png",
                        (true, true) => "preview-maintenance-busy-light.png",
                    }
                } else if std::env::args().any(|a| a == "--preview-playing") {
                    if light { "preview-playing-light.png" } else { "preview-playing.png" }
                } else if std::env::args().any(|a| a == "--preview-starting") {
                    if light { "preview-starting-light.png" } else { "preview-starting.png" }
                } else if std::env::args().any(|a| a == "--preview-details") {
                    "preview-details.png"
                } else if std::env::args().any(|a| a == "--preview-live-news") {
                    "preview-live-news.png"
                } else if std::env::args().any(|a| a == "--preview-vanilla") {
                    "preview-vanilla.png"
                } else if std::env::args().any(|a| a == "--preview-small") {
                    "preview-small.png"
                } else if std::env::args().any(|a| a == "--preview-wide") {
                    "preview-wide.png"
                } else {
                    if self.settings.light_theme {
                        "preview-portal-light.png"
                    } else {
                        "preview-portal-dark.png"
                    }
                };
                // XI_PREVIEW_DIR lets parallel worktrees keep their previews apart.
                let dir = std::env::var_os("XI_PREVIEW_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.config.experiment.join("iced-launcher"));
                let mut path = if self.page == Page::Settings {
                    dir.join(format!(
                        "preview-settings-{}{}.png",
                        self.settings_tab.flag(),
                        if light { "-light" } else { "" }
                    ))
                } else {
                    dir.join(filename)
                };
                // --preview-private captures get their own -private files.
                if privacy::on() {
                    let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                    path.set_file_name(format!("{stem}-private.png"));
                }
                // --preview-ja captures get their own -ja files.
                if i18n::current() == i18n::Lang::Ja {
                    let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                    path.set_file_name(format!("{stem}-ja.png"));
                }
                self.status = match save_screenshot(&path, screenshot) {
                    Ok(()) => format!("Preview saved: {}", path.display()),
                    Err(e) => e,
                };
                return iced::exit();
            }
        }
        Task::none()
    }

    /// After a language change, the stored lines that are fixed sentences
    /// follow it (texts with values or from other processes keep theirs until
    /// they next change).
    fn retranslate(&mut self, from: i18n::Lang) {
        let swap = |text: &mut String, keys: &[&'static str]| {
            if let Some(shown) = retranslated(text, from, keys) {
                *text = shown.into();
            }
        };
        swap(&mut self.maintenance_result, &RESULT_TEXTS);
        swap(&mut self.status, &STATUS_TEXTS);
        for row in &mut self.accounts {
            swap(&mut row.status, &ROW_TEXTS);
        }
        if let Some(patch) = &mut self.patch {
            swap(&mut patch.local_version, &PATCH_TEXTS);
            swap(&mut patch.remote_version, &PATCH_TEXTS);
            swap(&mut patch.detail, &PATCH_TEXTS);
        }
        if let Some(viewer) = &mut self.viewer { viewer.retranslate(from); }
    }

    /// `--preview-ja`: the sample news as the Japanese official site words it
    /// (headlines from its topics feed, dates as it dates them), so a capture
    /// shows Japanese cards, dates and tabs without the network.
    fn use_japanese_preview_news(&mut self) {
        let Some(snapshot) = &mut self.services else { return };
        let articles = [
            ("フリートライアルのレベル上限が75に！ プレイ期間が無制限になりました！", "Wed, 30 Sep 2026 14:00:00 JST",
             "「ファイナルファンタジーXI for Windows® フリートライアル」のレベル上限が75に引き上げられ、プレイ期間が無期限になりました！",
             "https://www.playonline.com/pcd2/topics/ff11/detail/40431/detail.html"),
            ("ヴァナ・ディールの異変を再調査せよ", "Thu, 24 Sep 2026 17:00:00 JST",
             "突如、とある巨大なモンスターが、本来は居ないはずのエリアに姿を現しました。冒険者の皆さんで、調査と討伐に向かいましょう！",
             "https://www.playonline.com/pcd2/topics/ff11/detail/40421/detail.html"),
            ("「真夏のノマドモグボナンザ2026」当せん番号発表！", "Fri, 18 Sep 2026 17:00:00 JST",
             "厳正なる抽選の結果、真夏のノマドモグボナンザ2026の当せん番号が決定しました！",
             "https://sqex.to/wuqaC"),
        ];
        for (item, (title, date, summary, url)) in snapshot.news.iter_mut().zip(articles) {
            item.title = title.into();
            item.date = date.into();
            item.summary = summary.into();
            item.url = url.into();
        }
        let notices = [
            ("全ワールド メンテナンス作業のお知らせ(9/29)", "2026.09.27(日) 14:00 From: ファイナルファンタジーXI",
             "下記日時におきまして、ファイナルファンタジーXIのメンテナンス作業を実施いたします。"),
            ("全ワールド メンテナンス作業のお知らせ(9/10)", "2026.09.08(火) 14:00 From: ファイナルファンタジーXI",
             "下記日時におきまして、サーバーメンテナンス作業を実施いたします。"),
            ("アカウント管理システム メンテナンス作業のお知らせ(9/28)", "2026.09.26(土) 01:30 From: プレイオンライン", ""),
        ];
        for (item, (title, date, summary)) in snapshot.notices.iter_mut().zip(notices) {
            item.title = title.into();
            item.date = date.into();
            item.summary = summary.into();
            item.url = "https://www.playonline.com/ff11/polnews/news.xml".into();
        }
    }

    /// The Friends previews: synthetic friends, mail and a conversation with
    /// messages both ways and one unread. Nothing is read from a session.
    fn load_ah_preview(&mut self) {
        self.ah_query = "copper".into();
        self.ah_server = "Bismarck";
        let generation = self.ah_lookup.begin("copper", "Bismarck");
        self.ah_lookup.finish(generation, Ok(ffxiah::Results {
            items: vec![ffxiah::Quote { name: "Copper Ore".into(), url: "https://www.ffxiah.com/item/640".into(),
                stack: Some(12), price: Some(2000), stock: Some(2), last_sale: false },
                ffxiah::Quote { name: "Copper Ingot".into(), url: "https://www.ffxiah.com/item/648".into(),
                stack: None, price: Some(2000), stock: Some(0), last_sale: false }], truncated: false,
        }));
    }

    fn load_friends_preview(&mut self, mailbox: bool, in_game: bool, compose: bool, new_mail: bool) {
        const ME: u64 = 900;
        self.page = Page::Friends;
        self.friends.account = self.accounts.first().map(|a| a.key.clone());
        self.friends.connected = true;
        let friend = |slot: u16, name: &str, online: Option<bool>, location: Option<&str>, character: Option<&str>, alternate: bool| friends::Friend {
            name: name.into(), alternate, slot, uid: Some(slot as u64 + 1 + if alternate { 500 } else { 0 }), online,
            location: location.map(Into::into), character: character.map(Into::into),
        };
        let entries = vec![
            friend(0, "Alyria", Some(true), Some("Lyrielle · Port Jeuno"), Some("Lyrielle"), false),
            friend(1, "Balthier", Some(true), Some("PlayOnline"), None, false),
            friend(2, "Celestine", Some(true), Some("FINAL FANTASY XI"), None, false),
            friend(3, "Haruko", Some(false), None, None, false),
            friend(4, "Nocturne", None, None, None, false),
            friend(0, "Mallory", None, None, None, true),
        ];
        // A day ago and today, at fixed clock times, so the capture reads naturally.
        let at = |days_ago: i64, hour: u32, minute: u32| -> u32 {
            let day = chrono::Local::now().date_naive() - chrono::Duration::days(days_ago);
            day.and_hms_opt(hour, minute, 0).and_then(|t| t.and_local_timezone(chrono::Local).single()).map_or(0, |t| t.timestamp() as u32)
        };
        let header = |id: u32, from: u64, to: u64, sender: &str, subject: &str, unread: bool, stamp: u32| {
            let mut bytes = [0u8; 72];
            bytes[..8].copy_from_slice(&from.to_le_bytes());
            bytes[8..16].copy_from_slice(&to.to_le_bytes());
            bytes[16..16 + sender.len().min(15)].copy_from_slice(&sender.as_bytes()[..sender.len().min(15)]);
            bytes[32..32 + subject.len().min(15)].copy_from_slice(&subject.as_bytes()[..subject.len().min(15)]);
            bytes[0x30..0x34].copy_from_slice(&id.to_le_bytes());
            bytes[0x34..0x38].copy_from_slice(&stamp.to_le_bytes());
            bytes[0x3e..0x40].copy_from_slice(&(if unread { 0x8000u16 } else { 0 }).to_le_bytes());
            let mut raw = Vec::new();
            pol_less::pipe::base64_into(&mut raw, &bytes);
            social::Header { raw: String::from_utf8(raw).unwrap(), sender: sender.into(), subject: subject.into(), unread, kind: 0, recipient: Some(to) }
        };
        // (id, from Alyria?, subject, text, unread, stamp)
        let alyria: [(u32, bool, &str, &str, bool, u32); 6] = [
            (1, true, "Sortie tonight?", "Are you free for Sortie tonight? We have five so far and could use your Red Mage.", false, at(1, 20, 41)),
            (2, false, "", "I'm in. Give me twenty minutes to finish crafting.", false, at(1, 20, 47)),
            (3, false, "", "Should I bring the Ambuscade set or the usual one?", false, at(1, 20, 48)),
            (4, true, "", "Usual one is fine. We'll meet at the Rhapsodies home point in Jeuno.", false, at(1, 20, 52)),
            (5, false, "Thanks!", "That was a great run. Thanks for carrying the last boss!", false, at(0, 9, 14)),
            (6, true, "Re: Thanks!", "Any time! Same time on Saturday? Celestine might join us too.", true, at(0, 18, 6)),
        ];
        let mut inbox = Vec::new();
        let mut sent = Vec::new();
        let mut bodies = Vec::new();
        for (id, theirs, subject, message, unread, stamp) in alyria {
            let h = if theirs { header(id, 1, ME, "Alyria", subject, unread, stamp) } else { header(id, ME, 1, "Ardent", subject, false, stamp) };
            bodies.push((social::message_key(&h.raw), subject, message));
            if theirs { inbox.push(h) } else { sent.push(h) }
        }
        inbox.push(header(7, 2, ME, "Balthier", "Linkshell event", true, at(0, 12, 30)));
        inbox.push(header(8, 77, ME, "Rook", "Bazaar price", false, at(2, 15, 2)));
        inbox.sort_by_key(|h| std::cmp::Reverse(h.stamp()));
        sent.sort_by_key(|h| std::cmp::Reverse(h.stamp()));
        let mut snapshot = friends::Snapshot {
            entries, fetched_at: friends::now(), game_running: Some(in_game),
            own_mode: Some(0), social: social::State { ready: true, ..Default::default() }, ..friends::Snapshot::default()
        };
        snapshot.social.inbox_unread = Some(if mailbox { 2 } else { 1 });
        if mailbox {
            snapshot.social.headers = inbox.clone();
            snapshot.social.total = inbox.len() as u32;
            snapshot.social.inbox_changed = new_mail;
            snapshot.social.opened = Some(social::Opened { sender: "Alyria".into(), subject: "Sortie tonight?".into(), message: "Are you free for Sortie tonight? We have five so far and could use your Red Mage.".into(), recipient: None });
        }
        self.friends.snapshot = Some(snapshot.clone());
        let mut listed = snapshot.clone();
        listed.social.action = Some("inbox".into());
        listed.social.headers = inbox;
        self.friends.completed(&listed);
        listed.social.folder = 2;
        listed.social.headers = sent;
        self.friends.completed(&listed);
        for (key, subject, message) in bodies {
            self.friends.mail.bodies.insert(key, friends::Body { subject: subject.into(), message: message.into() });
        }
        if mailbox {
            self.friends.select(Some(friends::Pane::Mailbox));
            return;
        }
        self.friends.select(Some(friends::Pane::Friend { slot: 0, name: "Alyria".into() }));
        // Opening the conversation read the unread one; the divider stays.
        for entry in self.friends.mail.listed[0].iter_mut().flatten().filter(|e| e.sender == "Alyria") {
            entry.unread = false;
        }
        if let Some(draft) = &mut self.friends.composer {
            if compose {
                draft.show_subject = true;
                draft.subject = "Saturday".into();
                draft.text = iced::widget::text_editor::Content::with_text("Saturday works for me.\nI'll bring the usual set.");
            }
        }
    }

    fn load_preview(&mut self) {
        let values = ["Main account", "Second account", "Crafting account"]
            .into_iter()
            .enumerate()
            .map(|(i, label)| {
                let mut a = blank_account();
                a.label = label.into();
                a.pol_id = format!("ABC1234{i}");
                a.pol_password = "synthetic".into();
                a.se_id = "preview".into();
                a.se_password = "synthetic".into();
                a
            })
            .collect();
        self.replace_accounts(values);
        self.vault_ready = true;
        self.status = t("Design preview — synthetic accounts. No account vault or game connection was accessed.").into();
        self.services = Some(services::ServiceSnapshot {
            news: vec![
                services::NewsItem {
                    title: "Free trial expands to level 75".into(),
                    url: "https://www.playonline.com/pcd2/topics/ff11us/detail/40432/detail.html"
                        .into(),
                    date: "Sep 30".into(),
                    summary: "No time limit. More to explore.".into(),
                    image_url: String::new(),
                    image: None,
                },
                services::NewsItem {
                    title: "Strange Happenings returns to Vana’diel".into(),
                    url: "https://www.playonline.com/pcd2/topics/ff11us/detail/40422/detail.html"
                        .into(),
                    date: "Sep 24".into(),
                    summary: "Unusual visitors await.".into(),
                    image_url: String::new(),
                    image: None,
                },
                services::NewsItem {
                    title: "Nomad Mog Bonanza · The results are in".into(),
                    url: "https://sqex.to/w97dY".into(),
                    date: "Sep 18".into(),
                    summary: "Find your prize collection details.".into(),
                    image_url: String::new(),
                    image: None,
                },
            ],
            notices: [
                ("All Worlds Maintenance (Sep. 28)", "Sep. 26, 2026 22:00 [PDT] From: FINAL FANTASY XI", "At the following time, we will be performing maintenance on all worlds."),
                ("All Worlds Maintenance (Sep. 9)", "Sep. 7, 2026 22:00 [PDT] From: FINAL FANTASY XI", "At the following time, we will be performing server maintenance."),
                ("Forum Technical Difficulties (Sep. 2)", "Sep. 3, 2026 01:24 [PDT] From: FINAL FANTASY XI", ""),
            ].into_iter().map(|(title, date, summary)| services::NewsItem {
                title: title.into(), date: date.into(), summary: summary.into(),
                url: "https://www.playonline.com/ff11us/polnews/news.xml".into(),
                image_url: String::new(), image: None,
            }).collect(),
            news_note: t("Preview content. Live feeds load when the launcher starts normally.").into(),
            availability: services::Availability::Unknown,
            availability_note: t("Not checked in design preview").into(),
            checked_at: t("Preview").into(),
        });
        if i18n::current() == i18n::Lang::Ja {
            self.use_japanese_preview_news();
        }
        self.patch = Some(patch::PatchReport {
            state: patch::PatchState::Current, // mockup shows "Ready to play"
            local_version: t("Not checked").into(),
            remote_version: t("Not checked").into(),
            detail: t("The launcher compares your installation with the official patch service.")
                .into(),
            source_url: None, files_verified: None,
        });
    }
}

fn blank_account() -> Account {
    Account {
        label: String::new(),
        mode: AccountMode::Retail,
        server: String::new(),
        pol_id: String::new(),
        pol_password: String::new(),
        se_id: String::new(),
        se_password: String::new(),
        region: 0,
        launch_with: None,
        ashita_boot_ini: None,
        windower_profile: None,
    }
}

fn default_installer_path() -> String {
    Config::default()
        .experiment
        .join("official-installer")
        .to_string_lossy()
        .into_owned()
}

/// Where a Browse dialog opens: the field's folder (or the file's folder), else the
/// nearest existing parent, else the dialog's own default.
fn dialog_start(raw: &str, file: bool) -> Option<PathBuf> {
    let mut path = PathBuf::from(raw.trim());
    if raw.trim().is_empty() || !path.is_absolute() {
        return None;
    }
    if file {
        path.pop();
    }
    let mut dir = Some(path.as_path());
    while let Some(candidate) = dir {
        if candidate.is_dir() {
            return Some(backend::plain_windows_path(candidate));
        }
        dir = candidate.parent();
    }
    None
}

async fn pick_path(target: PickTarget, start: Option<PathBuf>) -> Option<PathBuf> {
    let mut dialog = rfd::AsyncFileDialog::new();
    if let Some(start) = start {
        dialog = dialog.set_directory(start);
    }
    let handle = match target {
        PickTarget::LoaderFile => {
            dialog
                .set_title(t("Choose the Crystal loader (pol.exe)"))
                .add_filter(t("Executable"), &["exe"])
                .pick_file()
                .await
        }
        _ => {
            let title = match target {
                PickTarget::GameFolder | PickTarget::AccountGameFolder => t("Choose the FINAL FANTASY XI folder"),
                PickTarget::AccountDatFolder => t("Choose a DAT overlay folder containing ROM or sound folders"),
                PickTarget::InstallerFolder => t("Choose the download or extracted setup folder"),
                PickTarget::AshitaFolder => t("Choose the Ashita folder"),
                _ => t("Choose the Windower folder"),
            };
            dialog.set_title(title).pick_folder().await
        }
    };
    handle.map(|h| h.path().to_path_buf())
}

/// `\\?\C:\x` -> `C:\x` for a path kept as text; anything else is left alone.
fn plain_text_path(raw: &str) -> String {
    backend::plain_windows_path(std::path::Path::new(raw)).to_string_lossy().into_owned()
}

impl Settings {
    fn config_for(&self, base: &Config, account: &Account) -> Config {
        let mut config = base.clone();
        if self.install_selected { config.game_install = Some(PathBuf::from(&self.install_path)); }
        if let Some(path) = self.account_installs.get(&account_key(account)).filter(|p| !p.trim().is_empty()) {
            config.game_install = Some(backend::plain_windows_path(PathBuf::from(path).as_path()));
        }
        config.dat_overlays = if self.launch_with_for(account) == ui::LaunchWith::Native {
            self.account_overlays.get(&account_key(account)).into_iter().flatten()
                .map(|p| backend::plain_windows_path(PathBuf::from(p).as_path())).collect()
        } else { Vec::new() };
        config
    }

    /// Saved settings may hold `\\?\` paths from older builds' folder picker.
    fn plain_paths(&mut self) {
        for choice in self.account_auto_login.values_mut() { *choice = choice.normalized(); }
        for path in self.account_installs.values_mut() { *path = plain_text_path(path); }
        for overlays in self.account_overlays.values_mut() { for path in overlays { *path = plain_text_path(path); } }
        for field in [
            &mut self.install_path,
            &mut self.installer_path,
            &mut self.ashita_path,
            &mut self.windower_path,
            &mut self.loader_path,
        ] {
            let plain = plain_text_path(field);
            if plain != *field {
                *field = plain;
            }
        }
    }
}

fn selected_folder(raw: &str, must_exist: bool) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw.trim());
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(
            t("Enter an absolute folder path without parent-directory (..) components.").into(),
        );
    }
    // Plain `C:\...`, not canonicalize's `\\?\C:\...`: this is stored and shown.
    match std::fs::canonicalize(&path).map(|p| backend::plain_windows_path(&p)) {
        Ok(path) if path.is_dir() => Ok(path),
        Ok(_) => Err(t("Select a folder, not a file.").into()),
        Err(_) if must_exist => {
            Err(t("Choose the existing FINAL FANTASY XI installation folder.").into())
        }
        Err(_) => {
            // Resolve the existing parent so a new download folder uses its actual location.
            let mut ancestor = path.as_path();
            let mut suffix = Vec::new();
            loop {
                if let Ok(mut resolved) =
                    std::fs::canonicalize(ancestor).map(|p| backend::plain_windows_path(&p))
                {
                    if !resolved.is_dir() {
                        return Err(t("The download folder's parent is not a directory.").into());
                    }
                    for part in suffix.into_iter().rev() {
                        resolved.push(part);
                    }
                    return Ok(resolved);
                }
                suffix.push(
                    ancestor
                        .file_name()
                        .ok_or(t("The folder path could not be resolved."))?
                        .to_owned(),
                );
                ancestor = ancestor
                    .parent()
                    .ok_or(t("The folder path could not be resolved."))?;
            }
        }
    }
}

fn initial_window_size() -> iced::Size {
    // TEMP: XI_PREVIEW_SIZE=WxH for responsive checks.
    if let Some((w, h)) = std::env::var("XI_PREVIEW_SIZE").ok().and_then(|v| {
        let (w, h) = v.split_once('x')?;
        Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
    }) {
        return iced::Size::new(w, h);
    }
    if std::env::args().any(|arg| arg == "--preview-small") {
        iced::Size::new(900.0, 650.0)
    } else if std::env::args().any(|arg| arg == "--preview-wide") {
        // a large window, to check the layout reflows at 1.0.
        iced::Size::new(1920.0, 1080.0)
    } else {
        iced::Size::new(1440.0, 900.0)
    }
}

fn settings_path(config: &Config) -> PathBuf {
    config.experiment.join("iced-launcher/settings.json")
}
/// The headline cache for `lang`: one file per language, so one language's
/// headlines are never shown in the other.
fn news_cache_path(config: &Config, lang: i18n::Lang) -> PathBuf {
    services::cache_path(&config.experiment.join("iced-launcher/news-cache.json"), lang)
}
/// Loads the saved settings and sets the UI language from them: the Windows
/// display language until a choice is saved. The language is set before the
/// load, so a load error is already in it. The window and the background
/// session worker both start this way, so they always agree.
fn load_settings_and_language(config: &Config) -> Result<Settings, String> {
    i18n::set(i18n::system_default());
    let settings = load_settings(config)?;
    if let Some(language) = settings.language {
        i18n::set(language);
    }
    Ok(settings)
}

/// "A", "A and B", "A and B and C"; each join is a translatable sentence part.
fn join_and(names: &[&str]) -> String {
    let mut names = names.iter();
    let mut joined = names.next().map(|first| first.to_string()).unwrap_or_default();
    for next in names {
        joined = tf("{a} and {b}", &[("a", &joined), ("b", next)]);
    }
    joined
}

/// Fixed sentences the window keeps as text and re-words on a language change
/// (each is a `t("…")` key where it is set).
const RESULT_TEXTS: [&str; 3] = [
    "Files have not been checked. Version availability alone does not verify file integrity.",
    "Selected installation has not been checked. Use Check files to verify it.",
    "Installation path changed. Select this folder, then Check files.",
];
const STATUS_TEXTS: [&str; 9] = [
    "Loading saved accounts…",
    "Ready. Select accounts or use Launch all. Games continue when you close Crystal.",
    "Accounts saved.",
    "Enter account details, then save.",
    "Edit account details.",
    "Settings saved. Checking the installed version…",
    "Design preview — synthetic accounts. No account vault or game connection was accessed.",
    "Finding FINAL FANTASY XI…",
    "No installed FFXI folder was found. Run official setup or use Browse.",
];
const ROW_TEXTS: [&str; 9] = [
    "Ready to launch",
    "Private-server client temporarily unavailable",
    "Private-server client temporarily unavailable; saved account kept.",
    "Signed in to PlayOnline · FFXI is closed",
    "Client started · continue in game",
    "Client closed",
    "Starting sign-in…",
    "Checking the game version…",
    "Reconnecting to the background session…",
];
const PATCH_TEXTS: [&str; 7] = [
    "Unavailable",
    "Not checked",
    "Not initialized",
    "FFXI has not completed its initial update. Choose Update to finish and create its installed-version record.",
    "The official patch server is asked when you press Play, before FINAL FANTASY XI starts.",
    "A newer official version is available.",
    "The launcher compares your installation with the official patch service.",
];

/// The current language's text for `text` when it is one of `keys` as it read
/// in `from`.
fn retranslated(text: &str, from: i18n::Lang, keys: &[&'static str]) -> Option<&'static str> {
    keys.iter()
        .find(|key| text == i18n::t_in(from, key))
        .map(|key| i18n::t_in(i18n::current(), key))
}

fn load_settings(config: &Config) -> Result<Settings, String> {
    let bytes = match read_settings_bytes(&settings_path(config))? {
        Some(bytes) => bytes,
        None => return Ok(Settings::default()),
    };
    if bytes.len() > 1024 * 1024 {
        return Err(t("Settings file is too large.").into());
    }
    let mut settings: Settings =
        serde_json::from_slice(&bytes).map_err(|_| t("Settings could not be read."))?;
    settings.plain_paths();
    settings.launch_with = settings.launch_with.retail();
    Ok(settings)
}
fn save_settings(config: &Config, settings: &Settings) -> Result<(), String> {
    use std::io::Write;
    let path = settings_path(config);
    let data = serde_json::to_vec_pretty(settings).map_err(|_| t("Could not encode settings."))?;
    if data.len() > 1024 * 1024 { return Err(t("Launcher settings are too large to save.").into()); }
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|_| t("Could not create the launcher settings folder."))?;
    if let Some(previous) = read_settings_bytes(&path)? {
        if serde_json::from_slice::<Settings>(&previous).is_err() {
            let backup = path.with_extension(format!("invalid-{}.json", backend::unique_id()?));
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(backup)
                .map_err(|_| t("Could not preserve the damaged settings file; it was kept unchanged."))?;
            file.write_all(&previous).and_then(|_| file.sync_all())
                .map_err(|_| t("Could not preserve the damaged settings file; it was kept unchanged."))?;
        }
    }
    let temporary = path.with_extension(format!("{}.tmp", backend::unique_id()?));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)
            .map_err(|_| t("Could not write launcher settings."))?;
        file.write_all(&data).and_then(|_| file.sync_all()).map_err(|_| t("Could not write launcher settings."))?;
        drop(file);
        std::fs::rename(&temporary, &path).map_err(|_| t("Could not replace launcher settings.").into())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&temporary); }
    result
}

fn read_settings_bytes(path: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(t("Launcher settings are unavailable.").into()),
    };
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|_| t("Could not read launcher settings."))?;
    if bytes.len() > 1024 * 1024 { return Err(t("Launcher settings file is too large; it was kept unchanged.").into()); }
    Ok(Some(bytes))
}

fn allowed_link(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some(
                "www.playonline.com"
                    | "playonline.com"
                    | "sqex.to"
                    | "www.ffxiah.com"
                    | "ffxiah.com"
                    | "www.bg-wiki.com"
                    | "bg-wiki.com"
                    | "support.na.square-enix.com"
                    | "reshade.me"
            )
        )
}
fn open_url(url: &str) -> Result<(), String> {
    if !allowed_link(url) {
        return Err(t("This link is not an allowed FFXI website.").into());
    }
    shell_open(url).map_err(|_| t("Windows could not open the website.").into())
}
/// Open a client log, or the logs folder itself, in the default app. The target must resolve to
/// somewhere inside `logs_dir` (the account's own logs folder) and a file must be a `.log`.
fn open_log_path(logs_dir: &std::path::Path, target: &std::path::Path) -> Result<(), String> {
    let resolved = launch_log::resolve_log_target(logs_dir, target)?;
    // canonicalize yields \\?\C:\..., which ShellExecute handles but Notepad titles show verbatim.
    let plain = backend::plain_windows_path(&resolved);
    shell_open(&plain.to_string_lossy()).map_err(|_| t("Windows could not open the log.").into())
}
/// ShellExecute "open" on a URL or path the caller has already vetted.
fn shell_open(target: &str) -> Result<(), ()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
        let target: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        let verb: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if result as isize <= 32 {
            return Err(());
        }
    }
    #[cfg(not(windows))]
    let _ = target;
    Ok(())
}
fn save_screenshot(path: &std::path::Path, screenshot: window::Screenshot) -> Result<(), String> {
    let output = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut encoder = png::Encoder::new(output, screenshot.size.width, screenshot.size.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer
        .write_image_data(&screenshot.rgba)
        .map_err(|e| e.to_string())
}

#[cfg(windows)]
struct InstanceGuard(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
#[cfg(windows)]
fn claim_instance() -> Result<Option<InstanceGuard>, String> {
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_ALREADY_EXISTS},
        System::Threading::CreateMutexW,
        UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE},
    };
    let name: Vec<u16> = "Local\\XI-Iced-Launcher-Test-1"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(
            t("Windows could not start the launcher instance. Close any other copy and try again.")
                .into(),
        );
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        let title: Vec<u16> = "Crystal Launcher".encode_utf16().chain(Some(0)).collect();
        let window = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
        if !window.is_null() {
            unsafe {
                ShowWindow(window, SW_RESTORE);
                SetForegroundWindow(window);
            }
        }
        return Ok(None);
    }
    Ok(Some(InstanceGuard(handle)))
}

fn main() -> iced::Result {
    if let Some(result) = self_update::cli(&std::env::args().collect::<Vec<_>>()) {
        if let Err(error) = result { eprintln!("{error}"); std::process::exit(1); }
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("--windower-profile-editor-worker") {
        let _ = load_settings_and_language(&Config::default());
        let args: Vec<_> = std::env::args_os().collect();
        if args.len() != 3 { std::process::exit(1); }
        std::process::exit(launch_tools::run_profile_editor_worker(std::path::Path::new(&args[2])));
    }
    if std::env::args().any(|a| a == "--batch-worker") {
        let _ = load_settings_and_language(&Config::default());
        std::process::exit(session_worker::run_batch());
    }
    if std::env::args().any(|a| a == "--session-worker") {
        // The worker writes progress and error text that the window shows, so it
        // speaks the window's language: the same saved setting, read the same way.
        let _ = load_settings_and_language(&Config::default());
        let code = session_worker::run();
        pol_less::profile_access::finish_pending_launchers();
        std::process::exit(code);
    }
    // Anonymous maintenance diagnostics never load saved accounts or create UI.
    let args: Vec<String> = std::env::args().collect();
    if let Some(index) = args
        .iter()
        .position(|a| a == "--check-files" || a == "--patch-probe")
    {
        let Some(target) = args.get(index + 1) else {
            eprintln!("An absolute game installation path is required.");
            std::process::exit(2);
        };
        let path = PathBuf::from(target);
        if !path.is_absolute() {
            eprintln!("An absolute game installation path is required.");
            std::process::exit(2);
        }
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let op = operation::Operation::new();
        let result = runtime.block_on(async {
            let progress = op.clone();
            let monitor = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    let p = progress.snapshot();
                    eprintln!("{} {}/{}: {}", p.stage, p.done, p.total, p.detail);
                }
            });
            let result = if args[index] == "--patch-probe" {
                updater::probe(path, op).await
            } else {
                updater::run(path, updater::Mode::Check, op).await.map(|r| {
                    let mut details = r.summary;
                    for issue in r.issues {
                        details.push_str(&format!("\n{issue}"));
                    }
                    details
                })
            };
            monitor.abort();
            result
        });
        match result {
            Ok(report) => println!("{report}"),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    if std::env::args().any(|a| a == "--check-services") {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            // Installed record only: the official patch server is asked before a game start.
            let services = services::refresh().await;
            let patch = patch::local_report(&PathBuf::from(Settings::default().install_path));
            println!(
                "Patch: {:?}; local {}; official {}; {}; source {}",
                patch.state,
                patch.local_version,
                patch.remote_version,
                patch.detail,
                patch.source_url.as_deref().unwrap_or("Unavailable")
            );
            match services {
                Ok(s) => println!(
                    "News items: {}; decoded article images: {}; maintenance notices: {}; availability {:?}: {}; feed note: {}",
                    s.news.len(),
                    s.news.iter().filter(|item| item.image.is_some()).count(),
                    s.notices.len(),
                    s.availability,
                    s.availability_note,
                    s.news_note
                ),
                Err(e) => println!("Services: {e}"),
            }
        });
        return Ok(());
    }
    let preview = std::env::args().any(|a| a.starts_with("--preview"));
    // Before the single-instance check, so its error box is in the user's language too.
    if !preview {
        let _ = load_settings_and_language(&Config::default());
    }
    #[cfg(windows)]
    let _instance = if preview {
        None
    } else {
        match claim_instance() {
            Ok(Some(guard)) => Some(guard),
            Ok(None) => return Ok(()),
            Err(error) => {
                let text: Vec<u16> = error.encode_utf16().chain(Some(0)).collect();
                let title: Vec<u16> = "Crystal Launcher".encode_utf16().chain(Some(0)).collect();
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                        std::ptr::null_mut(),
                        text.as_ptr(),
                        title.as_ptr(),
                        windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
                    );
                }
                return Ok(());
            }
        }
    };
    // wgpu on Direct3D 12 only. Probing every backend (Vulkan and
    // GL too) costs ~150 ms of startup here; if D3D12 is unavailable iced falls
    // back to its tiny-skia software renderer. The low-power preference keeps
    // a hybrid laptop on its integrated GPU for a launcher. Either can still be
    // overridden from the environment.
    #[cfg(windows)]
    for (key, value) in [("WGPU_BACKEND", "dx12"), ("WGPU_POWER_PREF", "low")] {
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
        }
    }
    // Japanese text falls back to Yu Gothic UI glyph by glyph.
    i18n::install_font_fallback();
    iced::application(App::new, App::update, ui::view)
        .title("Crystal Launcher")
        .theme(|app: &App| ui::theme(app.settings.light_theme))
        .subscription(App::subscription)
        .default_font(Font::with_name("Segoe UI"))
        .window(window::Settings {
            size: initial_window_size(),
            position: window::Position::Centered,
            decorations: false,
            min_size: Some(iced::Size::new(900.0, 650.0)),
            exit_on_close_request: false,
            // The taskbar and Alt+Tab icon: the title bar's crystal (assets/crystal.ico
            // is the same art, embedded as the exe's icon by build.rs).
            icon: window::icon::from_rgba(include_bytes!("../assets/crystal-icon-64.rgba").to_vec(), 64, 64).ok(),
            // Rounded corners from the start; the shadow and border colour are
            // added by window_chrome once the window has a handle.
            #[cfg(windows)]
            platform_specific: window::settings::PlatformSpecific {
                corner_preference: window::settings::platform::CornerPreference::Round,                ..Default::default()
            },
            ..Default::default()
        })
        .run()
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn app() -> App {
        let (sender, receiver) = mpsc::channel();
        let mut app = App {
            viewport: iced::Size::new(1200.0, 820.0),
            maximized: false,
            title_held: false,
            page: Page::Home,
            show_details: false,
            accounts: vec![],
            account_drag: account_order::Drag::default(),
            windower_editor_open: false,
            editor: None, account_import: account_import_ui::State::default(),
            services: None,
            news_loading: false,
            news_lang: i18n::Lang::En,
            news_generation: 0,
            news_error: String::new(),
            startup_warning: None,
            patch: None,
            patch_loading: false,
            installation_loading: false,
            installation_generation: 0,
            viewer: None,
            viewer_loading: false,
            viewer_generation: 0,
            settings: Settings::default(),
            show_settings: false,
            status: String::new(),
            vault_ready: true,
            startup_signin_pending: false,
            confirm_close: false,
            pending_delete: None,
            wiki_query: String::new(),
            ah_query: String::new(),
            ah_lookup: ffxiah::Lookup::default(),
            ah_server: ui::DEFAULT_AH_SERVER,
            news_filter: 0,
            maintenance: None,
            maintenance_progress: operation::Progress::default(),
            maintenance_result: String::new(),
            installer_error: None,
            maintenance_tone: ResultTone::default(),
            maintenance_cancelled: false,
            active_operation: None,
            config: Config::default(),
            sender,
            receiver,
            vault_saving: false,
            patch_generation: 0,
            refreshed: Instant::now(),
            preview: true,
            preview_capture_sent: false,
            // The Settings page fields, so the test build compiles.
            settings_tab: settings_page::Tab::default(),
            game_config: game_config::State::default(),
            prereqs: prereqs::State::default(),
            gamepad: gamepad::State::default(),
            launch_tools: launch_tools::State::default(),
            friends: friends::State::default(),
            friends_generation: 0,
            friends_checked: Instant::now(),
            dgvoodoo: dgvoodoo::State::new(false),
            reshade: reshade::State::default(),
            self_update: self_update::State::default(),
            controller_nav: controller_nav::State::default(),
        };
        app.load_preview();
        app
    }
    #[test]
    fn reshade_setup_blocks_launch_updates_and_launcher_restart() {
        let mut app = app();
        app.preview = false;
        app.reshade.busy = true;
        assert!(app.launch_blocked());
        assert!(!app.can_restart_launcher());
        assert_eq!(app.main_action(), MainAction::Working);
        let _ = app.run_main_action();
        assert!(app.page == Page::Settings && app.settings_tab == settings_page::Tab::Graphics);
        assert_eq!(app.update(Message::CloseRequested).units(), 0);
        assert_eq!(app.update(Message::ConfirmClose).units(), 0);
        assert_eq!(app.start_maintenance(MaintenanceKind::Update).units(), 0);
        let _ = dgvoodoo::update(&mut app, dgvoodoo::Msg::Toggle(false));
        assert!(app.settings.dgvoodoo);
    }

    #[test]
    fn reshade_toggle_preserves_a_running_sessions_renderer() {
        let mut app = app();
        app.settings.reshade.enabled = true;
        app.reshade.installed = true;
        app.preview = false;
        app.accounts[0].pid = Some(12345);
        let _ = reshade::update(&mut app, reshade::Msg::Toggle(false));
        assert!(app.settings.reshade.enabled);
        let _ = dgvoodoo::update(&mut app, dgvoodoo::Msg::Toggle(false));
        assert!(app.settings.dgvoodoo && app.settings.reshade.enabled);
    }

    #[test]
    fn reshade_ignores_install_checks_from_before_a_newer_check() {
        let mut app = app();
        app.preview = false;
        app.reshade.installed = true;
        let _ = reshade::update(&mut app, reshade::Msg::Refresh);
        let _ = reshade::update(&mut app, reshade::Msg::Checked(0, false, "stale.ini".into()));
        assert!(app.reshade.installed && app.reshade.preset.is_empty());
        let _ = reshade::update(&mut app, reshade::Msg::Checked(1, true, "current.ini".into()));
        assert_eq!(app.reshade.preset, "current.ini");
        let _ = reshade::update(&mut app, reshade::Msg::ChoosePreset);
        assert!(app.reshade.busy);
        let _ = reshade::update(&mut app, reshade::Msg::Picked(None));
        let _ = reshade::update(&mut app, reshade::Msg::Checked(1, false, "stale.ini".into()));
        assert!(app.reshade.installed && app.reshade.preset == "current.ini");
    }

    #[test]
    fn ffxiah_lookup_tracks_world_changes_and_blocks_background_controller_actions() {
        let mut app = app();
        app.ah_query = "copper".into();
        let _ = app.update(Message::SearchAh);
        assert!(app.ah_lookup.open && app.controller_modal());
        let previous = app.ah_lookup.generation;
        let _ = app.update(Message::AhServerChanged("Bismarck"));
        assert_eq!(app.ah_lookup.world, "Bismarck");
        assert!(app.ah_lookup.generation > previous);
        let states: Vec<_> = app.accounts.iter().map(|a| a.busy).collect();
        let page = app.page;
        let _ = app.update(Message::ControllerPlay);
        let _ = app.update(Message::ControllerPage(1));
        assert_eq!(app.accounts.iter().map(|a| a.busy).collect::<Vec<_>>(), states);
        assert!(app.page == page);
        let _ = app.update(Message::ControllerBack);
        assert!(!app.ah_lookup.open && !app.controller_modal());
    }

    /// Privacy mode on: no synthetic name, ID, character, subject or message
    /// is readable anywhere the screens put text, and the names are there as
    /// blurred text (one `▒` per character), on every page that shows them.
    #[test]
    fn privacy_mode_blurs_every_identity_on_every_page() {
        i18n::set(i18n::Lang::En);
        let secrets = [
            "Main account", "Second account", "Crafting account", "ABC12340", "ABC12341",
            "Alyria", "Balthier", "Celestine", "Haruko", "Nocturne", "Mallory", "Rook", "Lyrielle", "Ardent",
            "Sortie tonight", "Rhapsodies", "Red Mage", "Linkshell event", "Bazaar price", "Saturday",
        ];
        let size = iced::Size::new(1440.0, 900.0);
        let render = |page: Page, pane: Option<friends::Pane>, mailbox: bool, in_game: bool| {
            let mut app = app();
            app.load_friends_preview(mailbox, in_game, true, false);
            app.page = page;
            if let Some(pane) = pane { app.friends.select(Some(pane)); }
            app.settings.privacy = true;
            app.status = "Main account: Alyria sent a message".into();
            privacy::set(true);
            let texts = privacy::testing::texts(ui::view(&app), size);
            privacy::set(false);
            texts
        };
        let pages = [
            (render(Page::Home, None, false, false), &["Main account"][..]),
            (render(Page::Accounts, None, false, false), &["Main account", "Second account", "Crafting account"][..]),
            (render(Page::Friends, None, false, false), &["Alyria", "Balthier", "Lyrielle"][..]),
            (render(Page::Friends, None, false, true), &["Alyria", "Haruko"][..]),
            (render(Page::Friends, Some(friends::Pane::Mailbox), true, false), &["Rook", "Sortie tonight?"][..]),
            (render(Page::Friends, Some(friends::Pane::Blocked), false, false), &["Mallory"][..]),
        ];
        for (texts, blurred) in &pages {
            assert!(texts.len() > 10, "the page rendered: {texts:?}");
            privacy::testing::assert_blurred(texts, &secrets, blurred);
        }
        // The same pages read normally with privacy off.
        let mut app = app();
        app.load_friends_preview(false, false, false, false);
        let texts = privacy::testing::texts(ui::view(&app), size);
        assert!(texts.iter().any(|t| t == "Alyria") && texts.iter().any(|t| t.contains("Rhapsodies")), "{texts:?}");
    }

    #[test]
    fn update_activity_shows_remaining_time_and_clears_it_when_cancelled() {
        privacy::set(false);
        for (language, remaining, estimate, estimating) in [
            (i18n::Lang::En, "65% remaining in this step", "About 2 min remaining", "Estimating time remaining…"),
            (i18n::Lang::Ja, "このステップの残り：65%", "残り約2分", "残り時間を計算しています…"),
        ] {
            i18n::set(language);
            let mut app = app();
            app.page = Page::Maintenance;
            app.maintenance = Some(MaintenanceKind::Update);
            app.maintenance_progress = operation::Progress {
                stage: "Download".into(), done: 42_000_000, total: 120_000_000,
                bytes: true, remaining: Some(Duration::from_secs(115)),
                ..operation::Progress::default()
            };
            for size in [iced::Size::new(1100.0, 720.0), iced::Size::new(1440.0, 900.0)] {
                let texts = privacy::testing::texts(ui::view(&app), size);
                assert!(texts.iter().any(|text| text.contains(remaining) && text.contains(estimate)), "{texts:?}");
                assert!(texts.iter().any(|text| text.contains("42 MB")), "byte progress must retain its units: {texts:?}");
            }
            app.maintenance_progress.remaining = None;
            let size = iced::Size::new(1100.0, 720.0);
            let texts = privacy::testing::texts(ui::view(&app), size);
            assert!(texts.iter().any(|text| text.contains(estimating)), "{texts:?}");
            app.maintenance_cancelled = true;
            let texts = privacy::testing::texts(ui::view(&app), size);
            assert!(texts.iter().any(|text| text.contains(remaining)));
            assert!(!texts.iter().any(|text| text.contains(estimating) || text.contains(estimate)));
            app.maintenance_cancelled = false;
            app.maintenance = Some(MaintenanceKind::Setup);
            app.maintenance_progress = operation::Progress::default();
            let texts = privacy::testing::texts(ui::view(&app), size);
            assert!(!texts.iter().any(|text| text.contains(estimating)));
        }
        i18n::set(i18n::Lang::En);
    }

    #[test]
    fn update_activity_uses_the_whole_update_estimate_above_current_step() {
        privacy::set(false);
        for (language, title, percent, estimate, calculating, estimating, finalizing) in [
            (i18n::Lang::En, "Overall update", "72% remaining overall", "About 5 min remaining for the whole update",
             "Calculating total update size…", "Estimating total time remaining…", "Finalizing update…"),
            (i18n::Lang::Ja, "アップデート全体", "全体の残り：72%", "アップデート全体の残り時間：約5分",
             "アップデート全体のサイズを計算しています…", "アップデート全体の残り時間を計算しています…", "アップデートの最終処理を行っています…"),
        ] {
            i18n::set(language);
            let mut app = app();
            app.page = Page::Maintenance;
            app.maintenance = Some(MaintenanceKind::Update);
            app.maintenance_progress = operation::Progress {
                stage: "Download".into(), done: 120_000_000, total: 120_000_000, bytes: true,
                remaining: Some(Duration::ZERO),
                overall: Some(operation::OverallProgress { fraction: Some(0.28), remaining: Some(Duration::from_secs(265)), complete: false }),
                ..operation::Progress::default()
            };
            for size in [iced::Size::new(900.0, 650.0), iced::Size::new(1440.0, 900.0)] {
                let texts = privacy::testing::texts(ui::view(&app), size);
                let title_index = texts.iter().position(|text| text == title).unwrap();
                let step_label = tf("Current step: {step}", &[("step", &t("Downloading official files"))]);
                let step_index = texts.iter().position(|text| text == &step_label).unwrap();
                assert!(title_index < step_index, "overall progress must lead the current step");
                assert!(texts.iter().any(|text| text == percent), "{texts:?}");
                assert!(texts.iter().any(|text| text == estimate), "{texts:?}");
                let step_remaining = tf("{percent}% remaining in this step", &[("percent", &0)]);
                assert!(!texts.iter().any(|text| text.contains(&step_remaining)));
            }
            let size = iced::Size::new(900.0, 650.0);
            app.maintenance_progress.overall.as_mut().unwrap().remaining = None;
            let texts = privacy::testing::texts(ui::view(&app), size);
            assert!(texts.iter().any(|text| text == estimating));
            app.maintenance_cancelled = true;
            let texts = privacy::testing::texts(ui::view(&app), size);
            assert!(!texts.iter().any(|text| text == estimating || text == estimate));
            app.maintenance_cancelled = false;
            app.maintenance_progress.overall = Some(operation::OverallProgress::default());
            let texts = privacy::testing::texts(ui::view(&app), size);
            assert!(texts.iter().any(|text| text == calculating));
            app.maintenance_progress.overall = Some(operation::OverallProgress {
                fraction: Some(0.99), remaining: Some(Duration::ZERO), complete: false,
            });
            let texts = privacy::testing::texts(ui::view(&app), size);
            assert!(texts.iter().any(|text| text == finalizing));
            assert!(texts.iter().any(|text| text == &tf("{percent}% remaining overall", &[("percent", &1)])));
        }
        i18n::set(i18n::Lang::En);
    }

    #[test]
    fn privacy_mode_is_a_saved_setting_that_changes_no_data() {
        let old: Settings = serde_json::from_str(r#"{"install_path":"C:\\Games\\FFXI"}"#).unwrap();
        assert!(!old.privacy);
        assert!(!serde_json::to_string(&old).unwrap().contains("privacy"));
        let mut app = app();
        let accounts: Vec<_> = app.accounts.iter().map(|a| (a.account.label.clone(), a.account.pol_id.clone())).collect();
        let _ = app.update(Message::SetPrivacy(true));
        assert!(app.settings.privacy && privacy::on());
        assert!(serde_json::to_string(&app.settings).unwrap().contains(r#""privacy":true"#));
        assert_eq!(app.accounts.iter().map(|a| (a.account.label.clone(), a.account.pol_id.clone())).collect::<Vec<_>>(), accounts);
        let _ = app.update(Message::SetPrivacy(false));
        assert!(!privacy::on());
    }

    #[test]
    fn deleting_an_account_requires_confirmation_and_cancel_keeps_it() {
        let mut app = app();
        let key = app.accounts[0].key.clone();
        app.preview = false;
        assert_eq!(app.update(Message::DeleteAccount(key.clone())).units(), 0);
        assert!(!app.vault_saving);
        let _ = app.update(Message::RequestDeleteAccount(key.clone()));
        assert_eq!(app.pending_delete.as_deref(), Some(key.as_str()));
        let _ = app.update(Message::CancelDeleteAccount);
        assert!(app.pending_delete.is_none());
        assert_eq!(app.accounts.len(), 3);
        let _ = app.update(Message::RequestDeleteAccount(key.clone()));
        assert_eq!(app.update(Message::DeleteAccount(key)).units(), 1);
        assert!(app.vault_saving);
        assert!(app.pending_delete.is_none());
    }

    #[test]
    fn switching_language_swaps_the_news_and_drops_the_other_languages_late_results() {
        let root = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("crystal-news-language-{}", backend::unique_id().unwrap()));
        let mut app = app();
        app.config = Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        app.preview = false;
        let en_cache = news_cache_path(&app.config, i18n::Lang::En);
        let ja_cache = news_cache_path(&app.config, i18n::Lang::Ja);
        assert_eq!(en_cache.file_name().unwrap(), "news-cache.json", "the English file keeps its name");
        assert_eq!(ja_cache.file_name().unwrap(), "news-cache.ja.json");
        std::fs::create_dir_all(en_cache.parent().unwrap()).unwrap();
        let saved = |title: &str, site: &str, lang: &str| format!(r#"{{"items":[{{"title":"{title}","url":"https://www.playonline.com/{site}/polnews/news1.shtml","date":"","summary":"","image_url":""}}],"saved_at":"2026-10-06 00:00:00 UTC","language":"{lang}"}}"#);
        std::fs::write(&en_cache, saved("English headline", "ff11us", "en")).unwrap();
        let headline = |app: &App| app.services.as_ref().map(|s| s.news[0].title.clone());
        let late = |title: &str| {
            let item = services::NewsItem { title: title.into(), url: "https://www.playonline.com/news/late.html".into(), ..Default::default() };
            services::ServiceSnapshot { news: vec![item], notices: Vec::new(), news_note: String::new(), availability: services::Availability::Unknown, availability_note: String::new(), checked_at: String::new() }
        };

        i18n::set(i18n::Lang::En);
        app.services = services::cached(&en_cache, i18n::Lang::En);
        assert_eq!(headline(&app).as_deref(), Some("English headline"));
        app.news_loading = true; // an English refresh is running
        app.news_lang = i18n::Lang::En;
        let first_en = app.news_generation;

        // No Japanese headlines saved yet: the English ones go, none take their place,
        // and a Japanese read starts although an English one is still running.
        assert_eq!(app.update(Message::SetLanguage(i18n::Lang::Ja)).units(), 1);
        assert_eq!(i18n::current(), i18n::Lang::Ja);
        assert!(app.services.is_none());
        assert!(app.news_loading && app.news_lang == i18n::Lang::Ja);
        let first_ja = app.news_generation;

        // The English read finishing now changes nothing.
        app.worker_event(WorkerEvent::Services(first_en, i18n::Lang::En, services::Update::Headlines { items: late("Late English").news, note: String::new() }));
        let _ = app.update(Message::ServicesLoaded(first_en, i18n::Lang::En, Ok(late("Late English"))));
        assert!(app.services.is_none(), "English results are not shown in Japanese");
        assert!(app.news_loading, "the Japanese read still owns the loading state");
        // A second request while the Japanese read runs does not start another.
        assert_eq!(app.start_news().units(), 0);

        // The Japanese read's own results show and end the loading state.
        app.worker_event(WorkerEvent::Services(first_ja, i18n::Lang::Ja, services::Update::Headlines { items: late("日本語の見出し").news, note: String::new() }));
        assert_eq!(headline(&app).as_deref(), Some("日本語の見出し"));
        let _ = app.update(Message::ServicesLoaded(first_ja, i18n::Lang::Ja, Ok(late("日本語の見出し"))));
        assert!(!app.news_loading);

        // Back to English: its saved headlines return (the cache file the Japanese
        // read would write is a different one), and a fresh read starts.
        std::fs::write(&ja_cache, saved("日本語の見出し", "ff11", "ja")).unwrap();
        assert_eq!(app.update(Message::SetLanguage(i18n::Lang::En)).units(), 1);
        assert_eq!(headline(&app).as_deref(), Some("English headline"));
        // The first English request still cannot publish after switching back.
        app.worker_event(WorkerEvent::Services(first_en, i18n::Lang::En, services::Update::Headlines { items: late("Stale English").news, note: String::new() }));
        let _ = app.update(Message::ServicesLoaded(first_en, i18n::Lang::En, Ok(late("Stale English"))));
        let _ = app.update(Message::ServiceStatusLoaded(first_en, i18n::Lang::En, Ok(late("Stale status"))));
        assert_eq!(headline(&app).as_deref(), Some("English headline"));
        assert!(app.news_loading, "the current English request still owns loading");
        assert_eq!(app.update(Message::SetLanguage(i18n::Lang::Ja)).units(), 1);
        assert_eq!(headline(&app).as_deref(), Some("日本語の見出し"));
        let _ = app.update(Message::ServicesLoaded(first_ja, i18n::Lang::Ja, Err("stale failure".into())));
        assert!(app.news_loading && app.news_error.is_empty());
        // Choosing the language already showing is not a change and fetches nothing.
        let _ = app.update(Message::ServicesLoaded(app.news_generation, i18n::Lang::Ja, Err("offline".into())));
        assert_eq!(app.update(Message::SetLanguage(i18n::Lang::Ja)).units(), 0);
        assert_eq!(headline(&app).as_deref(), Some("日本語の見出し"));
        i18n::set(i18n::Lang::En);
    }

    #[test]
    fn language_setting_is_optional_and_applies_at_once() {
        // A settings file from before the language setting still loads, follows
        // Windows, and is written back without a language until one is chosen.
        let old: Settings = serde_json::from_str(r#"{"install_path":"C:\\Games\\FFXI","light_theme":true}"#).unwrap();
        assert_eq!(old.language, None);
        assert!(!serde_json::to_string(&old).unwrap().contains("language"));
        let mut app = app();
        i18n::set(i18n::Lang::En);
        let _ = app.update(Message::SetLanguage(i18n::Lang::Ja));
        assert_eq!(app.settings.language, Some(i18n::Lang::Ja));
        assert_eq!(i18n::current(), i18n::Lang::Ja);
        assert_eq!(i18n::t("Settings"), "設定");
        let saved = serde_json::to_string(&app.settings).unwrap();
        assert!(saved.contains(r#""language":"ja""#), "{saved}");
        let back: Settings = serde_json::from_str(&saved).unwrap();
        assert_eq!(back.language, Some(i18n::Lang::Ja));
        let _ = app.update(Message::SetLanguage(i18n::Lang::En));
        assert_eq!(i18n::t("Settings"), "Settings");
    }

    #[test]
    fn window_and_session_worker_take_their_language_from_the_same_saved_settings() {
        let root = std::env::temp_dir().join(format!("crystal-language-{}", backend::unique_id().unwrap()));
        let config = Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        // No settings file yet: the Windows display language.
        i18n::set(i18n::Lang::En);
        load_settings_and_language(&config).unwrap();
        assert_eq!(i18n::current(), i18n::system_default());
        // A saved choice wins, whatever Windows says.
        let mut settings = Settings::default();
        settings.language = Some(i18n::Lang::Ja);
        save_settings(&config, &settings).unwrap();
        i18n::set(i18n::Lang::En);
        assert!(load_settings_and_language(&config).is_ok());
        assert_eq!(i18n::current(), i18n::Lang::Ja);
        // A damaged file reports its error in the Windows language and falls back to it.
        std::fs::write(settings_path(&config), b"{\"install_path\":\"unfinished").unwrap();
        i18n::set(i18n::Lang::Ja);
        assert!(load_settings_and_language(&config).is_err());
        assert_eq!(i18n::current(), i18n::system_default());
        i18n::set(i18n::Lang::En);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn launcher_names_join_as_translatable_pairs() {
        i18n::set(i18n::Lang::En);
        assert_eq!(join_and(&[]), "");
        assert_eq!(join_and(&["Ashita v4"]), "Ashita v4");
        assert_eq!(join_and(&["Ashita v4", "Windower 4"]), "Ashita v4 and Windower 4");
        assert_eq!(join_and(&["the native client", "Ashita v4", "Windower 4"]), "the native client and Ashita v4 and Windower 4");
    }

    #[test]
    fn the_last_result_carries_its_tone_apart_from_its_words() {
        let mut app = app();
        app.maintenance = Some(MaintenanceKind::Check);
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Check, Err("boom".into()), None));
        assert_eq!(app.maintenance_tone, ResultTone::Stopped);
        assert!(app.maintenance_result.starts_with("Operation stopped"));
        app.maintenance = Some(MaintenanceKind::Check);
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Check, Ok("Checked.".into()), None));
        assert_eq!(app.maintenance_tone, ResultTone::Done);
    }

    #[test]
    fn unchecked_selections_read_as_unverified_not_done() {
        let mut app = app();
        app.preview = false;
        app.maintenance_tone = ResultTone::Done;
        let _ = app.update(Message::InstallPathChanged(r"C:\Elsewhere".into()));
        assert_eq!(app.maintenance_tone, ResultTone::Unverified);
        assert_eq!(app.maintenance_result, t("Installation path changed. Select this folder, then Check files."));
    }

    #[test]
    fn choosing_an_install_folder_rejects_the_previous_folders_pending_report() {
        let root = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("crystal-picker-{}", backend::unique_id().unwrap()));
        let install = root.join("game");
        std::fs::create_dir_all(&install).unwrap();
        let mut app = app();
        app.config = Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        app.preview = false;
        app.patch_loading = true;
        let old_generation = app.patch_generation;
        let old_report = app.patch.clone().unwrap();
        let _ = app.update(Message::Picked(PickTarget::GameFolder, Some(install.clone())));
        assert!(app.settings.install_selected);
        assert_eq!(app.settings.install_path, install.to_string_lossy());
        assert!(!app.patch_loading && app.patch.is_none());
        assert_ne!(app.patch_generation, old_generation);
        let _ = app.update(Message::PatchLoaded(old_generation, old_report));
        assert!(app.patch.is_none());
        assert_eq!(load_settings(&app.config).unwrap().install_path, install.to_string_lossy());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn automatic_installation_detection_saves_the_folder_and_rejects_old_version_results() {
        let root = std::env::temp_dir().join(format!("crystal-detect-{}", backend::unique_id().unwrap()));
        let install = root.join("FINAL FANTASY XI");
        std::fs::create_dir_all(&install).unwrap();
        let mut app = app();
        app.config.experiment = root.clone();
        app.preview = false;
        app.installation_loading = true;
        app.installation_generation = 7;
        app.patch_loading = true;
        let previous = app.patch.clone().unwrap();
        let previous_generation = app.patch_generation;
        assert_eq!(app.update(Message::InstallationFound(7, true, Some(install.clone()))).units(), 1);
        assert!(app.settings.install_selected && !app.installation_loading);
        assert_eq!(load_settings(&app.config).unwrap().install_path, install.to_string_lossy());
        assert!(app.patch.is_none() && app.patch_loading);
        let _ = app.update(Message::PatchLoaded(previous_generation, previous));
        assert!(app.patch.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installation_detection_cannot_replace_later_edits_or_run_in_a_preview() {
        let mut app = app();
        assert_eq!(app.update(Message::FindInstallation).units(), 0);
        assert!(!app.installation_loading);
        app.preview = false;
        let original = app.settings.install_path.clone();
        assert_eq!(app.find_installation(true).units(), 1);
        let generation = app.installation_generation;
        let _ = app.update(Message::InstallPathChanged(r"C:\Another install".into()));
        let _ = app.update(Message::InstallPathChanged(original.clone()));
        let _ = app.update(Message::InstallationFound(generation, true, Some(PathBuf::from(r"C:\Late install"))));
        assert_eq!(app.settings.install_path, original);
        assert!(!app.installation_loading && !app.settings.install_selected);
        app.maintenance = Some(MaintenanceKind::Update);
        assert_eq!(app.update(Message::FindInstallation).units(), 0);
    }

    #[test]
    fn automatic_detection_keeps_a_damaged_settings_file_unchanged() {
        let root = std::env::temp_dir().join(format!("crystal-detect-damaged-{}", backend::unique_id().unwrap()));
        let install = root.join("game");
        std::fs::create_dir_all(&install).unwrap();
        let mut app = app();
        app.config.experiment = root.clone();
        let path = settings_path(&app.config);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let damaged = b"{\"install_path\":\"unfinished";
        std::fs::write(&path, damaged).unwrap();
        app.preview = false;
        app.startup_warning = Some("Settings could not be read".into());
        app.installation_loading = true;
        app.installation_generation = 5;
        assert_eq!(app.update(Message::InstallationFound(5, true, Some(install.clone()))).units(), 1);
        assert!(app.settings.install_selected);
        assert_eq!(app.settings.install_path, install.to_string_lossy());
        assert_eq!(std::fs::read(path).unwrap(), damaged);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn successful_official_setup_rescans_installation_and_failed_setup_does_not() {
        let mut app = app();
        app.preview = false;
        app.maintenance = Some(MaintenanceKind::Setup);
        assert_eq!(app.update(Message::MaintenanceFinished(MaintenanceKind::Setup, Ok("Setup closed".into()), None)).units(), 2);
        assert!(app.installation_loading && app.viewer_loading);
        app.installation_loading = false;
        app.maintenance = Some(MaintenanceKind::Setup);
        assert_eq!(app.update(Message::MaintenanceFinished(MaintenanceKind::Setup, Err("Cancelled".into()), None)).units(), 0);
        assert!(!app.installation_loading);
    }

    #[test]
    fn fixed_sentences_follow_a_language_change() {
        // A table entry (Settings ↔ 設定) read back from Japanese into English.
        i18n::set(i18n::Lang::En);
        assert_eq!(retranslated("設定", i18n::Lang::Ja, &["Settings"]), Some("Settings"));
        assert_eq!(retranslated("Settings", i18n::Lang::Ja, &["Settings"]), None);
        assert_eq!(retranslated("Ready (3 accounts)", i18n::Lang::Ja, &["Settings"]), None);
        // The window's own lines, both ways.
        let mut app = app();
        app.maintenance_result = t(RESULT_TEXTS[0]).into();
        app.accounts[0].status = t("Client closed").into();
        let _ = app.update(Message::SetLanguage(i18n::Lang::Ja));
        assert_eq!(app.maintenance_result, i18n::t_in(i18n::Lang::Ja, RESULT_TEXTS[0]));
        assert_eq!(app.accounts[0].status, i18n::t_in(i18n::Lang::Ja, "Client closed"));
        let _ = app.update(Message::SetLanguage(i18n::Lang::En));
        assert_eq!(app.maintenance_result, RESULT_TEXTS[0]);
        assert_eq!(app.accounts[0].status, "Client closed");
    }

    #[test]
    fn an_exit_report_on_a_row_is_worded_when_shown() {
        i18n::set(i18n::Lang::En);
        let mut app = app();
        let report = launch_log::classify_exit("", Some(3), Some(Duration::from_secs(1)));
        app.accounts[0].set_exit(report.clone());
        assert!(app.accounts[0].problem);
        assert_eq!(app.accounts[0].status, report.summary, "stored in English");
        assert_eq!(app.accounts[0].status_text(), report.message());
        // A later status replaces it.
        app.accounts[0].status = "Starting sign-in…".into();
        assert_eq!(app.accounts[0].status_text(), "Starting sign-in…");
    }

    #[test]
    fn window_input_is_forwarded_at_most_every_few_seconds() {
        let start = Instant::now();
        let mut last = None;
        assert!(activity_due(&mut last, start));
        assert!(!activity_due(&mut last, start + Duration::from_secs(1)));
        assert!(!activity_due(&mut last, start + ACTIVITY_INTERVAL - Duration::from_millis(1)));
        assert!(activity_due(&mut last, start + ACTIVITY_INTERVAL));
        assert!(!activity_due(&mut last, start + ACTIVITY_INTERVAL + Duration::from_secs(4)));
    }

    #[test]
    fn status_changes_include_groups_unless_unchecked_and_refresh_is_gone() {
        let mut app = app();
        assert!(!app.friends.skip_groups, "Also set for my groups starts checked");
        let _ = app.update(Message::FriendsStatusGroups(false));
        assert!(app.friends.skip_groups);
        let _ = app.update(Message::FriendsStatusGroups(true));
        assert!(!app.friends.skip_groups);
        // Preview: settings are not changed or saved by the Auto-Away picker.
        let _ = app.update(Message::FriendsAutoAway(30));
        assert!(app.settings.account_auto_away.is_empty());
    }

    #[test]
    fn switching_friends_accounts_rejects_an_earlier_read_after_switching_back() {
        let mut app = app();
        let first = app.accounts[0].key.clone();
        let second = app.accounts[1].key.clone();
        let _ = app.update(Message::FriendsAccount(first.clone()));
        let old_generation = app.friends_generation;
        let _ = app.update(Message::FriendsAccount(second));
        let _ = app.update(Message::FriendsAccount(first.clone()));
        app.friends.loading = true;
        app.friends.connected = true;
        app.friends.pending = Some(("current request".into(), Instant::now()));
        app.friends.snapshot = Some(friends::Snapshot { fetched_at: 42, ..Default::default() });

        app.worker_event(WorkerEvent::Friends(old_generation, first.clone(), false, None));
        assert!(app.friends.loading && app.friends.connected && app.friends.pending.is_some());
        assert_eq!(app.friends.snapshot.as_ref().unwrap().fetched_at, 42);
        app.worker_event(WorkerEvent::Friends(app.friends_generation, first, true,
            Some(friends::Snapshot { fetched_at: 43, ..Default::default() })));
        assert!(!app.friends.loading);
        assert_eq!(app.friends.snapshot.as_ref().unwrap().fetched_at, 43);
    }

    #[test]
    fn removing_the_friends_account_clears_its_session_and_pending_results() {
        let mut app = app();
        app.load_friends_preview(false, false, true, false);
        let removed = app.friends.account.clone().unwrap();
        let old_generation = app.friends_generation;
        app.friends.loading = true;
        app.friends.pending = Some(("old request".into(), Instant::now()));
        let remaining = app.accounts.iter().filter(|row| row.key != removed).map(|row| row.account.clone()).collect();
        let _ = app.update(Message::AccountsSaved(Ok(remaining)));
        assert!(app.friends.account.is_none() && app.friends.snapshot.is_none());
        assert!(!app.friends.loading && !app.friends.connected);
        assert!(app.friends.pending.is_none() && app.friends.composer.is_none());
        app.worker_event(WorkerEvent::Friends(old_generation, removed, true, Some(friends::Snapshot::default())));
        assert!(app.friends.snapshot.is_none());
        let _ = app.update(Message::FriendsAccount(app.accounts[0].key.clone()));
        assert_eq!(app.friends.account.as_ref(), Some(&app.accounts[0].key));
    }

    #[test]
    fn ignored_escape_uses_back_and_closes_friends_panes_before_leaving_the_page() {
        use iced::keyboard::{self, key};
        let escape = iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key::Named::Escape),
            modified_key: keyboard::Key::Named(key::Named::Escape),
            physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::default(), text: None, repeat: false,
        });
        assert!(!matches!(user_input(escape.clone(), iced::event::Status::Captured, window::Id::unique()), Some(Message::ControllerBack)));
        let back = user_input(escape, iced::event::Status::Ignored, window::Id::unique()).unwrap();
        assert!(matches!(back, Message::ControllerBack));

        let mut app = app();
        app.show_settings = true;
        let _ = app.update(back);
        assert!(!app.show_settings);
        app.load_friends_preview(false, false, true, false);
        app.friends.remove = Some(app.friends.snapshot.as_ref().unwrap().entries[0].clone());
        let _ = app.update(Message::ControllerBack);
        assert!(app.friends.remove.is_none() && app.friends.composer.is_some());
        let _ = app.update(Message::ControllerBack);
        assert!(app.page == Page::Friends && app.friends.pane.is_none() && app.friends.composer.is_none());
        for pane in [friends::Pane::Mailbox, friends::Pane::Blocked] {
            app.friends.select(Some(pane));
            let _ = app.update(Message::ControllerBack);
            assert!(app.page == Page::Friends && app.friends.pane.is_none());
        }
        let _ = app.update(Message::ControllerBack);
        assert!(app.page == Page::Home);
    }

    #[test]
    fn refreshing_during_news_load_does_not_start_another_feed_stream() {
        let mut app = app();
        app.preview = false;
        assert_eq!(app.refresh().units(), 2);
        assert!(app.news_loading);
        assert_eq!(app.refresh().units(), 1);
        assert_eq!(app.patch_generation, 2);
    }

    #[test]
    fn main_button_tracks_update_downtime_and_running_operations() {
        let mut app = app();
        assert_eq!(app.main_action(), MainAction::Play);
        app.services.as_mut().unwrap().availability = services::Availability::Maintenance;
        assert_eq!(app.main_action(), MainAction::Maintenance);
        let _ = app.update(Message::MainAction);
        assert!(app.show_details);
        assert!(app.accounts.iter().all(|row| !row.busy && row.pid.is_none()));
        app.patch.as_mut().unwrap().state = patch::PatchState::Required;
        assert_eq!(app.main_action(), MainAction::Update, "A published patch can be installed during maintenance");
        let _ = app.update(Message::MainAction);
        assert!(app.page == Page::Maintenance);
        assert!(!app.show_details);
        assert!(app.maintenance.is_none(), "Preview mode does not execute live updates");
        app.maintenance = Some(MaintenanceKind::Update);
        assert_eq!(app.main_action(), MainAction::Working);
        app.maintenance = None;
        app.patch.as_mut().unwrap().state = patch::PatchState::Current;
        app.services.as_mut().unwrap().availability = services::Availability::Outage;
        assert_eq!(app.main_action(), MainAction::Offline);
        for availability in [services::Availability::Operational, services::Availability::PartialOutage, services::Availability::Unknown] {
            app.services.as_mut().unwrap().availability = availability;
            assert_eq!(app.main_action(), MainAction::Play);
        }
    }

    #[test]
    fn main_update_starts_the_combined_updater_and_keeps_one_operation() {
        for viewer_needs_update in [false, true] {
            let mut app = app();
            app.preview = false;
            app.accounts.clear();
            app.settings.install_path = r"C:\fixture\FINAL FANTASY XI".into();
            app.settings.install_selected = true;
            app.show_settings = true;
            app.show_details = true;
            app.patch.as_mut().unwrap().state = if viewer_needs_update {
                patch::PatchState::Current
            } else {
                patch::PatchState::Required
            };
            if viewer_needs_update {
                app.viewer = Some(viewer_report(patch::PatchState::Required));
            }

            // Creating the task must schedule the shared updater without polling
            // it here: this test must not download or alter an installation.
            assert_eq!(app.update(Message::MainAction).units(), 1);
            assert!(app.page == Page::Maintenance);
            assert!(!app.show_settings && !app.show_details);
            assert!(app.maintenance == Some(MaintenanceKind::Update));
            assert!(app.active_operation.is_some());
            assert_eq!(app.main_action(), MainAction::Working);
            assert!(app.launch_blocked());
            let generation = app.viewer_generation;

            app.page = Page::Home;
            assert_eq!(app.update(Message::MainAction).units(), 0);
            assert!(app.page == Page::Maintenance);
            assert_eq!(app.viewer_generation, generation);
            assert!(app.maintenance == Some(MaintenanceKind::Update));
        }
    }

    #[test]
    fn main_update_keeps_folder_and_session_protections() {
        let mut app = app();
        app.preview = false;
        app.patch.as_mut().unwrap().state = patch::PatchState::Required;

        assert_eq!(app.update(Message::MainAction).units(), 0);
        assert!(app.page == Page::Maintenance);
        assert!(app.maintenance.is_none() && app.active_operation.is_none());
        assert_eq!(app.status, t("Select the target folder explicitly before starting this operation."));

        app.settings.install_selected = true;
        app.vault_ready = false;
        assert_eq!(app.update(Message::MainAction).units(), 0);
        assert!(app.maintenance.is_none() && app.active_operation.is_none());
        assert_eq!(app.status, t("Wait for saved accounts and running sessions to be checked before changing game files."));

        app.vault_ready = true;
        app.accounts[0].pid = Some(42);
        assert_eq!(app.update(Message::MainAction).units(), 0);
        assert!(app.maintenance.is_none() && app.active_operation.is_none());
        assert_eq!(app.accounts[0].pid, Some(42));
        assert_eq!(app.status, t("Close all games and sign out PlayOnline sessions before updating, repairing, or running official setup. Existing sessions were kept."));
    }

    #[test]
    fn local_refresh_preserves_a_known_update_until_the_installed_version_changes() {
        let mut app = app();
        app.patch.as_mut().unwrap().state = patch::PatchState::Required;
        let mut local = app.patch.as_ref().unwrap().clone();
        local.state = patch::PatchState::Unknown;
        local.remote_version = t("Not checked").into();
        let _ = app.update(Message::PatchLoaded(app.patch_generation, local.clone()));
        assert_eq!(app.main_action(), MainAction::Update);
        local.local_version = "30260904_2".into();
        let _ = app.update(Message::PatchLoaded(app.patch_generation, local));
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Unknown);
    }

    #[test]
    fn successful_file_check_survives_version_refresh_but_not_a_changed_record() {
        let mut app = app();
        let mut checked = app.patch.as_ref().unwrap().clone();
        checked.state = patch::PatchState::Current;
        checked.files_verified = Some(true);
        app.maintenance = Some(MaintenanceKind::Check);
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Check, Ok("Files verified".into()), Some(checked.clone())));
        let mut local = checked;
        local.state = patch::PatchState::Unknown;
        local.files_verified = None;
        local.remote_version = t("Not checked").into();
        let _ = app.update(Message::PatchLoaded(app.patch_generation, local.clone()));
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Current);
        assert_eq!(app.patch.as_ref().unwrap().files_verified, Some(true));
        assert_eq!(app.maintenance_tone, ResultTone::Done);
        local.local_version = "30260904_2".into();
        let _ = app.update(Message::PatchLoaded(app.patch_generation, local));
        assert_eq!(app.patch.as_ref().unwrap().files_verified, None);
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Unknown);
    }

    #[test]
    fn cancelled_import_picker_cannot_overwrite_a_reopened_import() {
        use account_import_ui::Msg;
        let mut app = app();
        let _ = app.update(Message::AccountImport(Msg::Open));
        let _ = app.update(Message::AccountImport(Msg::Sources(1, Vec::new())));
        let _ = app.update(Message::AccountImport(Msg::Browse(false)));
        assert!(app.account_import.busy);
        let _ = app.update(Message::AccountImport(Msg::Close));
        let _ = app.update(Message::AccountImport(Msg::Open));
        let _ = app.update(Message::AccountImport(Msg::Sources(4, Vec::new())));
        let _ = app.update(Message::AccountImport(Msg::Path(r"D:\Current\creds.dat".into())));
        let _ = app.update(Message::AccountImport(Msg::Picked(2, false, Some(PathBuf::from(r"D:\Old\creds.dat")))));
        assert_eq!(app.account_import.path, r"D:\Current\creds.dat");
        assert!(!app.account_import.busy);
    }

    #[test]
    fn back_cancels_visible_close_confirmation_before_a_pending_import() {
        let mut app = app();
        app.confirm_close = true;
        app.account_import.open = true;
        let _ = app.update(Message::ControllerBack);
        assert!(!app.confirm_close);
        assert!(app.account_import.open);
        let _ = app.update(Message::ControllerBack);
        assert!(!app.account_import.open);
    }

    #[test]
    fn unreadable_or_interrupted_update_replaces_a_previous_ready_verdict() {
        let mut app = app();
        app.patch.as_mut().unwrap().files_verified = Some(true);
        let mut invalid = app.patch.as_ref().unwrap().clone();
        invalid.state = patch::PatchState::Required;
        invalid.remote_version = t("Not checked").into();
        invalid.files_verified = None;
        invalid.detail = "Interrupted update".into();
        let _ = app.update(Message::PatchLoaded(app.patch_generation, invalid));
        assert_eq!(app.main_action(), MainAction::Update);
        assert_eq!(app.patch.as_ref().unwrap().files_verified, None);
    }

    #[test]
    fn a_fresh_install_stays_on_update_and_preserves_the_server_version_on_refresh() {
        let mut app = app();
        let fresh = patch::PatchReport { state: patch::PatchState::Required,
            local_version: t("Not initialized").into(), remote_version: "30260904_1".into(),
            detail: "Initial update required".into(), source_url: None, files_verified: None };
        app.maintenance = Some(MaintenanceKind::Update);
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Update,
            Ok("0 known files updated; files preserved".into()), Some(fresh.clone())));
        assert_eq!(app.main_action(), MainAction::Update);
        assert_eq!(app.maintenance_tone, ResultTone::Unverified);
        let mut local = fresh;
        local.remote_version = t("Not checked").into();
        let _ = app.update(Message::PatchLoaded(app.patch_generation, local));
        assert_eq!(app.patch.as_ref().unwrap().remote_version, "30260904_1");
        assert_eq!(app.main_action(), MainAction::Update);
    }

    #[test]
    fn confirmed_downtime_stops_new_game_starts_without_touching_sessions() {
        let mut app = app();
        app.preview = false;
        let snapshot = app.services.as_mut().unwrap();
        snapshot.availability = services::Availability::Outage;
        snapshot.checked_at = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
        let keys = app.launch_keys(true);
        assert!(!keys.is_empty());
        app.start_mode_at(keys, false, Instant::now());
        assert!(app.show_details);
        assert!(app.accounts.iter().all(|row| !row.busy && row.pid.is_none() && row.queued_play.is_none()));
    }

    #[test]
    fn status_polling_is_background_only_and_preserves_news_and_patch_verdict() {
        let mut app = app();
        app.preview = false;
        let original_news = app.services.as_ref().unwrap().news[0].title.clone();
        let generation = app.patch_generation;
        app.refreshed = Instant::now() - Duration::from_secs(301);
        assert_eq!(app.poll_service_status().units(), 1);
        assert!(app.news_loading);
        assert_eq!(app.poll_service_status().units(), 0);
        assert_eq!(app.patch_generation, generation, "Only public HTTP status is polled");
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Current);
        let mut snapshot = app.services.as_ref().unwrap().clone();
        snapshot.news.clear();
        snapshot.availability = services::Availability::Outage;
        snapshot.checked_at = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
        let _ = app.update(Message::ServiceStatusLoaded(app.news_generation, i18n::current(), Ok(snapshot)));
        assert!(!app.news_loading);
        assert_eq!(app.services.as_ref().unwrap().news[0].title, original_news);
        assert_eq!(app.main_action(), MainAction::Offline);
        app.refreshed = Instant::now() - Duration::from_secs(61);
        assert_eq!(app.poll_service_status().units(), 1, "Downtime is followed more closely");
        let _ = app.update(Message::ServiceStatusLoaded(app.news_generation, i18n::current(), Err("status unavailable".into())));
        assert_eq!(app.main_action(), MainAction::Play);
        assert_eq!(app.services.as_ref().unwrap().news[0].title, original_news);
    }

    #[test]
    fn main_button_has_update_and_maintenance_labels_in_both_languages() {
        for lang in i18n::Lang::ALL {
            i18n::set(lang);
            let mut app = app();
            for size in [iced::Size::new(900.0, 650.0), iced::Size::new(1440.0, 900.0)] {
                app.viewport = size;
                app.patch.as_mut().unwrap().state = patch::PatchState::Required;
                let texts = privacy::testing::texts(ui::view(&app), size);
                assert!(texts.iter().any(|line| line == t("Update")), "{texts:?}");
                assert!(texts.iter().any(|line| line == t("Click Update to install official updates")), "{texts:?}");
                app.patch.as_mut().unwrap().state = patch::PatchState::Current;
                app.services.as_mut().unwrap().availability = services::Availability::Maintenance;
                let texts = privacy::testing::texts(ui::view(&app), size);
                assert!(texts.iter().any(|line| line == t("Maintenance")), "{texts:?}");
                let _ = app.update(Message::MainAction);
                let texts = privacy::testing::texts(ui::view(&app), size);
                assert!(texts.iter().any(|line| line == t("Maintenance notices")), "{texts:?}");
                app.show_details = false;
            }
        }
        i18n::set(i18n::Lang::En);
    }

    #[test]
    fn damaged_settings_are_preserved_before_saving_and_warn_at_startup() {
        let root = std::env::temp_dir().join(format!("crystal-settings-{}", backend::unique_id().unwrap()));
        let config = Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        let path = settings_path(&config);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let damaged = b"{\"install_path\":\"unfinished";
        std::fs::write(&path, damaged).unwrap();
        assert!(load_settings(&config).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
        let mut settings = Settings::default();
        settings.install_path = root.join("client").to_string_lossy().into_owned();
        save_settings(&config, &settings).unwrap();
        assert_eq!(load_settings(&config).unwrap().install_path, settings.install_path);
        let backup = std::fs::read_dir(path.parent().unwrap()).unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.file_name().unwrap().to_string_lossy().starts_with("settings.invalid-")).unwrap();
        assert_eq!(std::fs::read(backup).unwrap(), damaged);
        let oversized = vec![b'x'; 1024 * 1024 + 1];
        std::fs::write(&path, &oversized).unwrap();
        assert!(save_settings(&config, &settings).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), oversized);
        std::fs::remove_dir_all(root).unwrap();

        let mut app = app();
        app.startup_warning = Some("Damaged settings were kept unchanged.".into());
        let _ = app.update(Message::AccountsLoaded(Ok(vec![])));
        assert!(app.status.contains("Damaged settings"));
        assert!(app.startup_warning.is_none());
    }

    #[test]
    fn verbatim_prefixes_are_cleaned_from_saved_settings_and_picked_folders() {
        let mut settings = Settings::default();
        settings.install_path = r"\\?\C:\Games\SquareEnix\FINAL FANTASY XI".into();
        settings.installer_path = r"\\?\C:\Downloads\installer".into();
        settings.ashita_path = r"\\?\D:\Ashita".into();
        settings.windower_path = r"\\?\UNC\srv\share\Windower".into();
        settings.loader_path = r"\\?\C:\crystal\pol.exe".into();
        settings.plain_paths();
        assert_eq!(settings.install_path, r"C:\Games\SquareEnix\FINAL FANTASY XI");
        assert_eq!(settings.installer_path, r"C:\Downloads\installer");
        assert_eq!(settings.ashita_path, r"D:\Ashita");
        assert_eq!(settings.windower_path, r"\\srv\share\Windower");
        assert_eq!(settings.loader_path, r"C:\crystal\pol.exe");
        // Plain and empty values are left alone.
        settings.ashita_path.clear();
        settings.plain_paths();
        assert!(settings.ashita_path.is_empty());

        let dir = std::env::temp_dir();
        let chosen = selected_folder(&dir.to_string_lossy(), true).unwrap();
        assert!(!chosen.to_string_lossy().starts_with(r"\\?\"), "{}", chosen.display());
        let missing = dir.join("xi-launcher-not-yet-there").join("deeper");
        let chosen = selected_folder(&missing.to_string_lossy(), false).unwrap();
        assert!(!chosen.to_string_lossy().starts_with(r"\\?\"), "{}", chosen.display());
        assert!(chosen.ends_with("deeper"));
    }

    #[test]
    fn dialogs_open_at_the_field_or_its_nearest_existing_parent() {
        let dir = std::env::temp_dir();
        assert_eq!(dialog_start("", false), None);
        assert_eq!(dialog_start("relative\\dir", false), None);
        let nested = dir.join("xi-launcher-missing-a").join("b");
        let start = dialog_start(&nested.to_string_lossy(), false).unwrap();
        assert!(dir.starts_with(&start) || start.starts_with(&dir) || start == dir);
        // A file field opens in the file's folder.
        let exe = dir.join("pol.exe");
        assert_eq!(dialog_start(&exe.to_string_lossy(), true), dialog_start(&dir.to_string_lossy(), false));
    }

    // the layout zooms only below the 1440 px design width.
    #[test]
    fn scale_reflows_above_design_width_and_shrinks_below() {
        let mut app = app();
        for (width, expected) in [(2560.0, 1.0), (1920.0, 1.0), (1440.0, 1.0), (1080.0, 0.75), (900.0, 0.68)] {
            app.viewport = iced::Size::new(width, 900.0);
            assert!((ui::scale(&app) - expected).abs() < 1e-6, "{width}: {}", ui::scale(&app));
        }
    }
    #[test]
    fn separate_completions_preserve_other_running_account() {
        let mut app = app();
        let first = app.accounts[0].key.clone();
        let second = app.accounts[1].key.clone();
        app.accounts[0].busy = true;
        app.accounts[1].busy = true;
        app.worker_event(WorkerEvent::Complete(
            second.clone(),
            Ok(LaunchResult {
                pid: 77,
                detail: "started".into(),
                alive: Arc::new(AtomicBool::new(true)),
                game_running: Arc::new(AtomicBool::new(true)),
                log: PathBuf::new(),
                outcome: Arc::new(std::sync::Mutex::new(None)),
                timing: "Signed in 2.8s · game started 9.4s".into(),
            }),
        ));
        assert!(app.accounts[0].busy);
        assert_eq!(app.accounts[1].pid, Some(77));
        assert_eq!(app.accounts[1].timing, "Signed in 2.8s · game started 9.4s");
        assert!(app.accounts[0].timing.is_empty());
        app.worker_event(WorkerEvent::Complete(first, Err("Rejected".into())));
        assert_eq!(app.accounts[1].pid, Some(77));
        assert_eq!(app.launch_keys(false).len(), 2);
    }
    #[test]
    fn ending_ffxi_keeps_the_pol_session_and_makes_play_available() {
        let mut app = app();
        let key = app.accounts[0].key.clone();
        let game = Arc::new(AtomicBool::new(true));
        app.worker_event(WorkerEvent::Complete(key, Ok(LaunchResult {
            pid: 123, detail: "fixture".into(), alive: Arc::new(AtomicBool::new(true)),
            game_running: game.clone(), log: PathBuf::new(), outcome: Arc::new(std::sync::Mutex::new(None)), timing: String::new(),
        })));
        assert!(app.accounts[0].is_playing());
        assert!(!app.accounts[0].can_play());
        game.store(false, Ordering::Release);
        let _ = app.update(Message::Tick);
        assert_eq!(app.accounts[0].pid, Some(123));
        assert!(app.has_sessions());
        assert!(app.accounts[0].can_play());
        assert!(app.accounts[0].status.contains("Signed in to PlayOnline"));
    }
    #[test]
    fn file_changes_block_launches_but_scans_allow_them() {
        let mut app = app();
        app.maintenance = Some(MaintenanceKind::Check);
        assert_eq!(app.launchable_count(), 3);
        for kind in [
            MaintenanceKind::Update,
            MaintenanceKind::Repair,
            MaintenanceKind::Setup,
        ] {
            app.maintenance = Some(kind);
            assert_eq!(app.launchable_count(), 0);
            assert_eq!(app.selected_count(), 0);
        }
    }
    #[test]
    fn maintenance_requires_explicit_path_and_keeps_sessions() {
        let mut app = app();
        app.preview = false;
        let _ = app.start_maintenance(MaintenanceKind::Update);
        assert!(app.maintenance.is_none());
        assert!(app.status.contains("Select the target folder"));
        app.settings.install_selected = true;
        app.accounts[0].pid = Some(42);
        let _ = app.start_maintenance(MaintenanceKind::Repair);
        assert!(app.maintenance.is_none());
        assert_eq!(app.accounts[0].pid, Some(42));
        assert!(app.status.contains("Close all games"));
    }
    #[test]
    fn installer_buttons_select_their_folder_on_click_and_report_invalid_paths() {
        let root = std::env::temp_dir().join(format!("crystal-installer-click-{}", backend::unique_id().unwrap()));
        for kind in [MaintenanceKind::Download, MaintenanceKind::Extract, MaintenanceKind::Setup] {
            let mut app = app();
            app.preview = false;
            app.config.experiment = root.clone();
            app.settings.install_path = root.join("game").to_string_lossy().into_owned();
            app.settings.installer_path = root.join("installer").to_string_lossy().into_owned();
            assert!(!app.settings.installer_selected);
            // Task creation must not run the download, extractor or installer.
            assert_eq!(app.start_maintenance(kind).units(), 1);
            assert!(app.maintenance == Some(kind));
            assert!(app.settings.installer_selected);
        }
        let mut app = app();
        app.preview = false;
        app.settings.installer_path = "relative-installer".into();
        assert_eq!(app.start_maintenance(MaintenanceKind::Download).units(), 0);
        assert!(app.maintenance.is_none());
        assert!(app.status.contains("absolute folder path"));
        assert_eq!(app.installer_error.as_deref(), Some(app.status.as_str()));
        let _ = app.update(Message::InstallerPathChanged(root.join("installer").to_string_lossy().into_owned()));
        assert!(app.installer_error.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn viewer_report(state: patch::PatchState) -> viewer::Status {
        viewer::Status { folder: Some(PathBuf::from(r"C:\fixture\PlayOnlineViewer")),
            version: patch::PatchReport { state, local_version: "20070101_E".into(), remote_version: "20110829_E".into(),
                detail: "Viewer fixture".into(), source_url: None, files_verified: None }, core_error: None }
    }

    #[test]
    fn viewer_verdict_routes_to_downloads_without_replacing_the_game_verdict() {
        let mut app = app();
        app.viewer_generation = 2;
        let game = patch::PatchReport { state: patch::PatchState::Current, local_version: "30260904_1".into(),
            remote_version: "30260904_1".into(), detail: "Game fixture".into(), source_url: None, files_verified: None };
        app.patch = Some(game);
        let _ = app.update(Message::ViewerLoaded(1, viewer_report(patch::PatchState::Required)));
        assert!(app.viewer.is_none()); assert_eq!(app.main_action(), MainAction::Play);
        let _ = app.update(Message::ViewerLoaded(2, viewer_report(patch::PatchState::Required)));
        assert_eq!(app.main_action(), MainAction::Update);
        let _ = app.run_main_action();
        assert!(app.page == Page::Maintenance); assert!(app.maintenance.is_none());
        assert_eq!(app.patch.as_ref().unwrap().local_version, "30260904_1");
        app.viewer_generation = 3;
        let _ = app.update(Message::ViewerLoaded(3, viewer_report(patch::PatchState::Current)));
        app.worker_event(WorkerEvent::Viewer(2, viewer_report(patch::PatchState::Required)));
        assert_eq!(app.main_action(), MainAction::Play);
        assert_eq!(app.patch.as_ref().unwrap().state, patch::PatchState::Current);
    }

    #[test]
    fn unified_update_refuses_idle_social_owners_and_unfinished_session_discovery() {
        let mut app = app(); app.preview = false;
        app.settings.install_selected = true;
        app.accounts[0].pid = Some(42);
        app.accounts[0].game_running = Some(Arc::new(AtomicBool::new(false)));
        let _ = app.start_maintenance(MaintenanceKind::Update);
        assert!(app.maintenance.is_none()); assert_eq!(app.accounts[0].pid, Some(42));
        assert!(app.status.contains("sign out PlayOnline sessions"));
        app.accounts[0].pid = None; app.vault_ready = false;
        let _ = app.start_maintenance(MaintenanceKind::Update);
        assert!(app.maintenance.is_none()); assert!(app.status.contains("Wait for saved accounts"));
    }

    #[test]
    fn installer_steps_start_when_the_download_folder_contains_the_game_folder() {
        let root = std::env::temp_dir().join(format!("crystal-installer-parent-{}", backend::unique_id().unwrap()));
        let game = root.join("SquareEnix/FINAL FANTASY XI");
        std::fs::create_dir_all(&game).unwrap();
        for kind in [MaintenanceKind::Download, MaintenanceKind::Extract, MaintenanceKind::Setup] {
            let mut app = app();
            app.preview = false;
            app.config.experiment = root.clone();
            app.settings.install_path = game.to_string_lossy().into_owned();
            app.settings.installer_path = game.parent().unwrap().to_string_lossy().into_owned();
            app.settings.install_selected = true;
            // This is the user's actual parent/child layout. A click must schedule
            // the operation; checking a path must not execute external setup itself.
            assert_eq!(app.start_maintenance(kind).units(), 1);
            assert!(app.maintenance == Some(kind));
            assert!(app.settings.installer_selected && app.installer_error.is_none());
        }
        let mut app = app();
        app.preview = false;
        app.config.experiment = root.clone();
        app.settings.install_path = game.to_string_lossy().into_owned();
        app.settings.installer_path = game.parent().unwrap().to_string_lossy().into_owned();
        app.select_path(false);
        assert!(app.settings.install_selected, "selecting the game folder must also accept its installer parent");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installer_failure_is_visible_until_the_next_folder_or_attempt() {
        let mut app = app();
        app.maintenance = Some(MaintenanceKind::Download);
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Download, Err("Metadata request failed".into()), None));
        assert_eq!(app.installer_error.as_deref(), Some("Metadata request failed"));
        let _ = app.update(Message::InstallerPathChanged(r"C:\Installer".into()));
        assert!(app.installer_error.is_none());
        app.maintenance = Some(MaintenanceKind::Download);
        app.installer_error = Some("Previous failure".into());
        let _ = app.update(Message::MaintenanceFinished(MaintenanceKind::Download, Ok("Downloaded".into()), None));
        assert!(app.installer_error.is_none());
    }

    #[test]
    fn disabled_private_client_cannot_launch_and_keeps_the_saved_profile() {
        let mut app = app();
        app.preview = false;
        app.accounts[0].account.mode = AccountMode::Private;
        let key = app.accounts[0].key.clone();
        assert!(!app.launch_keys(false).contains(&key));
        app.start(vec![key.clone()]);
        assert!(!app.accounts[0].busy);
        assert!(app.accounts[0].status.contains("temporarily unavailable"));
        assert_eq!(app.accounts[0].key, key);
        assert_eq!(app.accounts.len(), 3);
    }

    #[test]
    fn legacy_native_default_loads_as_an_available_retail_launcher() {
        let root = std::env::temp_dir().join(format!("crystal-retail-default-{}", backend::unique_id().unwrap()));
        let config = Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        let mut settings = Settings::default();
        settings.launch_with = ui::LaunchWith::Native;
        save_settings(&config, &settings).unwrap();
        assert_eq!(load_settings(&config).unwrap().launch_with, ui::LaunchWith::Windower);
        let account = blank_account();
        assert_eq!(load_settings(&config).unwrap().launch_plan(&account).unwrap().tool, pol_less::Tool::Windower);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn operation_close_request_keeps_launcher_open() {
        let mut app = app();
        app.maintenance = Some(MaintenanceKind::Update);
        let _ = app.update(Message::CloseRequested);
        assert!(app.page == Page::Maintenance);
        assert!(!app.confirm_close);
        assert!(app.status.contains("before closing"));
    }

    #[test]
    fn launcher_update_handshake_cannot_start_new_work_or_change_settings() {
        let mut app = app();
        app.self_update = self_update::State::Applying;
        let _ = app.update(Message::Navigate(Page::Accounts));
        let _ = app.update(Message::NewAccount);
        let _ = app.update(Message::MainAction);
        let _ = app.update(Message::SelfUpdate(self_update::Msg::AutoCheck(false)));
        assert!(app.page == Page::Home);
        assert!(app.editor.is_none());
        assert!(app.maintenance.is_none());
        assert!(app.settings.launcher_auto_updates);
        assert!(app.launch_blocked());
        let _ = app.update(Message::CloseRequested);
        assert!(app.status.contains("update helper"));
    }

    #[test]
    fn launcher_restart_waits_for_saves_and_requests_but_allows_running_games() {
        let mut app = app();
        for account in &mut app.accounts {
            account.busy = false;
            account.batch_pending = false;
            account.pid = Some(1234);
        }
        assert!(app.can_restart_launcher());
        app.vault_saving = true;
        assert!(!app.can_restart_launcher());
        app.vault_saving = false;
        app.friends.pending = Some(("pending-request".into(), Instant::now()));
        assert!(!app.can_restart_launcher());
        app.friends.pending = None;
        app.accounts[0].batch_pending = true;
        assert!(!app.can_restart_launcher());
        app.accounts[0].batch_pending = false;
        assert!(app.can_restart_launcher());
    }
    #[test]
    fn game_changes_wait_for_accounts_to_be_loaded_after_reopening() {
        let mut app = app();
        app.preview = false;
        app.accounts.clear();
        app.settings.install_selected = true;
        app.vault_ready = false;
        let _ = app.start_maintenance(MaintenanceKind::Update);
        assert!(app.maintenance.is_none());
        assert!(app.status.contains("running sessions"));
    }

    #[test]
    fn dependency_setup_keeps_the_window_open_and_account_saves_keep_the_editor() {
        let mut app = app();
        let component = prereqs::Component::DirectPlay;
        let _ = prereqs::update(&mut app.prereqs, prereqs::Msg::Ask(component));
        let _ = prereqs::update(&mut app.prereqs, prereqs::Msg::Install(component));
        assert!(app.prereqs.busy());
        let _ = app.update(Message::CloseRequested);
        assert!(app.page == Page::Settings);
        assert!(app.status.contains("Windows component"));
        let original = app.accounts[0].key.clone();
        let other = app.accounts[1].key.clone();
        let _ = app.update(Message::EditAccount(original.clone()));
        app.vault_saving = true;
        let _ = app.update(Message::EditAccount(other));
        assert_eq!(app.editor.as_ref().unwrap().key.as_ref(), Some(&original));
    }
    #[test]
    fn canonical_login_key_prevents_case_alias_duplicate() {
        let mut a = blank_account();
        a.pol_id = "abc12345".into();
        let mut b = a.clone();
        b.label = "another name".into();
        b.pol_id = "ABC12345".into();
        assert_eq!(account_key(&a), account_key(&b));
        a.mode = AccountMode::Private;
        b.mode = AccountMode::Private;
        a.server = "Example.COM".into();
        b.server = "example.com".into();
        b.pol_id = a.pol_id.clone();
        assert_eq!(account_key(&a), account_key(&b));
    }

    #[test]
    fn coordinated_selection_includes_warm_and_signing_in_accounts_once() {
        let mut app = app();
        let warm = app.accounts[0].key.clone();
        let signing_in = app.accounts[1].key.clone();
        let running = app.accounts[2].key.clone();
        app.accounts[0].pid = Some(77);
        app.accounts[0].game_running = Some(Arc::new(AtomicBool::new(false)));
        app.accounts[1].busy = true;
        app.accounts[1].preparing_session = true;
        app.accounts[2].pid = Some(88);
        app.accounts[2].game_running = Some(Arc::new(AtomicBool::new(true)));
        assert_eq!(app.coordinated_keys(&[warm.clone(), signing_in.clone(), warm.clone(), running]), vec![warm.clone(), signing_in]);
        app.accounts[1].batch_pending = true;
        assert_eq!(app.coordinated_keys(&app.launch_keys(false)), vec![warm]);
    }

    #[test]
    fn coordinated_play_before_worker_publication_keeps_both_original_signins() {
        let mut app = app();
        app.preview = false;
        app.config.experiment = std::env::temp_dir().join(format!("crystal-unpublished-batch-{}", std::process::id()));
        assert!(!app.config.experiment.exists());
        let keys: Vec<_> = app.accounts[..2].iter_mut().map(|row| {
            row.busy = true;
            row.preparing_session = true;
            row.key.clone()
        }).collect();
        let clicked = Instant::now() - Duration::from_secs(2);
        app.start_mode_at(keys, false, clicked);
        app.start_queued_plays();
        for row in &app.accounts[..2] {
            assert_eq!(row.queued_play, Some(clicked));
            assert!(row.busy && row.preparing_session && !row.batch_pending);
            assert!(row.pid.is_none() && !row.problem);
        }
        assert!(app.receiver.try_recv().is_err(), "keep both startup workers; never start a second sign-in");
    }

    #[test]
    fn coordinated_play_ignores_late_startup_results_and_keeps_other_game() {
        let mut app = app();
        let key = app.accounts[0].key.clone();
        app.accounts[0].launch_generation = 2;
        app.accounts[0].batch_pending = true;
        app.accounts[0].batch_requested = true;
        app.accounts[0].busy = true;
        app.accounts[0].status = "Waiting for the other account".into();
        app.accounts[1].pid = Some(88);
        app.accounts[1].game_running = Some(Arc::new(AtomicBool::new(true)));
        app.worker_event(WorkerEvent::Progress(key.clone(), "Old startup progress".into()).for_launch(&key, 1));
        app.worker_event(WorkerEvent::Complete(key.clone(), Err("Old startup result".into())).for_launch(&key, 1));
        let row = &app.accounts[0];
        assert!(row.batch_pending && row.busy && row.game_start_pending());
        assert!(!row.can_play());
        assert_eq!(row.status, "Waiting for the other account");
        app.worker_event(WorkerEvent::Complete(key.clone(), Ok(LaunchResult {
            pid: 77, detail: "started together".into(), alive: Arc::new(AtomicBool::new(true)),
            game_running: Arc::new(AtomicBool::new(true)), log: PathBuf::new(),
            outcome: Arc::new(std::sync::Mutex::new(None)), timing: String::new(),
        })).for_launch(&key, 2));
        assert!(!app.accounts[0].batch_pending && !app.accounts[0].busy);
        assert_eq!(app.accounts[0].pid, Some(77));
        assert_eq!(app.accounts[1].pid, Some(88));
        assert!(app.accounts[1].is_playing());
    }

    #[test]
    fn failed_coordinated_play_keeps_prepared_session_and_error() {
        let mut app = app();
        let key = app.accounts[0].key.clone();
        app.accounts[0].batch_pending = true;
        app.accounts[0].batch_requested = true;
        app.accounts[0].busy = true;
        app.worker_event(WorkerEvent::Complete(key.clone(), Ok(LaunchResult {
            pid: 77, detail: "signed in".into(), alive: Arc::new(AtomicBool::new(true)),
            game_running: Arc::new(AtomicBool::new(false)), log: PathBuf::from("batch.log"),
            outcome: Arc::new(std::sync::Mutex::new(None)), timing: String::new(),
        })));
        app.worker_event(WorkerEvent::Complete(key, Err("Another account needs an update".into())));
        let row = &app.accounts[0];
        assert!(!row.batch_pending && !row.busy && row.can_play());
        assert_eq!(row.pid, Some(77));
        assert_eq!(row.log, Some(PathBuf::from("batch.log")));
        assert!(row.problem);
        assert_eq!(row.status, "Another account needs an update");
        assert!(app.has_sessions(), "a prepared signed-in session still blocks file changes");
    }

    #[test]
    fn coordinated_play_stays_pending_when_an_old_host_exits() {
        let mut app = app();
        app.accounts[0].batch_pending = true;
        app.accounts[0].batch_requested = true;
        app.accounts[0].busy = true;
        app.accounts[0].pid = Some(77);
        app.accounts[0].alive = Some(Arc::new(AtomicBool::new(false)));
        let _ = app.update(Message::Tick);
        let row = &app.accounts[0];
        assert!(row.batch_pending && row.busy && row.game_start_pending());
        assert!(!row.can_play());
        assert!(app.has_sessions());
    }

    #[test]
    fn native_preparation_does_not_finish_batch_timing_before_release() {
        let mut app = app();
        let row = &mut app.accounts[0];
        row.pid = Some(77);
        row.alive = Some(Arc::new(AtomicBool::new(true)));
        row.game_running = Some(Arc::new(AtomicBool::new(true)));
        row.batch_pending = true;
        row.batch_requested = true;
        row.busy = true;
        row.play_timing = Some(launch_timing::Timeline::begin_at(Instant::now()));
        row.status = "Waiting for the other account".into();
        let _ = app.update(Message::Tick);
        assert!(app.accounts[0].play_timing.is_some());
        assert!(app.accounts[0].busy);
        assert_eq!(app.accounts[0].status, "Waiting for the other account");
        app.accounts[0].batch_requested = false;
        app.accounts[0].batch_pending = false;
        let _ = app.update(Message::Tick);
        assert!(app.accounts[0].play_timing.is_none());
        assert!(!app.accounts[0].busy);
        assert!(!app.accounts[0].timing.is_empty());
    }

    #[test]
    fn reconnected_batch_stops_blocking_play_after_ownership_ends() {
        let mut app = app();
        app.accounts[0].pid = Some(77);
        app.accounts[0].alive = Some(Arc::new(AtomicBool::new(true)));
        app.accounts[0].game_running = Some(Arc::new(AtomicBool::new(false)));
        app.accounts[0].batch_pending = true;
        app.accounts[0].batch_requested = false;
        assert!(!app.accounts[0].can_play());
        let _ = app.update(Message::Tick);
        assert!(!app.accounts[0].batch_pending);
        assert!(app.accounts[0].can_play());
        assert_eq!(app.accounts[0].pid, Some(77));
    }

    #[test]
    fn background_signin_accepts_one_play_and_keeps_its_click_time() {
        let mut app = app();
        app.preview = false;
        let row = &mut app.accounts[0];
        row.busy = true;
        row.preparing_session = true;
        let key = row.key.clone();
        assert!(row.can_play(), "startup sign-in must not disable Play");
        assert!(!row.game_start_pending(), "background preparation is not a requested game launch");
        assert!(app.launch_keys(true).contains(&key));
        let clicked = Instant::now() - Duration::from_secs(2);
        app.start_mode_at(vec![key.clone()], false, clicked);
        let row = &app.accounts[0];
        assert_eq!(row.queued_play, Some(clicked));
        assert!(row.game_start_pending());
        assert!(!row.can_play(), "a second click must not queue another launch");
        assert!(row.pid.is_none());
        assert!(!app.launch_keys(true).contains(&key));
        app.start(vec![key.clone()]);
        assert_eq!(app.accounts[0].queued_play, Some(clicked));
        app.start_queued_plays();
        assert_eq!(app.accounts[0].queued_play, Some(clicked), "an unpublished or legacy worker must keep the original queue");
        assert!(!app.accounts[0].batch_pending);
        app.worker_event(WorkerEvent::Progress(key, "Signing in to PlayOnline…".into()));
        assert!(app.accounts[0].status.contains("FFXI will start when sign-in finishes."));
        assert!(app.receiver.try_recv().is_err(), "queued Play reuses the existing worker");
    }

    #[test]
    fn failed_background_signin_clears_queued_play_without_retrying() {
        let mut app = app();
        app.preview = false;
        let row = &mut app.accounts[0];
        row.busy = true;
        row.preparing_session = true;
        row.queued_play = Some(Instant::now());
        let key = row.key.clone();
        app.worker_event(WorkerEvent::Complete(key, Err("Sign-in failed".into())));
        let row = &app.accounts[0];
        assert!(!row.busy && !row.preparing_session);
        assert!(row.queued_play.is_none() && row.pid.is_none());
        assert!(row.problem && row.can_play());
        assert_eq!(row.status, "Sign-in failed");
        assert!(app.receiver.try_recv().is_err());
    }

    #[test]
    fn queued_play_waits_for_vault_and_file_operations_then_uses_signed_in_host() {
        let mut app = app();
        app.preview = false;
        // An explicit missing install keeps this test entirely offline and
        // exercises the normal launch validation before any start request.
        let missing = std::env::temp_dir().join(format!("crystal-no-queued-game-{}", std::process::id()));
        assert!(!missing.exists());
        app.settings.install_selected = true;
        app.settings.install_path = missing.to_string_lossy().into_owned();
        app.settings.account_installs.clear();
        app.vault_saving = true;
        let row = &mut app.accounts[0];
        row.busy = true;
        row.preparing_session = true;
        let clicked = Instant::now() - Duration::from_secs(2);
        row.queued_play = Some(clicked);
        let key = row.key.clone();
        app.worker_event(WorkerEvent::Complete(key, Ok(LaunchResult {
            pid: 123, detail: "fixture".into(), alive: Arc::new(AtomicBool::new(true)),
            game_running: Arc::new(AtomicBool::new(false)), log: PathBuf::new(),
            outcome: Arc::new(std::sync::Mutex::new(None)), timing: String::new(),
        })));
        assert_eq!(app.accounts[0].queued_play, Some(clicked));
        assert!(!app.accounts[0].can_play());
        app.vault_saving = false;
        app.vault_ready = false;
        app.start_queued_plays();
        assert_eq!(app.accounts[0].queued_play, Some(clicked));
        app.vault_ready = true;
        app.maintenance = Some(MaintenanceKind::Repair);
        app.start_queued_plays();
        assert_eq!(app.accounts[0].queued_play, Some(clicked));
        app.maintenance = None;
        app.start_queued_plays();
        let row = &app.accounts[0];
        assert_eq!(row.pid, Some(123), "keep the original signed-in host");
        assert!(row.queued_play.is_none());
        assert!(row.problem && row.status.contains("FFXi.dll"), "normal launch validation still runs");
        assert!(app.receiver.try_recv().is_err(), "never start a second sign-in worker");
    }

    #[test]
    fn queued_play_remains_pending_during_confirmed_outage() {
        let mut app = app();
        app.preview = false;
        let clicked = Instant::now() - Duration::from_secs(2);
        let row = &mut app.accounts[0];
        row.pid = Some(123);
        row.alive = Some(Arc::new(AtomicBool::new(true)));
        row.game_running = Some(Arc::new(AtomicBool::new(false)));
        row.queued_play = Some(clicked);
        let snapshot = app.services.as_mut().unwrap();
        snapshot.availability = services::Availability::Outage;
        snapshot.checked_at = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
        snapshot.notices.clear();
        assert_eq!(app.service_availability(), services::Availability::Outage);
        app.start_queued_plays();
        assert_eq!(app.accounts[0].queued_play, Some(clicked));
        assert!(!app.accounts[0].busy && !app.accounts[0].batch_pending);
        assert!(app.receiver.try_recv().is_err());
    }

    #[test]
    fn startup_signin_defaults_off_and_selection_runs_only_once_after_vault_load() {
        let older: Settings = serde_json::from_str(r#"{"install_path":"C:\\Games\\FFXI"}"#).unwrap();
        assert!(older.account_startup_signin.is_empty());
        let mut app = app();
        app.preview = false;
        app.startup_signin_pending = true;
        app.vault_ready = false;
        let first = app.accounts[0].key.clone();
        app.settings.account_startup_signin = app.accounts.iter().map(|r| r.key.clone()).collect();
        app.accounts[1].busy = true; // Reconnecting to an existing background worker.
        app.accounts[2].account.mode = AccountMode::Private;
        assert!(app.take_startup_signins().is_empty());
        assert!(app.startup_signin_pending);
        app.vault_ready = true;
        app.installation_loading = true;
        assert!(app.take_startup_signins().is_empty());
        assert!(app.startup_signin_pending);
        app.installation_loading = false;
        assert_eq!(app.take_startup_signins(), vec![first]);
        app.accounts[1].busy = false;
        assert!(app.take_startup_signins().is_empty(), "refresh and client exit must not sign in again");
    }

    #[test]
    fn startup_signin_skips_existing_sessions_previews_and_file_operations() {
        let mut app = app();
        app.preview = false;
        app.startup_signin_pending = true;
        app.settings.account_startup_signin.insert(app.accounts[0].key.clone());
        app.accounts[0].pid = Some(99);
        app.accounts[0].game_running = Some(Arc::new(AtomicBool::new(false)));
        assert!(app.take_startup_signins().is_empty());
        // A social-only action on that session must not even schedule StartGame.
        app.start_mode(vec![app.accounts[0].key.clone()], true);
        assert!(!app.accounts[0].busy);
        assert!(app.accounts[0].play_timing.is_none());
        assert!(app.receiver.try_recv().is_err());
        app.accounts[0].pid = None;
        app.startup_signin_pending = true;
        app.preview = true;
        assert!(app.take_startup_signins().is_empty());
        app.preview = false;
        app.startup_signin_pending = true;
        app.maintenance = Some(MaintenanceKind::Repair);
        assert!(app.take_startup_signins().is_empty());
    }

    #[test]
    fn startup_signin_saved_in_settings_follows_edited_account_and_deleted_accounts() {
        let mut app = app();
        let old_key = app.accounts[0].key.clone();
        let other_key = app.accounts[1].key.clone();
        app.settings.account_startup_signin.extend([old_key.clone(), other_key.clone()]);
        let _ = app.update(Message::EditAccount(old_key.clone()));
        assert!(app.editor.as_ref().unwrap().startup_signin);
        let _ = app.update(Message::EditorChanged(Field::PolId, "NEW12345".into()));
        let edited = app.editor.as_ref().unwrap().account.clone();
        let new_key = account_key(&edited);
        let _ = app.update(Message::AccountsSaved(Ok(vec![edited])));
        assert_eq!(app.settings.account_startup_signin, BTreeSet::from([new_key.clone()]));
        let bytes = serde_json::to_vec(&app.settings).unwrap();
        let reloaded: Settings = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reloaded.account_startup_signin, app.settings.account_startup_signin);
        let _ = app.update(Message::EditAccount(new_key));
        let _ = app.update(Message::EditorStartupSignin(false));
        let saved = app.accounts.iter().map(|r| r.account.clone()).collect();
        let _ = app.update(Message::AccountsSaved(Ok(saved)));
        assert!(app.settings.account_startup_signin.is_empty());
    }

    #[test]
    fn ready_session_timing_waits_for_game_start_and_is_cleared_on_refusal() {
        let mut app = app();
        let game = Arc::new(AtomicBool::new(false));
        let row = &mut app.accounts[0];
        row.pid = Some(99);
        row.alive = Some(Arc::new(AtomicBool::new(true)));
        row.game_running = Some(game.clone());
        row.busy = true;
        row.play_timing = Some(launch_timing::Timeline::begin_at(Instant::now() - Duration::from_secs(1)));
        let _ = app.update(Message::Tick);
        assert!(app.accounts[0].play_timing.is_some());
        game.store(true, Ordering::Release);
        let _ = app.update(Message::Tick);
        assert!(app.accounts[0].play_timing.is_none());
        assert!(!app.accounts[0].busy);
        assert!(app.accounts[0].timing.starts_with("Game started "));
        assert!(!app.accounts[0].timing.contains("Signed in"), "do not reuse the earlier sign-in timing");
        let row = &mut app.accounts[0];
        row.play_timing = Some(launch_timing::Timeline::begin());
        let key = row.key.clone();
        let report = patch::PatchReport { state: patch::PatchState::Unknown, local_version: String::new(), remote_version: String::new(), detail: String::new(), source_url: None, files_verified: None };
        app.worker_event(WorkerEvent::StartGame(key, report, Err("Version check refused".into())));
        assert!(app.accounts[0].play_timing.is_none());
        assert!(app.accounts[0].problem);
    }

    #[test]
    fn vanilla_selection_in_home_and_the_editor_uses_the_same_direct_launch_plan() {
        let mut app = app();
        let key = app.accounts[0].key.clone();
        let other = app.accounts[1].account.launch_with;
        let _ = app.set_account_launcher(&key, ui::LaunchWith::Vanilla);
        let account = &app.accounts[0].account;
        assert_eq!(app.settings.launch_plan(account).unwrap().tool, pol_less::Tool::Vanilla);
        assert_eq!(app.accounts[1].account.launch_with, other);
        app.editor = Some(Editor { key: Some(key), account: account.clone(),
            game_install: String::new(), dat_overlays: Vec::new(), startup_signin: false, auto_login: pol_less::AutoLogin::default() });
        let _ = app.update(Message::EditorLaunchWith(Some(ui::LaunchWith::Windower)));
        assert_eq!(app.editor.as_ref().unwrap().account.launch_with, Some(ui::LaunchWith::Windower));
        let _ = app.update(Message::EditorLaunchWith(Some(ui::LaunchWith::Vanilla)));
        assert_eq!(app.settings.launch_plan(&app.editor.as_ref().unwrap().account).unwrap().tool, pol_less::Tool::Vanilla);
        assert_eq!(app.accounts[0].account.launch_with, Some(ui::LaunchWith::Vanilla));
    }

    #[test]
    fn save_failure_keeps_account_state_and_editor() {
        let mut app = app();
        app.editor = Some(Editor {
            key: None,
            account: blank_account(),
            game_install: String::new(),
            dat_overlays: Vec::new(),
            startup_signin: false, auto_login: pol_less::AutoLogin::default(),
        });
        app.vault_saving = true;
        let _ = app.update(Message::AccountsSaved(Err("Save failed".into())));
        assert_eq!(app.accounts.len(), 3);
        assert!(app.editor.is_some());
        assert!(!app.vault_saving);
    }

    #[cfg(windows)]
    #[test]
    fn dragging_accounts_persists_order_without_rewriting_credentials_or_live_state() {
        let mut app = app();
        let root = std::env::temp_dir().join(format!("crystal-account-order-{}", backend::unique_id().unwrap()));
        app.config.experiment = root.clone();
        app.preview = false;
        app.page = Page::Accounts;
        let values: Vec<_> = app.accounts.iter().map(|row| row.account.clone()).collect();
        backend::save_accounts(&app.config, &values).unwrap();
        let vault = root.join("iced-launcher/accounts.dpapi");
        let ciphertext = std::fs::read(&vault).unwrap();
        let original: Vec<_> = app.accounts.iter().map(|row| row.key.clone()).collect();
        let alive = Arc::new(AtomicBool::new(true));
        let running = Arc::new(AtomicBool::new(true));
        app.accounts[0].pid = Some(77);
        app.accounts[0].alive = Some(alive.clone());
        app.accounts[0].game_running = Some(running.clone());
        app.accounts[0].launch_generation = 19;
        app.accounts[0].otp = "123456".into();
        let _ = app.update(Message::AccountDragStart(original[0].clone()));
        let _ = app.update(Message::AccountDragOver(original[2].clone()));
        let _ = app.update(Message::AccountDragFinish);
        let expected = vec![original[1].clone(), original[2].clone(), original[0].clone()];
        assert_eq!(app.accounts.iter().map(|row| row.key.clone()).collect::<Vec<_>>(), expected);
        let moved = &app.accounts[2];
        assert!(Arc::ptr_eq(moved.alive.as_ref().unwrap(), &alive));
        assert!(Arc::ptr_eq(moved.game_running.as_ref().unwrap(), &running));
        assert_eq!((moved.pid, moved.launch_generation, moved.otp.as_str()), (Some(77), 19, "123456"));
        assert_eq!(std::fs::read(&vault).unwrap(), ciphertext);
        assert_eq!(load_settings(&app.config).unwrap().account_order, expected);
        let mut reopened = self::app();
        reopened.config.experiment = root.clone();
        reopened.settings = load_settings(&app.config).unwrap();
        reopened.replace_accounts(backend::load_accounts(&app.config).unwrap());
        assert_eq!(reopened.accounts.iter().map(|row| row.key.clone()).collect::<Vec<_>>(), expected);
        assert_eq!(std::fs::read(vault).unwrap(), ciphertext);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn canceled_or_failed_account_drags_restore_the_previous_order() {
        let mut app = app();
        app.page = Page::Accounts;
        let original: Vec<_> = app.accounts.iter().map(|row| row.key.clone()).collect();
        let _ = app.update(Message::AccountDragStart(original[0].clone()));
        let _ = app.update(Message::AccountDragOver(original[2].clone()));
        let _ = app.update(Message::ControllerBack);
        assert!(app.account_drag.source.is_none());
        assert_eq!(app.accounts.iter().map(|row| row.key.clone()).collect::<Vec<_>>(), original);
        let root = std::env::temp_dir().join(format!("crystal-order-failure-{}", backend::unique_id().unwrap()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("iced-launcher"), b"blocked settings directory").unwrap();
        app.config.experiment = root.clone();
        app.preview = false;
        let _ = app.update(Message::AccountDragStart(original[0].clone()));
        let _ = app.update(Message::AccountDragOver(original[2].clone()));
        let _ = app.update(Message::AccountDragFinish);
        assert_eq!(app.accounts.iter().map(|row| row.key.clone()).collect::<Vec<_>>(), original);
        assert!(app.settings.account_order.is_empty());
        assert!(app.status.starts_with("Account order could not be saved:"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_edits_during_play_keep_the_original_session_and_lock_its_login_identity() {
        let mut app = app();
        let keys: Vec<_> = app.accounts.iter().map(|row| row.key.clone()).collect();
        let alive = Arc::new(AtomicBool::new(true));
        let running = Arc::new(AtomicBool::new(true));
        app.accounts[0].pid = Some(77);
        app.accounts[0].alive = Some(alive.clone());
        app.accounts[0].game_running = Some(running.clone());
        app.accounts[0].launch_generation = 22;
        let _ = app.update(Message::EditAccount(keys[0].clone()));
        assert!(app.editor.is_some());
        let _ = app.update(Message::EditorChanged(Field::PolId, "DIFF1234".into()));
        assert_eq!(account_key(&app.editor.as_ref().unwrap().account), keys[0]);
        let _ = app.update(Message::EditorProfile(Some("Next launch".into())));
        let _ = app.update(Message::EditorChanged(Field::Label, "Updated label".into()));
        assert_ne!(app.accounts[0].account.label, "Updated label");
        let edited = app.editor.as_ref().unwrap().account.clone();
        let mut values: Vec<_> = app.accounts.iter().map(|row| row.account.clone()).collect();
        values[0] = edited;
        let _ = app.update(Message::AccountsSaved(Ok(values)));
        assert_eq!(app.accounts.iter().map(|row| row.key.clone()).collect::<Vec<_>>(), keys);
        let row = &app.accounts[0];
        assert_eq!(row.account.windower_profile.as_deref(), Some("Next launch"));
        assert_eq!(row.account.label, "Updated label");
        assert_eq!((row.pid, row.launch_generation), (Some(77), 22));
        assert!(Arc::ptr_eq(row.alive.as_ref().unwrap(), &alive));
        assert!(Arc::ptr_eq(row.game_running.as_ref().unwrap(), &running));
        assert!(row.is_playing());
    }
    #[test]
    fn saved_account_folders_are_isolated_and_loading_accounts_does_not_replace_them() {
        let mut app = app();
        let first = app.accounts[0].account.clone();
        let second = app.accounts[1].account.clone();
        let first_key = account_key(&first);
        let path = r"C:\Games\Private Server";
        app.settings.install_path = r"C:\Games\Retail".into();
        app.settings.install_selected = true;
        app.editor = Some(Editor { key: Some(first_key.clone()), account: first.clone(), game_install: path.into(), dat_overlays: vec![r"C:\Mods\High Priority".into(), r"C:\Mods\Fallback".into()], startup_signin: false, auto_login: pol_less::AutoLogin::default() });
        let values: Vec<_> = app.accounts.iter().map(|r| r.account.clone()).collect();
        let _ = app.update(Message::AccountsSaved(Ok(values.clone())));
        let config = app.settings.config_for(&app.config, &first);
        assert_eq!(config.game_directory(), PathBuf::from(path));
        assert!(config.dat_overlays.is_empty(), "retail injectors do not use the disabled native overlays");
        assert_eq!(app.settings.account_overlays[&first_key], [r"C:\Mods\High Priority", r"C:\Mods\Fallback"], "saved legacy overlay paths are kept");
        let other = app.settings.config_for(&app.config, &second);
        assert_eq!(other.game_directory(), PathBuf::from(r"C:\Games\Retail"));
        assert!(other.dat_overlays.is_empty());
        assert_eq!(other.experiment, config.experiment, "account storage is independent of the chosen game installation");
        let persisted = serde_json::to_vec(&app.settings).unwrap();
        let reloaded: Settings = serde_json::from_slice(&persisted).unwrap();
        assert_eq!(reloaded.config_for(&app.config, &first).game_directory(), PathBuf::from(path));
        let _ = app.update(Message::AccountsLoaded(Ok(values)));
        assert_eq!(app.settings.account_installs[&first_key], path);
        let _ = app.update(Message::EditAccount(first_key.clone()));
        let _ = app.update(Message::AccountInstallChanged(String::new()));
        let values = app.accounts.iter().map(|r| r.account.clone()).collect();
        let _ = app.update(Message::AccountsSaved(Ok(values)));
        assert_eq!(app.settings.config_for(&app.config, &first).game_directory(), PathBuf::from(r"C:\Games\Retail"));
    }

    #[test]
    fn account_key_edits_migrate_preferences_and_deletion_removes_them() {
        let mut app = app();
        let previous = app.accounts[0].key.clone();
        app.settings.selected_accounts = Some(BTreeSet::from([previous.clone(), "obsolete".into()]));
        app.settings.account_auto_away.insert(previous.clone(), 30);
        app.settings.account_auto_away.insert("obsolete".into(), 60);
        app.settings.account_installs.insert(previous.clone(), r"C:\Games\Account".into());
        app.settings.account_overlays.insert(previous.clone(), vec![r"C:\Mods\Account".into()]);
        app.settings.account_startup_signin.insert(previous.clone());
        app.settings.account_order = vec![previous.clone(), app.accounts[1].key.clone(), "obsolete".into()];
        let auto_login = pol_less::AutoLogin { enabled: true, character_slot: 3 };
        app.settings.account_auto_login.insert(previous.clone(), auto_login);
        let _ = app.update(Message::EditAccount(previous.clone()));
        let _ = app.update(Message::EditorChanged(Field::PolId, "XYZ99999".into()));
        let edited = app.editor.as_ref().unwrap().account.clone();
        let key = account_key(&edited);
        let mut values: Vec<_> = app.accounts.iter().filter(|row| row.key != previous).map(|row| row.account.clone()).collect();
        values.push(edited);
        let _ = app.update(Message::AccountsSaved(Ok(values)));
        assert_eq!(app.settings.selected_accounts, Some(BTreeSet::from([key.clone()])));
        assert_eq!(app.settings.account_auto_away, HashMap::from([(key.clone(), 30)]));
        assert_eq!(app.settings.account_auto_login, HashMap::from([(key.clone(), auto_login)]));
        let reloaded: Settings = serde_json::from_slice(&serde_json::to_vec(&app.settings).unwrap()).unwrap();
        assert_eq!(reloaded.account_auto_login, app.settings.account_auto_login);
        assert_eq!(app.settings.account_installs.get(&key).map(String::as_str), Some(r"C:\Games\Account"));
        assert!(app.settings.account_overlays.contains_key(&key) && app.settings.account_startup_signin.contains(&key));
        assert_eq!(app.settings.account_order, [key.clone(), app.accounts.iter().find(|row| row.key != key).unwrap().key.clone()]);
        assert!(app.accounts.iter().find(|row| row.key == key).unwrap().selected);
        for exists in [app.settings.account_installs.contains_key(&previous), app.settings.account_overlays.contains_key(&previous),
            app.settings.account_startup_signin.contains(&previous)] { assert!(!exists); }

        let values = app.accounts.iter().filter(|row| row.key != key).map(|row| row.account.clone()).collect();
        let _ = app.update(Message::AccountsSaved(Ok(values)));
        assert_eq!(app.settings.selected_accounts, Some(BTreeSet::new()));
        assert!(app.settings.account_auto_away.is_empty() && app.settings.account_installs.is_empty());
        assert!(app.settings.account_overlays.is_empty() && app.settings.account_startup_signin.is_empty());
        assert!(app.settings.account_auto_login.is_empty());
        assert!(!app.settings.account_order.contains(&key));
    }

    #[test]
    fn account_settings_failure_can_retry_after_the_vault_saved_a_new_key() {
        let root = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("crystal-account-retry-{}", backend::unique_id().unwrap()));
        std::fs::create_dir_all(&root).unwrap();
        let blocker = root.join("iced-launcher");
        std::fs::write(&blocker, b"settings parent is temporarily unavailable").unwrap();
        let mut app = app();
        app.config = Config { experiment: root.clone(), repository: root.join("runtime"), game_install: None, dat_overlays: Vec::new() };
        let previous = app.accounts[0].key.clone();
        app.settings.selected_accounts = Some(BTreeSet::from([previous.clone()]));
        app.settings.account_auto_away.insert(previous.clone(), 30);
        let _ = app.update(Message::EditAccount(previous.clone()));
        let _ = app.update(Message::EditorChanged(Field::PolId, "XYZ99999".into()));
        let edited = app.editor.as_ref().unwrap().account.clone();
        let key = account_key(&edited);
        let mut values: Vec<_> = app.accounts.iter().filter(|row| row.key != previous).map(|row| row.account.clone()).collect();
        values.push(edited);
        app.preview = false;
        let _ = app.update(Message::AccountsSaved(Ok(values.clone())));
        assert_eq!(app.editor.as_ref().unwrap().key.as_ref(), Some(&key));
        assert_eq!(app.settings.account_auto_away.get(&key), Some(&30));
        assert!(app.status.contains("launcher settings were not saved"));

        std::fs::remove_file(blocker).unwrap();
        // Creating this Task does not execute the vault write or launch a request.
        assert_eq!(app.update(Message::SaveAccount).units(), 1, "the editor must recognize its saved identity");
        let _ = app.update(Message::AccountsSaved(Ok(values)));
        assert!(app.editor.is_none() && !app.vault_saving);
        let saved = load_settings(&app.config).unwrap();
        assert_eq!(saved.account_auto_away.get(&key), Some(&30));
        assert_eq!(saved.selected_accounts, Some(BTreeSet::from([key])));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_folder_dialog_does_not_change_a_different_account_editor() {
        let mut app = app();
        let first_key = app.accounts[0].key.clone();
        let second_key = app.accounts[1].key.clone();
        let _ = app.update(Message::EditAccount(second_key));
        let _ = app.update(Message::PickedAccountFolder(first_key.clone(), Some(PathBuf::from(r"C:\Wrong account"))));
        let _ = app.update(Message::PickedAccountOverlay(first_key, Some(PathBuf::from(r"C:\Wrong overlay"))));
        assert!(app.editor.as_ref().unwrap().game_install.is_empty());
        assert!(app.editor.as_ref().unwrap().dat_overlays.is_empty());
    }
    #[test]
    fn closed_child_becomes_launchable_without_reusing_pid() {
        let mut app = app();
        app.accounts[0].pid = Some(99);
        app.accounts[0].alive = Some(Arc::new(AtomicBool::new(false)));
        let _ = app.update(Message::Tick);
        assert_eq!(app.accounts[0].pid, None);
        assert_eq!(app.launchable_count(), 3);
    }
    #[test]
    fn malicious_and_non_http_links_cannot_open() {
        assert!(allowed_link("https://www.ffxiah.com/"));
        assert!(allowed_link("https://www.playonline.com/ff11us/"));
        // The Japanese site's article and "View all news" links open too, and only on its host.
        assert!(allowed_link(services::sources(i18n::Lang::Ja).home));
        assert!(allowed_link("https://www.playonline.com/pcd2/topics/ff11/detail/40431/detail.html"));
        assert!(allowed_link("https://www.playonline.com/ff11/polnews/news27774.shtml"));
        assert!(allowed_link("https://sqex.to/1Twec"));
        for url in [
            "https://www.playonline.com.evil.test/ff11/",
            "https://evil.test/pcd2/topics/ff11/detail/40431/detail.html",
            "http://www.playonline.com/ff11/",
            "file:///C:/Windows/System32/cmd.exe",
            "https://www.playonline.com.evil.test/",
            "https://x@www.ffxiah.com/",
            "javascript:alert(1)",
        ] {
            assert!(!allowed_link(url));
        }
    }
}
