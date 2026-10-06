#![allow(clippy::disallowed_methods)]

//! The `surface` effect: a value handed to the builtin reaches the
//! host-installed sink unchanged, and with no sink installed the builtin
//! is the identity.  Drives the public `Shell::run` door the
//! same way `ral` and `exarch` do — including installing the name, which
//! core withholds for each host to carry.

mod common;

use common::prelude;

use ral_core::boot::{HostSurface, boot_shell};
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::untag;
use ral_core::protocol::Run;
use ral_core::run::{RunReport, RunRequest};
use ral_core::terminal::TerminalState;
use ral_core::types::{Observation, Settled, Shell, Value};
use ral_core::{EventSink, SurfaceSink};
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
        .filter(|ev| !matches!(untag(ev), Some((Observation::SURFACE_TAG, _))))
        .cloned()
        .collect()
}

/// Run one run of `source` with an optional surface sink, returning the
/// settled value.  These sources are well-formed, so a static diagnostic is
/// a test bug.
fn run(shell: &mut Shell, source: &str, surface: Option<SurfaceSink>) -> Settled<Value> {
    match shell.run(RunRequest {
        surface,
        ..RunRequest::from(Run::foreground(source, "<test>"))
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
