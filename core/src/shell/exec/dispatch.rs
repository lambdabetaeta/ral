//! Command dispatch: resolve a head, then run the arm.
//!
//! Order is env → handlers → external for a bare name; `^name` and a path
//! head are the external directly, consulting neither the env nor the stack.
//! `evaluator::machine`'s `Exec` rule is the entry that resolves and runs
//! every arm; pipeline staging reaches `resolve_command_word` directly.

use crate::fact::{AuditIo, Command, ExitCode};
use crate::ir::{CommandName, CommandWord};
use crate::shell::runtime::{
    AuditStart, Break, BuiltinEntry, CommandOrigin, Env, HandlerEntry, HandlerLookup, Mooring,
    Observation, Observed, Settled, Shell, Value, epoch_us,
};

use super::capture::with_audit_capture;
use super::command::{self, Admission, Head};
use super::redirect::scope::with_redirects;
use crate::ir::Redirects;
use crate::source::Span;

// ── Resolution ─────────────────────────────────────────────────────────

/// One arm per place a command name can live.
pub(crate) enum Resolution {
    Env(Value),
    /// `depth` counts handler frames from the top down to the matched one
    /// inclusive — what `strip_matched` takes.  Boxed: [`HandlerEntry`] would
    /// otherwise set the size of every arm.
    Handler {
        entry: Box<HandlerEntry>,
        depth: usize,
    },
    /// A base handler frame — a manifest row from the argv half.
    Base(BuiltinEntry),
    External(Head),
}

/// Bare-name lookup: env → handlers → external.  A head like `f x` is a
/// lexical name, so `env` — not `shell.env` — is what a bare name
/// resolves through.  No admission check and no audit.
pub(crate) fn resolve(name: &str, env: &Env, shell: &Shell) -> Resolution {
    if let Some(value) = crate::shell::runtime::lookup(name, env, &shell.sig) {
        return Resolution::Env(value.clone());
    }
    match shell.lookup_handler(name) {
        Some(HandlerLookup::Frame(entry, depth)) => Resolution::Handler { entry, depth },
        Some(HandlerLookup::Base(entry)) => Resolution::Base(entry),
        None => Resolution::External(Head::resolve(
            &CommandName::Bare(name.into()),
            &shell.context,
        )),
    }
}

/// Resolve a [`CommandWord`]: a bare name goes through [`resolve`]; `^name`
/// and a path head are the external directly, consulting neither the env nor
/// the handler stack.
pub(crate) fn resolve_command_word(head: &CommandWord, env: &Env, shell: &Shell) -> Resolution {
    match head {
        CommandWord::Name(CommandName::Bare(s)) => resolve(s, env, shell),
        CommandWord::External(name) | CommandWord::Name(name) => {
            Resolution::External(Head::resolve(name, &shell.context))
        }
    }
}

// ── Runners ─────────────────────────────────────────────────────────────

impl Shell {
    /// Run an external directly from a native body in the current run.
    ///
    /// `program` is a bare name or literal path, never source syntax; env and
    /// handler lookup are skipped. Pass the body's mooring to retain its audit
    /// and terminal authority. The current grants, scope and streams apply.
    ///
    /// # Errors
    /// Admission, argument, launch, child failure or cancellation.
    pub fn run_external_argv(
        &mut self,
        program: &str,
        args: &[Value],
        mooring: &Mooring,
    ) -> Settled<Value> {
        let head = Head::resolve(&CommandName::of(program), &self.context).admit(mooring, self)?;
        run_external(head, args, &Redirects::default(), None, mooring, self)
    }
}

/// Run a base handler frame directly with the argv slice — no adapter, no
/// masking: a native body never self-forwards.
///
/// The values arrive unrendered, unlike a ral arm's (`render_handler_args`):
/// a native body renders what it writes and vets what it launches, and the
/// exec boundary's refusal is a judgement on the value's shape.
pub(crate) fn run_base_frame(
    entry: &BuiltinEntry,
    args: &[Value],
    redirects: &Redirects<String>,
    span: Option<Span>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    entry.framed(span, shell, |shell, frame| {
        with_redirects(redirects, span, mooring, shell, |shell| {
            entry.call_body(frame, args, None, mooring, shell)
        })
    })
}

/// An external's door, with its redirects installed inside it.
pub(crate) fn run_external(
    head: Head<Admission>,
    args: &[Value],
    redirects: &Redirects<String>,
    span: Option<Span>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let shown = head.shown.clone();
    call_external(&shown, args, span, mooring, shell, |shell| {
        with_redirects(redirects, span, mooring, shell, |shell| {
            command::run(head, args, mooring, shell)
        })
    })
}

/// An external's door: stamp the start, tee its stdout and stderr through
/// the capture, name a failure, and settle the one [`Observed::Command`].
fn call_external(
    shown: &str,
    args: &[Value],
    span: Option<Span>,
    mooring: &Mooring,
    shell: &mut Shell,
    body: impl FnOnce(&mut Shell) -> Settled<Value>,
) -> Settled<Value> {
    shell.at_site(shown.to_owned(), span, |shell| {
        let start = shell.audit_start(mooring);
        let (result, io) = with_audit_capture(shell, body);
        let result = result.map_err(|b| b.locate(shown, span, shell));
        finish_command(shell, mooring, start, shown, args, &result, io);
        result
    })
}

fn finish_command(
    shell: &mut Shell,
    mooring: &Mooring,
    start: Option<AuditStart>,
    shown: &str,
    args: &[Value],
    result: &Settled<Value>,
    io: AuditIo,
) {
    let Some(start) = start else { return };
    // A stop is recorded too, as a pipeline's external stage records it.
    let broke = result.as_ref().err();
    let obs = Observation::spanning(
        start.site,
        start.time,
        epoch_us(),
        shell.context.principal(),
        Observed::Command(Command::new(
            shown,
            Value::render_argv(args),
            broke.map_or(0, ExitCode::code),
            CommandOrigin::External,
            io,
            broke.map(|b| match b {
                Break::Error(e) => e.describe().into_owned(),
                Break::Escape(e) => e.to_string(),
            }),
        )),
    );
    shell.observe_stamped(Some(mooring), obs);
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::authority::policy::{Capabilities, ExecGrant};
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
            .set_env_var("PATH", tmp.path().to_str().unwrap());
        (tmp, shell)
    }

    #[test]
    fn native_external_resolves_bare_and_path_heads_and_preserves_argv() {
        let (_tmp, mut shell) = external();
        for program in ["native-external", "./native-external"] {
            for code in [0, 7] {
                let result = shell.run_external_argv(
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
        let result = shell.run_external_argv(
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
        let result = shell.run_external_argv(
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
