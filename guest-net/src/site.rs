// The typed-site vocabulary of guest-net: see `lint/site.rs`.
include!("../../lint/site.rs");

/// Sites that raise no card.
#[derive(Clone, Copy)]
pub(crate) enum Silent {
    /// Turns the CONNECT target's name into the addresses `open` then judges:
    /// a resolution step, not the tunnel itself, and the verdict it feeds is
    /// what `exarch::egress`'s durable append-only ledger records.  It raises
    /// no card: the model reaches the network through `exec` (curl and kin),
    /// which surfaces as that exec.
    VetResolve,
    /// Dials one address `is_public` already admitted: the proxy's post-vet
    /// connect.  Its accounting is `exarch::egress`, a durable append-only
    /// ledger carrying every CONNECT verdict and the bytes it carried, so the
    /// tunnel is recorded without a card: the model reaches the network
    /// through `exec`, which surfaces as that exec.
    VetDial,
}

impl Site for Silent {}
