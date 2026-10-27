use super::super::InfoFd;
use super::*;
use std::io::{BufRead, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;

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
        assert!(
            record(bytes)
                .payload_pgid()
                .unwrap_err()
                .contains("--info-fd")
        );
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
    assert!(why.contains("cannot read bwrap's --info-fd"), "{why}");
    drop(writer);
}

#[test]
fn a_truncated_info_record_refuses_spawn_and_reaps_the_envelope() {
    use crate::process::{Launch, PgidPolicy};
    use std::sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    };

    let (reader, writer) = os_pipe::pipe().unwrap();
    let mut handoff = Handoff::default();
    let at = handoff.lend(writer.as_fd()).unwrap();
    let mut command = std::process::Command::new("/bin/sh");
    command
        .args(["-c", &format!("echo $$ > /proc/self/fd/{at}; read line")])
        .stdin(Stdio::piped());
    handoff.install(&mut command);
    let pid = Arc::new(AtomicI32::new(0));
    let child_pid = pid.clone();
    let info = record(br#"{"child-pid":"#);
    let mut launch = Launch::from_command(command);
    launch.envelope(crate::process::launch::Envelope {
        program: "bwrap",
        payload_pgid: Some(Box::new(move || {
            let mut line = String::new();
            std::io::BufReader::new(reader)
                .read_line(&mut line)
                .unwrap();
            child_pid.store(line.trim().parse().unwrap(), Ordering::SeqCst);
            info.payload_pgid()
        })),
    });
    let why = launch
        .spawn(PgidPolicy::NewLeader)
        .err()
        .expect("spawn must refuse");
    assert!(
        why.to_string().contains("malformed bwrap --info-fd"),
        "{why}"
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
