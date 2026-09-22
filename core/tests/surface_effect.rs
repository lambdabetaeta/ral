#![allow(clippy::disallowed_methods)]

//! The `surface` effect: a value handed to the builtin reaches the
//! host-installed sink unchanged, and with no sink installed the builtin
//! is the identity.  Drives the public `Shell::run` door the
//! same way `ral` and `exarch` do — including installing the name, which
//! core withholds for each host to carry.

mod common;

use common::prelude;

use ral_core::boot::{HostSurface, boot_shell};
use ral_core::io::TerminalState;
use ral_core::protocol::{Program, Run};
use ral_core::serial::FOValue;
use ral_core::types::{GrantStack, Settled, Shell, Value};
use ral_core::{
    EventSink, RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, SurfaceSink,
};
use std::sync::{Arc, Mutex};

/// A shell booted the way a host with a rail boots one: core's surface plus
/// the withheld `surface` name.
fn host_shell() -> Shell {
    boot_shell(
        TerminalState::default(),
        prelude(),
        &HostSurface {
            statics: vec![ral_core::builtins::SURFACE_BUILTIN],
            captured: Vec::new(),
        },
    )
}

/// A sink that records every surfaced value.
struct Recorder(Arc<Mutex<Vec<FOValue>>>);

impl EventSink for Recorder {
    fn emit(&self, ev: &FOValue) {
        self.0.lock().unwrap().push(ev.clone());
    }
}

/// A fresh recording sink plus the shared buffer the test inspects.
fn recording() -> (Arc<Mutex<Vec<FOValue>>>, SurfaceSink) {
    let log: Arc<Mutex<Vec<FOValue>>> = Arc::new(Mutex::new(Vec::new()));
    let sink: SurfaceSink = Arc::new(Recorder(Arc::clone(&log)));
    (log, sink)
}

/// The kit's own surfaces.  Core reports an observation per dispatch on the
/// same sink; this file is about what the `surface` builtin forwards.
fn kit_events(events: &[FOValue]) -> Vec<FOValue> {
    events
        .iter()
        .filter(|ev| ev.field("what").is_none())
        .cloned()
        .collect()
}

/// Run one run of `source` with an optional surface sink, returning the
/// settled value.  These sources are well-formed, so a static diagnostic is
/// a test bug.
fn run(shell: &mut Shell, source: &str, surface: Option<SurfaceSink>) -> Settled<Value> {
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
        surface,
        deferred: None,
        desk: None,
        fork: None,
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran { ending, .. } => ending.into_result(),
        RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    }
}

/// With no sink installed `surface` returns Unit and is otherwise inert.
#[test]
fn surface_without_sink_is_identity() {
    let mut shell = host_shell();
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
    let mut shell = host_shell();
    let (log, sink) = recording();
    let out = run(
        &mut shell,
        r#"surface `meter [done: 1, total: 3, label: "tasks"]"#,
        Some(sink),
    );
    assert_eq!(out.expect("surface should succeed"), Value::Unit);

    let events = kit_events(&log.lock().unwrap());
    assert_eq!(events.len(), 1, "exactly one event surfaced");
    let FOValue::Variant { label, payload } = &events[0] else {
        panic!("expected a variant, got {:?}", events[0]);
    };
    assert_eq!(label, "meter");
    let Some(payload) = payload.as_deref() else {
        panic!("expected a payload record");
    };
    assert_eq!(payload.field("done").and_then(FOValue::as_int), Some(1));
    assert_eq!(payload.field("total").and_then(FOValue::as_int), Some(3));
    assert_eq!(
        payload.field("label").and_then(FOValue::as_str),
        Some("tasks")
    );
}

/// The sink is inherited into thunk bodies, so an event surfaced from
/// inside a called closure still reaches the host.
#[test]
fn surface_reaches_through_a_thunk_body() {
    let mut shell = host_shell();
    let (log, sink) = recording();
    let out = run(
        &mut shell,
        r#"let emit = { |d| surface `task [status: "done", desc: $d] }
emit "ship it""#,
        Some(sink),
    );
    out.expect("surface from a thunk should succeed");

    let events = kit_events(&log.lock().unwrap());
    assert_eq!(events.len(), 1, "the thunk's event reached the sink");
    let FOValue::Variant { label, .. } = &events[0] else {
        panic!("expected a variant");
    };
    assert_eq!(label, "task");
}
