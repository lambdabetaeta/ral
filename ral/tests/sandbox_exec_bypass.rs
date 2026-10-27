#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(any(target_os = "linux", target_os = "macos"))]

//! An exec grant is enforced by the kernel, not merely rendered.
//!
//! The in-process guard never sees a command an interpreter spawns, so only
//! the kernel closes the `sh -c cat` route: Seatbelt by a `process-exec` regex
//! after the broad allow, Landlock by leaving the vetoed name out of the walk.
//! These runs walk that chain end to end.

mod common;

use ral_core::test_helper::confinement::{admitted, denied};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Content that must never reach us: it only prints if the veto lifted.
const SENTINEL: &str = "ral-exec-pwned";

/// Run `body` under a grant reading `dir` and admitting `/bin/` and `dir`,
/// vetoing the bare name `cat` wherever it resolves.  No write reaches `dir`:
/// beneath one, Linux holds a veto only in the guard.
fn under_exec_grant(dir: &Path, body: &str) -> common::Output {
    run_script(&format!(
        "grant [fs: [read: [`absolute '{d}']], \
         exec: [allow: [`dir `absolute /bin, `dir `absolute '{d}'], deny: [`name cat]]] {{ {body} }}",
        d = dir.display()
    ))
}

/// Run `body` under a grant writing under `write` and admitting `/bin/` and
/// `admit`, with no veto anywhere.
fn writing_beside_admit(write: &Path, admit: &Path, body: &str) -> common::Output {
    run_script(&format!(
        "grant [fs: [read: [`absolute '{w}'], write: [`absolute '{w}']], \
         exec: [allow: [`dir `absolute /bin, `dir `absolute '{}']]] {{ {body} }}",
        admit.display(),
        w = write.display()
    ))
}

fn run_script(script: &str) -> common::Output {
    let out = Command::new(common::ral_bin())
        .args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral")
        .wait_with_output()
        .unwrap();
    common::Output {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}

/// A `#!/bin/sh` script echoing `line`, executable.  A script rather than a
/// copied binary: relocating a signed Apple executable gets it killed by
/// code signing, which would prove nothing about the sandbox.
fn write_script(path: &Path, line: &str) {
    std::fs::write(path, format!("#!/bin/sh\necho {line}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn a_bare_name_deny_stops_a_grandchild_the_guard_never_sees() {
    ral_core::require_landlock!(1);
    let proof = ral_core::require_confinement!();
    let td = tempfile::tempdir().unwrap();
    let secret = td.path().join("secret");
    std::fs::write(&secret, format!("{SENTINEL}\n")).unwrap();
    write_script(&td.path().join("mimic"), SENTINEL);
    write_script(&td.path().join("cat"), SENTINEL);

    let control = under_exec_grant(
        td.path(),
        &format!("/bin/sh -c '{}/mimic'", td.path().display()),
    );
    let hidden = under_exec_grant(
        td.path(),
        &format!("/bin/sh -c '/bin/cat {}'", secret.display()),
    );
    denied(&proof, &control, &hidden, SENTINEL);
    let elsewhere = under_exec_grant(
        td.path(),
        &format!("/bin/sh -c '{}/cat'", td.path().display()),
    );
    denied(&proof, &control, &elsewhere, SENTINEL);
}

/// The declared limit that replaced the write freeze: a write prefix that
/// covers an admitted dir lets the child author in it.
#[test]
fn a_write_prefix_covering_an_admitted_dir_can_author_in_it() {
    ral_core::require_landlock!(1);
    let proof = ral_core::require_confinement!();
    let td = tempfile::tempdir().unwrap();
    let tools = td.path().join("tools");
    std::fs::create_dir(&tools).unwrap();
    let out = writing_beside_admit(
        td.path(),
        &tools,
        &format!(
            "/bin/sh -c 'echo x > {}/planted && echo ok'",
            tools.display()
        ),
    );
    admitted(&proof, &out, "ok");
    assert!(tools.join("planted").exists());
}
