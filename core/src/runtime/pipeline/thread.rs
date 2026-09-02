//! A stage thread: a ral-written pipeline stage evaluated on its own OS
//! thread, sharing the pipeline's process group with whatever externals it
//! spawns.  [`launch_thread_stage`] is the parent side — it wires the
//! stage's `Io` from its [`StageRoute`] and hands the closure to
//! [`Shell::spawn_thread`]; [`ThreadStage`] is the collector's handle onto
//! the running thread.

use super::launch::LaunchCx;
use super::resolve::StageSpec;
use super::route::{ByteOut, StageRoute};
use crate::evaluator::machine;
use crate::io::{Io, Sink};
use crate::ir::Comp;
use crate::source::Span;
use crate::types::{AuditFragment, Break, Closure, Error, Mooring, Settled, Value};
use crate::process::{
    CancelCause, CancelScope, StageGate, StagePark, StageState, StageStatus, Wake,
};
use std::sync::Arc;

/// A stage thread's result, returned on its `JoinHandle`.
pub(super) struct StageOutcome {
    pub result: Settled<Value>,
    pub audit: AuditFragment,
}

/// The parent's handle onto a running stage thread.
pub(super) struct ThreadStage {
    join: Option<std::thread::JoinHandle<StageOutcome>>,
    status: Arc<StageStatus>,
    cancel: CancelScope,
    wake: Arc<Wake>,
    span: Option<Span>,
}

impl ThreadStage {
    pub(super) fn probe(&self) -> StageState {
        self.status.get()
    }

    pub(super) fn cancel(&self, cause: CancelCause) {
        self.cancel.cancel(cause);
    }

    /// Fire the wake; on Windows also `CancelSynchronousIo` the stage
    /// thread's blocked `ReadFile`, retried until it acknowledges the wake
    /// or the stage has already finished on its own.
    pub(super) fn interrupt(&self) {
        self.wake.fire();
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::IO::CancelSynchronousIo;
            let Some(join) = &self.join else { return };
            while !self.wake.acknowledged() && self.status.get() != StageState::Finished {
                // SAFETY: `join`'s handle is valid for the thread's whole
                // life, which this `ThreadStage` outlives by construction.
                unsafe { CancelSynchronousIo(join.as_raw_handle().cast()) };
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    /// `Stopped` → `Running`; a `Running`/`Finished` status is left alone.
    pub(super) fn resume(&self) {
        if matches!(self.status.get(), StageState::Stopped(_)) {
            self.status.set(StageState::Running);
        }
    }

    /// Join the stage thread and reduce it to a [`super::collect::StageObservation`].
    /// A panic surfaces as an `Error` carrying this stage's span, since the
    /// stage's own stack carries none of its own to attribute it to.
    pub(super) fn observe(mut self, is_last: bool) -> super::collect::StageObservation {
        let outcome = match self.join.take().expect("observed once").join() {
            Ok(o) => o,
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                let mut err = Error::new(format!("ral pipeline stage panicked: {msg}"), 1);
                err.span = self.span;
                return super::collect::StageObservation::failure(err);
            }
        };
        match outcome.result {
            Ok(v) => super::collect::StageObservation::ok()
                .with_value(is_last.then_some(v))
                .with_audit(outcome.audit),
            Err(br) => super::collect::StageObservation::from_break(br).with_audit(outcome.audit),
        }
    }
}

impl Drop for ThreadStage {
    /// A stage never observed — an aborted launch, a panic elsewhere in the
    /// pipeline unwinding past it — is cancelled, interrupted, and joined
    /// rather than abandoned.
    fn drop(&mut self) {
        if self.join.is_none() {
            return;
        }
        self.cancel(CancelCause::Terminate);
        self.interrupt();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Spawn one ral-written stage on its own thread.
#[allow(
    clippy::needless_pass_by_value,
    reason = "LaunchCx bundles unique `&mut` borrows; by-value transfers them so this fn gets mutable access — a shared `&LaunchCx` cannot yield `&mut`"
)]
pub(super) fn launch_thread_stage(
    stage: &Arc<Comp>,
    spec: &StageSpec,
    route: StageRoute,
    cx: LaunchCx<'_>,
    gate: &Arc<StageGate>,
) -> Settled<ThreadStage> {
    let wake = Wake::new().map_err(|e| {
        let mut err = Error::new(format!("could not create a pipeline stage's wake: {e}"), 1);
        err.span = spec.span;
        Break::Error(err)
    })?;
    let StageRoute { stdin, stdout, .. } = route;
    let stdin = super::launch::stage_stdin(stdin, cx.group, cx.shell, &wake)?;

    let group = cx.group.leader_pgid();

    let stdout = match stdout {
        ByteOut::Downstream(w) => Sink::Pipe(Arc::new(w), Arc::clone(&wake)),
        ByteOut::Parent => cx.shell.io.stdout.clone(),
    };
    let stderr = cx.shell.io.stderr.clone();

    let park = StagePark {
        gate: Arc::clone(gate),
        status: StageStatus::new(),
    };
    let policy = cx.shell.local.audit.active_policy();
    let mooring = Mooring::for_stage_thread(cx.mooring, park.clone());

    let io = Io {
        stdin,
        stdout: stdout.clone(),
        ambient: stdout,
        stderr,
        interactive: cx.shell.io.interactive,
        terminal: cx.shell.io.terminal,
        launch_role: crate::io::LaunchRole::PipelineStage(group),
    };

    let env = cx.env.clone();
    let comp = Arc::clone(stage);
    let status = Arc::clone(&park.status);
    let thread_status = Arc::clone(&status);
    let span = spec.span;

    let spawned = cx.shell.spawn_thread(
        mooring,
        "ral pipeline stage",
        Arc::new(env.clone()),
        move |mooring, child| {
            child.io = io;
            child.local.audit.install_active_policy(policy);
            let result = machine::evaluate(Closure { comp, env }, mooring, child);
            let audit = child.local.audit.take_fragment();
            thread_status.set(StageState::Finished);
            StageOutcome { result, audit }
        },
    );
    let (join, cancel) = spawned.map_err(|e| {
        let mut err = Error::new(format!("could not start a pipeline stage thread: {e}"), 1);
        err.span = span;
        Break::Error(err)
    })?;

    Ok(ThreadStage {
        join: Some(join),
        status,
        cancel,
        wake,
        span,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use super::super::group::PipelineGroup;
    use super::super::resolve::{StageLaunch, TerminalPlan};
    use super::super::route::ByteIn;
    use crate::types::{Shell, TerminalAccess};
    use std::io::Read;
    use std::time::{Duration, Instant};

    fn compile_one(source: &str) -> Arc<Comp> {
        let ast = crate::parse(source).expect("parse");
        let top =
            crate::elaborate(&ast, std::collections::HashSet::default(), "").expect("elaborate");
        let [phrase] = top.phrases.as_slice() else {
            panic!("expected one phrase, got {:?}", top.phrases);
        };
        let crate::ir::Phrase::Run(comp) = &phrase.item else {
            panic!("expected a Run phrase, got {:?}", phrase.item);
        };
        comp.clone()
    }

    fn spec_for(comp: &Comp, feeds_pipe: bool) -> StageSpec {
        StageSpec {
            launch: StageLaunch::Thread,
            span: comp.span,
            feeds_pipe,
        }
    }

    fn prepared_group() -> PipelineGroup {
        PipelineGroup::prepare(TerminalPlan::NoTerminal, &Shell::default()).expect("anchor spawns")
    }

    #[test]
    fn a_stage_writes_into_a_sink_pipe_and_finishes() {
        let mut shell = Shell::default();
        shell.install_builtins(crate::builtins::CORE_BASE_FRAMES);
        let env = shell.env.clone();
        let mut group = prepared_group();
        let stage = compile_one("echo hi");
        let spec = spec_for(&stage, true);
        let (mut reader, writer) = crate::process::cloexec_pipe().expect("route pipe");
        let route = StageRoute {
            stdin: ByteIn::Parent,
            stdout: ByteOut::Downstream(writer),
            held: None,
        };
        let mooring = Mooring::adrift();
        let gate = StageGate::new();
        let cx = LaunchCx {
            mooring: &mooring,
            shell: &mut shell,
            env: &env,
            group: &mut group,
            stop: crate::process::StopPolicy::KillAndReap,
        };
        let handle = launch_thread_stage(&stage, &spec, route, cx, &gate).expect("launch");

        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read stage stdout");
        assert_eq!(out, b"hi\n");

        let obs = handle.observe(true);
        assert!(obs.break_.is_none(), "echo hi must not fail");
    }

    #[test]
    fn a_cancelled_spinning_stage_ends_within_500ms() {
        let mut shell = Shell::default();
        shell.install_builtins(crate::builtins::CORE_BASE_FRAMES);
        let env = shell.env.clone();
        let mut group = prepared_group();
        // A self-recursive, argument-incrementing call with no base case:
        // nothing but a cancel ends it.
        let stage = compile_one("!{ let go = { |n| go $[$n + 1] }; go 0 }");
        let spec = spec_for(&stage, true);
        let (_reader, writer) = crate::process::cloexec_pipe().expect("route pipe");
        let route = StageRoute {
            stdin: ByteIn::Parent,
            stdout: ByteOut::Downstream(writer),
            held: None,
        };
        let mooring = Mooring::adrift();
        let gate = StageGate::new();
        let cx = LaunchCx {
            mooring: &mooring,
            shell: &mut shell,
            env: &env,
            group: &mut group,
            stop: crate::process::StopPolicy::KillAndReap,
        };
        let handle = launch_thread_stage(&stage, &spec, route, cx, &gate).expect("launch");

        handle.cancel(CancelCause::ReaderGone);
        handle.interrupt();

        let start = Instant::now();
        let obs = handle.observe(true);
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(obs.break_.is_some(), "a killed stage must not settle Ok");
    }

    #[test]
    fn observe_of_a_panicking_body_carries_the_panic_and_its_span() {
        let file = Shell::default().install_script_context("<test>", "boom");
        let span = Span::new(file, 0, 4);
        let join = std::thread::Builder::new()
            .spawn(move || -> StageOutcome { panic!("boom") })
            .expect("spawn");
        let handle = ThreadStage {
            join: Some(join),
            status: StageStatus::new(),
            cancel: CancelScope::root(),
            wake: Wake::new().expect("wake"),
            span: Some(span),
        };

        let obs = handle.observe(true);
        match obs.break_ {
            Some(Break::Error(err)) => {
                assert!(
                    err.message.contains("boom"),
                    "the panic payload must ride in the message: {}",
                    err.message
                );
                assert_eq!(err.span, Some(span), "the stage's own span must survive");
            }
            other => panic!("expected an Error break, got {other:?}"),
        }
    }

    #[test]
    fn a_stages_terminal_lease_is_none() {
        let mut parent = Shell::default();
        parent.session.terminal_lease = crate::process::TerminalLease::mint_at_startup(true);
        let outer = Mooring {
            terminal_access: TerminalAccess::Leased,
            ..Mooring::adrift()
        };
        assert!(
            parent.terminal_lease(&outer).is_some(),
            "precondition: the outer mooring holds a lease"
        );

        let park = StagePark {
            gate: StageGate::new(),
            status: StageStatus::new(),
        };
        let stage_mooring = Mooring::for_stage_thread(&outer, park);
        assert!(
            parent.terminal_lease(&stage_mooring).is_none(),
            "a stage thread's mooring denies terminal access outright"
        );
    }
}
