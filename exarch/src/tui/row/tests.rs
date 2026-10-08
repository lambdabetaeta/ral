use super::super::block::AgentSlot;
use super::super::palette::PROMPT_INK;
use super::super::rail::{RAIL_SHAPES, span as rail_span};
use super::*;
use ratatui::style::Style;

/// Every shape in the vocabulary fills a gutter of the invariant width —
/// the geometric coupling that replaced the old copy-time glyph sniff.
#[test]
fn every_rail_shape_is_a_legal_gutter() {
    for &(kind, _) in RAIL_SHAPES {
        let _ = Row::new(rail_span(kind, AgentSlot(0), None), Line::default());
    }
}

/// Copy reads content, flatten reads both — the two must not be confused.
#[test]
fn plain_drops_the_margin_and_flatten_keeps_it() {
    let row = Row::new(Span::raw("· "), Line::from(Span::raw("text")));
    assert_eq!(row.plain(), "text");
    assert_eq!(row.into_line().width(), RAIL_W + 4);
}
/// Bug 2 as a contract: a selection can never reach the margin, whatever
/// column it is asked for.  `paint_selection`'s interior rows used to
/// reverse every span on the line, lighting the rail; now the margin is not
/// reachable from `highlight_range` at all.
#[test]
fn highlighting_a_whole_row_leaves_the_margin_alone() {
    use super::super::select::highlight_range;
    let mut row = Row::new(
        Span::styled("∴ ", Style::default().fg(PROMPT_INK)),
        Line::from(Span::raw("some prose to select")),
    );
    let before = row.gutter.style;
    highlight_range(&mut row, 0, u16::MAX);
    assert_eq!(row.gutter.style, before, "the margin was restyled");
    assert!(
        row.content
            .spans
            .iter()
            .any(|s| s.style.add_modifier(Modifier::REVERSED) == s.style),
        "the content was not highlighted at all"
    );
}

/// Bug 3 as a contract: content column zero is the same screen cell on every
/// row, so a drag lands where the pointer is.  Under the old span sniff a
/// blank-margin row disagreed with a glyph-margin one by two cells.
#[test]
fn every_margin_encoding_shares_one_content_origin() {
    use super::super::select::plain_slice;
    let content = || Line::from(Span::raw("abcdefgh"));
    let rows = [
        Row::new(Span::styled("▎ ", Style::default()), content()),
        Row::bare(content()),
        Row::new(Span::styled("──", Style::default()), content()),
    ];
    let origin = u16::try_from(RAIL_W).expect("the margin is two columns");
    for row in &rows {
        assert_eq!(
            plain_slice(row, origin, origin + 4),
            "abcd",
            "a selection at the content origin slipped: {:?}",
            row.gutter()
        );
    }
}

/// Bug 4 as a contract: a zero-width combining mark shares its base
/// character's cell, so a selection ending on that cell must not cut
/// between the two and drop the mark.
#[test]
fn plain_slice_keeps_a_trailing_combining_mark() {
    use super::super::select::plain_slice;
    let row = Row::bare(Line::from(Span::raw("e\u{301}bc")));
    let origin = u16::try_from(RAIL_W).expect("the margin is two columns");
    assert_eq!(plain_slice(&row, origin, origin + 1), "e\u{301}");
}
