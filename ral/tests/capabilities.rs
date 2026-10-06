#![allow(clippy::disallowed_methods)]

//! Integration tests for `ral --capabilities a.ral[,b.ral...]`.
//!
//! The flag loads each `.ral` capability profile, freezes it, and pushes it
//! as its own permanent session layer above `Capabilities::root()`, so the
//! stack is the composition.  Verified end-to-end by spawning the built
//! binary against tempfile profiles.

mod common;

use common::refused_by_a_confined_runner;
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
        if !refused_by_a_confined_runner(&out_ls) {
            assert_eq!(
                out_ls.status, 0,
                "both profiles allow ls, so the intersection must admit it; stderr:\n{}",
                out_ls.stderr
            );
        }

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

/// A ral confined by a ral grant reaches a granted file the way the kernel's
/// resolver does: by search on ancestors the profile admits as metadata only.
/// The walk once opened them for read and was refused at the first, so every
/// ral-owned read under a ral sandbox failed — a case no unconfined test sees.
#[cfg(target_os = "macos")]
#[test]
fn confined_ral_walks_to_a_granted_file() {
    let d = scratch_dir("walk");
    std::fs::write(d.join("f.txt"), "walked\n").unwrap();
    let d_s = d.to_string_lossy().into_owned();
    let bin = common::ral_bin();
    let bin_s = bin.to_string_lossy().into_owned();
    let bin_dir_s = bin.parent().unwrap().to_string_lossy().into_owned();

    let out = ral(&[
        "-c",
        &format!(
            "grant [fs: [read: ['{d_s}', '{bin_dir_s}']]] {{ {bin_s} -c 'cat < {d_s}/f.txt' }}"
        ),
    ]);
    std::fs::remove_dir_all(&d).ok();
    if refused_by_a_confined_runner(&out) {
        return;
    }

    assert_eq!(
        out.status, 0,
        "the confined ral could not read a granted file; stderr:\n{}",
        out.stderr
    );
    assert!(
        out.stdout.contains("walked"),
        "expected the file's content; stdout:\n{}",
        out.stdout
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
    if refused_by_a_confined_runner(&out_create) {
        std::fs::remove_dir_all(&d).ok();
        return;
    }
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

/// Seatbelt renders a deny whether or not its name exists, so a child cannot
/// create it inside a write prefix.  Linux and Windows hold such a name in
/// process alone until it exists (SPEC §12.3); only this backend pins it.
#[cfg(target_os = "macos")]
#[test]
fn seatbelt_holds_an_absent_deny_inside_a_write_prefix() {
    let d = scratch_dir("absent_deny");
    let d_s = d.to_string_lossy().into_owned();

    let out = ral(&[
        "-c",
        &format!(
            "grant [fs: [read: ['{d_s}'], write: ['{d_s}'], deny: ['{d_s}/.env']]] \
             {{ /bin/sh -c 'echo x > {d_s}/.env && echo made-env; \
             echo y > {d_s}/other && echo made-other' }}"
        ),
    ]);
    let env_exists = d.join(".env").exists();
    let other = std::fs::read_to_string(d.join("other"));
    std::fs::remove_dir_all(&d).ok();
    if refused_by_a_confined_runner(&out) {
        return;
    }

    assert_eq!(
        other.as_deref().ok(),
        Some("y\n"),
        "the control write did not land; stderr:\n{}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("made-env"),
        "the denied name was created; stdout:\n{}",
        out.stdout
    );
    assert!(!env_exists, "the denied name exists on the host");
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

/// A confined command forks as freely as a script needs, under a process
/// limit set in the child alone and as a hard limit, so the command cannot
/// raise it and the user's other processes never share it.
#[cfg(target_os = "macos")]
#[test]
fn confined_forks_run_under_a_process_budget() {
    let d = scratch_dir("brake");
    let d_s = d.to_string_lossy().into_owned();
    let out = common::run_with_timeout(
        "brake",
        &[],
        &format!(
            "grant [fs: [read: ['{d_s}']]] {{ sh -c 'echo $(ulimit -Su) $(ulimit -Hu); \
             i=0; while [ $i -lt 50 ]; do /usr/bin/true & i=$((i+1)); done; wait; echo forked' }}"
        ),
        std::time::Duration::from_secs(30),
    )
    .expect("the confined fork run hung");
    std::fs::remove_dir_all(&d).ok();
    if refused_by_a_confined_runner(&out) {
        return;
    }

    assert_eq!(
        out.status, 0,
        "fifty short children must fit the budget; stderr:\n{}",
        out.stderr
    );
    let mut lines = out.stdout.lines();
    let limits: Vec<u64> = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .map(|n| n.parse().expect("a numeric process limit"))
        .collect();
    assert_eq!(lines.next(), Some("forked"), "stdout:\n{}", out.stdout);

    let free = Command::new("sh")
        .args(["-c", "ulimit -Su"])
        .output()
        .unwrap();
    let free: u64 = String::from_utf8(free.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        matches!(*limits, [soft, hard] if soft == hard && soft <= free),
        "the confined limit must be one value no higher than the user's {free}: {limits:?}"
    );
}

/// A canonical scratch directory holding `bin/good/x` and an executable
/// stash script `x` outside `bin`, which a grant body copies in after the
/// grant froze.
#[cfg(unix)]
struct ExecScratch(common::Scratch);

#[cfg(unix)]
impl ExecScratch {
    fn new(tag: &str) -> Self {
        let s = Self(common::Scratch::new(&format!("exec_spelling_{tag}")));
        common::script(&s.stash());
        common::script(&s.bin().join("good/x"));
        s
    }

    fn bin(&self) -> std::path::PathBuf {
        self.0.join("bin")
    }

    fn stash(&self) -> std::path::PathBuf {
        self.0.join("x")
    }
}

/// `body` under `grant [exec: [<bin>/: allow, /bin/: allow, <deny>: deny]]`,
/// reading the scratch directory and writing `<bin>`.  The write is named at
/// the admit, so the body may author there: under an unrestricted `fs` the
/// deny would freeze the admitted set (`261006_a-veto-freezes-what-a-write-covers`).
#[cfg(unix)]
fn under_exec_deny(bin: &std::path::Path, deny: &str, body: &str) -> common::Output {
    let (root, b) = (bin.parent().unwrap().display(), bin.display());
    ral(&[
        "-c",
        &format!(
            "grant [fs: [read: ['{root}'], write: ['{b}']], \
             exec: ['{b}/': 'allow', '/bin/': 'allow', '{b}/{deny}': 'deny']] {{ {body} }}"
        ),
    ])
}

#[cfg(unix)]
const RESPELLED: &str =
    "under another spelling (case or Unicode form); a deny holds under every spelling";

/// E1: an exec deny frozen while its name is absent holds the program a
/// later create makes under another spelling, and the refusal says why.
/// Without `Evil` on disk the deny keeps its spelling; the body makes `evil`.
#[cfg(unix)]
#[test]
fn an_absent_exec_deny_holds_a_case_variant() {
    #[cfg(target_os = "linux")]
    if !common::bwrap_functional() {
        eprintln!("skip: bwrap cannot confine a child here");
        return;
    }
    let scratch = ExecScratch::new("dir");
    let (bin, stash) = (scratch.bin(), scratch.stash());
    let (b, s) = (bin.display(), stash.display());
    let out = under_exec_deny(
        &bin,
        "Evil/",
        &format!("{b}/good/x; /bin/mkdir {b}/evil; /bin/cp {s} {b}/evil/x; {b}/evil/x"),
    );
    if refused_by_a_confined_runner(&out) {
        return;
    }
    assert!(
        out.stdout.contains("ran"),
        "`good/x` must run; stderr:\n{}",
        out.stderr
    );
    assert_ne!(
        out.status, 0,
        "`evil/x` must be refused; stderr:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains(&format!("the denied {b}/Evil")) && out.stderr.contains(RESPELLED),
        "the refusal must name the deny and say why; stderr:\n{}",
        out.stderr
    );

    let scratch = ExecScratch::new("stored");
    let bin = scratch.bin();
    std::fs::create_dir(bin.join("Evil")).unwrap();
    std::fs::copy(bin.join("good/x"), bin.join("Evil/x")).unwrap();
    let b = bin.display();
    let out = under_exec_deny(&bin, "Evil/", &format!("{b}/Evil/x"));
    assert_ne!(
        out.status, 0,
        "`Evil/x` must be refused; stderr:\n{}",
        out.stderr
    );
    assert!(
        !out.stderr.contains(RESPELLED),
        "a deny holding the name as stored must not claim a respelling; stderr:\n{}",
        out.stderr
    );
}

/// E1, for a path key: a deny on the absent `Tool` holds a created `tool`.
#[cfg(unix)]
#[test]
fn an_absent_exec_path_deny_holds_a_case_variant() {
    #[cfg(target_os = "linux")]
    if !common::bwrap_functional() {
        eprintln!("skip: bwrap cannot confine a child here");
        return;
    }
    let scratch = ExecScratch::new("path");
    let (bin, stash) = (scratch.bin(), scratch.stash());
    let (b, s) = (bin.display(), stash.display());
    let out = under_exec_deny(
        &bin,
        "Tool",
        &format!("{b}/good/x; /bin/cp {s} {b}/tool; {b}/tool"),
    );
    if refused_by_a_confined_runner(&out) {
        return;
    }
    assert!(
        out.stdout.contains("ran"),
        "`good/x` must run; stderr:\n{}",
        out.stderr
    );
    assert_ne!(
        out.status, 0,
        "`tool` must be refused; stderr:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains(&format!("the denied {b}/Tool")) && out.stderr.contains(RESPELLED),
        "the refusal must name the deny and say why; stderr:\n{}",
        out.stderr
    );
}

/// E2: Seatbelt folds an exec deny as the guard does, so a child's exec of
/// the case variant is refused and the stored sibling admitted.
#[cfg(target_os = "macos")]
#[test]
fn seatbelt_refuses_a_case_variant_of_an_absent_exec_deny() {
    let scratch = ExecScratch::new("seatbelt");
    let (bin, stash) = (scratch.bin(), scratch.stash());
    let b = bin.display();
    // The grant freezes `Evil` before it exists; only then does `evil/x` appear.
    let out = under_exec_deny(
        &bin,
        "Evil/",
        &format!(
            "/bin/mkdir {b}/evil; /bin/cp {} {b}/evil/x; /bin/sh -c '{b}/good/x; {b}/evil/x || echo refused'",
            stash.display()
        ),
    );
    if refused_by_a_confined_runner(&out) {
        return;
    }
    assert_eq!(out.status, 0, "stderr:\n{}", out.stderr);
    assert_eq!(
        out.stdout.lines().collect::<Vec<_>>(),
        ["ran", "refused"],
        "a child must run `good/x` and be refused `evil/x`; stderr:\n{}",
        out.stderr
    );
}

/// E3: a stack whose layers both allow `B` and one also `b` admits `B/x`: a
/// key one layer never mentioned is not written down as a deny.
#[cfg(unix)]
#[test]
fn a_stacked_one_sided_allow_denies_no_other_spelling() {
    let scratch = ExecScratch::new("stack");
    let bin = scratch.bin();
    std::fs::create_dir(bin.join("B")).unwrap();
    std::fs::copy(bin.join("good/x"), bin.join("B/x")).unwrap();
    let b = bin.display();
    let out = ral(&[
        "-c",
        &format!(
            "grant [exec: ['{b}/B/': 'allow', '/bin/': 'allow']] {{ \
             grant [exec: ['{b}/B/': 'allow', '{b}/b/': 'allow', '/bin/': 'allow']] {{ {b}/B/x }} }}"
        ),
    ]);
    if refused_by_a_confined_runner(&out) {
        return;
    }
    assert_eq!(out.status, 0, "`B/x` must run; stderr:\n{}", out.stderr);
    assert!(out.stdout.contains("ran"), "stdout:\n{}", out.stdout);
}

/// An allow beneath a deny never reaches the OS backend, so a child is
/// refused what the guard refuses: a deny outranks an allow at any depth.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn an_allow_beneath_a_deny_never_reaches_the_backend() {
    #[cfg(target_os = "linux")]
    if !common::bwrap_functional() {
        eprintln!("skip: bwrap cannot confine a child here");
        return;
    }
    let d = common::Scratch::new("caps_beneath");
    let (beneath, beside) = (d.join("Secrets/sub/f"), d.join("other/f"));
    for (file, text) in [(&beneath, "beneath"), (&beside, "beside")] {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }
    let allows = common::quoted(&[&d.0, &d.join("Secrets/sub")]);
    let cat = |file: &std::path::Path| {
        common::run(
            "caps_beneath",
            &format!(
                "grant [fs: [read: [{allows}], write: [{allows}], deny: ['{}']]] \
                 {{ sh -c 'cat {}' }}",
                d.join("Secrets").display(),
                file.display()
            ),
        )
    };

    let out = cat(&beside);
    if refused_by_a_confined_runner(&out) {
        return;
    }
    assert_eq!(
        out.status, 0,
        "a child's read beside the deny must be admitted; stderr:\n{}",
        out.stderr
    );
    assert_eq!(out.stdout, "beside");

    let out = cat(&beneath);
    assert_ne!(
        out.status, 0,
        "a child's read beneath the deny must be refused; stderr:\n{}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("beneath"),
        "the denied file reached the child; stdout:\n{}",
        out.stdout
    );
}
