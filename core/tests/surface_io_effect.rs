#![allow(clippy::disallowed_methods)]
#![cfg(unix)]

//! Surface observations: core pushes a plain `Value` onto the run's
//! `surface` sink at every redirect read/write door and every external
//! command completion door — a builtin dispatch never reaches the rail.
//! These tests drive the public `Shell::run` door (exactly as
//! `surface_effect.rs` does) with a recording sink and assert the emitted
//! observation records, each fact tagged by its `what` variant.  Decoding
//! these into cards is exarch's job and out of scope here — we only check the
//! wire shape.

mod common;

use common::fresh_shell;

use ral_core::protocol::Run;
use ral_core::run::{RunReport, RunRequest};
use ral_core::types::{Observation, Settled, Shell, Value};
use ral_core::{EventSink, SurfaceSink};
use std::sync::{Arc, Mutex};

/// A sink that records every surfaced observation as the record a trail
/// carries.
struct Recorder(Arc<Mutex<Vec<Value>>>);

impl EventSink for Recorder {
    fn emit(&self, ev: &ral_core::first_order::FOValue) {
        if let Some(obs) = Observation::from_surface(ev) {
            self.0.lock().unwrap().push(Value::from_datum(obs));
        }
    }
}

fn recording() -> (Arc<Mutex<Vec<Value>>>, SurfaceSink) {
    let log: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink: SurfaceSink = Arc::new(Recorder(Arc::clone(&log)));
    (log, sink)
}

/// Run one run of `source` with a recording surface sink and return the
/// settled value alongside the captured events.  Unlike `surface_effect.rs`'s
/// helper this tolerates a failing run result, since several I/O doors
/// (a failed exec, an aborted write) emit their event *and* surface an
/// error.
fn run(shell: &mut Shell, source: &str) -> (Settled<Value>, Vec<Value>) {
    let (log, sink) = recording();
    let result = match shell.run(RunRequest {
        surface: Some(sink),
        ..RunRequest::from(Run::foreground(source, "<test>"))
    }) {
        RunReport::Ran { ending, .. } => ending.into_result(),
        RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    };
    let events = log.lock().unwrap().clone();
    (result, events)
}

/// The fact of the single observation tagged `kind` among the captured
/// events, asserting exactly one fired.  Core reports every dispatch it makes,
/// so a door test names the kind it is about rather than counting the whole
/// stream.
fn single_observation(events: &[Value], kind: &str) -> ral_core::types::Map {
    let facts: Vec<ral_core::types::Map> = events
        .iter()
        .filter_map(|v| match v {
            Value::Map(m) => match m.get("what").map(std::borrow::Cow::into_owned) {
                Some(Value::Variant {
                    label,
                    payload: Some(fact),
                }) if label.as_ref() == kind => match *fact {
                    Value::Map(fact) => Some(fact),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        facts.len(),
        1,
        "exactly one {kind} observation must fire, got {events:?}"
    );
    facts.into_iter().next().expect("checked above")
}

fn s(v: &str) -> Value {
    Value::string(v)
}

// ── COMMAND door ─────────────────────────────────────────────────────────
//
// A builtin dispatch never reaches the rail — only a command that actually
// crossed a door (external, detached) surfaces.  Redirect and command
// observations in the audit trail are goldened in `tests/unix/audit-trail.ral`;
// this one pins delivery through the surface sink itself.

/// A bare external command (`/usr/bin/true`) emits one command observation
/// with argv [program], origin "external", status 0.
#[test]
fn external_success_emits_command_observation() {
    let mut shell = fresh_shell();
    let (result, events) = run(&mut shell, "/usr/bin/true");
    result.expect("/usr/bin/true should succeed");

    let m = single_observation(&events, "command");
    assert_eq!(
        m.get("argv").as_deref(),
        Some(&Value::list(vec![s("/usr/bin/true")]))
    );
    assert_eq!(
        m.get("origin").as_deref(),
        Some(&Value::variant("external", None))
    );
    assert_eq!(m.get("status").as_deref(), Some(&Value::Int(0)));
}
