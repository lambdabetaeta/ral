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
//! rejection must read as English to a first-year undergraduate.  The
//! typechecker's fuzz suite already encodes that rule (`typecheck_fuzz.rs`,
//! [`JARGON_FRAGMENTS`]); we mirror the same shape here for the lexer and
//! parser:
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
//! The curated table near the bottom (`REACHABLE`) names each
//! diagnostic by an English tag and pins down a fragment we expect to see
//! — refactors that silently lose a message will fail loudly.

use ral_core::diagnostic::format_parse_error_ariadne;
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
/// user-facing message might legitimately use.  "atom" appears in
/// "expected expression atom"; that's still jargon for a beginner, but
/// the spelled-out alternative ("number, variable, or parenthesised
/// expression") is exactly the form we want — see `parse_expr_bad_atom`.
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
/// codes ariadne emits around words.  Copied in spirit from
/// `typecheck_fuzz.rs::strip_ansi` so the two suites assert against the
/// same definition of "what the user reads".
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
    let rendered = strip_ansi(&format_parse_error_ariadne("fuzz.ral", src, err));
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

/// 4096 short, fully-random ASCII inputs.  The lexer's metacharacter set
/// is mostly printable, so restricting the alphabet to printable + a few
/// control bytes exercises real code paths rather than spending most
/// iterations on UTF-8-error handling we already cover elsewhere.
#[test]
fn random_ascii_never_panics_and_messages_are_friendly() {
    let alphabet: Vec<u8> = (b' '..=b'~').chain(*b"\n\t\0").collect();
    for i in 0..4096u64 {
        let mut rng = SplitMix64::new(i);
        let len = rng.range(0, 96);
        let bytes: Vec<u8> = (0..len).map(|_| rng.pick(&alphabet)).collect();
        // Bytes are guaranteed printable ASCII (+ \n \t \0) so to_str
        // never fails — but use from_utf8_lossy to keep the fuzz robust
        // if the alphabet is expanded later.
        let src = String::from_utf8_lossy(&bytes).into_owned();
        must_not_panic_and_be_friendly(&format!("random_ascii[{i}]"), &src);
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
        "unit",
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
    ];
    for (tag, src) in cases {
        must_not_panic_and_be_friendly(tag, src);
    }
}

/// When a double-quoted string opens *and* a nested form inside it
/// (`!{…}`, `$(…)`, `$[…]`, `$name[…]`) is also left open, the user's
/// real mistake is "I never closed the string".  The lexer re-anchors so
/// the error names the outer string at its opening `"`, with the inner
/// culprit only as a hint.  This invariant is one property across four
/// inner forms, so it lives as a single test rather than four REACHABLE
/// rows.
#[test]
fn unterminated_string_with_inner_unclosed_anchors_on_outer_string() {
    let cases = &[
        ("inner_force", "\"foo !{cmd"),
        ("inner_paren", "\"foo $(cmd"),
        ("inner_arith", "\"foo $[1 + 2"),
        ("inner_index", "\"foo $name[k"),
    ];
    for (tag, src) in cases {
        let Err(err) = parse(src) else {
            panic!("{tag}: should reject {src:?}")
        };
        assert!(
            err.message.contains("unterminated double-quoted string"),
            "{tag}: rejection of {src:?} should anchor at the outer string; \
             got: {msg}",
            msg = err.message,
        );
        assert_friendly(tag, src, &err);
    }
}

/// A spread in a control-operator operand position is rejected by the
/// parser, so it never reaches the elaborator — whose `Ast::Spread` arm
/// is `unreachable!` and would otherwise panic.  Regression for the F1
/// finding of the 2026-06-11 deep review.  One representative per
/// arity-2 and arity-1 keyword; the operand index of the spread varies
/// so neither the leading nor the trailing position is left untested.
#[test]
fn spread_in_control_op_operand_is_a_parse_error_not_a_panic() {
    let cases = &[
        ("try_first", "let x = [1, 2]\ntry ...$x { |e| return unit }"),
        ("try_second", "try { return unit } ...$h"),
        ("guard_first", "guard ...$b { echo done }"),
        ("audit_only", "audit ...$b"),
    ];
    for (tag, src) in cases {
        let Err(err) = parse(src) else {
            panic!("{tag}: a spread operand should be rejected: {src:?}")
        };
        assert!(
            err.message.contains("spread"),
            "{tag}: rejection of {src:?} should name the spread; got: {msg}",
            msg = err.message,
        );
        assert_friendly(tag, src, &err);
    }
}

// ── Reachability and shape of every diagnostic ────────────────────────────
//
// Each entry pins down a real diagnostic site with:
//
// - `src`: an input that reaches it,
// - `tag`: a short label for failure messages,
// - `must_contain`: a substring the friendly message must include.
//
// The substring is chosen to be the *English* core of the message —
// "two hex digits", "reserved keyword", "after `if`" — so an
// implementation that paraphrases (but stays friendly) keeps passing,
// while one that regresses to jargon (e.g. "expected IDENT") fails.

struct Reachable {
    tag: &'static str,
    src: &'static str,
    must_contain: &'static str,
}

const fn r(tag: &'static str, src: &'static str, must_contain: &'static str) -> Reachable {
    Reachable {
        tag,
        src,
        must_contain,
    }
}

const REACHABLE: &[Reachable] = &[
    // ─── Lexer: structural unterminated forms ────────────────────────
    r("lex_unterm_single_quote", "echo 'hello", "unterminated"),
    r("lex_unterm_double_quote", "echo \"hello", "unterminated"),
    r("lex_unterm_bumped", "echo #'hello", "unterminated"),
    // `!{…}` only exists as a single form inside `"…"`; outside a
    // string the `!` and `{` lex separately, so the parser (not the
    // lexer) is what flags the unbalanced brace.  Test the
    // in-string form here.
    r("lex_unterm_brace_group", "\"foo !{ cmd", "unterminated"),
    r("lex_unterm_expr_block", "$[1+2", "unterminated"),
    r("lex_unclosed_dollar_paren", "\"$(", "unterminated"),
    // ─── Lexer: free-form errors ─────────────────────────────────────
    r("lex_empty_tag", "echo ` foo", "tag label"),
    r("lex_force_dollar_no_ident", "\"!$\"", "identifier"),
    r("lex_dollar_paren_no_ident", "echo $(123)", "identifier"),
    r("lex_dollar_paren_unclosed_inline", "echo $(name 42", "')'"),
    r("lex_redirect_amp_no_fd", "cmd >& foo", "file descriptor"),
    // ─── Lexer: escape errors ────────────────────────────────────────
    r("lex_x_too_short", "return \"\\x4\"", "two hex digits"),
    r("lex_x_non_hex", "return \"\\xZZ\"", "two hex digits"),
    r("lex_x_high_byte", "return \"\\x80\"", "ASCII"),
    r("lex_u_no_brace", "return \"\\u41\"", "\\u{"),
    r("lex_u_empty", "return \"\\u{}\"", "hex digits"),
    r("lex_u_too_long", "return \"\\u{1234567}\"", "hex digits"),
    r("lex_u_non_scalar", "return \"\\u{D800}\"", "Unicode"),
    r("lex_unknown_escape", "return \"\\z\"", "escape"),
    // ─── Parser: structural ──────────────────────────────────────────
    r("parse_let_no_eq", "let x foo", "expected '='"),
    r("parse_let_in_pipeline", "echo | let x = 1", "binding"),
    r("parse_return_too_many", "return a b", "at most one"),
    r("parse_bad_pattern", "let 42 = 1", "expected a pattern"),
    r("parse_reserved_as_pattern", "let if = 1", "reserved"),
    r("parse_rest_no_name", "let [a, ...] = $xs", "name after"),
    r("parse_rest_bad_name", "let [a, ...42] = $xs", "name"),
    r("parse_map_pattern_bad_key", "let [42: a] = m", "key"),
    r("parse_map_literal_bad_key", "[42: 1]", "key"),
    r(
        "parse_map_mix_alphabets",
        "[a: 1, `b: 2]",
        "bare and tag keys",
    ),
    r(
        "parse_record_mix_alphabets",
        "return [a: 1, `b: 2]",
        "bare and tag keys",
    ),
    r(
        "parse_lambda_empty_params",
        "{ || echo hi }",
        "at least one parameter",
    ),
    r("parse_redirect_no_command", "> out", "follow a command"),
    r("parse_caret_path", "^/abs/path", "bare command name"),
    r("parse_caret_bad", "^[1,2]", "command name"),
    r("parse_dollar_bare", "echo $", "$name"),
    r(
        "parse_caret_in_value",
        "return ^name",
        "command-head position",
    ),
    r(
        "parse_if_no_else_keyword",
        "if true { return 1 } { return 2 }",
        "else",
    ),
    r("parse_list_no_comma", "[a b c]", "',' or ']'"),
    // A control operator fills fixed operand positions, so a spread
    // in operand position is rejected at parse time rather than
    // surviving into the elaborator (which has no lowering for it).
    r(
        "parse_control_op_spread",
        "try ...$x { |e| return unit }",
        "spread",
    ),
    // Unterminated top-level `{` / `[`: the lexer tracks the open
    // delimiter and reports it at EOF (so batch scripts get a real
    // error and the REPL can prompt for continuation).  Map and list
    // literals share the `[…]` shape — one entry covers both.
    r("lex_unterm_block", "{ echo hello", "unterminated"),
    r("lex_unterm_list", "[a, b", "unterminated"),
    // ─── Parser: expression-block (Pratt) ────────────────────────────
    r("parse_expr_unexpected", "return $[$]", "unexpected"),
    r(
        "parse_expr_bad_atom",
        "return $[foo]",
        "did you mean `$foo`",
    ),
    // ─── Parser: sub-stream completion contract ──────────────────────
    // Every sub-token-stream parse goes through `Parser::run_complete`,
    // which requires EOF and names the first leftover token.  These
    // pin the three shapes that used to truncate silently (review F4,
    // F5): an expression block with extra atoms, index keys with a
    // second word, and a stray top-level `}` (which previously served
    // as a stop condition and dropped the rest of the program, and is
    // now named as an unmatched brace rather than generic trailing input).
    r(
        "parse_trailing_expr_block",
        "echo $[1 2 3]",
        "trailing input",
    ),
    r(
        "parse_trailing_index_keys",
        "let m = [a: 1]\necho $m[a b]",
        "trailing input",
    ),
    r(
        "parse_trailing_stray_rbrace",
        "echo a\n}\necho b",
        "unmatched `}`",
    ),
    // ─── Resource caps (depth limits) ────────────────────────────────
    // These two are the friendly outcome when adversarial input
    // forces unbounded recursion.  Each cap should name itself —
    // "nesting is too deep" — so a reader recognises the rejection
    // as a resource limit, not a syntax error.
    // A *balanced* deep nest: the lexer is happy (every `{` closes),
    // so the parser's recursive-descent depth cap is what fires.
    r(
        "parse_depth_cap_brace",
        "{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}}",
        "nesting is too deep",
    ),
    r(
        "lex_depth_cap_expr",
        "$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[$[",
        "nesting is too deep",
    ),
];

#[test]
fn every_diagnostic_is_reachable() {
    let mut silent: Vec<&str> = Vec::new();
    for r in REACHABLE {
        if parse(r.src).is_ok() {
            silent.push(r.tag);
        }
    }
    assert!(
        silent.is_empty(),
        "the following diagnostics no longer fire (the input now parses cleanly, \
         or the lexer changed shape) — refresh them: {silent:?}"
    );
}

#[test]
fn every_diagnostic_contains_its_english_anchor() {
    let mut drifted: Vec<(String, String)> = Vec::new();
    for r in REACHABLE {
        let Err(err) = parse(r.src) else { continue };
        let rendered = strip_ansi(&format_parse_error_ariadne("fuzz.ral", r.src, &err));
        if !err.message.contains(r.must_contain) && !rendered.contains(r.must_contain) {
            drifted.push((r.tag.to_string(), err.message.clone()));
        }
    }
    assert!(
        drifted.is_empty(),
        "messages drifted away from their English anchors: {drifted:#?}"
    );
}

#[test]
fn every_diagnostic_is_friendly() {
    for r in REACHABLE {
        let Err(err) = parse(r.src) else { continue };
        assert_friendly(r.tag, r.src, &err);
    }
}
