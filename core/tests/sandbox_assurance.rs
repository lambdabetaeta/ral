#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(all(feature = "test-util", any(target_os = "linux", target_os = "macos")))]

//! Direct filesystem operations and a survivor released only after its host exits.

use ral_core::authority::policy::{Capabilities, FsPolicy};
use ral_core::path::FrozenPath;
use ral_core::protocol::Run;
use ral_core::shell::boot::{BakedPrelude, HostSurface, boot_shell};
use ral_core::source::SourceOrigin;
use ral_core::test_helper::confinement::{Output, admitted, denied, run};
use ral_core::test_helper::{layer, stack};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const CHILD: &str = "RAL_ASSURANCE_CHILD";
const HOST: &str = "RAL_ASSURANCE_HOST";
const MARKER: &str = "ok\n";

fn operation(args: &[String]) -> io::Result<()> {
    match args {
        [op, from, to] if op == "rename" => std::fs::rename(from, to),
        [op, path] if op == "rmdir" => std::fs::remove_dir(path),
        [op, path] if op == "read" => std::fs::read(path).map(drop),
        _ => Err(io::Error::other(format!(
            "unknown assurance probe: {args:?}"
        ))),
    }
}

fn outcome(result: io::Result<()>) -> Output {
    match result {
        Ok(()) => Output {
            status: 0,
            stdout: MARKER.into(),
            stderr: String::new(),
        },
        Err(error) => Output {
            status: 1,
            stdout: String::new(),
            stderr: error.to_string(),
        },
    }
}

fn survive(gate: &str, report: &str, path: &str) -> io::Result<()> {
    let mut gate = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(gate)?;
    let mut report = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(report)?;
    // SAFETY: gate is held here; only its nonblocking flag changes.
    if unsafe { libc::fcntl(gate.as_raw_fd(), libc::F_SETFL, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    report.write_all(b"ready\n")?;
    let mut release = [0];
    gate.read_exact(&mut release)?;
    assert_eq!(release, [1]);
    let out = outcome(std::fs::read(path).map(drop));
    serde_json::to_writer(&mut report, &(out.status, out.stdout, out.stderr))?;
    report.write_all(b"\n")
}

#[ctor::ctor(unsafe)]
fn fixture() {
    if let Some(code) = ral_core::invocation::serve_process_before_main(&[]) {
        std::process::exit(i32::from(code));
    }
    if let Ok(request) = std::env::var(CHILD) {
        let args: Vec<String> = serde_json::from_str(&request).unwrap();
        let out = outcome(match args.as_slice() {
            [op, gate, report, path] if op == "survive" => survive(gate, report, path),
            args => operation(args),
        });
        io::stdout().write_all(out.stdout.as_bytes()).unwrap();
        io::stderr().write_all(out.stderr.as_bytes()).unwrap();
        std::process::exit(out.status);
    }
    if let Ok(request) = std::env::var(HOST) {
        // SAFETY: this constructor precedes threads and descendants.
        unsafe { std::env::remove_var(HOST) };
        let (work, source): (String, String) = serde_json::from_str(&request).unwrap();
        let mut shell = boot_shell(
            ral_core::io::terminal::TerminalState::default(),
            BakedPrelude::runtime(),
            &HostSurface {
                statics: vec![ral_core::shell::builtins::DETACH_BUILTIN],
                ..HostSurface::default()
            },
            ral_core::test_helper::process_cwd(),
        )
        .expect("the survivor's host boots");
        shell.arm_detach(1);
        let out = Output::from_report(shell.run(Run {
            caps: stack([layer(writable(Path::new(&work), &[]))]),
            ..Run::captured(source, SourceOrigin::named("<survivor host>"))
        }));
        io::stderr().write_all(out.stderr.as_bytes()).unwrap();
        std::process::exit(out.status);
    }
}

fn writable(root: &Path, deny: &[&Path]) -> Capabilities {
    let root = FrozenPath::freeze(root).unwrap();
    Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![
                root.clone(),
                FrozenPath::freeze(&std::env::current_exe().unwrap()).unwrap(),
            ],
            write_prefixes: vec![root],
            deny_paths: deny
                .iter()
                .map(|p| FrozenPath::freeze(p).unwrap())
                .collect(),
        }),
        ..Capabilities::default()
    }
}

fn source(args: &[&str], detached: bool) -> String {
    let exe = std::env::current_exe().unwrap();
    let command = if detached {
        format!(
            "detach #'a confined assurance survivor'# '{}'",
            exe.display()
        )
    } else {
        exe.display().to_string()
    };
    within(args, &command)
}

fn within(args: &[&str], command: &str) -> String {
    let request = serde_json::to_string(args).unwrap();
    format!("within [env: [{CHILD}: '{request}']] {{ {command} }}")
}

/// Both enforcers judge the path `execve` is given, never argv.
#[cfg(seccomp)]
#[test]
fn an_admitted_loader_runs_an_unadmitted_file() {
    use ral_core::authority::policy::{ExecGrant, ExecKey};

    #[cfg(target_arch = "x86_64")]
    const LOADER: &str = "/lib64/ld-linux-x86-64.so.2";
    #[cfg(target_arch = "aarch64")]
    const LOADER: &str = "/lib/ld-linux-aarch64.so.1";

    let proof = ral_core::require_confinement!();
    ral_core::require_landlock!(1);
    let tree = tempfile::tempdir().unwrap();
    let target = tree.path().join("unadmitted");
    std::fs::copy(std::env::current_exe().unwrap(), &target).unwrap();
    let caps = Capabilities {
        exec: Some(ExecGrant::of(
            [
                ExecKey::Path(FrozenPath::from_surface(LOADER)),
                ExecKey::Path(FrozenPath::from_surface("/bin/sh")),
            ],
            [],
        )),
        ..writable(tree.path(), &[])
    };
    let grants = stack([layer(caps)]);
    let read = ["read", target.to_str().unwrap()];
    let target = target.display();
    let launch = |command: &str| run(grants.clone(), &within(&read, command));
    let through_loader = launch(&format!("{LOADER} {target}"));
    let by_ral = launch(&target.to_string());
    let by_kernel = launch(&format!("/bin/sh -c 'exec \"$0\"' '{target}'"));
    denied(&proof, &through_loader, &by_ral, MARKER);
    assert!(
        by_ral.stderr.contains("denied by active grant"),
        "{by_ral:?}"
    );
    denied(&proof, &through_loader, &by_kernel, MARKER);
    assert!(!by_kernel.stderr.is_empty(), "{by_kernel:?}");
}

fn pinning_exclusion(target: &Path) -> std::path::PathBuf {
    // Seatbelt pins ancestors; Linux pins mask mountpoints.
    #[cfg(target_os = "macos")]
    return target.join("absent");
    #[cfg(target_os = "linux")]
    target.to_path_buf()
}

#[test]
fn a_rename_onto_a_pinned_exclusion_target_is_denied() {
    let proof = ral_core::require_confinement!();
    let tree = tempfile::tempdir().unwrap();
    let target = tree.path().join("excluded");
    let allowed = tree.path().join("allowed");
    let replacement = tree.path().join("replacement");
    for path in [&target, &allowed, &replacement] {
        std::fs::create_dir(path).unwrap();
    }
    let exclusion = pinning_exclusion(&target);
    let grants = stack([layer(writable(tree.path(), &[&exclusion]))]);
    let rename = |to: &Path| {
        run(
            grants.clone(),
            &source(
                &[
                    "rename",
                    replacement.to_str().unwrap(),
                    to.to_str().unwrap(),
                ],
                false,
            ),
        )
    };
    let control = rename(&allowed);
    admitted(&proof, &control, MARKER);
    std::fs::create_dir(&replacement).unwrap();
    let id = std::fs::metadata(&target).unwrap();
    let refused = rename(&target);
    denied(&proof, &control, &refused, MARKER);
    assert!(!refused.stderr.is_empty(), "{refused:?}");
    assert!(
        replacement.is_dir(),
        "the refused rename consumed its source"
    );
    use std::os::unix::fs::MetadataExt;
    let after = std::fs::metadata(&target).unwrap();
    assert_eq!((id.dev(), id.ino()), (after.dev(), after.ino()));
}

#[test]
fn rmdir_of_a_pinned_exclusion_directory_is_denied() {
    let proof = ral_core::require_confinement!();
    let tree = tempfile::tempdir().unwrap();
    let target = tree.path().join("excluded");
    let allowed = tree.path().join("allowed");
    for path in [&target, &allowed] {
        std::fs::create_dir(path).unwrap();
    }
    let exclusion = pinning_exclusion(&target);
    let grants = stack([layer(writable(tree.path(), &[&exclusion]))]);
    let rmdir = |path: &Path| {
        run(
            grants.clone(),
            &source(&["rmdir", path.to_str().unwrap()], false),
        )
    };
    let control = rmdir(&allowed);
    let refused = rmdir(&target);
    denied(&proof, &control, &refused, MARKER);
    assert!(!allowed.exists());
    assert!(target.is_dir(), "the pinned directory was removed");
    assert!(!refused.stderr.is_empty(), "{refused:?}");
}

fn fifo(path: &Path) -> File {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is NUL-terminated; this fixture owns the fresh path.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

fn line(reader: &mut File) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut bytes = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !left.is_zero(),
            "the survivor did not report within 10 seconds"
        );
        let mut poll = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll names one live descriptor and a writable record.
        let ready =
            unsafe { libc::poll(&raw mut poll, 1, i32::try_from(left.as_millis()).unwrap()) };
        if ready < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        assert_eq!(ready, 1, "the survivor did not report within 10 seconds");
        assert_ne!(
            poll.revents & libc::POLLIN,
            0,
            "the survivor's report pipe failed"
        );
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            return String::from_utf8(bytes).unwrap();
        }
    }
}

fn after_host_exit(work: &Path, path: &Path) -> Output {
    let channels = tempfile::tempdir_in(work).unwrap();
    let gate_path = channels.path().join("gate");
    let report_path = channels.path().join("report");
    let mut gate = fifo(&gate_path);
    let mut report = fifo(&report_path);
    let source = source(
        &[
            "survive",
            gate_path.to_str().unwrap(),
            report_path.to_str().unwrap(),
            path.to_str().unwrap(),
        ],
        true,
    );
    let host = std::process::Command::new(std::env::current_exe().unwrap())
        .env(HOST, serde_json::to_string(&(work, source)).unwrap())
        .output()
        .expect("the survivor's host starts and exits");
    assert!(
        host.status.success(),
        "the survivor's host failed: {host:?}"
    );
    assert_eq!(line(&mut report), "ready\n");
    gate.write_all(&[1]).unwrap();
    let (status, stdout, stderr) = serde_json::from_str(&line(&mut report)).unwrap();
    Output {
        status,
        stdout,
        stderr,
    }
}

#[test]
fn a_detached_survivor_keeps_its_confinement_after_its_host_exits() {
    let proof = ral_core::require_confinement!();
    let tree = tempfile::tempdir().unwrap();
    let work = tree.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let inside = work.join("inside");
    let outside = tree.path().join("outside");
    for path in [&inside, &outside] {
        std::fs::write(path, "private bytes").unwrap();
    }
    let control = after_host_exit(&work, &inside);
    let refused = after_host_exit(&work, &outside);
    denied(&proof, &control, &refused, MARKER);
    assert!(!refused.stderr.is_empty(), "{refused:?}");
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "private bytes");
}
