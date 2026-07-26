//! UA stylesheets and message-color adaptation to the application theme.

use blitz_dom::util::Color;
use blitz_traits::shell::ColorScheme;
use regex::{Captures, Regex};
use std::{borrow::Cow, sync::OnceLock};

/// Fixes applied over blitz-dom's UA stylesheet. Email styles retain priority,
/// but tables remain constrained to the reader pane
/// and their cells can wrap long content.
pub(super) const MAIL_UA_CSS: &str = r#"
html, body {
    font-family: Inter, "Noto Color Emoji", sans-serif;
    height: auto !important;
    min-height: 100% !important;
}
table[border]:not([border="0"]) {
    border-style: solid;
}
table[border]:not([border="0"]) > * > tr > td,
table[border]:not([border="0"]) > * > tr > th,
table[border]:not([border="0"]) > tr > td,
table[border]:not([border="0"]) > tr > th,
table[border]:not([border="0"]) > td,
table[border]:not([border="0"]) > th {
    border-style: solid;
}
table {
    max-width: 100%;
}
/*
 * Email builders use the legacy `height="100%"` attribute on nested layout
 * tables as a client-compatibility hint.  Their containing blocks usually have
 * an automatic height, so browsers treat that percentage as `auto`.  Taffy can
 * instead resolve it against the complete descendant height, making an early
 * masthead as tall as the entire newsletter and pushing every later section
 * below a large blank gap.
 */
table[height="100%"] {
    height: auto !important;
    min-height: 0 !important;
}
td, th {
    overflow-wrap: anywhere;
}
"#;

/// HTML embedded inside another view (signature fragment, quoted block) must
/// size itself to its contents instead of filling Blitz's synthetic viewport.
/// Kept separate from [`MAIL_UA_CSS`] because the main reader still benefits
/// from a document-height canvas for short messages.
pub(super) const FRAGMENT_UA_CSS: &str = r#"
html, body {
    min-height: 0 !important;
}
"#;

/// Optional typography-normalization stylesheet. `!important` is deliberate:
/// Outlook inline styles would otherwise override the UA stylesheet.
pub(super) fn uniform_typography_ua_css(
    force_font_family: bool,
    force_font_size: bool,
    font_size: f32,
) -> String {
    let family = if force_font_family {
        r#"font-family: Inter, "Noto Color Emoji", sans-serif !important;"#
    } else {
        ""
    };
    let size = if force_font_size {
        format!("font-size: {:.2}px !important;", font_size.clamp(9.0, 32.0))
    } else {
        String::new()
    };
    format!(
        r#"
html, body, body * {{
    {family}
    {size}
}}
"#
    )
}

/// Minimal palette carried to the actor thread and cache key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct MailTheme {
    pub(super) dark: bool,
    pub(super) background: u32,
    pub(super) foreground: u32,
    pub(super) border: u32,
    pub(super) link: u32,
}

impl MailTheme {
    pub(super) fn from_app(theme: &gpui_component::Theme, force_light: bool) -> Self {
        if force_light {
            Self::forced_light()
        } else {
            Self {
                dark: theme.mode.is_dark(),
                background: rgba8(theme.background),
                foreground: rgba8(theme.foreground),
                border: rgba8(theme.border),
                link: rgba8(theme.primary),
            }
        }
    }

    /// Browser-like palette used when the user explicitly asks to keep email
    /// previews light while the application itself is dark.
    pub(super) const fn forced_light() -> Self {
        Self {
            dark: false,
            background: 0xffffffff,
            foreground: 0x000000ff,
            border: 0xd4d4d4ff,
            link: 0x0563c1ff,
        }
    }

    pub(super) fn scheme(self) -> ColorScheme {
        if self.dark {
            ColorScheme::Dark
        } else {
            ColorScheme::Light
        }
    }

    pub(super) fn background_color(self) -> Color {
        let [r, g, b, a] = self.background.to_be_bytes();
        Color::from_rgba8(r, g, b, a)
    }

    pub(super) fn css(self) -> String {
        let scheme = if self.dark { "dark" } else { "light" };
        let black_attribute_override = if self.dark {
            format!(
                r#"[color="black" i] {{
    color: #{:06x} !important;
}}"#,
                self.foreground >> 8
            )
        } else {
            String::new()
        };
        format!(
            r#"
html, body {{
    color-scheme: {scheme};
    color: #{foreground:06x};
    background-color: transparent;
}}
a {{
    color: #{link:06x};
}}
table[border]:not([border="0"]),
table[border]:not([border="0"]) > * > tr > td,
table[border]:not([border="0"]) > * > tr > th,
table[border]:not([border="0"]) > tr > td,
table[border]:not([border="0"]) > tr > th,
table[border]:not([border="0"]) > td,
table[border]:not([border="0"]) > th {{
    border-color: #{border:06x};
}}
{black_attribute_override}
"#,
            foreground = self.foreground >> 8,
            link = self.link >> 8,
            border = self.border >> 8,
        )
    }
}

/// Adapts neutral colors that become unreadable in dark mode. The
/// chromatic brand colors remain unchanged.
pub(super) fn adapt_dark_colors<'a>(html: &'a str, theme: MailTheme) -> Cow<'a, str> {
    if !theme.dark {
        return Cow::Borrowed(html);
    }

    static COLOR_DECLARATION: OnceLock<Regex> = OnceLock::new();
    let declaration = COLOR_DECLARATION.get_or_init(|| {
        Regex::new(
            r#"(?im)(?P<prefix>^|[;{'"\s])(?P<property>color\s*:\s*)(?P<value>#[0-9a-f]{3}(?:[0-9a-f]{3})?\b|rgb\([^)]*\)|black\b)"#,
        )
        .expect("valid CSS color regex")
    });
    static BACKGROUND_DECLARATION: OnceLock<Regex> = OnceLock::new();
    let background_declaration = BACKGROUND_DECLARATION.get_or_init(|| {
        Regex::new(
            r#"(?im)(?P<prefix>^|[;{'"\s])(?P<property>background(?:-color)?\s*:\s*)(?P<value>#[0-9a-f]{3}(?:[0-9a-f]{3})?\b|rgb\([^)]*\)|white\b)"#,
        )
        .expect("valid CSS background regex")
    });
    static BGCOLOR_ATTRIBUTE: OnceLock<Regex> = OnceLock::new();
    let bgcolor_attribute = BGCOLOR_ATTRIBUTE.get_or_init(|| {
        Regex::new(
            r#"(?im)(?P<property>\bbgcolor\s*=\s*["']?)(?P<value>#[0-9a-f]{3}(?:[0-9a-f]{3})?\b|rgb\([^)]*\)|white\b)"#,
        )
        .expect("valid bgcolor attribute regex")
    });

    let background = packed_rgb(theme.background);
    let foreground = packed_rgb(theme.foreground);
    let foreground_replacement = format!(
        "#{:02x}{:02x}{:02x}",
        foreground.0, foreground.1, foreground.2
    );
    let background_replacement = format!(
        "#{:02x}{:02x}{:02x}",
        background.0, background.1, background.2
    );

    let html = declaration.replace_all(html, |captures: &Captures<'_>| {
        let value = captures.name("value").expect("capture value").as_str();
        let Some(color) = parse_css_rgb(value) else {
            return captures[0].to_string();
        };
        if is_neutral(color) && contrast_ratio(color, background) < 4.5 {
            format!(
                "{}{}{}",
                &captures["prefix"], &captures["property"], foreground_replacement
            )
        } else {
            captures[0].to_string()
        }
    });

    let html = background_declaration.replace_all(&html, |captures: &Captures<'_>| {
        replace_light_neutral_background(captures, foreground, &background_replacement, true)
    });
    let html = bgcolor_attribute.replace_all(&html, |captures: &Captures<'_>| {
        replace_light_neutral_background(captures, foreground, &background_replacement, false)
    });
    Cow::Owned(html.into_owned())
}

fn replace_light_neutral_background(
    captures: &Captures<'_>,
    foreground: (u8, u8, u8),
    replacement: &str,
    has_prefix: bool,
) -> String {
    let value = captures.name("value").expect("capture value").as_str();
    let Some(color) = parse_css_rgb(value) else {
        return captures[0].to_string();
    };
    if !is_neutral(color) || contrast_ratio(color, foreground) >= 4.5 {
        return captures[0].to_string();
    }

    if has_prefix {
        format!(
            "{}{}{}",
            &captures["prefix"], &captures["property"], replacement
        )
    } else {
        format!("{}{}", &captures["property"], replacement)
    }
}

fn packed_rgb(color: u32) -> (u8, u8, u8) {
    let [r, g, b, _a] = color.to_be_bytes();
    (r, g, b)
}

fn parse_css_rgb(value: &str) -> Option<(u8, u8, u8)> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("black") {
        return Some((0, 0, 0));
    }
    if value.eq_ignore_ascii_case("white") {
        return Some((255, 255, 255));
    }
    if let Some(hex) = value.strip_prefix('#') {
        return match hex.len() {
            3 => {
                let mut chars = hex.chars();
                let r = chars.next()?.to_digit(16)? as u8 * 17;
                let g = chars.next()?.to_digit(16)? as u8 * 17;
                let b = chars.next()?.to_digit(16)? as u8 * 17;
                Some((r, g, b))
            }
            6 => Some((
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            )),
            _ => None,
        };
    }

    let open = value.find('(')?;
    let close = value.rfind(')')?;
    let components = value[open + 1..close]
        .split(|c: char| c == ',' || c == '/' || c.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .take(3)
        .map(parse_rgb_component)
        .collect::<Option<Vec<_>>>()?;
    (components.len() == 3).then(|| (components[0], components[1], components[2]))
}

fn parse_rgb_component(component: &str) -> Option<u8> {
    if let Some(percent) = component.strip_suffix('%') {
        let value = percent.parse::<f32>().ok()?.clamp(0.0, 100.0);
        Some((value * 2.55).round() as u8)
    } else {
        Some(component.parse::<f32>().ok()?.clamp(0.0, 255.0).round() as u8)
    }
}

fn is_neutral((r, g, b): (u8, u8, u8)) -> bool {
    let min = r.min(g).min(b);
    let max = r.max(g).max(b);
    max - min <= 32
}

fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let a = relative_luminance(a);
    let b = relative_luminance(b);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn relative_luminance((r, g, b): (u8, u8, u8)) -> f32 {
    fn channel(value: u8) -> f32 {
        let value = f32::from(value) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

fn rgba8(color: gpui::Hsla) -> u32 {
    gpui::Rgba::from(color).into()
}
