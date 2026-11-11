//! Loader overrides, whether set after the inert parent has started or by a
//! `within`, never reach a confined child.

#[cfg(all(feature = "coreutils", target_os = "macos"))]
use ral_core::test_helper::confinement::{Confining, Output};
#[cfg(all(feature = "coreutils", any(target_os = "linux", target_os = "macos")))]
use ral_core::test_helper::confinement::{admitted, run};
#[cfg(all(feature = "coreutils", any(target_os = "linux", target_os = "macos")))]
use ral_core::test_helper::stack;
#[cfg(all(feature = "coreutils", target_os = "macos"))]
use std::io::Write;

#[cfg(all(feature = "coreutils", target_os = "macos"))]
const LOADER_VARS: &[(&str, &str)] = &[
    ("DYLD_INSERT_LIBRARIES", "/usr/lib/libSystem.B.dylib"),
    ("DYLD_LIBRARY_PATH", "/nonexistent/ral-libraries"),
];
#[cfg(all(feature = "coreutils", target_os = "macos"))]
const MARKER: &str = "RAL_LOADER_PROBE=present";
#[cfg(all(feature = "coreutils", target_os = "macos"))]
const FULL_VERBS: &str = "editor: [`read, `write, `tui], shell: [`chdir, `detach]";

#[cfg(all(feature = "coreutils", target_os = "macos"))]
pub(crate) fn fixture() {
    let Some(source) = std::env::var_os("RAL_LOADER_PARENT") else {
        return;
    };
    // SAFETY: this fixture runs before libtest starts any threads.
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

#[cfg(all(feature = "coreutils", target_os = "macos"))]
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

#[cfg(all(feature = "coreutils", target_os = "macos"))]
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

#[cfg(all(feature = "coreutils", target_os = "macos"))]
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

#[cfg(all(feature = "coreutils", target_os = "macos"))]
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

#[cfg(all(feature = "coreutils", target_os = "macos"))]
#[test]
fn a_withheld_verb_strips_loader_overrides() {
    let proof = ral_core::require_confinement!();
    stripped(&proof, "grant [shell: [`chdir]] { printenv }");
}

#[cfg(all(feature = "coreutils", target_os = "macos"))]
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

#[cfg(all(feature = "coreutils", target_os = "macos"))]
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

#[cfg(all(feature = "coreutils", target_os = "linux"))]
const LOADER_VARS: &[&str] = &["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"];

#[cfg(all(feature = "coreutils", target_os = "linux"))]
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
