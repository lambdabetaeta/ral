//! Readings of the engine's own filesystem: read-only metadata walks a host
//! asks for at a run boundary — operator diagnostics and completion, not
//! model I/O.  A listing shown to the user is a read, so each directory is
//! judged for reading under the live grant.

use crate::capability::ReadRegion;
use crate::path::walk::Kind;
use crate::path::{Dir, Judged, LeafMode, Reading, walk_dir};
use crate::protocol::probe::PathEntry;
use crate::types::Shell;
use std::path::Path;

/// `dir` held and judged for reading, with the region that judges what is
/// beneath it; `None` where it cannot be entered or read.
fn open(shell: &Shell, dir: &Path) -> Option<(ReadRegion, Dir<Judged<Reading>>)> {
    let entered = walk_dir(&shell.anchor(dir)).ok()?;
    let judged = (shell.context.grants)
        .judge_dir::<Reading>(shell.floor(), entered)
        .ok()?;
    Some((shell.context.grants.read_region(shell.floor()), judged))
}

/// `dir`'s entries with a UTF-8 name; none, if it cannot be read. `exec`
/// follows a symlink, as `PATH` lookup does.
pub(super) fn entries(shell: &Shell, dir: &Path) -> Vec<PathEntry> {
    let Some((region, dir)) = open(shell, dir) else {
        return Vec::new();
    };
    let Ok(listing) = dir.read_dir() else {
        return Vec::new();
    };
    listing
        .filter_map(|entry| {
            let seen = entry.seen().ok()?;
            // Shown is read: an entry the region withholds is not listed.
            if !region.admits(&entry) {
                return None;
            }
            let exec = (dir.locate(entry.name(), LeafMode::Follow).ok())
                .and_then(|found| region.judge_file(found).ok())
                .is_some_and(|found| found.is_executable());
            Some(PathEntry {
                dir: seen.stat.kind == Kind::Dir,
                exec,
                name: entry.name().as_os_str().to_owned().into_string().ok()?,
            })
        })
        .collect()
}

/// Total bytes of every regular file under `root`, recursively, each
/// directory judged for reading; zero for what cannot be read.  Symlinks are
/// not followed (their target may leave the tree) and an unreadable entry
/// counts zero rather than failing the fold.
pub(super) fn bytes(shell: &Shell, root: &Path) -> u64 {
    fn under(region: &ReadRegion, dir: &Dir<Judged<Reading>>) -> u64 {
        let Ok(listing) = dir.read_dir() else {
            return 0;
        };
        listing
            .map(|entry| match entry.seen().map(|seen| &seen.stat) {
                Ok(stat) if stat.kind == Kind::File && region.admits(&entry) => stat.len,
                Ok(stat) if stat.kind == Kind::Dir => dir
                    .open_dir(entry.name())
                    .ok()
                    .and_then(|child| region.judge(child).ok())
                    .map_or(0, |child| under(region, &child)),
                _ => 0,
            })
            .sum()
    }
    open(shell, root).map_or(0, |(region, dir)| under(&region, &dir))
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures build a real tree to list and count"
)]
mod tests {
    use super::*;
    use crate::capability::{Capabilities, FsPolicy};
    use crate::path::FrozenPath;
    use crate::test_helper::{core_shell, layer};

    fn names(shell: &Shell, dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = entries(shell, dir).into_iter().map(|e| e.name).collect();
        names.sort();
        names
    }

    #[test]
    fn a_withheld_entry_is_neither_listed_nor_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let t = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(t.join("sub")).unwrap();
        for (file, len) in [
            ("ok", 3),
            ("secret", 5),
            ("sub/deep", 7),
            ("sub/hidden", 11),
        ] {
            std::fs::write(t.join(file), vec![b'x'; len]).unwrap();
        }
        let spelled = |p: &Path| FrozenPath::from_surface(crate::path::utf8(p).unwrap());
        let layer = layer(Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![spelled(&t)],
                deny_paths: vec![spelled(&t.join("secret")), spelled(&t.join("sub/hidden"))],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        });
        let mut shell = core_shell();
        shell.with_layer(layer, |sh| {
            assert_eq!(names(sh, &t), ["ok", "sub"]);
            assert_eq!(bytes(sh, &t), 3 + 7);
        });
        assert_eq!(names(&shell, &t), ["ok", "secret", "sub"]);
        assert_eq!(bytes(&shell, &t), 26);
    }
}
