//! Filesystem queries, temp-path minting, and the `exists` / `is-file` /
//! `is-dir` / `is-link` / `is-readable` / `is-writable` predicates.
//!
//! Every path routes through [`Shell::locate`], [`Shell::locate_dir`] or
//! [`Shell::locate_existing`], so a probe answers about the `within [dir: …]`
//! cwd, not the OS cwd — bare `Path::exists` would miss a file a redirect just
//! wrote there.  `absolute-path` is exempt: lexical, with no filesystem to
//! gate.

use crate::path::shown;
use crate::path::tree::glob::Glob;
use crate::path::walk::{Kind, Stat};
use crate::path::{Dir, EntryName, Judged, LeafMode, Located, Reading, Visibility, Writing};
use crate::record;
use crate::types::{Break, Settled, Shell, Value, sig};
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

/// Seconds since the epoch, or 0 when the field is unrecorded or pre-epoch.
fn secs_since_epoch(t: Option<SystemTime>) -> i64 {
    t.map_or(0, crate::fact::unix_secs)
}

/// What `list-dir` reports of one entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirEntry {
    name: String,
    kind: Kind,
    size: u64,
    mtime: i64,
}

record!(typed DirEntry {
    name: "name",
    kind: "type",
    size: "size",
    mtime: "mtime",
});

impl DirEntry {
    pub(crate) fn of(name: &EntryName, stat: &Stat) -> Self {
        let Stat {
            kind, len, mtime, ..
        } = *stat;
        Self {
            name: shown(name.as_os_str()),
            kind,
            size: len,
            mtime: secs_since_epoch(mtime),
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

/// What `file-info` reports of one path: a [`DirEntry`]'s fields, access and
/// birth times, the readonly bit, and the symlink `target` when it is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileInfo {
    name: String,
    kind: Kind,
    size: u64,
    mtime: i64,
    atime: i64,
    btime: i64,
    readonly: bool,
    target: Option<String>,
}

record!(typed FileInfo {
    name: "name",
    kind: "type",
    size: "size",
    mtime: "mtime",
    atime: "atime",
    btime: "btime",
    readonly: "readonly",
    target: "target",
});

impl FileInfo {
    pub(crate) fn of(name: &OsStr, stat: &Stat, target: Option<String>) -> Self {
        Self {
            name: shown(name),
            kind: stat.kind,
            size: stat.len,
            mtime: secs_since_epoch(stat.mtime),
            atime: secs_since_epoch(stat.atime),
            btime: secs_since_epoch(stat.btime),
            readonly: stat.readonly,
            target,
        }
    }
}

pub(super) fn builtin_list_dir(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let rp = shell.resolve(args[0].as_str("list-dir")?);
    let dir = shell.locate_dir::<Reading>(&rp)?;
    let region = shell.context.grants.read_region(shell.floor());
    let mut entries: Vec<DirEntry> = Vec::new();
    for entry in dir
        .read_dir()
        .map_err(|e| io_err("list-dir", dir.real().as_path(), &e))?
    {
        // `locate_dir` admitted the directory; each entry is a distinct
        // position the region judges, by what the listing saw of it.  Drop a
        // denied entry rather than abort, as `grep-files` and `explore-dir`
        // do, and one the listing could not state: a listing is not a manifest.
        let admitted = entry.listed().map_or_else(
            || region.admits_unseen(&entry),
            |listed| region.admits_listed(&listed),
        );
        if let (true, Ok(seen)) = (admitted, entry.seen()) {
            entries.push(DirEntry::of(entry.name(), &seen.stat));
        }
    }
    entries.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(Value::list(
        entries.into_iter().map(Value::from_datum).collect(),
    ))
}

/// The system temp directory, located and authorised for writing: what a
/// fresh temp entry is created inside.
///
/// Refused, before anything is created, unless its real path is UTF-8: the
/// entry is kept and handed back as a string, which a name no string holds
/// would leave naming some other path, and the entry itself unreachable.
fn writable_temp_dir(ctx: &str, shell: &mut Shell) -> Settled<Dir<Judged<Writing>>> {
    let rp = shell.anchor(&std::env::temp_dir());
    let dir = shell.locate_dir::<Writing>(&rp)?;
    crate::path::utf8(dir.real().as_path()).map_err(|e| {
        sig(format!(
            "{ctx}: the temp directory {e}, so no path made in it can be returned as text; \
             point TMPDIR (TMP on Windows) at one that is"
        ))
    })?;
    Ok(dir)
}

/// A fresh entry of `parent`, made by `make`, as the text it is returned as.
/// Each name is invented, then judged as the absent leaf it is, and tried
/// again under another where it is taken: each retry judged anew, since each
/// is a different position.
fn made_in(
    ctx: &str,
    shell: &mut Shell,
    parent: &Dir<Judged<Writing>>,
    make: impl Fn(Located<Judged<Writing>>) -> io::Result<String>,
) -> Settled<Value> {
    let failed = |e: &dyn std::fmt::Display| sig(format!("{ctx}: {e}"));
    loop {
        let name = EntryName::fresh("ral-tmp-").map_err(|e| failed(&e))?;
        let target = parent.absent(name).map_err(|e| failed(&e))?;
        let target = shell.judge::<Writing>(target)?;
        match make(target) {
            Ok(made) => return Ok(Value::string(made)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(failed(&e)),
        }
    }
}

pub(super) fn builtin_temp_dir(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let parent = writable_temp_dir("temp-dir", shell)?;
    temp_dir_in(shell, &parent)
}

pub(super) fn builtin_temp_file(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let parent = writable_temp_dir("temp-file", shell)?;
    temp_file_in(shell, &parent)
}

fn temp_dir_in(shell: &mut Shell, parent: &Dir<Judged<Writing>>) -> Settled<Value> {
    made_in("temp-dir", shell, parent, |target| {
        target
            .create_dir(Visibility::Private)
            .map(|dir| shown(dir.real().as_path()))
    })
}

fn temp_file_in(shell: &mut Shell, parent: &Dir<Judged<Writing>>) -> Settled<Value> {
    made_in("temp-file", shell, parent, |target| {
        let made = target.create_file(Visibility::Private)?;
        Ok(shown(made.real().as_path()))
    })
}

/// Glob, preserving the pattern's shape: a cwd-relative pattern yields
/// cwd-relative matches, a sigil-rooted or absolute one absolute matches.
pub(super) fn builtin_glob(args: &[Value], shell: &Shell) -> Settled<Value> {
    let raw = args[0].as_str("glob")?;
    let pattern = shell.resolve(raw);
    let glob = Arc::new(Glob::parse(&pattern).map_err(|e| sig(format!("glob: {e}")))?);
    let mut walk = glob.walk().admitted(shell, "glob", raw);
    let mut hits = Vec::new();
    while let Some(entry) = walk.next()? {
        if glob.matches(&entry.shown) {
            hits.push(walk.name(&entry)?);
        }
    }
    hits.sort();
    Ok(Value::list(hits.into_iter().map(Value::string).collect()))
}

/// Label an `io::Error` with the operation and the path that provoked it.
fn io_err(ctx: &str, path: &Path, e: &std::io::Error) -> Break {
    sig(format!("{ctx}: {}: {e}", shown(path)))
}

/// Portable per-path metadata: the `stat` fields `std::fs::Metadata` carries
/// everywhere — mode bits, uid/gid, nlink and inode would want a `cfg(unix)`
/// companion.  Stats the link itself, not its target; compose with
/// `resolve-path` for the latter.
pub(super) fn builtin_file_info(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let raw = args[0].as_str("file-info")?;
    let rp = shell.resolve(raw);
    let path = rp.as_path().to_path_buf();
    let missing = || sig(format!("file-info: {raw}: no such file or directory"));
    let located = shell
        .locate_existing::<Reading>(&rp, LeafMode::NoFollow)?
        .ok_or_else(missing)?;
    let stat = located.stat().ok_or_else(missing)?;
    let target = (stat.kind == Kind::Symlink)
        .then(|| located.read_link().ok())
        .flatten()
        .map(|p| shown(&p));
    let name = path.file_name().unwrap_or(path.as_os_str());
    Ok(Value::from_datum(FileInfo::of(name, stat, target)))
}

pub(super) fn builtin_resolve_path(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let s = args[0].as_str("resolve-path")?;
    let rp = shell.resolve(s);
    let located = shell
        .locate_existing::<Reading>(&rp, LeafMode::Follow)?
        .filter(|located| located.stat().is_some())
        .ok_or_else(|| sig(format!("resolve-path: {s}: no such file or directory")))?;
    Ok(Value::string(shown(located.real().as_path())))
}

/// Lexical sibling of `resolve-path`: same anchoring, then the lexical fold
/// where `resolve-path` reads the disk, so symlinks stand and the path need
/// not exist — and no grant is asked, since that guard covers a stat this
/// never does.
pub(super) fn builtin_absolute_path(args: &[Value], shell: &Shell) -> Settled<Value> {
    let resolved = shell.resolve(args[0].as_str("absolute-path")?).folded();
    Ok(Value::string(shown(resolved.as_path())))
}

pub(super) fn builtin_abbreviate_home(args: &[Value], shell: &Shell) -> Settled<Value> {
    Ok(Value::string(crate::path::abbreviate_home(
        args[0].as_str("abbreviate-home")?,
        shell.context.home().as_deref(),
    )))
}

/// Shared predicate body.  `leaf` is the whole difference between the two
/// families: `Follow` gives `test -f`/`test -d`/`test -r` semantics, where a
/// link to a file is `is-file` and a dangling link fails every probe;
/// `NoFollow` gives `exists`/`is-link`, where a dangling link still exists and
/// a link is a link.  `probe` sees `None` when the path is missing or under a
/// directory that does not exist, so every predicate answers `false` there
/// rather than raising.
fn fs_probe(
    name: &str,
    args: &[Value],
    shell: &mut Shell,
    leaf: LeafMode,
    probe: impl FnOnce(Option<Stat>) -> bool,
) -> Settled<Value> {
    let rp = shell.resolve(args[0].as_str(name)?);
    let stat = shell
        .locate_existing::<Reading>(&rp, leaf)?
        .and_then(|located| located.stat().cloned());
    Ok(Value::Bool(probe(stat)))
}

pub(super) fn builtin_exists(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe("exists", args, shell, LeafMode::NoFollow, |s| s.is_some())
}

pub(super) fn builtin_is_file(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe("is-file", args, shell, LeafMode::Follow, |s| {
        s.is_some_and(|s| s.kind == Kind::File)
    })
}

pub(super) fn builtin_is_dir(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe("is-dir", args, shell, LeafMode::Follow, |s| {
        s.is_some_and(|s| s.kind == Kind::Dir)
    })
}

pub(super) fn builtin_is_link(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe("is-link", args, shell, LeafMode::NoFollow, |s| {
        s.is_some_and(|s| s.kind == Kind::Symlink)
    })
}

pub(super) fn builtin_is_readable(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    // Exact on Windows, where `readonly` governs writes alone; on Unix an
    // approximation of `test -r`, the truth needing uid/gid/acl logic.
    fs_probe("is-readable", args, shell, LeafMode::Follow, |s| {
        s.is_some()
    })
}

pub(super) fn builtin_is_writable(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let rp = shell.resolve(args[0].as_str("is-writable")?);
    let writable = shell
        .locate_existing::<Reading>(&rp, LeafMode::Follow)?
        .is_some_and(|located| located.is_writable());
    Ok(Value::Bool(writable))
}

#[cfg(all(test, unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures build the tree the temp entries are made in"
)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// The directory is held when it is judged, and what is made is made in it
    /// by that handle: renamed away since, with a link put at its name, the
    /// entries still land in the directory, not where its name leads now.
    #[test]
    fn temp_entries_are_made_in_the_directory_held_not_where_its_name_leads_now() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(root.join("held")).unwrap();
        let mut shell = crate::test_helper::core_shell();
        let held = shell.anchor(&root.join("held"));
        let Ok(parent) = shell.locate_dir::<Writing>(&held) else {
            panic!("a fresh directory is writable");
        };
        std::fs::rename(root.join("held"), root.join("moved")).unwrap();
        std::fs::create_dir(root.join("elsewhere")).unwrap();
        symlink(root.join("elsewhere"), root.join("held")).unwrap();

        assert!(temp_dir_in(&mut shell, &parent).is_ok());
        assert!(temp_file_in(&mut shell, &parent).is_ok());

        let names = |dir: &str| -> Vec<String> {
            std::fs::read_dir(root.join(dir))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect()
        };
        let moved = names("moved");
        assert_eq!(moved.len(), 2, "{moved:?}");
        assert!(moved.iter().all(|name| name.starts_with("ral-tmp-")));
        assert_eq!(names("elsewhere"), Vec::<String>::new());
    }

    /// A pattern is matched by the names the walk reached entries by, so a
    /// link in the root leads elsewhere and the matches are still named
    /// through it; a directory link beneath a wildcard is matched by its name.
    #[test]
    fn a_glob_matches_by_traversal_name_through_links() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::create_dir_all(root.join("repo")).unwrap();
        std::fs::write(root.join("elsewhere/a.txt"), "").unwrap();
        symlink(root.join("elsewhere"), root.join("repo/link")).unwrap();
        let shell = crate::test_helper::core_shell();
        let glob = |pattern: String| -> Vec<String> {
            let found = builtin_glob(&[Value::string(pattern)], &shell).unwrap();
            let Value::List(items) = found else {
                panic!("a glob is a list");
            };
            items
                .iter()
                .map(|v| v.as_str("test").unwrap().to_string())
                .collect()
        };
        let repo = shown(root.join("repo"));
        assert_eq!(
            glob(format!("{repo}/link/*.txt")),
            [format!("{repo}/link/a.txt")]
        );
        assert_eq!(glob(format!("{repo}/*")), [format!("{repo}/link")]);
    }

    /// A name ral invents is made private, whatever the umask: as `mktemp(1)`
    /// makes it.
    #[test]
    fn temp_entries_are_private_under_a_lax_umask() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let mut shell = crate::test_helper::core_shell();
        let Ok(parent) = shell.locate_dir::<Writing>(&shell.anchor(tmp.path())) else {
            panic!("a fresh directory is writable");
        };
        // SAFETY: `umask` only swaps the process mask, restored below.
        let before = unsafe { libc::umask(0o022) };
        let dir = temp_dir_in(&mut shell, &parent);
        let file = temp_file_in(&mut shell, &parent);
        unsafe { libc::umask(before) };

        let mode = |made: Settled<Value>| {
            let made = made.unwrap();
            let meta = std::fs::metadata(made.as_str("test").unwrap()).unwrap();
            meta.permissions().mode() & 0o777
        };
        assert_eq!(mode(dir), 0o700);
        assert_eq!(mode(file), 0o600);
    }
}
