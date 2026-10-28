//! A leased hook edits a private file and returns only a successful readback.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::io::{self, Write};

use ral_core::external_editor::{EditorCommand, admit_draft, after_cleanup, read_draft};
use ral_core::path::{Dir, EntryName, Kind, LeafMode, Reading, ScopedDir, Visibility, Writing};
use ral_core::shell::runtime::{
    Break, BuiltinBody, BuiltinEntry, DefaultPolicy, HookName, HookSig, Mooring, Settled, sig,
};
use ral_core::{Shell, Value};

const NAME: &str = "_external-editor";

static ENTRIES: [BuiltinEntry; 1] = [BuiltinEntry::new(
    Cow::Borrowed(NAME),
    scheme,
    "_external-editor <text>: edit with VISUAL or EDITOR; return the edited text on success.",
    BuiltinBody::Static(edit),
)];
pub(crate) static BUILTINS: &[BuiltinEntry] = &ENTRIES;

fn scheme(_u: &mut ral_core::typecheck::Unifier) -> ral_core::ty::Scheme {
    use ral_core::ty::Ty;
    use ral_core::typecheck::builtins::{fun, mk_scheme, pure, thunk};
    mk_scheme(&[], &[], thunk(fun(Ty::String, pure(Ty::String))))
}

pub(crate) fn register(shell: &mut Shell) -> Result<(), String> {
    let entry = shell
        .lookup_builtin(NAME)
        .ok_or_else(|| format!("this shell's surface has no {NAME}"))?;
    shell
        .register_hook(
            HookName::session("external-editor"),
            Value::Native {
                entry: entry.into(),
                applied: Box::new([]),
            },
            HookSig::Hook {
                kind: "external editor".into(),
            },
            DefaultPolicy::leased(),
        )
        .map_err(|e| e.to_string())
}

fn failure(context: &str, error: impl std::fmt::Display) -> Break {
    sig(format!(
        "external editor: {context}: {error}; the input buffer was not changed"
    ))
}

/// An error keeps its status and gains the other's message.  An escape is no
/// error to extend: it wins, and the other goes to stderr beside it.
fn joined(shell: &mut Shell, edit: Break, removal: Break) -> Break {
    match (edit, removal) {
        (Break::Error(mut edit), Break::Error(removal)) => {
            edit.message = format!("{}; {}", edit.message, removal.message);
            Break::Error(edit)
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
        let bytes = admit_draft(text).map_err(|e| failure("write buffer", e))?;
        let temp = ral_core::host::temp_dir();
        let temp = ral_core::path::utf8(&temp).map_err(|e| failure("temporary directory", e))?;
        let parent = shell.locate_dir::<Writing>(&shell.resolve(temp))?;
        let cleanup = loop {
            let name = EntryName::fresh(".ral-editor-")
                .map_err(|e| failure("temporary directory name", e))?;
            let target = shell.judge::<Writing>(parent.absent(name))?;
            match target.create_scoped_dir() {
                Ok(directory) => break directory,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(failure(
                        &format!("create private directory under {temp}"),
                        e,
                    ));
                }
            }
        };
        let workspace = Self {
            directory: cleanup.directory(),
            cleanup,
            file: EntryName::parse(OsStr::new("buffer.ral"))
                .map_err(|e| failure("buffer filename", e))?,
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
            .map_err(|e| failure("create private buffer", e))?
            .write_all(bytes)
            .map_err(|e| failure("write buffer", e))
    }

    /// Judge the held tree now, even when the editor or readback failed.
    /// Consumes the workspace: removal is attempted once.
    fn finish<T>(self, shell: &mut Shell, edited: Settled<T>) -> Settled<T> {
        let removed = shell
            .judge::<Writing>(self.cleanup.cleanup_target())
            .and_then(|target| {
                target
                    .remove()
                    .map_err(|e| failure("remove private workspace", e))
            });
        after_cleanup(edited, removed, |edit, removal| {
            joined(shell, edit, removal)
        })
    }

    fn path(&self) -> Settled<String> {
        let path = self.directory.real().as_path().join(self.file.as_os_str());
        ral_core::path::utf8(&path)
            .map(str::to_owned)
            .map_err(|e| failure("buffer path", e))
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
        shell.run_external_argv(program, &args, mooring)?;
        self.readback(shell).map(Value::string)
    }

    fn readback(&self, shell: &mut Shell) -> Settled<String> {
        // Re-locate after exit: editors commonly replace files by rename.
        let directory = shell
            .judge::<Reading>(self.cleanup.cleanup_target())?
            .into_dir()
            .map_err(|_| failure("read edited buffer", "workspace is not a directory"))?;
        let target = directory
            .locate(&self.file, LeafMode::NoFollow)
            .map_err(|e| failure("locate edited buffer", e))?;
        let target = shell.judge::<Reading>(target)?;
        if target.stat().is_none_or(|stat| stat.kind != Kind::File) {
            return Err(failure(
                "read edited buffer",
                "the editor must leave a regular file, not a link or special file",
            ));
        }
        let file = target
            .read()
            .map_err(|e| failure("open edited buffer", e))?;
        read_draft(file).map_err(|e| failure("read edited buffer", e))
    }
}

fn edit(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let text = args[0].as_str(NAME)?;
    let command = EditorCommand::select(
        shell.env_var("VISUAL").as_deref(),
        shell.env_var("EDITOR").as_deref(),
    )
    .map_err(|e| failure("choose editor", e))?;
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
    fn registers_the_leased_editor_hook() {
        let mut runtime = ral_core::test_helper::core_runtime();
        runtime.install_builtins(BUILTINS);
        let mut shell = Shell::root(runtime.into(), ral_core::test_helper::process_cwd());
        register(&mut shell).unwrap();
        assert!(shell.has_hook(&HookName::session("external-editor")));
    }

    #[test]
    fn bounds_initial_draft_before_creating_workspace() {
        let error = Workspace::create(
            &mut shell(),
            &"λ".repeat(ral_core::external_editor::MAX_DRAFT_BYTES / 2 + 1),
        )
        .err()
        .expect("an oversized UTF-8 draft must be refused");
        assert!(error.to_string().contains("16 MiB"), "{error}");
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
                error.to_string().contains("fs write denied by grant"),
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
                error.to_string().contains("fs read denied by grant"),
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
        let error = shell
            .with_layer(layer, |shell| {
                workspace.finish(shell, Err::<(), _>(sig("the editor failed")))
            })
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("the editor failed; "), "{error}");
        assert!(error.contains("fs write denied by grant"), "{error}");
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
        assert!(workspace.readback(&mut shell).is_err());
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
        ending.into_result()
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
        let result = run_child("printf discarded > \"$1\"; exit 7");
        assert!(
            matches!(result, Err(ral_core::shell::runtime::Break::Error(ref e)) if e.code() == 7),
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
        assert!(workspace.readback(&mut shell).is_err());
        workspace.finish(&mut shell, Ok(())).unwrap();
        assert_eq!(
            std::fs::read_to_string(outside.path()).unwrap(),
            "untouched"
        );
    }
}
