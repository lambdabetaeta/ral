//! Plugin manifest types and parsing.
//!
//! A plugin manifest is a Map returned by a plugin's top-level block.
//! The parser validates its shape, extracts hook handlers, keybindings,
//! and alias thunks, and yields a [`LoadedPlugin`] record plus the list
//! of `(name, thunk)` pairs that the loader installs into env.

use super::{HookHealth, load_err};
use ral_core::types::Error;
use ral_core::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// Hook events recognised in manifests.  Typos are load errors so a
/// plugin cannot silently register a handler that never fires.
pub(super) const KNOWN_HOOKS: &[&str] =
    &["buffer-change", "pre-exec", "post-exec", "chpwd", "prompt"];

/// A loaded plugin.  Hooks fire from the rustyline event loop;
/// keybindings register against rustyline on the next sync; `bindings`
/// records the env entries installed at load so `unload` can remove
/// exactly those.  `state_cell` is the plugin's persistent state (read
/// and updated via `_ed-state` from inside hook handlers).
///
/// Plugins run with host authority — the manifest cannot narrow it.  A
/// `capabilities:` key is rejected as a load error rather than accepted and
/// ignored, so a plugin author cannot mistake the declaration for
/// confinement; to confine a plugin invocation, wrap it in `grant { ... }`.
#[derive(Debug, Clone)]
pub(crate) struct LoadedPlugin {
    pub(crate) name: String,
    pub(crate) hooks: HashMap<String, Value>,
    pub(crate) keybindings: Vec<(String, Value)>,
    /// Names installed into env at load time; removed on unload.
    pub(crate) bindings: Vec<String>,
    pub(crate) state_cell: Option<Value>,
    /// The plugin file's source text, installed as the root context of every
    /// framed hook turn so a fault inside a handler renders against the right
    /// line of the right file.  Set by the loader once the file is read;
    /// [`parse`](Self::parse) leaves it empty (it sees only the manifest value).
    pub(crate) source: Arc<str>,
    /// Circuit-breaker state for this plugin's buffer-change hook — the only
    /// hook that fires on every keystroke and so needs a per-session brake.
    pub(crate) buffer_change_health: HookHealth,
}

impl LoadedPlugin {
    /// Parse a plugin manifest value into the plugin record plus the
    /// `(name, thunk)` pairs the caller installs into env.
    pub(super) fn parse(val: &Value) -> Result<(Self, Vec<(String, Value)>), Error> {
        let map = as_map_ref(val);
        // `capabilities:` is not enforced — plugins run with host authority.
        // Reject it explicitly: silently accepting a security-shaped field
        // would let an author believe the plugin is confined when it is not.
        if map.is_some_and(|m| m.get("capabilities").is_some()) {
            return Err(load_err(
                "manifest 'capabilities:' is not enforced — plugins run with host \
                 authority. Remove the key; to confine a plugin invocation, wrap it \
                 in `grant { ... }`.",
            ));
        }
        let name = map
            .and_then(|m| m.get("name"))
            .map(std::string::ToString::to_string)
            .ok_or_else(|| load_err("manifest missing required 'name' field"))?;
        let aliases = match map.and_then(|m| m.get("aliases")) {
            Some(Value::Map(m)) => parse_aliases(m)?,
            _ => Vec::new(),
        };
        let hooks = match map.and_then(|m| m.get("hooks")) {
            Some(Value::Map(m)) => parse_hooks(m)?,
            _ => HashMap::new(),
        };
        let keybindings = match map.and_then(|m| m.get("keybindings")) {
            Some(Value::List(l)) => parse_keybindings(l.iter().cloned())?,
            _ => Vec::new(),
        };
        let plugin = Self {
            hooks,
            keybindings,
            bindings: aliases.iter().map(|(n, _)| n.clone()).collect(),
            state_cell: None,
            source: Arc::from(""),
            buffer_change_health: HookHealth::default(),
            name,
        };
        Ok((plugin, aliases))
    }
}

fn as_map_ref(val: &Value) -> Option<&Map> {
    match val {
        Value::Map(m) => Some(m),
        _ => None,
    }
}

fn parse_hooks(entries: &Map) -> Result<HashMap<String, Value>, Error> {
    let mut out = HashMap::new();
    for (event, value) in entries {
        if !KNOWN_HOOKS.contains(&event.as_str()) {
            return Err(load_err(format!(
                "unknown hook event '{event}'. Valid events: {}",
                KNOWN_HOOKS.join(", ")
            )));
        }
        if !matches!(value, Value::Lambda { .. } | Value::Block { .. }) {
            return Err(load_err(format!(
                "hook '{event}': expected a block, got {}",
                value.type_name()
            )));
        }
        out.insert(event.clone(), value.clone());
    }
    Ok(out)
}

fn parse_keybindings<I>(entries: I) -> Result<Vec<(String, Value)>, Error>
where
    I: Iterator<Item = Value>,
{
    let mut out = Vec::new();
    for entry in entries {
        let Value::Map(map) = entry else {
            return Err(load_err(format!(
                "keybinding entry: expected Map, got {}",
                entry.type_name()
            )));
        };
        let key = map
            .get("key")
            .map(std::string::ToString::to_string)
            .ok_or_else(|| load_err("keybinding entry missing 'key' field"))?;
        let handler = map
            .get("handler")
            .cloned()
            .ok_or_else(|| load_err("keybinding entry missing 'handler' field"))?;
        match handler {
            Value::Lambda { .. } | Value::Block { .. } => out.push((key, handler)),
            _ => {
                eprintln!(
                    "load-plugin: warning: keybinding '{key}' handler is not a block, skipping"
                );
            }
        }
    }
    Ok(out)
}

fn parse_aliases(entries: &Map) -> Result<Vec<(String, Value)>, Error> {
    let mut out = Vec::with_capacity(entries.len());
    for (name, value) in entries {
        if !matches!(value, Value::Lambda { .. } | Value::Block { .. }) {
            return Err(load_err(format!(
                "alias '{name}': expected a block, got {}",
                value.type_name()
            )));
        }
        out.push((name.clone(), value.clone()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `capabilities:` key is rejected with a clear error rather than
    /// accepted and ignored: it is never enforced, so silently dropping it
    /// would let an author believe the plugin is confined when it is not.
    #[test]
    fn capabilities_key_is_rejected() {
        let manifest = Value::map(vec![
            ("name".into(), Value::String("p".into())),
            ("capabilities".into(), Value::map(vec![])),
        ]);
        let err = LoadedPlugin::parse(&manifest)
            .expect_err("a manifest with capabilities: must be rejected");
        assert!(
            err.message.contains("capabilities") && err.message.contains("grant"),
            "error should name the key and point at grant, got: {}",
            err.message
        );
    }

    /// A manifest without the dead field parses as before.
    #[test]
    fn manifest_without_capabilities_parses() {
        let manifest = Value::map(vec![("name".into(), Value::String("p".into()))]);
        let (plugin, aliases) = LoadedPlugin::parse(&manifest).expect("clean manifest parses");
        assert_eq!(plugin.name, "p");
        assert!(aliases.is_empty());
    }
}
