//! The transcript row: a [`RAIL_W`]-wide margin, then the content a reader
//! would copy.
//!
//! The split is represented, never recovered.  Every consumer downstream —
//! copy, drag-selection, hover, the log — reads `content` or `gutter` by name,
//! so no amount of span coalescing or restyling can smuggle chrome into a
//! clipboard.  Rail marks are set in one place — `Block::railed`, through
//! [`Row::rail`] — and rows are flattened by [`Row::into_line`] at exactly two
//! seams: the screen in [`super::render`] and `user.log` in
//! `super::scrollback`.

use super::line::{self, is_blank};
use super::palette::{GHOST, RAIL_W, content_w};
use ratatui::style::{Color, Modifier, Style};
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

    /// A row with a blank margin: content that wears no glyph.
    pub(super) fn bare(content: Line<'static>) -> Self {
        Self::new(Span::raw(BLANK), content)
    }

    /// Set `glyph` on the first row of `lines` that carries content, every
    /// other row wearing the blank margin.  The one way a rail mark is set: a
    /// `None` glyph — a continuing paragraph, a framed card that is its own
    /// mark — still yields the margin every row wears.  An all-blank body takes
    /// it on row 0, so a block that renders nothing shows nothing.
    pub(super) fn rail(lines: Vec<Line<'static>>, glyph: Option<Span<'static>>) -> Vec<Self> {
        let mark = glyph.map(|glyph| {
            let idx = lines.iter().position(|l| !is_blank(l)).unwrap_or(0);
            (glyph, idx)
        });
        lines
            .into_iter()
            .enumerate()
            .map(|(i, line)| match &mark {
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

    /// Repaint the row as a silhouette, margin included: the preview of text
    /// a `/rewind` would cut.
    pub(super) fn ghost(&mut self) {
        let ink = Style::default().fg(GHOST);
        self.gutter.style = ink;
        for span in &mut self.content.spans {
            span.style = ink;
        }
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
mod tests;
