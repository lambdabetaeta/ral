//! Decode a capability `Value` map into a frozen [`Capabilities`].
//!
//! **The surface is `grant [...]` and its equivalent `--capabilities
//! <file>` ceiling** — a map keyed by capability dimension (`exec`,
//! `fs`, `net`, `detach`, `editor`, `shell`, `audit`).  A dimension the
//! map does not name stays `None` and inherits the surrounding frame, so an
//! attenuation touches only the dimensions it lists.  The author is the
//! live user, so every malformed shape is a strict error carrying a
//! shape-specific hint.
//!
//! One kind of path-shaped entry is *not* a shape error: a well-formed
//! absolute path that names a location foreign to this platform (a
//! Unix prefix like `/usr/local/bin` frozen on a Windows build, which
//! has a root but no drive letter).  Such an entry can never match a
//! real access here, so it is dropped as a dead grant rather than
//! rejected — the same "unusable on this platform" treatment
//! `exarch::policy::base::drop_dead_exec_grants` gives a bundled-tool
//! name Windows can't back.  Because this dropping lives in the one
//! decoder both the `grant [...]` builtin and the capability-file
//! loader share, a `--extend-base`/user capability file gets it for
//! free, alongside the built-in bases.  A genuinely relative entry
//! (no root at all, on any platform) is still a strict error: that is
//! an authoring ambiguity (re-anchoring to a future `cd`), not a
//! platform mismatch.

use crate::path::NormalizedPrefix;
use crate::types::{
    Capabilities, EditorPolicy, ExecMap, ExecPolicy, FsPolicy, List, PolicyError, ShellPolicy,
    Value, as_map, as_map_ref,
};
use std::collections::{BTreeMap, BTreeSet};

/// The pre-freeze shape [`decode_exec_grant`] parses a surface `exec`
/// map into: `dirs` keys are still raw sigil-or-path strings (not yet a
/// [`NormalizedPrefix`], which only a freeze door can mint), paired
/// with `true` for a `'deny'` verdict and `false` for `'allow'`.
/// [`freeze_exec_map`] consumes this and mints the real [`ExecMap`].
#[derive(Debug, Default)]
struct RawExecMap {
    literals: BTreeMap<String, ExecPolicy>,
    dirs: BTreeMap<String, bool>,
}

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
) -> Result<FsPolicy, PolicyError> {
    let entries = as_map_ref(value, err_prefix).map_err(PolicyError::from)?;
    let mut fp = FsPolicy::default();
    for (sub, paths) in entries {
        let items = match paths {
            Value::List(items) => items,
            other => {
                return Err(PolicyError::new(format!(
                    "{err_prefix}: '{sub}' must be a list of paths, got {} (use [\"/path\"])",
                    other.type_name()
                )));
            }
        };
        let raw = string_list(items, &format!("'{sub}' entries"), err_prefix)?;
        let frozen = freeze_prefix_list(raw, ctx, err_prefix)?;
        match sub.as_str() {
            "read" => fp.read_prefixes = frozen,
            "write" => fp.write_prefixes = frozen,
            "deny" => fp.deny_paths = frozen,
            _ => {
                return Err(PolicyError::new(format!(
                    "{err_prefix}: unknown key '{sub}' — expected one of read, write, deny"
                )));
            }
        }
    }
    Ok(fp)
}

/// Collect a `Value::List` into owned strings, rejecting any non-String
/// element with a shape-specific hint.  Shared by the fs path lists and
/// the exec subcommand lists, both of which are string-only.
fn string_list(items: &List, what: &str, err_prefix: &str) -> Result<Vec<String>, PolicyError> {
    items
        .iter()
        .map(|item| match item {
            Value::String(s) => Ok(s.clone()),
            other => Err(PolicyError::new(format!(
                "{err_prefix}: {what} must be strings — expected a string, got {}",
                other.type_name()
            ))),
        })
        .collect()
}

/// Freeze one sigil-or-path entry and classify the result. Shared by
/// the fs prefix lists and the exec map keys.
///
/// * Absolute here: kept.
/// * Rooted under a foreign platform's convention (a Unix prefix like
///   `/usr/local/bin`, frozen on a build where it has a root but no
///   drive letter): dropped (`Ok(None)`) as a dead grant — it can
///   never match a real access on this host, so silently omitting it
///   is more honest than erroring the whole profile out.
/// * Genuinely relative (no root at all): a strict error naming the
///   spelling the author wrote.  A bare relative prefix (`proj`,
///   `./a`) folds to a still-relative path that would otherwise
///   re-anchor to the *live* cwd at check time — the same grant
///   meaning a different directory after a `cd` — so this is rejected
///   rather than silently dropped, pointing at the absolute form or
///   the `cwd:` sigil that pins "relative to here" at freeze.
fn freeze_absolute(
    entry: &str,
    ctx: &crate::path::sigil::FreezeCtx<'_>,
    err_prefix: &str,
) -> Result<Option<crate::path::NormalizedPrefix>, PolicyError> {
    let frozen = crate::path::sigil::freeze_one(entry, ctx)?;
    if frozen.is_absolute() {
        return Ok(Some(frozen));
    }
    if crate::path::lex::is_foreign_rooted(frozen.as_str(), cfg!(windows)) {
        return Ok(None);
    }
    Err(PolicyError::new(format!(
        "{err_prefix}: relative path '{entry}' is not allowed — \
         use an absolute path, or cwd:{entry} for \"relative to here\""
    )))
}

/// Freeze each raw fs entry, requiring the result absolute.  An entry
/// that freezes to a foreign-rooted dead grant (see [`freeze_absolute`])
/// is silently omitted rather than erroring.
fn freeze_prefix_list(
    raw: Vec<String>,
    ctx: &crate::path::sigil::FreezeCtx<'_>,
    err_prefix: &str,
) -> Result<Vec<crate::path::NormalizedPrefix>, PolicyError> {
    let mut out = Vec::new();
    for entry in raw {
        if let Some(frozen) = freeze_absolute(&entry, ctx, err_prefix)? {
            out.push(frozen);
        }
    }
    Ok(out)
}

/// A capability bool field: strictly `true` or `false`, any other shape
/// an error naming the wrong type.  Shared by every boolean dimension
/// (`net`, `detach`, `editor.*`, `shell.chdir`, `audit`) so a known key
/// with a non-Bool value fails loudly rather than silently denying.
fn decode_bool(value: &Value, err_prefix: &str) -> Result<bool, PolicyError> {
    match value {
        Value::Bool(b) => Ok(*b),
        other => Err(PolicyError::new(format!(
            "{err_prefix}: expected a Bool, got {}",
            other.type_name()
        ))),
    }
}

/// `editor: [read: bool, write: bool, tui: bool]`
fn decode_editor(value: &Value, err_prefix: &str) -> Result<EditorPolicy, PolicyError> {
    let mut cap = EditorPolicy::default();
    for (k, v) in as_map_ref(value, err_prefix).map_err(PolicyError::from)? {
        match k.as_str() {
            "read" => cap.read = decode_bool(v, err_prefix)?,
            "write" => cap.write = decode_bool(v, err_prefix)?,
            "tui" => cap.tui = decode_bool(v, err_prefix)?,
            _ => return Err(PolicyError::new(format!("{err_prefix}: unknown key '{k}'"))),
        }
    }
    Ok(cap)
}

/// `shell: [chdir: bool]`
fn decode_shell(value: &Value, err_prefix: &str) -> Result<ShellPolicy, PolicyError> {
    let mut cap = ShellPolicy::default();
    for (k, v) in as_map_ref(value, err_prefix).map_err(PolicyError::from)? {
        match k.as_str() {
            "chdir" => cap.chdir = decode_bool(v, err_prefix)?,
            _ => return Err(PolicyError::new(format!("{err_prefix}: unknown key '{k}'"))),
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
/// along the dimensions named and leaves the rest alone.
///
/// The freeze pass resolves `~` / `xdg:` / `cwd:` / `tempdir:` sigils and
/// rejects an `xdg:` value that escapes `ctx.home` (defence in depth — an
/// attacker-set `XDG_*_HOME=/etc` would otherwise silently widen the grant).
/// Freeze errors surface as a [`PolicyError`]; the caller prepends its own
/// provenance and mints a `Break` from it, since the author of a grant or
/// capability file is the live user and every malformed shape here is a
/// message, never a process exit.
pub(crate) fn decode_capability_map(
    value: &Value,
    err_prefix: &str,
    ctx: &crate::path::sigil::FreezeCtx<'_>,
) -> Result<Capabilities, PolicyError> {
    let entries = as_map_ref(value, err_prefix).map_err(PolicyError::from)?;
    let mut caps = Capabilities::default();
    for (k, v) in entries {
        match k.as_str() {
            "exec" => {
                let raw = decode_exec_grant(v, &format!("{err_prefix} exec"))?;
                caps.exec = Some(freeze_exec_map(raw, ctx, &format!("{err_prefix} exec"))?);
            }
            "fs" => caps.fs = Some(decode_fs(v, &format!("{err_prefix} fs"), ctx)?),
            "net" => caps.net = Some(decode_bool(v, &format!("{err_prefix} net"))?),
            "detach" => caps.detach = Some(decode_bool(v, &format!("{err_prefix} detach"))?),
            "audit" => caps.audit = decode_bool(v, &format!("{err_prefix} audit"))?,
            "editor" => caps.editor = Some(decode_editor(v, &format!("{err_prefix} editor"))?),
            "shell" => caps.shell = Some(decode_shell(v, &format!("{err_prefix} shell"))?),
            _ => return Err(PolicyError::new(format!("{err_prefix}: unknown key '{k}'"))),
        }
    }
    Ok(caps)
}

/// Freeze sigils in exec map keys.  Every `dirs` key is a path/sigil,
/// so all are frozen and must resolve absolute; among `literals`, only
/// the path-shaped keys (`xdg:bin`, `~/.cargo/bin`, absolute literal
/// paths) carry sigils and must resolve absolute, while bare command
/// names (`git`, `kubectl`) are names rather than paths and pass
/// through unchanged.
///
/// A path-shaped literal that resolves to an existing directory is an
/// error: the surface distinguishes the two kinds by trailing slash, so
/// `/usr/bin: 'allow'` would otherwise decode to a literal grant on a
/// binary that cannot exist and fail closed at use time as a baffling
/// "denied by active grant".  The freeze pass already consults the
/// environment (`path:` reads `$PATH`, sigils resolve against `ctx`), so
/// asking the filesystem here is in keeping.
///
/// Two sigils expand to more than one directory and so are special-
/// cased here rather than in [`freeze_absolute`]: `path:` (one `dirs`
/// entry per `$PATH` component) and `system:` (one `dirs` entry per
/// [`crate::path::sigil::system_tool_roots`] — the platform's tool
/// roots). Both only accept `allow`/`deny` — a subcommand list makes
/// no sense against a directory prefix.
fn freeze_exec_map(
    map: RawExecMap,
    ctx: &crate::path::sigil::FreezeCtx<'_>,
    err_prefix: &str,
) -> Result<ExecMap, PolicyError> {
    use crate::path::sigil::looks_like_path_or_sigil;
    // `None` means the key froze to a foreign-rooted dead grant (see
    // `freeze_absolute`) — the caller skips inserting it.
    let freeze_key = |key: &str| -> Result<Option<String>, PolicyError> {
        Ok(freeze_absolute(key, ctx, err_prefix)?.map(NormalizedPrefix::into_string))
    };
    let dir_is_deny = |sigil: &str, policy: ExecPolicy| -> Result<bool, PolicyError> {
        match policy {
            ExecPolicy::Allow => Ok(false),
            ExecPolicy::Deny => Ok(true),
            ExecPolicy::Subcommands(_) => Err(PolicyError::new(format!(
                "{err_prefix}: '{sigil}' only takes 'allow' or 'deny', not a subcommand list \
                 (a subcommand list matches a command's first argument, so it needs a literal command)"
            ))),
        }
    };

    let mut literals = BTreeMap::new();
    let mut allow_dirs = BTreeSet::new();
    let mut deny_dirs = BTreeSet::new();

    for (key, policy) in map.literals {
        if key == "path:" {
            let is_deny = dir_is_deny("path:", policy)?;
            for d in path_dirs(err_prefix)? {
                insert_dir_meet(&mut allow_dirs, &mut deny_dirs, d, is_deny);
            }
        } else if key == "system:" {
            let is_deny = dir_is_deny("system:", policy)?;
            for d in system_dirs() {
                insert_dir_meet(&mut allow_dirs, &mut deny_dirs, d, is_deny);
            }
        } else if looks_like_path_or_sigil(&key) {
            if let Some(frozen) = freeze_key(&key)? {
                if crate::path::is_dir(&frozen) {
                    return Err(PolicyError::new(format!(
                        "{err_prefix}: '{key}' is a directory, so as a literal command key it \
                         names a binary that cannot exist — did you mean '{key}/'?"
                    )));
                }
                literals.insert(frozen, policy);
            }
            // else: foreign-rooted dead grant — dropped, see `freeze_absolute`.
        } else {
            literals.insert(key, policy);
        }
    }

    // A trailing `/` (`'path:/'`, `'system:/'`) strips to the bare
    // sigil and lands here in `map.dirs`; reject it before generic
    // directory handling — `path:`/`system:` is the one spelling.
    for (key, is_deny) in map.dirs {
        if key == "path:" || key == "system:" {
            return Err(PolicyError::new(format!(
                "{err_prefix}: '{key}/' is not a directory grant — \
                 use '{key}' with no trailing slash"
            )));
        }
        if let Some(frozen) = freeze_absolute(&key, ctx, err_prefix)? {
            insert_dir_meet(&mut allow_dirs, &mut deny_dirs, frozen, is_deny);
        }
    }

    Ok(ExecMap {
        literals,
        allow_dirs,
        deny_dirs,
    })
}

/// Insert `key` into `allow`/`deny` according to `is_deny`, meeting with
/// whatever is already there rather than overwriting it.  `system:`'s
/// expansion and an author's explicit directory grant can name the
/// same resolved directory (`system:` folding in a Homebrew root that
/// an author also carves back out with an explicit `deny`); the two
/// insertion loops above populate the sets in a fixed order, but which
/// loop "wins" must not be an accident of that order.  So a `deny`
/// always removes the key from `allow` and adds it to `deny`; an
/// `allow` is only added when `deny` does not already hold the key —
/// deny is the sticky veto, matching `ExecPolicy`'s lattice, regardless
/// of which insertion happens first.
fn insert_dir_meet(
    allow: &mut BTreeSet<NormalizedPrefix>,
    deny: &mut BTreeSet<NormalizedPrefix>,
    key: NormalizedPrefix,
    is_deny: bool,
) {
    if is_deny {
        allow.remove(&key);
        deny.insert(key);
    } else if !deny.contains(&key) {
        allow.insert(key);
    }
}

/// Split `$PATH` on the platform separator, normalise each absolute
/// entry, skip empties and relatives.
fn path_dirs(err_prefix: &str) -> Result<Vec<NormalizedPrefix>, PolicyError> {
    let path = std::env::var("PATH").unwrap_or_default();
    let mut dirs = Vec::new();
    for entry in std::env::split_paths(&path) {
        if entry.as_os_str().is_empty() || !entry.is_absolute() {
            continue; // skip empty and relative PATH entries silently
        }
        dirs.push(NormalizedPrefix::from_surface(&entry));
    }
    if dirs.is_empty() {
        return Err(PolicyError::new(format!(
            "{err_prefix}: 'path:' expands to zero absolute directories — \
             PATH is empty, unset, or contains only relative entries"
        )));
    }
    Ok(dirs)
}

/// `system:` directory expansion — see
/// [`crate::path::sigil::system_tool_roots`].  Unlike `path:` this can
/// never expand to zero directories: the platform's own tool roots
/// (`/usr/bin`+`/bin`, or `%SystemRoot%\System32`) are unconditional,
/// so there is no empty-expansion error to raise.
fn system_dirs() -> Vec<NormalizedPrefix> {
    crate::path::sigil::system_tool_roots()
        .into_iter()
        .map(|p| NormalizedPrefix::from_surface(&p))
        .collect()
}

// ── Exec policy decoder ───────────────────────────────────────────────────

/// Decode the `exec` dimension of a grant into its pre-freeze shape.
///
/// A key ending in `/` names a directory prefix; the slash is dropped
/// and the entry lands in [`RawExecMap::dirs`], `true` for `'deny'` and
/// `false` for `'allow'`.  A directory entry must be `'allow'` or
/// `'deny'` — a subcommand list is name-shaped and requires a literal
/// key.
///
/// Every other key is a bare command name or absolute literal path and
/// lands in [`ExecMap::literals`].  Three surface forms per literal:
///   * `'allow'`         — admit the command with any arguments.
///   * `'deny'`          — sticky veto, propagates upward through `meet`.
///   * `[s, …]`          — subcommand allowlist.  Empty is an error, not a
///     third spelling of `'allow'`: `meet` on two subcommand sets is
///     intersection and can produce the empty set, which admits nothing,
///     so an empty surface list would mean ⊤ and ⊥ at once.
///
/// Lowercase strings are the ral surface convention; the internal serde
/// tags on `ExecPolicy` (capitalised) are reserved for the IPC wire format.
/// `Bool` and `Thunk` are rejected with shape-specific hints so authors
/// get better errors than "policy must be a list of subcommands".
fn decode_exec_grant(value: &Value, err_prefix: &str) -> Result<RawExecMap, PolicyError> {
    let entries = as_map(value, err_prefix).map_err(PolicyError::from)?;
    let mut out = RawExecMap::default();
    for (cmd, policy_val) in entries {
        if let Some(dir) = cmd.strip_suffix('/') {
            let is_deny = match policy_val {
                Value::String(s) if s == "allow" => false,
                Value::String(s) if s == "deny" => true,
                _ => {
                    return Err(PolicyError::new(format!(
                        "{err_prefix}: directory key '{cmd}' must be 'allow' or 'deny'; \
                         a subcommand list matches a command's first argument, \
                         so it requires a literal command key"
                    )));
                }
            };
            out.dirs.insert(dir.to_string(), is_deny);
            continue;
        }
        let policy = match policy_val {
            Value::String(s) => match s.as_str() {
                "allow" => ExecPolicy::Allow,
                "deny" => ExecPolicy::Deny,
                other => {
                    return Err(PolicyError::new(format!(
                        "{err_prefix}: policy for '{cmd}' must be 'allow', 'deny', or a list of subcommands; got '{other}'"
                    )));
                }
            },
            Value::Bool(_) => {
                return Err(PolicyError::new(format!(
                    "{err_prefix}: use 'allow', 'deny', or a subcommand list for '{cmd}', not true/false"
                )));
            }
            Value::List(items) => {
                let subs: BTreeSet<String> =
                    string_list(&items, &format!("subcommands for '{cmd}'"), err_prefix)?
                        .into_iter()
                        .collect();
                if subs.is_empty() {
                    return Err(PolicyError::new(format!(
                        "{err_prefix}: empty subcommand list for '{cmd}' — \
                         use 'allow' to admit any arguments, or 'deny' to refuse the command"
                    )));
                }
                ExecPolicy::Subcommands(subs)
            }
            Value::Lambda { .. } | Value::Block { .. } => {
                return Err(PolicyError::new(format!(
                    "{err_prefix}: block form for '{cmd}' is not a valid exec policy; use within [handlers: [{cmd}: ...]] instead"
                )));
            }
            other => {
                return Err(PolicyError::new(format!(
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
    fn decode_exec_grant_rejects_empty_list() {
        let v = exec_map(&[("ls", Value::list(vec![]))]);
        let err = format!("{:?}", decode_exec_grant(&v, "test").unwrap_err());
        assert!(err.contains("empty subcommand list"), "{err}");
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
                );
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
        let msg = decode_exec_grant(&v, "test").unwrap_err().message;
        assert!(msg.contains("'allow'"), "expected lowercase hint: {msg}");
        assert!(msg.contains("Allow"), "expected offending token: {msg}");
    }

    /// A `Bool` policy is rejected with a hint naming all three valid
    /// forms (`'allow'`, `'deny'`, a subcommand list).
    #[test]
    fn decode_exec_grant_bool_hint_lists_all_forms() {
        let v = exec_map(&[("git", Value::Bool(true))]);
        let msg = decode_exec_grant(&v, "test").unwrap_err().message;
        assert!(
            msg.contains("'allow'") && msg.contains("'deny'") && msg.contains("subcommand list"),
            "{msg}"
        );
    }

    /// M4 regression: `insert_dir_meet` must resolve a same-key
    /// collision to `Deny` regardless of which verdict is inserted
    /// first.  Before the fix, `freeze_exec_map`'s two insertion loops
    /// (`system:`/`path:` expansion, then explicit `dirs` entries)
    /// resolved a collision by last-write-wins — correct only by
    /// accident of loop order, and silently flipped to widening by a
    /// harmless-looking reorder or resigil.
    #[test]
    fn insert_dir_meet_lets_deny_win_regardless_of_insertion_order() {
        let x = NormalizedPrefix::from_surface("/x");

        let mut allow = BTreeSet::new();
        let mut deny = BTreeSet::new();
        insert_dir_meet(&mut allow, &mut deny, x.clone(), false);
        insert_dir_meet(&mut allow, &mut deny, x.clone(), true);
        assert!(deny.contains(&x) && !allow.contains(&x));

        let mut allow = BTreeSet::new();
        let mut deny = BTreeSet::new();
        insert_dir_meet(&mut allow, &mut deny, x.clone(), true);
        insert_dir_meet(&mut allow, &mut deny, x.clone(), false);
        assert!(deny.contains(&x) && !allow.contains(&x));
    }
}
