//! The records ral reads back: a failure, an observation, what a worker
//! wrote, what a detach hands over.
//!
//! Each is declared once, as the `record!`, `variant!` or `label!` that yields
//! both its encoding and its ral type, and knows no `Value` or `Shell`: the
//! checker reads its types off these declarations, and the doors that mint
//! them live above (`types::error`, `types::audit`, the builtins).

mod error;
mod observation;

pub use error::{ErrorRecord, Reason};
pub use observation::{
    Act, Check, Command, CommandOrigin, Decision, Grep, Observation, Observed, Read, Resource,
    Worker, Write, WriteOutcome,
};

use crate::first_order::Bytes;
use crate::first_order::datum::Datum;
use crate::ty::{Ty, Typed};
use crate::{label, record};
use strum::{IntoStaticStr, VariantArray};

/// Stable identifier for a registered worker, minted from a process-global
/// counter rather than a per-registry one, so ids never collide across
/// shells.
///
/// A fleet listing folds several agents' registries together.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WorkerId(pub u64);

impl Datum for WorkerId {
    fn encode(self) -> crate::first_order::FOValue {
        self.0.encode()
    }

    fn decode(v: &crate::first_order::FOValue) -> Result<Self, String> {
        u64::decode(v).map(Self)
    }
}

impl Typed for WorkerId {
    fn ty() -> Ty {
        Ty::Int
    }
}

/// Which reaping policy governs a worker, declared at birth by the spawning
/// door.
#[derive(Copy, Clone, Debug, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum LeaseClass {
    /// Governed by the frame's `WorkerLease` when one is supplied.
    Worker,
    /// A `service`-born worker: the lease chain is never armed for it, so it
    /// dies only by cancel, `/clear`, or process exit.
    Durable,
}

label!(typed LeaseClass);

/// What a worker wrote while it ran: `poll`'s `` `pending `` payload, and the
/// streams every settled record carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Io {
    pub stdout: Bytes,
    pub stderr: Bytes,
}

record!(typed Io {
    stdout: "stdout",
    stderr: "stderr",
});

impl Io {
    pub fn of(stdout: impl Into<Bytes>, stderr: impl Into<Bytes>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }
}

/// What a detach hands back in place of a handle: the survivor's pid and the
/// description it was born under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub pid: u32,
    pub desc: String,
}

record!(typed Receipt {
    pid: "pid",
    desc: "desc",
});
