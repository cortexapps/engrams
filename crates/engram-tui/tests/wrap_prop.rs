//! Property tests for the wrapper (ADR 0099 H3): any text at any width
//! wraps without panicking, no row is wider than the width, rows are on
//! char boundaries and in order, and every non-break character of the
//! text is covered exactly once.

use engram_tui::wrap::{display_width, wrap_into};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(FileFailurePersistence::WithSource("proptest-regressions"))),
        cases: 512,
        ..ProptestConfig::default()
    })]

    #[test]
    fn rows_fit_cover_and_stay_ordered(text in "[ -~\u{e9}\u{65e5}\u{672c}\n\t]{0,120}", width in 1u16..40) {
        let mut rows = Vec::new();
        wrap_into(&text, width, &mut rows);
        prop_assert!(!rows.is_empty());
        let mut covered = 0usize;
        let mut last_end = 0usize;
        for r in &rows {
            let (s, e) = (r.start as usize, r.end as usize);
            prop_assert!(s <= e && e <= text.len());
            prop_assert!(text.is_char_boundary(s) && text.is_char_boundary(e));
            prop_assert!(s >= last_end, "rows overlap or go backwards");
            // The gap between rows is only break whitespace.
            prop_assert!(text[last_end..s].chars().all(|c| c == ' ' || c == '\n' || c == '\r'));
            let row = &text[s..e];
            // A single grapheme wider than the width is the one allowed overflow.
            let w = display_width(row);
            prop_assert!(w <= width as usize || row.chars().count() == 1, "{row:?} is {w} wide at width {width}");
            covered += row.chars().filter(|c| *c != '\n').count();
            last_end = e;
        }
        let total = text.chars().filter(|c| !matches!(c, '\n' | '\r')).count();
        let spaces = text.chars().filter(|c| *c == ' ').count();
        prop_assert!(covered <= total && covered + spaces >= total);
    }
}
