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
| `blitz-dom` | `0.3.0-beta.1` | `11279518661e1e0af3ee141c232daffa8968d3fe` | `src/layout/construct.rs`, `src/layout/table.rs` | Preserve non-breaking spaces and correct email table track/row sizing. |
| `blitz-paint` | `0.3.0-beta.1` | `11279518661e1e0af3ee141c232daffa8968d3fe` | `src/render/border.rs`, `src/text.rs` | Do not paint `border-style: none` collapsed borders and correct faux-italic skew. |
| `cosmic-text` | `0.14.2` | `9e7a56f083db15f67510df4396351464df2e64bd` | `src/font/fallback/unix.rs` | Prefer the embedded color emoji font before monochrome text fallbacks on Unix. |
| `stylo_derive` | `0.19.0` | `e0bcd28a1f1a0b35903b4a9b7652c6b993a26ccc` | `to_css.rs` | Avoid ambiguous `?` conversions when GPUI enables `log`'s Serde key-value support. |

All other source files should remain identical to the corresponding published
crate.

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
