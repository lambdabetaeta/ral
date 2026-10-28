//! LOCAL PATCH: styling uses scalar coordinates; geometry is handled by LineGeometry.
//! A collection of internal datatypes for rendering.
//!
//! TODO: Refactor.
#[cfg(feature = "syntax-highlighting")]
use crate::SyntaxHighlighter;
use crate::{
    helper::split_str_at,
    state::highlight::Highlight,
    state::selection::Selection,
};
use jagged::Index2;
use ratatui_core::{style::Style, text::Span};

/// An internal data type that represent a styled span.
/// Unlike [`ratatui_core::text::Span`] it holds an owned string simplifying lifetime management.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct InternalSpan {
    pub(crate) content: String,
    pub(crate) style: Style,
}

impl InternalSpan {
    pub(crate) fn new<T: Into<String>>(content: T, style: &Style) -> Self {
        Self {
            content: content.into(),
            style: *style,
        }
    }

    pub(crate) fn spans_len(spans: &[Self]) -> usize {
        spans.iter().map(|span| span.content.chars().count()).sum()
    }

    fn split_at_selection(
        spans: &[Self],
        row_index: usize,
        selection: &Selection,
        style: &Style,
    ) -> Option<Vec<InternalSpan>> {
        let chars: Vec<char> = spans.iter().flat_map(|span| span.content.chars()).collect();
        let range = selection.grapheme_columns(&chars, row_index)?;
        if range.is_empty() {
            return None;
        }
        Some(Self::split_spans(spans, range.start, range.end - 1, style))
    }

    fn split_at_highlight(
        spans: &[Self],
        row_index: usize,
        highlight: &Highlight,
    ) -> Option<Vec<InternalSpan>> {
        let spans_len = InternalSpan::spans_len(spans);
        let (start_col, end_col) = highlight.get_columns_in_row(row_index, spans_len)?;
        if end_col < start_col {
            return None;
        }

        Some(Self::split_spans(
            spans,
            start_col,
            end_col,
            &highlight.style,
        ))
    }

    /// Splits spans by `crop_at` from the left.
    ///
    /// This is necessary because when the editor is scrolled horizontally,
    /// we have to cut off the text from the left.
    fn crop_spans(spans: &mut Vec<Self>, crop_at: usize) {
        if crop_at == 0 {
            return;
        }

        let mut span_offset = 0;
        let mut first_visible_span = 0;
        let mut split_span_at = 0;

        for (i, span) in spans.iter().enumerate() {
            let span_width = span.content.chars().count();
            let span_start = span_offset;
            let span_end = span_offset + span_width;

            // span is fully on screen
            // ---|span|
            //  c
            if span_start >= crop_at {
                break;
            }

            // span must be cut to fit on screen
            // -|span|
            //   c
            if span_end > crop_at {
                split_span_at = crop_at - span_start;
                break;
            }

            // span is not shown on screen - remove the full span
            first_visible_span = i + 1;
            span_offset += span_width;
        }

        if first_visible_span > 0 {
            spans.drain(0..first_visible_span);
        }

        if split_span_at > 0 {
            let first_span = spans.remove(0);
            let (_, right) = split_str_at(&first_span.content, split_span_at);
            spans.insert(0, InternalSpan::new(right, &first_span.style));
        }
    }

    fn split_spans(
        spans: &[Self],
        split_start: usize,
        split_end: usize,
        style: &Style,
    ) -> Vec<Self> {
        let mut new_spans: Vec<InternalSpan> = Vec::new();
        let mut offset = 0;

        for span in spans {
            let span_len = span.content.chars().count();
            let span_start = offset;
            let span_end = offset + span_len.saturating_sub(1);

            // Case a: Span ends before split_start, append it unchanged
            // Case b: Span starts after split_end, append it unchanged
            if span_end < split_start || span_start > split_end {
                new_spans.push(span.clone());
            }
            // Case c: Split front
            else if split_start <= span_start && split_end < span_end {
                let split_point = split_end - span_start + 1;
                let (left, right) = split_str_at(&span.content, split_point);
                new_spans.push(InternalSpan::new(left, style));
                new_spans.push(InternalSpan::new(right, &span.style));
            }
            // Case d: Split back
            else if split_start > span_start && split_end >= span_end {
                let split_point = split_start - span_start;
                let (left, right) = split_str_at(&span.content, split_point);
                new_spans.push(InternalSpan::new(left, &span.style));
                new_spans.push(InternalSpan::new(right, style));
            }
            // Case e: Split middle
            else if split_start > span_start && split_end < span_end {
                let split_front = split_start - span_start;
                let split_back = split_end - span_start + 1;
                let (left, rest) = split_str_at(&span.content, split_front);
                let (middle, right) = split_str_at(&rest, split_back - split_front);

                new_spans.push(InternalSpan::new(left, &span.style));
                new_spans.push(InternalSpan::new(middle, style));
                new_spans.push(InternalSpan::new(right, &span.style));
            }
            // Case f: Split none (entire span is between split_start and split_end)
            else if split_start <= span_start && split_end >= span_end {
                new_spans.push(InternalSpan::new(span.content.clone(), style));
            }

            offset += span_len;
        }

        new_spans
    }
}

impl From<Span<'_>> for InternalSpan {
    fn from(value: Span) -> Self {
        Self::new(value.content, &value.style)
    }
}

impl From<InternalSpan> for Span<'_> {
    fn from(value: InternalSpan) -> Self {
        Self::styled(value.content, value.style)
    }
}

/// Converts a line into a vector of `Span`s, applying styles based on the given selections.
pub(crate) fn line_into_spans_with_selections<'a>(
    line: &[char],
    selections: &[&Option<Selection>],
    highlights: &[Highlight],
    row_index: usize,
    col_skips: usize,
    base_style: &Style,
    selection_style: &Style,
) -> Vec<Span<'a>> {
    let mut spans = Vec::new();

    let mut current_span = String::new();
    let mut current_style = *base_style;
    let selected: Vec<_> = selections.iter()
        .filter_map(|s| s.as_ref()?.grapheme_columns(line, row_index))
        .collect();

    for (i, &ch) in line.iter().skip(col_skips).enumerate() {
        let position = Index2::new(row_index, col_skips + i);

        // Determine style: selection takes priority, then highlights, then base
        let new_style = if selected.iter().any(|range| range.contains(&position.col))
        {
            *selection_style
        } else if let Some(h) = highlights.iter().find(|h| h.contains(&position)) {
            h.style
        } else {
            *base_style
        };

        if i != 0 && new_style != current_style {
            spans.push(Span::styled(
                std::mem::take(&mut current_span),
                current_style,
            ));
        }

        current_style = new_style;
        current_span.push(ch);
    }

    if !current_span.is_empty() {
        spans.push(Span::styled(current_span, current_style));
    }

    spans
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "syntax-highlighting")]
pub(crate) fn line_into_highlighted_spans_with_selections<'a>(
    line: &[char],
    selections: &[&Option<Selection>],
    highlights: &[Highlight],
    syntax_highligher: &SyntaxHighlighter,
    row_index: usize,
    col_skips: usize,
    base_style: &Style,
    selection_style: &Style,
) -> Vec<Span<'a>> {
    let line: String = line.iter().collect();
    let mut internal_spans = syntax_highligher.highlight_line(&line, base_style);

    // Apply custom highlights first
    for highlight in highlights.iter().filter(|h| h.contains_row(row_index)) {
        if let Some(new_spans) =
            InternalSpan::split_at_highlight(&internal_spans, row_index, highlight)
        {
            internal_spans = new_spans;
        }
    }

    // Apply selections (they take priority over highlights)
    let selections = selections
        .iter()
        .filter_map(|selection| selection.as_ref().filter(|s| s.contains_row(row_index)));

    for selection in selections {
        if let Some(spans) = InternalSpan::split_at_selection(
            &internal_spans, row_index, selection, selection_style,
        ) {
            internal_spans = spans;
        }
    }

    if col_skips > 0 {
        InternalSpan::crop_spans(&mut internal_spans, col_skips);
    }

    internal_spans.into_iter().map(Span::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_internal_line_into_spans() {
        // given a highlighted line
        let base = Style::default();
        let hightlighted = Style::default().red();
        let line = "Hello".chars().into_iter().collect::<Vec<char>>();

        let selection = Some(Selection::new(Index2::new(0, 0), Index2::new(0, 2)));
        let selections = vec![&selection];

        // when `line_into_spans_with_selections` is called
        let spans =
            line_into_spans_with_selections(&line, &selections, &[], 0, 0, &base, &hightlighted);

        // then span is split into highlighted spans
        assert_eq!(spans[0], Span::styled("Hel", hightlighted));
        assert_eq!(spans[1], Span::styled("lo", base));
    }

    #[test]
    fn test_internal_span_split_spans() {
        // given a highlighted line
        let base = &Style::default();
        let hightlighted = &Style::default().red();
        let spans = vec![
            InternalSpan::new("Hel", base),
            InternalSpan::new("lo!", base),
        ];

        // when `split_spans` is called
        let new_spans = InternalSpan::split_spans(&spans, 1, 1, &hightlighted);

        // then the span is split correctly
        assert_eq!(new_spans[0], InternalSpan::new("H", base));
        assert_eq!(new_spans[1], InternalSpan::new("e", hightlighted));
        assert_eq!(new_spans[2], InternalSpan::new("l", base));
        assert_eq!(new_spans[3], InternalSpan::new("lo!", base));

        // when
        let new_spans = InternalSpan::split_spans(&spans, 1, 2, &hightlighted);

        // then
        assert_eq!(new_spans[0], InternalSpan::new("H", base));
        assert_eq!(new_spans[1], InternalSpan::new("el", hightlighted));
        assert_eq!(new_spans[2], InternalSpan::new("lo!", base));

        // when
        let new_spans = InternalSpan::split_spans(&spans, 1, 3, &hightlighted);

        // then
        assert_eq!(new_spans[0], InternalSpan::new("H", base));
        assert_eq!(new_spans[1], InternalSpan::new("el", hightlighted));
        assert_eq!(new_spans[2], InternalSpan::new("l", hightlighted));
        assert_eq!(new_spans[3], InternalSpan::new("o!", base));

        // when
        let new_spans = InternalSpan::split_spans(&spans, 1, 10, &hightlighted);

        // then
        assert_eq!(new_spans[0], InternalSpan::new("H", base));
        assert_eq!(new_spans[1], InternalSpan::new("el", hightlighted));
        assert_eq!(new_spans[2], InternalSpan::new("lo!", hightlighted));
    }

    #[test]
    fn test_split_spans_with_emoji() {
        // given a line with en emoji
        let base = &Style::default();
        let hightlighted = &Style::default().red();
        let spans = vec![InternalSpan::new("Hell🙂!", base)];

        // when `split_spans` is called
        let new_spans = InternalSpan::split_spans(&spans, 2, 4, &hightlighted);

        // then the span is split correctly
        assert_eq!(new_spans[0], InternalSpan::new("He", base));
        assert_eq!(new_spans[1], InternalSpan::new("ll🙂", hightlighted));
        assert_eq!(new_spans[2], InternalSpan::new("!", base));
    }

    #[test]
    fn test_internal_span_crop_spans() {
        // given a list of spans
        let base = &Style::default();
        let original_spans = vec![
            InternalSpan::new("Hel", base),
            InternalSpan::new("lo", base),
            InternalSpan::new("World!", base),
        ];
        let mut spans = original_spans.clone();

        // when `crop_spans` is called
        InternalSpan::crop_spans(&mut spans, 1);

        // then the spans are correctly cropped
        assert_eq!(spans[0], InternalSpan::new("el", base));
        assert_eq!(spans[1], InternalSpan::new("lo", base));
        assert_eq!(spans[2], InternalSpan::new("World!", base));

        // when
        let mut spans = original_spans.clone();
        InternalSpan::crop_spans(&mut spans, 2);

        // then
        assert_eq!(spans[0], InternalSpan::new("l", base));
        assert_eq!(spans[1], InternalSpan::new("lo", base));
        assert_eq!(spans[2], InternalSpan::new("World!", base));

        // when
        let mut spans = original_spans.clone();
        InternalSpan::crop_spans(&mut spans, 3);

        // then
        assert_eq!(spans[0], InternalSpan::new("lo", base));
        assert_eq!(spans[1], InternalSpan::new("World!", base));
    }

    #[test]
    fn test_internal_span_apply_selection() {
        // given a list of spans
        let base = &Style::default();
        let hightlighted = &Style::default().red();
        let spans = vec![
            InternalSpan::new("Hel", base),
            InternalSpan::new("lo!", base),
        ];

        // when `split_at_selection` is called
        let selection = Selection::new(Index2::new(0, 1), Index2::new(0, 3));
        let new_spans =
            InternalSpan::split_at_selection(&spans, 0, &selection, &hightlighted).unwrap();

        // then spans are correctly split
        assert_eq!(new_spans[0], InternalSpan::new("H", base));
        assert_eq!(new_spans[1], InternalSpan::new("el", hightlighted));
        assert_eq!(new_spans[2], InternalSpan::new("l", hightlighted));
        assert_eq!(new_spans[3], InternalSpan::new("o!", base));
    }

    #[test]
    fn test_internal_span_apply_selection_with_emoji() {
        // given a list of spans with an emoji
        let base = &Style::default();
        let hightlighted = &Style::default().red();
        let spans = vec![
            InternalSpan::new("Hell🙂", base),
            InternalSpan::new("!", base),
        ];

        // when `split_at_selection` is called
        let selection = Selection::new(Index2::new(0, 3), Index2::new(0, 5));
        let new_spans =
            InternalSpan::split_at_selection(&spans, 0, &selection, &hightlighted).unwrap();

        // then spans are correctly split
        assert_eq!(new_spans[0], InternalSpan::new("Hel", base));
        assert_eq!(new_spans[1], InternalSpan::new("l🙂", hightlighted));
        assert_eq!(new_spans[2], InternalSpan::new("!", hightlighted));
    }

}
