//! Spatial focus and local iced input for the launcher's existing controls.
//! No global mouse/key injection and no game input hooks.
mod controls;
use crate::{
    gamepad::detect::{Btn, Input, Pad},
    App, Message,
};
pub(crate) use controls::{button, checkbox, pick_list, slider, toggler, Button};
use iced::{
    advanced::{
        layout, mouse, overlay, renderer,
        widget::{self, tree, Operation, Tree},
        Clipboard, Layout, Shell, Widget,
    },
    keyboard, Border, Color, Element, Event, Length, Rectangle, Renderer, Size, Theme, Vector,
};
use std::{
    any::Any,
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Up,
    Down,
    Left,
    Right,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Move(Direction),
    Select,
    Back,
    Page(i8),
    Play,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Button,
    Picker,
    Slider,
    Text,
    Excluded,
}

#[derive(Default)]
struct Surface {
    pending: VecDeque<Command>,
    focus: Option<u64>,
    bounds: Option<Rectangle>,
    editing: bool,
    modifiers: keyboard::Modifiers,
}
#[derive(Default)]
struct InputState {
    device: Option<usize>,
    previous: Input,
    direction: Option<Direction>,
    repeat_at: Option<Instant>,
}
#[derive(Default)]
pub(crate) struct State {
    surface: Arc<Mutex<Surface>>,
    input: InputState,
}
impl State {
    pub(crate) fn clear(&mut self) {
        let mut state = self.surface.lock().unwrap_or_else(|e| e.into_inner());
        state.pending.clear();
        state.focus = None;
        state.bounds = None;
        state.editing = false;
        self.input.repeat_at = None;
    }
    pub(crate) fn repeating(&self) -> bool {
        self.input.repeat_at.is_some()
    }
    pub(crate) fn poll(&mut self, pad: Option<&Pad>, allowed: bool, now: Instant) -> bool {
        let commands = self.input.poll(pad.map(|p| (p.id, p.input)), allowed, now);
        let active = !commands.is_empty();
        let mut surface = self.surface.lock().unwrap_or_else(|e| e.into_inner());
        if !allowed || pad.is_none() {
            surface.pending.clear();
            surface.focus = None;
            surface.bounds = None;
            surface.editing = false;
        }
        for command in commands {
            if surface.pending.len() < 8 {
                surface.pending.push_back(command);
            }
        }
        active
    }
    pub(crate) fn preview_focus(&mut self) {
        self.surface
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .push_back(Command::Move(Direction::Down));
    }
}
fn direction(input: Input, previous: Option<Direction>) -> Option<Direction> {
    if input.pressed(Btn::DUp) {
        return Some(Direction::Up);
    }
    if input.pressed(Btn::DDown) {
        return Some(Direction::Down);
    }
    if input.pressed(Btn::DLeft) {
        return Some(Direction::Left);
    }
    if input.pressed(Btn::DRight) {
        return Some(Direction::Right);
    }
    let threshold = if previous.is_some() { 0.35 } else { 0.55 };
    if input.lx.abs().max(input.ly.abs()) < threshold {
        return None;
    }
    if input.lx.abs() > input.ly.abs() {
        Some(if input.lx < 0.0 {
            Direction::Left
        } else {
            Direction::Right
        })
    } else {
        Some(if input.ly > 0.0 {
            Direction::Up
        } else {
            Direction::Down
        })
    }
}
impl InputState {
    fn poll(&mut self, pad: Option<(usize, Input)>, allowed: bool, now: Instant) -> Vec<Command> {
        let Some((device, input)) = pad else {
            *self = Self::default();
            return Vec::new();
        };
        if !allowed || self.device != Some(device) {
            self.device = Some(device);
            self.previous = input;
            self.direction = direction(input, None);
            self.repeat_at = None;
            return Vec::new();
        }
        let mut output = Vec::new();
        let pressed = input.buttons & !self.previous.buttons;
        self.previous = input;
        for (button, command) in [
            (Btn::South, Command::Select),
            (Btn::East, Command::Back),
            (Btn::LB, Command::Page(-1)),
            (Btn::RB, Command::Page(1)),
            (Btn::Start, Command::Play),
        ] {
            if pressed & button.bit() != 0 {
                output.push(command);
            }
        }
        let next = direction(input, self.direction);
        if next != self.direction {
            self.direction = next;
            self.repeat_at = next.map(|_| now + Duration::from_millis(360));
            if let Some(next) = next {
                output.push(Command::Move(next));
            }
        } else if let (Some(next), Some(at)) = (next, self.repeat_at) {
            if now >= at {
                output.push(Command::Move(next));
                self.repeat_at = Some(now + Duration::from_millis(100));
            }
        }
        output
    }
}

struct Marker<'a, M> {
    child: Element<'a, M>,
    kind: Kind,
    enabled: bool,
}
struct MarkState {
    id: u64,
    kind: Kind,
    enabled: bool,
}
fn mark<'a, M: 'a>(child: Element<'a, M>, kind: Kind, enabled: bool) -> Element<'a, M> {
    Element::new(Marker {
        child,
        kind,
        enabled,
    })
}
pub(crate) fn exclude<'a>(child: Element<'a, Message>) -> Element<'a, Message> {
    mark(child, Kind::Excluded, false)
}
impl<M> Widget<M, Theme, Renderer> for Marker<'_, M> {
    fn size(&self) -> Size<Length> {
        self.child.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.child.as_widget().size_hint()
    }
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<MarkState>()
    }
    fn state(&self) -> tree::State {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        tree::State::new(MarkState {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            kind: self.kind,
            enabled: self.enabled,
        })
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.child)]
    }
    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.child));
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.child
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let state = tree.state.downcast_mut::<MarkState>();
        state.kind = self.kind;
        state.enabled = self.enabled;
        operation.custom(None, layout.bounds(), state);
        operation.traverse(&mut |operation| {
            self.child
                .as_widget_mut()
                .operate(&mut tree.children[0], layout, renderer, operation)
        });
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, M>,
        viewport: &Rectangle,
    ) {
        self.child.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.child.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.child.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, M, Theme, Renderer>> {
        self.child.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

#[derive(Clone)]
struct Target {
    id: u64,
    bounds: Rectangle,
    clip: Rectangle,
    kind: Kind,
    scrolls: Vec<usize>,
}
#[derive(Clone)]
struct Frame {
    offset: Vector,
    clip: Rectangle,
    allowed: bool,
    scrolls: Vec<usize>,
}
struct Scroll {
    bounds: Rectangle,
    content: Rectangle,
    translation: Vector,
}
struct Collect {
    frame: Frame,
    next: Option<Frame>,
    targets: Vec<Target>,
    scrolls: Vec<Scroll>,
    fields: u64,
}
fn translated(bounds: Rectangle, offset: Vector) -> Rectangle {
    Rectangle {
        x: bounds.x - offset.x,
        y: bounds.y - offset.y,
        ..bounds
    }
}
impl Collect {
    fn new(viewport: Rectangle) -> Self {
        Self {
            frame: Frame {
                offset: Vector::ZERO,
                clip: viewport,
                allowed: true,
                scrolls: Vec::new(),
            },
            next: None,
            targets: Vec::new(),
            scrolls: Vec::new(),
            fields: 0,
        }
    }
    fn add(&mut self, id: u64, bounds: Rectangle, kind: Kind) {
        if self.frame.allowed && bounds.width > 0.0 && bounds.height > 0.0 {
            self.targets.push(Target {
                id,
                bounds: translated(bounds, self.frame.offset),
                clip: self.frame.clip,
                kind,
                scrolls: self.frame.scrolls.clone(),
            });
        }
    }
}
impl Operation for Collect {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        let old = self.frame.clone();
        if let Some(next) = self.next.take() {
            self.frame = next;
        }
        operate(self);
        self.frame = old;
    }
    fn custom(&mut self, _: Option<&widget::Id>, bounds: Rectangle, state: &mut dyn Any) {
        if let Some(marker) = state.downcast_ref::<MarkState>() {
            if marker.kind == Kind::Excluded {
                self.next = Some(Frame {
                    allowed: false,
                    ..self.frame.clone()
                });
            } else if marker.enabled {
                self.add(marker.id, bounds, marker.kind);
            }
        }
    }
    fn focusable(
        &mut self,
        _: Option<&widget::Id>,
        bounds: Rectangle,
        _: &mut dyn widget::operation::Focusable,
    ) {
        self.fields += 1;
        self.add((1 << 63) | self.fields, bounds, Kind::Text);
    }
    fn scrollable(
        &mut self,
        _: Option<&widget::Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        _: &mut dyn widget::operation::Scrollable,
    ) {
        let index = self.scrolls.len();
        let bounds = translated(bounds, self.frame.offset);
        self.scrolls.push(Scroll {
            bounds,
            content,
            translation,
        });
        let mut frame = self.frame.clone();
        frame.offset = frame.offset + translation;
        frame.clip = bounds
            .intersection(&frame.clip)
            .unwrap_or(Rectangle::default());
        frame.scrolls.push(index);
        self.next = Some(frame);
    }
}

fn closest(targets: &[Target], current: &Target, direction: Direction) -> Option<u64> {
    let center = current.bounds.center();
    targets
        .iter()
        .filter(|t| t.id != current.id)
        .filter_map(|target| {
            let delta = target.bounds.center() - center;
            let (along, across) = match direction {
                Direction::Up => (-delta.y, delta.x.abs()),
                Direction::Down => (delta.y, delta.x.abs()),
                Direction::Left => (-delta.x, delta.y.abs()),
                Direction::Right => (delta.x, delta.y.abs()),
            };
            (along > 1.0).then_some((target.id, along + across * 3.0))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| id)
}

struct Reveal {
    changes: Vec<(usize, f32)>,
    index: usize,
}
impl Operation for Reveal {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn scrollable(
        &mut self,
        _: Option<&widget::Id>,
        _: Rectangle,
        _: Rectangle,
        _: Vector,
        state: &mut dyn widget::operation::Scrollable,
    ) {
        if let Some((_, y)) = self.changes.iter().find(|(index, _)| *index == self.index) {
            state.scroll_to(widget::operation::scrollable::AbsoluteOffset {
                x: None,
                y: Some(*y),
            });
        }
        self.index += 1;
    }
}
fn reveal(target: &Target, collection: &Collect) -> Vec<(usize, f32)> {
    let mut bounds = target.bounds;
    let mut changes = Vec::new();
    for index in target.scrolls.iter().rev() {
        let scroll = &collection.scrolls[*index];
        let delta = if bounds.y < scroll.bounds.y + 6.0 {
            bounds.y - scroll.bounds.y - 6.0
        } else if bounds.y + bounds.height > scroll.bounds.y + scroll.bounds.height - 6.0 {
            bounds.y + bounds.height - scroll.bounds.y - scroll.bounds.height + 6.0
        } else {
            0.0
        };
        let max = (scroll.content.height - scroll.bounds.height).max(0.0);
        let next = (scroll.translation.y + delta).clamp(0.0, max);
        bounds.y -= next - scroll.translation.y;
        if next != scroll.translation.y {
            changes.push((*index, next));
        }
    }
    changes
}

struct Root<'a> {
    child: Element<'a, Message>,
    state: Arc<Mutex<Surface>>,
}
pub(crate) fn view<'a>(state: &State, child: Element<'a, Message>) -> Element<'a, Message> {
    Element::new(Root {
        child,
        state: state.surface.clone(),
    })
}
fn key_event(named: keyboard::key::Named) -> Event {
    let key = keyboard::Key::Named(named);
    Event::Keyboard(keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: keyboard::key::Physical::Unidentified(
            keyboard::key::NativeCode::Unidentified,
        ),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::default(),
        text: None,
        repeat: false,
    })
}
impl Widget<Message, Theme, Renderer> for Root<'_> {
    fn size(&self) -> Size<Length> {
        self.child.as_widget().size()
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.child)]
    }
    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.child));
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.child
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.child
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) = event {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .modifiers = *modifiers;
        }
        if matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(_) | mouse::Event::CursorMoved { .. })
                | Event::Keyboard(keyboard::Event::KeyPressed { .. })
                | Event::Window(iced::window::Event::Unfocused)
        ) {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.pending.clear();
            state.focus = None;
            state.bounds = None;
            state.editing = false;
        }
        self.child.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if !matches!(
            event,
            Event::Window(iced::window::Event::RedrawRequested(_))
        ) {
            return;
        }
        let commands: Vec<_> = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .drain(..)
            .collect();
        let mut collection = Collect::new(*viewport);
        self.child.as_widget_mut().operate(
            &mut tree.children[0],
            layout,
            renderer,
            &mut collection,
        );
        for command in commands {
            if let Command::Page(step) = command {
                shell.publish(Message::ControllerPage(step));
                continue;
            }
            if command == Command::Play {
                shell.publish(Message::ControllerPlay);
                continue;
            }
            let (focus, editing) = {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                (state.focus, state.editing)
            };
            let current = focus
                .and_then(|id| collection.targets.iter().find(|t| t.id == id))
                .cloned();
            if command == Command::Back {
                if editing {
                    self.state.lock().unwrap_or_else(|e| e.into_inner()).editing = false;
                } else {
                    shell.publish(Message::ControllerBack);
                }
                continue;
            }
            let had_focus = current.is_some();
            let target = if let Some(current) = &current {
                if let Command::Move(direction) = command {
                    if editing {
                        Some(current.clone())
                    } else {
                        closest(&collection.targets, current, direction)
                            .and_then(|id| collection.targets.iter().find(|t| t.id == id))
                            .cloned()
                    }
                } else {
                    Some(current.clone())
                }
            } else {
                // First input establishes focus; it never clicks an unseen control.
                let visible = |target: &&Target| target.clip.contains(target.bounds.center());
                collection
                    .targets
                    .iter()
                    .filter(visible)
                    .find(|t| t.bounds.y > 85.0)
                    .or_else(|| collection.targets.iter().find(visible))
                    .cloned()
            };
            let Some(mut target) = target else { continue };
            let changes = reveal(&target, &collection);
            if !changes.is_empty() {
                self.child.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut Reveal { changes, index: 0 },
                );
                collection = Collect::new(*viewport);
                self.child.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut collection,
                );
                if let Some(updated) = collection.targets.iter().find(|t| t.id == target.id) {
                    target = updated.clone();
                }
            }
            let Some(visible) = target.bounds.intersection(&target.clip) else {
                continue;
            };
            if visible.width < 2.0 || visible.height < 2.0 {
                continue;
            }
            let virtual_cursor = mouse::Cursor::Available(visible.center());
            let mut events = Vec::new();
            if had_focus && command == Command::Select {
                if matches!(target.kind, Kind::Picker | Kind::Slider) {
                    self.state.lock().unwrap_or_else(|e| e.into_inner()).editing = !editing;
                } else {
                    events.extend([
                        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                    ]);
                }
            } else if editing {
                if let Command::Move(direction) = command {
                    let increase = matches!(direction, Direction::Right | Direction::Up);
                    if target.kind == Kind::Picker {
                        let modifiers = self
                            .state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .modifiers;
                        events.extend([
                            Event::Keyboard(keyboard::Event::ModifiersChanged(
                                modifiers | keyboard::Modifiers::CTRL,
                            )),
                            Event::Mouse(mouse::Event::WheelScrolled {
                                delta: mouse::ScrollDelta::Lines {
                                    x: 0.0,
                                    y: if increase { -1.0 } else { 1.0 },
                                },
                            }),
                            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)),
                        ]);
                    } else if target.kind == Kind::Slider {
                        events.push(key_event(if increase {
                            keyboard::key::Named::ArrowUp
                        } else {
                            keyboard::key::Named::ArrowDown
                        }));
                    }
                }
            }
            for event in events {
                // A separate shell per press/release prevents the press capture from
                // suppressing iced's release handler.
                let mut messages = Vec::new();
                let mut local_shell = Shell::new(&mut messages);
                self.child.as_widget_mut().update(
                    &mut tree.children[0],
                    &event,
                    layout,
                    virtual_cursor,
                    renderer,
                    clipboard,
                    &mut local_shell,
                    viewport,
                );
                shell.merge(local_shell, |message| message);
            }
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.focus = Some(target.id);
            state.bounds = Some(visible);
            if !editing && matches!(command, Command::Move(_)) {
                state.editing = false;
            }
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(target) = state
            .focus
            .and_then(|id| collection.targets.iter().find(|t| t.id == id))
        {
            state.bounds = target.bounds.intersection(&target.clip);
        } else {
            state.focus = None;
            state.bounds = None;
            state.editing = false;
        }
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use iced::advanced::Renderer as _;
        self.child.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(bounds) = state.bounds {
            let color = theme.palette().primary;
            renderer.with_layer(*viewport, |renderer| {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: bounds.x - 3.0,
                            y: bounds.y - 3.0,
                            width: bounds.width + 6.0,
                            height: bounds.height + 6.0,
                        },
                        border: Border {
                            color,
                            width: if state.editing { 3.0 } else { 2.0 },
                            radius: 8.0.into(),
                        },
                        ..Default::default()
                    },
                    Color::TRANSPARENT,
                )
            });
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.child.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.child.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

pub(crate) fn settings(app: &App) -> Element<'_, Message> {
    use crate::{
        i18n::t,
        ui::{colors, BODY},
    };
    use iced::widget::{column, text};
    let c = colors(app.settings.light_theme);
    column![
        toggler(app.settings.controller_navigation).label(t("Navigate Crystal with a controller"))
            .on_toggle(Message::ControllerNavigation).font(BODY).text_size(13).size(18),
        text(t("D-pad or left stick moves focus. Bottom face button selects; right face button goes back. Shoulders change pages; Start activates Play or Update."))
            .font(BODY).size(12).color(c.sub),
        text(t("Select a dropdown or slider, adjust it with the D-pad, then select again to finish. Text entry and Windows file dialogs use the keyboard and mouse."))
            .font(BODY).size(12).color(c.sub),
    ].spacing(10).padding([12, 0]).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn held_buttons_on_connect_or_focus_gain_never_activate_controls() {
        let mut state = InputState::default();
        let now = Instant::now();
        let held = Input {
            buttons: Btn::South.bit(),
            ..Default::default()
        };
        assert!(state.poll(Some((1, held)), true, now).is_empty());
        assert!(state.poll(Some((1, held)), true, now).is_empty());
        state.poll(Some((1, Input::default())), true, now);
        assert_eq!(state.poll(Some((1, held)), true, now), [Command::Select]);
        assert!(state.poll(Some((1, held)), false, now).is_empty());
        assert!(state.poll(Some((1, held)), true, now).is_empty());
    }
    #[test]
    fn only_directional_input_repeats_and_stick_noise_is_ignored() {
        let mut state = InputState::default();
        let now = Instant::now();
        state.poll(Some((1, Input::default())), true, now);
        let held = Input {
            buttons: Btn::DDown.bit() | Btn::South.bit(),
            ..Default::default()
        };
        assert_eq!(
            state.poll(Some((1, held)), true, now),
            [Command::Select, Command::Move(Direction::Down)]
        );
        assert!(state
            .poll(Some((1, held)), true, now + Duration::from_millis(100))
            .is_empty());
        assert_eq!(
            state.poll(Some((1, held)), true, now + Duration::from_millis(400)),
            [Command::Move(Direction::Down)]
        );
        assert_eq!(
            direction(
                Input {
                    lx: 0.2,
                    ly: -0.1,
                    ..Default::default()
                },
                None
            ),
            None
        );
        assert_eq!(
            direction(
                Input {
                    lx: -0.8,
                    ..Default::default()
                },
                None
            ),
            Some(Direction::Left)
        );
    }
    fn target(id: u64, x: f32, y: f32) -> Target {
        Target {
            id,
            bounds: Rectangle {
                x,
                y,
                width: 80.0,
                height: 30.0,
            },
            clip: Rectangle {
                x: 0.0,
                y: 0.0,
                width: 500.0,
                height: 300.0,
            },
            kind: Kind::Button,
            scrolls: Vec::new(),
        }
    }
    #[test]
    fn spatial_navigation_prefers_the_same_row_and_does_not_wrap() {
        let targets = [
            target(1, 10.0, 10.0),
            target(2, 110.0, 10.0),
            target(3, 50.0, 100.0),
            target(4, 110.0, 100.0),
        ];
        assert_eq!(closest(&targets, &targets[0], Direction::Right), Some(2));
        assert_eq!(closest(&targets, &targets[0], Direction::Down), Some(3));
        assert_eq!(closest(&targets, &targets[0], Direction::Left), None);
    }
    #[test]
    fn offscreen_focus_reveals_the_correct_scroll_container() {
        let mut collection = Collect::new(Rectangle {
            x: 0.0,
            y: 0.0,
            width: 400.0,
            height: 200.0,
        });
        collection.scrolls.push(Scroll {
            bounds: collection.frame.clip,
            content: Rectangle {
                height: 800.0,
                ..collection.frame.clip
            },
            translation: Vector::ZERO,
        });
        let mut below = target(1, 20.0, 300.0);
        below.scrolls.push(0);
        assert_eq!(reveal(&below, &collection), [(0, 136.0)]);
    }

    struct Harness<'a> {
        state: State,
        root: Element<'a, Message>,
        tree: Tree,
        renderer: Renderer,
        node: layout::Node,
        viewport: Rectangle,
    }
    impl<'a> Harness<'a> {
        fn new(child: Element<'a, Message>) -> Self {
            let state = State::default();
            let mut root = view(&state, child);
            let mut tree = Tree::new(&root);
            let renderer = Renderer::Secondary(iced_tiny_skia::Renderer::new(
                iced::Font::default(),
                iced::Pixels(16.0),
            ));
            let viewport = Rectangle {
                x: 0.0,
                y: 0.0,
                width: 500.0,
                height: 300.0,
            };
            let node = root.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            Self {
                state,
                root,
                tree,
                renderer,
                node,
                viewport,
            }
        }
        fn command(&mut self, command: Command) -> Vec<Message> {
            self.state
                .surface
                .lock()
                .unwrap()
                .pending
                .push_back(command);
            let mut output = Vec::new();
            self.root.as_widget_mut().update(
                &mut self.tree,
                &Event::Window(iced::window::Event::RedrawRequested(Instant::now())),
                Layout::new(&self.node),
                mouse::Cursor::Unavailable,
                &self.renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut output),
                &self.viewport,
            );
            output
        }
        fn focus(&mut self) {
            assert!(self.command(Command::Select).is_empty());
        }
    }
    #[test]
    fn a_controller_click_uses_the_real_button_and_skips_disabled_controls() {
        use iced::widget::{column, text};
        let child = column![
            button(text("disabled")),
            button(text("enabled")).on_press(Message::Refresh)
        ]
        .into();
        let mut harness = Harness::new(child);
        harness.focus();
        let output = harness.command(Command::Select);
        assert_eq!(output.len(), 1);
        assert!(matches!(output[0], Message::Refresh));
    }
    #[test]
    fn controller_confirmation_toggles_the_existing_checkbox() {
        let mut harness = Harness::new(checkbox(false).on_toggle(Message::SetPrivacy).into());
        harness.focus();
        assert!(matches!(
            harness.command(Command::Select).as_slice(),
            [Message::SetPrivacy(true)]
        ));
    }
    #[test]
    fn controller_adjustment_uses_the_pick_lists_own_selection_handler() {
        let mut harness = Harness::new(
            pick_list(
                crate::i18n::Lang::ALL,
                Some(crate::i18n::Lang::En),
                Message::SetLanguage,
            )
            .into(),
        );
        harness.focus();
        assert!(harness.command(Command::Select).is_empty());
        assert!(matches!(
            harness.command(Command::Move(Direction::Right)).as_slice(),
            [Message::SetLanguage(crate::i18n::Lang::Ja)]
        ));
        assert!(harness.command(Command::Back).is_empty()); // leave value editing
        assert!(matches!(
            harness.command(Command::Back).as_slice(),
            [Message::ControllerBack]
        ));
    }
    #[test]
    fn controller_adjustment_obeys_the_sliders_step() {
        let mut harness = Harness::new(
            slider(0.0f32..=100.0, 0.0, |n| {
                Message::InstallPathChanged(n.to_string())
            })
            .step(5.0f32)
            .into(),
        );
        harness.focus();
        harness.command(Command::Select);
        assert!(
            matches!(harness.command(Command::Move(Direction::Right)).as_slice(),
            [Message::InstallPathChanged(value)] if value == "5")
        );
    }
    #[test]
    fn modal_focus_cannot_activate_background_buttons() {
        use iced::widget::{opaque, stack, text};
        let child = stack![
            exclude(button(text("background")).on_press(Message::Refresh).into()),
            opaque(button(text("dialog")).on_press(Message::CancelClose))
        ]
        .into();
        let mut harness = Harness::new(child);
        harness.focus();
        assert!(matches!(
            harness.command(Command::Select).as_slice(),
            [Message::CancelClose]
        ));
    }
    #[test]
    fn moving_through_a_real_scrollable_keeps_focus_in_view() {
        use iced::widget::{column, scrollable, text};
        let mut list = column![].spacing(8);
        for i in 0..12 {
            list = list.push(
                button(text(format!("Button {i}")))
                    .height(30)
                    .on_press(Message::Refresh),
            );
        }
        let mut harness = Harness::new(scrollable(list).height(80).width(200).into());
        harness.focus();
        for _ in 0..5 {
            harness.command(Command::Move(Direction::Down));
        }
        let bounds = harness.state.surface.lock().unwrap().bounds.unwrap();
        assert!(bounds.y >= 0.0 && bounds.y + bounds.height <= 80.0);
        assert!(matches!(
            harness.command(Command::Select).as_slice(),
            [Message::Refresh]
        ));
    }

    #[test]
    fn losing_window_focus_discards_a_queued_controller_click() {
        let mut harness = Harness::new(
            button(iced::widget::text("Play"))
                .on_press(Message::MainAction)
                .into(),
        );
        harness.focus();
        harness
            .state
            .surface
            .lock()
            .unwrap()
            .pending
            .push_back(Command::Select);
        let mut output = Vec::new();
        harness.root.as_widget_mut().update(
            &mut harness.tree,
            &Event::Window(iced::window::Event::Unfocused),
            Layout::new(&harness.node),
            mouse::Cursor::Unavailable,
            &harness.renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut output),
            &harness.viewport,
        );
        assert!(output.is_empty());
        assert!(harness.state.surface.lock().unwrap().pending.is_empty());
        // Returning with Select establishes focus again, without playing.
        assert!(harness.command(Command::Select).is_empty());
    }

    #[test]
    fn a_disconnected_controller_clears_its_highlight_and_pending_input() {
        let mut state = State::default();
        {
            let mut surface = state.surface.lock().unwrap();
            surface.focus = Some(42);
            surface.pending.push_back(Command::Select);
        }
        assert!(!state.poll(None, true, Instant::now()));
        let surface = state.surface.lock().unwrap();
        assert!(surface.focus.is_none() && surface.pending.is_empty());
    }
}
