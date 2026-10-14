use super::*;
use std::sync::{Arc, Mutex};

/// Install `name`, a builtin running `act` inside whichever run calls it,
/// with that run's mooring — how a test acts mid-run, as an engine-side
/// door does.
pub(crate) fn install_act(
    shell: &mut Shell,
    name: &'static str,
    act: impl Fn(&Mooring, &mut Shell) + Send + Sync + 'static,
) {
    use crate::typecheck::builtins::{mk_scheme, pure, thunk};
    let entry = crate::types::BuiltinEntry::new(
        std::borrow::Cow::Borrowed(name),
        |_| mk_scheme(&[], &[], thunk(pure(crate::ty::Ty::Unit))),
        "test-only: act from inside the run.",
        crate::types::BuiltinBody::Captured(Arc::new(move |_, mooring, shell| {
            act(mooring, shell);
            Ok(crate::types::Value::Unit)
        })),
    );
    shell.install_captured_builtins(&vec![entry].into());
}

/// An act striking its own run with `cause`, as a `Control` does.
fn strike(cause: crate::process::CancelCause) -> impl Fn(&Mooring, &mut Shell) + Send + Sync {
    move |mooring, _| mooring.cancel.cancel(cause)
}

#[test]
fn clean_run_settles_with_zero_status() {
    let mut shell = crate::test_helper::core_shell();
    match shell.run(Run::captured("$[1 + 1]", "<test>")) {
        RunReport::Ran { ending, .. } => {
            assert_eq!(ending.status(), 0);
            let result = ending.into_result();
            assert!(result.is_ok(), "expected Ok, got {result:?}");
        }
        RunReport::Static { .. } => panic!("clean source must not be Static"),
    }
}

#[test]
fn parse_failure_is_static() {
    let mut shell = crate::test_helper::core_shell();
    match shell.run(Run::captured("let = ", "<test>")) {
        RunReport::Static { diagnostics } => {
            assert!(
                matches!(
                    diagnostics,
                    StaticDiagnostics::Compile(Rejection { status: 2, .. })
                ),
                "expected a parse diagnostic"
            );
        }
        RunReport::Ran { .. } => panic!("malformed source must be Static"),
    }
}

/// A hook's program is an already-compiled value, so `run_framed` has no
/// text to register. The registry never shrinks, so an entry naming the
/// empty string would stand for the session's whole life — one per prompt
/// draw, and one per keystroke under a `buffer-change` hook.
#[test]
fn a_hook_run_registers_no_source() {
    let mut shell = crate::test_helper::core_shell();
    shell.run(Run::captured("let body = { 1 }", "<test>"));
    let thunk = shell
        .scope_lookup("body")
        .cloned()
        .expect("body must be bound");
    let name = crate::types::HookName::session("test_hook");
    shell
        .register_hook(
            name.clone(),
            thunk,
            crate::types::HookSig::Block,
            crate::types::DefaultPolicy::denied(),
        )
        .expect("register the hook");

    let before = shell.sources().next_id();
    let report = shell.run(Run {
        program: Program::Hook { name, args: vec![] },
        ..Run::captured("", "<test>")
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
    name: crate::types::HookName,
    src: &str,
    policy: crate::types::DefaultPolicy,
) -> Ending {
    shell.run(Run::captured(format!("let hook_body = {src}"), "<test>"));
    let body = shell
        .scope_lookup("hook_body")
        .cloned()
        .expect("hook_body is bound");
    shell
        .register_hook(name.clone(), body, crate::types::HookSig::Block, policy)
        .expect("register the hook");
    let RunReport::Ran { ending, .. } = shell.run(Run {
        program: Program::Hook { name, args: vec![] },
        ..Run::captured("", "<test>")
    }) else {
        panic!("the registered hook must run");
    };
    ending
}

/// A hook run's fault names its hook, so no host needs a wrapper.
#[test]
fn a_hook_fault_names_its_hook() {
    let deny = crate::types::DefaultPolicy::denied;
    for (name, label) in [
        (crate::types::HookName::session("prompt"), "prompt: "),
        (
            crate::types::HookName::plugin("p", "h"),
            "plugin 'p' hook 'h': ",
        ),
    ] {
        let mut shell = crate::test_helper::core_shell();
        let Ending::Raised { error, .. } = run_block_hook(&mut shell, name, "{ cd '' }", deny())
        else {
            panic!("`cd ''` must raise");
        };
        assert!(error.message.starts_with(label), "{}", error.message);
    }
}

/// An aside hook runs beside the session: its `cd` dies with it.
#[test]
fn an_aside_hook_leaves_the_session_where_it_was() {
    let dir = tempfile::tempdir().expect("a tempdir");
    for (policy, moved) in [
        (crate::types::DefaultPolicy::denied(), true),
        (crate::types::DefaultPolicy::denied().aside(), false),
    ] {
        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(dir.path().to_path_buf());
        let start = shell.cwd();
        let name = crate::types::HookName::session("buffer-change");
        let ending = run_block_hook(&mut shell, name, "{ cd .. }", policy);
        assert!(matches!(ending, Ending::Settled { .. }), "{ending:?}");
        assert_eq!(shell.cwd() != start, moved);
    }
}

#[test]
fn type_failure_is_static() {
    let mut shell = crate::test_helper::core_shell();
    match shell.run(Run::captured("$[1 + true]", "<test>")) {
        RunReport::Static { diagnostics } => {
            assert!(
                matches!(
                    diagnostics,
                    StaticDiagnostics::Compile(Rejection { status: 1, .. })
                ),
                "expected type diagnostics"
            );
        }
        RunReport::Ran { .. } => panic!("ill-typed source must be Static"),
    }
}

#[test]
fn exit_escape_reports_code() {
    let mut shell = crate::test_helper::core_shell();
    match shell.run(Run::captured("exit 3", "<test>")) {
        RunReport::Ran { ending, .. } => {
            assert_eq!(ending.status(), 3);
            let result = ending.into_result();
            assert!(
                matches!(result, Err(Break::Escape(Escape::Exit(3)))),
                "expected Escape::Exit(3), got {result:?}"
            );
        }
        RunReport::Static { .. } => panic!("`exit 3` must reach evaluation"),
    }
}

#[test]
fn capture_returns_stdout_bytes() {
    let mut shell = crate::test_helper::core_shell();
    match shell.run(Run::captured("echo hi", "<test>")) {
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
        ..RunRequest::from(Run::captured("$[1 + 1]", "<test>"))
    });

    assert!(
        !shell.session.anchor.is_cancelled(),
        "a settled run must not cancel the anchor it hung off"
    );
    shell
        .session
        .anchor
        .cancel(crate::process::CancelCause::Cancelled);
    assert!(
        pre.is_cancelled(),
        "the anchor must still be the pre-run scope, not the run's own child"
    );
}

/// The host's own `Nursery` clone is empty afterward: teardown reaches
/// through the clone, not just the run's copy.
fn assert_nursery_emptied(panic: bool) {
    let mut shell = crate::test_helper::core_shell();

    let nursery = crate::types::Nursery::default();
    let parked_id: Arc<Mutex<Option<crate::types::NurseryId>>> = Arc::new(Mutex::new(None));
    let parked = parked_id.clone();
    install_act(&mut shell, "park-fork", move |mooring, shell| {
        let Some(crate::types::Fork::Park(nursery)) = mooring.fork() else {
            panic!("a nursery is installed on this run");
        };
        let id = nursery.park(shell.bequeath(crate::SpawnGrant::Inherit));
        *parked.lock().unwrap() = Some(id);
        assert!(
            !panic,
            "run-door test: deliberate panic after parking a fork"
        );
    });

    let _ = shell.run(RunRequest {
        fork: Some(crate::types::Fork::Park(nursery.clone())),
        ..RunRequest::from(Run::captured("park-fork", "<test>"))
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

/// A worker the lease chain reaps *between* runs has no live sink until the
/// next run installs one, so its reap surfaces as a `` `notice `` there,
/// ordered before that run's own settling.
#[cfg_attr(
    ctor_without_argv,
    ignore = "a test binary serves bundled tools before main, where musl gives std no argv"
)]
#[test]
fn ready_boundary_notice_surfaces_a_pending_worker_reap() {
    let mut shell = crate::test_helper::core_shell();

    // Never polled, under a millisecond-scale idle lease, so the background
    // lease chain reaps it quickly.
    let mut req = Run::captured("spawn { sleep 10 }", "<test>");
    req.deferred_lease = Some(crate::types::WorkerLease {
        idle: std::time::Duration::from_millis(20),
        backstop: std::time::Duration::from_secs(10),
    });
    let _ = shell.run(req);

    crate::test_helper::eventually(std::time::Duration::from_secs(2), || {
        (shell.worker_count() == 0).then_some(())
    })
    .expect("the unpolled worker must be reaped within the budget");

    // The first live sink of the session: until this run, the pending reap
    // has nowhere to push through.
    let notices = surfaced_notices(&mut shell, "$[1 + 1]");
    assert!(
        matches!(notices.as_slice(), [crate::types::Notice::Reap(_)]),
        "the settled run must surface the pending reap as a `notice`, got {notices:?}"
    );
}

/// An idle top-level name is pruned at the ready boundary of the run that
/// crosses its bound, and announced there exactly once.
#[test]
fn ready_boundary_notice_surfaces_an_idle_prune() {
    let mut shell = crate::test_helper::core_shell();
    shell.arm_binding_lease(crate::types::BindingLease {
        idle_calls: 2,
        large_binding_bytes: u64::MAX,
    });

    let mut notices = surfaced_notices(&mut shell, "let idle_x = 1");
    notices.extend(surfaced_notices(&mut shell, "$[0]"));
    notices.extend(surfaced_notices(&mut shell, "$[0]"));
    let [crate::types::Notice::Prune(pruned)] = notices.as_slice() else {
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
fn surfaced_notices(shell: &mut Shell, src: &str) -> Vec<crate::types::Notice> {
    struct CapturingSink(Arc<Mutex<Vec<crate::first_order::FOValue>>>);
    impl crate::types::EventSink for CapturingSink {
        fn emit(&self, ev: &crate::first_order::FOValue) {
            self.0.lock().unwrap().push(ev.clone());
        }
    }
    let captured: Arc<Mutex<Vec<crate::first_order::FOValue>>> = Arc::new(Mutex::new(Vec::new()));
    let _ = shell.run(RunRequest {
        surface: Some(Arc::new(CapturingSink(captured.clone()))),
        ..RunRequest::from(Run::captured(src, "<test>"))
    });
    let events = captured.lock().unwrap().clone();
    events
        .iter()
        .filter_map(crate::types::Notice::from_surface)
        .collect()
}

/// An interrupt struck mid-run is read at the next poll point, so
/// `Mooring::check` unwinds the eval at 130.
#[test]
fn an_interrupt_mid_run_unwinds_the_eval() {
    let mut shell = crate::test_helper::core_shell();
    install_act(
        &mut shell,
        "interrupt",
        strike(crate::process::CancelCause::Interrupted),
    );
    match shell.run(Run::captured(
        "interrupt\nlet rpv = 42\nreturn $rpv",
        "<test>",
    )) {
        RunReport::Ran { ending, .. } => {
            assert_eq!(
                ending.status(),
                130,
                "a foreground cancel observed mid-eval reports 130"
            );
            let result = ending.into_result();
            assert!(
                matches!(result, Err(Break::Error(_))),
                "the cancel must unwind into a Break::Error, got {result:?}"
            );
        }
        RunReport::Static { .. } => panic!("valid source must reach evaluation"),
    }
}

/// `try` classifies a cancellation like any recoverable error, so a handler
/// that settles cleanly makes the outcome `Ok`, whose status is 0 — the
/// run boundary must re-poll the monotone scope before the status is
/// read, or a final `try { … } { |_| return () }` suppresses the
/// interrupt entirely.
#[test]
fn a_try_handler_cannot_settle_a_cancelled_run() {
    let mut shell = crate::test_helper::core_shell();
    install_act(
        &mut shell,
        "interrupt",
        strike(crate::process::CancelCause::Interrupted),
    );
    match shell.run(Run::captured(
        "try { interrupt\nlet x = ()\nreturn $x } { |_| return () }",
        "<test>",
    )) {
        RunReport::Ran { ending, .. } => {
            assert_eq!(
                ending.status(),
                130,
                "a try handler must not settle a cancelled run at 0"
            );
            match ending.into_result() {
                Err(Break::Error(e)) => assert_eq!(
                    e.message, "interrupted",
                    "the boundary poll must report the cancel cause's own words"
                ),
                other => panic!("the re-poll must unwind into a Break::Error, got {other:?}"),
            }
        }
        RunReport::Static { .. } => panic!("valid source must reach evaluation"),
    }
}

/// An aside — a separate `Shell` beside the session, as the REPL runs plugin
/// hooks ([`Shell::join_session`]) — can neither absorb an interrupt aimed
/// at the session's run nor withhold it: the aside mints its frames under
/// its own anchor, off the struck run's chain.
#[test]
fn an_aside_cannot_absorb_its_callers_interrupt() {
    let mut shell = crate::test_helper::core_shell();
    let aside = Mutex::new(shell.join_session());
    let hook_status = Arc::new(Mutex::new(None));
    let status = hook_status.clone();
    install_act(&mut shell, "interrupt-then-aside", move |mooring, _| {
        mooring
            .cancel
            .cancel(crate::process::CancelCause::Interrupted);
        let report = aside
            .lock()
            .unwrap()
            .run(Run::captured("let y = 1\nreturn $y", "<test>"));
        match report {
            RunReport::Ran { ending, .. } => *status.lock().unwrap() = Some(ending.status()),
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    });
    match shell.run(Run::captured(
        "interrupt-then-aside\nlet rpv = 42\nreturn $rpv",
        "<test>",
    )) {
        RunReport::Ran { ending, .. } => assert_eq!(
            ending.status(),
            130,
            "the interrupt must survive the aside's entry and unwind the run it was aimed at"
        ),
        RunReport::Static { .. } => panic!("valid source must reach evaluation"),
    }
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
    let shell = crate::test_helper::core_shell();
    let mut aside = shell.join_session();
    install_act(
        &mut aside,
        "interrupt",
        strike(crate::process::CancelCause::Interrupted),
    );
    match aside.run(Run::captured("interrupt\nlet y = 1\nreturn $y", "<test>")) {
        RunReport::Ran { ending, .. } => assert_eq!(
            ending.status(),
            130,
            "a Ctrl-C during a hook must unwind the hook it lands in"
        ),
        RunReport::Static { .. } => panic!("valid source must reach evaluation"),
    }
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

    match aside.run(Run::captured("let y = 1\nreturn $y", "<test>")) {
        RunReport::Ran { ending, .. } => {
            assert_eq!(
                ending.status(),
                143,
                "the aside's run unwinds with the session"
            );
            let result = ending.into_result();
            assert!(
                matches!(result, Err(Break::Error(_))),
                "the handle cancel must unwind the aside, got {result:?}"
            );
        }
        RunReport::Static { .. } => panic!("valid source must reach evaluation"),
    }
}

/// No session folds the ambient causes: a signal reaches a run only as
/// the `Control` its host forwards, so one raised mid-run with nobody
/// listening leaves the eval undisturbed.
#[test]
fn a_session_is_deaf_to_the_ambient_causes() {
    let _serial = crate::process::cancel::REQUEST_SERIAL.lock();
    let mut shell = crate::test_helper::core_shell();
    install_act(&mut shell, "raise", |_, _| {
        crate::process::request_interrupt();
        crate::process::request_root_cancel(crate::process::CancelCause::Aborted);
    });
    let report = shell.run(Run::captured("raise\nlet rpv = 42\nreturn $rpv", "<test>"));
    crate::process::cancel::clear_root_request();
    match report {
        RunReport::Ran { ending, .. } => {
            assert_eq!(
                ending.status(),
                0,
                "an ambient cause must not reach the run"
            );
            assert!(ending.into_result().is_ok());
        }
        RunReport::Static { .. } => panic!("valid source must reach evaluation"),
    }
}

/// The act fires the `Deadline` cause directly, pinning the `Walled`
/// classification without sleeping on the reaper.
#[test]
fn deadline_cancel_reports_walled() {
    let mut shell = crate::test_helper::core_shell();
    install_act(
        &mut shell,
        "expire",
        strike(crate::process::CancelCause::TimedOut),
    );
    match shell.run(Run {
        wall: Some(std::time::Duration::from_secs(30)),
        ..Run::captured("expire\nlet rpv = 42\nreturn $rpv", "<test>")
    }) {
        RunReport::Ran { ending, .. } => {
            assert!(
                matches!(ending, Ending::Walled { .. }),
                "a Deadline foreground cancel must report Walled"
            );
        }
        RunReport::Static { .. } => panic!("valid source must reach evaluation"),
    }
}

/// A real wall, so the cancel lands *during* the script: in
/// `deadline_cancel_reports_walled` it fires ahead of every binding, so
/// none has landed there for the wall to be measured against.
#[cfg_attr(
    ctor_without_argv,
    ignore = "a test binary serves bundled tools before main, where musl gives std no argv"
)]
#[test]
fn mid_script_wall_keeps_the_bindings_that_landed() {
    let mut shell = crate::test_helper::core_shell();
    match shell.run(Run {
        wall: Some(std::time::Duration::from_millis(500)),
        ..Run::captured("let pre_wall = 1\nsleep 30\nlet post_wall = 2", "<test>")
    }) {
        RunReport::Ran { ending, .. } => assert!(
            matches!(ending, Ending::Walled { .. }),
            "a wall cut mid-script must report Walled, got {ending:?}"
        ),
        RunReport::Static { .. } => panic!("valid source must reach evaluation"),
    }
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
        ..Run::captured("echo hi", "<test>")
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
    _args: &[crate::types::Value],
    _mooring: &Mooring,
    _shell: &mut Shell,
) -> crate::types::Settled<crate::types::Value> {
    panic!("run-door test: deliberate mid-eval panic");
}

fn scheme_panic_now(_u: &mut crate::typecheck::Unifier) -> crate::ty::Scheme {
    use crate::typecheck::builtins::{mk_scheme, pure, thunk};
    mk_scheme(&[], &[], thunk(pure(crate::ty::Ty::Unit)))
}

static PANIC_BUILTINS_ARR: [crate::types::BuiltinEntry; 1] = [crate::types::BuiltinEntry::new(
    std::borrow::Cow::Borrowed("core-panic-now"),
    scheme_panic_now,
    "test-only: panic the evaluator mid-run.",
    crate::types::BuiltinBody::Static(builtin_panic_now),
)];
static PANIC_BUILTINS: &[crate::types::BuiltinEntry] = &PANIC_BUILTINS_ARR;

/// Rollback is to the run's entry, not to session birth: the panicking run's
/// partial binding goes, a pre-run one stays, and the shell runs on.
#[test]
fn panicking_run_reports_failed_and_rolls_back() {
    let mut shell = crate::test_helper::core_shell();
    shell.install_builtins(PANIC_BUILTINS);

    match shell.run(Run::captured("let pre_panic = 1", "<test>")) {
        RunReport::Ran { ending, .. } => assert!(matches!(ending, Ending::Settled { .. })),
        RunReport::Static { .. } => panic!("the pre-run binding must evaluate"),
    }

    match shell.run(Run::captured("let mid_panic = 2\ncore-panic-now", "<test>")) {
        RunReport::Static {
            diagnostics: StaticDiagnostics::Host(e),
        } => {
            assert!(
                e.message.contains("run panicked"),
                "the report must name the panic, got {:?}",
                e.message
            );
            assert!(
                e.message.contains("deliberate mid-eval panic"),
                "the report must carry the panic payload, got {:?}",
                e.message
            );
        }
        _ => panic!("a panicking run must report Static{{Host}}"),
    }

    assert!(
        shell.scope_lookup("pre_panic").is_some(),
        "a pre-run binding must survive a later run's panic"
    );
    assert!(
        shell.scope_lookup("mid_panic").is_none(),
        "the panicking run's own partial binding must be rolled back"
    );

    match shell.run(Run::captured("$pre_panic", "<test>")) {
        RunReport::Ran { ending, .. } => {
            assert!(
                matches!(ending, Ending::Settled { .. }),
                "the next run must evaluate clean on the same shell"
            );
        }
        RunReport::Static { .. } => panic!("the healed shell must still evaluate"),
    }
}

/// A host-supplied handler panicking mid-enquiry unwinds back through the
/// run, so the door catches it like any other.
struct PanickingDesk;
impl crate::types::EnquiryDesk for PanickingDesk {
    fn enquire(
        &self,
        _req: crate::first_order::FOValue,
        _cancel: &crate::process::CancelScope,
    ) -> Result<crate::first_order::FOValue, crate::types::Error> {
        panic!("run-door test: desk handler panic");
    }
}

#[test]
fn desk_handler_panic_is_caught_at_the_door() {
    let mut shell = crate::test_helper::core_shell();
    install_act(&mut shell, "enquire", |mooring, shell| {
        let _ = shell.enquire(mooring, crate::first_order::FOValue::Unit);
    });

    match shell.run(RunRequest {
        desk: Some(Arc::new(PanickingDesk)),
        ..RunRequest::from(Run::captured("let desk_panic = 4\nenquire", "<test>"))
    }) {
        RunReport::Static {
            diagnostics: StaticDiagnostics::Host(e),
        } => assert!(e.message.contains("run panicked")),
        _ => panic!("a desk-handler panic must report Static{{Host}}"),
    }
    assert!(
        shell.scope_lookup("desk_panic").is_none(),
        "the enquiring run's binding must be rolled back with the rest"
    );
}

// ── where a runtime error says it happened ───────────────────────────

/// The rendered runtime error of a run that must fault.
fn rendered_fault(shell: &mut Shell, src: &str) -> String {
    let report = shell.run(Run::captured(src, "<test>")).into_report(shell);
    let crate::protocol::Report::Ran { ending, .. } = report else {
        panic!("{src:?} must reach evaluation");
    };
    match ending {
        crate::protocol::Ending::Raised { rendered, .. }
        | crate::protocol::Ending::Walled { rendered, .. } => rendered,
        other => panic!("{src:?} must fault, got {other:?}"),
    }
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
        let report = shell.run(Run::captured(src, "<test>")).into_report(&shell);
        let crate::protocol::Report::Ran { ending, .. } = report else {
            panic!("{src:?} must reach evaluation");
        };
        let crate::protocol::Ending::Unreturnable { rendered, .. } = &ending else {
            panic!("{src:?} must end unreturnable, got {ending:?}");
        };
        assert!(rendered.contains(says), "{src:?}: {rendered:?}");
        assert!(rendered.contains(hint), "{src:?}: {rendered:?}");
        assert_eq!(ending.status(), 1, "{src:?} must not report success");
    }
}

/// A shell whose `_narrow` pushes an offline session ceiling and whose
/// `_narrowed` reports whether any ceiling holds.
fn narrowing_shell() -> Shell {
    use crate::capability::Capabilities;
    use crate::types::{BuiltinBody, BuiltinEntry};
    let mut shell = crate::test_helper::core_shell();
    let entries: Arc<[BuiltinEntry]> = vec![
        BuiltinEntry::new(
            "_narrow".into(),
            crate::typecheck::builtins::scheme::pure_bool,
            "",
            BuiltinBody::Captured(Arc::new(|_, _, s: &mut Shell| {
                s.push_session_capabilities(Capabilities {
                    net: Some(crate::capability::Net::Offline),
                    ..Capabilities::root()
                });
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
    shell.install_captured_builtins(&entries);
    shell
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
        shell.context.grants.net().collect::<Vec<_>>(),
        [crate::capability::Net::Offline]
    );
    let RunReport::Ran {
        ending: Ending::Settled { value, .. },
        ..
    } = shell.run(Run::captured("_narrowed", "<test>"))
    else {
        panic!("the probe run must settle");
    };
    assert!(
        matches!(value, Value::Bool(true)),
        "the ceiling must narrow the next run"
    );
}

/// A session ceiling pushed inside a run outlives it and narrows the next,
/// while the run's own `Run.caps` frames leave with it.
#[test]
fn a_session_ceiling_pushed_in_a_run_outlives_it() {
    let mut caps = GrantStack::root();
    caps.push(crate::capability::Capabilities {
        shell: Some([crate::capability::ShellVerb::Chdir].into()),
        ..crate::capability::Capabilities::root()
    });
    let mut narrow = Run::captured("_narrow", "<test>");
    narrow.caps = caps;
    assert_ceiling_survives(narrowing_shell(), narrow);
}

/// The same inside a `grant { }` block: its frame leaves by position, not
/// by count, so the ceiling pushed above it stays.
#[test]
fn a_session_ceiling_pushed_in_a_grant_block_outlives_it() {
    assert_ceiling_survives(
        narrowing_shell(),
        Run::captured("grant [net: `offline] { _narrow }", "<test>"),
    );
}

/// A lambda compiled by one run and called by the next draws its caret into
/// the text that defined it: the registry only grows, so a run boundary
/// costs a value nothing of its origin.
#[test]
fn a_lambda_faults_against_the_run_that_compiled_it() {
    let mut shell = crate::test_helper::core_shell();
    assert!(matches!(
        shell.run(Run::captured(
            "let boom = { |x| fail [status: 1, message: nope] }",
            "<test>"
        )),
        RunReport::Ran {
            ending: Ending::Settled { .. },
            ..
        }
    ));
    let rendered = crate::ansi::strip(&rendered_fault(&mut shell, "boom 1"));
    assert!(
        rendered.contains("message: nope] }"),
        "the caret must be drawn into the defining run's text:\n{rendered}"
    );
}

/// A fault in text the user can already see needs no header and no caret.
#[test]
fn a_single_command_faulting_in_its_own_text_renders_compact() {
    let mut shell = crate::test_helper::core_shell();
    let rendered = crate::ansi::strip(&rendered_fault(&mut shell, "no-such-command-xyz"));
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
        match shell.run(Run::captured(src, "<test>")) {
            RunReport::Ran {
                ending: Ending::Raised { compact, .. },
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
        trail: Some(crate::types::CapturePolicy::Off),
        ..Run::captured("/bin/echo hi", "<test>")
    });
    let crate::protocol::Report::Ran { trail, .. } = report.into_report(&shell) else {
        panic!("valid source must reach evaluation");
    };
    assert!(
        !trail.is_empty(),
        "the dispatch's own trail must carry its one command"
    );
    let round_tripped = trail.iter().any(|fo| {
        let obs = crate::first_order::datum::Datum::decode(fo);
        matches!(
            obs,
            Ok(crate::types::Observation {
                what: crate::types::Observed::Command(_),
                ..
            })
        )
    });
    assert!(
        round_tripped,
        "a wire `FOValue` must decode back as the `Command` observation it was"
    );
}

/// `Run.trail: None` neither opens a scope nor collects one — the REPL's
/// choice, and every other test's, `Run::captured` included.
#[test]
fn a_dispatch_asking_no_trail_reports_none() {
    let mut shell = crate::test_helper::core_shell();
    match shell.run(Run::captured("$[1 + 1]", "<test>")) {
        RunReport::Ran { trail, .. } => assert!(
            trail.is_empty(),
            "an unasked dispatch must report no trail at all"
        ),
        RunReport::Static { .. } => panic!("valid source must reach evaluation"),
    }
}

/// A panicked dispatch is excluded from the trail law by declaration: it
/// reports `Static`, and the scope `enter` held — outside the
/// `catch_unwind` the panic escapes through — still drains and closes,
/// discarded rather than attached. The next dispatch that asks opens a
/// fresh scope and sees nothing left over.
#[test]
fn a_panicked_dispatch_reports_static_and_leaves_the_next_trail_empty() {
    let mut shell = crate::test_helper::core_shell();
    shell.install_builtins(PANIC_BUILTINS);

    match shell.run(Run {
        trail: Some(crate::types::CapturePolicy::Off),
        ..Run::captured("core-panic-now", "<test>")
    }) {
        RunReport::Static {
            diagnostics: StaticDiagnostics::Host(e),
        } => assert!(e.message.contains("run panicked")),
        _ => panic!("a panicking dispatch must report Static{{Host}}"),
    }

    match shell.run(Run {
        trail: Some(crate::types::CapturePolicy::Off),
        ..Run::captured("$[1 + 1]", "<test>")
    }) {
        RunReport::Ran { trail, .. } => assert!(
            trail.is_empty(),
            "the panicked dispatch's scope must not leak into the next one"
        ),
        RunReport::Static { .. } => panic!("valid source must reach evaluation"),
    }
}
