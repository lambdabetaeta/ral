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
