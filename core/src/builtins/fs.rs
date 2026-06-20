//! Filesystem query and temporary-path builtins.

use crate::types::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::util::{arg0_str, check_arity};

/// Classify a `FileType` against the four labels exposed in ral —
/// `file`, `dir`, `symlink`, or `other`.  Symlinks are detected first
/// so callers using `symlink_metadata` see the link itself rather than
/// the target's classification.
fn classify(ft: fs::FileType) -> &'static str {
    if ft.is_symlink() {
        "symlink"
    } else if ft.is_dir() {
        "dir"
    } else if ft.is_file() {
        "file"
    } else {
        "other"
    }
}

/// Convert an `io::Result<SystemTime>` to seconds-since-epoch.
/// Returns 0 when the metadata field is unavailable on this filesystem
/// (e.g. older Linux kernels lack birthtime; mounts with `noatime`
/// still report a value, but pre-epoch times collapse to 0).
fn secs_since_epoch(t: std::io::Result<SystemTime>) -> i64 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:list-dir] `list-dir` builtin: reads a directory's entries as a stat/listing predicate, gated by `check_fs_read`; not turn-time model data I/O, raises no surface card."
)]
pub(super) fn builtin_list_dir(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "list-dir")?;
    let path = checked_read_path(shell, &args[0].to_string())?;
    let mut entries: Vec<(String, Value)> = Vec::new();
    for entry in fs::read_dir(&path).map_err(|e| io_err("list-dir", &path, e))? {
        let entry = entry.map_err(|e| io_err("list-dir", &path, e))?;
        // `checked_read_path` admitted the directory, but each entry is
        // a distinct path whose metadata (size/mtime/type) this stats and
        // returns, so a deny on a subpath must drop that entry.  Skip a
        // denied entry rather than abort the listing — the same policy
        // `_search-files` / `explore-dir` apply to their walked entries.
        let rp = shell.resolve(&entry.path().to_string_lossy());
        if shell.check_fs_read(&rp).is_err() {
            continue;
        }
        entries.push(dir_entry_value(entry)?);
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Value::list(entries.into_iter().map(|(_, v)| v).collect()))
}

pub(super) fn builtin_temp_dir(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let parent = std::env::temp_dir();
    let rp = shell.resolve(&parent.to_string_lossy());
    shell.check_fs_write(&rp)?;
    let path = tempfile::Builder::new()
        .prefix("ral-tmp-")
        .tempdir_in(&parent)
        .map_err(|e| sig(format!("temp-dir: {e}")))?
        .keep();
    Ok(Value::String(path.to_string_lossy().into_owned()))
}

pub(super) fn builtin_temp_file(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let parent = std::env::temp_dir();
    let rp = shell.resolve(&parent.to_string_lossy());
    shell.check_fs_write(&rp)?;
    let named = tempfile::Builder::new()
        .prefix("ral-tmp-")
        .tempfile_in(&parent)
        .map_err(|e| sig(format!("temp-file: {e}")))?;
    let (_file, path) = named.keep().map_err(|e| sig(format!("temp-file: {e}")))?;
    Ok(Value::String(path.to_string_lossy().into_owned()))
}

/// Glob, preserving the input pattern's shape: a cwd-relative
/// pattern returns cwd-relative matches, sigil-rooted or absolute
/// patterns return absolute matches.  Internally we still resolve
/// against ral's logical cwd, then strip that prefix back off when
/// the input did not name it.
pub(super) fn builtin_glob(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let raw = arg0_str(args, "glob")?;
    let expanded = crate::path::sigil::expand_path_prefix(&raw, &shell.mobile.context.home());
    let input_is_cwd_relative = !crate::path::is_absolute(&expanded);
    let pattern = checked_read_path(shell, &raw)?
        .to_string_lossy()
        .into_owned();
    let strip_prefix = input_is_cwd_relative.then(|| shell.cwd());

    // Skip dotfiles in wildcard matches and dot-directories under
    // `**`, matching the dominant Unix shell default.  The glob
    // crate's `require_literal_leading_dot` is stricter than bash's
    // `dotglob=off`: it filters every dotfile at directory-walk time,
    // so wildcards inside a dotfile name (`.h*`, `.*.txt`) never
    // match.  Fully-literal dotfile names still work; callers
    // wanting richer dotfile patterns should use `list-dir | filter`.
    let options = glob::MatchOptions {
        require_literal_leading_dot: true,
        ..glob::MatchOptions::new()
    };
    let mut results = Vec::new();
    match glob::glob_with(&pattern, options) {
        Ok(paths) => {
            for entry in paths {
                let path = entry.map_err(|e| sig(format!("glob: {e}")))?;
                // Gate every match: `checked_read_path` admitted the
                // *pattern*, but the walk visits and returns concrete
                // paths under it, so a deny on a subpath must drop the
                // matching hits.  Skip a denied match rather than abort
                // the whole glob — the same policy `_search-files` /
                // `explore-dir` apply to their walked entries.
                let rp = shell.resolve(&path.to_string_lossy());
                if shell.check_fs_read(&rp).is_err() {
                    continue;
                }
                let rendered = match &strip_prefix {
                    Some(cwd) => path
                        .strip_prefix(cwd)
                        .map(Path::to_path_buf)
                        .unwrap_or(path),
                    None => path,
                };
                results.push(Value::String(rendered.to_string_lossy().into_owned()));
            }
        }
        Err(e) => return Err(sig(format!("glob: {e}"))),
    }
    results.sort_by_key(|a| a.to_string());
    Ok(Value::list(results))
}

fn checked_read_path(shell: &mut Shell, path: &str) -> Settled<PathBuf> {
    let rp = shell.resolve(path);
    shell.check_fs_read(&rp)?;
    Ok(rp.into_inner())
}

/// Wrap a `std::io::Error` with the operation label and the path that
/// triggered it.  Stand-in for `fs-err`: every fs call here knows the
/// path it was acting on, so we attach it explicitly rather than relying
/// on a wrapper type.
fn io_err(ctx: &str, path: &Path, e: std::io::Error) -> Break {
    sig(format!("{ctx}: {}: {e}", path.display()))
}

/// One `list-dir` entry as `(sort_key, value_for_caller)`.  The sort
/// key is the filename; the value is the public map shape ral exposes.
fn dir_entry_value(entry: fs::DirEntry) -> Settled<(String, Value)> {
    let name = entry.file_name().to_string_lossy().into_owned();
    let path = entry.path();
    let file_type = entry
        .file_type()
        .map_err(|e| io_err("list-dir", &path, e))?;
    let meta = entry.metadata().map_err(|e| io_err("list-dir", &path, e))?;
    let v = Value::map(vec![
        ("name".into(), Value::String(name.clone())),
        ("type".into(), Value::String(classify(file_type).into())),
        ("size".into(), Value::Int(meta.len() as i64)),
        (
            "mtime".into(),
            Value::Int(secs_since_epoch(meta.modified())),
        ),
    ]);
    Ok((name, v))
}

/// Portable per-path metadata.  Mirrors GNU `stat`'s common fields,
/// restricted to those Rust's `std::fs::Metadata` exposes on every
/// platform (no mode bits, uid/gid, nlink, inode — those need a
/// `cfg(unix)` companion).  Uses `symlink_metadata` so a symlink
/// reports `type: "symlink"` and a non-empty `target`; follow with
/// `resolve-path` if the caller wants the target's stat instead.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:file-info] `file-info` builtin: `symlink_metadata`/`read_link` stat a path and read a symlink target as a metadata predicate, gated by `check_fs_read`; not turn-time model data I/O, raises no surface card."
)]
pub(super) fn builtin_file_info(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "file-info")?;
    let path_arg = args[0].to_string();
    let path = checked_read_path(shell, &path_arg)?;
    let meta = fs::symlink_metadata(&path).map_err(|e| io_err("file-info", &path, e))?;
    let ft = meta.file_type();
    let target = if ft.is_symlink() {
        fs::read_link(&path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    Ok(Value::map(vec![
        ("name".into(), Value::String(name)),
        ("type".into(), Value::String(classify(ft).into())),
        ("size".into(), Value::Int(meta.len() as i64)),
        (
            "mtime".into(),
            Value::Int(secs_since_epoch(meta.modified())),
        ),
        (
            "atime".into(),
            Value::Int(secs_since_epoch(meta.accessed())),
        ),
        ("btime".into(), Value::Int(secs_since_epoch(meta.created()))),
        (
            "readonly".into(),
            Value::Bool(meta.permissions().readonly()),
        ),
        ("target".into(), Value::String(target)),
    ]))
}

pub(super) fn builtin_resolve_path(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "resolve-path")?;
    let s = args[0].to_string();
    let rp = shell.resolve(&s);
    shell.check_fs_read(&rp)?;
    let resolved = rp
        .canonicalise_strict()
        .map_err(|e| sig(format!("resolve-path: {s}: {e}")))?;
    Ok(Value::String(resolved.to_string_lossy().into_owned()))
}
