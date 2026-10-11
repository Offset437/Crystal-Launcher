//! Small adapters keep iced's controls, styling and mouse behavior intact.
use super::{mark, Kind};
use iced::widget;
use iced::{Element, Font, Length, Padding, Pixels, Theme};
use std::{borrow::Borrow, marker::PhantomData, ops::RangeInclusive};

pub(crate) struct Control<'a, M, W> {
    inner: W,
    kind: Kind,
    enabled: bool,
    marker: PhantomData<&'a M>,
}
impl<'a, M, W> Control<'a, M, W> {
    fn new(inner: W, kind: Kind, enabled: bool) -> Self {
        Self {
            inner,
            kind,
            enabled,
            marker: PhantomData,
        }
    }
}
impl<'a, M: 'a, W: Into<Element<'a, M>>> From<Control<'a, M, W>> for Element<'a, M> {
    fn from(control: Control<'a, M, W>) -> Self {
        mark(control.inner.into(), control.kind, control.enabled)
    }
}

pub(crate) type Button<'a, M> = Control<'a, M, widget::Button<'a, M>>;

pub(crate) fn button<'a, M: Clone + 'a>(
    content: impl Into<Element<'a, M>>,
) -> Control<'a, M, widget::Button<'a, M>> {
    Control::new(widget::button(content), Kind::Button, false)
}
impl<'a, M: Clone + 'a> Control<'a, M, widget::Button<'a, M>> {
    pub(crate) fn on_press(mut self, message: M) -> Self {
        self.enabled = true;
        self.inner = self.inner.on_press(message);
        self
    }
    pub(crate) fn on_press_maybe(mut self, message: Option<M>) -> Self {
        self.enabled = message.is_some();
        self.inner = self.inner.on_press_maybe(message);
        self
    }
    pub(crate) fn width(mut self, value: impl Into<Length>) -> Self {
        self.inner = self.inner.width(value);
        self
    }
    pub(crate) fn height(mut self, value: impl Into<Length>) -> Self {
        self.inner = self.inner.height(value);
        self
    }
    pub(crate) fn padding(mut self, value: impl Into<Padding>) -> Self {
        self.inner = self.inner.padding(value);
        self
    }
    pub(crate) fn style(
        mut self,
        value: impl Fn(&Theme, widget::button::Status) -> widget::button::Style + 'a,
    ) -> Self {
        self.inner = self.inner.style(value);
        self
    }
    pub(crate) fn clip(mut self, value: bool) -> Self {
        self.inner = self.inner.clip(value);
        self
    }
}

pub(crate) fn checkbox<'a, M: Clone + 'a>(
    checked: bool,
) -> Control<'a, M, widget::Checkbox<'a, M>> {
    Control::new(widget::checkbox(checked), Kind::Button, false)
}
impl<'a, M: Clone + 'a> Control<'a, M, widget::Checkbox<'a, M>> {
    pub(crate) fn on_toggle(mut self, value: impl Fn(bool) -> M + 'a) -> Self {
        self.enabled = true;
        self.inner = self.inner.on_toggle(value);
        self
    }
    pub(crate) fn label(mut self, value: impl iced::widget::text::IntoFragment<'a>) -> Self {
        self.inner = self.inner.label(value);
        self
    }
    pub(crate) fn size(mut self, value: impl Into<Pixels>) -> Self {
        self.inner = self.inner.size(value);
        self
    }
    pub(crate) fn text_size(mut self, value: impl Into<Pixels>) -> Self {
        self.inner = self.inner.text_size(value);
        self
    }
    pub(crate) fn style(
        mut self,
        value: impl Fn(&Theme, widget::checkbox::Status) -> widget::checkbox::Style + 'a,
    ) -> Self {
        self.inner = self.inner.style(value);
        self
    }
}

pub(crate) fn toggler<'a, M: Clone + 'a>(checked: bool) -> Control<'a, M, widget::Toggler<'a, M>> {
    Control::new(widget::toggler(checked), Kind::Button, false)
}
impl<'a, M: Clone + 'a> Control<'a, M, widget::Toggler<'a, M>> {
    pub(crate) fn on_toggle(mut self, value: impl Fn(bool) -> M + 'a) -> Self {
        self.enabled = true;
        self.inner = self.inner.on_toggle(value);
        self
    }
    pub(crate) fn label(mut self, value: impl iced::widget::text::IntoFragment<'a>) -> Self {
        self.inner = self.inner.label(value);
        self
    }
    pub(crate) fn size(mut self, value: impl Into<Pixels>) -> Self {
        self.inner = self.inner.size(value);
        self
    }
    pub(crate) fn spacing(mut self, value: impl Into<Pixels>) -> Self {
        self.inner = self.inner.spacing(value);
        self
    }
    pub(crate) fn text_size(mut self, value: impl Into<Pixels>) -> Self {
        self.inner = self.inner.text_size(value);
        self
    }
    pub(crate) fn font(mut self, value: Font) -> Self {
        self.inner = self.inner.font(value);
        self
    }
    pub(crate) fn style(
        mut self,
        value: impl Fn(&Theme, widget::toggler::Status) -> widget::toggler::Style + 'a,
    ) -> Self {
        self.inner = self.inner.style(value);
        self
    }
}

pub(crate) fn pick_list<'a, T, L, V, M>(
    options: L,
    selected: Option<V>,
    on_select: impl Fn(T) -> M + 'a,
) -> Control<'a, M, widget::PickList<'a, T, L, V, M>>
where
    T: ToString + PartialEq + Clone + 'a,
    L: Borrow<[T]> + 'a,
    V: Borrow<T> + 'a,
    M: Clone + 'a,
{
    let enabled = !options.borrow().is_empty();
    Control::new(
        widget::pick_list(options, selected, on_select),
        Kind::Picker,
        enabled,
    )
}
impl<'a, T, L, V, M> Control<'a, M, widget::PickList<'a, T, L, V, M>>
where
    T: ToString + PartialEq + Clone + 'a,
    L: Borrow<[T]> + 'a,
    V: Borrow<T> + 'a,
    M: Clone + 'a,
{
    pub(crate) fn width(mut self, value: impl Into<Length>) -> Self {
        self.inner = self.inner.width(value);
        self
    }
    pub(crate) fn padding(mut self, value: impl Into<Padding>) -> Self {
        self.inner = self.inner.padding(value);
        self
    }
    pub(crate) fn text_size(mut self, value: impl Into<Pixels>) -> Self {
        self.inner = self.inner.text_size(value);
        self
    }
    pub(crate) fn font(mut self, value: Font) -> Self {
        self.inner = self.inner.font(value);
        self
    }
    pub(crate) fn placeholder(mut self, value: impl Into<String>) -> Self {
        self.inner = self.inner.placeholder(value);
        self
    }
    pub(crate) fn handle(mut self, value: widget::pick_list::Handle<Font>) -> Self {
        self.inner = self.inner.handle(value);
        self
    }
    pub(crate) fn style(
        mut self,
        value: impl Fn(&Theme, widget::pick_list::Status) -> widget::pick_list::Style + 'a,
    ) -> Self {
        self.inner = self.inner.style(value);
        self
    }
    pub(crate) fn menu_style(
        mut self,
        value: impl Fn(&Theme) -> iced::overlay::menu::Style + 'a,
    ) -> Self {
        self.inner = self.inner.menu_style(value);
        self
    }
}

pub(crate) fn slider<'a, T, M>(
    range: RangeInclusive<T>,
    value: T,
    on_change: impl Fn(T) -> M + 'a,
) -> Control<'a, M, widget::Slider<'a, T, M>>
where
    T: Copy + From<u8> + PartialOrd,
    M: Clone + 'a,
{
    Control::new(widget::slider(range, value, on_change), Kind::Slider, true)
}
impl<'a, T, M> Control<'a, M, widget::Slider<'a, T, M>>
where
    T: Copy + From<u8> + PartialOrd,
    M: Clone + 'a,
{
    pub(crate) fn width(mut self, value: impl Into<Length>) -> Self {
        self.inner = self.inner.width(value);
        self
    }
    pub(crate) fn step(mut self, value: impl Into<T>) -> Self {
        self.inner = self.inner.step(value);
        self
    }
    pub(crate) fn default(mut self, value: impl Into<T>) -> Self {
        self.inner = self.inner.default(value);
        self
    }
    pub(crate) fn style(
        mut self,
        value: impl Fn(&Theme, widget::slider::Status) -> widget::slider::Style + 'a,
    ) -> Self {
        self.inner = self.inner.style(value);
        self
    }
}
