//! Command dispatch: resolve a head, then run the arm.
//!
//! Order is env → handlers → external for a bare name; `^name` and a path
//! head are the external directly, consulting neither the env nor the stack.
//! `evaluator::machine`'s `Exec` rule is the entry that classifies and runs
//! every arm; pipeline staging reaches
//! `resolve_command_word`/`classify_command` directly.

use crate::fact::{AuditIo, Command};
use crate::ir::{CommandName, CommandWord};
use crate::shell::runtime::{
    AuditStart, Break, BuiltinEntry, CommandOrigin, Env, HandlerEntry, HandlerLookup, Mooring,
    Observation, Observed, Settled, Shell, Value, epoch_us, name_failure,
};

use super::capture::with_audit_capture;
use super::command::{self, Admission, Head, Target};
use super::redirect::scope::with_redirects;
use crate::ir::Redirects;
use crate::source::Span;

// ── Resolution ─────────────────────────────────────────────────────────

/// One arm per place a command name can live.
/// `P` is the external's target, or admission from [`classify_command`].
pub(crate) enum Resolution<P = Target> {
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
    External(Head<P>),
}

/// Bare-name lookup: env → handlers → external.  A head like `f x` is a
/// lexical name, so `env` — not `shell.env` — is what a bare name
/// resolves through.  No admission check and no audit — those belong to
/// [`classify_command`].
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

/// Resolution plus head admission, a plain call's one exec judgment:
/// `Err` is the grant refusing the head before its redirects open or its
/// call is recorded.  Grants govern exec alone, so the other arms pass
/// unconditionally.
pub(crate) fn classify_command(
    head: &CommandWord,
    env: &Env,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Resolution<Admission>> {
    Ok(match resolve_command_word(head, env, shell) {
        Resolution::External(head) => Resolution::External(head.admit(mooring, shell)?),
        Resolution::Env(value) => Resolution::Env(value),
        Resolution::Handler { entry, depth } => Resolution::Handler { entry, depth },
        Resolution::Base(entry) => Resolution::Base(entry),
    })
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
    entry.framed(shell, |shell, frame| {
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
    call_external(&shown, args, mooring, shell, |shell| {
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
    mooring: &Mooring,
    shell: &mut Shell,
    body: impl FnOnce(&mut Shell) -> Settled<Value>,
) -> Settled<Value> {
    let start = shell.audit_start(mooring);
    let (mut result, io) = with_audit_capture(shell, body);
    name_failure(shown, &mut result);
    finish_command(shell, mooring, start, shown, args, &result, io);
    result
}

fn finish_command(
    shell: &mut Shell,
    mooring: &Mooring,
    start: AuditStart,
    shown: &str,
    args: &[Value],
    result: &Settled<Value>,
    io: AuditIo,
) {
    if !shell.listening(mooring) {
        return;
    }
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
            broke.map_or(0, Break::code),
            CommandOrigin::External,
            io,
            broke.map(Break::to_string),
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
