//! The highlight-style vocabulary and the ANSI span renderer.
//!
//! [`STYLES`] is the single source of truth for the legal style names: each
//! row names a style and gives its ANSI escape.  `_ed-highlight` validates
//! against the same table via [`style_ansi`].

use ral_core::ansi;

use super::plugin::editor::HighlightSpan;

/// One highlight style: its name and the ANSI escape the readline surface emits.
struct HighlightStyle {
    name: &'static str,
    ansi: &'static str,
}

const STYLES: &[HighlightStyle] = &[
    HighlightStyle {
        name: "command",
        ansi: ansi::BOLD_GREEN,
    },
    HighlightStyle {
        name: "builtin",
        ansi: ansi::BOLD_CYAN,
    },
    HighlightStyle {
        name: "prelude",
        ansi: ansi::BOLD_BLUE,
    },
    HighlightStyle {
        name: "argument",
        ansi: "",
    },
    HighlightStyle {
        name: "option",
        ansi: ansi::CYAN,
    },
    HighlightStyle {
        name: "path-exists",
        ansi: ansi::UNDERLINE,
    },
    HighlightStyle {
        name: "path-missing",
        ansi: ansi::UNDERLINE_RED,
    },
    HighlightStyle {
        name: "string",
        ansi: ansi::YELLOW,
    },
    HighlightStyle {
        name: "number",
        ansi: ansi::MAGENTA,
    },
    HighlightStyle {
        name: "comment",
        ansi: ansi::DIM,
    },
    HighlightStyle {
        name: "error",
        ansi: ansi::BOLD_RED,
    },
    HighlightStyle {
        name: "match",
        ansi: ansi::BOLD,
    },
    HighlightStyle {
        name: "bracket-1",
        ansi: ansi::CYAN,
    },
    HighlightStyle {
        name: "bracket-2",
        ansi: ansi::MAGENTA,
    },
    HighlightStyle {
        name: "bracket-3",
        ansi: ansi::YELLOW,
    },
];

/// The ANSI escape for a highlight style name, or `None` if the name is not a
/// known style.  The legal style vocabulary lives in [`STYLES`].
pub(super) fn style_ansi(style: &str) -> Option<&'static str> {
    STYLES.iter().find(|s| s.name == style).map(|s| s.ansi)
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
