#![allow(clippy::disallowed_methods)]

//! Surface observations: core pushes a plain `Value` onto the run's
//! `surface` sink at every redirect read/write door and every external
//! command completion door — a builtin dispatch never reaches the rail.
//! These tests drive the public `Shell::run` door (exactly as
//! `surface_effect.rs` does) with a recording sink and assert the emitted
//! observation maps, tagged by `kind`.  Decoding these into cards is
//! exarch's job and out of scope here — we only check the wire shape.

mod common;

use ral_core::transport::{Program, Run};
use ral_core::types::{Capabilities, Settled, Shell, Value};
use ral_core::{
    EventSink, RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, SurfaceSink,
    builtins,
};
use std::sync::{Arc, Mutex};

fn fresh_shell() -> Shell {
    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());
    shell
}

/// A sink that records every surfaced value, decoded back to `Value` — every
/// surfaced observation is first-order data, so the door's `FOValue` encoding
/// is transparent to these tests.
struct Recorder(Arc<Mutex<Vec<Value>>>);

impl EventSink for Recorder {
    fn emit(&self, ev: &ral_core::serial::FOValue) {
        self.0.lock().unwrap().push(Value::from(ev.clone()));
    }
}

fn recording() -> (Arc<Mutex<Vec<Value>>>, SurfaceSink) {
    let log: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink: SurfaceSink = Arc::new(Recorder(Arc::clone(&log)));
    (log, sink)
}

/// Run one run of `source` with a recording surface sink and return the
/// settled value alongside the captured events.  Unlike `surface_effect.rs`'s
/// helper this tolerates a failing run result, since several I/O doors
/// (a failed exec, an aborted write) emit their event *and* surface an
/// error.
fn run(shell: &mut Shell, source: &str) -> (Settled<Value>, Vec<Value>) {
    let (log, sink) = recording();
    let result = match shell.run(RunRequest {
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
        surface: Some(sink),
        deferred: None,
        desk: None,
        nursery: None,
        lifecycle: Box::new(()),
    }) {
        RunReport::Ran { result, .. } => result,
        RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    };
    let events = log.lock().unwrap().clone();
    (result, events)
}

/// The single observation of `kind` among the captured events, asserting
/// exactly one fired.  Core reports every dispatch it makes, so a door test
/// names the kind it is about rather than counting the whole stream.
fn single_observation<'a>(events: &'a [Value], kind: &str) -> &'a ral_core::types::Map {
    let observations: Vec<&Value> = events
        .iter()
        .filter(|v| matches!(v, Value::Map(m) if m.get("kind") == Some(&s(kind))))
        .collect();
    assert_eq!(
        observations.len(),
        1,
        "exactly one {kind} observation must fire, got {events:?}"
    );
    match observations[0] {
        Value::Map(m) => m,
        _ => unreachable!("filtered to maps above"),
    }
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

/// A fresh, unique scratch directory under the system temp dir, keyed on
/// the pid so parallel test binaries do not collide.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ral-surface-io-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ── READ door ────────────────────────────────────────────────────────────

/// `from-string < file` opens the input file and emits one read observation
/// carrying the literal redirect path; read has no outcome field.
#[test]
fn stdin_redirect_emits_read_observation() {
    let dir = scratch("read");
    let input = dir.join("a.txt");
    std::fs::write(&input, "hello\n").unwrap();
    let mut shell = fresh_shell();
    let path = input.to_string_lossy().into_owned();
    let (result, events) = run(&mut shell, &format!("from-string < '{path}'"));
    result.expect("stdin redirect read should succeed");

    let m = single_observation(&events, "read");
    assert_eq!(m.get("kind"), Some(&s("read")));
    assert_eq!(m.get("path"), Some(&s(&path)));
    assert_eq!(m.get("outcome"), None, "read has no outcome field");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── WRITE door ───────────────────────────────────────────────────────────

/// `to-string "x" > file` commits an atomic write and emits one write
/// observation: mode "write", outcome "committed".
#[test]
fn write_redirect_emits_committed_observation() {
    let dir = scratch("write");
    let target = dir.join("b.txt");
    let mut shell = fresh_shell();
    let path = target.to_string_lossy().into_owned();
    let (result, events) = run(&mut shell, &format!("to-string 'x' > '{path}'"));
    result.expect("write redirect should succeed");

    let m = single_observation(&events, "write");
    assert_eq!(m.get("kind"), Some(&s("write")));
    assert_eq!(m.get("path"), Some(&s(&path)));
    assert_eq!(m.get("mode"), Some(&s("write")));
    assert_eq!(m.get("outcome"), Some(&s("committed")));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `>>` is an append: mode "append", outcome "committed".
#[test]
fn append_redirect_emits_append_mode() {
    let dir = scratch("append");
    let target = dir.join("log.txt");
    let mut shell = fresh_shell();
    let path = target.to_string_lossy().into_owned();
    let (result, events) = run(&mut shell, &format!("to-string 'y' >> '{path}'"));
    result.expect("append redirect should succeed");

    let m = single_observation(&events, "write");
    assert_eq!(m.get("kind"), Some(&s("write")));
    assert_eq!(m.get("mode"), Some(&s("append")));
    assert_eq!(m.get("outcome"), Some(&s("committed")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `>~` is a stream write: mode "stream", outcome "committed".
#[test]
fn stream_redirect_emits_stream_mode() {
    let dir = scratch("stream");
    let target = dir.join("s.txt");
    let mut shell = fresh_shell();
    let path = target.to_string_lossy().into_owned();
    let (result, events) = run(&mut shell, &format!("to-string 'z' >~ '{path}'"));
    result.expect("stream redirect should succeed");

    let m = single_observation(&events, "write");
    assert_eq!(m.get("kind"), Some(&s("write")));
    assert_eq!(m.get("mode"), Some(&s("stream")));
    assert_eq!(m.get("outcome"), Some(&s("committed")));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A write whose body errors before commit emits outcome "aborted": the
/// file open succeeded but the logical write never completed, so the
/// atomic temp is discarded and the target is never created.
#[test]
fn write_with_failing_body_emits_aborted() {
    let dir = scratch("abort");
    let target = dir.join("never.txt");
    let mut shell = fresh_shell();
    let path = target.to_string_lossy().into_owned();
    // The redirect wraps a forced block whose body fails after the open:
    // `with_redirects` opens the target first, then runs the body, which
    // errors — so the open succeeded but the logical write aborted.
    let (result, events) = run(
        &mut shell,
        &format!("!{{ fail [status: 1, message: 'boom'] }} > '{path}'"),
    );
    assert!(result.is_err(), "the failing body must surface its error");

    let m = single_observation(&events, "write");
    assert_eq!(m.get("kind"), Some(&s("write")));
    assert_eq!(m.get("path"), Some(&s(&path)));
    assert_eq!(m.get("mode"), Some(&s("write")));
    assert_eq!(m.get("outcome"), Some(&s("aborted")));
    assert!(
        !target.exists(),
        "an aborted atomic write must not create the target"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A write whose open fails (parent directory does not exist) emits
/// outcome "failed": no body runs.
#[test]
fn write_with_failing_open_emits_failed() {
    let dir = scratch("openfail");
    // A nested path whose parent directory does not exist: the atomic
    // tempfile create in that parent fails, so `open_file` errors.
    let target = dir.join("missing-subdir").join("out.txt");
    let mut shell = fresh_shell();
    let path = target.to_string_lossy().into_owned();
    let (result, events) = run(&mut shell, &format!("to-string 'x' > '{path}'"));
    assert!(result.is_err(), "a failed open must surface its error");

    let m = single_observation(&events, "write");
    assert_eq!(m.get("kind"), Some(&s("write")));
    assert_eq!(m.get("path"), Some(&s(&path)));
    assert_eq!(m.get("mode"), Some(&s("write")));
    assert_eq!(m.get("outcome"), Some(&s("failed")));
    let _ = std::fs::remove_dir_all(&dir);
}

// ── COMMAND door ─────────────────────────────────────────────────────────
//
// A builtin dispatch never reaches the rail — only a command that actually
// crossed a door (external, detached) surfaces.  Every test below drives a
// real external or bundled-external command for exactly that reason.

/// A bare external command (`/usr/bin/true`) emits one command observation
/// with argv [program], origin "external", status 0.
#[cfg(unix)]
#[test]
fn external_success_emits_command_observation() {
    let mut shell = fresh_shell();
    let (result, events) = run(&mut shell, "/usr/bin/true");
    result.expect("/usr/bin/true should succeed");

    let m = single_observation(&events, "command");
    assert_eq!(m.get("kind"), Some(&s("command")));
    assert_eq!(m.get("argv"), Some(&Value::list(vec![s("/usr/bin/true")])));
    assert_eq!(m.get("origin"), Some(&s("external")));
    assert_eq!(m.get("status"), Some(&Value::Int(0)));
}

/// A failing external command (`/usr/bin/false`) emits one command
/// observation with origin "external" and a nonzero status.
#[cfg(unix)]
#[test]
fn external_failure_emits_command_observation() {
    let mut shell = fresh_shell();
    let (result, events) = run(&mut shell, "/usr/bin/false");
    assert!(result.is_err(), "/usr/bin/false exits nonzero");

    let m = single_observation(&events, "command");
    assert_eq!(m.get("kind"), Some(&s("command")));
    assert_eq!(m.get("argv"), Some(&Value::list(vec![s("/usr/bin/false")])));
    assert_eq!(m.get("origin"), Some(&s("external")));
    assert_eq!(m.get("status"), Some(&Value::Int(1)));
}

/// An external command carrying an argument places the program first in
/// argv, then the args.
#[cfg(unix)]
#[test]
fn external_argv_lists_program_then_args() {
    let mut shell = fresh_shell();
    let (result, events) = run(&mut shell, "/bin/echo hi");
    result.expect("/bin/echo should succeed");

    let m = single_observation(&events, "command");
    assert_eq!(m.get("kind"), Some(&s("command")));
    assert_eq!(
        m.get("argv"),
        Some(&Value::list(vec![s("/bin/echo"), s("hi")]))
    );
    assert_eq!(m.get("origin"), Some(&s("external")));
    assert_eq!(m.get("status"), Some(&Value::Int(0)));
}

/// A bundled (uutils) command run inline emits exactly one command
/// observation — it resolves as an external dispatch same as a PATH binary,
/// so the inline door fires and no separate door double-fires. `printf ''`
/// produces no output and exits 0.  Portable: the bundled tool runs
/// in-process on every platform, so this observation's shape is not
/// Unix-specific (unlike the `/usr/bin`/`/bin` external tests above).
#[cfg(feature = "coreutils")]
#[test]
fn inline_bundled_emits_single_command_observation() {
    let mut shell = fresh_shell();
    let (result, events) = run(&mut shell, "printf ''");
    result.expect("bundled printf should succeed");

    let m = single_observation(&events, "command");
    assert_eq!(m.get("kind"), Some(&s("command")));
    assert_eq!(m.get("argv"), Some(&Value::list(vec![s("printf"), s("")])));
    assert_eq!(m.get("origin"), Some(&s("external")));
    assert_eq!(m.get("status"), Some(&Value::Int(0)));
}
