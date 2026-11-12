//! A leased hook edits a private file and returns only a successful readback.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::io::{self, Write};

use ral_core::error::Error;
use ral_core::external_editor::{EditorCommand, admit_draft, after_cleanup, read_draft};
use ral_core::fact::{
    DraftOperation, EditorFailure, EditorFailureDraft, EditorFailureWorkspace,
    EditorWorkspaceFailure, EditorWorkspaceFailureExpectedRegularFile, EditorWorkspaceStep,
    HostFailure, InvalidCause, InvalidFailure, IoCause, IoLeaf, PathFailure, PathIoLeaf,
    RalFailure, Reason, StateFailure,
};
use ral_core::path::{Dir, EntryName, Kind, LeafMode, Reading, ScopedDir, Visibility, Writing};
use ral_core::shell::runtime::{
    Break, BuiltinBody, BuiltinEntry, DefaultPolicy, HookName, HookSig, Mooring, Settled,
};
use ral_core::text::quote_native;
use ral_core::{RegisterError, Shell, Value};

const NAME: &str = "_external-editor";

static ENTRIES: [BuiltinEntry; 1] = [BuiltinEntry::new(
    Cow::Borrowed(NAME),
    scheme,
    "_external-editor <text>: edit with VISUAL or EDITOR; return the edited text on success.",
    BuiltinBody::Static(edit),
)];
pub(crate) static BUILTINS: &[BuiltinEntry] = &ENTRIES;

fn scheme(_: &mut ral_core::ty::Binders) -> ral_core::ty::Ty {
    use ral_core::ty::Ty;
    use ral_core::typecheck::builtins::{fun, pure, thunk};
    thunk(fun(Ty::string(), pure(Ty::string())))
}

pub(crate) fn register(shell: &mut Shell) -> Result<(), Error> {
    let entry = shell.lookup_builtin(NAME).ok_or_else(|| {
        Error::new(Reason::State(Box::new(StateFailure::MissingBuiltin(
            NAME.into(),
        ))))
    })?;
    shell
        .register_hook(
            HookName::session("external-editor"),
            Value::Native(entry.into(), [].into()),
            HookSig::Hook {
                kind: "external editor".into(),
            },
            DefaultPolicy::leased(),
        )
        .map_err(RegisterError::into_error)
}

fn failure(cause: EditorFailure) -> Break {
    Error::new(Reason::Host(Box::new(HostFailure::Ral(
        RalFailure::Editor(cause),
    ))))
    .into()
}

fn workspace_io(step: EditorWorkspaceStep, path: Option<String>, source: &io::Error) -> Break {
    failure(EditorFailure::Workspace(EditorFailureWorkspace {
        step,
        path,
        cause: EditorWorkspaceFailure::Io(ral_core::io::failure(source)),
    }))
}

fn path_failure(source: &ral_core::path::NotUtf8) -> Break {
    Error::new(Reason::Invalid(Box::new(InvalidFailure::new(
        NAME.into(),
        InvalidCause::Path(PathFailure::from(source)),
    ))))
    .into()
}

/// Preserve the edit failure; report cleanup separately. An escape wins.
fn joined(shell: &mut Shell, edit: Break, removal: Break) -> Break {
    match (edit, removal) {
        (edit @ Break::Error(_), Break::Error(removal)) => {
            let _ = shell.write_stderr(format!("{removal}\n").as_bytes());
            edit
        }
        (escape @ Break::Escape(_), other) | (other, escape @ Break::Escape(_)) => {
            let _ = shell.write_stderr(format!("{other}\n").as_bytes());
            escape
        }
    }
}

struct Workspace {
    directory: Dir,
    cleanup: ScopedDir,
    file: EntryName,
}

impl Workspace {
    fn create(shell: &mut Shell, text: &str) -> Settled<Self> {
        let bytes = admit_draft(text).map_err(|e| {
            failure(EditorFailure::Draft(EditorFailureDraft {
                operation: DraftOperation::Write,
                cause: e.failure(),
            }))
        })?;
        let temp_path = ral_core::host::temp_dir();
        let temp = ral_core::path::utf8(&temp_path).map_err(|e| path_failure(&e))?;
        let parent = shell.locate_dir::<Writing>(&shell.resolve(temp))?;
        let cleanup = loop {
            let name = EntryName::fresh(".ral-editor-").map_err(|e| {
                workspace_io(
                    EditorWorkspaceStep::TemporaryDirectory,
                    Some(quote_native(temp_path.as_os_str())),
                    &io::Error::from(e),
                )
            })?;
            let target = shell.judge::<Writing>(parent.absent(name))?;
            match target.create_scoped_dir() {
                Ok(directory) => break directory,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(workspace_io(
                        EditorWorkspaceStep::CreateDirectory,
                        Some(quote_native(temp_path.as_os_str())),
                        &e,
                    ));
                }
            }
        };
        let workspace = Self {
            directory: cleanup.directory(),
            cleanup,
            file: EntryName::parse(OsStr::new("buffer.ral")).map_err(|e| {
                workspace_io(
                    EditorWorkspaceStep::BufferPath,
                    Some(quote_native(OsStr::new("buffer.ral"))),
                    &io::Error::from(e),
                )
            })?,
        };
        match workspace.write(shell, bytes) {
            Ok(()) => Ok(workspace),
            Err(error) => workspace.finish(shell, Err(error)),
        }
    }

    fn write(&self, shell: &mut Shell, bytes: &[u8]) -> Settled<()> {
        let target = self.directory.absent(self.file.clone());
        shell
            .judge::<Writing>(target)?
            .create_writable_file(Visibility::Secret)
            .map_err(|e| {
                workspace_io(
                    EditorWorkspaceStep::CreateBuffer,
                    Some(quote_native(
                        self.directory
                            .real()
                            .as_path()
                            .join(self.file.as_os_str())
                            .as_os_str(),
                    )),
                    &e,
                )
            })?
            .write_all(bytes)
            .map_err(|e| {
                workspace_io(
                    EditorWorkspaceStep::WriteBuffer,
                    Some(quote_native(
                        self.directory
                            .real()
                            .as_path()
                            .join(self.file.as_os_str())
                            .as_os_str(),
                    )),
                    &e,
                )
            })
    }

    /// Judge the held tree now, even when the editor or readback failed.
    /// Consumes the workspace: removal is attempted once.
    fn finish<T>(self, shell: &mut Shell, edited: Settled<T>) -> Settled<T> {
        let removed = shell
            .judge::<Writing>(self.cleanup.cleanup_target())
            .and_then(|target| {
                target.remove().map_err(|e| {
                    workspace_io(
                        EditorWorkspaceStep::RemoveWorkspace,
                        Some(quote_native(self.directory.real().as_path().as_os_str())),
                        &e,
                    )
                })
            });
        after_cleanup(edited, removed, |edit, removal| {
            joined(shell, edit, removal)
        })
    }

    fn path(&self) -> Settled<String> {
        let path = self.directory.real().as_path().join(self.file.as_os_str());
        ral_core::path::utf8(&path)
            .map(str::to_owned)
            .map_err(|e| path_failure(&e))
    }

    /// Run the editor on the buffer under `mooring`, then read it back.
    fn edit(
        &self,
        shell: &mut Shell,
        program: &str,
        args: &[String],
        mooring: &Mooring,
    ) -> Settled<Value> {
        let args: Vec<Value> = args
            .iter()
            .map(|arg| Value::string(arg.as_str()))
            .chain([Value::string(self.path()?)])
            .collect();
        shell.run_external(program, &args, mooring)?;
        self.readback(shell).map(Value::string)
    }

    fn readback(&self, shell: &mut Shell) -> Settled<String> {
        // Re-locate after exit: editors commonly replace files by rename.
        let directory = shell
            .judge::<Reading>(self.cleanup.cleanup_target())?
            .into_dir()
            .map_err(|target| {
                let path = quote_native(target.real().as_path().as_os_str());
                failure(EditorFailure::Workspace(EditorFailureWorkspace {
                    step: EditorWorkspaceStep::InspectBuffer,
                    path: Some(path.clone()),
                    cause: EditorWorkspaceFailure::Io(IoCause::new(IoLeaf::Path(
                        PathIoLeaf::NotDirectory(path),
                    ))),
                }))
            })?;
        let target = directory
            .locate(&self.file, LeafMode::NoFollow)
            .map_err(|e| {
                workspace_io(
                    EditorWorkspaceStep::LocateBuffer,
                    Some(quote_native(
                        self.directory
                            .real()
                            .as_path()
                            .join(self.file.as_os_str())
                            .as_os_str(),
                    )),
                    &e,
                )
            })?;
        let target = shell.judge::<Reading>(target)?;
        if target.stat().is_none_or(|stat| stat.kind != Kind::File) {
            return Err(failure(EditorFailure::Workspace(EditorFailureWorkspace {
                step: EditorWorkspaceStep::InspectBuffer,
                path: Some(quote_native(target.real().as_path().as_os_str())),
                cause: EditorWorkspaceFailure::ExpectedRegularFile(
                    EditorWorkspaceFailureExpectedRegularFile {
                        found: target.stat().map(|stat| stat.kind),
                    },
                ),
            })));
        }
        let file = target.read().map_err(|e| {
            workspace_io(
                EditorWorkspaceStep::OpenBuffer,
                Some(quote_native(target.real().as_path().as_os_str())),
                &e,
            )
        })?;
        read_draft(file).map_err(|e| {
            failure(EditorFailure::Draft(EditorFailureDraft {
                operation: DraftOperation::Read,
                cause: e.failure(),
            }))
        })
    }
}

fn edit(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let text = args[0].as_str(NAME)?;
    let command = EditorCommand::select(
        shell.env_var("VISUAL").as_deref(),
        shell.env_var("EDITOR").as_deref(),
    )
    .map_err(|e| failure(EditorFailure::Command(e.failure())))?;
    edit_with(text, command.program(), command.args(), mooring, shell)
}

fn edit_with(
    text: &str,
    program: &str,
    args: &[String],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let workspace = Workspace::create(shell, text)?;
    let edited = workspace.edit(shell, program, args, mooring);
    workspace.finish(shell, edited)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    fn shell() -> Shell {
        Shell::root(
            ral_core::test_helper::core_runtime().into(),
            ral_core::test_helper::process_cwd(),
        )
    }

    #[test]
    fn bounds_initial_draft_before_creating_workspace() {
        let error = Workspace::create(
            &mut shell(),
            &"λ".repeat(ral_core::external_editor::MAX_DRAFT_BYTES / 2 + 1),
        )
        .err()
        .unwrap();
        let Break::Error(error) = error else {
            panic!("expected draft failure")
        };
        assert_eq!(
            error.reason,
            Reason::Host(Box::new(HostFailure::Ral(RalFailure::Editor(
                EditorFailure::Draft(EditorFailureDraft {
                    operation: DraftOperation::Write,
                    cause: ral_core::fact::DraftFailure::TooLarge(
                        ral_core::fact::DraftFailureTooLarge {
                            limit: ral_core::external_editor::MAX_DRAFT_BYTES.into()
                        }
                    ),
                })
            ))))
        );
    }

    #[test]
    fn restricted_write_grant_refuses_temporary_workspace() {
        use ral_core::authority::policy::{Capabilities, FsPolicy};
        let layer = ral_core::test_helper::layer(Capabilities {
            fs: Some(FsPolicy::default()),
            ..Capabilities::default()
        });
        shell().with_layer(layer, |shell| {
            let error = Workspace::create(shell, "draft")
                .err()
                .expect("write must be refused");
            assert!(
                error
                    .to_string()
                    .contains("grant does not allow write access"),
                "{error}"
            );
        });
    }

    #[test]
    fn restricted_read_grant_refuses_readback() {
        use ral_core::authority::policy::{Capabilities, FsPolicy};
        let mut shell = shell();
        let workspace = Workspace::create(&mut shell, "draft").unwrap();
        let layer = ral_core::test_helper::layer(Capabilities {
            fs: Some(FsPolicy::default()),
            ..Capabilities::default()
        });
        shell.with_layer(layer, |shell| {
            let error = workspace.readback(shell).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("grant does not allow read access"),
                "{error}"
            );
        });
        workspace.finish(&mut shell, Ok(())).unwrap();
    }

    #[test]
    fn cleanup_requires_a_fresh_write_judgment_for_the_held_tree() {
        use ral_core::authority::policy::{Capabilities, FsPolicy};
        use ral_core::path::FrozenPath;

        let mut shell = shell();
        let workspace = Workspace::create(&mut shell, "kept").unwrap();
        let path = std::path::PathBuf::from(workspace.path().unwrap());
        let layer = ral_core::test_helper::layer(Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![FrozenPath::root()],
                write_prefixes: vec![FrozenPath::root()],
                deny_paths: vec![FrozenPath::from_surface(
                    path.parent().unwrap().to_str().unwrap(),
                )],
            }),
            ..Capabilities::default()
        });
        shell.with_layer(layer, |shell| {
            assert!(workspace.finish(shell, Ok(())).is_err());
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_failed_edit_and_a_failed_cleanup_are_both_reported() {
        use ral_core::authority::policy::{Capabilities, FsPolicy};

        let mut shell = shell();
        let workspace = Workspace::create(&mut shell, "kept").unwrap();
        let path = std::path::PathBuf::from(workspace.path().unwrap());
        let layer = ral_core::test_helper::layer(Capabilities {
            fs: Some(FsPolicy::default()),
            ..Capabilities::default()
        });
        let workspace = std::sync::Mutex::new(Some(workspace));
        let entry = BuiltinEntry::new(
            Cow::Borrowed("failed-editor"),
            ral_core::typecheck::builtins::scheme::diverges,
            "test failed edit and cleanup",
            BuiltinBody::Captured(std::sync::Arc::new(move |_, _, shell| {
                let workspace = workspace.lock().unwrap().take().expect("called once");
                shell.with_layer(layer.clone(), |shell| {
                    workspace.finish(
                        shell,
                        Err::<Value, _>(
                            Error::new(Reason::Process(Box::new(ral_core::fact::Process {
                                program: "editor".into(),
                                failure: ral_core::fact::Failure::Exited(7),
                            })))
                            .into(),
                        ),
                    )
                })
            })),
        );
        let hook = HookName::session("failed-editor");
        shell
            .register_hook(
                hook.clone(),
                Value::Native(entry.into(), [].into()),
                HookSig::Block,
                DefaultPolicy::denied(),
            )
            .unwrap();
        let ral_core::shell::run::RunReport::Ran {
            ending,
            captured: Some(captured),
            ..
        } = shell.run(ral_core::protocol::Run {
            program: ral_core::protocol::Program::Hook {
                name: hook,
                args: Vec::new(),
            },
            ..ral_core::protocol::Run::captured(
                "",
                ral_core::source::SourceOrigin::named("<editor-test>"),
            )
        })
        else {
            panic!("the editor hook must run with captured streams")
        };
        let Err(Break::Error(error)) = ending.into_settled() else {
            panic!("expected editor failure")
        };
        assert_eq!(
            error.reason,
            Reason::Process(Box::new(ral_core::fact::Process {
                program: "editor".into(),
                failure: ral_core::fact::Failure::Exited(7)
            }))
        );
        assert_eq!(error.hints(), []);
        assert_eq!(error.context(), Some("failed-editor"));
        let cleanup = String::from_utf8(captured.stderr.bytes).unwrap();
        assert!(
            cleanup.contains("grant does not allow write access"),
            "{cleanup}"
        );
        assert!(!cleanup.contains("editor exited"), "{cleanup}");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn successful_readback_preserves_text_and_cleans_up() {
        let mut shell = shell();
        let workspace = Workspace::create(&mut shell, "λ\n\n").unwrap();
        let path = workspace.path().unwrap();
        assert_eq!(workspace.readback(&mut shell).unwrap(), "λ\n\n");
        workspace.finish(&mut shell, Ok(())).unwrap();
        assert!(!std::path::Path::new(&path).exists());
    }

    #[test]
    fn reads_atomic_replacement_and_refuses_invalid_utf8() {
        let mut shell = shell();
        let workspace = Workspace::create(&mut shell, "old").unwrap();
        let path = std::path::PathBuf::from(workspace.path().unwrap());
        let replacement = path.with_extension("new");
        std::fs::write(&replacement, "new\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(workspace.readback(&mut shell).unwrap(), "new\n");
        std::fs::write(&path, [0xff]).unwrap();
        let Err(Break::Error(error)) = workspace.readback(&mut shell) else {
            panic!("expected UTF-8 refusal")
        };
        assert_eq!(
            error.reason,
            Reason::Host(Box::new(HostFailure::Ral(RalFailure::Editor(
                EditorFailure::Draft(EditorFailureDraft {
                    operation: DraftOperation::Read,
                    cause: ral_core::fact::DraftFailure::Utf8(ral_core::fact::Utf8Failure {
                        valid_up_to: 0_usize.into(),
                        error_len: Some(1_usize.into())
                    }),
                })
            ))))
        );
        workspace.finish(&mut shell, Ok(())).unwrap();
    }

    #[cfg(unix)]
    fn child_edit(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        let script = args[0].as_str("test editor")?;
        let args = ["-c", script, "ral-editor-test"].map(str::to_owned);
        edit_with("original", "/bin/sh", &args, mooring, shell)
    }

    #[cfg(unix)]
    fn run_child(script: &str) -> Settled<Value> {
        run_child_under(script, ral_core::authority::policy::Capabilities::default())
    }

    #[cfg(unix)]
    fn run_child_under(
        script: &str,
        caps: ral_core::authority::policy::Capabilities,
    ) -> Settled<Value> {
        static TEST_ENTRIES: [BuiltinEntry; 1] = [BuiltinEntry::new(
            Cow::Borrowed("test-editor"),
            scheme,
            "test editor",
            BuiltinBody::Static(child_edit),
        )];
        let mut runtime = ral_core::test_helper::core_runtime();
        runtime.install_builtins(&TEST_ENTRIES);
        let mut shell = Shell::root(runtime.into(), ral_core::test_helper::process_cwd());
        let source = format!("test-editor '{script}'");
        let request = ral_core::shell::run::RunRequest::from(crate::repl::exec::line_run(&source));
        let report = shell.with_layer(ral_core::test_helper::layer(caps), |shell| {
            shell.run(request)
        });
        let ral_core::shell::run::RunReport::Ran { ending, .. } = report else {
            panic!("the test editor call must check and run");
        };
        ending.into_settled()
    }

    #[cfg(unix)]
    #[test]
    fn successful_child_returns_the_replacement_text() {
        let result = run_child("printf edited > \"$1.new\"; /bin/mv \"$1.new\" \"$1\"");
        assert!(
            matches!(result, Ok(Value::String(ref text)) if text.as_ref() == "edited"),
            "{result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restricted_exec_grant_refuses_editor_without_spawning() {
        use ral_core::authority::policy::{Capabilities, ExecGrant};
        let marker = tempfile::tempdir().unwrap();
        let path = marker.path().join("ran");
        let result = run_child_under(
            &format!("printf ran > \"{}\"", path.display()),
            Capabilities {
                exec: Some(ExecGrant::default()),
                ..Capabilities::default()
            },
        );
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("denied by active grant"),
            "{error}"
        );
        assert!(!path.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn confined_editor_never_falls_back_to_unconfined_execution() {
        use ral_core::authority::policy::{Capabilities, FsPolicy};
        use ral_core::path::FrozenPath;
        let marker = tempfile::tempdir().unwrap();
        let path = marker.path().join("ran");
        let script = format!("printf ran > \"{}\"", path.display());
        run_child(&script).expect("the unconfined control editor must run");
        assert!(path.exists());
        std::fs::remove_file(&path).unwrap();
        // The draft is writable; this marker outside the workspace is not.
        let result = run_child_under(
            &script,
            Capabilities {
                fs: Some(FsPolicy {
                    read_prefixes: vec![FrozenPath::root()],
                    write_prefixes: vec![FrozenPath::from_surface(
                        ral_core::host::temp_dir().to_str().unwrap(),
                    )],
                    deny_paths: vec![FrozenPath::from_surface(path.to_str().unwrap())],
                }),
                ..Capabilities::default()
            },
        );
        assert!(result.is_err(), "the forbidden write must fail: {result:?}");
        assert!(!path.exists(), "the editor escaped its filesystem grant");
    }

    #[cfg(unix)]
    #[test]
    fn failed_child_never_returns_its_changed_file() {
        use ral_core::fact::FailureCode as _;
        let result = run_child("printf discarded > \"$1\"; exit 7");
        assert!(
            matches!(result, Err(ral_core::shell::runtime::Break::Error(ref e)) if e.failure_code().get() == 7),
            "{result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_readback_and_cleanup_does_not_follow_it() {
        let mut shell = shell();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "untouched").unwrap();
        let workspace = Workspace::create(&mut shell, "old").unwrap();
        let path = workspace.path().unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(outside.path(), &path).unwrap();
        let Err(Break::Error(error)) = workspace.readback(&mut shell) else {
            panic!("expected non-regular-file refusal")
        };
        assert!(
            matches!(&error.reason, Reason::Host(host) if matches!(host.as_ref(), HostFailure::Ral(RalFailure::Editor(EditorFailure::Workspace(EditorFailureWorkspace { step: EditorWorkspaceStep::InspectBuffer, cause: EditorWorkspaceFailure::ExpectedRegularFile(EditorWorkspaceFailureExpectedRegularFile { found: Some(Kind::Symlink) }), .. })))))
        );
        workspace.finish(&mut shell, Ok(())).unwrap();
        assert_eq!(
            std::fs::read_to_string(outside.path()).unwrap(),
            "untouched"
        );
    }
    #[test]
    fn editor_command_refusals_keep_variable_and_cause() {
        use ral_core::fact::{EditorCommandCause, EditorFailureCommand, EditorVariable};
        for (variable, spec, cause) in [
            (
                EditorVariable::Visual,
                "bad\0editor",
                EditorCommandCause::Nul,
            ),
            (
                EditorVariable::Editor,
                "\"\"",
                EditorCommandCause::NoProgram,
            ),
            #[cfg(not(windows))]
            (EditorVariable::Visual, "'", EditorCommandCause::Unbalanced),
        ] {
            let mut shell = shell();
            shell.set_env_var("VISUAL", String::new()).unwrap();
            shell.set_env_var("EDITOR", String::new()).unwrap();
            shell
                .set_env_var(
                    match variable {
                        EditorVariable::Visual => "VISUAL",
                        EditorVariable::Editor => "EDITOR",
                    },
                    spec,
                )
                .unwrap();
            let Err(Break::Error(error)) =
                edit(&[Value::string("draft")], &Mooring::adrift(), &mut shell)
            else {
                panic!("expected command refusal")
            };
            assert_eq!(
                error.reason,
                Reason::Host(Box::new(HostFailure::Ral(RalFailure::Editor(
                    EditorFailure::Command(EditorFailureCommand { variable, cause })
                ))))
            );
            assert_eq!(error.hints(), []);
            assert!(error.context().is_none());
            assert!(
                error
                    .reason
                    .advice()
                    .iter()
                    .any(|hint| hint.contains("input buffer was not changed"))
            );
        }
    }

    #[test]
    fn cleanup_preserves_original_error_report_facts_and_escape_precedence() {
        let mut shell = shell();
        let origin = ral_core::source::SourceOrigin::named("editor-source");
        let Err(Break::Error(original)) = ral_core::shell::load::evaluate_closed(
            &Mooring::adrift(),
            &mut shell,
            "let [",
            &origin,
            None,
        ) else {
            panic!("expected compile refusal")
        };
        let original = original
            .with_context("editor launch")
            .with_hints(["first", "second"]);
        let removal = || failure(EditorFailure::NoPluginContext);
        let Break::Error(error) = joined(&mut shell, original.clone().into(), removal()) else {
            panic!("expected original error")
        };
        assert_eq!(error.reason, original.reason);
        assert_eq!(error.context(), original.context());
        assert_eq!(error.hints(), original.hints());
        assert_eq!(error.witness(), original.witness());
        assert!(error.rejection().is_some());
        assert!(matches!(
            joined(
                &mut shell,
                original.into(),
                Break::Escape(ral_core::shell::runtime::Escape::Exit(9))
            ),
            Break::Escape(ral_core::shell::runtime::Escape::Exit(9))
        ));
    }
}
