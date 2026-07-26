//! Maps a block input's source text onto what is actually laid out.
//!
//! Folding a link's destination means the string handed to the text system is no
//! longer the string the document holds: `[Aviary](https://example.test)` lays
//! out as `Aviary`. Offsets then live in two spaces — *source*, which the
//! document, the cursor, the clipboard and the IME speak, and *display*, which
//! the `TextLayout` speaks — and every conversion has exactly one correct
//! direction. This type is the only place that knows both, so a caller that
//! forgets to convert is a caller that fails to compile rather than one that
//! quietly puts the caret a few characters off.
//!
//! With no foldable range the map is the identity and costs nothing, which is
//! what every input outside a link keeps paying.

use std::ops::Range;

/// A stretch of source text that may be hidden, and what has to be touched for
/// it to come back.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FoldableRange {
    /// Hidden from the display while folded.
    pub hidden: Range<usize>,
    /// Unfolds `hidden` while the cursor or the selection touches this range —
    /// for a link, its whole extent: editing the label has to reveal the
    /// destination it points at.
    pub reveal: Range<usize>,
}

/// The hidden ranges currently in effect, sorted and disjoint.
#[derive(Default, Clone)]
pub(crate) struct DisplayMap {
    cuts: Vec<Range<usize>>,
}

impl DisplayMap {
    /// Folds every range of `foldable` that `selection` leaves alone.
    ///
    /// `selection` is the caret when empty, which is why touching is tested
    /// inclusively at both ends: a caret sitting right after a link still
    /// belongs to it. `None` means there is no caret at all — an unfocused
    /// input — and folds everything.
    pub(crate) fn new(
        source: &str,
        foldable: &[FoldableRange],
        selection: Option<&Range<usize>>,
    ) -> Self {
        let mut cuts: Vec<Range<usize>> = foldable
            .iter()
            .filter(|range| {
                // Inclusive on both sides, and empty reveal ranges never fold.
                selection.is_none_or(|selection| {
                    !(selection.start <= range.reveal.end && selection.end >= range.reveal.start)
                })
            })
            .map(|range| range.hidden.clone())
            .filter(|hidden| {
                hidden.start < hidden.end
                    && hidden.end <= source.len()
                    && source.is_char_boundary(hidden.start)
                    && source.is_char_boundary(hidden.end)
            })
            .collect();
        cuts.sort_by_key(|cut| cut.start);
        cuts.dedup();
        // Overlapping cuts would double-count their lengths in both directions.
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(cuts.len());
        for cut in cuts {
            match merged.last_mut() {
                Some(last) if cut.start <= last.end => last.end = last.end.max(cut.end),
                _ => merged.push(cut),
            }
        }
        Self { cuts: merged }
    }

    /// Whether display and source are the same string, so callers can skip the
    /// conversions entirely.
    pub(crate) fn is_identity(&self) -> bool {
        self.cuts.is_empty()
    }

    pub(crate) fn display_text(&self, source: &str) -> String {
        if self.is_identity() {
            return source.to_string();
        }
        let mut out = String::with_capacity(source.len());
        let mut at = 0;
        for cut in &self.cuts {
            out.push_str(&source[at..cut.start]);
            at = cut.end;
        }
        out.push_str(&source[at..]);
        out
    }

    /// Source → display. An offset inside a hidden range collapses onto its
    /// start, the only position that exists on screen.
    pub(crate) fn to_display(&self, offset: usize) -> usize {
        let mut hidden_before = 0;
        for cut in &self.cuts {
            if offset < cut.start {
                break;
            }
            if offset < cut.end {
                return cut.start - hidden_before;
            }
            hidden_before += cut.end - cut.start;
        }
        offset - hidden_before
    }

    /// Display → source. A display offset that lands exactly where a hidden
    /// range was resolves *before* it, so clicking at the end of a label puts
    /// the caret at the end of the label rather than past the destination.
    pub(crate) fn to_source(&self, offset: usize) -> usize {
        let mut hidden_before = 0;
        for cut in &self.cuts {
            let cut_at_display = cut.start - hidden_before;
            if offset <= cut_at_display {
                break;
            }
            hidden_before += cut.end - cut.start;
        }
        offset + hidden_before
    }

    /// Source range → display range, or `None` when it is entirely hidden.
    pub(crate) fn range_to_display(&self, range: &Range<usize>) -> Option<Range<usize>> {
        let start = self.to_display(range.start);
        let end = self.to_display(range.end);
        // A non-empty source range that maps onto nothing is fully hidden; an
        // empty one (the caret) is legitimately empty.
        if start >= end && range.start < range.end {
            return None;
        }
        Some(start..end.max(start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[Aviary](https://example.test)` — the brackets and the destination are
    /// hidden, exactly as `links::foldable_ranges` produces them.
    fn link_source() -> (String, Vec<FoldableRange>) {
        let source = "voir [Aviary](https://example.test) ici".to_string();
        // `[` at 5, label 6..12, `](…)` 12..35, the link itself 5..35.
        let link = 5..35;
        let foldable = vec![
            FoldableRange {
                hidden: 5..6,
                reveal: link.clone(),
            },
            FoldableRange {
                hidden: 12..35,
                reveal: link,
            },
        ];
        (source, foldable)
    }

    #[test]
    fn a_folded_link_lays_out_as_its_label() {
        let (source, foldable) = link_source();
        let map = DisplayMap::new(&source, &foldable, Some(&(0..0)));

        assert_eq!(map.display_text(&source), "voir Aviary ici");
        assert!(!map.is_identity());
    }

    /// The caret inside — or just past — the link brings the markdown back, so
    /// it can be edited at all.
    #[test]
    fn touching_the_link_unfolds_it() {
        let (source, foldable) = link_source();
        for caret in [5, 8, 35] {
            let map = DisplayMap::new(&source, &foldable, Some(&(caret..caret)));
            assert!(
                map.is_identity(),
                "caret {caret} must reveal the destination"
            );
        }
        let outside = DisplayMap::new(&source, &foldable, Some(&(36..36)));
        assert_eq!(outside.display_text(&source), "voir Aviary ici");
    }

    #[test]
    fn a_selection_across_the_link_unfolds_it() {
        let (source, foldable) = link_source();
        let map = DisplayMap::new(&source, &foldable, Some(&(0..38)));
        assert!(map.is_identity());
    }

    #[test]
    fn offsets_round_trip_across_the_hidden_ranges() {
        let (source, foldable) = link_source();
        let map = DisplayMap::new(&source, &foldable, Some(&(0..0)));

        // "voir " is untouched, the label shifts by the hidden `[`, and what
        // follows shifts by the destination too.
        assert_eq!(map.to_display(0), 0);
        assert_eq!(map.to_display(5), 5, "the hidden `[` collapses onto itself");
        assert_eq!(map.to_display(6), 5, "label start");
        assert_eq!(map.to_display(12), 11, "label end");
        assert_eq!(map.to_display(34), 11, "inside the destination");
        assert_eq!(map.to_display(35), 11, "the space after the link");
        assert_eq!(map.to_display(source.len()), 15);

        for display in 0..=15 {
            let round = map.to_display(map.to_source(display));
            assert_eq!(round, display, "display offset {display} must round-trip");
        }
        // Clicking at either edge of the label lands inside the link, which then
        // unfolds — the caret is where the click was, not past the destination.
        assert_eq!(map.to_source(5), 5, "left edge of the label");
        assert_eq!(map.to_source(11), 12, "right edge of the label");
        assert_eq!(map.to_source(12), 36, "the `i` of `ici`");
    }

    #[test]
    fn highlight_ranges_are_clipped_to_what_shows() {
        let (source, foldable) = link_source();
        let map = DisplayMap::new(&source, &foldable, Some(&(0..0)));

        assert_eq!(map.range_to_display(&(6..12)), Some(5..11), "the label");
        assert_eq!(map.range_to_display(&(12..34)), None, "the destination");
        assert_eq!(map.range_to_display(&(0..4)), Some(0..4), "plain text");
    }

    #[test]
    fn the_identity_map_costs_nothing() {
        let map = DisplayMap::new("plain text", &[], Some(&(0..0)));
        assert!(map.is_identity());
        assert_eq!(map.display_text("plain text"), "plain text");
        assert_eq!(map.to_display(4), 4);
        assert_eq!(map.to_source(4), 4);
    }

    /// An unfocused input has no caret, so nothing keeps a link expanded —
    /// leaving a block through its link must not leave it that way.
    #[test]
    fn without_a_caret_everything_folds() {
        let (source, foldable) = link_source();
        // A caret that would reveal the link if it were still there.
        let map = DisplayMap::new(&source, &foldable, None);
        assert_eq!(map.display_text(&source), "voir Aviary ici");
    }

    /// Stale ranges are the expected failure mode here: the editor recomputes
    /// them one frame after the text changes. They must be dropped, never
    /// applied to whatever now sits at those offsets.
    #[test]
    fn out_of_bounds_and_split_ranges_are_ignored() {
        let source = "héllo";
        let foldable = vec![
            FoldableRange {
                hidden: 0..99,
                reveal: 0..99,
            },
            FoldableRange {
                // Inside the two-byte `é`.
                hidden: 2..3,
                reveal: 90..99,
            },
        ];
        let map = DisplayMap::new(source, &foldable, Some(&(50..50)));
        assert!(map.is_identity());
    }

    #[test]
    fn overlapping_cuts_are_merged_before_counting() {
        let source = "0123456789";
        let foldable = vec![
            FoldableRange {
                hidden: 2..6,
                reveal: 2..6,
            },
            FoldableRange {
                hidden: 4..8,
                reveal: 4..8,
            },
        ];
        let map = DisplayMap::new(source, &foldable, Some(&(9..9)));

        assert_eq!(map.display_text(source), "0189");
        assert_eq!(map.to_display(8), 2);
        // A display offset sitting exactly where the cut was resolves before it.
        assert_eq!(map.to_source(2), 2);
        assert_eq!(map.to_source(3), 9);
    }
}
