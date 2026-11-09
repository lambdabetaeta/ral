//! Launch phase: wire one stage's byte endpoints and start it — a ral-written
//! thread, or a direct external spawned into the pipeline's process group.

use super::super::command;
use super::super::command::{Command, Request};
use super::super::job_control::TerminalInput;
use super::collect::Slot;
use super::group::PipelineGroup;
use super::resolve::StageLaunch;
use super::route::{ByteIn, ByteOut, HeldEdge, StageRoute};
use super::stage::{LiveExternal, StageAudit, StageHandle, StageKind};
use super::thread::launch_thread_stage;
use crate::error::io_message;
use crate::fact::AuditIo;
use crate::io::child::Connection;
use crate::io::{Source, SourceReader};
use crate::process::{PgidPolicy, Scope};
use crate::shell::runtime::{CommandOrigin, Env, Error, Mooring, Settled, Shell, Value};
use crate::source::Span;
use std::sync::Arc;

/// A thread stage's stdin: its boundary's, where the terminal passes as
/// itself (its externals inherit it, `stdin_reader` refuses it), and a reader
/// as one its scope interrupts.
pub(super) fn stage_stdin(
    route: ByteIn,
    shell: &Shell,
    scope: &Scope,
    holds_terminal: bool,
) -> Settled<Source> {
    let reader = match route {
        ByteIn::Upstream(r) => SourceReader::pipe(r),
        ByteIn::Parent => {
            match TerminalInput::for_pipeline(shell, holds_terminal).stdin(&shell.io.stdin) {
                Source::Empty => return Ok(Source::Empty),
                Source::Terminal => return Ok(Source::Terminal),
                Source::Reader(r) => r
                    .try_clone()
                    .map_err(|e| Error::new(io_message("could not duplicate stdin", &e)))?,
            }
        }
    };
    Ok(Source::Reader(reader.interruptible(scope.clone())))
}

pub(super) struct LaunchCx<'a> {
    pub(super) mooring: &'a Mooring,
    pub(super) shell: &'a mut Shell,
    /// The pipeline node's own lexical environment — a thread stage's captured
    /// closure env, distinct from `shell.env` inside a nested machine.
    pub(super) env: &'a Env,
    pub(super) eval: super::StageEval,
    pub(super) group: &'a PipelineGroup,
    /// Whether the group was lent the terminal, frozen before any stage exists.
    pub(super) holds_terminal: bool,
}

/// Dispatch one stage per its resolve-time [`StageLaunch`], which a direct
/// stage's command leaves by value: nothing holds the program after it launches.
/// Each launch consumes its route: an edge's writer goes to the process or
/// thread, its hold returns to the stage's handle.
pub(super) fn spawn_stage(
    stage: &Arc<crate::ir::Comp>,
    launch: StageLaunch,
    route: StageRoute,
    cx: &mut LaunchCx<'_>,
    slot: Slot,
) -> Settled<StageHandle> {
    let StageRoute { stdin, stdout } = route;
    let name = match &stage.item {
        crate::ir::CompKind::Exec(exec) => exec.head.name().written(),
        _ => "pipeline".into(),
    };
    let span = stage.span;
    let (kind, held) = match launch {
        StageLaunch::Direct { command, args } => {
            launch_external_stage_direct(command, &args, span, stdin, stdout, cx, &slot)
                .map(|(stage, held)| (StageKind::External(stage), held))
        }
        StageLaunch::Thread => launch_thread_stage(stage, stdin, stdout, cx, slot.clone())
            .map(|(stage, held)| (StageKind::Thread(stage), held)),
    }
    .map_err(|b| b.locate(&name, span, cx.shell))?;
    Ok(StageHandle::new(kind, held, slot))
}

/// Spawn an external stage with no thread hosting it.
fn launch_external_stage_direct(
    command: Command<Request>,
    args: &[Value],
    span: Option<Span>,
    stdin: ByteIn,
    stdout: ByteOut,
    cx: &mut LaunchCx<'_>,
    slot: &Slot,
) -> Settled<(LiveExternal, Option<HeldEdge>)> {
    let name = command.shown.clone();
    let mooring = cx.mooring;
    let group = cx.group;
    let holds_terminal = cx.holds_terminal;
    cx.shell.at_site(name.clone(), span, |shell| {
        let audit_start = shell.audit_start(mooring);
        let command = command.admit(mooring, shell)?;
        // Render audit values before any child exists, and only if somebody
        // will consume them. The command independently vets its exec argv.
        let audit_args = if audit_start.is_some() {
            Value::render_argv(args)
        } else {
            Vec::new()
        };
        let launched = (|| -> Settled<_> {
            let command = command.prepare(args, shell)?;
            let upstream;
            let stdin = match stdin {
                ByteIn::Upstream(r) => {
                    upstream = Source::Reader(SourceReader::pipe(r));
                    &upstream
                }
                ByteIn::Parent => {
                    TerminalInput::for_pipeline(shell, holds_terminal).stdin(&shell.io.stdin)
                }
            };
            // Admission is over: borrow parent destinations only now.
            let (io, held) = match stdout {
                ByteOut::Parent => (
                    Connection::new(stdin, &shell.io.stdout, &shell.io.stderr),
                    None,
                ),
                ByteOut::Downstream { writer, held } => (
                    Connection::to_pipe(stdin, writer, &shell.io.stderr),
                    Some(held),
                ),
            };
            let started = command.start(
                io.map_err(command::connection_error)?,
                PgidPolicy::Join(group.membership(&shell.scope)),
                &shell.scope,
            )?;
            let stage = started.register(slot, group.group().owned(), &shell.scope)?;
            Ok((stage, held))
        })();
        match launched {
            Ok((mut stage, held)) => {
                stage.end.settlement.audit = Some(Box::new(StageAudit {
                    prov: shell.local.audit.call_site().clone(),
                    start: audit_start,
                    args: audit_args,
                }));
                Ok((stage, held))
            }
            Err(broke) => {
                if let Some(start) = &audit_start {
                    let obs = shell.command_observation(
                        start,
                        &name,
                        audit_args,
                        CommandOrigin::External,
                        Some(&broke),
                        AuditIo::default(),
                    );
                    shell.observe_stamped(Some(mooring), obs);
                }
                Err(broke)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::super::job_control::Placement;
    use super::*;

    #[test]
    fn the_terminal_reaches_a_thread_stage_only_when_the_group_holds_it() {
        let mut shell = crate::test_helper::core_shell();
        shell.io.stdin = Source::Terminal;
        let scope = shell.scope.child();

        let held = stage_stdin(ByteIn::Parent, &shell, &scope, true).expect("stage_stdin");
        assert!(matches!(held, Source::Terminal));
        let denied = stage_stdin(ByteIn::Parent, &shell, &scope, false).expect("stage_stdin");
        assert!(matches!(denied, Source::Empty));
    }

    /// `!{ cat } | wc` and `!{ cat | head -1 } | wc` agree: `cat`, a lone
    /// child or a nested pipeline's stage, joins the stage's group and reads
    /// what the stage was given, the terminal under the loan and EOF without.
    #[test]
    fn a_stages_external_reads_the_stages_stdin_however_it_runs() {
        for lent in [true, false] {
            let expected = |seen: &Source| {
                if lent {
                    matches!(seen, Source::Terminal)
                } else {
                    matches!(seen, Source::Empty)
                }
            };
            let mut shell = crate::test_helper::core_shell();
            shell.io.stdin = Source::Terminal;
            let node = shell.scope.child();
            let given = stage_stdin(ByteIn::Parent, &shell, &node, lent).expect("stage_stdin");
            assert!(expected(&given));

            // The stage's own stdin and group, as `launch_thread_stage` seats it.
            let group = crate::process::Pgid::from_raw(1).expect("1 is positive");
            shell.io.stdin = given;
            shell.session.stage = Some(crate::process::Membership::new(group, Scope::clone(&node)));
            let placed = Placement::for_command(&shell, &Mooring::adrift(), false);
            assert!(matches!(placed, Placement::Joined(_)));
            // Joined, the lone child and the nested stage read the same source,
            // whatever their group could read.
            assert!(expected(placed.stdin(&shell.io.stdin)));
            for holds_terminal in [true, false] {
                assert!(expected(
                    TerminalInput::for_pipeline(&shell, holds_terminal).stdin(&shell.io.stdin)
                ));
                let nested = stage_stdin(ByteIn::Parent, &shell, &node, holds_terminal)
                    .expect("stage_stdin");
                assert!(expected(&nested));
            }
        }
    }
}
