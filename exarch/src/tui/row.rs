//! The transcript row: a [`RAIL_W`]-wide margin, then the content a reader
//! would copy.
//!
//! The split is represented, never recovered.  Every consumer downstream —
//! copy, drag-selection, hover, the log — reads `content` or `gutter` by name,
//! so no amount of span coalescing or restyling can smuggle chrome into a
//! clipboard.  Rows are born in the two places that seat rails — `Block::rows`
//! and `Viewport::render_group`, both through [`Row::seat`] — multiplied in
//! [`Row::wrap`], and flattened by [`Row::into_line`] at exactly two seams: the
//! screen in [`super::render`] and `user.log` in `super::viewport`.

use super::line::{self, is_blank, wrap_line};
use super::palette::{RAIL_W, content_w};
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// A blank margin, borrowed rather than allocated — the common case by far.
const BLANK: &str = "  ";
const _: () = assert!(BLANK.len() == RAIL_W);

#[derive(Clone, Debug)]
pub(super) struct Row {
    /// Exactly [`RAIL_W`] display columns.  A block's head row carries its
    /// shape glyph, the prompt fence its rule ink, every other row a blank.
    gutter: Span<'static>,
    content: Line<'static>,
}

impl Row {
    /// The one constructor, so the width invariant is checked here and nowhere
    /// else.  A gutter of any other width would shear every column downstream.
    pub(super) fn new(gutter: Span<'static>, content: Line<'static>) -> Self {
        debug_assert_eq!(
            UnicodeWidthStr::width(gutter.content.as_ref()),
            RAIL_W,
            "a gutter must be exactly RAIL_W columns"
        );
        Self { gutter, content }
    }

    /// A row with a blank margin: content that seats no glyph.
    pub(super) fn bare(content: Line<'static>) -> Self {
        Self::new(Span::raw(BLANK), content)
    }

    /// Seat `glyph` on the first row of `lines` that carries content, every
    /// other row wearing the blank margin.  The one way a rail is seated: a
    /// `None` glyph — a continuing paragraph, a framed card that is its own
    /// mark — still yields the margin every row wears.  An all-blank body seats
    /// on row 0, so a block that renders nothing shows nothing.
    pub(super) fn seat(lines: Vec<Line<'static>>, glyph: Option<Span<'static>>) -> Vec<Self> {
        let seat = glyph.map(|glyph| {
            let idx = lines.iter().position(|l| !is_blank(l)).unwrap_or(0);
            (glyph, idx)
        });
        lines
            .into_iter()
            .enumerate()
            .map(|(i, line)| match &seat {
                Some((glyph, idx)) if i == *idx => Self::new(glyph.clone(), line),
                _ => Self::bare(line),
            })
            .collect()
    }

    /// The mark this row wears in the margin.  Nothing in the app reads it —
    /// the margin is written, painted and flattened, never interpreted — so it
    /// exists for the tests that check which row wears which shape.
    #[cfg(test)]
    pub(super) fn gutter(&self) -> &str {
        self.gutter.content.as_ref()
    }

    pub(super) fn content_mut(&mut self) -> &mut Line<'static> {
        &mut self.content
    }

    /// The row as the text a reader would copy: content spans joined, margin
    /// dropped.  This is the whole copy contract.
    pub(super) fn plain(&self) -> String {
        self.content
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    /// True when the content carries no glyphs, so the row reads as a vertical
    /// gap.  The margin never counts: a blank gutter is not content.
    pub(super) fn is_blank(&self) -> bool {
        is_blank(&self.content)
    }

    /// Bytes the row holds, margin included — the render-cost probe's measure.
    pub(super) fn bytes(&self) -> usize {
        RAIL_W
            + self
                .content
                .spans
                .iter()
                .map(|s| s.content.len())
                .sum::<usize>()
    }

    /// Screen width including the margin — what a pointer hit-test measures.
    pub(super) fn width(&self) -> usize {
        RAIL_W + self.content.width()
    }

    /// Light the margin: the hovered block's one mark.
    pub(super) fn hover(&mut self) {
        self.gutter.style = self.gutter.style.add_modifier(Modifier::REVERSED);
    }

    /// Lay a background stratum across the whole row, margin included, and fill
    /// the content edge to edge — the queued-prompt plane, which must read as
    /// one band rather than a band with a notch cut out of its margin.
    pub(super) fn wash(self, bg: Color, width: u16) -> Self {
        Self::new(
            Span::styled(self.gutter.content, self.gutter.style.bg(bg)),
            line::wash(self.content, bg, Some(content_w(width).into())),
        )
    }

    /// Fold into visual rows no wider than `width`, margin included.  Row 0
    /// keeps this row's gutter and every continuation gets a blank, so a
    /// wrapped block seats its glyph once.
    pub(super) fn wrap(&self, width: usize) -> Vec<Self> {
        let mut rows = wrap_line(&self.content, width.saturating_sub(RAIL_W)).into_iter();
        let head = rows.next().unwrap_or_default();
        std::iter::once(Self::new(self.gutter.clone(), head))
            .chain(rows.map(Self::bare))
            .collect()
    }

    /// Margin then content: the one flatten, for the screen and for `user.log`.
    /// A row with nothing in either flattens to nothing rather than to a margin
    /// of trailing spaces — invisible on screen, and `user.log` stays clean.
    pub(super) fn into_line(self) -> Line<'static> {
        if self.is_blank() && self.gutter.content.trim().is_empty() {
            return Line::default();
        }
        let mut spans = Vec::with_capacity(self.content.spans.len() + 1);
        spans.push(self.gutter);
        spans.extend(self.content.spans);
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::super::block::AgentSlot;
    use super::super::palette::PROMPT_INK;
    use super::super::rail::{RAIL_SHAPES, span as rail_span};
    use super::*;
    use ratatui::style::Style;

    /// Every shape in the vocabulary seats a gutter of the invariant width —
    /// the geometric coupling that replaced the old copy-time glyph sniff.
    #[test]
    fn every_rail_shape_is_a_legal_gutter() {
        for &(kind, _) in RAIL_SHAPES {
            let _ = Row::new(rail_span(kind, AgentSlot(0), None), Line::default());
        }
    }

    /// The regression the `Row` split exists to make impossible: a gutter whose
    /// ink matches its content's used to be coalesced into the first body span
    /// by `wrap_line`, after which copy could no longer tell chrome from text.
    /// The prompt fence is exactly this collision — both are `PROMPT_INK`.
    #[test]
    fn wrapping_never_leaks_the_glyph_into_content() {
        let ink = Style::default().fg(PROMPT_INK);
        let row = Row::new(
            Span::styled("❖ ", ink),
            Line::from(Span::styled(
                "alpha beta gamma delta epsilon zeta eta theta iota",
                ink,
            )),
        );
        let rows = row.wrap(20);
        assert!(rows.len() > 1, "the fixture must actually wrap");
        for r in &rows {
            assert!(
                !r.plain().contains('❖'),
                "copy leaked the glyph: {:?}",
                r.plain()
            );
        }
    }

    /// Continuations hang at the content's own column zero, the glyph riding
    /// row 0 alone.
    #[test]
    fn wrap_seats_the_gutter_once() {
        let row = Row::new(
            Span::styled("▸ ", Style::default()),
            Line::from(Span::raw("alpha beta gamma delta epsilon zeta eta theta")),
        );
        let rows = row.wrap(20);
        assert_eq!(rows[0].gutter.content.as_ref(), "▸ ");
        for r in &rows[1..] {
            assert_eq!(r.gutter.content.as_ref(), BLANK);
            assert!(!r.plain().starts_with(' '), "continuation gained an indent");
        }
    }

    /// Wrapping respects the margin: no visual row exceeds the given width.
    #[test]
    fn wrap_reserves_the_margin() {
        let row = Row::bare(Line::from(Span::raw(
            "one two three four five six seven eight nine ten",
        )));
        for r in row.wrap(16) {
            assert!(r.width() <= 16, "row overflowed: {}", r.width());
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
}
