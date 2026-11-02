#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(all(feature = "test-util", any(target_os = "linux", target_os = "macos")))]

//! The same write is confined through each evaluation context.
//!
//! The grant reads the tree and writes only `work`, so the refused write
//! meets a read-only view: an unshown path on Linux is the envelope's own
//! memory, where a write lands harmlessly and proves nothing.

mod common;

use ral_core::authority::policy::{Capabilities, FsPolicy};
use ral_core::path::FrozenPath;
use ral_core::test_helper::confinement::{Confining, denied, run};
use ral_core::test_helper::{layer, stack};

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

#[test]
fn external_write_outside_grant_denied_at_top_level() {
    let proof = ral_core::require_confinement!();
    writes(&proof, str::to_owned, false);
}

#[test]
fn external_write_outside_grant_denied_in_block_body() {
    let proof = ral_core::require_confinement!();
    writes(&proof, str::to_owned, true);
}

#[test]
fn external_write_outside_grant_denied_in_spawn_body() {
    let proof = ral_core::require_confinement!();
    writes(
        &proof,
        |body| format!("let h = spawn {{ {body} }}; await $h"),
        false,
    );
}

#[test]
fn external_write_outside_grant_denied_in_par_task() {
    let proof = ral_core::require_confinement!();
    writes(
        &proof,
        |body| format!("!{{par {{ |x| {body}; return $x }} [1] 1}}"),
        false,
    );
}
