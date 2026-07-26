use chrono::Locale;

/// Picks the `chrono::Locale` matching the language currently set in
/// `rust-i18n`, so date formats with `%A`, `%B`, `%a`, `%b` render in the
/// user's language. Falls back to `en_US` for any tag we don't explicitly
/// map.
pub(super) fn current_locale() -> Locale {
    let lang = rust_i18n::locale();
    match lang.as_ref() {
        "fr" | "fr-FR" => Locale::fr_FR,
        _ => Locale::en_US,
    }
}
