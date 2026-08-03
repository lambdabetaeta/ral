#![allow(clippy::disallowed_methods)]
//! Integration tests for pipeline *value* semantics: capture rules,
//! data-last application across stage shapes, and value-edge error
//! reporting.
//!
//! Split out of `pipeline.rs`, which stays `#![cfg(unix)]` because most of
//! it drives real external processes (`/bin/sh`, absolute-path coreutils,
//! process groups, signals) to pin external-pipeline mechanics.  Every test
//! here uses only ral builtins, or bundled uutils tools invoked by bare
//! name (`cat`, `head`, `printf`, `wc`) — nothing that requires a Unix
//! shell, an absolute Unix path, or process-group/signal semantics — so it
//! runs on every platform ral targets, Windows included.

mod common;

use common::{run, run_with_stdin, run_with_timeout};
use std::path::PathBuf;
use std::time::Duration;

fn run_pipe(script: &str) -> common::Output {
    run("ral_pipeline_value", script)
}

fn run_pipe_stdin(script: &str, stdin_data: &[u8]) -> common::Output {
    run_with_stdin("ral_pipeline_value", script, stdin_data)
}

// ── Stdin-consuming builtins ─────────────────────────────────────────────────

#[test]
fn parse_json_from_arg() {
    // Decode an in-hand JSON string by piping through to-string | from-json.
    let o = run_pipe(
        r#"let d = !{to-string '{"y":7}' | from-json}
echo $d[y]"#,
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "7");
}

#[test]
fn read_json_from_non_utf8_pipeline_fails() {
    // json is strict: invalid UTF-8 input should fail instead of lossy-decoding.
    let o = run_pipe_stdin("from-json", &[0xff, 0xfe, b'A']);
    assert_ne!(o.status, 0);
    assert!(
        o.stderr.contains("from-json: input is not valid UTF-8"),
        "stderr: {}",
        o.stderr
    );
}

#[test]
fn to_bytes_roundtrips_through_from_bytes() {
    // to-bytes writes bytes to the channel (not a list of ints); from-bytes
    // decodes them back to Value::Bytes.
    // Verify the roundtrip via length and string decoding (pure ASCII input).
    let o = run_pipe(
        "let bs = !{return [65, 66, 67] | to-bytes | from-bytes}\necho !{length $bs}\nlet txt = !{to-bytes $bs | from-string}\necho $txt",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let lines: Vec<&str> = o.stdout.lines().collect();
    assert_eq!(lines, vec!["3", "ABC"]);
}

#[test]
fn to_bytes_rejects_out_of_range_values() {
    let o = run_pipe("return [256] | to-bytes");
    assert_ne!(o.status, 0);
    assert!(
        o.stderr.contains("to-bytes: byte at index 0 out of range"),
        "stderr: {}",
        o.stderr
    );
}

#[test]
fn to_bytes_rejects_non_int_values() {
    let o = run_pipe("return ['x'] | to-bytes");
    assert_ne!(o.status, 0);
    assert!(
        o.stderr.contains("to-bytes: expected Int at index 0"),
        "stderr: {}",
        o.stderr
    );
}

#[test]
fn ext_command_result_is_string_not_bytes() {
    // External command captures decode to String, one trailing \n stripped.
    let o = run_pipe("let xv = printf 'hello\\n'\necho $xv");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hello");
}

#[test]
fn ext_command_single_newline_stripped() {
    // One trailing \n stripped; a second \n is preserved.
    let o = run_pipe("let xv = printf 'a\\n\\n'\necho $xv");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "a\n\n");
}

#[test]
fn read_lines_from_stdin() {
    let o = run_pipe_stdin(
        "let listing = !{from-lines}\nlet listing = !{stream-to-list $listing}\necho !{length $listing}",
        b"one\ntwo\nthree\n",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "3");
}

#[test]
fn fold_lines_from_stdin() {
    let o = run_pipe_stdin(
        "let n = !{fold-lines { |acc _| return $[$acc + 1] } 0}\necho $n",
        b"x\ny\n",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "2");
}

#[test]
fn mixed_pipeline_internal_byte_stage_buffers_output_cleanly() {
    let script = r"
let s = !{printf 'a\nb\n' | map-lines { |x| return $x } | from-lines}
let lines = !{stream-to-list $s}
echo !{length $lines}
echo $lines[0]
echo $lines[1]
";
    let o = run_pipe(script);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let out: Vec<&str> = o.stdout.lines().collect();
    assert_eq!(out, vec!["2", "a", "b"]);
}

// ── Pure-value pipelines: data-last application across all stage shapes ────
//
// These exercise the post-redesign rule: the upstream value is the
// data-last argument of the next stage's call, regardless of whether the
// stage is `Exec`, `App`, a bare-block, or `Force(Variable)`.  Pure-value
// pipelines short-circuit to a sequential fold; no threading is involved.

#[test]
fn block_as_stage_binds_upstream_to_param() {
    // The original bug: `5 | { |x| echo $x }` returned the thunk instead
    // of echoing 5.  After the redesign the block's |x| binds to the
    // upstream value and the body runs.
    let o = run_pipe("5 | { |x| echo $x }");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "5");
}

#[test]
fn block_as_stage_returns_value_via_let() {
    let o = run_pipe("let res = 5 | { |x| return $x }\necho $res");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "5");
}

#[test]
fn pure_value_pipeline_chains_compose_data_last() {
    // [1,2,3] | map { |x| x*2 } | map { |y| y+1 } → [3,5,7]
    let o = run_pipe(
        r"let res = [1, 2, 3] | map { |x| return $[$x * 2] } | map { |y| return $[$y + 1] }
echo !{length $res} $res[0] $res[1] $res[2]",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "3 3 5 7");
}

#[test]
fn pure_value_pipeline_matches_explicit_data_last_call() {
    let o = run_pipe(
        r"let lhs = [1, 2, 3] | fold { |acc x| return $[$acc + $x] } 0
let rhs = !{fold { |acc x| return $[$acc + $x] } 0 [1, 2, 3]}
echo $lhs
echo $rhs",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "6\n6");
}

#[test]
fn force_variable_as_stage_applies_to_upstream() {
    // `5 | $f` where f is a unary block: f gets applied to 5.
    let o = run_pipe("let f = { |x| return $[$x + 10] }\nlet res = 5 | $f\necho $res");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "15");
}

#[test]
fn force_variable_stage_keeps_upstream_data_last_after_explicit_args() {
    let o = run_pipe(
        "let f = { |prefix x| return \"$prefix:$x\" }\n\
         let lhs = 5 | $f 'n'\n\
         let rhs = !{$f 'n' 5}\n\
         echo $lhs\n\
         echo $rhs",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "n:5\nn:5");
}

#[test]
fn pipe_into_non_function_value_errors() {
    // `5 | 6` — there's no function on the right; the trampoline says so.
    let o = run_pipe("5 | 6");
    assert_ne!(o.status, 0, "expected failure, got: {}", o.stdout);
    assert!(
        o.stderr.contains("is not a function"),
        "stderr: {}",
        o.stderr
    );
}

// ── Consumed stages with byte output: value edge in, byte channel out ──────

#[test]
fn consumed_stage_with_byte_output_feeds_external_consumer() {
    // `{ |xs| echo $xs }` takes its upstream on the value edge (input ∅)
    // but its application body emits bytes, which `wc -l` reads.  Pins
    // the consumed-stage wire: input ∅, output Bytes — one echoed list
    // line reaches wc.
    let o = run_pipe("return [1, 2] | { |xs| echo $xs } | wc -l");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "1");
}

#[test]
fn value_producer_block_is_forced_across_helper_value_edge() {
    // Process-staged because the last stage's body emits bytes: the
    // bare-block producer runs in a helper, and its block result is
    // forced once before crossing the value edge, so the middle stage
    // receives 5 rather than an unforced thunk.
    let o = run_pipe("{ return 5 } | { |v| return $[$v + 1] } | { |n| echo $n }");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "6");
}

#[test]
fn value_producer_lambda_returning_block_is_forced_across_helper_value_edge() {
    // The middle stage is a lambda whose body returns a block, so the
    // edge value after data-last application (`{ |x| { return 9 } } !{1}`)
    // is itself a block needing one force.  Pins that `force_pipe_value`
    // forces the post-application block once — not the lambda, which the
    // checker's `deref_forced_producer` passes through unforced — so the
    // consumer receives 9 rather than an unforced thunk.
    let o = run_pipe("return 1 | { |x| { return 9 } } | { |n| echo $n }");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "9");
}

#[test]
fn float_crosses_a_process_staged_value_edge_bit_exactly() {
    // A Float produced upstream must survive the value wire unchanged.
    // Non-finite floats are refused at construction, so the wire only ever
    // carries finite values; a full-mantissa one pins that the trailing
    // byte-emitting stage — which makes the pipeline `ProcessStaged`, so
    // the Float is reified into a `SerialValue::Float` and JSON-framed
    // between helpers — reproduces it digit for digit.
    let o = run_pipe("{ return $[0.1 + 0.2] } | { |x| return $x } | { |y| echo $y }");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "0.30000000000000004");
}

#[test]
fn captured_native_crosses_a_process_staged_value_edge_and_relinks() {
    // The byte-emitting last stage forces the pipeline process-staged, so
    // `$round` crosses a real process boundary by name and re-links against
    // the receiving process's own manifest; the middle stage applies it
    // fully, proving the re-linked value is the genuine native.
    let o = run_pipe("{ return $round } | { |f| return !{$f 1.567 2} } | { |n| echo $n }");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "1.57");
}

#[test]
fn value_edge_into_external_applies_data_last_on_every_dispatch_path() {
    // A value edge into an external head is data-last application —
    // `x | cmd = cmd !{x}` — so the upstream value becomes the
    // command's final argument.  The stage therefore routes through
    // the helper (a direct spawn cannot wait for the value), and the
    // result is the same whether or not the pipeline is interactive:
    // `true | cat` runs `cat true`, which fails on the missing file.
    let o = run_pipe("true | cat");
    assert_ne!(o.status, 0, "stdout: {}", o.stdout);
    assert!(
        o.stderr.contains("No such file") || o.stderr.contains("cat"),
        "expected cat's missing-file failure, got: {}",
        o.stderr
    );
}

#[test]
fn pipe_into_zero_arity_block_runs_then_errors_on_excess_arg() {
    // `5 | { echo hi }` — the block has no params; echo runs (eager)
    // and then the trampoline errors with "too many arguments" because
    // the block's Unit result can't accept the upstream 5.
    let o = run_pipe("5 | { echo hi }");
    assert_ne!(o.status, 0);
    // The side effect happened before the error.
    assert!(o.stdout.contains("hi"), "stdout: {}", o.stdout);
    assert!(
        o.stderr.contains("too many arguments") || o.stderr.contains("is not a function"),
        "stderr: {}",
        o.stderr
    );
}

// ── Capture semantics ────────────────────────────────────────────────────
//
// These tests verify the principle: `let` binds the return value of its RHS.
// For byte-output commands with no value return, the value is the decoded
// String of the final command's bytes.  For value-returning commands the
// return value is bound directly.  Non-final stdout remains an effect.

#[test]
fn block_return_captures_only_last_command() {
    // A sequence returns its final computation's value.  Non-final stdout is an
    // effect and remains visible.
    let o = run_pipe("let xv = !{ echo one; echo two; echo three }\necho \"[$xv]\"");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let lines: Vec<_> = o.stdout.trim().lines().collect();
    assert_eq!(
        lines,
        ["one", "two", "[three]"],
        "full stdout: {:?}",
        o.stdout
    );
}

#[test]
fn block_non_final_bytes_reach_terminal() {
    // Non-final commands in a value-bound sequence flush their bytes to the
    // outer stdout so side-effects remain visible.
    let o = run_pipe("let xv = !{ echo log; echo result }\necho \"x=$xv\"");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let lines: Vec<_> = o.stdout.trim().lines().collect();
    assert_eq!(lines, ["log", "x=result"], "full stdout: {:?}", o.stdout);
}

#[test]
fn higher_order_capture() {
    // Call-site mode instantiation: the higher-order function's Var output mode
    // is resolved to Bytes from the argument thunk's syntactic mode.
    let o = run_pipe("let f = { |cmd| !$cmd }\nlet xv = f { printf hello }\necho \"[$xv]\"");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "[hello]");
}

#[test]
fn to_json_pipes_to_from_json() {
    // to-json is a pure byte writer, like to-line and echo: the roundtrip
    // lives in the pipe, not at a value boundary.
    let o = run_pipe("let obj = !{to-json [a: 1] | from-json}\necho $obj[a]");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "1");
}

#[test]
fn to_bytes_non_utf8_survives_the_pipe() {
    // Binding a non-UTF-8 write at a value boundary now strict-decodes it as
    // a String and fails; the pipe form still carries the raw bytes through.
    let o = run_pipe("let bb = !{to-bytes [255, 0, 254] | from-bytes}\necho !{length $bb}");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "3");
}

#[test]
fn to_json_via_user_wrapper() {
    // to-json is a primitive; users who want a first-class handle wrap it
    // in a block.  Roundtrip: encode → pipe → decode.
    let o =
        run_pipe("let f = { |v| to-json $v }\nlet obj = !{f [a: 42] | from-json}\necho $obj[a]");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "42");
}

#[test]
fn block_mixed_modes_returns_value() {
    // When the final command of a block returns a value, that value is bound.
    // Preceding byte-output commands' output goes to the terminal as an effect.
    let o = run_pipe("let xv = !{ echo hello; length [1, 2, 3] }\necho $xv");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let lines: Vec<_> = o.stdout.trim().lines().collect();
    assert_eq!(lines, ["hello", "3"], "full stdout: {:?}", o.stdout);
}

#[test]
fn mixed_sequence_return_type_matches_runtime_value() {
    // Regression: the checker and evaluator must agree that this binds the
    // final Int value, not the non-final stdout string.
    let o = run_pipe("let n = !{ echo hello; length [1, 2, 3] }\necho $[$n + 1]");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let lines: Vec<_> = o.stdout.trim().lines().collect();
    assert_eq!(lines, ["hello", "4"], "full stdout: {:?}", o.stdout);
}

#[test]
fn function_non_final_stdout_does_not_replace_return_value() {
    let o = run_pipe("let f = { |xs| echo hello; length $xs }\nlet y = f [1, 2, 3]\necho \"y=$y\"");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let lines: Vec<_> = o.stdout.trim().lines().collect();
    assert_eq!(lines, ["hello", "y=3"], "full stdout: {:?}", o.stdout);
}

#[test]
fn try_handler_final_stdout_can_be_recovery_value() {
    let o = run_pipe(
        "let recovered = try { fail [status: 7] } { |_| echo caught }\necho \"x=$recovered\"",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "x=caught", "full stdout: {:?}", o.stdout);
}

// ── Value-edge structured errors ────────────────────────────────────────────

#[test]
fn required_value_edge_eof_is_structured_error() {
    // Producer fails before yielding a value; the consumer's
    // value-in fd closes empty.  Pre-Fix-3 the helper treated this
    // as "no upstream value" and ran the consumer with `None`.  Now
    // it's a structured pipeline protocol error or — depending on
    // which side of the race wins — the producer's own error.  Either
    // way the script must fail loudly rather than silently running
    // the consumer with a phantom upstream.
    let script = r#"
let res = { fail [status: 1, message: "producer failed"] } | { |x| return $[$x + 1] }
echo $res
"#;
    let o = run_with_timeout("ral_pipeline_value", &[], script, Duration::from_secs(5))
        .expect("required-value-edge test hung");
    assert_ne!(o.status, 0, "expected failure: {}", o.stdout);
    let combined = format!("{} {}", o.stderr, o.stdout);
    assert!(
        combined.contains("producer failed") || combined.contains("value edge"),
        "expected producer error or value-edge diagnostic; stderr: {}",
        o.stderr
    );
}

#[test]
fn value_edge_send_failure_is_structured() {
    // A producer that tries to send a non-transferable value (a
    // `Handle`) across an inter-stage value edge must surface the
    // boundary diagnostic from `pack_stage_value`, not the generic
    // "value edge closed" EOF that the consumer would otherwise
    // emit.  Pre-fix the producer logged the failure and continued
    // sending its (success) report; the consumer then ran without a
    // value or saw a confusing downstream-only error.
    //
    // The shape forces process-staged execution by including a byte
    // stage (`printf hi | from-string`); the next ral helper builds
    // a `Handle` and returns it, exercising the producer→consumer
    // value edge.
    let script = r"
let res = !{
  printf hi
  | from-string
  | { |_s| let h = !{spawn { return 1 }}; return $h }
  | { |_x| echo got }
}
echo done
";
    let o = run_with_timeout("ral_pipeline_value", &[], script, Duration::from_secs(5))
        .expect("value-edge send-failure test hung");
    assert_ne!(o.status, 0, "expected failure: {}", o.stdout);
    let combined = format!("{} {}", o.stderr, o.stdout);
    assert!(
        combined.contains("cannot cross the process boundary"),
        "expected boundary diagnostic; stderr: {}",
        o.stderr
    );
    assert!(
        !o.stdout.contains("got"),
        "downstream consumer body must not have run; stdout: {}",
        o.stdout
    );
}

// ── Handler-resolved head redirects (builtin bodies) ────────────────────────

#[test]
fn handler_mock_redirect_captures_builtin_stdout() {
    // A `within [handlers:]` mock whose body is the ral `echo` builtin:
    // `foo > file` routes the builtin's stdout into the file.  Exercises the
    // value-returning / builtin-forwarding handler shape, not just externals.
    let path = common::fresh_tmp_path("ral_handler_mock", "txt");
    let path_str = path.display().to_string();
    let _ = std::fs::remove_file(&path);

    let o = run_pipe(&format!(
        "within [handlers: [foo: {{ |args| echo mock_marker }}]] {{ foo > '{path_str}' }}\n"
    ));
    let body = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);

    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(
        !o.stdout.contains("mock_marker"),
        "mock output leaked to the terminal: {}",
        o.stdout
    );
    assert_eq!(
        body.as_deref().map(str::trim_end),
        Some("mock_marker"),
        "handler mock redirect file did not capture the builtin's stdout"
    );
}

#[test]
fn catch_all_handler_redirect_is_honored() {
    // The catch-all `within [handler: …]` resolves through the same
    // `Resolution::Handler` arm as per-name handlers, so its redirect must be
    // honored too.  The thunk binds `(name, args)` and echoes the name.
    let path = common::fresh_tmp_path("ral_catchall", "txt");
    let path_str = path.display().to_string();
    let _ = std::fs::remove_file(&path);

    let o = run_pipe(&format!(
        "within [handler: {{ |name _args| echo $name }}] {{ catchme > '{path_str}' }}\n"
    ));
    let body = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);

    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(
        body.as_deref().map(str::trim_end),
        Some("catchme"),
        "catch-all handler redirect file did not capture the body's stdout"
    );
}

// ── `echo` as a base handler frame ──────────────────────────────────────────

#[test]
fn echo_mixed_type_args_are_stringified_and_space_joined() {
    let o = run_pipe("let n = 5\necho \"count:\" $n");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "count: 5\n");
}

#[test]
fn echo_zero_args_emits_one_newline() {
    let o = run_pipe("echo");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "\n");
}

#[test]
fn stacked_echo_handler_forwards_to_base_frame() {
    // The stacked frame intercepts the bare head; its own `echo` calls reach
    // the base frame under self-masking instead of recursing.
    let o = run_pipe(
        "within [handlers: [echo: { |args| echo intercepted; echo ...$args }]] { echo original }",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "intercepted\noriginal\n");
}

// ── Outer stdin redirect feeds pipeline boundary ─────────────────────────────

fn write_tmp_lines(prefix: &str, lines: &[&str]) -> PathBuf {
    let path = common::fresh_tmp_path(prefix, "txt");
    let body = lines.join("\n") + "\n";
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn outer_stdin_redirect_feeds_external_pipeline_first_stage() {
    // SPEC: an outer `< file` on a function call whose body is a
    // pipeline must feed the first byte-consuming stage's stdin.
    // The first stage here is `cat -n`, which numbers its input.
    let path = write_tmp_lines("ral_pipe_stdin_ext", &["alpha", "beta", "gamma"]);
    let path_str = path.display().to_string();
    let script = format!("let f = {{ cat -n | head -n 1 }}\nf < '{path_str}'\n");
    let o = run_with_timeout("ral_pipeline_value", &[], &script, Duration::from_secs(5))
        .expect("outer stdin redirect to external pipeline first stage hung");
    let _ = std::fs::remove_file(&path);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(
        o.stdout.contains("alpha"),
        "first line should be numbered alpha; got: {:?}",
        o.stdout
    );
}

#[test]
fn outer_stdin_redirect_feeds_ral_builtin_pipeline_first_stage() {
    // SPEC: same rule for ral helper stage producers.
    // `fold-lines` reads bytes off stdin, counts lines, pipes the
    // value to the next ral stage which echoes it.
    let lines = ["one", "two", "three", "four"];
    let path = write_tmp_lines("ral_pipe_stdin_ral", &lines);
    let path_str = path.display().to_string();
    let script = format!(
        "let f = {{\n    fold-lines {{ |acc _| return $[$acc + 1] }} 0 | {{ |n| echo $n }}\n}}\nf < '{path_str}'\n"
    );
    let o = run_with_timeout("ral_pipeline_value", &[], &script, Duration::from_secs(5))
        .expect("outer stdin redirect to ral pipeline first stage hung");
    let _ = std::fs::remove_file(&path);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), lines.len().to_string());
}

// ─── guard / audit scope byte I/O ────────────────────────────────────────────
//
// `guard` and `audit` are channel-transparent: an arm's bytes flow through
// the scope's live streams rather than being silenced by the scope
// boundary.

#[test]
fn guard_body_bytes_flow_into_downstream_stage() {
    let o = run_pipe("let s = !{ guard { echo hi } { return unit } | from-string }\necho $s");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hi");
}

#[test]
fn audit_receives_upstream_piped_bytes() {
    let o = run_pipe("let r = !{ echo hi | audit { from-string } }\necho $r[value]");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hi");
}

#[test]
fn pipeline_ending_in_audit_binds_the_piped_string() {
    // Pins: `v` binds the piped text, not the record `audit` returns.
    let o = run_pipe("let v = echo hi | audit { cat }\necho $v");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hi");
}

#[test]
fn guard_bound_value_is_the_bytes_body_observed_as_a_string() {
    // `guard` keeps `try`'s observed-value convention: binding a guard whose
    // body emits bytes captures the decoded String, not the cleanup's value.
    let o = run_pipe("let gval = guard { echo hi } { return unit }\necho $gval");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hi");
}

// ─── Observed-value arm join: mixed byte/value arms bind either way ─────────
//
// `if`/`?` accept a byte-emitting arm mixed with a raw-value arm because both
// observe under the arms' joined output; whichever arm actually runs at
// runtime binds the same String the checker predicted.

#[test]
fn if_mixed_arms_bind_the_external_arm_when_taken() {
    let o = run_pipe("let v = if true { ^printf hi } else { echo bye }\necho $v");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hi");
}

#[test]
fn if_mixed_arms_bind_the_echo_arm_when_taken() {
    let o = run_pipe("let v = if false { ^printf hi } else { echo bye }\necho $v");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "bye");
}

#[test]
fn chain_mixed_arms_binds_the_succeeding_external_arm() {
    let o = run_pipe("let v = ^printf hi ? echo bye\necho $v");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hi");
}

#[test]
fn chain_mixed_arms_falls_through_to_the_echo_arm() {
    let o = run_pipe("let v = cat /nonexistent-ral-fixture 2> /dev/null ? echo bye\necho $v");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "bye");
}

#[test]
fn if_echo_arm_with_empty_else_binds_empty_string_when_else_taken() {
    let o = run_pipe("let v = if false { echo x } else {}\necho \"[$v]\"");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "[]");
}

#[test]
fn try_relaxed_echo_body_binds_the_line_when_body_succeeds() {
    let o = run_pipe("let v = try { echo hi } { |_| return unit }\necho $v");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hi");
}

#[test]
fn try_relaxed_echo_body_binds_empty_string_when_handler_taken() {
    let o = run_pipe("let v = try { fail [status: 7] } { |_| return unit }\necho \"[$v]\"");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "[]");
}
