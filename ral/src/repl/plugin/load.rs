//! Plugin loading and unloading, engine-side: the `load-plugin` and
//! `unload-plugin` builtins.
//!
//! A load resolves a plugin file under `~/.config/ral/plugins/` or
//! `RAL_PATH`, evaluates it under its own registered source, applies
//! options, and validates the result as a manifest; then commits the
//! plugin's hooks and aliases, and tells the host the manifest's first-order
//! part.  The host may refuse it, and any failure past the commit rolls the
//! plugin's whole namespace back, so a rejected load leaves the session
//! untouched.  Unloading is the exact inverse.

use ral_core::data::datum::Datum as _;
use ral_core::error::Error;
use ral_core::fact::{
    EscapedLoad, InvalidCause, InvalidFailure, IoOperation, IoSubject, PathFailure, PluginFailure,
    PluginFailureAliasInstallationEscaped, PluginFailureAlreadyLoaded,
    PluginFailureFactoryReturnedBlock, PluginFailureNotFound, PluginFailureNotLoaded,
    PluginFailureOptionsWithoutFactory, Reason, StateFailure,
};
use ral_core::path::{Judged, Located, Reading};
use ral_core::shell::runtime::{
    Break, BuiltinBody, BuiltinEntry, DefaultPolicy, Escape, FrameHandle, HookName, HookSig, Map,
    Mooring, PluginEntry, Settled,
};
use ral_core::source::{SourceFile, SourceOrigin};
use ral_core::typecheck::builtins::scheme;
use ral_core::{RegisterError, Shell, Value};
use std::borrow::Cow;

use super::super::enquiry::{Enquiry, PluginNote};
use super::manifest::{self, HookEvent, ManifestHandlers};

fn load_plugin_builtin(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    // No options: `load-plugin` takes a name alone, so a plugin loaded
    // through it stands on its own defaults.
    let name = args[0].as_str("load-plugin")?;
    load_plugin(name, &Map::new(), mooring, shell)
        .map(|()| Value::Unit)
        .map_err(|b| b.map_error(|e| e.with_context(format!("plugin '{name}'"))))
}

fn unload_plugin_builtin(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let name = args[0].as_str("unload-plugin")?;
    unload_plugin(name, mooring, shell)
        .map(|()| Value::Unit)
        .map_err(|b| b.map_error(|e| e.with_context(format!("plugin '{name}'"))))
}

static BUILTINS_ARR: [BuiltinEntry; 2] = [
    BuiltinEntry::new(
        Cow::Borrowed("load-plugin"),
        scheme::string_to_unit,
        "load-plugin <name>  — load a REPL plugin by name or path.",
        BuiltinBody::Static(load_plugin_builtin),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("unload-plugin"),
        scheme::string_to_unit,
        "unload-plugin <name>  — unload a previously loaded REPL plugin.",
        BuiltinBody::Static(unload_plugin_builtin),
    ),
];
pub(crate) static BUILTINS: &[BuiltinEntry] = &BUILTINS_ARR;

/// The plugin's source, read through the judged handle, which ends here:
/// nothing of the load after the read holds the file or its directory.
fn read_source(located: Located<Judged<Reading>>) -> Settled<(SourceFile, String)> {
    let file = SourceFile::new(located.real().as_path().to_path_buf()).map_err(|e| {
        Error::new(Reason::Invalid(Box::new(InvalidFailure::new(
            "load-plugin".into(),
            InvalidCause::Path(PathFailure::from(&e)),
        ))))
    })?;
    let read = located.read().and_then(std::io::read_to_string);
    drop(located);
    let source = read.map_err(|e| {
        Error::io(
            IoOperation::Read,
            IoSubject::Path(ral_core::path::shown(file.path())),
            &e,
        )
    })?;
    Ok((file, ral_core::source::normalize_source_text(source)))
}

/// Load a plugin by name (or path) with its options map — empty for a
/// plugin configured by nothing but its own defaults.
pub(crate) fn load_plugin(
    name_or_path: &str,
    options: &Map,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<()> {
    let (file, source) = read_source(resolve_plugin_path(name_or_path, shell)?)?;
    // Core's module loader owns cycle detection, the recursion guard, and the
    // source registration; the plugin policy adds only the fresh frame —
    // top-level helper bindings are discarded, since the manifest is the
    // file's *return value*, not its bindings.
    let origin = SourceOrigin::File(file);
    let value = shell.in_fresh_scope(|shell| {
        ral_core::shell::load::evaluate_source(
            mooring,
            shell,
            &source,
            &origin,
            Some(ral_core::typecheck::contract::declared(
                ral_core::typecheck::Form::Manifest,
            )),
        )
    })?;
    let module = instantiate(name_or_path, value, options, mooring, shell)?;
    check_is_manifest(name_or_path, &module)?;

    let (manifest, handlers) = manifest::parse(&module)?;
    let mut entry = PluginEntry {
        name: manifest.name.clone(),
        aliases: Vec::new(),
    };
    if shell
        .plugin_loaded(&entry.name)
        .map_err(RegisterError::into_error)?
    {
        return Err(
            super::refusal(PluginFailure::AlreadyLoaded(PluginFailureAlreadyLoaded {
                name: entry.name.clone(),
            }))
            .into(),
        );
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
    Ok(shell
        .commit_plugin(entry)
        .map_err(RegisterError::into_error)?)
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
            .map_err(|e| e.into_error().with_context(format!("hook '{name}'")))?;
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
            .map_err(|e| e.into_error().with_context(format!("keybinding '{key}'")))?;
    }
    Ok(())
}

/// Unload a plugin by name, fully reversing its load once the host has let
/// it go: drops every hook and keybinding handler it registered and removes
/// its aliases.
pub(crate) fn unload_plugin(name: &str, mooring: &Mooring, shell: &mut Shell) -> Settled<()> {
    if !shell
        .plugin_loaded(name)
        .map_err(RegisterError::into_error)?
    {
        return Err(
            super::refusal(PluginFailure::NotLoaded(PluginFailureNotLoaded {
                name: name.into(),
            }))
            .into(),
        );
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
        Some((name, _)) => Err(Error::new(Reason::State(Box::new(
            StateFailure::AliasConflict(name.clone()),
        )))),
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
                    Break::Error(err) => err.with_context(format!("alias '{name}'")),
                    Break::Escape(escape) => {
                        super::refusal(PluginFailure::AliasInstallationEscaped(
                            PluginFailureAliasInstallationEscaped {
                                alias: name.clone(),
                                cause: match escape {
                                    Escape::Exit(code) => EscapedLoad::Exit(code),
                                    Escape::Stopped(stop) => EscapedLoad::Stopped(stop.cause),
                                },
                            },
                        ))
                    }
                })?,
        );
    }
    Ok(())
}

/// Apply the options map to a factory, nested under the load's own mooring,
/// to yield its manifest.  If the plugin is already a manifest map, a
/// non-empty options map is a load-time error; an empty one is fine.
fn instantiate(
    plugin: &str,
    val: Value,
    options: &Map,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    match val {
        Value::Thunk(_) => ral_core::shell::evaluator::apply(
            &val,
            vec![Value::Map(options.clone())],
            mooring,
            shell,
        ),
        _ if !options.is_empty() => Err(super::refusal(PluginFailure::OptionsWithoutFactory(
            PluginFailureOptionsWithoutFactory {
                plugin: plugin.into(),
            },
        ))
        .into()),
        val => Ok(val),
    }
}

/// Error if `val` is still a block after instantiation: a factory that made
/// one, which only a return of unknown type can slip past the checker with.
fn check_is_manifest(plugin: &str, val: &Value) -> Result<(), Error> {
    match val {
        Value::Thunk(_) => Err(super::refusal(PluginFailure::FactoryReturnedBlock(
            PluginFailureFactoryReturnedBlock {
                plugin: plugin.into(),
            },
        ))),
        _ => Ok(()),
    }
}

/// Locate a plugin name or path: the first of these that is a file and the
/// grant lets be read, held.  Searches:
/// 1. `<config>/ral/plugins/<name>.ral`
/// 2. Each directory in `RAL_PATH`: `$dir/<name>.ral`
/// 3. The literal path, then `<name>.ral`
///
/// # Errors
/// The first refused candidate, where none was admitted; else `not found`.
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
    shell.locate_first(candidates)?.ok_or_else(|| {
        super::refusal(PluginFailure::NotFound(PluginFailureNotFound {
            requested: name_or_path.into(),
        }))
        .into()
    })
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
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
        use ral_core::fact::{
            HookArity, HookRef, HookRegistrationFailure, HookSignature, PluginHookRef,
        };
        let mut shell = ral_core::test_helper::core_shell();
        let h = crate::repl::eval(&mut shell, "{ |_src _status| return () }");
        let err =
            register_plugin_hooks("p", &hooks_only(vec![(HookEvent::PostExec, h)]), &mut shell)
                .unwrap_err();
        assert_eq!(
            err.reason,
            Reason::State(Box::new(StateFailure::HookRegistration(
                HookRegistrationFailure::Arity(HookArity {
                    name: HookRef::Plugin(PluginHookRef {
                        plugin: "p".into(),
                        hook: "post-exec".into()
                    }),
                    expected: 1_usize.into(),
                    actual: 2_usize.into(),
                    signature: HookSignature::Hook("post-exec".into()),
                })
            )))
        );
        assert_eq!(err.context(), Some("hook 'post-exec'"));
        assert_eq!(err.hints(), []);
        assert!(err.describe().contains("post-exec"));
        assert!(err.plain().contains("1 parameter"));
    }

    /// The candidate search takes the first candidate the grant lets be read:
    /// one it refuses is passed over for the next, and where every one is
    /// refused the plugin is not found.
    #[test]
    fn the_candidate_search_passes_over_what_the_grant_refuses() {
        use ral_core::authority::policy::{Capabilities, FsPolicy};
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
            refused.contains(&ral_core::path::shown(root.join("p"))),
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

    /// Every shipped plugin loads, alone, on the REPL's surface: `ral --check`
    /// lacks the `_ed-*` builtins they call.
    #[test]
    fn every_shipped_plugin_loads() {
        use crate::repl::host::ReplHost;
        use ral_core::protocol::{Ending, Ran, Report};

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins");
        let mut plugins: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "ral"))
            .collect();
        plugins.sort();
        assert!(!plugins.is_empty(), "no plugins under {}", dir.display());

        let failures: Vec<String> = plugins
            .iter()
            .filter_map(|plugin| {
                let mut runtime = crate::repl::prelude_runtime();
                runtime.install_builtins(crate::repl::ED_BUILTINS);
                runtime.install_builtins(crate::repl::PLUGIN_LOAD_BUILTINS);
                let t = crate::repl::in_temp::engine(runtime, |_| {});
                let line = format!("load-plugin '{}'", ral_core::path::shown(plugin));
                let run = crate::repl::exec::line_run(&line);
                match ReplHost::new(std::sync::Arc::default())
                    .dispatch(&t, run, None)
                    .0
                {
                    Ok(Report::Ran(Ran {
                        ending: Ending::Ok(_),
                        ..
                    })) => None,
                    Ok(Report::Ran(Ran {
                        ending: Ending::Err(_),
                        rendered,
                        ..
                    })) => Some(rendered),
                    other => Some(format!("{}: {other:?}", plugin.display())),
                }
            })
            .collect();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
    #[test]
    fn plugin_factory_refusals_and_alias_conflicts_keep_their_subjects() {
        let mut shell = ral_core::test_helper::core_shell();
        let options = Map::from_iter([("key".into(), Value::Int(1))]);
        let Err(Break::Error(error)) =
            instantiate("p", Value::Unit, &options, &Mooring::adrift(), &mut shell)
        else {
            panic!("expected non-factory refusal")
        };
        assert_eq!(
            error.reason,
            super::super::refusal(PluginFailure::OptionsWithoutFactory(
                PluginFailureOptionsWithoutFactory { plugin: "p".into() }
            ))
            .reason
        );
        assert_eq!(error.hints(), []);
        assert!(error.context().is_none());
        assert!(
            error
                .reason
                .advice()
                .iter()
                .any(|hint| hint.contains("[:]"))
        );
        let block = crate::repl::eval(&mut shell, "{ return () }");
        let error = check_is_manifest("p", &block).unwrap_err();
        assert_eq!(
            error.reason,
            super::super::refusal(PluginFailure::FactoryReturnedBlock(
                PluginFailureFactoryReturnedBlock { plugin: "p".into() }
            ))
            .reason
        );
        assert!(
            error
                .reason
                .advice()
                .iter()
                .any(|hint| hint.contains("name:"))
        );
        let block = crate::repl::eval(&mut shell, "{ |_args| return () }");
        shell.install_alias("ll".into(), block.clone()).unwrap();
        let error = check_no_binding_conflicts(&[("ll".into(), block)], &shell).unwrap_err();
        assert_eq!(
            error.reason,
            Reason::State(Box::new(StateFailure::AliasConflict("ll".into())))
        );
    }

    #[test]
    fn rejected_plugin_source_keeps_a_summary_and_the_rich_rejection() {
        use ral_core::fact::{CompileFailure, CompilePhase};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.ral");
        std::fs::write(&path, "let [").unwrap();
        let mut shell = ral_core::test_helper::core_shell();
        let Err(Break::Error(error)) = load_plugin(
            &ral_core::path::shown(&path),
            &Map::new(),
            &Mooring::adrift(),
            &mut shell,
        ) else {
            panic!("expected compile rejection")
        };
        let Reason::Compile(summary) = &error.reason else {
            panic!("expected compile summary")
        };
        assert_eq!(
            summary.as_ref(),
            &CompileFailure {
                source: ral_core::path::shown(std::fs::canonicalize(&path).unwrap()),
                phase: CompilePhase::Parse
            }
        );
        assert!(error.rejection().is_some());
        assert!(!error.describe().contains("L0002"));
        assert!(
            error
                .render(shell.sources(), None, shell.exit_hints())
                .contains("L0002")
        );
    }

    #[test]
    fn a_host_refusal_rolls_back_hooks_and_aliases_without_reclassifying() {
        struct Refuse(ral_core::error::Error);
        impl ral_core::shell::runtime::EnquiryDesk for Refuse {
            fn enquire(
                &self,
                _: ral_core::data::FOValue,
                _: &ral_core::process::Scope,
            ) -> Settled<ral_core::data::FOValue> {
                Err(self.0.clone().into())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin.ral");
        std::fs::write(
            &path,
            "return [name: p, hooks: [post-exec: { |_ev| return () }], aliases: [xx: { |_args| echo hi }]]",
        )
        .unwrap();
        let original =
            super::super::refusal(PluginFailure::AlreadyLoaded(PluginFailureAlreadyLoaded {
                name: "p".into(),
            }))
            .with_context("host registry")
            .with_hints(["first", "second"]);
        let mut runtime = ral_core::test_helper::core_runtime();
        runtime.install_builtins(BUILTINS);
        let mut shell = Shell::root(runtime.into(), ral_core::test_helper::process_cwd());
        let run = ral_core::shell::run::RunRequest {
            desk: Some(std::sync::Arc::new(Refuse(original.clone()))),
            ..ral_core::shell::run::RunRequest::from(crate::repl::exec::line_run(&format!(
                "load-plugin '{}'",
                ral_core::path::shown(&path)
            )))
        };
        let ral_core::shell::run::RunReport::Ran { ending, .. } = shell.run(run) else {
            panic!("load must run")
        };
        let Err(Break::Error(error)) = ending.into_settled() else {
            panic!("host must refuse")
        };
        assert_eq!(error.reason, original.reason);
        assert_eq!(error.hints(), original.hints());
        assert_eq!(
            error.context(),
            Some(format!("plugin '{}': host registry", ral_core::path::shown(&path)).as_str())
        );
        assert!(error.rejection().is_none());
        assert!(!shell.plugin_loaded("p").unwrap());
        assert!(!shell.has_alias("xx"));
        assert!(!shell.has_hook(&HookName::plugin("p", "post-exec")));
    }
}

/// A plugin bench: an engine with a `record` sink and plugins whose hooks
/// register as in production.
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
pub(in crate::repl) mod harness {
    use super::*;
    use crate::repl::host::ReplHost;
    use crate::repl::plugin::manifest::{LoadedPlugin, Manifest};
    use crate::repl::plugin::{PluginRuntime, lock};
    use ral_core::carrier::IdentityTransport;
    use ral_core::ty::{Binders, Ty};
    use ral_core::typecheck::builtins::{fun, pure, thunk};
    use std::sync::{Arc, Mutex};

    /// The sink's type: an argv in, `Unit` out — the base-frame convention,
    /// since the sink takes whatever a hook body hands it, however much of it.
    fn sink_scheme(_: &mut Binders) -> Ty {
        thunk(fun(Ty::argv(), pure(Ty::unit())))
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
        let t = crate::repl::in_temp::engine(runtime, move |shell| {
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
