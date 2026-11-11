use super::*;
use crate::fact::{
    DataStep, DecodeCause, JsonInputFailure, LaunchFailure, LauncherFailure, PayloadGroupFailure,
    Reason,
};
use std::io::{BufRead, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::process::Stdio;

fn record(bytes: &[u8]) -> InfoFd {
    let (reader, mut writer) = UnixStream::pair().unwrap();
    writer.write_all(bytes).unwrap();
    InfoFd {
        reader,
        writers: [writer.as_fd().try_clone_to_owned().unwrap(), writer.into()],
    }
}

#[test]
fn an_info_record_is_absent_valid_or_refused() {
    assert_eq!(record(b"").payload_pgid().unwrap(), None);
    assert_eq!(
        record(br#"{"child-pid":123}"#)
            .payload_pgid()
            .unwrap()
            .unwrap()
            .as_raw(),
        123
    );
    for bytes in [
        b"{".as_slice(),
        b" ",
        br#"{"child-pid":0}"#,
        br#"{"child-pid":-1}"#,
        br#"{"child-pid":"123"}"#,
        b"{}",
        b"null",
    ] {
        assert!(matches!(
            record(bytes).payload_pgid(),
            Err(PayloadGroupFailure::Json(_) | PayloadGroupFailure::InvalidPid(_))
        ));
    }
}

#[test]
fn an_unreadable_info_record_refuses_the_spawn() {
    let (reader, writer) = UnixStream::pair().unwrap();
    reader.set_nonblocking(true).unwrap();
    let info = InfoFd {
        reader,
        writers: [
            writer.as_fd().try_clone_to_owned().unwrap(),
            writer.as_fd().try_clone_to_owned().unwrap(),
        ],
    };
    let why = info.payload_pgid().unwrap_err();
    assert!(matches!(why, PayloadGroupFailure::Read(_)));
    drop(writer);
}

#[test]
fn a_truncated_info_record_refuses_spawn_and_reaps_the_envelope() {
    use crate::process::Placement;
    use crate::process::launch::EnvelopedLaunch;
    use std::sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    };

    let (reader, writer) = os_pipe::pipe().unwrap();
    let mut handoff = Handoff::default();
    let at = handoff
        .lend(writer.as_fd(), crate::fact::ParcelRole::InfoSocket)
        .unwrap();
    let mut command = std::process::Command::new("/bin/sh");
    command
        .args(["-c", &format!("echo $$ > /proc/self/fd/{at}; read line")])
        .stdin(Stdio::piped());
    let mut evidence = crate::process::launch::setup::Evidence::new().unwrap();
    handoff.install(&mut command, &mut evidence);
    let pid = Arc::new(AtomicI32::new(0));
    let child_pid = pid.clone();
    let info = record(br#"{"child-pid":"#);
    let (report, report_writer) = crate::process::cloexec_socketpair().unwrap();
    let launch = EnvelopedLaunch::synthetic(
        command,
        crate::fact::LauncherRole::Bwrap,
        Some(Box::new(move || {
            let mut line = String::new();
            std::io::BufReader::new(reader)
                .read_line(&mut line)
                .unwrap();
            child_pid.store(line.trim().parse().unwrap(), Ordering::SeqCst);
            info.payload_pgid()
        })),
        crate::process::report::Report::new(report, report_writer.into()),
    )
    .with_setup_evidence(evidence);
    let Err(crate::process::Unstarted::Refused(why)) = launch.spawn(
        Placement::Inherit,
        &crate::process::DurableRoot::default().child(),
    ) else {
        panic!("spawn must refuse");
    };
    assert!(
        matches!(&why.reason, Reason::Launch(failure) if matches!(**failure, LaunchFailure::Launcher(LauncherFailure::PayloadGroup(PayloadGroupFailure::Json(_)))))
    );
    let pid = rustix::process::Pid::from_raw(pid.load(Ordering::SeqCst)).unwrap();
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    assert_eq!(
        rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG).unwrap_err(),
        rustix::io::Errno::CHILD
    );
}

#[test]
fn bwrap_info_preserves_derived_struct_policies_and_pid_validation() {
    for bytes in [br#"{"child-pid":123,"unknown":false}"#.as_slice(), b"[123]"] {
        assert_eq!(record(bytes).payload_pgid().unwrap().unwrap().as_raw(), 123);
    }
    let Err(PayloadGroupFailure::Json(JsonInputFailure::Decode(duplicate))) =
        record(br#"{"child-pid":123,"child-pid":456}"#).payload_pgid()
    else {
        panic!("duplicate recognized field must be refused")
    };
    assert_eq!(duplicate.path, []);
    assert_eq!(
        *duplicate.cause,
        DecodeCause::DuplicateField("child-pid".into())
    );
    let Err(PayloadGroupFailure::Json(JsonInputFailure::Decode(wrong_type))) =
        record(br#"{"child-pid":"123"}"#).payload_pgid()
    else {
        panic!("PID must be an integer")
    };
    assert_eq!(wrong_type.path, [DataStep::Field("child-pid".into())]);
    assert!(matches!(*wrong_type.cause, DecodeCause::JsonShape(_)));
    for pid in [0, -1] {
        assert_eq!(
            record(format!("[{pid}]").as_bytes()).payload_pgid(),
            Err(PayloadGroupFailure::InvalidPid(pid))
        );
    }
    assert_eq!(record(b"").payload_pgid(), Ok(None));
    assert!(matches!(
        record(b" ").payload_pgid(),
        Err(PayloadGroupFailure::Json(JsonInputFailure::Syntax(_)))
    ));
    assert!(matches!(
        record(b"[123,456]").payload_pgid(),
        Err(PayloadGroupFailure::Json(JsonInputFailure::Decode(_)))
    ));
}
