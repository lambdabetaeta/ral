//! Plugin manifest types and parsing.
//!
//! A plugin manifest is a Map returned by a plugin's top-level block.
//! The parser validates its shape, extracts hook handlers, keybindings,
//! and alias thunks, and yields a [`LoadedPlugin`] record plus the list
//! of `(name, thunk)` pairs that the loader installs into env.

use super::router::{KeyChord, builtin_action, parse_key_notation, reserved_action};
use super::{HookHealth, load_err};
use ral_core::types::Error;
use ral_core::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// Hook events recognised in manifests.  Typos are load errors so a
/// plugin cannot silently register a handler that never fires.
pub(super) const KNOWN_HOOKS: &[&str] =
    &["buffer-change", "pre-exec", "post-exec", "chpwd", "prompt"];

/// One keybinding manifest entry.  `chord` is the parsed form of `key`,
/// validated at load so nothing downstream re-parses or skips.  `guard`,
/// when present, is a regex compiled at load; the binding claims its chord
/// only when the regex matches the text left of the cursor, otherwise
/// dispatch falls to the next table entry or the editor's built-in action.
#[derive(Debug, Clone)]
pub(crate) struct KeyBinding {
    pub(crate) key: String,
    pub(crate) chord: KeyChord,
    pub(crate) handler: Value,
    pub(crate) guard: Option<regex::Regex>,
}

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
    pub(crate) keybindings: Vec<KeyBinding>,
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

fn parse_keybindings<I>(entries: I) -> Result<Vec<KeyBinding>, Error>
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
        let chord = parse_key_notation(&key)
            .ok_or_else(|| load_err(format!("keybinding '{key}': unrecognised key notation")))?;
        if let Some(action) = reserved_action(chord) {
            return Err(load_err(format!(
                "keybinding '{key}' is reserved for {action} and cannot be bound by a plugin"
            )));
        }
        let handler = map
            .get("handler")
            .cloned()
            .ok_or_else(|| load_err("keybinding entry missing 'handler' field"))?;
        let guard = map
            .get("guard")
            .map(std::string::ToString::to_string)
            .map(|g| {
                regex::Regex::new(&g)
                    .map_err(|e| load_err(format!("keybinding '{key}': invalid guard regex: {e}")))
            })
            .transpose()?;
        if guard.is_none()
            && let Some(action) = builtin_action(chord)
        {
            return Err(load_err(format!(
                "unguarded keybinding '{key}' would shadow the built-in {action} on every \
                 press; add a 'guard:' regex so unmatched presses fall through"
            )));
        }
        match handler {
            Value::Lambda { .. } | Value::Block { .. } => out.push(KeyBinding {
                key,
                chord,
                handler,
                guard,
            }),
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

    /// A `capabilities:` key is rejected with an error that names the key and
    /// points at `grant` (the authority rationale lives on the struct doc).
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

    /// A manifest with no `capabilities:` key parses into a clean plugin
    /// record carrying its name and no aliases.
    #[test]
    fn manifest_without_capabilities_parses() {
        let manifest = Value::map(vec![("name".into(), Value::String("p".into()))]);
        let (plugin, aliases) = LoadedPlugin::parse(&manifest).expect("clean manifest parses");
        assert_eq!(plugin.name, "p");
        assert!(aliases.is_empty());
    }

    /// A trivial `Value::Block` handler, so keybinding entries survive the
    /// not-a-block filter in [`parse_keybindings`].
    fn dummy_block() -> Value {
        Value::Block {
            body: std::sync::Arc::new(ral_core::source::Spanned::synthetic(
                ral_core::ir::CompKind::Return(ral_core::ir::Val::Unit),
            )),
            captured: std::sync::Arc::new(ral_core::types::Env::default()),
        }
    }

    fn keybinding_entry(key: &str, guard: Option<&str>) -> Value {
        let mut fields = vec![
            ("key".into(), Value::String(key.into())),
            ("handler".into(), dummy_block()),
        ];
        if let Some(g) = guard {
            fields.push(("guard".into(), Value::String(g.into())));
        }
        Value::map(fields)
    }

    /// A `guard:` regex compiles and rides along on the parsed binding.
    #[test]
    fn guard_parses() {
        let out = parse_keybindings(std::iter::once(keybinding_entry("ctrl-t", Some(r"\S+"))))
            .expect("valid guard parses");
        assert_eq!(out.len(), 1);
        assert!(out[0].guard.is_some());
    }

    /// An invalid guard regex is a load error naming the key.
    #[test]
    fn invalid_guard_regex_is_load_error() {
        let err = parse_keybindings(std::iter::once(keybinding_entry("ctrl-t", Some("("))))
            .expect_err("malformed regex must be rejected");
        assert!(
            err.message.contains("ctrl-t") && err.message.contains("guard"),
            "error should name the key and the guard, got: {}",
            err.message
        );
    }

    /// An unguarded binding on a chord with a ral-owned built-in action is
    /// rejected — tab/completion is one instance of the general rule.
    #[test]
    fn unguarded_builtin_chord_is_load_error() {
        for (key, action) in [
            ("tab", "completion"),
            ("enter", "accept-line"),
            ("up", "history navigation"),
            ("t", "text insertion"),
        ] {
            let err = parse_keybindings(std::iter::once(keybinding_entry(key, None)))
                .expect_err("unguarded builtin chord must be rejected");
            assert!(
                err.message.contains(key)
                    && err.message.contains(action)
                    && err.message.contains("guard"),
                "error should name the key, the action, and the fix, got: {}",
                err.message
            );
        }
    }

    /// The same chords parse with a guard: they compose with the built-in
    /// tail instead of shadowing it.
    #[test]
    fn guarded_builtin_chord_parses() {
        let out = parse_keybindings(std::iter::once(keybinding_entry("enter", Some(r"^git "))))
            .expect("guarded enter parses");
        assert_eq!(out.len(), 1);
    }

    /// Interrupt and EOF are reserved outright — a guard does not help.
    #[test]
    fn reserved_chords_are_rejected_guard_or_not() {
        for (key, guard) in [("ctrl-c", Some(r"\S")), ("ctrl-d", None)] {
            let err = parse_keybindings(std::iter::once(keybinding_entry(key, guard)))
                .expect_err("reserved chord must be rejected");
            assert!(
                err.message.contains(key) && err.message.contains("reserved"),
                "error should name the key as reserved, got: {}",
                err.message
            );
        }
    }

    /// An unparseable key notation is a load error, not a sync-time skip.
    #[test]
    fn unrecognised_key_notation_is_load_error() {
        let err = parse_keybindings(std::iter::once(keybinding_entry("hyper-x", None)))
            .expect_err("bad notation must be rejected");
        assert!(
            err.message.contains("hyper-x") && err.message.contains("notation"),
            "error should name the notation, got: {}",
            err.message
        );
    }

    /// A modified chord with no ral-owned action parses unguarded —
    /// replacing the underlying editor default is the intended use.
    #[test]
    fn modified_chord_without_builtin_action_parses_unguarded() {
        let out = parse_keybindings(std::iter::once(keybinding_entry("ctrl-t", None)))
            .expect("ctrl-t without guard still parses");
        assert_eq!(out.len(), 1);
        assert!(out[0].guard.is_none());
    }
}
