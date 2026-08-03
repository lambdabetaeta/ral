#![allow(clippy::disallowed_methods)]

//! `cwd:` resolves exactly once, at grant freeze.
//!
//! `ral_core::path::sigil`'s module doc states the security invariant: the
//! freeze-only sigils "resolve exactly once, so a later `chdir` or `$TMPDIR`
//! change cannot retroactively widen a grant".  A `cwd:` re-expanded at check
//! time would let a script inside `grant [fs: [read: ['cwd:']]]` walk to any
//! directory it can name and read there.

mod common;

use ral_core::transport::{Program, Run};
use ral_core::types::{Break, Capabilities, Settled, Shell, Value};
use ral_core::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, builtins};

fn top_level(source: &str) -> Settled<Value> {
    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());
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
        },
        surface: None,
        deferred: None,
        desk: None,
        nursery: None,
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran { result, .. } => result,
        RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    }
}

/// A directory holding one file `f` whose contents name it.
fn seeded(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ral-freeze-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("f"), tag).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// The grant froze `cwd:` to the directory in force at the `grant` — the
/// positive control, without which the denial below could be vacuous.
#[test]
fn cwd_sigil_freezes_to_the_directory_at_the_grant() {
    let a = seeded("a-pos");
    let out = top_level(&format!(
        "cd '{}'; grant [fs: [read: ['cwd:']]] {{ from-string < 'f' }}",
        a.display()
    ))
    .expect("a read inside the frozen directory must be granted");
    assert_eq!(out, Value::String("a-pos".into()));
    let _ = std::fs::remove_dir_all(&a);
}

/// A `cd` inside the granted block moves the logical cwd, but the frozen
/// prefix does not follow it: the read at the new anchor is denied.
#[test]
fn chdir_inside_the_block_cannot_widen_a_frozen_cwd_grant() {
    let (a, b) = (seeded("a-neg"), seeded("b-neg"));
    let err = top_level(&format!(
        "cd '{}'; grant [fs: [read: ['cwd:']]] {{ cd '{}'; from-string < 'f' }}",
        a.display(),
        b.display()
    ))
    .expect_err("reading outside the frozen cwd must be denied");
    let Break::Error(e) = err else {
        panic!("expected a denial error, got {err:?}");
    };
    assert_eq!(
        e.message,
        format!("fs read denied by grant: {}", b.join("f").display()),
        "the denial must be the grant's, and must name the path the widened read wanted"
    );
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}
