//! Filesystem queries, temp-path minting, and the `exists` / `is-file` /
//! `is-dir` / `is-link` / `is-readable` / `is-writable` predicates.
//!
//! Every path routes through [`super::util::checked_read_path`] or
//! [`Shell::locate`]/[`Shell::locate_existing`], so a probe answers about
//! the `within [dir: …]` cwd, not the OS cwd — bare `Path::exists` would
//! miss a file a redirect just wrote there.  `absolute-path` is exempt:
//! lexical, with no filesystem to gate.

use crate::capability::FsOp;
use crate::path::walk::{DirEntry, FileInfo, Kind, Leaf, Stat};
use crate::types::{Break, Settled, Shell, Value, sig};
use std::path::Path;

use super::util::{admits_read, checked_read_path};

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
fn writable_temp_dir(shell: &mut Shell) -> Settled<crate::path::Located> {
    let rp = shell.resolve(&std::env::temp_dir().to_string_lossy());
    shell.locate(&rp, &crate::capability::FsOp::Write)
}

pub(super) fn builtin_temp_dir(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let parent = writable_temp_dir(shell)?;
    let path = tempfile::Builder::new()
        .prefix("ral-tmp-")
        .tempdir_in(parent.real())
        .map_err(|e| sig(format!("temp-dir: {e}")))?
        .keep();
    Ok(Value::string(path.to_string_lossy()))
}

pub(super) fn builtin_temp_file(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let parent = writable_temp_dir(shell)?;
    let named = tempfile::Builder::new()
        .prefix("ral-tmp-")
        .tempfile_in(parent.real())
        .map_err(|e| sig(format!("temp-file: {e}")))?;
    let (_file, path) = named.keep().map_err(|e| sig(format!("temp-file: {e}")))?;
    Ok(Value::string(path.to_string_lossy()))
}

/// Glob, preserving the pattern's shape: a cwd-relative pattern yields
/// cwd-relative matches, a sigil-rooted or absolute one absolute matches.
pub(super) fn builtin_glob(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let raw = args[0].as_str("glob")?;
    let home = shell.context.home();
    let expanded = crate::path::sigil::expand_path_prefix(raw, home.as_deref());
    let input_is_cwd_relative = !crate::path::is_absolute(&expanded);
    let pattern = checked_read_path(shell, raw)?
        .as_path()
        .to_string_lossy()
        .into_owned();
    let strip_prefix = input_is_cwd_relative.then(|| shell.cwd());

    // Hide dotfiles as Unix shells do, but stricter than `dotglob=off`:
    // the crate filters at walk time, so even `.h*` matches nothing.
    // Fully literal dotfile names still work.
    let options = glob::MatchOptions {
        require_literal_leading_dot: true,
        ..glob::MatchOptions::new()
    };
    let mut results = Vec::new();
    match glob::glob_with(&pattern, options) {
        Ok(paths) => {
            for entry in paths {
                let path = entry.map_err(|e| sig(format!("glob: {e}")))?;
                // As in `list-dir`: the *pattern* was admitted, the
                // concrete hits under it were not.
                if !admits_read(shell, &path.to_string_lossy()) {
                    continue;
                }
                let rendered = match &strip_prefix {
                    Some(cwd) => path
                        .strip_prefix(cwd)
                        .map(Path::to_path_buf)
                        .unwrap_or(path),
                    None => path,
                };
                results.push(rendered.to_string_lossy().into_owned());
            }
        }
        Err(e) => return Err(sig(format!("glob: {e}"))),
    }
    results.sort();
    Ok(Value::list(
        results.into_iter().map(Value::string).collect(),
    ))
}

/// Label an `io::Error` with the operation and the path that provoked it.
fn io_err(ctx: &str, path: &Path, e: &std::io::Error) -> Break {
    sig(format!("{ctx}: {}: {e}", path.display()))
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
        .map(|p| p.to_string_lossy().into_owned());
    let name = path.file_name().unwrap_or(path.as_os_str());
    Ok(Value::from_datum(FileInfo::of(name, &stat, target)))
}

pub(super) fn builtin_resolve_path(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let s = args[0].as_str("resolve-path")?;
    let resolved = checked_read_path(shell, s)?
        .canonicalise_strict()
        .map_err(|e| sig(format!("resolve-path: {s}: {e}")))?;
    Ok(Value::string(resolved.to_string_lossy()))
}

/// Lexical sibling of `resolve-path`: same anchoring, no
/// `canonicalise_strict`, so symlinks stand and the path need not exist —
/// and no `check_fs_read`, since that guard covers a stat this never does.
pub(super) fn builtin_absolute_path(args: &[Value], shell: &Shell) -> Settled<Value> {
    let resolved = shell.resolve(args[0].as_str("absolute-path")?);
    Ok(Value::string(resolved.as_path().to_string_lossy()))
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
