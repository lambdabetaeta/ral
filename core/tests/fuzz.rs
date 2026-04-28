//! Parser fuzz tests: throw hostile, malformed, edge-case, and adversarial
//! inputs at the parser. It must never panic — only return Ok or Err.

use ral_core::parse;

/// The parser must not panic on any input. It may return Ok (valid parse)
/// or Err (syntax error), but never crash.
fn must_not_panic(input: &str) {
    let _ = parse(input);
}

/// The parser must accept this input without error.
fn must_parse(input: &str) {
    parse(input).unwrap_or_else(|e| panic!("should parse: {input:?}\n  error: {e}"));
}

/// The parser must reject this input with an error.
fn must_reject(input: &str) {
    if parse(input).is_ok() {
        panic!("should reject: {input:?}");
    }
}

// ── Empty and whitespace ─────────────────────────────────────────────────

#[test]
fn empty() {
    must_parse("");
}

#[test]
fn whitespace_only() {
    must_parse("   \t  \n  ");
}

#[test]
fn just_newlines() {
    must_parse("\n\n\n");
}

#[test]
fn just_semicolons() {
    must_parse(";;;");
}

#[test]
fn just_comments() {
    must_parse("# hello\n# world\n");
}

// ── Unterminated constructs ──────────────────────────────────────────────

#[test]
fn unterminated_single_quote() {
    must_reject("echo 'hello");
}

#[test]
fn unterminated_double_quote() {
    must_reject("echo \"hello");
}

#[test]
fn unterminated_bumped_string_level0() {
    must_reject("echo 'unclosed");
}

#[test]
fn unterminated_bumped_string_level1() {
    // Has a `'` in body but no `'#` to close.
    must_reject("echo #'body with ' but no hash");
}

#[test]
fn unterminated_bumped_string_close_too_thin() {
    // Level 2 open, only level 1 close present.
    must_reject("echo ##'body'#");
}

#[test]
fn bumped_string_round_trip() {
    must_parse(r#"let p = "print('hello', 'world')""#);
    must_parse(r##"let p = #'say "hi" to 'alice''#"##);
    must_parse("let p = ##'body '# hash'##");
}

#[test]
fn unterminated_block() {
    must_reject("{ echo hello");
}

#[test]
fn unterminated_list() {
    must_reject("[a, b");
}

#[test]
fn unterminated_map() {
    must_reject("[a: 1, b: 2");
}

#[test]
fn unterminated_paren() {
    must_reject("$[(1 + 2");
}

#[test]
fn unterminated_substitution() {
    must_reject("!{echo hello");
}

#[test]
fn unterminated_arithmetic() {
    must_reject("$[1 + ");
}

#[test]
fn unterminated_lambda_params() {
    must_reject("|x y");
}

#[test]
fn unterminated_lambda_no_body() {
    must_reject("|x|");
}

// ── Mismatched delimiters ────────────────────────────────────────────────

#[test]
fn close_brace_without_open() {
    must_not_panic("}");
}

#[test]
fn close_bracket_without_open() {
    must_not_panic("]");
}

#[test]
fn close_paren_without_open() {
    must_not_panic(")");
}

#[test]
fn brace_bracket_mismatch() {
    must_not_panic("{ ]");
}

#[test]
fn bracket_brace_mismatch() {
    must_not_panic("[ }");
}

// ── Deeply nested ────────────────────────────────────────────────────────

#[test]
fn deep_blocks() {
    let input = "{".repeat(50) + &"}".repeat(50);
    must_not_panic(&input);
}

#[test]
fn deep_lists() {
    let input = "[".repeat(50) + &"]".repeat(50);
    must_not_panic(&input);
}

#[test]
fn deep_arithmetic() {
    let input = format!("$[{}1{}]", "(".repeat(50), ")".repeat(50));
    must_not_panic(&input);
}

#[test]
fn deep_substitution() {
    // !{!{!{echo hello}}}
    let input = "!{".repeat(20) + "echo hello" + &"}".repeat(20);
    must_not_panic(&input);
}

// ── Operator edge cases ──────────────────────────────────────────────────

#[test]
fn bare_pipe() {
    must_not_panic("|");
}

#[test]
fn bare_question() {
    must_not_panic("?");
}

#[test]
fn bare_equals() {
    must_not_panic("=");
} // standalone = is Equals token, not a bare word

#[test]
fn bare_colon() {
    must_not_panic(":");
}

#[test]
fn bare_spread() {
    must_not_panic("...");
}

#[test]
fn bare_dollar() {
    must_reject("$");
}

#[test]
fn bare_tilde() {
    must_parse("~");
}

#[test]
fn double_pipe() {
    must_not_panic("||");
}

#[test]
fn double_question() {
    must_not_panic("? ?");
}

#[test]
fn pipe_then_eof() {
    must_not_panic("echo |");
}

#[test]
fn question_then_eof() {
    must_not_panic("echo ?");
}

#[test]
fn only_redirects() {
    must_not_panic("> out.txt");
}

#[test]
fn redirect_no_target() {
    must_not_panic("echo >");
}

#[test]
fn redirect_chain() {
    must_not_panic("> > >");
}

// ── String edge cases ────────────────────────────────────────────────────

#[test]
fn empty_single_quote() {
    must_parse("return ''");
}

#[test]
fn empty_double_quote() {
    must_parse("return \"\"");
}

#[test]
fn single_quote_hash_bumped() {
    must_parse("return #'it's'#");
}

#[test]
fn double_quote_all_escapes() {
    must_parse("return \"\\n\\t\\\\\0\\\"\\$\"");
}

#[test]
fn double_quote_numeric_escapes() {
    must_parse("return \"\\x41\"");     // \x41 = 'A'
    must_parse("return \"\\x00\"");     // \x00 = NUL
    must_parse("return \"\\u{41}\"");   // U+0041 = 'A'
    must_parse("return \"\\u{1F600}\""); // emoji
}

#[test]
fn double_quote_bad_escape() {
    must_reject("return \"\\z\"");
}

#[test]
fn double_quote_x_escape_too_high() {
    must_reject("return \"\\x80\"");
}

#[test]
fn double_quote_x_escape_bad_digits() {
    must_reject("return \"\\xZZ\"");
    must_reject("return \"\\x4\"");
}

#[test]
fn double_quote_u_escape_surrogate() {
    must_reject("return \"\\u{D800}\"");
}

#[test]
fn double_quote_u_escape_out_of_range() {
    must_reject("return \"\\u{110000}\"");
}

#[test]
fn double_quote_u_escape_empty_braces() {
    must_reject("return \"\\u{}\"");
}

#[test]
fn double_quote_u_escape_no_braces() {
    must_reject("return \"\\u41\"");
}

#[test]
fn double_quote_u_escape_too_long() {
    must_reject("return \"\\u{1234567}\"");
}

#[test]
fn multiline_single_quote() {
    must_parse("return 'line one\nline two'");
}

#[test]
fn multiline_double_quote() {
    must_parse("return \"line one\nline two\"");
}

#[test]
fn dollar_in_single_quote() {
    must_parse("return '$not_interpolated'");
}

#[test]
fn interpolation_edge() {
    must_parse("return \"$\"");
} // lone $ in string

#[test]
fn nested_interpolation() {
    must_parse("return \"!{echo inner}\"");
}

// ── Assignment edge cases ────────────────────────────────────────────────

#[test]
fn assign_to_number() {
    must_not_panic("42 = x");
} // not valid pattern

#[test]
fn assign_empty_value() {
    must_not_panic("x =");
}

#[test]
fn destructure_empty() {
    must_parse("let [] = []");
}

#[test]
fn destructure_nested() {
    must_parse("[a, [b, c]] = $x");
}

#[test]
fn assign_with_equals_in_value() {
    must_parse("let x = -DFOO=bar");
}

// ── Lambda edge cases ────────────────────────────────────────────────────

#[test]
fn single_param_lambda() {
    must_parse("{ |x| echo hello }");
}

#[test]
fn lambda_many_params() {
    must_parse("{ |a b c d e f| echo $a }");
}

#[test]
fn lambda_two_params() {
    must_parse("{ |x y| echo $x }");
}

#[test]
fn lambda_destructuring_param() {
    must_parse("{ |[a, b]| echo $a }");
}

#[test]
fn lambda_map_destructure_param() {
    must_parse("{ |[host: h, port: p]| echo $h }");
}

// ── Collection edge cases ────────────────────────────────────────────────

#[test]
fn empty_list() {
    must_parse("[]");
}

#[test]
fn empty_map() {
    must_parse("[:]");
}

#[test]
fn trailing_comma_list() {
    must_parse("return [a, b, c,]");
}

#[test]
fn trailing_comma_map() {
    must_parse("return [a: 1, b: 2,]");
}

#[test]
fn multiline_list() {
    must_parse("return [\n  a,\n  b,\n  c,\n]");
}

#[test]
fn multiline_map() {
    must_parse("return [\n  host: prod,\n  port: 8080,\n]");
}

#[test]
fn spread_in_list() {
    must_parse("return [...$a, b, ...$c]");
}

#[test]
fn spread_in_map() {
    must_parse("return [key: val, ...$defaults]");
}

#[test]
fn nested_collections() {
    must_parse("return [a: [b: [c: 1]]]");
}

#[test]
fn list_of_blocks() {
    must_parse("return [{echo a}, {echo b}]");
}

#[test]
fn map_with_block_values() {
    must_parse("return [a: {echo 1}, b: {echo 2}]");
}

// ── Arithmetic edge cases ────────────────────────────────────────────────

#[test]
fn arith_just_number() {
    must_parse("return $[42]");
}

#[test]
fn arith_just_var() {
    must_parse("return $[$x]");
}

#[test]
fn arith_nested_parens() {
    must_parse("return $[((((1 + 2))))]");
}

#[test]
fn arith_all_operators() {
    must_parse("return $[1 + 2 - 3 * 4 / 5 % 6]");
}

#[test]
fn arith_comparisons() {
    must_parse("return $[1 == 2]");
    must_parse("return $[1 != 2]");
}

#[test]
fn arith_lt_gt() {
    must_parse("return $[1 < 2]");
    must_parse("return $[1 > 2]");
}

#[test]
fn arith_le_ge() {
    must_parse("return $[1 <= 2]");
    must_parse("return $[1 >= 2]");
}

#[test]
fn arith_float() {
    must_parse("return $[3.14 + 2.0]");
}

#[test]
fn arith_negative() {
    must_parse("return $[0 - 5]");
}

#[test]
fn arith_empty() {
    must_reject("$[]");
}

// ── Index edge cases ─────────────────────────────────────────────────────

#[test]
fn bracket_index() {
    must_parse("echo $m[key]");
}

#[test]
fn chained_bracket() {
    must_parse("echo $a[0][1][2]");
}

#[test]
fn index_in_interpolation() {
    must_parse("echo \"$m[key]\"");
}

// ── Pipeline edge cases ──────────────────────────────────────────────────

#[test]
fn simple_pipe() {
    must_parse("echo hello | cat");
}

#[test]
fn multi_pipe() {
    must_parse("a | b | c | d");
}

#[test]
fn pipe_with_lambda() {
    must_parse("echo hello | { |x| echo $x }");
}

#[test]
fn chain_simple() {
    must_parse("return true ? echo ok");
}

#[test]
fn return_lifts_value() {
    must_parse("return true ? echo ok");
}

#[test]
fn chain_multiline() {
    must_parse("return true\n? echo ok");
}

// ── Redirect edge cases ──────────────────────────────────────────────────

#[test]
fn redirect_stdout() {
    must_parse("echo hello > out.txt");
}

#[test]
fn redirect_append() {
    must_parse("echo hello >> out.txt");
}

#[test]
fn redirect_stdin() {
    must_parse("cat < in.txt");
}

#[test]
fn redirect_stderr() {
    must_parse("cmd 2> err.txt");
}

#[test]
fn redirect_merge() {
    must_parse("cmd 2>&1");
}

// ── Hostile/adversarial inputs ───────────────────────────────────────────

#[test]
fn null_byte() {
    must_not_panic("echo \0");
}

#[test]
fn unicode_identifiers() {
    must_not_panic("echo $über");
}

#[test]
fn emoji_in_string() {
    must_parse("echo '🎉'");
}

#[test]
fn very_long_line() {
    let long = "echo ".to_string() + &"x".repeat(10_000);
    must_not_panic(&long);
}

#[test]
fn many_statements() {
    let many = "echo x\n".repeat(1000);
    must_not_panic(&many);
}

#[test]
fn many_args() {
    let args = (0..1000)
        .map(|i| format!("arg{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let input = format!("echo {args}");
    must_not_panic(&input);
}

#[test]
fn pathological_backtracking() {
    // Assignment lookahead: [a, b, c, d, ...] could be pattern or list
    let input = format!(
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
    must_not_panic(&input);
}

#[test]
fn context_sensitive_colon() {
    must_parse("echo host:5432"); // embedded : stays bare
    must_parse("return [host: localhost]"); // standalone : splits
    must_parse("echo http://example.com"); // embedded : in URL
}

#[test]
fn context_sensitive_equals() {
    must_parse("let x = 5"); // standalone = is assignment
    must_parse("echo -DFOO=bar"); // embedded = stays bare
    must_parse("echo =="); // == is one token
}

#[test]
fn all_special_chars_in_single_quote() {
    must_parse("echo '|{}[]$<>\"#,();:= ...'");
}

#[test]
fn rapid_open_close() {
    must_not_panic("{}{}{}{}{}{}{}{}{}{}");
    must_not_panic("[][][][][][][][][][]");
}

#[test]
fn comment_at_eof_no_newline() {
    must_not_panic("echo hello # comment");
}

#[test]
fn only_dollars() {
    must_not_panic("$ $ $ $");
}

#[test]
fn spread_without_value() {
    must_not_panic("[...]");
}

#[test]
fn semicolon_flood() {
    must_not_panic(";;;;;;;;;;;;;;;;;;;;");
}

#[test]
fn mixed_terminators() {
    must_parse("echo a\necho b;echo c\necho d");
}
