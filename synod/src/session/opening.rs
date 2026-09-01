//! What the window shows before the first message: who is answering, and
//! the folder's own prose about the safety copy — see [`Opening`].

use crate::workspace;
use std::path::Path;

/// What the window shows before the first message: who is answering, at
/// what effort, and the ~2GiB warning when the folder is that large.
#[derive(Clone, serde::Serialize)]
pub struct Opening {
    /// The answering account's
    /// [`identity::label`](exarch::provider::identity::label), set-relative
    /// to every account available when the conversation began. A display
    /// string, and named as one on the wire, unlike
    /// [`crate::session::Choice::account`]'s id.
    pub label: String,
    /// The model that account is driving.
    pub model: String,
    /// The [`EFFORT_LADDER`](exarch::provider::EFFORT_LADDER) label of the
    /// effort actually in force, after
    /// [`crate::session::resolve_tuning`]'s masking — not what was asked
    /// for, which the window already knows and which a model that takes no
    /// reasoning control never receives.
    pub effort: String,
    /// What the folder itself made worth saying before the first message:
    /// that the copy will take a while, or that there was no room for one
    /// and this conversation has no undo.
    pub folder_line: Option<String>,
}

/// The free figure that says the folder's safety copy will not fit where the
/// store goes, or `None` when it will.
///
/// A free figure this host cannot read counts as room: a conversation keeps
/// its undo unless a positive number says it cannot be had. Asked from a
/// stat-only [`workspace::manifest::measure`], so the answer arrives before
/// a single byte is read or copied.
pub(super) fn no_room_for_copy(
    measure: workspace::manifest::Measure,
    folder: &Path,
) -> Option<u64> {
    workspace::history::free_bytes_for(folder).filter(|free| *free < measure.bytes)
}

/// The line before a copy that will take a while — nothing but the folder's
/// own size, since the store never survives the conversation that opened it.
pub(super) fn slow_copy_line(measure: workspace::manifest::Measure) -> Option<String> {
    (measure.bytes > workspace::LARGE_FOLDER_BYTES).then(|| {
        format!(
            "This folder holds about {} across {} files.  Synod keeps a copy of \
             everything before starting, which can take a while — possibly minutes \
             on a shared drive.",
            as_gb(measure.bytes),
            measure.files
        )
    })
}

/// The line when no copy was made at all, because there was nowhere to put
/// one.  Says what was not done and what follows from it, since a safety net
/// silently absent is worse than one openly declined.
pub(super) fn no_copy_line(measure: workspace::manifest::Measure, free: u64) -> String {
    format!(
        "This folder holds about {}, and the disk has about {} free — not room for the \
         copy Synod usually keeps before it starts.  It has not made one, so nothing done \
         in this conversation can be put back afterwards.  Would you rather free up some \
         space first, or work in a smaller folder?",
        as_gb(measure.bytes),
        as_gb(free)
    )
}

/// `bytes` as a one-decimal gigabyte figure, the large-folder warning's unit
/// throughout.
fn as_gb(bytes: u64) -> String {
    let tenths = bytes / 100_000_000;
    format!("{}.{} GB", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Which way this comparison runs decides whether a folder too big to
    /// copy is opened without undo or copied until the disk fills, and an
    /// inversion would pass every other test in this file.
    #[test]
    fn a_folder_larger_than_the_disk_gets_no_copy() {
        let folder = crate::test_fixture::workshop("session-room");
        let measure = |bytes| workspace::manifest::Measure { files: 1, bytes };

        assert!(
            no_room_for_copy(measure(u64::MAX), folder.path()).is_some(),
            "no disk holds u64::MAX bytes, so no copy can be promised"
        );
        assert!(
            no_room_for_copy(measure(0), folder.path()).is_none(),
            "an empty folder always has room"
        );
    }
}
