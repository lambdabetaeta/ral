#![allow(clippy::disallowed_methods)]

//! Grant-policy admittance rules at the `Shell` boundary.
//!
//! Each test drives `Shell::with_capabilities` (and its companions
//! `check_exec_args`, `sandbox_projection`) over the public API only.
//! No internals are reached into — the policies and their meet/join
//! semantics are the contract.

use ral_core::types::{Capabilities, ExecDir, ExecMap, ExecPolicy, FsPolicy, Shell};
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn explicit_grant_denies_omitted_exec() {
    let mut shell = Shell::default();
    let result = shell.with_capabilities(Capabilities::deny_all(), |shell| {
        shell.check_exec_args("/bin/echo", &["/bin/echo"], &[])
    });
    assert!(result.is_err(), "deny-all grant must refuse /bin/echo");
}

/// A subpath-style key (`/usr/bin/`) admits a command whose
/// resolved absolute path is inside, even when the same map has
/// no per-name entry for it.  Replaces the old `exec_dirs`
/// admittance route.
// Unix-only: `/usr/bin/...` paths have no Windows analogue and the
// grant matcher normalises them in ways that don't survive on Windows.
#[cfg(unix)]
#[test]
fn subpath_key_admits_path_under_prefix() {
    let mut shell = Shell::default();
    let grant = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            dirs: BTreeMap::from([("/usr/bin".into(), ExecDir::Allow)]),
        }),
        ..Capabilities::root()
    };
    shell
        .with_capabilities(grant, |shell| {
            shell.check_exec_args("ls", &["ls", "/usr/bin/ls"], &[])
        })
        .expect("ls under /usr/bin/ subpath key should be admitted");
}

/// A subpath key does not admit a binary outside its prefix.
#[test]
fn subpath_key_denies_outside_prefix() {
    let mut shell = Shell::default();
    let grant = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            dirs: BTreeMap::from([("/usr/bin".into(), ExecDir::Allow)]),
        }),
        ..Capabilities::root()
    };
    let result = shell.with_capabilities(grant, |shell| {
        shell.check_exec_args("evil", &["evil", "/tmp/evil"], &[])
    });
    assert!(result.is_err());
}

/// Literal beats subpath: a per-name `Subcommands` restriction on
/// `cargo` is not relaxed by a sibling subpath key admitting the
/// directory cargo lives in.
#[test]
fn literal_subcommands_beats_subpath_admit() {
    let mut shell = Shell::default();
    let grant = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([(
                "cargo".into(),
                ExecPolicy::Subcommands(BTreeSet::from(["build".into()])),
            )]),
            dirs: BTreeMap::from([("/opt/homebrew/bin".into(), ExecDir::Allow)]),
        }),
        ..Capabilities::root()
    };
    let result = shell.with_capabilities(grant, |shell| {
        shell.check_exec_args(
            "cargo",
            &["cargo", "/opt/homebrew/bin/cargo"],
            &["install".into()],
        )
    });
    assert!(
        result.is_err(),
        "literal Subcommands restriction must beat sibling subpath admit"
    );
}

/// Subpath `Deny` carves a hole inside a broader subpath `Allow`:
/// `/usr/bin/sensitive/` denied, `/usr/bin/` allowed.  Longest
/// subpath wins, so a binary in the inner directory is denied
/// even though the outer admits.
// Unix-only for the same reason as `subpath_key_admits_path_under_prefix`.
#[cfg(unix)]
#[test]
fn subpath_deny_carves_hole_in_subpath_allow() {
    let mut shell = Shell::default();
    let grant = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            dirs: BTreeMap::from([
                ("/usr/bin".into(), ExecDir::Allow),
                ("/usr/bin/sensitive".into(), ExecDir::Deny),
            ]),
        }),
        ..Capabilities::root()
    };
    // Outside the deny region: admitted.
    shell
        .with_capabilities(grant.clone(), |sh| {
            sh.check_exec_args("ls", &["ls", "/usr/bin/ls"], &[])
        })
        .expect("ls under /usr/bin/ subpath should be admitted");
    // Inside the deny region: denied by the deeper subpath.
    let result = shell.with_capabilities(grant, |sh| {
        sh.check_exec_args("payload", &["payload", "/usr/bin/sensitive/payload"], &[])
    });
    assert!(
        result.is_err(),
        "longer subpath Deny should beat shorter subpath Allow"
    );
}

/// Deny-by-default within an opining layer: a layer with `exec`
/// declared admits *only* what's in its map.  A nested layer that
/// names `git` does not pass through to outer `/usr/bin/` admits.
/// (This replaces the old "name-only abstains" pattern, which was
/// an artefact of the now-removed `exec_dirs` field.)
#[test]
fn nested_exec_layer_denies_outside_its_map() {
    let mut shell = Shell::default();
    let outer = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            dirs: BTreeMap::from([("/usr/bin".into(), ExecDir::Allow)]),
        }),
        ..Capabilities::root()
    };
    let inner = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("git".into(), ExecPolicy::Allow)]),
            dirs: BTreeMap::new(),
        }),
        ..Capabilities::root()
    };
    let result = shell.with_capabilities(outer, |shell| {
        shell.with_capabilities(inner, |shell| {
            shell.check_exec_args("ls", &["ls", "/usr/bin/ls"], &[])
        })
    });
    assert!(
        result.is_err(),
        "inner layer's exec map is a complete opinion; ls is not in it"
    );
}

#[test]
fn exec_path_override_requires_resolved_path_authority() {
    let mut shell = Shell::default();
    let grant = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("git".into(), ExecPolicy::Allow)]),
            dirs: BTreeMap::new(),
        }),
        ..Capabilities::root()
    };
    let args = vec!["status".into()];
    let result = shell.with_capabilities(grant, |shell| {
        shell.check_exec_args("git", &["/tmp/fake-bin/git"], &args)
    });
    assert!(result.is_err());
}

#[test]
fn exec_path_override_allows_explicit_resolved_path() {
    let mut shell = Shell::default();
    let grant = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("/tmp/fake-bin/git".into(), ExecPolicy::Allow)]),
            dirs: BTreeMap::new(),
        }),
        ..Capabilities::root()
    };
    let args = vec!["status".into()];
    shell
        .with_capabilities(grant, |shell| {
            shell.check_exec_args("git", &["/tmp/fake-bin/git"], &args)
        })
        .expect("resolved-path grant should allow the substituted executable");
}

#[test]
fn sandbox_projection_intersects_path_components() {
    let mut shell = Shell::default();
    let outer = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec!["/tmp/ral-prefix-a".into()],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };
    let inner = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec!["/tmp/ral-prefix-ab".into()],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };
    let projection = shell.with_capabilities(outer, |shell| {
        shell.with_capabilities(inner, |shell| shell.sandbox_projection().unwrap())
    });
    assert!(projection.bind_spec().read_prefixes.is_empty());
}

#[cfg(unix)]
#[test]
fn sandbox_projection_does_not_leak_outer_raw_prefix() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    let inner_dir = real.join("inner");
    let link = temp.path().join("link");
    std::fs::create_dir_all(&inner_dir).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let mut shell = Shell::default();
    let outer = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![link.to_string_lossy().into_owned().into()],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };
    let inner = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![inner_dir.to_string_lossy().into_owned().into()],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };

    let projection = shell.with_capabilities(outer, |shell| {
        shell.with_capabilities(inner, |shell| shell.sandbox_projection().unwrap())
    });
    let bind_spec = projection.bind_spec();
    assert!(
        !bind_spec
            .read_prefixes
            .contains(&link.to_string_lossy().into_owned())
    );
    assert!(
        bind_spec
            .read_prefixes
            .contains(&inner_dir.to_string_lossy().into_owned())
    );
}

/// The bare/absolute identity duality is closed on the veto side: a
/// `reasonable`-shaped grant denies `bash` by bare name but allows
/// `/bin/`.  An absolute invocation `/bin/bash` carries no bare name in
/// its narrow admission set, but its broad deny set surfaces the
/// basename `bash`, so the `bash: Deny` literal vetoes even though the
/// `/bin/` allow dir would otherwise admit the resolved path.
#[test]
fn broad_deny_set_vetoes_path_invoked_denied_basename() {
    let mut shell = Shell::default();
    let grant = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("bash".into(), ExecPolicy::Deny)]),
            dirs: BTreeMap::from([("/bin".into(), ExecDir::Allow)]),
        }),
        ..Capabilities::root()
    };
    // deny set (broad): resolved path plus its basename; allow set
    // (narrow): the resolved path only — exactly what a Path head's
    // deny_names / policy_names produce for `/bin/bash`.
    let result = shell.with_capabilities(grant, |sh| {
        sh.check_exec_call("/bin/bash", &["/bin/bash", "bash"], &["/bin/bash"], &[])
    });
    assert!(
        result.is_err(),
        "bare bash: Deny must veto a direct /bin/bash invocation"
    );
}

/// Anti-spoof preserved: a planted binary invoked by absolute path must
/// not inherit a bare-name `allow`.  The only `rg` grant is the bare
/// literal `rg: Allow` (no covering allow dir); the basename `rg` is in
/// the broad deny set but NOT the narrow admission set, so a Path head
/// `/tmp/evil/rg` is denied.
#[test]
fn broad_deny_set_does_not_admit_planted_path_invoked_basename() {
    let mut shell = Shell::default();
    let grant = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("rg".into(), ExecPolicy::Allow)]),
            dirs: BTreeMap::new(),
        }),
        ..Capabilities::root()
    };
    let result = shell.with_capabilities(grant, |sh| {
        sh.check_exec_call("/tmp/evil/rg", &["/tmp/evil/rg", "rg"], &["/tmp/evil/rg"], &[])
    });
    assert!(
        result.is_err(),
        "bare rg: Allow must not admit a Path-invoked /tmp/evil/rg via its basename"
    );
}

/// No regression on the resolved-absolute deny: a literal `Deny` on the
/// resolved absolute path still vetoes a bare invocation whose broad
/// set carries that path.
#[test]
fn literal_deny_on_resolved_absolute_still_vetoes() {
    let mut shell = Shell::default();
    let grant = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("/usr/bin/git".into(), ExecPolicy::Deny)]),
            dirs: BTreeMap::from([("/usr/bin".into(), ExecDir::Allow)]),
        }),
        ..Capabilities::root()
    };
    let result = shell.with_capabilities(grant, |sh| {
        sh.check_exec_call("git", &["git", "/usr/bin/git"], &["git", "/usr/bin/git"], &[])
    });
    assert!(
        result.is_err(),
        "literal Deny on the resolved absolute path must veto"
    );
}

/// No regression: bare `git` admitted under a `reasonable`-shaped grant
/// (bare `git: Allow`), and its `status` subcommand gating stays intact
/// when the literal carries a `Subcommands` restriction.
#[test]
fn bare_admit_and_subcommand_gating_unregressed() {
    // Bare git: Allow admits a bare invocation with any args.
    let mut shell = Shell::default();
    let allow = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("git".into(), ExecPolicy::Allow)]),
            dirs: BTreeMap::new(),
        }),
        ..Capabilities::root()
    };
    shell
        .with_capabilities(allow, |sh| {
            sh.check_exec_call("git", &["git", "/usr/bin/git"], &["git", "/usr/bin/git"], &[
                "status".into(),
            ])
        })
        .expect("bare git: Allow must admit");

    // Subcommands restriction: `status` admitted, `push` denied.
    let mut shell = Shell::default();
    let gated = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([(
                "git".into(),
                ExecPolicy::Subcommands(BTreeSet::from(["status".into()])),
            )]),
            dirs: BTreeMap::new(),
        }),
        ..Capabilities::root()
    };
    shell
        .with_capabilities(gated.clone(), |sh| {
            sh.check_exec_call("git", &["git", "/usr/bin/git"], &["git", "/usr/bin/git"], &[
                "status".into(),
            ])
        })
        .expect("git status must be admitted under Subcommands([status])");
    let denied = shell.with_capabilities(gated, |sh| {
        sh.check_exec_call("git", &["git", "/usr/bin/git"], &["git", "/usr/bin/git"], &[
            "push".into(),
        ])
    });
    assert!(
        denied.is_err(),
        "git push must be denied under Subcommands([status])"
    );
}
