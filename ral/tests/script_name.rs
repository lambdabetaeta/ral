#![cfg(target_os = "linux")]
#![allow(
    clippy::disallowed_macros,
    reason = "[test] libtest captures print macros"
)]
#![allow(clippy::disallowed_methods)]

//! A `#!` script launched through ral sees its own real path as `$0`, so the
//! lexical idiom for finding its siblings works: unconfined, under an
//! fs-only grant, and inside an exec-restricted envelope.
//!
//! The probe uses shell builtins alone, since the exec-restricted case admits
//! nothing else.  `${0%/*}` is `dirname "$0"`.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// A fresh temporary directory, kept alive by the caller, and its canonical
/// path (the tempdir may sit under a symlinked `/tmp`).  It holds the
/// executable `probe`, which prints its `$0` and the line of the `sibling`
/// beside it.
fn fixture() -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(tmp.path()).unwrap();
    let probe = dir.join("probe");
    std::fs::write(
        &probe,
        "#!/bin/sh\nprintf '%s\\n' \"$0\"; read -r l < \"${0%/*}/sibling\"; printf '%s\\n' \"$l\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("sibling"), "SIBLING\n").unwrap();
    (tmp, dir)
}

/// Run `script`, written to a temporary file named with `prefix`, and assert
/// that the probe in `dir` printed its real path and its sibling.
fn assert_sees_real_path(dir: &Path, prefix: &str, script: &str) {
    let out = common::run(prefix, script);
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert_eq!(out.stdout, format!("{}/probe\nSIBLING\n", dir.display()));
}

#[test]
fn an_unconfined_script_sees_its_real_path() {
    let (_tmp, dir) = fixture();
    let d = dir.display();
    assert_sees_real_path(&dir, "script_name_plain", &format!("{d}/probe"));
}

#[test]
fn a_script_under_an_fs_grant_sees_its_real_path() {
    if !common::bwrap_functional() {
        eprintln!("skip: no functional bwrap");
        return;
    }
    let (_tmp, dir) = fixture();
    let d = dir.display();
    assert_sees_real_path(
        &dir,
        "script_name_fs",
        &format!("grant [fs: [read: [`absolute '{d}']]] {{ {d}/probe }}"),
    );
}

#[test]
fn a_script_under_an_exec_grant_sees_its_real_path() {
    if !common::bwrap_functional() {
        eprintln!("skip: no functional bwrap");
        return;
    }
    let (_tmp, dir) = fixture();
    let d = dir.display();
    assert_sees_real_path(
        &dir,
        "script_name_exec",
        &format!(
            "grant [fs: [read: [`absolute '{d}']], \
             exec: [allow: [`absolute '{d}/probe', `absolute /bin/sh]]] {{ {d}/probe }}"
        ),
    );
}
