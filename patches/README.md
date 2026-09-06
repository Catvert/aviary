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
| `blitz-dom` | `0.3.0-beta.2` | `67edf2061121382b3e43af19977fc3472aa7e069` | `src/layout/construct.rs`, `src/layout/table.rs` | Preserve non-breaking spaces and correct email table tracks; backport used collapsed-border widths. |
| `blitz-paint` | `0.3.0-beta.2` | `67edf2061121382b3e43af19977fc3472aa7e069` | `src/render/border.rs`, `src/text.rs` | Do not paint `border-style: none` collapsed borders and correct faux-italic skew. |
| `cosmic-text` | `0.19.0` | `c24886c2471e5606587c46090cd25dbbf209186b` | `src/font/fallback/unix.rs` | Prefer the embedded color emoji font before monochrome text fallbacks on Unix. |
| `stylo_derive` | `0.20.0` | `67faaab3ff7aa66780ec1d0f51ca47e177b812d3` | `to_css.rs` | Avoid ambiguous `?` conversions when GPUI enables `log`'s Serde key-value support. |

All other source files should remain identical to the corresponding published
crate.

## Blitz beta.2 audit (2026-09-06)

The published beta.2 sources were tested without local Blitz patches before
reapplying fixes. The retained differences are:

- `construct.rs`: preserve NBSP runs. Parley 0.11.1 still trims Unicode
  whitespace at span boundaries; the indentation regression test fails without
  this fix.
- `table.rs`: retain the maximum column count across rows and collapse redundant
  single-cell `colspan` tracks. The wrapped-row regression test still fails
  without the column fix.
- **Removed** the forced `PerformLayout` pass during vertical intrinsic sizing.
  Both nested Outlook rows and wrapped text rows now size correctly with
  beta.2/Taffy 0.14 once the column fix is applied.
- Collapsed borders: replace the old paint-only workaround with the upstream
  [#791 fix](https://github.com/DioxusLabs/blitz/commit/2bc63f4b3f), including
  zero used widths in table spacing. This is already on `main` but was merged
  after beta.2, so the backport remains necessary until the next release.
- `text.rs`: retain the negative faux-italic skew. The published painter still
  supplies a positive skew, while Glifo 0.2 applies the glyph transform before
  its font-space Y flip. The local unit test checks the resulting direction.

Stylo's `ToCss` error-propagation patch is rebased onto 0.20.0, retaining its
upstream change for function variants without fields. All four Blitz crates,
AnyRender 0.13, its Vello CPU backend 0.17, Vello CPU 0.1 and Fontique 0.11 are
aligned in the root manifest. Incremental layout is now enabled by default at
runtime (`DocumentConfig::incremental`), replacing the removed Cargo feature.

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
