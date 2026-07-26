//! Structured mail search query.
//!
//! Pure data: the search field is parsed once, here, and every consumer
//! renders the result into its own dialect — the local FTS5 index
//! (`runtime::mail_cache`), Gmail's `q`, Graph's KQL `$search`, IMAP's
//! `SEARCH`. Keeping the grammar in one place is what makes a query behave the
//! same whether it is answered from the cache or from a provider.
//!
//! Filters that no backend can be trusted to apply identically (read state,
//! attachments, dates) are also enforced locally by [`SearchQuery::matches`],
//! so a provider that ignores or loosens one cannot leak non-matching messages
//! into the results.

use crate::model::MessageHeader;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};

/// A parsed search field.
///
/// Every list is an AND: `de:alice de:bob` means both, matching how the
/// provider dialects read repeated operators.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    /// Free text, searched across subject, sender and body.
    pub terms: Vec<String>,
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub subject: Vec<String>,
    /// Strictly before, at day granularity (UTC).
    pub before: Option<NaiveDate>,
    /// On or after, at day granularity (UTC).
    pub after: Option<NaiveDate>,
    pub has_attachment: Option<bool>,
    pub is_read: Option<bool>,
    pub is_flagged: Option<bool>,
}

/// Operator names, French and English. Both spellings are accepted whatever
/// the UI language: users paste queries and carry habits across clients.
const FROM_KEYS: &[&str] = &["de", "from", "exp", "expediteur", "expéditeur"];
const TO_KEYS: &[&str] = &["a", "à", "to", "dest", "destinataire", "pour"];
const SUBJECT_KEYS: &[&str] = &["objet", "sujet", "subject"];
const BEFORE_KEYS: &[&str] = &["avant", "before"];
const AFTER_KEYS: &[&str] = &["apres", "après", "after", "depuis", "since"];
const HAS_KEYS: &[&str] = &["avec", "has", "contient"];
const IS_KEYS: &[&str] = &["est", "is"];

const ATTACHMENT_VALUES: &[&str] = &[
    "pj",
    "piece-jointe",
    "pièce-jointe",
    "pieces-jointes",
    "pièces-jointes",
    "attachment",
    "attachments",
    "fichier",
    "file",
];
const UNREAD_VALUES: &[&str] = &["non-lu", "nonlu", "unread", "new", "nouveau"];
const READ_VALUES: &[&str] = &["lu", "read"];
const FLAGGED_VALUES: &[&str] = &["suivi", "marque", "marqué", "flagged", "starred", "star"];
const UNFLAGGED_VALUES: &[&str] = &["non-suivi", "nonsuivi", "unflagged", "unstarred"];

fn matches_key(key: &str, keys: &[&str]) -> bool {
    keys.contains(&key)
}

fn matches_value(value: &str, values: &[&str]) -> bool {
    values.contains(&value)
}

impl SearchQuery {
    /// Parses the raw search field.
    ///
    /// Unknown operators are deliberately *not* errors: `truc:machin` stays a
    /// free-text term. A search box that rejects input is worse than one that
    /// searches for what was typed, and it keeps colons inside subjects
    /// (`Re: contrat`) working.
    pub fn parse(input: &str) -> Self {
        let mut query = Self::default();
        for token in tokenize(input) {
            let Some((key, value)) = split_operator(&token) else {
                push_term(&mut query.terms, token);
                continue;
            };
            let key = key.to_lowercase();
            let folded = fold_value(&value);
            if matches_key(&key, FROM_KEYS) {
                push_term(&mut query.from, value);
            } else if matches_key(&key, TO_KEYS) {
                push_term(&mut query.to, value);
            } else if matches_key(&key, SUBJECT_KEYS) {
                push_term(&mut query.subject, value);
            } else if matches_key(&key, BEFORE_KEYS) {
                match parse_date(&value) {
                    Some(date) => query.before = Some(date),
                    None => push_term(&mut query.terms, token),
                }
            } else if matches_key(&key, AFTER_KEYS) {
                match parse_date(&value) {
                    Some(date) => query.after = Some(date),
                    None => push_term(&mut query.terms, token),
                }
            } else if matches_key(&key, HAS_KEYS) && matches_value(&folded, ATTACHMENT_VALUES) {
                query.has_attachment = Some(true);
            } else if matches_key(&key, IS_KEYS) {
                if matches_value(&folded, UNREAD_VALUES) {
                    query.is_read = Some(false);
                } else if matches_value(&folded, READ_VALUES) {
                    query.is_read = Some(true);
                } else if matches_value(&folded, FLAGGED_VALUES) {
                    query.is_flagged = Some(true);
                } else if matches_value(&folded, UNFLAGGED_VALUES) {
                    query.is_flagged = Some(false);
                } else if matches_value(&folded, ATTACHMENT_VALUES) {
                    query.has_attachment = Some(true);
                } else {
                    push_term(&mut query.terms, token);
                }
            } else {
                // Unrecognized operator: treat the whole token as text.
                push_term(&mut query.terms, token);
            }
        }
        query
    }

    /// True when nothing was typed that could select messages.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether any operator was used, as opposed to plain words. Callers use
    /// it to decide whether a backend needs the structured path at all.
    pub fn has_operators(&self) -> bool {
        !self.from.is_empty()
            || !self.to.is_empty()
            || !self.subject.is_empty()
            || self.before.is_some()
            || self.after.is_some()
            || self.has_attachment.is_some()
            || self.is_read.is_some()
            || self.is_flagged.is_some()
    }

    /// Locally verifiable part of the query, applied to results from any
    /// source.
    ///
    /// Backends disagree on the edge cases — Gmail's `has:attachment` counts
    /// inline images, IMAP `SEARCH` has no attachment predicate at all, Graph
    /// KQL ignores operators it fails to parse instead of erroring — so
    /// whatever came back is re-checked here against what was actually asked.
    /// Text and recipients are not re-checked: a header carries no body, and
    /// `to` is unknown for messages that were never opened.
    pub fn matches(&self, header: &MessageHeader) -> bool {
        if let Some(has_attachment) = self.has_attachment {
            if header.has_attachments != has_attachment {
                return false;
            }
        }
        if let Some(is_read) = self.is_read {
            if header.is_read != is_read {
                return false;
            }
        }
        if let Some(is_flagged) = self.is_flagged {
            if header.is_flagged != is_flagged {
                return false;
            }
        }
        if let Some(before) = self.before {
            if header.received >= start_of_day(before) {
                return false;
            }
        }
        if let Some(after) = self.after {
            if header.received < start_of_day(after) {
                return false;
            }
        }
        if !self.from.is_empty() {
            let sender = header.from.to_lowercase();
            if !self
                .from
                .iter()
                .all(|needle| sender.contains(&needle.to_lowercase()))
            {
                return false;
            }
        }
        true
    }
}

fn start_of_day(date: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight is always valid"))
}

fn push_term(target: &mut Vec<String>, value: String) {
    let value = value.trim().to_string();
    if !value.is_empty() {
        target.push(value);
    }
}

/// Splits `key:value`, honouring the quotes around a multi-word value.
/// Returns `None` for a bare word, or when either side is empty (`:x`, `x:`),
/// which keeps `Re:` in a pasted subject from being read as an operator.
fn split_operator(token: &str) -> Option<(String, String)> {
    let (key, value) = token.split_once(':')?;
    if key.is_empty() || value.is_empty() {
        return None;
    }
    if key.contains(char::is_whitespace) {
        return None;
    }
    Some((key.to_string(), unquote(value)))
}

fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(trimmed)
        .to_string()
}

/// Lowercases and strips the accents of an operator *value*, so `avec:pièce-jointe`
/// and `avec:piece-jointe` are the same switch.
fn fold_value(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'à' | 'â' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' => 'i',
            'ô' | 'ö' => 'o',
            'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            c => c,
        })
        .collect()
}

/// Splits on whitespace while keeping `"quoted runs"` together, including
/// after an operator (`objet:"bon de commande"`).
fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in input.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
        .into_iter()
        .map(|token| {
            // A token that is entirely quoted loses them here; an operator
            // value keeps them until `split_operator` unquotes it.
            if token.starts_with('"') && token.ends_with('"') && token.len() > 1 {
                unquote(&token)
            } else {
                token
            }
        })
        .filter(|token| !token.is_empty())
        .collect()
}

/// Accepts `YYYY-MM-DD`, `DD/MM/YYYY` and `YYYY/MM/DD`, plus the relative
/// shorthands a mail search actually gets typed with.
fn parse_date(value: &str) -> Option<NaiveDate> {
    let value = value.trim();
    let folded = fold_value(value);
    let today = Utc::now().date_naive();
    match folded.as_str() {
        "aujourdhui" | "aujourd'hui" | "today" => return Some(today),
        "hier" | "yesterday" => return today.pred_opt(),
        "demain" | "tomorrow" => return today.succ_opt(),
        _ => {}
    }
    // `7j` / `7d`: that many days back.
    if let Some(days) = folded
        .strip_suffix('j')
        .or_else(|| folded.strip_suffix('d'))
        .and_then(|digits| digits.parse::<i64>().ok())
    {
        return today.checked_sub_signed(chrono::Duration::days(days));
    }
    for format in ["%Y-%m-%d", "%d/%m/%Y", "%Y/%m/%d", "%d-%m-%Y", "%d.%m.%Y"] {
        if let Ok(date) = NaiveDate::parse_from_str(value, format) {
            return Some(date);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AccountId;

    fn header() -> MessageHeader {
        MessageHeader {
            id: "message-a".into(),
            account_id: AccountId("account-a".into()),
            subject: "Contrat de maintenance".into(),
            from: "Contact A <contact-a@example.test>".into(),
            received: Utc
                .with_ymd_and_hms(2026, 3, 15, 12, 0, 0)
                .single()
                .expect("fixed timestamp"),
            preview: String::new(),
            is_read: true,
            is_flagged: false,
            has_attachments: true,
            tags: Vec::new(),
            last_action: None,
            last_action_at: None,
            conversation_id: None,
            internet_message_id: None,
        }
    }

    #[test]
    fn plain_words_stay_free_text() {
        let query = SearchQuery::parse("contrat  maintenance ");
        assert_eq!(query.terms, vec!["contrat", "maintenance"]);
        assert!(!query.has_operators());
    }

    #[test]
    fn operators_are_recognized_in_both_languages() {
        let french = SearchQuery::parse("de:alice objet:contrat avec:pj est:non-lu");
        assert_eq!(french.from, vec!["alice"]);
        assert_eq!(french.subject, vec!["contrat"]);
        assert_eq!(french.has_attachment, Some(true));
        assert_eq!(french.is_read, Some(false));

        let english = SearchQuery::parse("from:alice subject:contrat has:attachment is:unread");
        assert_eq!(english.from, french.from);
        assert_eq!(english.subject, french.subject);
        assert_eq!(english.has_attachment, french.has_attachment);
        assert_eq!(english.is_read, french.is_read);
    }

    #[test]
    fn operator_values_may_be_quoted() {
        let query = SearchQuery::parse("objet:\"bon de commande\" de:alice");
        assert_eq!(query.subject, vec!["bon de commande"]);
        assert_eq!(query.from, vec!["alice"]);
        assert!(query.terms.is_empty());
    }

    #[test]
    fn quoted_free_text_survives_as_one_term() {
        let query = SearchQuery::parse("\"bon de commande\"");
        assert_eq!(query.terms, vec!["bon de commande"]);
    }

    /// A search box that rejects input is worse than one that searches for
    /// what was typed — and subjects are full of colons.
    #[test]
    fn unknown_operators_and_bare_colons_stay_text() {
        assert_eq!(SearchQuery::parse("truc:machin").terms, vec!["truc:machin"]);
        assert_eq!(
            SearchQuery::parse("Re: contrat").terms,
            vec!["Re:", "contrat"]
        );
        assert_eq!(SearchQuery::parse("de:").terms, vec!["de:"]);
        // A date operator that cannot be parsed must not silently vanish.
        let query = SearchQuery::parse("avant:jamais");
        assert!(query.before.is_none());
        assert_eq!(query.terms, vec!["avant:jamais"]);
    }

    #[test]
    fn accented_operator_values_fold() {
        assert_eq!(
            SearchQuery::parse("avec:pièce-jointe").has_attachment,
            Some(true)
        );
        assert_eq!(SearchQuery::parse("est:marqué").is_flagged, Some(true));
    }

    #[test]
    fn dates_accept_several_formats() {
        let iso = SearchQuery::parse("apres:2026-03-01");
        assert_eq!(iso.after, NaiveDate::from_ymd_opt(2026, 3, 1));
        let french = SearchQuery::parse("avant:15/03/2026");
        assert_eq!(french.before, NaiveDate::from_ymd_opt(2026, 3, 15));
        // Relative shorthands resolve against today.
        assert_eq!(
            SearchQuery::parse("depuis:aujourd'hui").after,
            Some(Utc::now().date_naive())
        );
        assert!(SearchQuery::parse("depuis:7j").after.is_some());
    }

    /// Backends disagree on these predicates, so results are re-checked
    /// locally whatever their source.
    #[test]
    fn local_filter_rejects_messages_the_backend_should_have_excluded() {
        let header = header();
        assert!(SearchQuery::parse("est:lu").matches(&header));
        assert!(!SearchQuery::parse("est:non-lu").matches(&header));
        assert!(SearchQuery::parse("avec:pj").matches(&header));
        assert!(!SearchQuery::parse("est:suivi").matches(&header));
        assert!(SearchQuery::parse("de:contact-a").matches(&header));
        assert!(!SearchQuery::parse("de:autre").matches(&header));
    }

    #[test]
    fn date_bounds_are_day_granular_and_half_open() {
        let header = header(); // 2026-03-15 12:00 UTC
        assert!(SearchQuery::parse("apres:2026-03-15").matches(&header));
        assert!(SearchQuery::parse("avant:2026-03-16").matches(&header));
        // Strictly before: its own day excludes it.
        assert!(!SearchQuery::parse("avant:2026-03-15").matches(&header));
        assert!(!SearchQuery::parse("apres:2026-03-16").matches(&header));
    }

    #[test]
    fn empty_input_produces_an_empty_query() {
        assert!(SearchQuery::parse("   ").is_empty());
        assert!(!SearchQuery::parse("contrat").is_empty());
    }
}
