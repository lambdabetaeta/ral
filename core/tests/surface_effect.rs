#![allow(clippy::disallowed_methods)]

//! The `surface` effect: a value handed to the builtin reaches the
//! host-installed sink unchanged, and with no sink installed the builtin
//! is the identity.  Drives the public `eval_top_level` boundary the same
//! way `ral` and `exarch` do, mirroring the harness in
//! `top_level_vs_block.rs`.

mod common;

use ral_core::evaluator;
use ral_core::types::{Settled, Shell, Value};
use ral_core::{Comp, CompileOutcome, builtins, compile_and_typecheck};
use std::sync::{Arc, Mutex};

fn fresh_shell() -> Shell {
    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());
    shell
}

fn top_level(shell: &mut Shell, source: &str) -> Settled<Value> {
    let comp: Arc<Comp> = match compile_and_typecheck(source, shell.session_schemes()) {
        CompileOutcome::Compiled(c) => Arc::new(c),
        CompileOutcome::Parse(e) => panic!("parse: {source:?}: {e}"),
        CompileOutcome::Types(errs) => {
            let msgs: Vec<_> = errs.iter().map(|e| e.kind.render_message()).collect();
            panic!("type: {source:?}: {}", msgs.join("; "));
        }
    };
    evaluator::eval_top_level(&comp, shell)
}

/// Install a sink that records every surfaced value, returning the shared
/// buffer so the test can inspect what arrived.
fn recording_shell() -> (Shell, Arc<Mutex<Vec<Value>>>) {
    let mut shell = fresh_shell();
    let log: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    shell.set_surface(Arc::new(move |v| sink.lock().unwrap().push(v)));
    (shell, log)
}

/// With no sink installed `surface` returns Unit and is otherwise inert.
#[test]
fn surface_without_sink_is_identity() {
    let mut shell = fresh_shell();
    let out = top_level(&mut shell, r#"surface `task [status: "open", desc: "x"]"#);
    assert_eq!(out.expect("surface should succeed"), Value::Unit);
}

/// The exact variant the body constructs reaches the installed sink.
#[test]
fn surface_forwards_the_event_to_the_sink() {
    let (mut shell, log) = recording_shell();
    let out = top_level(
        &mut shell,
        r#"surface `meter [done: 1, total: 3, label: "tasks"]"#,
    );
    assert_eq!(out.expect("surface should succeed"), Value::Unit);

    let events = log.lock().unwrap();
    assert_eq!(events.len(), 1, "exactly one event surfaced");
    let Value::Variant { label, payload } = &events[0] else {
        panic!("expected a variant, got {:?}", events[0]);
    };
    assert_eq!(label, "meter");
    let Some(payload) = payload.as_deref() else {
        panic!("expected a payload record");
    };
    let Value::Map(rec) = payload else {
        panic!("expected a record payload, got {payload:?}");
    };
    assert_eq!(rec.get("done"), Some(&Value::Int(1)));
    assert_eq!(rec.get("total"), Some(&Value::Int(3)));
    assert_eq!(rec.get("label"), Some(&Value::String("tasks".into())));
}

/// The sink is inherited into thunk bodies, so an event surfaced from
/// inside a called closure still reaches the host.
#[test]
fn surface_reaches_through_a_thunk_body() {
    let (mut shell, log) = recording_shell();
    let out = top_level(
        &mut shell,
        r#"let emit = { |d| surface `task [status: "done", desc: $d] }
emit "ship it""#,
    );
    out.expect("surface from a thunk should succeed");

    let events = log.lock().unwrap();
    assert_eq!(events.len(), 1, "the thunk's event reached the sink");
    let Value::Variant { label, .. } = &events[0] else {
        panic!("expected a variant");
    };
    assert_eq!(label, "task");
}
