use super::{
    FsRules, FsScope, GrantKind, ProjectionKey, existing_paths, filter_out_granted, profile_name,
    refuse_universal_prefix,
};
use crate::path::{FrozenPath, RealPath};
use std::collections::HashSet;
use std::path::PathBuf;

#[test]
fn existing_paths_drops_a_path_that_is_not_there() {
    let td = tempfile::tempdir().unwrap();
    let present = td.path().to_path_buf();
    let absent = td.path().join("does-not-exist-xyzzy");
    assert_eq!(existing_paths(vec![present.clone(), absent]), vec![present]);
}

#[test]
fn memo_drops_a_path_already_granted_at_the_same_kind() {
    let mut granted = HashSet::new();
    granted.insert((PathBuf::from(r"C:\work"), GrantKind::ReadWrite));
    let kept = filter_out_granted(
        &granted,
        vec![PathBuf::from(r"C:\work"), PathBuf::from(r"C:\other")],
        GrantKind::ReadWrite,
    );
    assert_eq!(kept, vec![PathBuf::from(r"C:\other")]);
}

#[test]
fn memo_is_specific_to_access_kind() {
    // Read-only coverage does not satisfy a read-write request: the two
    // stamp different access masks.
    let mut granted = HashSet::new();
    granted.insert((PathBuf::from(r"C:\work"), GrantKind::ReadOnly));
    let kept = filter_out_granted(
        &granted,
        vec![PathBuf::from(r"C:\work")],
        GrantKind::ReadWrite,
    );
    assert_eq!(kept, vec![PathBuf::from(r"C:\work")]);
}

#[test]
fn projection_key_identity_is_the_fs_triple() {
    let a = ProjectionKey {
        read: vec![r"C:\work".into()],
        write: vec![r"C:\work\out".into()],
        deny: vec![],
    };
    let same = a.clone();
    let narrower = ProjectionKey {
        read: vec![r"C:\work".into()],
        write: vec![],
        deny: vec![],
    };
    let denied = ProjectionKey {
        deny: vec![r"C:\work\secret".into()],
        ..a.clone()
    };
    assert_eq!(a, same);
    assert_ne!(a, narrower);
    assert_ne!(a, denied);
}

fn prefixes(paths: &[&str]) -> Vec<FsScope> {
    paths
        .iter()
        .map(|p| FsScope::Prefix(FrozenPath::from_surface(p)))
        .collect()
}

fn rules(read: &[&str], write: &[&str], deny: &[&str]) -> FsRules {
    FsRules {
        read: prefixes(read),
        write: prefixes(write),
        deny: deny.iter().map(FrozenPath::from_surface).collect(),
        pinned_dirs: Vec::new(),
    }
}

#[test]
fn the_universal_prefix_is_refused_in_either_region() {
    for rules in [
        rules(&["/"], &[], &[r"C:\secret"]),
        rules(&[], &["/"], &[r"C:\secret"]),
        rules(&[r"C:\work"], &["/"], &[]),
    ] {
        let why = refuse_universal_prefix(&rules).unwrap_err().to_string();
        assert!(why.contains("ral will not stamp every drive"), "{why}");
    }
    let why = refuse_universal_prefix(&rules(&["/"], &[], &[r"C:\secret"]))
        .unwrap_err()
        .to_string();
    assert!(why.contains("every path except some exclusions"), "{why}");
}

#[test]
fn the_root_is_refused_as_a_place_and_as_an_exclusion() {
    let place = FsRules {
        read: vec![FsScope::Place(RealPath::assumed("/"))],
        ..rules(&[], &[], &[])
    };
    for rules in [place, rules(&[r"C:\work"], &[], &["/"])] {
        let why = refuse_universal_prefix(&rules).unwrap_err().to_string();
        assert!(why.contains("ral will not stamp every drive"), "{why}");
    }
}

#[test]
fn a_drive_root_is_not_the_universal_prefix() {
    assert!(refuse_universal_prefix(&rules(&[r"C:"], &[r"C:\work"], &[])).is_ok());
}

#[test]
#[allow(clippy::disallowed_methods, reason = "[test] junction fixture")]
fn a_redirected_allowance_refuses_the_launch_before_the_memo_is_consulted() {
    use crate::process::Launch;
    use crate::process::cancel::CancelScope;
    use crate::sandbox::{ExecProjection, FsProjection, SandboxProjection};
    let _env = crate::test_env::env_guard();
    let root = tempfile::tempdir().unwrap();
    let (granted, outside) = (root.path().join("granted"), root.path().join("outside"));
    std::fs::create_dir(&granted).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let projection = SandboxProjection {
        fs: FsProjection::Restricted(rules(&[granted.to_str().unwrap()], &[], &[])),
        net: false,
        exec: ExecProjection::default(),
    };
    let confine = || {
        let mut launch = Launch::new(r"C:\Windows\System32\cmd.exe");
        super::confine(&mut launch, &projection, None, &CancelScope::default())
    };
    confine().expect("a real directory is granted");
    std::fs::remove_dir(&granted).unwrap();
    let junction = std::process::Command::new("cmd")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&granted)
        .arg(&outside)
        .output()
        .unwrap();
    assert!(junction.status.success(), "{junction:?}");
    let why = confine().unwrap_err().to_string();
    assert!(
        why.contains("an allowance names what it named when granted"),
        "{why}"
    );
    super::teardown();
}

/// End-to-end proof that the kernel honors a *private, derived*
/// capability SID: `CreateProcessW` must accept it inside
/// `SECURITY_CAPABILITIES`, the stamped allow-ACE must admit a write
/// inside the granted prefix (the positive control, load-bearing against
/// a vacuous denial), and deny-by-default must hold a write outside it.
///
/// Runs against the real user state dir under the shared env guard, so a
/// `dacl` test's scoped `XDG_STATE_HOME` can never point this test's
/// stamp writes at a deleted tempdir. What it leaves behind is one
/// stamp-store key naming a dead tempdir, inert.
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] e2e sandbox spawn scaffolding"
)]
fn confined_child_writes_inside_the_grant_and_not_outside() {
    let _env = crate::test_env::env_guard();
    use crate::process::cancel::CancelScope;
    use crate::process::{Launch, PgidPolicy, StdioSpec, WaitOutcome};
    use crate::sandbox::{ExecProjection, FsProjection, SandboxProjection};
    use std::path::Path;

    // Siblings under one root, so the child can name the ungranted one
    // relative to the granted cwd it runs in — see the quoting note below.
    let root = tempfile::tempdir().unwrap();
    let granted = root.path().join("granted");
    let outside = root.path().join("outside");
    std::fs::create_dir(&granted).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let projection = SandboxProjection {
        fs: FsProjection::Restricted(rules(
            &[granted.to_str().unwrap()],
            &[granted.to_str().unwrap()],
            &[],
        )),
        net: false,
        exec: ExecProjection::default(),
    };

    // `cmd.exe` loads via the ALL APPLICATION PACKAGES system grants, so
    // the only filesystem authority under test is the derived
    // capability's. Redirection creates the file directly; `copy NUL`
    // would additionally require access to a global DOS device.
    //
    // The `/c` string must carry no quote character, and so names its
    // target relative to the granted cwd the child runs in. That is not
    // tidiness: `CreateProcessW` takes one flat command line, so an
    // embedded `"` leaves this process escaped as `\"`: and `cmd.exe`,
    // which has no backslash escape, would then redirect into a filename
    // beginning with a backslash and a quote, fail its own syntax check,
    // and exit nonzero having never asked the filesystem anything. A bare
    // relative name admits no such reading, which is what leaves the exit
    // status below able to speak: the only way this child can now fail is
    // that the write was refused.
    //
    // Both arms report how the child *ended*, not just whether the file
    // appeared: a refused write and a child that never reached the write
    // leave the same empty directory, and only the exit status tells a
    // reader on another machine which of the two it is looking at.
    let cmd = Path::new(r"C:\Windows\System32\cmd.exe");
    let run_echo_to = |rel: &str| -> WaitOutcome {
        let mut launch = Launch::new(cmd);
        launch.args(["/d", "/s", "/c", &format!("echo ok>{rel}")]);
        launch.current_dir(&granted);
        launch.stdin(StdioSpec::null());
        launch.stdout(StdioSpec::null());
        launch.stderr(StdioSpec::null());
        super::confine(&mut launch, &projection, Some(cmd), &CancelScope::default())
            .expect("confine under a derived capability SID");
        let (mut child, _pgid, _) = launch
            .spawn(PgidPolicy::Inherit)
            .expect("CreateProcessW must accept the derived capability SIDs");
        let status = child.reap().expect("wait for the confined child");
        WaitOutcome::from_exit_status(status)
    };

    let ended_inside = run_echo_to("ok.txt");
    assert!(
        granted.join("ok.txt").exists(),
        "write inside the granted prefix must land, and did not; the child \
         ended {ended_inside:?}. A nonzero exit means it ran and the write \
         was refused: the stamped capability ACE or the Low mandatory \
         label failed to admit it. An exit that is neither 0 nor 1 means \
         the child never reached the write at all, and the grant is not \
         what is under test."
    );
    let ended_outside = run_echo_to(r"..\outside\nope.txt");
    assert!(
        !outside.join("nope.txt").exists(),
        "write outside the grant must be held by deny-by-default; the \
         child ended {ended_outside:?}"
    );

    super::teardown();
}

#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] LowBox deny enforcement through real file reads"
)]
fn confined_child_denies_protected_and_explicit_descendants() {
    use super::super::dacl::deny_tests::{DenyTree, READABLE, with_state};
    use crate::process::cancel::CancelScope;
    use crate::process::{Launch, PgidPolicy, StdioSpec};
    use crate::sandbox::{ExecProjection, FsProjection, SandboxProjection};
    use std::io::Read;
    use std::path::Path;

    with_state(|| {
        let tree = DenyTree::new();
        let denied = [tree.deny.to_str().unwrap()];
        let projection = |deny: bool| SandboxProjection {
            fs: FsProjection::Restricted(rules(
                &[tree.allow.to_str().unwrap()],
                &[],
                if deny { &denied[..] } else { &[] },
            )),
            net: false,
            exec: ExecProjection::default(),
        };
        let read = |relative: &str, deny: bool, readable: bool| {
            let cmd = Path::new(r"C:\Windows\System32\cmd.exe");
            let mut launch = Launch::new(cmd);
            launch.args(["/d", "/s", "/c", &format!("type {relative}")]);
            launch.current_dir(tree.cwd());
            launch.stdin(StdioSpec::null());
            launch.stdout(StdioSpec::Piped);
            launch.stderr(StdioSpec::Piped);
            super::confine(
                &mut launch,
                &projection(deny),
                Some(cmd),
                &CancelScope::default(),
            )
            .unwrap();
            let (mut child, _pgid, _) = launch.spawn(PgidPolicy::Inherit).unwrap();
            let mut stdout = Vec::new();
            child
                .take_stdout()
                .unwrap()
                .read_to_end(&mut stdout)
                .unwrap();
            let mut stderr = Vec::new();
            child
                .take_stderr()
                .unwrap()
                .read_to_end(&mut stderr)
                .unwrap();
            let ended = child.reap().unwrap();
            assert_eq!(
                ended.code(),
                Some(i32::from(!readable)),
                "{relative}, deny={deny}: {ended:?}, stderr={}",
                String::from_utf8_lossy(&stderr)
            );
            assert_eq!(
                stdout,
                if readable {
                    b"secret\r\n".as_slice()
                } else {
                    b""
                },
                "{relative}, deny={deny}"
            );
        };

        read("ok.txt", false, true);
        tree.shape();
        for relative in READABLE {
            read(relative, false, true);
        }
        read(r"D\dark\key.txt", false, false);
        read("ok.txt", true, true);
        for relative in READABLE.iter().skip(1) {
            read(relative, true, false);
        }
        read(r"D\dark\key.txt", true, false);
        // Persistent denies are projection-specific.
        read(r"D\protected\key.txt", false, true);
        super::teardown();
    });
}

#[test]
fn profile_name_is_pid_and_counter_keyed_within_the_length_cap() {
    let n0 = profile_name(0);
    let n1 = profile_name(1);
    assert_ne!(n0, n1);
    assert!(n0.starts_with(&format!("ral.sandbox.s{}.p", std::process::id())));
    // AppContainer profile names cap at 64 UTF-16 code units.
    assert!(profile_name(u32::MAX).encode_utf16().count() <= 64);
}
