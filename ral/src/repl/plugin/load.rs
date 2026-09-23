//! Plugin loading and unloading, engine-side: the `load-plugin` and
//! `unload-plugin` doors.
//!
//! A load resolves a plugin file under `~/.config/ral/plugins/` or
//! `RAL_PATH`, evaluates it under its own registered source, applies
//! options, and validates the result as a manifest; then commits the
//! plugin's hooks and aliases, and tells the host the manifest's first-order
//! part.  The host may refuse it, and any failure past the commit rolls the
//! plugin's whole namespace back, so a rejected load leaves the session
//! untouched.  Unloading is the exact inverse.

use ral_core::serial::datum::Datum as _;
use ral_core::source::Span;
use ral_core::typecheck::builtins::scheme;
use ral_core::types::{
    Break, BuiltinBody, BuiltinEntry, DefaultPolicy, Error, HookName, HookSig, Map, Mooring,
    PluginEntry, Settled,
};
use ral_core::{Shell, Value, diagnostic};
use std::borrow::Cow;

use super::super::enquiry::{Enquiry, PluginNote};
use super::load_err;
use super::manifest::{self, ManifestHandlers};

fn position(shell: &Shell, name: &str) -> Option<usize> {
    shell.repl().plugins.iter().position(|p| p.name == name)
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "a static builtin body's signature"
)]
fn load_door(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    // No options: `load-plugin` takes a name alone, so a plugin loaded
    // through it stands on its own defaults.
    if let Err(Break::Error(e)) = load_plugin(&args[0].to_string(), &Map::new(), mooring, shell) {
        diagnostic::cmd_error("load-plugin", &e.message);
    }
    Ok(Value::Unit)
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "a static builtin body's signature"
)]
fn unload_door(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    if let Err(e) = unload_plugin(&args[0].to_string(), mooring, shell) {
        diagnostic::cmd_error("unload-plugin", &e.message);
    }
    Ok(Value::Unit)
}

static DOORS_ARR: [BuiltinEntry; 2] = [
    BuiltinEntry::new(
        Cow::Borrowed("load-plugin"),
        scheme::string_to_unit,
        "load-plugin <name>  — load a REPL plugin by name or path.",
        BuiltinBody::Static(load_door),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("unload-plugin"),
        scheme::string_to_unit,
        "unload-plugin <name>  — unload a previously loaded REPL plugin.",
        BuiltinBody::Static(unload_door),
    ),
];
pub(crate) static DOORS: &[BuiltinEntry] = &DOORS_ARR;

/// Load a plugin by name (or path) with its options map — empty for a
/// plugin configured by nothing but its own defaults.
pub(crate) fn load_plugin(
    name_or_path: &str,
    options: &Map,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<()> {
    check_not_loaded(name_or_path, shell)?;

    let path = resolve_plugin_path(name_or_path, shell.env_overrides())?;
    let rp = shell.resolve(&path);
    shell.check_fs_read(&rp)?;
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:plugin-read] reads plugin source for loading; not turn-time model I/O"
    )]
    let source = std::fs::read_to_string(&path).map_err(|e| load_err(format!("{path}: {e}")))?;
    let source = ral_core::source::normalize_source_text(source);
    // Core's module loader owns cycle detection, the recursion guard, and the
    // source registration; the plugin policy adds only the fresh frame —
    // top-level helper bindings are discarded, since the manifest is the
    // file's *return value*, not its bindings.
    let value = shell.in_fresh_scope(|shell| {
        ral_core::builtins::modules::evaluate_source(
            mooring,
            shell,
            &source,
            &path,
            Some(ral_core::typecheck::contract::declared(
                ral_core::typecheck::Form::Manifest,
            )),
        )
    })?;
    let module = instantiate(value, options, name_or_path, mooring, shell)?;
    check_is_manifest(&module, name_or_path)?;

    let (manifest, handlers) = manifest::parse(&module)?;
    let name = manifest.name.clone();
    check_not_loaded(&name, shell)?;
    check_no_binding_conflicts(&handlers.aliases, &name, shell)?;

    // Past this point any failure — the host's refusal included — rolls the
    // plugin's whole namespace back, so nothing dispatchable survives it.
    let committed = register_plugin_hooks(&name, &handlers, shell)
        .and_then(|()| install_bindings(&handlers.aliases, &name, shell))
        .and_then(|()| {
            shell.enquire(
                mooring,
                Enquiry::Plugin(PluginNote::Loaded(manifest)).encode(),
            )
        });
    let aliases: Vec<String> = handlers.aliases.into_iter().map(|(n, _)| n).collect();
    if let Err(e) = committed {
        shell.remove_plugin_hooks(&name);
        for alias in &aliases {
            shell.remove_alias(alias);
        }
        return Err(Break::Error(e));
    }
    shell.repl_mut().plugins.push(PluginEntry { name, aliases });
    Ok(())
}

/// Register a plugin's hook-event and keybinding handlers in the session
/// hook table, each keyed under the plugin's namespace so
/// [`Shell::remove_plugin_hooks`] can drop them all at unload (or roll back
/// a failed load).  A buffer-change hook runs aside: nothing it does flows
/// back into the session.
fn register_plugin_hooks(
    plugin_name: &str,
    handlers: &ManifestHandlers,
    shell: &mut Shell,
) -> Result<(), Error> {
    let origin = Span::synthetic();
    for (hook_event, handler) in &handlers.hooks {
        let (sig, policy) = match hook_event.as_str() {
            "buffer-change" => (
                HookSig::Hook {
                    kind: hook_event.clone(),
                },
                DefaultPolicy::denied().aside(),
            ),
            "prompt" => (
                HookSig::Hook {
                    kind: "prompt hook".into(),
                },
                DefaultPolicy::denied(),
            ),
            _ => (
                HookSig::Lifecycle {
                    kind: hook_event.clone(),
                },
                DefaultPolicy::denied(),
            ),
        };
        shell
            .register_hook(
                HookName::plugin(plugin_name.to_string(), hook_event.clone()),
                handler.clone(),
                sig,
                policy,
                origin,
            )
            .map_err(|e| load_err(format!("plugin '{plugin_name}': hook '{hook_event}': {e}")))?;
    }
    for (key, handler) in &handlers.keybindings {
        shell
            .register_hook(
                HookName::plugin(plugin_name.to_string(), format!("key:{key}")),
                handler.clone(),
                HookSig::Hook {
                    kind: "keybinding".into(),
                },
                DefaultPolicy::leased(),
                origin,
            )
            .map_err(|e| load_err(format!("plugin '{plugin_name}': keybinding '{key}': {e}")))?;
    }
    Ok(())
}

/// Unload a plugin by name, fully reversing its load once the host has let
/// it go: drops every hook and keybinding handler it registered and removes
/// its aliases.
pub(crate) fn unload_plugin(name: &str, mooring: &Mooring, shell: &mut Shell) -> Result<(), Error> {
    let idx =
        position(shell, name).ok_or_else(|| load_err(format!("plugin '{name}' is not loaded")))?;
    shell.enquire(
        mooring,
        Enquiry::Plugin(PluginNote::Unloaded(name.to_string())).encode(),
    )?;
    let PluginEntry { name, aliases } = shell.repl_mut().plugins.remove(idx);
    shell.remove_plugin_hooks(&name);
    for alias in &aliases {
        shell.remove_alias(alias);
    }
    Ok(())
}

fn check_not_loaded(name: &str, shell: &Shell) -> Result<(), Error> {
    if position(shell, name).is_some() {
        return Err(load_err(format!("plugin '{name}' is already loaded")));
    }
    Ok(())
}

/// Reject the load if any alias name is already installed by another
/// plugin / rc-config / interactive `alias`.  Atomic: checked before
/// any insertion happens.  A lexical or native binding under the same
/// name is not a conflict — the alias installs, live only under `^name`.
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
    }
    Ok(())
}

fn install_bindings(
    bindings: &[(String, Value)],
    plugin_name: &str,
    shell: &mut Shell,
) -> Result<(), Error> {
    for (name, value) in bindings {
        shell
            .install_alias(name.clone(), value.clone())
            .map_err(|e| match e {
                Break::Error(err) => err.context(format!("plugin '{plugin_name}' alias '{name}'")),
                Break::Escape(_) => load_err(format!(
                    "plugin '{plugin_name}' alias '{name}' installation escaped"
                )),
            })?;
    }
    Ok(())
}

/// Apply the options map to a parameterised plugin block, nested under the
/// load's own mooring, to yield its manifest.  If the plugin is already a
/// manifest map, a non-empty options map is a load-time error; an empty one
/// is fine.
fn instantiate(
    val: Value,
    options: &Map,
    name: &str,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    match val {
        Value::Thunk(ref c) if c.comp.arrow().is_some() => {
            ral_core::builtins::apply(&val, vec![Value::Map(options.clone())], mooring, shell)
        }
        _ if !options.is_empty() => Err(Break::Error(load_err(format!(
            "plugin '{name}' takes no configuration; \
             its entry in the rc's plugins map wants the empty options map `[:]`"
        )))),
        val => Ok(val),
    }
}

/// Error if `val` is still a thunk after instantiation.  A `Lambda` means the
/// plugin's one options parameter was not supplied; a `Block` means the plugin
/// returned a block instead of a map.
fn check_is_manifest(val: &Value, name: &str) -> Result<(), Error> {
    match val {
        Value::Thunk(c) if c.comp.arrow().is_some() => Err(load_err(format!(
            "plugin '{name}' expects its options map but none was applied; \
             this is an internal error in load-plugin"
        ))),
        Value::Thunk(_) => Err(load_err(format!(
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
fn resolve_plugin_path(
    name_or_path: &str,
    env_overrides: &ral_core::types::EnvVars,
) -> Result<String, Error> {
    let plugin_file = format!("{name_or_path}.ral");
    let config_candidate =
        ral_core::path::config::xdg_config_subpath("ral/plugins").map(|dir| dir.join(&plugin_file));
    let ral_path_candidates = ral_core::path::ral_path::entries(env_overrides)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks_only(hooks: Vec<(String, Value)>) -> ManifestHandlers {
        ManifestHandlers {
            hooks,
            keybindings: Vec::new(),
            aliases: Vec::new(),
        }
    }

    /// A lifecycle handler taking the one event-record parameter registers.
    #[test]
    fn unary_lifecycle_handler_registers() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        let h = crate::repl::eval(&mut shell, "{ |_ev| return () }");
        register_plugin_hooks("p", &hooks_only(vec![("post-exec".into(), h)]), &mut shell)
            .expect("a unary lifecycle handler registers");
    }

    /// A two-parameter lifecycle handler is rejected at load, with an error
    /// naming the hook and the arity mismatch.
    #[test]
    fn two_parameter_lifecycle_handler_is_rejected() {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        let h = crate::repl::eval(&mut shell, "{ |_src _status| return () }");
        let err =
            register_plugin_hooks("p", &hooks_only(vec![("post-exec".into(), h)]), &mut shell)
                .expect_err("a two-parameter lifecycle handler must be rejected");
        assert!(
            err.message.contains("post-exec")
                && err.message.contains("1 parameter")
                && err.message.contains("got 2"),
            "error should name the hook and the arity mismatch, got: {}",
            err.message
        );
    }
}
