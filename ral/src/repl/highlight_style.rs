//! The highlight-style vocabulary and the ANSI span renderer.
//!
//! [`STYLES`] is the single source of truth for the legal style names: each
//! row names a style and gives its ANSI escape (the readline surface) and,
//! under the `structural` feature, its ratatui cell style (the structural
//! surface).  A style added to the vocabulary is given both in one place, so
//! the two surfaces cannot drift.  `_ed-highlight` validates against the same
//! table via [`style_ansi`].

use ral_core::ansi;

use super::plugin::editor::HighlightSpan;

#[cfg(feature = "structural")]
use ratatui::style::{Color, Modifier, Style};

/// One highlight style: its name, the ANSI escape the readline surface emits,
/// and — under `structural` — the foreground colour and modifier the
/// structural surface paints into a terminal cell.
struct HighlightStyle {
    name: &'static str,
    ansi: &'static str,
    #[cfg(feature = "structural")]
    fg: Option<Color>,
    #[cfg(feature = "structural")]
    modifier: Modifier,
}

/// One row per style: `name, ansi, ratatui-fg, ratatui-modifier`.  The last
/// two columns compile away when the `structural` feature is off.
macro_rules! highlight_styles {
    ($($name:literal, $ansi:expr, $fg:expr, $modifier:expr);+ $(;)?) => {
        &[$(HighlightStyle {
            name: $name,
            ansi: $ansi,
            #[cfg(feature = "structural")]
            fg: $fg,
            #[cfg(feature = "structural")]
            modifier: $modifier,
        }),+]
    };
}

const STYLES: &[HighlightStyle] = highlight_styles![
    "command",      ansi::BOLD_GREEN,    Some(Color::Green),   Modifier::BOLD;
    "builtin",      ansi::BOLD_CYAN,     Some(Color::Cyan),    Modifier::BOLD;
    "prelude",      ansi::BOLD_BLUE,     Some(Color::Blue),    Modifier::BOLD;
    "argument",     "",                  None,                 Modifier::empty();
    "option",       ansi::CYAN,          Some(Color::Cyan),    Modifier::empty();
    "path-exists",  ansi::UNDERLINE,     None,                 Modifier::UNDERLINED;
    "path-missing", ansi::UNDERLINE_RED, Some(Color::Red),     Modifier::UNDERLINED;
    "string",       ansi::YELLOW,        Some(Color::Yellow),  Modifier::empty();
    "number",       ansi::MAGENTA,       Some(Color::Magenta), Modifier::empty();
    "comment",      ansi::DIM,           None,                 Modifier::DIM;
    "error",        ansi::BOLD_RED,      Some(Color::Red),     Modifier::BOLD;
    "match",        ansi::BOLD,          None,                 Modifier::BOLD;
    "bracket-1",    ansi::CYAN,          Some(Color::Cyan),    Modifier::empty();
    "bracket-2",    ansi::MAGENTA,       Some(Color::Magenta), Modifier::empty();
    "bracket-3",    ansi::YELLOW,        Some(Color::Yellow),  Modifier::empty();
];

/// The ANSI escape for a highlight style name, or `None` if the name is not a
/// known style.  The legal style vocabulary lives in [`STYLES`].
pub(super) fn style_ansi(style: &str) -> Option<&'static str> {
    STYLES.iter().find(|s| s.name == style).map(|s| s.ansi)
}

/// The ratatui [`Style`] for a highlight style name, or `None` for an unknown
/// name — the structural surface's analogue of [`style_ansi`], painting a
/// terminal cell rather than emitting an escape.
#[cfg(feature = "structural")]
pub(super) fn style_ratatui(style: &str) -> Option<Style> {
    let hs = STYLES.iter().find(|s| s.name == style)?;
    let mut st = Style::default();
    if let Some(fg) = hs.fg {
        st = st.fg(fg);
    }
    Some(st.add_modifier(hs.modifier))
}

/// Render `line` with plugin highlight spans as an ANSI string: each span's
/// character range is painted with its style's escape, reset at every style
/// boundary.  Out-of-range and inverted spans are folded to safe ranges by
/// [`super::plugin::editor::Span`].
pub(super) fn apply_highlights(line: &str, spans: &[HighlightSpan]) -> String {
    if spans.is_empty() {
        return line.to_string();
    }

    let len = line.chars().count();
    let mut styles: Vec<Option<&str>> = vec![None; len];
    for span in spans {
        for slot in &mut styles[span.span.clamp_to(len).range()] {
            *slot = Some(span.style.as_str());
        }
    }

    let mut out = String::with_capacity(line.len() * 2);
    let mut cur: Option<&str> = None;
    for (ch, new) in line.chars().zip(styles) {
        if new != cur {
            if cur.is_some() {
                out.push_str(ansi::RESET);
            }
            if let Some(s) = new {
                out.push_str(style_ansi(s).unwrap_or(""));
            }
            cur = new;
        }
        out.push(ch);
    }
    if cur.is_some() {
        out.push_str(ansi::RESET);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::plugin::editor::Span;

    // `Span::clamped` orders its endpoints, so an inverted `(start, end)`
    // submitted by a plugin folds to the same range as the ordered pair and
    // can never produce a backwards slice in `apply_highlights`.

    #[test]
    fn apply_highlights_inverted_span_folds_to_ordered() {
        let line = "hello world";
        let bound = line.chars().count();
        let inverted = HighlightSpan {
            span: Span::clamped(5, 2, bound),
            style: "command".into(),
        };
        let ordered = HighlightSpan {
            span: Span::clamped(2, 5, bound),
            style: "command".into(),
        };
        assert_eq!(
            apply_highlights(line, &[inverted]),
            apply_highlights(line, &[ordered]),
        );
    }

    #[test]
    fn apply_highlights_span_reclamps_to_shorter_line() {
        // A span minted against a longer buffer must not panic when the line
        // rendered at the slice site has since shrunk.
        let span = HighlightSpan {
            span: Span::clamped(0, 10, 10),
            style: "command".into(),
        };
        let _ = apply_highlights("hi", &[span]);
    }
}
