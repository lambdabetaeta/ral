//! The one generic replay driver: `records.try_fold(Memo::default(),
//! F::step)`, carrying the `fold == memo` proof once for every [`Fold`]
//! rather than once per consumer.  No fold lives here — `record::model` and
//! `record::view` bring their own `step` in a later parcel — only the
//! driver and the error it can refuse a session with.

use super::log::Log;
use super::{Fold, Record, Recorded};
use std::fmt;
use std::io;
use std::path::Path;

/// Why a fold could not finish replaying the log: the file itself would not
/// read back, or a fold declined a record it does not recognise.
///
/// The latter is what makes the versioned display vocabulary a requirement
/// rather than a loss merely accepted, since an unrecognised record refuses
/// the session instead of silently falling out of it.
#[derive(Debug)]
pub enum Refusal {
    Unreadable(io::Error),
    Foreign { record: Box<Record>, reason: String },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(e) => write!(f, "the record log would not read back: {e}"),
            Self::Foreign { reason, .. } => write!(f, "refused a foreign record: {reason}"),
        }
    }
}

impl std::error::Error for Refusal {}

impl From<io::Error> for Refusal {
    fn from(e: io::Error) -> Self {
        Self::Unreadable(e)
    }
}

/// Fold `path` into `F::Memo`, refusing the session at the first record `F`
/// does not recognise.
///
/// # Errors
/// Returns [`Refusal`] if the file will not read back, or if `F::step`
/// refuses one of its records.
pub fn replay<F: Fold>(path: &Path) -> Result<F::Memo, Refusal> {
    Log::read(path)?.try_fold(F::Memo::default(), |mut memo, record| {
        let record: Recorded<Record> = record?;
        F::step(&mut memo, &record)?;
        Ok(memo)
    })
}
