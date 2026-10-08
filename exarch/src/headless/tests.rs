use super::*;
use crate::agent::spawn::{AsyncSpawn, spawn_async};
use crate::agent::{RootConfig, RootSeat, SPAWN_FUEL, Trunk};
use crate::bus::Signal;
use crate::provider::Toolset;
use crate::provider::scripted::{Reply, Script};
use crate::record::AgentState;
use crate::record::RecordedAccount;
use crate::record::{Display, Record};
use std::sync::Arc;

/// A fresh conversing trunk over a scripted provider, in a throwaway run
/// dir beside its scratch — both go when the trunk does.
fn converse_trunk(tag: &str, script: Script) -> Avatar {
    let scratch =
        Arc::new(crate::app::Scratch::for_test(crate::app::EXARCH, tag).expect("scratch dir"));
    let dir = scratch.test_sibling("run").expect("temp run dir");
    Avatar::root(
        RootConfig {
            system: "system".into(),
            caps: ral_core::capability::GrantStack::root(),
            run_dir: dir,
            account: RecordedAccount::for_test("test"),
            trunk: Trunk::Embedded,
            tools: Toolset::offered(false),
            allow_schedule: false,
            resume_on_reset: false,
            disk_warn_bytes: None,
            fuel: 0,
            egress: crate::egress::Egress::for_test(),
            dial: None,
            bureau: Arc::new(crate::provider::Bureau::Scripted),
        },
        RootSeat::Identity {
            scratch,
            cwd: std::env::current_dir().expect("test process has a cwd"),
            terminal: ral_core::terminal::TerminalState::default(),
        },
        Arc::new(Provider::scripted("test-model", script)),
    )
    .expect("root trunk")
}

/// A conversation, not a sequence of one-shot runs: the second exchange's
/// rendered context must carry the first's user text and reply.
#[test]
fn converse_carries_context_from_one_exchange_into_the_next() {
    let mut session = converse_trunk(
        "context",
        Script::new()
            .then(Reply::text("hello back"))
            .then(Reply::text("and again")),
    );
    converse(&mut session, "first message".into())
        .expect("a conversing trunk never fails for want of a reply");
    converse(&mut session, "second message".into())
        .expect("the second exchange runs on the same, unbroken session");

    let rendered = format!("{:?}", session.rendered_messages());
    assert!(rendered.contains("first message"), "{rendered}");
    assert!(rendered.contains("hello back"), "{rendered}");
    assert!(rendered.contains("second message"), "{rendered}");
}

/// The conversing trunk parks rather than returning: no payload rises, and
/// the digest never wears `Replied`, the tag only a deliberate `reply` earns.
#[test]
fn the_interactive_trunk_parks_rather_than_replying() {
    let mut session = converse_trunk("parks", Script::new().then(Reply::text("hi there")));
    session.seed("hello".into());
    let (tx, _rx) = crate::bus::channel();
    let emit = crate::bus::Emitter::new(tx, session.agent.id);
    let outcome = session.attend_backlog(&emit);
    assert!(
        session.agent.reply().is_none(),
        "a conversing trunk deposits no reply"
    );
    assert!(
        !matches!(outcome, AgentOutcome::Replied),
        "a conversing trunk never completes by returning a value: {outcome:?}"
    );
}

/// `pump` absorbs the unwind and returns normally, so `run` builds its
/// result from `Ok(())`; only the recorded `Forensic::Error` whose text
/// carries [`crate::bus::WORKER_PANIC_PREFIX`] betrays the panic, latched
/// in `print_block`'s `K::Error` arm.
#[test]
fn recovered_worker_panic_reports_error_not_success() {
    use crate::record::{Forensic, Locus, Record, Recorded, Seq};
    let root: AgentId = AgentId::new(1);
    let mut sink_out = Vec::new();
    let mut sink_err = Vec::new();
    let mut h = Headless::new(Projection::HeadlessJson, root, &mut sink_out, &mut sink_err);
    h.accept(Signal::Fact(
        root,
        Recorded::new(
            Locus::placeholder(Seq::new(1)),
            Record::Forensic(Forensic::Error {
                text: format!("{}boom", crate::bus::WORKER_PANIC_PREFIX),
            }),
        ),
    ));
    let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
    let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
    assert_eq!(v["is_error"], serde_json::json!(true), "{out}");
    assert_eq!(v["stop_reason"], serde_json::json!("panicked"), "{out}");
}

/// A `Display::Turn` fact through the real live path — `Sink::accept`,
/// which `drive` calls in production — folded into blocks and drawn from
/// there.  A child's own turns never count toward root's.
#[test]
fn num_turns_counts_root_turns() {
    use crate::record::{Display, Locus, Record, Recorded, Seq};
    let root: AgentId = AgentId::new(1);
    let sub: AgentId = AgentId::new(2);
    let mut sink_out = Vec::new();
    let mut sink_err = Vec::new();
    let mut h = Headless::new(Projection::HeadlessJson, root, &mut sink_out, &mut sink_err);
    let mut seq = 0u64;
    let mut turn_started = |id: AgentId, turn: u64| {
        seq += 1;
        Signal::Fact(
            id,
            Recorded::new(
                Locus::placeholder(Seq::new(seq)),
                Record::Display(Display::Turn { id: turn }),
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
    use crate::record::{Display, Locus, Record, Recorded, Seq};
    let root: AgentId = AgentId::new(1);
    let mut sink_out = Vec::new();
    let mut sink_err = Vec::new();
    let mut h = Headless::new(Projection::HeadlessText, root, &mut sink_out, &mut sink_err);
    let card = Card(vec![Mark::Raw {
        bytes: b"a rendered surface".to_vec(),
    }]);
    h.accept(Signal::Fact(
        root,
        Recorded::new(
            Locus::placeholder(Seq::new(1)),
            Record::Display(Display::Card { card }),
        ),
    ));
    let err = String::from_utf8_lossy(&sink_err);
    assert!(
        err.contains("a rendered surface"),
        "the fact's card must reach stderr: {err:?}"
    );
}

/// A seam fault reaches stderr from any source agent, root or a child —
/// it names a plumbing failure, not a fact about one agent's session.
#[test]
fn a_transient_fault_reaches_stderr_from_any_agent() {
    let root: AgentId = AgentId::new(1);
    let sub: AgentId = AgentId::new(2);
    let mut sink_out = Vec::new();
    let mut sink_err = Vec::new();
    let mut h = Headless::new(Projection::HeadlessText, root, &mut sink_out, &mut sink_err);
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
    let mut h = Headless::new(Projection::HeadlessJson, root, &mut sink_out, &mut sink_err);
    h.reply = Some(ral_core::first_order::FOValue::Map {
        entries: vec![(
            "files".into(),
            ral_core::first_order::FOValue::List {
                items: vec![
                    ral_core::first_order::FOValue::String {
                        value: "a.rs".into(),
                    },
                    ral_core::first_order::FOValue::String {
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
    let h = Headless::new(Projection::HeadlessJson, root, &mut sink_out, &mut sink_err);
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
    let mut h = Headless::new(Projection::HeadlessJson, root, &mut sink_out, &mut sink_err);
    let payload = ral_core::first_order::FOValue::String {
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
    let mut h = Headless::new(Projection::HeadlessJson, root, &mut sink_out, &mut sink_err);
    let payload = ral_core::first_order::FOValue::Map {
        entries: vec![(
            "files".to_string(),
            ral_core::first_order::FOValue::List {
                items: vec![
                    ral_core::first_order::FOValue::String {
                        value: "a.rs".into(),
                    },
                    ral_core::first_order::FOValue::String {
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
        &mut converse_trunk("blank-seed", Script::new()),
        &SessionInfo {
            system_size: 0,
            system_files: &[],
            base: "dangerous",
            extend_base: None,
            restrict_files: &[],
            cwd: "/tmp",
            resumed: None,
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

/// A host's own writers get the same split `converse` gives the process's
/// stdout/stderr — reply text on `out`, breadcrumbs on `err`.
#[test]
fn converse_on_projects_into_the_given_writers() {
    let mut session = converse_trunk("writers", Script::new().then(Reply::text("hi there")));
    let mut out = Vec::new();
    let mut err = Vec::new();
    converse_on(&mut session, "hello".into(), &mut out, &mut err)
        .expect("a conversing trunk never fails for want of a reply");
    assert!(
        String::from_utf8_lossy(&out).contains("hi there"),
        "reply text must land on `out`: {:?}",
        String::from_utf8_lossy(&out)
    );
    // One exchange spends two ids: the prompt takes 1, the reply 2.
    assert!(
        String::from_utf8_lossy(&err).contains("[turn 2]"),
        "turn breadcrumbs must land on `err`: {:?}",
        String::from_utf8_lossy(&err)
    );
}

// ── `converse_settled`: the quiescent exchange driver ──────────────────

/// A conversing trunk with real spawn fuel, so a test may register a
/// genuine live child under it — [`converse_trunk`]'s `fuel: 0` exists
/// precisely to refuse that.
fn settled_trunk(tag: &str, script: Script, allow_schedule: bool, resume_on_reset: bool) -> Avatar {
    let scratch =
        Arc::new(crate::app::Scratch::for_test(crate::app::EXARCH, tag).expect("scratch dir"));
    let dir = scratch.test_sibling("run").expect("temp run dir");
    Avatar::root(
        RootConfig {
            system: "system".into(),
            caps: ral_core::capability::GrantStack::root(),
            run_dir: dir,
            account: RecordedAccount::for_test("test"),
            trunk: Trunk::Embedded,
            tools: Toolset::offered(false),
            allow_schedule,
            resume_on_reset,
            disk_warn_bytes: None,
            fuel: SPAWN_FUEL,
            egress: crate::egress::Egress::for_test(),
            dial: None,
            bureau: Arc::new(crate::provider::Bureau::Scripted),
        },
        RootSeat::Identity {
            scratch,
            cwd: std::env::current_dir().expect("test process has a cwd"),
            terminal: ral_core::terminal::TerminalState::default(),
        },
        Arc::new(Provider::scripted("test-model", script)),
    )
    .expect("root trunk")
}

/// A live child of `parent` with no attend loop of its own. The caller
/// holds it — that is what keeps it live — and drops it on its own clock,
/// so a test can hold `parent` on it for exactly as long as it chooses,
/// deterministically, rather than racing a real child's own thread against
/// a sleep.
fn live_child(parent: &Avatar, name: &str) -> Avatar {
    parent.fork_named(name).expect("fork child")
}

/// Every signal a caller's own `Sink` can receive, folded into one place —
/// the seam's own `Record`/`Transient`.
#[derive(Default)]
struct Collecting {
    facts: Vec<Record>,
    transients: Vec<Transient>,
}
impl Sink for Collecting {
    fn fact(&mut self, _id: AgentId, rec: &Recorded<Record>) {
        self.facts.push(rec.value().clone());
    }
    fn transient(&mut self, _id: AgentId, t: &Transient) {
        self.transients.push(t.clone());
    }
}

/// Signals `release` the first time it sees [`AgentState::WaitingOnAgents`]
/// pass through, so a test's own background thread can settle its child
/// exactly once the exchange has genuinely parked on it — never before,
/// and with no sleep to race.
struct SignalOnWaiting<S> {
    inner: S,
    release: std::sync::mpsc::SyncSender<()>,
}

impl<S: Sink> Sink for SignalOnWaiting<S> {
    fn fact(&mut self, id: AgentId, rec: &Recorded<Record>) {
        self.inner.fact(id, rec);
    }
    fn transient(&mut self, id: AgentId, t: &Transient) {
        if matches!(t, Transient::State(AgentState::WaitingOnAgents)) {
            let _ = self.release.try_send(());
        }
        self.inner.transient(id, t);
    }
}

/// Law B, exercised deterministically: the exchange holds open while a
/// live child is registered but not yet settled, and only completes the
/// settlement — pushing its result and retiring itself —
/// once the sink has observed the exchange genuinely park on it
/// ([`AgentState::WaitingOnAgents`]). A driver that quiesced without
/// waiting would never signal the release, and the call would hang
/// rather than pass, so this also proves the hold is real.
#[test]
fn converse_settled_holds_for_a_live_child_then_quiesces() {
    let mut session = settled_trunk(
        "slow-child",
        Script::new()
            .then(Reply::text("on it"))
            .then(Reply::text("thanks for the update")),
        false,
        false,
    );
    let child = live_child(&session, "helper");
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let settler = std::thread::spawn(move || {
        release_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the exchange must park on the child within the timeout");
        child.settle(AgentOutcome::Stopped("done".into()));
    });

    let mut sink = SignalOnWaiting {
        inner: Collecting::default(),
        release: release_tx,
    };
    converse_settled(&mut session, "please help".into(), &mut sink)
        .expect("the exchange itself must not fail for want of a reply");
    settler.join().expect("the settling thread must not panic");

    assert!(
        sink.inner
            .transients
            .iter()
            .any(|t| matches!(t, Transient::State(AgentState::WaitingOnAgents))),
        "parking on a live child must emit WaitingOnAgents"
    );
    assert!(
        sink.inner.facts.iter().any(|f| matches!(
            f,
            Record::Display(Display::SubagentDone { name, error, .. })
                if name == "helper" && error.as_deref() == Some("done")
        )),
        "the settled child's end must reach the exchange's own sink"
    );
}

/// A returning child that finishes without ever calling `reply` fails
/// honestly — its `SubagentDone` names the failure — and the exchange
/// still ends rather than hanging on a child that will never settle
/// differently. Driven through the real fork/`spawn_async`/`attend` spine
/// rather than a hand-fed outcome, so the nudge-then-fail behavior itself
/// is genuine, not asserted; the child's own script has no tool calls, so
/// nothing races the parent's independent one.
#[test]
fn converse_settled_ends_even_when_a_child_never_replies() {
    let mut session = settled_trunk(
        "child-no-reply",
        Script::new()
            .then(Reply::text("on it"))
            .then(Reply::text("thanks for the update")),
        false,
        false,
    );
    let mut no_reply = Script::new();
    for _ in 0..8 {
        no_reply = no_reply.then(Reply::text("prose, but never a reply"));
    }
    let child = session.fork_named("flaky").expect("fork child");
    crate::agent::testkit::set_provider(
        &child,
        crate::agent::testkit::scripted("test-model", no_reply),
    );
    child.seed("go".into());
    let (spawn_emit, _rx) = crate::bus::dummy_emitter();
    spawn_async(
        child,
        AsyncSpawn {
            name: "flaky".into(),
            prompt: None,
        },
        &spawn_emit,
    )
    .expect("spawn must succeed");

    let mut sink = Collecting::default();
    converse_settled(&mut session, "please help".into(), &mut sink)
        .expect("the exchange itself must not fail merely because a child did");

    assert!(
        sink.facts.iter().any(|f| matches!(
            f,
            Record::Display(Display::SubagentDone { name, error, .. })
                if name == "flaky" && error.is_some()
        )),
        "the child's un-replied finish must surface as a failed SubagentDone"
    );
}

/// A trunk that arms its own wakeups is refused at construction, the same
/// class of refusal as a missing dialler: an armed wakeup may fire with
/// nothing left to wait it out once the fleet quiesces.
#[test]
fn converse_settled_refuses_a_trunk_that_arms_its_own_wakeups() {
    for (tag, allow_schedule, resume_on_reset) in [
        ("allow-schedule", true, false),
        ("resume-on-reset", false, true),
    ] {
        let mut session = settled_trunk(tag, Script::new(), allow_schedule, resume_on_reset);
        let mut sink = Collecting::default();
        let err = converse_settled(&mut session, "hello".into(), &mut sink)
            .expect_err("a trunk that arms its own wakeups must be refused, not run");
        assert!(
            err.contains("wakeup"),
            "the refusal must name what it refuses: {err}"
        );
        assert!(
            sink.facts.is_empty() && sink.transients.is_empty(),
            "a refused construction must touch no bus"
        );
    }
}
