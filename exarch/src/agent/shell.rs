//! One `ral` call — [`Agent::run_shell`] — and the two `Arc`-shared cells a
//! desk handler writes the agent through: a handler answers mid-dispatch, on
//! the attend thread's own stack inside [`shell_eval::run_shell`], so it can
//! never take `&mut Agent` and reaches [`ReplyCell`] and [`LogCell`] through
//! the [`desk::HostServices`] capture instead.  One thread means a failed
//! `try_lock` is reentrancy, not contention, so both cells panic where a
//! `lock` would deadlock.

use crate::agent::Agent;
use crate::agent::digest::{OPAQUE_CAP, clip, render};
use crate::agent::event::{AgentLog, ToolResult as SessionToolResult};
use crate::agent::seat::{RunInstall, Seat};
use crate::bus::{Emitter, Kind};
use crate::fleet::desk;
use crate::shell_eval;
use ral_core::serial::FOValue;
use std::sync::{Arc, Mutex};

/// One `ral` call's reply slot, minted fresh per call so a reply staged and
/// then abandoned cannot resurface in a later one.  The desk's `reply` handler
/// is its only writer; [`Agent::run_shell`] harvests it into [`Agent::reply`].
#[derive(Clone, Default)]
pub(crate) struct ReplyCell(Arc<Mutex<Option<FOValue>>>);

impl ReplyCell {
    /// Never waits: the one thread that could hold this guard is the asker.
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

    /// Stage `value` as the return payload; within one call, last write wins.
    pub(crate) fn set(&self, value: FOValue) {
        *self.lock() = Some(value);
    }

    pub(crate) fn take(&self) -> Option<FOValue> {
        self.lock().take()
    }
}

/// [`Agent::log`] behind its own lock, so a desk handler can be handed the log
/// off `&Agent` — the spawn spine forks a child's log through it — instead of
/// reaching back through `&mut Agent`.
#[derive(Clone)]
pub(crate) struct LogCell(Arc<Mutex<AgentLog>>);

impl LogCell {
    pub(crate) fn new(log: AgentLog) -> Self {
        Self(Arc::new(Mutex::new(log)))
    }

    /// The crate's one door onto the log, and it never waits: `WouldBlock`
    /// means a handler asked for a guard its own attend thread already holds.
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
    /// Dual-write, best effort: a failed log write must not swallow the line
    /// the user is owed.
    pub(crate) fn note_error(&self, msg: String, emit: &Emitter) {
        let _ = self.log.lock().record_error(msg.clone());
        emit.emit(Kind::Error(msg));
    }

    /// An operational note: it reaches the transcript and the display, but has
    /// no model-view `events.jsonl` twin, since the model never saw it.
    pub(crate) fn note(text: String, emit: &Emitter) {
        emit.emit(Kind::SystemNote(text));
    }

    /// Everything a desk handler may read off `&Agent`, since the reentrancy
    /// law bars it from reaching back through `&mut Agent`/`&mut Shell`.  Built
    /// fresh at each [`Self::run_shell`] install, so no capture goes stale.
    pub(crate) fn host_services(
        &self,
        emit: &Emitter,
        nursery: ral_core::types::Nursery,
        reply: ReplyCell,
    ) -> desk::HostServices {
        // A wire seat owns no host-side scratch — the session's real one lives
        // in the guest the transport dials — and `agent-start`, the one
        // consumer, has already refused on fuel 0 by the time it would look.
        let scratch = match &self.seat {
            Seat::Identity { scratch, .. } => Some(scratch.clone()),
            Seat::Wire { .. } => None,
        };
        // Stated, not inferred from `scratch`'s incidental absence:
        // `agent-start` chooses its arm on this one fact.
        let wire_seat = matches!(self.seat, Seat::Wire { .. });
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
            search: self.search,
            reply,
            schedules: self.schedules.clone(),
            log: self.log.clone(),
            // The unresolved template, not this agent's own `system`: a
            // desk-spawned child always returns, so it refilters its own index.
            system_template: self.system_base.clone(),
            index: self.index.clone(),
            interactive: self.interactive,
            nursery,
            generation: self.agents.generation(),
            disk_warn_bytes: self.disk_warn_bytes,
            egress: self.egress.clone(),
            // Minted here, once per `ral` call: this is the one place a call's
            // whole desk capture is built, so the fragment's extent is the call's.
            acts: desk::ActFragment::default(),
            principal: ral_core::host::user(),
            pins: Some(self.pins.clone()),
            wire_seat,
            hatchery: self.hatchery.clone(),
            pending_hatches: self.pending_hatches.clone(),
        }
    }

    pub(crate) fn run_shell(
        &mut self,
        id: String,
        cmd: &str,
        timeout_secs: u64,
        emit: &Emitter,
    ) -> SessionToolResult {
        // At entry, so a call that fails to evaluate still ages the clock.
        self.ral_epoch += 1;
        // The adoption end of a handler's body-side `Shell::fork_into_nursery`,
        // shared between the seat install and the desk's own capture.
        let nursery = ral_core::types::Nursery::default();
        let reply_cell = ReplyCell::default();
        // Built once and shared by `Arc`: the identity install and
        // `shell_eval::run_shell`'s own drain reach the very same desk and
        // the very same applier, so neither seam can be handed one this call
        // did not also hand the other.
        let seam = Arc::new(desk::HostSeam {
            desk: desk::ExarchDesk {
                services: self.host_services(emit, nursery.clone(), reply_cell.clone()),
            },
            apply: desk::SurfaceApplier {
                emit: emit.clone(),
                pins: Some(self.pins.clone()),
            },
        });
        let outcome = {
            let _guard = self.seat.install_run(RunInstall {
                seam: seam.clone(),
                // Stamped with the registry generation read now, so a batch
                // from a worker that settles after a `/clear` is dropped.
                deferred: shell_eval::deferred_sink(emit, self.id, &self.agents),
                nursery,
            });
            shell_eval::run_shell(
                self.seat.transport(),
                &self.caps,
                cmd,
                timeout_secs,
                emit,
                Some(&seam),
            )
        };
        // Only now, with the dispatch returned and the install dropped: the
        // worker probe below is legal at a run boundary and nowhere else.
        let content = match outcome {
            shell_eval::Outcome::Ran {
                stdout,
                mut stderr,
                value,
                ending,
                trail,
            } => {
                let workers = self.probe_workers();
                let (suffix, exit) = shell_eval::report::render(
                    &ending,
                    &trail,
                    &seam.desk.services.acts,
                    &workers,
                    timeout_secs,
                );
                stderr.extend_from_slice(suffix.as_bytes());
                render(&shell_eval::ToolResult {
                    stdout,
                    stderr,
                    value,
                    exit,
                })
            }
            shell_eval::Outcome::Static(s) => clip(&s, OPAQUE_CAP),
        };
        // `if let`, not an unconditional overwrite: last-wins is a property of
        // the batch, so a later call that stages nothing must leave an earlier
        // call's reply standing.
        if let Some(payload) = reply_cell.take() {
            self.reply = Some(payload);
        }
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
    }

    /// Every pinned slot's summary joined onto one line, for the periodic
    /// nudge reminder; `None` when nothing is pinned.  Rendered through the
    /// rail's own `summary_line`, so the reminder reads as the user sees it.
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
    //! `run_shell`'s call-boundary bookkeeping — binding-lease pruning, the
    //! large-binding warning, worker retention, the audit and the surviving
    //! workers a raise owes the model — and the panic recovery those boundaries
    //! rest on.

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

    /// Stands in for any Rust panic the evaluator can raise mid-eval.
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

    static PANIC_BUILTINS_ARR: [BuiltinEntry; 1] = [BuiltinEntry::new(
        Cow::Borrowed("a4-panic-now"),
        BuiltinTypeRule::Scheme(scheme_panic_now),
        "test-only: panic the evaluator mid-eval.",
        BuiltinBody::Static(builtin_panic_now),
    )];
    static PANIC_BUILTINS: &[BuiltinEntry] = &PANIC_BUILTINS_ARR;

    /// A panic mid-eval must preserve what completed calls bound and leave the
    /// dynamic context clean.  Driven through the real `attend` loop, so the
    /// recovery under test is the engine's own run door catching the unwind.
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

        // The panicking second call surfaces to the model as an ordinary
        // failed tool result, which is why a third, closing reply follows.
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

        // No grant frame leaked out of the panicking call's
        // `with_capabilities`.  Read before the scope probe below, which is
        // itself a call.
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

        let provider2 = scripted("test-model", Script::new().then(Reply::text("ok")));
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        let token = cancel::Token::new();
        let _slot = cancel::publish(&token);
        match session.deliberate(&provider2, Some("continue".into()), None, &token, &emit) {
            Ok(deliberate::Outcome::Complete(s)) => assert_eq!(s, "ok"),
            other => panic!("next exchange on the healed shell must complete, got {other:?}"),
        }
    }

    /// An oversize session-scope install warns on the installing run's own
    /// stderr — model-facing in the tool result, not a frontend card — and a
    /// later run that installs nothing new stays quiet.  The idle bound is
    /// armed out of reach, so only the size axis is in play.
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

        // `return` binds nothing, so no install meets the threshold again.
        let (tx2, _rx2) = crate::bus::channel();
        let emit2 = Emitter::with_mailbox(tx2, session.id, session.inbox.mailbox());
        let result2 = session.run_shell("c1".into(), "return 1", 5, &emit2);
        assert!(
            !result2.content.contains("large binding"),
            "nothing newly installed must warn again"
        );
    }

    /// The audit belongs to every raise, not only to the wall: a call that
    /// staged its reply and then died on a command's non-zero exit still made
    /// that reply stand, so the audit rides that stderr too — last, after the
    /// non-zero-exit branch's own remedy.
    #[cfg(unix)]
    #[test]
    fn a_non_zero_exit_carries_the_audit_of_what_already_stands() {
        let dir = tmp("audit-on-command-exit");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());

        let result = session.run_shell(
            "c0".into(),
            "reply 'the work stands'\n/bin/sh -c 'exit 3'",
            10,
            &emit,
        );

        assert!(
            result.content.contains("EXIT: 3"),
            "the command's own exit is the tool exit; content was: {}",
            result.content
        );
        let (remedy, audit) = (
            result.content.find("recovery: this non-zero exit raised"),
            result
                .content
                .find("audit: this call had already staged your reply"),
        );
        assert!(
            audit.is_some(),
            "a committed act must be audited on a non-zero exit too; content was: {}",
            result.content
        );
        assert!(
            remedy < audit,
            "the audit comes last, after the branch's own remedy; content was: {}",
            result.content
        );
    }

    /// A worker `defer`red before the wall outlives it — moored to the session
    /// root, out of the foreground cancel's reach — while the handle binding
    /// that named it is gone with the unwind.  The timeout stderr must say so,
    /// naming the work by joining this dispatch's own trail births against the
    /// live `` `workers `` probe, or the model is left unable to `await` and
    /// unaware there is anything to await.
    #[cfg(unix)]
    #[test]
    fn the_wall_names_the_workers_that_survived_it() {
        let dir = tmp("surviving-workers");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());

        let result = session.run_shell(
            "c0".into(),
            "let deferred = defer { sleep 20 }\nsleep 20",
            2,
            &emit,
        );

        assert!(
            result.content.contains("EXIT: 124"),
            "the wall exits 124; content was: {}",
            result.content
        );
        assert!(
            result
                .content
                .contains("work this call spawned is now orphaned: `<block>`"),
            "the surviving worker is named by the `cmd` core filed it under; content was: {}",
            result.content
        );
        assert!(
            result.content.contains("you cannot `await` it"),
            "the sentence says why the handle is no help; content was: {}",
            result.content
        );
    }

    /// The same probe, on a call the wall did not cut: a completed call's
    /// worker is nobody's orphan, so nothing is said about it.
    #[test]
    fn a_call_that_returns_says_nothing_about_its_workers() {
        let dir = tmp("no-surviving-note");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());

        let result = session.run_shell("c0".into(), "let ok = defer { return 1 }", 10, &emit);

        assert!(
            !result.content.contains("orphaned"),
            "a call that returned holds its own handle; content was: {}",
            result.content
        );
    }

    /// The widening this wave deliberately introduces: the handle is equally
    /// lost on a routine non-zero exit, not just the wall — so a live birth
    /// draws the same orphan sentence there too.
    #[cfg(unix)]
    #[test]
    fn a_non_zero_exit_with_a_live_birth_names_the_orphan() {
        let dir = tmp("exit-names-orphan");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());

        let result = session.run_shell(
            "c0".into(),
            "let deferred = defer { sleep 20 }\n/bin/sh -c 'exit 3'",
            10,
            &emit,
        );

        assert!(
            result.content.contains("EXIT: 3"),
            "the command's own exit is the tool exit; content was: {}",
            result.content
        );
        assert!(
            result
                .content
                .contains("work this call spawned is now orphaned: `<block>`"),
            "a non-zero exit orphans a live birth exactly as the wall does; content was: {}",
            result.content
        );
    }

    /// A panic cannot resurrect a name pruned before it: the run door's
    /// checkpoint is taken at the panicking call's own entry, by which time
    /// the prune is already part of the state being checkpointed.
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

        // The probe is itself a call, so it may prune the idle `_spin` names
        // too — nothing below asserts on those.
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

        // `survives_y` first: each probe ticks the armed idle bound of 2, and
        // reading a name renews it, so the second probe cannot prune it.
        assert!(
            scope_has(&mut session, "survives_y"),
            "a completed call's binding must survive a later call's panic"
        );
        assert!(
            !scope_has(&mut session, "panic_prune_x"),
            "the pruned name must not resurrect across the panic's mobile rollback"
        );
    }

    /// Against the real `BINDING_IDLE_CALLS`, not a re-armed test bound: a
    /// boot-seeded name is baseline and never ages out.
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

    /// The settled-worker retention ledger on the engine's own clock: one tick
    /// per dispatched call, a sweep at each ready boundary, and the expiry's
    /// notice riding a later run's surface stream back to the bus.
    #[test]
    fn run_shell_epoch_stamps_and_retention_renders_through_the_drain() {
        let dir = tmp("ral-epoch-retention");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        // A tiny bound so the expiry is a couple of calls away; this replaces
        // the production constant the seat's identity ceremony armed.
        session.seat.shell_mut().shell.arm_worker_retention(1);
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());

        session.run_shell("t1".into(), "spawn { return 1 }", 5, &emit);
        assert_eq!(session.ral_epoch, 1, "one call, one tick");

        // Through the probe rail, not a `run_shell`: a boundary read ticks
        // nothing, so the retention arithmetic below stays exact.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !session.probe_workers().iter().any(|w| !w.running) {
            assert!(
                std::time::Instant::now() < deadline,
                "the instant worker must settle within the budget"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        // Which call stamps and which expires depends on when the worker
        // settled, so drive calls until the notice lands, bounded.
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
