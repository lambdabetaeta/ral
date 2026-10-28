#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(all(
    feature = "test-util",
    feature = "coreutils",
    any(target_os = "linux", target_os = "macos")
))]

//! Host and bundled children share the grant, including pipeline stages.

mod common;

use ral_core::authority::policy::{Capabilities, FsPolicy};
use ral_core::path::FrozenPath;
use ral_core::test_helper::confinement::{Confining, admitted, denied, run};
use ral_core::test_helper::{layer, stack};

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

#[test]
fn external_read_outside_grant_denied() {
    let proof = ral_core::require_confinement!();
    paired(&proof, |p| format!("/bin/cat '{}'", p.display()), true);
}

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

#[test]
fn bundled_write_outside_grant_denied() {
    let proof = ral_core::require_confinement!();
    paired(
        &proof,
        |p| format!("touch '{}'; echo ok", p.display()),
        false,
    );
}

#[test]
fn bundled_pipeline_stage_write_outside_grant_denied() {
    let proof = ral_core::require_confinement!();
    paired(
        &proof,
        |p| format!("printf 'x\\n' | tee '{}'; echo ok", p.display()),
        false,
    );
}

#[test]
fn downstream_bundled_pipeline_stage_write_outside_grant_denied() {
    let proof = ral_core::require_confinement!();
    paired(
        &proof,
        |p| format!("printf 'x\\n' | cat | tee '{}'; echo ok", p.display()),
        false,
    );
}

#[test]
fn moved_exclusions_reach_children_through_the_identity_transport() {
    use ral_core::HostSurface;
    use ral_core::carrier::{IdentityTransport, dispatch_to_report};
    use ral_core::engine::EngineInstaller;
    use ral_core::io::terminal::TerminalState;
    use ral_core::path::EnginePath;
    use ral_core::protocol::{Attach, EngineSpec, Report, Root, Run};
    use ral_core::source::SourceOrigin;
    use std::sync::Arc;

    static ENGINES: [EngineInstaller; 1] = [EngineInstaller {
        tag: "bare",
        runtime: |_| HostSurface::default().runtime(TerminalState::default()),
        root: |_, _| Ok(()),
        narrow: |_, _| Err("this engine makes no children".into()),
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
        let Report::Ran {
            ending, captured, ..
        } = report
        else {
            panic!("expected evaluation, got {report:?}");
        };
        (ending.status(), captured.unwrap())
    };
    let (status, output) = dispatch("/bin/cat ok copy; /bin/ls absent");
    assert_eq!(status, 0, "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
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
            !String::from_utf8_lossy(&output.stdout).contains(marker),
            "{output:?}"
        );
    }
}
