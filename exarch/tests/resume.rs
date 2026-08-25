#![allow(clippy::disallowed_methods)]

//! Process-boundary resume coverage: a scripted child leaves a mid-exchange
//! ledger, the parent terminates it, and a fresh root continues the session.

use exarch::agent::{Agent, RecordedAccount, RootConfig, RootSeat, deliberate};
use exarch::bootstrap::{EXARCH, Scratch};
use exarch::bus::{Emitter, channel};
use exarch::provider::Provider;
use exarch::provider::scripted::{Reply, Script};
use exarch::record::{self, Refusal, View};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

exarch::pre_main_ctor!();

fn scripted(model: &str, script: Script) -> Arc<Provider> {
    Arc::new(Provider::scripted(model, script))
}

/// A hand-written `record.jsonl` line needs the `Entry` envelope too, since
/// [`record::log::Log::read`] is private to the crate and these tests write
/// past it on purpose.
fn envelope_line(record: &record::Record) -> String {
    serde_json::json!({ "at_unix_ms": 0, "record": record }).to_string()
}

fn drive(session: &mut Agent, provider: &Arc<Provider>, prompt: &str) {
    let (tx, _rx) = channel();
    let emit = Emitter::new(tx, session.id);
    let token = exarch::agent::cancel::Token::new();
    let outcome = session.deliberate(provider, Some(prompt.to_string()), None, &token, &emit);
    assert!(matches!(outcome, Ok(deliberate::Outcome::Complete(_))));
}

fn root_config(run_dir: &Path, resume: bool) -> RootConfig {
    RootConfig {
        system: "system".into(),
        caps: ral_core::types::Capabilities::default(),
        run_dir: run_dir.to_path_buf(),
        resume: resume.then(|| run_dir.to_path_buf()),
        no_logs: false,
        run_lock: None,
        model: "test-model".into(),
        account: RecordedAccount::for_test("test"),
        allow_schedule: false,
        interactive: true,
        chat: false,
        disk_warn_bytes: None,
        fuel: 0,
        egress: exarch::egress::Egress::for_test(),
        dial: None,
    }
}

fn identity_seat(tag: &str) -> RootSeat {
    RootSeat::Identity {
        scratch: Arc::new(Scratch::for_test(EXARCH, tag).expect("scratch dir")),
        cwd: std::env::current_dir().expect("test process has a cwd"),
        detach: false,
    }
}

#[test]
fn scripted_run_kill_resume_and_continue() {
    let root = tempfile::tempdir().expect("run root");
    let child_dir = root.path().join("child");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg("resume_child")
        .arg("--nocapture")
        .env("EXARCH_RESUME_CHILD_DIR", &child_dir)
        .spawn()
        .expect("spawn scripted child");

    let record_log = child_dir.join("sessions/0/record.jsonl");
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

    let mut resumed = Agent::root(
        root_config(&child_dir, true),
        identity_seat("resume-parent"),
        scripted("test-model", Script::new()),
    )
    .expect("resume child ledger");
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
    let record_path = resumed.log_dir().join("record.jsonl");
    let blocks = record::replay::<View>(&record_path)
        .expect("the resumed session's record log replays cleanly");
    assert!(
        blocks.render_log().contains("before kill"),
        "the view fold must carry the pre-kill exchange across the crash and resume too"
    );

    drive(
        &mut resumed,
        &scripted("test-model", Script::new().then(Reply::text("continued"))),
        "continue after the crash",
    );
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

    let blocks = record::replay::<View>(&record_path)
        .expect("the record log still replays cleanly after driving the resumed session");
    let rendered = blocks.render_log();
    assert!(
        rendered.contains("continued"),
        "the resumed session's own turn joins the view fold: {rendered:?}"
    );
    assert!(
        rendered.contains("before kill"),
        "and driving the resumed session further does not disturb the pre-kill turn: {rendered:?}"
    );
}

/// `render_log` is a pure rendering of whatever the fold admitted, so folding
/// the same file twice — the regenerability law step 6 exists for — must
/// agree byte for byte, whether or not a viewport in between ever flushed
/// `user.log` from a resident window rather than the whole history.
#[test]
fn the_view_folds_render_is_a_pure_function_of_the_log() {
    let root = tempfile::tempdir().expect("scratch dir");
    let path = root.path().join("record.jsonl");
    let emit = record::Emitter::create(&path).expect("fresh record log");
    let _ = emit
        .emit(record::Display::Prompt {
            text: "hello".into(),
        })
        .expect("a display commit records");
    let _ = emit
        .emit(record::Display::Answer {
            text: "hi back".into(),
        })
        .expect("a display commit records");
    let _ = emit
        .emit(record::Forensic::SystemNote {
            text: "a note".into(),
        })
        .expect("a forensic record records");

    let first = record::replay::<View>(&path)
        .expect("a fresh log replays cleanly")
        .render_log();
    let second = record::replay::<View>(&path)
        .expect("replaying the same log twice must agree")
        .render_log();
    assert_eq!(
        first, second,
        "the render is a pure function of the log, never an accumulator with its own state"
    );
    assert!(first.contains("hello") && first.contains("hi back") && first.contains("a note"));
}

/// A record the fold does not recognise refuses the whole session rather
/// than silently dropping the line or panicking — the versioned display
/// vocabulary's own law (`admissible_event`'s old `_ => true` catch-all dies
/// with this move).
#[test]
fn replay_refuses_a_ledger_line_it_does_not_recognise() {
    let root = tempfile::tempdir().expect("scratch dir");
    let path = root.path().join("record.jsonl");
    {
        let emit = record::Emitter::create(&path).expect("fresh record log");
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
        r#"{{"at_unix_ms":0,"record":{{"FutureClass":{{"anything":1}}}}}}"#
    )
    .expect("append a foreign line");
    file.flush().expect("flush the foreign line");

    match record::replay::<View>(&path) {
        Err(Refusal::Unreadable(_)) => {}
        Err(other) => panic!("expected an Unreadable refusal, not: {other}"),
        Ok(_) => panic!(
            "a record the fold cannot parse must refuse the whole replay, not silently succeed"
        ),
    }
}

/// A `record.jsonl` written before this change carries a bare `"provider"`
/// field and no `"service"`/`"account"` — the wire shape the rename-not-remove
/// on `record.rs`'s three identity fields exists to keep resumable.
#[test]
fn a_pre_change_record_log_still_resumes() {
    let root = tempfile::tempdir().expect("run root");
    let run_dir = root.path().to_path_buf();
    let sessions = run_dir.join("sessions/0");
    std::fs::create_dir_all(&sessions).expect("session dir");
    let path = sessions.join("record.jsonl");
    let line = serde_json::json!({
        "at_unix_ms": 0,
        "record": {
            "Protocol": {
                "kind": "session_started",
                "session_id": 0,
                "parent": null,
                "model": "old-model",
                "provider": "old-label",
                "system_prompt_bytes": 0,
                "log_dir": sessions,
                "at_unix_ms": 0,
            }
        }
    })
    .to_string();
    std::fs::write(&path, format!("{line}\n")).expect("write pre-change record.jsonl");

    let resumed = Agent::root(
        root_config(&run_dir, true),
        identity_seat("pre-change-resume"),
        scripted("test-model", Script::new()),
    )
    .expect("a pre-change record.jsonl must still resume");
    assert!(resumed.is_ready());
}

#[test]
fn resume_child() {
    let Ok(dir) = std::env::var("EXARCH_RESUME_CHILD_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    // Through the real root, at the run dir the parent chose: the parent
    // resumes from that ledger after the kill, so it cannot live in a scratch
    // of this child's own.  A real launch finds its run dir already made.
    std::fs::create_dir_all(&dir).expect("child run dir");
    let mut session = Agent::root(
        root_config(&dir, false),
        identity_seat("resume-child"),
        scripted("test-model", Script::new()),
    )
    .expect("child agent");
    let provider = scripted("test-model", Script::new().then(Reply::text("before kill")));
    drive(&mut session, &provider, "before the kill");

    let record = exarch::record::Record::Protocol(exarch::record::Protocol::UserPrompt {
        exchange: 2,
        text: "crash prompt".into(),
    });
    let path = dir.join("sessions/0/record.jsonl");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("record log");
    file.write_all(envelope_line(&record).as_bytes())
        .expect("write crash prompt");
    file.write_all(b"\n").expect("terminate crash prompt line");
    file.flush().expect("flush crash prompt");
    loop {
        std::thread::park_timeout(Duration::from_millis(50));
    }
}
