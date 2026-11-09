//! An external's audit record: one `Command` observation, begun at its
//! launch and ended with it.
//!
//! Every launch of an external keeps exactly one [`CommandRecord`], and only
//! an admitted command can begin one, so a refusal is the grant's check
//! record alone. The standalone external and `detach` end it before their
//! frame closes, through [`Command::with_record`]; a pipeline stage carries it
//! out of its frame and its settlement ends it.

use super::capture::with_audit_capture;
use super::command::Command;
use crate::authority::policy::Admitted;
use crate::error::Error;
use crate::fact::{self, AuditIo, ExitCode, Observation, Observed};
use crate::shell::runtime::{Break, CommandOrigin, Mooring, Settled, Shell, Value, epoch_us};
use crate::source::{CallSite, Span};

/// One external's `Command` observation, from launch to end. Silent when
/// nobody listened at launch: it then holds nothing and ending it records
/// nothing, so a site ends it unconditionally.
pub(super) struct CommandRecord(Option<Begun>);

/// What a listened-to launch fixes of its record: where and when it began,
/// and the command as a standalone dispatch spells it.
struct Begun {
    site: CallSite,
    time: i64,
    shown: String,
    args: Vec<String>,
    origin: CommandOrigin,
}

impl CommandRecord {
    /// The record of `command` launching now, at the current site. `args`
    /// are rendered only if somebody listens.
    pub(super) fn begin(
        command: &Command<Result<Admitted, Error>>,
        args: &[Value],
        origin: CommandOrigin,
        mooring: &Mooring,
        shell: &Shell,
    ) -> Self {
        Self(shell.listening(mooring).then(|| Begun {
            site: shell.call_site(),
            time: epoch_us(),
            shown: command.shown.clone(),
            args: Value::render_argv(args),
            origin,
        }))
    }

    /// The observation of a run ended as `broke`, status 0 if it did not,
    /// for a site that reports it later.
    pub(super) fn end(
        self,
        broke: Option<&Break>,
        io: AuditIo,
        shell: &Shell,
    ) -> Option<Observation> {
        let Begun {
            site,
            time,
            shown,
            args,
            origin,
        } = self.0?;
        let error = broke.map(|b| match b {
            Break::Error(e) => e.describe().into_owned(),
            Break::Escape(e) => e.to_string(),
        });
        let status = broke.map_or(0, ExitCode::code);
        let command = fact::Command::new(&shown, args, status, origin, io, error);
        Some(Observation::spanning(
            site,
            time,
            epoch_us(),
            shell.context.principal(),
            Observed::Command(command),
        ))
    }

    /// End it as [`Self::end`] does, and report it now.
    pub(super) fn close(
        self,
        broke: Option<&Break>,
        io: AuditIo,
        mooring: &Mooring,
        shell: &mut Shell,
    ) {
        if let Some(observation) = self.end(broke, io, shell) {
            shell.observe_stamped(Some(mooring), observation);
        }
    }

    /// A record nobody listened to.
    #[cfg(test)]
    pub(super) fn silent() -> Self {
        Self(None)
    }

    /// The record of `shown args…` begun at `time`, at the current site.
    #[cfg(test)]
    pub(super) fn begun_at(time: i64, shown: &str, args: &[&str], shell: &Shell) -> Self {
        Self(Some(Begun {
            site: shell.call_site(),
            time,
            shown: shown.into(),
            args: args.iter().map(|&arg| arg.into()).collect(),
            origin: CommandOrigin::External,
        }))
    }
}

impl Command<Result<Admitted, Error>> {
    /// Run `launch` of this command in a frame of its own at `span`, its
    /// record begun before and ended after, failed or stopped runs included,
    /// and its failure claimed by that frame.
    pub(super) fn with_record(
        self,
        args: &[Value],
        origin: CommandOrigin,
        span: Option<Span>,
        mooring: &Mooring,
        shell: &mut Shell,
        launch: impl FnOnce(Self, &mut Shell) -> Settled<Value>,
    ) -> Settled<Value> {
        let shown = self.shown.clone();
        shell.at_site(shown.clone(), span, |shell| {
            let record = CommandRecord::begin(&self, args, origin, mooring, shell);
            let (result, io) = with_audit_capture(shell, |shell| launch(self, shell));
            let result = result.map_err(|b| b.locate(&shown, span, shell));
            record.close(result.as_ref().err(), io, mooring, shell);
            result
        })
    }
}
