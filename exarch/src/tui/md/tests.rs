use super::*;

#[allow(
    clippy::suboptimal_flops,
    reason = "u8-rounded colour math; mul_add adds no precision and obscures the standard lerp/luma formula"
)]
fn luma(c: Color) -> f32 {
    let Color::Rgb(r, g, b) = c else {
        unreachable!("test colours are RGB")
    };
    0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)
}

/// Every non-blank span of a rendered block — the ink modulation acts on.
fn ink<'a>(lines: &'a [Line<'static>]) -> Vec<&'a Span<'static>> {
    lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .filter(|s| !s.content.trim().is_empty())
        .collect()
}

/// Rendered text of a block, one string per row.
fn rows(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

/// A display formula is typeset, not quoted: numerator over vinculum over
/// denominator, each row its own line.
#[test]
fn display_math_stacks() {
    let rows = rows(&render_md(
        "$$\\frac{x^2 + 1}{y}$$",
        80,
        MD_INDENT,
        Fidelity::default(),
    ));
    let body: Vec<&String> = rows.iter().filter(|r| !r.trim().is_empty()).collect();
    assert_eq!(body.len(), 3, "numerator, vinculum, denominator: {rows:?}");
    assert!(body[0].contains("x²"), "superscript typeset: {body:?}");
    assert!(body[1].contains('─'), "a vinculum divides them: {body:?}");
}

/// Inline notation joins the sentence it belongs to, on one row.
#[test]
fn inline_math_joins_the_prose() {
    let rows = rows(&render_md(
        "the bound $x^2 + y_1$ holds",
        80,
        MD_INDENT,
        Fidelity::default(),
    ));
    assert_eq!(rows.len(), 1, "one row: {rows:?}");
    assert!(
        rows[0].contains("the bound x² + y₁ holds"),
        "typeset in place: {rows:?}"
    );
}

/// A formula that cannot be flattened keeps its source, inked as the literal
/// it is — a stacked fraction would have to break the paragraph to draw.
#[test]
fn unflattenable_inline_math_keeps_its_source() {
    let lines = render_md(
        "the ratio $\\frac{a}{b}$ holds",
        80,
        MD_INDENT,
        Fidelity::default(),
    );
    let rows = rows(&lines);
    assert_eq!(rows.len(), 1, "the paragraph is intact: {rows:?}");
    assert!(
        rows[0].contains("\\frac{a}{b}"),
        "source stands in: {rows:?}"
    );
    let latex = ink(&lines)
        .into_iter()
        .find(|s| s.content.contains("frac"))
        .expect("the source is inked");
    assert_eq!(latex.style.fg, Some(LIME), "inked as a literal");
}

/// A table cell is one row, so a formula in one obeys the inline rule.
#[test]
fn table_cell_typesets_math() {
    let rows = rows(&render_md(
        "| bound |\n| --- |\n| $x^2$ |\n",
        80,
        MD_INDENT,
        Fidelity::default(),
    ));
    assert!(
        rows.iter().any(|r| r.contains("x²")),
        "the cell is typeset, not dropped: {rows:?}"
    );
}

/// The frame is pinned: one space of padding either side of the widest
/// cell in each column, and a rule that spans exactly that padded width.
#[test]
fn table_frame_is_drawn_exactly() {
    let rows = rows(&render_md(
        "| a | bb |\n| --- | --- |\n| ccc | d |\n",
        80,
        MD_INDENT,
        Fidelity::default(),
    ));
    let frame: Vec<&str> = rows
        .iter()
        .map(|r| r.trim_start())
        .filter(|r| !r.is_empty())
        .collect();
    assert_eq!(
        frame,
        ["│ a   │ bb │", "├─────┼────┤", "│ ccc │ d  │"],
        "the drawn frame: {rows:?}"
    );
}

/// A span wider than the budget breaks between characters.  Wrapping only
/// *before* it would run the tail off the row, where the terminal clips it.
#[test]
fn over_wide_span_keeps_its_tail() {
    let src = format!("prose `{}` more", "a".repeat(200));
    let rows = rows(&render_md(&src, 40, MD_INDENT, Fidelity::default()));
    for r in &rows {
        assert!(
            UnicodeWidthStr::width(r.as_str()) <= 40,
            "no row overruns the terminal: {r:?}"
        );
    }
    let kept: usize = rows.iter().map(|r| r.matches('a').count()).sum();
    assert_eq!(kept, 200, "every character survives: {rows:?}");
}

/// A styled span and the punctuation fused to it are one word: the fold
/// falls before the pair, never at the seam, so no row opens on a lone `,`.
#[test]
fn fused_punctuation_breaks_as_one_word() {
    let rows = rows(&render_md(
        "alpha beta `gamma`, delta",
        MD_INDENT + 14,
        MD_INDENT,
        Fidelity::default(),
    ));
    assert!(rows.len() > 1, "expected a fold: {rows:?}");
    assert!(
        rows.iter().any(|r| r.contains("gamma,")),
        "the span and its punctuation stayed whole: {rows:?}"
    );
    for r in &rows {
        assert!(
            !r.trim_start().starts_with(','),
            "punctuation orphaned onto its own row: {rows:?}"
        );
    }
}

/// A sound block is left alone: no wash, no carried-over `DIM`.
#[test]
fn sound_prose_is_untouched() {
    let lines = render_md("plain prose here", 80, MD_INDENT, Fidelity::default());
    for span in ink(&lines) {
        assert!(span.style.bg.is_none(), "sound prose wears no wash");
        assert!(!span.style.add_modifier.contains(Modifier::DIM));
    }
}

/// The drain holds luminance and adds no `DIM` — that idiom is for minor chrome.
#[test]
fn context_drains_without_dim() {
    let lines = render_md(
        "plain prose here",
        80,
        MD_INDENT,
        Fidelity {
            context: 2,
            echo: 0,
        },
    );
    let spans = ink(&lines);
    assert_ne!(spans, Vec::<&Span<'_>>::new());
    for span in spans {
        let fg = span.style.fg.expect("drained span carries an explicit fg");
        assert!(
            (luma(fg) - luma(BASE_FG)).abs() <= 1.0,
            "drain held luminance"
        );
        assert_ne!(fg, BASE_FG, "drain desaturated the ink");
        assert!(
            !span.style.add_modifier.contains(Modifier::DIM),
            "drain must not borrow the minor-chrome DIM idiom"
        );
    }
}

/// One wash on every row — a flag, not a glitch — and the foreground untouched.
#[test]
fn echo_washes_background_statically() {
    let lines = render_md(
        "first line is long enough to wrap onto a second rendered row here please",
        40,
        MD_INDENT,
        Fidelity {
            context: 0,
            echo: 2,
        },
    );
    let spans = ink(&lines);
    assert!(
        spans.len() >= 2,
        "needs multiple rows to test row-invariance"
    );
    let washes: Vec<Color> = spans
        .iter()
        .map(|s| s.style.bg.expect("echoed span carries a wash"))
        .collect();
    let first = washes[0];
    assert!(
        washes.iter().all(|&w| w == first),
        "wash is static across rows"
    );
    assert_eq!(
        first,
        mix(Color::Rgb(0, 0, 0), ECHO_WASH, 1.0),
        "wash is the full echo shade"
    );
    for s in &spans {
        assert_eq!(s.style.fg, None, "echo leaves the foreground untouched");
    }
}
