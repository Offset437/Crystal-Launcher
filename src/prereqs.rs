//! Windows components FINAL FANTASY XI needs (DirectPlay, DirectX 9, VC++ x86):
//! read-only detection at startup, a card per component on Settings → System
//! with a one-click fix, and a slim warning banner for Home and Downloads.
//!
//! Detection runs as a subscription while `checking` is set, so it starts with
//! the app and re-runs after every install. Nothing is installed without an
//! explicit click and a confirmation that says what will run.
mod detect;
mod install;
mod trust;

use crate::i18n::{t, tf};
use crate::icons::{icon, Icon};
use crate::ui::{colors, primary, scale, Colors, BODY, SEMI};
use crate::{App, Message};
pub(crate) use detect::Component;
use detect::{Evidence, FeatureState, Finding, Report, Status};
use iced::widget::{button, column, container, progress_bar, row, text, Space};
use iced::{Alignment, Border, Color, Element, Font, Length, Subscription, Task, Theme};
use install::{Event, Outcome, Phase};

const MONO: Font = Font::with_name("Consolas");

#[derive(Clone, Debug, PartialEq, Eq)]
enum Job {
    Idle,
    Running { id: u64, phase: Phase },
    Finished(Result<Outcome, String>),
    Cancelled,
}

pub(crate) struct State {
    report: Option<Report>,
    checking: bool,
    generation: u64,
    checked_at: Option<String>,
    jobs: [Job; 3],
    /// Set when an install reported "restart required" this session.
    restart: [bool; 3],
    confirm: Option<Component>,
    manual: bool,
    next_id: u64,
    /// Design preview: synthetic states, no detection or installs.
    preview: bool,
    elevated: bool,
}

impl Default for State {
    fn default() -> Self {
        let mut state = Self {
            report: None,
            checking: true,
            generation: 0,
            checked_at: None,
            jobs: [Job::Idle, Job::Idle, Job::Idle],
            restart: [false; 3],
            confirm: None,
            manual: false,
            next_id: 1,
            preview: false,
            elevated: is_elevated(),
        };
        if std::env::args().any(|a| a.starts_with("--preview")) {
            state.load_preview(&std::env::var("XI_PREVIEW_PREREQS").unwrap_or_default());
        }
        state
    }
}

#[cfg(windows)]
fn is_elevated() -> bool {
    detect::sys::is_elevated()
}
#[cfg(not(windows))]
fn is_elevated() -> bool {
    true
}

impl State {
    /// XI_PREVIEW_PREREQS: comma-separated flags. Default (or `mix`): one
    /// missing, one installing with progress, one installed. `ok` = all
    /// installed, `live` = real detection, plus `confirm`, `manual`,
    /// `restart`, `failed` overlays.
    fn load_preview(&mut self, flags: &str) {
        let has = |f: &str| flags.split(',').any(|x| x.trim().eq_ignore_ascii_case(f));
        if has("live") {
            return;
        }
        self.preview = true;
        self.checking = false;
        self.checked_at = Some("14:02".into());
        let installed = |v: Option<&str>, evidence: Evidence| Finding {
            status: Status::Installed { version: v.map(str::to_owned) },
            evidence,
        };
        let feature = |state| Evidence::Feature { state, missing: Vec::new() };
        let dx_files = || vec!["d3dx9_43.dll".to_owned(), "xinput1_3.dll".to_owned()];
        let directplay = if has("ok") {
            installed(None, feature(FeatureState::On))
        } else {
            Finding {
                status: Status::Missing { detail: "Turned off in Windows Features".into() },
                evidence: feature(FeatureState::Off),
            }
        };
        let directx = if has("ok") {
            installed(Some("9.29.952.3111"), Evidence::FilesPresent(dx_files()))
        } else {
            Finding {
                status: Status::Missing { detail: "d3dx9_43.dll, xinput1_3.dll not found".into() },
                evidence: Evidence::FilesMissing(dx_files()),
            }
        };
        let vc = installed(Some("14.44.35211"), Evidence::VcRecordAndFiles);
        self.report = Some(Report { findings: [directplay, directx, vc] });
        if !has("ok") {
            self.jobs[Component::DirectX9.index()] = Job::Running {
                id: 0,
                phase: Phase::Downloading { done: 44_130_000, total: Some(100_275_120) },
            };
        }
        if has("confirm") {
            self.confirm = Some(Component::DirectPlay);
        }
        if has("manual") {
            self.manual = true;
        }
        if has("restart") {
            self.restart[Component::DirectPlay.index()] = true;
            self.jobs[Component::DirectPlay.index()] = Job::Finished(Ok(Outcome::RestartRequired));
        }
        if has("failed") {
            self.jobs[Component::DirectX9.index()] =
                Job::Finished(Err(t("Couldn't reach Microsoft's download server. Check your internet connection.").into()));
        }
    }

    /// The status to show: a pending restart from this session's install
    /// overrides a still-missing reading.
    fn status(&self, c: Component) -> Option<&Status> {
        let status = &self.report.as_ref()?.get(c).status;
        static PENDING: Status = Status::RestartPending;
        if self.restart[c.index()] && status.is_missing() {
            Some(&PENDING)
        } else {
            Some(status)
        }
    }
    fn missing(&self) -> Vec<Component> {
        Component::ALL.into_iter().filter(|c| self.status(*c).is_some_and(Status::is_missing)).collect()
    }
    fn pending(&self) -> Vec<Component> {
        Component::ALL
            .into_iter()
            .filter(|c| self.status(*c) == Some(&Status::RestartPending))
            .collect()
    }
    pub(crate) fn busy(&self) -> bool {
        self.jobs.iter().any(|j| matches!(j, Job::Running { .. }))
    }
    fn recheck(&mut self) {
        if self.preview {
            return;
        }
        self.checking = true;
        self.generation += 1;
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Msg {
    Detected(u64, Report),
    CheckAgain,
    Ask(Component),
    Dismiss,
    Install(Component),
    Cancel(Component),
    Job(Component, u64, Event),
    ToggleManual,
    OpenWindowsFeatures,
}

pub(crate) fn update(state: &mut State, msg: Msg) -> Task<Msg> {
    match msg {
        Msg::Detected(generation, report) => {
            if generation == state.generation {
                // A component that now reads as present no longer needs its
                // restart note.
                for c in Component::ALL {
                    if !report.get(c).status.is_missing() && report.get(c).status != Status::RestartPending {
                        state.restart[c.index()] = false;
                    }
                }
                state.report = Some(report);
                state.checking = false;
                state.checked_at = Some(chrono::Local::now().format("%H:%M").to_string());
            }
        }
        Msg::CheckAgain => state.recheck(),
        Msg::Ask(c) => {
            if !matches!(state.jobs[c.index()], Job::Running { .. }) {
                state.confirm = Some(c);
            }
        }
        Msg::Dismiss => state.confirm = None,
        Msg::Install(c) => {
            // Only from the confirmation panel, and never twice at once.
            if state.confirm == Some(c) && !state.preview && !state.busy() {
                state.confirm = None;
                state.jobs[c.index()] = Job::Running {
                    id: state.next_id,
                    phase: if c == Component::DirectPlay {
                        Phase::Installing
                    } else {
                        Phase::Downloading { done: 0, total: None }
                    },
                };
                state.next_id += 1;
            }
        }
        Msg::Cancel(c) => {
            if let Job::Running { phase, .. } = &state.jobs[c.index()] {
                if phase.cancellable() {
                    // Dropping the subscription stops the download and removes the file.
                    state.jobs[c.index()] = Job::Cancelled;
                }
            }
        }
        Msg::Job(c, attempt, event) => {
            if let Job::Running { id, phase } = &mut state.jobs[c.index()] {
                if *id != attempt { return Task::none(); }
                match event {
                    Event::Phase(next) => *phase = next,
                    Event::Finished(result) => {
                        if result == Ok(Outcome::RestartRequired) {
                            state.restart[c.index()] = true;
                        }
                        state.jobs[c.index()] = Job::Finished(result);
                        state.recheck();
                    }
                }
            }
        }
        Msg::ToggleManual => state.manual = !state.manual,
        Msg::OpenWindowsFeatures => {
            if !state.preview {
                #[cfg(windows)]
                {
                    let _ = std::process::Command::new(install::system32().join("OptionalFeatures.exe")).spawn();
                }
            }
        }
    }
    Task::none()
}

pub(crate) fn subscription(state: &State) -> Subscription<Msg> {
    if state.preview {
        return Subscription::none();
    }
    let mut list = Vec::new();
    if state.checking {
        list.push(Subscription::run_with(state.generation, |generation| {
            let generation = *generation;
            iced::futures::stream::once(async move {
                let report = tokio::task::spawn_blocking(detect::detect)
                    .await
                    .unwrap_or_else(|_| detect::detect());
                Msg::Detected(generation, report)
            })
        }));
    }
    for c in Component::ALL {
        if let Job::Running { id, .. } = state.jobs[c.index()] {
            list.push(Subscription::run_with((c, id), |(c, id)| {
                use iced::futures::StreamExt;
                let c = *c;
                let id = *id;
                install::job(c).map(move |event| Msg::Job(c, id, event))
            }));
        }
    }
    Subscription::batch(list)
}

// ---- view ---------------------------------------------------------------------

fn rgb(value: u32) -> Color {
    Color::from_rgb8((value >> 16) as u8, (value >> 8) as u8, value as u8)
}
fn light(c: Colors) -> bool {
    c.bg.r > 0.5
}
pub(crate) fn ok_tone(c: Colors) -> Color {
    if light(c) { rgb(0x2E7A5E) } else { rgb(0x82CBA8) }
}
pub(crate) fn warn_tone(c: Colors) -> Color {
    if light(c) { rgb(0x9A6519) } else { rgb(0xE2B866) }
}
fn danger_tone(c: Colors) -> Color {
    if light(c) { rgb(0xB24A4A) } else { rgb(0xE59494) }
}
pub(crate) fn alpha(color: Color, a: f32) -> Color {
    Color { a, ..color }
}

fn secondary(c: Colors) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: Some(if hover { c.select } else { c.panel }.into()),
            text_color: if status == button::Status::Disabled { c.sub } else { c.ink },
            border: Border { color: if hover { c.accent } else { c.line }, width: 1.0, radius: 7.0.into() },
            ..Default::default()
        }
    }
}
fn link(c: Colors) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: matches!(status, button::Status::Hovered | button::Status::Pressed).then(|| c.select.into()),
        text_color: c.accent,
        border: Border { radius: 5.0.into(), ..Default::default() },
        ..Default::default()
    }
}

pub(crate) fn action<'a>(c: Colors, which: Icon, label: &'a str, emphasis: bool, message: Option<Message>) -> Element<'a, Message> {
    let enabled = message.is_some();
    let tint = match (enabled, emphasis) {
        (false, _) => c.sub,
        (true, true) => c.play_text,
        (true, false) => c.accent,
    };
    let body = row![icon(which, 15.0, tint), text(label).font(BODY).size(14)]
        .spacing(8)
        .align_y(Alignment::Center);
    let b = crate::controller_nav::button(body).padding([9, 16]).on_press_maybe(message);
    if emphasis {
        b.style(primary).into()
    } else {
        b.style(secondary(c)).into()
    }
}

pub(crate) fn divider<'a>(c: Colors) -> Element<'a, Message> {
    container(Space::new())
        .width(Length::Fill)
        .height(1)
        .style(move |_| container::Style { background: Some(c.line.into()), ..Default::default() })
        .into()
}

pub(crate) fn card<'a>(c: Colors, content: impl Into<Element<'a, Message>>, s: f32) -> Element<'a, Message> {
    container(content)
        .padding([20.0 * s, 24.0 * s])
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(c.panel.into()),
            text_color: Some(c.ink),
            border: Border { color: c.line, width: 1.0, radius: 9.0.into() },
            ..Default::default()
        })
        .into()
}

/// A small rounded status chip: tinted fill, tone text.
pub(crate) fn chip<'a>(label: String, tone: Color) -> Element<'a, Message> {
    container(text(label).font(SEMI).size(11.5).color(tone))
        .padding([2, 9])
        .style(move |_| container::Style {
            background: Some(alpha(tone, 0.13).into()),
            border: Border { color: alpha(tone, 0.32), width: 1.0, radius: 10.0.into() },
            ..Default::default()
        })
        .into()
}

/// The 38 px status badge at the start of a card.
fn badge<'a>(which: Icon, tone: Color) -> Element<'a, Message> {
    container(icon(which, 19.0, tone))
        .center_x(38)
        .center_y(38)
        .style(move |_| container::Style {
            background: Some(alpha(tone, 0.12).into()),
            border: Border { color: alpha(tone, 0.28), width: 1.0, radius: 19.0.into() },
            ..Default::default()
        })
        .into()
}

fn note<'a>(which: Icon, tone: Color, body: String, c: Colors) -> Element<'a, Message> {
    row![
        container(icon(which, 14.0, tone)).padding(iced::Padding::ZERO.top(2)),
        text(body).font(BODY).size(13).color(c.ink).width(Length::Fill)
    ]
    .spacing(10)
    .into()
}

fn bytes(value: u64) -> String {
    let mb = value as f64 / 1_000_000.0;
    if mb >= 10.0 { format!("{mb:.0} MB") } else { format!("{mb:.1} MB") }
}

fn phase_line(phase: &Phase, c: Component) -> (String, String, Option<f32>) {
    match phase {
        Phase::Downloading { done, total } => {
            let file = install::plan(c).download.map(|d| d.0).unwrap_or("installer");
            match total {
                Some(size) if *size > 0 => {
                    let f = (*done as f64 / *size as f64).clamp(0.0, 1.0) as f32;
                    (
                        tf("Downloading {file}", &[("file", &file)]),
                        tf(
                            "{done} of {total} · {percent}%",
                            &[("done", &bytes(*done)), ("total", &bytes(*size)), ("percent", &format!("{:.0}", f * 100.0))],
                        ),
                        Some(f),
                    )
                }
                _ => (
                    tf("Downloading {file}", &[("file", &file)]),
                    tf("{done} received", &[("done", &bytes(*done))]),
                    Some(0.0),
                ),
            }
        }
        Phase::Verifying => (
            t("Checking the Microsoft signature").into(),
            t("The installer runs only if it is validly signed by Microsoft.").into(),
            None,
        ),
        Phase::Extracting => (
            t("Unpacking the DirectX runtime").into(),
            t("Into a temporary folder that is removed afterwards.").into(),
            None,
        ),
        Phase::Installing => (
            match c {
                Component::DirectPlay => t("Enabling DirectPlay").into(),
                _ => tf("Installing the {name}", &[("name", &c.name())]),
            },
            t("This can take a minute. Windows may ask for permission.").into(),
            None,
        ),
    }
}

fn component_card<'a>(app: &'a App, comp: Component, c: Colors, s: f32) -> Element<'a, Message> {
    let state = &app.prereqs;
    let finding = state.report.as_ref().map(|r| r.get(comp));
    let status = state.status(comp);
    let job = &state.jobs[comp.index()];
    let running = matches!(job, Job::Running { .. });

    let (which, tone, label) = match status {
        None => (Icon::Refresh, c.sub, t("Checking…").to_owned()),
        Some(Status::Installed { .. }) => (Icon::CheckCircle, ok_tone(c), if comp == Component::DirectPlay { t("Enabled") } else { t("Installed") }.to_owned()),
        Some(Status::Missing { .. }) if running => (Icon::Download, c.accent, t("Installing").to_owned()),
        Some(Status::Missing { .. }) => (Icon::AlertCircle, warn_tone(c), if comp == Component::DirectPlay { t("Turned off") } else { t("Missing") }.to_owned()),
        Some(Status::RestartPending) => (Icon::Refresh, c.accent, t("Restart required").to_owned()),
    };
    let mut title = row![text(comp.name()).font(SEMI).size(16).color(c.ink), chip(label, tone)]
        .spacing(10)
        .align_y(Alignment::Center);
    if let Some(Status::Installed { version: Some(v) }) = status {
        title = title.push(text(detect::shown_version(comp, v)).font(MONO).size(12).color(c.sub));
    }
    let mut info = column![title, text(comp.why()).font(BODY).size(13).color(c.sub)].spacing(5).width(Length::Fill);
    if let Some(f) = finding {
        // Worded now, so it follows a language change without a new check.
        info = info.push(text(tf("Checked: {evidence}", &[("evidence", &f.evidence.text())])).font(BODY).size(11.5).color(alpha(c.sub, 0.8)));
    }

    let busy_elsewhere = state.busy() && !running;
    let button_el: Element<'a, Message> = match status {
        Some(Status::Missing { .. }) if !running && state.confirm == Some(comp) => Space::new().into(),
        Some(Status::Missing { .. }) if !running => {
            let icon_kind = if comp == Component::DirectPlay { Icon::Wrench } else { Icon::Download };
            action(c, icon_kind, comp.verb(), true, (!busy_elsewhere).then_some(Message::Prereqs(Msg::Ask(comp))))
        }
        Some(_) if !running => action(c, Icon::Refresh, t("Re-check"), false, (!state.checking).then_some(Message::Prereqs(Msg::CheckAgain))),
        _ => Space::new().into(),
    };
    let head = row![badge(which, tone), info, button_el].spacing(16).align_y(Alignment::Center);
    let mut body = column![head].spacing(16.0 * s);

    // Confirmation: exactly what will run.
    if state.confirm == Some(comp) && !running {
        let plan = install::plan(comp);
        let mut steps = column![].spacing(6);
        for step in &plan.steps {
            steps = steps.push(row![text("•").size(13).color(c.sub), text(*step).font(BODY).size(13).color(c.ink).width(Length::Fill)].spacing(8));
        }
        let heading = match plan.download {
            Some((file, size)) => tf(
                "{action} {name} — downloads {file} ({size})",
                &[("action", &comp.verb()), ("name", &comp.full_name()), ("file", &file), ("size", &size)],
            ),
            None => tf("{action} {name}", &[("action", &comp.verb()), ("name", &comp.full_name())]),
        };
        let command = container(text(plan.command).font(MONO).size(12).color(c.ink))
            .padding([8, 12])
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(c.field.into()),
                border: Border { color: c.line, width: 1.0, radius: 6.0.into() },
                ..Default::default()
            });
        let permission = if busy_elsewhere {
            t("Another component is being installed. This can start when it finishes.")
        } else if state.elevated {
            t("Runs with the launcher's administrator rights. Close FINAL FANTASY XI first.")
        } else {
            t("Windows will ask for administrator permission. Close FINAL FANTASY XI first.")
        };
        let confirm_label = if plan.download.is_some() { t("Download and install") } else { t("Enable DirectPlay") };
        body = body.push(divider(c)).push(
            column![
                text(heading).font(SEMI).size(14).color(c.ink),
                steps,
                column![text(t("Will run")).font(BODY).size(12).color(c.sub), command].spacing(6),
                row![
                    text(permission).font(BODY).size(12).color(c.sub).width(Length::Fill),
                    action(c, Icon::Close, t("Not now"), false, Some(Message::Prereqs(Msg::Dismiss))),
                    action(c, if plan.download.is_some() { Icon::Download } else { Icon::Wrench }, confirm_label, true, (!busy_elsewhere).then_some(Message::Prereqs(Msg::Install(comp))))
                ]
                .spacing(10)
                .align_y(Alignment::Center)
            ]
            .spacing(12),
        );
    }

    // Progress.
    if let Job::Running { phase, .. } = job {
        let (head_line, detail, fraction) = phase_line(phase, comp);
        let bar: Element<'a, Message> = progress_bar(0.0..=1.0, fraction.unwrap_or(1.0))
            .girth(6)
            .style(move |_| progress_bar::Style {
                background: c.select.into(),
                bar: if fraction.is_some() { c.accent.into() } else { alpha(c.accent, 0.35).into() },
                border: Border { radius: 3.0.into(), ..Default::default() },
            })
            .into();
        let mut line = row![
            column![
                text(head_line).font(BODY).size(14).color(c.ink),
                text(detail).font(BODY).size(12).color(c.sub)
            ]
            .spacing(3)
            .width(Length::Fill)
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        if phase.cancellable() {
            line = line.push(action(c, Icon::Close, t("Cancel"), false, Some(Message::Prereqs(Msg::Cancel(comp)))));
        }
        body = body.push(divider(c)).push(column![line, bar].spacing(10));
    }

    // Last result.
    let result: Option<Element<'a, Message>> = match job {
        Job::Finished(Ok(Outcome::Done)) => Some(note(
            Icon::CheckCircle,
            ok_tone(c),
            match status {
                Some(Status::Installed { .. }) => tf("{name} is ready.", &[("name", &comp.name())]),
                _ => t("The installer finished, but the component still reads as missing. Restart Windows, then Check again.").into(),
            },
            c,
        )),
        Job::Finished(Ok(Outcome::RestartRequired)) => Some(note(
            Icon::Refresh,
            c.accent,
            tf(
                "Restart Windows to finish setting up {name}, then press Check again.",
                &[("name", &comp.name())],
            ),
            c,
        )),
        Job::Finished(Err(error)) => Some(note(Icon::AlertCircle, danger_tone(c), error.clone(), c)),
        Job::Cancelled => Some(note(Icon::Close, c.sub, t("Download cancelled. Nothing was installed.").into(), c)),
        _ => None,
    };
    if let Some(result) = result {
        if state.confirm != Some(comp) {
            body = body.push(result);
        }
    }

    // DirectPlay's manual route.
    if comp == Component::DirectPlay && status.is_some_and(Status::is_missing) {
        let chevron = container(icon(Icon::ChevronDown, 14.0, c.accent)).padding(iced::Padding::ZERO.top(1));
        let toggle = crate::controller_nav::button(row![text(t("How to enable it manually")).font(BODY).size(13), chevron].spacing(6).align_y(Alignment::Center))
            .padding([4, 6])
            .style(link(c))
            .on_press(Message::Prereqs(Msg::ToggleManual));
        let mut manual = column![toggle].spacing(10);
        if state.manual {
            let steps = [
                t("Open “Turn Windows features on or off” (the button below, or search for it in Start)."),
                t("Expand Legacy Components."),
                t("Tick DirectPlay and press OK."),
                t("Restart if Windows asks, then press Check again here."),
            ];
            let mut list = column![].spacing(6);
            for (n, step) in steps.iter().enumerate() {
                list = list.push(
                    row![
                        text(format!("{}.", n + 1)).font(SEMI).size(13).color(c.accent).width(18),
                        text(*step).font(BODY).size(13).color(c.ink).width(Length::Fill)
                    ]
                    .spacing(6),
                );
            }
            manual = manual.push(container(list).padding(iced::Padding::ZERO.left(6))).push(
                container(action(c, Icon::ExternalLink, t("Open Windows Features"), false, Some(Message::Prereqs(Msg::OpenWindowsFeatures))))
                    .padding(iced::Padding::ZERO.left(6)),
            );
        }
        body = body.push(manual);
    }

    card(c, body, s)
}

pub(crate) fn view(app: &App) -> Element<'_, Message> {
    let c = colors(app.settings.light_theme);
    let s = scale(app);
    let state = &app.prereqs;
    let missing = state.missing();
    let pending = state.pending();
    let (summary, tone, which) = if state.report.is_none() {
        (t("Checking this PC…").to_owned(), c.sub, Icon::Refresh)
    } else if !missing.is_empty() {
        let n = missing.len();
        (
            if n == 1 {
                t("1 component needs attention").to_owned()
            } else {
                tf("{count} components need attention", &[("count", &n)])
            },
            warn_tone(c),
            Icon::AlertCircle,
        )
    } else if !pending.is_empty() {
        (t("Restart Windows to finish setting up").to_owned(), c.accent, Icon::Refresh)
    } else {
        (t("Everything FINAL FANTASY XI needs is installed").to_owned(), ok_tone(c), Icon::CheckCircle)
    };
    let when = if state.checking {
        t("Checking…").to_owned()
    } else {
        state.checked_at.as_ref().map(|time| tf("Last checked at {time}", &[("time", time)])).unwrap_or_default()
    };
    let header = row![
        column![
            text(t("Windows components")).font(SEMI).size(18).color(c.ink),
            row![icon(which, 15.0, tone), text(summary).font(BODY).size(13).color(tone), text(if when.is_empty() { String::new() } else { format!("·  {when}") }).font(BODY).size(13).color(c.sub)]
                .spacing(8)
                .align_y(Alignment::Center)
        ]
        .spacing(6)
        .width(Length::Fill),
        action(c, Icon::Refresh, t("Check again"), false, (!state.checking && !state.busy()).then_some(Message::Prereqs(Msg::CheckAgain)))
    ]
    .spacing(16)
    .align_y(Alignment::Center);
    let intro = row![
        container(icon(Icon::Help, 14.0, c.sub)).padding(iced::Padding::ZERO.top(2)),
        text(t("FINAL FANTASY XI is a 32-bit DirectX 8 game that relies on these parts of Windows. Checking is read-only: nothing is downloaded or changed until you press Install or Enable, and every download must be signed by Microsoft before it runs."))
            .font(BODY)
            .size(12.5)
            .color(c.sub)
            .width(Length::Fill)
    ]
    .spacing(10);
    let mut list = column![card(c, column![header, divider(c), intro].spacing(14.0 * s), s)].spacing(14.0 * s);
    for comp in Component::ALL {
        list = list.push(component_card(app, comp, c, s));
    }
    list.padding(iced::Padding::ZERO.bottom(24.0 * s).right(14.0 * s)).width(Length::Fill).into()
}

/// A slim warning strip for Home/Downloads when something is missing; `None`
/// when everything is installed (or still being checked).
pub(crate) fn banner(app: &App) -> Option<Element<'_, Message>> {
    let c = colors(app.settings.light_theme);
    let state = &app.prereqs;
    let missing = state.missing();
    let pending = state.pending();
    let (tone, which, lead, rest, button_label) = if let Some(first) = missing.first() {
        let lead = match missing.len() {
            1 => first.name().to_owned(),
            2 => tf("{first} and {second}", &[("first", &missing[0].name()), ("second", &missing[1].name())]),
            _ => tf(
                "{first}, {second} and {third}",
                &[("first", &missing[0].name()), ("second", &missing[1].name()), ("third", &missing[2].name())],
            ),
        };
        let rest = if missing.len() == 1 {
            tf(" is missing — {consequence}", &[("consequence", &first.consequence())])
        } else if missing.iter().any(|m| m.critical()) {
            t(" are missing — FINAL FANTASY XI can't start without DirectPlay.").to_owned()
        } else {
            t(" are missing — parts of FINAL FANTASY XI won't work.").to_owned()
        };
        let label = if state.busy() { t("View") } else { t("Fix") };
        (warn_tone(c), Icon::AlertCircle, lead, rest, label)
    } else if let Some(first) = pending.first() {
        (
            c.accent,
            Icon::Refresh,
            t("Restart Windows").to_owned(),
            tf(" to finish setting up {name}.", &[("name", &first.name())]),
            t("Details"),
        )
    } else {
        return None;
    };
    let fix = crate::controller_nav::button(
        row![text(button_label).font(SEMI).size(13), icon(Icon::ArrowRight, 14.0, tone)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .padding([4, 11])
    .style(move |_, status| {
        let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: Some(alpha(tone, if hover { 0.24 } else { 0.14 }).into()),
            text_color: tone,
            border: Border { color: alpha(tone, 0.45), width: 1.0, radius: 6.0.into() },
            ..Default::default()
        }
    })
    .on_press(Message::OpenSettingsTab(crate::settings_page::Tab::System));
    let strip = container(
        row![
            icon(which, 16.0, tone),
            row![text(lead).font(SEMI).size(13).color(c.ink), text(rest).font(BODY).size(13).color(c.ink)]
                .width(Length::Fill),
            fix
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding(iced::Padding { top: 5.0, bottom: 5.0, left: 14.0, right: 6.0 })
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(alpha(tone, if light(c) { 0.09 } else { 0.08 }).into()),
        text_color: Some(c.ink),
        border: Border { color: alpha(tone, 0.40), width: 1.0, radius: 8.0.into() },
        ..Default::default()
    });
    Some(strip.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(statuses: [Status; 3]) -> State {
        let mut state = State {
            report: None,
            checking: false,
            generation: 0,
            checked_at: None,
            jobs: [Job::Idle, Job::Idle, Job::Idle],
            restart: [false; 3],
            confirm: None,
            manual: false,
            next_id: 1,
            preview: false,
            elevated: true,
        };
        let [a, b, c] = statuses;
        let f = |status| Finding { status, evidence: Evidence::NotApplicable };
        state.report = Some(Report { findings: [f(a), f(b), f(c)] });
        state
    }
    fn ok() -> Status {
        Status::Installed { version: None }
    }
    fn missing() -> Status {
        Status::Missing { detail: "x".into() }
    }

    #[test]
    fn install_requires_confirmation() {
        let mut state = state_with([missing(), ok(), ok()]);
        let _ = update(&mut state, Msg::Install(Component::DirectPlay));
        assert_eq!(state.jobs[0], Job::Idle, "no install without the confirmation panel");
        let _ = update(&mut state, Msg::Ask(Component::DirectPlay));
        let _ = update(&mut state, Msg::Install(Component::DirectPlay));
        assert!(matches!(state.jobs[0], Job::Running { .. }));
        assert_eq!(state.confirm, None);
    }

    #[test]
    fn one_install_at_a_time() {
        let mut state = state_with([missing(), missing(), ok()]);
        let _ = update(&mut state, Msg::Ask(Component::DirectPlay));
        let _ = update(&mut state, Msg::Install(Component::DirectPlay));
        let _ = update(&mut state, Msg::Ask(Component::DirectX9));
        let _ = update(&mut state, Msg::Install(Component::DirectX9));
        assert_eq!(state.jobs[1], Job::Idle);
    }

    #[test]
    fn cancel_only_while_downloading() {
        let mut state = state_with([ok(), missing(), ok()]);
        state.jobs[1] = Job::Running { id: 1, phase: Phase::Installing };
        let _ = update(&mut state, Msg::Cancel(Component::DirectX9));
        assert!(matches!(state.jobs[1], Job::Running { .. }));
        state.jobs[1] = Job::Running { id: 1, phase: Phase::Downloading { done: 1, total: Some(2) } };
        let _ = update(&mut state, Msg::Cancel(Component::DirectX9));
        assert_eq!(state.jobs[1], Job::Cancelled);
    }

    #[test]
    fn finished_install_rechecks_and_tracks_restart() {
        let mut state = state_with([missing(), ok(), ok()]);
        state.jobs[0] = Job::Running { id: 1, phase: Phase::Installing };
        let _ = update(&mut state, Msg::Job(Component::DirectPlay, 1, Event::Finished(Ok(Outcome::RestartRequired))));
        assert!(state.checking);
        assert_eq!(state.generation, 1);
        // Still missing on disk until the restart: shown as restart pending.
        assert_eq!(state.status(Component::DirectPlay), Some(&Status::RestartPending));
        assert!(state.missing().is_empty());
        assert_eq!(state.pending(), vec![Component::DirectPlay]);
        // A stale detection result is ignored.
        let report = state.report.clone().unwrap();
        let _ = update(&mut state, Msg::Detected(0, report.clone()));
        assert!(state.checking);
        // Once it reads installed, the restart note clears.
        let mut fixed = report;
        fixed.findings[0].status = ok();
        let _ = update(&mut state, Msg::Detected(1, fixed));
        assert!(!state.checking);
        assert!(!state.restart[0]);
    }

    #[test]
    fn preview_mix_has_each_state() {
        let mut state = state_with([ok(), ok(), ok()]);
        state.load_preview("");
        assert!(state.status(Component::DirectPlay).is_some_and(Status::is_missing));
        assert!(matches!(state.jobs[1], Job::Running { .. }));
        assert!(matches!(state.status(Component::VcRedist), Some(Status::Installed { .. })));
        let mut all = state_with([ok(), ok(), ok()]);
        all.load_preview("ok");
        assert!(all.missing().is_empty());
    }

    #[test]
    fn delayed_job_events_do_not_finish_or_change_a_retried_download() {
        let mut state = state_with([ok(), missing(), ok()]);
        let component = Component::DirectX9;
        let _ = update(&mut state, Msg::Ask(component));
        let _ = update(&mut state, Msg::Install(component));
        let _ = update(&mut state, Msg::Cancel(component));
        let _ = update(&mut state, Msg::Ask(component));
        let _ = update(&mut state, Msg::Install(component));
        let retry = state.jobs[1].clone();
        let _ = update(&mut state, Msg::Job(component, 1, Event::Phase(Phase::Installing)));
        let _ = update(&mut state, Msg::Job(component, 1, Event::Finished(Ok(Outcome::Done))));
        assert_eq!(state.jobs[1], retry);
        assert!(!state.checking);
        let _ = update(&mut state, Msg::Job(component, 2, Event::Finished(Ok(Outcome::Done))));
        assert!(state.checking);
        assert!(!state.busy());
    }
}
