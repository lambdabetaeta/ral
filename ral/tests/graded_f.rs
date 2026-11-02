#![allow(clippy::disallowed_methods, reason = "test code")]
//! A command's value is its output: where a value is wanted, a command is
//! captured; where a command is wanted, a command runs.  Each case binds
//! something and renders it with `to-json`, so the value and what escaped to
//! the terminal are both on stdout, in order.

mod common;

use common::run;

/// `script`, run to completion: its stdout, with the trailing newline
/// trimmed.
fn out(script: &str) -> String {
    let o = run("ral_graded_f", script);
    assert_eq!(o.status, 0, "script: {script}\nstderr: {}", o.stderr);
    o.stdout.trim_end().to_string()
}

#[test]
fn a_let_binds_a_command_block_whole() {
    assert_eq!(
        out("let x = !{ echo pre; echo host }\nto-json $x"),
        r#""pre\nhost""#
    );
}

#[test]
fn a_bound_block_and_a_bound_function_that_run_a_command_are_commands() {
    assert_eq!(out("let f = { echo hi }\nlet x = f\nto-json $x"), r#""hi""#);
    assert_eq!(
        out("let f = { echo hi }\nlet x = !$f\nto-json $x"),
        r#""hi""#
    );
    assert_eq!(
        out("let g = { |n| echo $n }\nlet x = g 5\nto-json $x"),
        r#""5""#
    );
}

#[test]
fn a_join_of_command_arms_in_hand_is_one_command() {
    let script = "let t = { echo a }\nlet u = { echo b }\nlet x = if true $t else $u\nto-json $x";
    assert_eq!(out(script), r#""a""#);
}

#[test]
fn a_wrapper_that_runs_a_block_produces_what_the_block_does() {
    assert_eq!(out("let x = retry 1 { echo hi }\nto-json $x"), r#""hi""#);
    assert_eq!(out("let x = attempt { echo hi }\nto-json $x"), "hi\nnull");
}

#[test]
fn a_value_demanded_of_a_command_block_captures_per_call() {
    assert_eq!(
        out("let x = map { |f| echo $f } [1, 2]\nto-json $x"),
        r#"["1","2"]"#
    );
    assert_eq!(
        out("let g = { |f| echo $f }\nlet x = map $g [1, 2]\nto-json $x"),
        r#"["1","2"]"#
    );
    assert_eq!(
        out("let x = { |f| let y = f 5; return $y }\nlet r = x { |n| echo $n }\nto-json $r"),
        r#""5""#
    );
}

#[test]
fn a_stdout_redirect_leaves_a_value() {
    assert_eq!(
        out("let d = temp-dir\nlet x = to-json 1 > \"$d/f\"\nto-json $x"),
        "null"
    );
}

#[test]
fn a_try_joins_a_command_body_with_a_value_handler() {
    assert_eq!(
        out("let x = try { echo a } { |e| return 'none' }\nto-json $x"),
        r#""a""#
    );
    assert_eq!(
        out("let x = try { ^false } { |e| return 'none' }\nto-json $x"),
        r#""none""#
    );
}

#[test]
fn a_unit_arm_stands_as_a_command_beside_a_command_arm() {
    let script = "let c = true\nlet x = if $c { echo a } else { warn b }\nto-json $x";
    assert_eq!(out(script), r#""a""#);
    let script = "let c = false\nlet x = if $c { echo a } else { warn b }\nto-json $x";
    assert_eq!(out(script), r#""""#);
}

#[test]
fn a_block_runner_streams_a_command_at_any_grade_into_a_pipe() {
    assert_eq!(out("each { |x| echo $x } [1, 2] | cat"), "1\n2");
}

#[test]
fn a_unit_stub_stands_in_for_a_command_and_captures_nothing() {
    let script = "within [handlers: [curl: { |a| return () }]] { let x = curl foo; to-json $x }";
    assert_eq!(out(script), r#""""#);
}

#[test]
fn a_block_ending_in_a_value_streams_what_it_wrote_before() {
    let script = "let d = temp-dir\necho x > \"$d/f\"\nlet n = !{ echo progress; line-count \"$d/f\" }\nto-json $n";
    assert_eq!(out(script), "progress\n1");
}

#[test]
fn spawn_absorbs_a_command() {
    assert_eq!(
        out("let x = spawn { echo host }\nlet r = await $x\nto-json $r"),
        r#""host""#
    );
    assert_eq!(out("let x = spawn { echo host }\nawait $x"), "host");
}

#[test]
fn audit_absorbs_a_command() {
    let script = "let d = temp-dir\necho ERROR > \"$d/log\"\nlet r = audit { grep -c ERROR \"$d/log\" }\ncase $r[outcome] [`ok: { |v| to-json $v }, `err: { |_| echo failed }]";
    assert_eq!(out(script), "1\nnull");
}

#[test]
fn echo_binds_its_line() {
    assert_eq!(out("let x = echo\nto-json $x"), r#""""#);
    assert_eq!(out("let x = echo a b c\nto-json $x"), r#""a b c""#);
}

#[test]
fn echo_as_a_statement_prints() {
    assert_eq!(out("echo hi"), "hi");
}

#[test]
fn a_forced_command_block_piped_into_a_decoder_yields_its_output() {
    assert_eq!(
        out("let s = { echo one; echo two }\nlet x = !$s | from-string\nto-json $x"),
        r#""one\ntwo\n""#
    );
}
