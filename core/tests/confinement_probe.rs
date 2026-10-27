#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(all(feature = "test-util", unix))]

//! The constructor serves probes before libtest parses argv.

mod common;

use ral_core::test_helper::confinement::{Output, probe};

fn invoke(args: &[&str]) -> Output {
    std::process::Command::new(std::env::current_exe().unwrap())
        .env("RAL_TEST_PROBE", probe::request(args))
        .output()
        .expect("the probe role starts")
        .into()
}

#[test]
fn the_descriptor_probe_reports_open_descriptors_and_finishes() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let file = std::fs::File::open("/dev/null").unwrap();
    // SAFETY: F_DUPFD_CLOEXEC returns a fresh descriptor above stdio and runner pipes.
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64) };
    assert!(
        fd >= 0,
        "duplicate probe descriptor: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: fcntl returned a fresh descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let free = i32::MAX;
    // SAFETY: F_GETFD only inspects the candidate descriptor.
    assert_eq!(unsafe { libc::fcntl(free, libc::F_GETFD) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF)
    );
    let out = invoke(&[
        "fds",
        "0",
        "1",
        "2",
        &fd.as_raw_fd().to_string(),
        &free.to_string(),
    ]);
    assert_eq!(out.status, 0, "{out:?}");
    assert_eq!(out.stdout, "0 open\n1 open\n2 open\nok\n");
}

#[test]
fn an_unknown_probe_reports_its_arguments() {
    let out = invoke(&["no-such-probe"]);
    assert_eq!(out.status, 1, "{out:?}");
    assert!(out.stderr.contains("no-such-probe"), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}

#[test]
fn a_failed_connection_reports_the_os_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent");
    let out = invoke(&["unix", path.to_str().unwrap()]);
    assert_eq!(out.status, 1, "{out:?}");
    assert!(!out.stderr.is_empty(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}
