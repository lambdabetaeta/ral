//! Host and bundled children share a filesystem grant, including pipeline
//! stages, and the same write is confined through each evaluation context.

use ral_core::test_helper::confinement::{denied, run};
use ral_core::test_helper::stack;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use ral_core::authority::policy::{Capabilities, FsPolicy};
#[cfg(all(feature = "coreutils", any(target_os = "linux", target_os = "macos")))]
use ral_core::fact::ExitCode as _;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ral_core::path::FrozenPath;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ral_core::test_helper::confinement::Confining;
#[cfg(all(feature = "coreutils", any(target_os = "linux", target_os = "macos")))]
use ral_core::test_helper::confinement::admitted;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use ral_core::test_helper::layer;

#[test]
fn a_same_map_deny_holds_inside_a_write_prefix_at_the_shell_boundary() {
    let proof = ral_core::require_confinement!();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_string_lossy();
    let secret = tmp.path().join("secret");
    let allowed = tmp.path().join("allowed");
    std::fs::write(&secret, "original").unwrap();
    let write = |path: &std::path::Path| {
        run(
            stack([]),
            &format!(
                "grant [fs: [read: [`absolute '{root}'], write: [`absolute '{root}'], deny: [`absolute '{}']]] {{ echo changed > '{}'; echo ok }}",
                secret.display(),
                path.display(),
            ),
        )
    };
    let control = write(&allowed);
    let refused = write(&secret);
    denied(&proof, &control, &refused, "ok");
    assert_eq!(std::fs::read_to_string(allowed).unwrap(), "changed\n");
    assert_eq!(std::fs::read_to_string(secret).unwrap(), "original");
}

#[cfg(all(feature = "coreutils", any(target_os = "linux", target_os = "macos")))]
fn paired(proof: &Confining, body: impl Fn(&std::path::Path) -> String, read: bool) {
    let tree = tempfile::tempdir().unwrap();
    let work = tree.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let inside = work.join("inside");
    let outside = tree.path().join("outside");
    if read {
        for path in [&inside, &outside] {
            std::fs::write(path, "ok\n").unwrap();
        }
    }
    // A write probe reads the tree: an unseen target is the Linux envelope's own memory.
    let readable = if read { work.as_path() } else { tree.path() };
    let launch = |path: &std::path::Path| {
        let caps = Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![FrozenPath::freeze(readable).unwrap()],
                write_prefixes: vec![FrozenPath::freeze(&work).unwrap()],
                deny_paths: Vec::new(),
            }),
            ..Capabilities::default()
        };
        run(stack([layer(caps)]), &body(path))
    };
    let control = launch(&inside);
    let refused = launch(&outside);
    denied(proof, &control, &refused, "ok");
    if !read {
        assert!(inside.exists(), "the control write did not land");
        assert!(!outside.exists(), "the out-of-grant write landed");
    }
}

#[cfg(all(feature = "coreutils", any(target_os = "linux", target_os = "macos")))]
#[test]
fn external_read_outside_grant_denied() {
    let proof = ral_core::require_confinement!();
    paired(&proof, |p| format!("/bin/cat '{}'", p.display()), true);
}

#[cfg(all(feature = "coreutils", any(target_os = "linux", target_os = "macos")))]
#[test]
fn a_system_reads_file_reads_alike_in_process_and_in_a_child() {
    let proof = ral_core::require_confinement!();
    let tree = tempfile::tempdir().unwrap();
    let caps = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![FrozenPath::freeze(tree.path()).unwrap()],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    };
    let grants = stack([layer(caps)]);
    let marker = std::fs::read_to_string("/etc/hosts").unwrap();
    for body in ["echo !{from-string < /etc/hosts}", "/bin/cat /etc/hosts"] {
        admitted(&proof, &run(grants.clone(), body), marker.trim());
    }
}

// Linux runs these through the real binary in ral/tests/bundled_confinement.rs.
#[cfg(all(feature = "coreutils", target_os = "macos"))]
#[test]
fn bundled_write_outside_grant_denied() {
    let proof = ral_core::require_confinement!();
    paired(
        &proof,
        |p| format!("touch '{}'; echo ok", p.display()),
        false,
    );
}

#[cfg(all(feature = "coreutils", target_os = "macos"))]
#[test]
fn bundled_pipeline_stage_write_outside_grant_denied() {
    let proof = ral_core::require_confinement!();
    paired(
        &proof,
        |p| format!("printf 'x\\n' | tee '{}'; echo ok", p.display()),
        false,
    );
}

#[cfg(all(feature = "coreutils", target_os = "macos"))]
#[test]
fn downstream_bundled_pipeline_stage_write_outside_grant_denied() {
    let proof = ral_core::require_confinement!();
    paired(
        &proof,
        |p| format!("printf 'x\\n' | cat | tee '{}'; echo ok", p.display()),
        false,
    );
}

#[cfg(all(feature = "coreutils", any(target_os = "linux", target_os = "macos")))]
#[test]
fn moved_exclusions_reach_children_through_the_identity_transport() {
    use ral_core::HostSurface;
    use ral_core::carrier::{IdentityTransport, dispatch_to_report};
    use ral_core::engine::EngineInstaller;
    use ral_core::io::terminal::TerminalState;
    use ral_core::path::EnginePath;
    use ral_core::protocol::{Attach, EngineSpec, Ran, Report, Root, Run};
    use ral_core::source::SourceOrigin;
    use std::sync::Arc;

    static ENGINES: [EngineInstaller; 1] = [EngineInstaller {
        tag: "bare",
        runtime: |_| HostSurface::default().runtime(TerminalState::default()),
        root: |_, _| Ok(()),
        narrow: |_, _| {
            Err(ral_core::error::Error::new(ral_core::fact::Reason::State(
                Box::new(ral_core::fact::StateFailure::NoChildEngines),
            )))
        },
    }];

    let _proof = ral_core::require_confinement!();
    let tree = tempfile::tempdir().unwrap();
    let work = tree.path().join("work");
    let secret = tree.path().join("secret");
    let directory = tree.path().join("private");
    let absent = tree.path().join("absent");
    for dir in [&work, &directory, &absent] {
        std::fs::create_dir(dir).unwrap();
    }
    std::fs::write(&secret, "SECRET-BYTES").unwrap();
    std::fs::write(directory.join("key"), "DIRECTORY-SECRET").unwrap();
    std::fs::write(work.join("ok"), "ALLOWED-BYTES").unwrap();
    let grants = stack([layer(Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![FrozenPath::freeze(&work).unwrap()],
            deny_paths: [&secret, &directory, &absent.join(".env")]
                .into_iter()
                .map(|p| FrozenPath::freeze(p).unwrap())
                .collect(),
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    })]);
    std::fs::copy(&secret, work.join("copy")).unwrap();
    std::fs::rename(&secret, work.join("x")).unwrap();
    std::fs::rename(&directory, work.join("private")).unwrap();
    std::fs::rename(&absent, work.join("absent")).unwrap();
    let transport = IdentityTransport::boot(
        &ENGINES,
        &Attach::root(
            EngineSpec::new("bare", EnginePath::Host(work.clone())),
            Root::new(EnginePath::Host(work)).with_grants(grants),
        ),
    )
    .unwrap();
    let dispatch = |body: &str| {
        let report = dispatch_to_report(
            &transport,
            Run::captured(body, SourceOrigin::named("<moved exclusion>")),
            Arc::new(()),
            None,
        )
        .unwrap();
        let Report::Ran(Ran {
            ending, captured, ..
        }) = report
        else {
            panic!("expected evaluation, got {report:?}");
        };
        (ending.code(), captured.unwrap())
    };
    let (status, output) = dispatch("/bin/cat ok copy; /bin/ls absent");
    assert_eq!(status, 0, "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout.bytes);
    assert!(stdout.contains("ALLOWED-BYTES"), "{output:?}");
    assert!(
        stdout.contains("SECRET-BYTES"),
        "a copy is a new object: {output:?}"
    );
    for (body, marker) in [
        ("/bin/cat x", "SECRET-BYTES"),
        ("/bin/cat private/key", "DIRECTORY-SECRET"),
    ] {
        let (_, output) = dispatch(body);
        assert!(
            !String::from_utf8_lossy(&output.stdout.bytes).contains(marker),
            "{output:?}"
        );
    }
}

/// The grant reads the tree and writes only `work`, so the refused write
/// meets a read-only view: an unshown path on Linux is the envelope's own
/// memory, where a write lands harmlessly and proves nothing.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn writes(proof: &Confining, wrap: impl Fn(&str) -> String, grant_body: bool) {
    let tree = tempfile::tempdir().unwrap();
    let work = tree.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let inside = work.join("inside");
    let outside = tree.path().join("outside");
    let caps = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![FrozenPath::freeze(tree.path()).unwrap()],
            write_prefixes: vec![FrozenPath::freeze(&work).unwrap()],
            deny_paths: Vec::new(),
        }),
        ..Capabilities::default()
    };
    let launch = |path: &std::path::Path| {
        let source = wrap(&format!(
            "/bin/sh -c 'echo ok > {} && echo ok'",
            path.display()
        ));
        if grant_body {
            run(
                stack([]),
                &format!(
                    "grant [fs: [read: [`absolute '{t}'], write: [`absolute '{w}']]] {{ {source} }}",
                    t = tree.path().display(),
                    w = work.display(),
                ),
            )
        } else {
            run(stack([layer(caps.clone())]), &source)
        }
    };
    let control = launch(&inside);
    let refused = launch(&outside);
    denied(proof, &control, &refused, "ok");
    assert_eq!(std::fs::read_to_string(inside).unwrap(), "ok\n");
    assert!(!outside.exists(), "the out-of-grant write landed");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn external_write_outside_grant_denied_at_top_level() {
    let proof = ral_core::require_confinement!();
    writes(&proof, str::to_owned, false);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn external_write_outside_grant_denied_in_block_body() {
    let proof = ral_core::require_confinement!();
    writes(&proof, str::to_owned, true);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn external_write_outside_grant_denied_in_spawn_body() {
    let proof = ral_core::require_confinement!();
    writes(
        &proof,
        |body| format!("let h = spawn {{ {body} }}; await $h"),
        false,
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn external_write_outside_grant_denied_in_par_task() {
    let proof = ral_core::require_confinement!();
    writes(
        &proof,
        |body| format!("!{{par {{ |x| {body}; return $x }} [1] 1}}"),
        false,
    );
}
