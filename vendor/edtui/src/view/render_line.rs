// LOCAL PATCH: render the same indivisible graphemes used by measurement and mouse input.
use ratatui_core::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::Style,
    text::Span,
};

use super::geometry::LineGeometry;

pub(super) struct RenderLine {
    text: String,
    styles: Vec<Style>,
    geometry: LineGeometry,
    rows: usize,
    skipped_rows: usize,
}

impl RenderLine {
    pub(super) fn new(
        spans: Vec<Span<'_>>,
        width: usize,
        tab_width: usize,
        wrap: bool,
        cursor: Option<usize>,
    ) -> Self {
        let mut text = String::new();
        let mut styles = Vec::new();
        for span in spans {
            styles.extend(std::iter::repeat_n(span.style, span.content.chars().count()));
            text.push_str(&span.content);
        }
        let geometry = LineGeometry::new(&text, width, tab_width, wrap);
        let rows = cursor.map_or(geometry.rows(), |col| geometry.rows_with_cursor(col));
        Self { text, styles, geometry, rows, skipped_rows: 0 }
    }

    pub(super) fn skip_rows(mut self, rows: usize) -> Self {
        self.skipped_rows = rows;
        self
    }

    pub(super) fn num_lines(&self) -> usize {
        self.rows.saturating_sub(self.skipped_rows)
    }

    pub(super) fn data_coordinate_to_screen_coordinate(
        &self,
        data_col: usize,
        area: Rect,
    ) -> Option<Position> {
        let position = self.geometry.position(data_col);
        let row = position.row.checked_sub(self.skipped_rows)?;
        (position.col < usize::from(area.width) && row < usize::from(area.height))
            .then(|| Position::new(area.x + position.col as u16, area.y + row as u16))
    }

    pub(super) fn render(self, area: Rect, buf: &mut Buffer) {
        for glyph in &self.geometry.glyphs {
            let Some(row) = glyph.position.row.checked_sub(self.skipped_rows) else {
                continue;
            };
            if row >= usize::from(area.height)
                || glyph.position.col + glyph.width > usize::from(area.width)
                || glyph.width == 0
            {
                continue;
            }
            let grapheme = &self.text[glyph.bytes.clone()];
            let text = if glyph.replacement {
                "�".to_owned()
            } else if grapheme == "\t" {
                " ".repeat(glyph.width)
            } else {
                grapheme.to_owned()
            };
            // A style boundary inside a cluster inherits its first scalar's style.
            buf.set_stringn(
                area.x + glyph.position.col as u16,
                area.y + row as u16,
                text,
                glyph.width,
                self.styles[glyph.scalars.start],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_cannot_split_a_grapheme() {
        let line = RenderLine::new(
            vec![Span::raw("👩"), Span::styled("\u{200d}💻", Style::default().red())],
            2, 2, true, Some(3),
        );
        let area = Rect::new(0, 0, 2, 2);
        let mut buffer = Buffer::empty(area);
        assert_eq!(line.num_lines(), 2);
        assert_eq!(line.data_coordinate_to_screen_coordinate(3, area), Some(Position::new(0, 1)));
        line.render(area, &mut buffer);
        assert_eq!(buffer[(0, 0)].symbol(), "👩\u{200d}💻");
    }
}
