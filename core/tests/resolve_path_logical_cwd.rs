#![allow(clippy::disallowed_methods)]

//! Regression: `resolve-path` honours the *logical* cwd, not the
//! process cwd.
//!
//! `ral`'s `cd` and `within [dir: …]` move only the shell-owned logical
//! cwd; the process cwd is never mutated (spawned threads would race
//! it).  `resolve-path 'rel'` must canonicalise the very same
//! logical-cwd-anchored path the capability gate authorised
//! (`<logical>/rel`), never `realpath(3)` the raw relative string
//! against the *process* cwd (which would return `<process>/rel`).
//! The path authorised is the path returned.

mod common;

use ral_core::types::{Capabilities, Shell, Value};
use ral_core::{
    RequestedTerminalAccess, TurnIo, TurnReport, TurnRequest, TurnStdin, builtins,
};

fn fresh_shell() -> Shell {
    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());
    shell
}

fn top_level(shell: &mut Shell, source: &str) -> Value {
    match shell.run_turn(
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
            lifecycle: Box::new(()),
        },
    ) {
        TurnReport::Ran { result, .. } => result.expect("evaluation succeeds"),
        TurnReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    }
}

/// `within [dir: X] { resolve-path 'leaf' }` returns a path under `X`
/// (the logical cwd), regardless of the process cwd.  We canonicalise
/// `X` ourselves to absorb any symlink the temp root sits behind (e.g.
/// `/tmp` → `/private/tmp` on macOS) so the assertion compares like for
/// like with what `resolve-path` returns.
#[test]
fn resolve_path_anchors_to_within_dir_not_process_cwd() {
    let root = std::env::temp_dir().join(format!("ral-resolve-logical-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("leaf"), b"x").unwrap();
    let canonical_root = std::fs::canonicalize(&root).unwrap();

    let mut shell = fresh_shell();
    let src = format!(
        "within [dir: '{}'] {{ resolve-path 'leaf' }}",
        root.to_string_lossy()
    );
    let out = match top_level(&mut shell, &src) {
        Value::String(s) => s,
        other => panic!("expected a String, got {other:?}"),
    };

    let expected = canonical_root.join("leaf");
    assert_eq!(
        std::path::Path::new(&out),
        expected,
        "resolve-path must return the logical-cwd-anchored path"
    );
    std::fs::remove_dir_all(&root).ok();
}
