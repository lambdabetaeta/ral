#![allow(clippy::disallowed_methods, reason = "test code")]

//! The `use` module loader walks `RAL_PATH` past a directory named like the
//! module, and a file at the logical cwd wins over the walk.  The rest of the
//! loader's contract lives in `tests/lang/modules.ral`; this case stays here
//! because a script's relative `use` resolves against the script's own
//! directory, so the cwd-wins half is only reachable from a file-less run.

mod common;

use common::fresh_shell;

use ral_core::Value;
use ral_core::protocol::Run;
use ral_core::source::SourceOrigin;
use ral_core::types::{Settled, Shell};

/// Run one top-level run of `source` through the public `run` entry
/// and return the body's `Settled<Value>`.  Every test below picks source
/// it expects to compile, so a static diagnostic is a test bug.
fn top_level(shell: &mut Shell, source: &str) -> Settled<Value> {
    shell
        .run(Run::foreground(source, SourceOrigin::named("<test>")))
        .expect_ran()
        .into_result()
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

/// A relative `RAL_PATH` entry is read against the logical cwd, not the
/// process's: `modules/` exists only under `sub`.
#[test]
fn a_relative_ral_path_entry_is_read_against_the_logical_cwd() {
    let root = scratch("relative");
    let sub = root.join("sub");
    std::fs::create_dir_all(sub.join("modules")).unwrap();
    std::fs::write(sub.join("modules/m.ral"), "let answer = 9\n").unwrap();

    let source = format!(
        "within [dir: '{}', env: [RAL_PATH: 'modules']] {{ use 'm.ral' }}",
        sub.display()
    );
    let found = top_level(&mut fresh_shell(), &source).expect("the entry is read under `sub`");
    assert_eq!(answer_of(&found), 9);

    std::fs::remove_dir_all(&root).ok();
}

/// A `RAL_PATH` candidate is read through the grant, as the cwd's is: one the
/// grant refuses is passed over for the next, and where it refuses every one
/// the load fails, never reading the entry ambiently.
#[test]
fn ral_path_candidates_are_judged_for_reading() {
    let root = scratch("judged");
    let (d1, d2, here) = (root.join("d1"), root.join("d2"), root.join("here"));
    for dir in [&d1, &d2, &here] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(d1.join("m.ral"), "let answer = 1\n").unwrap();
    std::fs::write(d2.join("m.ral"), "let answer = 2\n").unwrap();
    let ral_path = std::env::join_paths([&d1, &d2])
        .expect("two ordinary scratch paths form RAL_PATH")
        .to_string_lossy()
        .into_owned();
    let under = |fs: String| {
        format!(
            "grant [fs: [{fs}]] {{ within [dir: '{}', env: [RAL_PATH: '{ral_path}']] {{ use 'm.ral' }} }}",
            here.display()
        )
    };
    let mut shell = fresh_shell();

    let denied = under(format!(
        "read: [`absolute '{}'], deny: [`absolute '{}']",
        root.display(),
        d1.join("m.ral").display()
    ));
    let found = top_level(&mut shell, &denied).expect("the next candidate is read");
    assert_eq!(answer_of(&found), 2, "a refused candidate is passed over");

    let narrow = under(format!("read: [`absolute '{}']", here.display()));
    let refused = top_level(&mut shell, &narrow).expect_err("a grant that reads neither entry");
    assert!(
        refused.to_string().contains(&d1.display().to_string()),
        "the error is the first candidate's refusal, not an absence: {refused}"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// The source of `use 'm.ral'` from `here` over `ral_path`, under a grant that
/// reads all of `root` but the local name `here/m.ral`, present or not: the
/// cwd is read, its candidate is not.
fn use_denying_the_local_name(
    root: &std::path::Path,
    here: &std::path::Path,
    ral_path: &str,
) -> String {
    format!(
        "grant [fs: [read: [`absolute '{}'], deny: [`absolute '{}']]] \
         {{ within [dir: '{}', env: [RAL_PATH: '{ral_path}']] {{ use 'm.ral' }} }}",
        root.display(),
        here.join("m.ral").display(),
        here.display()
    )
}

/// The name beside the script is the first candidate of one search, so an
/// absent one under a deny is passed over like a denied one.
#[test]
fn an_absent_local_name_under_a_deny_still_searches_ral_path() {
    let root = scratch("absent-denied");
    let (here, d1) = (root.join("here"), root.join("d1"));
    for dir in [&here, &d1] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(d1.join("m.ral"), "let answer = 1\n").unwrap();
    let source = use_denying_the_local_name(&root, &here, &d1.display().to_string());
    let found = top_level(&mut fresh_shell(), &source).expect("RAL_PATH is searched");
    assert_eq!(answer_of(&found), 1);

    std::fs::remove_dir_all(&root).ok();
}

/// A present local name the grant refuses yields to a `RAL_PATH` module, as a
/// denied `execvp` candidate does.
#[test]
fn a_denied_local_name_yields_to_ral_path() {
    let root = scratch("denied-yields");
    let (here, d1) = (root.join("here"), root.join("d1"));
    for dir in [&here, &d1] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(here.join("m.ral"), "let answer = 9\n").unwrap();
    std::fs::write(d1.join("m.ral"), "let answer = 1\n").unwrap();
    let source = use_denying_the_local_name(&root, &here, &d1.display().to_string());
    let found = top_level(&mut fresh_shell(), &source).expect("the library module is read");
    assert_eq!(answer_of(&found), 1);

    std::fs::remove_dir_all(&root).ok();
}

/// Where `RAL_PATH` has nothing, the local name's refusal is the error,
/// present or absent: the denial does not say which.
#[test]
fn a_denied_local_name_is_the_error_where_ral_path_has_nothing() {
    let root = scratch("denied-error");
    let here = root.join("here");
    std::fs::create_dir_all(&here).unwrap();
    let local = here.join("m.ral");
    let source = use_denying_the_local_name(&root, &here, "");
    let mut shell = fresh_shell();

    std::fs::write(&local, "let answer = 9\n").unwrap();
    let present = top_level(&mut shell, &source).expect_err("the local name is denied");
    assert!(
        present.to_string().contains(&local.display().to_string()),
        "{present}"
    );

    std::fs::remove_file(&local).unwrap();
    let absent = top_level(&mut shell, &source).expect_err("the absent name is denied alike");
    assert!(
        absent.to_string().contains(&local.display().to_string()),
        "{absent}"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// A denied library module and a denied local name: the error is the local
/// name's, whether or not it exists.
#[test]
fn a_denied_search_says_the_same_whether_or_not_the_local_name_exists() {
    let root = scratch("denied-both");
    let (here, lib) = (root.join("here"), root.join("lib"));
    for dir in [&here, &lib] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(lib.join("m.ral"), "let answer = 1\n").unwrap();
    let local = here.join("m.ral");
    let source = format!(
        "grant [fs: [read: [`absolute '{}'], deny: [`absolute '{}', `absolute '{}']]] \
         {{ within [dir: '{}', env: [RAL_PATH: '{}']] {{ use 'm.ral' }} }}",
        root.display(),
        local.display(),
        lib.join("m.ral").display(),
        here.display(),
        lib.display()
    );
    let mut shell = fresh_shell();

    std::fs::write(&local, "let answer = 9\n").unwrap();
    let present = top_level(&mut shell, &source).expect_err("both candidates are denied");
    std::fs::remove_file(&local).unwrap();
    let absent = top_level(&mut shell, &source).expect_err("both candidates are denied");
    assert_eq!(present.to_string(), absent.to_string());
    assert!(
        absent.to_string().contains(&local.display().to_string()),
        "{absent}"
    );

    std::fs::remove_dir_all(&root).ok();
}
