use super::*;
use crate::agent::testkit::{self, ral_call, scripted, set_provider};
use crate::agent::{Agent, Trunk};
use crate::provider::scripted::{Reply, Script};
use crate::record::{Commit, Display, Forensic, NonEmpty, Transcript};
use std::time::Duration;

mod host;
mod late_failure;

const WAIT: Duration = Duration::from_secs(5);

fn frontend_root(script: Script, trunk: Trunk) -> Agent {
    let mut root = testkit::trunk(trunk);
    set_provider(&mut root, scripted("test-model", script));
    root
}

fn frontend_host(script: Script, trunk: Trunk) -> Host {
    Host::adopt(vec![(frontend_root(script, trunk), None)]).expect("frontend host")
}

fn reply_script(value: &str) -> Script {
    Script::new().then(Reply::tool_calls(vec![ral_call(
        "reply",
        &format!("exarch-agents `reply {value}"),
    )]))
}

fn no_reply_script() -> Script {
    (0..4).fold(Script::new(), |script, _| {
        script.then(Reply::text("prose, but never a reply"))
    })
}

fn run_frontend(host: &Host, format: OutputFormat) -> Result<(), String> {
    run(
        host,
        &SessionInfo {
            system_size: 0,
            system_files: &[],
            base: "dangerous",
            extend: None,
            restrict_files: &[],
            cwd: &ral_core::host::cwd().expect("test process has a cwd"),
        },
        &Provider::scripted("test-model", Script::new()),
        Some("please work".into()),
        format,
    )
}

fn receive_until(host: &Host, mut done: impl FnMut(&Signal) -> bool) -> Vec<Signal> {
    let deadline = Instant::now() + WAIT;
    let mut signals = Vec::new();
    loop {
        let signal = host
            .feed()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("host must publish the expected signal before the deadline");
        let finished = done(&signal);
        signals.push(signal);
        if finished {
            return signals;
        }
    }
}

fn has_fact(signal: &Signal, id: AgentId, predicate: impl Fn(&Record) -> bool) -> bool {
    matches!(signal, Signal::Commit(agent, commit) if *agent == id
        && commit.records().iter().any(|rec| predicate(rec.value())))
}

fn wait_settled(host: &Host) -> Vec<Signal> {
    let root = host.roots()[0];
    receive_until(host, |signal| {
        has_fact(signal, root, |record| {
            matches!(record, Record::Control(Control::JobSettled { .. }))
        })
    })
}

fn wait_ready(host: &Host) {
    let deadline = Instant::now() + WAIT;
    // Leave both Born and Ready for the frontend.
    while host.feed().depth() < 2 {
        assert!(Instant::now() < deadline, "root did not announce readiness");
        std::thread::yield_now();
    }
}

fn status_signal(id: AgentId, status: ChildStatus) -> Signal {
    Signal::Commit(
        id,
        Commit::placeholder(
            1,
            NonEmpty::one(Record::Control(Control::Status { status })),
        ),
    )
}

/// A recovered worker panic remains an error in the JSON projection.
#[test]
fn recovered_worker_panic_reports_error_not_success() {
    use crate::record::{Commit, Forensic, NonEmpty, Record};
    let root: AgentId = AgentId::new(1);
    let mut sink_out = Vec::new();
    let mut sink_err = Vec::new();
    let mut h = Headless::new(root, &mut sink_out, &mut sink_err);
    h.accept(Signal::Commit(
        root,
        Commit::placeholder(
            1,
            NonEmpty::one(Record::Forensic(Forensic::Error {
                text: format!("{WORKER_PANIC_PREFIX}boom"),
            })),
        ),
    ));
    let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
    let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
    assert_eq!(v["is_error"], serde_json::json!(true), "{out}");
    assert_eq!(v["stop_reason"], serde_json::json!("panicked"), "{out}");
}

/// A `Forensic::TurnStarted` fact through the real live path — `Sink::accept`,
/// used by production — folded into blocks and drawn from
/// there.  A child's own turns never count toward root's.
#[test]
fn num_turns_counts_root_turns() {
    use crate::record::{Commit, Forensic, NonEmpty, Record};
    let root: AgentId = AgentId::new(1);
    let sub: AgentId = AgentId::new(2);
    let mut sink_out = Vec::new();
    let mut sink_err = Vec::new();
    let mut h = Headless::new(root, &mut sink_out, &mut sink_err);
    let mut seq = 0u64;
    let mut turn_started = |id: AgentId, turn: u64| {
        seq += 1;
        Signal::Commit(
            id,
            Commit::placeholder(
                seq,
                NonEmpty::one(Record::Forensic(Forensic::TurnStarted {
                    turn,
                    tuning: crate::provider::Tuning::initial(),
                    via: None,
                })),
            ),
        )
    };
    for turn in [1, 2, 3, 4, 5] {
        h.accept(turn_started(root, turn));
    }
    h.accept(turn_started(sub, 1));
    let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
    let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
    assert_eq!(v["num_turns"], serde_json::json!(5), "{out}");
}

/// A `Display::Card` fact reaches stderr only through the view fold
/// `Sink::accept` drives.
#[test]
fn a_card_fact_reaches_stderr_through_the_view_fold() {
    use crate::record::{Commit, Display, NonEmpty, Record};
    let root: AgentId = AgentId::new(1);
    let mut sink_out = Vec::new();
    let mut sink_err = Vec::new();
    let mut h = Headless::new(root, &mut sink_out, &mut sink_err);
    let card = Card(vec![Mark::Raw {
        bytes: b"a rendered surface".to_vec(),
    }]);
    h.accept(Signal::Commit(
        root,
        Commit::placeholder(1, NonEmpty::one(Record::Display(Display::Card { card }))),
    ));
    let err = String::from_utf8_lossy(&sink_err);
    assert!(
        err.contains("a rendered surface"),
        "the fact's card must reach stderr: {err:?}"
    );
}

/// A terminal recording failure announced by its owner, or a channel elision
/// marker, reaches stderr from any source agent.
#[test]
fn a_transient_fault_reaches_stderr_from_any_agent() {
    let root: AgentId = AgentId::new(1);
    let sub: AgentId = AgentId::new(2);
    let mut sink_out = Vec::new();
    let mut sink_err = Vec::new();
    let mut h = Headless::new(root, &mut sink_out, &mut sink_err);
    h.accept(Signal::Transient(
        sub,
        Transient::Fault {
            text: "disk is full".into(),
        },
    ));
    let err = String::from_utf8_lossy(&sink_err);
    assert!(err.contains("disk is full"), "{err:?}");
}

/// An object stays an object: stringifying it would double-encode the
/// structure, leaving the harness to parse JSON out of a JSON string.
#[test]
fn structured_result_is_faithful_not_stringified() {
    let root: AgentId = AgentId::new(1);
    let mut sink_out = Vec::new();
    let mut sink_err = Vec::new();
    let mut h = Headless::new(root, &mut sink_out, &mut sink_err);
    h.reply = Some(ral_core::data::FOValue::Map {
        entries: vec![(
            "files".into(),
            ral_core::data::FOValue::List {
                items: vec![
                    ral_core::data::FOValue::String {
                        value: "a.rs".into(),
                    },
                    ral_core::data::FOValue::String {
                        value: "b.rs".into(),
                    },
                ],
            },
        )],
    });
    let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
    let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
    assert!(
        v["result"].is_object(),
        "a structured reply stays structured, not stringified: {out}"
    );
    assert_eq!(v["result"]["files"][0], serde_json::json!("a.rs"), "{out}");
    assert_eq!(v["is_error"], serde_json::json!(false), "{out}");
}

/// A root that finished without calling `reply` fails honestly: null
/// `result`, `is_error`, and `no_reply` rather than the flattering
/// "completed".
#[test]
fn no_reply_root_is_error_with_null_result() {
    let root: AgentId = AgentId::new(1);
    let mut sink_out = Vec::new();
    let mut sink_err = Vec::new();
    let h = Headless::new(root, &mut sink_out, &mut sink_err);
    let out = result_json(
        &h,
        &Err("ended without calling `reply`".to_string()),
        std::time::Duration::ZERO,
    );
    let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
    assert_eq!(v["is_error"], serde_json::json!(true), "{out}");
    assert_eq!(v["result"], serde_json::Value::Null, "{out}");
    assert_eq!(v["stop_reason"], serde_json::json!("no_reply"), "{out}");
}

/// Through the real `FOValue -> user_json` projection, not just
/// `result_json`'s JSON-in JSON-out shape: no quoting inside quoting.
#[test]
fn user_json_projected_result_keeps_a_string_reply_raw() {
    let root: AgentId = AgentId::new(1);
    let mut sink_out = Vec::new();
    let mut sink_err = Vec::new();
    let mut h = Headless::new(root, &mut sink_out, &mut sink_err);
    let payload = ral_core::data::FOValue::String {
        value: "plain text reply".into(),
    };
    h.reply = Some(payload);
    let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
    let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
    assert_eq!(v["result"], serde_json::json!("plain text reply"), "{out}");
}

/// The structured half of the same projection: ordinary JSON, not
/// `FOValue`'s internally-tagged transport encoding.
#[test]
fn user_json_projected_result_keeps_structure_ordinary_json() {
    let root: AgentId = AgentId::new(1);
    let mut sink_out = Vec::new();
    let mut sink_err = Vec::new();
    let mut h = Headless::new(root, &mut sink_out, &mut sink_err);
    let payload = ral_core::data::FOValue::Map {
        entries: vec![(
            "files".to_string(),
            ral_core::data::FOValue::List {
                items: vec![
                    ral_core::data::FOValue::String {
                        value: "a.rs".into(),
                    },
                    ral_core::data::FOValue::String {
                        value: "b.rs".into(),
                    },
                ],
            },
        )],
    };
    h.reply = Some(payload);
    let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
    let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
    assert!(v["result"].is_object(), "{out}");
    assert_eq!(v["result"]["files"][0], serde_json::json!("a.rs"), "{out}");
}

/// A seed of whitespace is no seed: [`crate::cli::load_seed`] collapses it,
/// and `run` refuses before the provider is touched, naming both ways to
/// supply one — the only thing a scripted caller has to go on.
#[test]
fn a_blank_seed_is_no_seed_and_the_refusal_names_every_flag() {
    use clap::Parser;
    let cli = crate::cli::Cli::try_parse_from(["exarch", "--headless", "--prompt", "   \n\t"])
        .expect("whitespace is a value, not a parse error");
    let seed = crate::cli::load_seed(cli.prompt, cli.file).expect("no --file to read");
    assert_eq!(seed, None, "a blank seed must never reach the model");

    let err = run(
        &frontend_host(Script::new(), Trunk::Returns),
        &SessionInfo {
            system_size: 0,
            system_files: &[],
            base: "dangerous",
            extend: None,
            restrict_files: &[],
            cwd: &ral_core::host::cwd().expect("test process has a cwd"),
        },
        &Provider::scripted("test-model", Script::new()),
        seed,
        OutputFormat::Text,
    )
    .expect_err("a headless run with no seed has nothing to do");
    assert_eq!(err, "--headless requires a seed prompt: --prompt or --file");

    // The filter trims only to judge: real content keeps its own spacing.
    assert_eq!(
        crate::cli::load_seed(Some(" x ".into()), None)
            .expect("no --file to read")
            .as_deref(),
        Some(" x ")
    );
}

/// A helper's facts never step root's fold nor finish the frontend; only its
/// faults and terminal panics are remembered.
#[test]
fn helper_signals_never_finish_the_frontend() {
    let root = AgentId::new(1);
    let helper = AgentId::new(2);
    let fact = |record| Signal::Commit(helper, Commit::placeholder(1, NonEmpty::one(record)));
    let panic_text = format!("{WORKER_PANIC_PREFIX}helper panic");
    let cases = [
        (
            fact(Record::Display(Display::Card {
                card: Card(vec![Mark::Raw {
                    bytes: b"helper-only card".to_vec(),
                }]),
            })),
            false,
            None,
        ),
        (
            fact(Record::Forensic(Forensic::Error {
                text: panic_text.clone(),
            })),
            false,
            None,
        ),
        (
            status_signal(helper, ChildStatus::Replied(FOValue::Unit)),
            false,
            None,
        ),
        (
            Signal::Transient(helper, Transient::Died { failures: vec![] }),
            false,
            None,
        ),
        (
            Signal::Transient(helper, Transient::StopReason("helper-stop".into())),
            false,
            None,
        ),
        (
            Signal::Transient(
                helper,
                Transient::Fault {
                    text: "helper recording failed".into(),
                },
            ),
            false,
            Some("helper recording failed"),
        ),
        (
            Signal::Transient(
                helper,
                Transient::Died {
                    failures: vec![panic_text],
                },
            ),
            true,
            None,
        ),
    ];
    for (signal, panicked, fault) in cases {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut headless = Headless::new(root, &mut out, &mut err);
        headless.accept(signal);
        assert_eq!(headless.panicked, panicked);
        assert_eq!(headless.fault.as_deref(), fault);
        assert!(!headless.finished());
        assert_eq!(headless.last_stop, None);
        assert!(headless.fold.blocks().is_empty());
        headless.accept(status_signal(root, ChildStatus::Replied(FOValue::Unit)));
        assert!(headless.finished());
        assert_eq!(headless.status, Some(ChildStatus::Replied(FOValue::Unit)));
        if !panicked && fault.is_none() {
            assert!(
                err.is_empty(),
                "helper facts never render into root's stderr"
            );
        }
    }
}

#[test]
fn root_terminal_panic_is_the_verdict_and_json_panic_reason() {
    let root = AgentId::new(1);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut headless = Headless::new(root, &mut out, &mut err);
    let failure = format!("{WORKER_PANIC_PREFIX}whole owner");
    headless.accept(Signal::Transient(
        root,
        Transient::Died {
            failures: vec![failure.clone()],
        },
    ));
    let verdict = headless.verdict();
    assert_eq!(verdict, Err(failure.clone()));
    let json: serde_json::Value =
        serde_json::from_str(&result_json(&headless, &verdict, Duration::ZERO)).unwrap();
    assert_eq!(json["stop_reason"], "panicked");
    assert_eq!(json["error"], failure);
}

#[test]
fn bus_elision_is_visible_but_never_a_headless_failure() {
    let root = AgentId::new(1);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut headless = Headless::new(root, &mut out, &mut err);
    headless.accept(Signal::Transient(
        root,
        Transient::Elided {
            text: "elided".into(),
        },
    ));
    headless.accept(status_signal(root, ChildStatus::Replied(FOValue::Unit)));
    assert_eq!(headless.verdict(), Ok(()));
    assert!(!headless.panicked);
    drop(headless);
    assert!(String::from_utf8(err).unwrap().contains("elided"));
}
