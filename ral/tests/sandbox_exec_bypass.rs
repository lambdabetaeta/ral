#![cfg(target_os = "macos")]
#![allow(clippy::disallowed_methods)]

//! An exec grant is enforced by the kernel, not merely rendered.
//!
//! A bare-name `deny` projects into `(deny process-exec (regex #"/cat$"))`,
//! emitted *after* the broad allow because Seatbelt is last-match-wins.
//! Rule order is the whole enforcement, and the in-process guard never sees a
//! command an interpreter spawns — so only a real Seatbelt envelope can
//! close the `sh -c cat` route.  These runs walk that chain end to end.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Content that must never reach us: it only prints if the veto lifted.
const SENTINEL: &str = "ral-exec-pwned";

/// Run `body` under a grant admitting `/bin/` and `dir`, vetoing the bare
/// name `cat` wherever it resolves.
fn under_exec_grant(dir: &Path, body: &str) -> common::Output {
    run_script(&format!(
        "grant [exec: ['/bin/': 'allow', '{}/': 'allow', cat: 'deny']] {{ {body} }}",
        dir.display()
    ))
}

/// Run `body` under a grant writing under `write` and admitting `/bin/` and
/// `admit`, with no veto anywhere.
fn writing_beside_admit(write: &Path, admit: &Path, body: &str) -> common::Output {
    run_script(&format!(
        "grant [fs: [read: ['{w}'], write: ['{w}']], \
         exec: ['/bin/': 'allow', '{}/': 'allow']] {{ {body} }}",
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
    let td = tempfile::tempdir().unwrap();
    let secret = td.path().join("secret");
    std::fs::write(&secret, format!("{SENTINEL}\n")).unwrap();
    write_script(&td.path().join("mimic"), "OK");
    write_script(&td.path().join("cat"), SENTINEL);

    // Control: the envelope launched and a nested exec under an admitted
    // dir still runs, so a later denial is the veto and not a dead body.
    let ok = under_exec_grant(td.path(), "/bin/sh -c '/bin/echo OK'");
    assert_eq!(
        ok.status, 0,
        "an exec-only grant must still admit /bin/echo through sh; stderr:\n{}",
        ok.stderr
    );
    assert!(
        ok.stdout.contains("OK"),
        "stdout missing 'OK':\n{}",
        ok.stdout
    );

    // The shell spawns `cat`, so nothing in ral ever weighs it.
    let hidden = under_exec_grant(
        td.path(),
        &format!("/bin/sh -c '/bin/cat {}'", secret.display()),
    );
    assert_ne!(hidden.status, 0, "the denied grandchild must not succeed");
    assert_leaked_nothing(&hidden);

    // The veto follows the basename wherever it resolves: the sibling
    // script proves the directory itself is genuinely admitted.
    let elsewhere = under_exec_grant(
        td.path(),
        &format!(
            "/bin/sh -c '{}/mimic; {}/cat'",
            td.path().display(),
            td.path().display()
        ),
    );
    assert!(
        elsewhere.stdout.contains("OK"),
        "the admitted sibling did not run, so the deny below proves nothing:\n{}",
        elsewhere.stderr
    );
    assert_leaked_nothing(&elsewhere);
}

/// A write prefix that only covers an admitted dir would let the child author
/// the binaries the dir runs, so the kernel freezes the dir; a grant that names
/// the tree keeps it writable.  The veto-free grant shows the freeze needs no
/// veto to fire.
#[test]
fn a_write_prefix_that_only_covers_an_admitted_dir_cannot_author_in_it() {
    let td = tempfile::tempdir().unwrap();
    let tools = td.path().join("tools");
    let scratch = td.path().join("scratch");
    std::fs::create_dir(&tools).unwrap();
    std::fs::create_dir(&scratch).unwrap();
    let plant = |dir: &Path| format!("/bin/sh -c 'echo x > {}/planted'", dir.display());

    // Control: the covering prefix is live, so a later denial is the freeze
    // and not a dead sandbox.
    let beside = writing_beside_admit(td.path(), &tools, &plant(&scratch));
    assert_eq!(
        beside.status, 0,
        "a write beside the admitted dir must land; stderr:\n{}",
        beside.stderr
    );
    assert!(scratch.join("planted").exists());

    // Control: naming the tree keeps it writable.
    let named = writing_beside_admit(&tools, &tools, &plant(&tools));
    assert_eq!(
        named.status, 0,
        "a grant naming the admitted dir must keep it writable; stderr:\n{}",
        named.stderr
    );
    assert!(tools.join("planted").exists());
    std::fs::remove_file(tools.join("planted")).unwrap();

    let covered = writing_beside_admit(td.path(), &tools, &plant(&tools));
    assert_ne!(
        covered.status, 0,
        "a write into a dir the prefix only covers must be refused"
    );
    assert!(
        !tools.join("planted").exists(),
        "the write landed in the admitted dir"
    );
}

fn assert_leaked_nothing(out: &common::Output) {
    assert!(
        !out.stdout.contains(SENTINEL),
        "the denied command ran; stdout:\n{}",
        out.stdout
    );
    assert!(
        !out.stderr.contains(SENTINEL),
        "the denied command ran; stderr:\n{}",
        out.stderr
    );
}
