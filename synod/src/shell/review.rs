//! The report seam: the window's one read-only command over
//! [`synod::workspace`].
//!
//! The library speaks manifests and change sets; the window renders a flat
//! list of cards.  This module translates one into the other and holds the
//! result, so the window can ask for it again — a render, a resize, a
//! reopened panel — without the folder being walked on its behalf.  The
//! live conversation is what fills this: each finished exchange hands its
//! own report in through [`hold`], replacing the last one wholesale, so the
//! cards always describe that exchange's folder and never a stale one.
//!
//! Nothing here writes.  Every change the report names is already real in
//! the user's folder — that is what the report is an account of — so there
//! is no status to distinguish, nothing to put back, and no conflict a
//! put-back could discover.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ral_core::sync::LockExt;
use serde::Serialize;
use synod::workspace::{Change, JobReport};
use tauri::State;

/// How a file was changed, judged against the folder as it was before
/// the job.
#[derive(Clone, Copy, Serialize, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Created,
    Modified,
    /// Written to, with the size unchanged.  Synod reads no file's bytes,
    /// so whether the contents differ is not something it can say — and the
    /// window must show this as its own thing rather than as an edit.
    Touched,
    Deleted,
    Renamed,
}

/// One changed file, as one card on the report screen.
#[derive(Clone, Serialize, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub struct ChangeFile {
    /// The file, named relative to the folder.  For a rename, the name
    /// it has now.  The window keys its rows on this.
    pub path: String,
    /// For a rename, the name it had before.
    pub rename_from: Option<String>,
    pub kind: ChangeKind,
    /// The file as it is now, absolute, to open.  Absent for a deletion.
    pub current_path: Option<String>,
}

/// The whole payload the report screen renders.
#[derive(Clone, Serialize, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub struct WindowReport {
    pub files: Vec<ChangeFile>,
    /// Paths synod could not read while taking one of the two walks this
    /// report compares, so nothing about them could be shown above.
    pub unreadable: Vec<String>,
}

/// The cards the window is showing, and the folder they describe.
struct Held {
    /// Checked against every later call's own `folder`, so a stale or
    /// mismatched frontend can never be handed the report from a different
    /// conversation.
    folder: PathBuf,
    report: WindowReport,
}

/// The card list the window is showing.
#[derive(Default)]
pub struct Review(Mutex<Option<Held>>);

/// Take the conversation's own report for `folder` and hold it as cards for
/// the window to ask for.
///
/// Called by the worker thread after every exchange, whether the exchange
/// succeeded or not: whatever changed before a failure is still in the
/// folder, and a report that stopped at the last good turn would be a
/// quieter account than the truth.
pub(crate) fn hold(review: &Review, folder: &Path, report: &JobReport) {
    let files = report
        .changes
        .changes
        .iter()
        .map(|change| {
            let (kind, shown, from) = match change {
                Change::Created { path, .. } => (ChangeKind::Created, path.clone(), None),
                Change::Modified { path } => (ChangeKind::Modified, path.clone(), None),
                Change::Touched { path } => (ChangeKind::Touched, path.clone(), None),
                Change::Deleted { path, .. } => (ChangeKind::Deleted, path.clone(), None),
                Change::Renamed { from, to } => {
                    (ChangeKind::Renamed, to.clone(), Some(from.clone()))
                }
            };
            ChangeFile {
                current_path: (!matches!(change, Change::Deleted { .. }))
                    .then(|| folder.join(&shown).to_string_lossy().into_owned()),
                rename_from: from,
                path: shown,
                kind,
            }
        })
        .collect();

    *review.0.lock_ignore_poison() = Some(Held {
        folder: folder.to_path_buf(),
        report: WindowReport {
            files,
            unreadable: report.unreadable.clone(),
        },
    });
}

/// Forget whatever is held — a fresh conversation's cards must not open
/// showing the last one's.
pub(crate) fn forget(review: &Review) {
    *review.0.lock_ignore_poison() = None;
}

/// What the folder's conversation has changed so far, as cards.
///
/// # Errors
/// A plain sentence when no exchange has finished in this folder yet, or
/// when the window and the shell have drifted apart on which folder is
/// live.
#[tauri::command]
pub fn job_report(review: State<'_, Review>, folder: String) -> Result<WindowReport, String> {
    let folder = PathBuf::from(folder);
    let guard = review.0.lock_ignore_poison();
    let result = match guard.as_ref() {
        None => Err("Synod has not finished a job in this folder yet.".to_string()),
        Some(held) if held.folder == folder => Ok(held.report.clone()),
        Some(held) => Err(format!(
            "The open report is for {}, not {} — refusing to answer for a different folder.",
            held.folder.display(),
            folder.display()
        )),
    };
    drop(guard);
    result
}
