//! The resident signature: the chapter-agnostic interface every session-lived,
//! capability-reachable thing answers, so a fold over them — the REPL's `jobs`
//! listing, its exit-time survivor warning — is written once rather than once
//! per chapter. Each chapter answers through its own representation, never
//! restructuring to match a shared one; capabilities especially stay
//! unflattened, [`Resident::capability_kind`] naming a kind and never a value.
//! Core implements it for [`WorkerEntry`](super::WorkerEntry), the REPL for its
//! `Job`.

/// One resident's signature, projected from whatever representation its own
/// chapter keeps.
///
/// Every method takes `&self`: a fold reads these facets off the listing
/// entry it already holds, never by threading back a registry and an id.
pub trait Resident {
    /// This resident's id as shown to a human, unbracketed — a fold adds the
    /// `[...]`, so no two chapters can disagree on the bracketing.
    fn designator(&self) -> String;

    /// The chapter this resident belongs to, distinct from the capability that
    /// reaches it.
    fn population(&self) -> &'static str;

    /// The kind of typed value that reaches and controls this resident —
    /// `"handle"`, `"pgid"` — never the value itself.
    fn capability_kind(&self) -> &'static str;

    /// The lease-table row this resident renders for a probe fold: a clock, an
    /// idle bound, or a degenerate case like `"none — human-owned"`. Never
    /// renews the lease it describes.
    fn lease_row(&self) -> String;

    /// This resident's state as a human-facing word, carrying any
    /// chapter-specific qualifier (a worker's `"(worker)"` suffix); a fold
    /// prints it verbatim.
    fn state_label(&self) -> String;

    /// Fire this resident's own teardown edge, never a shared kill switch
    /// reaching in from outside its chapter. Cooperative: this raises a flag or
    /// sends a signal, and the resident notices at its own next check.
    fn cancel(&self);
}
