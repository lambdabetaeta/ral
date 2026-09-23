#![allow(clippy::disallowed_methods)]

//! Process-boundary resume coverage: a scripted child leaves a mid-exchange
//! ledger, the parent terminates it, and a fresh root continues the session.

use exarch::agent::{Avatar, RecordedAccount, RootConfig, RootSeat, deliberate};
use exarch::bootstrap::{EXARCH, Scratch};
use exarch::bus::{Emitter, channel};
use exarch::provider::Provider;
use exarch::provider::scripted::{Reply, Script};
use exarch::record::{self, Blocks, Refusal, View};
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

/// The fold's blocks, each as its own `{:?}` — the debug vocabulary these
/// tests compare against, rather than a rendered string.
fn debug_kinds(blocks: &Blocks) -> Vec<String> {
    blocks
        .blocks()
        .iter()
        .map(|b| format!("{:?}", b.kind()))
        .collect()
}

/// Whether any resident block carries `needle` in its own text — matched on
/// the fold's kinds directly (`Prompt`/`Answer`), not a rendered string.
fn kind_contains(blocks: &Blocks, needle: &str) -> bool {
    blocks.blocks().iter().any(|b| match b.kind() {
        record::BlockKind::Prompt { text } | record::BlockKind::Answer { text } => {
            text.contains(needle)
        }
        _ => false,
    })
}

fn drive(session: &mut Avatar, provider: &Arc<Provider>, prompt: &str) {
    let (tx, _rx) = channel();
    let emit = Emitter::new(tx, session.id());
    let token = exarch::agent::cancel::Token::new();
    let outcome = session.deliberate(provider, Some(prompt.to_string()), None, &token, &emit);
    assert!(matches!(outcome, Ok(deliberate::Outcome::Complete(_))));
}

fn root_config(run_dir: &Path, resume: bool) -> RootConfig {
    RootConfig {
        system: "system".into(),
        caps: ral_core::types::GrantStack::root(),
        run_dir: run_dir.to_path_buf(),
        resume: resume.then(|| run_dir.to_path_buf()),
        run_lock: None,
        model: "test-model".into(),
        account: RecordedAccount::for_test("test"),
        allow_schedule: false,
        interactive: true,
        chat: false,
        thinking_tool: false,
        disk_warn_bytes: None,
        fuel: 0,
        egress: exarch::egress::Egress::for_test(),
        dial: None,
        bureau: Arc::new(exarch::provider::Bureau::Scripted),
    }
}

fn identity_seat(tag: &str) -> RootSeat {
    RootSeat::Identity {
        scratch: Arc::new(Scratch::for_test(EXARCH, tag).expect("scratch dir")),
        cwd: std::env::current_dir().expect("test process has a cwd"),
        terminal: ral_core::io::TerminalState::default(),
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

    let mut resumed = Avatar::root(
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
    let blocks = record::replay::<View>(&record_path, Blocks::default())
        .expect("the resumed session's record log replays cleanly");
    assert!(
        kind_contains(&blocks, "before kill"),
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

    let blocks = record::replay::<View>(&record_path, Blocks::default())
        .expect("the record log still replays cleanly after driving the resumed session");
    assert!(
        kind_contains(&blocks, "continued"),
        "the resumed session's own turn joins the view fold"
    );
    assert!(
        kind_contains(&blocks, "before kill"),
        "and driving the resumed session further does not disturb the pre-kill turn"
    );
}

/// The fold's blocks are a pure function of whatever the log admitted, so
/// folding the same file twice — the regenerability law step 6 exists for —
/// must agree block for block, whether or not a scrollback in between ever
/// flushed `user.log` from a resident window rather than the whole history.
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

    let first = debug_kinds(
        &record::replay::<View>(&path, Blocks::default()).expect("a fresh log replays cleanly"),
    );
    let second = debug_kinds(
        &record::replay::<View>(&path, Blocks::default())
            .expect("replaying the same log twice must agree"),
    );
    assert_eq!(
        first, second,
        "the fold's blocks are a pure function of the log, never an accumulator with its own state"
    );
    assert!(
        first.iter().any(|k| k.contains("hello"))
            && first.iter().any(|k| k.contains("hi back"))
            && first.iter().any(|k| k.contains("a note"))
    );
}

/// A record the fold does not recognise refuses the whole session rather
/// than silently dropping the line or panicking — the versioned display
/// vocabulary's own law.
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

    match record::replay::<View>(&path, Blocks::default()) {
        Err(Refusal::Unreadable(_)) => {}
        Err(other) => panic!("expected an Unreadable refusal, not: {other}"),
        Ok(_) => panic!(
            "a record the fold cannot parse must refuse the whole replay, not silently succeed"
        ),
    }
}

/// A `record.jsonl` written before the identity fields split carries a bare
/// `"provider"` and no `"service"`/`"account"` — the wire shape the
/// rename-not-remove on `record.rs`'s three identity fields exists to keep
/// resumable. The bookend is `Forensic`: a log older than *that* change does
/// not resume at all, and is not what this pins.
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
            "Forensic": {
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

    let resumed = Avatar::root(
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
    let mut session = Avatar::root(
        root_config(&dir, false),
        identity_seat("resume-child"),
        scripted("test-model", Script::new()),
    )
    .expect("child agent");
    let provider = scripted("test-model", Script::new().then(Reply::text("before kill")));
    drive(&mut session, &provider, "before the kill");

    // The first exchange spent ids 1 and 2, so the next prompt is 3.
    let record = exarch::record::Record::Protocol(exarch::record::Protocol::UserPrompt {
        turn: 3,
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
