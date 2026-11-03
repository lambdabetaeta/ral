#![allow(clippy::disallowed_methods, reason = "test code")]
//! Only `let` captures a command's output. Elsewhere commands write to stdout;
//! `to-json` exposes bound values alongside that output.

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

// §1.10: bare `map` writes each command's output.
#[test]
fn map_command_blocks_write_in_input_order() {
    assert_eq!(out("map { |f| echo $f } [1, 2]"), "1\n2");
}

// §1.10: bound `map` writes output and binds Unit elements.
#[test]
fn a_bound_map_of_command_blocks_binds_units() {
    assert_eq!(
        out("let x = map { |f| echo $f } [1, 2]\nto-json $x"),
        "1\n2\n[null,null]"
    );
}

// §1.10: `map $g` follows the same rule for a command block in hand.
#[test]
fn a_bound_map_of_a_command_block_in_hand_binds_units() {
    assert_eq!(
        out("let g = { |f| echo $f }\nlet x = map $g [1, 2]\nto-json $x"),
        "1\n2\n[null,null]"
    );
}

// §1.10: `par` replays command output in input order and returns Units.
#[test]
fn par_command_blocks_replay_output_and_bind_units() {
    assert_eq!(
        out("let x = par { |u| echo $u } [1, 2] 2\nto-json $x"),
        "1\n2\n[null,null]"
    );
}

// §1.10: `cross`, a wrapper over `map`, writes command output.
#[test]
fn cross_command_blocks_write_in_input_order() {
    assert_eq!(out("cross [1, 3] [2] { |x y| echo \"$x$y\" }"), "12\n32");
}

// §1.10: heterogeneous `each` joins a command and a free Unit producer.
#[test]
fn each_runs_command_and_unit_blocks_from_one_list() {
    assert_eq!(out("each { |b| !$b } [{ echo hi }, { return () }]"), "hi");
}

// §1.10: a let-binding wrapper needs its argument to capture explicitly.
#[test]
fn a_value_demanding_wrapper_accepts_explicit_capture() {
    assert_eq!(
        out(
            "let x = { |f| let y = f 5; return $y }\nlet r = x { |n| return !{echo $n} }\nto-json $r"
        ),
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

// §1.10: a mixed `try` needs explicit capture in its command body.
#[test]
fn a_try_joins_explicitly_captured_output_with_a_value_handler() {
    assert_eq!(
        out("let x = try { return !{echo a} } { |e| return 'none' }\nto-json $x"),
        r#""a""#
    );
    assert_eq!(
        out("let x = try { return !{^false} } { |e| return 'none' }\nto-json $x"),
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
