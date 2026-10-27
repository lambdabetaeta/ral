use super::{SessionGuard, Tree, cmd, confined, projection, rules, with_scoped_state};
use crate::authority::policy::{Baseline, Capabilities, FsPolicy};
use crate::authority::sandbox::{ExecProjection, FsProjection, Refusal, SandboxProjection};
use crate::path::FrozenPath;
use crate::process::{Launch, PgidPolicy};
use crate::test_helper::confinement::{Output, admitted, denied};
use crate::test_helper::{layer, real, stack};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn capture(launch: &mut Launch) -> std::io::Result<Output> {
    let (mut child, _, _) = launch.spawn(PgidPolicy::Inherit)?;
    let mut stdout = String::new();
    child.take_stdout().unwrap().read_to_string(&mut stdout)?;
    let mut stderr = String::new();
    child.take_stderr().unwrap().read_to_string(&mut stderr)?;
    Ok(Output {
        status: child.reap()?.code().unwrap_or(1),
        stdout,
        stderr,
    })
}

fn run(projection: &SandboxProjection, cwd: &Path, line: &str) -> Output {
    capture(&mut confined(projection, cwd, line).expect("confine the matrix probe"))
        .expect("spawn and capture the matrix probe")
}

#[test]
fn read_write_and_excluded_arms_execute_under_a_carved_directory() {
    with_scoped_state(|| {
        let _session = SessionGuard;
        let proof = crate::require_confinement!();
        let tree = Tree::new();
        for path in [
            r"matrix\R\key.txt",
            r"matrix\W\key.txt",
            r"matrix\X\key.txt",
        ] {
            tree.write(path);
        }
        let root = tree.at("matrix");
        let (read, write, exclude) = (
            tree.real("matrix"),
            tree.real(r"matrix\W"),
            tree.real(r"matrix\X"),
        );
        let grant = SandboxProjection {
            fs: FsProjection::Restricted(rules(&[&read], &[&write], &[&exclude])),
            net: false,
            gpu: true,
            exec: ExecProjection::default(),
        };
        let control = run(&grant, &root, r"type R\key.txt");
        admitted(&proof, &control, "secret");
        denied(
            &proof,
            &control,
            &run(&grant, &root, r"type X\key.txt"),
            "secret",
        );
        let control = run(&grant, &root, r"echo secret>W\made.txt && type W\made.txt");
        admitted(&proof, &control, "secret");
        for arm in ["R", "X"] {
            let out = run(
                &grant,
                &root,
                &format!(r"echo secret>{arm}\made.txt && type {arm}\made.txt"),
            );
            denied(&proof, &control, &out, "secret");
            assert!(!root.join(arm).join("made.txt").exists(), "{out:?}");
        }
    });
}

fn fs_layer(read: &[&Path], write: &[&Path]) -> crate::authority::policy::GrantLayer {
    let freeze = |paths: &[&Path]| {
        paths
            .iter()
            .map(|path| FrozenPath::freeze(path).unwrap())
            .collect()
    };
    layer(Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: freeze(read),
            write_prefixes: freeze(write),
            deny_paths: Vec::new(),
        }),
        ..Capabilities::default()
    })
}

#[test]
fn nested_grants_omit_the_outer_launch_capabilities() {
    with_scoped_state(|| {
        let _session = SessionGuard;
        let proof = crate::require_confinement!();
        let tree = Tree::new();
        tree.write(r"B\key.txt");
        let (a, b) = (tree.at("A"), tree.at("B"));
        let mut grants = stack([fs_layer(&[tree.root.path()], &[&b])]);
        let outer = SandboxProjection::of(&grants, &Baseline::default(), None).unwrap();
        admitted(&proof, &run(&outer, &a, r"type ..\B\key.txt"), "secret");
        admitted(
            &proof,
            &run(
                &outer,
                &a,
                r"echo secret>..\B\outer.txt && type ..\B\outer.txt",
            ),
            "secret",
        );

        grants.push(fs_layer(&[&a], &[]));
        let inner = SandboxProjection::of(&grants, &Baseline::default(), None).unwrap();
        let control = run(&inner, &a, "type ok.txt");
        for target in ["key.txt", "outer.txt"] {
            denied(
                &proof,
                &control,
                &run(&inner, &a, &format!(r"type ..\B\{target}")),
                "secret",
            );
        }
        let out = run(
            &inner,
            &a,
            r"echo secret>..\B\inner.txt && type ..\B\inner.txt",
        );
        denied(&proof, &control, &out, "secret");
        assert!(!b.join("inner.txt").exists(), "{out:?}");
        admitted(&proof, &run(&outer, &a, r"type ..\B\key.txt"), "secret");
    });
}

struct Junction(PathBuf);

impl Junction {
    fn new(link: PathBuf, target: &Path) -> Self {
        let out = std::process::Command::new(cmd())
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&link)
            .arg(target)
            .output()
            .unwrap();
        assert!(out.status.success(), "create junction: {out:?}");
        Self(link)
    }
}

impl Drop for Junction {
    fn drop(&mut self) {
        std::fs::remove_dir(&self.0).expect("remove only the fixture's junction");
    }
}

#[test]
fn a_junction_replacing_a_granted_leaf_or_ancestor_refuses_the_next_launch() {
    with_scoped_state(|| {
        let _session = SessionGuard;
        let proof = crate::require_confinement!();
        let tree = Tree::new();
        for (index, suffix) in ["", "inner"].into_iter().enumerate() {
            let base = tree.at(&format!("grant{index}"));
            let granted = base.join(suffix);
            std::fs::create_dir_all(&granted).unwrap();
            std::fs::write(granted.join("key.txt"), "secret").unwrap();
            let grant = projection(&[&crate::path::shown(real(&granted).as_path())], &[]);
            admitted(&proof, &run(&grant, &granted, "type key.txt"), "secret");

            let outside = tree.at(&format!("outside{index}"));
            std::fs::create_dir_all(outside.join(suffix)).unwrap();
            std::fs::write(outside.join(suffix).join("key.txt"), "secret").unwrap();
            std::fs::rename(&base, tree.at(&format!("saved{index}"))).unwrap();
            let _junction = Junction::new(base, &outside);
            let refusal = match confined(&grant, tree.root.path(), "echo secret") {
                Err(Refusal::Launch(why)) => why,
                Err(other) => panic!("expected a redirected allowance, got {other}"),
                Ok(_) => panic!("a junction on the frozen allowance admitted a launch"),
            };
            assert!(refusal.contains("is now a link"), "{refusal}");
        }
    });
}

#[test]
fn a_staged_spawn_keeps_every_sid_owner_alive_through_teardown() {
    with_scoped_state(|| {
        let _session = SessionGuard;
        let proof = crate::require_confinement!();
        let tree = Tree::new();
        let a = tree.at("A");
        let mut grant = projection(&[&tree.real("A")], &[]);
        grant.net = true;
        admitted(&proof, &run(&grant, &a, "type ok.txt"), "secret");

        // Include teardown-before-spawn, independent of the race's scheduling.
        for race in [false, true] {
            let mut launch = confined(&grant, &a, "type ok.txt").unwrap();
            let (package, filesystem, network) = {
                use crate::sync::LockExt as _;
                let guard = super::super::cell().lock_ignore_poison();
                let session = guard.as_ref().unwrap();
                let key = super::super::ProjectionKey {
                    read: vec![real(&a).as_path().to_path_buf()],
                    write: Vec::new(),
                    deny: Vec::new(),
                };
                let package = Arc::downgrade(&session.projections[&key].sid());
                let filesystem: Vec<_> = session
                    .fs_cap_sids
                    .values()
                    .filter(|sid| Arc::strong_count(sid) > 1)
                    .map(Arc::downgrade)
                    .collect();
                let network = Arc::downgrade(session.network_caps.as_ref().unwrap());
                drop(guard);
                (package, filesystem, network)
            };
            assert!(!filesystem.is_empty());
            let out = if race {
                let (mut start, mut release) = os_pipe::pipe().unwrap();
                std::thread::scope(|scope| {
                    let teardown = scope.spawn(move || {
                        start.read_exact(&mut [0]).unwrap();
                        super::super::teardown();
                    });
                    release.write_all(&[1]).unwrap();
                    let out = capture(&mut launch);
                    teardown.join().unwrap();
                    out
                })
            } else {
                super::super::teardown();
                capture(&mut launch)
            };
            assert!(package.upgrade().is_some(), "launch lost its package SID");
            assert!(network.upgrade().is_some(), "launch lost its network SIDs");
            assert!(filesystem.iter().all(|sid| sid.upgrade().is_some()));
            match out {
                Ok(out) => admitted(&proof, &out, "secret"),
                Err(error) => assert!(error.raw_os_error().is_some(), "{error}"),
            }
            drop(launch);
            assert!(package.upgrade().is_none());
            assert!(network.upgrade().is_none());
            assert!(filesystem.iter().all(|sid| sid.upgrade().is_none()));
        }
    });
}
