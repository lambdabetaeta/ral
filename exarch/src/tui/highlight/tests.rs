use super::*;

/// A line's text, styling dropped.
fn text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The fg of the span whose text is exactly `needle`, in a one-line source.
fn fg_of(src: &str, needle: &str) -> Option<Color> {
    let lines = highlight_ral(src);
    assert_eq!(lines.len(), 1, "expected one line for {src:?}");
    lines[0]
        .spans
        .iter()
        .find(|s| s.content.as_ref() == needle)
        .unwrap_or_else(|| panic!("no span exactly {needle:?} in {src:?}"))
        .style
        .fg
}

#[test]
fn classifies_keyword_string_and_name() {
    let src = "let x = 'hi'";
    assert_eq!(fg_of(src, "let"), Some(CODE_KEYWORD));
    assert_eq!(fg_of(src, "'hi'"), Some(CODE_STRING));
    // `x` is a bound name, not a keyword.
    assert_eq!(fg_of(src, "x"), Some(Color::White));
}

#[test]
fn classifies_deref_tag_and_punctuation() {
    let src = "[`ok $x]";
    assert_eq!(fg_of(src, "["), Some(SLATE));
    assert_eq!(fg_of(src, "`ok"), Some(CODE_TAG));
    assert_eq!(fg_of(src, "$x"), Some(CODE_VARIABLE));
    assert_eq!(fg_of(src, "]"), Some(SLATE));
}

#[test]
fn invalid_code_falls_back_to_default_ink() {
    let lines = highlight_ral("let x = 'unterminated");
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0]
            .spans
            .iter()
            .all(|s| s.style.fg == Some(Color::White)),
        "fallback colours nothing"
    );
    assert_eq!(text(&lines[0]), "let x = 'unterminated");
}

#[test]
fn reassembled_text_equals_input() {
    for src in [
        "let x = 'hi'",
        "ls | filter { |f| $f }",
        "map [1, 2, 3] { |n| $n }\nlet y = `tag",
        "echo a # trailing comment",
    ] {
        let joined: Vec<String> = highlight_ral(src).iter().map(text).collect();
        assert_eq!(joined.join("\n"), src, "round-trip differs for {src:?}");
    }
}
