//! Privacy mode, for screenshots: names and identifiers are drawn blurred.
//!
//! Display only. Settings, the account vault, logs, the friend cache and
//! everything sent to the loader keep the real values; views pass identifying
//! text through here. What blurs: account labels, PlayOnline / Square Enix
//! IDs, character and friend names (and other PlayOnline handles on mail),
//! message subjects and bodies, and the Windows user name inside paths.
//!
//! Blurred text uses the same neutral placeholder for every value, so its
//! layout and glyphs do not reveal the original length or shape. Widget
//! operations report a fixed stand-in that cannot be found, selected or copied.
//! Text fields and drop-down labels use bullet masking instead.
use iced::advanced::{layout, renderer, widget, Layout, Widget};
use iced::widget::text::Text;
use iced::{mouse, Color, Element, Font, Length, Rectangle, Size, Theme, Vector};

// One view thread in the app; per test thread in tests (as `i18n::current`),
// so parallel tests cannot switch each other's privacy.
#[cfg(not(test))]
static ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
thread_local! {
    static ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static PROFILE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

#[cfg(not(test))]
pub(crate) fn set(on: bool) {
    ON.store(on, std::sync::atomic::Ordering::Relaxed);
}
#[cfg(test)]
pub(crate) fn set(on: bool) {
    ON.with(|c| c.set(on));
}
#[cfg(not(test))]
pub(crate) fn on() -> bool {
    ON.load(std::sync::atomic::Ordering::Relaxed)
}
#[cfg(test)]
pub(crate) fn on() -> bool {
    ON.with(|c| c.get())
}

// ---- The blur ----------------------------------------------------------

/// The blur radius on 12.5 px text,
/// scaled with the text so a heading blurs as much, proportionally, as a
/// caption, and a little under a true Gaussian's, since a sampled blur turns
/// to a smudge sooner.
const BLUR_PX: f32 = 7.0;
const BLUR_TEXT_PX: f32 = 12.5;
const SPREAD: f32 = 0.7;
/// The centre copy's alpha: no single copy reads as text.
const PEAK_ALPHA: f32 = 0.09;
/// Neither the drawn text nor the widget-tree text depends on the private value.
const MASKED_TEXT: &str = "••••••••";
const HIDDEN_TEXT: &str = "▒▒▒▒▒▒▒▒";

/// The Gaussian's standard deviation for text of `size` px.
pub(crate) fn blur_sigma(size: f32) -> f32 {
    (BLUR_PX * SPREAD * size / BLUR_TEXT_PX).max(1.5)
}

/// One copy of the text in the blur: its offset and alpha multiplier.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Tap {
    pub dx: f32,
    pub dy: f32,
    pub alpha: f32,
}

/// A centre copy and five rings out to two sigmas (91 copies), each copy's
/// alpha from the Gaussian at its distance. Alternate rings are staggered so
/// the copies merge into a smudge instead of ghosting the letters.
pub(crate) fn blur_taps(sigma: f32) -> Vec<Tap> {
    const RINGS: [(f32, usize); 6] = [(0.0, 1), (0.4, 6), (0.8, 12), (1.2, 18), (1.6, 24), (2.0, 30)];
    let mut taps = Vec::with_capacity(91);
    for (ring, (radius, count)) in RINGS.iter().enumerate() {
        let r = radius * sigma;
        let alpha = PEAK_ALPHA * (-radius * radius / 2.0).exp();
        let phase = if ring % 2 == 1 { 0.5 } else { 0.0 };
        for i in 0..*count {
            let angle = (i as f32 + phase) * std::f32::consts::TAU / *count as f32;
            taps.push(Tap { dx: r * angle.cos(), dy: r * angle.sin(), alpha });
        }
    }
    taps
}

/// What blurred text reports to the widget tree, independent of its value.
pub(crate) fn hidden(_text: &str) -> String {
    HIDDEN_TEXT.to_owned()
}

/// A text field's or drop-down's nonempty value where blur cannot apply:
/// always eight bullets. Empty fields retain their ordinary placeholders.
pub(crate) fn mask(value: &str) -> String {
    if !on() || value.is_empty() {
        return value.to_owned();
    }
    MASKED_TEXT.to_owned()
}

/// A neutral placeholder drawn blurred with the wrapped `Text`'s formatting.
pub(crate) struct Blurred<'a> {
    text: Text<'a>,
    color: Option<Color>,
    sigma: f32,
    hidden: String,
}

type TextState = widget::text::State<<iced::Renderer as iced::advanced::text::Renderer>::Paragraph>;

impl<M> Widget<M, Theme, iced::Renderer> for Blurred<'_> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<TextState>()
    }
    fn state(&self) -> widget::tree::State {
        <Text<'_> as Widget<M, Theme, iced::Renderer>>::state(&self.text)
    }
    fn size(&self) -> Size<Length> {
        <Text<'_> as Widget<M, Theme, iced::Renderer>>::size(&self.text)
    }
    fn layout(&mut self, tree: &mut widget::Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        // Text has no public content/format accessors. Its layout exposes the
        // paragraph's formatting; discard the original node and replace its
        // content before drawing or operating on the tree. Recompute the node
        // as well, so the original's width and wrapping cannot affect layout.
        let size = <Text<'_> as Widget<M, Theme, iced::Renderer>>::size(&self.text);
        let _ = <Text<'_> as Widget<M, Theme, iced::Renderer>>::layout(&mut self.text, tree, renderer, limits);
        let paragraph = tree.state.downcast_mut::<TextState>();
        let text = paragraph.as_text();
        let format = widget::text::Format {
            width: size.width,
            height: size.height,
            size: Some(text.size),
            font: Some(text.font),
            line_height: text.line_height,
            align_x: text.align_x,
            align_y: text.align_y,
            shaping: text.shaping,
            wrapping: text.wrapping,
        };
        widget::text::layout(paragraph, renderer, limits, MASKED_TEXT, format)
    }
    fn draw(
        &self,
        tree: &widget::Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use iced::advanced::text::{Paragraph as _, Renderer as _};
        let paragraph = tree.state.downcast_ref::<TextState>().raw();
        let anchor = layout.bounds().anchor(paragraph.min_bounds(), paragraph.align_x(), paragraph.align_y());
        let base = self.color.unwrap_or(style.text_color);
        for tap in blur_taps(self.sigma) {
            let color = Color { a: base.a * tap.alpha, ..base };
            renderer.fill_paragraph(paragraph, anchor + Vector::new(tap.dx, tap.dy), color, *viewport);
        }
    }
    fn operate(
        &mut self,
        _tree: &mut widget::Tree,
        layout: Layout<'_>,
        _renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        operation.text(None, layout.bounds(), &self.hidden);
    }
}

impl<'a, M: 'a> From<Blurred<'a>> for Element<'a, M> {
    fn from(blurred: Blurred<'a>) -> Self {
        Element::new(blurred)
    }
}

/// `content` shown as `text` (built with its font, size and colour as usual),
/// replaced by a fixed blurred placeholder while privacy mode is on.
/// `size` is the text's size and `color`
/// its colour (`None`: the parent's, as a button label takes the button's).
pub(crate) fn name<'a, M: 'a>(text: Text<'a>, content: &str, size: f32, color: impl Into<Option<Color>>) -> Element<'a, M> {
    if !on() {
        return text.into();
    }
    Blurred { text, color: color.into(), sigma: blur_sigma(size), hidden: hidden(content) }.into()
}

/// One line mixing plain parts and identifying parts (flag `true`). With
/// privacy off, or nothing to hide, it is one ordinary text that wraps as
/// before; otherwise a row whose identifying parts are blurred.
pub(crate) fn line<'a, M: 'a>(parts: Vec<(String, bool)>, size: f32, font: Font, color: Color) -> Element<'a, M> {
    if !on() || parts.iter().all(|(_, hide)| !hide) {
        let joined: String = parts.into_iter().map(|(text, _)| text).collect();
        return iced::widget::text(joined).font(font).size(size).color(color).into();
    }
    let mut row = iced::widget::Row::new().align_y(iced::Alignment::Center);
    for (part, hide) in parts {
        if part.is_empty() {
            continue;
        }
        let shown = iced::widget::text(part.clone()).font(font).size(size).color(color).wrapping(iced::widget::text::Wrapping::None);
        row = row.push(if hide { name(shown, &part, size, color) } else { shown.into() });
    }
    row.into()
}

/// `text` split into plain parts and the parts that identify someone: any of
/// `names` (as whole words) and the Windows user name inside a path.
pub(crate) fn split(text: &str, names: &[&str]) -> Vec<(String, bool)> {
    let mut needles: Vec<String> = names.iter().map(|n| n.trim()).filter(|n| !n.is_empty()).map(str::to_owned).collect();
    if let Some(user) = user_name() {
        needles.push(user);
    }
    needles.sort_by_key(|n| std::cmp::Reverse(n.chars().count()));
    needles.dedup();
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut parts: Vec<(String, bool)> = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    'scan: while !rest.is_empty() {
        for needle in &needles {
            if rest.len() >= needle.len() && rest.is_char_boundary(needle.len()) && rest[..needle.len()].eq_ignore_ascii_case(needle) {
                let before = plain.chars().next_back().or_else(|| parts.last().and_then(|(p, _)| p.chars().next_back()));
                let after = rest[needle.len()..].chars().next();
                if !word(before) && !word(after) {
                    if !plain.is_empty() {
                        parts.push((std::mem::take(&mut plain), false));
                    }
                    parts.push((rest[..needle.len()].to_owned(), true));
                    rest = &rest[needle.len()..];
                    continue 'scan;
                }
            }
        }
        let ch = rest.chars().next().unwrap();
        plain.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    if !plain.is_empty() {
        parts.push((plain, false));
    }
    parts
}

// ---- Paths ---------------------------------------------------------------

/// The Windows user folder (`C:\Users\<name>`), whose last part names the user.
#[cfg(not(test))]
fn profile_dir() -> Option<String> {
    std::env::var("USERPROFILE").ok().filter(|p| p.len() > 3)
}
#[cfg(test)]
fn profile_dir() -> Option<String> {
    PROFILE.with(|p| p.borrow().clone())
}
#[cfg(test)]
pub(crate) fn set_profile_dir_for_test(dir: &str) {
    PROFILE.with(|p| *p.borrow_mut() = Some(dir.to_owned()));
}

/// The Windows user name, as it appears inside the user's paths.
fn user_name() -> Option<String> {
    let dir = profile_dir()?;
    let name = dir.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next()?.to_owned();
    (name.len() > 1).then_some(name)
}

/// A path for a text field: with privacy on, everything after the drive is
/// bullets, so no folder name shows. Path fields are read-only while privacy
/// is on (see [`editable`]), so a masked value is never typed back.
pub(crate) fn mask_path(value: &str) -> String {
    if !on() || value.trim().is_empty() {
        return value.to_owned();
    }
    let drive = value.char_indices().nth(2).filter(|_| value.get(1..3) == Some(":\\")).map_or(0, |(i, _)| i + 1);
    format!("{}{}", &value[..drive], mask(&value[drive..]))
}

/// Whether a path field takes typing: not while privacy mode hides it.
pub(crate) fn editable() -> bool {
    !on()
}

/// A path shown as text: blurred whole while privacy mode is on.
pub(crate) fn path<'a, M: 'a>(path: String, size: f32, font: Font, color: Color) -> Element<'a, M> {
    let shown = iced::widget::text(path.clone()).font(font).size(size).color(color);
    name(shown, &path, size, color)
}
/// What a path field holds after the user edits its masked text: the masked
/// user folder goes back to the real one, so stored paths are never masked.
pub(crate) fn unmask_path(input: String) -> String {
    let (Some(dir), Some(user)) = (profile_dir(), user_name()) else { return input };
    if !on() {
        return input;
    }
    let masked = format!("{}{}", &dir[..dir.len() - user.len()], mask(&user));
    if starts_with_ignore_case(&input, &masked) {
        format!("{dir}{}", &input[masked.len()..])
    } else {
        input
    }
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len() && text.is_char_boundary(prefix.len()) && text[..prefix.len()].eq_ignore_ascii_case(prefix)
}

#[cfg(test)]
pub(crate) mod testing {
    //! Collects the text a view reports to the widget tree, as a screen
    //! reader or a test selector would see it.
    use iced::advanced::widget::{self, Operation, Tree};
    use iced::advanced::{layout, Layout};
    use iced::{Element, Rectangle, Size};

    pub(super) struct Texts(pub(super) Vec<(String, Rectangle)>);
    impl Operation for Texts {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<()>)) {
            operate(self);
        }
        fn text(&mut self, _id: Option<&iced::advanced::widget::Id>, bounds: Rectangle, text: &str) {
            self.0.push((text.to_owned(), bounds));
        }
        fn text_input(&mut self, _id: Option<&iced::advanced::widget::Id>, bounds: Rectangle, state: &mut dyn widget::operation::TextInput) {
            self.0.push((state.text().to_owned(), bounds));
        }
    }

    pub(super) fn renderer() -> iced::Renderer {
        use iced::advanced::renderer::Headless;
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(<iced::Renderer as Headless>::new(
            crate::ui::BODY,
            iced::Pixels(14.0),
            Some("tiny-skia"),
        ))
        .expect("a headless renderer")
    }

    /// Every text and its laid-out bounds, in tree order.
    pub(crate) fn text_bounds<M>(mut element: Element<'_, M>, size: Size) -> Vec<(String, Rectangle)> {
        let renderer = renderer();
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(Size::ZERO, size));
        let mut texts = Texts(Vec::new());
        element.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
        texts.0
    }

    /// Every text `element` lays out at `size`, in tree order.
    pub(crate) fn texts<M>(element: Element<'_, M>, size: Size) -> Vec<String> {
        text_bounds(element, size).into_iter().map(|(text, _)| text).collect()
    }

    /// Focuses an input and delivers typing through its real widget callback.
    pub(crate) fn type_text<M>(mut element: Element<'_, M>, text: &str) -> Vec<M> {
        struct Focus;
        impl Operation for Focus {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<()>)) {
                operate(self);
            }
            fn focusable(&mut self, _id: Option<&widget::Id>, _bounds: Rectangle, state: &mut dyn widget::operation::Focusable) {
                state.focus();
            }
        }
        let renderer = renderer();
        let mut tree = Tree::new(&element);
        let size = Size::new(500.0, 200.0);
        let node = element.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(Size::ZERO, size));
        element.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer, &mut Focus);
        let event = iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
            key: iced::keyboard::Key::Character(text.into()),
            modified_key: iced::keyboard::Key::Character(text.into()),
            physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::KeyX),
            location: iced::keyboard::Location::Standard,
            modifiers: iced::keyboard::Modifiers::empty(),
            text: Some(text.into()),
            repeat: false,
        });
        let mut messages = Vec::new();
        element.as_widget_mut().update(&mut tree, &event, Layout::new(&node), iced::mouse::Cursor::Unavailable,
            &renderer, &mut iced::advanced::clipboard::Null, &mut iced::advanced::Shell::new(&mut messages), &Rectangle::with_size(size));
        messages
    }

    /// With privacy on, none of `secrets` is readable anywhere in `texts`,
    /// and there is a fixed stand-in for every identity `blurred` lists.
    pub(crate) fn assert_blurred(texts: &[String], secrets: &[&str], blurred: &[&str]) {
        for secret in secrets {
            if let Some(text) = texts.iter().find(|t| t.to_lowercase().contains(&secret.to_lowercase())) {
                panic!("{secret:?} is readable in {text:?} with privacy mode on");
            }
        }
        let count = texts.iter().filter(|t| t.as_str() == super::HIDDEN_TEXT).count();
        assert!(count >= blurred.len(), "expected {} blurred identities, found {count} in {texts:#?}", blurred.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_text_never_reports_the_original_length() {
        for name in ["", "Al", "Alyria", "An exceptionally long account name", "アリス", "e\u{301}🐉"] {
            assert_eq!(hidden(name), "▒▒▒▒▒▒▒▒");
        }
    }

    #[test]
    fn private_names_have_identical_layout_and_neutral_paragraphs() {
        use iced::advanced::text::Paragraph as _;
        use iced::alignment::{Horizontal, Vertical};
        use iced::widget::text::{LineHeight, Wrapping};

        set(true);
        let renderer = testing::renderer();
        let names = ["", "Al", "An exceptionally long account name", "アリスドラゴン", "e\u{301}🐉", "One\nTwo\nThree"];
        for font in [crate::ui::BODY, crate::ui::SEMI, iced::Font::with_name("Consolas")] {
            for size in [12.5, 24.0] {
                for bounds in [Size::new(500.0, 200.0), Size::new(24.0, 200.0)] {
                    let mut expected = None;
                    for name_value in names {
                        let text = iced::widget::text(name_value).font(font).size(size).line_height(1.45)
                            .align_x(Horizontal::Right).align_y(Vertical::Center).wrapping(Wrapping::WordOrGlyph);
                        let mut element: Element<'_, ()> = name(text, name_value, size, Color::WHITE);
                        let mut tree = widget::Tree::new(&element);
                        let node = element.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(Size::ZERO, bounds));
                        let paragraph = tree.state.downcast_ref::<TextState>();
                        assert_eq!(paragraph.content(), MASKED_TEXT, "only neutral glyphs may be drawn");
                        assert_eq!(paragraph.raw().font(), font);
                        assert_eq!(paragraph.raw().size(), iced::Pixels(size));
                        assert_eq!(paragraph.raw().line_height(), LineHeight::Absolute(iced::Pixels(size * 1.45)));
                        assert_eq!(paragraph.raw().align_x(), Horizontal::Right.into());
                        assert_eq!(paragraph.raw().align_y(), Vertical::Center);
                        assert_eq!(paragraph.raw().wrapping(), Wrapping::WordOrGlyph);
                        let result = (node.bounds(), paragraph.min_bounds());
                        let mut texts = testing::Texts(Vec::new());
                        element.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
                        assert_eq!(texts.0, [(HIDDEN_TEXT.to_owned(), node.bounds())]);
                        if let Some(expected) = expected {
                            assert_eq!(result, expected, "private layout changed for {name_value:?}");
                        } else {
                            expected = Some(result);
                        }
                    }
                }
            }
        }
        set(false);
    }

    #[test]
    fn toggling_privacy_relayouts_existing_text_and_preserves_explicit_dimensions() {
        let renderer = testing::renderer();
        let limits = layout::Limits::new(Size::ZERO, Size::new(500.0, 200.0));
        let value = "An exceptionally long account name";
        let build = || name::<()>(iced::widget::text(value).font(crate::ui::BODY).size(14).width(200).height(40).center(), value, 14.0, Color::WHITE);
        set(false);
        let mut element = build();
        let mut tree = widget::Tree::new(&element);
        for privacy in [false, true, false] {
            set(privacy);
            element = build();
            tree.diff(&element);
            let node = element.as_widget_mut().layout(&mut tree, &renderer, &limits);
            assert_eq!(node.size(), Size::new(200.0, 40.0));
            assert_eq!(tree.state.downcast_ref::<TextState>().content(), if privacy { MASKED_TEXT } else { value });
            let mut texts = testing::Texts(Vec::new());
            element.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
            assert_eq!(texts.0, [(if privacy { HIDDEN_TEXT } else { value }.to_owned(), node.bounds())]);
        }
    }

    #[test]
    fn the_blur_is_a_centred_gaussian_scaled_with_the_text() {
        assert!((blur_sigma(12.5) - 7.0 * SPREAD).abs() < 1e-4);
        assert!((blur_sigma(2.0) - 1.5).abs() < 1e-4);
        let taps = blur_taps(4.0);
        assert_eq!(taps.len(), 91);
        let (sx, sy) = taps.iter().fold((0.0, 0.0), |(x, y), t| (x + t.dx, y + t.dy));
        assert!(sx.abs() < 1e-3 && sy.abs() < 1e-3);
        assert!(taps.iter().all(|t| t.alpha <= PEAK_ALPHA + 1e-6), "no copy is readable on its own");
    }

    #[test]
    fn identifying_parts_are_split_out_as_whole_words() {
        set_profile_dir_for_test(r"C:\Users\Jane");
        let parts = split(r"Al: log at C:\users\jane\x.log. Also Alyria.", &["Al", "Alyria"]);
        let hidden: Vec<_> = parts.iter().filter(|(_, h)| *h).map(|(p, _)| p.as_str()).collect();
        assert_eq!(hidden, ["Al", "jane", "Alyria"]);
        assert_eq!(parts.iter().map(|(p, _)| p.as_str()).collect::<String>(), r"Al: log at C:\users\jane\x.log. Also Alyria.");
    }

    #[test]
    fn fields_mask_and_restore_the_user_folder_and_ids() {
        set_profile_dir_for_test(r"C:\Users\Jane");
        set(false);
        assert_eq!(mask("ABC12345"), "ABC12345");
        assert_eq!(mask_path(r"C:\Users\Jane\FFXI"), r"C:\Users\Jane\FFXI");
        set(true);
        for value in ["Al", "ABC12345", "An exceptionally long account name", "アリスドラゴン", "e\u{301}🐉"] {
            assert_eq!(mask(value), "••••••••");
        }
        assert_eq!(mask(""), "");
        // Whole locations are hidden, not just the user name; the fields are read-only meanwhile.
        assert_eq!(mask_path(r"C:\Users\Jane\Downloads\FFXI"), r"C:\••••••••");
        assert_eq!(mask_path(r"C:\Al"), mask_path(r"C:\An exceptionally long user\FFXI"));
        assert_eq!(mask_path(r"C:\アリス\FFXI"), r"C:\••••••••");
        assert_eq!(unmask_path(r"C:\Users\••••••••\FFXI".to_owned()), r"C:\Users\Jane\FFXI");
        assert!(!mask_path(r"D:\Games\FFXI").contains("Games"));
        assert!(mask_path(r"D:\Games\FFXI").starts_with(r"D:\"));
        assert_eq!(mask_path(""), "");
        assert!(!editable());
        set(false);
        assert!(editable());
    }
}
