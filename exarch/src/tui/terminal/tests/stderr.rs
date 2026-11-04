//! Real child stdio; process-wide redirection runs only in an isolated child.

use super::super::{editor_stderr, redirect_stderr_to_file, restore_stderr};
use std::fs::File;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

const DIRECTORY: &str = "EXARCH_EDITOR_STDERR_TEST_DIRECTORY";
const CHILD_MARKER: &[u8] = b"editor diagnostic\n";
const PARENT_MARKER: &[u8] = b"parent diagnostic\n";

fn child(test: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            test,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[test]
#[ignore = "writes to the stderr supplied by its parent"]
fn editor_stderr_writer() {
    std::io::stderr().write_all(CHILD_MARKER).unwrap();
}

#[test]
fn saved_stderr_is_reusable_and_only_routed_to_the_child() {
    check_routing("native");
    #[cfg(windows)]
    check_routing("crt-alias");
}

fn check_routing(scenario: &str) {
    let dir = tempfile::tempdir().unwrap();
    let mut command = child("tui::terminal::tests::stderr::saved_stderr_routing_child");
    command
        .env(DIRECTORY, dir.path())
        .env("EXARCH_EDITOR_STDERR_SCENARIO", scenario);
    assert!(ral_core::process::status(&mut command).unwrap().success());
    // Actual bytes also prove the exact child tests were selected and ran.
    assert_eq!(
        std::fs::read(dir.path().join("saved")).unwrap(),
        CHILD_MARKER.repeat(2)
    );
    assert_eq!(
        std::fs::read(dir.path().join("log")).unwrap(),
        PARENT_MARKER.repeat(2)
    );
}

#[test]
#[ignore = "owns process-wide stderr redirection"]
fn saved_stderr_routing_child() {
    let dir = std::env::var_os(DIRECTORY).expect("isolated child directory");
    let dir = Path::new(&dir);
    let original = File::create(dir.join("saved")).unwrap();
    #[cfg(unix)]
    let initial = redirect_stderr_to_file(original.try_clone().unwrap()).unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        if std::env::var("EXARCH_EDITOR_STDERR_SCENARIO").unwrap() == "crt-alias" {
            install_crt_stderr(original.try_clone().unwrap());
            // SAFETY: fd 2 is open; keep the native slot aliased to that CRT handle.
            set_native_stderr(unsafe { libc::get_osfhandle(2) } as _);
        } else {
            set_native_stderr(original.as_raw_handle());
        }
    }
    #[cfg(windows)]
    let saved = super::super::snapshot_editor_stderr().unwrap().unwrap();
    let log = File::create(dir.join("log")).unwrap();
    #[cfg(unix)]
    let saved = redirect_stderr_to_file(log).unwrap();
    #[cfg(windows)]
    let backup = redirect_stderr_to_file(log).unwrap();

    assert_not_inheritable(&saved);
    for _ in 0..2 {
        let mut command = child("tui::terminal::tests::stderr::editor_stderr_writer");
        command.stderr(editor_stderr(Some(&saved)).unwrap());
        assert!(ral_core::process::status(&mut command).unwrap().success());
        std::io::stderr().write_all(PARENT_MARKER).unwrap();
        assert_not_inheritable(&saved);
    }
    #[cfg(unix)]
    {
        restore_stderr(saved);
        restore_stderr(initial);
    }
    #[cfg(windows)]
    {
        restore_stderr(backup);
        // Do not leave the preexisting restoration ordering's raw handle installed.
        set_native_stderr(std::ptr::null_mut());
    }
}

#[cfg(unix)]
fn assert_not_inheritable(saved: &std::os::fd::OwnedFd) {
    assert!(
        rustix::io::fcntl_getfd(saved)
            .unwrap()
            .contains(rustix::io::FdFlags::CLOEXEC)
    );
}

#[cfg(windows)]
fn assert_not_inheritable(saved: &std::os::windows::io::OwnedHandle) {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
    let mut flags = 0;
    // SAFETY: saved owns a live handle and flags is an out-parameter.
    assert_ne!(
        unsafe { GetHandleInformation(saved.as_raw_handle(), &raw mut flags) },
        0
    );
    assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
}

#[cfg(windows)]
fn set_native_stderr(handle: std::os::windows::io::RawHandle) {
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
    // SAFETY: the isolated process owns this slot, including absent/stale test cases.
    assert_ne!(unsafe { SetStdHandle(STD_ERROR_HANDLE, handle) }, 0);
}

#[cfg(windows)]
fn install_crt_stderr(file: File) {
    use std::os::windows::io::IntoRawHandle as _;
    // SAFETY: the CRT adopts this owned file handle on success.
    let fd = unsafe { libc::open_osfhandle(file.into_raw_handle() as isize, libc::O_BINARY) };
    assert!(fd >= 0);
    // SAFETY: fd is open, and this process owns stderr's CRT slot.
    assert_eq!(unsafe { libc::dup2(fd, 2) }, 0);
    // SAFETY: fd 2 holds its own duplicate.
    assert_eq!(unsafe { libc::close(fd) }, 0);
}

#[cfg(windows)]
#[test]
fn absent_native_stderr_never_inherits_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let mut command = child("tui::terminal::tests::stderr::absent_native_stderr_child");
    command.env(DIRECTORY, dir.path());
    assert!(ral_core::process::status(&mut command).unwrap().success());
    assert_eq!(
        std::fs::read(dir.path().join("log")).unwrap(),
        PARENT_MARKER.repeat(3)
    );
}

#[cfg(windows)]
#[test]
#[ignore = "owns process-wide native stderr"]
fn absent_native_stderr_child() {
    use std::os::windows::io::{AsRawHandle as _, IntoRawHandle as _};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    let dir = std::env::var_os(DIRECTORY).expect("isolated child directory");
    let dir = Path::new(&dir);
    let log = File::create(dir.join("log")).unwrap();
    for absent in [std::ptr::null_mut(), INVALID_HANDLE_VALUE] {
        set_native_stderr(absent);
        check_absent(&log);
    }
    let stale = File::create(dir.join("closed")).unwrap().into_raw_handle();
    set_native_stderr(stale);
    // SAFETY: this handle is owned here. Snapshot immediately, before any open
    // could reuse its value; it must never become a BorrowedHandle.
    assert_ne!(unsafe { CloseHandle(stale) }, 0);
    let saved = super::super::snapshot_editor_stderr().unwrap();
    assert!(saved.is_none());
    set_native_stderr(log.as_raw_handle());
    discard_child_stderr(saved.as_ref());
    std::io::stderr().write_all(PARENT_MARKER).unwrap();
    set_native_stderr(std::ptr::null_mut());
}

#[cfg(windows)]
fn check_absent(log: &File) {
    use std::os::windows::io::AsRawHandle as _;
    let saved = super::super::snapshot_editor_stderr().unwrap();
    assert!(saved.is_none());
    set_native_stderr(log.as_raw_handle());
    discard_child_stderr(saved.as_ref());
    std::io::stderr().write_all(PARENT_MARKER).unwrap();
}

#[cfg(windows)]
fn discard_child_stderr(saved: Option<&std::os::windows::io::OwnedHandle>) {
    let mut command = child("tui::terminal::tests::stderr::editor_stderr_writer");
    command.stderr(editor_stderr(saved).unwrap());
    assert!(ral_core::process::status(&mut command).unwrap().success());
}
