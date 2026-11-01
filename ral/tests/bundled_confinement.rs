#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(all(target_os = "linux", feature = "coreutils"))]

//! Bundled tools spawned by the real binary share its grant.  The Linux
//! statements of core's bundled-tool tests, which keeps macOS's.

mod common;

use common::{Output, Scratch, places};
use ral_core::test_helper::confinement::{Confining, admitted, denied};
use std::path::Path;

const LOADER_VARS: &[(&str, &str)] = &[
    ("LD_PRELOAD", "/nonexistent/ral-preload.so"),
    ("LD_AUDIT", "/nonexistent/ral-audit.so"),
    ("LD_LIBRARY_PATH", "/nonexistent/ral-libraries"),
];
const MARKER: &str = "RAL_LOADER_PROBE=present";
const NEUTRAL: &[&str] = &[
    "",
    "editor: [`read, `write, `tui]",
    "shell: [`chdir, `detach]",
    "editor: [`read, `write, `tui], shell: [`chdir, `detach]",
];

/// A write by `body` lands inside the granted dir and is refused outside it.
fn paired_write(proof: &Confining, body: impl Fn(&str) -> String) {
    let tree = Scratch::new("bundled_write");
    let work = tree.join("work");
    std::fs::create_dir(&work).unwrap();
    let (inside, outside) = (work.join("inside"), tree.join("outside"));
    let launch = |path: &Path| {
        common::run(
            "bundled_write",
            &format!(
                "grant [fs: [read: [{}], write: [{}]]] {{ {} }}",
                places(&[&tree.0]),
                places(&[&work]),
                body(&ral_core::path::shown(path))
            ),
        )
    };
    let control = launch(&inside);
    let refused = launch(&outside);
    denied(proof, &control, &refused, "ok");
    assert!(inside.exists(), "the control write did not land");
    assert!(!outside.exists(), "the out-of-grant write landed");
}

#[test]
fn bundled_write_outside_grant_denied() {
    let proof = ral_core::require_confinement!();
    paired_write(&proof, |p| format!("touch '{p}'; echo ok"));
}

#[test]
fn bundled_pipeline_stage_write_outside_grant_denied() {
    let proof = ral_core::require_confinement!();
    paired_write(&proof, |p| format!("printf 'x\\n' | tee '{p}'; echo ok"));
}

#[test]
fn downstream_bundled_pipeline_stage_write_outside_grant_denied() {
    let proof = ral_core::require_confinement!();
    paired_write(&proof, |p| {
        format!("printf 'x\\n' | cat | tee '{p}'; echo ok")
    });
}

/// `script` run with the loader overrides and a marker in ral's environment.
fn loaded(script: &str) -> Output {
    let envs = (LOADER_VARS.iter().copied())
        .chain([("RAL_LOADER_PROBE", "present")])
        .collect::<Vec<_>>();
    common::run_with_env("bundled_loader", &envs, script)
}

fn stripped(proof: &Confining, script: &str) -> Output {
    let out = loaded(script);
    admitted(proof, &out, MARKER);
    for (key, _) in LOADER_VARS {
        assert!(
            !out.stdout.contains(&format!("{key}=")),
            "loader override reached the confined child: {out:?}"
        );
    }
    out
}

#[test]
fn loader_overrides_never_reach_a_confined_child() {
    let proof = ral_core::require_confinement!();
    let dir = Scratch::new("bundled_loader");
    stripped(
        &proof,
        &format!("grant [fs: [read: [{}]]] {{ printenv }}", places(&[&dir.0])),
    );
}

#[test]
fn within_overrides_never_reach_a_confined_child() {
    let proof = ral_core::require_confinement!();
    let vars = (LOADER_VARS.iter())
        .map(|(key, value)| format!("{key}: '{value}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let out = stripped(
        &proof,
        &format!(
            "within [env: [{vars}, RAL_LOADER_WITHIN: 'present']] {{ grant [shell: [`chdir]] {{ printenv }} }}"
        ),
    );
    admitted(&proof, &out, "RAL_LOADER_WITHIN=present");
}

#[test]
fn a_withheld_verb_strips_loader_overrides() {
    let proof = ral_core::require_confinement!();
    stripped(&proof, "grant [shell: [`chdir]] { printenv }");
}

#[test]
fn neutral_layers_around_a_restriction_keep_stripping() {
    let proof = ral_core::require_confinement!();
    let dir = Scratch::new("bundled_loader");
    for neutral in NEUTRAL {
        stripped(
            &proof,
            &format!(
                "grant [{neutral}] {{ grant [fs: [read: [{}]]] {{ grant [{neutral}] {{ printenv }} }} }}",
                places(&[&dir.0]),
            ),
        );
    }
}

#[test]
fn a_neutral_policy_leaves_loader_overrides_alone() {
    let proof = ral_core::require_confinement!();
    for neutral in NEUTRAL {
        let out = loaded(&format!("grant [{neutral}] {{ printenv }}"));
        admitted(&proof, &out, MARKER);
        for (key, value) in LOADER_VARS {
            assert!(
                out.stdout.contains(&format!("{key}={value}")),
                "an ambient child lost a loader override: {out:?}"
            );
        }
    }
}
