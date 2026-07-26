//! `mailto:` URL parsing, per [RFC 6068].
//!
//! Two callers share this: a `mailto:` anchor clicked inside a message body
//! (`ui/blitz_body/element.rs`) and the URL the desktop hands us on the command
//! line when Aviary is the registered handler (`single_instance.rs`). Both
//! receive a string chosen by someone other than the user — a web page, an
//! email — which is what shapes the rules below.
//!
//! [RFC 6068]: https://www.rfc-editor.org/rfc/rfc6068

/// The parts of a `mailto:` URL Aviary is willing to act on.
///
/// Serializable because a second process hands one to the running instance over
/// a socket (see `single_instance`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MailtoRequest {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
}

/// Parses a `mailto:` URL, or returns `None` if it is not one.
///
/// Deliberately narrower than RFC 6068 in two places:
///
/// * **`attach` (and any other header) is ignored.** A `mailto:` can arrive
///   from a web page, and honouring an attachment header would let that page
///   name a path on this machine and have the user mail it out. It is a known
///   attack against mail clients, not a hypothetical one.
/// * **Control characters are stripped, CR and LF included.** Header fields
///   here become recipient and subject inputs, and a newline is the classic way
///   to inject a second header into whatever consumes them downstream.
///
/// `+` is left alone rather than decoded as a space: RFC 6068 percent-encodes,
/// it does not use form encoding, and `user+tag@example.com` addresses are far
/// too common to mangle.
pub fn parse(raw: &str) -> Option<MailtoRequest> {
    let raw = raw.trim();
    let rest = strip_scheme(raw)?;

    // Split the path (recipients) from the query, by hand rather than through a
    // URL crate: `mailto:` is opaque-path, and generic parsers disagree on
    // whether unencoded spaces or a bare `?` in a subject are fatal. Being
    // lenient here costs nothing — every field is sanitized below anyway.
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (rest, None),
    };

    let mut request = MailtoRequest {
        to: addresses(&decode(path)),
        ..Default::default()
    };

    for (key, value) in query.into_iter().flat_map(query_pairs) {
        let value = decode(&value);
        match key.to_ascii_lowercase().as_str() {
            // RFC 6068 allows `to` in the query too; it adds to the path's
            // recipients rather than replacing them.
            "to" => extend(&mut request.to, &addresses(&value)),
            "cc" => extend(&mut request.cc, &addresses(&value)),
            "bcc" => extend(&mut request.bcc, &addresses(&value)),
            "subject" => request.subject = single_line(&value),
            "body" => request.body = text(&value),
            _ => {}
        }
    }

    Some(request)
}

fn strip_scheme(raw: &str) -> Option<&str> {
    let (scheme, rest) = raw.split_once(':')?;
    scheme.eq_ignore_ascii_case("mailto").then_some(rest)
}

fn query_pairs(query: &str) -> impl Iterator<Item = (&str, String)> {
    query.split('&').filter_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        Some((key, value.to_string()))
    })
}

/// Percent-decoding that survives malformed input: an isolated `%` or a
/// non-hexadecimal escape is kept verbatim instead of failing the whole URL,
/// since a mangled subject is a better outcome than a composer that never
/// opens.
fn decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    // A percent-escape can encode any byte, so the result is not guaranteed to
    // be UTF-8; replacing the invalid sequences keeps a usable string.
    String::from_utf8_lossy(&out).into_owned()
}

/// Keeps only what looks like an address, comma-separated. Anything without an
/// `@`, or carrying a control character, is dropped rather than passed on to
/// the recipient field.
fn addresses(value: &str) -> String {
    crate::ui::util::parse_bare_addresses(value)
        .into_iter()
        .filter(|address| address.contains('@') && !address.chars().any(char::is_control))
        .collect::<Vec<_>>()
        .join(", ")
}

fn extend(field: &mut String, addition: &str) {
    if addition.is_empty() {
        return;
    }
    if field.is_empty() {
        field.push_str(addition);
    } else {
        field.push_str(", ");
        field.push_str(addition);
    }
}

/// A subject is one line: newlines are removed outright, and other control
/// characters with them.
fn single_line(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

/// A body may hold newlines and tabs, but nothing else that is a control
/// character. Dropping the lone `\r` is what turns a CRLF body into LF.
fn text(value: &str) -> String {
    value
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_address() {
        let request = parse("mailto:contact@example.com").expect("a mailto URL");
        assert_eq!(request.to, "contact@example.com");
        assert!(request.subject.is_empty());
    }

    #[test]
    fn rejects_other_schemes() {
        assert!(parse("https://example.com").is_none());
        assert!(parse("mailtoo:contact@example.com").is_none());
    }

    #[test]
    fn scheme_is_case_insensitive() {
        assert_eq!(
            parse("MailTo:contact@example.com")
                .expect("a mailto URL")
                .to,
            "contact@example.com"
        );
    }

    #[test]
    fn decodes_subject_and_body() {
        let request =
            parse("mailto:a@example.com?subject=Devis%20n%C2%B02&body=Bonjour%2C%0ARendez-vous%3F")
                .expect("a mailto URL");
        assert_eq!(request.subject, "Devis n°2");
        assert_eq!(request.body, "Bonjour,\nRendez-vous?");
    }

    #[test]
    fn collects_every_recipient_field() {
        let request = parse(
            "mailto:a@example.com,b@example.com?to=c@example.com&cc=d@example.com&bcc=e@example.com",
        )
        .expect("a mailto URL");
        assert_eq!(request.to, "a@example.com, b@example.com, c@example.com");
        assert_eq!(request.cc, "d@example.com");
        assert_eq!(request.bcc, "e@example.com");
    }

    /// A page that can attach a local file to a message the user is about to
    /// send has exfiltrated it; the header is ignored on purpose.
    #[test]
    fn ignores_attach_and_unknown_headers() {
        let request = parse("mailto:a@example.com?attach=/etc/passwd&in-reply-to=%3Cx%40y%3E")
            .expect("a mailto URL");
        assert_eq!(request.to, "a@example.com");
        assert!(request.body.is_empty());
        assert!(request.subject.is_empty());
    }

    /// Header injection: the newline must not survive into the subject.
    #[test]
    fn strips_control_characters_from_the_subject() {
        let request = parse("mailto:a@example.com?subject=Hello%0D%0ABcc:%20victim@example.com")
            .expect("a mailto URL");
        assert_eq!(request.subject, "HelloBcc: victim@example.com");
        assert!(!request.subject.contains('\n'));
        assert!(request.bcc.is_empty());
    }

    #[test]
    fn keeps_plus_addressing_intact() {
        let request = parse("mailto:user+invoices@example.com").expect("a mailto URL");
        assert_eq!(request.to, "user+invoices@example.com");
    }

    #[test]
    fn drops_entries_that_are_not_addresses() {
        let request = parse("mailto:not-an-address,real@example.com").expect("a mailto URL");
        assert_eq!(request.to, "real@example.com");
    }

    /// `mailto:` on its own is still a mail link: it opens an empty composer
    /// rather than being handed back to the desktop.
    #[test]
    fn empty_url_parses_to_an_empty_request() {
        assert_eq!(parse("mailto:"), Some(MailtoRequest::default()));
    }

    #[test]
    fn malformed_percent_escapes_do_not_fail_the_parse() {
        let request =
            parse("mailto:a@example.com?subject=100%%20sure&body=%ZZ").expect("a mailto URL");
        assert_eq!(request.subject, "100% sure");
        assert_eq!(request.body, "%ZZ");
    }

    #[test]
    fn a_question_mark_inside_the_subject_survives() {
        let request = parse("mailto:a@example.com?subject=Ready?").expect("a mailto URL");
        assert_eq!(request.subject, "Ready?");
    }
}
