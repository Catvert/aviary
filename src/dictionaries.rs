//! Shared, lazily parsed spelling dictionaries used by the editor.

use spellbook::Dictionary;
use std::sync::OnceLock;

const FR_AFF: &str = include_str!("../assets/spellcheck/fr.aff");
const FR_DIC: &str = include_str!("../assets/spellcheck/fr.dic");
const EN_AFF: &str = include_str!("../assets/spellcheck/en.aff");
const EN_DIC: &str = include_str!("../assets/spellcheck/en.dic");

static FRENCH: OnceLock<Option<Dictionary>> = OnceLock::new();
static ENGLISH: OnceLock<Option<Dictionary>> = OnceLock::new();

pub(crate) fn french() -> Option<&'static Dictionary> {
    FRENCH
        .get_or_init(|| match Dictionary::new(FR_AFF, FR_DIC) {
            Ok(dictionary) => Some(dictionary),
            Err(error) => {
                log::error!("failed to initialize French spelling dictionary: {error}");
                None
            }
        })
        .as_ref()
}

pub(crate) fn english() -> Option<&'static Dictionary> {
    ENGLISH
        .get_or_init(|| match Dictionary::new(EN_AFF, EN_DIC) {
            Ok(dictionary) => Some(dictionary),
            Err(error) => {
                log::error!("failed to initialize English spelling dictionary: {error}");
                None
            }
        })
        .as_ref()
}
