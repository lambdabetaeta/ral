#![allow(clippy::disallowed_methods)]

//! Grant-policy admittance rules at the `Shell` boundary.
//!
//! Each test drives `Shell::with_capabilities` and `sandbox_projection` over
//! the public API, and the in-process exec guard through `test_access`'s doors, which
//! judge a file by the real path a test names.  The policies and their
//! meet/widen semantics are the contract.

use ral_core::path::NormalizedPrefix;
use ral_core::test_access::check_file;
use ral_core::types::{Capabilities, ExecGrant, FsPolicy, Shell, Verdict};
#[cfg(unix)]
use ral_core::types::{ExecProjection, ExecRule};
use std::collections::BTreeMap;
#[cfg(unix)]
use std::path::Path;

/// `p` as an absolute path on the host, which for Windows needs a drive.
/// Rooted at `/ral-test`, which no host has, so a frozen key resolves to
/// itself and names the file a test judges.
fn host(p: &str) -> String {
    let p = format!("/ral-test{p}");
    if cfg!(windows) {
        format!("C:{}", p.replace('/', r"\"))
    } else {
        p
    }
}

fn names(entries: &[(&str, Verdict)]) -> BTreeMap<String, Verdict> {
    entries
        .iter()
        .map(|(name, v)| ((*name).to_string(), v.clone()))
        .collect()
}

fn paths(entries: &[(&str, Verdict)]) -> BTreeMap<NormalizedPrefix, Verdict> {
    entries
        .iter()
        .map(|(path, v)| (NormalizedPrefix::from_surface(host(path)), v.clone()))
        .collect()
}

fn dirs(entries: &[(&str, bool)]) -> BTreeMap<NormalizedPrefix, bool> {
    entries
        .iter()
        .map(|(dir, v)| (NormalizedPrefix::from_surface(host(dir)), *v))
        .collect()
}

fn exec_only(exec: ExecGrant) -> Capabilities {
    Capabilities {
        exec: Some(exec),
        ..Capabilities::root()
    }
}

/// A file rule beats a dir: an `Only` restriction on `cargo`'s file is not
/// relaxed by a sibling dir key admitting the directory cargo lives in.
#[test]
fn a_file_rule_restriction_beats_a_covering_allow_dir() {
    let mut shell = Shell::default();
    let grant = exec_only(ExecGrant {
        paths: paths(&[("/bin/cargo", Verdict::Only(["build".to_string()].into()))]),
        dirs: dirs(&[("/bin", true)]),
        ..ExecGrant::default()
    });
    let result = shell.with_capabilities(grant, |shell| {
        check_file(shell, &host("/bin/cargo"), &["install".into()])
    });
    assert!(
        result.is_err(),
        "a file rule's subcommand restriction must beat a covering allow dir"
    );
}

/// A deny dir carves a hole inside a broader allow dir: the deepest dir
/// wins, so a binary in the inner directory is denied even though the outer
/// admits.
#[test]
fn a_deeper_deny_dir_carves_a_hole_in_an_allow_dir() {
    let mut shell = Shell::default();
    let grant = exec_only(ExecGrant {
        dirs: dirs(&[("/bin", true), ("/bin/sensitive", false)]),
        ..ExecGrant::default()
    });
    shell
        .with_capabilities(grant.clone(), |sh| check_file(sh, &host("/bin/ls"), &[]))
        .expect("ls under the allow dir should be admitted");
    let result = shell.with_capabilities(grant, |sh| {
        check_file(sh, &host("/bin/sensitive/payload"), &[])
    });
    assert!(
        result.is_err(),
        "the deeper deny dir should beat the allow dir"
    );
}

/// A bare key is the file the host `PATH` finds, so it admits no other file
/// of that name.
#[test]
fn a_bare_allow_admits_no_other_file_of_its_name() {
    let mut shell = Shell::default();
    let grant = exec_only(ExecGrant {
        names: names(&[("git", Verdict::Allow)]),
        ..ExecGrant::default()
    });
    let result = shell.with_capabilities(grant, |shell| {
        check_file(shell, &host("/fake-bin/git"), &["status".into()])
    });
    assert!(result.is_err());
}

#[test]
fn a_path_key_admits_its_file() {
    let mut shell = Shell::default();
    let grant = exec_only(ExecGrant {
        paths: paths(&[("/fake-bin/git", Verdict::Allow)]),
        ..ExecGrant::default()
    });
    shell
        .with_capabilities(grant, |shell| {
            check_file(shell, &host("/fake-bin/git"), &["status".into()])
        })
        .expect("a path key should admit the file it names");
}

#[test]
fn sandbox_projection_intersects_path_components() {
    let mut shell = Shell::default();
    let outer = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![NormalizedPrefix::from_surface("/tmp/ral-prefix-a")],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };
    let inner = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![NormalizedPrefix::from_surface("/tmp/ral-prefix-ab")],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };
    let projection = shell.with_capabilities(outer, |shell| {
        shell.with_capabilities(inner, |shell| shell.sandbox_projection().unwrap())
    });
    assert!(
        ral_core::test_access::fs_rules(&projection.fs).is_some_and(|r| r.read_prefixes.is_empty())
    );
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
            read_prefixes: vec![NormalizedPrefix::from_surface(
                link.to_string_lossy().into_owned(),
            )],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };
    let inner = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![NormalizedPrefix::from_surface(
                inner_dir.to_string_lossy().into_owned(),
            )],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        }),
        ..Capabilities::root()
    };

    let projection = shell.with_capabilities(outer, |shell| {
        shell.with_capabilities(inner, |shell| shell.sandbox_projection().unwrap())
    });
    let read = &ral_core::test_access::fs_rules(&projection.fs)
        .expect("fs restricted")
        .read_prefixes;
    assert!(!read.contains(&link.to_string_lossy().into_owned()));
    assert!(read.contains(&inner_dir.to_string_lossy().into_owned()));
}

/// A bare deny vetoes its name everywhere: a `reasonable`-shaped grant denies
/// `bash` by bare name but allows a bin dir, and a `bash` in that dir is
/// still denied, however the head spelled it.
#[test]
fn a_bare_deny_vetoes_its_name_under_an_allow_dir() {
    let mut shell = Shell::default();
    let grant = exec_only(ExecGrant {
        names: names(&[("bash", Verdict::Deny)]),
        dirs: dirs(&[("/bin", true)]),
        ..ExecGrant::default()
    });
    let result = shell.with_capabilities(grant, |sh| check_file(sh, &host("/bin/bash"), &[]));
    assert!(
        result.is_err(),
        "a bare bash: deny must veto a bash under an allow dir"
    );
}

/// A planted binary must not inherit a bare-name `allow`: the bare `rg` is
/// the host's `rg`, and `evil/rg` is another file.
#[test]
fn a_bare_allow_does_not_admit_a_planted_file_of_its_name() {
    let mut shell = Shell::default();
    let grant = exec_only(ExecGrant {
        names: names(&[("rg", Verdict::Allow)]),
        ..ExecGrant::default()
    });
    let result = shell.with_capabilities(grant, |sh| check_file(sh, &host("/evil/rg"), &[]));
    assert!(
        result.is_err(),
        "a bare rg: allow must not admit a planted evil/rg"
    );
}

/// A path key's deny beats the allow dir the file sits in.
#[test]
fn a_path_key_deny_beats_a_covering_allow_dir() {
    let mut shell = Shell::default();
    let grant = exec_only(ExecGrant {
        paths: paths(&[("/bin/git", Verdict::Deny)]),
        dirs: dirs(&[("/bin", true)]),
        ..ExecGrant::default()
    });
    let result = shell.with_capabilities(grant, |sh| check_file(sh, &host("/bin/git"), &[]));
    assert!(result.is_err(), "a path key's deny must veto its file");
}

/// A broad fs grant, so `sandbox_projection()` returns `Some` on every
/// platform (it short-circuits to `None` for an exec-only restriction
/// off macOS) and the exec dimension under test is observable.
///
/// Unix-only: every caller below is `#[cfg(unix)]` (the Seatbelt/bwrap
/// projection shape this models is Unix-specific).
#[cfg(unix)]
fn projection_fs() -> FsPolicy {
    FsPolicy {
        read_prefixes: vec![NormalizedPrefix::root()],
        write_prefixes: Vec::new(),
        deny_paths: Vec::new(),
    }
}

/// Model the kernel over the projection's rules: the last rule that matches
/// a resolved path decides, and none denies.
///
/// Unix-only: its sole caller is `#[cfg(unix)]`.
#[cfg(unix)]
fn projection_admits(exec: &ExecProjection, resolved: &str) -> bool {
    let ExecProjection::Restricted(rules) = exec else {
        return true;
    };
    let base = Path::new(resolved).file_name().and_then(|n| n.to_str());
    rules
        .iter()
        .rev()
        .find_map(|rule| match rule {
            ExecRule::Dir { path, allow } => {
                let path = path.to_string();
                (resolved == path || resolved.starts_with(&format!("{path}/"))).then_some(*allow)
            }
            ExecRule::File { path, allow } => (resolved == path.to_string()).then_some(*allow),
            ExecRule::Veto(name) => (base == Some(name.as_str())).then_some(false),
        })
        .unwrap_or(false)
}

#[cfg(unix)]
fn with_fs(exec: ExecGrant) -> Capabilities {
    Capabilities {
        exec: Some(exec),
        fs: Some(projection_fs()),
        ..Capabilities::root()
    }
}

/// A path key admitted by one layer and covered only by a *sibling* layer's
/// allow dir must reach the OS projection as an allowed file: the in-process
/// guard admits the file, so the OS profile, built from the same table, must
/// list it, or the kernel would kill a command the guard ran.
#[cfg(unix)]
#[test]
fn sandbox_projection_admits_a_path_key_covered_by_a_sibling_dir() {
    let outer = with_fs(ExecGrant {
        dirs: dirs(&[("/bin", true)]),
        ..ExecGrant::default()
    });
    let inner = with_fs(ExecGrant {
        paths: paths(&[("/bin/git", Verdict::Allow)]),
        ..ExecGrant::default()
    });
    let git = host("/bin/git");

    let mut shell = Shell::default();
    let projection = shell.with_capabilities(outer.clone(), |sh| {
        sh.with_capabilities(inner.clone(), |sh| sh.sandbox_projection().unwrap())
    });
    let ExecProjection::Restricted(rules) = &projection.exec else {
        panic!("exec should be restricted, got {:?}", projection.exec);
    };
    assert!(
        rules.iter().any(
            |rule| matches!(rule, ExecRule::File { path, allow: true } if path.to_string() == git)
        ),
        "the path key covered by the sibling allow dir must reach the rules, got {rules:?}"
    );

    let mut shell = Shell::default();
    shell
        .with_capabilities(outer, |sh| {
            sh.with_capabilities(inner, |sh| check_file(sh, &git, &[]))
        })
        .expect("the in-process guard must admit the file");
}

/// Conservatism invariant (safety direction): the OS projection must never
/// admit a command the in-process guard would deny.  Checked differentially
/// over adversarial two-layer stacks, each probed with a spread of real paths.
/// Only a carrier may widen the kernel's set, and no probe here is one.
#[cfg(unix)]
#[test]
fn exec_projection_never_out_permits_the_guard() {
    let allow_dir = |d: &str| ExecGrant {
        dirs: dirs(&[(d, true)]),
        ..ExecGrant::default()
    };
    let cases: Vec<(ExecGrant, ExecGrant, Vec<&str>)> = vec![
        // A path key admitted by inner, covered only by outer's allow dir.
        (
            allow_dir("/bin"),
            ExecGrant {
                paths: paths(&[("/bin/git", Verdict::Allow)]),
                ..ExecGrant::default()
            },
            vec!["/bin/git", "/bin/ls", "/evil"],
        ),
        // A path key's deny carves a hole in a shared allow dir.
        (
            allow_dir("/bin"),
            ExecGrant {
                paths: paths(&[("/bin/sudo", Verdict::Deny)]),
                dirs: dirs(&[("/bin", true)]),
                ..ExecGrant::default()
            },
            vec!["/bin/ls", "/bin/sudo"],
        ),
        // A bare deny vetoes its name wherever it lands under the allow dir.
        (
            allow_dir("/bin"),
            ExecGrant {
                names: names(&[("bash", Verdict::Deny)]),
                dirs: dirs(&[("/bin", true)]),
                ..ExecGrant::default()
            },
            vec!["/bin/ls", "/bin/bash", "/bin/nested/bash"],
        ),
        // A path key's deny must veto a file both dirs would admit.
        (
            allow_dir("/bin"),
            ExecGrant {
                paths: paths(&[("/bin/bash", Verdict::Deny)]),
                dirs: dirs(&[("/bin", true)]),
                ..ExecGrant::default()
            },
            vec!["/bin/ls", "/bin/bash"],
        ),
        // Disjoint allow dirs meet to nothing.
        (
            allow_dir("/bin"),
            allow_dir("/opt/bin"),
            vec!["/bin/ls", "/opt/bin/tool"],
        ),
    ];

    for (outer_exec, inner_exec, probes) in cases {
        let outer = with_fs(outer_exec);
        let inner = with_fs(inner_exec);
        let mut shell = Shell::default();
        let projection = shell.with_capabilities(outer.clone(), |sh| {
            sh.with_capabilities(inner.clone(), |sh| sh.sandbox_projection().unwrap())
        });
        for probe in probes {
            let real = host(probe);
            let mut shell = Shell::default();
            let guard_ok = shell
                .with_capabilities(outer.clone(), |sh| {
                    sh.with_capabilities(inner.clone(), |sh| check_file(sh, &real, &[]))
                })
                .is_ok();
            if projection_admits(&projection.exec, &real) {
                assert!(
                    guard_ok,
                    "OS projection admits {real} but the in-process guard denies it (unsound)"
                );
            }
        }
    }
}
