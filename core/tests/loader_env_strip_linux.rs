#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(all(feature = "test-util", feature = "coreutils", target_os = "linux"))]

//! Loader overrides a `within` sets never reach a confined child.

mod common;

use ral_core::test_helper::confinement::{admitted, run};
use ral_core::test_helper::stack;

const LOADER_VARS: &[&str] = &["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"];

#[test]
fn within_overrides_never_reach_a_confined_child() {
    let proof = ral_core::require_confinement!();
    let vars = LOADER_VARS
        .iter()
        .map(|key| format!("{key}: '/nonexistent/ral-loader'"))
        .collect::<Vec<_>>()
        .join(", ");
    let out = run(
        stack([]),
        &format!(
            "within [env: [{vars}, RAL_LOADER_WITHIN: 'present']] {{ grant [shell: [`chdir]] {{ /usr/bin/printenv }} }}"
        ),
    );
    admitted(&proof, &out, "RAL_LOADER_WITHIN=present");
    for key in LOADER_VARS {
        assert!(
            !out.stdout.contains(&format!("{key}=")),
            "loader override reached the confined child: {out:?}"
        );
    }
}
