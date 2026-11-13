//! Startup plugin loading, engine-side.
//!
//! A load resolves one plugin file, checks and evaluates it under Σ, applies
//! options, and validates the result as a manifest; then commits the
//! plugin's hooks and handlers, and tells the host the manifest's first-order
//! part.  The host may refuse it, and any failure past the commit rolls the
//! plugin's whole namespace back, so a rejected load leaves the session
//! untouched. Committed plugins live for the session.

use ral_core::data::datum::Datum as _;
use ral_core::error::Error;
use ral_core::fact::{
    EscapedLoad, InvalidCause, InvalidFailure, IoKind, IoLeaf, IoOperation, IoSubject, PathFailure,
    PluginFailure, PluginFailureAlreadyLoaded, PluginFailureHandlerInstallationEscaped,
    PluginFailureNotFound, PluginFailureOptionsWithoutFactory, Reason, StateFailure,
};
use ral_core::path::{EntryName, Judged, Kind, Located, Reading};
use ral_core::shell::runtime::{
    Break, DefaultPolicy, Escape, FrameHandle, HookName, HookSig, Map, Mooring, Settled,
};
use ral_core::source::{SourceFile, SourceOrigin};
use ral_core::{RegisterError, Shell, ValClos, View};

use super::super::enquiry::{Enquiry, PluginNote};
use super::manifest::{self, HookEvent, ManifestHandlers};

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
            IoSubject::Path(ral_core::text::quote_native(file.path().as_os_str())),
            &e,
        )
    })?;
    Ok((file, ral_core::source::normalize_source_text(source)))
}

/// Load a plugin by name (or path) with its options map — empty for a
/// plugin configured by nothing but its own defaults.
pub(crate) fn load_plugin(
    name_or_path: &str,
    options: Map,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<()> {
    let (file, source) = read_source(resolve_plugin_path(name_or_path, shell)?)?;
    let origin = SourceOrigin::File(file);
    let value = ral_core::shell::load::evaluate_closed(
        mooring,
        shell,
        &source,
        &origin,
        Some(ral_core::typecheck::table::declared(
            ral_core::typecheck::Form::Manifest,
        )),
    )?;
    let value = instantiate(name_or_path, value, options, mooring, shell)?;
    let View::Map(module) = value.view() else {
        unreachable!("a manifest ends in its fields written out (SPEC §15.5)")
    };
    let (manifest, handlers) = manifest::parse(module)?;
    let name = manifest.name.clone();
    let mut installed = Vec::new();
    if shell
        .plugin_loaded(&name)
        .map_err(RegisterError::into_error)?
    {
        return Err(
            super::refusal(PluginFailure::AlreadyLoaded(PluginFailureAlreadyLoaded {
                name: name.clone(),
            }))
            .into(),
        );
    }
    check_no_binding_conflicts(&handlers.handlers, shell)?;

    // Past this point any failure — the host's refusal included — rolls the
    // plugin's whole namespace back, so nothing dispatchable survives it.
    let committed = register_plugin_hooks(&name, &handlers, shell)
        .map_err(Break::from)
        .and_then(|()| {
            install_bindings(&handlers.handlers, &mut installed, shell).map_err(Break::from)
        })
        .and_then(|()| {
            shell.enquire(
                mooring,
                Enquiry::Plugin(PluginNote::Loaded(manifest)).encode(),
            )
        });
    if let Err(e) = committed {
        shell.remove_plugin_hooks(&name);
        for handle in installed {
            shell.remove_handler_frame(handle);
        }
        return Err(e);
    }
    Ok(shell
        .commit_plugin(name)
        .map_err(RegisterError::into_error)?)
}

/// Register a plugin's hook-event and keybinding handlers in the session
/// hook table, each keyed under the plugin's namespace so
/// [`Shell::remove_plugin_hooks`] can roll back a failed load. A buffer-change
/// or complete hook runs aside: nothing it does flows back into the session.
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

/// Reject the load if any handler name is already installed by another
/// plugin, rc config, or host. Atomic: checked before
/// any insertion happens.  A lexical or native binding under the same
/// name is not a conflict, but the binding shadows the handler.
fn check_no_binding_conflicts(bindings: &[(String, ValClos)], shell: &Shell) -> Result<(), Error> {
    match bindings.iter().find(|(name, _)| shell.has_handler(name)) {
        Some((name, _)) => Err(Error::new(Reason::State(Box::new(
            StateFailure::HandlerConflict(name.clone()),
        )))),
        None => Ok(()),
    }
}

fn install_bindings(
    bindings: &[(String, ValClos)],
    handles: &mut Vec<FrameHandle>,
    shell: &mut Shell,
) -> Result<(), Error> {
    for (name, value) in bindings {
        handles.push(
            shell
                .install_handler(name.clone(), value.clone())
                .map_err(|e| match e {
                    Break::Error(err) => err.with_context(format!("handler '{name}'")),
                    Break::Escape(escape) => {
                        super::refusal(PluginFailure::HandlerInstallationEscaped(
                            PluginFailureHandlerInstallationEscaped {
                                handler: name.clone(),
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
    val: ValClos,
    options: Map,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    if !matches!(val.view(), View::Map(_)) {
        return ral_core::shell::evaluator::apply(&val, vec![options.into()], mooring, shell);
    }
    if options.is_empty() {
        Ok(val)
    } else {
        Err(super::refusal(PluginFailure::OptionsWithoutFactory(
            PluginFailureOptionsWithoutFactory {
                plugin: plugin.into(),
            },
        ))
        .into())
    }
}

/// Locate the one file a plugin name or written path names.
///
/// # Errors
/// The grant's refusal, a walk failure, a nonregular plugin candidate,
/// or `not found` naming that one path.
fn resolve_plugin_path(name_or_path: &str, shell: &mut Shell) -> Settled<Located<Judged<Reading>>> {
    let candidate = if name_or_path.contains(std::path::is_separator) {
        shell.resolve(name_or_path)
    } else {
        let base = shell.resolve("xdg:config/ral/plugins");
        let name = EntryName::parse(std::ffi::OsStr::new(&format!("{name_or_path}.ral"))).map_err(
            |error| {
                Error::io(
                    IoOperation::Read,
                    IoSubject::Path(ral_core::text::quote_native(error.name())),
                    &error.into(),
                )
            },
        )?;
        shell.anchor(&base.as_path().join(name.as_os_str()))
    };
    let missing = || {
        Break::from(super::refusal(PluginFailure::NotFound(
            PluginFailureNotFound {
                path: ral_core::text::quote_native(candidate.as_path().as_os_str()),
            },
        )))
    };
    match shell.locate::<Reading>(&candidate) {
        Ok(located) => match located.stat().map(|stat| stat.kind) {
            Some(Kind::File) => Ok(located),
            Some(Kind::Dir) => Err(Error::io(
                IoOperation::Read,
                IoSubject::Path(ral_core::text::quote_native(
                    candidate.as_path().as_os_str(),
                )),
                &std::io::ErrorKind::IsADirectory.into(),
            )
            .into()),
            Some(Kind::Symlink | Kind::Other) => Err(super::refusal(PluginFailure::NotFile(
                ral_core::text::quote_native(candidate.as_path().as_os_str()),
            ))
            .into()),
            None => Err(missing()),
        },
        Err(Break::Error(error))
            if matches!(&error.reason, Reason::Io(failure)
                if matches!(failure.cause.leaf.as_ref(), IoLeaf::Os(cause)
                    if cause.kind == IoKind::NotFound)) =>
        {
            Err(missing())
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use ral_core::builtin::{BuiltinBody, BuiltinEntry};

    fn loader(path: String) -> BuiltinEntry {
        BuiltinEntry::new(
            "_test-load".into(),
            |_| {
                ral_core::typecheck::builtins::thunk(ral_core::typecheck::builtins::pure(
                    ral_core::ty::Ty::unit(),
                ))
            },
            "_test-load: load a plugin under the dispatch's mooring.",
            BuiltinBody::Captured(std::sync::Arc::new(move |_, mooring, shell| {
                load_plugin(&path, Map::new(), mooring, shell).map(|()| ValClos::UNIT)
            })),
        )
    }

    fn hooks_only(hooks: Vec<(HookEvent, ValClos)>) -> ManifestHandlers {
        ManifestHandlers {
            hooks,
            keybindings: Vec::new(),
            handlers: Vec::new(),
        }
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

    /// A non-function handler is refused by the install check, naming the handler.
    #[test]
    fn non_function_handler_is_refused_at_install() {
        let mut shell = ral_core::test_helper::core_shell();
        let Err(error) = install_bindings(
            &[("foo".into(), ValClos::int(3))],
            &mut Vec::new(),
            &mut shell,
        ) else {
            panic!("an Int handler must be refused")
        };
        assert!(
            error.describe().contains("handler: field \"foo\""),
            "{}",
            error.describe()
        );
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
    }

    /// A written path has no suffixed or denial fallback.
    #[test]
    fn a_written_path_names_only_itself() {
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
        let refused = found(&["p"]).unwrap_err().to_string();
        assert!(!refused.contains("not found"), "{refused}");
        assert!(
            refused.contains(&ral_core::path::shown(root.join("p"))),
            "{refused}"
        );
        let missing = root.join("missing");
        std::fs::write(missing.with_extension("ral"), "no implicit extension").unwrap();
        let mut shell = ral_core::test_helper::core_shell();
        let error = resolve_plugin_path(utf8(&missing).unwrap(), &mut shell)
            .err()
            .expect("no implicit extension");
        let Break::Error(error) = error else {
            panic!("an absent plugin is a typed refusal")
        };
        assert_eq!(
            error.reason,
            super::super::refusal(PluginFailure::NotFound(PluginFailureNotFound {
                path: format!("{missing:?}"),
            }))
            .reason,
        );
        let missing_parent = root.join("absent").join("p.ral");
        let error = resolve_plugin_path(utf8(&missing_parent).unwrap(), &mut shell)
            .err()
            .expect("the parent does not exist");
        let Break::Error(error) = error else {
            panic!("an absent directory is a typed refusal")
        };
        assert_eq!(
            error.reason,
            super::super::refusal(PluginFailure::NotFound(PluginFailureNotFound {
                path: format!("{missing_parent:?}"),
            }))
            .reason,
        );
    }

    /// A separatorless name names only `<config>/ral/plugins/NAME.ral`.
    #[test]
    fn a_name_names_one_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let plugins = root.join("ral/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(root.join("p.ral"), "not the named plugin").unwrap();
        let mut shell = ral_core::test_helper::core_shell();
        shell
            .set_env_var("XDG_CONFIG_HOME", ral_core::path::utf8(&root).unwrap())
            .unwrap();
        shell
            .set_env_var("RAL_PATH", ral_core::path::utf8(&root).unwrap())
            .unwrap();
        for name in ["p", "~", "a.b", "plugin name"] {
            let candidate = plugins.join(format!("{name}.ral"));
            std::fs::write(&candidate, "named").unwrap();
            let found = resolve_plugin_path(name, &mut shell).unwrap();
            assert_eq!(found.real().as_path(), candidate);
        }
        assert!(resolve_plugin_path("p.ral", &mut shell).is_err());
        std::fs::write(root.join("missing.ral"), "not searched").unwrap();
        assert!(resolve_plugin_path("missing", &mut shell).is_err());
    }

    #[test]
    fn a_directory_is_not_a_missing_plugin_under_any_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let plugins = root.join("ral/plugins");
        let candidate = plugins.join("p.ral");
        std::fs::create_dir_all(&candidate).unwrap();
        std::fs::write(plugins.join("p.ral.ral"), "not a fallback").unwrap();
        let mut shell = ral_core::test_helper::core_shell();
        for variable in ["HOME", "XDG_CONFIG_HOME"] {
            shell
                .set_env_var(variable, ral_core::path::utf8(&root).unwrap())
                .unwrap();
        }
        for spelling in [
            ral_core::path::utf8(&candidate).unwrap(),
            "~/ral/plugins/p.ral",
            "xdg:config/ral/plugins/p.ral",
            "p",
        ] {
            let error = resolve_plugin_path(spelling, &mut shell)
                .err()
                .expect("a directory is not a plugin file");
            let Break::Error(error) = error else {
                panic!("a directory must produce a typed error: {spelling}")
            };
            let Reason::Io(failure) = &error.reason else {
                panic!("a directory must not become not-found: {error}")
            };
            assert_eq!(failure.operation, IoOperation::Read);
            assert_eq!(
                failure.subject,
                IoSubject::Path(ral_core::text::quote_native(candidate.as_os_str()))
            );
            assert!(matches!(failure.cause.leaf.as_ref(), IoLeaf::Os(cause)
                if cause.kind == IoKind::IsADirectory));
            assert!(error.to_string().contains("is a directory"), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_is_not_a_missing_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let plugins = root.join("ral/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let candidate = plugins.join("p.ral");
        let _socket = std::os::unix::net::UnixListener::bind(&candidate).unwrap();
        std::fs::write(plugins.join("p.ral.ral"), "not a fallback").unwrap();
        let mut shell = ral_core::test_helper::core_shell();
        shell
            .set_env_var("XDG_CONFIG_HOME", ral_core::path::utf8(&root).unwrap())
            .unwrap();
        let path = ral_core::text::quote_native(candidate.as_os_str());
        let failure = PluginFailure::NotFile(path.clone());
        assert_eq!(
            failure.describe(),
            format!("plugin path {path} is not a regular file")
        );
        assert_eq!(
            failure.clone().encode(),
            ral_core::data::FOValue::Variant {
                label: "not-file".into(),
                payload: Some(Box::new(path.encode())),
            }
        );
        for spelling in [ral_core::path::utf8(&candidate).unwrap(), "p"] {
            let error = resolve_plugin_path(spelling, &mut shell)
                .err()
                .expect("a socket is not a plugin file");
            let Break::Error(error) = error else {
                panic!("a socket must produce a typed refusal: {spelling}")
            };
            assert_eq!(error.reason, super::super::refusal(failure.clone()).reason);
        }
    }

    /// Every shipped plugin loads, alone, on the REPL's editor surface.
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
                runtime
                    .install_captured_builtins(&vec![loader(ral_core::path::shown(plugin))].into());
                let t = crate::repl::in_temp::engine(runtime, |_| {});
                match ReplHost::new(std::sync::Arc::default())
                    .dispatch(&t, crate::repl::exec::line_run("_test-load"), None)
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
    fn file_and_factory_failures_and_exits_propagate_without_installation() {
        for factory in [false, true] {
            for (stop, exit) in [("fail plugin-stopped", false), ("exit 7", true)] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("plugin.ral");
                let body = format!(
                    "{stop}\nreturn [name: p, hooks: [post-exec: {{ |_ev| return () }}], keybindings: [[key: ctrl-t, handler: {{ |_ev| return () }}]], handlers: [:, xx: {{ |_args| echo hi }}]]"
                );
                let source = if factory {
                    format!("return {{ |options|\n{body}\n}}")
                } else {
                    body
                };
                std::fs::write(&path, &source).unwrap();
                let mut runtime = ral_core::test_helper::core_runtime();
                runtime
                    .install_captured_builtins(&vec![loader(ral_core::path::shown(&path))].into());
                let mut shell = Shell::root(runtime.into(), ral_core::test_helper::process_cwd());
                let ral_core::shell::run::RunReport::Ran { ending, .. } =
                    shell.run(crate::repl::exec::line_run("_test-load"))
                else {
                    panic!("load must run: {source}")
                };
                match ending.into_settled() {
                    Err(Break::Escape(Escape::Exit(7))) if exit => {}
                    Err(Break::Error(error)) if !exit => {
                        assert_eq!(error.reason, Reason::raised("plugin-stopped"));
                        assert_eq!(error.hints(), []);
                        assert!(error.context().is_none());
                    }
                    other => panic!("load must propagate {stop}, got {other:?}: {source}"),
                }
                assert!(!shell.plugin_loaded("p").unwrap());
                assert!(!shell.has_handler("xx"));
                assert!(!shell.has_hook(&HookName::plugin("p", "post-exec")));
                assert!(!shell.has_hook(&HookName::plugin("p", "key#0")));
            }
        }
    }

    #[test]
    fn a_plugin_cannot_read_a_session_binding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin.ral");
        std::fs::write(&path, "return [name: $rc-only]").unwrap();
        let mut runtime = ral_core::test_helper::core_runtime();
        runtime.install_captured_builtins(&vec![loader(ral_core::path::shown(&path))].into());
        let mut shell = Shell::root(runtime.into(), ral_core::test_helper::process_cwd());
        let ral_core::shell::run::RunReport::Ran { ending, .. } =
            shell.run(crate::repl::exec::line_run("let rc-only = 'p'; _test-load"))
        else {
            panic!("the loader call must check")
        };
        let Err(Break::Error(error)) = ending.into_settled() else {
            panic!("the plugin must be refused")
        };
        assert!(matches!(error.reason, Reason::Compile(_)));
        let rendered = error.rejection().expect("a static plugin refusal").render();
        assert!(rendered.contains("rc-only"), "{rendered}");
        assert!(shell.scope_lookup("rc-only").is_some());
        assert!(!shell.plugin_loaded("p").unwrap());
    }

    #[test]
    fn a_host_refusal_rolls_back_hooks_and_handlers_without_reclassifying() {
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
            "let private-helper = 'private'\nreturn [name: p, hooks: [post-exec: { |_ev| return () }], handlers: [:, xx: { |_args| echo hi }]]",
        )
        .unwrap();
        let original =
            super::super::refusal(PluginFailure::AlreadyLoaded(PluginFailureAlreadyLoaded {
                name: "p".into(),
            }))
            .with_context("host registry")
            .with_hints(["first", "second"]);
        let mut runtime = ral_core::test_helper::core_runtime();
        runtime.install_captured_builtins(&vec![loader(ral_core::path::shown(&path))].into());
        let mut shell = Shell::root(runtime.into(), ral_core::test_helper::process_cwd());
        let request = ral_core::shell::run::RunRequest {
            desk: Some(std::sync::Arc::new(Refuse(original.clone()))),
            ..ral_core::shell::run::RunRequest::from(crate::repl::exec::line_run("_test-load"))
        };
        let ral_core::shell::run::RunReport::Ran { ending, .. } = shell.run(request) else {
            panic!("load must run")
        };
        let Err(Break::Error(error)) = ending.into_settled() else {
            panic!("host must refuse")
        };
        assert_eq!(error.reason, original.reason);
        assert_eq!(error.hints(), original.hints());
        assert_eq!(error.context(), original.context());
        assert!(error.rejection().is_none());
        assert!(shell.scope_lookup("private-helper").is_none());
        assert!(!shell.plugin_loaded("p").unwrap());
        assert!(!shell.has_handler("xx"));
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
    use ral_core::builtin::{BuiltinBody, BuiltinEntry};
    use ral_core::carrier::IdentityTransport;
    use std::borrow::Cow;
    use std::sync::{Arc, Mutex};

    /// A test-only sink base frame: `record` appends its argument values into
    /// the shared vector, so a hook handler's projections out of the event
    /// record become observable.
    fn sink_builtin(sink: Arc<Mutex<Vec<ValClos>>>) -> BuiltinEntry {
        BuiltinEntry::base_frame(
            Cow::Borrowed("record"),
            "record: test sink appending its arguments.",
            BuiltinBody::Captured(Arc::new(move |args, _mooring, _shell| {
                sink.lock().unwrap().extend(args.iter().cloned());
                Ok(ValClos::UNIT)
            })),
        )
    }

    /// An engine carrying the sink builtin and the plugins `hooks` name, each
    /// handler compiled from its source and registered as a load would, the
    /// host told of the plugins in first-appearance order, and the sink the
    /// builtin appends every call to.
    pub(in crate::repl) fn dressed(
        hooks: &[(&str, HookEvent, &str)], // (plugin, event, handler source)
    ) -> (IdentityTransport, Arc<ReplHost>, Arc<Mutex<Vec<ValClos>>>) {
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
                    handlers: Vec::new(),
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
