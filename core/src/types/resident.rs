//! The resident signature (`decisions/260705_session-ledger`): the small,
//! chapter-agnostic interface every session-lived, capability-reachable
//! "thing that stays alive between runs" answers, so the folds built
//! against it — listing, the exit warning, cancellation, `/resources` —
//! are written once instead of once per chapter.
//!
//! The signature is the unification; the chapters keep their own
//! representations, locks, and homes
//! (`decisions/260615_no-core-repr-leak-into-exarch`). [`Resident`] asks
//! each chapter to *answer* through its own representation, never to
//! restructure itself to match a shared one. Capabilities in particular are
//! deliberately not unified: [`Resident::capability_kind`] names the KIND
//! (`"handle"`, `"pgid"`, `"agent-id"`, `"schedule-id"`, `"name"`), never a
//! value, so the honest variance between a `Value::Handle` and a pgid stays
//! typed and distinct.
//!
//! Core implements it for [`WorkerEntry`](super::WorkerEntry); the REPL
//! implements it for its own `Job`. exarch's chapters refuse: they hand out
//! bare snapshot types for listing rather than a live handle to their own
//! registry, and `decisions/260705_session-ledger` licenses that refusal —
//! the ledger is an interface, not a struct.

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
    /// `"agent"`, `"schedule"`, `"binding"`, distinct from the capability
    /// that reaches it. Part of the shape the ADR licenses; no fold groups
    /// by it yet.
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
