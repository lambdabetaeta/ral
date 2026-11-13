//! Launch phase: wire one stage's byte endpoints and start it — a ral-written
//! thread, or a direct external spawned into the pipeline's process group.

use super::super::command::{Command, Request, StartFailure};
use super::super::record::CommandRecord;
use super::collect::Slot;
use super::group::PipelineGroup;
use super::resolve::StageLaunch;
use super::route::{ByteIn, ByteOut, HeldEdge, StageRoute};
use super::stage::{LiveExternal, StageFrame, StageHandle, StageKind};
use super::thread::launch_thread_stage;
use crate::authority::sandbox::Ownership;
use crate::fact::AuditIo;
use crate::io::child::Connection;
use crate::io::{Source, SourceReader};
use crate::ir::{Env, ValClos};
use crate::process::{Placement, Scope, TerminalInput};
use crate::shell::runtime::{CommandOrigin, Error, Mooring, Settled, Shell};
use crate::source::Span;
use std::sync::Arc;

/// A thread stage's stdin: its boundary's, where the terminal passes as
/// itself (its externals inherit it, `stdin_reader` refuses it), and a reader
/// as one its scope interrupts.
pub(super) fn stage_stdin(
    route: ByteIn,
    shell: &Shell,
    scope: &Scope,
    input: TerminalInput,
) -> Settled<Source> {
    let reader = match route {
        ByteIn::Upstream(r) => SourceReader::pipe(r),
        ByteIn::Parent => match shell.io.stdin.offered(input) {
            Source::Empty => return Ok(Source::Empty),
            Source::Terminal => return Ok(Source::Terminal),
            Source::Reader(r) => r.try_clone().map_err(|e| {
                Error::io(
                    crate::fact::IoOperation::Duplicate,
                    crate::fact::IoSubject::Stream(crate::fact::StandardStream::Stdin),
                    &e,
                )
            })?,
        },
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
    /// Terminal input selected once, before any stage exists.
    pub(super) input: TerminalInput,
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
) -> Result<StageHandle, StartFailure> {
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
            .map(|(stage, held)| (StageKind::Thread(stage), held))
            .map_err(StartFailure::from),
    }
    .map_err(|mut failure| {
        failure.reason = Box::new(failure.reason.locate(&name, span, cx.shell));
        failure
    })?;
    Ok(StageHandle::new(kind, held, slot))
}

/// Spawn an external stage with no thread hosting it.
fn launch_external_stage_direct(
    command: Command<Request>,
    args: &[ValClos],
    span: Option<Span>,
    stdin: ByteIn,
    stdout: ByteOut,
    cx: &mut LaunchCx<'_>,
    slot: &Slot,
) -> Result<(LiveExternal, Option<HeldEdge>), StartFailure> {
    let mooring = cx.mooring;
    let group = cx.group;
    let input = cx.input;
    cx.shell.at_site(command.shown.clone(), span, |shell| {
        let command = command
            .lookup(&shell.context.grants, shell.baseline())
            .admit(mooring, shell)?;
        let record = CommandRecord::begin(&command, args, CommandOrigin::External, mooring, shell);
        let launched = (|| -> Result<_, StartFailure> {
            let command = command.prepare(args, Ownership::Kept, shell)?;
            let upstream;
            let stdin = match stdin {
                ByteIn::Upstream(r) => {
                    upstream = Source::Reader(SourceReader::pipe(r));
                    &upstream
                }
                ByteIn::Parent => shell.io.stdin.offered(input),
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
            let placement = Placement::Join(group.membership(&shell.scope));
            let io =
                io.map_err(|e| crate::shell::runtime::Break::from(command.connection_error(&e)))?;
            let started = command.start(io, placement, &shell.scope)?;
            let stage = started.register(slot, group.group().owned());
            Ok((stage, held))
        })();
        match launched {
            Ok((mut stage, held)) => {
                let prov = shell.local.audit.call_site().clone();
                stage.end.settlement.frame = Some(Box::new(StageFrame { prov, record }));
                Ok((stage, held))
            }
            Err(broke) => {
                record.close(Some(&broke.reason), AuditIo::default(), mooring, shell);
                Err(broke)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lone child and a nested pipeline both preserve the stage owner's
    /// source, including EOF selected after a refused handoff.
    #[test]
    fn a_stages_external_reads_the_stages_stdin_however_it_runs() {
        for input in [TerminalInput::Readable, TerminalInput::Withheld] {
            let mut shell = crate::test_helper::core_shell();
            shell.io.stdin = Source::Terminal;
            let node = shell.scope.child();
            let given = stage_stdin(ByteIn::Parent, &shell, &node, input).expect("stage_stdin");
            let readable = matches!(input, TerminalInput::Readable);
            assert_eq!(matches!(given, Source::Terminal), readable);
            shell.io.stdin = given;
            let group = crate::process::Pgid::from_raw(1).expect("positive group");
            shell.session.stage = Some(crate::process::Membership::new(group, Scope::clone(&node)));
            let placed = Placement::command(shell.group_owner(&Mooring::adrift()));
            assert!(matches!(placed, Placement::Join(_)));
            assert_eq!(
                matches!(shell.io.stdin.offered(placed.input()), Source::Terminal),
                readable
            );
            let nested = stage_stdin(ByteIn::Parent, &shell, &node, TerminalInput::Readable)
                .expect("stage_stdin");
            assert_eq!(matches!(nested, Source::Terminal), readable);
        }
    }
}
