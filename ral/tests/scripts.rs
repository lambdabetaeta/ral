mod common;

use std::fs;
use std::path::{Path, PathBuf};

fn run_script(path: &Path) {
    // Run as a subprocess to avoid dup2 redirect interference between tests.
    let output = std::process::Command::new(common::ral_bin())
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", path.display()));
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("script failed in {}:\n{stderr}", path.display());
    }
}

fn discover(dir: &Path) -> Vec<PathBuf> {
    let mut scripts = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                scripts.extend(discover(&path));
            } else if path.extension().is_some_and(|e| e == "ral") {
                scripts.push(path);
            }
        }
    }
    scripts.sort();
    scripts
}

#[test]
fn scripts() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests");
    let scripts = discover(&base);
    assert!(
        !scripts.is_empty(),
        "no .ral test scripts found in {}",
        base.display()
    );

    let total = scripts.len();
    let mut passed = 0;
    let mut skipped = 0;
    let mut failures = Vec::new();

    for script in &scripts {
        let name = script.file_stem().unwrap().to_string_lossy();
        if name == "args" {
            skipped += 1;
            continue;
        }
        // grep-files is only available with the `grep` Cargo feature.
        #[cfg(not(feature = "grep"))]
        if name == "grep-files" {
            skipped += 1;
            continue;
        }
        // Skip Unix-specific scripts on non-Unix platforms.
        #[cfg(not(unix))]
        if script.components().any(|c| c.as_os_str() == "unix") {
            skipped += 1;
            continue;
        }
        let result = std::panic::catch_unwind(|| run_script(script));
        if result.is_ok() {
            passed += 1;
        } else {
            failures.push(script.display().to_string());
        }
    }

    eprintln!(
        "{passed} passed, {} failed, {skipped} skipped out of {total}",
        failures.len()
    );

    if !failures.is_empty() {
        panic!(
            "{} script(s) failed:\n  {}",
            failures.len(),
            failures.join("\n  ")
        );
    }
}
