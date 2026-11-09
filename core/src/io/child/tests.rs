use super::*;
use crate::io::{Edge, new_buffer};
use crate::process::{DurableRoot, Placement};
use std::io::Write;
use std::sync::Arc;

#[test]
fn descriptor_destinations_need_no_drains() {
    let file = Sink::File(Arc::new(tempfile::tempfile().unwrap()));
    let (_reader, writer) = crate::process::cloexec_pipe().unwrap();
    let pipe = Sink::Pipe {
        writer: Arc::new(writer),
        edge: Edge::new(),
        stage: DurableRoot::default().child().id(),
    };
    let mut drains = PendingDrains::default();
    for sink in [&file, &pipe] {
        for fd in [OutFd::Stdout, OutFd::Stderr] {
            assert!(matches!(
                drains.bind(sink, fd).unwrap(),
                StdioSpec::Owned(_)
            ));
        }
    }
    assert!(drains.0.is_empty());
}

#[test]
fn standard_streams_need_no_drains() {
    let mut drains = PendingDrains::default();
    assert!(matches!(
        drains.bind(&Sink::Stdout, OutFd::Stdout).unwrap(),
        StdioSpec::Inherit
    ));
    assert!(matches!(
        drains.bind(&Sink::Stderr, OutFd::Stderr).unwrap(),
        StdioSpec::Inherit
    ));
    for (sink, fd) in [(Sink::Stdout, OutFd::Stderr), (Sink::Stderr, OutFd::Stdout)] {
        assert!(matches!(
            drains.bind(&sink, fd).unwrap(),
            StdioSpec::Owned(_)
        ));
    }
    assert!(drains.0.is_empty());
}

#[test]
fn a_drain_already_owns_its_reader_before_any_child_exists() {
    let (sink, bytes) = new_buffer();
    let mut pending = PendingDrains::default();
    let StdioSpec::Owned(writer) = pending.bind(&sink, OutFd::Stdout).unwrap() else {
        panic!("a drain has a writer");
    };
    let scope = DurableRoot::default().child();
    let drains = pending.start(&scope).unwrap();
    let mut writer = std::fs::File::from(writer);
    writer.write_all(b"owned reader").unwrap();
    drop(writer);
    drains.join_under(&scope, || panic!("not cancelled"));
    assert_eq!(bytes.take().bytes, b"owned reader");
}

#[test]
fn joined_destinations_have_one_drain_and_separate_destinations_have_two() {
    let (stdout, _) = new_buffer();
    let (stderr, _) = new_buffer();
    let joined = Connection::new(&Source::Empty, &stdout, &stdout).unwrap();
    assert!(matches!(joined.stderr, Stderr::Stdout));
    assert_eq!(joined.drains.0.len(), 1);
    let separate = Connection::new(&Source::Empty, &stdout, &stderr).unwrap();
    assert!(matches!(separate.stderr, Stderr::Separate(_)));
    assert_eq!(separate.drains.0.len(), 2);
}

#[test]
fn a_downstream_writer_moves_without_a_drain_or_retained_duplicate() {
    let (mut reader, writer) = crate::process::cloexec_pipe().unwrap();
    let io = Connection::to_pipe(&Source::Empty, writer, &Sink::Stderr).unwrap();
    assert!(io.drains.0.is_empty());
    let mut launch = Launch::new("unused");
    let _pending = io.install(&mut launch);
    drop(launch);
    assert_eq!(reader.read(&mut [0; 1]).unwrap(), 0);
}

#[test]
fn failed_spawn_releases_writers_without_starting_drains() {
    let (sink, bytes) = new_buffer();
    let mut launch = Launch::new("ral-no-such-program-exists");
    let pending = Connection::new(&Source::Empty, &sink, &sink)
        .unwrap()
        .install(&mut launch);
    let scope = DurableRoot::default().child();
    assert!(launch.spawn(Placement::Inherit, &scope).is_err());
    let drains = pending.start(&scope).unwrap();
    drains.join_under(&scope, || panic!("not cancelled"));
    assert_eq!(bytes.take().bytes, b"");
}

#[test]
fn merged_child_streams_reach_the_same_reader_on_each_platform() {
    #[cfg(unix)]
    let mut launch = Launch::new("/bin/sh");
    #[cfg(unix)]
    launch.args(["-c", "printf 'OUT\\n'; printf 'ERR\\n' >&2"]);
    #[cfg(windows)]
    let mut launch = Launch::new("cmd.exe");
    #[cfg(windows)]
    launch.args(["/d", "/c", "echo OUT&echo ERR 1>&2"]);

    let (sink, bytes) = new_buffer();
    let pending = Connection::new(&Source::Empty, &sink, &sink)
        .unwrap()
        .install(&mut launch);
    let scope = DurableRoot::default().child();
    let mut child = launch.spawn(Placement::Inherit, &scope).unwrap().child;
    // Explicit endpoints, not backend-created output pipes.
    assert!(child.take_stdout().is_none());
    assert!(child.take_stderr().is_none());
    let drains = pending.start(&scope).unwrap();
    assert!(child.reap().unwrap().success());
    drains.join_under(&scope, || panic!("not cancelled"));
    let output = bytes.take();
    let lines: Vec<_> = std::str::from_utf8(&output.bytes)
        .unwrap()
        .lines()
        .map(str::trim)
        .collect();
    assert_eq!(lines, ["OUT", "ERR"]);
}

#[test]
fn a_drain_flushes_a_watch_tail_once() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    let sink = Sink::Watch {
        line: Arc::new(move |line| log.lock().unwrap().push(line.to_vec())),
        pending: Vec::new(),
    };
    let mut pending = PendingDrains::default();
    let StdioSpec::Owned(writer) = pending.bind(&sink, OutFd::Stdout).unwrap() else {
        panic!("a drain has a writer");
    };
    let scope = DurableRoot::default().child();
    let drains = pending.start(&scope).unwrap();
    let mut writer = std::fs::File::from(writer);
    writer.write_all(b"line\ntail").unwrap();
    drop(writer);
    drains.join_under(&scope, || panic!("not cancelled"));
    assert_eq!(*seen.lock().unwrap(), [b"line".to_vec(), b"tail".to_vec()]);
}

#[cfg(unix)]
#[test]
fn a_drain_blocked_on_a_full_edge_still_drains_its_source_after_cancellation() {
    use crate::process::CancelCause;
    use rustix::event::PollFlags;
    use std::time::{Duration, Instant};

    let scope = DurableRoot::default().child();
    let (_reader, writer) = crate::process::cloexec_pipe().unwrap();
    while crate::io::poll_slice(&writer, PollFlags::OUT)
        .unwrap()
        .is_some()
    {
        (&writer).write_all(&[0; libc::PIPE_BUF]).unwrap();
    }
    let sink = Sink::Pipe {
        writer: Arc::new(writer),
        edge: Edge::new(),
        stage: scope.id(),
    };
    let (reader, mut feed) = crate::process::cloexec_pipe().unwrap();
    let feeder = std::thread::spawn(move || feed.write_all(&vec![b'x'; 1 << 20]).unwrap());
    let drains = PendingDrains(vec![PendingDrain { reader, sink }])
        .start(&scope)
        .unwrap();
    std::thread::sleep(Duration::from_millis(30));
    assert!(!drains.0[0].is_finished());
    scope.cancel(CancelCause::Interrupted);
    let start = Instant::now();
    while !drains.0[0].is_finished() {
        assert!(start.elapsed() < crate::test_helper::NEVER_RETURNS);
        std::thread::sleep(Duration::from_millis(1));
    }
    drains.join_under(&scope, || {});
    feeder.join().unwrap();
}
