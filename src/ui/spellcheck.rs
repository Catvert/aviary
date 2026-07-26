//! Offline bilingual spelling engine used by mail body inputs.
//!
//! The Hunspell dictionaries are parsed once, on GPUI's background executor.
//! Checks accept a word found in either French or English, which keeps mixed
//! business emails useful without forcing a language switch per paragraph.
//! Suggestions are generated lazily on right click and cached.

use regex::Regex;
use spellbook::Dictionary;
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::PathBuf,
    sync::{Mutex, OnceLock, RwLock},
};

const SUGGESTION_LIMIT: usize = 5;
const SUGGESTION_CACHE_CAP: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Issue {
    pub range: Range<usize>,
    pub word: String,
}

struct Engine {
    fr: &'static Dictionary,
    en: &'static Dictionary,
    personal: RwLock<HashSet<String>>,
    ignored: RwLock<HashSet<String>>,
    suggestions: Mutex<HashMap<String, Vec<String>>>,
}

static ENGINE: OnceLock<Option<Engine>> = OnceLock::new();
static WORD_RE: OnceLock<Regex> = OnceLock::new();
static PERSONAL_SAVE_LOCK: Mutex<()> = Mutex::new(());

fn word_re() -> &'static Regex {
    WORD_RE.get_or_init(|| {
        Regex::new(r"(?u)\p{L}[\p{L}\p{M}'’\-]*\p{L}|\p{L}").expect("valid spelling tokenizer")
    })
}

fn personal_dictionary_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("be", "acetics", "aviary")
        .map(|dirs| dirs.config_dir().join("personal_dictionary.txt"))
}

fn normalize(word: &str) -> String {
    word.trim_matches(['\'', '’', '-']).to_lowercase()
}

fn load_personal_dictionary() -> HashSet<String> {
    let Some(path) = personal_dictionary_path() else {
        return HashSet::new();
    };
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .map(normalize)
                .filter(|word| !word.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn engine() -> Option<&'static Engine> {
    ENGINE
        .get_or_init(|| {
            let fr = crate::dictionaries::french()?;
            let en = crate::dictionaries::english()?;
            Some(Engine {
                fr,
                en,
                personal: RwLock::new(load_personal_dictionary()),
                ignored: RwLock::new(HashSet::new()),
                suggestions: Mutex::new(HashMap::new()),
            })
        })
        .as_ref()
}

/// Starts dictionary parsing away from the UI thread.
pub(crate) fn warm_up() {
    std::thread::Builder::new()
        .name("aviary-spellcheck-init".into())
        .spawn(|| {
            let _ = engine();
        })
        .ok();
}

fn is_session_word(engine: &Engine, word: &str) -> bool {
    let normalized = normalize(word);
    normalized.is_empty()
        || engine
            .personal
            .read()
            .is_ok_and(|words| words.contains(&normalized))
        || engine
            .ignored
            .read()
            .is_ok_and(|words| words.contains(&normalized))
}

fn should_skip_token(text: &str, range: &Range<usize>, word: &str) -> bool {
    if word.chars().count() <= 1
        || (word.chars().all(|character| character.is_uppercase()) && word.chars().count() <= 6)
    {
        return true;
    }

    // Ignore URL/email/inline-image chunks and Markdown link destinations.
    let chunk_start = text[..range.start]
        .rfind(char::is_whitespace)
        .map_or(0, |index| index + 1);
    let chunk_end = text[range.end..]
        .find(char::is_whitespace)
        .map_or(text.len(), |index| range.end + index);
    let chunk = &text[chunk_start..chunk_end];
    chunk.contains("://")
        || chunk.contains('@')
        || chunk.starts_with("www.")
        || chunk.starts_with("cid:")
        || (chunk.starts_with("](") && chunk.ends_with(')'))
}

fn is_correct(engine: &Engine, word: &str) -> bool {
    is_session_word(engine, word) || engine.fr.check(word) || engine.en.check(word)
}

/// Whether Aviary's dictionaries (including personal/session words) accept a
/// word. LanguageTool spelling matches use this to preserve the personal
/// dictionary as the final authority.
pub(crate) fn word_is_accepted(word: &str) -> bool {
    engine().is_none_or(|engine| is_correct(engine, word))
}

pub(crate) fn check_text(text: &str) -> Vec<Issue> {
    let Some(engine) = engine() else {
        return Vec::new();
    };
    word_re()
        .find_iter(text)
        .filter_map(|found| {
            let range = found.range();
            let word = found.as_str();
            (!should_skip_token(text, &range, word) && !is_correct(engine, word)).then(|| Issue {
                range,
                word: word.to_string(),
            })
        })
        .collect()
}

/// Returns a misspelling only when the click is within the offending word.
pub(crate) fn issue_at(text: &str, offset: usize) -> Option<Issue> {
    let engine = engine()?;
    word_re().find_iter(text).find_map(|found| {
        let range = found.range();
        let contains = range.contains(&offset) || (offset == range.end && range.start < range.end);
        let word = found.as_str();
        (contains && !should_skip_token(text, &range, word) && !is_correct(engine, word)).then(
            || Issue {
                range,
                word: word.to_string(),
            },
        )
    })
}

fn levenshtein(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (row, left_char) in left.chars().enumerate() {
        current[0] = row + 1;
        for (column, right_char) in right.iter().enumerate() {
            current[column + 1] = (previous[column + 1] + 1)
                .min(current[column] + 1)
                .min(previous[column] + usize::from(left_char != *right_char));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

fn match_case(source: &str, suggestion: String) -> String {
    if source.chars().all(|character| character.is_uppercase()) {
        return suggestion.to_uppercase();
    }
    if source.chars().next().is_some_and(char::is_uppercase) {
        let mut characters = suggestion.chars();
        if let Some(first) = characters.next() {
            return first.to_uppercase().collect::<String>() + characters.as_str();
        }
    }
    suggestion
}

pub(crate) fn suggestions(word: &str) -> Vec<String> {
    let Some(engine) = engine() else {
        return Vec::new();
    };
    let key = normalize(word);
    if let Ok(cache) = engine.suggestions.lock() {
        if let Some(cached) = cache.get(&key) {
            return cached.clone();
        }
    }

    let mut candidates = Vec::new();
    engine.fr.suggest(word, &mut candidates);
    let french_count = candidates.len();
    let mut english = Vec::new();
    engine.en.suggest(word, &mut english);
    candidates.extend(english);

    let mut seen = HashSet::new();
    let lower = word.to_lowercase();
    let mut ranked: Vec<(usize, usize, String)> = candidates
        .into_iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            let candidate = match_case(word, candidate);
            seen.insert(candidate.to_lowercase()).then(|| {
                let distance = levenshtein(&lower, &candidate.to_lowercase());
                // Preserve Hunspell's own ordering as a tie-breaker. French
                // candidates precede English ones only when edit distance is equal.
                let language_rank = usize::from(index >= french_count);
                (
                    distance,
                    language_rank.saturating_mul(10_000) + index,
                    candidate,
                )
            })
        })
        .collect();
    ranked.sort_by_key(|(distance, order, _)| (*distance, *order));
    let suggestions: Vec<String> = ranked
        .into_iter()
        .take(SUGGESTION_LIMIT)
        .map(|(_, _, candidate)| candidate)
        .collect();

    if let Ok(mut cache) = engine.suggestions.lock() {
        if cache.len() >= SUGGESTION_CACHE_CAP {
            cache.clear();
        }
        cache.insert(key, suggestions.clone());
    }
    suggestions
}

pub(crate) fn ignore_for_session(word: &str) {
    let Some(engine) = engine() else { return };
    if let Ok(mut ignored) = engine.ignored.write() {
        ignored.insert(normalize(word));
    }
}

pub(crate) fn add_to_personal_dictionary(word: &str) {
    let Some(spell_engine) = engine() else {
        return;
    };
    let word = normalize(word);
    if word.is_empty() {
        return;
    }
    {
        let Ok(mut personal) = spell_engine.personal.write() else {
            return;
        };
        personal.insert(word);
    }
    if let Ok(mut cache) = spell_engine.suggestions.lock() {
        cache.clear();
    }

    // Persistence is deliberately detached from the UI thread. The file is
    // replaced atomically so a shutdown during the write retains the old copy.
    std::thread::Builder::new()
        .name("aviary-spellcheck-save".into())
        .spawn(move || {
            let Ok(_guard) = PERSONAL_SAVE_LOCK.lock() else {
                return;
            };
            // Read after taking the save lock: concurrent additions cannot
            // let an older snapshot overwrite a newer one.
            let Some(engine) = engine() else { return };
            let mut words: Vec<_> = match engine.personal.read() {
                Ok(personal) => personal.iter().cloned().collect(),
                Err(_) => return,
            };
            words.sort_unstable();
            let Some(path) = personal_dictionary_path() else {
                return;
            };
            if let Some(parent) = path.parent() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    log::warn!("failed to create personal dictionary directory: {error}");
                    return;
                }
            }
            let temporary = path.with_extension("txt.tmp");
            let text = words.join("\n") + "\n";
            if let Err(error) = write_personal_dictionary(&temporary, text.as_bytes())
                .and_then(|()| std::fs::rename(&temporary, &path))
            {
                log::warn!("failed to save personal dictionary: {error}");
            }
        })
        .ok();
}

#[cfg(unix)]
fn write_personal_dictionary(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_personal_dictionary(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_french_and_english_in_the_same_text() {
        let issues = check_text("Bonjour, this message est correct.");
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
    }

    #[test]
    fn finds_misspelling_at_utf8_offsets() {
        let text = "Un mesage très court";
        let issue = check_text(text)
            .into_iter()
            .find(|issue| issue.word == "mesage")
            .expect("misspelling");
        assert_eq!(&text[issue.range.clone()], "mesage");
        assert_eq!(issue_at(text, issue.range.start + 2), Some(issue));
    }

    #[test]
    fn skips_email_urls_and_short_acronyms() {
        assert!(check_text("API https://aviary.invalid foo@example.invalid").is_empty());
    }

    #[test]
    fn suggestions_are_bounded() {
        let suggestions = suggestions("mesage");
        assert!(suggestions.len() <= SUGGESTION_LIMIT);
        assert!(suggestions.iter().any(|word| word == "message"));
    }
}
