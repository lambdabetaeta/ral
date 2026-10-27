#![allow(clippy::disallowed_methods, reason = "test code")]

//! Grant-policy admittance rules at the `Shell` boundary.
//!
//! Each test drives `Shell::with_layer` and `sandbox_projection` over
//! the public API, and the in-process exec guard through `test_access`'s entries, which
//! judge a file a test lays on disk in a tempdir.  The policies and their
//! meet/extension semantics are the contract.

use ral_core::authority::policy::{Capabilities, ExecGrant, ExecKey, FsPolicy};
use ral_core::path::FrozenPath;
use ral_core::test_access::check_file;
#[cfg(unix)]
use ral_core::test_access::{KernelExecRule, exec_projection_files, exec_projection_rules};
use ral_core::test_helper::{Tree, layer};
#[cfg(unix)]
use std::path::Path;

/// `names` laid in a tempdir: a program is judged as the file a walk holds.
fn laid(names: &[&str]) -> (tempfile::TempDir, Tree) {
    let tmp = tempfile::tempdir().unwrap();
    let tree = Tree::lay(tmp.path(), names);
    (tmp, tree)
}

fn name(name: &str) -> ExecKey {
    ExecKey::Name(name.to_string())
}

fn path(tree: &Tree, path: &str) -> ExecKey {
    ExecKey::Path(FrozenPath::from_surface(&tree.at(path)))
}

fn dir(tree: &Tree, dir: &str) -> ExecKey {
    ExecKey::Dir(FrozenPath::from_surface(&tree.at(dir)))
}

fn exec_only(exec: ExecGrant) -> Capabilities {
    Capabilities {
        exec: Some(exec),
        ..Capabilities::default()
    }
}

/// An excluded dir carves a hole inside a broader allowed dir: a binary in
/// the inner directory is refused though the outer admits.
#[test]
fn an_excluded_dir_carves_a_hole_in_an_allowed_dir() {
    let (_tmp, tree) = laid(&["/bin/ls", "/bin/sensitive/payload"]);
    let mut shell = ral_core::test_helper::core_shell();
    let grant = exec_only(ExecGrant::of(
        [dir(&tree, "/bin")],
        [dir(&tree, "/bin/sensitive")],
    ));
    shell
        .with_layer(layer(grant.clone()), |sh| {
            check_file(sh, &tree.at("/bin/ls"))
        })
        .expect("ls under the allow dir should be admitted");
    let result = shell.with_layer(layer(grant), |sh| {
        check_file(sh, &tree.at("/bin/sensitive/payload"))
    });
    assert!(result.is_err(), "the exclusion should beat the allowed dir");
}

/// A bare key is the file the host `PATH` finds, so it admits no other file
/// of that name.
#[test]
fn a_bare_allow_admits_no_other_file_of_its_name() {
    let (_tmp, tree) = laid(&["/fake-bin/git"]);
    let mut shell = ral_core::test_helper::core_shell();
    let grant = exec_only(ExecGrant::of([name("git")], []));
    let result = shell.with_layer(layer(grant), |shell| {
        check_file(shell, &tree.at("/fake-bin/git"))
    });
    assert!(result.is_err());
}

#[test]
fn a_path_key_admits_its_file() {
    let (_tmp, tree) = laid(&["/fake-bin/git"]);
    let mut shell = ral_core::test_helper::core_shell();
    let grant = exec_only(ExecGrant::of([path(&tree, "/fake-bin/git")], []));
    shell
        .with_layer(layer(grant), |shell| {
            check_file(shell, &tree.at("/fake-bin/git"))
        })
        .expect("a path key should admit the file it names");
}

#[test]
fn sandbox_projection_intersects_path_components() {
    let tree = Tree::nowhere();
    let mut shell = ral_core::test_helper::core_shell();
    let outer = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![FrozenPath::from_surface(&tree.at("/ral-prefix-a"))],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        }),
        ..Capabilities::default()
    };
    let inner = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![FrozenPath::from_surface(&tree.at("/ral-prefix-ab"))],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        }),
        ..Capabilities::default()
    };
    let projection = shell.with_layer(layer(outer), |shell| {
        shell.with_layer(layer(inner), |shell| shell.sandbox_projection().unwrap())
    });
    let read = ral_core::test_access::fs_read_paths(&projection.fs).expect("fs restricted");
    let prefix = ral_core::path::shown(tree.path("/ral-prefix"));
    assert!(!read.iter().any(|p| p.starts_with(&prefix)), "{read:?}");
}

#[cfg(unix)]
#[test]
fn sandbox_projection_does_not_leak_outer_raw_prefix() {
    let temp = tempfile::tempdir().unwrap();
    // Canonical, as the projection reads: macOS's temp dir lies under `/var`.
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let real = root.join("real");
    let inner_dir = real.join("inner");
    let link = root.join("link");
    std::fs::create_dir_all(&inner_dir).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let mut shell = ral_core::test_helper::core_shell();
    let outer = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![FrozenPath::from_surface(&link.to_string_lossy())],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        }),
        ..Capabilities::default()
    };
    let inner = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![FrozenPath::from_surface(&inner_dir.to_string_lossy())],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        }),
        ..Capabilities::default()
    };

    let projection = shell.with_layer(layer(outer), |shell| {
        shell.with_layer(layer(inner), |shell| shell.sandbox_projection().unwrap())
    });
    let read = ral_core::test_access::fs_read_paths(&projection.fs).expect("fs restricted");
    assert!(!read.contains(&link.to_string_lossy().into_owned()));
    assert!(read.contains(&inner_dir.to_string_lossy().into_owned()));
}

/// A bare deny vetoes its name everywhere: a `reasonable`-shaped grant denies
/// `bash` by bare name but allows a bin dir, and a `bash` in that dir is
/// still denied, however the head spelled it.
#[test]
fn a_bare_deny_vetoes_its_name_under_an_allow_dir() {
    let (_tmp, tree) = laid(&["/bin/bash"]);
    let mut shell = ral_core::test_helper::core_shell();
    let grant = exec_only(ExecGrant::of([dir(&tree, "/bin")], [name("bash")]));
    let result = shell.with_layer(layer(grant), |sh| check_file(sh, &tree.at("/bin/bash")));
    assert!(
        result.is_err(),
        "a bare bash: deny must veto a bash under an allow dir"
    );
}

/// A planted binary must not inherit a bare-name `allow`: the bare `rg` is
/// the host's `rg`, and `evil/rg` is another file.
#[test]
fn a_bare_allow_does_not_admit_a_planted_file_of_its_name() {
    let (_tmp, tree) = laid(&["/evil/rg"]);
    let mut shell = ral_core::test_helper::core_shell();
    let grant = exec_only(ExecGrant::of([name("rg")], []));
    let result = shell.with_layer(layer(grant), |sh| check_file(sh, &tree.at("/evil/rg")));
    assert!(
        result.is_err(),
        "a bare rg: allow must not admit a planted evil/rg"
    );
}

/// A path key's deny beats the allow dir the file sits in.
#[test]
fn a_path_key_deny_beats_a_covering_allow_dir() {
    let (_tmp, tree) = laid(&["/bin/git"]);
    let mut shell = ral_core::test_helper::core_shell();
    let grant = exec_only(ExecGrant::of(
        [dir(&tree, "/bin")],
        [path(&tree, "/bin/git")],
    ));
    let result = shell.with_layer(layer(grant), |sh| check_file(sh, &tree.at("/bin/git")));
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
        read_prefixes: vec![FrozenPath::root()],
        write_prefixes: Vec::new(),
        deny_paths: Vec::new(),
    }
}

#[cfg(unix)]
fn with_fs(exec: ExecGrant) -> Capabilities {
    Capabilities {
        exec: Some(exec),
        fs: Some(projection_fs()),
        ..Capabilities::default()
    }
}

/// A path key admitted by one layer and covered only by a *sibling* layer's
/// allow dir must reach the OS projection as an allowed file: the in-process
/// guard admits the file, so the OS profile, built from the same table, must
/// list it, or the kernel would kill a command the guard ran.
#[cfg(unix)]
#[test]
fn sandbox_projection_admits_a_path_key_covered_by_a_sibling_dir() {
    let (_tmp, tree) = laid(&["/bin/git"]);
    let outer = with_fs(ExecGrant::of([dir(&tree, "/bin")], []));
    let inner = with_fs(ExecGrant::of([path(&tree, "/bin/git")], []));
    let git = tree.at("/bin/git");

    let mut shell = ral_core::test_helper::core_shell();
    let projection = shell.with_layer(layer(outer.clone()), |sh| {
        sh.with_layer(layer(inner.clone()), |sh| sh.sandbox_projection().unwrap())
    });
    let files = exec_projection_files(&projection);
    assert!(
        files.contains(&git),
        "the path key covered by the sibling allow dir must reach the rules, got {files:?}"
    );

    let mut shell = ral_core::test_helper::core_shell();
    shell
        .with_layer(layer(outer), |sh| {
            sh.with_layer(layer(inner), |sh| check_file(sh, &git))
        })
        .expect("the in-process guard must admit the file");
}

/// Model the kernel over the rules it is handed: a real path runs where some
/// allowing clause holds it and no excluding clause does.
#[cfg(unix)]
fn kernel_admits(rules: Option<&[KernelExecRule]>, real: &str) -> bool {
    let Some(rules) = rules else {
        return true;
    };
    let real = Path::new(real);
    let holds = |rule: &KernelExecRule| match rule {
        KernelExecRule::Any { .. } => true,
        KernelExecRule::Dir { path, .. } => real.starts_with(path),
        KernelExecRule::File { path, .. } => real == Path::new(path),
        KernelExecRule::Name(name) => real.file_name() == Some(name.as_ref()),
    };
    let allows = |rule: &KernelExecRule| match rule {
        KernelExecRule::Any { allow }
        | KernelExecRule::Dir { allow, .. }
        | KernelExecRule::File { allow, .. } => *allow,
        KernelExecRule::Name(_) => false,
    };
    let held = || rules.iter().filter(|rule| holds(rule));
    held().any(allows) && !held().any(|rule| !allows(rule))
}

/// Conservatism invariant (safety direction): the OS projection must never
/// admit a command the in-process guard would deny.  Checked differentially
/// over adversarial two-layer stacks, each probed with a spread of real paths.
/// Only support may widen the kernel's set, and no probe here is any.
#[cfg(unix)]
#[test]
fn exec_projection_never_out_permits_the_guard() {
    let (_tmp, tree) = laid(&[
        "/bin/git",
        "/bin/ls",
        "/bin/sudo",
        "/bin/bash",
        "/bin/nested/bash",
        "/opt/bin/tool",
        "/evil",
    ]);
    let allow_dir = |d: &str| ExecGrant::of([dir(&tree, d)], []);
    let cases: Vec<(ExecGrant, ExecGrant, Vec<&str>)> = vec![
        // A path key admitted by inner, covered only by outer's allow dir.
        (
            allow_dir("/bin"),
            ExecGrant::of([path(&tree, "/bin/git")], []),
            vec!["/bin/git", "/bin/ls", "/evil"],
        ),
        // A path key's deny carves a hole in a shared allow dir.
        (
            allow_dir("/bin"),
            ExecGrant::of([dir(&tree, "/bin")], [path(&tree, "/bin/sudo")]),
            vec!["/bin/ls", "/bin/sudo"],
        ),
        // A bare deny vetoes its name wherever it lands under the allow dir.
        (
            allow_dir("/bin"),
            ExecGrant::of([dir(&tree, "/bin")], [name("bash")]),
            vec!["/bin/ls", "/bin/bash", "/bin/nested/bash"],
        ),
        // A path key's deny must veto a file both dirs would admit.
        (
            allow_dir("/bin"),
            ExecGrant::of([dir(&tree, "/bin")], [path(&tree, "/bin/bash")]),
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
        let mut shell = ral_core::test_helper::core_shell();
        let projection = shell.with_layer(layer(outer.clone()), |sh| {
            sh.with_layer(layer(inner.clone()), |sh| sh.sandbox_projection().unwrap())
        });
        let kernel = exec_projection_rules(&projection);
        for probe in probes {
            let real = tree.at(probe);
            let mut shell = ral_core::test_helper::core_shell();
            let guard_ok = shell
                .with_layer(layer(outer.clone()), |sh| {
                    sh.with_layer(layer(inner.clone()), |sh| check_file(sh, &real))
                })
                .is_ok();
            if kernel_admits(kernel.as_deref(), &real) {
                assert!(
                    guard_ok,
                    "OS projection admits {real} but the in-process guard denies it (unsound)"
                );
            }
        }
    }
}
