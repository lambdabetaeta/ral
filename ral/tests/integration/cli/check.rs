//! `--check` agrees with a real run.

use std::time::Duration;

// ── batch honesty: `--check` sees exactly what `run` will run ────────────
//
// `--check`'s seed table is core plus `WATCH_BUILTIN` — exactly what the
// batch shell installs (`ral/src/batch.rs`), never the REPL-only editor
// surface (`ral::repl::plugin::ed_builtins::ED_BUILTINS`).  So `_ed-insert`
// resolves as an external command to the checker, not the real builtin's
// `String → F[∅] Unit` scheme: piping its result into `from-json` (whose
// input mode is ground `Bytes`) only typechecks under the external
// reading.  `--check` and a real `run` must therefore agree — both treat
// `_ed-insert` as unknown, and `run` fails only where an external
// resolution actually fails, at the PATH lookup, never with a static type
// error `--check` would have caught first.

const ED_INSERT_PIPED_TO_FROM_JSON: &str = r#"_ed-insert "hi" | from-json"#;

/// `--check` typechecks `_ed-insert` as external, not as the REPL's `_ed-*`
/// scheme — the mode that would clash with `from-json`'s ground `Bytes`
/// input never gets a chance to.
#[test]
fn check_typechecks_ed_insert_as_external_in_batch() {
    let r = crate::common::run_with_timeout(
        "batch_check_ed_insert",
        &["--check"],
        ED_INSERT_PIPED_TO_FROM_JSON,
        Duration::from_secs(10),
    )
    .expect("ral --check must not hang");
    assert_eq!(
        r.status, 0,
        "expected `--check` to typecheck _ed-insert as an external command; stderr was:\n{}",
        r.stderr
    );
}

/// A real `run` of the same script agrees with `--check`: it clears
/// typechecking (no type error ever reaches stderr) and fails only at the
/// external-exec boundary, because the batch shell never installed
/// `_ed-insert` either.
#[test]
fn run_agrees_ed_insert_is_external_in_batch() {
    let r = crate::common::run_with_timeout(
        "batch_run_ed_insert",
        &[],
        ED_INSERT_PIPED_TO_FROM_JSON,
        Duration::from_secs(10),
    )
    .expect("ral must not hang");
    assert_ne!(r.status, 0, "an unresolvable external command must fail");
    assert!(
        r.stderr.contains("_ed-insert: command not found"),
        "expected an external-exec failure, not a type error; stderr was:\n{}",
        r.stderr
    );
}
