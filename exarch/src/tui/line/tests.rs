use super::*;

/// Five three-cell spans, `[a]` to `[e]`, windowed to `width` around `at`.
fn windowed(at: usize, width: usize) -> String {
    let spans: Vec<_> = ["[a]", "[b]", "[c]", "[d]", "[e]"].map(Span::raw).into();
    window_around(&spans, at, width, Style::default())
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

#[test]
fn a_run_that_fits_is_returned_unchanged() {
    assert_eq!(windowed(2, 15), "[a][b][c][d][e]");
}

#[test]
fn at_the_left_end_only_the_right_is_cut() {
    assert_eq!(windowed(0, 11), "[a][b] …");
}

#[test]
fn at_the_right_end_only_the_left_is_cut() {
    assert_eq!(windowed(4, 11), "… [d][e]");
}

#[test]
fn in_the_middle_both_sides_are_cut() {
    assert_eq!(windowed(2, 11), "… [b][c] …");
}

#[test]
fn the_active_span_survives_a_width_too_small_for_it() {
    assert_eq!(windowed(2, 2), "… [c] …");
}
