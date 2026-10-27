//! File buttons act only on checked descendants of the held report's folder.

use crate::site::{Silent, site};
use ral_core::path::{shown, spelled};
use std::path::{Path, PathBuf};

use super::state::Review;
use crate::wire::OpenAs;
use ral_core::authority::policy::Ungoverned;
use ral_core::path::{EntryName, Kind, LeafMode, Stat, descendant, walk_dir};
use ral_core::sync::LockExt;
use tauri::{AppHandle, State};

/// Ordinary path components only; refuse traversal, roots and Windows alternate streams.
fn inside(rel: &str) -> Result<Vec<EntryName>, String> {
    descendant(rel)
        .map_err(|_| format!("Synod will not open {rel}: its name points outside your folder."))
}

pub(crate) struct Target {
    /// Keep the joined spelling: some Windows applications cannot open canonical verbatim paths.
    pub(crate) path: PathBuf,
    pub(crate) how: OpenAs,
}

/// Verify index and shown name, then walk without following links; only plain files survive.
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
        return Err(format!(
            "{rel} is not a plain file now (it was deleted, or is a folder or a link), \
             so there is nothing to open."
        ));
    };
    let names = inside(&rel)?;
    let (last, above) = names.split_last().expect("a descendant has a name");

    // The user's folder, without a grant: ask only what kind of object the handle holds.
    let missing =
        |e: &dyn std::fmt::Display| format!("Synod could not find {rel} in your folder ({e}).");
    let held = walk_dir(&spelled(&folder)?).map_err(|e| missing(&e))?;
    let dir = above
        .iter()
        .try_fold(held, |dir, name| {
            site!(Silent::ReviewTarget, Ungoverned::read_dir(dir)).open_dir(name)
        })
        .map_err(|e| missing(&e))?;
    let located = site!(Silent::ReviewTarget, Ungoverned::read_dir(dir))
        .locate(last, LeafMode::NoFollow)
        .map(|entry| site!(Silent::ReviewTarget, Ungoverned::read(entry)))
        .map_err(|e| missing(&e))?;
    let stat = located
        .stat()
        .ok_or_else(|| missing(&std::io::ErrorKind::NotFound))?;
    if stat.kind == Kind::Symlink || is_reparse_point(stat) {
        return Err(format!(
            "{rel} is a link to somewhere else, not a file of its own, so synod will not open it."
        ));
    }
    if stat.kind != Kind::File {
        return Err(format!("{rel} is not a file, so there is nothing to open."));
    }
    let path = names
        .iter()
        .fold(folder, |at, name| at.join(name.as_os_str()));
    Ok(Target { path, how })
}

#[cfg(windows)]
fn is_reparse_point(stat: &Stat) -> bool {
    stat.reparse_tag.is_some()
}

#[cfg(not(windows))]
fn is_reparse_point(_: &Stat) -> bool {
    false
}

#[tauri::command]
pub(crate) fn open_file(
    app: AppHandle,
    review: State<'_, Review>,
    index: usize,
    path: String,
) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let target = file_to_open(&review, index, &path)?;
    mark_from_elsewhere(&target.path);
    match target.how {
        OpenAs::Open => open_with_default(&target.path),
        OpenAs::Reveal => app
            .opener()
            .reveal_item_in_dir(&target.path)
            .map_err(|e| format!("Could not show {path} in its folder: {e}")),
    }
}

/// Marking is best-effort: FAT and some network shares cannot store alternate streams.
#[cfg(windows)]
#[allow(
    clippy::disallowed_macros,
    reason = "[silent:synod-gui] synod carries no ral shell to print through"
)]
fn mark_from_elsewhere(path: &Path) {
    let mut stream = path.as_os_str().to_os_string();
    stream.push(":Zone.Identifier");
    let written = site!(
        Silent::MarkOfTheWeb,
        std::fs::write(&stream, "[ZoneTransfer]\r\nZoneId=3\r\n")
    );
    if let Err(e) = written {
        eprintln!(
            "synod: could not mark {} as from elsewhere: {e}",
            shown(path)
        );
    }
}

#[cfg(not(windows))]
fn mark_from_elsewhere(_path: &Path) {}

/// Untrusted links admit only hosted http(s) or mailto; other schemes can execute local files.
pub(crate) fn link_to_follow(raw: &str) -> Result<url::Url, String> {
    let refuse = || {
        format!("Synod opens only web and email links, and {raw} is neither, so it was not opened.")
    };
    let url = url::Url::parse(raw).map_err(|_| refuse())?;
    match url.scheme() {
        "http" | "https" if url.host().is_some() => Ok(url),
        "mailto" => Ok(url),
        _ => Err(refuse()),
    }
}

#[tauri::command]
pub(crate) fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let link = link_to_follow(&url)?;
    app.opener()
        .open_url(link.as_str(), None::<&str>)
        .map_err(|e| format!("Could not open {url}: {e}"))
}

/// Caller must pass a checked document: the system can run whatever this path names.
fn open_with_default(path: &Path) -> Result<(), String> {
    tauri_plugin_opener::open_path(path, None::<&str>)
        .map_err(|e| format!("Could not open {}: {e}", shown(path)))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::window::state::Held;
    use crate::wire::{ChangeFile, ChangeKind, WindowReport, open_as};
    use std::sync::Mutex;

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
    fn a_plain_relative_name_stays_inside_the_folder() {
        let names = inside("sub/report.pdf").expect("plain name");
        assert_eq!(
            names.iter().map(EntryName::as_os_str).collect::<Vec<_>>(),
            ["sub", "report.pdf"]
        );
    }

    #[test]
    fn a_name_that_climbs_or_is_rooted_is_refused() {
        for name in [
            "../outside.pdf",
            "sub/../../x.pdf",
            "/etc/passwd",
            "./x.pdf",
            "",
        ] {
            assert!(inside(name).is_err(), "{name}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_share_or_stream_name_is_refused_on_windows() {
        for name in [
            r"C:\Windows\notepad.exe",
            r"\host\share\x.exe",
            r"sub\..\..\x.pdf",
            "tool.exe:notes.txt",
        ] {
            assert!(inside(name).is_err(), "{name}");
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
    fn a_placeholder_is_a_reparse_point_though_it_presents_as_a_file() {
        const CLOUD: u32 = 0x9000_001A;
        let placeholder = Stat {
            kind: Kind::File,
            len: 0,
            readonly: false,
            mtime: None,
            atime: None,
            btime: None,
            reparse_tag: Some(CLOUD),
            hidden: false,
        };
        assert!(is_reparse_point(&placeholder));
        assert!(!is_reparse_point(&Stat {
            reparse_tag: None,
            ..placeholder
        }));
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

    #[test]
    fn web_and_email_links_are_followed() {
        for link in [
            "https://example.com/page?q=1",
            "http://example.com",
            "HTTPS://Example.com/",
            "mailto:someone@example.com",
        ] {
            assert!(link_to_follow(link).is_ok(), "{link}");
        }
    }

    #[test]
    fn anything_that_could_run_a_program_is_refused() {
        for link in [
            "cmd.exe",
            "calc",
            "./run.bat",
            "../x.exe",
            r"\\host\share\x.exe",
            "//host/share/x.exe",
            r"C:\Windows\System32\cmd.exe",
            "C:/Windows/System32/cmd.exe",
            "file:///C:/Windows/System32/cmd.exe",
            "file://host/share/x.exe",
            "javascript:alert(1)",
            "ms-settings:",
            "search-ms:query=x",
            "vbscript:x",
            "tel:123",
            "http://",
            "",
        ] {
            assert!(link_to_follow(link).is_err(), "{link}");
        }
    }
}
