//! One external command is one fact, whether it ran alone or as a pipeline
//! stage.  Both doors mint their `Observed::Command` through
//! `evaluator::audit::command_fact` and both leave the emission to the one
//! gated door, so the two must agree on the argv they spell *and* on whether
//! anything is emitted at all under a given sink/trail configuration.
//!
//! Every assertion here compares the staged run against the standalone one
//! rather than against a literal: the test's job is to fail when a fourth
//! construction site diverges, whatever it spells.
//!
//! Unix-only: both doors need a real external, and the paths named here are
//! `/bin`.

#![cfg(unix)]
#![allow(clippy::disallowed_methods)]

mod common;

use common::fresh_shell;

use ral_core::protocol::Run;
use ral_core::run::{RunReport, RunRequest};
use ral_core::types::{Map, Observation, Settled, Value};
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

/// Run one run of `source`, with a surface sink installed or not, returning
/// the settled value and whatever reached the sink.
fn run(source: &str, sink: bool) -> (Settled<Value>, Vec<Value>) {
    let log: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder: SurfaceSink = Arc::new(Recorder(Arc::clone(&log)));
    let mut shell = fresh_shell();
    let result = match shell.run(RunRequest {
        surface: sink.then_some(recorder),
        ..RunRequest::from(Run::foreground(source, "<test>"))
    }) {
        RunReport::Ran { ending, .. } => ending.into_result(),
        RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    };
    let events = log.lock().unwrap().clone();
    (result, events)
}

/// The `command` facts among a stream of observation records, in the order
/// they were reported.
fn command_facts(observations: &[Value]) -> Vec<Map> {
    observations
        .iter()
        .filter_map(|v| match v {
            Value::Map(m) => match m.get("what").as_deref() {
                Some(Value::Variant {
                    label,
                    payload: Some(fact),
                }) if label.as_ref() == "command" => match fact.as_ref() {
                    Value::Map(fact) => Some(fact.clone()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The trail `audit { … }` returns, as the same observation records the sink
/// would have received.
fn trail_of(report: &Value) -> Vec<Value> {
    let Value::Map(report) = report else {
        panic!("audit returns a record, got {report:?}");
    };
    match report.get("trail").as_deref() {
        Some(Value::List(items)) => items.iter().map(std::borrow::Cow::into_owned).collect(),
        other => panic!("audit's report carries a trail list, got {other:?}"),
    }
}

/// The three fields a reader of a command fact judges it by.
fn shape(fact: &Map) -> (Option<Value>, Option<Value>, Option<Value>) {
    (
        fact.get("argv").map(std::borrow::Cow::into_owned),
        fact.get("origin").map(std::borrow::Cow::into_owned),
        fact.get("status").map(std::borrow::Cow::into_owned),
    )
}

/// The same external, alone and as the head of a pipeline: the head's fact
/// must be the standalone's fact — the whole argv, not just the program.
#[test]
fn a_pipeline_stage_reports_the_same_command_fact_as_a_standalone_run() {
    let (alone_result, alone_events) = run("/bin/echo hi", true);
    alone_result.expect("/bin/echo should succeed");
    let alone = command_facts(&alone_events);
    assert_eq!(alone.len(), 1, "one dispatch, one fact: {alone_events:?}");

    let (staged_result, staged_events) = run("/bin/echo hi | /bin/cat", true);
    staged_result.expect("the pipeline should succeed");
    let staged = command_facts(&staged_events);
    let head = staged
        .first()
        .expect("a stage that ran must report its own fact, as a standalone run does");

    assert_eq!(
        shape(head),
        shape(&alone[0]),
        "the head stage must spell what the standalone dispatch spelled"
    );
}

/// The gate too: with a trail open and no sink, and with a sink and no trail,
/// the staged run must be as loud as the standalone one.
#[test]
fn a_pipeline_stage_and_a_standalone_run_agree_on_when_to_report() {
    for sink in [false, true] {
        let (alone_report, alone_events) = run("audit { /bin/echo hi }", sink);
        let alone_report = alone_report.expect("the audited standalone run should succeed");
        let (staged_report, staged_events) = run("audit { /bin/echo hi | /bin/cat }", sink);
        let staged_report = staged_report.expect("the audited pipeline should succeed");

        let alone_trail = command_facts(&trail_of(&alone_report));
        let staged_trail = command_facts(&trail_of(&staged_report));
        assert_eq!(
            shape(staged_trail.first().expect("the head stage's own fact")),
            shape(alone_trail.first().expect("the standalone dispatch's fact")),
            "trail parity fails with sink={sink}"
        );

        assert_eq!(
            command_facts(&staged_events).is_empty(),
            command_facts(&alone_events).is_empty(),
            "with sink={sink} the two doors must agree on whether anything surfaces"
        );
    }
}
