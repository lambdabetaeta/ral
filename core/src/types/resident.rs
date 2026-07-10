//! The resident signature (`decisions/260705_session-ledger`): the small,
//! chapter-agnostic interface every session-lived, capability-reachable
//! "thing that stays alive between turns" answers, so the folds built
//! against it — listing, the exit warning, cancellation, `/resources` —
//! are written once instead of once per chapter.
//!
//! The signature is the unification; the chapters keep their own
//! representations, locks, and homes
//! (`decisions/260615_no-core-repr-leak-into-exarch`) — the worker
//! registry beside core's handles, the job table in the REPL binary, the
//! agent registry and schedules in exarch. Fusing them into one registry
//! struct is exactly what this trait refuses to do: [`Resident`] asks each
//! chapter to *answer* through its own representation, never to restructure
//! itself to match a shared one. Capabilities in particular are
//! deliberately not unified: [`Resident::capability_kind`] names the KIND
//! (`"handle"`, `"pgid"`, `"agent-id"`, `"schedule-id"`, `"name"`), never a
//! value, so the honest variance between a `Value::Handle` and a pgid stays
//! typed and distinct.
//!
//! Extracted in parcel 9 from the first two folds that needed it — the
//! REPL's `jobs` listing and its exit-time survivor warning
//! (`ral/src/repl/host_handlers.rs`) — shaped by exactly what they consume:
//! a bracketed designator and a state word. A fold wraps [`designator`](Resident::designator)
//! in `[...]` uniformly rather than each chapter deciding its own
//! bracketing; population-specific detail a fold also needs (a worker's
//! `cmd`, a job's `pgid`) stays exactly that — read directly off the
//! concrete type, never lifted into the signature, per the ADR's line that
//! the unification is the interface, never a flattening of the structs.
//! [`lease_row`](Resident::lease_row) and [`capability_kind`](Resident::capability_kind)
//! round out the shape the ADR licenses for `/resources` and future folds,
//! even though no fold built so far reads them.
//!
//! ## Implemented, and refused
//!
//! Core implements it for [`WorkerEntry`](super::WorkerEntry) (`shell/workers.rs`);
//! the REPL implements it for its own `Job` (`ral/src/jobs.rs`) — the two
//! chapters whose folds motivated the extraction. exarch's three chapters
//! (agent registry entries, schedules, the binding ledger) all refuse: each
//! only ever hands out a bare snapshot type for listing (`AgentInfo`,
//! `ScheduleInfo`; the binding ledger does not even have one), built by
//! cloning fields out from under a lock that is dropped before the snapshot
//! reaches the caller. [`Resident::cancel`] takes `&self` alone, by design —
//! a fold cancels *through* the resident, never by also passing back a
//! registry and an id — but none of those three snapshots carries a live
//! handle to its own registry, and adding one would either grow the
//! snapshot into something that holds a lock across a listing (breaking the
//! "enumeration is not observation" rule every listing here obeys) or
//! duplicate the registry's own cancel/unschedule plumbing onto a type that
//! was never meant to outlive the call that built it. That is the
//! structural cost `decisions/260705_session-ledger`'s "the ledger is an
//! interface, not a struct" explicitly licenses refusing rather than
//! forcing: `agent_cancel`, `schedules`' own cancel, and the binding
//! prune sweep remain each chapter's own verb, not a detour through this
//! trait.

/// One resident's signature, projected from whatever representation its
/// own chapter keeps.
///
/// Every method takes `&self`: a fold reaches a
/// resident's facets through the value it already has in hand (an entry
/// from a listing snapshot), never by also threading back a registry and
/// an id.
pub trait Resident {
    /// This resident's id as shown to a human — `"w3"`, `"3"`, an agent
    /// id, a schedule id — unbracketed. A fold wraps it in `[...]`
    /// uniformly, so no two chapters can disagree on the bracketing.
    fn designator(&self) -> String;

    /// The chapter this resident belongs to: `"worker"`, `"job"`,
    /// `"agent"`, `"schedule"`, `"binding"` — the population a listing
    /// fold groups by, distinct from the capability that reaches it.
    fn population(&self) -> &'static str;

    /// The kind of typed value that reaches and controls this resident —
    /// `"handle"`, `"pgid"`, `"agent-id"`, `"schedule-id"`, `"name"` — never
    /// the value itself: capabilities are deliberately not unified.
    fn capability_kind(&self) -> &'static str;

    /// The lease-table row this resident renders for a probe fold: a
    /// clock, an idle bound, a backstop, or a degenerate case like
    /// `"none — human-owned"` (a stopped job) or `"none — durable"` (a
    /// service). Never renews the lease it describes.
    fn lease_row(&self) -> String;

    /// This resident's state as a human-facing word — `"running"`,
    /// `"done"`, `"stopped"` — including whatever chapter-specific
    /// qualifier the population wants folded in (a worker's `"(worker)"`
    /// suffix, say); a fold prints it verbatim.
    fn state_label(&self) -> String;

    /// Fire this resident's own teardown edge — the chapter's `cancel`,
    /// never a shared kill switch reaching in from outside it. Cooperative,
    /// like every cancel in this system: this call only ever raises a flag
    /// or sends a signal, and the resident notices at its own next check.
    fn cancel(&self);
}
