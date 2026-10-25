#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(unix)]

//! Surface observations: core pushes a plain `Value` onto the run's
//! `surface` sink at every redirect read/write and every external
//! command completion — a builtin dispatch never reaches the rail.
//! These tests drive the public `Shell::run` entry (exactly as
//! `surface_effect.rs` does) with a recording sink and assert the emitted
//! observation records, each fact tagged by its `what` variant.  Decoding
//! these into cards is exarch's job and out of scope here — we only check the
//! wire shape.

mod common;

use common::fresh_shell;

use ral_core::protocol::Run;
use ral_core::run::RunRequest;
use ral_core::source::SourceOrigin;
use ral_core::types::{Observation, Settled, Shell, Value};
use ral_core::{EventSink, SurfaceSink};
use std::sync::{Arc, Mutex};

/// A sink that records every surfaced observation as the record a trail
/// carries.
struct Recorder(Arc<Mutex<Vec<Value>>>);

impl EventSink for Recorder {
    fn emit(&self, ev: &ral_core::data::FOValue) {
        if let Some(obs) = Observation::from_surface(ev) {
            self.0.lock().unwrap().push(Value::from_datum(obs));
        }
    }
}

fn recording() -> (Arc<Mutex<Vec<Value>>>, SurfaceSink) {
    let log: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink: SurfaceSink = Arc::new(Recorder(Arc::clone(&log)));
    (log, sink)
}

/// Run one run of `source` with a recording surface sink and return the
/// settled value alongside the captured events.  Unlike `surface_effect.rs`'s
/// helper this tolerates a failing run result, since several I/O operations
/// (a failed exec, an aborted write) emit their event *and* surface an
/// error.
fn run(shell: &mut Shell, source: &str) -> (Settled<Value>, Vec<Value>) {
    let (log, sink) = recording();
    let result = shell
        .run(RunRequest {
            surface: Some(sink),
            ..RunRequest::from(Run::foreground(source, SourceOrigin::named("<test>")))
        })
        .expect_ran()
        .into_result();
    let events = log.lock().unwrap().clone();
    (result, events)
}

/// The fact of the single observation tagged `kind` among the captured
/// events, asserting exactly one fired.  Core reports every dispatch it makes,
/// so a test of one observation names the kind it is about rather than counting the whole
/// stream.
fn single_observation(events: &[Value], kind: &str) -> ral_core::types::Map {
    let facts: Vec<ral_core::types::Map> = events
        .iter()
        .filter_map(|v| match v {
            Value::Map(m) => match m.get("what").map(std::borrow::Cow::into_owned) {
                Some(Value::Variant {
                    label,
                    payload: Some(fact),
                }) if label.as_ref() == kind => match *fact {
                    Value::Map(fact) => Some(fact),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        facts.len(),
        1,
        "exactly one {kind} observation must fire, got {events:?}"
    );
    facts.into_iter().next().expect("checked above")
}

fn s(v: &str) -> Value {
    Value::string(v)
}

// ── COMMAND ──────────────────────────────────────────────────────────────
//
// A builtin dispatch never reaches the rail — only a command that actually
// left the shell (external, detached) surfaces.  Redirect and command
// observations in the audit trail are goldened in `tests/unix/audit-trail.ral`;
// this one pins delivery through the surface sink itself.

/// A bare external command (`/usr/bin/true`) emits one command observation
/// with argv [program], origin "external", status 0.
#[test]
fn external_success_emits_command_observation() {
    let mut shell = fresh_shell();
    let (result, events) = run(&mut shell, "/usr/bin/true");
    result.expect("/usr/bin/true should succeed");

    let m = single_observation(&events, "command");
    assert_eq!(
        m.get("argv").as_deref(),
        Some(&Value::list(vec![s("/usr/bin/true")]))
    );
    assert_eq!(
        m.get("origin").as_deref(),
        Some(&Value::variant("external", None))
    );
    assert_eq!(m.get("status").as_deref(), Some(&Value::Int(0)));
}

// ── WRITE ────────────────────────────────────────────────────────────────
//
// A write's images are read through the handles the redirect opened — the
// staging file, the target's own object — so a name replaced while the body
// runs cannot change what is reported.

fn some(bytes: &[u8]) -> Value {
    Value::variant("some", Some(Value::bytes(bytes)))
}

/// Run `script` against a fresh directory holding `f` as `old`, `{d}` in the
/// script naming the directory, and return the directory with the one write
/// observation.
fn write_over(old: &[u8], script: &str) -> (tempfile::TempDir, ral_core::types::Map) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::write(dir.join("f"), old).unwrap();
    let mut shell = fresh_shell();
    let (result, events) = run(
        &mut shell,
        &script.replace("{d}", &dir.display().to_string()),
    );
    result.expect("the write should settle");
    (tmp, single_observation(&events, "write"))
}

/// The after-image of `>` is what ral wrote into the staging file, though
/// another file has taken the staging name by the commit.
#[test]
fn an_atomic_writes_after_image_is_read_from_the_staging_handle() {
    let (_tmp, m) = write_over(
        b"old\n",
        "/bin/sh -c 'cd {d}; echo wrote; for s in .ral-write-*; do rm $s; echo substitute > $s; done' > '{d}/f'",
    );
    assert_eq!(m.get("new-bytes").as_deref(), Some(&some(b"wrote\n")));
    assert_eq!(m.get("old-bytes").as_deref(), Some(&some(b"old\n")));
}

/// Both images of `>>` are read through the handle derived from the held
/// leaf, though another file has taken the leaf's name by the settle.
#[test]
fn a_streaming_appends_images_are_read_from_the_derived_reader() {
    let (tmp, m) = write_over(
        b"old\n",
        "/bin/sh -c 'echo new; echo other > {d}/g; mv {d}/g {d}/f' >> '{d}/f'",
    );
    assert_eq!(m.get("old-bytes").as_deref(), Some(&some(b"old\n")));
    assert_eq!(m.get("new-bytes").as_deref(), Some(&some(b"old\nnew\n")));
    let dir = tmp.path().canonicalize().unwrap();
    assert_eq!(std::fs::read(dir.join("f")).unwrap(), b"other\n");
}

/// A file the user may not read has no before-image, and is written all the
/// same.
#[cfg(target_os = "linux")]
#[test]
fn a_write_to_an_unreadable_file_lands_with_no_before_image() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let target = dir.join("f");
    std::fs::write(&target, b"old\n").unwrap();
    std::fs::set_permissions(&target, PermissionsExt::from_mode(0o200)).unwrap();
    let mut shell = fresh_shell();
    let (result, events) = run(
        &mut shell,
        &format!("/bin/echo new > '{}'", target.display()),
    );
    result.expect("a 0200 file is writable");
    let m = single_observation(&events, "write");
    assert_eq!(
        m.get("old-bytes").as_deref(),
        Some(&Value::variant("none", None))
    );
    std::fs::set_permissions(&target, PermissionsExt::from_mode(0o600)).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"new\n");
}

/// A file the walk cannot hold is stated, and a stated leaf is not staged
/// over: the write is refused and the file is left as it was.
#[cfg(target_os = "macos")]
#[test]
fn a_write_to_a_file_the_walk_cannot_hold_is_refused_and_leaves_it() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let target = dir.join("f");
    std::fs::write(&target, b"old\n").unwrap();
    std::fs::set_permissions(&target, PermissionsExt::from_mode(0o200)).unwrap();
    let mut shell = fresh_shell();
    let (result, _events) = run(
        &mut shell,
        &format!("/bin/echo new > '{}'", target.display()),
    );
    assert!(result.is_err(), "a stated leaf is not staged over");
    std::fs::set_permissions(&target, PermissionsExt::from_mode(0o600)).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"old\n");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        1,
        "nothing staged"
    );
}
