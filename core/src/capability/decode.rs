//! Decode a capability `Value` map — the argument of `grant [...]`, or a
//! `--capabilities` profile — into a frozen [`Capabilities`].
//!
//! A dimension the map does not name stays `None` and inherits the
//! surrounding frame, so an attenuation touches only what it lists.  The
//! author is the live user, so every malformed shape is a strict error.
//! The one exception is a path rooted under a foreign platform's
//! convention: it can never match an access here, so it is dropped as a
//! dead grant rather than failing the whole profile — the treatment
//! `exarch::policy::base::drop_dead_exec_grants` gives a bundled tool
//! Windows cannot back.

use crate::path::NormalizedPrefix;
use crate::types::{
    Capabilities, EditorPolicy, ExecMap, ExecPolicy, FsPolicy, List, PolicyError, ShellPolicy,
    Value, as_map, as_map_ref,
};
use std::collections::{BTreeMap, BTreeSet};

/// [`decode_exec_grant`]'s output, which [`freeze_exec_map`] turns into
/// the real [`ExecMap`]: `dirs` keys are still raw sigil-or-path strings,
/// and the bool is `true` for `'deny'`.
#[derive(Debug, Default)]
struct RawExecMap {
    literals: BTreeMap<String, ExecPolicy>,
    dirs: BTreeMap<String, bool>,
}

// ── Dimension decoders ────────────────────────────────────────────────────

/// `fs: [read: [...], write: [...], deny: [...]]`, where `deny` carves
/// holes inside the `read`/`write` prefix regions.
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

/// The fs path lists and the exec subcommand lists are both string-only.
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

/// Freeze one sigil-or-path entry, requiring the result absolute.
/// Shared by the fs prefix lists and the exec map keys.
///
/// A path rooted under a foreign platform's convention (`/usr/local/bin`
/// on a Windows build) yields `Ok(None)` — the dead grant the module
/// header describes.  A genuinely relative entry is an error instead: it
/// would re-anchor to the *live* cwd at check time, meaning a different
/// directory after a `cd`, so the message points at `cwd:`, which pins
/// "relative to here" at freeze.
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

/// [`freeze_absolute`] over a list, omitting the dead grants.
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

/// Strictly `true` or `false`, so a known key carrying some other shape
/// fails loudly rather than silently denying.
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

/// Walk a capability map into a frozen [`Capabilities`], resolving every
/// sigil against `ctx`.
///
/// The single `Value::Map → Capabilities` constructor, shared by the
/// `grant [...] { body }` builtin in `evaluator::scope` and the
/// capability-file loader in `capability::load`; both callers are
/// in-crate, so "a `Capabilities` has every path already resolved" holds
/// by construction.  Unknown keys error here and in each dimension's
/// decoder rather than being silently dropped.
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

/// Freeze the exec map's keys.  Every `dirs` key and every path-shaped
/// `literals` key (`xdg:bin`, `~/.cargo/bin`, `/usr/bin/git`) must
/// resolve absolute; bare command names (`git`, `kubectl`) are names
/// rather than paths and pass through unchanged.
///
/// `path:` and `system:` are special-cased here rather than in
/// [`freeze_absolute`] because each expands to *many* directories — one
/// per `$PATH` component, one per
/// [`crate::path::sigil::system_tool_roots`] entry.
fn freeze_exec_map(
    map: RawExecMap,
    ctx: &crate::path::sigil::FreezeCtx<'_>,
    err_prefix: &str,
) -> Result<ExecMap, PolicyError> {
    use crate::path::sigil::looks_like_path_or_sigil;
    // `None` is `freeze_absolute`'s dead grant: skip the key.
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
                // A decoder that stats: the freeze pass already reads
                // `$PATH` and the environment, so disk is in reach.
                if crate::path::is_dir(&frozen) {
                    return Err(PolicyError::new(format!(
                        "{err_prefix}: '{key}' is a directory, so as a literal command key it \
                         names a binary that cannot exist — did you mean '{key}/'?"
                    )));
                }
                literals.insert(frozen, policy);
            }
        } else {
            literals.insert(key, policy);
        }
    }

    // `'path:/'` strips its slash and lands here in `map.dirs`; reject it
    // before generic directory handling — `path:` is the one spelling.
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

/// Insert into `allow`/`deny`, meeting rather than overwriting.
/// `system:`'s expansion and an author's explicit grant can name the same
/// resolved directory — a Homebrew root the author carves back out with a
/// `deny` — and which of the two insertion loops above runs first must not
/// decide the verdict.  Deny is the sticky veto, as in [`ExecPolicy`].
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

/// Split `$PATH` on the platform separator, keeping the absolute entries.
/// A relative `$PATH` entry is the environment's business, not the grant
/// author's, so unlike a relative grant path it is dropped in silence.
fn path_dirs(err_prefix: &str) -> Result<Vec<NormalizedPrefix>, PolicyError> {
    let path = std::env::var("PATH").unwrap_or_default();
    let mut dirs = Vec::new();
    for entry in std::env::split_paths(&path) {
        if entry.as_os_str().is_empty() || !entry.is_absolute() {
            continue;
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

/// `system:` expansion.  Unlike `path:` it can never come back empty —
/// the platform's own tool roots are unconditional — so there is no
/// empty-expansion error to raise here.
fn system_dirs() -> Vec<NormalizedPrefix> {
    crate::path::sigil::system_tool_roots()
        .into_iter()
        .map(|p| NormalizedPrefix::from_surface(&p))
        .collect()
}

// ── Exec policy decoder ───────────────────────────────────────────────────

/// Decode the `exec` dimension of a grant into its pre-freeze shape.
///
/// A key ending in `/` names a directory prefix and lands in
/// [`RawExecMap::dirs`]; every other key is a literal command name or
/// path, taking `'allow'`, `'deny'`, or a subcommand allowlist.  An empty
/// allowlist is an error rather than a third spelling of `'allow'`:
/// `meet` intersects subcommand sets, so the empty set already means
/// "admits nothing", and one surface spelling cannot mean ⊤ and ⊥ at once.
///
/// The surface takes lowercase strings only; the capitalised serde tags
/// on [`ExecPolicy`] belong to the IPC wire format.
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

    #[test]
    fn decode_exec_grant_rejects_capitalised_string() {
        let v = exec_map(&[("git", Value::String("Allow".into()))]);
        let msg = decode_exec_grant(&v, "test").unwrap_err().message;
        assert!(msg.contains("'allow'"), "expected lowercase hint: {msg}");
        assert!(msg.contains("Allow"), "expected offending token: {msg}");
    }

    #[test]
    fn decode_exec_grant_bool_hint_lists_all_forms() {
        let v = exec_map(&[("git", Value::Bool(true))]);
        let msg = decode_exec_grant(&v, "test").unwrap_err().message;
        assert!(
            msg.contains("'allow'") && msg.contains("'deny'") && msg.contains("subcommand list"),
            "{msg}"
        );
    }

    /// Order-independence is what keeps a reorder or a resigil of
    /// [`freeze_exec_map`]'s two insertion loops from quietly widening
    /// authority.
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
