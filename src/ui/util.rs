//! Pure helpers shared by views.

use crate::model::{AccountId, MessageHeader};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Utc};
use gpui_kit::Hsla;

pub fn escape_html_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Splits input such as "a@b.c, d@e.f; g@h.i" into individual addresses.
///
/// Separators inside a quoted display name (`"Doe, Jane" <j@x.test>`) or
/// inside angle brackets belong to the entry and do not split it.
pub fn parse_addresses(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    let mut angle_depth = 0u32;
    for ch in s.chars() {
        if in_quotes {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_quotes = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_quotes = true;
                current.push(ch);
            }
            '<' => {
                angle_depth += 1;
                current.push(ch);
            }
            '>' => {
                angle_depth = angle_depth.saturating_sub(1);
                current.push(ch);
            }
            ',' | ';' if angle_depth == 0 => {
                push_entry(&mut out, &mut current);
            }
            _ => current.push(ch),
        }
    }
    push_entry(&mut out, &mut current);
    out
}

fn push_entry(out: &mut Vec<String>, current: &mut String) {
    let entry = current.trim();
    if !entry.is_empty() {
        out.push(entry.to_string());
    }
    current.clear();
}

/// `Name <addr>` with the name quoted (RFC 5322 quoted-string) when it holds
/// characters that would otherwise break list parsing, such as a comma.
pub fn format_mailbox(name: &str, address: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return address.to_string();
    }
    if address.is_empty() {
        return name.to_string();
    }
    let special = name.chars().any(|c| {
        matches!(
            c,
            ',' | ';' | '<' | '>' | '"' | '@' | '(' | ')' | ':' | '\\' | '[' | ']'
        )
    });
    let already_quoted = name.len() >= 2 && name.starts_with('"') && name.ends_with('"');
    if special && !already_quoted {
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\" <{address}>")
    } else {
        format!("{name} <{address}>")
    }
}

/// Like `parse_addresses`, but reduces each `Name <addr@host>` entry to its
/// bare address. The named form is accepted in input (drafts and completion),
/// while providers expect the address alone.
pub fn parse_bare_addresses(s: &str) -> Vec<String> {
    parse_addresses(s)
        .into_iter()
        .map(|a| extract_email(&a).unwrap_or(a))
        .collect()
}

/// Extracts the email address from a `Display <addr@host>` string.
pub fn extract_email(from: &str) -> Option<String> {
    if let (Some(open), Some(close)) = (from.find('<'), from.rfind('>')) {
        if open < close {
            let inner = from[open + 1..close].trim();
            if inner.contains('@') {
                return Some(inner.to_string());
            }
        }
    }
    let trimmed = from.trim();
    if trimmed.contains('@') {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// Display-name part of `Display <addr@host>`, falling back to the address.
/// A quoted name keeps its commas and has its escapes undone.
pub fn display_name(from: &str) -> String {
    let trimmed = from.trim();
    let name_end = if trimmed.starts_with('"') {
        let mut escaped = false;
        let mut end = None;
        for (i, c) in trimmed.char_indices().skip(1) {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                end = Some(i + 1);
                break;
            }
        }
        end.unwrap_or(trimmed.len())
    } else {
        trimmed.find('<').unwrap_or(trimmed.len())
    };
    let name = trimmed[..name_end].trim();
    if name.is_empty() {
        extract_email(from).unwrap_or_else(|| from.to_string())
    } else if name.len() >= 2 && name.starts_with('"') && name.ends_with('"') {
        unescape_quoted(&name[1..name.len() - 1])
    } else {
        name.trim_matches('"').to_string()
    }
}

fn unescape_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Stable per-account color: user override (`0xRRGGBB`) or hue
/// derived from the ID hash.
pub fn account_color(account_id: &AccountId, override_rgb: Option<u32>) -> Hsla {
    if let Some(rgb) = override_rgb {
        return gpui_kit::Rgba {
            r: ((rgb >> 16) & 0xff) as f32 / 255.0,
            g: ((rgb >> 8) & 0xff) as f32 / 255.0,
            b: (rgb & 0xff) as f32 / 255.0,
            a: 1.0,
        }
        .into();
    }
    let mut hash: u32 = 0;
    for b in account_id.0.as_bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(*b as u32);
    }
    gpui_kit::hsla((hash % 360) as f32 / 360.0, 0.55, 0.55, 1.0)
}

/// Color derived from a tag name when the provider does not supply one.
pub fn name_color(name: &str) -> Hsla {
    let mut hash: u32 = 0;
    for b in name.as_bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(*b as u32);
    }
    gpui_kit::hsla((hash % 360) as f32 / 360.0, 0.45, 0.45, 1.0)
}

/// Color of a tag packed as `0xRRGGBB`.
pub fn packed_color(rgb: u32) -> Hsla {
    gpui_kit::Rgba {
        r: ((rgb >> 16) & 0xff) as f32 / 255.0,
        g: ((rgb >> 8) & 0xff) as f32 / 255.0,
        b: (rgb & 0xff) as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

/// Converts a gpui color to `0xRRGGBB` for persisting account customization.
pub fn color_to_packed(color: Hsla) -> u32 {
    let rgb = color.to_rgb();
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
    (channel(rgb.r) << 16) | (channel(rgb.g) << 8) | channel(rgb.b)
}

/// Short list date: time for today, day otherwise, and year when different.
pub fn short_date(dt: &DateTime<Utc>) -> String {
    let local = dt.with_timezone(&Local);
    let now = Local::now();
    let locale = super::datefmt::current_locale();
    if local.date_naive() == now.date_naive() {
        local.format("%H:%M").to_string()
    } else if local.year_ce() == now.year_ce() {
        local.format_localized("%e %b", locale).to_string()
    } else {
        local.format_localized("%e %b %Y", locale).to_string()
    }
}

/// Time only for rows already grouped beneath a day separator.
pub fn short_time(dt: &DateTime<Utc>) -> String {
    dt.with_timezone(&Local).format("%H:%M").to_string()
}

/// Label for a daily group in the message list.
pub fn message_day_label(day: NaiveDate) -> String {
    let today = Local::now().date_naive();
    if day == today {
        tr!("date-today").to_string()
    } else if day == today - Duration::days(1) {
        tr!("date-yesterday-short").to_string()
    } else {
        let locale = super::datefmt::current_locale();
        if day.year() == today.year() {
            day.format_localized("%A %e %B", locale).to_string()
        } else {
            day.format_localized("%A %e %B %Y", locale).to_string()
        }
    }
}

/// Full date for the viewer header.
pub fn full_date(dt: &DateTime<Utc>) -> String {
    let locale = super::datefmt::current_locale();
    dt.with_timezone(&Local)
        .format_localized("%A %e %B %Y, %H:%M", locale)
        .to_string()
}

/// Cleans a message preview by collapsing repeated spaces and nbsp characters.
pub fn clean_preview(preview: &str) -> String {
    let mut out = String::with_capacity(preview.len());
    let mut last_space = false;
    for ch in preview.chars() {
        let is_space = ch.is_whitespace() || ch == '\u{a0}';
        if is_space {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out.trim().to_string()
}

/// Performs a rough check that an address resembles an email address.
pub fn is_valid_email(s: &str) -> bool {
    let s = s.trim();
    match s.split_once('@') {
        Some((user, host)) => !user.is_empty() && host.contains('.') && !host.ends_with('.'),
        None => false,
    }
}

/// Storage key for a tag on a message, depending on the provider (Graph
/// references categories by display name, Gmail/IMAP by ID).
pub fn tag_storage_key(provider: crate::model::Provider, tag: &crate::model::Tag) -> String {
    match provider {
        crate::model::Provider::Microsoft => tag.display_name.clone(),
        _ => tag.id.clone(),
    }
}

/// Deduplicates headers by account and ID for pagination. Provider IDs are
/// unique only within an
/// account.
pub fn dedup_append(list: &mut Vec<MessageHeader>, extra: Vec<MessageHeader>) {
    for m in extra {
        if !list
            .iter()
            .any(|x| x.account_id == m.account_id && x.id == m.id)
        {
            list.push(m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{display_name, format_mailbox, parse_addresses, parse_bare_addresses};

    #[test]
    fn quoted_name_with_comma_stays_one_recipient() {
        let list = r#""Doe, Jane" <jane@example.com>, bob@example.com"#;
        assert_eq!(
            parse_addresses(list),
            vec![r#""Doe, Jane" <jane@example.com>"#, "bob@example.com"]
        );
        assert_eq!(
            parse_bare_addresses(list),
            vec!["jane@example.com", "bob@example.com"]
        );
    }

    #[test]
    fn escaped_quotes_and_semicolons_do_not_split() {
        let list = r#""Doe \"JD\", Jane; Jr" <jane@example.com>; a@example.com"#;
        assert_eq!(parse_addresses(list).len(), 2);
        assert_eq!(
            display_name(&parse_addresses(list)[0]),
            r#"Doe "JD", Jane; Jr"#
        );
    }

    #[test]
    fn plain_lists_and_single_address_still_split() {
        assert_eq!(parse_addresses("a@x.test, b@x.test; c@x.test").len(), 3);
        assert_eq!(parse_addresses("a@x.test"), vec!["a@x.test"]);
        assert_eq!(parse_addresses("Jane <a@x.test>, b@x.test").len(), 2);
    }

    #[test]
    fn format_mailbox_quotes_only_when_needed() {
        assert_eq!(
            format_mailbox("Jane Doe", "j@x.test"),
            "Jane Doe <j@x.test>"
        );
        let quoted = format_mailbox("Doe, Jane", "j@x.test");
        assert_eq!(quoted, r#""Doe, Jane" <j@x.test>"#);
        assert_eq!(display_name(&quoted), "Doe, Jane");
        assert_eq!(parse_addresses(&quoted), vec![quoted.clone()]);
        let tricky = format_mailbox(r#"Doe "J", Jane"#, "j@x.test");
        assert_eq!(display_name(&tricky), r#"Doe "J", Jane"#);
        assert_eq!(format_mailbox("", "j@x.test"), "j@x.test");
    }
    use super::{color_to_packed, packed_color};

    #[test]
    fn packed_color_round_trip() {
        for rgb in [0x000000, 0x4A90E2, 0xFFFFFF] {
            assert_eq!(color_to_packed(packed_color(rgb)), rgb);
        }
    }
}
