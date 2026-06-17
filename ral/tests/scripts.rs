#![allow(clippy::disallowed_methods)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};

/// Scripts excluded from running at all — not because they're broken, but
/// because they can't reach a clean exit in the configuration the test builds.
/// Each reason is noted inline.
const RUN_SKIP: &[&str] = &[
    "args", // takes positional argv; not a self-contained script
];

/// Scripts that run cleanly (exit 0) but whose stdout is NOT golden-checked:
/// either a deferred output bug, or machine-specific / nondeterministic output.
const GOLDEN_SKIP: &[&str] = &[
    // non-portable / nondeterministic output
    "log-processor", // nondeterministic line counts between runs
    "devops",        // prints hostname / username
    "environment",   // prints $HOME
    "pipes",         // prints OS-specific `ls` error text
    "concurrency",   // `spawn { /bin/false }` — status differs by platform
                     // (`/bin/false` is 127 not-found on macOS, 1 on Linux)
];

/// Scripts whose regex builtins need the `grep` Cargo feature; skipped without
/// it. Note the prelude's `lines`/`words` are `re-split`-backed, so any script
/// using them is gated too (e.g. `stdlib` via `lines`).
#[cfg_attr(feature = "grep", allow(dead_code))]
const GREP_GATED: &[&str] = &[
    "split-regex",
    "strings",
    "batch-convert",
    "log-processor",
    "dual-input-strings",
    "filesystem",
    "stdlib",
];

/// Drop `\r` bytes so CRLF goldens (as git checks them out on Windows)
/// compare equal to ral's LF-only output.
fn strip_cr(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().copied().filter(|&b| b != b'\r').collect()
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

/// Run a `.ral` script through the `ral` binary and return its captured output.
fn run_capture(path: &Path) -> std::process::Output {
    // Run as a subprocess to avoid dup2 redirect interference between tests.
    std::process::Command::new(common::ral_bin())
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", path.display()))
}

/// Smoke-test the whole `.ral` corpus, and golden-check stdout for the portable,
/// deterministic subset. Every runnable script must exit 0; every goldened
/// script must reproduce its sibling `<name>.out` byte for byte. `RAL_BLESS=1`
/// (re)writes the goldens from current output instead of comparing — bless with
/// `--features grep,ripgrep` so the regex-backed scripts are captured too.
#[test]
fn scripts() {
    let bless = std::env::var_os("RAL_BLESS").is_some();
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests");
    let scripts = discover(&base);
    assert!(
        !scripts.is_empty(),
        "no .ral test scripts found in {}",
        base.display()
    );

    #[cfg(windows)]
    let windows_skips = [
        "glob",
        "json",
        "path-ops",
        "predicates",
        "stdin-redirect",
        "within",
        "capture-semantics",
        "indexing",
        "modules",
        "devops",
        "file-ops",
        "safety",
        "scripting",
        // Unix-only externals / filesystem layout: `/bin/echo`, coreutils
        // path and output assumptions that don't hold on Windows.
        "dual-input-strings",
        "filesystem",
        "stdlib",
        "batch-convert",
        "log-processor",
    ];

    let total = scripts.len();
    let (mut passed, mut skipped, mut blessed) = (0, 0, 0);
    let mut failures = Vec::new();

    for script in &scripts {
        let name = script.file_stem().unwrap().to_string_lossy();

        if RUN_SKIP.contains(&name.as_ref()) {
            skipped += 1;
            continue;
        }
        #[cfg(not(feature = "grep"))]
        if GREP_GATED.contains(&name.as_ref()) {
            skipped += 1;
            continue;
        }
        #[cfg(windows)]
        if windows_skips.contains(&name.as_ref()) {
            skipped += 1;
            continue;
        }
        // Skip Unix-specific scripts on non-Unix platforms.
        #[cfg(not(unix))]
        if script.components().any(|c| c.as_os_str() == "unix") {
            skipped += 1;
            continue;
        }

        let output = run_capture(script);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            failures.push(format!("{}: non-zero exit\n{stderr}", script.display()));
            continue;
        }
        if GOLDEN_SKIP.contains(&name.as_ref()) {
            passed += 1; // smoke-tested for exit 0; stdout intentionally not goldened
            continue;
        }

        let golden = script.with_extension("out");
        if bless {
            fs::write(&golden, &output.stdout)
                .unwrap_or_else(|e| panic!("bless write {}: {e}", golden.display()));
            blessed += 1;
            continue;
        }
        match fs::read(&golden) {
            // Compare line-ending-insensitively: on Windows git checks the
            // golden `.out` files out with CRLF, while `ral` always emits LF,
            // so a raw byte compare would spuriously differ on every script.
            Ok(expected) if strip_cr(&expected) == strip_cr(&output.stdout) => passed += 1,
            Ok(expected) => failures.push(format!(
                "{}: stdout differs from {}\n--- golden ---\n{}\n--- actual ---\n{}",
                script.display(),
                golden.display(),
                String::from_utf8_lossy(&expected),
                String::from_utf8_lossy(&output.stdout),
            )),
            Err(_) => failures.push(format!(
                "{}: missing golden {} — run `RAL_BLESS=1 cargo test -p ral --features grep,ripgrep --test scripts`",
                script.display(),
                golden.display(),
            )),
        }
    }

    eprintln!(
        "{passed} passed, {} failed, {skipped} skipped, {blessed} blessed out of {total}",
        failures.len()
    );

    if !failures.is_empty() {
        panic!(
            "{} script(s) failed:\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }
}
