//! How one deliberation ends: the vocabulary [`Agent::deliberate`] answers
//! in, and [`attend`](super::attend) and [`nudge`](super::nudge) read.
//!
//! [`Agent::deliberate`]: super::Agent::deliberate

use crate::provider::ProviderError;
use crate::record::{Unavailable, Uncommitted};
use ral_core::carrier::Severed;

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
    /// The session log refused a mutation as written; the session is usable.
    Refused(String),
    /// Recording failed: the session has ended.
    Unavailable(Unavailable),
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

impl From<Unavailable> for Fault {
    fn from(failure: Unavailable) -> Self {
        Self::Unavailable(failure)
    }
}

impl From<Uncommitted> for Fault {
    fn from(uncommitted: Uncommitted) -> Self {
        match uncommitted {
            Uncommitted::Refused(why) => Self::Refused(why),
            Uncommitted::Unavailable(failure) => Self::Unavailable(failure),
        }
    }
}
