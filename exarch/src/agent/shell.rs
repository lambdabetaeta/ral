//! One `ral` call — [`Avatar::ral`] — and the two `Arc`-shared cells a
//! desk handler writes the agent through: a handler answers mid-dispatch, on
//! the attend thread's own stack inside [`shell_eval::run_shell`], so it can
//! never take `&mut Avatar` and reaches [`ReplyCell`] and [`LogCell`] through
//! the [`desk::HostServices`] capture instead.  One thread means a failed
//! `try_lock` is reentrancy, not contention, so both cells panic where a
//! `lock` would deadlock.

use crate::agent::Avatar;
use crate::agent::digest::{OPAQUE_CAP, clip, render};
use crate::agent::log::{AgentLog, EditAuthority};
use crate::agent::seat::EngineLost;
use crate::bus::{AgentState, Emitter};
use crate::fleet::desk;
use crate::shell_eval;
use ral_core::protocol::{Report, reading};
use ral_core::serial::FOValue;
use ral_core::sync::LockExt;
use std::fmt::Write;
use std::io;
use std::sync::{Arc, Mutex};

/// The desk's `reply` slot: within one batch the last write wins, and
/// [`Avatar::deliberate`] takes it once the batch drains.
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

    pub(crate) fn set(&self, value: FOValue) {
        *self.lock() = Some(value);
    }

    pub(crate) fn take(&self) -> Option<FOValue> {
        self.lock().take()
    }
}

/// [`Avatar::log`] behind its own lock, so a desk handler can be handed the log
/// off `&Avatar` — the spawn spine forks a child's log through it — instead of
/// reaching back through `&mut Avatar`.
#[derive(Clone)]
pub(crate) struct LogCell(Arc<Mutex<AgentLog>>);

impl LogCell {
    pub(crate) fn new(log: AgentLog) -> Self {
        Self(Arc::new(Mutex::new(log)))
    }

    /// The crate's one way onto the log, and it never waits: `WouldBlock`
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

/// What one `ral` call leaves the model: the text it reads, and whether the
/// run failed — rejected, raised, nonzero, or lost with its engine.
pub(crate) struct Evaluated {
    pub text: String,
    pub failed: bool,
}

impl Avatar {
    /// Point this session's recorder at the bus `emit` rides, so every fact
    /// the log authors is published live as it lands on disk.  Called at each
    /// entry where a session meets a bus — `attend`, `deliberate`,
    /// [`Self::ral`], `rewind` — because the recorder outlives any one bus:
    /// idempotent, and re-coupling over a dead per-exchange channel is how a
    /// headless session's next exchange comes back on air.
    pub(crate) fn couple(&self, emit: &Emitter) {
        self.log.lock().record_emitter().attach(emit.fleet_sink());
    }

    /// This session's recorder, for whoever authors a display commit — the
    /// chopper, the surface buffer, a tool-call row.
    pub(crate) fn recorder(&self) -> crate::record::Emitter {
        self.log.lock().record_emitter()
    }

    /// Evict on an authority no act row speaks for — the harness's pressure
    /// cut, the user's `/rewind` — so the eviction draws its own row.
    pub(crate) fn evict_unbidden(&self, turns: &[u64], by: EditAuthority) -> Result<(), String> {
        let cut = self.log.lock().evict(turns, None, by)?;
        self.recorder()
            .emit(crate::record::Display::Evicted { cut, by })
            .map(drop)
            .map_err(|e| e.to_string())
    }

    /// `record_error` is durable and published in one call, so there is no
    /// second write to keep in step with it.  A log that cannot take it is
    /// the one failure that cannot go through the log, so the line rides a
    /// fault transient instead — never lost silently.
    pub(crate) fn note_error(&self, msg: &str) {
        let recorded = self.log.lock().record_error(msg.to_string());
        if let Err(error) = recorded {
            self.recorder()
                .report_fault(&io::Error::new(error.kind(), format!("{msg} ({error})")));
        }
    }

    /// An operational note: recorded as a [`Forensic::SystemNote`], with no
    /// model-view twin, since the model never saw it.
    pub(crate) fn note(&self, text: String) {
        let recorder = self.recorder();
        if let Err(error) = recorder.emit(crate::record::Forensic::SystemNote { text }) {
            recorder.report_fault(&error);
        }
    }

    /// Everything a desk handler may read off `&Avatar`, since the reentrancy
    /// law bars it from reaching back through `&mut Avatar`/`&mut Shell`.
    /// Built fresh at each [`Self::ral`] install, so no capture goes stale.
    pub(crate) fn host_services(&self, emit: &Emitter) -> desk::HostServices {
        desk::HostServices {
            fleet: self.fleet.clone(),
            kind: self.seat.kind(),
            agent: self.agent.clone(),
            emit: emit.clone(),
            reply: self.reply.clone(),
            log: self.log.clone(),
            branch: None,
            stamp: self.agent.mailbox.stamp(),
            // Minted here, once per `ral` call: this is the one place a call's
            // whole desk capture is built, so the fragment's extent is the call's.
            acts: desk::ActFragment::default(),
            principal: ral_core::host::user(),
        }
    }

    /// Evaluate one `ral` call.
    pub(crate) fn ral(&self, cmd: &str, timeout_secs: u64, emit: &Emitter) -> Evaluated {
        self.couple(emit);
        // The assistant turn is recorded before its results are built, so this
        // is the id of the turn this result closes — and the call's source name.
        let turn = self.log.lock().context().current_turn();
        let source = turn.map_or_else(|| "tool call".to_string(), |turn| format!("turn {turn}"));
        let host = Arc::new(desk::RunHost {
            desk: desk::ExarchDesk {
                services: self.host_services(emit),
            },
            apply: desk::SurfaceApplier::new(self.recorder()),
        });
        // Stamped with this session's inbox epoch as read now, so a batch
        // from a worker that settles after a `/clear` is dropped.
        self.seat.install_deferred(shell_eval::deferred_sink(emit));
        self.recorder()
            .transient(crate::record::Transient::State(AgentState::Evaluating));
        let report = shell_eval::run_shell(
            self.seat.transport(),
            &self.agent.caps,
            &source,
            cmd,
            timeout_secs,
            host.clone() as Arc<dyn ral_core::protocol::Host>,
        );
        let lost = |s| EngineLost::running(&s, self.agent.run_dir()).to_string();
        // Only now, with the dispatch returned: the worker probe below is
        // legal at a run boundary and nowhere else.
        let (mut text, failed) = match report {
            Ok(Report::Ran {
                ending, captured, ..
            }) => match self.seat.read(reading::workers) {
                Ok(workers) => {
                    let result = shell_eval::report::tool_result(
                        &ending,
                        captured,
                        &host.apply.births(),
                        &host.desk.services.acts,
                        &workers,
                        timeout_secs,
                    );
                    (render(&result), result.exit != 0)
                }
                Err(s) => (lost(s), true),
            },
            Ok(Report::Static { rendered, .. }) => (clip(&rendered, OPAQUE_CAP), true),
            Err(s) => (lost(s), true),
        };
        if let Some(turn) = turn {
            let _ = write!(text, "\nTURN: {turn}");
        }
        Evaluated { text, failed }
    }

    /// Every pinned slot's summary joined onto one line, for the periodic
    /// nudge reminder; `None` when nothing is pinned.  Rendered through the
    /// rail's own `summary_line`, so the reminder reads as the user sees it.
    pub(super) fn pinned_digest(&self) -> Option<String> {
        let lines: Vec<String> = self
            .agent
            .pins
            .lock_ignore_poison()
            .values()
            .map(|pin| crate::bus::card::summary_line(&pin.card))
            .collect();
        (!lines.is_empty()).then(|| lines.join("; "))
    }
}

#[cfg(test)]
mod tests {
    //! `Avatar::ral`'s call-boundary bookkeeping — binding-lease pruning, the
    //! large-binding warning, worker retention, the audit and the surviving
    //! workers a raise owes the model — and the panic recovery those boundaries
    //! rest on.

    use super::*;
    use crate::agent::deliberate;
    use crate::agent::testkit::*;
    use crate::provider::scripted::{Reply, Script};
    use ral_core::Shell;
    use ral_core::Value;
    use ral_core::typecheck::builtins::{mk_scheme, pure, thunk};
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
        mk_scheme(&[], &[], thunk(pure(Ty::Unit)))
    }

    static PANIC_BUILTINS_ARR: [BuiltinEntry; 1] = [BuiltinEntry::new(
        Cow::Borrowed("a4-panic-now"),
        scheme_panic_now,
        "test-only: panic the evaluator mid-eval.",
        BuiltinBody::Static(builtin_panic_now),
    )];
    static PANIC_BUILTINS: &[BuiltinEntry] = &PANIC_BUILTINS_ARR;

    #[test]
    fn let_bound_context_read_does_not_echo_the_transcript() {
        let session = Avatar::for_test("system").unwrap();
        {
            let mut log = session.log.lock();
            log.append_user("material that must stay bound".into(), None)
                .unwrap();
            log.append_assistant(
                genai::chat::ChatMessage::assistant("the answer stays in the binding"),
                Vec::new(),
                None,
            )
            .unwrap();
        }
        // Held, not read: the emitter's far end must outlive the call.
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.agent.id);

        let result = session
            .ral(
                "let ctx = exarch-transcript `read [turns: [1, 2]]",
                5,
                &emit,
            )
            .text;
        assert!(
            !result.contains("material that must stay bound")
                && !result.contains("the answer stays in the binding"),
            "a let-bound read must not echo its value: {result}"
        );
        assert!(
            result.ends_with("\nTURN: 2"),
            "every tool result closes with the id of the turn it closes: {result}"
        );
    }

    /// A panic mid-eval must preserve what completed calls bound and leave the
    /// dynamic context clean.  Driven through the real `attend` loop, so the
    /// recovery under test is the engine's own run door catching the unwind.
    #[test]
    fn worker_panic_preserves_completed_bindings_and_clean_context() {
        let mut session = dressed_trunk(|shell| shell.install_builtins(PANIC_BUILTINS));
        let baseline_grant_depth = probe_count(&session, ral_core::test_access::grant_depth);

        // The panicking second call surfaces to the model as an ordinary
        // failed tool result, which is why a third, closing reply follows.
        let provider = scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("c1", "let a4_x = 7")]))
                .then(Reply::tool_calls(vec![ral_call("c2", "a4-panic-now")]))
                .then(Reply::text("recovered")),
        );
        session.agent.provider.swap(provider);
        session.seed("compute then crash".into());
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.agent.id);
        let _ = session.attend(&emit);

        // No grant frame leaked out of the panicking call's
        // `with_capabilities`.  Read before the scope probe below, which is
        // itself a call.
        assert_eq!(
            probe_count(&session, ral_core::test_access::grant_depth),
            baseline_grant_depth,
            "the panicking call's grant frame must not leak into the next run"
        );
        // The completed call's binding survives the panic.
        assert!(
            scope_has(&session, "a4_x"),
            "a binding from a completed tool call must survive a later call's panic"
        );
        // The attend loop handed the session back ready for a fresh prompt.
        assert!(
            session.is_ready(),
            "attend must leave the session ReadyForUser even after a worker panic"
        );

        let provider2 = scripted("test-model", Script::new().then(Reply::text("ok")));
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.agent.id);
        match session.deliberate(&provider2, Some("continue".into()), None, &emit) {
            Ok(deliberate::Outcome::Complete) => {}
            other => panic!("next exchange on the healed shell must complete, got {other:?}"),
        }
    }

    /// An oversize session-scope install warns on the installing run's own
    /// stderr — model-facing in the tool result, not a frontend card — and a
    /// later run that installs nothing new stays quiet.  The idle bound is
    /// armed out of reach, so only the size axis is in play.
    #[test]
    fn large_binding_install_warns_on_its_own_run_stderr() {
        let session = dressed_trunk(|shell| {
            shell.arm_binding_lease(ral_core::types::BindingLease {
                idle_calls: 1_000_000,
                large_binding_bytes: 8,
            });
        });

        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());
        let result = session
            .ral(
                "let large_binding_x = 'well over eight bytes long'",
                5,
                &emit,
            )
            .text;

        let warnings = result.matches("large binding `large_binding_x`").count();
        assert_eq!(warnings, 1, "exactly one warning per offending install");
        assert!(
            result.contains("held in session memory"),
            "the warning carries the file-path recommendation"
        );

        // `return` binds nothing, so no install meets the threshold again.
        let (tx2, _rx2) = crate::bus::channel();
        let emit2 = Emitter::with_mailbox(tx2, session.agent.id, session.inbox.mailbox());
        let result2 = session.ral("return 1", 5, &emit2).text;
        assert!(
            !result2.contains("large binding"),
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
        let session = Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());

        let result = session
            .ral(
                "exarch-agents `reply 'the work stands'\n/bin/sh -c 'exit 3'",
                10,
                &emit,
            )
            .text;

        assert!(
            result.contains("EXIT: 3"),
            "the command's own exit is the tool exit; content was: {result}"
        );
        let (remedy, audit) = (
            result.find("recovery: this non-zero exit raised"),
            result.find("audit: this call had already staged your reply"),
        );
        assert!(
            audit.is_some(),
            "a committed act must be audited on a non-zero exit too; content was: {result}"
        );
        assert!(
            remedy < audit,
            "the audit comes last, after the branch's own remedy; content was: {result}"
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
        let session = Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());

        let result = session
            .ral("let deferred = defer { sleep 20 }\nsleep 20", 2, &emit)
            .text;

        assert!(
            result.contains("EXIT: 124"),
            "the wall exits 124; content was: {result}"
        );
        assert!(
            result.contains("`block at tool call, line 1`"),
            "the surviving worker is named by the line that deferred it; content was: {result}"
        );
    }

    /// The same probe, on a call the wall did not cut: a completed call's
    /// worker is nobody's orphan, so nothing is said about it.
    #[test]
    fn a_call_that_returns_says_nothing_about_its_workers() {
        let session = Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());

        let result = session.ral("let ok = defer { return 1 }", 10, &emit).text;

        assert!(
            !result.contains("`block at tool call, line 1`"),
            "a call that returned holds its own handle; content was: {result}"
        );
    }

    /// The widening this wave deliberately introduces: a live birth outlives a
    /// routine non-zero exit exactly as it outlives the wall, so it draws the
    /// same sentence there too.
    #[cfg(unix)]
    #[test]
    fn a_non_zero_exit_with_a_live_birth_names_the_orphan() {
        let session = Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());

        let result = session
            .ral(
                "let deferred = defer { sleep 20 }\n/bin/sh -c 'exit 3'",
                10,
                &emit,
            )
            .text;

        assert!(
            result.contains("EXIT: 3"),
            "the command's own exit is the tool exit; content was: {result}"
        );
        assert!(
            result.contains("`block at tool call, line 1`"),
            "a non-zero exit leaves a live birth standing exactly as the wall does; content was: {result}"
        );
    }

    /// A panic cannot resurrect a name pruned before it: the run door's
    /// checkpoint is taken at the panicking call's own entry, by which time
    /// the prune is already part of the state being checkpointed.
    #[test]
    fn panic_after_prune_does_not_resurrect_binding() {
        let mut session = dressed_trunk(|shell| {
            shell.install_builtins(PANIC_BUILTINS);
            shell.arm_binding_lease(ral_core::types::BindingLease {
                idle_calls: 2,
                large_binding_bytes: u64::MAX,
            });
        });

        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.agent.id);
        session.ral("let panic_prune_x = 1", 5, &emit);
        session.ral("let _spin1 = 0", 5, &emit);
        session.ral("let _spin2 = 0", 5, &emit);

        // The probe is itself a call, so it may prune the idle `_spin` names
        // too — nothing below asserts on those.
        assert!(
            !scope_has(&session, "panic_prune_x"),
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
        session.agent.provider.swap(provider);
        session.seed("compute then crash".into());
        let _ = session.attend(&emit);

        // `survives_y` first: each probe ticks the armed idle bound of 2, and
        // reading a name renews it, so the second probe cannot prune it.
        assert!(
            scope_has(&session, "survives_y"),
            "a completed call's binding must survive a later call's panic"
        );
        assert!(
            !scope_has(&session, "panic_prune_x"),
            "the pruned name must not resurrect across the panic's rollback"
        );
    }

    /// Against the real `BINDING_IDLE_CALLS`, not a re-armed test bound: a
    /// boot-seeded name is baseline and never ages out.
    #[test]
    fn boot_names_survive_past_the_idle_bound() {
        let session = Avatar::for_test("system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.agent.id);

        let boot_name = session
            .seat
            .read(reading::bindings)
            .expect("an identity seat never severs")
            .into_iter()
            .next()
            .expect("the boot sequence seeds at least one binding")
            .name;

        for _ in 0..(shell_eval::BINDING_IDLE_CALLS + 5) {
            session.ral("let _boot_spin = 0", 5, &emit);
        }
        assert!(
            scope_has(&session, &boot_name),
            "a boot-seeded (baseline) name must survive past the idle bound"
        );
    }

    /// The settled-worker retention ledger on the engine's own clock: one tick
    /// per dispatched call, a sweep at each ready boundary, and the expiry's
    /// notice riding a later run's surface stream back to the bus.
    #[test]
    fn retention_expiry_renders_through_the_drain() {
        // A tiny bound so the expiry is a couple of calls away; this replaces
        // the production constant the recipe armed.
        let session = dressed_trunk(|shell| shell.arm_worker_retention(1));
        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.agent.id, session.inbox.mailbox());

        session.ral("spawn { return 1 }", 5, &emit);

        // Through the probe rail, not a `ral` call: a boundary read ticks
        // nothing, so the retention arithmetic below stays exact.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !session
            .seat
            .read(reading::workers)
            .expect("an identity seat never severs")
            .iter()
            .any(|w| !w.running)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the instant worker must settle within the budget"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        // Which call stamps and which expires depends on when the worker
        // settled, so drive calls until the notice lands, bounded.
        let mut reaps = 0;
        for _ in 0..6 {
            session.ral("$[0]", 5, &emit);
            for record in crate::bus::drain_records(&rx) {
                let crate::record::Record::Display(crate::record::Display::Notice {
                    notice: crate::record::NoticeFact::Reap { cmd, cause },
                }) = record
                else {
                    continue;
                };
                assert_eq!(
                    cmd, "block at tool call, line 1",
                    "the reap names the spawned body by its line"
                );
                assert_eq!(
                    cause, "retention",
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
            probe_count(&session, ral_core::test_access::worker_count),
            0,
            "the expired entry left the registry"
        );
    }
}
