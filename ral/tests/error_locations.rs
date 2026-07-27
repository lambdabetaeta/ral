//! A runtime error says where it happened, even when the value that raised
//! it was compiled by an earlier run.
//!
//! An alias, a `source`d function, a lambda bound at the prompt: each is a
//! live value in the next run, carrying spans the run before it minted.  The
//! source registry only grows, so those spans still resolve — and the caret
//! is drawn wherever they point, which is the whole answer when the text is
//! not the text the user just typed.

#![allow(clippy::disallowed_methods)]

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Boot `ral -i` with its config isolated to `config`, feeding `line` as the
/// REPL's whole stdin.  Returns stderr.  `--norc` is left to the caller: an
/// isolated but rc-less config dir and an isolated rc are both wanted below.
fn repl_stderr(config: &Path, args: &[&str], line: &str) -> String {
    let mut child = Command::new(common::ral_bin())
        .args(args)
        .arg("-i")
        .env("HOME", config)
        .env("XDG_CONFIG_HOME", config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(line.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The reported bug: `boom` at the prompt is one command, so the compact
/// one-liner used to be all the user got — but the fault is inside the rc,
/// where only a caret can point.
#[test]
fn an_rc_alias_faults_against_the_rc_file() {
    let dir = common::fresh_tmp_path("ral_rc_alias_fault", "d");
    std::fs::create_dir_all(dir.join("ral")).unwrap();
    std::fs::write(
        dir.join("ral").join("rc"),
        "[\n  aliases: [ boom: { |args| $undefined_name } ]\n]\n",
    )
    .unwrap();
    let stderr = repl_stderr(&dir, &[], "boom\n");
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        stderr.contains("ral/rc:2:") && stderr.contains("$undefined_name }"),
        "the caret must name the rc file, at the alias body's line; stderr was:\n{stderr}"
    );
}

/// A `source`d file's function, called at the prompt, names that file — not
/// the prompt line, and not nothing.
#[test]
fn a_sourced_function_faults_against_its_own_file() {
    let dir = common::fresh_tmp_path("ral_sourced_fault", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let module = dir.join("mod.ral");
    std::fs::write(&module, "let boom = { |x| $undefined_name }\n").unwrap();

    let stderr = repl_stderr(
        &dir,
        &["--norc"],
        &format!("source '{}'\n$boom 1\n", module.display()),
    );
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        stderr.contains("mod.ral:1:") && stderr.contains("$undefined_name }"),
        "the caret must name the sourced file; stderr was:\n{stderr}"
    );
}

/// A bare external command that is not found still renders compact: the
/// fault is the command itself, and it is the text already on screen.
#[test]
fn a_bare_missing_command_still_renders_compact() {
    let dir = common::fresh_tmp_path("ral_bare_missing", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let stderr = repl_stderr(&dir, &["--norc"], "no-such-command-xyz\n");
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        stderr.contains("no-such-command-xyz: command not found"),
        "the failure must still be reported; stderr was:\n{stderr}"
    );
    assert!(
        !stderr.contains('╭'),
        "a fault in the text on screen needs no caret; stderr was:\n{stderr}"
    );
}
