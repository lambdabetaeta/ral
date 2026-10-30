//! Process-boundary resume coverage: a scripted child leaves a mid-exchange
//! ledger, the parent terminates it, and a fresh root continues the session.

use crate::agent::fleet::HELPER_IDLE;
use crate::agent::outcome::Outcome;
use crate::agent::{Admission, Agent, RootConfig, RootEngine, Trunk};
use crate::app::{EXARCH, Scratch};
use crate::bus::{Emitter, Signal, channel};
use crate::host::Host;
use crate::provider::Provider;
use crate::provider::Toolset;
use crate::provider::scripted::{Reply, Script};
use crate::record::{self, Blocks, RECORD, Refusal, View, Voice};
use ral_core::own::OwnDir;
use ral_core::path::XdgKind;
use ral_core::process::DurableRoot;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

/// Set in the scripted child's environment, which is what makes
/// `resume_child` act: run bare, it does nothing.
const CHILD_VAR: &str = "EXARCH_RESUME_CHILD";

/// The child's run directory, in the state home the parent made for it.
const CHILD_RUN: &str = "child-run";

fn scripted(model: &str, script: Script) -> Arc<Provider> {
    Arc::new(Provider::scripted(model, script))
}

/// A hand-written line, sealed by the first 16 hex digits of its commit's blake3.
fn commit_line(records: &serde_json::Value) -> String {
    let commit = serde_json::json!({ "at_unix_ms": 0, "records": records }).to_string();
    let sum = &blake3::hash(commit.as_bytes()).to_hex()[..16];
    format!(r#"{{"sum":"{sum}","commit":{commit}}}"#)
}

/// Whether any resident block carries `needle` in its own text — matched on
/// the fold's kinds directly (`Prompt`/`Answer`), not a rendered string.
fn kind_contains(blocks: &Blocks, needle: &str) -> bool {
    blocks.blocks().iter().any(|b| match b.kind() {
        record::BlockKind::Prompt { text, .. } | record::BlockKind::Answer { text } => {
            text.contains(needle)
        }
        _ => false,
    })
}

fn drive(session: &mut Agent, prompt: &str) {
    let (tx, _rx) = channel();
    let emit = Emitter::new(tx, session.id());
    let outcome = session.deliberate(
        &DurableRoot::default().child(),
        Some((prompt.to_string(), Voice::Human)),
        None,
        &emit,
    );
    assert!(matches!(outcome, Ok(Outcome::Complete)));
}

fn root_config(run_dir: &OwnDir) -> RootConfig {
    RootConfig {
        system: "system".into(),
        run_dir: OwnDir::clone(run_dir),
        trunk: Trunk::Converses,
        admission: Admission::Free,
        helper_idle: HELPER_IDLE,
        max_tokens: None,
        tools: Toolset::offered(false),
        allow_schedule: false,
        resume_on_reset: false,
        disk_warn_bytes: None,
        fuel: 0,
        search: true,
        factory: None,
        bureau: Arc::new(crate::provider::Bureau::scripted([])),
        roles: crate::provider::roles::Roles::default(),
    }
}

fn identity_engine(tag: &str) -> RootEngine {
    RootEngine::Identity {
        scratch: Arc::new(Scratch::for_test(EXARCH, tag).expect("scratch dir")),
        cwd: ral_core::host::cwd().expect("test process has a cwd"),
        grants: ral_core::authority::policy::GrantStack::default(),
        terminal: ral_core::io::terminal::TerminalState::default(),
    }
}

/// A killed child's crash-shaped ledger, in a run directory of its own; the
/// state home is returned to keep that directory alive.
fn crashed_run() -> (ral_core::own::TestDir, OwnDir) {
    // A state home of this test's own, with the run made in it: the child
    // reaches the same directory through `XDG_STATE_HOME`, as exarch's own
    // launches reach theirs.
    let state = OwnDir::for_test("resume-state").expect("a state home");
    let child_dir = state
        .make_dir(EXARCH.name())
        .and_then(|app| app.make_dir(CHILD_RUN))
        .expect("the child's run directory");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg("host::tests::resume::resume_child")
        .arg("--nocapture")
        .env("XDG_STATE_HOME", state.path())
        .env(CHILD_VAR, "1")
        .spawn()
        .expect("spawn scripted child");

    let record_log = child_dir.path().join("sessions/0/record.jsonl");
    let ready = (0..200).any(|_| {
        if std::fs::read_to_string(&record_log).is_ok_and(|text| text.contains("crash prompt")) {
            true
        } else {
            std::thread::sleep(Duration::from_millis(10));
            false
        }
    });
    if !ready {
        let _ = child.kill();
        let _ = child.wait();
        panic!("scripted child did not leave its crash-shaped ledger");
    }
    child.kill().expect("terminate scripted child");
    let _ = child.wait();
    (state, child_dir)
}

#[test]
fn scripted_run_kill_resume_and_continue() {
    let (_state, child_dir) = crashed_run();
    let [(mut resumed, _)] = <[_; 1]>::try_from(
        Agent::resume_for_test(
            root_config(&child_dir),
            identity_engine("resume-parent"),
            &scripted("test-model", Script::new().then(Reply::text("continued"))),
        )
        .expect("resume child ledger"),
    )
    .unwrap_or_else(|roots| panic!("one open session, got {}", roots.len()));
    assert!(resumed.is_ready());
    assert!(
        resumed
            .rendered_messages()
            .iter()
            .any(|message| message.content.first_text() == Some("before kill")),
        "the pre-kill exchange must survive the resume, not be thrown away"
    );
    // The bug this whole plan exists for: not the model's memory (asserted
    // above) but the *user's* — the view fold `record.jsonl` folds into,
    // which a resumed TUI seeds its scrollback from (`tui_loop::run`).
    let record_path = resumed.log_dir().path().join(RECORD);
    let replayed = || {
        let file = File::open(&record_path).expect("the record opens");
        record::replay::<View>(file, &record_path, Blocks::default())
    };
    let blocks = replayed().expect("the resumed session's record log replays cleanly");
    assert!(
        kind_contains(&blocks, "before kill"),
        "the view fold must carry the pre-kill exchange across the crash and resume too"
    );

    drive(&mut resumed, "continue after the crash");
    assert!(resumed.is_ready());
    assert!(
        resumed
            .rendered_messages()
            .iter()
            .any(|message| message.content.first_text() == Some("continued"))
    );
    assert!(
        resumed
            .rendered_messages()
            .iter()
            .any(|message| message.content.first_text() == Some("before kill")),
        "the pre-kill exchange must still be present after driving the resumed session further"
    );

    let blocks =
        replayed().expect("the record log still replays cleanly after driving the resumed session");
    assert!(
        kind_contains(&blocks, "continued"),
        "the resumed session's own turn joins the view fold"
    );
    assert!(
        kind_contains(&blocks, "before kill"),
        "and driving the resumed session further does not disturb the pre-kill turn"
    );
}

#[test]
fn the_host_resumes_a_crashed_run_and_carries_on() {
    let (_state, child_dir) = crashed_run();
    let host = Host::resume(
        root_config(&child_dir),
        identity_engine("resume-host"),
        &scripted("test-model", Script::new().then(Reply::text("continued"))),
    )
    .expect("resume the crashed run");
    let [root] = host.roots() else {
        panic!("one open session, got {}", host.roots().len())
    };
    host.port()
        .submit(*root, "continue after the crash".into())
        .unwrap();

    let (mut resumed, mut answered) = (None, false);
    while !answered {
        match host
            .feed()
            .recv_timeout(Duration::from_secs(10))
            .expect("the resumed root settles its job")
        {
            Signal::Born(id, born) if id == *root => resumed = born.resumed,
            Signal::Commit(id, commit) if id == *root => {
                answered = commit.records().iter().any(|record| {
                    matches!(record.value(), record::Record::Display(record::Display::Answer { text })
                        if text == "continued")
                });
            }
            _ => {}
        }
    }

    let resumed = resumed.expect("a resumed root is born with what it picked up");
    assert!(resumed.bytes > 0);
    assert_eq!(host.end(), [] as [String; 0]);
}

/// A record the fold does not recognise refuses the whole session rather
/// than silently dropping the line or panicking — the versioned display
/// vocabulary's own law.
#[test]
fn replay_refuses_a_ledger_line_it_does_not_recognise() {
    let dir = OwnDir::for_test("replay-refuses").expect("scratch dir");
    let path = dir.path().join(RECORD);
    {
        let emit = record::Emitter::create(&dir).expect("fresh record log");
        let _ = emit
            .emit(record::Forensic::SystemNote {
                text: "a genuine record".into(),
            })
            .expect("a forensic record records");
    }
    // Appended by hand: no `Record` variant is named `FutureClass`, and
    // `Record`'s derive carries no `#[serde(other)]` fallback, so this line
    // must refuse to parse rather than silently vanish.
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("record log");
    writeln!(
        file,
        "{}",
        commit_line(&serde_json::json!([{"FutureClass": {"anything": 1}}]))
    )
    .expect("append a foreign line");
    file.flush().expect("flush the foreign line");

    let file = File::open(&path).expect("the record opens");
    match record::replay::<View>(file, &path, Blocks::default()) {
        Err(Refusal::Unreadable(_)) => {}
        Err(other) => panic!("expected an Unreadable refusal, not: {other}"),
        Ok(_) => panic!(
            "a record the fold cannot parse must refuse the whole replay, not silently succeed"
        ),
    }
}

#[test]
fn resume_child() {
    if std::env::var_os(CHILD_VAR).is_none() {
        return;
    }
    // Through the real root, at the run dir the parent made in the state home
    // it named: the parent resumes from that ledger after the kill, so it
    // cannot live in a scratch of this child's own.  A real launch finds its
    // run dir already made.
    let dir = EXARCH
        .own(XdgKind::State)
        .and_then(|app| app.dir(CHILD_RUN))
        .expect("the run dir the parent made");
    let mut session = Agent::root_for_test(
        root_config(&dir),
        identity_engine("resume-child"),
        scripted("test-model", Script::new().then(Reply::text("before kill"))),
    )
    .expect("child agent");
    drive(&mut session, "before the kill");

    // The first exchange spent ids 1 and 2, so the next prompt is 3.
    let record = crate::record::Record::Transcript(crate::record::Transcript::UserPrompt {
        turn: 3,
        text: "crash prompt".into(),
        voice: Voice::Human,
    });
    let path = dir.path().join("sessions/0/record.jsonl");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("record log");
    file.write_all(commit_line(&serde_json::json!([record])).as_bytes())
        .expect("write crash prompt");
    file.write_all(b"\n").expect("terminate crash prompt line");
    file.flush().expect("flush crash prompt");
    loop {
        std::thread::park_timeout(Duration::from_millis(50));
    }
}
