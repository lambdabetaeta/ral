//! `Agent::resume`'s refusals, and the rollback of a reopening that fails midway.

use super::*;
use crate::record::{Control, Forensic, Opening, Record, read_records};

fn seed_session(sessions: &OwnDir, id: u64, name: &str) {
    drop(
        AgentLog::create(
            sessions,
            AgentId::new(id),
            Opening {
                parent: None,
                name,
                returns: false,
                selected: &Selected::for_test("old-model"),
                system: "system",
                inherited: None,
            },
        )
        .unwrap(),
    );
}

fn records_of(sessions: &OwnDir, id: u64) -> Vec<Record> {
    read_records(&sessions.path().join(id.to_string()).join(RECORD)).unwrap()
}

fn resume(
    dir: &OwnDir,
    cfg: impl FnOnce(LaunchConfig) -> LaunchConfig,
    engine: RootEngine,
) -> io::Result<Vec<(Agent, Resumed)>> {
    Agent::resume(
        cfg(launch_config(dir, 0)),
        engine,
        &scripted("new-model", Script::new()),
    )
}

#[test]
fn chat_is_refused_and_the_open_session_is_left_as_it_was() {
    let dir = tmp("resume-chat");
    let sessions = dir.make_dir("sessions").unwrap();
    seed_session(&sessions, 0, "main");
    let before = records_of(&sessions, 0).len();
    let refused = resume(
        &dir,
        |cfg| LaunchConfig {
            tools: Toolset::default(),
            ..cfg
        },
        identity_engine("resume-chat"),
    );
    assert!(refused.is_err());
    assert_eq!(records_of(&sessions, 0).len(), before);
}

#[cfg(unix)]
#[test]
fn a_wire_engine_is_refused_and_the_open_session_is_left_as_it_was() {
    use std::os::unix::net::UnixStream;
    let dir = tmp("resume-wire");
    let sessions = dir.make_dir("sessions").unwrap();
    seed_session(&sessions, 0, "main");
    let before = records_of(&sessions, 0).len();
    let (host, _guest) = UnixStream::pair().unwrap();
    let (spec, root) = crate::boot::test_attach();
    let refused = resume(
        &dir,
        |cfg| cfg,
        RootEngine::Wire {
            transport: Box::new(WireTransport::adopt(host, Liveness::default()).unwrap()),
            attach: Attach::root(spec, root),
        },
    );
    assert!(refused.is_err());
    assert_eq!(records_of(&sessions, 0).len(), before);
}

#[test]
fn a_run_with_no_open_session_is_refused() {
    let dir = tmp("resume-none");
    dir.make_dir("sessions").unwrap();
    let Err(error) = resume(&dir, |cfg| cfg, identity_engine("resume-none")) else {
        panic!("nothing is open to resume");
    };
    assert_eq!(error.kind(), io::ErrorKind::Other);
}

/// The second root's name is taken by the first: the first is torn down,
/// never closed, so it stays resumable, and the second is never opened.
#[test]
fn a_name_collision_discards_the_roots_already_reopened() {
    let dir = tmp("resume-collision");
    let sessions = dir.make_dir("sessions").unwrap();
    seed_session(&sessions, 0, "main");
    seed_session(&sessions, 1, "main");
    let refused = resume(&dir, |cfg| cfg, identity_engine("resume-collision"));
    assert!(refused.is_err());
    let first = records_of(&sessions, 0);
    assert!(
        first
            .iter()
            .any(|record| matches!(record, Record::Control(Control::SessionResumed { .. })))
    );
    assert!(
        first
            .iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::SessionEnded)))
    );
    assert!(
        !first
            .iter()
            .any(|record| matches!(record, Record::Forensic(Forensic::SessionClosed)))
    );
    assert!(
        !records_of(&sessions, 1)
            .iter()
            .any(|record| matches!(record, Record::Control(Control::SessionResumed { .. })))
    );
    assert_eq!(AgentLog::human_children(&sessions).unwrap().len(), 2);
}

#[test]
fn teardown_failures_ride_on_the_error_that_caused_the_discard() {
    let cause = || io::Error::new(io::ErrorKind::AlreadyExists, "cause");
    let kept = with_teardown_failures(cause(), &[]);
    assert_eq!(kept.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(kept.to_string(), cause().to_string());
    let failures = ["first failure".to_string(), "second failure".to_string()];
    let folded = with_teardown_failures(cause(), &failures).to_string();
    assert!(folded.contains("cause"));
    assert!(failures.iter().all(|failure| folded.contains(failure)));
}
