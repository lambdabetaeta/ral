//! Confinement enforced on pipeline stages: a grant's stdio routing, the grace
//! signal reaching a confined stage, and the scoped `PATH` it decodes under.

#![cfg(unix)]

use crate::cli::pipeline::{mkfifo, run, run_with_timeout};
use crate::common::fresh_tmp_path;
#[cfg(target_os = "linux")]
use crate::common::ral_command;
use ral_core::test_helper::confinement::admitted;
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "linux")]
use std::process::Stdio;
use std::time::Duration;

// ── Sandbox IPC subprocess stdio routing ────────────────────────────────
//
// These three tests verify that the grant IPC subprocess correctly handles
// all three stdio configurations from the parent:
//
//   1. stdout → Pipe  (grant body is a pipeline stage)
//   2. stdout → capture via let (grant body produces a value via from-X)
//   3. stdin  → pipe reader (grant body has upstream pipeline input)
//
// Without the sandboxing feature the IPC subprocess is not spawned and the
// tests exercise the in-process fallback path; with the feature the same
// scripts exercise the new configure_subprocess_stdio wiring.

#[test]
fn grant_fs_pipeline_stdout_flows() {
    // Grant is a pipeline stage: its stdout goes to a Pipe sink.
    // configure_subprocess_stdio must clone the pipe writer and hand it to
    // the IPC subprocess; cat on the right side must receive the output.
    let proof = ral_core::require_confinement!();
    let o = run(
        "grant [exec: [allow: [`absolute /bin/echo]], fs: [read: [`absolute /tmp]]] { /bin/echo sandboxed } | cat",
    );
    admitted(&proof, &o, "sandboxed");
    assert_eq!(o.stdout.trim(), "sandboxed");
}

#[test]
fn grant_fs_capture_returns_output() {
    // Grant body result is captured in a let binding and echoed.
    // Tests that output produced inside the grant (via from-lines) reaches the
    // parent — both via the in-process fallback and the IPC subprocess path.
    let proof = ral_core::require_confinement!();
    let o = run(
        "let xv = grant [exec: [allow: [`absolute /bin/echo]], fs: [read: [`absolute /tmp]]] { /bin/echo captured | from-lines }; echo $xv[0]",
    );
    admitted(&proof, &o, "captured");
    assert_eq!(o.stdout.trim(), "captured");
}

#[test]
fn grant_pipeline_abort_after_missing_later_stage_does_not_hang() {
    let _proof = ral_core::require_confinement!();
    for n in [5, 50_000] {
        let script = format!(
            "grant [net: `offline, fs: [read: [`cwd ., `absolute /tmp, `tempdir .], write: [`cwd ., `absolute /tmp, `tempdir .]]] {{ to-lines !{{range 0 {n}}} | limit 80 }}"
        );
        let o = run_with_timeout(&[], &script, Duration::from_secs(5))
            .expect("sandboxed byte-to-missing-external pipeline hung");
        assert_eq!(o.status, 127, "stdout: {}\nstderr: {}", o.stdout, o.stderr);
        assert!(
            o.stderr.contains("limit: command not found"),
            "missing-command diagnostic absent for n={n}: {}",
            o.stderr,
        );
        assert!(
            !o.stderr
                .contains("pipeline helper: parent closed before sending a stage job"),
            "abort path leaked helper diagnostic for n={n}: {}",
            o.stderr,
        );
    }
}

/// A confined stage rolled back as soon as it has spawned, here because the
/// next stage cannot launch, ends with its launch.
#[test]
fn a_confined_stage_rolled_back_at_once_does_not_outlive_its_launch() {
    let _proof = ral_core::require_confinement!();
    let o = run_with_timeout(
        &[],
        "grant [net: `offline] { /bin/sleep 30 | ral-no-such-command }",
        Duration::from_secs(10),
    )
    .expect("the confined sleep outlived its pipeline's failed launch");
    assert_eq!(o.status, 127, "stderr: {}", o.stderr);
}

#[test]
fn grant_fs_pipeline_stdin_forwarded() {
    // An upstream stage pipes data into the grant body.
    // configure_subprocess_stdio must move the pipe reader into the IPC
    // subprocess's stdin so that the body reads the upstream data.
    // Uses from-lines (ral builtin) so that pipe_stdin is consumed directly
    // rather than through an inner pipeline.
    let proof = ral_core::require_confinement!();
    let o = run(
        "let xv = /bin/echo piped | grant [fs: [read: [`absolute /tmp]]] { from-lines }; echo $xv[0]",
    );
    admitted(&proof, &o, "piped");
    assert_eq!(o.stdout.trim(), "piped");
}

/// The regression this closes: `launch_external_stage_direct` spawns a
/// confined pipeline stage under `Placement::Join`, and until the collector
/// also addresses that stage's own envelope group, the group-wide grace
/// signal never reaches it — bwrap's monitor joins the pipeline group, the
/// payload does not.  The fixture's `TERM` trap writing its own evidence
/// file is the only honest witness: a trap that ends in `exit 0` exits
/// cleanly, so the outcome carries no sign that a signal ever arrived.
#[test]
fn a_cancelled_confined_pipeline_stage_gets_its_grace_signal() {
    let _proof = ral_core::require_confinement!();
    let gate = fresh_tmp_path("ral_pipeline_confined_grace", "gate");
    let trapfile = fresh_tmp_path("ral_pipeline_confined_grace", "trap");
    let fixture = fresh_tmp_path("ral_pipeline_confined_grace", "sh");
    mkfifo(&gate);
    std::fs::write(
        &fixture,
        format!(
            "#!/bin/sh\ntrap 'echo GRACE > {}; exit 0' TERM\nsleep 30 &\n: > {}\nwait\n",
            trapfile.display(),
            gate.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Parked in `wait` before the gate opens: `sh` defers a trap while a
    // foreground child runs, and a TERM in that child's fork→exec window is lost.
    // No `exec:` clause: exec stays unrestricted, only `fs` is confined —
    // the fixture and `cat` both run inside the grant's envelope, which must
    // cover the temp dir itself: on macOS that is `$TMPDIR`, not `/tmp`.
    let tmp = std::env::temp_dir();
    let script = format!(
        "let job = watch \"tests\" {{ grant [fs: [read: [`absolute '{0}'], write: [`absolute '{0}']]] {{ {1} | cat }} ; return `done }}\ncat {2}\ncancel $job\n",
        tmp.display(),
        fixture.display(),
        gate.display(),
    );
    let out = run_with_timeout(&[], &script, Duration::from_secs(30));

    let trapped = std::fs::read_to_string(&trapfile);
    std::fs::remove_file(&fixture).ok();
    std::fs::remove_file(&gate).ok();
    std::fs::remove_file(&trapfile).ok();

    let Some(out) = out else {
        panic!("ral never exited: the confined stage's gate never opened");
    };
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert_eq!(
        trapped.unwrap_or_default().trim(),
        "GRACE",
        "the confined stage's TERM trap never ran: its own envelope group was never signalled"
    );
}

/// An envelope spawned inside a stage thread leads its payload's group, so
/// the stage's own waiter owns it: the one SIGINT reaches the payload, which
/// neither the monitor's pid nor the pipeline's group can reach.
#[cfg(target_os = "linux")]
#[test]
fn a_struck_frame_signals_an_enveloped_stage_member_once() {
    let _proof = ral_core::require_confinement!();
    let hits = fresh_tmp_path("ral_enveloped_member", "hits");
    let ready = fresh_tmp_path("ral_enveloped_member", "ready");
    let script_path = fresh_tmp_path("ral_enveloped_member", "ral");
    let tmp = std::env::temp_dir();
    std::fs::write(
        &script_path,
        format!(
            "grant [fs: [read: [`absolute '{0}'], write: [`absolute '{0}']]] {{ !{{ sh -c 'trap \"echo x >> {1}\" INT TERM; : > {2}; while :; do sleep 0.05; done' }} | cat }}\n",
            tmp.display(),
            hits.display(),
            ready.display(),
        ),
    )
    .unwrap();

    // A group of its own, so `kill(-pid)` strikes ral's frame and nothing else.
    let mut cmd = ral_command();
    cmd.arg(&script_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn ral");
    let within = |secs, done: &mut dyn FnMut() -> bool| {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while !done() {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    };
    let trapped = within(10, &mut || ready.exists());
    unsafe {
        libc::kill(-child.id().cast_signed(), libc::SIGINT);
    }
    let ended = within(10, &mut || child.try_wait().unwrap().is_some());
    if !ended {
        child.kill().ok();
    }
    child.wait().ok();

    let heard = std::fs::read_to_string(&hits).unwrap_or_default();
    for path in [&hits, &ready, &script_path] {
        std::fs::remove_file(path).ok();
    }
    assert!(trapped, "the confined trapper never set its trap");
    assert!(ended, "ral never exited after SIGINT");
    assert_eq!(
        heard.lines().count(),
        1,
        "the enveloped member must hear exactly one SIGINT"
    );
}

#[test]
fn grant_exec_bare_name_resolves_on_the_scoped_path_at_decode() {
    let _proof = ral_core::require_confinement!();
    let dir = tempfile::tempdir().unwrap();
    let fake_git = dir.path().join("git");
    std::fs::write(&fake_git, "#!/bin/sh\necho spoofed\n").unwrap();
    let mut perms = std::fs::metadata(&fake_git).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_git, perms).unwrap();

    // fs:read is needed so the bwrap sandbox binds the tempdir (/tmp is
    // tmpfs'd by default).  The fake git is a `#!/bin/sh` script, so its
    // interpreter is admitted too.  The grant is decoded under the scoped
    // PATH, so `name git` names the fake.
    let script = format!(
        "within [env: [PATH: '{0}']] {{ grant [exec: [allow: [`name git, `absolute /bin/sh]], fs: [read: [`absolute '{0}']]] {{ git }} }}",
        dir.path().to_string_lossy()
    );
    let o = run(&script);
    assert_eq!(o.status, 0, "stdout: {}\nstderr: {}", o.stdout, o.stderr);
    assert_eq!(o.stdout.trim(), "spoofed");
}

#[test]
fn grant_exec_bare_name_is_not_redirected_by_a_path_scoped_inside_the_body() {
    let _proof = ral_core::require_confinement!();
    let dir = tempfile::tempdir().unwrap();
    let fake_git = dir.path().join("git");
    std::fs::write(&fake_git, "#!/bin/sh\necho spoofed\n").unwrap();
    let mut perms = std::fs::metadata(&fake_git).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_git, perms).unwrap();

    // `name git` was resolved on the outer PATH, so the planted git is not it.
    let script = format!(
        "grant [exec: [allow: [`name git, `absolute /bin/sh]], fs: [read: [`absolute '{0}']]] {{ within [env: [PATH: '{0}']] {{ git }} }}",
        dir.path().to_string_lossy()
    );
    let o = run(&script);
    assert_eq!(o.status, 1, "stdout: {}\nstderr: {}", o.stdout, o.stderr);
    assert!(o.stderr.contains("denied by active grant"));
}

#[test]
fn grant_exec_explicit_path_allows_scoped_path_command() {
    let _proof = ral_core::require_confinement!();
    let dir = tempfile::tempdir().unwrap();
    let fake_git = dir.path().join("git");
    std::fs::write(&fake_git, "#!/bin/sh\necho spoofed\n").unwrap();
    let mut perms = std::fs::metadata(&fake_git).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_git, perms).unwrap();

    // See sibling test: fs:read for the tempdir is required so the bwrap
    // sandbox can actually exec the spoofed git from /tmp/...  Allowing the
    // script does not allow its `#!` interpreter, so the grant names /bin/sh.
    let script = format!(
        "within [env: [PATH: '{0}']] {{ grant [exec: [allow: [`absolute '{1}', `absolute /bin/sh]], fs: [read: [`absolute '{0}']]] {{ git }} }}",
        dir.path().to_string_lossy(),
        fake_git.to_string_lossy()
    );
    let o = run(&script);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout.trim(), "spoofed");
}

#[test]
fn grant_fs_write_through_symlinked_prefix_to_nonexistent_target() {
    // Regression: resolve_grant_path must canonicalize the grant prefix and
    // the target path consistently.  On macOS `/tmp -> /private/tmp`, so
    // `canonicalize('/tmp/')` returns `/private/tmp` — but `canonicalize` of
    // a non-existent file returns ENOENT, leaving the target unresolved.
    // `starts_with` then fails and the write is denied.  The fix walks up
    // to the longest existing ancestor and re-appends the tail, so the
    // target resolves through the symlink too.
    let _proof = ral_core::require_confinement!();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let real = std::env::temp_dir().join(format!("ral_grant_real_{pid}_{nanos}"));
    let link = std::env::temp_dir().join(format!("ral_grant_link_{pid}_{nanos}"));
    std::fs::create_dir_all(&real).unwrap();
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let target_via_link = link.join("new-file.log");
    let target_via_real = real.join("new-file.log");
    assert!(
        !target_via_real.exists(),
        "precondition: target must not exist"
    );

    let script = format!(
        "grant [fs: [write: [`absolute '{prefix}']]] {{ to-string 'hi' > '{path}' }}; printf done\\n",
        prefix = link.display(),
        path = target_via_link.display(),
    );
    let o = run(&script);

    // Cleanup before assertions so a failure doesn't leak the symlink.
    let wrote = target_via_real.exists();
    let _ = std::fs::remove_file(&target_via_real);
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&real);

    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert!(
        !o.stderr.contains("fs write denied by grant")
            && !o.stderr.contains("grant does not allow write access"),
        "write was denied: {}",
        o.stderr,
    );
    assert!(
        wrote,
        "redirect did not create the file under the symlink target"
    );
}
