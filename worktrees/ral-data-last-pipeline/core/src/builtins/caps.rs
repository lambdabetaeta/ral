//! Shared parsing for capability maps.
//!
//! Both `grant [...]` (a user-visible attenuation) and a plugin manifest's
//! `capabilities: [...]` block describe the same shape — a map keyed by
//! capability dimension (`exec`, `fs`, `net`, `editor`, `shell`, …) — but
//! with different defaults:
//!
//!   * **Grant** leaves dimensions the caller did not name as `None` so
//!     they inherit the surrounding frame; the caller is the live user
//!     and gets strict, helpful errors for malformed shapes.
//!   * **Plugin manifest** starts from `RawCapabilities::deny_all()` so
//!     anything the manifest omits is denied; unknown keys and shapes
//!     are silently ignored to keep manifests forward-compatible.
//!
//! The dimension parsers below are mode-agnostic: every helper writes
//! into the matching `RawCapabilities` field, taking an error-message
//! prefix so diagnostics stay precise.

use std::collections::BTreeMap;

use crate::types::*;

use super::util::{as_map, fold_map, sig};

// ── Dimension parsers ─────────────────────────────────────────────────────

/// `fs: [read: [...], write: [...], deny: [...]]`
///
/// `allow_deny` controls whether `deny` is recognised: `grant` admits it
/// (so users can carve a hole in their own grant); plugin manifests do
/// not — manifests express positive capability, not denial.  Unknown
/// sub-keys are an error iff `strict` is set.
pub(crate) fn parse_fs(
    value: &Value,
    err_prefix: &str,
    allow_deny: bool,
    strict: bool,
) -> Result<FsPolicy, EvalSignal> {
    let entries = as_map(value, err_prefix)?;
    let mut fp = FsPolicy::default();
    for (sub, paths) in entries {
        let list = match paths {
            Value::List(items) => items.iter().map(|i| i.to_string()).collect(),
            other => {
                return Err(sig(format!(
                    "{err_prefix}: '{sub}' must be a list of paths, got {} (use [\"/path\"])",
                    other.type_name()
                )));
            }
        };
        match sub.as_str() {
            "read" => fp.read_prefixes = list,
            "write" => fp.write_prefixes = list,
            "deny" if allow_deny => fp.deny_paths = list,
            _ if strict => {
                let allowed = if allow_deny {
                    "read, write, deny"
                } else {
                    "read, write"
                };
                return Err(sig(format!(
                    "{err_prefix}: unknown key '{sub}' — expected one of {allowed}"
                )));
            }
            _ => {} // silent for non-strict (manifest)
        }
    }
    Ok(fp)
}

/// `net: true | false`
pub(crate) fn parse_net(value: &Value, err_prefix: &str) -> Result<bool, EvalSignal> {
    match value {
        Value::Bool(b) => Ok(*b),
        other => Err(sig(format!(
            "{err_prefix}: expected a Bool, got {}",
            other.type_name()
        ))),
    }
}

/// `editor: [read: bool, write: bool, tui: bool]`
pub(crate) fn parse_editor(
    value: &Value,
    err_prefix: &str,
    strict: bool,
) -> Result<EditorPolicy, EvalSignal> {
    fold_map(
        value,
        err_prefix,
        |v| matches!(v, Value::Bool(true)),
        |cap: &mut EditorPolicy, k, b| match k {
            "read" => cap.read = b,
            "write" => cap.write = b,
            "tui" => cap.tui = b,
            _ => {}
        },
    )
    .and_then(|policy| {
        if strict {
            // Re-walk to catch unknown keys; cheap, runs once per grant.
            for (k, _) in as_map(value, err_prefix)? {
                if !matches!(k.as_str(), "read" | "write" | "tui") {
                    return Err(sig(format!("{err_prefix}: unknown key '{k}'")));
                }
            }
        }
        Ok(policy)
    })
}

/// `shell: [chdir: bool]`
pub(crate) fn parse_shell(
    value: &Value,
    err_prefix: &str,
    strict: bool,
) -> Result<ShellPolicy, EvalSignal> {
    fold_map(
        value,
        err_prefix,
        |v| matches!(v, Value::Bool(true)),
        |cap: &mut ShellPolicy, k, b| {
            if k == "chdir" {
                cap.chdir = b;
            }
        },
    )
    .and_then(|policy| {
        if strict {
            for (k, _) in as_map(value, err_prefix)? {
                if k != "chdir" {
                    return Err(sig(format!("{err_prefix}: unknown key '{k}'")));
                }
            }
        }
        Ok(policy)
    })
}

// ── Exec policy: two flavours ─────────────────────────────────────────────

/// Grant flavour: a user-visible attenuation.
///
/// Reject `Bool` and `Thunk` early with shape-specific hints so authors
/// get better errors than "policy must be a list of subcommands".
pub(crate) fn parse_exec_grant(
    value: &Value,
    err_prefix: &str,
) -> Result<BTreeMap<String, ExecPolicy>, EvalSignal> {
    let entries = as_map(value, err_prefix)?;
    let mut out = BTreeMap::new();
    for (cmd, policy_val) in entries {
        let policy = match policy_val {
            Value::Bool(_) => {
                return Err(sig(format!(
                    "{err_prefix}: use [] to allow all subcommands for '{cmd}', not true/false"
                )));
            }
            Value::List(items) => {
                let subs: Vec<String> = items.iter().map(|i| i.to_string()).collect();
                if subs.is_empty() {
                    ExecPolicy::Allow
                } else {
                    ExecPolicy::Subcommands(subs)
                }
            }
            Value::Thunk { .. } => {
                return Err(sig(format!(
                    "{err_prefix}: block form for '{cmd}' removed; use within [handlers: [{cmd}: ...]] instead"
                )));
            }
            other => {
                return Err(sig(format!(
                    "{err_prefix}: policy for '{cmd}' must be a list of subcommands; got {}",
                    other.type_name()
                )));
            }
        };
        out.insert(cmd, policy);
    }
    Ok(out)
}

/// Plugin-manifest flavour: a self-contained ceiling that may evolve.
///
/// `Bool(false)` skips an entry — the only way for a manifest to express
/// "this command's capability is intentionally absent."  Anything other
/// than a non-empty list is treated as `Allow` so manifests stay forward-
/// compatible.
pub(crate) fn parse_exec_manifest(
    value: &Value,
) -> Result<BTreeMap<String, ExecPolicy>, EvalSignal> {
    fold_map(
        value,
        "plugin capabilities exec",
        |v| match v {
            Value::Bool(false) => None,
            Value::List(items) if !items.is_empty() => Some(ExecPolicy::Subcommands(
                items.iter().map(|i| i.to_string()).collect(),
            )),
            _ => Some(ExecPolicy::Allow),
        },
        |acc: &mut BTreeMap<String, ExecPolicy>, cmd, policy| {
            if let Some(p) = policy {
                acc.insert(cmd.to_owned(), p);
            }
        },
    )
}

