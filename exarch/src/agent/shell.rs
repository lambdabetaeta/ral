//! The desk / `ral`-call seam: for the span of one [`Agent::run_shell`]
//! call, the attend thread installs a fresh [`desk::HostServices`] and then
//! sits parked inside [`shell_eval::run_shell`], off to one side of the seat,
//! while a desk handler thread runs on the other. [`ReplyCell`] and
//! [`LogCell`] are the two `Arc`-shared cells that window exists to make
//! safe: each rides in the capture [`Agent::host_services`] hands a
//! handler, so the handler can write a reply or a log line without ever
//! reaching back through `&mut Agent`. Neither cell blocks on contention —
//! the one thread that could contend is parked by construction, so a lock
//! taken outside that window is a scheduling bug, not a legitimate wait, and
//! every accessor panics didactically to say so rather than hang.

use crate::agent::Agent;
use crate::agent::digest::{OPAQUE_CAP, clip, render};
use crate::agent::event::{AgentLog, ToolResult as SessionToolResult};
use crate::agent::seat::{RunInstall, Seat};
use crate::bus::{Emitter, Kind};
use crate::fleet::desk;
use crate::shell_eval;
use ral_core::serial::FOValue;
use std::sync::{Arc, Mutex};

/// One `ral` call's shared reply slot.  [`Agent::run_shell`] mints a fresh
/// cell for each call and hands a clone into [`Agent::host_services`]; the
/// desk's `reply` handler ([`Self::set`]) is the cell's only writer, running
/// on the handler thread while the attend thread sits parked inside
/// [`Agent::run_shell`]'s [`shell_eval::run_shell`] — the very window
/// [`desk::HostServices`] as a whole depends on.  The instant the
/// desk is retired, `run_shell` harvests the cell by ownership
/// ([`Self::take`]) into [`Agent::reply`], the plain field that then carries
/// last-wins across the rest of the batch. `Arc`-shared for that one call's
/// extent only, so the desk handler thread can write without ever reaching
/// back through `&mut Agent`.  Because within-call contention is now
/// structurally a bug — the cell exists for exactly one call, touched by
/// exactly one handler while the attend thread is elsewhere — every accessor
/// `try_lock`s and panics didactically rather than blocking.
#[derive(Clone, Default)]
pub(crate) struct ReplyCell(Arc<Mutex<Option<FOValue>>>);

impl ReplyCell {
    /// Lock the cell — the sole accessor, every method below goes through
    /// this.  [`std::sync::TryLockError::WouldBlock`] means a desk handler ran outside the one window
    /// it is ever entitled to; [`std::sync::TryLockError::Poisoned`] means a prior holder panicked
    /// while holding it.  Both are fatal, but only the first names the
    /// scheduling law being violated — the same discipline [`LogCell::lock`]
    /// applies to its own lock.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<FOValue>> {
        match self.0.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => panic!(
                "reply cell contended: a desk handler may only run while the attend thread is \
                 parked in run_shell"
            ),
            Err(std::sync::TryLockError::Poisoned(_)) => panic!("reply cell poisoned"),
        }
    }

    /// Stage `value` as the return payload. Last write wins within a call:
    /// an earlier stage in the same call is silently overwritten.
    pub(crate) fn set(&self, value: FOValue) {
        *self.lock() = Some(value);
    }

    /// Take the staged payload, leaving the cell empty — `run_shell`'s
    /// post-retire harvest.
    pub(crate) fn take(&self) -> Option<FOValue> {
        self.lock().take()
    }
}

/// [`Agent::log`]'s lock: taken only by the attend thread between calls or by
/// a desk handler while the attend thread sits parked inside
/// [`Agent::run_shell`]'s [`shell_eval::run_shell`] — the same window
/// [`ReplyCell`] and [`desk::HostServices`] as a whole depend on.  Concurrent
/// access from both threads at once is a scheduling bug, not a legitimate
/// wait, so [`Self::lock`] `try_lock`s and panics didactically rather than
/// blocking — this codebase's standing law that a violation panics, never
/// hangs, the same discipline the enquiry reentrancy check applies to a
/// handler that reaches back through `&mut Agent`.
#[derive(Clone)]
pub(crate) struct LogCell(Arc<Mutex<AgentLog>>);

impl LogCell {
    pub(crate) fn new(log: AgentLog) -> Self {
        Self(Arc::new(Mutex::new(log)))
    }

    /// Lock the log — the sole accessor, every call site in the crate goes
    /// through this.  [`std::sync::TryLockError::WouldBlock`] means the attend thread and a desk handler
    /// touched the log at once; [`std::sync::TryLockError::Poisoned`] means a prior holder panicked
    /// while holding it.  Both are fatal, but only the first names the
    /// scheduling law being violated.
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, AgentLog> {
        match self.0.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => panic!(
                "log cell contended: the log may only be locked by the attend thread between \
                 calls or by a desk handler while the attend thread is parked in run_shell — \
                 concurrent access is a scheduling bug, not a wait"
            ),
            Err(std::sync::TryLockError::Poisoned(_)) => panic!("log poisoned"),
        }
    }
}

impl Agent {
    /// Best-effort dual-write: log the chrome line, then forward it
    /// through `emit`.  A log write-failure must not block the user line.
    pub(crate) fn note_error(&self, msg: String, emit: &Emitter) {
        let _ = self.log.lock().record_error(msg.clone());
        emit.emit(Kind::Error(msg));
    }

    /// Emit an operational system note — a truncation recovery, a compaction
    /// step.  Recorded in `transcript.jsonl` at the emit seam and surfaced on
    /// the display dim; never written to the model-view `events.json`, since it
    /// is not a message the model saw.
    pub(crate) fn note(text: String, emit: &Emitter) {
        emit.emit(Kind::SystemNote(text));
    }

    /// Capture this agent's [`desk::HostServices`] snapshot for a fresh
    /// desk: everything a handler may read off `&Agent`, since the
    /// reentrancy law bars a handler from ever reaching back through
    /// `&mut Agent`/`&mut Shell` to get it. Built fresh at every
    /// [`Self::run_shell`] install, beside the deferred sink, so the
    /// generation, fuel, caps, and grant a handler captures can never go
    /// stale — the same reasoning [`ral_core::transport::IdentityTransport::set_deferred_sink`] documents. `reply` is
    /// `run_shell`'s own fresh [`ReplyCell`] for this one call, not a clone
    /// of [`Self::reply`] — `run_shell` keeps its own handle to harvest back
    /// once the desk retires.
    pub(crate) fn host_services(
        &self,
        emit: &Emitter,
        nursery: ral_core::types::Nursery,
        reply: ReplyCell,
    ) -> desk::HostServices {
        // A wire seat owns no host-side scratch (the session's real one
        // lives inside the guest the transport dials); `agent-start`'s spawn
        // spine, the one consumer, never reaches its `None` in practice — a
        // wire session's fuel is always 0 (session-is-a-process).
        let scratch = match &self.seat {
            Seat::Identity { scratch, .. } => Some(scratch.clone()),
            Seat::Wire { .. } => None,
        };
        desk::HostServices {
            registry: self.agents.clone(),
            scratch,
            parent: self.id,
            mailbox: self.mailbox(),
            emit: emit.clone(),
            provider: self.provider.clone(),
            caps: self.caps.clone(),
            cwd: self.cwd(),
            fuel: self.fuel,
            returns: self.returns,
            allow_schedule: self.allow_schedule,
            reply,
            schedules: self.schedules.clone(),
            log: self.log.clone(),
            // A desk-spawned child (via the `agent` builtin, either memory
            // mode) always returns, which may differ from this agent's own
            // `returns`.
            system_template: self.system_base.clone(),
            indexes: self.indexes.clone(),
            interactive: self.interactive,
            nursery,
            generation: self.agents.generation(),
            disk_warn_bytes: self.disk_warn_bytes,
            egress: self.egress.clone(),
        }
    }

    pub(crate) fn run_shell(
        &mut self,
        id: String,
        cmd: &str,
        timeout_secs: u64,
        emit: &Emitter,
    ) -> SessionToolResult {
        // One ral call, one epoch tick — counted at entry so a call that
        // fails to evaluate still advances the retention clock.
        self.ral_epoch += 1;
        // The nursery is the adoption end of a desk handler's body-side
        // session fork (`Shell::fork_into_nursery`) — shared between the
        // seat install and the desk's own capture.
        let nursery = ral_core::types::Nursery::default();
        // This call's own reply slot: fresh, never a clone of `self.reply`,
        // so a staged-then-abandoned reply from an earlier call can never
        // resurface here.  Held on to below so it can be harvested back once
        // the desk retires, the instant this call's one legitimate writer
        // (the `reply` desk handler) is gone.
        let reply_cell = ReplyCell::default();
        // The desk and its whole capture set are fresh for every ral call —
        // a handler's captured generation, fuel, caps, and grant must never
        // go stale — and the guard retires them on every exit ([`seat::RunGuard`]).
        let desk = Arc::new(desk::ExarchDesk {
            services: self.host_services(emit, nursery.clone(), reply_cell.clone()),
        });
        let content = {
            let _guard = self.seat.install_run(RunInstall {
                desk: desk.clone(),
                apply: desk::SurfaceApplier {
                    emit: emit.clone(),
                    pins: Some(self.pins.clone()),
                },
                // A detached `spawn` worker flushes its buffered batch here
                // at completion, posted into this session's own inbox (via
                // `emit`'s mailbox) and guarded by the agent registry's
                // generation (so a `/clear` drops a stale batch).
                deferred: shell_eval::deferred_sink(emit, self.id, &self.agents),
                nursery,
            });
            match shell_eval::run_shell(
                self.seat.transport(),
                &self.caps,
                cmd,
                timeout_secs,
                emit,
                Some(&self.pins),
                Some(&desk),
            ) {
                shell_eval::Outcome::Ran(r) => render(&r),
                shell_eval::Outcome::Static(s) => clip(&s, OPAQUE_CAP),
            }
        };
        // Harvest whatever this call's `reply` handler staged, right after
        // the desk that could still write it is gone — the one legitimate
        // writer has retired, so the cell is ours by ownership now. `if let`,
        // not an unconditional overwrite: a later call in the same batch that
        // stages nothing must not erase an earlier call's staged reply —
        // last-wins is a property of the batch, not of any one call.
        if let Some(payload) = reply_cell.take() {
            self.reply = Some(payload);
        }
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
    }

    /// The current pinned state as a one-line description for the periodic
    /// nudge reminder, or `None` when nothing is pinned.  Joins each slot's
    /// digest (`tasks 3/8`) — the model's labels already name them — so the
    /// reminder reads as the user sees the rail.
    pub(super) fn pinned_digest(&self) -> Option<String> {
        let m = self.pins.lock().expect("pin register poisoned");
        if m.is_empty() {
            return None;
        }
        Some(
            m.values()
                .map(|pin| crate::bus::card::summary_line(&pin.card))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    //! `run_shell`'s call-boundary bookkeeping: binding-lease pruning (idle
    //! names, the large-binding warning) and the worker-retention clock, and
    //! the panic-recovery guarantee that boundary depends on.

    use super::*;
    use crate::agent::cancel;
    use crate::agent::testkit::*;
    use crate::agent::{NoControl, ProviderHandle, deliberate};
    use crate::provider::scripted::{Reply, Script};
    use ral_core::Shell;
    use ral_core::Value;
    use ral_core::typecheck::builtins::{BuiltinTypeRule, mk_scheme, pure, thunk};
    use ral_core::typecheck::{Scheme, Ty, Unifier};
    use ral_core::types::{BuiltinBody, BuiltinEntry, Mooring, Settled};
    use std::borrow::Cow;

    /// A nullary builtin whose body panics — stands in for any Rust panic
    /// the evaluator can raise mid-tool-eval.
    fn builtin_panic_now(
        _args: &[Value],
        _mooring: &Mooring,
        _shell: &mut Shell,
    ) -> Settled<Value> {
        panic!("a4 test: deliberate mid-eval panic");
    }

    fn scheme_panic_now(_u: &mut Unifier) -> Scheme {
        mk_scheme(&[], &[], &[], thunk(pure(Ty::Unit)))
    }

    static PANIC_BUILTINS: &[BuiltinEntry] = &[BuiltinEntry {
        name: Cow::Borrowed("a4-panic-now"),
        type_rule: BuiltinTypeRule::Scheme(Some(0), scheme_panic_now),
        doc: "test-only: panic the evaluator mid-eval.",
        body: BuiltinBody::Static(builtin_panic_now),
    }];

    /// Panic-recovery integrity (A4): a panic mid-tool-eval must preserve
    /// the bindings completed tool calls left behind and leave the dynamic
    /// context clean. Driven through the scripted provider and the shared
    /// `attend` loop — the real path: the engine's own run door
    /// (`Shell::run`) catches the unwind, rolls the dynamic context
    /// back, and reports the panicking call as a failed run.
    #[test]
    fn worker_panic_preserves_completed_bindings_and_clean_context() {
        let dir = tmp("panic-recovery");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .install_builtins(PANIC_BUILTINS);
        let baseline_grant_depth = probe_int(&session, "grant-depth");

        // 1st call binds `a4_x` (completes); 2nd call panics mid-eval.  The
        // panic is caught at the engine's own run door (`Shell::run`),
        // which rolls the shell back and reports a failed run — so the model
        // sees an ordinary failed tool result and the batch continues to a
        // third, closing reply.
        let provider = scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("c1", "let a4_x = 7")]))
                .then(Reply::tool_calls(vec![ral_call("c2", "a4-panic-now")]))
                .then(Reply::text("recovered")),
        );
        session.provider = ProviderHandle::new(provider);
        session.seed("compute then crash".into());
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        let _ = session.attend(&mut NoControl, &emit);

        // The dynamic context is rolled back to the clean boundary: no
        // leaked grant frame from the panicking call's `with_capabilities`.
        // (Read before the scope probe below, which is itself a call.)
        assert_eq!(
            probe_int(&session, "grant-depth"),
            baseline_grant_depth,
            "the panicking call's grant frame must not leak into the next run"
        );
        // The completed call's binding survives the panic.
        assert!(
            scope_has(&mut session, "a4_x"),
            "a binding from a completed tool call must survive a later call's panic"
        );
        // The attend loop handed the session back ready for a fresh prompt.
        assert!(
            session.is_ready(),
            "attend must leave the session ReadyForUser even after a worker panic"
        );

        // The next exchange is admissible and runs to completion on the
        // healed shell.
        let provider2 = scripted("test-model", Script::new().then(Reply::text("ok")));
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        let token = cancel::Token::new();
        let _slot = cancel::publish(&token);
        match session.deliberate(&provider2, Some("continue".into()), &token, &emit) {
            Ok(deliberate::Outcome::Complete(s)) => assert_eq!(s, "ok"),
            other => panic!("next exchange on the healed shell must complete, got {other:?}"),
        }
    }

    /// A session-scope install that meets the large-binding threshold writes
    /// the warning onto the installing run's own stderr — model-facing
    /// feedback in the tool result, not a frontend card — from inside that
    /// very run. A further run with no new session-scope install writes no
    /// warning. The two axes are independent: nothing here is idle enough to
    /// prune.
    #[test]
    fn large_binding_install_warns_on_its_own_run_stderr() {
        let dir = tmp("reap-large-binding");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .arm_binding_lease(ral_core::types::BindingLease {
                idle_calls: 1_000_000,
                large_binding_bytes: 8,
            });

        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());
        let result = session.run_shell(
            "c0".into(),
            "let large_binding_x = 'well over eight bytes long'",
            5,
            &emit,
        );

        let warnings = result
            .content
            .matches("large binding `large_binding_x`")
            .count();
        assert_eq!(warnings, 1, "exactly one warning per offending install");
        assert!(
            result.content.contains("held in session memory"),
            "the warning carries the file-path recommendation"
        );

        // No new session-scope install this time (`return` binds nothing),
        // so nothing meets the threshold again.
        let (tx2, _rx2) = crate::bus::channel();
        let emit2 = Emitter::with_mailbox(tx2, session.id, session.inbox.mailbox());
        let result2 = session.run_shell("c1".into(), "return 1", 5, &emit2);
        assert!(
            !result2.content.contains("large binding"),
            "nothing newly installed must warn again"
        );
    }

    /// A prune that fires between two runs cannot be undone by a later
    /// call's panic: the rollback checkpoint is taken at that call's own
    /// run entry, after the prune, so the pruned name cannot resurrect.
    /// A completed call's own binding, made after the prune, survives the
    /// panic exactly as
    /// `worker_panic_preserves_completed_bindings_and_clean_context` pins.
    #[test]
    fn panic_after_prune_does_not_resurrect_binding() {
        let dir = tmp("panic-after-prune");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .install_builtins(PANIC_BUILTINS);
        session
            .seat
            .shell_mut()
            .shell
            .arm_binding_lease(ral_core::types::BindingLease {
                idle_calls: 2,
                large_binding_bytes: u64::MAX,
            });

        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        session.run_shell("c0".into(), "let panic_prune_x = 1", 5, &emit);
        session.run_shell("c1".into(), "let _spin1 = 0", 5, &emit);
        session.run_shell("c2".into(), "let _spin2 = 0", 5, &emit);

        // The prune fired at the last call's own ready boundary, before
        // any panic.  (The probe is itself a call: it may prune the idle
        // `_spin` names, which nothing below asserts on.)
        assert!(
            !scope_has(&mut session, "panic_prune_x"),
            "panic_prune_x must already be pruned before the panicking call"
        );

        let provider = scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call(
                    "c3",
                    "let survives_y = 9",
                )]))
                .then(Reply::tool_calls(vec![ral_call("c4", "a4-panic-now")]))
                .then(Reply::text("recovered")),
        );
        session.provider = ProviderHandle::new(provider);
        session.seed("compute then crash".into());
        let _ = session.attend(&mut NoControl, &emit);

        // `survives_y` first: each probe ticks the armed idle bound (2), and
        // reading it renews it, so the second probe's tick cannot prune it.
        assert!(
            scope_has(&mut session, "survives_y"),
            "a completed call's binding must survive a later call's panic"
        );
        assert!(
            !scope_has(&mut session, "panic_prune_x"),
            "the pruned name must not resurrect across the panic's mobile rollback"
        );
    }

    /// End-to-end against the real production constant, not a re-armed
    /// test bound: a boot-seeded (baseline) name survives past
    /// `BINDING_IDLE_CALLS` real `run_shell` calls.
    #[test]
    fn boot_names_survive_past_the_idle_bound() {
        let dir = tmp("boot-names-survive");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);

        let (boot_name, _) = session
            .seat
            .shell_mut()
            .shell
            .bindings()
            .into_iter()
            .next()
            .expect("the boot sequence seeds at least one binding");

        for i in 0..(shell_eval::BINDING_IDLE_CALLS + 5) {
            session.run_shell(format!("spin{i}"), "let _boot_spin = 0", 5, &emit);
        }
        assert!(
            scope_has(&mut session, &boot_name),
            "a boot-seeded (baseline) name must survive past the idle bound"
        );
    }

    /// The settled-worker retention ledger, end to end through the
    /// engine's own clock: the registry ticks once per dispatched call,
    /// its sweep runs at each call's ready boundary, and an expiry's
    /// `Retention`-cause `Kind::Notice` rides a run's own surface stream.
    #[test]
    fn run_shell_epoch_stamps_and_retention_renders_through_the_drain() {
        let dir = tmp("ral-epoch-retention");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        // A tiny bound so the expiry is a couple of calls away (`assemble`
        // armed the production constant; re-arming replaces it).
        session.seat.shell_mut().shell.arm_worker_retention(1);
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());

        session.run_shell("t1".into(), "spawn { return 1 }", 5, &emit);
        assert_eq!(session.ral_epoch, 1, "one call, one tick");

        // Settle observed through the probe rail — a boundary read that
        // ticks nothing, so the retention arithmetic below stays exact.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !session.probe_workers().iter().any(|w| !w.running) {
            assert!(
                std::time::Instant::now() < deadline,
                "the instant worker must settle within the budget"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        // Stamped at the first boundary sweep that observes it settled,
        // expired once a later call's sweep finds it a full retention old —
        // which call does which depends on when the worker settled, so
        // drive calls until the notice lands (bounded).
        let mut reaps = 0;
        for i in 0..6 {
            session.run_shell(format!("spin{i}"), "$[0]", 5, &emit);
            while let Ok(event) = rx.try_recv() {
                let Kind::Notice {
                    notice: crate::bus::card::Notice::Reap { cmd, cause },
                    ..
                } = event.kind
                else {
                    continue;
                };
                assert_eq!(cmd, "<block>", "the reap names the spawned body");
                assert_eq!(
                    cause,
                    ral_core::types::ReapCause::Retention,
                    "an unclaimed settled entry expires as Retention"
                );
                reaps += 1;
            }
            if reaps > 0 {
                break;
            }
        }
        assert_eq!(reaps, 1, "exactly one notice per retention expiry");
        assert_eq!(
            probe_int(&session, "worker-count"),
            0,
            "the expired entry left the registry"
        );
    }
}
