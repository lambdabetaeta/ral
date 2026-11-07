//! Launch phase: wire one stage's byte endpoints and start it — a ral-written
//! thread, or a direct external spawned into the pipeline's process group.

use super::super::command;
use super::super::command::{Born, Head, Started};
use super::collect::Slot;
use super::group::PipelineGroup;
use super::resolve::{StageLaunch, StageSpec};
use super::route::{ByteIn, ByteOut, StageRoute};
use super::stage::{ExternalStage, StageHandle, StageKind};
use super::thread::launch_thread_stage;
use crate::io::{Sink, Source, SourceReader};
use crate::process::{PgidPolicy, Scope, Spawned};
use crate::shell::runtime::{Env, Error, Mooring, Settled, Shell, Value};
use crate::source::Span;
use std::sync::Arc;

/// A thread stage's stdin: its boundary's, where a tty passes as the
/// terminal (its externals inherit it, `stdin_reader` refuses it), and any
/// other fd as a reader its scope interrupts.
pub(super) fn stage_stdin(
    route: ByteIn,
    shell: &Shell,
    scope: &Scope,
    holds_terminal: bool,
) -> Settled<Source> {
    let reader = match route {
        ByteIn::Upstream(r) => SourceReader::pipe(r),
        ByteIn::Parent => match command::boundary_stdin(&shell.io, holds_terminal) {
            Source::Empty => return Ok(Source::Empty),
            Source::Terminal if shell.io.terminal.startup_stdin_tty => {
                return Ok(Source::Terminal);
            }
            Source::Terminal => SourceReader::file(
                dup_stdin_file().map_err(|e| Error::io("could not duplicate stdin", &e))?,
            ),
            Source::Reader(r) => r
                .try_clone()
                .map_err(|e| Error::io("could not duplicate stdin", &e))?,
        },
    };
    Ok(Source::Reader(reader.interruptible(scope.clone())))
}

#[cfg(unix)]
fn dup_stdin_file() -> std::io::Result<std::fs::File> {
    use std::os::fd::AsFd;
    std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map(std::fs::File::from)
}

#[cfg(windows)]
fn dup_stdin_file() -> std::io::Result<std::fs::File> {
    use std::os::windows::io::AsHandle;
    std::io::stdin()
        .as_handle()
        .try_clone_to_owned()
        .map(std::fs::File::from)
}

/// Wire a direct external stage: its pipe ends are the source and sink
/// `wire_stdio` sees, and the parent's streams fill the boundary edges.
fn wire_stage(
    cmd: &mut crate::process::Launch,
    stdin: ByteIn,
    stdout: ByteOut,
    holds_terminal: bool,
    shell: &Shell,
) -> Settled<command::Pumps<Sink>> {
    let upstream;
    let stdin = match stdin {
        ByteIn::Upstream(r) => {
            upstream = Source::Reader(SourceReader::pipe(r));
            &upstream
        }
        ByteIn::Parent => command::boundary_stdin(&shell.io, holds_terminal),
    };
    let stdout = match stdout {
        ByteOut::Downstream(writer, _edge) => command::ChildOut::Edge(writer),
        ByteOut::Parent => command::ChildOut::Sink(&shell.io.stdout),
    };
    let io = command::ChildIo {
        stdin,
        stdout,
        stderr: &shell.io.stderr,
    };
    command::wire_stdio(cmd, io)
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
/// stage's head leaves by value: nothing holds the program after it launches.
pub(super) fn spawn_stage(
    stage: &Arc<crate::ir::Comp>,
    spec: StageSpec,
    route: StageRoute,
    cx: &mut LaunchCx<'_>,
    slot: Slot,
) -> Settled<StageHandle> {
    let StageRoute {
        stdin,
        stdout,
        held,
    } = route;
    let name = match &stage.item {
        crate::ir::CompKind::Exec(exec) => exec.head.name().written(),
        _ => "pipeline".into(),
    };
    let span = spec.span;
    let kind = match spec.launch {
        StageLaunch::Direct { head, args } => {
            launch_external_stage_direct(head, &args, span, stdin, stdout, cx, &slot)
                .map(StageKind::External)
        }
        StageLaunch::Thread => {
            launch_thread_stage(stage, stdin, stdout, cx, slot.clone()).map(StageKind::Thread)
        }
    }
    .map_err(|b| b.locate(&name, span, cx.shell))?;
    Ok(StageHandle::new(kind, held, slot))
}

/// Spawn an external stage with no thread hosting it.
fn launch_external_stage_direct(
    head: Head,
    args: &[Value],
    span: Option<Span>,
    stdin: ByteIn,
    stdout: ByteOut,
    cx: &mut LaunchCx<'_>,
    slot: &Slot,
) -> Settled<ExternalStage> {
    let name = head.shown.clone();
    let mooring = cx.mooring;
    let group = cx.group;
    let holds_terminal = cx.holds_terminal;
    cx.shell.at_site(name, span, |shell| {
        let head = head.admit(mooring, shell)?;
        let mut prepared = command::prepare(
            head,
            args,
            crate::authority::sandbox::Ownership::Kept,
            shell,
        )?;
        let pumps = wire_stage(&mut prepared.launch, stdin, stdout, holds_terminal, shell)?;
        let Started {
            shown,
            args,
            born:
                Born {
                    spawned:
                        Spawned {
                            mut child,
                            landing,
                            jail,
                        },
                    claim,
                },
        } = command::start(prepared, PgidPolicy::Join(group.leader_pgid()))?;
        let pumps = pumps.start(&mut child, &shell.scope);
        Ok(ExternalStage {
            watch: slot.watch(child),
            prov: shell.local.audit.call_site().clone(),
            name: shown,
            args,
            jail,
            pumps,
            landing,
            claim,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_startup_tty_reaches_a_thread_stage_only_when_the_group_holds_it() {
        let mut shell = crate::test_helper::core_shell();
        shell.io.terminal.startup_stdin_tty = true;
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
            shell.io.terminal.startup_stdin_tty = true;
            shell.io.stdin = Source::Terminal;
            let node = shell.scope.child();
            let given = stage_stdin(ByteIn::Parent, &shell, &node, lent).expect("stage_stdin");
            assert!(expected(&given));

            // The stage's own `Io`, as `launch_thread_stage` builds it.
            let group = crate::process::Pgid::from_raw(1).expect("1 is positive");
            shell.io.stdin = given;
            shell.io.stage = Some(crate::process::Membership::new(group, Scope::clone(&node)));
            let (policy, lending) = command::placement(&shell, &Mooring::adrift(), false);
            assert!(matches!(policy, PgidPolicy::Join(_)) && lending.is_none());
            // Joined, the lone child and the nested stage read the same source,
            // whatever their group could read.
            for readable in [true, false] {
                assert!(expected(command::boundary_stdin(&shell.io, readable)));
                let nested =
                    stage_stdin(ByteIn::Parent, &shell, &node, readable).expect("stage_stdin");
                assert!(expected(&nested));
            }
        }
    }
}
