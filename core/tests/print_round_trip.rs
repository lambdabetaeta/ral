//! `pretty_print` prints ral surface syntax, and two consumers hand that text
//! straight back to a reader: the REPL echo the user copies, and exarch's
//! tool-result `VALUE` section the model reads.  So the quote fence
//! `print::quote_bump_level` chooses must survive the lexer — an off-by-one
//! terminates the string early and the reader silently sees a different value.
//!
//! The harness mirrors `comparison.rs`: bootstrap a prelude-registered
//! `Shell`, then drive each source string through the public `run` door.

mod common;

use ral_core::builtins::{PrintParams, REPL_PRINT_PARAMS, pretty_print};
use ral_core::transport::{Program, Run};
use ral_core::types::{Capabilities, Settled, Shell, Value};
use ral_core::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, builtins};

fn fresh_shell() -> Shell {
    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());
    shell
}

fn eval(shell: &mut Shell, source: &str) -> Settled<Value> {
    match shell.run(RunRequest {
        run: Run {
            program: Program::Source(source.into()),
            script_name: "<test>".into(),
            caps: Capabilities::root(),
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
        nursery: None,
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran { ending, .. } => ending.into_result(),
        RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    }
}

/// Strings whose quotes and `#` runs are exactly what the fence must clear.
const ADVERSARIAL: &[&str] = &["plain", "it's", "a'#b", "x'##y", "'", "#", "''#'##"];

/// Printing a `String` and reading the result back yields the same `String`,
/// both at the minimal fence and at the `#`-forced one exarch demands.
#[test]
fn a_printed_string_re_reads_as_itself() {
    for min_quote_hashes in [0, 1] {
        let params = PrintParams {
            max_string: 0,
            min_quote_hashes,
            ..REPL_PRINT_PARAMS
        };
        for body in ADVERSARIAL {
            let printed = pretty_print(&Value::String((*body).into()), 0, &params);
            let mut shell = fresh_shell();
            match eval(&mut shell, &format!("return {printed}")) {
                Ok(Value::String(s)) => assert_eq!(
                    s.as_str(),
                    *body,
                    "printed as {printed:?} (min_quote_hashes: {min_quote_hashes})"
                ),
                other => panic!("{printed:?} must re-read as {body:?}, got {other:?}"),
            }
        }
    }
}
