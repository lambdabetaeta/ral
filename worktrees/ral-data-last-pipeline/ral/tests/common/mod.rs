//! Shared scaffolding for `ral` integration tests.
//!
//! Each integration file used to rebuild the same skeleton: locate the
//! `ral` binary built by cargo, generate a fresh temp script path,
//! spawn the binary on it, capture stdout/stderr, propagate the exit
//! code.  The helpers below collect that scaffolding in one place.
//!
//! Cargo treats `tests/common/mod.rs` as a module rather than its own
//! integration test target — that is why it lives here and not in
//! `tests/common.rs`.

#![allow(dead_code)] // not every test file uses every helper

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// Captured result of a one-shot `ral` invocation.
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub status: i32,
}

/// Path to the `ral` binary cargo built for this test target.
pub fn ral_bin() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let name = if cfg!(windows) { "ral.exe" } else { "ral" };
    manifest_dir.join("../target").join(profile).join(name)
}

static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(1);

/// Build a unique temp file path of the form `<prefix>_<pid>_<id>.<ext>`.
pub fn fresh_tmp_path(prefix: &str, ext: &str) -> PathBuf {
    let mut tmp = std::env::temp_dir();
    let pid = std::process::id();
    let id = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
    tmp.push(format!("{prefix}_{pid}_{id}.{ext}"));
    tmp
}

/// Write `script` to a fresh temp file, run `ral <file>`, return captured I/O.
/// stdin is `/dev/null`.  The temp file is removed once the child exits.
pub fn run(prefix: &str, script: &str) -> Output {
    let tmp = fresh_tmp_path(prefix, "ral");
    std::fs::write(&tmp, script).unwrap();

    let child = Command::new(ral_bin())
        .arg(&tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral");

    let out = child.wait_with_output().unwrap();
    std::fs::remove_file(&tmp).ok();

    Output {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}
