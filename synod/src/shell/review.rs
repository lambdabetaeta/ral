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

use std::path::{Component, Path, PathBuf};
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

/// What a card's one button does with its file.  Everything a change card
/// names was written by the guest, so only a kind of document the user's own
/// application reads as data is handed to it; anything else is shown in its
/// folder instead, where the user can see what it is before deciding.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
#[serde(rename_all = "snake_case")]
pub enum OpenAs {
    /// Open it with the user's own application for that kind of document.
    Open,
    /// Show it selected in the system's file manager, and run nothing.
    Reveal,
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
    /// What the card's button does with the file as it is now.  Absent for
    /// a deletion, which leaves nothing to open.  The window names the file
    /// back by its place in [`WindowReport::files`], never by a path of its
    /// own, so the shell alone decides which file on disk a button reaches.
    pub open: Option<OpenAs>,
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
                open: (!matches!(change, Change::Deleted { .. })).then(|| open_as(&shown)),
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

/// The document kinds a card may hand to the user's own application,
/// matched against the final extension without regard to case.  Each is read
/// as data by the application that usually owns it.  Web pages (`html`,
/// `htm`) are left out on purpose: a browser runs their scripts and lets them
/// reach other files on this computer.
const OPENABLE: &[&str] = &[
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "txt", "md",
    "csv", "tsv", "json", "png", "jpg", "jpeg", "gif", "webp", "svg",
];

/// Whether the file named `rel` is opened or only shown in its folder,
/// judged by its final extension alone: `Invoice.pdf.lnk` is a shortcut, not
/// a PDF.  A name carrying a colon is never opened, since on Windows that
/// names an alternate stream of some other file rather than a file of its
/// own.
pub(crate) fn open_as(rel: &str) -> OpenAs {
    let name = rel.rsplit(['/', '\\']).next().unwrap_or(rel);
    let openable = !name.contains(':')
        && name
            .rsplit_once('.')
            .is_some_and(|(_, ext)| OPENABLE.iter().any(|ok| ok.eq_ignore_ascii_case(ext)));
    if openable {
        OpenAs::Open
    } else {
        OpenAs::Reveal
    }
}

/// Join `rel`, a name the report gives relative to `folder`, onto it, or
/// refuse when the name could reach outside: every part must be an ordinary
/// name, never a root, a drive, `..` or `.`, and on Windows none may carry a
/// colon, which there names an alternate stream.
///
/// This checks only the name.  [`file_to_open`] also asks the disk, because
/// a link inside the folder can still point out of it.
fn inside(folder: &Path, rel: &str) -> Result<PathBuf, String> {
    let refuse = || format!("Synod will not open {rel}: its name points outside your folder.");
    #[allow(
        clippy::disallowed_methods,
        reason = "path: splitting a name the change report already holds into its parts, \
                  to refuse any that is not a plain name; nothing is resolved here"
    )]
    let parts = Path::new(rel).components();
    let mut joined = folder.to_path_buf();
    let mut any = false;
    for part in parts {
        let Component::Normal(name) = part else {
            return Err(refuse());
        };
        if cfg!(windows) && name.to_string_lossy().contains(':') {
            return Err(refuse());
        }
        joined.push(name);
        any = true;
    }
    if any { Ok(joined) } else { Err(refuse()) }
}

/// A file a card's button may act on, found by the shell itself.
pub(crate) struct Target {
    /// The held folder joined to the held name, checked to be a plain file
    /// inside the folder.  Not the canonical form: on Windows that is a
    /// verbatim `\\?\` path, which some applications cannot open.
    pub path: PathBuf,
    pub how: OpenAs,
}

/// Find the file the card at `index` names, for its button to act on.
///
/// The window sends only the card's place in the report it was given and
/// the name it showed there, so a report that has moved on since the card
/// was drawn is caught rather than acted on for a different file.  The path
/// itself is the shell's own: the held folder joined to the held name.  The
/// file must be a plain file, never a link or other reparse point that would
/// send the system somewhere the card does not name, and must still be
/// inside the folder once every link on the way to it is followed.
///
/// # Errors
/// A plain sentence naming the file when any of that does not hold.
pub(crate) fn file_to_open(review: &Review, index: usize, shown: &str) -> Result<Target, String> {
    let guard = review.0.lock_ignore_poison();
    let found = guard.as_ref().and_then(|held| {
        let file = held.report.files.get(index)?;
        (file.path == shown).then(|| (held.folder.clone(), file.path.clone(), file.open))
    });
    drop(guard);
    let Some((folder, rel, open)) = found else {
        return Err(format!(
            "The list of changes has moved on since {shown} was shown, so nothing was opened. \
             Look at the list again and try once more."
        ));
    };
    let Some(how) = open else {
        return Err(format!("{rel} was deleted, so there is nothing to open."));
    };
    let path = inside(&folder, &rel)?;

    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:open-link-check] a stat of the file a change card names, without \
                  following it, to refuse a link before anything hands it to the system"
    )]
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|e| format!("Synod could not find {rel} in your folder ({e})."))?;
    if meta.file_type().is_symlink() || is_reparse_point(&meta) {
        return Err(format!(
            "{rel} is a link to somewhere else, not a file of its own, so synod will not open it."
        ));
    }
    if !meta.is_file() {
        return Err(format!("{rel} is not a file, so there is nothing to open."));
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "path: strict canonicalisation of the folder and of the file a card names, \
                  so a linked folder on the way cannot carry the file outside it. \
                  `canonicalise_strict` is pub(crate) in ral_core, as grant.rs notes"
    )]
    let (root, real) = (std::fs::canonicalize(&folder), std::fs::canonicalize(&path));
    match (root, real) {
        (Ok(root), Ok(real)) if real.starts_with(&root) => Ok(Target { path, how }),
        (Ok(_), Ok(_)) => Err(format!(
            "{rel} leads outside your folder, so synod will not open it."
        )),
        (Err(e), _) | (_, Err(e)) => {
            Err(format!("Synod could not find {rel} in your folder ({e})."))
        }
    }
}

/// Whether the file is a Windows reparse point: a junction, a symbolic link,
/// or anything else the file system redirects rather than reads.
#[cfg(windows)]
fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Elsewhere a link is the only redirection, and `is_symlink` has already
/// answered for it.
#[cfg(not(windows))]
fn is_reparse_point(_meta: &std::fs::Metadata) -> bool {
    false
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

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] scratch folders and files made and read only by these tests"
)]
mod tests {
    use super::*;

    /// A review holding `names` as created files in `folder`, in that order.
    fn holding(folder: &Path, names: &[&str]) -> Review {
        let files = names
            .iter()
            .map(|name| ChangeFile {
                path: (*name).to_string(),
                rename_from: None,
                kind: ChangeKind::Created,
                open: Some(open_as(name)),
            })
            .collect();
        Review(Mutex::new(Some(Held {
            folder: folder.to_path_buf(),
            report: WindowReport {
                files,
                unreadable: Vec::new(),
            },
        })))
    }

    #[test]
    fn documents_open_and_everything_else_is_shown_in_its_folder() {
        for name in [
            "report.pdf",
            "notes/Minutes.DOCX",
            "a/b/chart.Png",
            "data.csv",
            "README.md",
            "diagram.svg",
        ] {
            assert_eq!(open_as(name), OpenAs::Open, "{name}");
        }
        for name in [
            "summary.bat",
            "Invoice.pdf.lnk",
            "report.url",
            "page.html",
            "page.HTM",
            "setup.exe",
            "script.ps1",
            "Makefile",
            "trailing.pdf.",
            "spaced.pdf ",
            "tool.exe:notes.txt",
            "sub.pdf/inner.cmd",
        ] {
            assert_eq!(open_as(name), OpenAs::Reveal, "{name}");
        }
    }

    #[test]
    fn a_plain_relative_name_stays_inside_the_folder() {
        let folder = PathBuf::from("granted");
        assert_eq!(
            inside(&folder, "sub/report.pdf").expect("plain name"),
            folder.join("sub").join("report.pdf")
        );
    }

    #[test]
    fn a_name_that_climbs_or_is_rooted_is_refused() {
        let folder = PathBuf::from("granted");
        for name in [
            "../outside.pdf",
            "sub/../../x.pdf",
            "/etc/passwd",
            "./x.pdf",
            "",
        ] {
            assert!(inside(&folder, name).is_err(), "{name}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_share_or_stream_name_is_refused_on_windows() {
        let folder = PathBuf::from("granted");
        for name in [
            r"C:\Windows\notepad.exe",
            r"\host\share\x.exe",
            r"sub\..\..\x.pdf",
            "tool.exe:notes.txt",
        ] {
            assert!(inside(&folder, name).is_err(), "{name}");
        }
    }

    #[test]
    fn the_card_names_its_file_by_place_and_name() {
        let dir = tempfile::tempdir().expect("scratch folder");
        std::fs::write(dir.path().join("report.pdf"), b"%PDF").expect("scratch file");
        std::fs::write(dir.path().join("run.bat"), b"echo").expect("scratch file");
        let review = holding(dir.path(), &["report.pdf", "run.bat"]);

        let target = file_to_open(&review, 0, "report.pdf").expect("the held file");
        assert_eq!(target.how, OpenAs::Open);
        assert!(target.path.ends_with("report.pdf"));

        let target = file_to_open(&review, 1, "run.bat").expect("the held file");
        assert_eq!(target.how, OpenAs::Reveal);

        // A place that no longer holds that name, or no place at all.
        assert!(file_to_open(&review, 1, "report.pdf").is_err());
        assert!(file_to_open(&review, 7, "report.pdf").is_err());
    }

    #[test]
    fn a_missing_file_or_a_folder_is_not_opened() {
        let dir = tempfile::tempdir().expect("scratch folder");
        std::fs::create_dir(dir.path().join("sub.pdf")).expect("scratch folder");
        let review = holding(dir.path(), &["gone.pdf", "sub.pdf"]);
        assert!(file_to_open(&review, 0, "gone.pdf").is_err());
        assert!(file_to_open(&review, 1, "sub.pdf").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_or_a_linked_folder_is_not_opened() {
        let dir = tempfile::tempdir().expect("scratch folder");
        let outside = tempfile::tempdir().expect("scratch folder");
        std::fs::write(outside.path().join("secret.pdf"), b"%PDF").expect("scratch file");
        std::os::unix::fs::symlink(
            outside.path().join("secret.pdf"),
            dir.path().join("link.pdf"),
        )
        .expect("link");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("away")).expect("link");
        let review = holding(dir.path(), &["link.pdf", "away/secret.pdf"]);
        assert!(file_to_open(&review, 0, "link.pdf").is_err());
        assert!(file_to_open(&review, 1, "away/secret.pdf").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_to_elsewhere_is_not_followed_out_of_the_folder() {
        let dir = tempfile::tempdir().expect("scratch folder");
        let outside = tempfile::tempdir().expect("scratch folder");
        std::fs::write(outside.path().join("secret.pdf"), b"%PDF").expect("scratch file");
        let junction = dir.path().join("away");
        // A junction needs no privilege, unlike a symbolic link.
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .output()
            .is_ok_and(|out| out.status.success());
        if !made {
            return;
        }
        let review = holding(dir.path(), &["away", "away/secret.pdf"]);
        assert!(file_to_open(&review, 0, "away").is_err());
        assert!(file_to_open(&review, 1, "away/secret.pdf").is_err());
    }
}
