//! Filesystem query and temporary-path builtins, plus the filesystem
//! predicates `exists`, `is-file`, `is-dir`, `is-link`, `is-readable`, and
//! `is-writable`.
//!
//! Every query resolves its path argument through
//! [`super::util::checked_read_path`], which resolves against the
//! `within [dir: …]` scoped `dynamic.ambient.cwd` (via `shell.resolve`) and
//! capability-checks the read.  Probing via `Path::new(p).exists()` would
//! resolve against the OS cwd instead, returning false for files written
//! via redirects inside a within-scoped directory.  The predicates each
//! record their boolean outcome in `shell.last_status`, so pipeline `?`
//! chaining and `if` see an exit-code-shaped signal alongside the `Bool`.
//! `absolute-path` is the one exception: purely lexical, it routes
//! through `shell.resolve` alone — there is no filesystem touch to gate.

use crate::types::{Break, Settled, Shell, Value, sig};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::util::{admits_read, arg0_str, check_arity, checked_read_path};

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
    check_arity(args, 1, "list-dir")?;
    let path = checked_read_path(shell, &args[0].to_string())?;
    let dir = path.as_path();
    let mut entries: Vec<(String, Value)> = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| io_err("list-dir", dir, &e))? {
        let entry = entry.map_err(|e| io_err("list-dir", dir, &e))?;
        // `checked_read_path` admitted the directory, but each entry is
        // a distinct path whose metadata (size/mtime/type) this stats and
        // returns, so a deny on a subpath must drop that entry.  Skip a
        // denied entry rather than abort the listing — the same policy
        // `_search-files` / `explore-dir` apply to their walked entries.
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
        .as_path()
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

/// Wrap a `std::io::Error` with the operation label and the path that
/// triggered it.  Stand-in for `fs-err`: every fs call here knows the
/// path it was acting on, so we attach it explicitly rather than relying
/// on a wrapper type.
fn io_err(ctx: &str, path: &Path, e: &std::io::Error) -> Break {
    sig(format!("{ctx}: {}: {e}", path.display()))
}

/// One `list-dir` entry as `(sort_key, value_for_caller)`.  The sort
/// key is the filename; the value is the public map shape ral exposes.
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
    check_arity(args, 1, "resolve-path")?;
    let s = args[0].to_string();
    let resolved = checked_read_path(shell, &s)?
        .canonicalise_strict()
        .map_err(|e| sig(format!("resolve-path: {s}: {e}")))?;
    Ok(Value::String(resolved.to_string_lossy().into_owned()))
}

/// Lexical sibling of `resolve-path`: the same sigil expansion and
/// logical-cwd anchoring (`shell.resolve`), minus `canonicalise_strict` —
/// symlinks stay as written, `.`/`..` fold by pure string math, and the
/// path need not exist.  No `check_fs_read`: the gate guards the stat
/// this builtin never performs.
pub(super) fn builtin_absolute_path(args: &[Value], shell: &Shell) -> Settled<Value> {
    check_arity(args, 1, "absolute-path")?;
    let resolved = shell.resolve(&args[0].to_string());
    Ok(Value::String(
        resolved.as_path().to_string_lossy().into_owned(),
    ))
}

/// Shared probe: read `path` through [`checked_read_path`] (which honours
/// `dynamic.ambient.cwd` via `shell.resolve`), then run `probe` against the
/// metadata produced by `read_meta`.  `probe` receives `None` when the path
/// doesn't exist or metadata can't be read, so predicates uniformly return
/// `false` for missing paths rather than surfacing the I/O error.
fn fs_probe_with(
    args: &[Value],
    shell: &mut Shell,
    name: &str,
    read_meta: impl FnOnce(&Path) -> std::io::Result<fs::Metadata>,
    probe: impl FnOnce(Option<fs::Metadata>) -> bool,
) -> Settled<Value> {
    check_arity(args, 1, name)?;
    let rp = checked_read_path(shell, &args[0].to_string())?;
    let meta = read_meta(rp.as_path()).ok();
    let r = probe(meta);
    shell.set_status_from_bool(r);
    Ok(Value::Bool(r))
}

/// Probe that *does not* follow symlinks (uses `symlink_metadata`).  A
/// dangling symlink still satisfies `exists`; a symlink to a regular file
/// reports `is-link` true.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:stat-nofollow] `exists`/stat predicates: `symlink_metadata` stats a path without following symlinks, gated by `check_fs_read`; a metadata predicate, not turn-time model data I/O, raises no surface card."
)]
fn fs_probe(
    args: &[Value],
    shell: &mut Shell,
    name: &str,
    probe: impl FnOnce(Option<fs::Metadata>) -> bool,
) -> Settled<Value> {
    fs_probe_with(args, shell, name, |p| fs::symlink_metadata(p), probe)
}

/// Probe that *does* follow symlinks (uses `metadata`).  Mirrors `test -f` /
/// `test -d` / `test -r` semantics: a symlink-to-a-file satisfies `is-file`,
/// a dangling symlink fails every probe.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:stat-follow] `is-file`/`is-dir` predicates: `metadata` stats a path following symlinks, gated by `check_fs_read`; a metadata predicate, not turn-time model data I/O, raises no surface card."
)]
fn fs_probe_follow(
    args: &[Value],
    shell: &mut Shell,
    name: &str,
    probe: impl FnOnce(Option<fs::Metadata>) -> bool,
) -> Settled<Value> {
    fs_probe_with(args, shell, name, |p| fs::metadata(p), probe)
}

pub(super) fn builtin_exists(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe(args, shell, "exists", |m| m.is_some())
}

pub(super) fn builtin_is_file(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe_follow(args, shell, "is-file", |m| m.is_some_and(|m| m.is_file()))
}

pub(super) fn builtin_is_dir(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe_follow(args, shell, "is-dir", |m| m.is_some_and(|m| m.is_dir()))
}

pub(super) fn builtin_is_link(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe(args, shell, "is-link", |m| {
        m.is_some_and(|m| m.file_type().is_symlink())
    })
}

pub(super) fn builtin_is_readable(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    // Portable readability test: `metadata()` is sufficient on Windows
    // (where `readonly` only governs writes) and approximates `test -r`
    // on Unix (a true permission probe would need uid/gid/acl logic).
    fs_probe_follow(args, shell, "is-readable", |m| m.is_some())
}

/// Resolve and capability-check `path`, then answer `probe` against the
/// resolved path itself (symlinks intact in the string, but a following
/// probe is free to resolve them).  The path-level sibling of
/// [`fs_probe_with`], for predicates whose honest answer needs more than
/// `Metadata` — e.g. an `access(2)` query against the real uid/gid.
fn fs_probe_path(
    args: &[Value],
    shell: &mut Shell,
    name: &str,
    probe: impl FnOnce(&Path) -> bool,
) -> Settled<Value> {
    check_arity(args, 1, name)?;
    let rp = checked_read_path(shell, &args[0].to_string())?;
    let r = probe(rp.as_path());
    shell.set_status_from_bool(r);
    Ok(Value::Bool(r))
}

/// True if `path` is writable by the calling process.  Follows symlinks
/// (answering about the target, like `is-readable`/`is-file`/`is-dir`),
/// and reports honestly: `permissions().readonly()` ignores ownership, so
/// a 0644 file owned by another user would read writable; `access(2)`
/// with `W_OK` evaluates the real uid/gid against the file's mode.
#[cfg(unix)]
fn is_writable_path(path: &Path) -> bool {
    rustix::fs::access(path, rustix::fs::Access::WRITE_OK).is_ok()
}

/// Windows has no `access(2)`; `readonly()` on the followed target's
/// metadata is the portable test there (the OS governs writes by the
/// read-only attribute, not a uid/gid mode).
#[cfg(not(unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:writable-stat-nonunix] Stat behind the `is-writable` predicate on non-unix (readonly-attribute test). A stat predicate, not turn-time model data I/O — raises no card, like its unix sibling."
)]
fn is_writable_path(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| !m.permissions().readonly())
}

pub(super) fn builtin_is_writable(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe_path(args, shell, "is-writable", is_writable_path)
}
