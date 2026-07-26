//! Plugin loading and unloading.
//!
//! Resolves a plugin file path under `~/.config/ral/plugins/` or
//! `RAL_PATH`, evaluates the file under its own registered source, applies
//! options, validates the result as a manifest, then — once every
//! validation has passed — commits the plugin's hooks and alias bindings
//! and records it in [`PluginRuntime`].  Commit is deferred past the
//! validations so a rejected load leaves the session untouched, and any
//! partial failure rolls the plugin's whole hook namespace back.
//! Unloading is the exact inverse: it removes the plugin's hooks and
//! keybindings, undoes the env installation, and drops the record.

use ral_core::source::Span;
use ral_core::transport::Program;
use ral_core::types::{Break, DefaultPolicy, Error, HookName, HookSig, Mooring, Settled};
use ral_core::{RequestedTerminalAccess, RunReport, Shell, Value};
use std::sync::{Arc, Mutex};

use super::super::errfmt::plugin_warning;
use super::manifest::LoadedPlugin;
use super::{PluginRuntime, framed_run_request, load_err, lock, unload_err};

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
    mooring: &Mooring,
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
    // Core's module loader owns cycle detection, the recursion guard, and
    // the script-context swap; the plugin policy adds only the fresh frame —
    // top-level helper bindings are discarded, since the manifest is the
    // file's *return value*, not its bindings.
    let value = shell.in_fresh_scope(|shell| {
        ral_core::builtins::modules::evaluate_source(mooring, shell, &source, &path)
    })?;
    let module = instantiate(value, options, name_or_path, shell)?;
    check_is_manifest(&module, name_or_path)?;

    let (mut plugin, bindings) = LoadedPlugin::parse(&module)?;
    // The manifest value carries no source; retain the file text the handler
    // thunks were compiled from, so a fault inside one renders against it.
    plugin.source = std::sync::Arc::from(source.as_str());

    // All validations run before any hook or alias is committed, so a
    // rejected load leaves the session exactly as it found it.
    check_not_loaded(&plugin.name, runtime)?;
    check_no_binding_conflicts(&bindings, &plugin.name, shell)?;

    // Commit the hooks and alias bindings.  Any partial failure past this
    // point rolls back the whole plugin namespace, so nothing dispatchable
    // survives a failed load.
    if let Err(e) =
        register_plugin_hooks(&plugin, shell).and_then(|()| install_bindings(&bindings, shell))
    {
        shell.remove_plugin_hooks(&plugin.name);
        return Err(Break::Error(e));
    }

    let plugin_name = plugin.name.clone();
    let mut rt = lock(runtime);
    rt.plugins.push(plugin);
    rt.keybindings_changed();
    // Shadow lint: a dead binding (an earlier unguarded entry owns its
    // chord) can only be introduced by this load, and only among this
    // plugin's own entries — earlier plugins keep their precedence.
    let shadowed: Vec<String> = rt
        .router
        .dead_entries()
        .into_iter()
        .filter(|(dead, _)| dead.plugin == plugin_name)
        .map(|(dead, blocker)| {
            format!(
                "keybinding '{}' will never fire: an unguarded '{}' binding of plugin '{}' \
                 precedes it",
                dead.key, blocker.key, blocker.plugin
            )
        })
        .collect();
    drop(rt);
    for msg in shadowed {
        plugin_warning(&plugin_name, &msg);
    }
    Ok(())
}

/// Register a plugin's hook-event and keybinding handlers in the session
/// hook table, each keyed under the plugin's namespace so
/// [`Shell::remove_plugin_hooks`] can drop them all at unload (or roll back
/// a failed load).  Every handler fires as a `Program::Hook` through
/// `Shell::run`.
fn register_plugin_hooks(plugin: &LoadedPlugin, shell: &mut Shell) -> Result<(), Error> {
    let origin = Span::synthetic();
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
        shell
            .register_hook(
                HookName::plugin(plugin.name.clone(), hook_event.clone()),
                handler.clone(),
                sig,
                DefaultPolicy::denied(),
                origin,
            )
            .map_err(|e| {
                load_err(format!(
                    "plugin '{}': hook '{}': {e}",
                    plugin.name, hook_event
                ))
            })?;
    }
    for kb in &plugin.keybindings {
        shell
            .register_hook(
                HookName::plugin(plugin.name.clone(), format!("key:{}", kb.key)),
                kb.handler.clone(),
                HookSig::Hook {
                    kind: "keybinding".into(),
                },
                DefaultPolicy::leased(),
                origin,
            )
            .map_err(|e| {
                load_err(format!(
                    "plugin '{}': keybinding '{}': {e}",
                    plugin.name, kb.key
                ))
            })?;
    }
    Ok(())
}

/// Unload a plugin by name, fully reversing its load: drops every hook and
/// keybinding handler it registered, removes its env bindings (innermost
/// scope), and drops the runtime record.  rustyline keybindings are rebound
/// on the next readline iteration via the dirty flag.
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
    shell.remove_plugin_hooks(&plugin.name);
    for binding_name in &plugin.bindings {
        shell.remove_alias(binding_name);
    }
    lock(runtime).keybindings_changed();
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
        if shell.scope_lookup(name).is_some() || shell.lookup_builtin(name).is_some() {
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
            let origin = Span::synthetic();
            if let Err(e) = shell.register_hook(
                factory_name.clone(),
                val,
                HookSig::PluginFactory,
                DefaultPolicy::denied(),
                origin,
            ) {
                return Err(Break::Error(load_err(format!("plugin '{name}': {e}"))));
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
            let req = framed_run_request(
                "<plugin>",
                RequestedTerminalAccess::Denied,
                Program::Hook {
                    name: factory_name.clone(),
                    args: vec![fo_arg],
                },
            );
            let report = shell.run(req);
            // The factory is construction-time only: it built the manifest and
            // is never dispatched again, so it leaves the hook table now rather
            // than outliving the load.
            shell.unregister_hook(&factory_name);
            match report {
                RunReport::Ran { result, .. } => result,
                RunReport::Static { .. } => {
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
    ral_core::path::Resolver::shell_less()
        .resolve(&cand.to_string_lossy())
        .canonicalise_strict()
        .ok()
}
