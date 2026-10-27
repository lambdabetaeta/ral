#![allow(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::disallowed_macros,
    reason = "[test] libtest captures print macros"
)]

//! Integration tests for `ral --capabilities a.ral[,b.ral...]`.
//!
//! The flag loads each `.ral` capability profile, freezes it, and pushes it
//! as its own permanent session layer above `Capabilities::root()`, so the
//! stack is the composition.  Verified end-to-end by spawning the built
//! binary against tempfile profiles.

mod common;

#[cfg(unix)]
use ral_core::test_helper::confinement::Confining;
use ral_core::test_helper::confinement::{admitted, denied};
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
    ral_core::require_landlock!(1);
    let proof = ral_core::require_confinement!();
    let prof = write_profile("single_deny", "return [exec: [deny: [`name ls]]]\n");
    let control = ral(&[
        "--capabilities",
        prof.to_str().unwrap(),
        "-c",
        "echo caps-ran",
    ]);
    let out = ral(&[
        "--capabilities",
        prof.to_str().unwrap(),
        "-c",
        "ls .; echo caps-ran",
    ]);
    denied(&proof, &control, &out, "caps-ran");
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
/// ral's dispatch trace) — `echo` is a ral builtin so a ``deny: [`name echo]``
/// would do nothing; we use `ls` and `cat` which both fall through to
/// the system PATH.
#[test]
fn comma_separated_profiles_meet_both_denies() {
    ral_core::require_landlock!(1);
    let proof = ral_core::require_confinement!();
    let a = write_profile("meet_a", "return [exec: [deny: [`name ls]]]\n");
    let b = write_profile("meet_b", "return [exec: [deny: [`name cat]]]\n");
    let arg = format!("{},{}", a.display(), b.display());

    let control = ral(&["--capabilities", &arg, "-c", "echo caps-ran"]);
    let out_ls = ral(&["--capabilities", &arg, "-c", "ls .; echo caps-ran"]);
    denied(&proof, &control, &out_ls, "caps-ran");
    assert_ne!(out_ls.status, 0, "ls denied by file a should fire");

    let out_cat = ral(&[
        "--capabilities",
        &arg,
        "-c",
        "cat Cargo.toml; echo caps-ran",
    ]);
    denied(&proof, &control, &out_cat, "caps-ran");

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
    ral_core::require_landlock!(1);
    let proof = ral_core::require_confinement!();
    let a = write_profile("allow_a", "return [exec: [allow: [`name ls, `name cat]]]\n");
    let b = write_profile("allow_b", "return [exec: [allow: [`name ls]]]\n");

    for arg in [
        format!("{},{}", a.display(), b.display()),
        format!("{},{}", b.display(), a.display()),
    ] {
        let out_ls = ral(&["--capabilities", &arg, "-c", "ls .; echo caps-ran"]);
        admitted(&proof, &out_ls, "caps-ran");

        let out_cat = ral(&[
            "--capabilities",
            &arg,
            "-c",
            "cat Cargo.toml; echo caps-ran",
        ]);
        denied(&proof, &out_ls, &out_cat, "caps-ran");
        assert!(
            out_cat.stderr.contains("denied by active grant"),
            "expected the grant-denied diagnostic; got:\n{}",
            out_cat.stderr
        );
    }

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

/// A misspelt `xdg-*` tag is a load-time error naming the typo and every
/// kind it could have meant.  The companion run proves the rejection is the
/// tag, not `xdg-` as such.
#[test]
fn unknown_xdg_token_in_profile_names_the_typo_and_the_alternatives() {
    let bad = write_profile("xdg_typo", "return [fs: [read: [`xdg-cofnig .]]]\n");
    let out = ral(&["--capabilities", bad.to_str().unwrap(), "-c", "echo never"]);
    std::fs::remove_file(&bad).ok();
    assert_ne!(out.status, 0, "a typo'd xdg token must not load");
    assert!(
        out.stderr.contains("`xdg-cofnig"),
        "expected the unknown-tag diagnostic; got:\n{}",
        out.stderr
    );
    for kind in ["config", "data", "cache", "state", "bin"] {
        assert!(
            out.stderr.contains(&format!("xdg-{kind}")),
            "the alternatives must list 'xdg-{kind}'; got:\n{}",
            out.stderr
        );
    }

    let good = write_profile("xdg_known", "return [fs: [read: [`xdg-config .]]]\n");
    let out = ral(&["--capabilities", good.to_str().unwrap(), "-c", "echo never"]);
    std::fs::remove_file(&good).ok();
    assert_eq!(
        out.status, 0,
        "a known xdg tag must load; stderr:\n{}",
        out.stderr
    );
}

/// A ral confined by a ral grant reaches a granted file the way the kernel's
/// resolver does: by search on ancestors the profile admits as metadata only.
/// The walk once opened them for read and was refused at the first, so every
/// ral-owned read under a ral sandbox failed — a case no unconfined test sees.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn confined_ral_walks_to_a_granted_file() {
    let proof = ral_core::require_confinement!();
    let d = std::fs::canonicalize(scratch_dir("walk")).unwrap();
    std::fs::write(d.join("f.txt"), "walked\n").unwrap();
    let d_s = d.to_string_lossy().into_owned();
    let bin = common::ral_bin();
    let bin_s = bin.to_string_lossy().into_owned();
    let bin_dir_s = bin.parent().unwrap().to_string_lossy().into_owned();

    let out = ral(&[
        "-c",
        &format!(
            "within [dir: '{d_s}'] {{ grant [fs: [read: [`absolute '{d_s}', `absolute '{bin_dir_s}']]] {{ {bin_s} -c 'cat < {d_s}/f.txt' }} }}"
        ),
    ]);
    std::fs::remove_dir_all(&d).ok();

    admitted(&proof, &out, "walked");
    assert!(
        out.stdout.contains("walked"),
        "expected the file's content; stdout:\n{}",
        out.stdout
    );
}

#[cfg(target_os = "macos")]
#[test]
fn confined_ral_can_read_a_link_but_not_its_target_directory() {
    let d = std::fs::canonicalize(scratch_dir("walk_link")).unwrap();
    std::fs::create_dir_all(d.join("real/leaf")).unwrap();
    std::fs::write(d.join("real/leaf/f.txt"), "walked\n").unwrap();
    std::os::unix::fs::symlink("real", d.join("link")).unwrap();
    let leaf = d.join("link/leaf").to_string_lossy().into_owned();
    let bin = common::ral_bin();
    let bin_s = bin.to_string_lossy();
    let bin_dir = bin.parent().unwrap().to_string_lossy();
    let out = ral(&[
        "-c",
        &format!(
            "within [dir: '{leaf}'] {{ grant [fs: [read: [`absolute '{leaf}', `absolute '{bin_dir}']]] {{ {bin_s} -c 'cat < {leaf}/f.txt' }} }}"
        ),
    ]);
    if refused_by_a_confined_runner(&out) {
        std::fs::remove_dir_all(&d).unwrap();
        return;
    }
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert!(out.stdout.contains("walked"), "{}", out.stdout);
    let listed = ral(&[
        "-c",
        &format!(
            "within [dir: '{leaf}'] {{ grant [fs: [read: [`absolute '{leaf}', `absolute '{bin_dir}']]] {{ /bin/ls {}/real }} }}",
            d.display()
        ),
    ]);
    std::fs::remove_dir_all(&d).unwrap();
    assert_ne!(listed.status, 0, "the target directory became listable");
    assert!(
        listed.stderr.contains("Operation not permitted"),
        "{}",
        listed.stderr
    );
}

/// Content that must never survive a defeated `deny`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const DENY_PIN_SENTINEL: &str = "ral-deny-pin-sentinel-do-not-leak";

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = common::fresh_tmp_path(&format!("pin_{tag}"), "dir");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

/// The running launch holds the deny through an ancestor rename; macOS also
/// refuses the rename, while Linux's next launch reads the moved file.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn deny_pin_survives_ancestor_rename() {
    let proof = ral_core::require_confinement!();
    let d = scratch_dir("ancestor");
    seed_secret(&d);
    let d_s = d.to_string_lossy().into_owned();

    let launch = |body: &str| {
        ral(&[
            "-c",
            &format!(
                "grant [fs: [read: [`absolute '{d_s}'], write: [`absolute '{d_s}'], deny: [`absolute '{d_s}/.ssh/id_rsa']]] {{ sh -c '{body}' }}"
            ),
        ])
    };
    std::fs::write(d.join("control"), DENY_PIN_SENTINEL).unwrap();
    let control = launch(&format!("mv {d_s}/control {d_s}/moved && cat {d_s}/moved"));
    let out = launch(&format!("mv {d_s}/.ssh {d_s}/x && cat {d_s}/x/id_rsa"));
    denied(&proof, &control, &out, DENY_PIN_SENTINEL);
    #[cfg(target_os = "macos")]
    let refused = d.join(".ssh").join("id_rsa").exists() && !d.join("x").exists();
    #[cfg(target_os = "linux")]
    {
        assert!(
            !d.join(".ssh").exists() && d.join("x/id_rsa").exists(),
            "the ancestor rename must land"
        );
        admitted(
            &proof,
            &launch(&format!("cat {d_s}/x/id_rsa")),
            DENY_PIN_SENTINEL,
        );
    }
    std::fs::remove_dir_all(&d).ok();

    assert!(
        !out.stdout.contains(DENY_PIN_SENTINEL) && !out.stderr.contains(DENY_PIN_SENTINEL),
        "sentinel leaked past a renamed ancestor directory (exit {}); stdout:\n{}\nstderr:\n{}",
        out.status,
        out.stdout,
        out.stderr
    );
    #[cfg(target_os = "macos")]
    assert!(refused, "the ancestor rename was not refused");
}

/// The pin freezes only the ancestor's own name-in-parent (`literal`, not
/// `subpath`) — mutating what's inside it must keep working, or the fix
/// overshoots.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn deny_pin_leaves_directory_entries_mutable() {
    let proof = ral_core::require_confinement!();
    let d = scratch_dir("entries");
    seed_secret(&d);
    let d_s = d.to_string_lossy().into_owned();
    let created = d.join(".ssh").join("created.txt");
    let created_s = created.to_string_lossy().into_owned();
    let under_grant = |body: String| {
        format!(
            "grant [fs: [read: [`absolute '{d_s}'], write: [`absolute '{d_s}'], deny: [`absolute '{d_s}/.ssh/id_rsa']]] {{ {body} }}"
        )
    };

    let out_create = ral(&[
        "-c",
        &under_grant(format!("sh -c 'touch {created_s} && echo ok'")),
    ]);

    admitted(&proof, &out_create, "ok");
    assert!(
        created.exists(),
        "new file inside the pinned directory did not land"
    );

    let out_remove = ral(&[
        "-c",
        &under_grant(format!("sh -c 'rm {created_s} && echo ok'")),
    ]);
    admitted(&proof, &out_remove, "ok");
    assert!(
        !created.exists(),
        "file inside the pinned directory survived removal"
    );

    std::fs::remove_dir_all(&d).ok();
}

/// Moving the write-prefix root is refused: Seatbelt pins it, and in the
/// envelope it is a mountpoint, so `mv` falls back to a copy that meets the mask.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn deny_pin_survives_write_prefix_root_rename() {
    let proof = ral_core::require_confinement!();
    let d = scratch_dir("root_d");
    let s = scratch_dir("root_s");
    seed_secret(&d);
    let d_s = d.to_string_lossy().into_owned();
    let s_s = s.to_string_lossy().into_owned();

    let launch = |body: &str| {
        ral(&[
            "-c",
            &format!(
                "grant [fs: [read: [`absolute '{d_s}', `absolute '{s_s}'], write: [`absolute '{d_s}', `absolute '{s_s}'], deny: [`absolute '{d_s}/.ssh/id_rsa']]] {{ sh -c '{body}' }}"
            ),
        ])
    };
    std::fs::create_dir(d.join("control")).unwrap();
    std::fs::write(d.join("control/f"), DENY_PIN_SENTINEL).unwrap();
    let control = launch(&format!(
        "mv {d_s}/control {s_s}/control && cat {s_s}/control/f"
    ));
    let out = launch(&format!("mv {d_s} {s_s}/r && cat {s_s}/r/.ssh/id_rsa"));
    denied(&proof, &control, &out, DENY_PIN_SENTINEL);
    let refused = d.join(".ssh/id_rsa").exists() && !s.join("r/.ssh/id_rsa").exists();
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
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn seatbelt_holds_an_absent_deny_inside_a_write_prefix() {
    let proof = ral_core::require_confinement!();
    let d = common::Scratch::new("absent_deny");
    let write = |name: &str| {
        common::run(
            "absent_deny",
            &format!(
                "grant [fs: [read: [`absolute '{d}'], write: [`absolute '{d}'], deny: [`absolute '{d}/.env']]] {{ /bin/sh -c 'echo ok > {d}/{name} && echo ok' }}",
                d = d.0.display(),
            ),
        )
    };
    let control = write("other");
    let out = write(".env");
    #[cfg(target_os = "macos")]
    {
        denied(&proof, &control, &out, "ok");
        assert!(!d.join(".env").exists(), "the denied name was created");
    }
    #[cfg(target_os = "linux")]
    {
        admitted(&proof, &control, "ok");
        admitted(&proof, &out, "ok");
        assert!(
            d.join(".env").exists(),
            "Linux admits an absent name until the next launch"
        );
    }
}

/// A table reaching `exec:` through a parameter is left to the runtime
/// decoder, which must refuse an ill-shaped one *before* the grant frame is
/// pushed and the body entered.
#[test]
fn undecodable_exec_table_passes_the_checker_and_is_refused_at_run() {
    let src = "let g = { |t| grant [exec: $t] { echo BODYRAN } }; g [git: 5]";
    assert_eq!(
        ral(&["--check", "-c", src]).status,
        0,
        "the checker does not constrain a parameter"
    );
    let out = ral(&["-c", src]);
    assert_ne!(out.status, 0, "an argument policy must not decode");
    assert!(
        out.stderr.contains("grant exec: unknown key 'git'"),
        "expected the exec-table diagnostic; got:\n{}",
        out.stderr
    );
    // stdout only: stderr echoes the source line inside the ariadne snippet.
    assert!(
        !out.stdout.contains("BODYRAN"),
        "the body must not run under an undecodable grant; stdout:\n{}",
        out.stdout
    );
}

/// A confined command can fork; macOS also sets a hard process budget.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn confined_forks_run_under_a_process_budget() {
    let proof = ral_core::require_confinement!();
    let d = scratch_dir("brake");
    let d_s = d.to_string_lossy().into_owned();
    let out = common::run_with_timeout(
        "brake",
        &[],
        &format!(
            "grant [fs: [read: [`absolute '{d_s}']]] {{ /bin/bash -c 'echo $(ulimit -Su) $(ulimit -Hu); \
             i=0; while [ $i -lt 50 ]; do /usr/bin/true & i=$((i+1)); done; wait; echo forked' }}"
        ),
        std::time::Duration::from_secs(30),
    )
    .expect("the confined fork run hung");
    std::fs::remove_dir_all(&d).ok();

    admitted(&proof, &out, "forked");
    #[cfg(target_os = "macos")]
    {
        let mut lines = out.stdout.lines();
        let limits: Vec<u64> = lines
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .map(process_limit)
            .collect();
        assert_eq!(lines.next(), Some("forked"), "stdout:\n{}", out.stdout);

        let free = free_limits()[0];
        assert!(
            matches!(*limits, [soft, hard] if soft == hard && soft <= free),
            "the confined limit must be one value no higher than the user's {free}: {limits:?}"
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn process_limit(n: &str) -> u64 {
    match n {
        "unlimited" => u64::MAX,
        _ => n.parse().expect("a process limit is numeric or unlimited"),
    }
}

/// The `(soft, hard)` process limits a `bash` child reports, run as `wrap`
/// writes the probe into a script.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn child_limits(proof: &Confining, tag: &str, wrap: impl Fn(&str) -> String) -> Vec<u64> {
    const PROBE: &str = "/bin/bash -c 'echo $(ulimit -Su) $(ulimit -Hu); echo limits-ok'";
    let out = common::run_with_timeout(tag, &[], &wrap(PROBE), std::time::Duration::from_secs(30))
        .expect("the limit probe hung");

    admitted(proof, &out, "limits-ok");
    out.stdout
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .map(process_limit)
        .collect()
}

/// The `(soft, hard)` process limits an unconfined `bash` has.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn free_limits() -> [u64; 2] {
    let ulimit = |flag: &str| -> u64 {
        let out = Command::new("/bin/bash")
            .args(["-c", &format!("ulimit {flag}")])
            .output()
            .unwrap();
        assert!(out.status.success(), "ulimit {flag} failed: {out:?}");
        process_limit(String::from_utf8(out.stdout).unwrap().trim())
    };
    [ulimit("-Su"), ulimit("-Hu")]
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
const FULL_VERBS: &str = "editor: [`read, `write, `tui], shell: [`chdir, `detach]";

/// A neutral policy launches ambient: the child keeps the user's own limits.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_neutral_policy_leaves_the_process_limits_alone() {
    let proof = ral_core::require_confinement!();
    let limits = child_limits(&proof, "brake_neutral", |probe| {
        format!("grant [{FULL_VERBS}] {{ {probe} }}")
    });
    assert_eq!(limits, free_limits(), "a neutral grant applies no budget");
}

/// Neutral layers preserve launch restrictions, including macOS's process budget.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn neutral_layers_around_a_restriction_keep_the_process_budget() {
    let proof = ral_core::require_confinement!();
    let d = scratch_dir("brake_wrapped");
    let d_s = d.to_string_lossy().into_owned();
    let wrapped = child_limits(&proof, "brake_wrapped", |probe| {
        format!(
            "grant [{FULL_VERBS}] {{ grant [fs: [read: [`absolute '{d_s}']]] {{ \
             grant [{FULL_VERBS}] {{ {probe} }} }} }}"
        )
    });
    let verb = child_limits(&proof, "brake_verb", |probe| {
        format!("grant [shell: [`chdir]] {{ {probe} }}")
    });
    std::fs::remove_dir_all(&d).ok();
    #[cfg(target_os = "macos")]
    {
        let free = free_limits()[0];
        for limits in [wrapped, verb] {
            assert!(
                matches!(*limits, [soft, hard] if soft == hard && soft <= free),
                "the budget must survive: {limits:?} against the user's {free}"
            );
        }
    }
    #[cfg(target_os = "linux")]
    assert_eq!(
        [wrapped.len(), verb.len()],
        [2, 2],
        "both confined probes must run"
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

/// `body` under an exec grant allowing `<bin>` and `/bin` and denying
/// `<bin>/<deny>` (a directory if `deny` ends in `/`),
/// reading the scratch directory and writing `<bin>`, so the body may author
/// there.
#[cfg(unix)]
fn under_exec_deny(bin: &std::path::Path, deny: &str, body: &str) -> common::Output {
    let (root, b) = (bin.parent().unwrap().display(), bin.display());
    let deny = match deny.strip_suffix('/') {
        Some(dir) => format!("`dir `absolute '{b}/{dir}'"),
        None => format!("`absolute '{b}/{deny}'"),
    };
    ral(&[
        "-c",
        &format!(
            "grant [fs: [read: [`absolute '{root}'], write: [`absolute '{b}']], \
             exec: [allow: [`dir `absolute '{b}', `dir `absolute /bin, `name mkdir, `name cp], deny: [{deny}]]] {{ {body} }}"
        ),
    ])
}

#[cfg(unix)]
const RESPELLED: &str =
    "under another spelling (case or Unicode form); a deny holds under every spelling";

/// An absent exclusion keeps its frozen spelling on every volume.
#[cfg(unix)]
fn assert_names_the_absent_deny(out: &common::Output, kind: &str, denied: &std::path::Path) {
    let said = format!("is the denied `` {kind}{} `` {RESPELLED}", denied.display());
    assert!(
        out.stderr.contains(&said),
        "the refusal must name the deny and say why: {said}; stderr:\n{}",
        out.stderr
    );
}

/// E1: an exec deny frozen while its name is absent holds the program a
/// later create makes under another spelling, and the refusal says why.
/// Without `Evil` on disk the deny keeps its spelling; the body makes `evil`.
#[cfg(unix)]
#[test]
fn an_absent_exec_deny_holds_a_case_variant() {
    ral_core::require_landlock!(1);
    let proof = ral_core::require_confinement!();

    let scratch = ExecScratch::new("dir");
    let (bin, stash) = (scratch.bin(), scratch.stash());
    let (b, s) = (bin.display(), stash.display());
    let control = under_exec_deny(&bin, "Evil/", &format!("{b}/good/x"));
    let out = under_exec_deny(
        &bin,
        "Evil/",
        &format!("/bin/mkdir {b}/evil; /bin/cp {s} {b}/evil/x; {b}/evil/x"),
    );

    denied(&proof, &control, &out, "ran");
    assert_ne!(
        out.status, 0,
        "`evil/x` must be refused; stderr:\n{}",
        out.stderr
    );
    assert_names_the_absent_deny(&out, "`dir `absolute ", &bin.join("Evil"));

    let scratch = ExecScratch::new("stored");
    let bin = scratch.bin();
    std::fs::create_dir(bin.join("Evil")).unwrap();
    std::fs::copy(bin.join("good/x"), bin.join("Evil/x")).unwrap();
    let b = bin.display();
    let control = under_exec_deny(&bin, "Evil/", &format!("{b}/good/x"));
    let out = under_exec_deny(&bin, "Evil/", &format!("{b}/Evil/x"));
    denied(&proof, &control, &out, "ran");
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
    ral_core::require_landlock!(1);
    let proof = ral_core::require_confinement!();

    let scratch = ExecScratch::new("path");
    let (bin, stash) = (scratch.bin(), scratch.stash());
    let (b, s) = (bin.display(), stash.display());
    let control = under_exec_deny(&bin, "Tool", &format!("{b}/good/x"));
    let out = under_exec_deny(&bin, "Tool", &format!("/bin/cp {s} {b}/tool; {b}/tool"));

    denied(&proof, &control, &out, "ran");
    assert_ne!(
        out.status, 0,
        "`tool` must be refused; stderr:\n{}",
        out.stderr
    );
    assert_names_the_absent_deny(&out, "`absolute ", &bin.join("Tool"));
}

/// E2: Seatbelt folds an exec deny as the guard does, so a child's exec of
/// the case variant is refused and the stored sibling admitted.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn seatbelt_refuses_a_case_variant_of_an_absent_exec_deny() {
    ral_core::require_landlock!(1);
    let proof = ral_core::require_confinement!();
    let scratch = ExecScratch::new("seatbelt");
    let (bin, stash) = (scratch.bin(), scratch.stash());
    let b = bin.display();
    // The grant freezes `Evil` before it exists; only then does `evil/x` appear.
    let control = under_exec_deny(&bin, "Evil/", &format!("/bin/sh -c {b}/good/x"));
    let out = under_exec_deny(
        &bin,
        "Evil/",
        &format!(
            "/bin/mkdir {b}/evil; /bin/cp {} {b}/evil/x; /bin/sh -c {b}/evil/x",
            stash.display()
        ),
    );

    denied(&proof, &control, &out, "ran");
    assert!(
        bin.join("evil/x").is_file(),
        "the denied executable must have been created: {out:?}"
    );
}

/// E3: a stack whose layers both allow `B` and one also `b` admits `B/x`: a
/// key one layer never mentioned is not written down as a deny.
#[cfg(unix)]
#[test]
fn a_stacked_one_sided_allow_denies_no_other_spelling() {
    ral_core::require_landlock!(1);
    let proof = ral_core::require_confinement!();
    let scratch = ExecScratch::new("stack");
    let bin = scratch.bin();
    std::fs::create_dir(bin.join("B")).unwrap();
    std::fs::copy(bin.join("good/x"), bin.join("B/x")).unwrap();
    let b = bin.display();
    let out = ral(&[
        "-c",
        &format!(
            "grant [exec: [allow: [`dir `absolute '{b}/B', `dir `absolute /bin]]] {{ \
             grant [exec: [allow: [`dir `absolute '{b}/B', `dir `absolute '{b}/b', `dir `absolute /bin]]] {{ {b}/B/x }} }}"
        ),
    ]);

    admitted(&proof, &out, "ran");
    assert!(out.stdout.contains("ran"), "stdout:\n{}", out.stdout);
}

/// An allow beneath a deny never reaches the OS backend, so a child is
/// refused what the guard refuses: a deny outranks an allow at any depth.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn an_allow_beneath_a_deny_never_reaches_the_backend() {
    let proof = ral_core::require_confinement!();

    let d = common::Scratch::new("caps_beneath");
    let (beneath, beside) = (d.join("Secrets/sub/f"), d.join("other/f"));
    for (file, text) in [(&beneath, "beneath"), (&beside, "beneath")] {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }
    let allows = common::places(&[&d.0, &d.join("Secrets/sub")]);
    let cat = |file: &std::path::Path| {
        common::run(
            "caps_beneath",
            &format!(
                "grant [fs: [read: [{allows}], write: [{allows}], deny: [`absolute '{}']]] \
                 {{ sh -c 'cat {}' }}",
                d.join("Secrets").display(),
                file.display()
            ),
        )
    };

    let control = cat(&beside);
    admitted(&proof, &control, "beneath");
    let out = cat(&beneath);
    denied(&proof, &control, &out, "beneath");
}
