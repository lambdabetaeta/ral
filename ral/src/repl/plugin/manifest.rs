//! Plugin manifest types and parsing.
//!
//! A plugin manifest is a Map returned by a plugin's top-level block.
//! The parser validates its shape, extracts hook handlers, keybindings,
//! and alias thunks, and yields a [`LoadedPlugin`] record plus the
//! [`ManifestHandlers`] the caller registers into the shell hook table and
//! env.

use super::router::{KeyChord, builtin_action, parse_key_notation, reserved_action};
use super::{HookHealth, load_err};
use ral_core::types::Error;
use ral_core::{Map, Value};
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
    /// Hook event names this plugin registered.  The handler values
    /// themselves live only in the shell hook table.
    pub(crate) hooks: Vec<String>,
    pub(crate) keybindings: Vec<KeyBinding>,
    /// Names installed into env at load time; removed on unload.
    pub(crate) bindings: Vec<String>,
    pub(crate) state_cell: Option<Value>,
    /// The plugin file's source text, installed as the root context of every
    /// framed hook run so a fault inside a handler renders against the right
    /// line of the right file.  Set by the loader once the file is read;
    /// [`parse`](Self::parse) leaves it empty (it sees only the manifest value).
    pub(crate) source: Arc<str>,
    /// Circuit-breaker state for this plugin's buffer-change hook — the only
    /// hook that fires on every keystroke and so needs a per-session brake.
    pub(crate) buffer_change_health: HookHealth,
}

/// The handler values a manifest parse extracts, separate from the
/// [`LoadedPlugin`] record so the loader can register them into the shell
/// hook table and env without the record retaining a second copy.
#[derive(Debug)]
pub(super) struct ManifestHandlers {
    pub(super) hooks: Vec<(String, Value)>,
    pub(super) keybindings: Vec<(String, Value)>,
    pub(super) aliases: Vec<(String, Value)>,
}

impl LoadedPlugin {
    /// Parse a plugin manifest value into the plugin record plus the
    /// handler values the caller registers into the hook table and env.
    pub(super) fn parse(val: &Value) -> Result<(Self, ManifestHandlers), Error> {
        let Value::Map(map) = val else {
            return Err(load_err(format!(
                "plugin manifest: expected Map, got {}",
                val.type_name()
            )));
        };
        if map.get("capabilities").is_some() {
            return Err(load_err(
                "manifest 'capabilities:' is not enforced — plugins run with host \
                 authority. Remove the key; to confine a plugin invocation, wrap it \
                 in `grant { ... }`.",
            ));
        }
        let name = match map.get("name") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => {
                return Err(load_err(format!(
                    "manifest 'name': expected String, got {}",
                    other.type_name()
                )));
            }
            None => return Err(load_err("manifest missing required 'name' field")),
        };
        let aliases = match map.get("aliases") {
            Some(Value::Map(m)) => parse_aliases(m)?,
            Some(other) => {
                return Err(load_err(format!(
                    "manifest 'aliases': expected Map, got {}",
                    other.type_name()
                )));
            }
            None => Vec::new(),
        };
        let hooks = match map.get("hooks") {
            Some(Value::Map(m)) => parse_hooks(m)?,
            Some(other) => {
                return Err(load_err(format!(
                    "manifest 'hooks': expected Map, got {}",
                    other.type_name()
                )));
            }
            None => Vec::new(),
        };
        let (keybindings, keybinding_handlers) = match map.get("keybindings") {
            Some(Value::List(l)) => parse_keybindings(l.iter().cloned())?,
            Some(other) => {
                return Err(load_err(format!(
                    "manifest 'keybindings': expected List, got {}",
                    other.type_name()
                )));
            }
            None => (Vec::new(), Vec::new()),
        };
        let plugin = Self {
            hooks: hooks.iter().map(|(event, _)| event.clone()).collect(),
            keybindings,
            bindings: aliases.iter().map(|(n, _)| n.clone()).collect(),
            state_cell: None,
            source: Arc::from(""),
            buffer_change_health: HookHealth::default(),
            name,
        };
        let handlers = ManifestHandlers {
            hooks,
            keybindings: keybinding_handlers,
            aliases,
        };
        Ok((plugin, handlers))
    }
}

fn parse_hooks(entries: &Map) -> Result<Vec<(String, Value)>, Error> {
    let mut out = Vec::new();
    for (event, value) in entries {
        if !KNOWN_HOOKS.contains(&event.as_str()) {
            return Err(load_err(format!(
                "unknown hook event '{event}'. Valid events: {}",
                KNOWN_HOOKS.join(", ")
            )));
        }
        if !matches!(value, Value::Thunk(_)) {
            return Err(load_err(format!(
                "hook '{event}': expected a block, got {}",
                value.type_name()
            )));
        }
        out.push((event.clone(), value.clone()));
    }
    Ok(out)
}

type KeybindingHandlers = Vec<(String, Value)>;

fn parse_keybindings<I>(entries: I) -> Result<(Vec<KeyBinding>, KeybindingHandlers), Error>
where
    I: Iterator<Item = Value>,
{
    let mut out = Vec::new();
    let mut handlers = Vec::new();
    for entry in entries {
        let Value::Map(map) = entry else {
            return Err(load_err(format!(
                "keybinding entry: expected Map, got {}",
                entry.type_name()
            )));
        };
        let key = match map.get("key") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => {
                return Err(load_err(format!(
                    "keybinding 'key': expected String, got {}",
                    other.type_name()
                )));
            }
            None => return Err(load_err("keybinding entry missing 'key' field")),
        };
        let chord = parse_key_notation(&key)
            .ok_or_else(|| load_err(format!("keybinding '{key}': unrecognised key notation")))?;
        if let Some(action) = reserved_action(chord) {
            return Err(load_err(format!(
                "keybinding '{key}' is reserved for {action} and cannot be bound by a plugin"
            )));
        }
        let handler = match map.get("handler") {
            Some(h @ Value::Thunk(_)) => h.clone(),
            Some(other) => {
                return Err(load_err(format!(
                    "keybinding '{key}': handler: expected a block, got {}",
                    other.type_name()
                )));
            }
            None => return Err(load_err("keybinding entry missing 'handler' field")),
        };
        let guard =
            match map.get("guard") {
                Some(Value::String(g)) => Some(regex::Regex::new(g).map_err(|e| {
                    load_err(format!("keybinding '{key}': invalid guard regex: {e}"))
                })?),
                Some(other) => {
                    return Err(load_err(format!(
                        "keybinding '{key}': guard: expected String, got {}",
                        other.type_name()
                    )));
                }
                None => None,
            };
        if guard.is_none()
            && let Some(action) = builtin_action(chord)
        {
            return Err(load_err(format!(
                "unguarded keybinding '{key}' would shadow the built-in {action} on every \
                 press; add a 'guard:' regex so unmatched presses fall through"
            )));
        }
        handlers.push((key.clone(), handler));
        out.push(KeyBinding { key, chord, guard });
    }
    Ok((out, handlers))
}

fn parse_aliases(entries: &Map) -> Result<Vec<(String, Value)>, Error> {
    let mut out = Vec::with_capacity(entries.len());
    for (name, value) in entries {
        if !matches!(value, Value::Thunk(_)) {
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
    use ral_core::types::Closure;

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
        let (plugin, handlers) = LoadedPlugin::parse(&manifest).expect("clean manifest parses");
        assert_eq!(plugin.name, "p");
        assert!(handlers.aliases.is_empty());
    }

    /// A trivial block-shaped `Value::Thunk` handler for well-formed
    /// keybinding entries.
    fn dummy_block() -> Value {
        Value::Thunk(Closure {
            comp: std::sync::Arc::new(ral_core::source::Spanned::synthetic(
                ral_core::ir::CompKind::Return(ral_core::ir::Val::Unit),
            )),
            env: ral_core::types::Env::default(),
        })
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

    /// Every manifest field with a declared type rejects a value of any
    /// other type with an error naming the field and both types — nothing
    /// stringifies or silently degrades to an empty collection.
    #[test]
    fn wrong_typed_fields_are_load_errors() {
        for (field, value, expected) in [
            ("name", Value::Int(42), "String"),
            ("aliases", Value::Int(7), "Map"),
            ("hooks", Value::Int(7), "Map"),
            ("keybindings", Value::Int(7), "List"),
        ] {
            let manifest = Value::map(vec![
                ("name".into(), Value::String("p".into())),
                (field.into(), value),
            ]);
            let err = LoadedPlugin::parse(&manifest).expect_err("wrong type must be rejected");
            assert!(
                err.message.contains(field)
                    && err.message.contains(expected)
                    && err.message.contains("Int"),
                "error should name the field and both types, got: {}",
                err.message
            );
        }
    }

    /// A manifest that is not a Map at all is named as such, not reported
    /// as a missing field.
    #[test]
    fn non_map_manifest_is_load_error() {
        let err = LoadedPlugin::parse(&Value::Int(42)).expect_err("non-map must be rejected");
        assert!(
            err.message.contains("manifest") && err.message.contains("Int"),
            "error should name the manifest shape, got: {}",
            err.message
        );
    }

    /// A non-block keybinding handler rejects the manifest — it is not
    /// warned about and skipped.
    #[test]
    fn non_block_handler_is_load_error() {
        let entry = Value::map(vec![
            ("key".into(), Value::String("ctrl-t".into())),
            ("handler".into(), Value::Int(3)),
        ]);
        let err = parse_keybindings(std::iter::once(entry))
            .expect_err("non-block handler must be rejected");
        assert!(
            err.message.contains("ctrl-t") && err.message.contains("block"),
            "error should name the key and expected type, got: {}",
            err.message
        );
    }

    /// A non-string guard is rejected, not stringified into a regex.
    #[test]
    fn non_string_guard_is_load_error() {
        let entry = Value::map(vec![
            ("key".into(), Value::String("ctrl-t".into())),
            ("handler".into(), dummy_block()),
            ("guard".into(), Value::Int(3)),
        ]);
        let err = parse_keybindings(std::iter::once(entry)).expect_err("non-string guard rejected");
        assert!(
            err.message.contains("guard") && err.message.contains("Int"),
            "error should name the guard and its type, got: {}",
            err.message
        );
    }

    /// A `guard:` regex compiles and rides along on the parsed binding.
    #[test]
    fn guard_parses() {
        let (out, _) = parse_keybindings(std::iter::once(keybinding_entry("ctrl-t", Some(r"\S+"))))
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
        let (out, _) =
            parse_keybindings(std::iter::once(keybinding_entry("enter", Some(r"^git "))))
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
        let (out, _) = parse_keybindings(std::iter::once(keybinding_entry("ctrl-t", None)))
            .expect("ctrl-t without guard still parses");
        assert_eq!(out.len(), 1);
        assert!(out[0].guard.is_none());
    }
}
