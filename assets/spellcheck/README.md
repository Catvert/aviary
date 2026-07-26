# Embedded spelling dictionaries

The `fr` and `en` Hunspell dictionaries are vendored from the normalized
[`wooorm/dictionaries`](https://github.com/wooorm/dictionaries) collection so
the mail editor can spell-check fully offline on every supported platform.

- French: Grammalecte “classique” 7.5, MPL-2.0 (`fr.LICENSE`).
- English: SCOWL `en_US` 2020.12.07, permissive/MIT-style licenses detailed in
  `en.LICENSE`.

The `.aff` and `.dic` files are consumed by the pure-Rust `spellbook` crate and
embedded in the Aviary binary with `include_str!`.
