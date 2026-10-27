#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(all(
    feature = "test-util",
    feature = "coreutils",
    any(target_os = "linux", target_os = "macos")
))]

//! Loader overrides are set only after the inert parent has started.

mod common;

use ral_core::test_helper::confinement::{Confining, Output, admitted, run};
use ral_core::test_helper::stack;
use std::io::Write;

#[cfg(target_os = "linux")]
const LOADER_VARS: &[(&str, &str)] = &[
    ("LD_PRELOAD", "/nonexistent/ral-preload.so"),
    ("LD_AUDIT", "/nonexistent/ral-audit.so"),
    ("LD_LIBRARY_PATH", "/nonexistent/ral-libraries"),
];
#[cfg(target_os = "macos")]
const LOADER_VARS: &[(&str, &str)] = &[
    ("DYLD_INSERT_LIBRARIES", "/usr/lib/libSystem.B.dylib"),
    ("DYLD_LIBRARY_PATH", "/nonexistent/ral-libraries"),
];
const MARKER: &str = "RAL_LOADER_PROBE=present";
const FULL_VERBS: &str = "editor: [`read, `write, `tui], shell: [`chdir, `detach]";

#[ctor::ctor(unsafe)]
fn loader_parent() {
    let Some(source) = std::env::var_os("RAL_LOADER_PARENT") else {
        return;
    };
    if let Some(code) = ral_core::invocation::serve_process_before_main(&[]) {
        std::process::exit(i32::from(code));
    }
    // SAFETY: this constructor runs before libtest starts any threads.
    unsafe {
        std::env::remove_var("RAL_LOADER_PARENT");
        for (key, value) in LOADER_VARS {
            std::env::set_var(key, value);
        }
        std::env::set_var("RAL_LOADER_PROBE", "present");
    }
    let out = run(stack([]), source.to_str().expect("a UTF-8 probe"));
    std::io::stdout().write_all(out.stdout.as_bytes()).unwrap();
    std::io::stderr().write_all(out.stderr.as_bytes()).unwrap();
    std::process::exit(out.status);
}

fn child_env(source: &str) -> Output {
    let mut parent = std::process::Command::new(std::env::current_exe().unwrap());
    parent.env("RAL_LOADER_PARENT", source);
    for (key, _) in LOADER_VARS {
        parent.env_remove(key);
    }
    parent
        .output()
        .expect("the inert loader parent starts")
        .into()
}

fn stripped(proof: &Confining, source: &str) -> Output {
    let out = child_env(source);
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
    let dir = tempfile::tempdir().unwrap();
    stripped(
        &proof,
        &format!(
            "grant [fs: [read: [`absolute '{}']]] {{ printenv }}",
            dir.path().display()
        ),
    );
}

#[test]
fn within_overrides_never_reach_a_confined_child() {
    let proof = ral_core::require_confinement!();
    let vars = LOADER_VARS
        .iter()
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
    let dir = tempfile::tempdir().unwrap();
    for neutral in [
        "",
        "editor: [`read, `write, `tui]",
        "shell: [`chdir, `detach]",
        FULL_VERBS,
    ] {
        stripped(
            &proof,
            &format!(
                "grant [{neutral}] {{ grant [fs: [read: [`absolute '{}']]] {{ grant [{neutral}] {{ printenv }} }} }}",
                dir.path().display(),
            ),
        );
    }
}

#[test]
fn a_neutral_policy_leaves_loader_overrides_alone() {
    let proof = ral_core::require_confinement!();
    for neutral in [
        "",
        "editor: [`read, `write, `tui]",
        "shell: [`chdir, `detach]",
        FULL_VERBS,
    ] {
        let out = child_env(&format!("grant [{neutral}] {{ printenv }}"));
        admitted(&proof, &out, MARKER);
        for (key, value) in LOADER_VARS {
            assert!(
                out.stdout.contains(&format!("{key}={value}")),
                "an ambient child lost a loader override: {out:?}"
            );
        }
    }
}
