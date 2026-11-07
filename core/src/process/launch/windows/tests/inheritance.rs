//! Real children at both Windows inheritance entries; no sandbox prerequisites.

use super::*;
use crate::process::spawn_lock::{self, test_phase};
use std::io::Read as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, GetHandleInformation, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;

const DEADLINE: Duration = Duration::from_secs(10);
const HELD: [&str; 3] = [
    "/d",
    "/c",
    "echo inheritance-ready&set /p release=&exit /b 0",
];

fn cmd_exe() -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").expect("Windows system root"))
        .join("System32")
        .join("cmd.exe")
}

/// All returned children are owned even if a channel send or assertion fails.
struct ReapedChild(ChildHandle);

impl ReapedChild {
    fn assert_alive(&self) {
        assert_eq!(
            unsafe { WaitForSingleObject(self.0.raw_process_handle(), 0) },
            WAIT_TIMEOUT,
            "release barrier must still hold the child alive"
        );
    }

    fn finish(&mut self) {
        assert_eq!(
            unsafe { WaitForSingleObject(self.0.raw_process_handle(), 10_000) },
            WAIT_OBJECT_0,
            "child must exit within deadline"
        );
        assert!(self.0.reap().expect("reap child").success());
    }
}

impl Drop for ReapedChild {
    fn drop(&mut self) {
        // Termination is harmless if already exited. No child can outlive a test.
        let _ = self.0.kill();
        let _ = self.0.reap();
    }
}

fn raw_child(launch: &mut Launch) -> io::Result<ReapedChild> {
    match launch.spawn(PgidPolicy::Inherit) {
        Ok(spawned) => Ok(ReapedChild(spawned.child)),
        Err(crate::process::Unstarted::Program(e)) => Err(e),
        Err(crate::process::Unstarted::Refused(why)) => panic!("the launch was refused: {why}"),
        Err(crate::process::Unstarted::Stopped(stop)) => panic!("the launch was stopped: {stop}"),
    }
}

fn phase() -> (Sender<()>, Receiver<()>, Sender<()>, Receiver<()>) {
    let (reached_tx, reached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    (reached_tx, reached_rx, release_tx, release_rx)
}

/// Never block in Read: even a leaked writer must produce a bounded failure.
fn pipe_bytes(reader: &mut os_pipe::PipeReader, until_eof: bool) -> Vec<u8> {
    let deadline = Instant::now() + DEADLINE;
    let mut bytes = Vec::new();
    loop {
        let mut available = 0;
        let ok = unsafe {
            PeekNamedPipe(
                reader.as_raw_handle(),
                null_mut(),
                0,
                null_mut(),
                &raw mut available,
                null_mut(),
            )
        };
        if ok == 0 {
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(i32::try_from(ERROR_BROKEN_PIPE).unwrap()),
                "peek pipe"
            );
            assert!(until_eof, "child exited before readiness marker");
            return bytes;
        }
        if available != 0 {
            let mut buffer = [0; 4096];
            let count = usize::try_from(available).unwrap().min(buffer.len());
            let read = reader
                .read(&mut buffer[..count])
                .expect("read pending bytes");
            bytes.extend_from_slice(&buffer[..read]);
            if !until_eof
                && bytes
                    .windows(b"inheritance-ready".len())
                    .any(|s| s == b"inheritance-ready")
            {
                return bytes;
            }
        }
        assert!(Instant::now() < deadline, "pipe observation timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn assert_not_inheritable(writer: &os_pipe::PipeWriter) {
    let mut flags = 0;
    assert_ne!(
        unsafe { GetHandleInformation(writer.as_raw_handle(), &raw mut flags) },
        0
    );
    assert_eq!(flags & HANDLE_FLAG_INHERIT, 0, "temporary flag restored");
}

fn overlap(fail_creation: bool) {
    let missing = tempfile::tempdir().expect("unique absent image directory");
    let image = if fail_creation {
        missing.path().join("absent.exe")
    } else {
        cmd_exe()
    };
    let (mut reader, writer) = spawn_lock::cloexec_pipe().expect("unique raw pipe");
    let (raw_stdin, raw_release) = spawn_lock::cloexec_pipe().expect("raw release pipe");
    let (std_stdin, std_release) = spawn_lock::cloexec_pipe().expect("standard release pipe");
    let (raw_reached_tx, raw_reached, raw_continue, raw_continue_rx) = phase();
    let (std_reached_tx, std_reached, std_continue, std_continue_rx) = phase();
    let (raw_tx, raw_rx) = mpsc::channel();
    let (std_tx, std_rx) = mpsc::channel();

    std::thread::scope(|scope| {
        // On unwinding, these close stdin before scoped joins. Phase receivers
        // also disconnect, so no worker or child waits for a failed observer.
        let raw_release = raw_release;
        let std_release = std_release;
        let raw_continue = raw_continue;
        let std_continue = std_continue;
        scope.spawn(move || {
            let mut launch = Launch::new(image);
            launch.args(HELD);
            launch.stdin(StdioSpec::from_owned(raw_stdin));
            launch.stdout(StdioSpec::from_owned(
                writer.try_clone().expect("stdout copy"),
            ));
            launch.stderr(StdioSpec::Null);
            // Retain one admitted handle after failure to observe rollback,
            // rather than letting ChildStdio's close mask a missing restoration.
            launch.admitted_handles.push(writer.as_raw_handle());
            test_phase::install(raw_reached_tx, raw_continue_rx);
            let result = raw_child(&mut launch);
            let _ = raw_tx.send((result, writer));
        });
        raw_reached
            .recv_timeout(DEADLINE)
            .expect("raw inheritance flags set");
        scope.spawn(move || {
            let mut command = Command::new(cmd_exe());
            command
                .args(HELD)
                .stdin(std_stdin)
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            test_phase::install(std_reached_tx, std_continue_rx);
            let child = spawn_lock::spawn(&mut command)
                .map(|child| ReapedChild(ChildHandle::from_std(child)));
            let _ = std_tx.send(child);
        });
        std_reached
            .recv_timeout(DEADLINE)
            .expect("standard spawn at lock entry");
        std_continue.send(()).expect("attempt standard spawn");
        assert!(
            matches!(
                std_rx.recv_timeout(Duration::from_millis(200)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "standard child must not spawn inside raw inheritance window"
        );
        raw_continue.send(()).expect("finish raw creation");
        let (raw_result, writer) = raw_rx.recv_timeout(DEADLINE).expect("raw spawn returned");
        assert_not_inheritable(&writer);
        let mut unrelated = std_rx
            .recv_timeout(DEADLINE)
            .expect("standard spawn returned")
            .expect("standard spawn");
        unrelated.assert_alive();
        if fail_creation {
            let Err(error) = raw_result else {
                panic!("absent image unexpectedly launched");
            };
            assert_eq!(
                error.raw_os_error(),
                Some(i32::try_from(ERROR_FILE_NOT_FOUND).unwrap())
            );
            // A distinct inheriting child after rollback must not retain the
            // still-open admitted writer, even if no concurrent spawn leaked it.
            let mut command = Command::new(cmd_exe());
            command
                .args(HELD)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child = spawn_lock::spawn(&mut command).expect("subsequent standard spawn");
            let release = child.stdin.take().expect("release stdin");
            let mut subsequent = ReapedChild(ChildHandle::from_std(child));
            drop(writer);
            assert_eq!(pipe_bytes(&mut reader, true), [] as [u8; 0]);
            unrelated.assert_alive();
            subsequent.assert_alive();
            drop(release);
            subsequent.finish();
            let mut launch = Launch::new(cmd_exe());
            launch.args(["/d", "/c", "exit /b 0"]);
            launch
                .stdin(StdioSpec::Null)
                .stdout(StdioSpec::Null)
                .stderr(StdioSpec::Null);
            raw_child(&mut launch)
                .expect("subsequent raw launch")
                .finish();
        } else {
            let mut intended = raw_result.expect("raw spawn");
            intended.assert_alive();
            drop(raw_release);
            intended.finish();
            drop(writer);
            let bytes = pipe_bytes(&mut reader, true);
            assert!(
                bytes
                    .windows(b"inheritance-ready".len())
                    .any(|s| s == b"inheritance-ready"),
                "intended child wrote to its pipe"
            );
            unrelated.assert_alive();
        }
        drop(std_release);
        unrelated.finish();
    });
}

#[test]
fn raw_launch_and_standard_spawn_serialize_inheritance_until_eof() {
    overlap(false);
}

#[test]
fn failed_raw_creation_restores_inheritance_and_allows_subsequent_launches() {
    overlap(true);
}

#[test]
fn inheritance_lock_does_not_span_standard_or_raw_child_waits() {
    let (stdin, release_standard) = spawn_lock::cloexec_pipe().expect("standard release pipe");
    let (mut ready, ready_writer) = spawn_lock::cloexec_pipe().expect("standard readiness pipe");
    let (status_tx, status_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let release_standard = release_standard;
        scope.spawn(move || {
            let mut command = Command::new(cmd_exe());
            command
                .args(HELD)
                .stdin(stdin)
                .stdout(ready_writer)
                .stderr(Stdio::null());
            let _ = status_tx.send(spawn_lock::status(&mut command));
        });
        pipe_bytes(&mut ready, false);
        assert!(matches!(
            status_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        let (stdin, release_raw) = spawn_lock::cloexec_pipe().expect("raw release pipe");
        let (raw_tx, raw_rx) = mpsc::channel();
        scope.spawn(move || {
            let mut launch = Launch::new(cmd_exe());
            launch.args(HELD);
            launch
                .stdin(StdioSpec::from_owned(stdin))
                .stdout(StdioSpec::Null)
                .stderr(StdioSpec::Null);
            let _ = raw_tx.send(raw_child(&mut launch));
        });
        let mut raw = raw_rx
            .recv_timeout(DEADLINE)
            .expect("raw launch while status waits")
            .expect("raw launch");
        raw.assert_alive();
        let (output_tx, output_rx) = mpsc::channel();
        scope.spawn(move || {
            let mut command = Command::new(cmd_exe());
            command.args(["/d", "/c", "echo independent"]);
            let _ = output_tx.send(spawn_lock::output(&mut command));
        });
        let output = output_rx
            .recv_timeout(DEADLINE)
            .expect("standard output while raw lives")
            .expect("output");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"independent\r\n");
        raw.assert_alive();
        drop(release_raw);
        raw.finish();
        assert!(matches!(
            status_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        drop(release_standard);
        assert!(
            status_rx
                .recv_timeout(DEADLINE)
                .expect("status completed")
                .expect("status")
                .success()
        );
    });
}
