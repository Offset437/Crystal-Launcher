//! Crystal's interface icons, drawn on a 24-unit grid with beveled outlines.
//! The glyphs share one cached SVG handle per icon and inherit the UI color.
use iced::widget::svg;
use iced::{Color, Element};
use std::sync::OnceLock;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Sun,
    Moon,
    Settings,
    Minimize,
    Maximize,
    Close,
    Plus,
    ChevronDown,
    ArrowRight,
    ExternalLink,
    Search,
    BookOpen,
    Scale,
    Rss,
    Refresh,
    CheckCircle,
    AlertCircle,
    Wrench,
    Play,
    User,
    Help,
    Download,
    Folder,
    Trash,
    Edit,
    Eye,
    EyeOff,
    Send,
    Inbox,
    Ban,
    UserMinus,
    Users,
    Gamepad,
    MessageCircle,
    ChevronRight,
    Grip,
}

/// Every variant, in declaration order (the index into the handle cache).
const ALL: [Icon; 36] = [
    Icon::Sun,
    Icon::Moon,
    Icon::Settings,
    Icon::Minimize,
    Icon::Maximize,
    Icon::Close,
    Icon::Plus,
    Icon::ChevronDown,
    Icon::ArrowRight,
    Icon::ExternalLink,
    Icon::Search,
    Icon::BookOpen,
    Icon::Scale,
    Icon::Rss,
    Icon::Refresh,
    Icon::CheckCircle,
    Icon::AlertCircle,
    Icon::Wrench,
    Icon::Play,
    Icon::User,
    Icon::Help,
    Icon::Download,
    Icon::Folder,
    Icon::Trash,
    Icon::Edit,
    Icon::Eye,
    Icon::EyeOff,
    Icon::Send,
    Icon::Inbox,
    Icon::Ban,
    Icon::UserMinus,
    Icon::Users,
    Icon::Gamepad,
    Icon::MessageCircle,
    Icon::ChevronRight,
    Icon::Grip,
];

fn body(which: Icon) -> &'static str {
    match which {
        Icon::Sun => concat!(
            r#"<path d="M10 8H14L16 10V14L14 16H10L8 14V10Z"/>"#,
            r#"<path d="M12 3V5.5M12 18.5V21M3 12H5.5M18.5 12H21M5.5 5.5 7 7M17 17 18.5 18.5M5.5 18.5 7 17M17 7 18.5 5.5"/>"#
        ),
        Icon::Moon => r#"<path d="M15.5 3.5C9.5 2.5 4 7 4 12.5C4 18 9 21.5 14 20C17 19.5 19.5 17 20 14C16 15.5 11.5 12.5 11.5 8.5C11.5 6.5 13 4.5 15.5 3.5Z"/>"#,
        Icon::Settings => concat!(
            r#"<path d="M6 3.5V8M6 12V20.5M12 3.5V14M12 18V20.5M18 3.5V6M18 10V20.5"/>"#,
            r#"<path d="M4 8H8L9 9V11L8 12H4L3 11V9ZM10 14H14L15 15V17L14 18H10L9 17V15ZM16 6H20L21 7V9L20 10H16L15 9V7Z"/>"#
        ),
        Icon::Minimize => r#"<path d="M7 13.5H17"/>"#,
        Icon::Maximize => r#"<path d="M8 5.5H16L18.5 8V16L16 18.5H8L5.5 16V8Z"/>"#,
        Icon::Close => r#"<path d="M7 7 17 17M7 17 17 7"/>"#,
        Icon::Plus => r#"<path d="M7 12H17M12 7V17"/>"#,
        Icon::ChevronDown => r#"<path d="M6.5 9.5 12 15 17.5 9.5"/>"#,
        Icon::ArrowRight => r#"<path d="M5.5 12H18.5M14 7.5 18.5 12 14 16.5"/>"#,
        Icon::ExternalLink => r#"<path d="M11 5H6L4.5 6.5V18L6 19.5H17.5L19 18V13M13 4.5H19.5V11M19.5 4.5 10.5 13.5"/>"#,
        Icon::Search => r#"<path d="M8 4H13L17 8V13L13 17H8L4 13V8ZM16 16 20.5 20.5"/>"#,
        Icon::BookOpen => r#"<path d="M3.5 5H10L12 7 14 5H20.5V18H14L12 20 10 18H3.5ZM12 7V20M6.5 8.5H9M15 8.5H17.5"/>"#,
        Icon::Scale => r#"<path d="M12 3.5V20.5M8.5 20.5H15.5M4.5 7H19.5M5.5 7 2.5 14H8.5ZM18.5 7 15.5 14H21.5Z"/>"#,
        Icon::Rss => concat!(
            r#"<path d="M5 11C9.4 11 13 14.6 13 19M5 4.5C13 4.5 19.5 11 19.5 19"/>"#,
            r#"<path d="M5 17.5 6.5 19 5 20.5 3.5 19Z" fill="currentColor" stroke="none"/>"#
        ),
        Icon::Refresh => r#"<path d="M19.5 9 17 5.5H9L5.5 9V11M19.5 4.5V9H15M4.5 15 7 18.5H15L18.5 15V13M4.5 19.5V15H9"/>"#,
        Icon::CheckCircle => r#"<path d="M7 3.5H17L20.5 7V17L17 20.5H7L3.5 17V7ZM7.5 12 10.5 15 16.5 8.5"/>"#,
        Icon::AlertCircle => concat!(
            r#"<path d="M7 3.5H17L20.5 7V17L17 20.5H7L3.5 17V7ZM12 7.5V12.5"/>"#,
            r#"<path d="M12 15 13.25 16.25 12 17.5 10.75 16.25Z" fill="currentColor" stroke="none"/>"#
        ),
        Icon::Wrench => r#"<path d="M15 3.5 12 6.5V10L4 18V20H6L14 12H17.5L20.5 9V4.5L16.5 8.5 15 7 18.5 3.5Z"/>"#,
        Icon::Play => r#"<path d="M7 5.5H8.5L19 12 8.5 18.5H7Z" fill="currentColor" stroke="none"/>"#,
        Icon::User => r#"<path d="M10 3.5H14L16 5.5V9L14 11H10L8 9V5.5ZM4.5 20V17L8 13.5H16L19.5 17V20"/>"#,
        Icon::Help => concat!(
            r#"<path d="M7 3.5H17L20.5 7V17L17 20.5H7L3.5 17V7ZM8.5 8.5 10 7H13.5L15.5 9V11L12 13V14"/>"#,
            r#"<path d="M12 16.5 13 17.5 12 18.5 11 17.5Z" fill="currentColor" stroke="none"/>"#
        ),
        Icon::Download => r#"<path d="M12 3.5V14.5M7.5 10 12 14.5 16.5 10M4 16.5V19L6 21H18L20 19V16.5"/>"#,
        Icon::Folder => r#"<path d="M3.5 6H10L12 8H20.5V18L18.5 20H5.5L3.5 18ZM3.5 11H20.5"/>"#,
        Icon::Trash => r#"<path d="M4.5 7H19.5M9 7V4.5H15V7M6 7 7 19.5 8.5 21H15.5L17 19.5 18 7M10 10.5V17.5M14 10.5V17.5"/>"#,
        Icon::Edit => r#"<path d="M4 16.5 16.5 4H19L20 5V7.5L7.5 20H4ZM14 6.5 17.5 10M4 16.5 7.5 20"/>"#,
        Icon::Eye => r#"<path d="M3 12 7 7H17L21 12 17 17H7ZM12 9 15 12 12 15 9 12Z"/>"#,
        Icon::EyeOff => r#"<path d="M3 12 6.5 7.5M9 7H17L21 12 18.5 15M16 17H7L3 12M11 10 9 12 12 15 13.5 13.5M4 4 20 20"/>"#,
        Icon::Send => r#"<path d="M3.5 9.5 20.5 3.5 14.5 20.5 11 13ZM11 13 20.5 3.5"/>"#,
        Icon::Inbox => r#"<path d="M6 4.5H18L21 13V18.5L19 20.5H5L3 18.5V13ZM3 13H8L10 16H14L16 13H21"/>"#,
        Icon::Ban => r#"<path d="M7 3.5H17L20.5 7V17L17 20.5H7L3.5 17V7ZM7 7 17 17"/>"#,
        Icon::UserMinus => r#"<path d="M6.5 3.5H10.5L12 5V8.5L10.5 10H6.5L5 8.5V5ZM3.5 20V16.5L7 13H12L15.5 16.5V20M16 9.5H21"/>"#,
        Icon::Users => r#"<path d="M6 4H10L11.5 5.5V9L10 10.5H6L4.5 9V5.5ZM2.5 20V17L6 13.5H11L14.5 17V20M15.5 5H18L19.5 6.5V9L18 10.5H15.5M16.5 13.5H18L21.5 17V20"/>"#,
        Icon::Gamepad => concat!(
            r#"<path d="M7 6.5H17L19.5 9 21 17.5 19.5 19H17.5L14.5 15.5H9.5L6.5 19H4.5L3 17.5 4.5 9ZM6 10.5H10M8 8.5V12.5"/>"#,
            r#"<path d="M15 10.5 16 11.5 15 12.5 14 11.5ZM18 8.5 19 9.5 18 10.5 17 9.5Z" fill="currentColor" stroke="none"/>"#
        ),
        Icon::MessageCircle => r#"<path d="M6 4.5H18L20.5 7V15L18 17.5H10L5 20.5V17L3.5 15.5V7ZM7 10.5H17"/>"#,
        Icon::ChevronRight => r#"<path d="M8.5 6.5 14 12 8.5 17.5"/>"#,
        Icon::Grip => r#"<path d="M8 5 9.5 6.5 8 8 6.5 6.5ZM16 5 17.5 6.5 16 8 14.5 6.5ZM8 10.5 9.5 12 8 13.5 6.5 12ZM16 10.5 17.5 12 16 13.5 14.5 12ZM8 16 9.5 17.5 8 19 6.5 17.5ZM16 16 17.5 17.5 16 19 14.5 17.5Z" fill="currentColor" stroke="none"/>"#,
    }
}

/// White strokes and fills let the SVG widget tint the entire glyph.
fn build(which: Icon) -> svg::Handle {
    let doc = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#ffffff" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><title>{which:?}</title>{}</svg>"##,
        body(which).replace("currentColor", "#ffffff")
    );
    svg::Handle::from_memory(doc.into_bytes())
}

fn handle(which: Icon) -> svg::Handle {
    static CACHE: OnceLock<Vec<svg::Handle>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| ALL.iter().map(|i| build(*i)).collect());
    cache[ALL.iter().position(|i| *i == which).unwrap_or(0)].clone()
}

/// Draws `which` as a square `size` px glyph tinted `color`.
pub fn icon<'a, M: 'a>(which: Icon, size: f32, color: Color) -> Element<'a, M> {
    svg(handle(which))
        .width(size)
        .height(size)
        .style(move |_, _| svg::Style { color: Some(color) })
        .into()
}
