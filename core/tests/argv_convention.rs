//! The argv convention at run time: what a name taking an argv actually
//! receives, and where an argv stops being text the shell may render freely.
//!
//! A handler arm, a base frame and an external are variadic over an argv, and
//! one renderer serves all three inside the shell — `Value::render_argv`, total
//! by construction, so a map, a lambda and a block all have a text form.  The
//! exec boundary is the one place that is *not* total: heading for `execve(2)`,
//! it refuses the shapes an operating system has no argument for.  Total
//! rendering inside, gated at the OS call; a rule uniform across both would be
//! wrong in one direction.
//!
//! That gate is read twice.  A shape is what a type states, so wherever an
//! argument's type is concrete the checker refuses it before the run (T0057);
//! where polymorphism hides it, the pre-spawn refusal is still there to catch it.
//! One refused set, two moments — and the tests below pin which moment answers.
//!
//! Everything here drives the public `run` door, so each test is the session a
//! user has.

mod common;

use ral_core::protocol::{Program, Run};
use ral_core::types::{Break, Capabilities, Shell, Value};
use ral_core::{
    RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, Settled, StaticDiagnostics,
};

/// A session as every front end builds one: prelude registered, env seeded,
/// capabilities at root.
fn fresh_shell() -> Shell {
    ral_core::boot::boot_shell(
        ral_core::io::TerminalState::default(),
        common::prelude(),
        &ral_core::boot::HostSurface::default(),
    )
}

/// One top-level dispatch through the public door, stdout captured — the report
/// as a front end receives it, diagnosed or run.
fn report(src: &str) -> RunReport {
    fresh_shell().run(RunRequest {
        run: Run {
            program: Program::Source(src.into()),
            script_name: "<argv-convention>".into(),
            caps: Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Capture,
            terminal: RequestedTerminalAccess::Denied,
            stdin: RunStdin::Empty,
            trail: None,
        },
        surface: None,
        deferred: None,
        desk: None,
        fork: None,
        lifecycle: Box::new(()),
    })
}

/// A run that must reach the evaluator, with what it wrote to stdout.
fn run_capture(src: &str) -> (Settled<Value>, String) {
    match report(src) {
        RunReport::Ran {
            ending, captured, ..
        } => {
            let stdout = captured.map(|c| c.stdout).unwrap_or_default();
            (
                ending.into_result(),
                String::from_utf8(stdout).expect("captured stdout is UTF-8"),
            )
        }
        RunReport::Static { diagnostics, .. } => panic!(
            "{src:?} must reach the evaluator, got {}",
            match diagnostics {
                StaticDiagnostics::Parse(e) => format!("parse {e:?}"),
                StaticDiagnostics::Types(errs) => format!("{} type diagnostic(s)", errs.len()),
                StaticDiagnostics::Host(e) => format!("host {e:?}"),
            }
        ),
    }
}

/// What `src` writes to stdout, the run having succeeded.
fn printed(src: &str) -> String {
    let (result, out) = run_capture(src);
    result.unwrap_or_else(|e| panic!("{src:?} must run: {e:?}"));
    out
}

/// The type diagnostics `src` earns without ever reaching the evaluator, by
/// code — empty when it does reach it.
fn static_codes(src: &str) -> Vec<String> {
    match report(src) {
        RunReport::Static {
            diagnostics: StaticDiagnostics::Types(errs),
            ..
        } => errs.iter().map(|e| e.kind.code().to_string()).collect(),
        RunReport::Static {
            diagnostics: StaticDiagnostics::Parse(e),
            ..
        } => panic!("{src:?}: expected type diagnostics, got parse {e:?}"),
        RunReport::Static {
            diagnostics: StaticDiagnostics::Host(e),
            ..
        } => panic!("{src:?}: expected type diagnostics, got host {e:?}"),
        RunReport::Ran { .. } => Vec::new(),
    }
}

/// Run `src` expecting a `Break::Error` whose message contains `needle`.
fn refused(src: &str, needle: &str) {
    match run_capture(src).0 {
        Err(Break::Error(e)) => assert!(
            e.message.contains(needle),
            "{src:?}: error {:?} should mention {needle:?}",
            e.message
        ),
        Ok(v) => panic!("{src:?}: expected a refusal mentioning {needle:?}, got {v:?}"),
        Err(other) => panic!("{src:?}: expected Break::Error, got {other:?}"),
    }
}

// ── An arm receives renderings ────────────────────────────────────────────────

/// The runtime half of the argv rule: the arm is handed the argv as text, so
/// what it spreads onward is the text form of each atom the call site wrote.
#[test]
fn an_arm_receives_its_argv_rendered() {
    assert_eq!(
        printed(r"within [handlers: [mycmd: { |args| echo 'got' ...$args }]] { mycmd 1 true }"),
        "got 1 true\n"
    );
    assert_eq!(
        printed(r"alias mycmd { |args| echo 'got' ...$args }; mycmd 3.5 [a: 1]"),
        "got 3.5 [a: 1]\n"
    );
}

/// An arm may also read its argv as the list it is, element by element.
#[test]
fn an_arm_can_index_its_argv() {
    assert_eq!(
        printed(r"within [handlers: [mycmd: { |args| echo $args[1] }]] { mycmd first second }"),
        "second\n"
    );
}

// ── Rendering inside is total; the exec boundary is gated ─────────────────────

/// Every value has a text form, so every value crosses an in-shell argv.  A
/// map, a lambda and a block are the shapes an external will refuse, and here
/// they simply print.
#[test]
fn rendering_an_argv_is_total() {
    assert_eq!(
        printed(
            r"let f = { |x| return $x }; let blk = { echo hi }; echo [a: 1] $f $blk 3.5 [1, 2]"
        ),
        "[a: 1] <|x| block> <block> 3.5 [1, 2]\n"
    );
}

/// The same shapes reaching a real spawn are refused, because `execve(2)` has
/// no argument for them.  This is the boundary the total renderer must not be
/// mistaken for: `cat` is a bundled tool, vetted exactly as a host binary is.
///
/// A parameter is what hides the shape from the checker here — inside the arm
/// `$v` is any type at all — so these runs reach the evaluator and are refused
/// by `vet`, one step from the syscall.
#[test]
fn the_exec_boundary_refuses_what_rendering_accepts() {
    refused(
        "let show = { |v| cat $v }; show [a: 1]",
        "cannot pass Map to external command",
    );
    refused(
        r"let show = { |v| cat $v }; let f = { |x| return $x }; show $f",
        "cannot pass Lambda to external command",
    );
}

/// A list is refused there too, and the refusal names the notation that lowers
/// it — so the gate teaches the argv the user meant.
#[test]
fn the_exec_boundary_names_the_spread_that_lowers_a_list() {
    match run_capture("let show = { |v| cat $v }; show [a, b]").0 {
        Err(Break::Error(e)) => assert!(
            e.hint.is_some_and(|h| h.contains("...")),
            "the refusal should point at `...`"
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// ── One refused set, read at two moments ──────────────────────────────────────

/// Written out, the shape is in the type, and the type is in hand before the
/// run: the refusal comes as a diagnostic and nothing is spawned.
#[test]
fn a_concretely_unrenderable_argument_never_reaches_the_spawn() {
    for src in [
        "let r = [a: 1]; cat $r",
        "let xs = [a, b]; cat $xs",
        r"let f = { |x| return $x }; cat $f",
        "let h = spawn { return 1 }; cat $h",
    ] {
        assert_eq!(static_codes(src), ["T0057"], "{src:?}");
    }
}

/// And what the type does not say, the checker does not say either: a
/// parameter's shape is the run's business, so the same call passes the gate
/// and meets `vet` instead.  This is why no program that ran before stops
/// running.
#[test]
fn a_polymorphic_argument_is_left_to_the_spawn() {
    assert!(
        static_codes("let show = { |v| cat $v }; show [a: 1]").is_empty(),
        "a parameter's shape is not known here, so nothing may be refused"
    );
}

/// Nor does the checker say anything about a spread's elements, however concrete
/// their type — and this is the run that says why.  A spread contributes as many
/// arguments as its list holds; an empty one contributes none and so reaches
/// `execve(2)` cleanly, though every element it could have held is a refused
/// shape.  A static refusal here would reject a call that runs.
#[test]
fn an_empty_spread_of_unrenderable_elements_spawns_cleanly() {
    let src = "let none = filter { |xs| return $[false] } [[1], [2]]; cat ...$none";
    assert!(static_codes(src).is_empty(), "a spread is not gated");
    assert_eq!(printed(src), "", "`cat` with no argument reads empty stdin");
}

/// The gate is the exec boundary's alone.  An argv the shell renders itself —
/// `echo`'s frame, a handler arm — takes every shape, before the run as at it.
#[test]
fn an_in_shell_argv_is_gated_by_nothing() {
    for src in [
        "echo [a: 1]",
        r"let f = { |x| return $x }; echo $f [1, 2]",
        r"alias mycmd { |args| ^echo ...$args }; mycmd [a: 1]",
        r"within [handlers: [mycmd: { |args| ^echo ...$args }]] { mycmd [a: 1] }",
    ] {
        assert!(static_codes(src).is_empty(), "{src:?} must typecheck");
    }
}

// ── `echo` is a base frame ────────────────────────────────────────────────────

/// `^echo` reaches the frame rather than a `PATH` binary — and the frame is
/// what proves it: a lambda has a text form here, where an external would
/// refuse it outright.
#[test]
fn caret_echo_reaches_the_frame_not_a_path_binary() {
    assert_eq!(
        printed(r"let f = { |x| return $x }; ^echo $f"),
        "<|x| block>\n"
    );
}

/// Being a frame rather than a value, `echo` can be stacked on: a handler under
/// its name intercepts the bare head.
#[test]
fn a_handler_stacked_on_echo_intercepts_the_bare_name() {
    assert_eq!(
        printed(r"within [handlers: [echo: { |args| ^echo 'mocked' ...$args }]] { echo hi there }"),
        "mocked hi there\n"
    );
}

/// And the frame is restored at the brace, the stack being scoped.
#[test]
fn echos_frame_returns_at_the_brace() {
    assert_eq!(
        printed(r"within [handlers: [echo: { |args| ^echo 'mocked' }]] { echo hi }; echo plain"),
        "mocked\nplain\n"
    );
}

/// A base frame keeps its manifest row, so the two verbs that read the manifest
/// still answer for it: `explain` prints the row's doc and type, and reports the
/// frame as what would run — as the builtin it is, since answering through the
/// stack is what distinguishes it from a handler someone installed, not
/// something the reader has to be told.
#[test]
fn explain_still_reads_echos_manifest_row() {
    let out = printed("explain echo");
    for fragment in ["write one line", "[String] → Command", "echo: builtin"] {
        assert!(
            out.contains(fragment),
            "explain echo should mention {fragment:?}, got:\n{out}"
        );
    }
}

/// `help`'s overview likewise still lists it.
#[test]
fn help_still_lists_echo() {
    let out = printed("help");
    assert!(
        out.contains("echo — echo <args...>"),
        "help should list `echo`, got:\n{out}"
    );
}
