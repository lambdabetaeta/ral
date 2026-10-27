#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(all(feature = "test-util", any(target_os = "linux", target_os = "macos")))]

//! Descriptor fixtures run before libtest can start threads or open files.

use ral_core::test_helper::confinement::{Output, admitted, probe, run};
use ral_core::test_helper::stack;
use std::io::Write;
use std::os::fd::IntoRawFd;
use std::process::Command;

const FIXTURE: &str = "RAL_WARRANT_FD_FIXTURE";
const HELD: &[i32] = &[3, 97, 200, 300];

#[ctor::ctor(unsafe)]
fn fixture() {
    let source = std::env::var(FIXTURE).ok();
    if source.is_some() {
        // SAFETY: this constructor precedes all threads.
        unsafe { std::env::remove_var(FIXTURE) };
        // Before boot, which would otherwise lose a pin it opened at one of them.
        hold_descriptors();
    }
    if let Some(code) = ral_core::invocation::serve_process_before_main(&[]) {
        std::process::exit(i32::from(code));
    }
    let Some(source) = source else {
        return;
    };
    let out = run(stack([]), &source);
    std::io::stdout().write_all(out.stdout.as_bytes()).unwrap();
    std::io::stderr().write_all(out.stderr.as_bytes()).unwrap();
    std::process::exit(out.status);
}

fn hold_descriptors() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: limit is writable.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) },
        0,
        "getrlimit(RLIMIT_NOFILE): {}",
        std::io::Error::last_os_error()
    );
    if limit.rlim_cur < 301 {
        limit.rlim_cur = 301;
        // SAFETY: only this subprocess's soft limit changes.
        assert_eq!(
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) },
            0,
            "the descriptor fixture needs fd 300: {}",
            std::io::Error::last_os_error()
        );
    }
    // Never closed: under bare stdio it is itself the first held descriptor.
    let null = std::fs::File::open("/dev/null").unwrap().into_raw_fd();
    for &fd in HELD {
        // SAFETY: only this isolated fixture uses these descriptor numbers.
        assert_eq!(
            unsafe { libc::dup2(null, fd) },
            fd,
            "dup2 to fd {fd}: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: fd is open; clear CLOEXEC even when dup2 was a no-op.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, 0);
    }
    limit.rlim_cur = 256;
    // SAFETY: the hard limit stays unchanged, and all fixture descriptors are open.
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) },
        0
    );
    for &fd in HELD {
        // SAFETY: F_GETFD only inspects a descriptor, including above the soft limit.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, 0);
    }
}

fn launch(source: &str) -> Output {
    Command::new(std::env::current_exe().unwrap())
        .env(FIXTURE, source)
        .output()
        .expect("the descriptor fixture starts")
        .into()
}

/// An fs grant reading only this binary's directory.
fn confining(body: &str) -> String {
    let exe = std::env::current_exe().unwrap();
    format!(
        "grant [fs: [read: [`absolute '{}']]] {{ {body} }}",
        exe.parent().unwrap().display()
    )
}

fn fd_probe(confined: bool) -> Output {
    let exe = std::env::current_exe().unwrap();
    let request = probe::request(&["fds", "0", "1", "2", "3", "97", "200", "300"]);
    let source = format!(
        "within [env: [RAL_TEST_PROBE: '{request}']] {{ {} }}",
        exe.display()
    );
    launch(&if confined { confining(&source) } else { source })
}

#[test]
fn a_confined_file_inherits_only_stdio_even_above_a_lowered_soft_limit() {
    let proof = ral_core::require_confinement!();
    let ambient = fd_probe(false);
    let confined = fd_probe(true);
    admitted(&proof, &ambient, "ok\n");
    admitted(&proof, &confined, "ok\n");
    for fd in HELD {
        let marker = format!("{fd} open\n");
        admitted(&proof, &ambient, &marker);
    }
    assert_eq!(
        confined.stdout, "0 open\n1 open\n2 open\nok\n",
        "{confined:?}"
    );
}

#[cfg(all(target_os = "linux", feature = "coreutils"))]
#[cfg_attr(
    ctor_without_argv,
    ignore = "bundled tools need std's argv, absent in musl constructors"
)]
#[test]
fn a_confined_bundled_tool_holds_only_stdio_and_its_own_directory() {
    let proof = ral_core::require_confinement!();
    let out = launch(&confining("ls -1 /proc/self/fd"));
    admitted(&proof, &out, "0\n1\n2\n");
    assert_eq!(out.stdout, "0\n1\n2\n3\n", "{out:?}");
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use ral_core::test_helper::confinement::denied;
    use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};
    use std::io::Seek;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;

    fn parcel(args: &[&[u8]], sealed: bool) -> std::os::fd::OwnedFd {
        let exe = std::env::current_exe().unwrap();
        let argc = args.len().to_string();
        let fields = [
            b"ral-warrant/3".as_slice(),
            b"unconfined",
            b"file",
            exe.as_os_str().as_bytes(),
            b"probe",
            b"",
            argc.as_bytes(),
        ];
        let mut file = std::fs::File::from(
            memfd_create(
                "warrant-fixture",
                MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
            )
            .unwrap(),
        );
        for field in fields.into_iter().chain(args.iter().copied()) {
            file.write_all(field).unwrap();
            file.write_all(&[0]).unwrap();
        }
        file.rewind().unwrap();
        if sealed {
            fcntl_add_seals(
                &file,
                SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE | SealFlags::SEAL,
            )
            .unwrap();
        }
        rustix::io::fcntl_dupfd_cloexec(&file, 98).unwrap()
    }

    fn invoke(parcel: &std::os::fd::OwnedFd, extra: &[&str]) -> Output {
        let fd = parcel.as_raw_fd();
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--warrant", &fd.to_string()])
            .args(extra)
            .env("RAL_TEST_PROBE", probe::request(&["fds", "0", "1", "2"]));
        // SAFETY: the parcel stays open through spawn; fcntl is async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.output().expect("the warrant trampoline starts").into()
    }

    #[test]
    fn malformed_live_handoffs_refuse_before_the_probe_runs() {
        let proof = ral_core::require_confinement!();
        let sealed = parcel(&[], true);
        let control = invoke(&sealed, &[]);
        admitted(&proof, &control, "ok\n");
        let unsealed = invoke(&parcel(&[], false), &[]);
        denied(&proof, &control, &unsealed, "ok\n");
        assert_eq!(unsealed.status, 126, "{unsealed:?}");
        assert!(unsealed.stderr.contains("not sealed"), "{unsealed:?}");
        assert!(unsealed.stdout.is_empty(), "{unsealed:?}");

        let extra = invoke(&sealed, &["extra"]);
        denied(&proof, &control, &extra, "ok\n");
        assert_eq!(extra.status, 126, "{extra:?}");
        assert!(extra.stderr.contains("takes one argument"), "{extra:?}");
        assert!(extra.stdout.is_empty(), "{extra:?}");

        let nested = invoke(&parcel(&[b"--warrant", b"98"], true), &[]);
        denied(&proof, &control, &nested, "ok\n");
        assert_eq!(nested.status, 126, "{nested:?}");
        assert!(nested.stderr.contains("no warrant at fd 98"), "{nested:?}");
        assert!(nested.stdout.is_empty(), "{nested:?}");
    }
}
