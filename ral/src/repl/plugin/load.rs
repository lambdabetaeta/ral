//! Plugin loading and unloading.
//!
//! Resolves a plugin file path under `~/.config/ral/plugins/` or
//! `RAL_PATH`, evaluates the file under a `ScriptContextGuard`, applies
//! options, validates the result as a manifest, installs the alias
//! bindings into `env`, and records the plugin in [`PluginRuntime`].
//! Unloading reverses the env installation and drops the record.

use ral_core::source::Span;
use ral_core::types::{Break, DefaultPolicy, Error, HookName, HookSig, Settled};
use ral_core::{RequestedTerminalAccess, Shell, TurnReport, Value};
use std::sync::{Arc, Mutex};

use super::manifest::LoadedPlugin;
use super::{PluginRuntime, framed_turn_request, lock};

fn load_err(msg: impl std::fmt::Display) -> Error {
    Error::new(format!("load-plugin: {msg}"), 1)
}

fn unload_err(msg: impl std::fmt::Display) -> Error {
    Error::new(format!("unload-plugin: {msg}"), 1)
}

/// Load a plugin by name (or path) with an optional options map.
///
/// 1. Resolve the path (search `~/.config/ral/plugins/`, `RAL_PATH`, literal).
/// 2. Evaluate the plugin file with host authority.
/// 3. Apply `options` to the manifest block.
/// 4. Parse the result as a [`LoadedPlugin`] plus alias bindings.
/// 5. Insert the alias thunks into `env` (innermost scope).
/// 6. Push the plugin record into the runtime, set the keybinding-dirty flag.
pub(crate) fn load_plugin(
    name_or_path: &str,
    options: Option<&Value>,
    shell: &mut Shell,
    runtime: &Arc<Mutex<PluginRuntime>>,
) -> Settled<()> {
    if let Some(opts) = options
        && !matches!(opts, Value::Map(_))
    {
        return Err(load_err(format!(
            "plugin '{name_or_path}': options must be a Map, got {}",
            opts.type_name()
        ))
        .into());
    }

    check_not_loaded(name_or_path, runtime)?;

    let path = resolve_plugin_path(name_or_path)?;
    let rp = shell.resolve(&path);
    shell.check_fs_read(&rp)?;
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:plugin-read] reads plugin source for loading; not turn-time model I/O"
    )]
    let source = std::fs::read_to_string(&path).map_err(|e| load_err(format!("{path}: {e}")))?;
    let source = ral_core::source::normalize_source_text(source);
    let value = eval_plugin_file(&path, &source, shell)?;
    let module = instantiate(value, options, name_or_path, shell)?;
    check_is_manifest(&module, name_or_path)?;

    let (mut plugin, bindings) = LoadedPlugin::parse(&module)?;
    // The manifest value carries no source; retain the file text the handler
    // thunks were compiled from, so a fault inside one renders against it.
    plugin.source = std::sync::Arc::from(source.as_str());

    // Register hook and keybinding handlers in the hook table.
    // Each fires via `run_hook` rather than the old `run_value_turn`.
    let origin = Span::new(ral_core::source::FileId(0), 0, 0);
    for (hook_event, handler) in &plugin.hooks {
        let sig = match hook_event.as_str() {
            "buffer-change" | "keybinding" => HookSig::Hook {
                kind: hook_event.clone(),
            },
            "prompt" => HookSig::Hook {
                kind: "prompt hook".into(),
            },
            _ => HookSig::Lifecycle {
                kind: hook_event.clone(),
            },
        };
        if let Err(e) = shell.register_hook(
            HookName::plugin(plugin.name.clone(), hook_event.clone()),
            handler.clone(),
            sig,
            DefaultPolicy::denied(),
            origin,
        ) {
            return Err(Break::Error(load_err(format!(
                "plugin '{}': hook '{}': {}",
                plugin.name, hook_event, e
            ))));
        }
    }
    for (key_notation, handler) in &plugin.keybindings {
        if let Err(e) = shell.register_hook(
            HookName::plugin(plugin.name.clone(), format!("key:{key_notation}")),
            handler.clone(),
            HookSig::Hook {
                kind: "keybinding".into(),
            },
            DefaultPolicy::leased(),
            origin,
        ) {
            return Err(Break::Error(load_err(format!(
                "plugin '{}': keybinding '{}': {}",
                plugin.name, key_notation, e
            ))));
        }
    }

    check_not_loaded(&plugin.name, runtime)?;
    check_no_binding_conflicts(&bindings, &plugin.name, shell)?;
    install_bindings(&bindings, shell)?;

    let mut rt = lock(runtime);
    rt.plugins.push(plugin);
    rt.keybindings_dirty = true;
    Ok(())
}

/// Unload a plugin by name.  Removes its env bindings (innermost scope)
/// and drops the runtime record.  Keybindings are rebound on the next
/// readline iteration via the dirty flag.
pub(crate) fn unload_plugin(
    name: &str,
    shell: &mut Shell,
    runtime: &Arc<Mutex<PluginRuntime>>,
) -> Result<(), Error> {
    let mut rt = lock(runtime);
    let idx = rt
        .plugins
        .iter()
        .position(|p| p.name == name)
        .ok_or_else(|| unload_err(format!("plugin '{name}' is not loaded")))?;
    let plugin = rt.plugins.remove(idx);
    drop(rt);
    for binding_name in &plugin.bindings {
        shell.remove_alias(binding_name);
    }
    lock(runtime).keybindings_dirty = true;
    Ok(())
}

fn check_not_loaded(name: &str, runtime: &Arc<Mutex<PluginRuntime>>) -> Result<(), Error> {
    if lock(runtime).plugins.iter().any(|p| p.name == name) {
        return Err(load_err(format!("plugin '{name}' is already loaded")));
    }
    Ok(())
}

/// Reject the load if any alias name is already installed by another
/// plugin / rc-config / interactive `alias`.  Atomic: checked before
/// any insertion happens.
fn check_no_binding_conflicts(
    bindings: &[(String, Value)],
    plugin_name: &str,
    shell: &Shell,
) -> Result<(), Error> {
    for (name, _) in bindings {
        if shell.has_alias(name) {
            return Err(load_err(format!(
                "alias '{name}' from plugin '{plugin_name}' conflicts with an existing alias"
            )));
        }
        if shell.scope_lookup(name).is_some() || ral_core::builtins::is_builtin(name) {
            return Err(load_err(format!(
                "alias '{name}' from plugin '{plugin_name}' conflicts with a lexical or builtin binding"
            )));
        }
    }
    Ok(())
}

fn install_bindings(bindings: &[(String, Value)], shell: &mut Shell) -> Result<(), Error> {
    for (name, value) in bindings {
        shell
            .install_alias(name.clone(), value.clone())
            .map_err(|e| match e {
                Break::Error(err) => load_err(err.message),
                Break::Escape(_) => load_err("alias installation escaped"),
            })?;
    }
    Ok(())
}

/// Evaluate a plugin file (already read and canonicalized) in an isolated scope.
///
/// Routes through core's shared module loader
/// ([`evaluate_source`](ral_core::builtins::modules::evaluate_source)), which
/// owns the cycle-detection stack, the recursion-depth guard, the type-check
/// pass that writes the evaluator's mode wires, and the script-context swap
/// that keeps diagnostics pointing at the plugin file.  The plugin policy
/// adds only lexical-scope isolation: a plugin file's top-level helper
/// bindings live in a fresh frame and are discarded, since the manifest is
/// the file's *return value*, not its bindings.
fn eval_plugin_file(path: &str, source: &str, shell: &mut Shell) -> Settled<Value> {
    shell
        .in_fresh_scope(|shell| ral_core::builtins::modules::evaluate_source(shell, source, path))
        .map_err(tag_load_error)
}

/// Prefix a loader failure with `load-plugin:` while preserving its status,
/// location, and hint — the surface every plugin-load error carries.  Skips
/// the prefix when the message already begins with it.
fn tag_load_error(e: Break) -> Break {
    match e {
        Break::Error(mut err) => {
            if !err.message.starts_with("load-plugin:") {
                err.message = format!("load-plugin: {}", err.message);
            }
            Break::Error(err)
        }
        other => other,
    }
}

/// Apply the options map to a parameterised plugin block to yield its
/// manifest.  If the plugin is already a manifest map, a non-empty options
/// map is a load-time error; an absent or empty options map is fine.
fn instantiate(
    val: Value,
    options: Option<&Value>,
    name: &str,
    shell: &mut Shell,
) -> Settled<Value> {
    let empty = Value::Map(ral_core::Map::new());
    match val {
        val @ (Value::Lambda { .. } | Value::Block { .. }) => {
            let factory_name = HookName::plugin(name.to_string(), "factory");
            let origin = Span::new(ral_core::source::FileId(0), 0, 0);
            if let Err(e) = shell.register_hook(
                factory_name.clone(),
                val,
                HookSig::PluginFactory,
                DefaultPolicy::denied(),
                origin,
            ) {
                return Err(Break::Error(load_err(format!("plugin '{name}': {}", e))));
            }
            let arg = options.cloned().unwrap_or(empty);
            let fo_arg = match ral_core::serial::FOValue::try_from(&arg) {
                Ok(fo) => fo,
                Err(e) => {
                    return Err(Break::Error(load_err(format!(
                        "plugin '{name}' options are not first-order: {}",
                        e.message
                    ))));
                }
            };
            let req = framed_turn_request("<plugin>", RequestedTerminalAccess::Denied);
            let report = shell.run_hook(&factory_name, vec![fo_arg], req);
            match report {
                TurnReport::Ran { result, .. } => result,
                TurnReport::Static { .. } => {
                    unreachable!("a thunk plugin factory never compiles source")
                }
            }
        }
        _ if matches!(options, Some(Value::Map(e)) if !e.is_empty()) => {
            Err(Break::Error(load_err(format!(
                "plugin '{name}' takes no configuration; \
                 remove 'options:' from the rc entry"
            ))))
        }
        val => Ok(val),
    }
}

/// Error if `val` is still a thunk after instantiation.  A `Lambda` means the
/// plugin's one options parameter was not supplied; a `Block` means the plugin
/// returned a block instead of a map.
fn check_is_manifest(val: &Value, name: &str) -> Result<(), Error> {
    match val {
        Value::Lambda { .. } => Err(load_err(format!(
            "plugin '{name}' expects its options map but none was applied; \
             this is an internal error in load-plugin"
        ))),
        Value::Block { .. } => Err(load_err(format!(
            "plugin '{name}' returned a Block as its manifest; \
             expected a Map (e.g. [name: '...', hooks: [...], keybindings: [...]])"
        ))),
        _ => Ok(()),
    }
}

/// Resolve a plugin name or path to a canonical absolute path.  Searches:
/// 1. `<config>/ral/plugins/<name>.ral`
/// 2. Each directory in `RAL_PATH`: `$dir/<name>.ral`
/// 3. The literal path, then `<name>.ral`
fn resolve_plugin_path(name_or_path: &str) -> Result<String, Error> {
    let plugin_file = format!("{name_or_path}.ral");
    let config_candidate =
        ral_core::path::config::xdg_config_subpath("ral/plugins").map(|dir| dir.join(&plugin_file));
    let ral_path_candidates = ral_core::path::ral_path::entries()
        .into_iter()
        .map(|dir| dir.join(&plugin_file));
    // Final fallbacks: the user-supplied identifier verbatim, then
    // the same with `.ral` appended.  These are intentional
    // literal-path candidates (no cwd-anchoring, no sigil expansion),
    // so the `PathBuf::from` is the correct constructor here.
    #[allow(clippy::disallowed_methods)]
    let literal_candidates = [
        std::path::PathBuf::from(name_or_path),
        std::path::PathBuf::from(&plugin_file),
    ];
    config_candidate
        .into_iter()
        .chain(ral_path_candidates)
        .chain(literal_candidates)
        .find_map(|cand| canonicalise_candidate(&cand))
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| load_err(format!("plugin '{name_or_path}' not found")))
}

/// Strictly canonicalise one plugin candidate through a shell-less
/// resolver — the public door to canonicalisation.  `cand` is already
/// absolute (or literal cwd-relative), so the empty cwd anchors nothing
/// it should not; `None` when the candidate does not name a file.
fn canonicalise_candidate(cand: &std::path::Path) -> Option<std::path::PathBuf> {
    ral_core::path::Resolver {
        home: String::new(),
        cwd: None,
        mode: ral_core::path::CanonMode::Lenient,
    }
    .resolve(&cand.to_string_lossy())
    .canonicalise_strict()
    .ok()
}
