#![allow(clippy::disallowed_methods)]

//! Process-boundary resume coverage: a scripted child leaves a mid-exchange
//! ledger, the parent terminates it, and a fresh root continues the session.

use exarch::agent::event::SessionEvent;
use exarch::agent::{Agent, RootConfig, RootSeat, deliberate};
use exarch::bootstrap::{EXARCH, Scratch};
use exarch::bus::{Emitter, channel};
use exarch::provider::scripted::{Reply, Script};
use exarch::provider::{Provider, ProviderKind};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

exarch::pre_main_ctor!();

fn scripted(model: &str, script: Script) -> Arc<Provider> {
    Arc::new(Provider::scripted(model, ProviderKind::Openai, script))
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
        provider_label: "test".into(),
        allow_schedule: false,
        interactive: true,
        chat: false,
        disk_warn_bytes: None,
        fuel: 0,
        egress: exarch::egress::Egress::for_test(),
        hatchery: None,
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

    let events = child_dir.join("sessions/0/events.jsonl");
    let ready = (0..200).any(|_| {
        if std::fs::read_to_string(&events).is_ok_and(|text| text.contains("crash prompt")) {
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

    let event = SessionEvent::UserPrompt {
        exchange: 2,
        text: "crash prompt".into(),
    };
    let path = dir.join("sessions/0/events.jsonl");
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("event ledger");
    serde_json::to_writer(&mut file, &event).expect("write crash prompt");
    file.write_all(b"\n").expect("terminate crash prompt line");
    file.flush().expect("flush crash prompt");
    loop {
        std::thread::park_timeout(Duration::from_millis(50));
    }
}
