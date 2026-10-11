//! The Friends page: a friends sidebar and a conversation, like a desktop
//! chat client. `friends::State` and the snapshot's `social::State` stay the
//! source of truth; this module only draws them.
use super::{message_time, Body, Composer, Entry, Friend, Pane, Snapshot, AUTO_AWAY_OPTIONS, AutoAway, SUBJECT_MAX, TEXT_MAX};
use crate::i18n::{t, tf};
use crate::icons::{icon, Icon};
use crate::privacy;
use crate::session_worker::NativeControl;
use crate::ui::{
    self, card_style, colors, control, dl_danger, dl_ok, hint, hline, is_light, mix, name_text, private_text,
    profile_menu_style, profile_pick_style, soft_tile, with_alpha, Colors, Tone, BODY, CONTROL_R, SEMI,
};
use crate::{App, Message};
use iced::widget::tooltip::Position;
use iced::widget::{button, column, container, pick_list, row, scrollable, stack, text, text_editor, text_input, Space};
use iced::{Alignment, Border, Color, Element, Font, Length, Theme};

type El<'a> = Element<'a, Message>;

/// Space around a banner under a pane header.
const BANNER_PAD: iced::Padding = iced::Padding { top: 12.0, right: 16.0, bottom: 0.0, left: 16.0 };

/// What the page can do right now, worked out once.
struct Ctx<'a> {
    app: &'a App,
    c: Colors,
    account: Option<&'a crate::AccountRow>,
    /// A request or sign-in is in flight.
    busy: bool,
    /// Signed in with FFXI closed: the Viewer's social functions are open.
    idle: bool,
    /// `idle`, nothing in flight, and the social service is up.
    ready: bool,
    /// FFXI is running on this account: presence only.
    in_game: bool,
}

impl<'a> Ctx<'a> {
    fn new(app: &'a App) -> Self {
        let state = &app.friends;
        let account = app.accounts.iter().find(|a| state.account.as_ref() == Some(&a.key));
        let busy = account.is_some_and(|a| a.busy) || state.pending.is_some() || state.snapshot.as_ref().is_some_and(|s| s.social.busy);
        let idle = state.connected && state.snapshot.as_ref().is_some_and(|s| s.game_running == Some(false));
        let ready = idle && !busy && state.snapshot.as_ref().is_some_and(|s| s.social.ready);
        let in_game = state.connected && state.snapshot.as_ref().is_some_and(|s| s.game_running == Some(true));
        Self { app, c: colors(app.settings.light_theme), account, busy, idle, ready, in_game }
    }
    fn light(&self) -> bool {
        self.app.settings.light_theme
    }
}

pub(crate) fn view(app: &App) -> El<'_> {
    let x = Ctx::new(app);
    let s = ui::scale(app);
    let state = &app.friends;
    let mut page = column![header(&x, s)].spacing(16.0 * s);
    // Session problems belong to the whole page.
    let mut problems: Vec<String> = Vec::new();
    if !state.error.is_empty() {
        problems.push(state.error.clone());
    }
    if let Some(error) = state.snapshot.as_ref().and_then(|s| s.session_error.as_ref()) {
        problems.push(error.clone());
    }
    let shown = state.snapshot.as_ref().filter(|s| s.fetched_at != 0 && state.connected);
    if let Some(error) = shown.and_then(|s| s.error.as_ref()) {
        problems.push(tf("{error} The previous list is still shown.", &[("error", error)]));
    }
    for problem in problems {
        page = page.push(banner(&x, Icon::AlertCircle, &problem, true));
    }
    let body: El<'_> = match shown {
        Some(snapshot) => two_pane(&x, snapshot),
        None => sign_in(&x),
    };
    page.push(container(body).height(Length::Fill))
        .padding(iced::Padding::ZERO.bottom(ui::GUTTER * s))
        .height(Length::Fill)
        .into()
}

// ---- Page header -----------------------------------------------------------

fn header<'a>(x: &Ctx<'a>, s: f32) -> El<'a> {
    let c = x.c;
    let state = &x.app.friends;
    let mut description = String::new();
    if state.connected {
        description.push_str(match state.snapshot.as_ref().and_then(|s| s.game_running) {
            Some(false) => t("Signed in to PlayOnline"),
            Some(true) => t("FFXI session active"),
            None if x.busy => t("Signing in to PlayOnline…"),
            None => t("PlayOnline session active"),
        });
        if let Some(snapshot) = state.snapshot.as_ref().filter(|s| s.fetched_at != 0) {
            let stamp = chrono::DateTime::from_timestamp(snapshot.fetched_at as i64, 0)
                .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
                .unwrap_or_default();
            let count = snapshot.entries.iter().filter(|f| !f.alternate).count();
            description.push_str(" · ");
            description.push_str(&tf("{count} friends · Updated {time}", &[("count", &count), ("time", &stamp)]));
        }
    } else {
        description.push_str(t("Message your PlayOnline friends without opening FFXI."));
    }
    let mut actions = row![].spacing(8).align_y(Alignment::Center);
    if state.connected {
        actions = actions.push(control(c, Tone::Secondary, None, t("Sign out"), (x.idle && !x.busy).then_some(Message::FriendsSignOut), None));
        actions = actions.push(control(
            c,
            Tone::Primary,
            Some(Icon::Play),
            t("Start FFXI"),
            (x.idle && !x.busy && !x.app.launch_blocked()).then_some(Message::FriendsPlay),
            None,
        ));
    }
    row![
        column![
            text(t("PlayOnline friends")).font(BODY).size(26.0 * s).color(c.ink),
            text(description).font(BODY).size(14.0 * s).color(c.sub),
        ]
        .spacing(6.0 * s)
        .width(Length::Fill),
        actions
    ]
    .spacing(16)
    .align_y(Alignment::Center)
    .into()
}

/// A one-line message in a tinted tile: a problem (danger) or a notice.
fn banner<'a>(x: &Ctx<'a>, glyph: Icon, message: &str, problem: bool) -> El<'a> {
    let c = x.c;
    let tone = if problem { dl_danger(c) } else { c.accent };
    container(
        row![icon(glyph, 16.0, tone), container(private_text(x.app, message, 13.0, BODY, if problem { tone } else { c.ink })).width(Length::Fill)]
            .spacing(10)
            .align_y(Alignment::Center),
    )
    .padding([10, 14])
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(with_alpha(tone, if is_light(c) { 0.08 } else { 0.10 }).into()),
        border: Border { color: with_alpha(tone, 0.25), width: 1.0, radius: CONTROL_R.into() },
        ..Default::default()
    })
    .into()
}

// ---- Not signed in -------------------------------------------------------------

fn sign_in<'a>(x: &Ctx<'a>) -> El<'a> {
    let c = x.c;
    let app = x.app;
    let state = &app.friends;
    let retail = app.accounts.iter().any(|a| a.account.mode == crate::AccountMode::Retail);
    let message = if !retail {
        t("Add a retail account to use PlayOnline friends.")
    } else if state.snapshot.as_ref().is_some_and(|s| s.error.is_some()) {
        t("Sign in again to reconnect to PlayOnline.")
    } else if state.connected {
        t("Loading your PlayOnline friend list…")
    } else {
        t("Sign in to use PlayOnline friends without opening FFXI. The session continues when Crystal is closed.")
    };
    let mut body = column![
        round_icon(c, Icon::Users, 64.0),
        Space::new().height(4),
        text(t("Your PlayOnline friends")).font(SEMI).size(18).color(c.ink),
        container(text(message).font(BODY).size(13).color(c.sub).align_x(Alignment::Center)).max_width(440),
    ]
    .spacing(8)
    .align_x(Alignment::Center);
    if let Some(error) = state.snapshot.as_ref().and_then(|s| s.error.as_ref()) {
        body = body.push(container(banner(x, Icon::AlertCircle, &tf("Could not load friends: {error}", &[("error", error)]), true)).max_width(440));
    }
    if retail && !state.connected {
        body = body.push(Space::new().height(8));
        if app.accounts.iter().filter(|a| a.account.mode == crate::AccountMode::Retail).count() > 1 {
            body = body.push(container(account_switcher(x, text(t("Choose the account to sign in")).font(BODY).size(12).color(c.sub).into(), None, None)).width(320));
        }
        let mut controls = row![].spacing(8).align_y(Alignment::Center);
        if let Some(account) = x.account {
            let key = account.key.clone();
            controls = controls.push(
                ui::text_field(t("One-time code (optional)"), &account.otp)
                    .secure(true)
                    .on_input(move |value| Message::OtpChanged(key.clone(), value))
                    .width(200),
            );
        }
        controls = controls.push(control(
            c,
            Tone::Primary,
            None,
            if x.busy { t("Connecting…") } else { t("Sign in to PlayOnline") },
            (x.account.is_some() && !x.busy && app.vault_ready && !app.launch_blocked()).then_some(Message::FriendsConnect),
            None,
        ));
        body = body.push(controls);
    }
    container(body).center(Length::Fill).padding(24).style(card_style(c)).into()
}

// ---- Signed in: sidebar and conversation -------------------------------------------

/// The sidebar's own fill, a step darker than the card (lighter in light theme).
fn side_fill(c: Colors) -> Color {
    if is_light(c) { mix(c.panel, c.bg_low, 0.45) } else { mix(c.panel, c.bg_low, 0.42) }
}

fn two_pane<'a>(x: &Ctx<'a>, snapshot: &'a Snapshot) -> El<'a> {
    let c = x.c;
    let side_w = if x.app.viewport.width >= 1200.0 { 296.0 } else { 256.0 };
    let fill = side_fill(c);
    let sidebar = container(sidebar(x, snapshot)).width(side_w).height(Length::Fill).style(move |_| container::Style {
        background: Some(fill.into()),
        border: Border { radius: iced::border::left(ui::RADIUS), ..Default::default() },
        ..Default::default()
    });
    let divider = container(Space::new()).width(1).height(Length::Fill).style(move |_| container::Style {
        background: Some(c.line.into()),
        ..Default::default()
    });
    let state = &x.app.friends;
    let right: El<'a> = match &state.pane {
        Some(Pane::Friend { .. }) => match state.friend() {
            Some(friend) => conversation(x, snapshot, friend),
            None => welcome(x),
        },
        Some(Pane::Mailbox) => mailbox(x, snapshot),
        Some(Pane::Blocked) => blocked(x, snapshot),
        None => welcome(x),
    };
    container(row![sidebar, divider, container(right).width(Length::Fill).height(Length::Fill)])
        .width(Length::Fill)
        .height(Length::Fill)
        .style(card_style(c))
        .into()
}

#[derive(Clone, PartialEq, Eq)]
struct AccountChoice {
    key: String,
    label: String,
}
impl std::fmt::Display for AccountChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A drop-down draws its own labels: masked in privacy mode.
        f.write_str(&privacy::mask(&self.label))
    }
}

/// The signed-in account: avatar with your presence, name, and under it
/// `status` (your status control, or a line of text). With several retail
/// accounts the whole block is also the account switcher: a drop-down lies
/// under the face and opens wherever the face has no control of its own.
fn account_switcher<'a>(x: &Ctx<'a>, status: El<'a>, presence: Option<Presence>, trailing: Option<El<'a>>) -> El<'a> {
    let c = x.c;
    let app = x.app;
    let light = x.light();
    let label = x.account.map(|a| a.account.label.clone()).unwrap_or_default();
    let choices: Vec<AccountChoice> = app
        .accounts
        .iter()
        .filter(|a| a.account.mode == crate::AccountMode::Retail)
        .map(|a| AccountChoice { key: a.key.clone(), label: a.account.label.clone() })
        .collect();
    let switchable = choices.len() > 1;
    let mut name = row![if label.is_empty() {
        text(t("No account")).font(SEMI).size(14).color(c.ink).into()
    } else {
        name_text(&label, 14.0, SEMI, c.ink)
    }]
    .spacing(6)
    .align_y(Alignment::Center);
    if switchable {
        name = name.push(icon(Icon::ChevronDown, 12.0, c.sub));
    }
    let mut face = row![
        avatar(c, &label, 38.0, true, presence, side_fill(c)),
        column![name, status].spacing(0).width(Length::Fill),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    if let Some(trailing) = trailing {
        face = face.push(trailing);
    }
    let height = 54.0;
    let face = container(face).padding([0, 6]).height(height).center_y(height).width(Length::Fill);
    if !switchable {
        return face.into();
    }
    let selected = choices.iter().find(|choice| app.friends.account.as_ref() == Some(&choice.key)).cloned();
    let picker = crate::controller_nav::pick_list(choices, selected, |choice: AccountChoice| Message::FriendsAccount(choice.key))
        .padding([8, 8])
        .text_size(14)
        .font(BODY)
        .width(Length::Fill)
        .handle(pick_list::Handle::None)
        .style(profile_pick_style(Color::TRANSPARENT, Border { radius: CONTROL_R.into(), ..Default::default() }, true, light))
        .menu_style(profile_menu_style(light));
    stack![ui::profile_tall(picker, height), face].into()
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct OwnStatus(NativeControl);
impl std::fmt::Display for OwnStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            NativeControl::Away => t("Away"),
            NativeControl::Invisible => t("Invisible"),
            _ => t("Visible"),
        })
    }
}
const OWN_STATUSES: [OwnStatus; 3] = [OwnStatus(NativeControl::Visible), OwnStatus(NativeControl::Away), OwnStatus(NativeControl::Invisible)];

/// A drop-down drawn as quiet text with a small caret, for inline choices.
fn inline_pick(c: Colors) -> impl Fn(&Theme, pick_list::Status) -> pick_list::Style {
    move |_, status| pick_list::Style {
        // Its own label is drawn above it (`sidebar`).
        text_color: Color::TRANSPARENT,
        placeholder_color: Color::TRANSPARENT,
        handle_color: c.sub,
        background: match status {
            pick_list::Status::Hovered | pick_list::Status::Opened { .. } => with_alpha(c.select, 0.7).into(),
            _ => Color::TRANSPARENT.into(),
        },
        border: Border { radius: 5.0.into(), ..Default::default() },
    }
}

fn sidebar<'a>(x: &Ctx<'a>, snapshot: &'a Snapshot) -> El<'a> {
    let c = x.c;
    let app = x.app;
    let state = &app.friends;
    let light = x.light();
    // Own status: the Viewer's three choices, inline under the name.
    let selected = match snapshot.own_mode {
        Some(1) => Some(OWN_STATUSES[1]),
        Some(4) => Some(OWN_STATUSES[2]),
        Some(0) => Some(OWN_STATUSES[0]),
        _ => None,
    };
    let presence = match snapshot.own_mode {
        Some(0) => Presence::Online,
        Some(1) => Presence::Away,
        _ => Presence::Hollow,
    };
    let status: El<'a> = if x.idle && !x.busy {
        // The drop-down lies under its own label and caret, so they line up
        // with the name above; a click anywhere on them opens it.
        let shown = selected.map_or_else(|| t("Set your status").to_owned(), |s| s.to_string());
        stack![
            crate::controller_nav::pick_list(OWN_STATUSES, selected, |choice: OwnStatus| Message::FriendsStatus(choice.0))
                .font(BODY)
                .text_size(12)
                .padding(iced::Padding { top: 1.0, right: 18.0, bottom: 1.0, left: 0.0 })
                .handle(pick_list::Handle::None)
                .style(inline_pick(c))
                .menu_style(profile_menu_style(light)),
            container(row![text(shown).font(BODY).size(12).color(c.sub), icon(Icon::ChevronDown, 11.0, c.sub)].spacing(4).align_y(Alignment::Center))
                .padding(iced::Padding { top: 1.0, right: 0.0, bottom: 1.0, left: 0.0 }),
        ]
        .into()
    } else {
        let line = match (x.in_game, selected) {
            (true, _) => t("In FINAL FANTASY XI"),
            (false, Some(OwnStatus(NativeControl::Away))) => t("Away"),
            (false, Some(OwnStatus(NativeControl::Invisible))) => t("Invisible"),
            (false, Some(_)) => t("Visible"),
            (false, None) => t("Waiting for PlayOnline status"),
        };
        text(line).font(BODY).size(12).color(c.sub).into()
    };
    let options = hint(
        ui::icon_control(c, if state.status_options { Icon::ChevronDown } else { Icon::Settings }, false, Some(Message::FriendsStatusOptions)),
        t("Status options"),
        Position::Bottom,
    );
    let mut me = column![account_switcher(x, status, Some(presence), Some(options))].spacing(8);
    if state.status_options {
        let mut extra = column![
            crate::controller_nav::checkbox(!state.skip_groups)
                .label(t("Also set for my groups"))
                .on_toggle(Message::FriendsStatusGroups)
                .size(15)
                .text_size(12)
                .style(ui::check),
        ]
        .spacing(8);
        let picker_style2 = profile_pick_style(c.well, Border { color: c.hairline, width: 1.0, radius: CONTROL_R.into() }, false, light);
        if let Some(account) = x.account {
            let current = AutoAway(app.settings.auto_away_for(&account.account));
            extra = extra.push(
                crate::controller_nav::pick_list(&AUTO_AWAY_OPTIONS[..], Some(current), |choice: AutoAway| Message::FriendsAutoAway(choice.0))
                    .font(BODY)
                    .text_size(13)
                    .padding([7, 10])
                    .width(Length::Fill)
                    .style(move |theme, status| pick_list::Style { handle_color: c.sub, ..picker_style2(theme, status) })
                    .menu_style(profile_menu_style(light)),
            );
            extra = extra.push(text(t("Set Away when Crystal sees no input while FFXI is closed. Applies at the next sign-in.")).font(BODY).size(11).color(c.sub));
        }
        me = me.push(container(extra).padding([4, 4]));
    }

    let search = text_input(t("Find a friend…"), &state.query)
        .on_input(Message::FriendsQuery)
        .font(BODY)
        .size(13)
        .padding([8, 12])
        .style(ui::input);

    let (friends, _) = super::split_lists(&snapshot.entries, &state.query);
    let (online, offline): (Vec<&Friend>, Vec<&Friend>) = friends.into_iter().partition(|f| f.online == Some(true));
    let mut list = column![].spacing(2);
    for (index, (label, group)) in [(t("Online"), online), (t("Offline"), offline)].into_iter().enumerate() {
        let folded = state.collapsed[index];
        list = list.push(
            crate::controller_nav::button(
                row![
                    icon(if folded { Icon::ChevronRight } else { Icon::ChevronDown }, 12.0, c.sub),
                    text(label).font(SEMI).size(11.5).color(c.sub),
                    text(group.len().to_string()).font(BODY).size(11.5).color(with_alpha(c.sub, 0.8)),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .padding([8, 8])
            .width(Length::Fill)
            .style(|_, _| button::Style::default())
            .on_press(Message::FriendsSection(index)),
        );
        if !folded {
            for friend in group {
                list = list.push(friend_row(x, friend));
            }
        }
    }
    if !snapshot.entries.iter().any(|f| !f.alternate) {
        list = list.push(container(text(t("No friends returned for this account.")).font(BODY).size(12).color(c.sub)).padding([4, 8]));
    } else if !state.query.is_empty() && list_is_empty(&snapshot.entries, &state.query) {
        list = list.push(container(text(t("No friends match your search.")).font(BODY).size(12).color(c.sub)).padding([4, 8]));
    }

    let unread = snapshot.social.inbox_unread.unwrap_or(0) as usize;
    let mailbox = nav_row(x, Icon::Inbox, t("Mailbox"), (unread > 0 || snapshot.social.inbox_changed).then_some(if unread > 0 { unread.to_string() } else { t("New").to_owned() }), state.pane == Some(Pane::Mailbox), Pane::Mailbox);
    let blocked_count = snapshot.entries.iter().filter(|f| f.alternate).count();
    let blocked_row = nav_row(x, Icon::Ban, t("Blocked"), (blocked_count > 0).then(|| blocked_count.to_string()), state.pane == Some(Pane::Blocked), Pane::Blocked);

    column![
        container(me).padding(iced::Padding { top: 12.0, right: 12.0, bottom: 12.0, left: 12.0 }),
        container(search).padding([0, 12]),
        Space::new().height(6),
        scrollable(container(list).padding([0, 8]))
            .direction(scrollable::Direction::Vertical(scrollable::Scrollbar::new().width(4).scroller_width(4).spacing(2)))
            .style(ui::thin_scrollbar)
            .height(Length::Fill),
        hline(),
        container(column![mailbox, blocked_row].spacing(2)).padding(8),
    ]
    .height(Length::Fill)
    .into()
}

fn list_is_empty(entries: &[Friend], query: &str) -> bool {
    super::split_lists(entries, query).0.is_empty()
}

/// A sidebar entry's look: tinted when selected, a softer tint on hover.
fn item_style(c: Colors, selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hot = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: if selected {
                Some(with_alpha(c.select, if is_light(c) { 0.95 } else { 0.85 }).into())
            } else if hot {
                Some(with_alpha(c.select, 0.45).into())
            } else {
                None
            },
            text_color: c.ink,
            border: Border { radius: CONTROL_R.into(), ..Default::default() },
            ..Default::default()
        }
    }
}

fn display_name(friend: &Friend) -> String {
    if friend.name.is_empty() { t("Unnamed handle").to_owned() } else { friend.name.clone() }
}

fn presence_of(friend: &Friend) -> Presence {
    match friend.online {
        Some(true) => Presence::Online,
        Some(false) => Presence::Offline,
        None => Presence::Unknown,
    }
}

/// The line under a friend's name: where they are when online, else their state.
fn secondary<'a>(x: &Ctx<'a>, friend: &Friend, size: f32) -> El<'a> {
    let c = x.c;
    let line = match friend.online {
        Some(true) => friend.location.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| t("Online").to_owned()),
        Some(false) => t("Offline").to_owned(),
        None => t("Waiting for PlayOnline status").to_owned(),
    };
    let names: Vec<&str> = friend.character.iter().map(String::as_str).collect();
    privacy::line(privacy::split(&line, &names), size, BODY, c.sub)
}

fn friend_row<'a>(x: &Ctx<'a>, friend: &'a Friend) -> El<'a> {
    let c = x.c;
    let state = &x.app.friends;
    let selected = state.friend().is_some_and(|f| f.slot == friend.slot && f.name == friend.name);
    let unread = state.unread_from(friend);
    let shown = display_name(friend);
    let strong = unread > 0 || selected;
    let mut line = row![
        avatar(c, &friend.name, 34.0, false, Some(presence_of(friend)), if selected { mix(side_fill(c), c.select, 0.85) } else { side_fill(c) }),
        column![
            name_text(&shown, 14.0, if unread > 0 { SEMI } else { BODY }, if strong { c.ink } else { mix(c.ink, c.sub, 0.3) }),
            secondary(x, friend, 12.0),
        ]
        .spacing(1)
        .width(Length::Fill),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    if unread > 0 {
        line = line.push(badge(c, &unread.to_string()));
    }
    crate::controller_nav::button(line)
        .padding([6, 8])
        .width(Length::Fill)
        .style(item_style(c, selected))
        .on_press(Message::FriendsSelect(Some(Pane::Friend { slot: friend.slot, name: friend.name.clone() })))
        .into()
}

fn nav_row<'a>(x: &Ctx<'a>, glyph: Icon, label: &'a str, count: Option<String>, selected: bool, pane: Pane) -> El<'a> {
    let c = x.c;
    let mut line = row![
        container(icon(glyph, 17.0, if selected { c.accent } else { c.sub })).center(34),
        text(label).font(if selected { SEMI } else { BODY }).size(14).color(c.ink).width(Length::Fill),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    if let Some(count) = count {
        line = line.push(if pane == Pane::Mailbox { badge(c, &count) } else { text(count).font(BODY).size(12).color(c.sub).into() });
    }
    crate::controller_nav::button(line).padding([3, 8]).width(Length::Fill).style(item_style(c, selected)).on_press(Message::FriendsSelect(Some(pane))).into()
}

/// An unread count: a small accent pill.
fn badge<'a>(c: Colors, value: &str) -> El<'a> {
    container(text(value.to_owned()).font(SEMI).size(11).color(c.play_text))
        .padding([1, 7])
        .style(move |_| container::Style {
            background: Some(c.play.into()),
            border: Border { radius: 9.0.into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Presence {
    Online,
    Away,
    Offline,
    /// No status received yet, or invisible: an outline only.
    Unknown,
    Hollow,
}

/// Quiet tints for avatars, picked by name so a friend keeps one colour.
const AVATAR_TINTS: [u32; 6] = [0x5B7FA6, 0x4E8C8A, 0x6E8F6A, 0xA08A5C, 0x8A6F96, 0x6B7A8F];

fn tint(value: u32) -> Color {
    Color::from_rgb8((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

/// A round avatar: the name's initial on a quiet tint (your own: the accent
/// with the person glyph), and a presence dot whose shape, not only colour,
/// says the state: filled online, half for away, an outline otherwise.
fn avatar<'a>(c: Colors, name: &str, size: f32, mine: bool, presence: Option<Presence>, ring: Color) -> El<'a> {
    let hash = name.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    let (fill, ink) = if mine {
        (c.play, c.play_text)
    } else if privacy::on() {
        (c.select, c.sub)
    } else {
        (mix(c.select, tint(AVATAR_TINTS[hash as usize % AVATAR_TINTS.len()]), if is_light(c) { 0.35 } else { 0.55 }), c.ink)
    };
    let initial = name.chars().next().map(|ch| ch.to_uppercase().to_string()).unwrap_or_default();
    let glyph: El<'a> = if mine || privacy::on() || initial.is_empty() {
        icon(Icon::User, size * 0.48, ink)
    } else {
        text(initial).font(SEMI).size(size * 0.42).color(ink).into()
    };
    let disc = container(glyph).center(size).style(move |_| container::Style {
        background: Some(fill.into()),
        border: Border { radius: (size / 2.0).into(), ..Default::default() },
        ..Default::default()
    });
    let Some(presence) = presence else { return disc.into() };
    let dot = (size * 0.32).max(10.0);
    let ok = dl_ok(c);
    let away = if is_light(c) { tint(0xB7862F) } else { tint(0xD9B060) };
    let (inner, edge, width) = match presence {
        Presence::Online => (ok, ring, 2.0),
        Presence::Away => (away, ring, 2.0),
        Presence::Offline => (ring, with_alpha(c.sub, 0.9), 2.0),
        Presence::Unknown | Presence::Hollow => (ring, with_alpha(c.sub, 0.5), 1.5),
    };
    let mark = container(Space::new().width(dot).height(dot)).style(move |_| container::Style {
        background: Some(inner.into()),
        border: Border { color: edge, width, radius: (dot / 2.0).into() },
        ..Default::default()
    });
    // Away: an outlined dot with a filled half moon, readable without colour.
    let mark: El<'a> = if presence == Presence::Away {
        container(container(Space::new().width(dot * 0.36).height(dot * 0.36)).style(move |_| container::Style {
            background: Some(ring.into()),
            border: Border { radius: (dot * 0.18).into(), ..Default::default() },
            ..Default::default()
        }))
        .width(dot)
        .height(dot)
        .padding(iced::Padding { top: dot * 0.14, left: dot * 0.14, right: 0.0, bottom: 0.0 })
        .style(move |_| container::Style {
            background: Some(away.into()),
            border: Border { color: ring, width: 2.0, radius: (dot / 2.0).into() },
            ..Default::default()
        })
        .into()
    } else if presence == Presence::Online {
        // The ring around a filled dot.
        container(Space::new().width(dot).height(dot)).style(move |_| container::Style {
            background: Some(ok.into()),
            border: Border { color: ring, width: 2.0, radius: (dot / 2.0).into() },
            ..Default::default()
        })
        .into()
    } else {
        // An outline on the ring colour, in a ring of its own.
        container(mark).padding(1.5).style(move |_| container::Style {
            background: Some(ring.into()),
            border: Border { radius: (dot / 2.0 + 1.5).into(), ..Default::default() },
            ..Default::default()
        })
        .into()
    };
    stack![
        disc,
        container(mark).width(size).height(size).align_x(Alignment::End).align_y(Alignment::End)
    ]
    .into()
}

fn round_icon<'a>(c: Colors, glyph: Icon, size: f32) -> El<'a> {
    container(icon(glyph, size * 0.42, c.accent)).center(size).style(move |_| container::Style {
        background: Some(c.tint.into()),
        border: Border { radius: (size / 2.0).into(), ..Default::default() },
        ..Default::default()
    })
    .into()
}

/// A bare 36 px icon button for pane headers.
fn icon_button<'a>(c: Colors, glyph: Icon, message: Option<Message>, label: &'a str) -> El<'a> {
    let enabled = message.is_some();
    hint(
        crate::controller_nav::button(container(icon(glyph, 17.0, if enabled { c.sub } else { with_alpha(c.sub, 0.4) })).center(36))
            .padding(0)
            .style(move |_, status| button::Style {
                background: matches!(status, button::Status::Hovered | button::Status::Pressed).then(|| with_alpha(c.select, 0.8).into()),
                border: Border { radius: CONTROL_R.into(), ..Default::default() },
                ..Default::default()
            })
            .on_press_maybe(message),
        label,
        Position::Bottom,
    )
}

/// A pane's header row: identity on the left, actions on the right.
fn pane_header<'a>(lead: El<'a>, actions: Vec<El<'a>>) -> El<'a> {
    let mut line = row![container(lead).width(Length::Fill)].spacing(4).align_y(Alignment::Center);
    for action in actions {
        line = line.push(action);
    }
    container(line).padding([0, 16]).height(64).center_y(64).width(Length::Fill).into()
}

/// The centred empty state of a pane.
fn empty<'a>(c: Colors, glyph: Icon, title: &'a str, body: &'a str) -> El<'a> {
    container(
        column![
            round_icon(c, glyph, 56.0),
            Space::new().height(4),
            text(title).font(SEMI).size(16).color(c.ink).align_x(Alignment::Center),
            container(text(body).font(BODY).size(13).color(c.sub).align_x(Alignment::Center)).max_width(360),
        ]
        .spacing(6)
        .align_x(Alignment::Center),
    )
    .center(Length::Fill)
    .padding(24)
    .into()
}

fn welcome<'a>(x: &Ctx<'a>) -> El<'a> {
    empty(x.c, Icon::MessageCircle, t("Choose a friend to see your messages"), t("Your PlayOnline mail with each friend appears here as one conversation."))
}

// ---- Conversation ----------------------------------------------------------------

fn conversation<'a>(x: &Ctx<'a>, snapshot: &'a Snapshot, friend: &'a Friend) -> El<'a> {
    let c = x.c;
    let state = &x.app.friends;
    let social = &snapshot.social;
    let shown = display_name(friend);
    let lead = row![
        avatar(c, &friend.name, 38.0, false, Some(presence_of(friend)), c.panel),
        column![name_text(&shown, 16.0, SEMI, c.ink), secondary(x, friend, 12.0)].spacing(1),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    let checking = x.busy && social.action.as_deref() == Some("check");
    let actions = vec![
        icon_button(
            c,
            Icon::Refresh,
            x.ready.then_some(Message::FriendsSocial(crate::social::Request::Check {})),
            if checking { t("Checking…") } else { t("Check for new messages") },
        ),
        icon_button(c, Icon::UserMinus, (x.ready && friend.uid.is_some()).then(|| Message::FriendsRemove(friend.clone())), t("Remove from friends")),
    ];
    let mut pane = column![pane_header(lead.into(), actions), hline()];
    if let Some(remove) = state.remove.as_ref().filter(|f| !f.alternate) {
        pane = pane.push(container(confirm_remove(x, remove)).padding([12, 16]));
    }
    if let Some(error) = &social.error {
        pane = pane.push(container(banner(x, Icon::AlertCircle, &crate::social::localize(error), true)).padding(BANNER_PAD));
    }
    // A plain "sent" is the message appearing; other notices still show.
    if let Some(notice) = social.notice.as_ref().filter(|n| n.as_str() != "Message sent by PlayOnline.") {
        pane = pane.push(container(banner(x, Icon::CheckCircle, &crate::social::localize(notice), false)).padding(BANNER_PAD));
    }
    pane = pane.push(container(messages(x, friend)).height(Length::Fill));
    pane = pane.push(hline());
    pane = pane.push(if x.in_game {
        in_game_notice(x)
    } else {
        match &state.composer {
            Some(composer) => composer_view(x, composer),
            None => container(text(t("This friend can't receive messages from Crystal.")).font(BODY).size(13).color(c.sub)).padding([16, 20]).into(),
        }
    });
    pane.height(Length::Fill).into()
}

fn confirm_remove<'a>(x: &Ctx<'a>, friend: &Friend) -> El<'a> {
    let c = x.c;
    let shown = display_name(friend);
    let (question, action) = if friend.alternate {
        (tf("Unblock {name}? Their messages will reach you again.", &[("name", &shown)]), t("Unblock"))
    } else {
        (tf("Remove {name} from your PlayOnline friends?", &[("name", &shown)]), t("Remove"))
    };
    container(
        row![
            container(private_text(x.app, &question, 13.0, BODY, c.ink)).width(Length::Fill),
            control(c, Tone::Secondary, None, t("Cancel"), Some(Message::FriendsDraftClose), None),
            control(c, if friend.alternate { Tone::Primary } else { Tone::Danger }, None, action, x.ready.then_some(Message::FriendsRemoveConfirm), None),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([10, 14])
    .width(Length::Fill)
    .style(soft_tile(c))
    .into()
}

/// A run of messages from one side: avatar and name once, then each message.
struct Group<'a> {
    mine: bool,
    time: Option<chrono::DateTime<chrono::Local>>,
    items: Vec<El<'a>>,
}

fn day_label(day: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    let today = chrono::Local::now().date_naive();
    if day == today {
        return t("Today").to_owned();
    }
    if Some(day) == today.pred_opt() {
        return t("Yesterday").to_owned();
    }
    match crate::i18n::current() {
        crate::i18n::Lang::Ja => {
            const WEEKDAYS: [&str; 7] = ["月", "火", "水", "木", "金", "土", "日"];
            let weekday = WEEKDAYS[day.weekday().num_days_from_monday() as usize];
            if day.year() == today.year() {
                format!("{}月{}日（{weekday}）", day.month(), day.day())
            } else {
                format!("{}年{}月{}日（{weekday}）", day.year(), day.month(), day.day())
            }
        }
        crate::i18n::Lang::En => {
            if day.year() == today.year() { day.format("%A, %B %-d").to_string() } else { day.format("%A, %B %-d, %Y").to_string() }
        }
    }
}

/// A centred label on a hairline: a day, or the "new" mark.
fn divider<'a>(label: String, ink: Color, line: Color) -> El<'a> {
    let rule = move || container(Space::new()).width(Length::Fill).height(1).style(move |_| container::Style { background: Some(line.into()), ..Default::default() });
    row![rule(), text(label).font(SEMI).size(11).color(ink), rule()].spacing(10).align_y(Alignment::Center).padding([10, 0]).into()
}

fn skeleton<'a>(c: Colors) -> El<'a> {
    let bar = move |width: f32| {
        container(Space::new().width(width).height(9)).style(move |_| container::Style {
            background: Some(with_alpha(c.sub, 0.18).into()),
            border: Border { radius: 4.5.into(), ..Default::default() },
            ..Default::default()
        })
    };
    column![bar(260.0), bar(170.0)].spacing(7).padding([4, 0]).into()
}

/// One message's subject (small, muted) and text.
fn message_item<'a>(c: Colors, entry: &Entry, body: Option<&Body>) -> El<'a> {
    let subject = body.map_or(entry.subject.as_str(), |b| b.subject.as_str()).trim().to_owned();
    let mut item = column![].spacing(2);
    if !subject.is_empty() && !body.is_some_and(Body::unsupported) {
        item = item.push(privacy::name(text(subject.clone()).font(SEMI).size(12).color(c.sub), &subject, 12.0, c.sub));
    }
    item = item.push(match body {
        Some(body) if body.unsupported() => text(crate::social::localize(&body.message))
            .font(Font { style: iced::font::Style::Italic, ..BODY })
            .size(13)
            .color(c.sub)
            .into(),
        Some(body) => privacy::name(
            text(body.message.clone()).font(BODY).size(14).line_height(1.45).color(c.ink),
            &body.message,
            14.0,
            c.ink,
        ),
        None => skeleton(c),
    });
    item.into()
}

fn group_view<'a>(x: &Ctx<'a>, friend: &Friend, group: Group<'a>) -> El<'a> {
    let c = x.c;
    let shown = display_name(friend);
    let who: El<'a> = if group.mine {
        text(t("You")).font(SEMI).size(14).color(c.accent).into()
    } else {
        name_text(&shown, 14.0, SEMI, c.ink)
    };
    let mut head = row![who].spacing(8).align_y(Alignment::Center);
    if let Some(time) = group.time {
        head = head.push(text(time.format("%H:%M").to_string()).font(BODY).size(11).color(with_alpha(c.sub, 0.9)));
    }
    let mut body = column![head].spacing(4).width(Length::Fill);
    for item in group.items {
        body = body.push(item);
    }
    let own_label = x.account.map(|a| a.account.label.clone()).unwrap_or_default();
    row![
        if group.mine { avatar(c, &own_label, 36.0, true, None, c.panel) } else { avatar(c, &friend.name, 36.0, false, None, c.panel) },
        body
    ]
    .spacing(12)
    .padding([6, 0])
    .into()
}

fn messages<'a>(x: &Ctx<'a>, friend: &'a Friend) -> El<'a> {
    let c = x.c;
    let state = &x.app.friends;
    let thread = state.thread(friend);
    let outgoing = state.outgoing.as_ref().filter(|o| Some(o.uid) == friend.uid);
    if thread.is_empty() && outgoing.is_none() {
        let loading = state.mail.listed[0].is_none() || (state.syncing() && state.mail.listed[2].is_none());
        return if loading && !x.in_game {
            empty(c, Icon::MessageCircle, t("Loading messages…"), t("Your stored PlayOnline mail with this friend is being read."))
        } else {
            empty(c, Icon::MessageCircle, t("No messages yet"), t("Send a message to start the conversation. It is delivered as PlayOnline mail."))
        };
    }
    let mut list = column![].spacing(0);
    if state.mail.listed.iter().flatten().any(|page| page.len() >= 32) {
        list = list.push(
            container(text(t("Showing your newest PlayOnline mail. Older messages are in the Mailbox.")).font(BODY).size(12).color(c.sub))
                .width(Length::Fill)
                .align_x(Alignment::Center)
                .padding([4, 0]),
        );
    }
    let fresh = thread.iter().position(|e| !e.mine() && state.mail.fresh.contains(&e.key));
    let mut day: Option<chrono::NaiveDate> = None;
    let mut group: Option<Group<'a>> = None;
    let mut last: Option<(bool, Option<i64>)> = None;
    for (index, entry) in thread.iter().enumerate() {
        let time = message_time(entry.stamp);
        let mut breaks = false;
        if let Some(time) = time {
            if day != Some(time.date_naive()) {
                if let Some(done) = group.take() {
                    list = list.push(group_view(x, friend, done));
                }
                day = Some(time.date_naive());
                list = list.push(divider(day_label(time.date_naive()), c.sub, with_alpha(c.line, 0.8)));
                breaks = true;
            }
        }
        if fresh == Some(index) {
            if let Some(done) = group.take() {
                list = list.push(group_view(x, friend, done));
            }
            list = list.push(divider(t("New messages").to_owned(), c.accent, with_alpha(c.accent, 0.45)));
            breaks = true;
        }
        let seconds = time.map(|t| t.timestamp());
        let continues = !breaks
            && last.is_some_and(|(mine, at)| mine == entry.mine() && match (at, seconds) {
                (Some(a), Some(b)) => b - a < 10 * 60,
                _ => true,
            });
        if !continues {
            if let Some(done) = group.take() {
                list = list.push(group_view(x, friend, done));
            }
            group = Some(Group { mine: entry.mine(), time, items: Vec::new() });
        }
        if let Some(group) = &mut group {
            group.items.push(message_item(c, entry, state.mail.bodies.get(&entry.key)));
        }
        last = Some((entry.mine(), seconds));
    }
    if let Some(outgoing) = outgoing {
        if !last.is_some_and(|(mine, _)| mine) {
            if let Some(done) = group.take() {
                list = list.push(group_view(x, friend, done));
            }
            group = Some(Group { mine: true, time: None, items: Vec::new() });
        }
        let mut item = column![].spacing(2);
        if !outgoing.subject.is_empty() {
            item = item.push(privacy::name(text(outgoing.subject.clone()).font(SEMI).size(12).color(c.sub), &outgoing.subject, 12.0, c.sub));
        }
        let ink = if outgoing.sent { c.ink } else { with_alpha(c.ink, 0.6) };
        item = item.push(privacy::name(text(outgoing.message.clone()).font(BODY).size(14).line_height(1.45).color(ink), &outgoing.message, 14.0, ink));
        if !outgoing.sent {
            item = item.push(text(t("Sending…")).font(BODY).size(11).color(c.sub));
        }
        if let Some(group) = &mut group {
            group.items.push(item.into());
        }
    }
    if let Some(done) = group.take() {
        list = list.push(group_view(x, friend, done));
    }
    scrollable(container(list).padding([12, 20]))
        .direction(scrollable::Direction::Vertical(scrollable::Scrollbar::new().width(4).scroller_width(4).spacing(2)))
        .anchor_bottom()
        .style(ui::thin_scrollbar)
        .height(Length::Fill)
        .into()
}

fn in_game_notice<'a>(x: &Ctx<'a>) -> El<'a> {
    let c = x.c;
    container(
        container(
            row![
                round_icon(c, Icon::Gamepad, 36.0),
                column![
                    text(t("FINAL FANTASY XI is running")).font(SEMI).size(14).color(c.ink),
                    text(t("Messages and friend changes open again when FINAL FANTASY XI closes. Presence keeps updating while you play."))
                        .font(BODY)
                        .size(12)
                        .color(c.sub),
                ]
                .spacing(2)
                .width(Length::Fill),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .padding([12, 14])
        .width(Length::Fill)
        .style(soft_tile(c)),
    )
    .padding([14, 16])
    .into()
}

fn composer_view<'a>(x: &Ctx<'a>, composer: &'a Composer) -> El<'a> {
    use iced::keyboard::{key::Named, Key};
    let c = x.c;
    let state = &x.app.friends;
    let sending = state.outgoing.as_ref().is_some_and(|o| !o.sent);
    let can_send = x.ready && !sending && state.send_request().is_some();
    let editable = !x.busy;
    let hide = privacy::on();
    let editor = text_editor(&composer.text)
        .placeholder(t("Write a PlayOnline message…"))
        .size(14)
        .padding([10, 12])
        // Grows with its lines up to six, then scrolls inside.
        .height(19.0 * composer.text.line_count().clamp(1, 6) as f32 + 22.0)
        .key_binding(move |press| {
            if matches!(press.key, Key::Named(Named::Enter)) && !press.modifiers.shift() {
                return can_send.then_some(text_editor::Binding::Custom(Message::FriendsSend));
            }
            text_editor::Binding::from_key_press(press)
        })
        .style(move |_, _| text_editor::Style {
            background: Color::TRANSPARENT.into(),
            border: Border::default(),
            placeholder: with_alpha(c.sub, 0.8),
            // Privacy mode draws the draft blurred over a hidden one.
            value: if hide { Color::TRANSPARENT } else { c.ink },
            selection: with_alpha(c.accent, 0.3),
        });
    let editor = if editable { editor.on_action(Message::FriendsMessageEdit) } else { editor };
    let draft = composer.text.text();
    let field: El<'a> = if hide && !draft.trim().is_empty() {
        stack![
            editor,
            container(privacy::name(text(draft.clone()).font(BODY).size(14).color(c.ink), &draft, 14.0, c.ink)).padding([10, 12]).width(Length::Fill)
        ]
        .into()
    } else {
        editor.into()
    };
    let send = hint(
        crate::controller_nav::button(container(icon(Icon::Send, 16.0, if can_send { c.play_text } else { with_alpha(c.sub, 0.6) })).center(34))
            .padding(0)
            .style(ui::control_style(c, Tone::Primary))
            .on_press_maybe(can_send.then_some(Message::FriendsSend)),
        t("Send"),
        Position::Top,
    );
    let shell = container(row![container(field).width(Length::Fill), send].spacing(6).align_y(Alignment::End))
        .padding(4)
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(c.field.into()),
            border: Border { color: c.line, width: 1.0, radius: 10.0.into() },
            ..Default::default()
        });
    let mut column = column![].spacing(8);
    if composer.show_subject {
        let subject = text_input(t("Subject (optional)"), &composer.subject)
            .secure(hide)
            .font(BODY)
            .size(13)
            .padding([8, 12])
            .style(ui::input);
        column = column.push(if editable { subject.on_input(Message::FriendsSubject) } else { subject });
    }
    column = column.push(shell);
    // Under the box: the subject switch, then the keys or the limits.
    let text_len = super::cp932_len(draft.trim_end_matches('\n'));
    let subject_len = super::cp932_len(&composer.subject);
    let note: El<'a> = match (text_len, subject_len) {
        (None, _) | (_, None) if composer.show_subject || text_len.is_none() => {
            text(t("Contains characters PlayOnline can't send.")).font(BODY).size(11).color(dl_danger(c)).into()
        }
        (Some(n), _) if n > TEXT_MAX * 3 / 4 => text(tf("{used} of {limit} bytes", &[("used", &n), ("limit", &TEXT_MAX)]))
            .font(BODY)
            .size(11)
            .color(if n > TEXT_MAX { dl_danger(c) } else { c.sub })
            .into(),
        (_, Some(n)) if composer.show_subject && n > SUBJECT_MAX => text(tf("Subject: {used} of {limit} bytes", &[("used", &n), ("limit", &SUBJECT_MAX)]))
            .font(BODY)
            .size(11)
            .color(dl_danger(c))
            .into(),
        _ => text(t("Enter to send · Shift+Enter for a new line")).font(BODY).size(11).color(with_alpha(c.sub, 0.85)).into(),
    };
    let toggle = crate::controller_nav::button(text(if composer.show_subject { t("Remove subject") } else { t("Add subject") }).font(SEMI).size(12).color(c.accent))
        .padding(0)
        .style(|_, _| button::Style::default())
        .on_press_maybe(editable.then_some(Message::FriendsAddSubject(!composer.show_subject)));
    column = column.push(row![toggle, Space::new().width(Length::Fill), note].align_y(Alignment::Center).padding([0, 4]));
    container(column).padding([12, 16]).into()
}

// ---- Mailbox and block list --------------------------------------------------------

fn folder_tab<'a>(c: Colors, label: &'a str, active: bool, message: Option<Message>) -> El<'a> {
    crate::controller_nav::button(text(label).font(if active { SEMI } else { BODY }).size(13).color(if active { c.accent } else { c.sub }))
        .padding([6, 12])
        .style(move |_, status| button::Style {
            background: if active {
                Some(c.select.into())
            } else if matches!(status, button::Status::Hovered) {
                Some(with_alpha(c.select, 0.45).into())
            } else {
                None
            },
            border: Border { radius: CONTROL_R.into(), ..Default::default() },
            ..Default::default()
        })
        .on_press_maybe(message)
        .into()
}

fn mailbox<'a>(x: &Ctx<'a>, snapshot: &'a Snapshot) -> El<'a> {
    use crate::social::Request;
    let c = x.c;
    let social = &snapshot.social;
    let enabled = x.ready;
    let lead = row![
        round_icon(c, Icon::Inbox, 38.0),
        column![
            text(t("Mailbox")).font(SEMI).size(16).color(c.ink),
            text(t("All your stored PlayOnline mail, from friends and others.")).font(BODY).size(12).color(c.sub),
        ]
        .spacing(1),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    let checking = x.busy && social.action.as_deref() == Some("check");
    let actions = vec![icon_button(c, Icon::Refresh, enabled.then_some(Message::FriendsSocial(Request::Check {})), if checking { t("Checking…") } else { t("Check for new messages") })];
    let page = social.page;
    let pages = social.total.div_ceil(32).max(1);
    let mut toolbar = row![
        tabs_row(c, social, enabled),
        Space::new().width(Length::Fill),
        text(tf("{total} messages · Page {page} of {pages}", &[("total", &social.total), ("page", &(page + 1)), ("pages", &pages)])).font(BODY).size(12).color(c.sub),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    // Paging only when there is more than one page.
    if pages > 1 {
        toolbar = toolbar
            .push(control(c, Tone::Secondary, None, t("Previous"), (enabled && page > 0).then(|| Message::FriendsSocial(Request::Inbox { folder: social.folder, page: page - 1 })), None))
            .push(control(c, Tone::Secondary, None, t("Next"), (enabled && page + 1 < pages).then(|| Message::FriendsSocial(Request::Inbox { folder: social.folder, page: page + 1 })), None));
    }
    let names = ui::identities(x.app);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let correspondent = |sender: &str, recipient: Option<u64>| -> El<'a> {
        let line = if social.folder == 2 {
            snapshot.entries.iter().find(|f| f.uid.is_some() && f.uid == recipient)
                .map_or_else(|| t("Sent message").to_owned(), |f| tf("To {name}", &[("name", &f.name)]))
        } else {
            tf("From {sender}", &[("sender", &sender)])
        };
        privacy::line(privacy::split(&line, &names), 14.0, SEMI, c.ink)
    };
    let mut list = column![].spacing(2);
    if let Some(opened) = &social.opened {
        let message = crate::social::localize(&opened.message);
        let mut card = column![correspondent(&opened.sender, opened.recipient)].spacing(6);
        if !opened.subject.is_empty() {
            card = card.push(privacy::name(text(opened.subject.clone()).font(SEMI).size(13).color(c.sub), &opened.subject, 13.0, c.sub));
        }
        card = card.push(privacy::name(text(message.clone()).font(BODY).size(14).line_height(1.45).color(c.ink), &message, 14.0, c.ink));
        list = list.push(container(card).padding([14, 16]).width(Length::Fill).style(soft_tile(c)));
        list = list.push(Space::new().height(8));
    }
    for header in &social.headers {
        let subject = if header.subject.is_empty() { t("No subject").to_owned() } else { header.subject.clone() };
        let subject_view: El<'a> = if header.subject.is_empty() {
            text(subject).font(BODY).size(12).color(c.sub).into()
        } else {
            privacy::name(text(subject.clone()).font(BODY).size(12).color(c.sub), &subject, 12.0, c.sub)
        };
        let dot: El<'a> = if header.unread {
            container(Space::new().width(8).height(8)).style(move |_| container::Style {
                background: Some(c.accent.into()),
                border: Border { radius: 4.0.into(), ..Default::default() },
                ..Default::default()
            })
            .into()
        } else {
            Space::new().width(8).into()
        };
        let line = row![
            dot,
            avatar(c, if social.folder == 2 { "" } else { &header.sender }, 32.0, social.folder == 2, None, c.panel),
            column![correspondent(&header.sender, header.recipient), subject_view].spacing(1).width(Length::Fill),
            icon(Icon::ChevronRight, 14.0, c.sub),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        list = list.push(
            crate::controller_nav::button(line)
                .padding([8, 10])
                .width(Length::Fill)
                .style(item_style(c, false))
                .on_press_maybe(enabled.then(|| Message::FriendsSocial(Request::Read { header: header.raw.clone() }))),
        );
    }
    if social.headers.is_empty() {
        list = list.push(container(text(t("New PlayOnline messages are downloaded automatically while you are signed in.")).font(BODY).size(13).color(c.sub)).padding([12, 10]));
    } else {
        list = list.push(container(text(t("Messages you read here remain saved in Crystal.")).font(BODY).size(11).color(c.sub)).padding([8, 10]));
    }
    let mut pane = column![pane_header(lead.into(), actions), hline()];
    if let Some(error) = &social.error {
        pane = pane.push(container(banner(x, Icon::AlertCircle, &crate::social::localize(error), true)).padding(BANNER_PAD));
    }
    if social.inbox_changed {
        pane = pane.push(container(banner(x, Icon::Inbox, t("New PlayOnline mail has arrived. Open Inbox to read it."), false)).padding(BANNER_PAD));
    }
    if x.in_game {
        pane = pane.push(in_game_notice(x));
    }
    pane = pane.push(container(toolbar).padding([12, 16]));
    pane = pane.push(
        scrollable(container(list).padding([0, 12]))
            .direction(scrollable::Direction::Vertical(scrollable::Scrollbar::new().width(4).scroller_width(4).spacing(2)))
            .style(ui::thin_scrollbar)
            .height(Length::Fill),
    );
    pane.height(Length::Fill).into()
}

fn tabs_row<'a>(c: Colors, social: &crate::social::State, enabled: bool) -> El<'a> {
    use crate::social::Request;
    let mut tabs = row![].spacing(4).align_y(Alignment::Center);
    for (folder, label) in [(0u8, t("Inbox")), (2, t("Sent")), (1, t("Archived"))] {
        tabs = tabs.push(folder_tab(c, label, social.folder == folder, enabled.then_some(Message::FriendsSocial(Request::Inbox { folder, page: 0 }))));
    }
    tabs.into()
}

fn blocked<'a>(x: &Ctx<'a>, snapshot: &'a Snapshot) -> El<'a> {
    let c = x.c;
    let state = &x.app.friends;
    let lead = row![
        round_icon(c, Icon::Ban, 38.0),
        column![
            text(t("Blocked")).font(SEMI).size(16).color(c.ink),
            text(t("People on your block list can't message you. Unblocking lets their messages reach you again.")).font(BODY).size(12).color(c.sub),
        ]
        .spacing(1),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    let mut pane = column![pane_header(lead.into(), Vec::new()), hline()];
    if let Some(remove) = state.remove.as_ref().filter(|f| f.alternate) {
        pane = pane.push(container(confirm_remove(x, remove)).padding(BANNER_PAD));
    }
    if let Some(error) = &snapshot.social.error {
        pane = pane.push(container(banner(x, Icon::AlertCircle, &crate::social::localize(error), true)).padding(BANNER_PAD));
    }
    if let Some(notice) = &snapshot.social.notice {
        pane = pane.push(container(banner(x, Icon::CheckCircle, &crate::social::localize(notice), false)).padding(BANNER_PAD));
    }
    let blocked: Vec<&Friend> = snapshot.entries.iter().filter(|f| f.alternate).collect();
    if blocked.is_empty() {
        return pane.push(empty(c, Icon::Ban, t("Nobody is blocked"), t("Players you block in PlayOnline appear here."))).height(Length::Fill).into();
    }
    let mut list = column![].spacing(2);
    for friend in blocked {
        let shown = display_name(friend);
        list = list.push(
            container(
                row![
                    avatar(c, &friend.name, 34.0, false, None, c.panel),
                    container(name_text(&shown, 14.0, BODY, c.ink)).width(Length::Fill),
                    control(c, Tone::Secondary, None, t("Unblock"), (x.ready && friend.uid.is_some()).then(|| Message::FriendsRemove(friend.clone())), None),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            )
            .padding([6, 10]),
        );
    }
    if x.in_game {
        pane = pane.push(in_game_notice(x));
    }
    pane.push(
        scrollable(container(list).padding([12, 12]))
            .direction(scrollable::Direction::Vertical(scrollable::Scrollbar::new().width(4).scroller_width(4).spacing(2)))
            .style(ui::thin_scrollbar)
            .height(Length::Fill),
    )
    .height(Length::Fill)
    .into()
}
