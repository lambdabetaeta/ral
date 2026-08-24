#![allow(clippy::disallowed_methods)]
//! Integration tests for byte-pipeline boundaries: capture rules, decoder
//! tails, and the compile-time rejection of implicit value edges.
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
fn ints_to_bytes_roundtrips_through_from_bytes() {
    // Both writers put bytes on the channel — `ints-to-bytes` from numbers,
    // `to-bytes` from a Bytes value — and from-bytes decodes them back.
    // Verify the roundtrip via length and string decoding (pure ASCII input).
    let o = run_pipe(
        "let bs = !{ints-to-bytes [65, 66, 67] | from-bytes}\necho !{length $bs}\nlet txt = !{to-bytes $bs | from-string}\necho $txt",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    let lines: Vec<&str> = o.stdout.lines().collect();
    assert_eq!(lines, vec!["3", "ABC"]);
}

#[test]
fn ints_to_bytes_rejects_out_of_range_values() {
    let o = run_pipe("!{ints-to-bytes [256]}");
    assert_ne!(o.status, 0);
    assert!(
        o.stderr
            .contains("ints-to-bytes: byte at index 0 out of range"),
        "stderr: {}",
        o.stderr
    );
}

#[test]
fn ints_to_bytes_rejects_non_int_values() {
    // The list element type is checked before the codec runs, so this is a
    // static failure rather than the builtin's runtime index diagnostic.
    let o = run_pipe("!{ints-to-bytes ['x']}");
    assert_ne!(o.status, 0);
    assert!(
        o.stderr
            .contains("couldn't match type String with type Integer"),
        "stderr: {}",
        o.stderr
    );
}

/// The two writers keep their own argument: neither stands in for the other,
/// and both refusals are static.
#[test]
fn each_byte_writer_refuses_the_others_argument() {
    let o = run_pipe("!{to-bytes [65, 66]}");
    assert_ne!(o.status, 0);
    assert!(
        o.stderr.contains("couldn't match"),
        "a list is not a Bytes value: {}",
        o.stderr
    );

    let o = run_pipe("let bs = !{ints-to-bytes [65] | from-bytes}\nints-to-bytes $bs");
    assert_ne!(o.status, 0);
    assert!(
        o.stderr.contains("couldn't match"),
        "a Bytes value is not a list of Ints: {}",
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

// ── A stage's returned value simply goes nowhere ─────────────────────────
//
// A pipeline has one transport: bytes on an operating-system wire, chosen by
// position.  A non-final stage's returned value is discarded — not
// serialised, not an error.  Ordinary application (`f $x`) remains the
// spelling for combining values, and encoders make a value explicit bytes.

#[test]
fn a_value_producer_into_a_block_writes_nothing_and_is_accepted() {
    let o = run_pipe("5 | { |x| echo $x }");
    assert_eq!(o.status, 0, "expected acceptance: {}", o.stderr);
    // The block literal is a value: it is returned, never applied, so the
    // `echo` inside it never runs.
    assert!(
        o.stdout.is_empty(),
        "a returned thunk must not run: {}",
        o.stdout
    );
}

/// The program is accepted, and `hi` still never prints.  That side-effect
/// assertion is the interesting half: a thunk in stage position is returned,
/// and a returned thunk is never forced.  Acceptance changed; this did not.
#[test]
fn a_thunk_stage_is_accepted_and_never_forced() {
    let o = run_pipe("5 | { echo hi }");
    assert_eq!(o.status, 0, "expected acceptance: {}", o.stderr);
    assert!(
        !o.stdout.contains("hi"),
        "a returned thunk must never run: {}",
        o.stdout
    );
}

/// `|` is not application.  Both stages run, `5`'s value is discarded because
/// its stage is non-final, and the pipeline's value is its last stage's.
#[test]
fn a_pipeline_takes_its_value_from_its_last_stage() {
    let o = run_pipe("5 | 6");
    assert_eq!(o.status, 0, "expected acceptance: {}", o.stderr);
}

#[test]
fn an_interior_value_goes_nowhere_and_an_encoder_still_works() {
    // No value is serialised onto an edge, so `wc -l` reads EOF and counts
    // nothing.  This is the whole difference between a byte wire and the
    // value pipe ral deliberately does not have.
    let discarded = run_pipe("return [1, 2] | { |xs| echo $xs } | wc -l");
    assert_eq!(
        discarded.status, 0,
        "expected acceptance: {}",
        discarded.stderr
    );
    assert_eq!(
        discarded.stdout.trim(),
        "0",
        "an interior value must not reach the wire: {}",
        discarded.stdout
    );

    let encoded = run_pipe("to-json [1, 2] | wc -l");
    assert_eq!(
        encoded.status, 0,
        "explicit encoder pipeline failed: {}",
        encoded.stderr
    );
}

/// An interior decoder returns its bytes as a *value*, so the following
/// stage receives EOF.  The consumer is `wc -l` rather than the `grep x` the
/// design writes: `grep` on an empty stream exits 1, and that status would
/// confound "nothing arrived" with "nothing matched".
#[test]
fn an_interior_decoders_returned_bytes_never_reach_the_next_stage() {
    let path = write_tmp_lines("ral_decoder_interior", &["x marks it", "and again x"]);
    let path_str = path.display().to_string();
    let o = run_pipe(&format!("cat '{path_str}' | from-bytes | wc -l"));
    let _ = std::fs::remove_file(&path);
    assert_eq!(o.status, 0, "expected acceptance: {}", o.stderr);
    assert_eq!(
        o.stdout.trim(),
        "0",
        "from-bytes' returned Bytes must not be serialised onto the wire: {}",
        o.stdout
    );
}

/// A producer need not write; an empty stream is still a byte stream.
#[test]
fn a_silent_producer_gives_its_consumer_eof() {
    let o = run_pipe("!{ return () } | cat");
    assert_eq!(o.status, 0, "expected acceptance: {}", o.stderr);
    assert!(
        o.stdout.is_empty(),
        "a value-returning producer must write nothing: {:?}",
        o.stdout
    );
}

/// The bytes of a discarded statement are still bytes, and inside a non-final
/// stage the visible stream *is* the wire — so `x` reaches `cat` even though
/// the stage's own value is `unit`.
#[test]
fn a_discarded_statements_bytes_reach_the_pipe() {
    let o = run_pipe("!{ echo x; return () } | cat");
    assert_eq!(o.status, 0, "expected acceptance: {}", o.stderr);
    assert_eq!(
        o.stdout, "x\n",
        "the discarded statement's bytes must reach the consumer: {:?}",
        o.stdout
    );
}

/// A consumer need not read, and the pipeline's value is its final stage's.
#[test]
fn a_consumer_that_ignores_its_stdin_still_returns_its_value() {
    let o = run_pipe("let result = echo x | !{ return 5 }\necho $result");
    assert_eq!(o.status, 0, "expected acceptance: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "5", "full stdout: {:?}", o.stdout);
}

/// `from-bytes` returns the byte-exact `Bytes` `[65]` and `cat` still sees
/// EOF rather than `A`.  There is no implicit value pipe.
#[test]
fn a_returned_bytes_value_is_not_written_to_the_next_stage() {
    let o = run_pipe("ints-to-bytes [65] | from-bytes | cat");
    assert_eq!(o.status, 0, "expected acceptance: {}", o.stderr);
    assert!(
        o.stdout.is_empty(),
        "a returned Bytes must not be serialised onto the wire: {:?}",
        o.stdout
    );
}

/// The stdin wiring belongs to the pipeline, not to a thunk that escapes it.
/// `echo hi`'s bytes go into an edge nobody reads, and forcing `reader`
/// afterwards runs `from-line` against the script's own stdin.
#[test]
fn a_thunk_that_escapes_a_pipeline_reads_ambient_stdin() {
    let o = run_pipe_stdin(
        "let reader = echo hi | { from-line }\n\
         let s = !$reader\n\
         echo \"[$s]\"",
        b"ambient\n",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(
        o.stdout.trim(),
        "[ambient]",
        "the forced thunk must read the script's stdin, not the dead edge: {:?}",
        o.stdout
    );
    assert!(
        !o.stdout.contains("hi"),
        "the unread edge must swallow the producer's bytes: {:?}",
        o.stdout
    );
}

// ── A stage must be ready to run ─────────────────────────────────────────
//
// One static rule about stages survives: a stage's type is `F[ρ] A`, so a
// stage still waiting for an argument is rejected — in either position, and
// on its own rather than against its neighbour.

#[test]
fn a_final_stage_waiting_for_an_argument_is_rejected() {
    let o = run_pipe("echo hi | !{ |x| echo $x }");
    assert_ne!(o.status, 0, "expected a stage-shape error: {}", o.stdout);
    assert!(
        o.stderr.contains("T0011"),
        "expected the shape error code; stderr: {}",
        o.stderr
    );
    assert!(
        o.stderr.contains("apply it to its argument"),
        "the hint must name application as the fix; stderr: {}",
        o.stderr
    );
}

#[test]
fn an_interior_stage_waiting_for_an_argument_is_rejected() {
    // `fold-lines $step` is one argument short of a computation.
    let o = run_pipe(
        "let step = { |acc _| return $acc }\n\
         echo hi | fold-lines $step | cat",
    );
    assert_ne!(o.status, 0, "expected a stage-shape error: {}", o.stdout);
    assert!(
        o.stderr.contains("T0011"),
        "expected the shape error code; stderr: {}",
        o.stderr
    );
    assert!(
        o.stderr.contains("apply it to its argument"),
        "the hint must reach stderr in interior position too; stderr: {}",
        o.stderr
    );
}

// ── Streaming line combinators ───────────────────────────────────────────

/// Every line is offered to the predicate on its own, the kept lines are
/// written in order, and the decoder at the tail returns them as text.
#[test]
fn filter_lines_streams_its_input_through_the_predicate() {
    let o = run_pipe(
        "let s = !{printf 'aa\\nbb\\ncc\\ndd\\n' | filter-lines { |l| contains ['bb', 'dd'] $l } | from-string}\n\
         echo \"[$s]\"",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "[bb\ndd\n]\n", "full stdout: {:?}", o.stdout);
}

/// A non-final reducer's accumulator is discarded; what its callback writes
/// is what the next stage reads.
#[test]
fn an_interior_folds_accumulator_is_discarded_and_its_callbacks_bytes_are_piped() {
    let o = run_pipe(
        "printf 'a\\nb\\n' | fold-lines { |acc line| echo $line; return $[$acc + 1] } 0 | cat",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(
        o.stdout, "a\nb\n",
        "the callback's bytes, not the accumulator, cross the edge: {:?}",
        o.stdout
    );
}

/// The dual: in final position the same reducer's accumulator is the
/// pipeline's value.
#[test]
fn a_final_folds_accumulator_is_the_pipelines_value() {
    let o = run_pipe(
        "let n = printf 'a\\nb\\n' | fold-lines { |acc _| return $[$acc + 1] } 0\necho $n",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "2", "full stdout: {:?}", o.stdout);
}

// ── Route forwarding through `spawn` ─────────────────────────────────────
//
// `spawn` forwards its body's payload route, so `await` observes the same
// kind of result the body would have produced in place.  `watch` and
// `service` forward by the same one-variable mechanism, but both are
// long-running forms this file has no scaffolding to start and stop.

#[test]
fn spawn_forwards_its_bodys_route_to_the_awaited_result() {
    let byte_routed = run_pipe(
        "let h = spawn { echo hi }\n\
         let result = await $h\n\
         echo \"[!{bytes-to-string $result[stdout]}]\"",
    );
    assert_eq!(byte_routed.status, 0, "stderr: {}", byte_routed.stderr);
    assert_eq!(
        byte_routed.stdout, "[hi\n]\n",
        "a byte-routed body's payload is its stdout: {:?}",
        byte_routed.stdout
    );

    let value_routed = run_pipe(
        "let h = spawn { return 5 }\n\
         let result = await $h\n\
         echo \"$result[value] [!{bytes-to-string $result[stdout]}]\"",
    );
    assert_eq!(value_routed.status, 0, "stderr: {}", value_routed.stderr);
    assert_eq!(
        value_routed.stdout, "5 []\n",
        "a value-routed body's payload is its return, and it wrote nothing: {:?}",
        value_routed.stdout
    );
}

#[test]
fn decoder_tail_returns_its_value_after_a_byte_pipeline() {
    let o = run_pipe("let s = !{echo abc | from-string}\necho !{length $s}");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "4");
}

#[test]
fn float_value_crosses_the_final_report_boundary_bit_exactly() {
    let o = run_pipe("let value = !{printf '0.30000000000000004' | from-json}\necho $value");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "0.30000000000000004");
}

/// A captured native crosses the Report boundary by *name* and re-links
/// against the receiving process's own manifest.  The tail runs in a helper,
/// so applying `$f` in the parent proves the re-linked value is the genuine
/// native rather than a decoded husk.
#[test]
fn captured_native_crosses_the_final_report_boundary_and_relinks() {
    let o = run_pipe(
        "let hand = { from-line; return $round }\n\
         let f = !{printf hi | !$hand}\n\
         echo !{$f 1.567 2}",
    );
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "1.57");
}

/// A value a helper cannot serialize fails at the Report boundary with the
/// process-boundary diagnostic, not a silent `None`.  `spawn`'s `Handle` is
/// the non-transferable case.
#[test]
fn non_transferable_value_fails_at_the_final_report_boundary() {
    let o = run_pipe(
        "let hold = { from-line; let h = !{spawn { return 1 }}; return $h }\n\
         let result = !{printf hi | !$hold}\n\
         echo got",
    );
    assert_ne!(o.status, 0, "expected a boundary failure: {}", o.stdout);
    assert!(
        o.stderr.contains("cannot cross the process boundary"),
        "expected the boundary diagnostic; stderr: {}",
        o.stderr
    );
    assert!(
        !o.stdout.contains("got"),
        "the script must not continue past the failed report: {}",
        o.stdout
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
fn ints_to_bytes_non_utf8_survives_the_pipe() {
    // Binding a non-UTF-8 write at a value boundary now strict-decodes it as
    // a String and fails; the pipe form still carries the raw bytes through.
    let o = run_pipe("let bb = !{ints-to-bytes [255, 0, 254] | from-bytes}\necho !{length $bb}");
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
fn mixed_sequence_return_type_matches_runtime_value() {
    // Regression: the checker and evaluator must agree that this binds the
    // final Int value, not the non-final stdout string.  The preceding
    // byte-output command's output reaches the terminal as an effect.
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

// ── Diagnostics stand apart from the byte channel ───────────────────────────

#[test]
fn warn_writes_stderr_and_stays_out_of_the_capture() {
    // `warn` is the whole diagnostic surface, and its route is Value: the line
    // reaches standard error while the capture binds the byte channel alone.
    // The retired `1>&2` could not do this — it worked by making the two
    // streams one, so the message went wherever the payload went.
    let o = run_pipe("let payload = !{ warn 'note'; echo carried }\necho \"[$payload]\"");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "[carried]", "full stdout: {:?}", o.stdout);
    assert!(
        o.stderr.contains("note"),
        "warn must reach standard error: {:?}",
        o.stderr
    );
}

// ── Pipeline failure reporting ──────────────────────────────────────────────

#[test]
fn failing_decoder_tail_surfaces_its_own_diagnostic() {
    let o = run_pipe("echo not-json | from-json");
    assert_ne!(o.status, 0, "expected decoder failure: {}", o.stdout);
    assert!(
        o.stderr.contains("from-json:"),
        "expected the decoder's own message; stderr: {}",
        o.stderr
    );
}

/// A producer that fails before writing anything is an ordinary pipeline
/// failure: its message wins, and the tail contributes no value.
#[test]
fn producer_failure_before_any_bytes_wins_over_the_tail() {
    let o = run_pipe(
        "let boom = { fail [status: 3, message: 'producer failed'] }\n\
         let result = !{!$boom | from-json}\n\
         echo $result",
    );
    assert_ne!(o.status, 0, "expected failure: {}", o.stdout);
    assert!(
        o.stderr.contains("producer failed"),
        "expected the producer's error; stderr: {}",
        o.stderr
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
fn outer_stdin_redirect_feeds_ral_helper_pipeline_first_stage() {
    // SPEC: same rule for ral helper stage producers.  The forced block is
    // the *first* stage: it reads the redirected stdin, materialises the
    // lines, and emits the count as bytes for the external consumer.
    let lines = ["one", "two", "three", "four"];
    let path = write_tmp_lines("ral_pipe_stdin_ral", &lines);
    let path_str = path.display().to_string();
    let script = format!(
        "let f = {{\n\
         \x20   let count = {{\n\
         \x20       let rows = !{{stream-to-list !{{from-lines}}}}\n\
         \x20       echo !{{length $rows}}\n\
         \x20   }}\n\
         \x20   !$count | cat\n\
         }}\n\
         f < '{path_str}'\n"
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
    let o = run_pipe("let s = !{ guard { echo hi } { return () } | from-string }\necho $s");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hi");
}

#[test]
fn audit_receives_upstream_piped_bytes() {
    let o = run_pipe("let result = !{ echo hi | audit { from-string } }\necho $result[value]");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hi");
}

/// Pins: `v` binds the record `audit` returns — its payload — while `cat`'s
/// bytes go where `cat` was already writing them, the pipeline's own stdout.
///
/// This test used to pin the opposite, and the reason is the whole of the
/// WF-1 repeal: an audit-tailed pipeline had `output = Bytes`, the runtime
/// read that field as "the payload is bytes", and so threw the record away.
/// The two facts now live in two fields and neither is mistaken for the
/// other.
#[test]
fn pipeline_ending_in_audit_binds_the_audit_record() {
    let o = run_pipe("let v = echo hi | audit { cat }\necho $v[status]");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.lines().collect::<Vec<_>>(), vec!["hi", "0"]);
}

#[test]
fn guard_bound_value_is_the_bytes_body_observed_as_a_string() {
    // `guard` keeps `try`'s observed-value convention: binding a guard whose
    // body emits bytes captures the decoded String, not the cleanup's value.
    let o = run_pipe("let gval = guard { echo hi } { return () }\necho $gval");
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
    let o = run_pipe("let v = try { echo hi } { |_| return () }\necho $v");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "hi");
}

#[test]
fn try_relaxed_echo_body_binds_the_handlers_unit_when_handler_taken() {
    let o = run_pipe("let v = try { fail [status: 7] } { |_| return () }\necho \"[$v]\"");
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "[()]");
}
