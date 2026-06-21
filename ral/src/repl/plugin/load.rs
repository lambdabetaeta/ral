//! Plugin loading and unloading.
//!
//! Resolves a plugin file path under `~/.config/ral/plugins/` or
//! `RAL_PATH`, evaluates the file under a `ScriptContextGuard`, applies
//! options, validates the result as a manifest, installs the alias
//! bindings into `env`, and records the plugin in [`PluginRuntime`].
//! Unloading reverses the env installation and drops the record.

use ral_core::builtins::modules::ScriptContextGuard;
use ral_core::types::{Break, Error, Settled};
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
        if shell.mobile.scope.get(name).is_some() || ral_core::builtins::is_builtin(name) {
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
fn eval_plugin_file(path: &str, source: &str, shell: &mut Shell) -> Settled<Value> {
    let path_owned = path.to_owned();
    if shell.mobile.context.modules.stack.contains(&path_owned) {
        return Err(Break::Error(load_err(format!(
            "circular dependency: {path}"
        ))));
    }
    // The inference pass writes the evaluator's mode wires, so a plugin
    // file passes through the checker before it runs.  A type error is
    // fatal and the file is reported and skipped.  The schemes seed the
    // check from the session scope the plugin loads into.
    let comp = std::sync::Arc::new(
        ral_core::compile_and_typecheck(source, shell.session_schemes())
            .into_comp_or_message()
            .map_err(|m| Break::Error(load_err(format!("{path}: {m}"))))?,
    );
    let mut ctx = ScriptContextGuard::enter(shell, &path_owned, source);
    ctx.shell_mut()
        .mobile
        .context
        .modules
        .stack
        .push(path_owned);
    ctx.shell_mut().mobile.scope.push_scope();
    // `evaluate` returns `Settled<Value>` directly.
    let result = ral_core::evaluator::evaluate(&comp, ctx.shell_mut());
    ctx.shell_mut().mobile.scope.pop_scope();
    ctx.shell_mut().mobile.context.modules.stack.pop();
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
) -> Settled<Value> {
    let empty = Value::Map(ral_core::Map::new());
    match val {
        val @ (Value::Lambda { .. } | Value::Block { .. }) => {
            let arg = options.cloned().unwrap_or(empty);
            // The plugin factory runs through the value turn door: a fresh
            // frame with the session's live streams and `Denied` terminal
            // authority — instantiating a plugin must not foreground a child.
            let req = framed_turn_request("<plugin>", RequestedTerminalAccess::Denied);
            match shell.run_value_turn(val, vec![arg], "", req) {
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
