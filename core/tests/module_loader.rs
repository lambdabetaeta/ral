#![allow(clippy::disallowed_methods)]

//! Behaviour of the `use` / `source` module loader.
//!
//! Both verbs route through `evaluate_source`, the shared guarded
//! parse + elaborate + evaluate core; they differ only in scope and
//! return shape.  These tests pin the observable contract at the public
//! evaluator API:
//!
//! - `use` re-reads the file on every call (no cache): editing a module
//!   between two `use`s of the same path yields the new value.
//! - a file that loads itself is rejected as a circular dependency, for
//!   both verbs (the cycle guard is load-bearing once the cache is gone).
//! - `use` projects the loaded scope into a Map; `source` leaks the
//!   file's bindings into the caller's scope.
//!
//! The harness mirrors `top_level_vs_block.rs`: bootstrap a `Shell` with
//! the prelude registered, then drive each source string through the
//! public `run_source_turn` door like a REPL turn would.

mod common;

use std::io::Write;

use ral_core::types::{Break, Capabilities, Settled, Shell};
use ral_core::{
    RequestedTerminalAccess, TurnIo, TurnReport, TurnRequest, TurnStdin, Value, builtins,
    diagnostic,
};

// ── Harness (same shape as `top_level_vs_block.rs`) ─────────────────────

fn fresh_shell() -> Shell {
    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());
    shell
}

/// Run one top-level turn of `source` through the public `run_source_turn` door
/// and return the body's `Settled<Value>`.  Every test below picks source
/// it expects to compile, so a static diagnostic is a test bug.
fn top_level(shell: &mut Shell, source: &str) -> Settled<Value> {
    match shell.run_source_turn(
        source,
        TurnRequest {
            script_name: "<test>",
            caps: Capabilities::root(),
            turn_limit: None,
            detached_limit: None,
            io: TurnIo::Inherit,
            terminal: RequestedTerminalAccess::Leased,
            stdin: TurnStdin::Inherit,
            surface: None,
            boundary: None,
            lifecycle: Box::new(()),
        },
    ) {
        TurnReport::Ran { result, .. } => result,
        TurnReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    }
}

/// Write `contents` to a fresh temp `.ral` file and return its path.  The
/// caller is responsible for removal; tests clean up at the end.
fn write_module(name: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(name);
    let mut f = std::fs::File::create(&path).expect("create temp module");
    f.write_all(contents.as_bytes()).expect("write temp module");
    path
}

// ── Cacheless re-`use` ──────────────────────────────────────────────────

/// `use` reads the file on every call: rewriting the module between two
/// `use`s of the same path surfaces the new value.  A result cache keyed
/// by canonical path (the previous design) would have served the stale
/// `answer = 1` on the second load.
#[test]
fn use_rereads_a_rewritten_module() {
    let path = write_module("ral_use_reread.ral", "let answer = 1\n");
    let p = path.to_string_lossy();
    let mut shell = fresh_shell();

    let first = top_level(&mut shell, &format!("use '{p}'")).expect("first use");
    assert_eq!(answer_of(&first), 1);

    std::fs::write(&path, "let answer = 2\n").expect("rewrite temp module");

    let second = top_level(&mut shell, &format!("use '{p}'")).expect("second use");
    assert_eq!(
        answer_of(&second),
        2,
        "second `use` must re-read, not serve a cache"
    );

    std::fs::remove_file(&path).ok();
}

/// Pull the `answer` field out of a module Map.
fn answer_of(m: &Value) -> i64 {
    match m {
        Value::Map(rec) => match rec.get("answer") {
            Some(Value::Int(n)) => *n,
            other => panic!("expected Int `answer`, got {other:?}"),
        },
        other => panic!("expected Map, got {other:?}"),
    }
}

// ── Cycle detection ─────────────────────────────────────────────────────

/// A module that `use`s itself is a cycle; the loader reports it rather
/// than recursing forever.  With no cache, the cycle guard is the only
/// thing keeping re-evaluation terminating.
#[test]
fn use_self_reference_is_a_cycle() {
    let path = write_module(
        "ral_use_cycle.ral",
        "let _ = use \"PLACEHOLDER\"\nreturn unit\n",
    );
    let p = path.to_string_lossy().into_owned();
    std::fs::write(&path, format!("let _ = use '{p}'\nreturn unit\n")).unwrap();

    let mut shell = fresh_shell();
    let err = top_level(&mut shell, &format!("use '{p}'")).unwrap_err();
    match err {
        Break::Error(e) => assert!(
            e.message.contains("circular dependency"),
            "expected circular dependency, got: {}",
            e.message
        ),
        other => panic!("expected Break::Error, got {other:?}"),
    }

    std::fs::remove_file(&path).ok();
}

/// A module that `source`s itself is likewise a cycle — both verbs share
/// the same guard in `evaluate_source`.
#[test]
fn source_self_reference_is_a_cycle() {
    let path = write_module("ral_source_cycle.ral", "return unit\n");
    let p = path.to_string_lossy().into_owned();
    std::fs::write(&path, format!("source '{p}'\n")).unwrap();

    let mut shell = fresh_shell();
    let err = top_level(&mut shell, &format!("source '{p}'")).unwrap_err();
    match err {
        Break::Error(e) => assert!(
            e.message.contains("circular dependency"),
            "expected circular dependency, got: {}",
            e.message
        ),
        other => panic!("expected Break::Error, got {other:?}"),
    }

    std::fs::remove_file(&path).ok();
}

// ── Scope / return semantics ────────────────────────────────────────────

/// `use` returns a Map of the loaded file's bindings without leaking them
/// into the caller's scope.
#[test]
fn use_returns_a_map_and_does_not_leak() {
    let path = write_module("ral_use_map.ral", "let x = 7\nlet y = 8\n");
    let p = path.to_string_lossy();
    let mut shell = fresh_shell();

    let m = top_level(&mut shell, &format!("use '{p}'")).expect("use");
    match m {
        Value::Map(rec) => {
            assert_eq!(rec.get("x"), Some(&Value::Int(7)));
            assert_eq!(rec.get("y"), Some(&Value::Int(8)));
        }
        other => panic!("expected Map, got {other:?}"),
    }
    assert!(shell.scope_lookup("x").is_none(), "`use` must not leak `x`");

    std::fs::remove_file(&path).ok();
}

/// `source` leaks the file's bindings into the caller's scope.
#[test]
fn source_leaks_bindings_into_caller_scope() {
    let path = write_module("ral_source_leak.ral", "let leaked = 5\n");
    let p = path.to_string_lossy();
    let mut shell = fresh_shell();

    top_level(&mut shell, &format!("source '{p}'")).expect("source");
    assert_eq!(
        shell.scope_lookup("leaked"),
        Some(&Value::Int(5)),
        "`source` must leak `leaked` into the caller's scope"
    );

    std::fs::remove_file(&path).ok();
}

// ── Cross-source diagnostics ─────────────────────────────────────────────

/// A runtime error raised inside a `source`d module renders its caret into
/// the module's own bytes, not the top-level script's.
///
/// The error's location carries the module's source identity; the renderer
/// resolves it against the session's `SourceDb` once at render time.  The
/// module's failing line has no counterpart in the top-level turn, so a
/// caret drawn against the top-level text (the dead-guard regression) would
/// land at unrelated bytes — this test pins that it lands in the module.
#[test]
fn sourced_module_runtime_error_points_into_module() {
    // The error is on line 3 of the module: a runtime division by zero,
    // which carries a source location (unlike a bare `fail`).  Line 3 has
    // no counterpart in the one-line top-level turn that sources it.
    let path = write_module(
        "ral_source_runtime_err.ral",
        "let a = 1\nlet z = 0\nreturn $[$a / $z]\n",
    );
    let p = path.to_string_lossy().into_owned();
    let mut shell = fresh_shell();
    shell.install_script_context("<top>".to_string(), &format!("source '{p}'"));

    let err = match top_level(&mut shell, &format!("source '{p}'")) {
        Err(Break::Error(e)) => e,
        other => panic!("expected a runtime error from the module, got {other:?}"),
    };
    assert!(
        err.message.contains("division by zero"),
        "expected the module's division-by-zero, got: {}",
        err.message
    );

    let rendered = diagnostic::format_runtime_error_auto(shell.sources(), &err, false);
    assert!(
        rendered.contains("ral_source_runtime_err.ral"),
        "the caret must be drawn against the module's source:\n{rendered}"
    );
    assert!(
        !rendered.contains("<top>"),
        "the top-level source must not appear:\n{rendered}"
    );
    // The caret must land on the module's failing line — line 3, where the
    // division lives. We pin it via the rendered location header
    // (`…ral_source_runtime_err.ral:3:…`) rather than the snippet text, which
    // ariadne lays out from the span and is sensitive to caret width.
    assert!(
        rendered.contains("ral_source_runtime_err.ral:3"),
        "the caret must point at the module's failing line (line 3):\n{rendered}"
    );

    std::fs::remove_file(&path).ok();
}
