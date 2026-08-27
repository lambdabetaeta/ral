#![allow(clippy::disallowed_methods)]

//! [`exarch::policy::for_invocation`] end to end, in its own test binary: the
//! two lattice phases in their fixed order, the deny that has to land on the
//! real fs gate rather than merely on a list, the two diagnostics a user meets
//! before a session starts, and the grant summary the model reads.
//!
//! Every scenario needs a directory of `.ral` profiles on disk — a composed
//! grant is a fact about files, and the assertions here are worth only as much
//! as the loader they went through.

use exarch::bootstrap::{EXARCH, Scratch};
use exarch::policy::for_invocation;
use exarch::prompt::host_section;
use ral_core::path::NormalizedPrefix;
use ral_core::types::{Break, ExecPolicy, Settled, Shell};
use std::path::PathBuf;

exarch::pre_main_ctor!();

/// The six bake-ins, in the order the unknown-base message names them: the
/// list and `resolve_base`'s match arms are pinned to each other below.
const BASES: [&str; 6] = [
    "dangerous",
    "reasonable",
    "edit-only",
    "read-only",
    "minimal",
    "confined",
];

/// Write a capability profile into `dir`, returning its path.
fn profile(dir: &Scratch, name: &str, source: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, source).expect("profile file");
    path
}

/// The one summary line opening with `label`.
fn bullet<'a>(text: &'a str, label: &str) -> &'a str {
    text.lines()
        .find(|l| l.starts_with(label))
        .unwrap_or_else(|| panic!("no `{label}` line in:\n{text}"))
}

/// The gate's refusal message, or a panic naming what arrived instead.
fn refusal(r: Settled<()>) -> String {
    match r {
        Ok(()) => panic!("the gate admitted a call it had to refuse"),
        Err(Break::Error(err)) => err.message,
        Err(other) => panic!("expected a grant denial, got: {other:?}"),
    }
}

/// The join runs before the meet, so a widened grant the attenuation file
/// never names is erased by it.  Inverting the phases turns `--extend-base`
/// into an escape hatch from `--restrict`; `ls`, named on both sides, is the
/// control that says the meet attenuated rather than emptied.
#[test]
fn an_extend_base_grant_cannot_survive_a_restrict_that_omits_it() {
    let dir = Scratch::for_test(EXARCH, "extend-then-restrict").expect("scratch dir");
    let extend = profile(
        &dir,
        "ext.ral",
        "return [exec: [rustc: 'allow', ls: 'allow']]\n",
    );
    let restrict = profile(&dir, "restrict.ral", "return [exec: [ls: 'allow']]\n");
    let cwd = dir.path().to_string_lossy().into_owned();

    let (caps, _) =
        for_invocation(&cwd, "minimal", Some(&extend), &[restrict]).expect("profiles compose");
    let exec = caps.exec.as_ref().expect("the meet leaves an exec opinion");
    assert_ne!(
        exec.literals.get("rustc"),
        Some(&ExecPolicy::Allow),
        "the restrict names only ls, so the widened rustc grant must not be in the lattice"
    );

    let mut shell = Shell::default();
    shell
        .with_capabilities(caps.clone(), |sh| {
            sh.check_exec_args("rustc", &["rustc", "/usr/bin/rustc"], &[])
        })
        .expect_err("--extend-base must not outlive a --restrict that omits it");
    let mut shell = Shell::default();
    shell
        .with_capabilities(caps, |sh| {
            sh.check_exec_args("ls", &["ls", "/usr/bin/ls"], &[])
        })
        .expect("what both sides name survives the meet");
}

/// `--restrict` is documented as order-free.  Meet is commutative, and the
/// deny list is sorted and deduped, so the two argv orders must compose to one
/// value — not merely to two equivalent ones.
#[test]
fn two_restricts_compose_to_the_same_grant_in_either_order() {
    let dir = Scratch::for_test(EXARCH, "restrict-commutes").expect("scratch dir");
    let a = profile(
        &dir,
        "a.ral",
        "return [fs: [read: ['/'], write: ['/']], exec: [git: 'allow', ls: 'allow']]\n",
    );
    let b = profile(
        &dir,
        "b.ral",
        "return [exec: [ls: 'allow', cat: 'allow'], net: false]\n",
    );
    let cwd = dir.path().to_string_lossy().into_owned();

    let ab = for_invocation(&cwd, "dangerous", None, &[a.clone(), b.clone()])
        .expect("profiles compose")
        .0;
    let ba = for_invocation(&cwd, "dangerous", None, &[b, a])
        .expect("profiles compose")
        .0;
    assert_eq!(ab, ba, "flag order must not reach the composed grant");

    let fs = ab.fs.expect("a restrict file installs an fs carve-out");
    assert!(
        fs.deny_paths.is_sorted(),
        "the deny list is canonical, not argv-ordered: {:?}",
        fs.deny_paths
    );
    for name in ["a.ral", "b.ral"] {
        let path = dir.path().join(name);
        assert_eq!(
            fs.deny_paths
                .iter()
                .filter(|p| **p == *path.to_string_lossy())
                .count(),
            1,
            "{name} should be denied exactly once in {:?}",
            fs.deny_paths
        );
    }
}

/// Being in `deny_paths` is not the claim; being unwritable is.  The frozen
/// entry is lexical, and the gate expands it — so the file is refused by both
/// its own spelling and its canonical one, while its sibling stays writable.
///
/// The ceiling is `cwd:`, the directory holding both files: a literal `/`
/// would be a foreign-rooted dead grant on Windows, leaving nothing writable
/// and the sibling refused for the wrong reason.
#[test]
fn a_restrict_file_is_refused_by_the_fs_gate_under_either_spelling() {
    let dir = Scratch::for_test(EXARCH, "restrict-unwritable").expect("scratch dir");
    let restrict = profile(
        &dir,
        "restrict.ral",
        "return [fs: [read: ['cwd:'], write: ['cwd:']]]\n",
    );
    let sibling = profile(&dir, "scratch.txt", "ordinary work\n");
    let canonical = std::fs::canonicalize(&restrict).expect("the restrict file exists");
    let cwd = dir.path().to_string_lossy().into_owned();

    let (caps, _) = for_invocation(&cwd, "dangerous", None, std::slice::from_ref(&restrict))
        .expect("profile composes");

    let mut shell = Shell::default();
    shell.with_capabilities(caps, |sh| {
        for spelling in [&restrict, &canonical] {
            let path = sh.resolve(&spelling.to_string_lossy());
            let message = refusal(sh.check_fs_write(&path));
            assert!(
                message.contains("denied by grant"),
                "{} should be refused by the grant, got: {message}",
                spelling.display()
            );
        }
        let path = sh.resolve(&sibling.to_string_lossy());
        sh.check_fs_write(&path)
            .expect("the deny is targeted: a sibling file stays writable");
    });
}

/// The summary is the model's only view of its own authority, so it has to
/// agree with the grant the session holds: `minimal` must not read as ambient,
/// and the veto it exists to carve out — Homebrew on Unix, the interactive
/// shell on Windows, where a Unix-rooted deny freezes to a dead grant — must
/// reach the page.
#[test]
fn the_grant_summary_agrees_with_an_attenuated_grant() {
    let dir = Scratch::for_test(EXARCH, "grant-prompt").expect("scratch dir");
    let cwd = dir.path().to_string_lossy().into_owned();
    let (caps, _) = for_invocation(&cwd, "minimal", None, &[]).expect("minimal composes");
    let text = host_section(&caps, &dir);

    assert!(
        !text.contains("Ambient authority"),
        "an attenuated session must not be told it holds everything:\n{text}"
    );
    assert_eq!(caps.net, Some(true), "minimal declares net: true");
    let rendered = match caps.net {
        None => "inherit",
        Some(true) => "allow",
        Some(false) => "deny",
    };
    assert_eq!(bullet(&text, "- net:"), format!("- net: {rendered}"));
    let veto = if cfg!(windows) {
        "cmd"
    } else {
        "/opt/homebrew/"
    };
    assert!(
        bullet(&text, "- exec deny:").contains(veto),
        "minimal's {veto} veto must reach the model:\n{text}"
    );
    let frozen_cwd = NormalizedPrefix::from_surface(&cwd).into_string();
    assert!(bullet(&text, "- fs read:").contains(&frozen_cwd), "{text}");
    assert!(bullet(&text, "- fs write:").contains(&frozen_cwd), "{text}");
    assert!(bullet(&text, "- scratch:").contains(&*dir.path().to_string_lossy()));
}

/// `dangerous` attenuates nothing, so the denial legend would describe an
/// event that cannot happen: the summary collapses to one line, and still
/// names the scratch path the agent needs.
#[test]
fn the_grant_summary_collapses_for_an_unattenuated_grant() {
    let dir = Scratch::for_test(EXARCH, "grant-prompt-dangerous").expect("scratch dir");
    let cwd = dir.path().to_string_lossy().into_owned();
    let (caps, _) = for_invocation(&cwd, "dangerous", None, &[]).expect("dangerous composes");
    let text = host_section(&caps, &dir);

    assert!(text.contains("Ambient authority"), "{text}");
    assert!(bullet(&text, "- scratch:").contains(&*dir.path().to_string_lossy()));
}

/// A misspelt base names every live one back, and each name it offers really
/// resolves — the message and the match arms pinned to each other in both
/// directions, so a seventh bake-in cannot arrive unadvertised.
#[test]
fn an_unknown_base_names_every_base_that_exists() {
    let dir = Scratch::for_test(EXARCH, "unknown-base").expect("scratch dir");
    let cwd = dir.path().to_string_lossy().into_owned();

    assert_eq!(
        for_invocation(&cwd, "resonable", None, &[]).unwrap_err(),
        format!(
            "unknown base 'resonable'; expected one of: {}",
            BASES.join(", ")
        )
    );
    for name in BASES {
        assert!(
            for_invocation(&cwd, name, None, &[]).is_ok(),
            "'{name}' is advertised but does not resolve"
        );
    }
}

/// A missing `--restrict` file is reported by the path exarch actually looked
/// at, so a relative spelling cannot leave the user guessing which directory
/// that was.
#[test]
fn a_missing_restrict_file_is_reported_by_its_absolute_path() {
    let dir = Scratch::for_test(EXARCH, "missing-restrict").expect("scratch dir");
    let cwd = dir.path().to_string_lossy().into_owned();

    let err = for_invocation(
        &cwd,
        "reasonable",
        None,
        &[PathBuf::from("no-such-file.ral")],
    )
    .unwrap_err();
    assert_eq!(
        err,
        format!(
            "--restrict path does not exist: {}",
            dir.path().join("no-such-file.ral").display()
        )
    );
}
