#![allow(
    clippy::disallowed_methods,
    reason = "[test] filesystem and process fixtures"
)]

//! [`exarch::policy::for_invocation`] end to end, in its own test binary: the
//! two lattice phases in their fixed order, the deny that has to land on the
//! real fs guard rather than merely on a list, and the two diagnostics a user
//! meets before a session starts.
//!
//! Every scenario needs a directory of `.ral` profiles on disk — a composed
//! grant is a fact about files, and the assertions here are worth only as much
//! as the loader they went through.

use exarch::app::{EXARCH, SYNOD, Scratch};
use exarch::policy::{Invocation, for_invocation};
use ral_core::capability::FsOp;
use ral_core::capability::GrantStack;
use ral_core::path::XdgKind;
use ral_core::path::{Resolver, locate};
use ral_core::test_access::check_file;
use ral_core::types::{Break, Settled, Shell};
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

/// The real path of this test binary, standing for exarch's own.
fn own_binary() -> String {
    ral_core::path::shown(
        std::fs::canonicalize(std::env::current_exe().expect("the test binary has a path"))
            .expect("the test binary exists"),
    )
}

/// The real path of the file `name` runs on this host — what a bare key
/// denotes: the canonical path of what `PATH` finds.  `rustc` is wherever
/// rustup put it, so the fixture asks `PATH` rather than naming a directory.
fn on_path(name: &str) -> String {
    let path = std::env::var("PATH").expect("PATH is set");
    let hit = locate(name, Some(&path), None).unwrap_or_else(|| {
        panic!("no executable '{name}' on this host's PATH, so the fixture has no real path for it:\n{path}")
    });
    let cwd = ral_core::test_helper::process_cwd();
    let object = Resolver::at(&ral_core::types::EnvVars::new(), &cwd)
        .anchor(&hit)
        .canonicalise_strict();
    ral_core::path::shown(object.expect("a PATH hit exists"))
}

/// The guard's refusal message, or a panic naming what arrived instead.
fn refusal(r: Settled<()>) -> String {
    match r {
        Ok(()) => panic!("the guard admitted a call it had to refuse"),
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

/// The extension runs before the meet, so an extended grant the attenuation file
/// never names is erased by it.  Inverting the phases turns `--extend`
/// into an escape hatch from `--restrict`; `ls`, named on both sides, is the
/// control that says the meet attenuated rather than emptied.
#[test]
fn an_extend_grant_cannot_survive_a_restrict_that_omits_it() {
    let dir = Scratch::for_test(EXARCH, "extend-then-restrict").expect("scratch dir");
    let extend = profile(
        &dir,
        "ext.ral",
        "return [exec: [allow: [`name rustc, `name ls]]]\n",
    );
    let restrict = profile(&dir, "restrict.ral", "return [exec: [allow: [`name ls]]]\n");
    let cwd = dir.path();

    let Invocation { stack, .. } =
        for_invocation(cwd, "minimal", Some(&extend), &[restrict]).expect("profiles compose");

    let mut shell = ral_core::test_helper::core_shell();
    install(&mut shell, &stack);
    check_file(&mut shell, &on_path("rustc"), &[])
        .expect_err("--extend must not outlive a --restrict that omits it");
    check_file(&mut shell, &on_path("ls"), &[]).expect("what both sides name survives the fold");
}

/// Two restrictions whose verdicts do not depend on their order (it can: see
/// the order test below).  Each file becomes its own
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
        "return [fs: [read: [`cwd .], write: [`cwd .]], exec: [allow: [`name git, `name ls]]]\n",
    );
    // `rustc`, not `cat`: an all-in-one coreutils makes `cat` and `ls` one file.
    let b = profile(
        &dir,
        "b.ral",
        "return [exec: [allow: [`name ls, `name rustc]], net: `offline]\n",
    );
    let cwd = dir.path();

    let Invocation { stack: ab, .. } =
        for_invocation(cwd, "dangerous", None, &[a.clone(), b.clone()]).expect("profiles compose");
    let Invocation { stack: ba, .. } =
        for_invocation(cwd, "dangerous", None, &[b, a]).expect("profiles compose");

    let ls = on_path("ls");
    for stack in [&ab, &ba] {
        let mut shell = ral_core::test_helper::core_shell();
        install(&mut shell, stack);
        check_file(&mut shell, &ls, &[])
            .expect("what both files name is admitted regardless of restrict argv order");
        for one_sided in ["git", "rustc"] {
            let real = on_path(one_sided);
            let message = refusal(check_file(&mut shell, &real, &[]));
            assert!(
                message.contains(&real),
                "a name only one restrict file carries must be refused, by its real path: {message}"
            );
        }
        assert!(
            stack.net().next().is_some(),
            "net: `offline in either file must survive the fold, flag order must not reach it"
        );
        for name in ["a.ral", "b.ral"] {
            let path = dir.path().join(name);
            let denied = stack
                .fs()
                .flat_map(|fs| fs.deny_paths.iter())
                .filter(|p| ral_core::path::shown(p.as_str()) == ral_core::path::shown(&path))
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
/// entry is lexical, and the guard expands it — so the file is refused by both
/// its own spelling and its canonical one, while its sibling stays writable.
///
/// The ceiling is `` `cwd . ``, the directory holding both files: a literal `/`
/// would be a foreign-rooted dead grant on Windows, leaving nothing writable
/// and the sibling refused for the wrong reason.
#[test]
fn a_restrict_file_is_refused_by_the_fs_guard_under_either_spelling() {
    let dir = Scratch::for_test(EXARCH, "restrict-unwritable").expect("scratch dir");
    let restrict = profile(
        &dir,
        "restrict.ral",
        "return [fs: [read: [`cwd .], write: [`cwd .]]]\n",
    );
    let sibling = profile(&dir, "scratch.txt", "ordinary work\n");
    let canonical = std::fs::canonicalize(&restrict).expect("the restrict file exists");
    let cwd = dir.path();

    let Invocation { stack, .. } =
        for_invocation(cwd, "dangerous", None, std::slice::from_ref(&restrict))
            .expect("profile composes");

    let mut shell = ral_core::test_helper::core_shell();
    install(&mut shell, &stack);
    for spelling in [&restrict, &canonical] {
        let path = shell.anchor(spelling);
        let message = refusal(shell.locate(&path, &FsOp::Write).map(|_| ()));
        assert!(
            message.contains("denied by grant"),
            "{} should be refused by the grant, got: {message}",
            ral_core::path::shown(spelling)
        );
    }
    let path = shell.anchor(&sibling);
    shell
        .locate(&path, &FsOp::Write)
        .expect("the deny is targeted: a sibling file stays writable");
}

/// The key that pays for the turn is not a thing the turn may read back.
/// `reasonable`, `read-only` and `edit-only` read `xdg-config .` and `xdg-state .`
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
    let cwd = dir.path();
    let secrets = [
        EXARCH.xdg_dir(XdgKind::Config).join("keys.json"),
        SYNOD.xdg_dir(XdgKind::Config).join("keys.json"),
        EXARCH.xdg_dir(XdgKind::State).join("oauth.json"),
    ];

    for base in BASES.into_iter().filter(|b| *b != "dangerous") {
        let Invocation { stack, .. } = for_invocation(cwd, base, None, &[]).expect("base composes");
        assert!(
            stack.fs().next().is_some(),
            "{base} attenuates the filesystem"
        );
        let mut shell = ral_core::test_helper::core_shell();
        install(&mut shell, &stack);
        for secret in &secrets {
            let denied_somewhere = stack
                .fs()
                .flat_map(|fs| fs.deny_paths.iter())
                .any(|p| ral_core::path::shown(p.as_str()) == ral_core::path::shown(secret));
            assert!(
                denied_somewhere,
                "{base} must veto {}, not merely leave it ungranted",
                ral_core::path::shown(secret)
            );
            let path = shell.anchor(secret);
            let message = refusal(shell.check_fs_read(&path));
            assert!(
                message.contains("denied by grant"),
                "{base} should refuse to read {}, got: {message}",
                ral_core::path::shown(secret)
            );
        }
    }
}

/// `dangerous` is ambient authority: it installs no restriction, and three
/// denies are not worth turning every unconfined session into a confined one
/// against an agent that can reach the same bytes a hundred other ways.
/// Naming a restrict file *does* attenuate, and then the credential carve-out
/// lands with it.
#[test]
fn dangerous_stays_ambient_until_something_attenuates_it() {
    let dir = Scratch::for_test(EXARCH, "credential-deny-dangerous").expect("scratch dir");
    let cwd = dir.path();
    let oauth = EXARCH.xdg_dir(XdgKind::State).join("oauth.json");

    let Invocation { stack, .. } =
        for_invocation(cwd, "dangerous", None, &[]).expect("dangerous composes");
    assert!(!stack.is_restrictive(), "dangerous must attenuate nothing");
    let mut shell = ral_core::test_helper::core_shell();
    install(&mut shell, &stack);
    check_file(&mut shell, &own_binary(), &[]).expect("exarch runs");
    check_file(&mut shell, &on_path("ls"), &[]).expect("everything runs");

    let restrict = profile(
        &dir,
        "restrict.ral",
        "return [fs: [read: [`cwd .], write: [`cwd .]]]\n",
    );
    let Invocation { stack, .. } =
        for_invocation(cwd, "dangerous", None, std::slice::from_ref(&restrict))
            .expect("dangerous composes with a restrict file");
    let denies: Vec<String> = stack
        .fs()
        .flat_map(|fs| fs.deny_paths.iter())
        .map(|p| ral_core::path::shown(p.as_str()))
        .collect();
    assert!(
        denies.iter().any(|p| *p == ral_core::path::shown(&oauth)),
        "an attenuated dangerous must still carve out the tokens: {denies:?}"
    );
}

/// Only a restrict file that restricts fs brings the exclusions of its own
/// path: an exec-only one leaves fs top, so the session is not confined for
/// its sake, while one with fs allowances is itself out of reach.
#[test]
fn only_an_fs_restricting_restrict_file_is_excluded() {
    let dir = Scratch::for_test(EXARCH, "restrict-file-exclusion").expect("scratch dir");
    let cwd = dir.path();
    let exec_only = profile(&dir, "exec.ral", "return [exec: [allow: [`name ls]]]\n");
    let with_fs = profile(
        &dir,
        "fs.ral",
        "return [fs: [read: [`cwd .], write: [`cwd .]]]\n",
    );

    let Invocation { stack, .. } =
        for_invocation(cwd, "dangerous", None, &[exec_only]).expect("composes");
    assert!(
        stack.fs_is_top(),
        "an fs-silent restrict file confines no fs"
    );

    let Invocation { stack, .. } =
        for_invocation(cwd, "dangerous", None, std::slice::from_ref(&with_fs)).expect("composes");
    let mut shell = ral_core::test_helper::core_shell();
    install(&mut shell, &stack);
    let path = shell.anchor(&with_fs);
    let message = refusal(shell.locate(&path, &FsOp::Write).map(|_| ()));
    assert!(message.contains("denied by grant"), "{message}");
}

/// An extension's allowance never removes permission, so `dangerous` stays
/// unrestricted, exarch's own binary included; its exclusion makes every
/// request, less it, and so restricts exec and brings the self layer.
#[test]
fn dangerous_extended_is_unrestricted_less_the_extensions_exclusions() {
    let dir = Scratch::for_test(EXARCH, "extend-dangerous").expect("scratch dir");
    let cwd = dir.path();
    let positive = profile(&dir, "positive.ral", "return [exec: [allow: [`name ls]]]\n");
    let excluding = profile(
        &dir,
        "excluding.ral",
        "return [exec: [deny: [`name rustc]]]\n",
    );

    let Invocation { stack, .. } =
        for_invocation(cwd, "dangerous", Some(&positive), &[]).expect("profiles compose");
    assert!(
        stack.iter().nth(1).is_some_and(|c| !c.is_restrictive()),
        "an allowance beside `dangerous` leaves the ceiling unrestricted"
    );
    let mut shell = ral_core::test_helper::core_shell();
    install(&mut shell, &stack);
    check_file(&mut shell, &on_path("rustc"), &[]).expect("`every` still runs what is unnamed");
    check_file(&mut shell, &own_binary(), &[]).expect("exec is top, so no self layer");

    let Invocation { stack, .. } =
        for_invocation(cwd, "dangerous", Some(&excluding), &[]).expect("profiles compose");
    let mut shell = ral_core::test_helper::core_shell();
    install(&mut shell, &stack);
    let message = refusal(check_file(&mut shell, &on_path("rustc"), &[]));
    assert!(message.contains(&on_path("rustc")), "{message}");
    check_file(&mut shell, &on_path("ls"), &[]).expect("every request less the exclusion");
    refusal(check_file(&mut shell, &own_binary(), &[]));
}

/// `--restrict` files apply in argument order: a layer reads the run places
/// of the layers beneath it.  A layer that reads nothing, beneath one that
/// runs a file, refuses it; the other way round the file runs and is read.
#[test]
fn restrictions_apply_in_argument_order() {
    let dir = Scratch::for_test(EXARCH, "restrict-order").expect("scratch dir");
    let cwd = dir.path();
    let bin = tempfile::tempdir().expect("scratch dir for the program");
    let prog = bin.path().join("prog");
    std::fs::write(&prog, "#!/bin/sh\n").expect("the program");
    let prog = ral_core::path::shown(std::fs::canonicalize(prog).expect("the program exists"));
    let reads_nothing = profile(&dir, "reads-nothing.ral", "return [fs: [read: []]]\n");
    let runs = profile(
        &dir,
        "runs.ral",
        &format!("return [exec: [allow: [`absolute {prog}]]]\n"),
    );
    let verdict = |files: [PathBuf; 2]| {
        let Invocation { stack, .. } =
            for_invocation(cwd, "dangerous", None, &files).expect("composes");
        let mut shell = ral_core::test_helper::core_shell();
        install(&mut shell, &stack);
        check_file(&mut shell, &prog, &[])
    };
    verdict([reads_nothing.clone(), runs.clone()])
        .expect_err("a layer that reads nothing, beneath the run, cannot read it");
    verdict([runs, reads_nothing]).expect("the run beneath makes the program readable");
}

/// The self layer follows exec restriction, as the credential layer follows
/// fs: a restrict file admitting exarch's own directory leaves the self layer
/// alone to refuse the binary, and plain `dangerous`, restricting no exec,
/// pushes none.
#[test]
fn the_self_layer_follows_an_exec_restriction() {
    let dir = Scratch::for_test(EXARCH, "self-layer").expect("scratch dir");
    let cwd = dir.path();
    let exe = own_binary();
    let parent = std::path::Path::new(&exe).parent().expect("a directory");
    let admitting = profile(
        &dir,
        "admitting.ral",
        &format!(
            "return [exec: [allow: [`dir `absolute {}]]]\n",
            ral_core::path::shown(parent)
        ),
    );
    let verdict = |restricts: &[PathBuf]| {
        let Invocation { stack, .. } =
            for_invocation(cwd, "dangerous", None, restricts).expect("profiles compose");
        let mut shell = ral_core::test_helper::core_shell();
        install(&mut shell, &stack);
        check_file(&mut shell, &exe, &[])
    };
    refusal(verdict(std::slice::from_ref(&admitting)));
    verdict(&[]).expect("plain `dangerous` restricts no exec, so pushes no self layer");
}

/// The credential layer follows fs restriction, not fs presence: an extension
/// that excludes a path restricts, and one that only allows does not.
#[test]
fn the_credential_layer_follows_an_fs_restriction() {
    let dir = Scratch::for_test(EXARCH, "credential-extend").expect("scratch dir");
    let cwd = dir.path();
    let oauth = EXARCH.xdg_dir(XdgKind::State).join("oauth.json");
    let carved = |stack: &GrantStack| {
        stack
            .fs()
            .flat_map(|fs| fs.deny_paths.iter())
            .any(|p| ral_core::path::shown(p.as_str()) == ral_core::path::shown(&oauth))
    };
    let excluding = profile(
        &dir,
        "excluding.ral",
        "return [fs: [deny: [`cwd secret]]]\n",
    );
    let positive = profile(&dir, "positive.ral", "return [fs: [read: [`cwd .]]]\n");

    let Invocation { stack, .. } =
        for_invocation(cwd, "dangerous", Some(&excluding), &[]).expect("profiles compose");
    assert!(
        carved(&stack),
        "an fs exclusion restricts, so the tokens are carved out"
    );
    let Invocation { stack, .. } =
        for_invocation(cwd, "dangerous", Some(&positive), &[]).expect("profiles compose");
    assert!(!carved(&stack), "a positive-only fs restricts nothing");
}

/// A misspelt base names every live one back, and each name it offers really
/// resolves — the message and the match arms pinned to each other in both
/// directions, so a seventh bake-in cannot arrive unadvertised.
#[test]
fn an_unknown_base_names_every_base_that_exists() {
    let dir = Scratch::for_test(EXARCH, "unknown-base").expect("scratch dir");
    let cwd = dir.path();

    assert_eq!(
        for_invocation(cwd, "resonable", None, &[]).unwrap_err(),
        format!(
            "unknown base 'resonable'; expected one of: {}",
            BASES.join(", ")
        )
    );
    for name in BASES {
        assert!(
            for_invocation(cwd, name, None, &[]).is_ok(),
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
    let cwd = dir.path();

    let err = for_invocation(
        cwd,
        "reasonable",
        None,
        &[PathBuf::from("no-such-file.ral")],
    )
    .unwrap_err();
    assert_eq!(
        err,
        format!(
            "--restrict path does not exist: {}",
            ral_core::path::shown(dir.path().join("no-such-file.ral"))
        )
    );
}

/// `--extend` is a union through `for_invocation`: the extended ceiling admits
/// what the extension names and still everything `reasonable` admitted.  Skipped
/// where the host has no `rustc`, which the extension's key resolves through.
#[test]
fn an_extend_union_keeps_what_the_base_admitted() {
    let path = std::env::var("PATH").expect("PATH is set");
    if locate("rustc", Some(&path), None).is_none() {
        return;
    }
    let dir = Scratch::for_test(EXARCH, "extend-union").expect("scratch dir");
    let extend = profile(&dir, "ext.ral", "return [exec: [allow: [`name rustc]]]\n");
    let cwd = dir.path();

    let base = for_invocation(cwd, "reasonable", None, &[]).expect("base composes");
    let extended =
        for_invocation(cwd, "reasonable", Some(&extend), &[]).expect("extension composes");

    let mut shell = ral_core::test_helper::core_shell();
    install(&mut shell, &extended.stack);
    check_file(&mut shell, &on_path("rustc"), &[]).expect("the extension's name is admitted");
    let mut plain = ral_core::test_helper::core_shell();
    install(&mut plain, &base.stack);
    for name in ["ls", "cat"] {
        let real = on_path(name);
        if check_file(&mut plain, &real, &[]).is_ok() {
            check_file(&mut shell, &real, &[]).unwrap_or_else(|_| {
                panic!("--extend must not narrow what reasonable admits: {name}")
            });
        }
    }
}
