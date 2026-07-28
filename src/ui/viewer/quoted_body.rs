//! Splits quoted history included in an email body.
//!
//! Providers often deliver each reply with the entire previous conversation
//! nested in its HTML. The provider-native thread therefore does not prevent
//! a very long body and is not always available in IMAP. This module recognizes
//! containers produced by major clients and returns the current message followed
//! by independent quoted fragments. gpui can then present them as collapsible
//! sub-blocks without losing the original HTML used by Blitz.

use crate::model::{BodyFormat, Message, MessageHeader};
use crate::providers::html::{collapse_blank_lines, convert_email_html};
use chrono::{DateTime, FixedOffset, Local, TimeZone, Utc};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub(super) struct QuotedBody {
    pub(super) current: Option<BodyPart>,
    pub(super) quoted: Vec<BodyPart>,
}

#[derive(Debug, Clone)]
pub(super) struct BodyPart {
    body: String,
    raw_html: Option<String>,
    pub(super) preview: String,
    headers: QuotedHeaders,
}

#[derive(Debug, Clone, Default)]
struct QuotedHeaders {
    from: Option<String>,
    to: Vec<String>,
    cc: Vec<String>,
    subject: Option<String>,
    received: Option<DateTime<Utc>>,
}

impl BodyPart {
    /// Construit la vue `Message` minimale attendue par les renderers. Les
    /// attachments are not copied, and only CID images actually referenced by
    /// this fragment are cloned.
    pub(super) fn as_message(&self, source: &Message, suffix: &str) -> Message {
        let mut header = source.header.clone();
        header.id = format!("{}:{suffix}", source.header.id);
        if let Some(from) = &self.headers.from {
            header.from = from.clone();
        }
        if let Some(subject) = &self.headers.subject {
            header.subject = subject.clone();
        }
        if let Some(received) = self.headers.received {
            header.received = received;
        }
        let references = self.raw_html.as_deref().unwrap_or(&self.body);
        Message {
            header,
            body: self.body.clone(),
            format: if self.raw_html.is_some() {
                BodyFormat::Markdown
            } else {
                source.format
            },
            inline_images: source
                .inline_images
                .iter()
                .filter(|image| references.contains(&image.cid))
                .cloned()
                .collect(),
            attachments: Vec::new(),
            tags: Vec::new(),
            raw_body: self.raw_html.clone(),
            to: self.headers.to.clone(),
            cc: self.headers.cc.clone(),
            bcc: Vec::new(),
            draft_id: None,
            invitation: None,
        }
    }

    pub(super) fn can_reply(&self) -> bool {
        self.headers
            .from
            .as_deref()
            .and_then(super::super::util::extract_email)
            .is_some()
    }

    pub(super) fn received(&self) -> Option<DateTime<Utc>> {
        self.headers.received
    }

    /// Whether this embedded quote can be tied reliably to a provider-native
    /// message from the loaded conversation. A date plus sender or subject is
    /// sufficient; without a date, both sender and subject must match.
    pub(super) fn matches_header(&self, header: &MessageHeader) -> bool {
        let from_matches = self.headers.from.as_deref().is_some_and(|from| {
            normalized_address(from).eq_ignore_ascii_case(&normalized_address(&header.from))
        });
        let subject = self.headers.subject.as_deref().map(str::trim);
        let subject_matches =
            subject.is_some_and(|subject| subject.eq_ignore_ascii_case(header.subject.trim()));
        // Quoting a reply rewrites its "Objet :" line as often as not: Outlook
        // repeats the subject of the mail it is answering rather than the one
        // it quotes, and a thread accumulates "RE:"/"TR :" as it goes. When a
        // date is there to discriminate, the subject only has to corroborate,
        // so compare it without those prefixes.
        let subject_corroborates = subject.is_some_and(|subject| {
            super::strip_reply_prefixes(subject)
                .eq_ignore_ascii_case(super::strip_reply_prefixes(&header.subject))
        });
        let date_matches = self
            .headers
            .received
            .is_some_and(|received| (received - header.received).num_seconds().abs() <= 300);

        if self.headers.received.is_some() {
            date_matches
                && (self.headers.from.is_none() || from_matches)
                && (subject.is_none() || subject_corroborates)
                && (self.headers.from.is_some() || subject.is_some())
        } else {
            from_matches && subject_matches
        }
    }
}

fn normalized_address(address: &str) -> String {
    super::super::util::extract_email(address).unwrap_or_else(|| address.trim().to_string())
}

const CACHE_CAPACITY: usize = 32;
thread_local! {
    /// gpui renders on a single thread. A local cache avoids traversing the DOM
    /// and reconverting each fragment on every `cx.notify()`.
    static SPLIT_CACHE: RefCell<VecDeque<(u64, Option<QuotedBody>)>> =
        const { RefCell::new(VecDeque::new()) };
}

/// Separates a message's own content from its quoted replies.
///
/// `None` means that no sufficiently reliable boundary was found:
/// l'appelant doit alors rendre le message original tel quel.
pub(super) fn split_message(message: &Message) -> Option<QuotedBody> {
    let key = split_cache_key(message);
    if let Some(cached) = SPLIT_CACHE.with(|cache| {
        cache
            .borrow()
            .iter()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, value)| value.clone())
    }) {
        return cached;
    }

    let split = split_message_uncached(message);
    SPLIT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= CACHE_CAPACITY {
            cache.pop_front();
        }
        cache.push_back((key, split.clone()));
    });
    split
}

fn split_cache_key(message: &Message) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    message.header.id.hash(&mut hasher);
    message.body.hash(&mut hasher);
    message.raw_body.hash(&mut hasher);
    hasher.finish()
}

fn split_message_uncached(message: &Message) -> Option<QuotedBody> {
    let (current, quoted) = match message.raw_body.as_deref() {
        Some(html) => split_html(html)
            .map(|(current, quoted)| {
                let current = current
                    .filter(|html| html_has_visible_content(html))
                    .map(|html| html_part(&html, message));
                let quoted = quoted
                    .into_iter()
                    .filter(|html| html_has_quoted_content(html))
                    .map(|html| html_part(&html, message))
                    .collect::<Vec<_>>();
                (current, quoted)
            })
            // Certains clients envoient du HTML sans conteneur de citation
            // identifiable, mais leur conversion Markdown conserve les `>`.
            .or_else(|| {
                split_markdown(&message.body).map(|parts| parts_from_markdown(parts, true))
            })?,
        None => split_markdown(&message.body).map(|parts| parts_from_markdown(parts, false))?,
    };

    if quoted.is_empty() {
        return None;
    }

    Some(QuotedBody { current, quoted })
}

fn html_part(html: &str, source: &Message) -> BodyPart {
    // A fragment extracted from `<body>` no longer contains `<head>` rules.
    // Reinjecting them preserves email classes and fonts in Blitz; the Markdown
    // converter already ignores `head` and `style`.
    let inherited;
    let html = if html.contains("<head") {
        html
    } else if let Some(head) = source.raw_body.as_deref().and_then(document_head) {
        inherited = format!("{head}{html}");
        &inherited
    } else {
        html
    };
    let mut body = collapse_blank_lines(&convert_email_html(html));
    for image in &source.inline_images {
        body = body.replace(
            &format!("cid:{}", image.cid),
            &format!("bytes://cid-{}", image.cid),
        );
    }
    BodyPart {
        // Inherited HTML may contain Outlook CSS comments that `scraper::text()`
        // treats as visible text. Markdown has already passed through the
        // converter's head/style/script exclusions, so it is the reliable source
        // for the collapsed-block summary.
        preview: markdown_body_preview(&body),
        headers: extract_quoted_headers(&body),
        body,
        raw_html: Some(html.to_string()),
    }
}

fn document_head(html: &str) -> Option<String> {
    static HEAD: OnceLock<Selector> = OnceLock::new();
    let selector = HEAD.get_or_init(|| Selector::parse("head").expect("head selector"));
    Html::parse_document(html)
        .select(selector)
        .next()
        .map(|head| head.html())
        .filter(|head| html_has_visible_head(head))
}

fn html_has_visible_head(head: &str) -> bool {
    // html5ever adds an empty `<head></head>` even to fragments. Copying it is
    // useful only when it actually contains metadata or CSS.
    head.len() > "<head></head>".len()
}

fn parts_from_markdown(
    (current, quoted): (Option<String>, Vec<String>),
    synthesize_html: bool,
) -> (Option<BodyPart>, Vec<BodyPart>) {
    let make = |body: String| BodyPart {
        preview: markdown_body_preview(&body),
        headers: extract_quoted_headers(&body),
        raw_html: synthesize_html.then(|| markdown_to_html(&body)),
        body,
    };
    (current.map(make), quoted.into_iter().map(make).collect())
}

fn extract_quoted_headers(body: &str) -> QuotedHeaders {
    let mut headers = QuotedHeaders::default();
    for line in body.lines().take(24) {
        let line = clean_markdown_header_line(line);
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        let label = label.trim().to_lowercase();
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match label.as_str() {
            "de" | "from" | "van" | "von" | "sender" | "expéditeur" => {
                headers.from.get_or_insert_with(|| value.to_string());
            }
            "à" | "to" | "aan" | "an" if headers.to.is_empty() => {
                headers.to = split_header_addresses(value);
            }
            "cc" | "copie" if headers.cc.is_empty() => {
                headers.cc = split_header_addresses(value);
            }
            "objet" | "subject" | "onderwerp" | "betreff" => {
                headers.subject.get_or_insert_with(|| value.to_string());
            }
            "date" | "datum" | "envoyé" | "envoye" | "sent" | "verzonden" | "gesendet" => {
                headers.received = headers.received.or_else(|| parse_header_date(value));
            }
            _ => {}
        }
    }

    // Gmail often places all metadata in an "On ... <mail> wrote" line
    // au lieu de lignes De/Date/Objet distinctes.
    if headers.from.is_none() {
        static EMAIL: OnceLock<Regex> = OnceLock::new();
        let email = EMAIL.get_or_init(|| {
            Regex::new(r"(?i)[a-z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-z0-9.-]+\.[a-z]{2,}")
                .expect("email regex")
        });
        for line in body.lines().take(12) {
            let lower = line.to_lowercase();
            if [
                "wrote",
                "a écrit",
                "schrieb",
                "schreef",
                "escribió",
                "ha scritto",
            ]
            .iter()
            .any(|marker| lower.contains(marker))
            {
                if let Some(address) = email.find(line) {
                    headers.from = Some(address.as_str().to_string());
                }
                break;
            }
        }
    }
    headers
}

fn clean_markdown_header_line(line: &str) -> String {
    let mut line = line.trim();
    while let Some(rest) = line.strip_prefix('>') {
        line = rest.trim_start();
    }
    line.replace("**", "").replace("__", "")
}

fn split_header_addresses(value: &str) -> Vec<String> {
    value
        .split(';')
        .map(str::trim)
        .filter(|address| !address.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_header_date(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(date) = DateTime::parse_from_rfc3339(value) {
        return Some(date.with_timezone(&Utc));
    }
    if let Ok(date) = DateTime::parse_from_rfc2822(value) {
        return Some(date.with_timezone(&Utc));
    }

    static FRENCH: OnceLock<Regex> = OnceLock::new();
    let french = FRENCH.get_or_init(|| {
        Regex::new(
            r"(?ix)^(?:(?:lundi|mardi|mercredi|jeudi|vendredi|samedi|dimanche)\s+)?
                (\d{1,2})\s+([[:alpha:]éû]+)\s+(\d{4})\s+(?:à\s+)?
                (\d{1,2}):(\d{2})(?::(\d{2}))?(?:\s+UTC([+-]\d{1,2}))?$",
        )
        .expect("French date regex")
    });
    let captures = french.captures(value.trim())?;
    let month = match captures[2].to_lowercase().as_str() {
        "janvier" => 1,
        "février" => 2,
        "mars" => 3,
        "avril" => 4,
        "mai" => 5,
        "juin" => 6,
        "juillet" => 7,
        "août" => 8,
        "septembre" => 9,
        "octobre" => 10,
        "novembre" => 11,
        "décembre" => 12,
        _ => return None,
    };
    let parse = |index: usize| captures[index].parse::<u32>().ok();
    let day = parse(1)?;
    let year = captures[3].parse::<i32>().ok()?;
    let hour = parse(4)?;
    let minute = parse(5)?;
    let second = captures
        .get(6)
        .and_then(|value| value.as_str().parse::<u32>().ok())
        .unwrap_or(0);
    if let Some(offset_hours) = captures
        .get(7)
        .and_then(|value| value.as_str().parse::<i32>().ok())
    {
        let offset = FixedOffset::east_opt(offset_hours * 3600)?;
        offset
            .with_ymd_and_hms(year, month, day, hour, minute, second)
            .single()
            .map(|date| date.with_timezone(&Utc))
    } else {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, second)
            .single()
            .map(|date| date.with_timezone(&Utc))
    }
}

fn markdown_to_html(markdown: &str) -> String {
    let parser = pulldown_cmark::Parser::new_ext(markdown, pulldown_cmark::Options::all());
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    html
}

// -------------------------------------------------------------------------
// HTML
// -------------------------------------------------------------------------

fn quote_selector() -> &'static Selector {
    static SELECTOR: OnceLock<Selector> = OnceLock::new();
    SELECTOR.get_or_init(|| {
        Selector::parse(
            r#"blockquote[type="cite"],
               div.gmail_quote, div.gmail_quote_container,
               div.yahoo_quoted, div.protonmail_quote,
               div[class~="moz-forward-container"]"#,
        )
        .expect("quoted-message selector")
    })
}

/// Conteneurs Gmail/Yahoo/Proton et `blockquote[type=cite]` Apple/Thunderbird.
/// The outermost elements become sections; nested boundaries are then
/// extracted recursively.
fn split_explicit_html(html: &str) -> Option<(Option<String>, Vec<String>)> {
    let mut document = Html::parse_document(html);
    let roots = document
        .select(quote_selector())
        .filter(|element| !has_quote_ancestor(*element))
        .map(|element| (element.id(), element.html()))
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return None;
    }

    for (id, _) in &roots {
        if let Some(mut node) = document.tree.get_mut(*id) {
            node.detach();
        }
    }

    let current = document.html();
    let mut quoted = Vec::new();
    for (_, fragment) in roots {
        flatten_quote_fragment(&fragment, &mut quoted, 0);
    }
    Some((Some(current), quoted))
}

fn has_quote_ancestor(element: ElementRef<'_>) -> bool {
    element
        .ancestors()
        .filter_map(ElementRef::wrap)
        .any(|ancestor| quote_selector().matches(&ancestor))
}

fn flatten_quote_fragment(fragment: &str, output: &mut Vec<String>, depth: usize) {
    // Les courriels pathologiques peuvent imbriquer des centaines de niveaux.
    // A generous limit protects the UI thread from hostile recursion.
    if depth >= 64 {
        output.push(fragment.to_string());
        return;
    }

    let mut document = Html::parse_fragment(fragment);
    let boundaries = document
        .select(quote_selector())
        .filter(|element| has_quote_ancestor(*element))
        .filter(|element| {
            // Ne retenir que la prochaine profondeur, pas tous les descendants
            // at once. They will be processed by the recursive call.
            element
                .ancestors()
                .filter_map(ElementRef::wrap)
                .filter(|ancestor| quote_selector().matches(ancestor))
                .count()
                == 1
        })
        .map(|element| (element.id(), element.html()))
        .collect::<Vec<_>>();

    if boundaries.is_empty() {
        output.push(fragment.to_string());
        return;
    }

    for (id, _) in &boundaries {
        if let Some(mut node) = document.tree.get_mut(*id) {
            node.detach();
        }
    }
    let outer = document.html();
    if html_has_visible_content(&outer) {
        output.push(outer);
    }
    for (_, nested) in boundaries {
        flatten_quote_fragment(&nested, output, depth + 1);
    }
}

/// Outlook generally places a `divRplyFwdMsg` marker (desktop) or `appendonsend`
/// (new Outlook) before copied content. Splitting at markers is more reliable
/// than guessing which of their many sibling `<div>` elements contains the
/// quoted body; html5ever repairs markup fragments.
fn split_outlook_html(html: &str) -> Option<(Option<String>, Vec<String>)> {
    static MARKER: OnceLock<Regex> = OnceLock::new();
    let marker = MARKER.get_or_init(|| {
        Regex::new(
            r#"(?isx)<(?:div|hr)\b[^>]*(?:
                \bid\s*=\s*["']?(?:divRplyFwdMsg|appendonsend)\b|
                \bclass\s*=\s*["'][^"']*\bOutlookMessageHeader\b
            )[^>]*>"#,
        )
        .expect("Outlook quote marker regex")
    });
    let mut starts = marker
        .find_iter(html)
        .map(|m| m.start())
        .collect::<Vec<_>>();
    if starts.is_empty() {
        starts = outlook_transport_header_starts(html);
    }
    let first = *starts.first()?;

    let current = (first > 0).then(|| html[..first].to_string());
    let mut quoted = Vec::new();
    for (index, start) in starts.iter().copied().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(html.len());
        quoted.push(html[start..end].to_string());
    }
    Some((current, quoted))
}

/// Outlook desktop sometimes omits its usual reply marker and emits only the
/// Word-generated transport header. It is recognizable by the horizontal
/// border and the complete From/Sent/To/Subject label set. Requiring both
/// signals avoids splitting an ordinary table or prose that happens to mention
/// a subset of those words.
fn outlook_transport_header_starts(html: &str) -> Vec<usize> {
    static DIV_OPEN: OnceLock<Regex> = OnceLock::new();
    static DIV_SELECTOR: OnceLock<Selector> = OnceLock::new();
    let div_open = DIV_OPEN.get_or_init(|| {
        Regex::new(r#"(?is)<div\b(?:"[^"]*"|'[^']*'|[^>])*>"#).expect("opening div regex")
    });
    let div_selector = DIV_SELECTOR.get_or_init(|| Selector::parse("div").expect("div selector"));

    div_open
        .find_iter(html)
        .filter(|candidate| {
            candidate
                .as_str()
                .to_ascii_lowercase()
                .contains("border-top")
        })
        .filter_map(|candidate| {
            let fragment = Html::parse_fragment(&html[candidate.start()..]);
            let header = fragment.select(div_selector).next()?;
            let text = header.text().collect::<Vec<_>>().join(" ");
            looks_like_outlook_transport_header(&text).then_some(candidate.start())
        })
        .collect()
}

fn looks_like_outlook_transport_header(text: &str) -> bool {
    static LABELS: OnceLock<Vec<Regex>> = OnceLock::new();
    let labels = LABELS.get_or_init(|| {
        [
            r"(?i)(?:^|\s)(?:from|de|van|von|sender|expéditeur)\s*:",
            r"(?i)(?:^|\s)(?:sent|envoyé|envoye|verzonden|gesendet|date|datum)\s*:",
            r"(?i)(?:^|\s)(?:to|à|aan|an)\s*:",
            r"(?i)(?:^|\s)(?:subject|objet|onderwerp|betreff)\s*:",
        ]
        .into_iter()
        .map(|pattern| Regex::new(pattern).expect("Outlook header-label regex"))
        .collect()
    });
    labels.iter().all(|label| label.is_match(text))
}

fn split_html(html: &str) -> Option<(Option<String>, Vec<String>)> {
    let merge_fragments = is_single_forwarded_message(html);
    let repaired = super::super::blitz_body::repair_fragmented_outlook_cids(html);
    let mut split = split_explicit_html(&repaired).or_else(|| split_outlook_html(&repaired))?;
    if merge_fragments && split.1.len() > 1 {
        split.1 = vec![split.1.concat()];
    }
    Some(split)
}

/// Apple Mail and Outlook may serialize the header, content, and each CID
/// resource of a single forward into sibling `blockquote` elements. These
/// boundaries are technical fragments, not separate messages.
fn is_single_forwarded_message(html: &str) -> bool {
    static FORWARDED: OnceLock<Regex> = OnceLock::new();
    let forwarded = FORWARDED.get_or_init(|| {
        Regex::new(
            r"(?i)(begin forwarded message|début du message transféré|begin doorgestuurd bericht|anfang der weitergeleiteten nachricht|inicio del mensaje reenviado)",
        )
        .expect("forwarded-message marker regex")
    });
    let fragmented_outlook =
        html.contains("role=\"textbox\"") && html.match_indices("cid:").nth(1).is_some();
    fragmented_outlook || forwarded.is_match(html)
}

fn html_has_visible_content(html: &str) -> bool {
    let document = Html::parse_fragment(html);
    if html_document_has_text(&document) {
        return true;
    }
    static MEDIA: OnceLock<Selector> = OnceLock::new();
    let media = MEDIA.get_or_init(|| {
        Selector::parse("img, table, video, audio, svg, hr").expect("visible media selector")
    });
    document.select(media).next().is_some()
}

fn html_has_quoted_content(html: &str) -> bool {
    let document = Html::parse_fragment(html);
    if html_document_has_text(&document) {
        return true;
    }
    // Outlook can emit `appendonsend`, an `<hr>`, then `divRplyFwdMsg`.
    // The rule is only transport scaffolding between two quote markers, not a
    // quoted message of its own. Other non-text content remains significant.
    static MEDIA: OnceLock<Selector> = OnceLock::new();
    let media = MEDIA.get_or_init(|| {
        Selector::parse("img, table, video, audio, svg").expect("quoted media selector")
    });
    document.select(media).next().is_some()
}

fn html_document_has_text(document: &Html) -> bool {
    document.root_element().text().any(|text| {
        text.chars()
            .any(|ch| !ch.is_whitespace() && ch != '\u{feff}')
    })
}

// -------------------------------------------------------------------------
// Markdown et text/plain
// -------------------------------------------------------------------------

fn split_markdown(markdown: &str) -> Option<(Option<String>, Vec<String>)> {
    if let Some(parts) = split_reply_separators(markdown) {
        return Some(parts);
    }

    let lines = markdown.lines().collect::<Vec<_>>();
    let first_quote = lines.iter().position(|line| quote_depth(line) > 0)?;
    let max_depth = lines[first_quote..]
        .iter()
        .map(|line| quote_depth(line))
        .max()
        .unwrap_or(0);
    if max_depth == 0 {
        return None;
    }
    let quoted_nonempty = lines[first_quote..]
        .iter()
        .filter(|line| {
            !strip_quote_prefix(line, quote_depth(line))
                .trim()
                .is_empty()
        })
        .count();
    let quoted_chars = lines[first_quote..]
        .iter()
        .map(|line| strip_quote_prefix(line, quote_depth(line)).chars().count())
        .sum::<usize>();
    if max_depth == 1
        && quoted_nonempty < 3
        && quoted_chars < 160
        && !has_reply_intro(&lines, first_quote)
    {
        // Un blockquote bref est vraisemblablement une citation volontaire,
        // pas la copie automatique d'un ancien courriel.
        return None;
    }

    let current = lines[..first_quote].join("\n");
    let mut quoted = vec![String::new(); max_depth];
    let mut active_depth: usize = 1;
    for line in &lines[first_quote..] {
        let depth = quote_depth(line);
        if depth == 0 {
            // A signature or unprefixed separator in the middle of the block
            // belongs to the outer quote.
            quoted[active_depth.saturating_sub(1)].push_str(line);
            quoted[active_depth.saturating_sub(1)].push('\n');
            continue;
        }
        active_depth = depth.min(max_depth);
        let stripped = strip_quote_prefix(line, depth);
        quoted[active_depth - 1].push_str(stripped);
        quoted[active_depth - 1].push('\n');
    }
    let quoted = quoted
        .into_iter()
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    (!quoted.is_empty()).then(|| {
        let current = current.trim().to_string();
        ((!current.is_empty()).then_some(current), quoted)
    })
}

fn has_reply_intro(lines: &[&str], first_quote: usize) -> bool {
    static INTRO: OnceLock<Regex> = OnceLock::new();
    let intro = INTRO.get_or_init(|| {
        Regex::new(
            r"(?i)(\bwrote\s*:|a écrit\s*:|schrieb\s*:|schreef\s*:|escribió\s*:|ha scritto\s*:|\*\*(from|de|van|von|sender|expéditeur)\s*:)",
        )
        .expect("reply introduction regex")
    });
    lines[first_quote.saturating_sub(5)..]
        .iter()
        .take(12)
        .any(|line| intro.is_match(line))
}

fn split_reply_separators(markdown: &str) -> Option<(Option<String>, Vec<String>)> {
    let lines = markdown.lines().collect::<Vec<_>>();
    let starts = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| is_reply_separator(&lines, index, line).then_some(index))
        .collect::<Vec<_>>();
    let first = *starts.first()?;
    let current = lines[..first].join("\n").trim().to_string();
    let mut quoted = Vec::new();
    for (position, start) in starts.iter().copied().enumerate() {
        let end = starts.get(position + 1).copied().unwrap_or(lines.len());
        let part = lines[start..end].join("\n").trim().to_string();
        if !part.is_empty() {
            quoted.push(part);
        }
    }
    (!quoted.is_empty()).then(|| ((!current.is_empty()).then_some(current), quoted))
}

fn is_reply_separator(lines: &[&str], index: usize, line: &str) -> bool {
    static NAMED: OnceLock<Regex> = OnceLock::new();
    let named = NAMED.get_or_init(|| {
        Regex::new(
            r"(?i)^\s*-{2,}\s*(original message|message d['’]origine|oorspronkelijk bericht|urspr(?:ü|u)ngliche nachricht|mensaje original|forwarded message)\s*-{2,}\s*$",
        )
        .expect("reply separator regex")
    });
    if named.is_match(line) {
        return true;
    }

    let trimmed = line.trim();
    if !matches!(trimmed, "---" | "___" | "***") {
        return false;
    }
    lines
        .iter()
        .skip(index + 1)
        .take(6)
        .map(|candidate| candidate.trim().to_ascii_lowercase())
        .any(|candidate| {
            [
                "**from:**",
                "**de :**",
                "**de:**",
                "**van:**",
                "**von:**",
                "**expéditeur :**",
                "**sender:**",
            ]
            .iter()
            .any(|prefix| candidate.starts_with(prefix))
        })
}

fn quote_depth(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut index = 0;
    let mut depth = 0;
    while index < bytes.len() {
        let spaces = bytes[index..]
            .iter()
            .take_while(|byte| **byte == b' ')
            .count()
            .min(3);
        index += spaces;
        if bytes.get(index) != Some(&b'>') {
            break;
        }
        depth += 1;
        index += 1;
        if bytes.get(index) == Some(&b' ') {
            index += 1;
        }
    }
    depth
}

fn strip_quote_prefix(line: &str, depth: usize) -> &str {
    let bytes = line.as_bytes();
    let mut index = 0;
    for _ in 0..depth {
        let spaces = bytes[index..]
            .iter()
            .take_while(|byte| **byte == b' ')
            .count()
            .min(3);
        index += spaces;
        if bytes.get(index) != Some(&b'>') {
            break;
        }
        index += 1;
        if bytes.get(index) == Some(&b' ') {
            index += 1;
        }
    }
    &line[index..]
}

fn text_preview(text: &str) -> String {
    const MAX_CHARS: usize = 240;
    let mut normalized = String::with_capacity(text.len().min(MAX_CHARS));
    let mut pending_blank_line = false;
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            if !normalized.is_empty() {
                pending_blank_line = true;
            }
            continue;
        }
        if !normalized.is_empty() {
            normalized.push('\n');
            if pending_blank_line {
                normalized.push('\n');
            }
        }
        normalized.push_str(&line);
        pending_blank_line = false;
    }

    if normalized.chars().count() <= MAX_CHARS {
        return normalized;
    }
    let mut preview = normalized.chars().take(MAX_CHARS).collect::<String>();
    while preview.ends_with('\n') {
        preview.pop();
    }
    preview.push('…');
    preview
}

fn ensure_preview_break(text: &mut String, lines: usize) {
    while matches!(text.as_bytes().last(), Some(b' ' | b'\t')) {
        text.pop();
    }
    let existing = text.chars().rev().take_while(|ch| *ch == '\n').count();
    for _ in existing..lines {
        text.push('\n');
    }
}

pub(super) fn markdown_preview(markdown: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, TagEnd};

    let mut plain = String::with_capacity(markdown.len().min(256));
    for event in Parser::new_ext(markdown, Options::all()) {
        match event {
            Event::Text(text) | Event::Code(text) => plain.push_str(&text),
            Event::SoftBreak | Event::HardBreak => ensure_preview_break(&mut plain, 1),
            Event::End(TagEnd::Paragraph) => ensure_preview_break(&mut plain, 2),
            Event::End(TagEnd::Heading(_) | TagEnd::Item | TagEnd::BlockQuote(_)) => {
                ensure_preview_break(&mut plain, 1);
            }
            // Residual inline HTML (images, underlined spans, etc.) must never
            // become markup again in the preview.
            _ => {}
        }
    }
    text_preview(&plain)
}

/// Removes the transport block placed by email clients before the quoted body.
/// This information remains available in `QuotedHeaders` (and the date appears
/// in the banner), so it adds nothing to the summary.
fn markdown_body_preview(markdown: &str) -> String {
    let mut body = String::with_capacity(markdown.len());
    let mut scanning_headers = true;
    let mut found_header = false;

    for line in markdown.lines() {
        if scanning_headers {
            let clean = clean_markdown_header_line(line);
            let trimmed = clean.trim();
            if trimmed.is_empty() || matches!(trimmed, "---" | "___" | "***") {
                continue;
            }
            let is_header = trimmed
                .split_once(':')
                .map(|(label, _)| {
                    matches!(
                        label.trim().to_lowercase().as_str(),
                        "de" | "from"
                            | "van"
                            | "von"
                            | "sender"
                            | "expéditeur"
                            | "à"
                            | "to"
                            | "aan"
                            | "an"
                            | "cc"
                            | "copie"
                            | "objet"
                            | "subject"
                            | "onderwerp"
                            | "betreff"
                            | "date"
                            | "datum"
                            | "envoyé"
                            | "envoye"
                            | "sent"
                            | "verzonden"
                            | "gesendet"
                    )
                })
                .unwrap_or(false);
            if is_header {
                found_header = true;
                continue;
            }
            scanning_headers = false;
        }

        body.push_str(line);
        body.push('\n');
    }

    // If a client placed all metadata and the body on a
    // ligne unique, mieux vaut conserver le Markdown original que produire
    // an empty preview.
    if found_header && body.trim().is_empty() {
        markdown_preview(markdown)
    } else {
        markdown_preview(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountId, MessageHeader};
    use chrono::Utc;

    fn message(body: &str, html: Option<&str>) -> Message {
        Message {
            header: MessageHeader {
                account_id: AccountId("test".into()),
                id: "message-1".into(),
                subject: "Sujet".into(),
                from: "Contact A <contact-a@example.test>".into(),
                received: Utc::now(),
                preview: String::new(),
                is_read: true,
                is_flagged: false,
                has_attachments: false,
                tags: Vec::new(),
                last_action: None,
                last_action_at: None,
                conversation_id: None,
                internet_message_id: None,
            },
            body: body.into(),
            format: if html.is_some() {
                BodyFormat::Markdown
            } else {
                BodyFormat::Text
            },
            inline_images: Vec::new(),
            attachments: Vec::new(),
            tags: Vec::new(),
            raw_body: html.map(str::to_string),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            draft_id: None,
            invitation: None,
        }
    }

    #[test]
    fn separates_nested_gmail_history() {
        let html = r#"<div>Réponse actuelle</div>
            <div class="gmail_quote gmail_quote_container">
              <div class="gmail_attr">Le 17 juillet, Contact B a écrit :</div>
              <blockquote class="gmail_quote"><div>Réponse du contact B</div>
                <div class="gmail_quote"><div class="gmail_attr">Contact A a écrit :</div>
                  <blockquote class="gmail_quote"><div>Premier message</div></blockquote>
                </div>
              </blockquote>
            </div>"#;
        let split = split_message(&message("", Some(html))).expect("conversation detected");

        assert!(split
            .current
            .as_ref()
            .is_some_and(|part| part.body.contains("Réponse actuelle")));
        assert_eq!(split.quoted.len(), 2);
        assert!(split.quoted[0].body.contains("Réponse du contact B"));
        assert!(split.quoted[1].body.contains("Premier message"));
    }

    #[test]
    fn separates_nested_cite_blockquotes() {
        let html = r#"<p>Actuel</p><blockquote type="cite"><p>Précédent</p>
            <blockquote type="cite"><p>Ancien</p></blockquote></blockquote>"#;
        let split = split_message(&message("", Some(html))).expect("conversation detected");

        assert_eq!(split.quoted.len(), 2);
        assert!(split.quoted[0].body.contains("Précédent"));
        assert!(!split.quoted[0].body.contains("Ancien"));
        assert!(split.quoted[1].body.contains("Ancien"));
    }

    #[test]
    fn separates_markdown_quote_depths() {
        let body = "Réponse\n\nLe 17 juillet, Contact B a écrit :\n> Précédent\n>\n> Contact A a écrit :\n>> Ancien";
        let split = split_message(&message(body, None)).expect("conversation detected");

        assert_eq!(split.quoted.len(), 2);
        assert!(split.quoted[0].body.contains("Précédent"));
        assert!(split.quoted[1].body.contains("Ancien"));
    }

    #[test]
    fn keeps_a_short_editorial_quote_in_the_body() {
        let body = "Une citation littéraire :\n\n> Être ou ne pas être.";

        assert!(split_message(&message(body, None)).is_none());
    }

    #[test]
    fn separates_outlook_markers() {
        let html = r#"<div>Réponse</div><div id="divRplyFwdMsg"><b>De :</b> Contact B</div>
            <div>Précédent</div><div id="divRplyFwdMsg"><b>De :</b> Contact A</div><div>Ancien</div>"#;
        let split = split_message(&message("", Some(html))).expect("conversation detected");

        assert_eq!(split.quoted.len(), 2);
        assert!(split.quoted[0].body.contains("Précédent"));
        assert!(split.quoted[1].body.contains("Ancien"));
    }

    #[test]
    fn ignores_outlook_scaffolding_between_quote_markers() {
        let html = r#"<html><body><div><br></div>
            <div id="appendonsend"></div>
            <hr style="display:inline-block;width:98%">
            <div id="divRplyFwdMsg"><b>From:</b>
                Contact A &lt;contact-a@example.test&gt;<br>
                <b>Sent:</b> Monday, January 5, 2026 9:00 AM<br>
                <b>To:</b> Contact B &lt;contact-b@example.test&gt;<br>
                <b>Subject:</b> Previous subject
            </div>
            <div>Previous message body.</div>
            </body></html>"#;
        let split = split_message(&message("", Some(html))).expect("conversation detected");

        assert!(split.current.is_none());
        assert_eq!(split.quoted.len(), 1);
        assert!(split.quoted[0].body.contains("Previous message body."));
    }

    #[test]
    fn separates_outlook_word_header_without_reply_marker() {
        let html = r#"<html><head><style>
            p.MsoNormal { margin:0; font-family:"Aptos",sans-serif }
            </style></head><body><div class="WordSection1">
            <p class="MsoNormal"><span>Réponse actuelle distincte.</span></p>
            <table><tr><td>Signature de Contact A</td></tr></table>
            <div><div style="border:none; border-top:solid #E1E1E1 1.0pt;
                padding:3.0pt 0cm 0cm 0cm"><p class="MsoNormal">
                <b><span lang="EN-US">From:</span></b>
                <span lang="EN-US"> Contact B &lt;contact-b@example.test&gt;<br>
                <b>Sent:</b> lundi 3 août 2026 09:12<br>
                <b>To:</b> Contact A &lt;contact-a@example.test&gt;<br>
                <b>Subject:</b> Sujet précédent</span></p></div></div>
            <p class="MsoNormal">Message précédent distinct.</p>
            <img src="cid:previous-image">
            </div></body></html>"#;
        let split = split_message(&message("", Some(html))).expect("conversation detected");

        let current = split.current.expect("current message");
        assert!(current.body.contains("Réponse actuelle distincte."));
        assert!(!current.body.contains("Message précédent distinct."));
        assert!(!current
            .raw_html
            .as_deref()
            .expect("current HTML")
            .contains("previous-image"));

        assert_eq!(split.quoted.len(), 1);
        let quoted = &split.quoted[0];
        assert!(quoted.body.contains("Message précédent distinct."));
        assert!(!quoted.body.contains("Réponse actuelle distincte."));
        assert!(quoted
            .raw_html
            .as_deref()
            .expect("quoted HTML")
            .contains("previous-image"));
        assert_eq!(
            quoted.headers.from.as_deref(),
            Some("Contact B <contact-b@example.test>")
        );
    }

    #[test]
    fn outlook_preview_uses_clean_markdown_instead_of_head_css() {
        let html = r#"<html><head><style type="text/css"><!--
            p { margin-top:0; margin-bottom:0 }
            --></style></head><body><div>Réponse</div>
            <div id="divRplyFwdMsg"><b>De :</b> Support &lt;support@example.test&gt;<br>
            <b>Envoyé :</b> vendredi 17 juillet 2026 16:18:39<br>
            <b>À :</b> Client &lt;client@example.test&gt;<br>
            <b>Objet :</b> Un sujet</div><div>Salut, le vrai contenu cité.</div>
            </body></html>"#;
        let split = split_message(&message("", Some(html))).expect("conversation detected");
        let part = &split.quoted[0];
        let preview = &part.preview;

        assert!(preview.contains("Salut, le vrai contenu cité."));
        assert!(!preview.contains("De : Support"));
        assert!(!preview.contains("Envoyé"));
        assert!(!preview.contains("Objet"));
        assert!(!preview.contains("margin-top"));
        assert!(!preview.contains("<!--"));
        assert_eq!(
            part.received()
                .expect("quoted date")
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            "2026-07-17 16:18:39"
        );
    }

    #[test]
    fn markdown_preview_preserves_line_and_paragraph_breaks() {
        let preview = markdown_preview("Première ligne.  \nDeuxième ligne.\n\nNouveau paragraphe.");

        assert_eq!(
            preview,
            "Première ligne.\nDeuxième ligne.\n\nNouveau paragraphe."
        );
    }

    #[test]
    fn merges_fragmented_iphone_forward_into_one_message() {
        let html = r#"<div dir="auto"><div>Envoyé de mon iPhone</div>
            <div>Début du message transféré :</div>
            <blockquote type="cite"><div><b>De:</b> Contact B &lt;contact-b@example.test&gt;<br>
              <b>Date:</b> 17 juillet 2026 à 17:19:33 UTC+2<br>
              <b>Objet:</b> Message historique</div></blockquote>
            <blockquote type="cite"><div>Corps du message transféré</div></blockquote></div>
            <div><img src="cid:header"></div>
            <div role="textbox"><blockquote type="cite"><div>Pied de page</div></blockquote></div>
            <div><img src="cid:logo"></div>"#;
        let source = message("", Some(html));
        let split = split_message(&source).expect("conversation detected");

        assert_eq!(split.quoted.len(), 1);
        assert!(split.quoted[0].body.contains("Corps du message transféré"));
        assert!(split.quoted[0].body.contains("Pied de page"));
        assert!(split.quoted[0].can_reply());
        let target = split.quoted[0].as_message(&source, "quoted-0");
        assert_eq!(target.header.from, "Contact B <contact-b@example.test>");
        assert_eq!(target.header.subject, "Message historique");
        assert_eq!(
            target.header.received.to_rfc3339(),
            "2026-07-17T15:19:33+00:00"
        );
        assert!(split.quoted[0].matches_header(&target.header));
        let mut unrelated = target.header.clone();
        unrelated.from = "Contact C <contact-c@example.test>".into();
        assert!(!split.quoted[0].matches_header(&unrelated));
    }

    #[test]
    fn dated_quote_matches_through_accumulated_reply_prefixes() {
        let html = r#"<div>Ma réponse</div>
            <div id="divRplyFwdMsg"><b>De :</b> Contact B &lt;contact-b@example.test&gt;<br>
              <b>Envoyé :</b> 17 juillet 2026 à 17:19:33 UTC+2<br>
              <b>Objet :</b> RE: Message historique</div>
            <div>Corps cité</div>"#;
        let source = message("", Some(html));
        let split = split_message(&source).expect("conversation detected");
        let mut header = split.quoted[0].as_message(&source, "quoted-0").header;

        // Même mail, même date : seuls les préfixes de réponse diffèrent.
        header.subject = "TR : RE: Message historique".into();
        assert!(split.quoted[0].matches_header(&header));

        // Un autre objet reste un autre message, préfixes ou pas.
        header.subject = "RE: Autre sujet".into();
        assert!(!split.quoted[0].matches_header(&header));

        // Et la date reste le discriminant : même objet, autre horodatage.
        header.subject = "RE: Message historique".into();
        header.received += chrono::Duration::hours(3);
        assert!(!split.quoted[0].matches_header(&header));
    }
}
