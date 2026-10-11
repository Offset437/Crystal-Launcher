use crate::Message;
use iced::{event, mouse, window, Event};
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct Drag {
    pub source: Option<String>,
    pub original: Vec<String>,
}

/// Stable ordering leaves newly added accounts after the saved list. Duplicate
/// or deleted keys in an older settings file do not hide any accounts.
pub(crate) fn apply<T>(items: &mut [T], order: &[String], key: impl Fn(&T) -> &str) {
    let mut positions = HashMap::new();
    for (index, value) in order.iter().enumerate() {
        positions.entry(value.as_str()).or_insert(index);
    }
    items.sort_by_key(|item| positions.get(key(item)).copied().unwrap_or(usize::MAX));
}

/// Releases outside the grip or account list must finish the drag too.
pub(crate) fn input(event: Event, _: event::Status, _: window::Id) -> Option<Message> {
    match event {
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => Some(Message::AccountDragFinish),
        Event::Window(window::Event::Unfocused) => Some(Message::AccountDragCancel),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_order_keeps_unknown_accounts_and_ignores_duplicate_and_stale_keys() {
        let mut items = vec!["one".to_owned(), "new".into(), "two".into(), "other".into()];
        apply(&mut items, &["two".into(), "deleted".into(), "two".into(), "one".into()], String::as_str);
        assert_eq!(items, ["two", "one", "new", "other"]);
    }
}
