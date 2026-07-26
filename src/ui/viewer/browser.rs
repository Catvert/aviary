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
        use std::hash::{Hash, Hasher};
        use std::io::Write as _;

        let html = browser_document(&message, allow_remote, action);
        let directory = std::env::temp_dir().join("aviary-browser");
        if let Err(error) = std::fs::create_dir_all(&directory) {
            log::warn!("failed to create browser preview directory: {error:#}");
            return;
        }

        // The name reveals neither the subject nor sender in the
        // temporary directory and remains stable when the user reopens the message.
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        message.header.account_id.hash(&mut hash);
        message.header.id.hash(&mut hash);
        let suffix = match action {
            BrowserAction::Preview => "",
            BrowserAction::Print => "-print",
        };
        let path = directory.join(format!("{:016x}{suffix}.html", hash.finish()));

        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) => {
                log::warn!("failed to create browser preview: {error:#}");
                return;
            }
        };
        if let Err(error) = file.write_all(html.as_bytes()) {
            log::warn!("failed to write browser preview: {error:#}");
            return;
        }
        if let Err(error) = open::that_detached(&path) {
            log::warn!("failed to open message in browser: {error:#}");
        }
    });
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
    let body = match action {
        BrowserAction::Preview => body,
        BrowserAction::Print => inject_print_header(body, message),
    };
    inject_browser_metadata(body, &message.header.subject, allow_remote, action)
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

fn inject_print_header(mut html: String, message: &Message) -> String {
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
    let header = format!(
        "<header class=\"aviary-print-header\"><h1>{}</h1><dl>{}</dl></header>",
        util::escape_html_text(&subject),
        rows.concat()
    );

    let body = regex::Regex::new(r"(?is)<body(?:\s[^>]*)?>").expect("valid body regex");
    if let Some(found) = body.find(&html) {
        html.insert_str(found.end(), &header);
        html
    } else {
        format!("{header}{html}")
    }
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

fn inject_browser_metadata(
    mut html: String,
    subject: &str,
    allow_remote: bool,
    action: BrowserAction,
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

    let head = regex::Regex::new(r"(?is)<head(?:\s[^>]*)?>").expect("valid head regex");
    if let Some(found) = head.find(&html) {
        html.insert_str(found.end(), &metadata);
        return html;
    }
    let root = regex::Regex::new(r"(?is)<html(?:\s[^>]*)?>").expect("valid html regex");
    if let Some(found) = root.find(&html) {
        html.insert_str(found.end(), &format!("<head>{metadata}</head>"));
        return html;
    }
    format!("<!doctype html><html><head>{metadata}</head><body>{html}</body></html>")
}

#[cfg(test)]
mod tests {
    use super::{inject_browser_metadata, inline_cid_images, BrowserAction, PRINT_SCRIPT};
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

    #[test]
    fn browser_metadata_blocks_active_and_remote_content_by_default() {
        let output = inject_browser_metadata(
            "<html><head></head><body>hello</body></html>".into(),
            "A < B & C",
            false,
            BrowserAction::Preview,
        );

        assert!(output.contains("Content-Security-Policy"));
        assert!(output.contains("script-src 'none'"));
        assert!(output.contains("img-src data:;"));
        assert!(!output.contains("img-src data: http: https:"));
        assert!(output.contains("<title>A &lt; B &amp; C</title>"));
    }

    #[test]
    fn browser_metadata_allows_remote_content_when_enabled() {
        let output = inject_browser_metadata(
            "<p>hello</p>".into(),
            "Subject",
            true,
            BrowserAction::Preview,
        );

        assert!(output.contains("img-src data: http: https:"));
        assert!(output.starts_with("<!doctype html>"));
    }

    #[test]
    fn print_metadata_only_allows_the_generated_print_script() {
        let output = inject_browser_metadata(
            "<p>hello</p>".into(),
            "Subject",
            false,
            BrowserAction::Print,
        );

        assert!(output.contains("script-src 'sha256-"));
        assert!(output.contains(PRINT_SCRIPT));
        assert!(output.contains("@page{margin:16mm}"));
        assert!(!output.contains("script-src 'unsafe-inline'"));
    }
}
