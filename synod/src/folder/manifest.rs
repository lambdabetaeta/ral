//! Stat-only manifests never read file bytes or follow links; keys are slash-joined relative paths.

use crate::site::{Silent, site};
use ral_core::authority::policy::Ungoverned;
use ral_core::path::tree::walk::{Found, Unentered, Walk, WalkEntry};
use ral_core::path::{
    Kind, LeafMode, LexicalPath, Stat, relative_key, shown, spelled, spelled_parent, utf8, walk,
};
use ral_core::process::{CancelCause, DurableRoot, Scope};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) fn covers(path: &str, name: &str) -> bool {
    path == name
        || path
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File {
        size: u64,
        /// Unknown timestamps are 0; two unknown values compare equal.
        mtime_ns: u64,
        /// Unix permission bits; 0 on non-Unix hosts.
        mode: u32,
    },
    Folder,
    /// Target text only; never followed.
    Link {
        target: String,
    },
}

impl EntryKind {
    /// A timestamp alone cannot prove changed contents.
    pub(crate) fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::File { size, mode, .. },
                Self::File {
                    size: other_size,
                    mode: other_mode,
                    ..
                },
            ) => size == other_size && mode == other_mode,
            _ => self == other,
        }
    }
}

/// Empty folders are recorded too.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub(crate) entries: BTreeMap<String, EntryKind>,
    /// Listed but unrecorded paths are unread, never absent.
    pub(crate) unread: Vec<String>,
}

impl Manifest {
    pub(crate) fn of_folder(root: &Path) -> Result<Self, String> {
        Self::of_folder_via(root, &Stop::default(), &mut |_so_far| {})
    }

    /// A stopped walk is an error, never a short manifest.
    pub(crate) fn of_folder_via(
        root: &Path,
        stop: &Stop,
        progress: Progress<'_>,
    ) -> Result<Self, String> {
        let named = spelled(root)?;
        let mut manifest = Self::default();
        let mut files: u64 = 0;
        for entry in Walk::all(&named)
            .readable_by(site!(Silent::FolderManifest, Ungoverned::region()))
            .entries(stop.scope())
        {
            let entry = entry.map_err(|_| {
                "Synod stopped reading this folder before it had finished, so it cannot say what \
                 changed."
                    .to_string()
            })?;
            if entry.depth == 0 {
                entered(&entry, root)?;
                continue;
            }
            let key = relative_key(&entry.shown, &named).ok_or_else(|| {
                format!(
                    "A file in {} has a name this computer cannot read; please rename it first.",
                    shown(spelled_parent(entry.shown.as_path()).unwrap_or(root))
                )
            })?;
            if manifest.record(&entry, key)? {
                files += 1;
                progress(files);
            }
        }
        Ok(manifest)
    }

    fn record(&mut self, entry: &WalkEntry, key: String) -> Result<bool, String> {
        let unreadable = |e: &io::Error, what: &str| {
            format!(
                "Synod could not {what} {}: {e}.",
                shown(entry.shown.as_path())
            )
        };
        let kind = match &entry.found {
            Found::Seen(stat) => match stat.kind {
                Kind::Dir => EntryKind::Folder,
                Kind::File => EntryKind::File {
                    size: stat.len,
                    mtime_ns: mtime_ns(stat),
                    mode: file_mode(stat),
                },
                Kind::Symlink => match link_target(&entry.shown) {
                    Ok(target) => EntryKind::Link {
                        target: target_text(&target, entry)?,
                    },
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        record_unread(&mut self.unread, key);
                        return Ok(false);
                    }
                    Err(e) => return Err(unreadable(&e, "read the link")),
                },
                Kind::Other => {
                    return Err(format!(
                        "{} is not an ordinary file, folder, or link, so synod could not \
                         account for it; please move it out of the folder first.",
                        shown(entry.shown.as_path())
                    ));
                }
            },
            // Listed but unrecorded: unread, not absent.
            Found::Unseen(e)
            | Found::Unentered {
                why: Unentered::Refused(e),
                ..
            } if e.kind() == io::ErrorKind::NotFound => {
                record_unread(&mut self.unread, key);
                return Ok(false);
            }
            Found::Unseen(e) => return Err(unreadable(e, "look at")),
            Found::Unentered {
                why: Unentered::Refused(e),
                ..
            } => return Err(unreadable(e, "look inside")),
            Found::Unentered {
                why: Unentered::Pruned,
                ..
            } => unreachable!("a walk with no depth or mount limit prunes nothing"),
        };
        let file = matches!(kind, EntryKind::File { .. });
        self.entries.insert(key, kind);
        Ok(file)
    }
}

/// An unentered root fails the whole manifest; an empty one would falsely imply deletions.
fn entered(root: &WalkEntry, named: &Path) -> Result<(), String> {
    match &root.found {
        Found::Seen(Stat {
            kind: Kind::Dir, ..
        }) => Ok(()),
        Found::Unseen(e)
        | Found::Unentered {
            why: Unentered::Refused(e),
            ..
        } if e.kind() == io::ErrorKind::NotFound => Err(format!(
            "Synod could not find {} any more — has the folder been moved, renamed, \
             or unplugged?",
            shown(named)
        )),
        Found::Unseen(e)
        | Found::Unentered {
            why: Unentered::Refused(e),
            ..
        } => Err(format!(
            "Synod could not look inside {}: {e}.",
            shown(named)
        )),
        _ => Err(format!(
            "Synod could not look inside {}: it is not a folder.",
            shown(named)
        )),
    }
}

fn target_text(target: &Path, link: &WalkEntry) -> Result<String, String> {
    utf8(target).map(str::to_string).map_err(|_| {
        format!(
            "The link {} points at a name this computer cannot read; \
             please fix or remove it first.",
            shown(link.shown.as_path())
        )
    })
}

/// Ungoverned bookkeeping over the user's folder, through the held leaf.
fn link_target(at: &LexicalPath) -> io::Result<PathBuf> {
    site!(
        Silent::FolderManifest,
        Ungoverned::read(walk(at, LeafMode::NoFollow)?)
    )
    .read_link()
}

fn file_mode(stat: &Stat) -> u32 {
    #[cfg(unix)]
    return stat.mode & 0o7777;
    #[cfg(not(unix))]
    return {
        let _ = stat;
        0
    };
}

fn mtime_ns(stat: &Stat) -> u64 {
    stat.mtime
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Borrowed callback keeps `of_folder_via` non-generic; progress is the cumulative file count.
pub(crate) type Progress<'a> = &'a mut dyn FnMut(u64);

#[derive(Clone, Default)]
pub(crate) struct Stop(DurableRoot);

impl Stop {
    /// Stop is irreversible; an abandoned walk is never resumed.
    pub(crate) fn stop(&self) {
        self.0.cancel(CancelCause::Interrupted);
    }

    #[cfg(test)]
    pub(crate) fn is_stopped(&self) -> bool {
        self.0.is_cancelled()
    }

    fn scope(&self) -> Scope {
        Scope::clone(&self.0)
    }
}

/// Keep only the minimal set of unread subtree roots.
fn record_unread(unread: &mut Vec<String>, key: String) {
    if !unread.iter().any(|root| covers(&key, root)) {
        unread.push(key);
    }
}

pub(crate) fn merge_unread(a: &[String], b: &[String]) -> Vec<String> {
    let mut all: Vec<&String> = a.iter().chain(b).collect();
    all.sort();
    all.dedup();
    all.iter()
        .filter(|key| {
            !all.iter()
                .any(|other| *other != **key && covers(key, other))
        })
        .map(|key| (*key).clone())
        .collect()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::test_fixture::workshop;

    #[test]
    fn files_folders_and_empty_folders_are_all_recorded() {
        let dir = workshop("manifest-record");
        std::fs::write(dir.path().join("letter.txt"), b"dear all").expect("fixture");
        std::fs::create_dir(dir.path().join("sent")).expect("fixture");
        std::fs::write(dir.path().join("sent").join("a.txt"), b"gone").expect("fixture");
        std::fs::create_dir(dir.path().join("empty")).expect("fixture");

        let manifest = Manifest::of_folder(dir.path()).expect("an ordinary folder reads");
        match manifest.entries.get("letter.txt") {
            Some(EntryKind::File { size, .. }) => assert_eq!(*size, 8),
            other => panic!("expected a recorded file, got {other:?}"),
        }
        assert_eq!(manifest.entries.get("sent"), Some(&EntryKind::Folder));
        assert!(manifest.entries.contains_key("sent/a.txt"));
        assert_eq!(
            manifest.entries.get("empty"),
            Some(&EntryKind::Folder),
            "an empty folder is a fact about the folder"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_recorded_as_its_target_and_never_followed() {
        let dir = workshop("manifest-link");
        std::os::unix::fs::symlink("nowhere/at-all", dir.path().join("dangling")).expect("fixture");
        let manifest =
            Manifest::of_folder(dir.path()).expect("a dangling link must not break the walk");
        assert_eq!(
            manifest.entries.get("dangling"),
            Some(&EntryKind::Link {
                target: "nowhere/at-all".into(),
            })
        );
    }

    #[test]
    fn the_walk_reports_progress_after_every_file() {
        let dir = workshop("manifest-progress");
        std::fs::write(dir.path().join("a.txt"), b"dear all").expect("fixture");
        std::fs::write(dir.path().join("b.txt"), b"gone").expect("fixture");
        std::fs::create_dir(dir.path().join("empty")).expect("fixture");

        let mut seen = Vec::new();
        Manifest::of_folder_via(dir.path(), &Stop::default(), &mut |so_far| {
            seen.push(so_far);
        })
        .expect("an ordinary folder reads");

        assert_eq!(
            seen,
            vec![1, 2],
            "one progress call per file — never per folder, never per byte"
        );
    }

    #[test]
    fn a_walk_stopped_partway_errs_rather_than_answering_a_short_manifest() {
        let dir = workshop("manifest-stopped");
        std::fs::write(dir.path().join("a.txt"), b"dear all").expect("fixture");
        std::fs::write(dir.path().join("b.txt"), b"gone").expect("fixture");
        std::fs::write(dir.path().join("c.txt"), b"more").expect("fixture");

        let stop = Stop::default();
        let result = Manifest::of_folder_via(dir.path(), &stop, &mut |_so_far| stop.stop());

        assert!(
            result.is_err(),
            "a walk stopped after its first file must answer Err, never Ok(a short manifest)"
        );
    }

    #[test]
    fn a_root_that_cannot_be_entered_fails_the_manifest() {
        let dir = workshop("manifest-root");
        std::fs::write(dir.path().join("file"), b"x").expect("fixture");
        assert!(Manifest::of_folder(&dir.path().join("gone")).is_err());
        assert!(Manifest::of_folder(&dir.path().join("file")).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = dir.path().join("locked");
            std::fs::create_dir(&locked).expect("fixture");
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
                .expect("fixture");
            // Root can list a 0000 directory; this case binds only where permissions do.
            let refused = std::fs::read_dir(&locked).is_err();
            let result = Manifest::of_folder(&locked);
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))
                .expect("fixture");
            assert!(!refused || result.is_err());
        }
    }

    #[test]
    fn a_subfolder_that_vanished_is_unread() {
        let dir = workshop("manifest-vanished");
        std::fs::write(dir.path().join("a.txt"), b"dear all").expect("fixture");
        std::fs::create_dir(dir.path().join("sub")).expect("fixture");
        std::fs::write(dir.path().join("sub").join("x.txt"), b"gone").expect("fixture");

        let manifest = Manifest::of_folder_via(dir.path(), &Stop::default(), &mut |_so_far| {
            std::fs::remove_dir_all(dir.path().join("sub")).expect("fixture");
        })
        .expect("a folder that vanishes is not a failure");

        assert_eq!(manifest.unread, vec!["sub".to_string()]);
        assert!(manifest.entries.contains_key("a.txt"));
        assert!(!manifest.entries.contains_key("sub"));
    }

    #[cfg(unix)]
    #[test]
    fn a_subfolder_that_refuses_permission_fails_the_manifest() {
        use std::os::unix::fs::PermissionsExt;
        let dir = workshop("manifest-refused");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("fixture");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("fixture");
        // Root can list a 0000 directory; this case binds only where permissions do.
        let refused = std::fs::read_dir(&locked).is_err();
        let result = Manifest::of_folder(dir.path());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).expect("fixture");
        assert!(!refused || result.is_err());
    }

    #[test]
    fn record_unread_skips_a_key_already_covered_by_a_recorded_root() {
        let mut unread = vec!["target".to_string()];
        record_unread(&mut unread, "target/debug/build".to_string());
        assert_eq!(unread, vec!["target".to_string()]);

        record_unread(&mut unread, "vm-image".to_string());
        assert_eq!(unread, vec!["target".to_string(), "vm-image".to_string()]);
    }

    #[test]
    fn merge_unread_reduces_two_lists_to_their_minimal_covering_set() {
        let a = vec!["target".to_string(), "elsewhere.txt".to_string()];
        let b = vec!["target/debug".to_string(), "vm-image/out".to_string()];
        let mut merged = merge_unread(&a, &b);
        merged.sort();
        assert_eq!(
            merged,
            vec![
                "elsewhere.txt".to_string(),
                "target".to_string(),
                "vm-image/out".to_string(),
            ]
        );
    }
}
