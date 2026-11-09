//! Command words: what a head names, and how an external one runs.
//!
//! A bare name is looked up in the env, then the handler stack, then `PATH`;
//! `^name` and a path are always externals. The evaluator's `Exec` rule runs
//! every arm but the external, which runs here.

use super::command::{Command, Request};
use super::redirect::scope::with_redirects;
use crate::authority::policy::Admitted;
use crate::authority::sandbox::Ownership;
use crate::error::Error;
use crate::ir::{CommandName, CommandWord, Redirects};
use crate::shell::runtime::{
    BuiltinEntry, CommandOrigin, Env, HandlerEntry, HandlerLookup, Mooring, Settled, Shell, Value,
};
use crate::source::Span;

/// What a command head names.
pub(crate) enum Resolution {
    Env(Value),
    /// `depth` counts handler frames from the top down to the match,
    /// inclusive, as `strip_matched` takes it. Boxed to keep the enum small.
    Handler {
        entry: Box<HandlerEntry>,
        depth: usize,
    },
    /// One of ral's own commands, from the base handler frame.
    Base(BuiltinEntry),
    External(Command<Request>),
}

/// Resolve a command head. `env` is the lexical env, not `shell.env`.
pub(crate) fn resolve_command_word(word: &CommandWord, env: &Env, shell: &Shell) -> Resolution {
    let name = match word {
        CommandWord::Name(CommandName::Bare(name)) => name,
        CommandWord::External(name) | CommandWord::Name(name) => {
            return Resolution::External(Command::from_name(name, &shell.context));
        }
    };
    if let Some(value) = crate::shell::runtime::lookup(name, env, &shell.sig) {
        return Resolution::Env(value.clone());
    }
    match shell.lookup_handler(name) {
        Some(HandlerLookup::Frame(entry, depth)) => Resolution::Handler { entry, depth },
        Some(HandlerLookup::Base(entry)) => Resolution::Base(entry),
        None => Resolution::External(Command::from_name(
            &CommandName::Bare(name.clone()),
            &shell.context,
        )),
    }
}

impl Shell {
    /// `^program args…`, for a host holding no syntax: no handlers, no
    /// redirects, no span. Otherwise exactly the `Exec` rule's external.
    ///
    /// # Errors
    /// Admission, argument, launch, child failure or cancellation.
    pub fn run_external(
        &mut self,
        program: &str,
        args: &[Value],
        mooring: &Mooring,
    ) -> Settled<Value> {
        let command = Command::from_name(&CommandName::of(program), &self.context)
            .lookup(&self.context.grants, self.baseline())
            .admit(mooring, self)?;
        run_external(command, args, &Redirects::default(), None, mooring, self)
    }
}

/// Run an admitted external under its redirects.
pub(crate) fn run_external(
    command: Command<Result<Admitted, Error>>,
    args: &[Value],
    redirects: &Redirects<String>,
    span: Option<Span>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let origin = CommandOrigin::External;
    command.with_record(args, origin, span, mooring, shell, |command, shell| {
        with_redirects(redirects, span, mooring, shell, |shell| {
            command
                .prepare(args, Ownership::Kept, shell)?
                .run(mooring, shell)
        })
    })
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::authority::policy::{Capabilities, ExecGrant};
    use crate::fact::ExitCode as _;
    use crate::shell::runtime::{Break, Observed};
    use std::os::unix::fs::PermissionsExt;

    fn external() -> (tempfile::TempDir, Shell) {
        let tmp = tempfile::tempdir().unwrap();
        let program = tmp.path().join("native-external");
        std::fs::write(
            &program,
            "#!/bin/sh\n[ \"$1\" = 'one argument' ] || exit 9\nexit \"$2\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(crate::path::CwdPath::new(tmp.path().to_path_buf()).unwrap());
        shell
            .context
            .set_env_var("PATH", tmp.path().to_str().unwrap())
            .unwrap();
        (tmp, shell)
    }

    #[test]
    fn native_external_resolves_bare_names_and_paths_and_preserves_argv() {
        let (_tmp, mut shell) = external();
        for program in ["native-external", "./native-external"] {
            for code in [0, 7] {
                let result = shell.run_external(
                    program,
                    &[
                        Value::string("one argument"),
                        Value::string(code.to_string()),
                    ],
                    &Mooring::adrift(),
                );
                if code == 0 {
                    assert!(matches!(result, Ok(Value::Unit)), "{result:?}");
                } else {
                    assert_eq!(result.unwrap_err().code(), code);
                }
            }
        }
    }

    #[test]
    fn native_external_audit_names_the_program_and_sites_the_visible_register() {
        let (_tmp, mut shell) = external();
        let wrapper = crate::shell::runtime::Prov::Resolved(Box::new((
            "native-wrapper".into(),
            crate::source::CallSite {
                script: "wrapper.ral".into(),
                line: 2,
                col: 3,
            },
        )));
        let _site = shell.local.audit.enter_site(wrapper);
        let scope = shell
            .local
            .audit
            .open(crate::shell::runtime::CapturePolicy::Off);
        let result = shell.run_external(
            "native-external",
            &[Value::string("one argument"), Value::string("7")],
            &Mooring::adrift(),
        );
        let Err(Break::Error(error)) = result else {
            panic!("the child exits 7")
        };
        assert!(
            error.prov.is_none(),
            "an outer visible frame may still claim it"
        );
        let trail = shell.local.audit.close(scope);
        let [observation] = trail.as_slice() else {
            panic!("one command observation")
        };
        let Observed::Command(command) = &observation.what else {
            panic!("the external command")
        };
        assert_eq!(
            command.error.as_deref(),
            Some("native-external: exited with status 7")
        );
        assert_eq!(observation.site, shell.call_site());
    }

    #[test]
    fn native_external_admits_before_running() {
        let (_tmp, mut shell) = external();
        shell
            .context
            .grants
            .push(crate::test_helper::layer(Capabilities {
                exec: Some(ExecGrant::default()),
                ..Capabilities::default()
            }));
        let result = shell.run_external(
            "native-external",
            &[Value::string("one argument"), Value::string("0")],
            &Mooring::adrift(),
        );
        assert!(
            result.is_err(),
            "an empty exec grant must refuse the program"
        );
    }
}
