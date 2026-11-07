use super::*;
use crate::fact::ExitCode as _;
use crate::process::CancelCause;
use crate::process::cancel::TestClock;
use crate::protocol::Ran;
use crate::shell::runtime::Escape;
use crate::source::SourceOrigin;
use crate::test_helper::SLEEP;
use std::sync::{Arc, Mutex};

/// The origin every test run is attributed to.
fn test_origin() -> SourceOrigin {
    SourceOrigin::named("<test>")
}

/// Install `name`, a builtin running `act` inside whichever run calls it,
/// with that run's mooring — how a test acts mid-run, as an engine-side
/// door does.
pub(crate) fn install_act(
    runtime: &mut crate::Runtime,
    name: &'static str,
    act: impl Fn(&Mooring, &mut Shell) + Send + Sync + 'static,
) {
    install_body(runtime, name, move |mooring, shell| {
        act(mooring, shell);
        Ok(Value::Unit)
    });
}

/// [`install_act`]'s sibling whose act halts the run with `halt`.
fn install_halting_act(
    runtime: &mut crate::Runtime,
    name: &'static str,
    halt: impl Fn(&Mooring) -> Break + Send + Sync + 'static,
) {
    install_body(runtime, name, move |mooring, _| Err(halt(mooring)));
}

fn install_body(
    runtime: &mut crate::Runtime,
    name: &'static str,
    body: impl Fn(&Mooring, &mut Shell) -> Settled<Value> + Send + Sync + 'static,
) {
    use crate::typecheck::builtins::{mk_scheme, pure, thunk};
    let entry = crate::shell::runtime::BuiltinEntry::new(
        std::borrow::Cow::Borrowed(name),
        |_| mk_scheme(&[], &[], thunk(pure(crate::ty::Ty::Unit))),
        "test-only: act from inside the run.",
        crate::shell::runtime::BuiltinBody::Captured(Arc::new(move |_, mooring, shell| {
            body(mooring, shell)
        })),
    );
    runtime.install_captured_builtins(&vec![entry].into());
}

/// An act striking its own run with `cause`, as a `Control` does.
fn strike(cause: crate::process::CancelCause) -> impl Fn(&Mooring, &mut Shell) + Send + Sync {
    move |mooring, _| mooring.home.cancel(cause)
}

#[test]
fn clean_run_settles_with_zero_status() {
    let mut shell = crate::test_helper::core_shell();
    let ending = shell
        .run(Run::captured("$[1 + 1]", test_origin()))
        .expect_ran();
    assert_eq!(ending.code(), 0);
    let result = ending.into_settled();
    assert!(result.is_ok(), "expected Ok, got {result:?}");
}

#[test]
fn a_last_run_drops_its_loose_workers_endings() {
    let mut runtime = crate::test_helper::core_runtime();
    runtime.install_builtins(&PANIC_BUILTINS_ARR);
    install_halting_act(&mut runtime, "foreign-stop", |_| {
        Break::stopped(CancelCause::Cancelled, None)
    });
    let mut shell = Shell::root(runtime.into(), crate::test_helper::process_cwd());
    for body in ["fail 'boom'", "core-panic-now", "exit 7", "foreign-stop"] {
        let report = shell.run(Run {
            last: true,
            ..Run::captured(
                format!("let h = spawn {{ {body} }}; echo end"),
                test_origin(),
            )
        });
        let RunReport::Ran {
            ending: End::Ok(_),
            captured: Some(captured),
            ..
        } = report
        else {
            panic!("a loose worker's `{body}` must not end its run");
        };
        assert_eq!(captured.stdout, b"end\n", "{body}");
        assert_eq!(captured.stderr.as_slice(), b"", "{body}");
    }
}

#[cfg(unix)]
#[test]
fn a_halting_last_run_does_not_wait() {
    let mut shell = crate::test_helper::core_shell();
    let started = std::time::Instant::now();
    let ending = shell
        .run(Run {
            last: true,
            ..Run::captured("let h = spawn { ^sleep 30 }; exit 3", test_origin())
        })
        .expect_ran();
    assert!(matches!(ending, End::Exited(3)), "{ending:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn a_run_that_is_not_last_does_not_wait() {
    let mut shell = crate::test_helper::core_shell();
    let report = shell.run(Run::captured(
        "let h = spawn 't' { echo late }; echo end",
        test_origin(),
    ));
    let RunReport::Ran {
        ending: End::Ok(_),
        captured: Some(captured),
        ..
    } = report
    else {
        panic!("a returning run must succeed");
    };
    assert_eq!(captured.stdout, b"end\n");
    assert_eq!(captured.stderr.as_slice(), b"");
}

#[test]
fn parse_failure_is_static() {
    let mut shell = crate::test_helper::core_shell();
    match shell.run(Run::captured("let = ", test_origin())) {
        RunReport::Static { diagnostics } => {
            assert!(
                matches!(
                    diagnostics,
                    StaticDiagnostics::Compile(Rejection { status, .. }) if status.get() == 2
                ),
                "expected a parse diagnostic"
            );
        }
        RunReport::Ran { .. } => {
            panic!("malformed source must be Static")
        }
    }
}

/// A hook's program is an already-compiled value, so `run_framed` has no
/// text to register. The registry never shrinks, so an entry naming the
/// empty string would stand for the session's whole life — one per prompt
/// draw, and one per keystroke under a `buffer-change` hook.
#[test]
fn a_hook_run_registers_no_source() {
    let mut shell = crate::test_helper::core_shell();
    shell.run(Run::captured("let body = { 1 }", test_origin()));
    let thunk = shell
        .scope_lookup("body")
        .cloned()
        .expect("body must be bound");
    let name = crate::shell::runtime::HookName::session("test_hook");
    shell
        .register_hook(
            name.clone(),
            thunk,
            crate::shell::runtime::HookSig::Block,
            crate::shell::runtime::DefaultPolicy::denied(),
        )
        .expect("register the hook");

    let before = shell.sources().next_id();
    let report = shell.run(Run {
        program: Program::Hook { name, args: vec![] },
        ..Run::captured("", test_origin())
    });
    assert!(
        matches!(report, RunReport::Ran { .. }),
        "the registered hook must run"
    );
    assert_eq!(
        shell.sources().next_id(),
        before,
        "a hook run must mint no source id"
    );
}

/// Register the block `src` as hook `name` under `policy`, and run it.
fn run_block_hook(
    shell: &mut Shell,
    name: crate::shell::runtime::HookName,
    src: &str,
    policy: crate::shell::runtime::DefaultPolicy,
) -> End {
    shell.run(Run::captured(
        format!("let hook_body = {src}"),
        test_origin(),
    ));
    let body = shell
        .scope_lookup("hook_body")
        .cloned()
        .expect("hook_body is bound");
    shell
        .register_hook(
            name.clone(),
            body,
            crate::shell::runtime::HookSig::Block,
            policy,
        )
        .expect("register the hook");
    let RunReport::Ran { ending, .. } = shell.run(Run {
        program: Program::Hook { name, args: vec![] },
        ..Run::captured("", test_origin())
    }) else {
        panic!("the registered hook must run");
    };
    ending
}

/// A hook run's fault names its hook, so no host needs a wrapper.
#[test]
fn a_hook_fault_names_its_hook() {
    let deny = crate::shell::runtime::DefaultPolicy::denied;
    for (name, label) in [
        (
            crate::shell::runtime::HookName::session("prompt"),
            "prompt: ",
        ),
        (
            crate::shell::runtime::HookName::plugin("p", "h"),
            "plugin 'p' hook 'h': ",
        ),
    ] {
        let mut shell = crate::test_helper::core_shell();
        let End::Err(error) = run_block_hook(&mut shell, name, "{ cd '' }", deny()) else {
            panic!("`cd ''` must raise");
        };
        assert_eq!(error.context.as_deref(), Some(label.trim_end_matches(": ")));
    }
}

/// An aside hook runs beside the session: its `cd` dies with it.
#[test]
fn an_aside_hook_leaves_the_session_where_it_was() {
    let dir = tempfile::tempdir().expect("a tempdir");
    for (policy, moved) in [
        (crate::shell::runtime::DefaultPolicy::denied(), true),
        (
            crate::shell::runtime::DefaultPolicy::denied().aside(),
            false,
        ),
    ] {
        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(dir.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );
        let start = shell.cwd();
        let name = crate::shell::runtime::HookName::session("buffer-change");
        let ending = run_block_hook(&mut shell, name, "{ cd .. }", policy);
        assert!(matches!(ending, End::Ok(_)), "{ending:?}");
        assert_eq!(shell.cwd() != start, moved);
    }
}

#[test]
fn type_failure_is_static() {
    let mut shell = crate::test_helper::core_shell();
    match shell.run(Run::captured("$[1 + true]", test_origin())) {
        RunReport::Static { diagnostics } => {
            assert!(
                matches!(
                    diagnostics,
                    StaticDiagnostics::Compile(Rejection { status, .. }) if status.get() == 1
                ),
                "expected type diagnostics"
            );
        }
        RunReport::Ran { .. } => {
            panic!("ill-typed source must be Static")
        }
    }
}

#[test]
fn exit_escape_reports_code() {
    let mut shell = crate::test_helper::core_shell();
    let ending = shell
        .run(Run::captured("exit 3", test_origin()))
        .expect_ran();
    assert_eq!(ending.code(), 3);
    let result = ending.into_settled();
    assert!(
        matches!(result, Err(Break::Escape(Escape::Exit(3)))),
        "expected Escape::Exit(3), got {result:?}"
    );
}

#[test]
fn capture_returns_stdout_bytes() {
    let mut shell = crate::test_helper::core_shell();
    match shell.run(Run::captured("echo hi", test_origin())) {
        RunReport::Ran { captured, .. } => {
            let captured = captured.expect("Capture must return buffers");
            assert!(
                String::from_utf8_lossy(&captured.stdout).contains("hi"),
                "captured stdout must hold the run's output, got {:?}",
                captured.stdout
            );
        }
        RunReport::Static { .. } => panic!("`echo hi` must reach evaluation"),
    }
}

/// A run's frame descends from the session anchor and dies with the run, so
/// the next top-level run hangs off the anchor and not off the last run's.
#[test]
fn a_settled_run_leaves_the_anchor_untouched() {
    let mut shell = crate::test_helper::core_shell();
    let pre = shell.session.anchor.clone();
    assert!(!pre.is_cancelled(), "pre-run anchor is live");

    let _ = shell.run(RunRequest {
        surface: Some(Arc::new(())),
        ..RunRequest::from(Run::captured("$[1 + 1]", test_origin()))
    });

    assert!(
        !shell.session.anchor.is_cancelled(),
        "a settled run must not cancel the anchor it hung off"
    );
    assert_eq!(
        shell.session.anchor.id(),
        pre.id(),
        "the anchor must still be the pre-run scope, not the run's own child"
    );
}

/// The host's own `Nursery` clone is empty afterward: teardown reaches
/// through the clone, not just the run's copy.
fn assert_nursery_emptied(panic: bool) {
    let mut runtime = crate::test_helper::core_runtime();

    let nursery = crate::shell::runtime::Nursery::default();
    let parked_id: Arc<Mutex<Option<crate::shell::runtime::NurseryId>>> =
        Arc::new(Mutex::new(None));
    let parked = parked_id.clone();
    install_act(&mut runtime, "park-fork", move |mooring, shell| {
        let Some(crate::shell::runtime::Fork::Park(nursery)) = mooring.fork() else {
            panic!("a nursery is installed on this run");
        };
        let id = nursery.park(shell.bequeath(crate::SpawnGrant::Inherit));
        *parked.lock().unwrap() = Some(id);
        assert!(
            !panic,
            "run-door test: deliberate panic after parking a fork"
        );
    });
    let mut shell = Shell::root(runtime.into(), crate::test_helper::process_cwd());

    let _ = shell.run(RunRequest {
        fork: Some(crate::shell::runtime::Fork::Park(nursery.clone())),
        ..RunRequest::from(Run::captured("park-fork", test_origin()))
    });

    let id = parked_id
        .lock()
        .unwrap()
        .expect("the run must fork into the nursery");
    assert!(
        nursery.adopt(id).is_none(),
        "a fork parked during the run and never adopted must not survive the run's teardown"
    );
}

#[test]
fn nursery_is_emptied_at_run_teardown() {
    assert_nursery_emptied(false);
    assert_nursery_emptied(true);
}

/// A settled worker whose result outlives the retention bound is reaped at a
/// run door, and its reap surfaces as a `` `notice `` at the next ready
/// boundary.
#[test]
fn ready_boundary_notice_surfaces_a_pending_retention_reap() {
    let mut shell = crate::test_helper::core_shell();
    shell.arm_worker_retention(1);
    let _ = shell.run(Run::captured("spawn { return 1 }", test_origin()));

    crate::test_helper::eventually(std::time::Duration::from_secs(2), || {
        shell
            .workers()
            .iter()
            .all(|e| e.handle.sample().has_ended())
            .then_some(())
    })
    .expect("the worker must settle within the budget");

    let _ = shell.run(Run::captured("$[1 + 1]", test_origin()));
    let notices = surfaced_notices(&mut shell, "$[1 + 1]");
    assert!(
        matches!(
            notices.as_slice(),
            [crate::shell::runtime::Notice::Reap(r)] if r.cause == crate::shell::runtime::ReapCause::Retention
        ),
        "the second call must surface the retention reap as a `notice`, got {notices:?}"
    );
}

/// An idle top-level name is pruned at the ready boundary of the run that
/// crosses its bound, and announced there exactly once.
#[test]
fn ready_boundary_notice_surfaces_an_idle_prune() {
    let mut shell = crate::test_helper::core_shell();
    shell.arm_binding_lease(crate::shell::runtime::BindingLease {
        idle_calls: 2,
        large_binding_bytes: u64::MAX,
    });

    let mut notices = surfaced_notices(&mut shell, "let idle_x = 1");
    notices.extend(surfaced_notices(&mut shell, "$[0]"));
    notices.extend(surfaced_notices(&mut shell, "$[0]"));
    let [crate::shell::runtime::Notice::Prune(pruned)] = notices.as_slice() else {
        panic!("exactly one prune across the bound, got {notices:?}");
    };
    assert!(
        matches!(pruned.as_slice(), [p] if p.name == "idle_x"),
        "the prune names the idle binding, got {pruned:?}"
    );

    assert!(
        surfaced_notices(&mut shell, "$[0]").is_empty(),
        "nothing left idle: no second prune"
    );
}

/// Run `src` under a capturing surface sink, answering the notices it pushed.
fn surfaced_notices(shell: &mut Shell, src: &str) -> Vec<crate::shell::runtime::Notice> {
    struct CapturingSink(Arc<Mutex<Vec<crate::data::FOValue>>>);
    impl crate::shell::runtime::EventSink for CapturingSink {
        fn emit(&self, ev: &crate::data::FOValue) {
            self.0.lock().unwrap().push(ev.clone());
        }
    }
    let captured: Arc<Mutex<Vec<crate::data::FOValue>>> = Arc::new(Mutex::new(Vec::new()));
    let _ = shell.run(RunRequest {
        surface: Some(Arc::new(CapturingSink(captured.clone()))),
        ..RunRequest::from(Run::captured(src, test_origin()))
    });
    let events = captured.lock().unwrap().clone();
    events
        .iter()
        .filter_map(crate::shell::runtime::Notice::from_surface)
        .collect()
}

/// An interrupt struck mid-run is read at the next poll point, so
/// `Shell::poll` unwinds the eval at 130.
#[test]
fn an_interrupt_mid_run_unwinds_the_eval() {
    let mut runtime = crate::test_helper::core_runtime();
    install_act(
        &mut runtime,
        "interrupt",
        strike(crate::process::CancelCause::Interrupted),
    );
    let mut shell = Shell::root(runtime.into(), crate::test_helper::process_cwd());
    let ending = shell
        .run(Run::captured(
            "interrupt\nlet rpv = 42\nreturn $rpv",
            test_origin(),
        ))
        .expect_ran();
    assert_eq!(
        ending.code(),
        130,
        "a foreground cancel observed mid-eval reports 130"
    );
    assert!(
        matches!(
            ending,
            End::Stopped(crate::shell::runtime::Stop {
                cause: crate::process::CancelCause::Interrupted,
                ..
            })
        ),
        "the cancel must unwind into a stop, got {ending:?}"
    );
}

/// A stop is no error, so `try` passes it and its handler never runs; the
/// run ends stopped, as the monotone scope's closing poll would have it.
#[test]
fn a_try_handler_cannot_settle_a_cancelled_run() {
    let mut runtime = crate::test_helper::core_runtime();
    install_act(
        &mut runtime,
        "interrupt",
        strike(crate::process::CancelCause::Interrupted),
    );
    let mut shell = Shell::root(runtime.into(), crate::test_helper::process_cwd());
    let ending = shell
        .run(Run::captured(
            "try { interrupt\nlet x = ()\nreturn $x } { |_| return () }",
            test_origin(),
        ))
        .expect_ran();
    assert_eq!(
        ending.code(),
        130,
        "a try handler must not settle a cancelled run at 0"
    );
    assert!(
        matches!(
            ending,
            End::Stopped(crate::shell::runtime::Stop {
                cause: crate::process::CancelCause::Interrupted,
                ..
            })
        ),
        "a try handler must not catch a stop, got {ending:?}"
    );
}

/// An aside — a separate `Shell` beside the session, as the REPL runs plugin
/// hooks ([`Shell::join_session`]) — can neither absorb an interrupt aimed
/// at the session's run nor withhold it: the aside mints its frames under
/// its own anchor, off the struck run's chain.
#[test]
fn an_aside_cannot_absorb_its_callers_interrupt() {
    let mut runtime = crate::test_helper::core_runtime();
    let aside: Arc<Mutex<Option<Shell>>> = Arc::default();
    let slot = aside.clone();
    let hook_status = Arc::new(Mutex::new(None));
    let status = hook_status.clone();
    install_act(&mut runtime, "interrupt-then-aside", move |mooring, _| {
        mooring
            .home
            .cancel(crate::process::CancelCause::Interrupted);
        let report = slot
            .lock()
            .unwrap()
            .as_mut()
            .expect("the aside is joined before the run")
            .run(Run::captured("let y = 1\nreturn $y", test_origin()));
        let ending = report.expect_ran();
        *status.lock().unwrap() = Some(ending.code());
    });
    let mut shell = Shell::root(runtime.into(), crate::test_helper::process_cwd());
    *aside.lock().unwrap() = Some(shell.join_session());
    let ending = shell
        .run(Run::captured(
            "interrupt-then-aside\nlet rpv = 42\nreturn $rpv",
            test_origin(),
        ))
        .expect_ran();
    assert_eq!(
        ending.code(),
        130,
        "the interrupt must survive the aside's entry and unwind the run it was aimed at"
    );
    assert_eq!(
        *hook_status.lock().unwrap(),
        Some(0),
        "while the aside's own run, off the struck chain, is deaf to it"
    );
}

/// The other half: an interrupt raised *while* the aside runs does unwind
/// it, so plugin code is interruptible for its whole life.
#[test]
fn an_aside_unwinds_on_an_interrupt_raised_while_it_runs() {
    let mut runtime = crate::test_helper::core_runtime();
    install_act(
        &mut runtime,
        "interrupt",
        strike(crate::process::CancelCause::Interrupted),
    );
    let shell = Shell::root(runtime.into(), crate::test_helper::process_cwd());
    let mut aside = shell.join_session();
    let ending = aside
        .run(Run::captured(
            "interrupt\nlet y = 1\nreturn $y",
            test_origin(),
        ))
        .expect_ran();
    assert_eq!(
        ending.code(),
        130,
        "a Ctrl-C during a hook must unwind the hook it lands in"
    );
}

/// [`Shell::cancel_handle`] — how a host stops a session it owns — does
/// reach the aside beside it, the one thing that can, because the handle
/// names the very root the aside shares.
#[test]
fn cancelling_a_session_by_handle_reaches_its_aside() {
    let shell = crate::test_helper::core_shell();
    let mut aside = shell.join_session();

    shell
        .cancel_handle()
        .cancel(crate::process::CancelCause::Cancelled);

    let ending = aside
        .run(Run::captured("let y = 1\nreturn $y", test_origin()))
        .expect_ran();
    assert_eq!(
        ending.code(),
        143,
        "the aside's run unwinds with the session"
    );
    assert!(
        matches!(
            ending,
            End::Stopped(crate::shell::runtime::Stop {
                cause: crate::process::CancelCause::Cancelled,
                ..
            })
        ),
        "the handle cancel must unwind the aside, got {ending:?}"
    );
}

/// No session folds the ambient causes: a signal reaches a run only as
/// the `Control` its host forwards, so one raised mid-run with nobody
/// listening leaves the eval undisturbed.
#[test]
fn a_session_is_deaf_to_the_ambient_causes() {
    let _serial = crate::process::cancel::REQUEST_SERIAL.lock();
    let mut runtime = crate::test_helper::core_runtime();
    install_act(&mut runtime, "raise-ambient-causes", |_, _| {
        crate::process::request_interrupt();
        crate::process::request_root_cancel(crate::process::CancelCause::Aborted);
    });
    let mut shell = Shell::root(runtime.into(), crate::test_helper::process_cwd());
    let report = shell.run(Run::captured(
        "raise-ambient-causes\nlet rpv = 42\nreturn $rpv",
        test_origin(),
    ));
    crate::process::cancel::clear_root_request();
    let ending = report.expect_ran();
    assert_eq!(ending.code(), 0, "an ambient cause must not reach the run");
    assert!(ending.into_settled().is_ok());
}

/// The act fires the `TimedOut` cause directly, pinning the `Stopped`
/// classification without sleeping on the reaper.
#[test]
fn a_walled_run_ends_stopped_timed_out() {
    let mut runtime = crate::test_helper::core_runtime();
    install_act(
        &mut runtime,
        "expire",
        strike(crate::process::CancelCause::TimedOut),
    );
    let mut shell = Shell::root(runtime.into(), crate::test_helper::process_cwd());
    let ending = shell
        .run(Run {
            wall: Some(std::time::Duration::from_secs(30)),
            ..Run::captured("expire\nlet rpv = 42\nreturn $rpv", test_origin())
        })
        .expect_ran();
    assert!(
        matches!(
            ending,
            End::Stopped(crate::shell::runtime::Stop {
                cause: crate::process::CancelCause::TimedOut,
                ..
            })
        ),
        "a TimedOut foreground cancel must end the run stopped"
    );
}

/// A real wall, so the cancel lands *during* the script: in
/// `a_walled_run_ends_stopped_timed_out` it fires ahead of every binding, so
/// none has landed there for the wall to be measured against.
#[test]
fn mid_script_wall_keeps_the_bindings_that_landed() {
    let mut shell = crate::test_helper::core_shell();
    let ending = shell
        .run(Run {
            wall: Some(std::time::Duration::from_millis(500)),
            ..Run::captured(
                format!("let pre_wall = 1\n{SLEEP} 30\nlet post_wall = 2"),
                test_origin(),
            )
        })
        .expect_ran();
    assert!(
        matches!(
            ending,
            End::Stopped(crate::shell::runtime::Stop {
                cause: crate::process::CancelCause::TimedOut,
                ..
            })
        ),
        "a wall cut mid-script must end the run stopped, got {ending:?}"
    );
    assert!(
        shell.scope_lookup("pre_wall").is_some(),
        "the `let` that landed before the wall stays bound in the session"
    );
    assert!(
        shell.scope_lookup("post_wall").is_none(),
        "the `let` after the wall never ran"
    );
}

/// Under `RunIo::Inherit` the guard restores the session's stdout sink to
/// the *same* object it was before the run, and the run's output lands in
/// it.
#[test]
fn inherit_leaves_session_streams_untouched() {
    let mut shell = crate::test_helper::core_shell();
    let (marker_sink, marker) = crate::io::new_buffer();
    shell.io.stdout = marker_sink;

    let _ = shell.run(Run {
        io: RunIo::Inherit,
        ..Run::captured("echo hi", test_origin())
    });

    assert!(
        matches!(&shell.io.stdout, Sink::Buffer(b) if Arc::ptr_eq(b, &marker)),
        "Inherit must restore the session's stdout sink after the run"
    );
    let written = marker.peek();
    assert!(
        !written.is_empty(),
        "the run's stdout must land in the inherited sink"
    );
    assert!(
        String::from_utf8_lossy(&written).contains("hi"),
        "the inherited sink must receive the run's output"
    );
}

// ── Run-door durability ──────────────────────────────────────────
//
// A panic anywhere in the run reports as a failed run with the shell's
// fields already rolled back to their run-entry state.

/// Stands in for any Rust panic the evaluator can raise mid-run.
fn builtin_panic_now(
    _args: &[crate::shell::runtime::Value],
    _mooring: &Mooring,
    _shell: &mut Shell,
) -> crate::shell::runtime::Settled<crate::shell::runtime::Value> {
    panic!("run-door test: deliberate mid-eval panic");
}

fn scheme_panic_now(_u: &mut crate::typecheck::Unifier) -> crate::ty::Scheme {
    use crate::typecheck::builtins::{mk_scheme, pure, thunk};
    mk_scheme(&[], &[], thunk(pure(crate::ty::Ty::Unit)))
}

static PANIC_BUILTINS_ARR: [crate::shell::runtime::BuiltinEntry; 1] =
    [crate::shell::runtime::BuiltinEntry::new(
        std::borrow::Cow::Borrowed("core-panic-now"),
        scheme_panic_now,
        "test-only: panic the evaluator mid-run.",
        crate::shell::runtime::BuiltinBody::Static(builtin_panic_now),
    )];
static PANIC_BUILTINS: &[crate::shell::runtime::BuiltinEntry] = &PANIC_BUILTINS_ARR;

fn panicking_shell() -> Shell {
    crate::HostSurface {
        statics: vec![PANIC_BUILTINS],
        ..crate::HostSurface::default()
    }
    .shell(
        crate::io::terminal::TerminalState::default(),
        crate::test_helper::process_cwd(),
    )
    .expect("a test host resolves its baseline")
}

/// Rollback is to the run's entry, not to session birth: the panicking run's
/// partial binding goes, a pre-run one stays, and the shell runs on.
#[test]
fn panicking_run_reports_panic_and_rolls_back() {
    let mut shell = panicking_shell();

    let ending = shell
        .run(Run::captured("let pre_panic = 1", test_origin()))
        .expect_ran();
    assert!(matches!(ending, End::Ok(_)));

    match shell.run(Run::captured(
        "let mid_panic = 2\ncore-panic-now",
        test_origin(),
    )) {
        RunReport::Ran {
            ending: End::Panic(text),
            captured: None,
            compact: None,
            trail,
            ..
        } => {
            assert_eq!(text, "run-door test: deliberate mid-eval panic");
            assert_eq!(trail, []);
        }
        _ => panic!("a panicking run must report a panic ending"),
    }

    assert!(
        shell.scope_lookup("pre_panic").is_some(),
        "a pre-run binding must survive a later run's panic"
    );
    assert!(
        shell.scope_lookup("mid_panic").is_none(),
        "the panicking run's own partial binding must be rolled back"
    );

    let ending = shell
        .run(Run::captured("$pre_panic", test_origin()))
        .expect_ran();
    assert!(
        matches!(ending, End::Ok(_)),
        "the next run must evaluate clean on the same shell"
    );
}

/// A host-supplied handler panicking mid-enquiry unwinds back through the
/// run, so the door catches it like any other.
struct PanickingDesk;
impl crate::shell::runtime::EnquiryDesk for PanickingDesk {
    fn enquire(
        &self,
        _req: crate::data::FOValue,
        _cancel: &crate::process::Scope,
    ) -> Settled<crate::data::FOValue> {
        panic!("run-door test: desk handler panic");
    }
}

#[test]
fn desk_handler_panic_is_caught_at_the_door() {
    let mut runtime = crate::test_helper::core_runtime();
    install_act(&mut runtime, "enquire", |mooring, shell| {
        let _ = shell.enquire(mooring, crate::data::FOValue::Unit);
    });
    let mut shell = Shell::root(runtime.into(), crate::test_helper::process_cwd());

    match shell.run(RunRequest {
        desk: Some(Arc::new(PanickingDesk)),
        ..RunRequest::from(Run::captured("let desk_panic = 4\nenquire", test_origin()))
    }) {
        RunReport::Ran {
            ending: End::Panic(text),
            captured: None,
            trail,
            ..
        } => {
            assert_ne!(text, "");
            assert_eq!(trail, []);
        }
        _ => panic!("a desk-handler panic must report a panic ending"),
    }
    assert!(
        shell.scope_lookup("desk_panic").is_none(),
        "the enquiring run's binding must be rolled back with the rest"
    );
}

// ── where a runtime error says it happened ───────────────────────────

/// The rendered runtime error of a run that must fault.
fn rendered_fault(shell: &mut Shell, src: &str) -> String {
    let report = shell
        .run(Run::captured(src, test_origin()))
        .into_report(shell);
    let crate::protocol::Report::Ran(Ran {
        ending, rendered, ..
    }) = report
    else {
        panic!("{src:?} must reach evaluation");
    };
    assert!(
        matches!(
            ending,
            crate::protocol::Ending::Err(_) | crate::protocol::Ending::Stopped(_)
        ),
        "{src:?} must fault, got {ending:?}"
    );
    rendered
}

/// A result the wire cannot carry names what it is or holds, and the
/// remedy that kind of value suggests.
#[test]
fn a_result_that_is_not_data_says_what_it_holds() {
    let mut shell = crate::test_helper::core_shell();
    for (src, says, hint) in [
        ("spawn { echo hi }", "the result is a handle", "let h ="),
        ("{ echo hi }", "the result is a block", "!{ … }"),
        ("{ |x| echo $x }", "the result is a function", "arguments"),
        (
            "[n: 1, h: !{spawn { echo hi }}]",
            "the result holds a handle",
            "let h =",
        ),
        ("[k: { echo hi }]", "the result holds a block", "!{ … }"),
    ] {
        let report = shell
            .run(Run::captured(src, test_origin()))
            .into_report(&shell);
        let crate::protocol::Report::Ran(Ran {
            ending, rendered, ..
        }) = report
        else {
            panic!("{src:?} must reach evaluation");
        };
        let crate::protocol::Ending::Err(_) = &ending else {
            panic!("{src:?} must fail at the boundary, got {ending:?}");
        };
        assert!(rendered.contains(says), "{src:?}: {rendered:?}");
        assert!(rendered.contains(hint), "{src:?}: {rendered:?}");
        assert_eq!(ending.code(), 1, "{src:?} must not report success");
    }
}

/// A shell whose `_narrow` pushes an offline session ceiling and whose
/// `_narrowed` reports whether any ceiling holds.
fn narrowing_shell() -> Shell {
    use crate::authority::policy::Capabilities;
    use crate::shell::runtime::{BuiltinBody, BuiltinEntry};
    let entries: Arc<[BuiltinEntry]> = vec![
        BuiltinEntry::new(
            "_narrow".into(),
            crate::typecheck::builtins::scheme::pure_bool,
            "",
            BuiltinBody::Captured(Arc::new(|_, _, s: &mut Shell| {
                s.push_session_capabilities(crate::test_helper::layer(Capabilities {
                    net: Some(crate::authority::policy::Net::Offline),
                    ..Capabilities::default()
                }));
                Ok(Value::Bool(true))
            })),
        ),
        BuiltinEntry::new(
            "_narrowed".into(),
            crate::typecheck::builtins::scheme::pure_bool,
            "",
            BuiltinBody::Captured(Arc::new(|_, _, s: &mut Shell| {
                Ok(Value::Bool(s.has_active_capabilities()))
            })),
        ),
    ]
    .into();
    crate::HostSurface {
        captured: vec![entries],
        ..crate::HostSurface::default()
    }
    .shell(
        crate::io::terminal::TerminalState::default(),
        crate::test_helper::process_cwd(),
    )
    .expect("a test host resolves its baseline")
}

/// Run `narrow`, then assert its session ceiling alone outlived it and
/// narrows the next run.
fn assert_ceiling_survives(mut shell: Shell, narrow: Run) {
    let before = shell.context.grants.len();
    assert!(matches!(shell.run(narrow), RunReport::Ran { .. }));
    assert_eq!(
        shell.context.grants.len(),
        before + 1,
        "the run's own frames must leave with it, and only they"
    );
    assert_eq!(
        shell.context.grants.net(),
        Some(crate::authority::policy::Net::Offline)
    );
    let RunReport::Ran {
        ending: End::Ok(value),
        ..
    } = shell.run(Run::captured("_narrowed", test_origin()))
    else {
        panic!("the probe run must settle");
    };
    assert!(
        matches!(value, Value::Bool(true)),
        "the ceiling must narrow the next run"
    );
}

/// The same inside a `grant { }` block: its frame leaves by position, not
/// by count, so the ceiling pushed above it stays.
#[test]
fn a_session_ceiling_pushed_in_a_grant_block_outlives_it() {
    assert_ceiling_survives(
        narrowing_shell(),
        Run::captured("grant [net: `offline] { _narrow }", test_origin()),
    );
}

fn caught_record(shell: &mut Shell, source: &str) -> crate::fact::ErrorRecord {
    use crate::data::datum::Datum as _;
    let End::Ok(value) = shell.run(Run::captured(source, test_origin())).expect_ran() else {
        panic!("the handler must return the error record: {source}");
    };
    crate::fact::ErrorRecord::decode(&crate::data::FOValue::try_from(&value).expect("data"))
        .expect("error record")
}

#[test]
fn an_invisible_prelude_failure_names_and_sites_the_users_call() {
    let mut shell = crate::test_helper::fresh_shell();
    let record = caught_record(&mut shell, "try { last [] } { |e| return $e }");
    assert_eq!(record.frame(), "last");
    assert_eq!(record.site().script.as_ref(), "<test>");
    assert_eq!((record.site().line, record.site().col), (1, 7));
}

#[test]
fn a_private_call_is_as_visible_as_any_other_source_frame() {
    let mut shell = crate::test_helper::fresh_shell();
    let record = caught_record(
        &mut shell,
        "let _last = $last\ntry { _last [] } { |e| return $e }",
    );
    assert_eq!(record.frame(), "_last");
    assert_eq!((record.site().line, record.site().col), (2, 7));
}

#[test]
fn index_refusals_name_the_operation_inside_and_outside_a_function() {
    for (source, line, col) in [
        (
            "let xs = [1]\ntry { let _ = return $xs[3]; fail 'unexpected success' } { |e| return $e }",
            2,
            22,
        ),
        (
            "let f = { |xs| return $xs[3] }\ntry { let _ = f [1]; fail 'unexpected success' } { |e| return $e }",
            1,
            23,
        ),
    ] {
        let mut shell = crate::test_helper::core_shell();
        let record = caught_record(&mut shell, source);
        assert_eq!(record.frame(), "index", "{source}");
        assert_eq!(
            (record.site().line, record.site().col),
            (line, col),
            "{source}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_direct_external_pipeline_stage_keeps_its_head_and_site() {
    let mut shell = crate::test_helper::core_shell();
    let record = caught_record(
        &mut shell,
        "try { let _ = !{ ^sh -c 'exit 7' | ^cat }; fail 'unexpected success' } { |e| return $e }",
    );
    assert_eq!(
        record.reason(),
        &crate::fact::Reason::process("sh", crate::fact::Failure::Exited(7))
    );
    assert_eq!(record.frame(), "sh");
    assert_eq!((record.site().line, record.site().col), (1, 18));
}

#[cfg(unix)]
#[test]
fn an_invisible_external_pipeline_stage_keeps_its_visible_caller() {
    let mut shell = crate::test_helper::core_shell();
    crate::shell::evaluator::run_source("let hidden-pipe = { ^sh -c 'exit 7' | ^cat }", &mut shell)
        .expect("invisible library");
    let record = caught_record(
        &mut shell,
        "try { let _ = hidden-pipe; fail 'unexpected success' } { |e| return $e }",
    );
    assert_eq!(
        record.reason(),
        &crate::fact::Reason::process("sh", crate::fact::Failure::Exited(7))
    );
    assert_eq!(record.frame(), "hidden-pipe");
    assert_eq!((record.site().line, record.site().col), (1, 15));
}

#[test]
fn named_invisible_blocks_keep_their_call_frames_with_and_without_arguments() {
    for (source, name, line) in [
        ("try { hidden-zero } { |e| return $e }", "hidden-zero", 1),
        ("try { hidden-arg 0 } { |e| return $e }", "hidden-arg", 1),
        (
            "let local-hidden = $hidden-zero\ntry { local-hidden } { |e| return $e }",
            "local-hidden",
            2,
        ),
        (
            "let local-hidden = $hidden-arg\ntry { local-hidden 0 } { |e| return $e }",
            "local-hidden",
            2,
        ),
    ] {
        let mut shell = crate::test_helper::fresh_shell();
        crate::shell::evaluator::run_source(
            "let hidden-zero = { last [] }\nlet hidden-arg = { |x| last [] }",
            &mut shell,
        )
        .expect("invisible library");
        let before = shell.local.audit.call_site().clone();
        let record = caught_record(&mut shell, source);
        assert_eq!(record.frame(), name, "{source}");
        assert_eq!(record.site().script.as_ref(), "<test>", "{source}");
        assert_eq!(
            (record.site().line, record.site().col),
            (line, 7),
            "{source}"
        );
        assert_eq!(shell.local.audit.call_site(), &before, "{source}");
    }
}

#[test]
fn a_visible_fail_names_and_sites_itself_even_in_a_private_function() {
    let mut shell = crate::test_helper::core_shell();
    let record = caught_record(
        &mut shell,
        "let _boom = { |x| fail 'nope' }\ntry { _boom 1 } { |e| return $e }",
    );
    assert_eq!(record.frame(), "fail");
    assert_eq!((record.site().line, record.site().col), (1, 19));
}

#[test]
fn completed_sibling_calls_do_not_leak_their_provenance() {
    let mut shell = crate::test_helper::fresh_shell();
    let before = shell.local.audit.call_site().clone();
    let record = caught_record(
        &mut shell,
        "let f = { |x| return $x }\ntry { f 1; last [] } { |e| return $e }",
    );
    assert_eq!(record.frame(), "last");
    assert_eq!((record.site().line, record.site().col), (2, 12));
    assert_eq!(shell.local.audit.call_site(), &before);
}

#[test]
fn invisible_code_recovers_its_caller_after_a_visible_nested_call() {
    let mut shell = crate::test_helper::fresh_shell();
    shell
        .run(Run::captured(
            "let visible = { echo nested; return () }",
            test_origin(),
        ))
        .expect_ran();
    crate::shell::evaluator::run_source("let hidden = { visible; last [] }", &mut shell)
        .expect("compile invisible library code");
    let record = caught_record(&mut shell, "try { hidden } { |e| return $e }");
    assert_eq!(record.frame(), "hidden");
    assert_eq!((record.site().line, record.site().col), (1, 7));
}

#[test]
fn visible_tail_calls_keep_a_bounded_stack_and_restore_the_register() {
    let mut shell = crate::test_helper::core_shell();
    shell.set_stack_limit(8);
    let before = shell.local.audit.call_site().clone();
    let ending = shell
        .run(Run::captured(
            "let loop = { |n| if $[$n == 0] { return 0 } else { loop $[$n - 1] } }\nloop 1000",
            test_origin(),
        ))
        .expect_ran();
    assert!(matches!(ending, End::Ok(Value::Int(0))), "{ending:?}");
    assert_eq!(shell.local.audit.call_site(), &before);
}

#[test]
fn visible_non_tail_recursion_uses_the_computational_stack_budget() {
    let mut shell = crate::test_helper::core_shell();
    shell.set_stack_limit(30_000);
    let before = shell.local.audit.call_site().clone();
    let ending = shell
        .run(Run::captured(
            "let sum = { |n| if $[$n == 0] { return 0 } else { let prev = sum $[$n - 1]; return $[$n + $prev] } }\nsum 10000",
            test_origin(),
        ))
        .expect_ran();
    assert!(
        matches!(ending, End::Ok(Value::Int(50_005_000))),
        "{ending:?}"
    );
    assert_eq!(shell.local.audit.call_site(), &before);
}

#[test]
fn an_unclaimed_hook_failure_uses_its_origin_base() {
    use crate::shell::runtime::{DefaultPolicy, HookName, HookSig};
    let mut runtime = crate::test_helper::core_runtime();
    install_body(&mut runtime, "hook-refusal", |_, _| {
        Err(Error::new("hook refused").into())
    });
    let mut shell = Shell::root(runtime.into(), crate::test_helper::process_cwd());
    let name = HookName::session("origin-probe");
    let value = shell
        .scope_lookup("hook-refusal")
        .expect("native binding")
        .clone();
    shell
        .register_hook(name.clone(), value, HookSig::Block, DefaultPolicy::denied())
        .expect("hook registration");
    let origin = SourceOrigin::named("<hook-origin>");
    let run = Run {
        program: Program::Hook {
            name: name.clone(),
            args: vec![],
        },
        ..Run::captured("", origin)
    };
    let End::Err(error) = shell.run(run).expect_ran() else {
        panic!("hook refusal")
    };
    let record = error.record(&shell);
    assert_eq!(record.frame(), name.fault_label());
    assert_eq!(record.site().script.as_ref(), "<hook-origin>");
    assert_eq!((record.site().line, record.site().col), (1, 1));
}

/// A lambda compiled by one run and called by the next draws its caret into
/// the text that defined it: the registry only grows, so a run boundary
/// costs a value nothing of its origin.
#[test]
fn a_lambda_faults_against_the_run_that_compiled_it() {
    let mut shell = crate::test_helper::core_shell();
    assert!(matches!(
        shell.run(Run::captured(
            "let boom = { |x| fail 'nope' }",
            test_origin()
        )),
        RunReport::Ran {
            ending: End::Ok(_),
            ..
        }
    ));
    let rendered = crate::io::ansi::strip(&rendered_fault(&mut shell, "boom 1"));
    assert!(
        rendered.contains("fail 'nope' }") && rendered.contains("<test>:1:18"),
        "the caret must be drawn into the defining run's text:\n{rendered}"
    );
}

/// A fault in text the user can already see needs no header and no caret.
#[test]
fn a_single_command_faulting_in_its_own_text_renders_compact() {
    let mut shell = crate::test_helper::core_shell();
    let rendered = crate::io::ansi::strip(&rendered_fault(&mut shell, "no-such-command-xyz"));
    assert!(
        !rendered.contains('╭'),
        "a fault in the text on screen needs no caret:\n{rendered}"
    );
}

/// exarch reads `single_command` to say whether a non-zero exit aborted
/// anything after it, so the flag must keep reporting the input's shape.
#[test]
fn single_command_still_reports_the_input_shape() {
    let mut shell = crate::test_helper::core_shell();
    for (src, shape) in [
        ("no-such-command-xyz", true),
        ("no-such-command-xyz; true", false),
    ] {
        match shell.run(Run::captured(src, test_origin())) {
            RunReport::Ran {
                ending: End::Err(_),
                compact,
                ..
            } => {
                assert_eq!(compact.is_some(), shape, "{src:?} misreports its shape");
            }
            RunReport::Ran { ending, .. } => panic!("{src:?} must fault, got {ending:?}"),
            RunReport::Static { .. } => panic!("{src:?} must reach evaluation"),
        }
    }
}

// ── The dispatch's own trail ─────────────────────────────────────────

/// `Run.trail: Some` opens a scope at `enter`, closes it into the
/// `RunReport`, and `into_report` projects each observation through
/// `encode` onto the wire — the whole path a dispatching host takes.
#[test]
fn a_dispatch_trail_projects_and_round_trips_through_the_wire() {
    let mut shell = crate::test_helper::core_shell();
    let report = shell.run(Run {
        trail: Some(crate::shell::runtime::CapturePolicy::Off),
        ..Run::captured("/bin/echo hi", test_origin())
    });
    let crate::protocol::Report::Ran(Ran { trail, .. }) = report.into_report(&shell) else {
        panic!("valid source must reach evaluation");
    };
    assert!(
        !trail.is_empty(),
        "the dispatch's own trail must carry its one command"
    );
    let round_tripped = trail.iter().any(|fo| {
        let obs = crate::data::datum::Datum::decode(fo);
        matches!(
            obs,
            Ok(crate::shell::runtime::Observation {
                what: crate::shell::runtime::Observed::Command(_),
                ..
            })
        )
    });
    assert!(
        round_tripped,
        "a wire `FOValue` must decode back as the `Command` observation it was"
    );
}

#[test]
fn a_trailed_run_reports_the_deputy_prefixes_of_its_session_stack() {
    use crate::authority::policy::{Capabilities, ExecGrant, ExecKey, FsPolicy};
    use crate::fact::{Observed, Resource};
    use crate::path::FrozenPath;

    let tmp = tempfile::tempdir().unwrap();
    let dir = crate::test_helper::real(tmp.path());
    let prefix = FrozenPath::freeze(dir.as_path()).unwrap();
    let mut shell = crate::test_helper::core_shell();
    shell.push_session_capabilities(crate::test_helper::layer(Capabilities {
        exec: Some(ExecGrant::of([ExecKey::Dir(prefix.clone())], [])),
        fs: Some(FsPolicy {
            write_prefixes: vec![prefix],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    }));
    let RunReport::Ran { trail, .. } = shell.run(Run {
        trail: Some(crate::shell::runtime::CapturePolicy::Off),
        ..Run::captured("$[1 + 1]", test_origin())
    }) else {
        panic!("valid source must reach evaluation");
    };
    assert!(
        trail.iter().any(|obs| matches!(
            &obs.what,
            Observed::Check(check)
                if check.resource == Resource::Deputy
                    && check.fields.get("prefix") == Some(&crate::path::shown(dir.as_path()))
        )),
        "the run's trail must report its session's deputy prefix: {trail:?}"
    );
}

/// `Run.trail: None` neither opens a scope nor collects one — the REPL's
/// choice, and every other test's, `Run::captured` included.
#[test]
fn a_dispatch_asking_no_trail_reports_none() {
    let mut shell = crate::test_helper::core_shell();
    match shell.run(Run::captured("$[1 + 1]", test_origin())) {
        RunReport::Ran { trail, .. } => assert!(
            trail.is_empty(),
            "an unasked dispatch must report no trail at all"
        ),
        RunReport::Static { .. } => panic!("valid source must reach evaluation"),
    }
}

/// A panicked dispatch is excluded from the trail law by declaration: it
/// reports `Panic`, and the scope `enter` held — outside the
/// `catch_unwind` the panic escapes through — still drains and closes,
/// discarded rather than attached. The next dispatch that asks opens a
/// fresh scope and sees nothing left over.
#[test]
fn a_panicked_dispatch_reports_panic_and_leaves_the_next_trail_empty() {
    let mut shell = panicking_shell();

    match shell.run(Run {
        trail: Some(crate::shell::runtime::CapturePolicy::Off),
        ..Run::captured("core-panic-now", test_origin())
    }) {
        RunReport::Ran {
            ending: End::Panic(text),
            captured: None,
            trail,
            ..
        } => {
            assert_ne!(text, "");
            assert_eq!(trail, []);
        }
        _ => panic!("a panicking dispatch must report a panic ending"),
    }

    match shell.run(Run {
        trail: Some(crate::shell::runtime::CapturePolicy::Off),
        ..Run::captured("$[1 + 1]", test_origin())
    }) {
        RunReport::Ran { trail, .. } => assert!(
            trail.is_empty(),
            "the panicked dispatch's scope must not leak into the next one"
        ),
        RunReport::Static { .. } => panic!("valid source must reach evaluation"),
    }
}

// ── delimiters are handlers for `poll` ───────────────────────────────

/// A run's report over a runtime, its ending and captured stdout.
fn ran(runtime: crate::Runtime, run: Run) -> (End, String) {
    match Shell::root(runtime.into(), crate::test_helper::process_cwd()).run(run) {
        RunReport::Ran {
            ending, captured, ..
        } => {
            let stdout = captured.map(|c| c.stdout).unwrap_or_default();
            (ending, String::from_utf8_lossy(&stdout).into_owned())
        }
        RunReport::Static { .. } => panic!("valid source must reach evaluation"),
    }
}

fn captured_with_grace(src: &str, grace: std::time::Duration) -> Run {
    Run {
        cleanup_grace: grace,
        ..Run::captured(src, test_origin())
    }
}

fn is_stopped(ending: &End, expected: CancelCause) -> bool {
    matches!(ending, End::Stopped(crate::shell::runtime::Stop { cause, .. }) if *cause == expected)
}

/// A runtime with `strike`, `strike-aborted` and `mark`, and the flag `mark` sets.
fn marking_runtime() -> (crate::Runtime, Arc<Mutex<bool>>) {
    let mut runtime = crate::test_helper::core_runtime();
    install_act(&mut runtime, "strike", strike(CancelCause::Interrupted));
    install_act(&mut runtime, "strike-aborted", strike(CancelCause::Aborted));
    let marked = Arc::new(Mutex::new(false));
    let flag = marked.clone();
    install_act(&mut runtime, "mark", move |_, _| {
        *flag.lock().unwrap() = true;
    });
    (runtime, marked)
}

/// A stop no condition stands behind, so a frame that caught it would run
/// on unobstructed: the witness for law 1's third equation.
fn unstruck_runtime() -> crate::Runtime {
    let mut runtime = crate::test_helper::core_runtime();
    install_halting_act(&mut runtime, "stop-unstruck", |_| {
        Break::stopped(CancelCause::Interrupted, None)
    });
    runtime
}

#[test]
fn a_try_cannot_see_a_stop() {
    let (ending, out) = ran(
        unstruck_runtime(),
        Run::captured(
            "try { stop-unstruck\necho after } { |e| echo handled }",
            test_origin(),
        ),
    );
    assert!(is_stopped(&ending, CancelCause::Interrupted), "{ending:?}");
    assert!(
        !out.contains("after") && !out.contains("handled"),
        "{out:?}"
    );
}

#[test]
fn an_audit_passes_a_stop() {
    let (ending, out) = ran(
        unstruck_runtime(),
        Run::captured("audit { stop-unstruck\necho after }", test_origin()),
    );
    assert!(is_stopped(&ending, CancelCause::Interrupted), "{ending:?}");
    assert!(!out.contains("after"), "{out:?}");
}

#[test]
fn a_fallback_passes_a_stop() {
    let (ending, out) = ran(
        unstruck_runtime(),
        Run::captured(
            "!{ stop-unstruck\nfail 'x' } ? echo fallback",
            test_origin(),
        ),
    );
    assert!(is_stopped(&ending, CancelCause::Interrupted), "{ending:?}");
    assert!(!out.contains("fallback"), "{out:?}");
}

#[test]
fn a_run_ending_in_an_error_under_a_stop_ends_stopped() {
    let mut runtime = crate::test_helper::core_runtime();
    install_halting_act(&mut runtime, "strike-then-fail", |mooring| {
        mooring.home.cancel(CancelCause::Interrupted);
        crate::shell::runtime::sig("x")
    });
    let (ending, _) = ran(runtime, Run::captured("strike-then-fail", test_origin()));
    assert!(is_stopped(&ending, CancelCause::Interrupted), "{ending:?}");
}

#[test]
fn a_cleanup_runs_shielded_and_the_stop_follows() {
    let (runtime, marked) = marking_runtime();
    let (ending, _) = ran(
        runtime,
        Run::captured(
            format!("guard {{ strike }} {{ {SLEEP} 0.2\nmark }}"),
            test_origin(),
        ),
    );
    assert!(is_stopped(&ending, CancelCause::Interrupted), "{ending:?}");
    assert!(
        *marked.lock().unwrap(),
        "the cleanup must have run to its end"
    );
}

#[test]
fn a_cleanup_under_a_value_is_shielded_too() {
    let (runtime, marked) = marking_runtime();
    let (ending, _) = ran(
        runtime,
        Run::captured(
            format!("guard {{ return 1 }} {{ strike\n{SLEEP} 0.2\nmark }}"),
            test_origin(),
        ),
    );
    assert!(is_stopped(&ending, CancelCause::Interrupted), "{ending:?}");
    assert!(*marked.lock().unwrap());
}

/// 0.7 s from the strike is within the grace; 1.2 s from the cleanup's
/// start would not be.
#[test]
fn a_cleanup_struck_late_gets_the_grace() {
    let _clock = TestClock::enter(std::time::Instant::now());
    let (mut runtime, marked) = marking_runtime();
    install_act(&mut runtime, "advance-before", |_, _| {
        TestClock::advance(std::time::Duration::from_millis(500));
    });
    install_act(&mut runtime, "advance-after", |_, _| {
        TestClock::advance(std::time::Duration::from_millis(700));
    });
    let (ending, _) = ran(
        runtime,
        captured_with_grace(
            "guard { return 1 } { advance-before\nstrike\nadvance-after\nmark }",
            std::time::Duration::from_secs(1),
        ),
    );
    assert!(is_stopped(&ending, CancelCause::Interrupted), "{ending:?}");
    assert!(*marked.lock().unwrap());
}

#[test]
fn a_cleanup_that_outlives_its_grace_is_stopped() {
    let (runtime, marked) = marking_runtime();
    let started = std::time::Instant::now();
    let (ending, _) = ran(
        runtime,
        captured_with_grace(
            &format!("guard {{ strike }} {{ {SLEEP} 5\nmark }}"),
            std::time::Duration::from_millis(50),
        ),
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
    assert!(is_stopped(&ending, CancelCause::Interrupted), "{ending:?}");
    assert!(!*marked.lock().unwrap());
}

#[test]
fn an_abort_passes_a_cleanup() {
    let (runtime, marked) = marking_runtime();
    let started = std::time::Instant::now();
    let (ending, _) = ran(
        runtime,
        Run::captured(
            format!("guard {{ strike-aborted }} {{ {SLEEP} 5\nmark }}"),
            test_origin(),
        ),
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
    assert!(is_stopped(&ending, CancelCause::Aborted), "{ending:?}");
    assert!(!*marked.lock().unwrap());
}

/// The shield is cooperative (ADR 261016_a-cleanup-is-shielded-within-its-grace): a signalled child is a failure `try`
/// catches, the current scope carrying no cause to attribute it to.
#[cfg(unix)]
#[test]
fn a_signalled_child_under_a_shield_is_a_failure() {
    let (runtime, marked) = marking_runtime();
    let (ending, _) = ran(
        runtime,
        Run::captured(
            "guard { strike } { try { /bin/sh -c 'kill -INT $$' } { |e| mark } }",
            test_origin(),
        ),
    );
    assert!(is_stopped(&ending, CancelCause::Interrupted), "{ending:?}");
    assert!(
        *marked.lock().unwrap(),
        "the handler must have seen a failure"
    );
}

#[test]
fn the_scope_comes_back_after_a_cleanup() {
    for src in [
        "guard { return 1 } { echo in }\nstrike\necho after",
        "try { guard { fail 'x' } { echo in } } { |e|\nstrike\necho after }",
    ] {
        let (runtime, _) = marking_runtime();
        let (ending, out) = ran(runtime, Run::captured(src, test_origin()));
        assert!(
            is_stopped(&ending, CancelCause::Interrupted),
            "{src}: {ending:?}"
        );
        assert!(
            out.contains("in") && !out.contains("after"),
            "{src}: {out:?}"
        );
    }
}

fn install_shared_grace_acts(runtime: &mut crate::Runtime) -> Arc<Mutex<[bool; 2]>> {
    let ran = Arc::new(Mutex::new([false; 2]));
    let first = Arc::clone(&ran);
    install_act(runtime, "consume-first", move |_, _| {
        TestClock::advance(std::time::Duration::from_millis(600));
        first.lock().unwrap()[0] = true;
    });
    let second = Arc::clone(&ran);
    install_act(runtime, "cross-deadline", move |_, shell| {
        assert_eq!(shell.scope.poll(), Ok(()));
        TestClock::advance(std::time::Duration::from_millis(500));
        assert_eq!(
            shell.scope.poll().expect_err("grace expired").cause,
            CancelCause::Interrupted
        );
        second.lock().unwrap()[1] = true;
    });
    ran
}

/// Shared grace ends at 1 s; renewed grace would last until 1.6 s.
#[test]
fn nested_cleanups_share_one_grace() {
    let _clock = TestClock::enter(std::time::Instant::now());
    let (mut runtime, marked) = marking_runtime();
    let callbacks = install_shared_grace_acts(&mut runtime);
    let (ending, _) = ran(
        runtime,
        captured_with_grace(
            "guard { strike } { guard { consume-first } { cross-deadline\nmark } }",
            std::time::Duration::from_secs(1),
        ),
    );
    assert_eq!(*callbacks.lock().unwrap(), [true, true]);
    assert!(is_stopped(&ending, CancelCause::Interrupted), "{ending:?}");
    assert!(!*marked.lock().unwrap());
}

#[test]
fn successive_cleanups_share_one_grace() {
    let _clock = TestClock::enter(std::time::Instant::now());
    let (mut runtime, marked) = marking_runtime();
    let callbacks = install_shared_grace_acts(&mut runtime);
    let (ending, _) = ran(
        runtime,
        captured_with_grace(
            "guard { guard { strike } { consume-first } } { cross-deadline\nmark }",
            std::time::Duration::from_secs(1),
        ),
    );
    assert_eq!(*callbacks.lock().unwrap(), [true, true]);
    assert!(is_stopped(&ending, CancelCause::Interrupted), "{ending:?}");
    assert!(!*marked.lock().unwrap());
}

#[test]
fn an_exit_outranks_the_closing_poll() {
    let mut runtime = crate::test_helper::core_runtime();
    install_halting_act(&mut runtime, "strike-then-exit", |mooring| {
        mooring.home.cancel(CancelCause::Interrupted);
        Break::Escape(Escape::Exit(3))
    });
    let (ending, _) = ran(runtime, Run::captured("strike-then-exit", test_origin()));
    assert!(matches!(ending, End::Exited(3)), "{ending:?}");
}

#[test]
fn a_stop_before_an_exit_wins() {
    let (runtime, _) = marking_runtime();
    let (ending, _) = ran(runtime, Run::captured("strike\nexit 3", test_origin()));
    assert!(is_stopped(&ending, CancelCause::Interrupted), "{ending:?}");
}

#[test]
fn a_stronger_cause_lands_at_the_closing_poll() {
    let mut runtime = crate::test_helper::core_runtime();
    install_halting_act(&mut runtime, "strike-twice", |mooring| {
        mooring.home.cancel(CancelCause::Interrupted);
        let stop = mooring.home.poll().expect_err("the node was just struck");
        mooring.home.cancel(CancelCause::Aborted);
        stop.into()
    });
    let (ending, _) = ran(runtime, Run::captured("strike-twice", test_origin()));
    assert!(is_stopped(&ending, CancelCause::Aborted), "{ending:?}");
}

#[test]
fn the_closing_poll_keeps_a_located_stop() {
    let (runtime, _) = marking_runtime();
    let (ending, _) = ran(runtime, Run::captured("strike\necho x", test_origin()));
    assert!(
        matches!(
            ending,
            End::Stopped(crate::shell::runtime::Stop { at: Some(_), .. })
        ),
        "{ending:?}"
    );
}

#[test]
fn a_stage_panic_stops_its_siblings_and_is_reraised() {
    let mut runtime = crate::test_helper::core_runtime();
    install_act(&mut runtime, "boom", |_, _| panic!("stage panic"));
    let started = std::time::Instant::now();
    let report = Shell::root(runtime.into(), crate::test_helper::process_cwd()).run(Run::captured(
        "!{ each { |i| echo $i } !{range 1 1000000} } | !{ boom }",
        test_origin(),
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
    match report {
        RunReport::Ran {
            ending: End::Panic(text),
            captured: None,
            trail,
            ..
        } => {
            assert_eq!(text, "stage panic");
            assert_eq!(trail, []);
        }
        _ => panic!("a stage panic must report a panic ending"),
    }
}

/// A block's arity is its type, not its spelling: `{ on 'seen' }` takes the
/// one parameter `on 'seen'` still waits for, a block whose type is still a
/// variable fits, a binary lambda is no hook, and `{ add 1 }` is no prompt
/// block.
#[test]
fn a_hooks_arity_is_its_type() {
    let mut shell = crate::test_helper::core_shell();
    shell.run(Run::captured(
        "let on = { |tag event| return $tag }\n\
         let made = { on 'seen' }\n\
         let two = { |a b| return () }\n\
         let add = { |a b| return $[$a + $b] }\n\
         let inc = { add 1 }\n\
         let wrap = { |f| return { !$f } }\n\
         let held = wrap { |e| return () }",
        test_origin(),
    ));
    let bound = |name: &str| shell.scope_lookup(name).cloned().expect("bound by the run");
    let name = crate::shell::runtime::HookName::session("t");
    let chpwd = || crate::shell::runtime::HookSig::Hook {
        kind: "chpwd".into(),
    };
    let deny = crate::shell::runtime::DefaultPolicy::denied;
    assert!(
        shell
            .vet_hook(&name, bound("made"), chpwd(), deny())
            .is_ok()
    );
    assert!(
        shell
            .vet_hook(&name, bound("held"), chpwd(), deny())
            .is_ok()
    );
    assert!(matches!(
        shell.vet_hook(&name, bound("two"), chpwd(), deny()),
        Err(crate::shell::runtime::RegisterError::ArityMismatch {
            expected: 1,
            actual: 2,
            ..
        })
    ));
    assert!(matches!(
        shell.vet_hook(
            &name,
            bound("inc"),
            crate::shell::runtime::HookSig::Block,
            deny()
        ),
        Err(crate::shell::runtime::RegisterError::ArityMismatch {
            expected: 0,
            actual: 1,
            ..
        })
    ));
}
