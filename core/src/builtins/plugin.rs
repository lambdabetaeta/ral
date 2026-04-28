use crate::types::*;
use std::collections::HashMap;

use super::util::{arg0_str, fold_map, get, list_entries, map_entries, sig, str_list};

// ── Plugin helpers ────────────────────────────────────────────────────────

fn load_err(msg: impl std::fmt::Display) -> EvalSignal {
    sig(format!("_plugin 'load': {msg}"))
}

// ── Public entry point ────────────────────────────────────────────────────

pub fn builtin_plugin(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    match arg0_str(args).as_str() {
        "load" => plugin_load(&args[1..], shell),
        "unload" => plugin_unload(&args[1..], shell),
        op => Err(sig(format!("_plugin: unknown operation '{op}'"))),
    }
}

// ── Load ──────────────────────────────────────────────────────────────────

/// `_plugin 'load' <name-or-path> [<options-map>]`
///
/// 1. Resolve path + read source atomically (search `~/.config/ral/plugins/`,
///    `RAL_PATH`, literal).
/// 2. Under a `manifest_sandbox` grant that denies all effects, evaluate the
///    plugin file, instantiate its options thunk (if any), and validate the
///    result as a manifest.  Honest plugins build pure values here; malicious
///    ones fail load.
/// 3. Parse the manifest into a `LoadedPlugin` (outside the sandbox — pure).
/// 4. Register aliases and push into `shell.plugins`.
fn plugin_load(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let name_or_path = args
        .first()
        .map(|v| v.to_string())
        .ok_or_else(|| load_err("requires a plugin name or path"))?;

    let options = parse_options_arg(&args[1..], &name_or_path)?;

    check_not_loaded(&name_or_path, shell)?;

    let path = resolve_plugin_path(&name_or_path)?;
    shell.check_fs_read(&path)?;
    let source = std::fs::read_to_string(&path).map_err(|e| load_err(format!("{path}: {e}")))?;
    // Manifest sandbox: deny every effect.  The plugin body may build pure
    // values, but any fs / exec / net op fails.
    let module = shell.with_capabilities(Capabilities::deny_all(), |shell| {
        let value = eval_plugin_file(&path, &source, shell)?;
        let module = instantiate(value, options, &name_or_path, shell)?;
        check_is_manifest(&module, &name_or_path)?;
        Ok::<_, EvalSignal>(module)
    })?;

    let plugin = LoadedPlugin::parse(&module)?;
    check_not_loaded(&plugin.name, shell)?;
    register_aliases(&plugin.aliases, &plugin.name, shell)?;

    shell.registry.plugins.push(plugin);
    shell.registry.generation += 1;
    Ok(Value::Unit)
}

/// Accept zero or one trailing argument.  If present it must be a `Map`.
/// Returns `None` if no options were supplied, `Some(map)` otherwise.
fn parse_options_arg<'a>(rest: &'a [Value], name: &str) -> Result<Option<&'a Value>, EvalSignal> {
    match rest {
        [] => Ok(None),
        [opts @ Value::Map(_)] => Ok(Some(opts)),
        [other] => Err(load_err(format!(
            "plugin '{name}': options must be a Map, got {}",
            other.type_name()
        ))),
        _ => Err(load_err(format!(
            "plugin '{name}': expected at most one options-map argument, got {}",
            rest.len()
        ))),
    }
}

impl LoadedPlugin {
    fn parse(val: &Value) -> Result<Self, EvalSignal> {
        let map = map_entries(val);
        let name = get(map, "name")
            .map(|v| v.to_string())
            .ok_or_else(|| load_err("manifest missing required 'name' field"))?;
        Ok(Self {
            capabilities: parse_capabilities(get(map, "capabilities").map_or(&[], map_entries))?,
            hooks: parse_hooks(get(map, "hooks").map_or(&[], map_entries))?,
            keybindings: parse_keybindings(get(map, "keybindings").map_or(&[], list_entries))?,
            aliases: parse_aliases(get(map, "aliases").map_or(&[], map_entries), &name)?,
            state_cell: None,
            name,
        })
    }
}

fn check_not_loaded(name: &str, shell: &Shell) -> Result<(), EvalSignal> {
    if shell.registry.plugins.iter().any(|p| p.name == name) {
        return Err(load_err(format!("plugin '{name}' is already loaded")));
    }
    Ok(())
}

/// Check all aliases for conflicts first, then insert all; never commits a partial set.
fn register_aliases(
    aliases: &[(String, AliasEntry)],
    plugin_name: &str,
    shell: &mut Shell,
) -> Result<(), EvalSignal> {
    for (name, _) in aliases {
        if shell.registry.aliases.contains_key(name.as_str()) {
            return Err(load_err(format!(
                "alias '{name}' from plugin '{plugin_name}' conflicts with an existing alias"
            )));
        }
    }
    for (name, entry) in aliases {
        shell.registry.aliases.insert(name.clone(), entry.clone());
    }
    Ok(())
}

/// Evaluate a plugin file (already read and canonicalized) in an isolated scope.
fn eval_plugin_file(path: &str, source: &str, shell: &mut Shell) -> Result<Value, EvalSignal> {
    let path_owned = path.to_owned();
    if shell.modules.stack.contains(&path_owned) {
        return Err(load_err(format!("circular dependency: {path}")));
    }
    let comp = crate::compile(source).map_err(|e| load_err(format!("{path}: {e}")))?;

    let mut ctx = super::modules::ScriptContextGuard::enter(shell, &path_owned);
    ctx.env_mut().modules.stack.push(path_owned);
    ctx.env_mut().push_scope();
    let result = crate::evaluate(&comp, ctx.env_mut());
    ctx.env_mut().pop_scope();
    ctx.env_mut().modules.stack.pop();
    result
}

/// Apply the options map to a parameterised plugin block to yield its
/// manifest.  If the plugin is already a manifest map, a non-empty options
/// map is a load-time error; an absent or empty options map is fine.
fn instantiate(
    val: Value,
    options: Option<&Value>,
    name: &str,
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    let empty = Value::Map(Vec::new());
    match &val {
        Value::Thunk { .. } => crate::evaluator::call_value_pub(
            &val,
            std::slice::from_ref(options.unwrap_or(&empty)),
            shell,
        ),
        _ if matches!(options, Some(Value::Map(e)) if !e.is_empty()) => Err(load_err(format!(
            "plugin '{name}' takes no configuration; \
             remove 'options:' from the rc entry"
        ))),
        _ => Ok(val),
    }
}

/// Error if `val` is still a `Thunk` after instantiation.
/// A lambda body means the plugin's one options parameter was not supplied;
/// any other block body means the plugin returned a block instead of a map.
fn check_is_manifest(val: &Value, name: &str) -> Result<(), EvalSignal> {
    let Value::Thunk { body, .. } = val else {
        return Ok(());
    };
    Err(load_err(
        if matches!(body.as_ref().kind, crate::ir::CompKind::Lam { .. }) {
            format!(
                "plugin '{name}' expects its options map but none was applied; \
                 this is an internal error in _plugin 'load'"
            )
        } else {
            format!(
                "plugin '{name}' returned a Block as its manifest; \
                 expected a Map (e.g. [name: '...', capabilities: [...], keybindings: [...]])"
            )
        },
    ))
}

// ── Unload ────────────────────────────────────────────────────────────────

/// `_plugin 'unload' <name>`
fn plugin_unload(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let name = args
        .first()
        .map(|v| v.to_string())
        .ok_or_else(|| sig("_plugin 'unload' requires a plugin name"))?;
    let idx = shell
        .registry
        .plugins
        .iter()
        .position(|p| p.name == name)
        .ok_or_else(|| sig(format!("_plugin 'unload': plugin '{name}' is not loaded")))?;
    let plugin = shell.registry.plugins.remove(idx);
    for (alias_name, _) in &plugin.aliases {
        shell.registry.aliases.remove(alias_name.as_str());
    }
    shell.registry.generation += 1;
    Ok(Value::Unit)
}

// ── Path resolution ───────────────────────────────────────────────────────

/// Resolve a plugin name or path to a canonical absolute path.  Searches:
/// 1. `~/.config/ral/plugins/<name>.ral`
/// 2. Each directory in `RAL_PATH`: `$dir/<name>.ral`
/// 3. The literal path, then `<name>.ral`
///
/// Returns the canonicalized path so the later `check_fs_read` and
/// `read_to_string` operate on the same normalized name — collapsing the
/// `Path::exists()` probe and the `read_to_string` into one syscall window.
fn resolve_plugin_path(name_or_path: &str) -> Result<String, EvalSignal> {
    let ral_path = std::env::var("RAL_PATH").unwrap_or_default();
    config_base()
        .into_iter()
        .map(|cfg| format!("{cfg}/ral/plugins/{name_or_path}.ral"))
        .chain(
            ral_path
                .split(':')
                .map(|dir| format!("{dir}/{name_or_path}.ral")),
        )
        .chain([name_or_path.to_string(), format!("{name_or_path}.ral")])
        .find_map(|cand| std::fs::canonicalize(&cand).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| load_err(format!("plugin '{name_or_path}' not found")))
}

/// Return the XDG config base dir, falling back to `$HOME/.config` (Unix) or `$APPDATA` (Windows).
fn config_base() -> Option<String> {
    std::env::var("XDG_CONFIG_HOME").ok().or_else(|| {
        #[cfg(unix)]
        {
            std::env::var("HOME").ok().map(|h| format!("{h}/.config"))
        }
        #[cfg(not(unix))]
        {
            std::env::var("APPDATA").ok()
        }
    })
}

// ── Manifest parsing ──────────────────────────────────────────────────────

/// Build a `Capabilities` from capability entries; unknown keys are silently ignored.
fn parse_capabilities(entries: &[(String, Value)]) -> Result<Capabilities, EvalSignal> {
    let mut capabilities = Capabilities::deny_all();
    for (k, v) in entries {
        match k.as_str() {
            "exec" => capabilities.exec = Some(parse_exec_policy(v)?),
            "fs" => {
                capabilities.fs = Some(fold_map(
                    v,
                    "plugin capabilities fs",
                    str_list,
                    |fp: &mut FsPolicy, k, vs| match k {
                        "read" => fp.read_prefixes = vs,
                        "write" => fp.write_prefixes = vs,
                        _ => {}
                    },
                )?)
            }
            "net" => {
                capabilities.net = Some(match v {
                    Value::Bool(b) => *b,
                    other => {
                        return Err(load_err(format!(
                            "plugin capabilities net: expected a Bool, got {}",
                            other.type_name()
                        )));
                    }
                });
            }
            "editor" => {
                capabilities.editor = Some(fold_map(
                    v,
                    "plugin capabilities editor",
                    |v| matches!(v, Value::Bool(true)),
                    |cap: &mut EditorPolicy, k, b| match k {
                        "read" => cap.read = b,
                        "write" => cap.write = b,
                        "tui" => cap.tui = b,
                        _ => {}
                    },
                )?)
            }
            "shell" => {
                capabilities.shell = Some(fold_map(
                    v,
                    "plugin capabilities shell",
                    |v| matches!(v, Value::Bool(true)),
                    |cap: &mut ShellPolicy, k, b| {
                        if k == "chdir" {
                            cap.chdir = b
                        }
                    },
                )?)
            }
            _ => {}
        }
    }
    Ok(capabilities)
}

/// `false` → skip entry; `[sub, ...]` → `Subcommands`; anything else → `Allow`.
fn parse_exec_policy(v: &Value) -> Result<Vec<(String, ExecPolicy)>, EvalSignal> {
    fold_map(
        v,
        "plugin capabilities exec",
        |v| match v {
            Value::Bool(false) => None,
            Value::List(items) if !items.is_empty() => Some(ExecPolicy::Subcommands(
                items.iter().map(|i| i.to_string()).collect(),
            )),
            _ => Some(ExecPolicy::Allow),
        },
        |acc: &mut Vec<(String, ExecPolicy)>, cmd, policy| {
            if let Some(p) = policy {
                acc.push((cmd.to_owned(), p));
            }
        },
    )
}

const KNOWN_HOOKS: &[&str] = &["buffer-change", "pre-exec", "post-exec", "chpwd", "prompt"];

/// Validate hook entries: every key must be a known event name and every value
/// must be a thunk.  Typos and bad shapes are load errors so plugins can't
/// silently register handlers that never fire.
fn parse_hooks(entries: &[(String, Value)]) -> Result<HashMap<String, Value>, EvalSignal> {
    let mut out = HashMap::new();
    for (event, value) in entries {
        if !KNOWN_HOOKS.contains(&event.as_str()) {
            return Err(load_err(format!(
                "unknown hook event '{event}'. Valid events: {}",
                KNOWN_HOOKS.join(", ")
            )));
        }
        if !matches!(value, Value::Thunk { .. }) {
            return Err(load_err(format!(
                "hook '{event}': expected a block, got {}",
                value.type_name()
            )));
        }
        out.insert(event.clone(), value.clone());
    }
    Ok(out)
}

/// Each entry must be a map with required `key` (string) and `handler` (block) fields.
fn parse_keybindings(entries: &[Value]) -> Result<Vec<(String, Value)>, EvalSignal> {
    entries
        .iter()
        .map(|entry| {
            let Value::Map(map) = entry else {
                return Err(sig(format!(
                    "_plugin: keybinding entry: expected Map, got {}",
                    entry.type_name()
                )));
            };
            let key = get(map, "key")
                .map(|v| v.to_string())
                .ok_or_else(|| sig("_plugin: keybinding entry missing 'key' field"))?;
            let handler = get(map, "handler")
                .cloned()
                .ok_or_else(|| sig("_plugin: keybinding entry missing 'handler' field"))?;
            Ok(match handler {
                Value::Thunk { .. } => Some((key, handler)),
                _ => {
                    eprintln!(
                        "_plugin: warning: keybinding '{key}' handler is not a block, skipping"
                    );
                    None
                }
            })
        })
        .filter_map(Result::transpose)
        .collect()
}

fn parse_aliases(
    entries: &[(String, Value)],
    plugin_name: &str,
) -> Result<Vec<(String, AliasEntry)>, EvalSignal> {
    entries
        .iter()
        .map(|(name, value)| {
            if !matches!(value, Value::Thunk { .. }) {
                return Err(load_err(format!(
                    "alias '{name}': expected a block, got {}",
                    value.type_name()
                )));
            }
            Ok((
                name.clone(),
                AliasEntry::from_plugin(value.clone(), plugin_name),
            ))
        })
        .collect()
}
