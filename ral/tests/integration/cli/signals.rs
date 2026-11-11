//! A stop's path from the key (or the closed stdout) to the top of a batch
//! run: `try` does not see it, a `guard`'s acquisition and release are shielded
//! under it within one grace. Law 1's witness is core's `a_try_cannot_see_a_stop`;
//! these read the exit status and the marker files a run leaves.

#![cfg(unix)]

use crate::common::{
    Output, diagnostics, fresh_tmp_path, ral_command, spawn_script, wait_with_timeout,
};
use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

const SIGINT_STATUS: i32 = 130;
const SIGPIPE_STATUS: i32 = 141;

/// A marker file's path, absent.
fn marker(tag: &str) -> PathBuf {
    let path = fresh_tmp_path(&format!("ral_cancel_{tag}"), "mark");
    let _ = std::fs::remove_file(&path);
    path
}

/// A path as a ral string literal.
fn lit(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// Run `script` (whose protected step touches `ready`), strike
/// it with SIGINT once `ready` exists, and return how it ended and how long
/// the strike took to be answered.
fn strike(script: &str, ready: &Path) -> (Output, Duration) {
    let (mut child, tmp) = spawn_script("ral_cancel", &[], script);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        if Instant::now() > deadline {
            child.kill().ok();
            let _ = child.wait();
            let _ = std::fs::remove_file(&tmp);
            panic!("the protected step never started");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let struck = Instant::now();
    unsafe { libc::kill(child.id().cast_signed(), libc::SIGINT) };
    let out = wait_with_timeout(child, Duration::from_secs(15));
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(ready);
    (out.expect("ral did not end after SIGINT"), struck.elapsed())
}

#[test]
fn sigint_is_not_caught_by_try() {
    let (ready, caught) = (marker("ready"), marker("caught"));
    let script = format!(
        "try {{ touch {}; /bin/sleep 30 }} {{ |e| touch {}; /bin/sleep 1 }}",
        lit(&ready),
        lit(&caught)
    );
    let (out, _) = strike(&script, &ready);
    let ran = caught.exists();
    let _ = std::fs::remove_file(&caught);
    assert_eq!(out.status, SIGINT_STATUS, "stderr: {}", out.stderr);
    assert!(!ran, "a try handler saw the stop");
}

#[test]
fn sigint_runs_a_guards_cleanup() {
    let (ready, f) = (marker("ready"), marker("cleanup"));
    let script = format!(
        "guard {{ () }} {{ |_| touch {}; /bin/sleep 30 }} {{ |_ _| touch {} }}",
        lit(&ready),
        lit(&f)
    );
    let (out, _) = strike(&script, &ready);
    let ran = f.exists();
    let _ = std::fs::remove_file(&f);
    assert_eq!(out.status, SIGINT_STATUS, "stderr: {}", out.stderr);
    assert!(ran, "the cleanup did not run");
}

#[test]
fn sigint_during_acquisition_finishes_creation_then_releases_without_running_the_body() {
    let root = tempfile::tempdir().expect("create acquisition fixture");
    let ready = root.path().join("ready");
    let acquired = root.path().join("acquired");
    let body = root.path().join("body");
    let dir = root.path().join("d");
    let script = format!(
        "guard {{ touch {}; /bin/sleep 1; mkdir {}; touch {}; return {} }} \
         {{ |_| touch {} }} {{ |d _| rm -rf $d }}",
        lit(&ready),
        lit(&dir),
        lit(&acquired),
        lit(&dir),
        lit(&body)
    );
    let (out, _) = strike(&script, &ready);
    assert_eq!(out.status, SIGINT_STATUS, "stderr: {}", out.stderr);
    assert!(acquired.exists(), "the shielded acquisition did not finish");
    assert!(
        !body.exists(),
        "the body ran after SIGINT during acquisition"
    );
    assert!(
        !dir.exists(),
        "the release left the acquired directory behind"
    );
}

#[test]
fn sigint_inside_with_temp_dir_removes_the_directory() {
    let ready = marker("ready");
    let script = format!(
        "with-temp-dir {{ |d| echo $d; touch {}; /bin/sleep 30 }}",
        lit(&ready)
    );
    let (out, _) = strike(&script, &ready);
    assert_eq!(out.status, SIGINT_STATUS, "stderr: {}", out.stderr);
    let dir = Path::new(out.stdout.trim());
    assert!(
        dir.is_absolute(),
        "the body did not publish its path: {out:?}"
    );
    assert!(!dir.exists(), "SIGINT left the temporary directory {dir:?}");
}

#[test]
fn sigint_bounds_a_cleanup_by_the_grace() {
    let ready = marker("ready");
    let script = format!(
        "guard {{ () }} {{ |_| touch {}; /bin/sleep 30 }} {{ |_ _| /bin/sleep 30 }}",
        lit(&ready)
    );
    let (out, elapsed) = strike(&script, &ready);
    assert_eq!(out.status, SIGINT_STATUS, "stderr: {}", out.stderr);
    assert!(
        elapsed < Duration::from_secs(5 + 3),
        "the cleanup outlived its grace: {elapsed:?}"
    );
}

#[test]
fn sigint_lets_a_cleanups_pipeline_finish() {
    let (ready, f) = (marker("ready"), marker("pipeline"));
    let script = format!(
        "guard {{ () }} {{ |_| touch {}; /bin/sleep 30 }} {{ |_ _| !{{ /bin/sleep 0.3; echo done }} | /bin/cat > {} }}",
        lit(&ready),
        lit(&f)
    );
    let (out, _) = strike(&script, &ready);
    let body = std::fs::read_to_string(&f).ok();
    let _ = std::fs::remove_file(&f);
    assert_eq!(out.status, SIGINT_STATUS, "stderr: {}", out.stderr);
    assert_eq!(body.as_deref().map(str::trim_end), Some("done"));
}

/// A ral loop writing lines of its own: no external, whose SIGPIPE death is
/// its own.
const WRITER: &str = "each { |i| echo $i } !{range 1 1000000}";

/// Run `ral -c script` with stdout a pipe, read one line, close the pipe, and
/// return how it ended.
fn close_stdout_after_a_line(script: &str) -> Output {
    let mut child = ral_command()
        .args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral");
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().expect("piped stdout"))
        .read_line(&mut line)
        .expect("read one line");
    assert!(!line.is_empty(), "the writer wrote nothing");
    wait_with_timeout(child, Duration::from_secs(15)).expect("ral did not end on a closed stdout")
}

#[test]
fn a_closed_stdout_is_a_stop() {
    let out = close_stdout_after_a_line(WRITER);
    assert_eq!(out.status, SIGPIPE_STATUS);
    assert_eq!(
        diagnostics(&out.stderr),
        "",
        "a closed stdout prints nothing"
    );
}

#[test]
fn a_closed_stdout_is_not_caught_by_try() {
    let out = close_stdout_after_a_line(&format!("try {{ {WRITER} }} {{ |e| warn caught }}"));
    assert_eq!(out.status, SIGPIPE_STATUS);
    assert!(!out.stderr.contains("caught"), "stderr: {}", out.stderr);
}

/// The test that tells the stop from a SIGPIPE death, which shares its status.
#[test]
fn a_closed_stdout_runs_a_cleanup() {
    let f = marker("cleanup");
    let out = close_stdout_after_a_line(&format!(
        "guard {{ () }} {{ |_| {WRITER} }} {{ |_ _| touch {} }}",
        lit(&f)
    ));
    let ran = f.exists();
    let _ = std::fs::remove_file(&f);
    assert_eq!(out.status, SIGPIPE_STATUS, "stderr: {}", out.stderr);
    assert!(ran, "the cleanup did not run");
}

/// Law 10's analogue of law 6's dead-edge write: the cleanup's own write to
/// the gone stdout is the stop again.
#[test]
fn a_cleanups_write_to_a_closed_stdout_halts_it() {
    let f = marker("cleanup");
    let out = close_stdout_after_a_line(&format!(
        "guard {{ () }} {{ |_| {WRITER} }} {{ |_ _| echo more; touch {} }}",
        lit(&f)
    ));
    let ran = f.exists();
    let _ = std::fs::remove_file(&f);
    assert_eq!(out.status, SIGPIPE_STATUS, "stderr: {}", out.stderr);
    assert!(!ran, "the cleanup ran past its write to a closed stdout");
}
