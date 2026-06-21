#![allow(clippy::disallowed_methods)]

//! The `surface` effect: a value handed to the builtin reaches the
//! host-installed sink unchanged, and with no sink installed the builtin
//! is the identity.  Drives the public `Shell::run_source_turn` boundary the
//! same way `ral` and `exarch` do.

mod common;

use ral_core::types::{Capabilities, Settled, Shell, Value};
use ral_core::{
    EventSink, RequestedTerminalAccess, SurfaceSink, TurnIo, TurnReport, TurnRequest, TurnStdin,
    builtins,
};
use std::sync::{Arc, Mutex};

fn fresh_shell() -> Shell {
    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());
    shell
}

/// A sink that records every surfaced value.
struct Recorder(Arc<Mutex<Vec<Value>>>);

impl EventSink for Recorder {
    fn emit(&self, ev: &Value) {
        self.0.lock().unwrap().push(ev.clone());
    }
}

/// A fresh recording sink plus the shared buffer the test inspects.
fn recording() -> (Arc<Mutex<Vec<Value>>>, SurfaceSink) {
    let log: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink: SurfaceSink = Arc::new(Recorder(Arc::clone(&log)));
    (log, sink)
}

/// Run one turn of `source` with an optional surface sink, returning the
/// settled value.  These sources are well-formed, so a static diagnostic is
/// a test bug.
fn run(shell: &mut Shell, source: &str, surface: Option<SurfaceSink>) -> Settled<Value> {
    match shell.run_source_turn(
        source,
        TurnRequest {
            script_name: "<test>",
            caps: Capabilities::root(),
            turn_limit: None,
            detached_limit: None,
            io: TurnIo::Inherit,
            terminal: RequestedTerminalAccess::Leased,
            stdin: TurnStdin::Inherit,
            surface,
            lifecycle: Box::new(()),
        },
    ) {
        TurnReport::Ran { result, .. } => result,
        TurnReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    }
}

/// With no sink installed `surface` returns Unit and is otherwise inert.
#[test]
fn surface_without_sink_is_identity() {
    let mut shell = fresh_shell();
    let out = run(
        &mut shell,
        r#"surface `task [status: "open", desc: "x"]"#,
        None,
    );
    assert_eq!(out.expect("surface should succeed"), Value::Unit);
}

/// The exact variant the body constructs reaches the installed sink.
#[test]
fn surface_forwards_the_event_to_the_sink() {
    let mut shell = fresh_shell();
    let (log, sink) = recording();
    let out = run(
        &mut shell,
        r#"surface `meter [done: 1, total: 3, label: "tasks"]"#,
        Some(sink),
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
    let mut shell = fresh_shell();
    let (log, sink) = recording();
    let out = run(
        &mut shell,
        r#"let emit = { |d| surface `task [status: "done", desc: $d] }
emit "ship it""#,
        Some(sink),
    );
    out.expect("surface from a thunk should succeed");

    let events = log.lock().unwrap();
    assert_eq!(events.len(), 1, "the thunk's event reached the sink");
    let Value::Variant { label, .. } = &events[0] else {
        panic!("expected a variant");
    };
    assert_eq!(label, "task");
}
