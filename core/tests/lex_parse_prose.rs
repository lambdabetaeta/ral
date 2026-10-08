//! Lexer / parser fuzz tests focused on diagnostic prose quality.
//!
//! There are two jobs here.  First, neither the lexer nor the parser may
//! panic on any input — we throw both grammar-shaped noise and pure random
//! bytes at `parse()` and require an `Ok` or an `Err`, never an unwind.
//! That contract is shared with the older hand-curated suite in
//! `core/tests/eval_fuzz.rs`; the random side here is its broader, less
//! ergonomic complement.
//!
//! Second — and this is the bar the user actually cares about — every
//! rejection must read as English to a first-year undergraduate:
//!
//! - No Rust `Debug` dumps (variant names, struct headers, panic strings)
//!   in the user-facing message.
//! - No internal compiler vocabulary the reader hasn't been introduced to
//!   ("deref", "IDENT" as an abbreviation, debug-only enum prefixes).
//! - Every error carries a real source span (recovered to line/col at
//!   render time by the ariadne layer).
//! - The rendered ariadne report stays jargon-free under the same scan
//!   (this is what the human actually sees on stderr).
//!
//! The catalogue of diagnostics — one program per message, each pinning an
//! English fragment — is the `tests/reject/parse-*.ral` corpus, which
//! `ral/tests/corpus.rs` runs under the same jargon scan.

use ral_core::source::Source;
use ral_core::syntax::parser::ParseError;
use ral_core::syntax::parser::parse;

// ── Prose policy ──────────────────────────────────────────────────────────

/// Internal vocabulary the user-facing message must not contain.
///
/// The first group is Rust-internal noise: anything that suggests the
/// renderer accidentally serialised a `Debug` dump rather than a message
/// we wrote on purpose.  The second group is *language*-internal: terms
/// the implementation uses to talk about itself that a first-year would
/// not have met.
///
/// These are matched as plain substrings — no regex — so each entry must
/// be specific enough that it doesn't false-positive on a sentence a
/// user-facing message might legitimately use.  "atom" is jargon for a
/// beginner; a message names the shapes instead ("an operand on each
/// side").
const JARGON_FRAGMENTS: &[&str] = &[
    // Rust-internal: structural give-aways of an unintended Debug print.
    "ParseError {",
    "LexError {",
    "LexErrorKind::",
    "Token::",
    "Word::",
    "StringPart::",
    "Ast::",
    "called `Option::unwrap",
    "panicked at",
    // Language-internal: terms we don't introduce to first-year readers.
    // "deref" / "dereference" — the user knows `$name`; the compiler
    // calls that a "dereference" internally.  The English message
    // should name the surface form.
    "deref form",
    "deref in expression",
    // "IDENT" — uppercase abbreviation.  The friendly word is
    // "identifier" or "name".  We allow "identifier" through.
    "IDENT)",
    " IDENT ",
    "an IDENT",
];

/// Heuristic: a message that *only* renders a Rust enum debug.  Catches
/// regressions where someone wires `format!("{:?}", err)` instead of
/// `err.to_string()`.
fn looks_like_debug_dump(msg: &str) -> bool {
    // The Debug impls in this crate print as `Variant(...)` with the
    // variant name CamelCased — distinctive enough that we can flag
    // structural shapes without false-positives on prose.
    msg.contains("Variant(") || msg.starts_with("LexError {") || msg.starts_with("ParseError {")
}

/// Strip ANSI escape sequences so the jargon scan doesn't trip on colour
/// codes ariadne emits around words.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && matches!(chars.peek(), Some(&'[')) {
            chars.next();
            while let Some(&c) = chars.peek() {
                chars.next();
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The report `err` draws against `src`, as stderr shows it.
fn rendered_report(src: &str, err: &ParseError) -> String {
    let source = Source::from_text("fuzz.ral", src);
    err.report(&source).render(&source)
}

/// Assert that `err`'s user-facing surfaces are intelligible:
///
/// - the bare message has a position and no jargon;
/// - the rendered ariadne report (which is what stderr shows) has no
///   jargon either;
/// - neither form looks like an accidental Debug dump.
///
/// `tag` is a short identifier for the input — included in every panic
/// message so a failing assertion names the offending scenario.
fn assert_friendly(tag: &str, src: &str, err: &ParseError) {
    let msg = &err.message;
    assert!(
        !msg.is_empty(),
        "{tag}: empty error message for input {src:?}"
    );
    assert!(
        err.span.is_some(),
        "{tag}: error for {src:?} has no source span: msg={msg:?}"
    );
    for bad in JARGON_FRAGMENTS {
        assert!(
            !msg.contains(bad),
            "{tag}: error message contains jargon {bad:?}\n  input: {src:?}\n  message: {msg}"
        );
    }
    assert!(
        !looks_like_debug_dump(msg),
        "{tag}: error message looks like a Rust Debug dump\n  input: {src:?}\n  message: {msg}"
    );
    let rendered = strip_ansi(&rendered_report(src, err));
    for bad in JARGON_FRAGMENTS {
        assert!(
            !rendered.contains(bad),
            "{tag}: rendered report contains jargon {bad:?}\n  input: {src:?}\n  rendered:\n{rendered}"
        );
    }
}

/// The parser must not panic on `src`, and if it rejects, the error must
/// pass `assert_friendly`.  This is the property we check on every fuzz
/// iteration.
fn must_not_panic_and_be_friendly(tag: &str, src: &str) {
    match parse(src) {
        Ok(_) => {}
        Err(e) => assert_friendly(tag, src, &e),
    }
}

// ── Deterministic PRNG ────────────────────────────────────────────────────
//
// We avoid pulling in `proptest` / `rand` for this suite — splitmix64 is
// short, drop-in, and deterministic.  Seeds are derived from the
// iteration index so a failing case reproduces with no extra ceremony.

struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "PRNG output reduced mod len; any truncation still yields a valid in-range index"
        )]
        let i = (self.next() as usize) % xs.len();
        xs[i]
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "PRNG output reduced mod len; any truncation still yields a valid in-range index"
        )]
        {
            lo + (self.next() as usize) % (hi - lo)
        }
    }
}

// ── Random-byte fuzz ──────────────────────────────────────────────────────

/// 4096 short, fully-random inputs.  The lexer's metacharacter set is mostly
/// printable ASCII, so that dominates the alphabet; the few non-ASCII and
/// control code points exercise the bidi and control-character refusals.
#[test]
fn random_chars_never_panics_and_messages_are_friendly() {
    let alphabet: Vec<char> = (' '..='~')
        .chain([
            '\n',
            '\t',
            '\r',
            '\0',
            '\u{1}',
            '\u{7F}',
            '\u{202E}',
            '\u{2066}',
            'é',
            '日',
            '\u{1F600}',
        ])
        .collect();
    for i in 0..4096u64 {
        let mut rng = SplitMix64::new(i);
        let len = rng.range(0, 96);
        let src: String = (0..len).map(|_| rng.pick(&alphabet)).collect();
        must_not_panic_and_be_friendly(&format!("random_chars[{i}]"), &src);
    }
}

/// 4096 inputs drawn from a small alphabet of *language tokens* —
/// keywords, operators, brackets, escape sequences.  This biases the
/// fuzzer toward inputs that get past the first few characters of the
/// lexer rather than spending most iterations on pure character noise.
#[test]
fn random_token_soup_never_panics_and_messages_are_friendly() {
    // Each entry is a snippet the lexer can chew on.  Mixing structural
    // tokens (brackets, `?`, `|`) with full keywords ("let", "case")
    // and partial constructs ("$[", "!{", "\"") produces inputs that
    // reach the parser more often than a pure-character alphabet would.
    let tokens: &[&str] = &[
        // Keywords and reserved heads.
        "let",
        "return",
        "if",
        "elsif",
        "else",
        "case",
        "true",
        "false",
        "try",
        "guard",
        "within",
        "grant",
        "audit",
        // Identifier-shaped fragments.
        "x",
        "foo",
        "bar123",
        "_",
        "name-with-dash",
        // Operators and punctuation.
        "=",
        "==",
        "!=",
        "<",
        ">",
        "<=",
        ">=",
        "+",
        "-",
        "*",
        "/",
        "%",
        "&&",
        "||",
        "not",
        // Brackets and grouping.
        "{",
        "}",
        "[",
        "]",
        "(",
        ")",
        "()",
        // Pipeline and chain.
        "|",
        "?",
        "&",
        ";",
        // Strings (opens and partials).
        "\"",
        "'",
        "\"foo\"",
        "'foo'",
        "#'",
        "##'",
        "'#",
        "''",
        // Interpolation / dereference / expression block.
        "$",
        "$x",
        "$(name)",
        "$[1+2]",
        "!{cmd}",
        "\"$x\"",
        "\"!{x}\"",
        // Escapes.
        "\\n",
        "\\t",
        "\\x41",
        "\\u{41}",
        "\\z",
        "\\x",
        "\\u{}",
        // Redirects.
        ">",
        "<",
        ">>",
        "2>",
        ">&",
        "2>&1",
        ">~",
        // Spreads, commas, colons.
        "...",
        ",",
        ":",
        "::",
        // Tags.
        "`ok",
        "`err",
        // Whitespace.
        " ",
        "\n",
        "\t",
    ];
    for i in 0..4096u64 {
        let mut rng = SplitMix64::new(0xfeed_face ^ i);
        let n = rng.range(1, 24);
        let mut src = String::new();
        for _ in 0..n {
            src.push_str(rng.pick(tokens));
        }
        must_not_panic_and_be_friendly(&format!("token_soup[{i}]"), &src);
    }
}

/// Pathological structural inputs: very deep nesting, repeated openers
/// without closers, mismatched pairs.  Stresses the lexer's
/// delim-stack bookkeeping and the parser's recovery on EOF.
#[test]
fn pathological_structural_inputs() {
    // Assignment lookahead: `[a, b, c, ...] = [...]` could be a pattern
    // or a list; the parser commits one way or the other.  100-element
    // sides stress the lookahead without crossing the depth cap.
    let pattern_backtrack = format!(
        "let [{}] = [{}]",
        (0..100)
            .map(|i| format!("x{i}"))
            .collect::<Vec<_>>()
            .join(", "),
        (0..100)
            .map(|i| format!("{i}"))
            .collect::<Vec<_>>()
            .join(", "),
    );
    let cases: &[(&str, String)] = &[
        ("deep_braces_unclosed", "{".repeat(200)),
        ("deep_brackets_unclosed", "[".repeat(200)),
        ("deep_parens_unclosed", format!("$[{}", "(".repeat(200))),
        ("close_only", "}".repeat(200)),
        ("alternating_open", "{[".repeat(100)),
        ("alternating_close", "]}".repeat(100)),
        ("force_chain_unclosed", "!{".repeat(50)),
        ("string_then_force_chain", format!("\"{}", "!{".repeat(50))),
        ("expr_block_chain_unclosed", "$[".repeat(50)),
        ("dollar_paren_chain_unclosed", "$(".repeat(50)),
        ("hash_run_no_quote", "#".repeat(200)),
        (
            "bumped_open_no_close",
            "#####'".to_string() + &"body".repeat(50),
        ),
        ("escape_storm", "\"".to_string() + &r"\".repeat(200)),
        ("interleaved_tokens", "$ | ? & < > ".repeat(50)),
        ("pattern_vs_list_backtrack_100", pattern_backtrack),
        (
            "case_arm_lambda_nesting",
            "case $x [`a: {|p| ".repeat(200) + "1" + &"}]".repeat(200),
        ),
    ];
    for (tag, src) in cases {
        must_not_panic_and_be_friendly(tag, src);
    }
}

/// A nested form inside a double-quoted string (`$[…]`, `$name[…]`) is
/// lexed once, by the outer lexer, and its tokens are handed to the
/// sub-parser as they are.  So a diagnostic raised deep inside one still
/// underlines the outer file at the right column.  Re-lexing an inner body
/// from a substring would restart every span at 0 and point every
/// in-string error at column 1.
#[test]
fn nested_stream_error_spans_point_into_the_outer_source() {
    let cases = &[
        (
            "expr_in_string",
            "echo \"aaa !{return $[1 2]} bbb\"",
            "expected an operator",
            "2",
            ":1:24",
        ),
        (
            "index_in_string",
            "let m = [a: 1]\necho \"xx $m[a b] yy\"",
            "expected ]",
            "b",
            ":2:15",
        ),
    ];
    for (tag, src, anchor, offender, position) in cases {
        let Err(err) = parse(src) else {
            panic!("{tag}: should reject {src:?}")
        };
        assert!(
            err.message.contains(anchor),
            "{tag}: rejection of {src:?} should say {anchor:?}; got: {msg}",
            msg = err.message,
        );
        let span = err.span.expect("nested error must carry a span");
        assert_eq!(
            &src[span.start as usize..span.end as usize],
            *offender,
            "{tag}: span should cover the offending token in the outer source"
        );
        let rendered = strip_ansi(&rendered_report(src, &err));
        assert!(
            rendered.contains(position),
            "{tag}: report should point at {position}; rendered:\n{rendered}"
        );
        assert_friendly(tag, src, &err);
    }
}
