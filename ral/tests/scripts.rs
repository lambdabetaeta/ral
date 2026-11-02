#![allow(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::disallowed_macros,
    reason = "[test] libtest captures print macros"
)]

mod common;

use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// On Windows git checks goldens out with CRLF while `ral` emits LF; compare
/// modulo `\r` there, byte for byte elsewhere.
fn same_bytes(expected: &[u8], actual: &[u8]) -> bool {
    #[cfg(unix)]
    return expected == actual;
    #[cfg(not(unix))]
    return expected
        .iter()
        .filter(|&&b| b != b'\r')
        .eq(actual.iter().filter(|&&b| b != b'\r'));
}

/// A script that outlives this is hung, not slow.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(120);

/// The static corpora, run by `corpus.rs`: their programs are rejected or
/// merely checked, never run.
const CORPUS_DIRS: &[&str] = &["reject", "accept"];

/// Run a `.ral` script through the `ral` binary; `None` if it hung and was killed.
fn run_capture(path: &Path) -> Option<common::Output> {
    // Run as a subprocess to avoid dup2 redirect interference between tests.
    let child = common::ral_command()
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", path.display()));
    common::wait_with_timeout(child, SCRIPT_TIMEOUT)
}

/// Run every `.ral` script: it must exit 0 and reproduce its sibling
/// `<name>.out` on stdout byte for byte. `RAL_BLESS=1`
/// (re)writes the goldens from current output instead of comparing.
#[test]
fn scripts() {
    let bless = std::env::var_os("RAL_BLESS").is_some();
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests");
    let scripts = common::discover(&base, CORPUS_DIRS);
    assert!(
        !scripts.is_empty(),
        "no .ral test scripts found in {}",
        base.display()
    );

    let total = scripts.len();
    let (mut passed, mut blessed) = (0, 0);
    let mut failures = Vec::new();

    for script in &scripts {
        // Unix-specific scripts do not run elsewhere.
        #[cfg(not(unix))]
        if script.components().any(|c| c.as_os_str() == "unix") {
            continue;
        }

        let Some(output) = run_capture(script) else {
            failures.push(format!(
                "{}: no exit within {SCRIPT_TIMEOUT:?}; killed",
                script.display()
            ));
            continue;
        };
        if output.status != 0 {
            failures.push(format!(
                "{}: non-zero exit\n{}",
                script.display(),
                output.stderr
            ));
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
            Ok(expected) if same_bytes(&expected, output.stdout.as_bytes()) => passed += 1,
            Ok(expected) => failures.push(format!(
                "{}: stdout differs from {}\n--- golden ---\n{}\n--- actual ---\n{}",
                script.display(),
                golden.display(),
                String::from_utf8_lossy(&expected),
                output.stdout,
            )),
            Err(_) => failures.push(format!(
                "{}: missing golden {} — run `RAL_BLESS=1 cargo test -p ral --test scripts`",
                script.display(),
                golden.display(),
            )),
        }
    }

    eprintln!(
        "{passed} passed, {} failed, {blessed} blessed out of {total}",
        failures.len()
    );

    assert!(
        failures.is_empty(),
        "{} script(s) failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// A `.ral` source authored on Windows (Notepad, VS Code set to CRLF) has
/// `\r\n` line endings throughout — inside comments, between statements,
/// and inside a multi-line raw-string literal. This proves the lexer
/// treats CRLF as one logical newline everywhere, not just at top level:
/// it still finds the raw string's closing `'#` on the far side of a
/// CRLF-turned-LF newline, and the run reproduces LF-only output exactly.
///
/// Disk-loaded source is run through `normalize_source_text`
/// (core/src/source.rs) before lexing, which strips every `\r` —
/// including inside the raw string — so this integration path does not
/// observe CR *preservation*; the raw-string contract (a `\r` inside the
/// literal is carried into its value verbatim) is pinned directly against
/// un-normalized in-memory source by the lexer unit test
/// `bumped_string_multiline_preserves_embedded_cr`
/// (core/src/syntax/lexer.rs:2362-2374).
///
/// Synthesized here rather than checked into the repo as a real CRLF
/// file: with no `.gitattributes` override, `core.autocrlf` differs by
/// contributor and platform, so a checked-in CRLF fixture risks being
/// silently renormalized (or double-CR'd) on some future checkout.
/// Building the exact bytes at test time sidesteps that.
#[test]
fn crlf_source_lexes_and_runs() {
    let lf = "\
# CRLF fixture -- this comment, and every line below, is CRLF once converted.
let peek = 2
echo $[$peek + 1]

let greeting = \"hello\"
echo $greeting

# A raw string spanning the converted newline keeps its content verbatim
# once normalized (the lexer must still find the closing '# on the far
# side of it, even though normalization turned this into a plain LF).
echo #'raw
line'#
";
    let crlf = lf.replace('\n', "\r\n");
    assert!(
        !lf.contains('\r'),
        "fixture must be authored LF-only before the conversion"
    );

    let out = common::run("crlf-fixture", &crlf);
    assert_eq!(out.status, 0, "stderr:\n{}", out.stderr);
    assert_eq!(
        out.stdout.as_bytes(),
        b"3\nhello\nraw\nline\n",
        "normalized-CRLF run must reproduce exact LF-only output"
    );
}
