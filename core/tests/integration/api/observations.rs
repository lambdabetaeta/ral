//! What a host's `surface` sink observes.  The `surface` builtin forwards a
//! value to the host-installed sink unchanged, and with no sink installed it is
//! the identity; both tests drive the public `Shell::run` entry the same way
//! `ral` and `exarch` do, including installing the name, which core withholds
//! for each host to carry.
//!
//! Core pushes a plain `ValClos` onto the run's `surface` sink at every redirect
//! read/write and every external command completion — a builtin dispatch never
//! reaches the rail.  Those tests assert the emitted observation records, each
//! fact tagged by its `what` variant; decoding them into cards is exarch's job
//! and out of scope — only the wire shape is checked.
//!
//! One external command is one fact, whether it ran alone or as a pipeline
//! stage.  Both paths mint their `Observed::Command` through one
//! `CommandRecord` and both leave the emission to the one gated recorder, so
//! the two must agree on the argv they spell *and* on whether anything is
//! emitted at all under a given sink/trail configuration.  Every assertion
//! about them compares the staged run against the standalone one rather than
//! against a literal: the test's job is to fail when either path diverges,
//! whatever it spells.
//!
//! The observation tests are Unix-only: they need a real external, and the
//! paths named are `/bin`.

use crate::common::prelude;
#[cfg(unix)]
use crate::common::{check_fields, commands, facts, run_recorded, under_grant};

use ral_core::data::FOValue;
use ral_core::data::datum::untag;
use ral_core::io::terminal::TerminalState;
use ral_core::protocol::Run;
use ral_core::shell::boot::{HostSurface, boot_shell};
use ral_core::shell::run::RunRequest;
#[cfg(unix)]
use ral_core::shell::runtime::{Map, View};
use ral_core::shell::runtime::{Observation, Settled, Shell, ValClos};
use ral_core::source::SourceOrigin;
#[cfg(unix)]
use ral_core::test_helper::fresh_shell;
use ral_core::{EventSink, SurfaceSink};
use std::sync::{Arc, Mutex};

/// A shell booted the way a host with a rail boots one: core's surface plus
/// the withheld `surface` name.
fn host_shell() -> Shell {
    boot_shell(
        TerminalState::default(),
        prelude(),
        &HostSurface {
            statics: vec![ral_core::shell::builtins::SURFACE_BUILTIN],
            captured: Vec::new(),
        },
        ral_core::test_helper::process_cwd(),
    )
    .expect("a test host resolves its baseline")
}

/// A sink that records every surfaced value.
struct RawRecorder(Arc<Mutex<Vec<FOValue>>>);

impl EventSink for RawRecorder {
    fn emit(&self, ev: &FOValue) {
        self.0.lock().unwrap().push(ev.clone());
    }
}

/// A fresh recording sink plus the shared buffer the test inspects.
fn recording() -> (Arc<Mutex<Vec<FOValue>>>, SurfaceSink) {
    let log: Arc<Mutex<Vec<FOValue>>> = Arc::new(Mutex::new(Vec::new()));
    let sink: SurfaceSink = Arc::new(RawRecorder(Arc::clone(&log)));
    (log, sink)
}

/// The kit's own surfaces.  Core reports an observation per dispatch on the
/// same sink; this file is about what the `surface` builtin forwards.
fn kit_events(events: &[FOValue]) -> Vec<FOValue> {
    events
        .iter()
        .filter(|ev| !matches!(untag(ev), Some((Observation::SURFACE_TAG, _))))
        .cloned()
        .collect()
}

/// Run one run of `source` with an optional surface sink, returning the
/// settled value.  These sources are well-formed, so a static diagnostic is
/// a test bug.
fn run_with_sink(
    shell: &mut Shell,
    source: &str,
    surface: Option<SurfaceSink>,
) -> Settled<ValClos> {
    shell
        .run(RunRequest {
            surface,
            ..RunRequest::from(Run::foreground(source, SourceOrigin::named("<test>")))
        })
        .expect_ran()
        .into_settled()
}

/// With no sink installed `surface` returns Unit and is otherwise inert.
#[test]
fn surface_without_sink_is_identity() {
    let mut shell = host_shell();
    let out = run_with_sink(
        &mut shell,
        r#"surface `task [status: "open", desc: "x"]"#,
        None,
    );
    assert_eq!(out.expect("surface should succeed"), ValClos::UNIT);
}

/// The exact variant the body constructs reaches the installed sink.
#[test]
fn surface_forwards_the_event_to_the_sink() {
    let mut shell = host_shell();
    let (log, sink) = recording();
    let out = run_with_sink(
        &mut shell,
        r#"surface `meter [done: 1, total: 3, label: "tasks"]"#,
        Some(sink),
    );
    assert_eq!(out.expect("surface should succeed"), ValClos::UNIT);

    let events = kit_events(&log.lock().unwrap());
    assert_eq!(events.len(), 1, "exactly one event surfaced");
    let FOValue::Variant { label, payload } = &events[0] else {
        panic!("expected a variant, got {:?}", events[0]);
    };
    assert_eq!(label, "meter");
    let Some(payload) = payload.as_deref() else {
        panic!("expected a payload record");
    };
    assert_eq!(payload.field("done").and_then(FOValue::as_int), Some(1));
    assert_eq!(payload.field("total").and_then(FOValue::as_int), Some(3));
    assert_eq!(
        payload.field("label").and_then(FOValue::as_str),
        Some("tasks")
    );
}

/// The fact of the single observation tagged `kind` among the captured
/// events, asserting exactly one fired.  Core reports every dispatch it makes,
/// so a test of one observation names the kind it is about rather than counting the whole
/// stream.
#[cfg(unix)]
fn single_observation(events: &[ValClos], kind: &str) -> ral_core::shell::runtime::Map {
    let facts: Vec<ral_core::shell::runtime::Map> = events
        .iter()
        .filter_map(|v| match v.view() {
            View::Map(m) => match m.get("what").as_deref().map(ValClos::view) {
                Some(View::Variant {
                    label,
                    payload: Some(fact),
                }) if label.as_ref() == kind => match fact.view() {
                    View::Map(fact) => Some(fact.to_map()),
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

/// [`run_recorded`] with the sink always installed: several I/O operations
/// (a failed exec, an aborted write) emit their event *and* surface an error.
#[cfg(unix)]
fn run_observed(shell: &mut Shell, source: &str) -> (Settled<ValClos>, Vec<ValClos>) {
    run_recorded(shell, source, true)
}

#[cfg(unix)]
fn s(v: &str) -> ValClos {
    ValClos::string(v)
}

// ── COMMAND ──────────────────────────────────────────────────────────────
//
// A builtin dispatch never reaches the rail — only a command that actually
// left the shell (external, detached) surfaces.  Redirect and command
// observations in the audit trail are goldened in `tests/run/unix/audit-*.ral`;
// this one pins delivery through the surface sink itself.

/// A bare external command (`/usr/bin/true`) emits one command observation
/// with argv [program], origin "external", status 0.
#[cfg(unix)]
#[test]
fn external_success_emits_command_observation() {
    let mut shell = fresh_shell();
    let (result, events) = run_observed(&mut shell, "/usr/bin/true");
    result.expect("/usr/bin/true should succeed");

    let m = single_observation(&events, "command");
    assert_eq!(
        m.get("argv"),
        Some(&ValClos::list(vec![s("/usr/bin/true")]))
    );
    assert_eq!(m.get("origin"), Some(&ValClos::variant("external", None)));
    assert_eq!(m.get("status"), Some(&ValClos::int(0)));
}

// ── WRITE ────────────────────────────────────────────────────────────────
//
// A write's images are read through the handles the redirect opened — the
// staging file, the target's own object — so a name replaced while the body
// runs cannot change what is reported.

#[cfg(unix)]
fn some(bytes: &[u8]) -> ValClos {
    ValClos::variant("some", Some(ValClos::bytes(bytes)))
}

/// Run `script` against a fresh directory holding `f` as `old`, `{d}` in the
/// script naming the directory, and return the directory with the one write
/// observation.
#[cfg(unix)]
fn write_over(old: &[u8], script: &str) -> (tempfile::TempDir, ral_core::shell::runtime::Map) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::write(dir.join("f"), old).unwrap();
    let mut shell = fresh_shell();
    let (result, events) = run_observed(
        &mut shell,
        &script.replace("{d}", &dir.display().to_string()),
    );
    result.expect("the write should settle");
    (tmp, single_observation(&events, "write"))
}

/// The after-image of `>` is what ral wrote into the staging file, though
/// another file has taken the staging name by the commit.
#[cfg(unix)]
#[test]
fn an_atomic_writes_after_image_is_read_from_the_staging_handle() {
    let (_tmp, m) = write_over(
        b"old\n",
        "/bin/sh -c 'cd {d}; echo wrote; for s in .ral-write-*; do rm $s; echo substitute > $s; done' > '{d}/f'",
    );
    assert_eq!(m.get("new-bytes"), Some(&some(b"wrote\n")));
    assert_eq!(m.get("old-bytes"), Some(&some(b"old\n")));
}

/// Both images of `>>` are read through the handle derived from the held
/// leaf, though another file has taken the leaf's name by the settle.
#[cfg(unix)]
#[test]
fn a_streaming_appends_images_are_read_from_the_derived_reader() {
    let (tmp, m) = write_over(
        b"old\n",
        "/bin/sh -c 'echo new; echo other > {d}/g; mv {d}/g {d}/f' >> '{d}/f'",
    );
    assert_eq!(m.get("old-bytes"), Some(&some(b"old\n")));
    assert_eq!(m.get("new-bytes"), Some(&some(b"old\nnew\n")));
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
        ral_core::errln!("skipped: running as root");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let target = dir.join("f");
    std::fs::write(&target, b"old\n").unwrap();
    std::fs::set_permissions(&target, PermissionsExt::from_mode(0o200)).unwrap();
    let mut shell = fresh_shell();
    let (result, events) = run_observed(
        &mut shell,
        &format!("/bin/echo new > '{}'", target.display()),
    );
    result.expect("a 0200 file is writable");
    let m = single_observation(&events, "write");
    assert_eq!(m.get("old-bytes"), Some(&ValClos::variant("none", None)));
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
        ral_core::errln!("skipped: running as root");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let target = dir.join("f");
    std::fs::write(&target, b"old\n").unwrap();
    std::fs::set_permissions(&target, PermissionsExt::from_mode(0o200)).unwrap();
    let mut shell = fresh_shell();
    let (result, _events) = run_observed(
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

#[cfg(unix)]
fn run_fresh(source: &str, sink: bool) -> (Settled<ValClos>, Vec<ValClos>) {
    run_recorded(&mut fresh_shell(), source, sink)
}

#[cfg(unix)]
fn command_facts(observations: &[ValClos]) -> Vec<Map> {
    facts(observations, "command")
        .into_iter()
        .map(|(fact, _)| fact)
        .collect()
}

/// The trail `audit { … }` returns, as the same observation records the sink
/// would have received.
#[cfg(unix)]
fn trail_of(report: &ValClos) -> Vec<ValClos> {
    let View::Map(report) = report.view() else {
        panic!("audit returns a record, got {report:?}");
    };
    match report.get("trail").as_deref().map(ValClos::view) {
        Some(View::List(items)) => items.iter().map(std::borrow::Cow::into_owned).collect(),
        other => panic!("audit's report carries a trail list, got {other:?}"),
    }
}

/// The three fields a reader of a command fact judges it by.
#[cfg(unix)]
fn shape(fact: &Map) -> (Option<ValClos>, Option<ValClos>, Option<ValClos>) {
    (
        fact.get("argv").cloned(),
        fact.get("origin").cloned(),
        fact.get("status").cloned(),
    )
}

/// The same external, alone and as the head of a pipeline: the head's fact
/// must be the standalone's fact — the whole argv, not just the program.
#[cfg(unix)]
#[test]
fn a_pipeline_stage_reports_the_same_command_fact_as_a_standalone_run() {
    let (alone_result, alone_events) = run_fresh("/bin/echo hi", true);
    alone_result.expect("/bin/echo should succeed");
    let alone = command_facts(&alone_events);
    assert_eq!(alone.len(), 1, "one dispatch, one fact: {alone_events:?}");

    let (staged_result, staged_events) = run_fresh("/bin/echo hi | /bin/cat", true);
    staged_result.expect("the pipeline should succeed");
    let staged = command_facts(&staged_events);
    assert_eq!(staged.len(), 2, "each stage surfaces exactly once");
    let head = staged
        .first()
        .expect("a stage that ran must report its own fact, as a standalone run does");

    assert_eq!(
        shape(head),
        shape(&alone[0]),
        "the head stage must spell what the standalone dispatch spelled"
    );
}

/// The gate too: with a trail open and no sink, and with a sink and no trail,
/// the staged run must be as loud as the standalone one.
#[cfg(unix)]
#[test]
fn a_pipeline_stage_and_a_standalone_run_agree_on_when_to_report() {
    for sink in [false, true] {
        let (alone_report, alone_events) = run_fresh("audit { /bin/echo hi }", sink);
        let alone_report = alone_report.expect("the audited standalone run should succeed");
        let (staged_report, staged_events) = run_fresh("audit { /bin/echo hi | /bin/cat }", sink);
        let staged_report = staged_report.expect("the audited pipeline should succeed");

        let alone_trail = command_facts(&trail_of(&alone_report));
        let staged_trail = command_facts(&trail_of(&staged_report));
        assert_eq!(staged_trail.len(), 2, "each stage enters the trail once");
        if sink {
            assert_eq!(command_facts(&staged_events).len(), 2);
        }
        assert_eq!(
            shape(staged_trail.first().expect("the head stage's own fact")),
            shape(alone_trail.first().expect("the standalone dispatch's fact")),
            "trail parity fails with sink={sink}"
        );

        assert_eq!(
            command_facts(&staged_events).is_empty(),
            command_facts(&alone_events).is_empty(),
            "with sink={sink} the two paths must agree on whether anything surfaces"
        );
    }
}

#[cfg(unix)]
#[test]
fn nested_pipelines_publish_each_fact_once_and_keep_the_whole_trail() {
    let (report, events) = run_fresh("audit { /bin/echo hi | !{ /bin/cat | /bin/cat } }", true);
    let report = report.expect("nested pipelines succeed");
    let trail = trail_of(&report);
    assert_eq!(command_facts(&trail).len(), 3);
    assert_eq!(command_facts(&events).len(), 3);
    assert!(trail.iter().all(|fact| events.contains(fact)));
}

#[cfg(unix)]
#[test]
fn audit_retention_preserves_pipeline_traffic_and_reports_the_cut() {
    use ral_core::data::FOValue;
    use ral_core::data::datum::Datum as _;
    use ral_core::fact::Observed;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large.bin");
    let bytes: Vec<u8> = (0..=255).cycle().take(1024 * 1024 + 4096).collect();
    std::fs::write(&path, &bytes).unwrap();
    let source = format!("audit {{ /bin/cat '{}' | /usr/bin/wc -c }}", path.display());
    let (report, events) = run_fresh(&source, true);
    let trail = trail_of(&report.expect("the pipeline succeeds past the retention limit"));
    let commands: Vec<_> = trail
        .iter()
        .map(|v| Observation::decode(&FOValue::try_from(v).unwrap()).unwrap())
        .filter_map(|obs| match obs.what {
            Observed::Command(command) => Some(command),
            _ => None,
        })
        .collect();
    assert_eq!(commands.len(), 2);
    assert_eq!(command_facts(&events).len(), 2);
    assert!(trail.iter().all(|fact| events.contains(fact)));
    let producer = &commands[0];
    assert_eq!(producer.stdout.as_slice(), &bytes[..1024 * 1024]);
    assert_eq!(producer.stdout_total, bytes.len() as u64);
    let count = std::str::from_utf8(&commands[1].stdout).unwrap().trim();
    assert_eq!(count.parse::<usize>().unwrap(), bytes.len());
    assert_eq!(commands[1].stdout_total, commands[1].stdout.len() as u64);
}

/// A refused external is one check fact, alone or as a direct stage: the
/// same fields at the same site, both on the sink, and no argv among them.
#[cfg(unix)]
#[test]
fn a_refused_stage_reports_the_same_check_as_a_refused_standalone_run() {
    let (alone_result, alone_events) = run_fresh(&under_grant("/bin/echo hi"), true);
    let (staged_result, staged_events) = run_fresh(&under_grant("/bin/echo hi | /bin/cat"), true);
    assert!(alone_result.is_err() && staged_result.is_err());
    let alone = facts(&alone_events, "check");
    let staged = facts(&staged_events, "check");
    assert_eq!(alone.len(), 1, "one refusal, one check: {alone_events:?}");
    assert_eq!(
        staged, alone,
        "a direct stage's refusal must be the standalone's"
    );
    assert!(check_fields(&alone[0].0).get("args").is_none());
}

/// A direct stage that `prepare`'s vetting refuses never spawned, and still
/// leaves its one `Command` record, carrying the refusal.
#[cfg(unix)]
#[test]
fn a_direct_stage_refused_by_vetting_leaves_one_command_record() {
    // The list arrives decoded, so only the run, not the checker, refuses it.
    let (result, events) = run_fresh(
        "let xs = /bin/echo '[1, 2, 3]' | from-json; /bin/echo hi | /usr/bin/printf $xs",
        true,
    );
    assert!(result.is_err());
    let commands = commands(&events);
    let refused: Vec<_> = commands
        .iter()
        .filter(|command| {
            command
                .argv
                .first()
                .is_some_and(|name| name == "/usr/bin/printf")
        })
        .collect();
    let [command] = refused.as_slice() else {
        panic!("one record for the refused stage, got {commands:?}");
    };
    assert_ne!(command.status, 0);
    assert!(
        command
            .error
            .as_deref()
            .is_some_and(|e| e.contains("cannot pass a list")),
        "{command:?}"
    );
}
