use super::*;

fn call(intent: &str, magnitude: Option<u32>) -> Call {
    let mut call = Call::open(Seq::new(1), intent.into(), String::new(), 0);
    if let Some(lines) = magnitude {
        call.settle(Verdict {
            lines,
            failed: false,
        });
    }
    call
}

/// The script paints a left-inset panel: [`BODY_INDENT`] stays unwashed and
/// every row is washed to the full width — no ragged right edge.
#[test]
fn source_rows_paint_an_inset_panel() {
    let c = Call::open(Seq::new(1), "x".into(), "let x = 1\nlet y = 2".into(), 0);
    let rows = source_rows(&c, 60);
    assert_eq!(rows.len(), 2);
    for r in &rows {
        let w: usize = r.spans.iter().map(ratatui::prelude::Span::width).sum();
        assert_eq!(w, 60, "panel row padded to full width");
        assert!(
            r.spans.iter().any(|s| s.style.bg == Some(CODE_BG)),
            "the row is washed"
        );
        assert!(
            r.spans
                .iter()
                .filter(|s| s.style.bg.is_some())
                .all(|s| s.style.bg == Some(CODE_BG)),
            "washed cells wear CODE_BG"
        );
    }
}

fn plain(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn nonblank(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| !line::is_blank(l))
        .map(plain)
        .collect()
}

#[test]
fn live_tip_anchors_on_latest_settled_call_not_a_pending_one() {
    let width = 100;
    let calls = vec![call("settled read", Some(7)), call("pending grep", None)];
    let rows = nonblank(&body(&calls, Detail::Summary, width));

    let head = &rows[0];
    assert!(
        head.contains("settled read"),
        "tip narrates the settled call"
    );
    assert!(!head.contains("pending grep"), "not the in-flight call");
    // The pending call still counts toward the sparkline, as its shortest bar.
    assert!(head.ends_with(line::spark_glyph(None)));
}
