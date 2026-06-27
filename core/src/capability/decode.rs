//! Decode a capability `Value` map into a frozen [`Capabilities`].
//!
//! **The surface is `grant [...]` and its equivalent `--capabilities
//! <file>` ceiling** — a map keyed by capability dimension (`exec`,
//! `fs`, `net`, `editor`, `shell`, `audit`).  A dimension the map does
//! not name stays `None` and inherits the surrounding frame, so an
//! attenuation touches only the dimensions it lists.  The author is the
//! live user, so every malformed shape is a strict error carrying a
//! shape-specific hint.

use crate::types::*;
use std::collections::{BTreeMap, BTreeSet};

// ── Dimension decoders ────────────────────────────────────────────────────

/// `fs: [read: [...], write: [...], deny: [...]]`
///
/// `read` and `write` name prefix regions, `deny` carves a hole inside
/// them.  Each value is a list of paths; any other sub-key is an error.
/// Every entry is frozen against `ctx` into a [`NormalizedPrefix`]
/// (sigil-expanded, `.`/`..`-collapsed, required absolute) right here,
/// so a decoded `FsPolicy` is in the grant-side normal form by
/// construction.
fn decode_fs(
    value: &Value,
    err_prefix: &str,
    ctx: &crate::path::sigil::FreezeCtx<'_>,
) -> Settled<FsPolicy> {
    let entries = as_map(value, err_prefix)?;
    let mut fp = FsPolicy::default();
    for (sub, paths) in entries {
        let raw: Vec<String> = match paths {
            Value::List(items) => items.iter().map(|i| i.to_string()).collect(),
            other => {
                return Err(sig(format!(
                    "{err_prefix}: '{sub}' must be a list of paths, got {} (use [\"/path\"])",
                    other.type_name()
                )));
            }
        };
        let frozen = freeze_prefix_list(raw, ctx, err_prefix)?;
        match sub.as_str() {
            "read" => fp.read_prefixes = frozen,
            "write" => fp.write_prefixes = frozen,
            "deny" => fp.deny_paths = frozen,
            _ => {
                return Err(sig(format!(
                    "{err_prefix}: unknown key '{sub}' — expected one of read, write, deny"
                )));
            }
        }
    }
    Ok(fp)
}

/// Freeze each raw fs entry, requiring the result absolute.  Pairs the
/// raw entry with its frozen form so a rejected relative prefix names
/// the spelling the author wrote.
fn freeze_prefix_list(
    raw: Vec<String>,
    ctx: &crate::path::sigil::FreezeCtx<'_>,
    err_prefix: &str,
) -> Settled<Vec<crate::path::NormalizedPrefix>> {
    use crate::path::sigil::freeze_one;
    raw.into_iter()
        .map(|entry| {
            let frozen = freeze_one(&entry, ctx).map_err(sig)?;
            require_absolute(&frozen, &entry, err_prefix).map_err(sig)?;
            Ok(frozen)
        })
        .collect()
}

/// A capability bool field: strictly `true` or `false`, any other shape
/// an error naming the wrong type.  Shared by every boolean dimension
/// (`net`, `editor.*`, `shell.chdir`, `audit`) so a known key with a
/// non-Bool value fails loudly rather than silently denying.
fn decode_bool(value: &Value, err_prefix: &str) -> Settled<bool> {
    match value {
        Value::Bool(b) => Ok(*b),
        other => Err(sig(format!(
            "{err_prefix}: expected a Bool, got {}",
            other.type_name()
        ))),
    }
}

/// `editor: [read: bool, write: bool, tui: bool]`
fn decode_editor(value: &Value, err_prefix: &str) -> Settled<EditorPolicy> {
    let mut cap = EditorPolicy::default();
    for (k, v) in as_map_ref(value, err_prefix)? {
        match k.as_str() {
            "read" => cap.read = decode_bool(v, err_prefix)?,
            "write" => cap.write = decode_bool(v, err_prefix)?,
            "tui" => cap.tui = decode_bool(v, err_prefix)?,
            _ => return Err(sig(format!("{err_prefix}: unknown key '{k}'"))),
        }
    }
    Ok(cap)
}

/// `shell: [chdir: bool]`
fn decode_shell(value: &Value, err_prefix: &str) -> Settled<ShellPolicy> {
    let mut cap = ShellPolicy::default();
    for (k, v) in as_map_ref(value, err_prefix)? {
        match k.as_str() {
            "chdir" => cap.chdir = decode_bool(v, err_prefix)?,
            _ => return Err(sig(format!("{err_prefix}: unknown key '{k}'"))),
        }
    }
    Ok(cap)
}

// ── Capability map walker ─────────────────────────────────────────────────

/// Walk a capability map (a `Value::Map`) into a frozen [`Capabilities`],
/// resolving every sigil against `ctx` before returning.  The single
/// `Value::Map → Capabilities` constructor, shared between the
/// `grant [...] { body }` builtin and the capability-file loader, so both
/// surfaces accept the same schema, produce the same errors, and yield a
/// bundle whose paths are already resolved.  Both callers are in-crate, so
/// "a `Capabilities` is always frozen" is a type-level invariant with no
/// escape hatch.
///
/// Strict on top-level keys: an unknown key errors instead of being silently
/// dropped.  Each dimension's decoder is in turn strict on its own keys.
///
/// Every dimension stays `None` ("no opinion → inherits caller") unless
/// the map names it.  A grant or capability-file map attenuates only
/// along the dimensions named and leaves the rest alone.  Critically
/// this keeps `grant [exec: [foo: []]] body` from triggering OS-level
/// fs/net sandboxing (no fs/net dimension is restricted, so no child
/// sandbox is needed).
///
/// The freeze pass resolves `~` / `xdg:` / `cwd:` / `tempdir:` sigils and
/// rejects an `xdg:` value that escapes `ctx.home` (defence in depth — an
/// attacker-set `XDG_*_HOME=/etc` would otherwise silently widen the grant).
/// Freeze errors surface as neutral [`Break::Error`]s; the caller prepends
/// its own provenance.
pub(crate) fn decode_capability_map(
    value: &Value,
    err_prefix: &str,
    ctx: &crate::path::sigil::FreezeCtx<'_>,
) -> Settled<Capabilities> {
    let entries = as_map(value, err_prefix)?;
    let mut caps = Capabilities::default();
    for (k, v) in &entries {
        match k.as_str() {
            "exec" => caps.exec = Some(decode_exec_grant(v, &format!("{err_prefix} exec"))?),
            "fs" => caps.fs = Some(decode_fs(v, &format!("{err_prefix} fs"), ctx)?),
            "net" => caps.net = Some(decode_bool(v, &format!("{err_prefix} net"))?),
            "audit" => caps.audit = decode_bool(v, &format!("{err_prefix} audit"))?,
            "editor" => caps.editor = Some(decode_editor(v, &format!("{err_prefix} editor"))?),
            "shell" => caps.shell = Some(decode_shell(v, &format!("{err_prefix} shell"))?),
            _ => return Err(sig(format!("{err_prefix}: unknown key '{k}'"))),
        }
    }
    if let Some(exec) = caps.exec.as_mut() {
        *exec = freeze_exec_map(std::mem::take(exec), ctx, &format!("{err_prefix} exec"))
            .map_err(sig)?;
    }
    Ok(caps)
}

/// A frozen path-shaped grant entry must be absolute.  A bare relative
/// prefix (`proj`, `./a`) folds to a still-relative path that would
/// otherwise re-anchor to the *live* cwd at check time — the same grant
/// meaning a different directory after a `cd`.  Reject it, naming the
/// spelling the author wrote and pointing at the absolute form or the
/// `cwd:` sigil that pins "relative to here" at freeze.
fn require_absolute(
    frozen: &crate::path::NormalizedPrefix,
    raw: &str,
    err_prefix: &str,
) -> Result<(), String> {
    if frozen.is_absolute() {
        Ok(())
    } else {
        Err(format!(
            "{err_prefix}: relative path '{raw}' is not allowed — \
             use an absolute path, or cwd:{raw} for \"relative to here\""
        ))
    }
}

/// Freeze sigils in exec map keys.  Every `dirs` key is a path/sigil,
/// so all are frozen and must resolve absolute; among `literals`, only
/// the path-shaped keys (`xdg:bin`, `~/.cargo/bin`, absolute literal
/// paths) carry sigils and must resolve absolute, while bare command
/// names (`git`, `kubectl`) are names rather than paths and pass
/// through unchanged.
///
/// The `path:` sigil is expanded here: it reads `$PATH`, splits on `:`,
/// and inserts each absolute component as a `dirs` entry with the given
/// verdict.  `path:` only accepts `allow`/`deny` — a subcommand list
/// makes no sense against a directory prefix.
fn freeze_exec_map(
    map: ExecMap,
    ctx: &crate::path::sigil::FreezeCtx<'_>,
    err_prefix: &str,
) -> Result<ExecMap, String> {
    use crate::path::sigil::{freeze_one, looks_like_path_or_sigil};
    let freeze_key = |key: &str| -> Result<String, String> {
        let frozen = freeze_one(key, ctx)?;
        require_absolute(&frozen, key, err_prefix)?;
        Ok(frozen.into_string())
    };

    let mut literals = BTreeMap::new();
    let mut dirs = BTreeMap::new();

    // Expand `path:` sigil — one directory entry per PATH component.
    for (key, policy) in map.literals {
        if key == "path:" {
            let verdict = match policy {
                ExecPolicy::Allow => ExecDir::Allow,
                ExecPolicy::Deny => ExecDir::Deny,
                _ => return Err(format!(
                    "{err_prefix}: 'path:' only takes 'allow' or 'deny', not a subcommand list"
                )),
            };
            for d in path_dirs(err_prefix)? {
                dirs.insert(d, verdict.clone());
            }
        } else {
            let key = if looks_like_path_or_sigil(&key) {
                freeze_key(&key)?
            } else {
                key
            };
            literals.insert(key, policy);
        }
    }

    // Also expand `path:/` — the decoder strips the trailing `/`, so a
    // `'path:/': 'allow'` entry lands here as `"path:"` in `dirs`.
    for (key, dir) in map.dirs {
        if key == "path:" {
            for d in path_dirs(err_prefix)? {
                dirs.insert(d, dir.clone());
            }
        } else {
            let frozen = freeze_key(&key)?;
            dirs.insert(frozen, dir);
        }
    }

    Ok(ExecMap { literals, dirs })
}

/// Split `$PATH`, normalise each absolute entry, skip empties and relatives.
fn path_dirs(err_prefix: &str) -> Result<Vec<String>, String> {
    let path = std::env::var("PATH").unwrap_or_default();
    let mut dirs = Vec::new();
    for entry in path.split(':') {
        if entry.is_empty() {
            continue;
        }
        let p = std::path::Path::new(entry);
        if !p.is_absolute() {
            continue; // skip relative PATH entries silently
        }
        let normalized = crate::path::NormalizedPrefix::from_surface(p);
        dirs.push(normalized.into_string());
    }
    if dirs.is_empty() {
        return Err(format!(
            "{err_prefix}: 'path:' expands to zero absolute directories — \
             PATH is empty, unset, or contains only relative entries"
        ));
    }
    Ok(dirs)
}

// ── Exec policy decoder ───────────────────────────────────────────────────

/// Decode the `exec` dimension of a grant.
///
/// A key ending in `/` names a directory prefix; the slash is dropped
/// and the entry lands in [`ExecMap::dirs`] with a two-valued
/// [`ExecDir`].  A directory entry must be `'allow'` or `'deny'` — a
/// subcommand list is name-shaped and requires a literal key.
///
/// Every other key is a bare command name or absolute literal path and
/// lands in [`ExecMap::literals`].  Three surface forms per literal:
///   * `'allow'`         — admit the command with any arguments.
///   * `'deny'`          — sticky veto, propagates upward through `meet`.
///   * `[]` / `[s, …]`   — subcommand allowlist (empty = `'allow'`).
///
/// Lowercase strings are the ral surface convention; the internal serde
/// tags on `ExecPolicy` (capitalised) are reserved for the IPC wire format.
/// `Bool` and `Thunk` are rejected with shape-specific hints so authors
/// get better errors than "policy must be a list of subcommands".
fn decode_exec_grant(value: &Value, err_prefix: &str) -> Settled<ExecMap> {
    let entries = as_map(value, err_prefix)?;
    let mut out = ExecMap::default();
    for (cmd, policy_val) in entries {
        if let Some(dir) = cmd.strip_suffix('/') {
            let verdict = match policy_val {
                Value::String(s) if s == "allow" => ExecDir::Allow,
                Value::String(s) if s == "deny" => ExecDir::Deny,
                _ => {
                    return Err(sig(format!(
                        "{err_prefix}: directory key '{cmd}' must be 'allow' or 'deny'; \
                         a subcommand list is name-shaped and requires a literal key"
                    )));
                }
            };
            out.dirs.insert(dir.to_string(), verdict);
            continue;
        }
        let policy = match policy_val {
            Value::String(s) => match s.as_str() {
                "allow" => ExecPolicy::Allow,
                "deny" => ExecPolicy::Deny,
                other => {
                    return Err(sig(format!(
                        "{err_prefix}: policy for '{cmd}' must be 'allow', 'deny', or a list of subcommands; got '{other}'"
                    )));
                }
            },
            Value::Bool(_) => {
                return Err(sig(format!(
                    "{err_prefix}: use 'allow', 'deny', or [] (subcommand list) for '{cmd}', not true/false"
                )));
            }
            Value::List(items) => {
                let subs: BTreeSet<String> = items.iter().map(|i| i.to_string()).collect();
                if subs.is_empty() {
                    ExecPolicy::Allow
                } else {
                    ExecPolicy::Subcommands(subs)
                }
            }
            Value::Lambda { .. } | Value::Block { .. } => {
                return Err(sig(format!(
                    "{err_prefix}: block form for '{cmd}' is not a valid exec policy; use within [handlers: [{cmd}: ...]] instead"
                )));
            }
            other => {
                return Err(sig(format!(
                    "{err_prefix}: policy for '{cmd}' must be 'allow', 'deny', or a list of subcommands; got {}",
                    other.type_name()
                )));
            }
        };
        out.literals.insert(cmd, policy);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec_map(entries: &[(&str, Value)]) -> Value {
        Value::Map(
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn decode_exec_grant_accepts_lowercase_allow_string() {
        let v = exec_map(&[("git", Value::String("allow".into()))]);
        let m = decode_exec_grant(&v, "test").unwrap();
        assert_eq!(m.literals.get("git"), Some(&ExecPolicy::Allow));
    }

    #[test]
    fn decode_exec_grant_accepts_lowercase_deny_string() {
        let v = exec_map(&[("bash", Value::String("deny".into()))]);
        let m = decode_exec_grant(&v, "test").unwrap();
        assert_eq!(m.literals.get("bash"), Some(&ExecPolicy::Deny));
    }

    #[test]
    fn decode_exec_grant_empty_list_means_allow() {
        let v = exec_map(&[("ls", Value::list(vec![]))]);
        let m = decode_exec_grant(&v, "test").unwrap();
        assert_eq!(m.literals.get("ls"), Some(&ExecPolicy::Allow));
    }

    #[test]
    fn decode_exec_grant_nonempty_list_is_subcommands() {
        let v = exec_map(&[(
            "cargo",
            Value::list(vec![
                Value::String("build".into()),
                Value::String("test".into()),
            ]),
        )]);
        let m = decode_exec_grant(&v, "test").unwrap();
        match m.literals.get("cargo") {
            Some(ExecPolicy::Subcommands(s)) => {
                assert_eq!(
                    s,
                    &BTreeSet::from(["build".to_string(), "test".to_string()])
                )
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// The ral surface accepts lowercase policy strings only; the
    /// capitalised serde tag on `ExecPolicy` is reserved for the IPC
    /// wire path.
    #[test]
    fn decode_exec_grant_rejects_capitalised_string() {
        let v = exec_map(&[("git", Value::String("Allow".into()))]);
        let err = decode_exec_grant(&v, "test").unwrap_err();
        let msg = match err {
            crate::types::Break::Error(e) => e.message,
            other => panic!("unexpected: {other:?}"),
        };
        assert!(msg.contains("'allow'"), "expected lowercase hint: {msg}");
        assert!(msg.contains("Allow"), "expected offending token: {msg}");
    }

    /// A `Bool` policy is rejected with a hint naming all three valid
    /// forms (`'allow'`, `'deny'`, `[]`).
    #[test]
    fn decode_exec_grant_bool_hint_lists_all_forms() {
        let v = exec_map(&[("git", Value::Bool(true))]);
        let err = decode_exec_grant(&v, "test").unwrap_err();
        let msg = match err {
            crate::types::Break::Error(e) => e.message,
            other => panic!("unexpected: {other:?}"),
        };
        assert!(
            msg.contains("'allow'") && msg.contains("'deny'") && msg.contains("[]"),
            "{msg}"
        );
    }
}
