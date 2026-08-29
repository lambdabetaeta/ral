#![allow(clippy::disallowed_methods)]

//! W3: a pipeline's failure reports through the frames above it exactly as
//! any other rule's does (§5 of the CEK plan,
//! `dev/docs/plans/260825_cek_machine.md`).
//!
//! Drives the public run door, so the test is the session a user has: a
//! failing pipeline stage's error must climb through the `Capture` frame
//! above it — flushed bytes aside, capture re-raises — and land in the
//! `Try` frame above that, the same `Halt` column every other frame walks.

mod common;

use ral_core::builtins;
use ral_core::protocol::{Program, Run};
use ral_core::types::{Capabilities, Settled, Shell, Value};
use ral_core::{
    RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, StaticDiagnostics,
};

/// A real PATH, so `echo` is a genuine external stage — the pipeline this
/// test drives spawns actual processes, not just ral computations.
fn fresh_shell() -> Shell {
    let mut shell = Shell::new(ral_core::io::TerminalState::default());
    shell.set_env_var("PATH", "/bin:/usr/bin");
    builtins::register(&mut shell, common::prelude_comp());
    shell
}

fn run(shell: &mut Shell, source: &str) -> Settled<Value> {
    match shell.run(RunRequest {
        run: Run {
            program: Program::Source(source.into()),
            script_name: "<pipeline-frame>".into(),
            caps: Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Inherit,
            terminal: RequestedTerminalAccess::Denied,
            stdin: RunStdin::Empty,
            trail: None,
        },
        surface: None,
        deferred: None,
        desk: None,
        fork: None,
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran { ending, .. } => ending.into_result(),
        RunReport::Static { diagnostics } => {
            let msg = match diagnostics {
                StaticDiagnostics::Parse(e) => e.to_string(),
                StaticDiagnostics::Types(errs) => errs
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
                StaticDiagnostics::Host(e) => e.to_string(),
            };
            panic!("static diagnostic on {source:?}: {msg}");
        }
    }
}

/// The stack when the pipeline joins is `Try`, `Capture` — a pipeline
/// sitting under a byte capture sitting under a handler.  `fail` in the
/// second stage raises inside the child machine and crosses back as the
/// pipeline's own error (`collect::PipelineCollector`); the `Pipeline` rule
/// turns it into `Halt` on the spot; `Capture`'s `Halt` rule flushes
/// whatever bytes `echo a` wrote and re-raises; `Try`'s `Halt` rule is what
/// finally converts it into the handler's argument.  No step here is
/// special-cased for a pipeline — the same `Halt` column every frame walks.
#[test]
fn a_failing_pipeline_under_try_and_capture_reports_through_both_frames() {
    let mut shell = fresh_shell();
    let v = run(
        &mut shell,
        "let r = try { !{ echo a | fail [status: 7, message: 'boom'] } } { |e| return $e[status] }\n\
         return $r",
    )
    .unwrap_or_else(|e| panic!("the try handler must catch the pipeline's failure: {e:?}"));
    assert_eq!(v, Value::Int(7));
}

// A test that a panic mid-pipeline leaves no live child is not implemented
// here: the `Pipeline` rule launches and joins within a single step, with
// no frame of its own on the stack in between — so the only window in which
// a panic could interrupt a live pipeline is *inside* `PipeNode::join`
// itself, a private call this test has no way to interrupt without adding
// test-only hooks to the machine. There is no sane way to provoke that from
// outside the crate.
