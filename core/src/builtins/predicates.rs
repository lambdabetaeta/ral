//! Map / value predicates: `keys`, `has`, `is-empty`, `equal`, `lt`, `gt`,
//! plus filesystem predicates `exists`, `is-file`, `is-dir`, `is-link`,
//! `is-readable`, `is-writable`.
//!
//! Each comparison records its boolean outcome in `shell.last_status` so
//! that pipeline `?` chaining and `if` see a familiar exit-code-shaped
//! signal alongside the returned `Bool`.
//!
//! The filesystem predicates resolve their path argument through
//! `shell.resolve_path`, which honours the `within [dir: ...]` scoped
//! `dynamic.ambient.cwd`.  Probing via `Path::new(p).exists()` would resolve
//! against the OS cwd instead, returning false for files written via
//! redirects inside a within-scoped directory.

use crate::types::*;
use std::fs;
use std::path::Path;

use super::util::{check_arity, order_cmp, values_equal};

pub(super) fn builtin_keys(args: &[Value]) -> Settled<Value> {
    check_arity(args, 1, "keys")?;
    let m = as_map_ref(&args[0], "keys")?;
    Ok(Value::list(
        m.keys().map(|k| Value::String(k.clone())).collect(),
    ))
}

pub(super) fn builtin_has(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "has")?;
    let m = as_map_ref(&args[0], "has")?;
    let found = m.contains_key(&args[1].to_string());
    shell.set_status_from_bool(found);
    Ok(Value::Bool(found))
}

pub(super) fn builtin_is_empty(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "is-empty")?;
    let val = &args[0];
    let r = match val {
        Value::List(items) => items.is_empty(),
        Value::Map(m) => m.is_empty(),
        Value::Bytes(b) => b.is_empty(),
        Value::String(s) => s.is_empty(),
        _ => {
            return Err(Break::Error(
                Error::new(
                    format!(
                        "is-empty expects List, Map, Bytes, or String, got {}",
                        val.type_name()
                    ),
                    1,
                )
                .with_hint("use file-empty to test whether a file or directory is empty"),
            ));
        }
    };
    shell.set_status_from_bool(r);
    Ok(Value::Bool(r))
}

pub(super) fn builtin_equal(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "equal")?;
    let r = values_equal(&args[0], &args[1])?;
    shell.set_status_from_bool(r);
    Ok(Value::Bool(r))
}

pub(super) fn builtin_lt(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    order_cmp(args, shell, "lt", |o| o.is_lt())
}

pub(super) fn builtin_gt(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    order_cmp(args, shell, "gt", |o| o.is_gt())
}

/// Shared probe: resolve `path` through `shell.resolve_path` (which honours
/// `dynamic.ambient.cwd`), check capability, then run `probe` against the
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
    let rp = shell.resolve(&args[0].to_string());
    shell.check_fs_read(&rp)?;
    let meta = read_meta(rp.as_path()).ok();
    let r = probe(meta);
    shell.set_status_from_bool(r);
    Ok(Value::Bool(r))
}

/// Probe that *does not* follow symlinks (uses `symlink_metadata`).  A
/// dangling symlink still satisfies `exists`; a symlink to a regular file
/// reports `is-link` true.
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
    fs_probe_follow(args, shell, "is-file", |m| {
        m.map(|m| m.is_file()).unwrap_or(false)
    })
}

pub(super) fn builtin_is_dir(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe_follow(args, shell, "is-dir", |m| {
        m.map(|m| m.is_dir()).unwrap_or(false)
    })
}

pub(super) fn builtin_is_link(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe(args, shell, "is-link", |m| {
        m.map(|m| m.file_type().is_symlink()).unwrap_or(false)
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
    let rp = shell.resolve(&args[0].to_string());
    shell.check_fs_read(&rp)?;
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
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(c_path.as_ptr(), libc::W_OK) == 0 }
}

/// Windows has no `access(2)`; `readonly()` on the followed target's
/// metadata is the portable test there (the OS governs writes by the
/// read-only attribute, not a uid/gid mode).
#[cfg(not(unix))]
fn is_writable_path(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false)
}

pub(super) fn builtin_is_writable(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    fs_probe_path(args, shell, "is-writable", is_writable_path)
}
