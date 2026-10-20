//! Filesystem queries, temp-path minting, and the `exists` / `is-file` /
//! `is-dir` / `is-link` / `is-readable` / `is-writable` predicates.
//!
//! Every path routes through [`super::util::checked_read_path`] or
//! [`Shell::locate`]/[`Shell::locate_existing`], so a probe answers about
//! the `within [dir: …]` cwd, not the OS cwd — bare `Path::exists` would
//! miss a file a redirect just wrote there.  `absolute-path` is exempt:
//! lexical, with no filesystem to gate.

use crate::capability::FsOp;
use crate::path::shown;
use crate::path::tree::glob::Glob;
use crate::path::walk::{DirEntry, FileInfo, Kind, Leaf, Stat};
use crate::types::{Break, Settled, Shell, Value, sig};
use std::path::Path;
use std::sync::Arc;

use super::util::checked_read_path;

pub(super) fn builtin_list_dir(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let rp = shell.resolve(args[0].as_str("list-dir")?);
    let located = shell.locate(&rp, &FsOp::Read)?;
    let dir = located.real();
    let mut entries: Vec<DirEntry> = Vec::new();
    for entry in located
        .read_dir()
        .map_err(|e| io_err("list-dir", dir, &e))?
    {
        // `locate` admitted the directory; each entry is a distinct path
        // whose metadata this returns.  Drop a denied entry rather than
        // abort, as `grep-files` and `explore-dir` do.
        if !shell.admits_fs_exact(&FsOp::Read, &dir.join(&entry.name)) {
            continue;
        }
        entries.push(DirEntry::of(&entry));
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
fn writable_temp_dir(ctx: &str, shell: &mut Shell) -> Settled<crate::path::Located> {
    let rp = shell.anchor(&std::env::temp_dir());
    let located = shell.locate(&rp, &crate::capability::FsOp::Write)?;
    crate::path::utf8(located.real()).map_err(|e| {
        sig(format!(
            "{ctx}: the temp directory {e}, so no path made in it can be returned as text; \
             point TMPDIR (TMP on Windows) at one that is"
        ))
    })?;
    Ok(located)
}

pub(super) fn builtin_temp_dir(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let parent = writable_temp_dir("temp-dir", shell)?;
    let path = tempfile::Builder::new()
        .prefix("ral-tmp-")
        .tempdir_in(parent.real())
        .map_err(|e| sig(format!("temp-dir: {e}")))?
        .keep();
    Ok(Value::string(shown(&path)))
}

pub(super) fn builtin_temp_file(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let parent = writable_temp_dir("temp-file", shell)?;
    let named = tempfile::Builder::new()
        .prefix("ral-tmp-")
        .tempfile_in(parent.real())
        .map_err(|e| sig(format!("temp-file: {e}")))?;
    let (_file, path) = named.keep().map_err(|e| sig(format!("temp-file: {e}")))?;
    Ok(Value::string(shown(&path)))
}

/// Glob, preserving the pattern's shape: a cwd-relative pattern yields
/// cwd-relative matches, a sigil-rooted or absolute one absolute matches.
pub(super) fn builtin_glob(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let raw = args[0].as_str("glob")?;
    let pattern = checked_read_path(shell, raw)?;
    let glob = Arc::new(Glob::parse(pattern.as_path()).map_err(|e| sig(format!("glob: {e}")))?);
    let mut walk = glob.walk().admitted(shell, "glob", raw);
    let mut hits = Vec::new();
    while let Some(entry) = walk.next(shell)? {
        if glob.matches(entry.path.as_path()) {
            hits.push(entry.name()?);
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
        .locate_existing(&rp, &FsOp::Read, Leaf::AsNamed)?
        .ok_or_else(missing)?;
    let stat = located
        .stat()
        .map_err(|e| io_err("file-info", &path, &e))?
        .ok_or_else(missing)?;
    let target = (stat.kind == Kind::Symlink)
        .then(|| located.read_link().ok())
        .flatten()
        .map(|p| shown(&p));
    let name = path.file_name().unwrap_or(path.as_os_str());
    Ok(Value::from_datum(FileInfo::of(name, &stat, target)))
}

pub(super) fn builtin_resolve_path(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let s = args[0].as_str("resolve-path")?;
    let resolved = checked_read_path(shell, s)?
        .canonicalise_strict()
        .map_err(|e| sig(format!("resolve-path: {s}: {e}")))?;
    Ok(Value::string(shown(&resolved)))
}

/// Lexical sibling of `resolve-path`: same anchoring, no
/// `canonicalise_strict`, so symlinks stand and the path need not exist —
/// and no `check_fs_read`, since that guard covers a stat this never does.
pub(super) fn builtin_absolute_path(args: &[Value], shell: &Shell) -> Settled<Value> {
    let resolved = shell.resolve(args[0].as_str("absolute-path")?);
    Ok(Value::string(shown(resolved.as_path())))
}

pub(super) fn builtin_abbreviate_home(args: &[Value], shell: &Shell) -> Settled<Value> {
    Ok(Value::string(crate::path::abbreviate_home(
        args[0].as_str("abbreviate-home")?,
        shell.context.home().as_deref(),
    )))
}

/// Shared predicate body.  `leaf` is the whole difference between the two
/// families: `Resolve` gives `test -f`/`test -d`/`test -r` semantics, where a
/// link to a file is `is-file` and a dangling link fails every probe;
/// `AsNamed` gives `exists`/`is-link`, where a dangling link still exists and
/// a link is a link.  `probe` sees `None` when the path is missing or under a
/// directory that does not exist, so every predicate answers `false` there
/// rather than raising.
fn fs_probe(
    name: &str,
    args: &[Value],
    shell: &mut Shell,
    leaf: Leaf,
    probe: impl FnOnce(Option<Stat>) -> bool,
) -> Settled<Value> {
    let rp = shell.resolve(args[0].as_str(name)?);
    let stat = shell
        .locate_existing(&rp, &FsOp::Read, leaf)?
        .and_then(|located| located.stat().ok().flatten());
    Ok(Value::Bool(probe(stat)))
}

pub(super) fn builtin_exists(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe("exists", args, shell, Leaf::AsNamed, |s| s.is_some())
}

pub(super) fn builtin_is_file(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe("is-file", args, shell, Leaf::Resolve, |s| {
        s.is_some_and(|s| s.kind == Kind::File)
    })
}

pub(super) fn builtin_is_dir(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe("is-dir", args, shell, Leaf::Resolve, |s| {
        s.is_some_and(|s| s.kind == Kind::Dir)
    })
}

pub(super) fn builtin_is_link(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe("is-link", args, shell, Leaf::AsNamed, |s| {
        s.is_some_and(|s| s.kind == Kind::Symlink)
    })
}

pub(super) fn builtin_is_readable(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    // Exact on Windows, where `readonly` governs writes alone; on Unix an
    // approximation of `test -r`, the truth needing uid/gid/acl logic.
    fs_probe("is-readable", args, shell, Leaf::Resolve, |s| s.is_some())
}

pub(super) fn builtin_is_writable(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let rp = shell.resolve(args[0].as_str("is-writable")?);
    let writable = shell
        .locate_existing(&rp, &FsOp::Read, Leaf::Resolve)?
        .is_some_and(|located| located.is_writable());
    Ok(Value::Bool(writable))
}
