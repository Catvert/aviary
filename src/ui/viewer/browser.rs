//! Message preview and printing through the system browser.

use super::super::util;
use crate::model::{BodyFormat, InlineImage, Message};

/// Produces a self-contained HTML copy of the message and opens it in the
/// system browser. Work remains off the UI thread: CID images can be large,
/// and encoding them as base64 must not block
/// gpui.
pub(super) fn open_message(message: Message, allow_remote: bool) {
    open_document(message, allow_remote, BrowserAction::Preview);
}

/// Opens a print-friendly, self-contained copy of the message and asks the
/// system browser to display its print dialog.
pub(crate) fn print_message(message: Message, allow_remote: bool) {
    open_document(message, allow_remote, BrowserAction::Print);
}

#[derive(Clone, Copy)]
enum BrowserAction {
    Preview,
    Print,
}

fn open_document(message: Message, allow_remote: bool, action: BrowserAction) {
    std::thread::spawn(move || {
        let html = browser_document(&message, allow_remote, action);
        let suffix = match action {
            BrowserAction::Preview => "message",
            BrowserAction::Print => "message-print",
        };
        let path = match write_private_document(&format!("{suffix}.html"), html.as_bytes()) {
            Ok(path) => path,
            Err(error) => {
                log::warn!("failed to create browser preview: {error:#}");
                return;
            }
        };
        if let Err(error) = open::that_detached(&path) {
            log::warn!("failed to open message in browser: {error:#}");
        }
    });
}

/// How long a previous preview is kept before the next open prunes it. The
/// browser may reload the file long after it opened it, so this is generous.
const STALE_PREVIEW_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Writes `bytes` into a fresh, private directory of its own and returns the
/// file's path.
///
/// `temp_dir()` is shared by every user of the machine: a predictable path
/// there could be pre-created by someone else — a symlink redirecting our
/// write, or a directory they can read. Each open therefore gets a random
/// `0700` subdirectory of a root we verified we own, and the file is created
/// with `create_new` (`O_EXCL`, which never follows a symlink at the last
/// component) and mode `0600`. The name reveals neither subject nor sender.
fn write_private_document(filename: &str, bytes: &[u8]) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context as _;
    use std::io::Write as _;

    let temp = std::env::temp_dir();
    let directory = match private_subdirectory(&temp.join("aviary-browser")) {
        Some(directory) => directory,
        // The shared root belongs to someone else (or is not a directory): a
        // uniquely named directory directly in the sticky temp dir is ours
        // alone, since `create_dir` fails if anything already has the name.
        None => {
            let directory = temp.join(format!("aviary-browser-{}", random_suffix()));
            create_private_dir(&directory)?;
            directory
        }
    };

    let path = directory.join(filename);
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Creates a fresh random subdirectory of `root` once `root` is a real
/// directory (not a symlink) owned by the current user and closed to everyone
/// else, creating `root` when missing. `None` means `root` cannot be trusted.
fn private_subdirectory(root: &std::path::Path) -> Option<std::path::PathBuf> {
    if let Err(error) = create_private_dir(root) {
        log::debug!("reusing browser preview root: {error:#}");
    }
    let metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) => {
            log::warn!("browser preview root unavailable: {error:#}");
            return None;
        }
    };
    if !metadata.file_type().is_dir() {
        log::warn!("browser preview root is not a directory; using a private one");
        return None;
    }
    let directory = root.join(random_suffix());
    if let Err(error) = create_private_dir(&directory) {
        log::warn!("browser preview directory: {error:#}");
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        // The directory we just created is ours by construction, which gives
        // our uid without a platform call.
        let ours = std::fs::symlink_metadata(&directory).map(|created| created.uid());
        if ours.ok() != Some(metadata.uid()) {
            log::warn!("browser preview root is owned by another user; using a private one");
            let _ = std::fs::remove_dir(&directory);
            return None;
        }
        if metadata.mode() & 0o077 != 0 {
            let restricted = std::fs::Permissions::from_mode(0o700);
            if let Err(error) = std::fs::set_permissions(root, restricted) {
                log::warn!("cannot restrict browser preview root: {error:#}");
                let _ = std::fs::remove_dir(&directory);
                return None;
            }
        }
    }
    prune_stale_previews(root, &directory);
    Some(directory)
}

/// Drops previews older than [`STALE_PREVIEW_AGE`]. Only called on a root
/// verified as ours, so everything inside was written by us.
fn prune_stale_previews(root: &std::path::Path, keep: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > STALE_PREVIEW_AGE);
        if stale && metadata.is_dir() && entry.path() != keep {
            if let Err(error) = std::fs::remove_dir_all(entry.path()) {
                log::debug!("pruning browser preview: {error:#}");
            }
        }
    }
}

fn create_private_dir(path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .with_context(|| format!("creating {}", path.display()))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path).with_context(|| format!("creating {}", path.display()))
    }
}

fn random_suffix() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // Still unique per open: the directory is created with `create_dir`,
        // which refuses an existing name.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        return format!("{}-{nanos:x}", std::process::id());
    }
    hex::encode(bytes)
}

/// Builds a safe, self-contained document from the original body. CSP
/// neutralizes scripts, forms, and embedded content, so the remote-images
/// setting remains honored in the browser.
fn browser_document(message: &Message, allow_remote: bool, action: BrowserAction) -> String {
    let body = match message.format {
        BodyFormat::Markdown => message.raw_body.clone().unwrap_or_else(|| {
            let parser =
                pulldown_cmark::Parser::new_ext(&message.body, pulldown_cmark::Options::all());
            let mut html = String::new();
            pulldown_cmark::html::push_html(&mut html, parser);
            html
        }),
        BodyFormat::Text => format!(
            "<pre style=\"white-space:pre-wrap;font-family:sans-serif\">{}</pre>",
            util::escape_html_text(&message.body)
        ),
    };
    let body = super::super::blitz_body::repair_outlook_html(&body);
    let body = inline_cid_images(body, &message.inline_images);
    let body_prefix = match action {
        BrowserAction::Preview => String::new(),
        BrowserAction::Print => print_header(message),
    };
    inject_browser_metadata(
        &body,
        &message.header.subject,
        allow_remote,
        action,
        &body_prefix,
    )
}

fn inline_cid_images(mut html: String, images: &[InlineImage]) -> String {
    use base64::Engine as _;

    let mut encoded = std::collections::HashMap::new();
    for image in images {
        if image.cid.is_empty() {
            continue;
        }
        let mime = if image.mime.contains('/')
            && image.mime.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '/' | '+' | '-' | '.')
            }) {
            image.mime.as_str()
        } else {
            "application/octet-stream"
        };
        encoded.insert(
            image.cid.to_ascii_lowercase(),
            format!(
                "data:{mime};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&image.bytes)
            ),
        );
    }
    let reference = regex::Regex::new(r#"(?i)(?:cid:|bytes://cid-)([^"'\s>)]+)"#)
        .expect("valid inline image regex");
    html = reference
        .replace_all(&html, |captures: &regex::Captures<'_>| {
            encoded
                .get(&captures[1].to_ascii_lowercase())
                .cloned()
                .unwrap_or_else(|| captures[0].to_string())
        })
        .into_owned();
    html
}

const PRINT_SCRIPT: &str =
    r#"window.addEventListener("load",()=>setTimeout(()=>window.print(),0));"#;

/// The print header (subject, sender, date, recipients) placed by
/// [`inject_browser_metadata`] as the first child of the body.
fn print_header(message: &Message) -> String {
    let mut rows = vec![
        print_header_row(
            tr!("compose-from-label").to_string(),
            [&message.header.from],
        ),
        print_header_row(
            tr!("viewer-print-date").to_string(),
            [util::full_date(&message.header.received)],
        ),
    ];
    if !message.to.is_empty() {
        rows.push(print_header_row(
            tr!("compose-to-label").to_string(),
            &message.to,
        ));
    }
    if !message.cc.is_empty() {
        rows.push(print_header_row(
            tr!("compose-cc-label").to_string(),
            &message.cc,
        ));
    }
    if !message.bcc.is_empty() {
        rows.push(print_header_row(
            tr!("compose-bcc").to_string(),
            &message.bcc,
        ));
    }

    let subject = if message.header.subject.is_empty() {
        tr!("no-subject").to_string()
    } else {
        message.header.subject.clone()
    };
    format!(
        "<header class=\"aviary-print-header\"><h1>{}</h1><dl>{}</dl></header>",
        util::escape_html_text(&subject),
        rows.concat()
    )
}

fn print_header_row<I, S>(label: String, values: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let values = values
        .into_iter()
        .map(|value| util::escape_html_text(value.as_ref()))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "<div><dt>{}</dt><dd>{values}</dd></div>",
        util::escape_html_text(&label)
    )
}

/// Rebuilds the mail as a document whose `<head>` is Aviary's.
///
/// The body is **parsed** (html5ever, through `scraper`) and re-serialized
/// rather than patched as text: inserting the CSP after the first `<head`
/// found in the raw source let `<!--<head>-->` or `<p title="<head>">` bury
/// it in a comment or an attribute, leaving the file free to run scripts and
/// load remote content. Here the CSP `<meta>` is the head's first child after
/// the charset, before any byte of the mail, and the mail's own head content
/// (its styles) follows it. Active content is also removed outright
/// ([`MailSerializer`]) — the CSP stays the primary defence, this is depth.
fn inject_browser_metadata(
    html: &str,
    subject: &str,
    allow_remote: bool,
    action: BrowserAction,
    body_prefix: &str,
) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;

    let remote_sources = if allow_remote { " http: https:" } else { "" };
    let (script_source, print_assets) = match action {
        BrowserAction::Preview => ("'none'".to_string(), String::new()),
        BrowserAction::Print => {
            let digest = sha2::Sha256::digest(PRINT_SCRIPT.as_bytes());
            let source = format!(
                "'sha256-{}'",
                base64::engine::general_purpose::STANDARD.encode(digest)
            );
            let assets = format!(
                concat!(
                    "<style>",
                    "@page{{margin:16mm}}",
                    "html{{color:#111;background:#fff}}",
                    "body{{margin:0;font-family:system-ui,-apple-system,sans-serif}}",
                    ".aviary-print-header{{margin:0 0 24px;padding:0 0 16px;",
                    "border-bottom:1px solid #bbb}}",
                    ".aviary-print-header h1{{margin:0 0 14px;font-size:22px;",
                    "line-height:1.25;overflow-wrap:anywhere}}",
                    ".aviary-print-header dl{{display:grid;gap:5px;margin:0;",
                    "font-size:12px;line-height:1.4}}",
                    ".aviary-print-header dl div{{display:grid;",
                    "grid-template-columns:90px minmax(0,1fr);gap:10px}}",
                    ".aviary-print-header dt{{font-weight:600}}",
                    ".aviary-print-header dd{{margin:0;overflow-wrap:anywhere}}",
                    "@media print{{body{{print-color-adjust:exact;",
                    "-webkit-print-color-adjust:exact}}}}",
                    "</style><script>{}</script>"
                ),
                PRINT_SCRIPT
            );
            (source, assets)
        }
    };
    let metadata = format!(
        concat!(
            "<meta charset=\"utf-8\">",
            "<meta http-equiv=\"Content-Security-Policy\" content=\"",
            "default-src 'none'; script-src {2}; object-src 'none'; frame-src 'none'; ",
            "form-action 'none'; base-uri 'none'; img-src data:{0}; ",
            "style-src 'unsafe-inline'{0}; font-src data:{0}; media-src data:{0}\">",
            "<title>{1}</title>{3}"
        ),
        remote_sources,
        util::escape_html_text(subject),
        script_source,
        print_assets,
    );

    let document = scraper::Html::parse_document(html);
    let serializer = MailSerializer { allow_remote };
    let mut root_attributes = String::new();
    let mut head = String::new();
    let mut body_attributes = String::new();
    let mut body = String::new();
    let html_element = document.root_element();
    // Only the language and direction of the root survive; anything else on
    // `<html>` (a `manifest`, handlers) has no business in a preview.
    for (name, value) in &html_element.value().attrs {
        if name.prefix.is_none() && matches!(&*name.local, "lang" | "dir") {
            push_attribute(&mut root_attributes, &name.local, value);
        }
    }
    for section in html_element
        .children()
        .filter_map(scraper::ElementRef::wrap)
    {
        match section.value().name() {
            "head" => serializer.children(section, &mut head),
            "body" => {
                serializer.attributes(section.value(), &mut body_attributes);
                serializer.children(section, &mut body);
            }
            // `frameset` and anything else: frames are refused anyway.
            _ => {}
        }
    }

    format!(
        "<!doctype html><html{root_attributes}><head>{metadata}{head}</head>\
         <body{body_attributes}>{body_prefix}{body}</body></html>"
    )
}

/// Elements dropped with their whole subtree: scripts, frames and plugins,
/// `base` (would re-target every relative URL), `template`/`noscript`
/// (parsed differently with scripting on, as the browser will), and SVG
/// animations (able to rewrite an `href` after the fact).
const DROPPED_ELEMENTS: &[&str] = &[
    "script",
    "base",
    "iframe",
    "frame",
    "frameset",
    "object",
    "embed",
    "applet",
    "portal",
    "noscript",
    "noembed",
    "noframes",
    "template",
    "title",
    "animate",
    "set",
    "animatemotion",
    "animatetransform",
];

const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "keygen", "link", "meta", "param",
    "source", "track", "wbr",
];

const HTML_NAMESPACE: &str = "http://www.w3.org/1999/xhtml";

/// Serializes a parsed mail, keeping only passive content. Comments,
/// doctypes and processing instructions are dropped; text is re-escaped,
/// except inside an HTML `<style>` (raw text, which the parser guarantees
/// cannot contain its own end tag).
struct MailSerializer {
    allow_remote: bool,
}

impl MailSerializer {
    fn children(&self, parent: scraper::ElementRef<'_>, out: &mut String) {
        let raw_text = is_html_style(parent.value());
        for child in parent.children() {
            match child.value() {
                scraper::Node::Text(text) if raw_text => out.push_str(text),
                scraper::Node::Text(text) => push_escaped(out, text, false),
                scraper::Node::Element(_) => {
                    if let Some(element) = scraper::ElementRef::wrap(child) {
                        self.element(element, out);
                    }
                }
                _ => {}
            }
        }
    }

    fn element(&self, node: scraper::ElementRef<'_>, out: &mut String) {
        let element = node.value();
        let name = element.name();
        let lower = name.to_ascii_lowercase();
        if DROPPED_ELEMENTS.contains(&lower.as_str()) || !self.keeps(element, &lower) {
            return;
        }
        let html = &*element.name.ns == HTML_NAMESPACE;
        // Legacy raw-text containers cannot be re-serialized faithfully
        // (their text would come back escaped): shown as preformatted text.
        let name = match lower.as_str() {
            "xmp" | "plaintext" | "listing" if html => "pre",
            _ => name,
        };
        if !is_safe_name(name) {
            // An exotic tag name is not worth reasoning about: its content
            // survives, the tag itself does not.
            self.children(node, out);
            return;
        }
        out.push('<');
        out.push_str(name);
        self.attributes(element, out);
        out.push('>');
        if html && VOID_ELEMENTS.contains(&name) {
            return;
        }
        self.children(node, out);
        out.push_str("</");
        out.push_str(name);
        out.push('>');
    }

    /// Element-specific rules on top of [`DROPPED_ELEMENTS`].
    fn keeps(&self, element: &scraper::node::Element, lower: &str) -> bool {
        match lower {
            // `http-equiv` covers refresh, a competing CSP, set-cookie…; the
            // charset is Aviary's. Only descriptive `name=` metas survive.
            "meta" => element.attr("http-equiv").is_none() && element.attr("charset").is_none(),
            // A remote stylesheet is a remote load: kept only when remote
            // content is allowed (the CSP enforces the same), and never any
            // other `rel` (preload, prefetch, icon, import…).
            "link" => {
                self.allow_remote
                    && element.attr("rel").is_some_and(|rel| {
                        rel.split_ascii_whitespace()
                            .all(|token| token.eq_ignore_ascii_case("stylesheet"))
                    })
                    && element.attr("href").is_some_and(|href| {
                        reqwest::Url::parse(href.trim())
                            .is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
                    })
            }
            _ => true,
        }
    }

    fn attributes(&self, element: &scraper::node::Element, out: &mut String) {
        for (name, value) in &element.attrs {
            let local = &*name.local;
            if local.len() > 2 && local[..2].eq_ignore_ascii_case("on") {
                continue;
            }
            if local.eq_ignore_ascii_case("srcdoc") || is_script_url(value) {
                continue;
            }
            let qualified = match &name.prefix {
                Some(prefix) => format!("{}:{local}", &**prefix),
                None => local.to_string(),
            };
            if !is_safe_name(&qualified) {
                continue;
            }
            push_attribute(out, &qualified, value);
        }
    }
}

fn is_html_style(element: &scraper::node::Element) -> bool {
    &*element.name.ns == HTML_NAMESPACE && element.name() == "style"
}

/// Tag and attribute names serialized as-is: anything else (quotes, `<`,
/// `=`… which the tokenizer tolerates in names) is dropped.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
}

/// `javascript:` / `vbscript:` in any attribute, as browsers read it:
/// leading whitespace and embedded tabs/newlines/control characters ignored,
/// case-insensitive.
fn is_script_url(value: &str) -> bool {
    let normalized = value
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && !c.is_control())
        .take(11)
        .collect::<String>()
        .to_ascii_lowercase();
    normalized.starts_with("javascript:") || normalized.starts_with("vbscript:")
}

fn push_attribute(out: &mut String, name: &str, value: &str) {
    out.push(' ');
    out.push_str(name);
    out.push_str("=\"");
    push_escaped(out, value, true);
    out.push('"');
}

fn push_escaped(out: &mut String, text: &str, attribute: bool) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            '"' if attribute => out.push_str("&quot;"),
            '<' if !attribute => out.push_str("&lt;"),
            '>' if !attribute => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        inject_browser_metadata, inline_cid_images, write_private_document, BrowserAction,
        PRINT_SCRIPT,
    };
    use crate::model::InlineImage;

    #[test]
    fn embeds_cid_images_as_data_uris() {
        let html = r#"<img src="CID:logo@example"><img src="bytes://cid-logo@example">"#;
        let images = [InlineImage {
            cid: "logo@example".into(),
            mime: "image/png".into(),
            bytes: vec![0, 1, 2],
        }];

        let output = inline_cid_images(html.into(), &images);

        assert_eq!(output.matches("data:image/png;base64,AAEC").count(), 2);
        assert!(!output.to_ascii_lowercase().contains("cid:logo@example"));
    }

    fn preview(html: &str, allow_remote: bool) -> String {
        inject_browser_metadata(html, "Subject", allow_remote, BrowserAction::Preview, "")
    }

    /// The CSP must be the first thing the browser reads in the head, before
    /// any byte coming from the mail.
    fn assert_csp_leads_the_head(output: &str) {
        let head = output.find("<head>").expect("head") + "<head>".len();
        let rest = &output[head..];
        assert!(
            rest.starts_with(
                "<meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\""
            ),
            "{output}"
        );
        let before_body = &output[..output.find("<body").expect("body")];
        assert_eq!(before_body.matches("<head").count(), 1, "{output}");
    }

    #[test]
    fn browser_metadata_blocks_active_and_remote_content_by_default() {
        let output = inject_browser_metadata(
            "<html><head></head><body>hello</body></html>",
            "A < B & C",
            false,
            BrowserAction::Preview,
            "",
        );

        assert!(output.contains("Content-Security-Policy"));
        assert!(output.contains("script-src 'none'"));
        assert!(output.contains("img-src data:;"));
        assert!(!output.contains("img-src data: http: https:"));
        assert!(output.contains("<title>A &lt; B &amp; C</title>"));
        assert!(output.contains("<body>hello</body>"));
        assert_csp_leads_the_head(&output);
    }

    #[test]
    fn browser_metadata_allows_remote_content_when_enabled() {
        let output = preview("<p>hello</p>", true);

        assert!(output.contains("img-src data: http: https:"));
        assert!(output.starts_with("<!doctype html>"));
        assert!(output.contains("<p>hello</p>"));
    }

    #[test]
    fn print_metadata_only_allows_the_generated_print_script() {
        let output = inject_browser_metadata(
            "<p>hello</p>",
            "Subject",
            false,
            BrowserAction::Print,
            "<header class=\"aviary-print-header\">H</header>",
        );

        assert!(output.contains("script-src 'sha256-"));
        assert!(output.contains(PRINT_SCRIPT));
        assert!(output.contains("@page{margin:16mm}"));
        assert!(!output.contains("script-src 'unsafe-inline'"));
        assert!(
            output.contains("<body><header class=\"aviary-print-header\">H</header><p>hello</p>")
        );
        assert_eq!(output.matches("<script").count(), 1);
    }

    #[test]
    fn a_head_inside_a_comment_cannot_displace_the_csp() {
        let output = preview(
            "<!--<head>--><html><head><style>p{color:red}</style></head>\
             <body><script>alert(1)</script><p>hi</p></body></html>",
            false,
        );

        assert_csp_leads_the_head(&output);
        assert!(!output.contains("<script"), "{output}");
        assert!(!output.contains("alert(1)"), "{output}");
        assert!(!output.contains("<!--"), "{output}");
        // The mail's own styles follow Aviary's metadata inside the head.
        assert!(
            output.contains("</title><style>p{color:red}</style></head>"),
            "{output}"
        );
    }

    #[test]
    fn a_head_inside_an_attribute_cannot_displace_the_csp() {
        let output = preview(r#"<p title="<head>">x</p><script>alert(1)</script>"#, false);

        assert_csp_leads_the_head(&output);
        assert!(output.contains(r#"<p title="<head>">x</p>"#), "{output}");
        assert!(!output.contains("<script"), "{output}");
    }

    #[test]
    fn http_equiv_metas_and_base_are_removed() {
        let output = preview(
            "<html><head><meta http-equiv=\"refresh\" content=\"0;url=https://example.invalid\">\
             <meta http-equiv=\"Content-Security-Policy\" content=\"script-src *\">\
             <meta charset=\"iso-8859-1\"><base href=\"https://example.invalid/\">\
             <meta name=\"viewport\" content=\"width=device-width\"></head><body>x</body></html>",
            true,
        );

        assert!(!output.to_ascii_lowercase().contains("refresh"), "{output}");
        assert!(!output.contains("example.invalid"), "{output}");
        assert!(!output.contains("script-src *"), "{output}");
        assert!(!output.contains("iso-8859-1"), "{output}");
        assert_eq!(output.matches("http-equiv").count(), 1, "{output}");
        assert!(output.contains("name=\"viewport\""), "{output}");
    }

    #[test]
    fn event_handlers_and_script_urls_are_removed() {
        let output = preview(
            "<img src=\"x\" onerror=\"alert(1)\" ONLOAD=\"alert(2)\">\
             <a href=\" java\tscript:alert(3)\">a</a><a href=\"https://example.com/\">b</a>\
             <iframe src=\"https://example.invalid\"></iframe><object data=\"x\"></object>\
             <svg><a href=\"javascript:alert(4)\"><text>t</text></a><animate attributeName=\"href\"/></svg>",
            false,
        );

        assert!(!output.contains("alert"), "{output}");
        assert!(!output.contains("<iframe"), "{output}");
        assert!(!output.contains("<object"), "{output}");
        assert!(!output.contains("<animate"), "{output}");
        assert!(output.contains("<img src=\"x\">"), "{output}");
        assert!(
            output.contains("<a href=\"https://example.com/\">b</a>"),
            "{output}"
        );
    }

    #[test]
    fn stylesheets_are_kept_only_when_remote_content_is_allowed() {
        let html = "<html><head><link rel=\"stylesheet\" href=\"https://example.com/a.css\">\
                    <link rel=\"preload\" href=\"https://example.com/b.js\"></head><body></body></html>";

        let blocked = preview(html, false);
        assert!(!blocked.contains("<link"), "{blocked}");

        let allowed = preview(html, true);
        assert!(allowed.contains("a.css"), "{allowed}");
        assert!(!allowed.contains("b.js"), "{allowed}");
    }

    #[test]
    fn text_and_styles_survive_reserialization() {
        let output = preview(
            "<style>a > b { content: \"&\" }</style><p>1 &lt; 2 &amp; 3</p>",
            false,
        );

        assert!(
            output.contains("<style>a > b { content: \"&\" }</style>"),
            "{output}"
        );
        assert!(output.contains("<p>1 &lt; 2 &amp; 3</p>"), "{output}");
    }

    #[test]
    fn previews_are_written_to_a_fresh_private_file() {
        let first = write_private_document("message.html", b"one").expect("first");
        let second = write_private_document("message.html", b"two").expect("second");

        assert_ne!(first, second);
        assert_eq!(std::fs::read(&first).expect("read"), b"one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let file = std::fs::metadata(&first)
                .expect("file")
                .permissions()
                .mode();
            let directory = std::fs::metadata(first.parent().expect("parent"))
                .expect("directory")
                .permissions()
                .mode();
            assert_eq!(file & 0o777, 0o600);
            assert_eq!(directory & 0o777, 0o700);
        }
        for path in [first, second] {
            let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
        }
    }
}
