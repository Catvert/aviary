# Patched crates

Cargo's `[patch.crates-io]` mechanism replaces a complete crate; it does not
overlay individual files on the crates.io release. These directories therefore
contain the complete source needed to compile each patched crate, but they are
not Git clones and contain no upstream history.

Tests, examples, benchmarks, screenshots, CI files, and other material that is
not needed to build Aviary are intentionally omitted. Upstream manifests and
license files are retained where the published package provides them.

## Patch inventory

| Crate | Base | Upstream commit | Modified files | Purpose |
| --- | --- | --- | --- | --- |
| `blitz-dom` | `0.3.0-beta.4` | `92b11f1bd8ab7975411328b0406a95a09445b4e7` | `src/layout/table.rs` | Collapse the redundant `colspan` of tables holding one cell per row into a single column. |
| `blitz-paint` | `0.3.0-beta.4` | `92b11f1bd8ab7975411328b0406a95a09445b4e7` | `src/text.rs` | Correct faux-italic skew. |
| `cosmic-text` | `0.19.0` | `c24886c2471e5606587c46090cd25dbbf209186b` | `src/font/fallback/unix.rs` | Prefer the embedded color emoji font before monochrome text fallbacks on Unix. |

All other source files should remain identical to the corresponding published
crate.

## Blitz beta.4 audit (2026-11-11)

The published beta.4 sources (Stylo 0.22, Parley 0.12, Taffy 0.15, AnyRender
0.14, Vello CPU 0.3, Fontique 0.12, html5ever 0.40) were built and tested
without any local Blitz patch before reapplying fixes. `cargo test blitz_body`
then failed on exactly one test, which decided what stays:

| Former patch | Outcome on beta.4 |
| --- | --- |
| `construct.rs` NBSP runs | **Dropped.** Parley 0.12 no longer trims U+00A0 at span boundaries; `nonbreaking_spaces_preserve_outlook_style_indentation` passes unpatched. |
| `table.rs` column count across rows | **Dropped.** Upstream (`ColumnCursor.num_columns`). |
| `table.rs` collapsed-border used widths | **Dropped.** Upstream (#791). |
| `border.rs` `none`/`hidden` collapsed borders | **Dropped.** Upstream (#791), so `border.rs` is unmodified. |
| `table.rs` single-cell `colspan` collapse | **Kept**, re-applied by hand onto `ColumnCursor`: `wrapped_text_rows_expand_before_the_next_row` fails without it ("a redundant colspan created an empty text column"). The cursor now records whether any row places more than one cell or any cell spans rows; the collapse is also skipped for tables declaring `<col>` elements, which the rewritten column code keeps in step with the track list. |
| `text.rs` faux-italic skew direction | **Kept.** beta.4 still supplies `Affine::skew(+tan, 0)` and Glifo 0.4 still composes the glyph transform with `Affine::FLIP_Y` (`glifo/src/glyph.rs`). Only the unit test inside the patched crate covers it (`cargo test -p blitz-paint synthetic_italic`). Worth reporting upstream. |
| `stylo_derive` explicit `match` | **Dropped, directory removed.** The ambiguity came from gpui enabling `log/kv_serde`; gpui-pre 0.3.12 no longer does, and Stylo 0.22's derive compiles unmodified in Aviary's graph. If a future gpui re-enables it, the symptom is E0282/E0283 inside `derive(ToCss)` output. |
| cosmic-text emoji fallback order | Unchanged: still 0.19.0 upstream. |

API changes absorbed in `src/ui/blitz_body/`: `DocumentConfig::ua_stylesheets`
is a `Vec<UserAgentStylesheet>` of parsed sheets (the static ones are parsed
once and shared), and `string_cache` atoms gained a second `AsRef` impl, so
name comparisons deref (`&*name.local == "a"`) instead of calling `as_ref()`.
`StyleThreading` defaults to `Sequential`, which is what one actor thread per
document needs — do not switch it to `Parallel`.

## Updating a patched crate

1. Extract the new crates.io package into a temporary directory.
2. Check whether the listed fix is already present upstream. Remove the local
   patch entirely when it is.
3. Otherwise, reapply only the relevant changes to a fresh copy and remove the
   non-build material described above.
4. Update this inventory and the local manifest version, then run `just check`
   and `just clippy`.

If an upstream dependency no longer accepts the version declared by the local
manifest, Cargo will stop selecting that patch (and normally reports that it was
unused). Never solve that by changing only the version number: always rebase the
modified files onto the matching upstream sources.
