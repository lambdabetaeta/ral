//! The report-and-undo seam: the window's commands over
//! [`synod::workspace`].
//!
//! The library speaks manifests, change sets, and restore outcomes; the
//! window renders a flat list of cards and remembers nothing between
//! calls.  This module translates one into the other, and holds the card
//! list so an undo's outcome can be folded into the last report returned.
//! Each finished exchange rebuilds the cards fresh from its own job
//! report, so a card's status reflects that job, never a stale one.
//!
//! Conflicts surface where they matter.  An undo is first tried gently
//! ([`Resolution::KeepCurrent`]): a file the user edited after the job
//! comes back marked conflicted and nothing is touched.  The window then
//! asks — keep yours, or put back the older one — and only the explicit
//! choice retries with [`Resolution::PutBack`].  The library keeps the
//! newer bytes first either way, so no click here destroys anything.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ral_core::sync::LockExt;
use serde::Serialize;
use synod::workspace::restore::covers;
use synod::workspace::{self, Change, EntryKind, HistoryStore, Resolution, RestoreOutcome};
use tauri::{AppHandle, State};

/// How a file was changed, judged against the folder as it was before
/// the job.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
    Renamed,
}

/// Where a change stands: already real in the folder, or put back.
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Applied,
    PutBack,
}

/// One changed file, as one card on the report screen.
#[derive(Clone, Serialize)]
pub struct ChangeFile {
    /// The handle the undo commands name the card by — the path as the
    /// report shows it, which the library resolves even across a rename.
    pub id: String,
    /// The file, named relative to the folder.  For a rename, the name
    /// it has now.
    pub path: String,
    /// For a rename, the name it had before.
    pub rename_from: Option<String>,
    pub kind: ChangeKind,
    pub status: ChangeStatus,
    /// True when putting this file back was refused because the user
    /// changed it after the job — the card becomes a choice.
    pub conflict: bool,
    /// The file as it is now, absolute, to open.  Absent for a deletion.
    pub current_path: Option<String>,
    /// The name to ask [`open_earlier`] for — the before-side name.
    /// Absent for a creation: there was nothing there before.
    pub before_path: Option<String>,
}

/// The whole payload the report screen renders.
#[derive(Clone, Serialize)]
pub struct WindowReport {
    pub files: Vec<ChangeFile>,
    /// Paths synod could not read while taking one of the two checkpoints
    /// this report compares, so nothing about them could be shown above.
    pub unreadable: Vec<String>,
}

/// What [`job_report`] last found, kept so undo outcomes can be folded
/// into the card list without losing the report's other facts.
struct Held {
    /// The folder this report was taken from — checked against every later
    /// call's own `folder`, so a stale or mismatched frontend can never
    /// fold an undo into a report from a different job.
    folder: PathBuf,
    files: Vec<ChangeFile>,
    unreadable: Vec<String>,
}

/// The card list the window is showing, kept so undo outcomes can be
/// folded into it.
#[derive(Default)]
pub struct Review(Mutex<Option<Held>>);

/// What the folder's last job changed, as cards.
///
/// # Errors
/// A plain sentence when no job has finished in this folder, or its
/// records cannot be read.
#[tauri::command]
pub fn job_report(review: State<'_, Review>, folder: String) -> Result<WindowReport, String> {
    let folder = PathBuf::from(folder);
    let store = HistoryStore::open_for(&folder)?;
    let report = workspace::job_report(&store, &folder)?;

    let files: Vec<ChangeFile> = report
        .changes
        .changes
        .iter()
        .map(|change| {
            let (kind, shown, from) = match change {
                Change::Created { path, .. } => (ChangeKind::Created, path.clone(), None),
                Change::Modified { path } => (ChangeKind::Modified, path.clone(), None),
                Change::Deleted { path, .. } => (ChangeKind::Deleted, path.clone(), None),
                Change::Renamed { from, to } => {
                    (ChangeKind::Renamed, to.clone(), Some(from.clone()))
                }
            };
            ChangeFile {
                id: shown.clone(),
                current_path: (!matches!(change, Change::Deleted { .. }))
                    .then(|| folder.join(&shown).to_string_lossy().into_owned()),
                before_path: (!matches!(change, Change::Created { .. }))
                    .then(|| from.clone().unwrap_or_else(|| shown.clone())),
                rename_from: from,
                path: shown,
                kind,
                status: ChangeStatus::Applied,
                conflict: false,
            }
        })
        .collect();

    let unreadable = report.unreadable;
    *review.0.lock_ignore_poison() = Some(Held {
        folder,
        files: files.clone(),
        unreadable: unreadable.clone(),
    });
    Ok(WindowReport { files, unreadable })
}

/// Put one file — or, through a rename, its pair of names — back the way
/// it was.  `force` is the "put back the older one" choice at a conflict.
///
/// # Errors
/// A plain sentence when no job has run here, when `id` names nothing
/// known, or when the restore fails partway.
#[tauri::command]
pub fn undo_file(
    review: State<'_, Review>,
    folder: String,
    id: String,
    force: bool,
) -> Result<WindowReport, String> {
    let folder = PathBuf::from(folder);
    check_folder(&review, &folder)?;
    let store = HistoryStore::open_for(&folder)?;
    let outcome = workspace::undo_file(&store, &folder, &id, resolution(force))?;
    absorb(&review, &outcome)
}

/// Put everything from the last job back.  Files the user edited after
/// the job stay put and come back as conflicts unless `force`.
///
/// # Errors
/// A plain sentence when no job has run here, or the restore fails
/// partway.
#[tauri::command]
pub fn undo_all(
    review: State<'_, Review>,
    folder: String,
    force: bool,
) -> Result<WindowReport, String> {
    let folder = PathBuf::from(folder);
    check_folder(&review, &folder)?;
    let store = HistoryStore::open_for(&folder)?;
    let outcome = workspace::undo_all(&store, &folder, resolution(force))?;
    absorb(&review, &outcome)
}

/// Open the version of `path` from before the job — materialised from the
/// folder's history into a temporary file, then handed to the user's own
/// application, so comparing at a conflict is looking at real documents.
///
/// # Errors
/// A plain sentence when there is no recorded earlier version, or the
/// copy cannot be made or opened.
#[tauri::command]
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: setting out a recorded earlier version inside the history \
              store's own scratch area, because the user asked to see it; the model is not \
              in this loop and the folder itself is untouched"
)]
pub fn open_earlier(
    app: AppHandle,
    review: State<'_, Review>,
    folder: String,
    path: String,
) -> Result<(), String> {
    let folder = PathBuf::from(folder);
    check_folder(&review, &folder)?;
    let store = HistoryStore::open_for(&folder)?;
    let Some((before, _)) = store.latest_job()? else {
        return Err("Synod has no record of this folder before the job.".to_string());
    };
    match before.manifest.entries.get(&path) {
        Some(EntryKind::File { hash, .. }) => {
            let bytes = store.read_object(hash)?;
            let dest = store.scratch_dir()?.join(&path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Could not set out the earlier version: {e}"))?;
            }
            std::fs::write(&dest, bytes)
                .map_err(|e| format!("Could not set out the earlier version: {e}"))?;
            super::commands::open_with_default(&app, &dest.to_string_lossy())
        }
        Some(_) => Err(format!(
            "{path} was a folder before the job, so there is no earlier document to open."
        )),
        None => Err(format!("There was nothing called {path} before the job.")),
    }
}

/// Refuse a call whose `folder` does not match the one [`job_report`] last
/// held — the window always sends `state.folder`, so a mismatch means the
/// window and the shell have drifted apart on which folder is live.
fn check_folder(review: &Review, folder: &Path) -> Result<(), String> {
    let guard = review.0.lock_ignore_poison();
    let result = match guard.as_ref() {
        None => Err("There is no job report open.".to_string()),
        Some(held) if held.folder == folder => Ok(()),
        Some(held) => Err(format!(
            "The open report is for {}, not {} — refusing to act on a different folder.",
            held.folder.display(),
            folder.display()
        )),
    };
    drop(guard);
    result
}

fn resolution(force: bool) -> Resolution {
    if force {
        Resolution::PutBack
    } else {
        Resolution::KeepCurrent
    }
}

/// Fold a restore's outcome into the held cards and return the updated
/// report.  A card counts as touched when any of its names — both names,
/// for a rename — was put back, removed, or conflicted, exactly or as a
/// folder with the outcome's path inside it.  A conflict outranks being
/// partly done: the card stays in place and asks.
fn absorb(review: &Review, outcome: &RestoreOutcome) -> Result<WindowReport, String> {
    let mut guard = review.0.lock_ignore_poison();
    let held = guard
        .as_mut()
        .ok_or_else(|| "There is no job report open.".to_string())?;

    for file in &mut held.files {
        let mut names = vec![file.path.as_str()];
        if let Some(from) = &file.rename_from {
            names.push(from.as_str());
        }
        let touched = |path: &String| names.iter().any(|name| covers(path, name));
        if outcome.conflicts.iter().any(touched) {
            file.conflict = true;
        } else if outcome.put_back.iter().any(touched) || outcome.removed.iter().any(touched) {
            file.status = ChangeStatus::PutBack;
            file.conflict = false;
        }
    }
    let report = WindowReport {
        files: held.files.clone(),
        unreadable: held.unreadable.clone(),
    };
    drop(guard);
    Ok(report)
}
