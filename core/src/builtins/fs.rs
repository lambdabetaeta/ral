//! Filesystem queries, temp-path minting, and the `exists` / `is-file` /
//! `is-dir` / `is-link` / `is-readable` / `is-writable` predicates.
//!
//! Every path routes through [`super::util::checked_read_path`], so a probe
//! answers about the `within [dir: …]` cwd, not the OS cwd — bare
//! `Path::exists` would miss a file a redirect just wrote there.  Predicates
//! also set `last_status`, so `?` and `if` see an exit code beside the
//! `Bool`.  `absolute-path` is exempt: lexical, with no filesystem to gate.

use crate::types::{Break, Settled, Shell, Value, sig};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::util::{admits_read, arg0_str, checked_read_path};

/// Symlink first, so a `symlink_metadata` caller sees the link, not its target.
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

/// Seconds since the epoch, or 0 when the field is unrecorded or pre-epoch.
fn secs_since_epoch(t: std::io::Result<SystemTime>) -> i64 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "u64 seconds-since-epoch cannot reach i64::MAX (~292e9 years)"
            )]
            {
                d.as_secs() as i64
            }
        })
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:list-dir] `list-dir` builtin: reads a directory's entries as a stat/listing predicate, gated by `check_fs_read`; not turn-time model data I/O, raises no surface card."
)]
pub(super) fn builtin_list_dir(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let path = checked_read_path(shell, &args[0].to_string())?;
    let dir = path.as_path();
    let mut entries: Vec<(String, Value)> = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| io_err("list-dir", dir, &e))? {
        let entry = entry.map_err(|e| io_err("list-dir", dir, &e))?;
        // `checked_read_path` admitted the directory; each entry is a
        // distinct path whose metadata this returns.  Drop a denied entry
        // rather than abort, as `grep-files` and `explore-dir` do.
        if !admits_read(shell, &entry.path().to_string_lossy()) {
            continue;
        }
        entries.push(dir_entry_value(&entry)?);
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

/// Glob, preserving the pattern's shape: a cwd-relative pattern yields
/// cwd-relative matches, a sigil-rooted or absolute one absolute matches.
pub(super) fn builtin_glob(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let raw = arg0_str(args)?;
    let expanded = crate::path::sigil::expand_path_prefix(&raw, &shell.mobile.context.home());
    let input_is_cwd_relative = !crate::path::is_absolute(&expanded);
    let pattern = checked_read_path(shell, &raw)?
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
                results.push(Value::String(rendered.to_string_lossy().into_owned()));
            }
        }
        Err(e) => return Err(sig(format!("glob: {e}"))),
    }
    results.sort_by_key(std::string::ToString::to_string);
    Ok(Value::list(results))
}

/// Label an `io::Error` with the operation and the path that provoked it.
fn io_err(ctx: &str, path: &Path, e: &std::io::Error) -> Break {
    sig(format!("{ctx}: {}: {e}", path.display()))
}

/// One `list-dir` entry, paired with the filename its caller sorts on.
fn dir_entry_value(entry: &fs::DirEntry) -> Settled<(String, Value)> {
    let name = entry.file_name().to_string_lossy().into_owned();
    let path = entry.path();
    let file_type = entry
        .file_type()
        .map_err(|e| io_err("list-dir", &path, &e))?;
    let meta = entry
        .metadata()
        .map_err(|e| io_err("list-dir", &path, &e))?;
    let v = Value::map(vec![
        ("name".into(), Value::String(name.clone())),
        ("type".into(), Value::String(classify(file_type).into())),
        (
            "size".into(),
            Value::Int({
                #[allow(
                    clippy::cast_possible_wrap,
                    reason = "u64 file size in bytes is far below i64::MAX (8 EiB)"
                )]
                {
                    meta.len() as i64
                }
            }),
        ),
        (
            "mtime".into(),
            Value::Int(secs_since_epoch(meta.modified())),
        ),
    ]);
    Ok((name, v))
}

/// Portable per-path metadata: the `stat` fields `std::fs::Metadata` carries
/// everywhere — mode bits, uid/gid, nlink and inode would want a `cfg(unix)`
/// companion.  Stats the link itself, not its target; compose with
/// `resolve-path` for the latter.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:file-info] `file-info` builtin: `symlink_metadata`/`read_link` stat a path and read a symlink target as a metadata predicate, gated by `check_fs_read`; not turn-time model data I/O, raises no surface card."
)]
pub(super) fn builtin_file_info(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let resolved = checked_read_path(shell, &args[0].to_string())?;
    let path = resolved.as_path();
    let meta = fs::symlink_metadata(path).map_err(|e| io_err("file-info", path, &e))?;
    let ft = meta.file_type();
    let target = if ft.is_symlink() {
        fs::read_link(path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let name = path.file_name().map_or_else(
        || path.to_string_lossy().into_owned(),
        |s| s.to_string_lossy().into_owned(),
    );
    Ok(Value::map(vec![
        ("name".into(), Value::String(name)),
        ("type".into(), Value::String(classify(ft).into())),
        (
            "size".into(),
            Value::Int({
                #[allow(
                    clippy::cast_possible_wrap,
                    reason = "u64 file size in bytes is far below i64::MAX (8 EiB)"
                )]
                {
                    meta.len() as i64
                }
            }),
        ),
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
    let s = args[0].to_string();
    let resolved = checked_read_path(shell, &s)?
        .canonicalise_strict()
        .map_err(|e| sig(format!("resolve-path: {s}: {e}")))?;
    Ok(Value::String(resolved.to_string_lossy().into_owned()))
}

/// Lexical sibling of `resolve-path`: same anchoring, no
/// `canonicalise_strict`, so symlinks stand and the path need not exist —
/// and no `check_fs_read`, since that gate guards a stat this never does.
pub(super) fn builtin_absolute_path(args: &[Value], shell: &Shell) -> Settled<Value> {
    let resolved = shell.resolve(&args[0].to_string());
    Ok(Value::String(
        resolved.as_path().to_string_lossy().into_owned(),
    ))
}

/// Shared predicate body.  `probe` sees `None` when the path is missing or
/// unreadable, so every predicate answers `false` there rather than raising.
fn fs_probe_with(
    args: &[Value],
    shell: &mut Shell,
    read_meta: impl FnOnce(&Path) -> std::io::Result<fs::Metadata>,
    probe: impl FnOnce(Option<fs::Metadata>) -> bool,
) -> Settled<Value> {
    let rp = checked_read_path(shell, &args[0].to_string())?;
    let meta = read_meta(rp.as_path()).ok();
    let r = probe(meta);
    shell.set_status_from_bool(r);
    Ok(Value::Bool(r))
}

/// Stats without following: a dangling link still `exists`, a link is `is-link`.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:stat-nofollow] `exists`/stat predicates: `symlink_metadata` stats a path without following symlinks, gated by `check_fs_read`; a metadata predicate, not turn-time model data I/O, raises no surface card."
)]
fn fs_probe(
    args: &[Value],
    shell: &mut Shell,
    probe: impl FnOnce(Option<fs::Metadata>) -> bool,
) -> Settled<Value> {
    fs_probe_with(args, shell, |p| fs::symlink_metadata(p), probe)
}

/// Follows, as `test -f` / `test -d` / `test -r` do: a link to a file is
/// `is-file`, a dangling link fails every probe.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:stat-follow] `is-file`/`is-dir` predicates: `metadata` stats a path following symlinks, gated by `check_fs_read`; a metadata predicate, not turn-time model data I/O, raises no surface card."
)]
fn fs_probe_follow(
    args: &[Value],
    shell: &mut Shell,
    probe: impl FnOnce(Option<fs::Metadata>) -> bool,
) -> Settled<Value> {
    fs_probe_with(args, shell, |p| fs::metadata(p), probe)
}

pub(super) fn builtin_exists(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe(args, shell, |m| m.is_some())
}

pub(super) fn builtin_is_file(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe_follow(args, shell, |m| m.is_some_and(|m| m.is_file()))
}

pub(super) fn builtin_is_dir(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe_follow(args, shell, |m| m.is_some_and(|m| m.is_dir()))
}

pub(super) fn builtin_is_link(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe(args, shell, |m| {
        m.is_some_and(|m| m.file_type().is_symlink())
    })
}

pub(super) fn builtin_is_readable(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    // Exact on Windows, where `readonly` governs writes alone; on Unix an
    // approximation of `test -r`, the truth needing uid/gid/acl logic.
    fs_probe_follow(args, shell, |m| m.is_some())
}

/// [`fs_probe_with`] for predicates whose honest answer needs the path
/// itself, not just `Metadata` — an `access(2)` against the real uid/gid.
fn fs_probe_path(
    args: &[Value],
    shell: &mut Shell,
    probe: impl FnOnce(&Path) -> bool,
) -> Settled<Value> {
    let rp = checked_read_path(shell, &args[0].to_string())?;
    let r = probe(rp.as_path());
    shell.set_status_from_bool(r);
    Ok(Value::Bool(r))
}

/// `access(2)`, not `permissions().readonly()` — the latter ignores
/// ownership, so another user's 0644 file would read as writable.
#[cfg(unix)]
fn is_writable_path(path: &Path) -> bool {
    rustix::fs::access(path, rustix::fs::Access::WRITE_OK).is_ok()
}

/// Windows has no `access(2)`: the read-only attribute is what governs writes.
#[cfg(not(unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:writable-stat-nonunix] Stat behind the `is-writable` predicate on non-unix (readonly-attribute test). A stat predicate, not turn-time model data I/O — raises no card, like its unix sibling."
)]
fn is_writable_path(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| !m.permissions().readonly())
}

pub(super) fn builtin_is_writable(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe_path(args, shell, is_writable_path)
}
