use super::*;
use crate::agent::spawn::{AsyncSpawn, spawn_async};
use crate::agent::{RootConfig, RootEngine, SPAWN_FUEL, Trunk};
use crate::bus::Signal;
use crate::provider::Toolset;
use crate::provider::scripted::{Reply, Script};
use crate::record::AgentState;
use crate::record::{Display, Record};
use ral_core::process::DurableRoot;
use std::sync::Arc;

/// A fresh conversing trunk over a scripted provider, in a throwaway run
/// dir beside its scratch — both go when the trunk does.
fn converse_trunk(tag: &str, script: Script) -> Agent {
    frontend_trunk(tag, script, Trunk::Embedded)
}

fn frontend_trunk(tag: &str, script: Script, trunk: Trunk) -> Agent {
    let scratch =
        Arc::new(crate::app::Scratch::for_test(crate::app::EXARCH, tag).expect("scratch dir"));
    let dir = scratch.test_sibling("run").expect("temp run dir");
    Agent::root(
        RootConfig {
            system: "system".into(),
            run_dir: dir,
            trunk,
            tools: Toolset::offered(false),
            allow_schedule: false,
            resume_on_reset: false,
            disk_warn_bytes: None,
            fuel: 0,
            search: true,
            factory: None,
            bureau: Arc::new(crate::provider::Bureau::Scripted),
            roles: crate::provider::roles::Roles::default(),
        },
        RootEngine::Identity {
            scratch,
            cwd: ral_core::host::cwd().expect("test process has a cwd"),
            grants: ral_core::authority::policy::GrantStack::default(),
            terminal: ral_core::io::terminal::TerminalState::default(),
        },
        Arc::new(Provider::scripted("test-model", script)),
    )
    .expect("root trunk")
}

/// A conversation, not a sequence of one-shot runs: the second exchange's
/// rendered context must carry the first's user text and reply.
#[test]
fn converse_carries_context_from_one_exchange_into_the_next() {
    let session = converse_trunk(
        "context",
        Script::new()
            .then(Reply::text("hello back"))
            .then(Reply::text("and again")),
    );
    let mut settled = SettledConversation::new(session).unwrap();
    let mut sink = Collecting::default();
    for message in ["first message", "second message"] {
        settled.handle().steer(message.into()).unwrap();
        let job = settled.await_job().expect("a queued message opens a job");
        assert!(settled.run(job, &mut sink).outcome.is_ok());
    }

    let rendered = format!("{:?}", settled.root.rendered_messages());
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
    let emit = crate::bus::Emitter::new(tx, session.id());
    let outcome = session.attend(&emit);
    assert!(
        session.handle().reply().is_none(),
        "a conversing trunk deposits no reply"
    );
    assert!(
        !matches!(outcome, AgentOutcome::Replied),
        "a conversing trunk never completes by returning a value: {outcome:?}"
    );
}

#[test]
fn headless_recording_failure_ends_the_run_in_both_formats() {
    for format in [OutputFormat::Text, OutputFormat::Json] {
        // An empty script fails if a request crosses the failed prompt commit.
        let mut session = frontend_trunk("recording-failure", Script::new(), Trunk::Headless);
        session.recorder().fail_after(7);
        let why = run(
            &mut session,
            &SessionInfo {
                system_size: 0,
                system_files: &[],
                base: "dangerous",
                extend: None,
                restrict_files: &[],
                cwd: &ral_core::host::cwd().expect("test process has a cwd"),
            },
            &Provider::scripted("test-model", Script::new()),
            Some("must be recorded before sending".into()),
            format,
        )
        .expect_err("a failed recording is not a successful run");
        assert!(session.check_available().is_err(), "{why}");
        assert!(session.severance().is_none(), "this is not engine loss");
        assert!(session.handle().reply().is_none());
        assert_eq!(
            session.handle().steer("later".into()),
            Err(crate::agent::Ended)
        );
    }
}

#[test]
fn settled_conversation_reports_recording_failure_instead_of_quiescent_success() {
    let root = converse_trunk("unavailable-job", Script::new());
    let recorder = root.recorder();
    let mut conversation = SettledConversation::new(root).unwrap();
    conversation.handle().steer("first".into()).unwrap();
    let job = conversation.await_job().expect("the prompt opens a job");
    recorder.fail_after(7);
    recorder
        .emit(Display::Card {
            card: Card(Vec::new()),
        })
        .expect_err("a display producer fails on the shared writer before attendance");
    let mut sink = Collecting::default();

    let why = conversation
        .run(job, &mut sink)
        .outcome
        .expect_err("an unavailable session cannot report ordinary quiescence");

    assert!(conversation.root.check_available().is_err(), "{why}");
    assert!(
        conversation.severance().is_none(),
        "recording failed, not the engine"
    );
    assert_eq!(
        sink.transients
            .iter()
            .filter_map(|t| match t {
                Transient::Fault { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [why.as_str()]
    );
    assert!(
        !sink
            .transients
            .iter()
            .any(|t| matches!(t, Transient::State(AgentState::Ready)))
    );
    assert_eq!(
        conversation.handle().steer("later".into()),
        Err(crate::agent::Ended)
    );
    assert!(conversation.await_job().is_none());
}

mod late_failure;

#[test]
fn ordinary_failed_conversation_remains_usable_for_the_next_job() {
    let root = converse_trunk(
        "failed-usable-job",
        Script::new()
            .then(Reply::error(crate::provider::ProviderError::Other(
                "request refused".into(),
            )))
            .then(Reply::text("next job succeeds")),
    );
    let mut conversation = SettledConversation::new(root).unwrap();
    let mut sink = Collecting::default();
    for prompt in ["first", "next"] {
        conversation.handle().steer(prompt.into()).unwrap();
        let job = conversation
            .await_job()
            .expect("the session still accepts jobs");
        conversation.run(job, &mut sink).outcome.unwrap();
        conversation
            .root
            .check_available()
            .expect("a request failure leaves recording usable");
    }
    let context = format!("{:?}", conversation.root.rendered_messages());
    assert!(context.contains("next job succeeds"), "{context}");
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
    let mut h = Headless::new(root, &mut sink_out, &mut sink_err);
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

/// A `Forensic::TurnStarted` fact through the real live path — `Sink::accept`,
/// which `drive` calls in production — folded into blocks and drawn from
/// there.  A child's own turns never count toward root's.
#[test]
fn num_turns_counts_root_turns() {
    use crate::record::{Forensic, Locus, Record, Recorded, Seq};
    let root: AgentId = AgentId::new(1);
    let sub: AgentId = AgentId::new(2);
    let mut sink_out = Vec::new();
    let mut sink_err = Vec::new();
    let mut h = Headless::new(root, &mut sink_out, &mut sink_err);
    let mut seq = 0u64;
    let mut turn_started = |id: AgentId, turn: u64| {
        seq += 1;
        Signal::Fact(
            id,
            Recorded::new(
                Locus::placeholder(Seq::new(seq)),
                Record::Forensic(Forensic::TurnStarted {
                    turn,
                    tuning: crate::provider::Tuning::initial(),
                    via: None,
                }),
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
    let mut h = Headless::new(root, &mut sink_out, &mut sink_err);
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
        &mut converse_trunk("blank-seed", Script::new()),
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

// ── The quiescent conversation driver ─────────────────────────────────

/// A conversing trunk with real spawn fuel, so a test may register a
/// genuine live child under it — [`converse_trunk`]'s `fuel: 0` exists
/// precisely to refuse that.
fn settled_trunk(
    tag: &str,
    script: Script,
    allow_schedule: bool,
    resume_on_reset: bool,
    lease: std::time::Duration,
) -> Agent {
    settled_trunk_with_factory(tag, script, allow_schedule, resume_on_reset, lease, None)
}

fn settled_trunk_with_factory(
    tag: &str,
    script: Script,
    allow_schedule: bool,
    resume_on_reset: bool,
    lease: std::time::Duration,
    factory: Option<Arc<dyn crate::agent::Factory>>,
) -> Agent {
    let scratch =
        Arc::new(crate::app::Scratch::for_test(crate::app::EXARCH, tag).expect("scratch dir"));
    let dir = scratch.test_sibling("run").expect("temp run dir");
    Agent::first(
        crate::agent::fresh_id(),
        RootConfig {
            system: "system".into(),
            run_dir: dir,
            trunk: Trunk::Embedded,
            tools: Toolset::offered(false),
            allow_schedule,
            resume_on_reset,
            disk_warn_bytes: None,
            fuel: SPAWN_FUEL,
            search: true,
            factory,
            bureau: Arc::new(crate::provider::Bureau::Scripted),
            roles: crate::provider::roles::Roles::default(),
        },
        RootEngine::Identity {
            scratch,
            cwd: ral_core::host::cwd().expect("test process has a cwd"),
            grants: ral_core::authority::policy::GrantStack::default(),
            terminal: ral_core::io::terminal::TerminalState::default(),
        },
        Arc::new(Provider::scripted("test-model", script)),
        &crate::INSTALLERS,
        lease,
    )
    .expect("root trunk")
}

/// A live child of `parent` with no attend loop of its own. The caller
/// holds it — that is what keeps it live — and drops it on its own clock,
/// so a test can hold `parent` on it for exactly as long as it chooses,
/// deterministically, rather than racing a real child's own thread against
/// a sleep.
fn live_child(parent: &Agent, name: &str) -> Agent {
    parent.fork_named(name).expect("fork child")
}

/// Every signal a caller's own `Sink` can receive, folded into one place —
/// the seam's own `Record`/`Transient`.
#[derive(Default)]
struct Collecting {
    facts: Vec<Record>,
    transients: Vec<Transient>,
    signals: Vec<Signal>,
}
impl Sink for Collecting {
    fn accept(&mut self, signal: Signal) {
        match &signal {
            Signal::Fact(id, rec) => self.fact(*id, rec),
            Signal::Transient(id, t) => self.transient(*id, t),
            Signal::Born(id, born) => self.born(*id, born),
        }
        self.signals.push(signal);
    }

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
fn settled_conversation_holds_for_a_live_child_then_quiesces() {
    let session = settled_trunk(
        "slow-child",
        Script::new()
            .then(Reply::text("on it"))
            .then(Reply::text("thanks for the update")),
        false,
        false,
        crate::agent::fleet::AGENT_LEASE_IDLE,
    );
    let child = live_child(&session, "helper");
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let settler = std::thread::spawn(move || {
        release_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the exchange must park on the child within the timeout");
        let (emit, _rx) = crate::bus::dummy_emitter();
        child.settle(AgentOutcome::Stopped("done".into()), &emit);
    });

    let mut sink = SignalOnWaiting {
        inner: Collecting::default(),
        release: release_tx,
    };
    let mut settled = SettledConversation::new(session).unwrap();
    settled.handle().steer("please help".into()).unwrap();
    let job = settled.await_job().unwrap();
    settled
        .run(job, &mut sink)
        .outcome
        .expect("the job itself must not fail for want of a reply");
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
            Record::Display(Display::SubagentDone { name, outcome, .. })
                if name == "helper" && matches!(outcome, Settlement::Failed { reason } if reason == "done")
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
fn settled_conversation_ends_even_when_a_child_never_replies() {
    let session = settled_trunk(
        "child-no-reply",
        Script::new()
            .then(Reply::text("on it"))
            .then(Reply::text("thanks for the update")),
        false,
        false,
        crate::agent::fleet::AGENT_LEASE_IDLE,
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
            resumed: None,
        },
        &spawn_emit,
    )
    .expect("spawn must succeed");

    let mut sink = Collecting::default();
    let mut settled = SettledConversation::new(session).unwrap();
    settled.handle().steer("please help".into()).unwrap();
    let job = settled.await_job().unwrap();
    settled
        .run(job, &mut sink)
        .outcome
        .expect("the job itself must not fail merely because a child did");

    assert!(
        sink.facts.iter().any(|f| matches!(
            f,
            Record::Display(Display::SubagentDone { name, outcome, .. })
                if name == "flaky" && matches!(outcome, Settlement::Failed { .. })
        )),
        "the child's un-replied finish must surface as a failed SubagentDone"
    );
}

/// A trunk that arms its own wakeups is refused at construction, the same
/// class of refusal as a missing engine factory: an armed wakeup may fire
/// with nothing left to wait it out once the fleet quiesces.
#[test]
fn settled_conversation_refuses_a_trunk_that_arms_its_own_wakeups() {
    for (tag, allow_schedule, resume_on_reset) in [
        ("allow-schedule", true, false),
        ("resume-on-reset", false, true),
    ] {
        let session = settled_trunk(
            tag,
            Script::new(),
            allow_schedule,
            resume_on_reset,
            crate::agent::fleet::AGENT_LEASE_IDLE,
        );
        let handle = session.handle();
        let err = SettledConversation::new(session)
            .err()
            .expect("a trunk that arms its own wakeups must be refused, not run");
        assert!(
            err.contains("wakeup"),
            "the refusal must name what it refuses: {err}"
        );
        assert_eq!(handle.steer("refused".into()), Err(crate::agent::Ended));
    }
}

#[test]
fn owning_end_fleet_waits_for_a_held_helper() {
    let root = crate::agent::testkit::trunk(false);
    let child = crate::agent::testkit::started_child(&root, "helper", "`inherit");
    let handle = child.handle();
    let settled = SettledConversation::new(root).unwrap();
    let (waiting, parked) = std::sync::mpsc::channel();
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        settled.end_fleet_after(|| waiting.send(()).unwrap());
        done.send(()).unwrap();
    });
    parked
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(!handle.is_live());
    assert!(matches!(
        finished.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    drop(child);
    finished
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    worker.join().unwrap();
}

#[test]
fn owning_end_fleet_waits_for_helper_teardown_and_releases_the_factory() {
    struct HeldFactory {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        _resource: Arc<()>,
    }
    impl crate::agent::Factory for HeldFactory {
        fn engine(&self) -> Result<ral_core::protocol::channel::WireStream, String> {
            unreachable!("this fixture uses identity engines")
        }
    }
    impl Drop for HeldFactory {
        fn drop(&mut self) {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
    }
    let resource = Arc::new(());
    let released = Arc::downgrade(&resource);
    let (entered, tearing_down) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel();
    let root = settled_trunk_with_factory(
        "held-factory",
        Script::new(),
        false,
        false,
        crate::agent::fleet::AGENT_LEASE_IDLE,
        Some(Arc::new(HeldFactory {
            entered,
            release: std::sync::Mutex::new(held),
            _resource: resource,
        })),
    );
    let child = crate::agent::testkit::started_child(&root, "helper", "`inherit");
    let handle = child.handle();
    let settled = SettledConversation::new(root).unwrap();
    let (done, finished) = std::sync::mpsc::channel();
    let ending = std::thread::spawn(move || {
        settled.end_fleet();
        assert!(
            released.upgrade().is_none(),
            "end_fleet must release the launch factory"
        );
        done.send(()).unwrap();
    });
    let helper = std::thread::spawn(move || drop(child));
    tearing_down
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(!handle.is_live());
    assert!(matches!(
        finished.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    release.send(()).unwrap();
    finished
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    helper.join().unwrap();
    ending.join().unwrap();
    assert_eq!(handle.steer("late".into()), Err(crate::agent::Ended));
}

#[test]
fn owning_end_fleet_cancels_other_roots_and_siblings() {
    let root = crate::agent::testkit::trunk(false);
    let first = crate::agent::testkit::started_child(&root, "first", "`inherit");
    let second = crate::agent::testkit::started_child(&root, "second", "`inherit");
    let other = crate::agent::testkit::test_agent(
        &root.fleet,
        crate::agent::testkit::TestAgentSpec::new("other-root"),
    )
    .unwrap();
    let handles = [
        root.handle(),
        first.handle(),
        second.handle(),
        (*other).clone(),
    ];
    let settled = SettledConversation::new(root).unwrap();
    let consumer = std::thread::spawn(move || {
        assert!(other.inbox.await_job().is_none());
    });
    let (release, held) = std::sync::mpsc::channel();
    let helpers = std::thread::spawn(move || {
        held.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        drop(first);
        drop(second);
    });
    let (done, finished) = std::sync::mpsc::channel();
    let ending = std::thread::spawn(move || {
        settled.end_fleet_after(|| {
            assert!(handles.iter().all(|handle| !handle.is_live()));
            release.send(()).unwrap();
        });
        done.send(()).unwrap();
    });
    finished
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("every root and helper must finish teardown");
    consumer.join().unwrap();
    helpers.join().unwrap();
    ending.join().unwrap();
}

#[test]
fn await_job_is_none_once_the_root_has_ended() {
    let settled = SettledConversation::new(converse_trunk("ended", Script::new())).unwrap();
    let handle = settled.handle();
    handle.steer("queued".into()).unwrap();
    handle.end();
    assert!(settled.await_job().is_none());
    assert_eq!(handle.steer("too late".into()), Err(crate::agent::Ended));
}

#[test]
fn a_stopped_job_settles_though_its_helper_had_not_replied() {
    let root = crate::agent::testkit::trunk(false);
    crate::agent::testkit::set_provider(
        &root,
        crate::agent::testkit::scripted(
            "test-model",
            Script::new()
                .then(Reply::text("waiting for help"))
                .then(Reply::text("fresh work")),
        ),
    );
    let child = crate::agent::testkit::started_child(&root, "helper", "`inherit");
    let mut settled = SettledConversation::new(root).unwrap();
    let handle = settled.handle();
    handle.steer("please help".into()).unwrap();
    let job = settled.await_job().unwrap();
    let id = job.id();
    let (release, held) = std::sync::mpsc::sync_channel(1);
    let stopper = std::thread::spawn(move || {
        held.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the root must park on its busy helper");
        assert!(!child.handle().has_reply());
        assert_eq!(handle.stop(id), Some(Vec::new()));
        assert!(!child.handle().is_live());
        let (emit, _rx) = crate::bus::dummy_emitter();
        child.settle(AgentOutcome::Stopped("late result".into()), &emit);
    });
    let mut sink = SignalOnWaiting {
        inner: Collecting::default(),
        release,
    };
    let handle = settled.handle();
    let timeout = ral_core::process::arm_callback(std::time::Duration::from_secs(5), {
        let handle = handle.clone();
        move || handle.end()
    });
    let end = settled.run(job, &mut sink);
    assert_eq!(end.id, id);
    assert!(end.stopped);
    assert!(end.outcome.is_ok());
    stopper.join().expect("the stopping thread must not panic");
    drop(timeout);
    assert!(handle.is_live());
    assert!(!handle.has_busy_children());
    assert_eq!(handle.queued(), [] as [String; 0]);
    handle.steer("next job".into()).unwrap();
    let next = settled.await_job().unwrap();
    assert!(settled.run(next, &mut sink).outcome.is_ok());
    assert!(
        !sink
            .inner
            .facts
            .iter()
            .any(|fact| matches!(fact, Record::Display(Display::SubagentDone { .. })))
    );
}

fn parent_script() -> Script {
    (0..8).fold(Script::new(), |script, _| {
        script.then(Reply::text("settled"))
    })
}

#[test]
fn helper_ready_precedes_its_parent_result_before_the_next_draw() {
    let root = settled_trunk(
        "ready-order",
        parent_script(),
        false,
        false,
        crate::agent::fleet::AGENT_LEASE_IDLE,
    );
    let mut settled = SettledConversation::new(root).unwrap();
    let root = &settled.root;
    let mut child = root.fork_named("helper").unwrap();
    let child_handle = child.handle();
    let root_emit = settled.bus.emitter(root.id());
    let emit = root_emit.child(
        child_handle.id(),
        crate::record::Born {
            log_dir: child.log_dir().clone(),
            name: "helper".into(),
            parent: Some(root.id()),
            resumed: None,
            recorder: child.recorder(),
        },
    );
    let provider = crate::agent::testkit::scripted(
        "test-model",
        Script::new().then(Reply::tool_calls(vec![crate::agent::testkit::ral_call(
            "reply",
            "exarch-agents `reply 42",
        )])),
    );
    let (release, held) = std::sync::mpsc::channel();
    let helper = std::thread::spawn(move || {
        let outcome = child
            .deliberate(
                &DurableRoot::default().child(),
                &provider,
                Some(("help".into(), crate::record::Voice::Human)),
                None,
                &emit,
            )
            .unwrap();
        assert!(matches!(outcome, crate::agent::outcome::Outcome::Replied));
        // No attend/draw may publish the idle state while the parent finishes.
        held.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        drop(child);
    });
    let mut sink = Collecting::default();
    let handle = root.handle();
    let timeout = ral_core::process::arm_callback(std::time::Duration::from_secs(5), {
        let handle = handle.clone();
        move || handle.end()
    });
    handle.steer("help".into()).unwrap();
    let job = settled.await_job().unwrap();
    assert!(settled.run(job, &mut sink).outcome.is_ok());
    drop(timeout);
    let root = &settled.root;
    assert!(handle.is_live(), "the parent must settle");
    release.send(()).unwrap();
    helper.join().unwrap();
    let ready = sink
        .signals
        .iter()
        .position(|signal| {
            matches!(
                signal,
                Signal::Transient(id, Transient::State(AgentState::Ready)) if *id == child_handle.id()
            )
        })
        .expect("reply publishes Ready without another draw");
    let result = sink.signals.iter().position(|signal| matches!(
        signal,
        Signal::Fact(id, rec) if *id == root.id() && matches!(
            rec.value(), Record::Display(Display::SubagentDone { name, .. }) if name == "helper"
        )
    )).expect("parent announces its helper's result");
    assert!(ready < result);
    assert!(!sink.signals.iter().any(|signal| matches!(
        signal, Signal::Transient(id, Transient::Died) if *id == child_handle.id()
    )));
    assert_eq!(child_handle.reply(), Some(FOValue::Int { value: 42 }));
}

#[test]
fn stopping_a_job_recalls_queued_prompts_and_is_idempotent() {
    let mut settled =
        SettledConversation::new(converse_trunk("recall-job", Script::new())).unwrap();
    let handle = settled.handle();
    handle.steer("first".into()).unwrap();
    let job = settled.await_job().unwrap();
    let id = job.id();
    handle.steer("second".into()).unwrap();
    assert_eq!(handle.stop(id), Some(vec!["first\nsecond".into()]));
    assert_eq!(handle.stop(id), None);
    assert_eq!(handle.queued(), [] as [String; 0]);
    let mut sink = Collecting::default();
    let end = settled.run(job, &mut sink);
    assert_eq!(end.id, id);
    assert!(end.stopped);
    assert!(end.outcome.is_ok());
    assert!(!sink.facts.iter().any(|fact| matches!(
        fact,
        Record::Forensic(crate::record::Forensic::TurnStarted { .. })
    )));
    assert_eq!(handle.stop(id), None);
}

#[test]
fn a_stale_stop_leaves_the_next_jobs_queue_untouched() {
    let mut settled = SettledConversation::new(converse_trunk(
        "stale-stop",
        Script::new()
            .then(Reply::text("done"))
            .then(Reply::text("again")),
    ))
    .unwrap();
    let handle = settled.handle();
    handle.steer("first".into()).unwrap();
    let first = settled.await_job().unwrap();
    let id = first.id();
    let end = settled.run(first, &mut Collecting::default());
    assert_eq!(end.id, id);
    assert!(!end.stopped);
    assert!(end.outcome.is_ok());
    handle.steer("next".into()).unwrap();
    assert_eq!(handle.stop(id), None);
    assert_eq!(handle.queued(), ["next"]);
    let next = settled.await_job().unwrap();
    assert!(next.id().get() > id.get());
    assert_eq!(handle.stop(id), None);
    assert_eq!(handle.queued(), ["next"]);
    let end = settled.run(next, &mut Collecting::default());
    assert!(!end.stopped);
    assert!(end.outcome.is_ok());
}

#[test]
fn text_sent_after_stop_opens_a_new_stoppable_job() {
    let mut settled = SettledConversation::new(converse_trunk("next-job", Script::new())).unwrap();
    let handle = settled.handle();
    handle.steer("abandoned".into()).unwrap();
    let first = settled.await_job().unwrap();
    let id = first.id();
    assert_eq!(handle.stop(id), Some(vec!["abandoned".into()]));
    handle.steer("fresh".into()).unwrap();
    let end = settled.run(first, &mut Collecting::default());
    assert_eq!(end.id, id);
    assert!(end.stopped);
    assert!(end.outcome.is_ok());
    assert_eq!(handle.queued(), ["fresh"]);
    let next = settled.await_job().unwrap();
    let next_id = next.id();
    assert!(next_id.get() > id.get());
    assert_eq!(handle.stop(next_id), Some(vec!["fresh".into()]));
    let end = settled.run(next, &mut Collecting::default());
    assert_eq!(end.id, next_id);
    assert!(end.stopped);
    assert!(end.outcome.is_ok());
    assert!(handle.is_live());
}

#[test]
fn dropping_an_unattended_job_closes_it_without_consuming_the_prompt() {
    let mut settled = SettledConversation::new(converse_trunk(
        "dropped-job",
        Script::new().then(Reply::text("taken up once")),
    ))
    .unwrap();
    let handle = settled.handle();
    handle.steer("keep me".into()).unwrap();
    let job = settled.await_job().unwrap();
    let id = job.id();
    drop(job);
    assert_eq!(handle.stop(id), None);
    assert_eq!(handle.queued(), ["keep me"]);
    let next = settled.await_job().unwrap();
    assert!(next.id().get() > id.get());
    let mut sink = Collecting::default();
    let end = settled.run(next, &mut sink);
    assert!(!end.stopped);
    assert!(end.outcome.is_ok());
    assert_eq!(
        sink.facts
            .iter()
            .filter(|fact| matches!(
                fact,
                Record::Forensic(crate::record::Forensic::TurnStarted { .. })
            ))
            .count(),
        1
    );
}

struct TeardownGate {
    emit: crate::bus::Emitter,
    recorder: crate::record::Emitter,
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl ral_core::shell::runtime::DeferredSink for TeardownGate {
    fn deliver(&self, _values: Vec<FOValue>) {}
}

impl Drop for TeardownGate {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        self.release
            .get_mut()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        self.recorder.attach(self.emit.fleet_sink());
        let _ = self
            .recorder
            .emit(Display::Answer {
                text: "helper teardown complete".into(),
            })
            .unwrap();
    }
}

#[test]
fn a_stopped_job_waits_for_its_helpers_engine_teardown_even_if_the_fleet_ends() {
    for end_fleet in [false, true] {
        let root = crate::agent::testkit::trunk(false);
        let mut child = root.fork_named("helper").unwrap();
        let (entered, tearing_down) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel();
        let mut settled = SettledConversation::new(root).unwrap();
        let helper_id = child.id();
        let helper_emit = settled.bus.emitter(helper_id);
        child.engine.install_raw(Arc::new(TeardownGate {
            emit: settled.bus.emitter(helper_id),
            recorder: child.recorder(),
            entered,
            release: std::sync::Mutex::new(held),
        }));
        let handle = settled.handle();
        handle.steer("start".into()).unwrap();
        let job = settled.await_job().unwrap();
        let id = job.id();
        assert!(handle.stop(id).is_some());
        child
            .engine
            .publish_death_on_release(helper_emit.fleet_sink());
        let helper = std::thread::spawn(move || {
            child.settle(AgentOutcome::Cancelled, &helper_emit);
        });
        tearing_down
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let fleet = settled.root.fleet.clone();
        let driver = std::thread::spawn(move || {
            let mut sink = Collecting::default();
            let end = settled.run(job, &mut sink);
            done.send((end, sink.signals)).unwrap();
            settled
        });
        assert!(matches!(
            finished.recv_timeout(std::time::Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        if end_fleet {
            fleet.end(ral_core::process::CancelCause::Terminated);
            assert!(matches!(
                finished.recv_timeout(std::time::Duration::from_millis(20)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
        }
        release.send(()).unwrap();
        let (end, signals) = finished
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(signals.iter().any(|signal| matches!(signal,
            Signal::Transient(id, Transient::Died) if *id == helper_id
        )));
        assert_eq!(end.id, id);
        assert!(end.stopped);
        assert!(end.outcome.is_ok());
        helper.join().unwrap();
        let settled = driver.join().unwrap();
        assert!(settled.root.fleet.claim("helper").is_ok());
        assert_eq!(handle.is_live(), !end_fleet);
        assert_eq!(handle.stop(id), None);
    }
}

fn spawn_settled_helper(root: &Agent, settled: &SettledConversation) -> AgentHandle {
    let child = root.fork_named("helper").unwrap();
    crate::agent::testkit::set_provider(
        &child,
        crate::agent::testkit::scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![crate::agent::testkit::ral_call(
                    "first",
                    "exarch-agents `reply 1",
                )]))
                .then(Reply::tool_calls(vec![crate::agent::testkit::ral_call(
                    "again",
                    "exarch-agents `reply 2",
                )])),
        ),
    );
    let handle = child.handle();
    let emit = settled.bus.emitter(root.id());
    spawn_async(
        child,
        AsyncSpawn {
            name: "helper".into(),
            prompt: Some("help".into()),
            resumed: None,
        },
        &emit,
    )
    .unwrap();
    handle
}

#[test]
fn settled_conversation_reuses_a_helper_without_a_second_birth() {
    let root = settled_trunk(
        "reuse-helper",
        parent_script(),
        false,
        false,
        crate::agent::fleet::AGENT_LEASE_IDLE,
    );
    let mut settled = SettledConversation::new(root).unwrap();
    let helper = spawn_settled_helper(&settled.root, &settled);
    let mut first = Collecting::default();
    settled.handle().steer("first".into()).unwrap();
    let job = settled.await_job().unwrap();
    assert!(settled.run(job, &mut first).outcome.is_ok());
    assert_eq!(helper.reply(), Some(FOValue::Int { value: 1 }));
    assert!(settled.root.fleet.resolve("helper").is_some());
    assert_eq!(
        first
            .signals
            .iter()
            .filter(|s| matches!(s, Signal::Born(id, _) if *id == helper.id()))
            .count(),
        1
    );

    helper
        .message(&settled.handle(), "follow up".into())
        .unwrap();
    let mut second = Collecting::default();
    settled.handle().steer("second".into()).unwrap();
    let job = settled.await_job().unwrap();
    assert!(settled.run(job, &mut second).outcome.is_ok());
    assert_eq!(helper.reply(), Some(FOValue::Int { value: 2 }));
    assert!(!second.signals.iter().any(|s| matches!(s, Signal::Born(..))));
    for state in [AgentState::AwaitingModel, AgentState::Ready] {
        assert!(second.signals.iter().any(|s| matches!(s,
            Signal::Transient(id, Transient::State(actual)) if *id == helper.id() && *actual == state
        )), "helper state {state:?} must remain connected");
    }
    assert!(second.signals.iter().any(|s| matches!(s,
        Signal::Fact(id, rec) if *id == helper.id() && matches!(rec.value(), Record::Display(Display::ToolCall { .. }))
    )), "helper tool facts must remain connected");
    assert!(second.signals.iter().any(|s| matches!(s,
        Signal::Fact(id, rec) if *id == helper.id() && matches!(rec.value(), Record::Forensic(crate::record::Forensic::UsageDelta { .. }))
    )), "helper usage must remain connected");
}

#[test]
fn settled_conversation_drains_a_lease_death_at_the_next_job() {
    let root = settled_trunk(
        "late-death",
        parent_script(),
        false,
        false,
        std::time::Duration::from_millis(50),
    );
    let mut settled = SettledConversation::new(root).unwrap();
    let mut first = Collecting::default();
    settled.handle().steer("first".into()).unwrap();
    let job = settled.await_job().unwrap();
    assert!(settled.run(job, &mut first).outcome.is_ok());
    let helper = spawn_settled_helper(&settled.root, &settled);
    let id = helper.id();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while helper.is_live() {
        assert!(
            std::time::Instant::now() < deadline,
            "the helper's reply lease must expire"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    settled
        .root
        .fleet
        .wait_settled(settled.root.tenant(), &crate::bus::Pumping::for_test())
        .unwrap();
    assert!(!first.signals.iter().any(|s| matches!(s,
        Signal::Transient(agent, Transient::Died) if *agent == id
    )));
    let mut second = Collecting::default();
    settled.handle().steer("second".into()).unwrap();
    let job = settled.await_job().unwrap();
    assert!(settled.run(job, &mut second).outcome.is_ok());
    let died = second
        .signals
        .iter()
        .position(|s| {
            matches!(s,
                Signal::Transient(agent, Transient::Died) if *agent == id
            )
        })
        .expect("the queued death must drain at the next exchange");
    assert!(
        second.facts.iter().any(|fact| matches!(fact,
            Record::Display(Display::SubagentDone { name, outcome: Settlement::Replied, .. })
                if name == "helper"
        )),
        "the expired helper must have replied successfully"
    );
    let working = second.signals.iter().position(|s| matches!(s,
        Signal::Transient(agent, Transient::State(AgentState::AwaitingModel)) if *agent == settled.handle().id()
    )).expect("the next exchange must run");
    assert!(died < working);
}
