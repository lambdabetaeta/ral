//! The static half of the rounding builtins `round`, `floor`, `ceil`, `trunc`;
//! their runtime behaviour is the golden `tests/builtins/numeric.ral`.
//!
//! `round <x> <places>` is the decimal dial — always Float; `floor`/`ceil`/
//! `trunc` map a Float to the Int in their direction.  All four take a Float
//! only (an Int is a static type error — it is already rounded).  The harness
//! mirrors `comparison.rs`: bootstrap a prelude-registered `Shell` and drive
//! each source string through the public `run` door like a REPL
//! run.

mod common;

use common::fresh_shell;

use ral_core::protocol::{Program, Run};
use ral_core::types::GrantStack;
use ral_core::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin};

/// Run `source`, expecting it to be rejected by the type checker before it
/// ever runs (a `RunReport::Static`).
fn expect_static_reject(source: &str) {
    let mut shell = fresh_shell();
    match shell.run(RunRequest {
        run: Run {
            program: Program::Source(source.into()),
            script_name: "<test>".into(),
            caps: GrantStack::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Inherit,
            terminal: RequestedTerminalAccess::Leased,
            stdin: RunStdin::Inherit,
            trail: None,
        },
        surface: None,
        deferred: None,
        desk: None,
        fork: None,
    }) {
        RunReport::Static { .. } => {}
        RunReport::Ran { ending, .. } => {
            panic!("{source:?}: expected a static type error, but it ran: {ending:?}")
        }
    }
}

#[test]
fn an_int_argument_is_a_static_type_error() {
    expect_static_reject("return !{round 5 0}");
    expect_static_reject("return !{floor 7}");
    expect_static_reject("return !{ceil 0}");
    expect_static_reject("return !{trunc 42}");
}
