#![allow(clippy::disallowed_methods, reason = "test code")]

//! What a top-level run leaves in the session across calls — the one thing an
//! in-script golden cannot observe — with and without an active sandbox
//! projection, plus the one `within [dir:]` exit the script that ran it cannot
//! outlive.  Block scoping, `cd` in blocks and the working-directory cell's
//! handler are goldens: `tests/lang/scoping.ral` and `tests/unix/cwd-blocks.ral`.
//!
//! The two-call harness mirrors what exarch's `shell_eval::run_shell`
//! does between consecutive tool calls and what the ral REPL's
//! `execute_input` does between consecutive prompt runs: hold a single
//! [`Shell`] across calls and route each body through the public
//! `run` entry, which checks against the live session before running.

mod common;

use common::fresh_shell;

use ral_core::capability::Capabilities;
#[cfg(unix)]
use ral_core::capability::FsPolicy;
use ral_core::path::shown;
use ral_core::protocol::Run;
use ral_core::source::SourceOrigin;
use ral_core::test_helper::{layer, stack};
use ral_core::types::{Settled, Shell};
use ral_core::{Break, Value};
use std::path::{Path, PathBuf};

// ── Harness ─────────────────────────────────────────────────────────────

/// Run one top-level run against `shell` through the public `run` entry under
/// `caps`, carried into the [`RunRequest`] exactly as exarch attenuates its
/// tool runs.  Returns whatever the body returned (or the body's error).  Every
/// test below picks source it expects to compile, so a static diagnostic is a
/// test bug.
fn top_level_under(shell: &mut Shell, caps: Capabilities, source: &str) -> Settled<Value> {
    shell
        .run(Run {
            caps: stack([layer(caps)]),
            ..Run::foreground(source, SourceOrigin::named("<test>"))
        })
        .expect_ran()
        .into_result()
}

fn top_level(shell: &mut Shell, source: &str) -> Settled<Value> {
    top_level_under(shell, Capabilities::default(), source)
}

/// Capability frame that actually triggers `sandbox_projection()` to
/// return `Some(_)`: any `fs` policy is enough.  Read prefix `/` makes
/// every read pass the in-process guard; the projection still goes through
/// the OS-sandbox machinery because `saw_fs` is true.
#[cfg(unix)]
fn projecting_caps() -> Capabilities {
    Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![ral_core::path::FrozenPath::root()],
            write_prefixes: vec![ral_core::path::FrozenPath::root()],
            deny_paths: Vec::new(),
        }),
        ..Capabilities::default()
    }
}

/// The frames under which top-level state semantics must agree: an active
/// projection changes where children are confined, never what a run leaves in
/// the session.
fn parity_caps() -> Vec<Capabilities> {
    #[cfg(unix)]
    return vec![Capabilities::default(), projecting_caps()];
    #[cfg(not(unix))]
    return vec![Capabilities::default()];
}

/// `path` as ral shows it (`/`-separated on every host, as `cwd` returns it),
/// without a trailing separator: `std::env::temp_dir()` may end in one
/// (`/tmp/`, `…\Temp\`), and `Shell::cwd()` — canonicalised — never does.
/// Re-collecting the components drops it and keeps the root itself.
fn shown_dir(path: &Path) -> String {
    shown(path.components().collect::<PathBuf>())
}

// ── (1) Top-level persistence ───────────────────────────────────────────

/// Two sequential top-level calls share state: a `let` from the first
/// call is visible in the second.  This is the load-bearing property of
/// the run's install-mobile-on-Ok rule, and the whole reason exarch's
/// tool-call harness routes through the top-level run entry instead of
/// the block boundary.
#[test]
fn top_level_let_persists_across_calls() {
    for caps in parity_caps() {
        let mut shell = fresh_shell();
        top_level_under(&mut shell, caps.clone(), "let persist_n = 41").expect("first run");
        let result =
            top_level_under(&mut shell, caps, "return $[$persist_n + 1]").expect("second run");
        assert_eq!(result, Value::Int(42));
    }
}

// ── (2) Top-level partial effects ───────────────────────────────────────

/// A top-level run that fails partway through still installs the
/// mobile mutations made before the failure.  This locks in the run's
/// install-on-Error rule: bindings made before a fatal command survive
/// into the next run; bindings *after* the failure do not exist because
/// that line never ran.
#[test]
fn top_level_partial_effects_persist_on_error() {
    for caps in parity_caps() {
        let mut shell = fresh_shell();
        // `cat /nonexistent` fails between the two `let` lines.  The whole
        // run returns Err, but the pre-failure binding is in the mobile,
        // which the top-level run installs unconditionally.
        let _ = top_level_under(
            &mut shell,
            caps,
            "let pre_fail_x = 1\ncat /nonexistent\nlet post_fail_y = 2",
        );
        assert!(
            shell.scope_lookup("pre_fail_x").is_some(),
            "pre-failure `let` must survive into the next run"
        );
        assert!(
            shell.scope_lookup("post_fail_y").is_none(),
            "post-failure `let` never ran, must not be present"
        );
    }
}

/// A destructuring bind is all-or-nothing even across the run entry.  The
/// outer pattern's first element matches and would stage `matched_x`; the
/// second fails.  Since a top-level run installs its mobile on error (the
/// sibling test above), a stage-as-you-recurse regression would leak
/// `matched_x` into the session — a half-destructured record visible at the
/// next prompt.
#[test]
fn top_level_partial_destructure_binds_nothing() {
    let mut shell = fresh_shell();
    let result = top_level(
        &mut shell,
        "let [[matched_x], [unmatched_a, unmatched_b]] = [[1], [2]]",
    );
    assert!(result.is_err(), "the inner pattern must fail to match");
    for name in ["matched_x", "unmatched_a", "unmatched_b"] {
        assert!(
            shell.scope_lookup(name).is_none(),
            "`{name}` must not survive a partially matched destructure"
        );
    }
}

// ── (3) Top-level cwd ───────────────────────────────────────────────────

/// `cd` in one top-level run is visible to subsequent runs: a later
/// `cwd` reflects the directory set by the earlier `cd`, the session
/// being the cell's outermost handler.
#[test]
fn top_level_cd_persists_across_calls() {
    for caps in parity_caps() {
        let mut shell = fresh_shell();
        let tmp = std::env::temp_dir();
        let tmp_disp = shown_dir(&tmp);
        top_level_under(&mut shell, caps.clone(), &format!("cd '{tmp_disp}'"))
            .expect("cd should succeed");
        let result = top_level_under(&mut shell, caps, "cwd").expect("cwd should succeed");
        // `cwd` returns the canonicalised path, shown; comparing shown forms
        // tolerates the macOS `/var` ↔ `/private/var` firmlink and the
        // Windows 8.3 short name `temp_dir()` may carry.
        let canon = shown_dir(&tmp.canonicalize().unwrap_or_else(|_| tmp.clone()));
        let got = match result {
            Value::String(s) => s.into_string(),
            other => panic!("cwd must return a String, got {other:?}"),
        };
        assert!(
            got == tmp_disp || got == canon,
            "cwd after cd: expected {tmp_disp:?} or {canon:?}, got {got:?}"
        );
    }
}

// ── (4) `within [dir:]` and `exit` ──────────────────────────────────────

/// An `exit` inside `within [dir:]` ends the run, and the session's cell is
/// still restored: a script cannot observe its own cwd after it has exited.
#[test]
fn within_dir_restores_the_cell_on_exit() {
    let root = tempfile::tempdir().unwrap();
    let outer = root.path().join("outer");
    let inner = root.path().join("inner");
    std::fs::create_dir(&outer).unwrap();
    std::fs::create_dir_all(inner.join("sub")).unwrap();
    let mut shell = fresh_shell();
    top_level(&mut shell, &format!("cd '{}'", outer.display())).expect("cd");
    let exited = top_level(
        &mut shell,
        &format!("within [dir: '{}'] {{ cd sub; exit 0 }}", inner.display()),
    );
    assert!(matches!(exited, Err(Break::Escape(_))), "got {exited:?}");
    assert_eq!(
        shell.cwd(),
        Path::new(&outer),
        "an exit must restore the cell"
    );
}
