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

use ral_core::first_order::datum::Datum as _;
use ral_core::path::{Judged, Located, Reading};
use ral_core::source::{SourceFile, SourceOrigin};
use ral_core::typecheck::builtins::scheme;
use ral_core::types::{
    Break, BuiltinBody, BuiltinEntry, DefaultPolicy, Error, FrameHandle, HookName, HookSig, Map,
    Mooring, PluginEntry, Settled,
};
use ral_core::{Shell, Value};
use std::borrow::Cow;

use super::super::enquiry::{Enquiry, PluginNote};
use super::load_err;
use super::manifest::{self, HookEvent, ManifestHandlers};

fn load_door(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    // No options: `load-plugin` takes a name alone, so a plugin loaded
    // through it stands on its own defaults.
    let name = args[0].as_str("load-plugin")?;
    load_plugin(name, &Map::new(), mooring, shell)
        .map(|()| Value::Unit)
        .map_err(|b| match b {
            Break::Error(e) => e.context(format!("plugin '{name}'")).into(),
            escape @ Break::Escape(_) => escape,
        })
}

fn unload_door(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let name = args[0].as_str("unload-plugin")?;
    unload_plugin(name, mooring, shell)
        .map(|()| Value::Unit)
        .map_err(|b| match b {
            Break::Error(e) => e.context(format!("plugin '{name}'")).into(),
            escape @ Break::Escape(_) => escape,
        })
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
    let located = resolve_plugin_path(name_or_path, shell)?;
    let file = SourceFile::new(located.real().as_path().to_path_buf()).map_err(load_err)?;
    let name = ral_core::path::shown(file.path());
    let source = located
        .read()
        .and_then(std::io::read_to_string)
        .map_err(|e| load_err(format!("{name}: {e}")))?;
    let source = ral_core::source::normalize_source_text(source);
    // Core's module loader owns cycle detection, the recursion guard, and the
    // source registration; the plugin policy adds only the fresh frame —
    // top-level helper bindings are discarded, since the manifest is the
    // file's *return value*, not its bindings.
    let origin = SourceOrigin::File(file);
    let value = shell.in_fresh_scope(|shell| {
        ral_core::load::evaluate_source(
            mooring,
            shell,
            &source,
            &origin,
            Some(ral_core::typecheck::contract::declared(
                ral_core::typecheck::Form::Manifest,
            )),
        )
    })?;
    let module = instantiate(value, options, mooring, shell)?;
    check_is_manifest(&module)?;

    let (manifest, handlers) = manifest::parse(&module)?;
    let mut entry = PluginEntry {
        name: manifest.name.clone(),
        aliases: Vec::new(),
    };
    if shell.plugin_loaded(&entry.name).map_err(load_err)? {
        return Err(load_err("already loaded").into());
    }
    check_no_binding_conflicts(&handlers.aliases, shell)?;

    // Past this point any failure — the host's refusal included — rolls the
    // plugin's whole namespace back, so nothing dispatchable survives it.
    let committed = register_plugin_hooks(&entry.name, &handlers, shell)
        .map_err(Break::from)
        .and_then(|()| {
            install_bindings(&handlers.aliases, &mut entry.aliases, shell).map_err(Break::from)
        })
        .and_then(|()| {
            shell.enquire(
                mooring,
                Enquiry::Plugin(PluginNote::Loaded(manifest)).encode(),
            )
        });
    if let Err(e) = committed {
        shell.remove_plugin_hooks(&entry.name);
        for handle in entry.aliases {
            shell.remove_alias_frame(handle);
        }
        return Err(e);
    }
    Ok(shell.commit_plugin(entry).map_err(load_err)?)
}

/// Register a plugin's hook-event and keybinding handlers in the session
/// hook table, each keyed under the plugin's namespace so
/// [`Shell::remove_plugin_hooks`] can drop them all at unload (or roll back
/// a failed load).  A buffer-change or complete hook runs aside: nothing it
/// does flows back into the session.
fn register_plugin_hooks(
    plugin_name: &str,
    handlers: &ManifestHandlers,
    shell: &mut Shell,
) -> Result<(), Error> {
    for (event, handler) in &handlers.hooks {
        let name = <&str>::from(*event);
        let (kind, policy) = match event {
            HookEvent::BufferChange | HookEvent::Complete => {
                (name, DefaultPolicy::denied().aside())
            }
            HookEvent::Prompt => ("prompt hook", DefaultPolicy::denied_capture()),
            HookEvent::PreExec | HookEvent::PostExec | HookEvent::Chpwd => {
                (name, DefaultPolicy::denied())
            }
        };
        shell
            .register_hook(
                HookName::plugin(plugin_name.to_string(), name),
                handler.clone(),
                HookSig::Hook { kind: kind.into() },
                policy,
            )
            .map_err(|e| load_err(format!("hook '{name}': {e}")))?;
    }
    for (idx, (key, handler)) in handlers.keybindings.iter().enumerate() {
        shell
            .register_hook(
                HookName::plugin(plugin_name.to_string(), format!("key#{idx}")),
                handler.clone(),
                HookSig::Hook {
                    kind: "keybinding".into(),
                },
                DefaultPolicy::leased(),
            )
            .map_err(|e| load_err(format!("keybinding '{key}': {e}")))?;
    }
    Ok(())
}

/// Unload a plugin by name, fully reversing its load once the host has let
/// it go: drops every hook and keybinding handler it registered and removes
/// its aliases.
pub(crate) fn unload_plugin(name: &str, mooring: &Mooring, shell: &mut Shell) -> Settled<()> {
    if !shell.plugin_loaded(name).map_err(load_err)? {
        return Err(load_err("not loaded").into());
    }
    shell.enquire(
        mooring,
        Enquiry::Plugin(PluginNote::Unloaded(name.to_string())).encode(),
    )?;
    for handle in shell.take_plugin(name).into_iter().flat_map(|p| p.aliases) {
        shell.remove_alias_frame(handle);
    }
    shell.remove_plugin_hooks(name);
    Ok(())
}

/// Reject the load if any alias name is already installed by another
/// plugin / rc-config / interactive `alias`.  Atomic: checked before
/// any insertion happens.  A lexical or native binding under the same
/// name is not a conflict — the alias installs, live only under `^name`.
fn check_no_binding_conflicts(bindings: &[(String, Value)], shell: &Shell) -> Result<(), Error> {
    match bindings.iter().find(|(name, _)| shell.has_alias(name)) {
        Some((name, _)) => Err(load_err(format!(
            "alias '{name}' conflicts with an existing alias"
        ))),
        None => Ok(()),
    }
}

fn install_bindings(
    bindings: &[(String, Value)],
    handles: &mut Vec<FrameHandle>,
    shell: &mut Shell,
) -> Result<(), Error> {
    for (name, value) in bindings {
        handles.push(
            shell
                .install_alias(name.clone(), value.clone())
                .map_err(|e| match e {
                    Break::Error(err) => err.context(format!("alias '{name}'")),
                    Break::Escape(_) => load_err(format!("alias '{name}' installation escaped")),
                })?,
        );
    }
    Ok(())
}

/// Apply the options map to a parameterised plugin block, nested under the
/// load's own mooring, to yield its manifest.  If the plugin is already a
/// manifest map, a non-empty options map is a load-time error; an empty one
/// is fine.
fn instantiate(val: Value, options: &Map, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    match val {
        Value::Thunk(ref c) if c.comp().arrow().is_some() => {
            ral_core::evaluator::apply(&val, vec![Value::Map(options.clone())], mooring, shell)
        }
        _ if !options.is_empty() => Err(Break::Error(load_err(
            "it takes no configuration; \
             its entry in the rc's plugins map wants the empty options map `[:]`",
        ))),
        val => Ok(val),
    }
}

/// Error if `val` is still a thunk after instantiation.  A `Lambda` means the
/// plugin's one options parameter was not supplied; a `Block` means the plugin
/// returned a block instead of a map.
fn check_is_manifest(val: &Value) -> Result<(), Error> {
    match val {
        Value::Thunk(c) if c.comp().arrow().is_some() => Err(load_err(
            "it expects its options map but none was applied; \
             this is an internal error in load-plugin",
        )),
        Value::Thunk(_) => Err(load_err(
            "it returned a Block as its manifest; \
             expected a Map (e.g. [name: '...', hooks: [...], keybindings: [...]])",
        )),
        _ => Ok(()),
    }
}

/// Locate a plugin name or path: the first of these that is a file and the
/// grant lets be read, held.  A file the grant refuses is passed over, as
/// `execvp` does, and the first refusal is the error if none is admitted.
/// Searches:
/// 1. `<config>/ral/plugins/<name>.ral`
/// 2. Each directory in `RAL_PATH`: `$dir/<name>.ral`
/// 3. The literal path, then `<name>.ral`
///
/// # Errors
/// The first candidate's refusal, where none was admitted; else `not found`.
fn resolve_plugin_path(name_or_path: &str, shell: &mut Shell) -> Settled<Located<Judged<Reading>>> {
    let plugin_file = format!("{name_or_path}.ral");
    let config_candidate = ral_core::path::app_dir(ral_core::path::XdgKind::Config, "ral")
        .map(|dir| dir.join("plugins").join(&plugin_file));
    let ral_path_candidates = ral_core::path::ral_path_entries(shell.env_overrides())
        .into_iter()
        .map(|dir| dir.join(&plugin_file));
    // The directories above are the host's, so their candidates are taken as
    // they are.  Final fallbacks: the identifier verbatim, then with `.ral`;
    // these are written in ral, so the live cwd and `~` apply, as to any
    // other written path.
    let written = [name_or_path, &plugin_file]
        .into_iter()
        .map(|cand| shell.resolve(cand));
    let candidates: Vec<_> = config_candidate
        .into_iter()
        .chain(ral_path_candidates)
        .map(|cand| shell.anchor(&cand))
        .chain(written)
        .collect();
    let mut first_denial = None;
    for cand in &candidates {
        let Ok(located) = ral_core::path::walk(cand, ral_core::path::LeafMode::Follow) else {
            continue;
        };
        if located.trace().kind() != Some(ral_core::path::Kind::File) {
            continue;
        }
        match shell.judge::<Reading>(located) {
            Ok(found) => return Ok(found),
            Err(denial) => {
                first_denial.get_or_insert(denial);
            }
        }
    }
    Err(first_denial.unwrap_or_else(|| {
        load_err("not found; is it in <config>/ral/plugins or RAL_PATH?").into()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks_only(hooks: Vec<(HookEvent, Value)>) -> ManifestHandlers {
        ManifestHandlers {
            hooks,
            keybindings: Vec::new(),
            aliases: Vec::new(),
        }
    }

    /// A lifecycle handler taking the one event-record parameter registers.
    #[test]
    fn unary_lifecycle_handler_registers() {
        let mut shell = ral_core::test_helper::core_shell();
        let h = crate::repl::eval(&mut shell, "{ |_ev| return () }");
        register_plugin_hooks("p", &hooks_only(vec![(HookEvent::PostExec, h)]), &mut shell)
            .expect("a unary lifecycle handler registers");
    }

    /// Two guarded bindings on one key in one plugin both register.
    #[test]
    fn two_bindings_on_one_key_both_register() {
        let mut shell = ral_core::test_helper::core_shell();
        let h = crate::repl::eval(&mut shell, "{ |_ev| return () }");
        register_plugin_hooks(
            "p",
            &ManifestHandlers {
                keybindings: vec![("tab".into(), h.clone()), ("tab".into(), h)],
                ..hooks_only(Vec::new())
            },
            &mut shell,
        )
        .expect("two bindings on one key must not collide");
    }

    /// A two-parameter lifecycle handler is rejected at load, with an error
    /// naming the hook and the arity mismatch.
    #[test]
    fn two_parameter_lifecycle_handler_is_rejected() {
        let mut shell = ral_core::test_helper::core_shell();
        let h = crate::repl::eval(&mut shell, "{ |_src _status| return () }");
        let err =
            register_plugin_hooks("p", &hooks_only(vec![(HookEvent::PostExec, h)]), &mut shell)
                .expect_err("a two-parameter lifecycle handler must be rejected");
        assert!(
            err.message.contains("post-exec")
                && err.message.contains("1 parameter")
                && err.message.contains("got 2"),
            "error should name the hook and the arity mismatch, got: {}",
            err.message
        );
    }

    /// The candidate search takes the first candidate the grant lets be read:
    /// one it refuses is passed over for the next, and where every one is
    /// refused the plugin is not found.
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "[test] fixtures build the candidates the search is made over"
    )]
    fn the_candidate_search_passes_over_what_the_grant_refuses() {
        use ral_core::capability::{Capabilities, FsPolicy};
        use ral_core::path::{FrozenPath, utf8};
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::write(root.join("p"), "bare").unwrap();
        std::fs::write(root.join("p.ral"), "suffixed").unwrap();
        let name = utf8(&root.join("p")).unwrap().to_owned();
        let frozen = |path: &std::path::Path| FrozenPath::from_surface(utf8(path).unwrap());
        let found = |denied: &[&str]| {
            let layer = ral_core::test_helper::layer(Capabilities {
                fs: Some(FsPolicy {
                    read_prefixes: vec![frozen(&root)],
                    deny_paths: denied.iter().map(|n| frozen(&root.join(n))).collect(),
                    ..FsPolicy::default()
                }),
                ..Capabilities::default()
            });
            let mut shell = ral_core::test_helper::core_shell();
            shell
                .with_layer(layer, |sh| resolve_plugin_path(&name, sh))
                .map(|located| located.real().as_path().to_path_buf())
        };
        assert_eq!(found(&["p.ral"]).unwrap(), root.join("p"));
        assert_eq!(found(&["p"]).unwrap(), root.join("p.ral"));
        let refused = found(&["p", "p.ral"]).unwrap_err().to_string();
        assert!(!refused.contains("not found"), "{refused}");
        assert!(
            refused.contains(utf8(&root.join("p")).unwrap()),
            "{refused}"
        );
        let absent = ral_core::test_helper::core_shell()
            .with_layer(
                ral_core::test_helper::layer(Capabilities::default()),
                |sh| resolve_plugin_path(&format!("{name}-missing"), sh),
            )
            .map(|located| located.real().as_path().to_path_buf());
        assert!(absent.unwrap_err().to_string().contains("not found"));
    }
}

/// A plugin bench: an engine with a `record` sink and plugins whose hooks
/// register as in production.
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "[test] test scaffolding")]
pub(in crate::repl) mod harness {
    use super::*;
    use crate::repl::host::ReplHost;
    use crate::repl::plugin::manifest::{LoadedPlugin, Manifest};
    use crate::repl::plugin::{PluginRuntime, lock};
    use ral_core::carrier::IdentityTransport;
    use ral_core::ty::{Scheme, Ty};
    use ral_core::typecheck::Unifier;
    use ral_core::typecheck::builtins::{fun, mk_scheme, pure, thunk};
    use std::sync::{Arc, Mutex};

    /// The sink's type: an argv in, `Unit` out — the base-frame convention,
    /// since the sink takes whatever a hook body hands it, however much of it.
    fn sink_scheme(_u: &mut Unifier) -> Scheme {
        mk_scheme(&[], &[], thunk(fun(Ty::argv(), pure(Ty::Unit))))
    }

    /// A test-only sink base frame: `record` appends its argument values into
    /// the shared vector, so a hook handler's projections out of the event
    /// record become observable.
    fn sink_builtin(sink: Arc<Mutex<Vec<Value>>>) -> BuiltinEntry {
        BuiltinEntry::base_frame(
            Cow::Borrowed("record"),
            sink_scheme,
            "record: test sink appending its arguments.",
            BuiltinBody::Captured(Arc::new(move |args, _mooring, _shell| {
                sink.lock().unwrap().extend(args.iter().cloned());
                Ok(Value::Unit)
            })),
        )
    }

    /// An engine carrying the sink builtin and the plugins `hooks` name, each
    /// handler compiled from its source and registered as a load would, the
    /// host told of the plugins in first-appearance order, and the sink the
    /// builtin appends every call to.
    pub(in crate::repl) fn dressed(
        hooks: &[(&str, HookEvent, &str)], // (plugin, event, handler source)
    ) -> (IdentityTransport, Arc<ReplHost>, Arc<Mutex<Vec<Value>>>) {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let recorder = sink_builtin(sink.clone());
        let owned: Vec<_> = hooks
            .iter()
            .map(|&(p, e, s)| (p.to_owned(), e, s.to_owned()))
            .collect();
        let mut runtime = crate::repl::prelude_runtime();
        runtime.install_captured_builtins(&vec![recorder].into());
        let t = crate::repl::engine(runtime, move |shell| {
            for (plugin, event, src) in &owned {
                let handlers = ManifestHandlers {
                    hooks: vec![(*event, crate::repl::eval(shell, src))],
                    keybindings: Vec::new(),
                    aliases: Vec::new(),
                };
                register_plugin_hooks(plugin, &handlers, shell).expect("register");
            }
        });
        let mut manifests: Vec<Manifest> = Vec::new();
        for &(plugin, event, _) in hooks {
            match manifests.iter_mut().find(|m| m.name == plugin) {
                Some(m) => m.hooks.push(event),
                None => manifests.push(Manifest {
                    name: plugin.into(),
                    hooks: vec![event],
                    keybindings: Vec::new(),
                }),
            }
        }
        let runtime = Arc::new(Mutex::new(PluginRuntime::default()));
        lock(&runtime).plugins = manifests
            .into_iter()
            .map(|m| LoadedPlugin::admit(m).expect("admit"))
            .collect();
        (t, ReplHost::new(runtime), sink)
    }
}
