#![allow(clippy::disallowed_methods)]

//! Behaviour of the `use` / `source` module loader.
//!
//! Both verbs route through `evaluate_source`, the shared guarded
//! parse + elaborate + evaluate core; they differ only in scope and
//! return shape.  These tests pin the observable contract at the public
//! evaluator API:
//!
//! - a file that loads itself is rejected as a circular dependency, for
//!   both verbs — nothing else keeps re-evaluation terminating.
//! - a module's failure reaches the caller with its own status, named once
//!   by the verb that loaded it.
//! - `use` projects the loaded scope into a Map; `source` leaks the
//!   file's bindings into the caller's scope.
//!
//! The harness mirrors `top_level_vs_block.rs`: bootstrap a `Shell` with
//! the prelude registered, then drive each source string through the
//! public `run` door like a REPL run would.

mod common;

use std::io::Write;

use ral_core::transport::{Program, Run};
use ral_core::types::{Break, Capabilities, Settled, Shell, Status};
use ral_core::{
    RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, Value, builtins, diagnostic,
};

// ── Harness (same shape as `top_level_vs_block.rs`) ─────────────────────

fn fresh_shell() -> Shell {
    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());
    shell
}

/// Run one top-level run of `source` through the public `run` door
/// and return the body's `Settled<Value>`.  Every test below picks source
/// it expects to compile, so a static diagnostic is a test bug.
fn top_level(shell: &mut Shell, source: &str) -> Settled<Value> {
    match shell.run(RunRequest {
        run: Run {
            program: Program::Source(source.into()),
            script_name: "<test>".into(),
            caps: Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Inherit,
            terminal: RequestedTerminalAccess::Leased,
            stdin: RunStdin::Inherit,
            trail: None,
        },
        surface: None,
        deferred: None,
        desk: None,
        fork: None,
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran { ending, .. } => ending.into_result(),
        RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
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
        "let _ = use \"PLACEHOLDER\"\nreturn ()\n",
    );
    let p = path.to_string_lossy().into_owned();
    std::fs::write(&path, format!("let _ = use '{p}'\nreturn ()\n")).unwrap();

    let mut shell = fresh_shell();
    let err = top_level(&mut shell, &format!("use '{p}'")).unwrap_err();
    match err {
        Break::Error(e) => assert!(
            e.message.contains("circular dependency"),
            "expected circular dependency, got: {}",
            e.message
        ),
        other @ Break::Escape(_) => panic!("expected Break::Error, got {other:?}"),
    }

    std::fs::remove_file(&path).ok();
}

/// A module that `source`s itself is likewise a cycle — both verbs share
/// the same guard in `evaluate_source`.
#[test]
fn source_self_reference_is_a_cycle() {
    let path = write_module("ral_source_cycle.ral", "return ()\n");
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
        other @ Break::Escape(_) => panic!("expected Break::Error, got {other:?}"),
    }

    std::fs::remove_file(&path).ok();
}

// ── Failure propagation ─────────────────────────────────────────────────

/// The loader names itself on a module's failure without rewriting it: the
/// status the module chose survives, and two nested loads do not stack the
/// prefix.  Re-signalling (`sig(format!("source: {e}"))`) would flatten the
/// 7 to a 1 and read `source: source: boom`.
#[test]
fn a_sourced_failure_keeps_its_status_and_is_tagged_once() {
    let inner = write_module("ral_fail_inner.ral", "fail [status: 7, message: 'boom']\n");
    let i = inner.to_string_lossy().into_owned();
    let outer = write_module("ral_fail_outer.ral", &format!("source '{i}'\n"));
    let o = outer.to_string_lossy().into_owned();

    let mut shell = fresh_shell();
    let e = match top_level(&mut shell, &format!("source '{o}'")) {
        Err(Break::Error(e)) => e,
        other => panic!("expected the module's failure, got {other:?}"),
    };
    assert_eq!(e.status, Status::Code(7), "the module's status survives");
    assert_eq!(e.message, "source: boom");
    assert_eq!(
        e.message.matches("source: ").count(),
        1,
        "two nested loads must not stack the prefix"
    );

    // `use` tags with its own verb, from the same guard.
    let e = match top_level(&mut shell, &format!("use '{i}'")) {
        Err(Break::Error(e)) => e,
        other => panic!("expected the module's failure, got {other:?}"),
    };
    assert_eq!(e.status, Status::Code(7));
    assert_eq!(e.message, "use: boom");

    std::fs::remove_file(&inner).ok();
    std::fs::remove_file(&outer).ok();
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

// ── `RAL_PATH` fallback ─────────────────────────────────────────────────

/// A fresh directory under the system temp dir, keyed on the pid.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ral-modpath-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `use` of a bare name that the logical cwd cannot supply falls through to
/// a `RAL_PATH` walk — reading the `within [env: …]` overlay, not the host
/// env — and a *directory* bearing the module's name on an earlier entry
/// does not end that walk.  Planting a real file in the cwd afterwards
/// flips the answer, pinning which anchor wins.
#[test]
fn use_falls_through_to_ral_path_and_the_cwd_wins_when_it_can() {
    let root = scratch("walk");
    let (d1, d2, here) = (root.join("d1"), root.join("d2"), root.join("here"));
    std::fs::create_dir_all(d1.join("m.ral")).unwrap();
    std::fs::create_dir_all(&d2).unwrap();
    std::fs::create_dir_all(&here).unwrap();
    std::fs::write(d2.join("m.ral"), "let answer = 42\n").unwrap();

    let ral_path = std::env::join_paths([&d1, &d2])
        .expect("two ordinary scratch paths form RAL_PATH")
        .to_string_lossy()
        .into_owned();
    let source = format!(
        "within [dir: '{}', env: [RAL_PATH: '{}']] {{ use 'm.ral' }}",
        here.display(),
        ral_path
    );
    let mut shell = fresh_shell();
    let found = top_level(&mut shell, &source).expect("RAL_PATH walk finds the module");
    assert_eq!(
        answer_of(&found),
        42,
        "a directory named `m.ral` must not end the walk"
    );

    std::fs::write(here.join("m.ral"), "let answer = 7\n").unwrap();
    let shadowed = top_level(&mut shell, &source).expect("cwd-relative module loads");
    assert_eq!(
        answer_of(&shadowed),
        7,
        "a real file at the logical cwd must beat the RAL_PATH walk"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// Discovery does not widen a grant: the module the `RAL_PATH` walk found is
/// still read through `check_fs_read`, so a read set naming only a sibling
/// directory denies the load.
#[test]
fn a_ral_path_find_still_answers_to_the_fs_read_grant() {
    let root = scratch("grant");
    let (lib, other) = (root.join("lib"), root.join("other"));
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(lib.join("m.ral"), "let answer = 42\n").unwrap();

    let mut shell = fresh_shell();
    let source = format!(
        "grant [fs: [read: ['{o}']]] {{ within [dir: '{o}', env: [RAL_PATH: '{l}']] {{ use 'm.ral' }} }}",
        o = other.display(),
        l = lib.display()
    );
    match top_level(&mut shell, &source).unwrap_err() {
        Break::Error(e) => assert!(
            e.message.contains("denied by grant"),
            "expected an fs-grant denial, got: {}",
            e.message
        ),
        other @ Break::Escape(_) => panic!("expected Break::Error, got {other:?}"),
    }

    std::fs::remove_dir_all(&root).ok();
}

// ── Cross-source diagnostics ─────────────────────────────────────────────

/// A runtime error raised inside a `source`d module renders its caret into
/// the module's own bytes, not the top-level script's.
///
/// The error's location carries the module's source identity; the renderer
/// resolves it against the session's `SourceDb` once at render time.  The
/// module's failing line has no counterpart in the top-level run, so a
/// caret drawn against the top-level text (the dead-guard regression) would
/// land at unrelated bytes — this test pins that it lands in the module.
#[test]
fn sourced_module_runtime_error_points_into_module() {
    // The error is on line 3 of the module: a runtime division by zero,
    // which carries a source location (unlike a bare `fail`).  Line 3 has
    // no counterpart in the one-line top-level run that sources it.
    let path = write_module(
        "ral_source_runtime_err.ral",
        "let a = 1\nlet z = 0\nreturn $[$a / $z]\n",
    );
    let p = path.to_string_lossy().into_owned();
    let mut shell = fresh_shell();

    let err = match top_level(&mut shell, &format!("source '{p}'")) {
        Err(Break::Error(e)) => e,
        other => panic!("expected a runtime error from the module, got {other:?}"),
    };
    assert!(
        err.message.contains("division by zero"),
        "expected the module's division-by-zero, got: {}",
        err.message
    );

    let rendered = diagnostic::format_runtime_error_auto(shell.sources(), &err, None);
    assert!(
        rendered.contains("ral_source_runtime_err.ral"),
        "the caret must be drawn against the module's source:\n{rendered}"
    );
    assert!(
        !rendered.contains("<test>"),
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
