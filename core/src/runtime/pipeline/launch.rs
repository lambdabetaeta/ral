//! Launch phase: wire one stage's byte endpoints and start it — a ral-written
//! thread, or a direct external spawned into the pipeline's process group.

use super::super::command;
use super::super::command::Head;
use super::collect::Slot;
use super::group::PipelineGroup;
use super::resolve::{StageLaunch, StageSpec};
use super::route::{ByteIn, ByteOut, StageRoute};
use super::stage::{ExternalStage, StageHandle, StageKind};
use super::thread::launch_thread_stage;
use crate::io::{Sink, Source, SourceReader};
use crate::process::PgidPolicy;
use crate::types::{Env, Error, Mooring, Settled, Shell, Value};
use std::sync::Arc;

/// A thread reads what a child would inherit, except a tty: reading the
/// controlling terminal from the shell's own process would SIGTTIN the whole
/// shell, so it sees EOF, as under capture.  The thread's own wake ends a
/// blocked read as EOF.
pub(super) fn stage_stdin(
    route: ByteIn,
    shell: &Shell,
    wake: &Arc<crate::io::Wake>,
) -> Settled<Source> {
    let reader = match route {
        ByteIn::Upstream(r) => Some(SourceReader::pipe(r)),
        ByteIn::Parent => match &shell.io.stdin {
            Source::Empty => None,
            Source::Terminal if shell.io.terminal.startup_stdin_tty => None,
            Source::Terminal => Some(SourceReader::file(
                dup_stdin_file().map_err(|e| Error::io("could not duplicate stdin", &e))?,
            )),
            Source::Reader(r) => Some(
                r.try_clone()
                    .map_err(|e| Error::io("could not duplicate stdin", &e))?,
            ),
        },
    };
    Ok(reader.map_or(Source::Empty, |r| {
        Source::Reader(r.interruptible(Arc::clone(wake)))
    }))
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
        ByteIn::Parent => &shell.io.stdin,
    };
    let downstream;
    let stdout = match stdout {
        ByteOut::Downstream(writer, edge) => {
            let wake = crate::io::Wake::new().map_err(|e| Error::io("pipe", &e))?;
            downstream = Sink::Pipe {
                writer: Arc::new(writer),
                wake,
                edge,
            };
            &downstream
        }
        ByteOut::Parent => &shell.io.stdout,
    };
    let io = command::ChildIo {
        stdin,
        stdout,
        stderr: &shell.io.stderr,
    };
    // Inherit ral's real fd 1 so a pager or `ls` still sees a TTY.
    let inherit_tty = shell.io.terminal.startup_stdout_tty && holds_terminal;
    let grant = holds_terminal.then(command::TtyInputPermit::for_pure_external_pipeline);
    command::wire_stdio(cmd, shell, &io, grant, inherit_tty)
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

/// Dispatch one stage per its resolve-time [`StageLaunch`].
pub(super) fn spawn_stage(
    stage: &Arc<crate::ir::Comp>,
    spec: &StageSpec,
    route: StageRoute,
    cx: &mut LaunchCx<'_>,
    slot: Slot,
) -> Settled<StageHandle> {
    let StageRoute {
        stdin,
        stdout,
        held,
    } = route;
    let kind = match &spec.launch {
        StageLaunch::Direct { head, args } => StageKind::External(launch_external_stage_direct(
            head, args, stdin, stdout, cx, &slot,
        )?),
        StageLaunch::Thread => StageKind::Thread(launch_thread_stage(
            stage,
            spec,
            stdin,
            stdout,
            cx,
            slot.clone(),
        )?),
    };
    Ok(StageHandle::new(kind, held, slot))
}

/// Spawn an external stage with no thread hosting it.
fn launch_external_stage_direct(
    head: &Head,
    args: &[Value],
    stdin: ByteIn,
    stdout: ByteOut,
    cx: &mut LaunchCx<'_>,
    slot: &Slot,
) -> Settled<ExternalStage> {
    let rc = command::vet(head, args, cx.shell)?;
    let mut cmd = command::build_launch(
        &rc,
        crate::sandbox::Ownership::Kept,
        cx.shell,
        cx.mooring.cancel.as_scope(),
    )?;
    // Confinement may have taken seconds since the caller's own poll, so poll
    // again rather than spawn into an expired wall.
    cx.mooring.check()?;

    let pumps = wire_stage(&mut cmd, stdin, stdout, cx.holds_terminal, cx.shell)?;

    let (mut child, leader, jail) = command::spawn(
        &mut cmd,
        PgidPolicy::Join(cx.group.leader_pgid()),
        &rc.shown,
        cx.shell,
    )?;
    let pumps = pumps.start(&mut child);
    // A joining stage leads a group only behind an envelope, whose payload
    // leads a session of its own out of the pipeline group's reach.
    Ok(ExternalStage {
        watch: slot.watch(child),
        name: rc.shown,
        args: rc.admitted.args().to_vec(),
        jail,
        pumps,
        envelope: leader,
    })
}
