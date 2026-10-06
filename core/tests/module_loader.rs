#![allow(clippy::disallowed_methods)]

//! The `use` module loader walks `RAL_PATH` past a directory named like the
//! module, and a file at the logical cwd wins over the walk.  The rest of the
//! loader's contract lives in `tests/lang/modules.ral`; this case stays here
//! because a script's relative `use` resolves against the script's own
//! directory, so the cwd-wins half is only reachable from a file-less run.

mod common;

use common::fresh_shell;

use ral_core::Value;
use ral_core::protocol::Run;
use ral_core::run::RunReport;
use ral_core::types::{Settled, Shell};

/// Run one top-level run of `source` through the public `run` door
/// and return the body's `Settled<Value>`.  Every test below picks source
/// it expects to compile, so a static diagnostic is a test bug.
fn top_level(shell: &mut Shell, source: &str) -> Settled<Value> {
    match shell.run(Run::foreground(source, "<test>")) {
        RunReport::Ran { ending, .. } => ending.into_result(),
        RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    }
}

/// Pull the `answer` field out of a module Map.
fn answer_of(m: &Value) -> i64 {
    match m {
        Value::Map(rec) => match rec.get("answer").as_deref() {
            Some(Value::Int(n)) => *n,
            other => panic!("expected Int `answer`, got {other:?}"),
        },
        other => panic!("expected Map, got {other:?}"),
    }
}

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
