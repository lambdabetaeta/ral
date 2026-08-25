//! `detach` (`docs/SPEC.md` §13.7): the one concurrency verb whose work the
//! session stops owning.
//!
//! Everything here is checked from outside the mechanism — through the
//! public `run` door, the receipt it hands back, and the kernel — because
//! the whole point of the verb is that nothing inside this process can name
//! what it started.  The crown jewel is
//! [`a_detached_process_outlives_the_full_exit_of_the_host_that_birthed_it`]:
//! a host that is a whole process, gone, and a survivor still working.
//!
//! Confinement is tested from two sides.  Whether a frame *permits* the
//! verb is ordinary capability behaviour, checked here through the same
//! public door as everything else.  What the survivor is confined *to* is
//! not: it is one flag in a bwrap argv, so it is checked where that argv is
//! built (`core/src/sandbox/linux.rs`) rather than by birthing a process
//! and interrogating a namespace this test cannot enter.
//!
//! Three properties the ADR argues about are deliberately *not* tested
//! here, and none of them is faked: macOS reparenting, which `/proc` cannot
//! answer; the teardown footrace, which by construction has no determinate
//! outcome, so a test asserting either side would be exactly the flake the
//! ADR warns of; and pid recycling, which no test can provoke on demand.

#![cfg(unix)]
#![allow(clippy::disallowed_methods)]

mod common;

use ral_core::serial::FOValue;
use ral_core::transport::{Program, Run};
use ral_core::types::{Capabilities, Shell};
use ral_core::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, Value, builtins};
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The marker the hidden birth flag's survivor writes to its trace file and
/// to both its streams, which go to `/dev/null`.
const MARKER: &str = "survivor-8fb1";

/// Every test here births OS processes, and one of them sweeps
/// `waitpid(-1)` — which would happily reap another test's intermediate out
/// from under the birth still waiting for it.  One at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// A shell holding `detach` exactly as exarch's seat hands it over: the name
/// installed and the budget armed in one act, so a shell that has the verb
/// always has births to spend.
fn armed() -> Shell {
    let mut shell = ral_core::boot::boot_shell(
        ral_core::io::TerminalState::default(),
        common::prelude(),
        &ral_core::boot::HostSurface::default(),
    );
    shell.install_builtins(builtins::DETACH_BUILTIN);
    shell.arm_detach(16);
    shell
}

/// Birth one process through `call` and return the receipt as JSON — via
/// `to-json` rather than the `Value`, so what the model would read is what
/// the test reads.
fn birth(shell: &mut Shell, call: &str) -> serde_json::Value {
    let report = shell.run(RunRequest {
        run: Run {
            program: Program::Source(format!("let r = {call}; to-json $r")),
            script_name: "<test>".into(),
            caps: Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Capture,
            terminal: RequestedTerminalAccess::Leased,
            stdin: RunStdin::Inherit,
            trail: None,
        },
        surface: None,
        deferred: None,
        desk: None,
        fork: None,
        lifecycle: Box::new(()),
    });
    match report {
        RunReport::Ran {
            ending: ral_core::Ending::Settled {
                value: Value::Unit, ..
            },
            captured: Some(captured),
            ..
        } => serde_json::from_slice(&captured.stdout).expect("`to-json` emits JSON"),
        RunReport::Ran { ending, .. } => {
            panic!("{call} must return a receipt record, got {ending:?}")
        }
        RunReport::Static { .. } => panic!("{call} must compile"),
    }
}

/// `birth`'s negative: run `source` and return the message it was refused
/// with, insisting the run reached evaluation so a refusal is never
/// confused with a compile failure.
fn refusal(shell: &mut Shell, source: &str) -> String {
    let report = shell.run(RunRequest {
        run: Run {
            program: Program::Source(source.into()),
            script_name: "<test>".into(),
            caps: Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Capture,
            terminal: RequestedTerminalAccess::Leased,
            stdin: RunStdin::Inherit,
            trail: None,
        },
        surface: None,
        deferred: None,
        desk: None,
        fork: None,
        lifecycle: Box::new(()),
    });
    match report {
        RunReport::Ran {
            ending: ral_core::Ending::Raised { error, .. } | ral_core::Ending::Walled { error, .. },
            ..
        } => error.to_string(),
        RunReport::Ran { ending, .. } => panic!("{source} must be refused, got {ending:?}"),
        RunReport::Static { .. } => panic!("{source} must compile"),
    }
}

/// Re-exec this test binary under the hidden birth flag, wait for it to exit
/// *completely*, and return the pid it printed for the survivor it left
/// behind.  `host_out` and `host_err` take the host's own two streams, so a
/// caller can also ask where the survivor's bytes did *not* go.
///
/// The re-exec lands in `common`'s `#[ctor]`, which serves the flag before
/// libtest ever sees the argv.
fn host_that_births_and_exits(trace: &Path, host_out: &Path, host_err: &Path) -> Survivor {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(ral_core::test_helper::DETACH_BIRTH_FLAG)
        .arg(trace)
        .arg(MARKER)
        .stdout(std::fs::File::create(host_out).unwrap())
        .stderr(std::fs::File::create(host_err).unwrap())
        .status()
        .expect("the test binary can re-exec itself");
    assert!(status.success(), "the birthing host exited {status}");
    let printed = std::fs::read_to_string(host_out).unwrap();
    Survivor(
        printed
            .split_whitespace()
            .next()
            .and_then(|pid| pid.parse().ok())
            .expect("the host prints the pid of what it birthed, and nothing else"),
    )
}

/// Poll `holds` to a deliberately generous ceiling: the dev box is a VM
/// whose scheduler jitter dwarfs any honest guess at how long a fork, an
/// exec, and a write take.
fn settles(holds: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if holds() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    holds()
}

/// The parent pid `/proc` reports for `pid`.  `comm` is parenthesised and
/// may itself contain spaces and parens, so the fields are counted from the
/// last `)`: state, then ppid.
#[cfg(target_os = "linux")]
fn ppid_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// A process this session no longer owns, sent SIGKILL when the guard
/// leaves scope — on the assertion path and the panic path alike.  A test
/// that forgot would leak it past the whole run, since by construction
/// nothing else in this process can name it.
struct Survivor(libc::pid_t);

impl Survivor {
    /// The process a receipt names.
    fn of(receipt: &serde_json::Value) -> Self {
        Self(
            receipt["pid"]
                .as_i64()
                .and_then(|pid| libc::pid_t::try_from(pid).ok())
                .expect("a receipt names its process by pid"),
        )
    }

    fn alive(&self) -> bool {
        unsafe { libc::kill(self.0, 0) == 0 }
    }
}

impl Drop for Survivor {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0, libc::SIGKILL) };
    }
}

/// The receipt is the whole of what a birth leaves this session: the
/// description the caller gave and the pid the kernel gave.  There is
/// nothing else, and nothing to look up.
#[test]
fn a_birth_hands_back_a_pid_and_a_desc_and_leaves_nothing_else() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let mut shell = armed();
    let receipt = birth(&mut shell, "detach #'a long sleep'# /bin/sleep 300");
    let survivor = Survivor::of(&receipt);

    let mut fields: Vec<&str> = receipt
        .as_object()
        .expect("the receipt is a record")
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(fields, ["desc", "pid"], "the receipt names nothing else");
    assert_eq!(receipt["desc"], "a long sleep");
    assert!(
        survivor.alive(),
        "the pid the receipt names must be a live process"
    );
}

/// The survivor is alive and is nobody's child here.  On Linux `/proc`
/// can say who took it instead; on macOS nothing can, so this asserts only
/// what it can observe there.
#[test]
fn a_survivor_is_alive_and_is_no_longer_a_child_of_this_process() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let mut shell = armed();
    let receipt = birth(&mut shell, "detach #'a long sleep'# /bin/sleep 300");
    let survivor = Survivor::of(&receipt);
    assert!(
        survivor.alive(),
        "the pid the receipt names must be a live process"
    );

    #[cfg(target_os = "linux")]
    {
        let me = libc::pid_t::try_from(std::process::id()).unwrap();
        let parent = ppid_of(survivor.0).expect("/proc knows a live process's parent");
        assert_ne!(
            parent, me,
            "the double fork must hand the survivor away, and it is still our child"
        );
        if parent != 1 {
            // An ambient `PR_SET_CHILD_SUBREAPER` ancestor — a container
            // init, a session manager, a test runner — claims orphans before
            // init does.  That is legal, and it makes the `ppid == 1` half of
            // the property unobservable, so it is not asserted.  The reaper
            // must still be an ancestor of ours: anything else means the
            // survivor went somewhere nobody intended.
            let mut walk = me;
            let mut ancestors = Vec::new();
            while walk > 1 {
                walk = ppid_of(walk).unwrap_or(1);
                ancestors.push(walk);
            }
            assert!(
                ancestors.contains(&parent),
                "the survivor reparented to {parent}, which is neither init nor a subreaper ancestor of this test"
            );
        }
    }
}

/// The intermediate is reaped inside the birth, so nothing of ours is left
/// waiting to be collected — and, the survivor never having been ours, this
/// process ends the birth with no children at all.
#[test]
fn a_birth_leaves_no_zombie_because_the_intermediate_is_reaped_inside_it() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let mut shell = armed();
    let receipt = birth(&mut shell, "detach #'a long sleep'# /bin/sleep 300");
    let survivor = Survivor::of(&receipt);

    let mut status = 0;
    let swept = unsafe { libc::waitpid(-1, &raw mut status, libc::WNOHANG) };
    assert_eq!(
        swept, -1,
        "a `waitpid(-1, WNOHANG)` sweep answered {swept}: after a birth this process must have no children at all"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD),
        "the sweep must fail with ECHILD — the intermediate was reaped inside the birth, and the survivor was never ours"
    );
    assert!(survivor.alive(), "the sweep must not have touched it");
}

/// The verb's negative: a detached process is not a worker.  It files
/// nothing in the registry (SPEC §13.3) and appears in no `` `workers ``
/// listing, because neither has anything to hold for it — there is no live
/// handle, and `await`/`poll`/`race`/`cancel` do not apply.
#[test]
fn a_detached_process_is_in_no_worker_registry_and_no_workers_listing() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let mut shell = armed();
    let receipt = birth(&mut shell, "detach #'a long sleep'# /bin/sleep 300");
    let survivor = Survivor::of(&receipt);
    assert!(
        survivor.alive(),
        "the survivor must be running to be missed"
    );

    assert_eq!(shell.worker_count(), 0, "a birth occupies no worker seat");
    assert!(
        shell.workers().is_empty(),
        "a birth files nothing in the worker registry"
    );
    match ral_core::transport::answer_probe(
        &mut shell,
        &FOValue::Variant {
            label: "workers".into(),
            payload: None,
        },
    ) {
        Ok(FOValue::List { items }) => assert!(
            items.is_empty(),
            "`workers listed {items:?} for a process no session owns"
        ),
        other => panic!("the `workers probe must answer a list, got {other:?}"),
    }
}

/// A frame that withholds the verb refuses the call and births nothing —
/// and, because the refusal comes before admission, spends no birth either.
/// The budget is monotone, so a refusal that quietly counted would be
/// unrecoverable.
#[test]
fn a_frame_that_withholds_detach_refuses_the_call_and_spends_no_birth() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let mut shell = armed();
    // One birth in the whole session, so the surviving process itself is the
    // evidence: had the refusal counted, this budget would be gone and the
    // birth below would be refused for exhaustion instead.
    shell.arm_detach(1);
    let message = refusal(
        &mut shell,
        "grant [detach: false] { detach #'a long sleep'# /bin/sleep 300 }",
    );
    assert!(
        message.contains("withholds"),
        "the refusal must say the grant withheld the verb, got {message:?}"
    );

    let receipt = birth(&mut shell, "detach #'a long sleep'# /bin/sleep 300");
    let survivor = Survivor::of(&receipt);
    assert!(
        survivor.alive(),
        "the one birth the session had must still be spendable after a refusal"
    );
}

/// A launch that is admitted and then fails — the path head passes vet, whose
/// existence probe covers bare names only, and dies at the kernel's ENOENT —
/// must give its reservation back.  The budget is whole-life and monotone for
/// births; a failed launch birthed nothing, so it spends nothing.
#[test]
fn a_launch_that_fails_after_admission_gives_the_slot_back() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let mut shell = armed();
    // One birth in the whole session: had the failed launch counted, the
    // real birth below would be refused for exhaustion instead.
    shell.arm_detach(1);
    let message = refusal(
        &mut shell,
        "detach #'never born'# /definitely/not/a/real/binary-8fb1",
    );
    assert!(
        message.contains("cannot launch"),
        "the refusal must come from the failed spawn, not from vet or exhaustion, got {message:?}"
    );

    let receipt = birth(&mut shell, "detach #'a long sleep'# /bin/sleep 300");
    let survivor = Survivor::of(&receipt);
    assert!(
        survivor.alive(),
        "the one birth the session had must still be spendable after a failed launch"
    );
}

/// Silence permits: a grant that attenuates some *other* dimension says
/// nothing about survivors, so the verb is spendable inside it — the whole
/// point of the axis being a meet rather than an opt-in.  The survivor is
/// born under that frame's projection and keeps it for life.
#[test]
fn a_grant_that_attenuates_something_else_still_permits_a_birth() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let mut shell = armed();
    let receipt = birth(
        &mut shell,
        "grant [fs: [read: ['/bin'], write: []]] \
         { detach #'a long sleep'# /bin/sleep 300 }",
    );
    let survivor = Survivor::of(&receipt);
    assert!(
        survivor.alive(),
        "a grant silent on detach must birth a living survivor"
    );
}

/// The survivor's three streams are `/dev/null`, never the host's own.  The
/// birth points them there by handing `Command` its stdio and forking a
/// second time from a `pre_exec` hook — which works only because `std` dup2s
/// the stdio *before* it runs those hooks.  If that order ever flips, the
/// second fork happens first and the survivor keeps the host's streams: a
/// pipe whose read end dies with the host, which is the hazard the whole
/// arrangement exists to avoid.  This test is what notices — the survivor
/// writes its marker to both streams, and neither of the host's may carry
/// it.
#[test]
fn the_survivor_writes_to_dev_null_because_std_dup2s_stdio_before_pre_exec_runs() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let (trace, host_out, host_err) = (
        dir.path().join("trace"),
        dir.path().join("host.out"),
        dir.path().join("host.err"),
    );
    let _survivor = host_that_births_and_exits(&trace, &host_out, &host_err);
    assert!(
        settles(|| std::fs::read_to_string(&trace).is_ok_and(|t| t.contains(MARKER))),
        "the survivor never ran: it wrote nothing to the file it keeps for itself"
    );

    for (stream, path) in [("stdout", &host_out), ("stderr", &host_err)] {
        let spilled = std::fs::read_to_string(path).unwrap();
        assert!(
            !spilled.contains(MARKER),
            "the survivor's bytes landed on the host's {stream}: {spilled}"
        );
    }
}

/// A user handler stacks on `detach`, and a `detach` call from inside its
/// body reaches the base frame under self-masking, birthing a real survivor
/// instead of recursing.
#[test]
fn stacked_detach_handler_forwards_to_base_frame_under_self_masking() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let mut shell = armed();
    let receipt = birth(
        &mut shell,
        "within [handlers: [detach: { |args| detach ...$args }]] \
         { detach #'through a stacked frame'# /bin/sleep 300 }",
    );
    let survivor = Survivor::of(&receipt);
    assert_eq!(receipt["desc"], "through a stacked frame");
    assert!(
        survivor.alive(),
        "the forwarded call must still birth a live survivor"
    );
}

/// `unalias` indexes run frames alone; `detach`'s base frame is not one, so
/// `unalias detach` refuses as for any name with no alias installed.
#[test]
fn unalias_detach_refuses_because_no_run_frame_holds_it() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let mut shell = armed();
    let message = refusal(&mut shell, "unalias detach");
    assert!(
        message.contains("no alias named"),
        "expected a no-alias refusal, got {message:?}"
    );
}

/// The single property the whole verb exists for.  The host is a real
/// process, it is fully gone — waited for, not merely told to leave — and
/// its survivor is not only alive but still working.
#[test]
fn a_detached_process_outlives_the_full_exit_of_the_host_that_birthed_it() {
    let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let (trace, host_out, host_err) = (
        dir.path().join("trace"),
        dir.path().join("host.out"),
        dir.path().join("host.err"),
    );
    let survivor = host_that_births_and_exits(&trace, &host_out, &host_err);
    assert!(
        survivor.alive(),
        "the host exited and took its survivor with it — the verb bought nothing"
    );

    let so_far = std::fs::metadata(&trace)
        .map(|m| m.len())
        .unwrap_or_default();
    assert!(
        settles(|| std::fs::metadata(&trace).is_ok_and(|m| m.len() > so_far)),
        "the survivor is alive but stopped working once its host was gone"
    );
    assert!(survivor.alive(), "and it is still there afterwards");
}
