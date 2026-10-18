//! How one deliberation ends: the vocabulary [`Avatar::deliberate`] answers
//! in, and [`attend`](super::attend) and [`nudge`](super::nudge) read.
//!
//! [`Avatar::deliberate`]: super::Avatar::deliberate

use crate::provider::ProviderError;
use ral_core::carrier::Severed;
use std::io;

/// [`Self::Empty`] and [`Self::Stopped`] become nudges, [`Self::Cancelled`]
/// does not.
#[derive(Debug)]
pub enum Outcome {
    /// The model spoke and called no tool.
    Complete,
    /// A returning agent called `reply`: the value is deposited on its agent
    /// for its consumer to read.  Distinct from [`Self::Complete`] so the
    /// nudge layer never re-nudges an agent that already answered.
    Replied,
    Empty,
    Stopped {
        reason: String,
    },
    Cancelled,
}

/// Why a deliberation ended short of an [`Outcome`].
#[derive(Debug)]
pub enum Fault {
    /// The provider's own failure, which the attend loop records and may
    /// nudge over.
    Provider(ProviderError),
    /// The engine is gone: no further tool call can dispatch, and the loop
    /// around this one ends too.
    Severed(Severed),
    /// The session log refused or failed an append; the context is left as
    /// it lay.
    Log(String),
    /// A host-side panic: transport, surface decode, render.  An eval-side
    /// panic is caught at `Shell::run`, which rolls the dynamic state back,
    /// so it never unwinds this far.
    Panicked(String),
}

impl From<ProviderError> for Fault {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}

impl From<Severed> for Fault {
    fn from(severed: Severed) -> Self {
        Self::Severed(severed)
    }
}

impl From<io::Error> for Fault {
    fn from(error: io::Error) -> Self {
        Self::Log(error.to_string())
    }
}
