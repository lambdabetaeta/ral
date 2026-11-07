//! Launch phase: wire one stage's byte endpoints and start it — a ral-written
//! thread, or a direct external spawned into the pipeline's process group.

use super::super::command;
use super::super::command::{Born, ChildOut, Head, Started};
use super::collect::Settlement;
use super::collect::Slot;
use super::group::PipelineGroup;
use super::resolve::StageLaunch;
use super::route::{ByteIn, ByteOut, StageRoute};
use super::stage::{Drain, ExternalEnd, LiveExternal, StageAudit, StageHandle, StageKind};
use super::thread::launch_thread_stage;
use crate::error::io_message;
use crate::fact::AuditIo;
use crate::io::{Sink, Source, SourceReader};
use crate::process::{Group, Scope, Spawned};
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
        ByteIn::Parent => match command::boundary_stdin(shell, holds_terminal) {
            Source::Empty => return Ok(Source::Empty),
            Source::Terminal => return Ok(Source::Terminal),
            Source::Reader(r) => r
                .try_clone()
                .map_err(|e| Error::new(io_message("could not duplicate stdin", &e)))?,
        },
    };
    Ok(Source::Reader(reader.interruptible(scope.clone())))
}

/// Wire a direct external stage: its upstream pipe end is the source
/// `wire_stdio` sees, the parent's stdin filling the boundary edge.
fn wire_stage(
    cmd: &mut crate::process::Launch,
    stdin: ByteIn,
    stdout: ChildOut,
    holds_terminal: bool,
    shell: &Shell,
) -> Settled<command::Pumps<Sink>> {
    let upstream;
    let stdin = match stdin {
        ByteIn::Upstream(r) => {
            upstream = Source::Reader(SourceReader::pipe(r));
            &upstream
        }
        ByteIn::Parent => command::boundary_stdin(shell, holds_terminal),
    };
    command::wire_stdio(cmd, stdin, stdout, &shell.io.stderr)
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
/// Its stdout is resolved here, once for either kind: an edge's write end and
/// fate go to the stage, its hold to the stage's handle.
pub(super) fn spawn_stage(
    stage: &Arc<crate::ir::Comp>,
    launch: StageLaunch,
    route: StageRoute,
    cx: &mut LaunchCx<'_>,
    slot: Slot,
) -> Settled<StageHandle> {
    let StageRoute { stdin, stdout } = route;
    let (stdout, held) = match stdout {
        ByteOut::Parent => (ChildOut::Sink(cx.shell.io.stdout.clone()), None),
        ByteOut::Downstream { writer, held } => {
            let edge = Arc::clone(&held.edge);
            (ChildOut::Edge { writer, edge }, Some(held))
        }
    };
    let name = match &stage.item {
        crate::ir::CompKind::Exec(exec) => exec.head.name().written(),
        _ => "pipeline".into(),
    };
    let span = stage.span;
    let kind = match launch {
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
    stdout: ChildOut,
    cx: &mut LaunchCx<'_>,
    slot: &Slot,
) -> Settled<LiveExternal> {
    let name = head.shown.clone();
    let mooring = cx.mooring;
    let group = cx.group;
    let holds_terminal = cx.holds_terminal;
    cx.shell.at_site(name.clone(), span, |shell| {
        let audit_start = shell.audit_start(mooring);
        let head = head.admit(mooring, shell)?;
        let launched = (|| -> Settled<_> {
            let mut prepared = command::prepare(
                head,
                args,
                crate::authority::sandbox::Ownership::Kept,
                shell,
            )?;
            let pumps = wire_stage(&mut prepared.launch, stdin, stdout, holds_terminal, shell)?;
            let policy =
                command::member_policy(group.membership(&shell.scope), prepared.launch.enveloped());
            let started = command::start(prepared, policy, &shell.scope)?;
            Ok((started, pumps))
        })();
        let (started, pumps) = launched.inspect_err(|broke| {
            if let Some(start) = &audit_start {
                let obs = shell.command_observation(
                    start,
                    &name,
                    Value::render_argv(args),
                    CommandOrigin::External,
                    Some(broke),
                    AuditIo::default(),
                );
                shell.observe_stamped(Some(mooring), obs);
            }
        })?;
        let Started {
            shown,
            args,
            born:
                Born {
                    spawned:
                        Spawned {
                            mut child,
                            group: landed,
                            jail,
                        },
                    claim,
                },
        } = started;
        let pumps = pumps.start(&mut child, &shell.scope);
        let payload = match landed {
            Group::Enveloped(payload) => Some(payload),
            Group::Owns(_) | Group::Joins(_) | Group::Shares => None,
        };
        Ok(LiveExternal {
            watch: slot.watch(child),
            payload,
            end: ExternalEnd {
                drain: Drain { jail, pumps },
                settlement: Settlement {
                    name: shown,
                    audit: Some(Box::new(StageAudit {
                        prov: shell.local.audit.call_site().clone(),
                        start: audit_start,
                        args,
                    })),
                    claim,
                },
            },
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::PgidPolicy;

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
            let placed = command::placement(&shell, &Mooring::adrift(), false);
            assert!(matches!(placed.policy, PgidPolicy::Join(_)) && placed.loan.is_none());
            // Joined, the lone child and the nested stage read the same source,
            // whatever their group could read.
            for readable in [true, false] {
                assert!(expected(command::boundary_stdin(&shell, readable)));
                let nested =
                    stage_stdin(ByteIn::Parent, &shell, &node, readable).expect("stage_stdin");
                assert!(expected(&nested));
            }
        }
    }
}
