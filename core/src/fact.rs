//! The records ral reads back: a failure, an observation, what a worker
//! wrote, what a detach hands over.
//!
//! Each owns its encoding and its ral type, and knows no `Value` or `Shell`: the
//! checker reads its types off these declarations, and the doors that mint
//! them live above (`shell::runtime::error`, `shell::runtime::audit`, the builtins).

mod ending;
mod error;
mod fs;
mod observation;

pub use ending::{Done, Ending, ExitCode, FailureCode, PANIC_CODE, tag};
pub use error::{ErrorRecord, Failure, NotRunnable, Process, Reason, Unrunnable};
pub(crate) use fs::{DirEntry, FileInfo};
pub use observation::{
    Act, AuditIo, Check, Command, CommandOrigin, Decision, Grep, Observation, Observed, Read,
    Resource, Worker, Write, WriteOutcome,
};

use crate::data::Bytes;
use crate::data::datum::Datum;
use crate::io::Streams;
use crate::record;
use crate::ty::{Ty, Typed};
use std::time::SystemTime;

/// Stable identifier for a registered worker, minted from a process-global
/// counter rather than a per-registry one, so ids never collide across
/// shells.
///
/// A fleet listing folds several agents' registries together.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkerId(pub u64);

impl Datum for WorkerId {
    fn encode(self) -> crate::data::FOValue {
        self.0.encode()
    }

    fn decode(v: &crate::data::FOValue) -> Result<Self, String> {
        u64::decode(v).map(Self)
    }
}

impl Typed for WorkerId {
    fn ty() -> Ty {
        Ty::int()
    }
}

/// Seconds since the epoch, or 0 for a pre-epoch instant: how a record
/// carries a wall-clock time.
pub(crate) fn unix_secs(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// What a worker wrote while it ran: the streams both of `poll`'s records
/// carry.
pub type Io = Streams<Bytes>;

record!(typed Io {
    stdout: "stdout",
    stderr: "stderr",
});

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
