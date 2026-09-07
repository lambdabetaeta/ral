#![allow(clippy::disallowed_methods)]

//! [`exarch::policy::for_invocation`] end to end, in its own test binary: the
//! two lattice phases in their fixed order, the deny that has to land on the
//! real fs gate rather than merely on a list, the two diagnostics a user meets
//! before a session starts, and the grant summary the model reads.
//!
//! Every scenario needs a directory of `.ral` profiles on disk — a composed
//! grant is a fact about files, and the assertions here are worth only as much
//! as the loader they went through.

use exarch::bootstrap::{EXARCH, SYNOD, Scratch};
use exarch::policy::for_invocation;
use exarch::prompt::host_section;
use ral_core::capability::FsOp;
use ral_core::path::basedir::XdgKind;
use ral_core::path::{NormalizedPrefix, SearchCwd, resolve_in_path};
use ral_core::types::{Break, GrantStack, Settled, Shell};
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

/// Where `name` really lives on this host, for the resolved half of an exec
/// check.  `/usr/bin/ls` is `/bin/ls` on macOS and `rustc` is wherever rustup
/// put it, so the fixture asks `PATH` rather than naming a directory.
fn on_path(name: &str) -> String {
    let path = std::env::var("PATH").expect("PATH is set");
    resolve_in_path(name, &path, SearchCwd::nowhere()).unwrap_or_else(|| {
        panic!("no executable '{name}' on this host's PATH, so the fixture has no real path for it:\n{path}")
    })
}

/// The gate's refusal message, or a panic naming what arrived instead.
fn refusal(r: Settled<()>) -> String {
    match r {
        Ok(()) => panic!("the gate admitted a call it had to refuse"),
        Err(Break::Error(err)) => err.message,
        Err(other) => panic!("expected a grant denial, got: {other:?}"),
    }
}

/// Install every layer of `stack` onto a fresh throwaway shell — the stack is
/// the meet, so a real session's whole composed authority is these layers
/// folded at every check, never one flattened `Capabilities`.
fn install(shell: &mut Shell, stack: &GrantStack) {
    for layer in stack {
        shell.push_session_capabilities(layer.clone());
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

    let (stack, _) =
        for_invocation(&cwd, "minimal", Some(&extend), &[restrict]).expect("profiles compose");

    let mut shell = Shell::default();
    install(&mut shell, &stack);
    shell
        .check_exec_args("rustc", &["rustc", &on_path("rustc")], &[])
        .expect_err("--extend-base must not outlive a --restrict that omits it");
    shell
        .check_exec_args("ls", &["ls", &on_path("ls")], &[])
        .expect("what both sides name survives the fold");
}

/// `--restrict` is documented as order-free.  Each file becomes its own
/// layer, and a layer's exec map is an allowlist over the whole namespace —
/// `b` naming no directory answers "outside my reach" for `git`, exactly as
/// the restrict file of the test above answers for `rustc`.  So the fold
/// admits `ls` alone, the name both files carry, and the two argv orders,
/// which build stacks differing in shape, must agree on that verdict and on
/// every other, each file's own deny entry surviving whichever side it was.
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

    let (ab, _) = for_invocation(&cwd, "dangerous", None, &[a.clone(), b.clone()])
        .expect("profiles compose");
    let (ba, _) =
        for_invocation(&cwd, "dangerous", None, &[b, a]).expect("profiles compose");

    let ls = on_path("ls");
    for stack in [&ab, &ba] {
        let mut shell = Shell::default();
        install(&mut shell, stack);
        shell
            .check_exec_args("ls", &["ls", &ls], &[])
            .expect("what both files name is admitted regardless of restrict argv order");
        for one_sided in ["git", "cat"] {
            let resolved = on_path(one_sided);
            let message = refusal(shell.check_exec_args(one_sided, &[one_sided, &resolved], &[]));
            assert!(
                message.contains(one_sided),
                "a name only one restrict file carries must be refused, by name: {message}"
            );
        }
        assert!(
            !stack.net().all(|n| n),
            "net: false in either file must survive the fold, flag order must not reach it"
        );
        for name in ["a.ral", "b.ral"] {
            let path = dir.path().join(name);
            let denied = stack
                .fs()
                .flat_map(|fs| fs.deny_paths.iter())
                .filter(|p| p.as_str() == path.to_string_lossy())
                .count();
            assert_eq!(
                denied, 1,
                "{name} should be denied exactly once whichever order it was named"
            );
        }
        for fs in stack.fs() {
            assert!(
                fs.deny_paths.is_sorted(),
                "each layer's own deny list is canonical, not argv-ordered: {:?}",
                fs.deny_paths
            );
        }
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

    let (stack, _) = for_invocation(&cwd, "dangerous", None, std::slice::from_ref(&restrict))
        .expect("profile composes");

    let mut shell = Shell::default();
    install(&mut shell, &stack);
    for spelling in [&restrict, &canonical] {
        let path = shell.resolve(&spelling.to_string_lossy());
        let message = refusal(shell.locate(&path, &FsOp::Write).map(|_| ()));
        assert!(
            message.contains("denied by grant"),
            "{} should be refused by the grant, got: {message}",
            spelling.display()
        );
    }
    let path = shell.resolve(&sibling.to_string_lossy());
    shell
        .locate(&path, &FsOp::Write)
        .expect("the deny is targeted: a sibling file stays writable");
}

/// The key that pays for the turn is not a thing the turn may read back.
/// `reasonable`, `read-only` and `edit-only` read `xdg:config` and `xdg:state`
/// wholesale so tools find their configs, and our own credential files sit in
/// exactly that reach — `cat` was the whole exploit.  Every attenuated base is
/// checked, since a base that does not grant the read today may tomorrow.
///
/// The paths are spelled out here rather than taken from
/// `provider::credential_files`, which would only agree with itself; and both
/// halves are asserted, because being unreadable by *veto* is the claim — an
/// unreadable path that is merely ungranted is one widening away from readable.
#[test]
fn no_attenuated_base_can_read_a_credential_file() {
    let dir = Scratch::for_test(EXARCH, "credential-deny").expect("scratch dir");
    let cwd = dir.path().to_string_lossy().into_owned();
    let secrets = [
        EXARCH.xdg_dir(XdgKind::Config).join("keys.json"),
        SYNOD.xdg_dir(XdgKind::Config).join("keys.json"),
        EXARCH.xdg_dir(XdgKind::State).join("oauth.json"),
    ];

    for base in BASES.into_iter().filter(|b| *b != "dangerous") {
        let (stack, _) = for_invocation(&cwd, base, None, &[]).expect("base composes");
        assert!(
            stack.fs().next().is_some(),
            "{base} attenuates the filesystem"
        );
        let mut shell = Shell::default();
        install(&mut shell, &stack);
        for secret in &secrets {
            let denied_somewhere = stack
                .fs()
                .flat_map(|fs| fs.deny_paths.iter())
                .any(|p| p.as_str() == secret.to_string_lossy());
            assert!(
                denied_somewhere,
                "{base} must veto {}, not merely leave it ungranted",
                secret.display()
            );
            let path = shell.resolve(&secret.to_string_lossy());
            let message = refusal(shell.check_fs_read(&path));
            assert!(
                message.contains("denied by grant"),
                "{base} should refuse to read {}, got: {message}",
                secret.display()
            );
        }
    }
}

/// `dangerous` is ambient authority by contract, so it installs no fs policy
/// at all — and two denies are not worth turning every unconfined session into
/// a confined one against an agent that can reach the same bytes a hundred
/// other ways.  Naming a restrict file *does* attenuate, and then the
/// credential carve-out lands with it.
#[test]
fn dangerous_stays_ambient_until_something_attenuates_it() {
    let dir = Scratch::for_test(EXARCH, "credential-deny-dangerous").expect("scratch dir");
    let cwd = dir.path().to_string_lossy().into_owned();
    let oauth = EXARCH.xdg_dir(XdgKind::State).join("oauth.json");

    let (stack, _) = for_invocation(&cwd, "dangerous", None, &[]).expect("dangerous composes");
    assert!(
        stack.fs().next().is_none(),
        "dangerous must attenuate nothing"
    );

    let restrict = profile(&dir, "restrict.ral", "return [net: false]\n");
    let (stack, _) = for_invocation(&cwd, "dangerous", None, std::slice::from_ref(&restrict))
        .expect("dangerous composes with a restrict file");
    let denies: Vec<String> = stack
        .fs()
        .flat_map(|fs| fs.deny_paths.iter())
        .map(|p| p.as_str().to_string())
        .collect();
    assert!(
        denies.iter().any(|p| *p == *oauth.to_string_lossy()),
        "an attenuated dangerous must still carve out the tokens: {denies:?}"
    );
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
    let (stack, _) = for_invocation(&cwd, "minimal", None, &[]).expect("minimal composes");
    let text = host_section(&stack, &dir);

    assert!(
        !text.contains("Ambient authority"),
        "an attenuated session must not be told it holds everything:\n{text}"
    );
    assert_eq!(
        stack.net().collect::<Vec<_>>(),
        vec![true],
        "minimal declares net: true, and is the only layer with an opinion"
    );
    assert_eq!(bullet(&text, "- net:"), "- net: allow");
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
    let (stack, _) = for_invocation(&cwd, "dangerous", None, &[]).expect("dangerous composes");
    let text = host_section(&stack, &dir);

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
