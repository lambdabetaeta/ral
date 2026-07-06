//! The host-embedding accessor surface.
//!
//! [`Shell::turn`](super::Shell), `session`, and `local` are `pub(crate)`:
//! the fields that encode turn safety are not a public API, so a host cannot
//! reach in and install an unrelated foreground scope, clear the source
//! registry mid-turn, or swap a stream behind the turn guard's back.  Hosts
//! drive a session through the narrow operations gathered here — the ones the
//! REPL and exarch actually need — while [`Shell::mobile`](super::Shell) stays
//! the public embedding seam.

use super::MobileSnapshot;
use super::Shell;
use super::TerminalAccess;
use super::bindings::{BindingLease, BindingPruneNotice, LargeBindingNotice};
use super::repl::ReplScratch;
use super::workers::ReapCause;
use crate::diagnostic::SourceDb;
use crate::exit_hints::ExitHints;
use crate::io::{Sink, TerminalState};
use crate::process::{DurableRoot, ForegroundScope, TerminalLease};
use crate::types::{AuditFragment, ReapNotice, Value, WorkerEntry, WorkerId};

/// An in-flight terminal loan, returned by [`Shell::begin_terminal_loan`] and
/// surrendered to [`Shell::end_terminal_loan`].
///
/// Opaque to hosts: it carries the prior [`TerminalAccess`] to restore but
/// exposes no way to read or forge one. Obtaining it raises an already-`Leased`
/// turn to [`TerminalAccess::ExplicitLoan`] (a `Denied` turn is left untouched,
/// so the loan can only raise authority, never mint it); surrendering it
/// restores the prior access. This is the only path to an `ExplicitLoan` — a
/// `TurnRequest` cannot seed it — so the elevation is always a within-turn loan
/// held by the host that suspended its own terminal surface.
pub struct TerminalLoan(TerminalAccess);

impl Shell {
    /// The session's durable source registry.  Hosts read it after a turn
    /// returns to render a runtime error against the right source text.
    pub fn sources(&self) -> &SourceDb {
        &self.session.sources
    }

    /// The session's durable cancel root.  A turn door mints a turn's
    /// foreground scope from it (`self.durable_root().child()`); the typed
    /// relation keeps that scope rooted here.  No longer a host accessor —
    /// frame assembly moved behind the turn doors, so this is `pub(crate)`.
    pub(crate) fn durable_root(&self) -> &DurableRoot {
        &self.session.root
    }

    /// A clonable cancel handle on this session's durable root.  A host
    /// that runs several sessions in one process (exarch's agent fleet)
    /// keeps one per session, so its own cancel cascade can reach the eval
    /// layer: cancelling the handle unwinds the session's in-flight turn at
    /// the evaluator's poll points and stops its detached workers.  For a
    /// forked session — which never publishes the process-global signal
    /// slots — this handle is the *only* way to stop a running eval.
    pub fn cancel_handle(&self) -> DurableRoot {
        self.session.root.clone()
    }

    /// The current turn's foreground scope.  A host clones a deadline child
    /// of it, or cancels it to interrupt the foreground work.
    pub fn foreground(&self) -> &ForegroundScope {
        &self.turn.cancel
    }

    /// Install the startup-loaded exit-code hint table.
    pub fn set_exit_hints(&mut self, hints: ExitHints) {
        self.session.exit_hints = hints;
    }

    /// Cached terminal state probed at startup (isatty / ANSI / mode bits).
    /// `Copy`, so frontends read the bits they need without borrowing.
    pub fn terminal(&self) -> TerminalState {
        self.turn.io.terminal
    }

    /// Whether the shell is running as an interactive REPL.
    pub fn is_interactive(&self) -> bool {
        self.turn.io.interactive
    }

    /// Mark the shell interactive (or not).  The interactive REPL sets this
    /// at boot so external commands and prompts behave as a live session.
    pub fn set_interactive(&mut self, interactive: bool) {
        self.turn.io.interactive = interactive;
    }

    /// Install the session stdout sink.  The interactive REPL installs its
    /// `ExternalPrinter` here so background output lands above the prompt.
    pub fn set_stdout(&mut self, stdout: Sink) {
        self.turn.io.stdout = stdout;
    }

    /// The session stderr sink, for a host to write diagnostics into.
    pub fn stderr_mut(&mut self) -> &mut Sink {
        &mut self.turn.io.stderr
    }

    /// Turn on top-level audit collection with byte capture (`ral --audit`).
    /// SPEC §10.3: every emitted command node carries stdout/stderr, so the
    /// trail is installed under `CapturePolicy::Bytes` — mirroring the
    /// `audit { … }` builtin, not the default `None` policy that would leave
    /// those fields empty.
    pub fn enable_audit(&mut self) {
        self.local
            .audit
            .install_active_policy(Some(crate::types::CapturePolicy::Bytes));
    }

    /// Drain the accumulated audit trail as a fragment (e.g. for `--audit`
    /// JSON output at end of run).
    pub fn take_audit_fragment(&mut self) -> AuditFragment {
        self.local.audit.take_fragment()
    }

    /// The names of every installed builtin command, for tab completion.
    pub fn builtin_names(&self) -> impl Iterator<Item = &str> {
        self.session.builtins.names()
    }

    /// Read-only access to REPL/editor scratch (plugin context, TUI flag,
    /// queued chpwd notification).
    pub fn repl(&self) -> &ReplScratch {
        &self.local.repl
    }

    /// Mutable access to REPL/editor scratch.  This is the REPL host's own
    /// state; exposing it to the host that owns it is the seam, not a leak.
    pub fn repl_mut(&mut self) -> &mut ReplScratch {
        &mut self.local.repl
    }

    /// Every worker (`spawn`, `watch`, `service`) registered on this shell,
    /// settled or still running. There is no by-id control plane: a listing
    /// hands back the handle itself, so rediscovering a worker is list, take
    /// the handle back, and resume `poll`/`await`/`race`/`cancel` as usual.
    /// Never mutates the registry.
    pub fn workers(&self) -> Vec<WorkerEntry> {
        self.local.workers.snapshot()
    }

    /// Re-acquire one registered worker's entry by id, once a host has
    /// learned it some other way (a ledger row it maintains).  A pure read
    /// like [`Self::workers`]: renews no lease.  `None` names an id with no
    /// live entry.
    pub fn worker_by_id(&self, id: WorkerId) -> Option<WorkerEntry> {
        self.local.workers.lookup(id)
    }

    /// Number of workers currently registered on this shell.
    pub fn worker_count(&self) -> usize {
        self.local.workers.count()
    }

    /// Drain the reap notices recorded since the last drain — one compact
    /// record per entry removed by policy (the lease chain's idle bound or
    /// backstop, the retention sweep's expiry), never one for an entry an
    /// eliminator observed away first. Crate-private: this used to be a host
    /// accessor a frontend polled at its own ready boundary and composed into
    /// a transcript event itself; [`Self::emit_ready_boundary_notices`] is now
    /// the one caller, pushing the fact as a `` `notice `` surface class from
    /// *inside* the turn that produced it instead
    /// (`decisions/260706_enquiry-channel` §4.2).
    pub(crate) fn take_worker_reap_notices(&mut self) -> Vec<ReapNotice> {
        self.local.workers.take_reap_notices()
    }

    /// Advance this shell's worker registry to the host's ral-call `epoch`,
    /// sweeping settled entries against `retention` — stamp an entry first
    /// observed settled, expire one whose unclaimed result has sat stamped
    /// for `retention` or more calls (a
    /// [`Retention`](crate::types::ReapCause::Retention) notice rides the
    /// same drain as the lease chain's). The host calls
    /// this once per ral call, after the evaluation returns; a host that
    /// never calls it (the REPL) retains settled entries indefinitely.
    pub fn advance_worker_epoch(&mut self, epoch: u64, retention: u64) {
        self.local.workers.advance_epoch(epoch, retention);
    }

    /// Cancel every worker registered on this shell and reset the registry
    /// wholesale, entries and pending reap notices alike. This is the
    /// host's `/clear` arm: explicit destruction outranks every lease, the
    /// durable class included, so a cleared shell starts with neither
    /// stray running workers nor stale reap events. Returns how many
    /// entries were cancelled.
    pub fn cancel_workers(&mut self) -> usize {
        self.local.workers.cancel_all()
    }

    /// Number of lexical bindings in the session scope, innermost shadowing
    /// counted once — the binding chapter's probe figure for a host's
    /// resource fold. A count, never the values: enumeration renews
    /// nothing, exactly as listing workers renews no lease.
    pub fn binding_count(&self) -> usize {
        self.bindings().len()
    }

    /// Number of names currently leased (tracked, non-baseline) by the
    /// binding-lease ledger — the `/resources` probe's leased-count figure,
    /// a narrower read than [`Self::binding_count`] (which counts every
    /// visible binding, baseline included). `0` when unarmed. Read-only:
    /// counting renews nothing.
    pub fn leased_binding_count(&self) -> usize {
        self.local.bindings.leased_count()
    }

    /// Drain every large-binding notice queued since the last drain — one
    /// per session-scope install whose value's shallow-size estimate met
    /// the armed lease's threshold (`decisions/260629_agent-binding-reaping`).
    /// Crate-private for the same reason as [`Self::take_worker_reap_notices`]:
    /// [`Self::emit_ready_boundary_notices`] is now the one caller; the
    /// binding itself is never touched by this call.
    pub(crate) fn take_large_binding_notices(&mut self) -> Vec<LargeBindingNotice> {
        self.local.bindings.take_large_binding_notices()
    }

    /// Push this shell's ready-boundary housekeeping as `` `notice `` surface
    /// events — a worker the lease chain reaped and a session-scope install
    /// past the large-binding threshold — through the installed turn's own
    /// surface sink, in the order the underlying ledgers accumulated them.
    /// Called once per settled turn ([`crate::turn::run_framed`]), *before*
    /// the turn's frame tears down, so the notice rides the same turn's
    /// surface stream, ordered before its Report
    /// (`decisions/260706_enquiry-channel` §4.2).
    ///
    /// A no-op — the ledgers are left untouched, not drained-and-dropped —
    /// when no surface sink is installed (a bare REPL, or a host-embedding
    /// test dispatching a raw `TurnRequest`): with nobody to push to, the
    /// notices simply wait for a turn that does install one, exactly as
    /// they did before this turn ran.
    ///
    /// The binding-lease ledger's *idle-prune* notice does not ride here: a
    /// prune is a host-timed act, called between turns
    /// ([`Self::prune_idle_bindings`] hands back the post-prune
    /// [`MobileSnapshot`] a panic-recovery baseline needs, which cannot
    /// itself cross the turn/surface/Report seam) — with no turn installed
    /// at that call site, there is no live surface sink to push through
    /// either. It stays the one notice a host still composes itself from
    /// the polled return, an acknowledged residue of this migration.
    pub(crate) fn emit_ready_boundary_notices(&mut self) {
        if self.turn.surface.is_none() {
            return;
        }
        for notice in self.take_worker_reap_notices() {
            self.surface(reap_notice_value(&notice));
        }
        for notice in self.take_large_binding_notices() {
            self.surface(large_binding_notice_value(&notice));
        }
    }

    /// Arm this shell's binding-lease ledger and seal the boot baseline:
    /// every name currently visible anywhere in the scope chain — prelude,
    /// agent library, rc bindings, host seed vars — becomes permanently
    /// exempt from expiry. Idempotent by replacement: a re-arm discards any
    /// prior ledger state and reseals from the scope as it stands now. A
    /// host that never calls this observes no expiry
    /// (`decisions/260629_agent-binding-reaping`).
    pub fn arm_binding_lease(&mut self, lease: BindingLease) {
        let baseline = self
            .mobile
            .scope
            .all_bindings()
            .into_iter()
            .map(|(name, _)| name);
        self.local.bindings.arm(lease, baseline);
    }

    /// Prune every leased name idle past the armed lease's bound.
    ///
    /// Returns `None` when the ledger is unarmed, when the shell is not at
    /// session scope (a mid-frame caller — e.g. a lifecycle hook — is
    /// refused rather than allowed to unset from a transient frame), or
    /// when nothing was pruned. Otherwise returns the notices **and a
    /// [`MobileSnapshot`] taken after the prune**: any durable checkpoint
    /// the host holds is now stale, and the returned one replaces it — the
    /// signature makes forgetting to refresh it a type error rather than a
    /// reviewer's job (`decisions/260629_agent-binding-reaping`).
    ///
    /// One pass, in deterministic (sorted) name order, over every expired
    /// candidate: a name absent from scope (its install was rolled back by
    /// a panic restore) drops its orphaned entry silently, with no notice;
    /// a name whose value still structurally reaches a running handle is
    /// skipped — pinned, not pruned, re-examined at the next boundary; every
    /// other name is unset (removing its scheme in the same act, since a
    /// `Binding` couples both), its entry dropped, and a notice recorded.
    /// Afterward, an adoption sweep gives any untracked, non-baseline name
    /// still in the session's top scope a fresh lease starting now — this
    /// runs even on a pass that prunes nothing, self-healing a missed
    /// install path into "leased late" rather than "immortal".
    pub fn prune_idle_bindings(&mut self) -> Option<(Vec<BindingPruneNotice>, MobileSnapshot)> {
        if !self.local.bindings.armed() || !self.mobile.scope.at_session_scope() {
            return None;
        }
        let mut notices = Vec::new();
        for (name, idle_calls) in self.local.bindings.expired() {
            match self.mobile.scope.get(&name) {
                None => {
                    // Orphaned by a panic rollback: nothing to prune.
                    self.local.bindings.drop_entry(&name);
                }
                Some(value) if crate::types::pins_running_work(value) => {
                    // Pinned: leave the entry exactly as it is.
                }
                Some(value) => {
                    let kind = value.type_name();
                    self.mobile.scope.unset(&name);
                    self.local.bindings.drop_entry(&name);
                    notices.push(BindingPruneNotice {
                        name,
                        idle_calls,
                        kind,
                    });
                }
            }
        }
        let top_scope_names: Vec<String> = self.mobile.scope.top_scope().keys().cloned().collect();
        for name in top_scope_names {
            self.local.bindings.adopt(&name);
        }
        if notices.is_empty() {
            None
        } else {
            Some((notices, self.mobile_snapshot()))
        }
    }

    /// The terminal-foreground handoff borrow: `Some(&TerminalLease)` iff the
    /// installed turn's [`TerminalAccess`] permits it (`Leased` or
    /// `ExplicitLoan`) *and* the session actually owns a lease. The single
    /// gate every post-startup foreground handoff funnels through — the
    /// pipeline launch, the standalone foreground command, and `fg`-resume —
    /// so a turn that was not handed authority (an exarch tool turn installs
    /// `Denied`) cannot construct the handoff: it has no `&TerminalLease` to
    /// pass [`ForegroundGuard::try_acquire`](crate::process::ForegroundGuard::try_acquire).
    pub fn terminal_lease(&self) -> Option<&TerminalLease> {
        match self.turn.terminal_access {
            TerminalAccess::Denied => None,
            TerminalAccess::Leased | TerminalAccess::ExplicitLoan => {
                self.session.terminal_lease.as_ref()
            }
        }
    }

    /// Begin an explicit terminal loan on the installed turn (the `_ed-tui`
    /// case): a foreground handoff may now fire even though stdout is captured,
    /// because the body draws on `/dev/tty` and must own the foreground pgid.
    /// The host must have suspended its own terminal reader/renderer first. The
    /// loan only *raises* an already-`Leased` turn to [`TerminalAccess::ExplicitLoan`];
    /// a `Denied` turn is left untouched, so the loan can only raise an
    /// authorised turn, never mint authority. The returned [`TerminalLoan`]
    /// restores the prior access when surrendered to [`Self::end_terminal_loan`].
    /// Mirrors the within-turn set/clear of the retired `tui_active` flag.
    pub fn begin_terminal_loan(&mut self) -> TerminalLoan {
        let prev = self.turn.terminal_access;
        // The loan may only *raise* an already-authorised turn; it never mints
        // authority. A `Denied` turn is left untouched, closing the
        // `Denied → ExplicitLoan` door the manual token previously left open.
        if matches!(prev, TerminalAccess::Leased) {
            self.turn.terminal_access = TerminalAccess::ExplicitLoan;
        }
        TerminalLoan(prev)
    }

    /// End an explicit terminal loan, restoring the access recorded when it
    /// began. Pair with [`Self::begin_terminal_loan`].
    pub fn end_terminal_loan(&mut self, loan: TerminalLoan) {
        self.turn.terminal_access = loan.0;
    }

    /// Whether the installed turn is currently under an explicit terminal loan.
    /// The re-entrancy guard for `_ed-tui`: a nested loan would be a logic
    /// error, so the editor builtin refuses when this is already true.
    pub fn in_terminal_loan(&self) -> bool {
        matches!(self.turn.terminal_access, TerminalAccess::ExplicitLoan)
    }

    /// Exit status of the last command (`$?`).  A host reads it to set its
    /// own process exit code or to seed a prompt's status segment.
    pub fn last_status(&self) -> i32 {
        self.mobile.control.last_status
    }

    /// Set the last-command exit status (`$?`) to an explicit code.  The
    /// integer-valued sibling of
    /// [`set_status_from_bool`](Shell::set_status_from_bool).
    pub fn set_last_status(&mut self, status: i32) {
        self.mobile.control.last_status = status;
    }

    /// Run `f` with `last_status` saved across it and restored afterwards.
    /// The prompt cycle uses it: rendering `RAL_PROMPT` runs a value turn
    /// whose own status must not clobber the previous command's exit code,
    /// which the next prompt segment still wants to read.
    pub fn with_preserved_status<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.mobile.control.last_status;
        let r = f(self);
        self.mobile.control.last_status = saved;
        r
    }

    /// The active non-tail call-depth ceiling.
    pub fn recursion_limit(&self) -> usize {
        self.mobile.control.recursion_limit
    }

    /// Set the non-tail call-depth ceiling (rc `recursion_limit:` /
    /// `--recursion-limit`).
    pub fn set_recursion_limit(&mut self, n: usize) {
        self.mobile.control.recursion_limit = n;
    }

    /// Install the invocation positional args (`$args`, `$1`, …) — the
    /// script arguments a CLI host passes after the program path.
    pub fn set_args(&mut self, args: Vec<String>) {
        self.mobile.context.args = args;
    }

    /// The acting principal (`$USER` from the dynamic env, empty if unset).
    /// Forwards to [`Context::principal`](super::Context::principal).
    pub fn principal(&self) -> String {
        self.mobile.context.principal()
    }

    /// Set a dynamic env-var override (`within [shell: …]`'s per-key door,
    /// also the seam a host uses to seed `NO_COLOR`, `EXARCH_SESSION_DIR`,
    /// and the like).  Forwards to
    /// [`Context::set_env_var`](super::Context::set_env_var).
    pub fn set_env_var(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.mobile.context.set_env_var(k, v);
    }

    /// Bulk-insert dynamic env-var overrides.  Forwards to
    /// [`Context::extend_env`](super::Context::extend_env) — the seam a host
    /// uses to seed a batch of vars at boot.
    pub fn extend_env<I, K, V>(&mut self, items: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.mobile.context.extend_env(items);
    }

    /// Read an env var through the dynamic overlay, falling back to the host
    /// process environment — the `within [shell: K=…]` overlay-on-process
    /// rule. A host driving command completion reads `PATH` here.
    pub fn env_var(&self, name: &str) -> Option<String> {
        self.mobile.context.env_overrides().get_or_host(name)
    }

    /// Number of capability frames on the grant stack (the ambient root plus
    /// every live `grant` / `within` attenuation).  Hosts assert grant-stack
    /// balance across a turn boundary with it — e.g. that a panicking tool
    /// call left no leaked frame behind.  The qualitative companion is
    /// [`Shell::has_active_capabilities`](Shell::has_active_capabilities).
    pub fn grant_depth(&self) -> usize {
        self.mobile.context.grants.iter().count()
    }
}

/// Encode one [`ReapNotice`] as `` `notice [kind: `reap, cmd, cause] `` — the
/// wire shape [`Self::emit_ready_boundary_notices`] pushes and the exarch
/// host decodes back (`decisions/260706_enquiry-channel` §4.2). `cause`
/// travels as the same lowercase tag exarch's own transcript writer already
/// used for it, so the wire word and the forensic-record word are one word.
fn reap_notice_value(notice: &ReapNotice) -> Value {
    let cause = match notice.cause {
        ReapCause::Idle => "idle",
        ReapCause::Backstop => "backstop",
        ReapCause::Retention => "retention",
    };
    Value::Variant {
        label: "notice".into(),
        payload: Some(Box::new(Value::map(vec![
            (
                "kind".into(),
                Value::Variant {
                    label: "reap".into(),
                    payload: None,
                },
            ),
            ("cmd".into(), Value::String(notice.cmd.clone())),
            ("cause".into(), Value::String(cause.into())),
        ]))),
    }
}

/// Encode one [`LargeBindingNotice`] as
/// `` `notice [kind: `large-binding, name, bytes] ``, the large-binding
/// sibling of [`reap_notice_value`].
fn large_binding_notice_value(notice: &LargeBindingNotice) -> Value {
    Value::Variant {
        label: "notice".into(),
        payload: Some(Box::new(Value::map(vec![
            (
                "kind".into(),
                Value::Variant {
                    label: "large-binding".into(),
                    payload: None,
                },
            ),
            ("name".into(), Value::String(notice.name.clone())),
            ("bytes".into(), Value::Int(notice.bytes as i64)),
        ]))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::TerminalLease;

    /// The handoff door: the lease borrow is reachable *only* when the turn's
    /// access permits *and* the session owns a lease. A `Denied` turn cannot
    /// reach the borrow even though the session owns the lease — the exarch
    /// tool-turn case, where the foreground handoff becomes unconstructable.
    #[test]
    #[cfg(unix)]
    fn terminal_lease_gated_by_access_and_session() {
        let mut shell = Shell::default();
        shell.session.terminal_lease = TerminalLease::mint_at_startup(true);
        assert!(
            shell.session.terminal_lease.is_some(),
            "session owns a lease"
        );

        shell.turn.terminal_access = TerminalAccess::Denied;
        assert!(
            shell.terminal_lease().is_none(),
            "a Denied turn cannot borrow the session lease"
        );

        shell.turn.terminal_access = TerminalAccess::Leased;
        assert!(
            shell.terminal_lease().is_some(),
            "a Leased turn borrows the session lease"
        );

        // Even with authority, no borrow when the session minted no lease
        // (a backgrounded / piped / tty-less launch).
        shell.session.terminal_lease = None;
        assert!(
            shell.terminal_lease().is_none(),
            "no session lease → no borrow, regardless of access"
        );
    }

    /// The loan token raises the turn to `ExplicitLoan` and restores the prior
    /// access on surrender — the within-turn `_ed-tui` elevation, mirroring the
    /// retired `tui_active` set/clear.
    #[test]
    fn terminal_loan_round_trips_access() {
        let mut shell = Shell::default();
        shell.turn.terminal_access = TerminalAccess::Leased;
        assert!(!shell.in_terminal_loan());

        let loan = shell.begin_terminal_loan();
        assert!(
            shell.in_terminal_loan(),
            "loan raises the turn to ExplicitLoan"
        );

        shell.end_terminal_loan(loan);
        assert!(!shell.in_terminal_loan());
        assert_eq!(
            shell.turn.terminal_access,
            TerminalAccess::Leased,
            "ending the loan restores the pre-loan access"
        );
    }

    /// The loan only *raises* an authorised turn; it never mints authority.
    /// A `Denied` turn calling `begin_terminal_loan` is left `Denied` — the
    /// `Denied → ExplicitLoan` door is closed — so even with a session lease the
    /// foreground borrow stays unreachable.
    #[test]
    #[cfg(unix)]
    fn denied_turn_loan_does_not_elevate() {
        let mut shell = Shell::default();
        shell.session.terminal_lease = TerminalLease::mint_at_startup(true);
        shell.turn.terminal_access = TerminalAccess::Denied;

        let loan = shell.begin_terminal_loan();
        assert!(
            !shell.in_terminal_loan(),
            "a Denied turn is not raised to ExplicitLoan"
        );
        assert!(
            shell.terminal_lease().is_none(),
            "no foreground borrow: the loan cannot mint authority from Denied"
        );

        shell.end_terminal_loan(loan);
        assert_eq!(shell.turn.terminal_access, TerminalAccess::Denied);
    }
}
