#![allow(clippy::disallowed_methods)]

//! Integration tests for `ral --capabilities a.ral[,b.ral...]`.
//!
//! The flag loads each `.ral` capability profile, left-to-right `meet`s
//! them, freezes once, and pushes the result as a permanent session
//! frame above `Capabilities::root()`.  Verified end-to-end by spawning
//! the built binary against tempfile profiles.

mod common;

use std::process::{Command, Stdio};

fn ral(args: &[&str]) -> common::Output {
    let child = Command::new(common::ral_bin())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral");
    let out = child.wait_with_output().unwrap();
    common::Output {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}

fn write_profile(suffix: &str, body: &str) -> std::path::PathBuf {
    let path = common::fresh_tmp_path(&format!("caps_{suffix}"), "ral");
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn single_file_deny_blocks_command() {
    let prof = write_profile("single_deny", "return [exec: [ls: 'deny']]\n");
    let out = ral(&["--capabilities", prof.to_str().unwrap(), "-c", "ls ."]);
    std::fs::remove_file(&prof).ok();
    assert_ne!(
        out.status, 0,
        "ls should be denied; stderr:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("denied by active grant") || out.stderr.contains("'ls'"),
        "expected grant-denied diagnostic; got:\n{}",
        out.stderr
    );
}

#[test]
fn missing_profile_file_errors_clean() {
    let out = ral(&[
        "--capabilities",
        "/no/such/profile/exists.ral",
        "-c",
        "echo never",
    ]);
    assert_ne!(out.status, 0);
    assert!(
        out.stderr.contains("file not found") || out.stderr.contains("does not exist"),
        "expected file-not-found error; got:\n{}",
        out.stderr
    );
}

/// Two profiles compose by left-to-right meet — both denies survive.
/// The user can layer narrower restrictions across multiple files
/// without one file having to know about the other's vetoes.
///
/// Both denied commands here resolve as external (`arm=External` in
/// ral's dispatch trace) — `echo` is a ral builtin so an `'echo': 'deny'`
/// would do nothing; we use `ls` and `cat` which both fall through to
/// the system PATH.
#[test]
fn comma_separated_profiles_meet_both_denies() {
    let a = write_profile("meet_a", "return [exec: [ls: 'deny']]\n");
    let b = write_profile("meet_b", "return [exec: [cat: 'deny']]\n");
    let arg = format!("{},{}", a.display(), b.display());

    let out_ls = ral(&["--capabilities", &arg, "-c", "ls ."]);
    assert_ne!(out_ls.status, 0, "ls denied by file a should fire");

    let out_cat = ral(&["--capabilities", &arg, "-c", "cat Cargo.toml"]);
    assert_ne!(out_cat.status, 0, "cat denied by file b should fire");

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

/// Composition only ever narrows: a key one profile allows and the other
/// never names does not survive the meet.  `join` would keep it, and so
/// would a `meet_literal_exec` that carried one-sided allows — either
/// silently widens the ceiling of every `--capabilities` session.  Asserted
/// in both orders, since a fold that narrows must also commute.
#[test]
fn a_one_sided_allow_does_not_survive_the_meet() {
    let a = write_profile("allow_a", "return [exec: [ls: 'allow', cat: 'allow']]\n");
    let b = write_profile("allow_b", "return [exec: [ls: 'allow']]\n");

    for arg in [
        format!("{},{}", a.display(), b.display()),
        format!("{},{}", b.display(), a.display()),
    ] {
        let out_ls = ral(&["--capabilities", &arg, "-c", "ls ."]);
        assert_eq!(
            out_ls.status, 0,
            "both profiles allow ls, so the intersection must admit it; stderr:\n{}",
            out_ls.stderr
        );

        let out_cat = ral(&["--capabilities", &arg, "-c", "cat Cargo.toml"]);
        assert_ne!(out_cat.status, 0, "cat is allowed by one profile only");
        assert!(
            out_cat.stderr.contains("denied by active grant"),
            "expected the grant-denied diagnostic; got:\n{}",
            out_cat.stderr
        );
    }

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

/// Without `--capabilities`, ambient root authority lets a normal
/// command through — pins the negative case so a regression making the
/// flag load even when absent gets caught.
#[test]
fn no_flag_leaves_session_unrestricted() {
    let out = ral(&["-c", "echo hello"]);
    assert_eq!(
        out.status, 0,
        "echo without --capabilities should succeed; stderr:\n{}",
        out.stderr
    );
    assert!(
        out.stdout.contains("hello"),
        "stdout missing 'hello':\n{}",
        out.stdout
    );
}

/// A misspelt `xdg:` name is a load-time error naming the typo and every
/// kind it could have meant — not a silently frozen literal prefix, and not
/// a bare non-absolute complaint that hides the real cause.  The companion
/// run proves the rejection is the *name*, not `xdg:` as such.
#[test]
fn unknown_xdg_token_in_profile_names_the_typo_and_the_alternatives() {
    let bad = write_profile("xdg_typo", "return [fs: [read: ['xdg:cofnig']]]\n");
    let out = ral(&["--capabilities", bad.to_str().unwrap(), "-c", "echo never"]);
    std::fs::remove_file(&bad).ok();
    assert_ne!(out.status, 0, "a typo'd xdg token must not load");
    assert!(
        out.stderr.contains("unknown xdg token 'xdg:cofnig'"),
        "expected the unknown-token diagnostic; got:\n{}",
        out.stderr
    );
    for kind in ["config", "data", "cache", "state", "bin"] {
        assert!(
            out.stderr.contains(kind),
            "the alternatives must list '{kind}'; got:\n{}",
            out.stderr
        );
    }

    let good = write_profile("xdg_known", "return [fs: [read: ['xdg:config']]]\n");
    let out = ral(&["--capabilities", good.to_str().unwrap(), "-c", "echo never"]);
    std::fs::remove_file(&good).ok();
    assert_eq!(
        out.status, 0,
        "a known xdg token must load; stderr:\n{}",
        out.stderr
    );
}

/// Content that must never survive a defeated `deny`.
#[cfg(target_os = "macos")]
const DENY_PIN_SENTINEL: &str = "ral-deny-pin-sentinel-do-not-leak";

#[cfg(target_os = "macos")]
fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = common::fresh_tmp_path(&format!("pin_{tag}"), "dir");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(target_os = "macos")]
fn seed_secret(dir: &std::path::Path) {
    let ssh = dir.join(".ssh");
    std::fs::create_dir_all(&ssh).unwrap();
    std::fs::write(ssh.join("id_rsa"), DENY_PIN_SENTINEL).unwrap();
}

// A Seatbelt deny names a path, so `FsRules::pinned_dirs` pins every
// ancestor of a deny that lies within a write prefix, and the macOS backend
// renders each as a `file-write-unlink` deny.  Only that backend needs it:
// bwrap's deny is an object-anchored mount, which survives the same rename
// unpinned.  Hence macOS-only.

/// Renaming a denied file's parent must not carry the secret to a name no
/// deny rule covers.
#[cfg(target_os = "macos")]
#[test]
fn deny_pin_survives_ancestor_rename() {
    let d = scratch_dir("ancestor");
    seed_secret(&d);
    let d_s = d.to_string_lossy().into_owned();

    let out = ral(&[
        "-c",
        &format!(
            "grant [fs: [read: ['{d_s}'], write: ['{d_s}'], deny: ['{d_s}/.ssh/id_rsa']]] \
             {{ sh -c 'mv {d_s}/.ssh {d_s}/x && cat {d_s}/x/id_rsa' }}"
        ),
    ]);
    // The rename must be *refused*, not merely followed by an unreadable
    // file: a body that never ran would satisfy the sentinel check vacuously.
    let refused = d.join(".ssh").join("id_rsa").exists() && !d.join("x").exists();
    std::fs::remove_dir_all(&d).ok();

    assert!(
        !out.stdout.contains(DENY_PIN_SENTINEL) && !out.stderr.contains(DENY_PIN_SENTINEL),
        "sentinel leaked past a renamed ancestor directory (exit {}); stdout:\n{}\nstderr:\n{}",
        out.status,
        out.stdout,
        out.stderr
    );
    assert!(refused, "the ancestor rename was not refused");
}

/// The pin freezes only the ancestor's own name-in-parent (`literal`, not
/// `subpath`) — mutating what's inside it must keep working, or the fix
/// overshoots.
#[cfg(target_os = "macos")]
#[test]
fn deny_pin_leaves_directory_entries_mutable() {
    let d = scratch_dir("entries");
    seed_secret(&d);
    let d_s = d.to_string_lossy().into_owned();
    let created = d.join(".ssh").join("created.txt");
    let created_s = created.to_string_lossy().into_owned();
    let under_grant = |body: String| {
        format!(
            "grant [fs: [read: ['{d_s}'], write: ['{d_s}'], deny: ['{d_s}/.ssh/id_rsa']]] {{ {body} }}"
        )
    };

    let out_create = ral(&["-c", &under_grant(format!("sh -c 'touch {created_s}'"))]);
    assert_eq!(
        out_create.status, 0,
        "creating a file inside the pinned directory must succeed; stderr:\n{}",
        out_create.stderr
    );
    assert!(
        created.exists(),
        "new file inside the pinned directory did not land"
    );

    let out_remove = ral(&["-c", &under_grant(format!("sh -c 'rm {created_s}'"))]);
    assert_eq!(
        out_remove.status, 0,
        "removing a file inside the pinned directory must succeed; stderr:\n{}",
        out_remove.stderr
    );
    assert!(
        !created.exists(),
        "file inside the pinned directory survived removal"
    );

    std::fs::remove_dir_all(&d).ok();
}

/// A distinct escape: relocating the *write-prefix root* itself rather
/// than an intermediate ancestor. Two prefixes share one deny; the pinned
/// set must cover each root or the secret resurfaces under the sibling
/// prefix once the rename lands.
#[cfg(target_os = "macos")]
#[test]
fn deny_pin_survives_write_prefix_root_rename() {
    let d = scratch_dir("root_d");
    let s = scratch_dir("root_s");
    seed_secret(&d);
    let d_s = d.to_string_lossy().into_owned();
    let s_s = s.to_string_lossy().into_owned();

    let out = ral(&[
        "-c",
        &format!(
            "grant [fs: [read: ['{d_s}', '{s_s}'], write: ['{d_s}', '{s_s}'], \
             deny: ['{d_s}/.ssh/id_rsa']]] \
             {{ sh -c 'mv {d_s} {s_s}/r && cat {s_s}/r/.ssh/id_rsa' }}"
        ),
    ]);
    let refused = d.exists() && !s.join("r").exists();
    std::fs::remove_dir_all(&d).ok();
    std::fs::remove_dir_all(&s).ok();

    assert!(
        !out.stdout.contains(DENY_PIN_SENTINEL) && !out.stderr.contains(DENY_PIN_SENTINEL),
        "sentinel leaked past a renamed write-prefix root (exit {}); stdout:\n{}\nstderr:\n{}",
        out.status,
        out.stdout,
        out.stderr
    );
    assert!(refused, "the write-prefix root rename was not refused");
}

/// The grant schema leaves `exec`/`fs` policy values to the runtime
/// decoder — they are key-shaped and heterogeneous, inexpressible as one
/// homogeneous element type.  So `--check` waves an ill-shaped policy
/// through, and the decoder must refuse it *before* the grant frame is
/// pushed and the body entered.
#[test]
fn undecodable_exec_policy_passes_the_checker_and_is_refused_at_run() {
    let src = "grant [exec: [git: 5]] { echo BODYRAN }";
    assert_eq!(
        ral(&["--check", "-c", src]).status,
        0,
        "the checker deliberately does not police policy values"
    );
    let out = ral(&["-c", src]);
    assert_ne!(out.status, 0, "an Int policy must not decode");
    assert!(
        out.stderr
            .contains("must be 'allow', 'deny', or a list of subcommands"),
        "expected the exec-policy diagnostic; got:\n{}",
        out.stderr
    );
    // stdout only: stderr echoes the source line inside the ariadne snippet.
    assert!(
        !out.stdout.contains("BODYRAN"),
        "the body must not run under an undecodable grant; stdout:\n{}",
        out.stdout
    );
}

/// The `fs` axis refuses a relative prefix outright — a grant whose
/// meaning would shift after a `cd` is no grant at all.
#[test]
fn relative_fs_prefix_is_refused_before_the_body_runs() {
    let out = ral(&["-c", "grant [fs: [read: ['proj']]] { echo BODYRAN }"]);
    assert_ne!(out.status, 0, "a relative fs prefix must not decode");
    assert!(
        out.stderr.contains("'proj'") && out.stderr.contains("cwd:"),
        "expected the relative-path diagnostic naming the cwd: form; got:\n{}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("BODYRAN"),
        "the body must not run under an undecodable grant; stdout:\n{}",
        out.stdout
    );
}
