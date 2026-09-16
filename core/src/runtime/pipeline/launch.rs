//! Launch phase: wire one stage's byte endpoints and start it — a ral-written
//! thread, or a direct external spawned into the pipeline's process group.

use super::super::command;
use super::super::command::CommandIdentity;
use super::collect::Slot;
use super::group::PipelineGroup;
use super::resolve::{StageLaunch, StageSpec};
use super::route::{ByteIn, ByteOut, StageRoute};
use super::stage::{ExternalStage, StageHandle, StageKind};
use super::thread::launch_thread_stage;
use crate::io::{Sink, Source, SourceReader};
use crate::process::PgidPolicy;
use crate::types::{Env, Mooring, Settled, Shell, Value};
use std::sync::Arc;

/// Route stdin for the stage on the pipeline's input boundary, consuming
/// whatever `<file` or parent pipe sits on `shell.io.stdin` — otherwise
/// `f < file` on a function whose body is a pipeline drops the redirect.
/// Unlike `command::stdio::wire_stdin`, a tty fd 0 is inherited only when this
/// pgid will own the terminal; a backgrounded reader takes SIGTTIN.
fn route_parent_stdin(group: &PipelineGroup, shell: &Shell) -> Settled<command::StdinRoute> {
    // `Source::Empty` — an exarch tool run — denies byte input outright.
    if matches!(shell.io.stdin, Source::Empty) {
        return Ok(command::StdinRoute::Null);
    }
    let reader = shell.io.stdin.reader().map_err(command::stdin_error)?;
    Ok(match reader {
        Some(r) => command::StdinRoute::Reader(r),
        None if !shell.io.terminal.startup_stdin_tty => {
            command::StdinRoute::Inherit(command::TtyInputPermit::for_non_tty_stdin())
        }
        None if group.holds_terminal() => {
            command::StdinRoute::Inherit(command::TtyInputPermit::for_pure_external_pipeline())
        }
        None => command::StdinRoute::Null,
    })
}

/// A thread reads what a child would inherit, except a tty: reading the
/// controlling terminal from the shell's own process would SIGTTIN the whole
/// shell, so it sees EOF, as under capture.  The thread's own wake ends a
/// blocked read as EOF.
pub(super) fn stage_stdin(
    route: ByteIn,
    shell: &Shell,
    wake: &Arc<crate::process::Wake>,
) -> Settled<Source> {
    let reader = match route {
        ByteIn::Upstream(r) => Some(SourceReader::pipe(r)),
        ByteIn::Parent => match &shell.io.stdin {
            Source::Empty => None,
            Source::Terminal if shell.io.terminal.startup_stdin_tty => None,
            Source::Terminal => Some(SourceReader::file(
                dup_stdin_file().map_err(command::stdin_error)?,
            )),
            Source::Reader(r) => Some(r.try_clone().map_err(command::stdin_error)?),
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

/// Wire a stage's stdout against its [`ByteOut`] edge, returning the sink the
/// parent must pump when the boundary stdout is not a plain fd.
pub(super) fn wire_stage_stdout(
    cmd: &mut crate::process::Launch,
    stdout: ByteOut,
    group: &PipelineGroup,
    shell: &Shell,
) -> Settled<Option<Sink>> {
    match stdout {
        ByteOut::Downstream(writer, _edge) => {
            cmd.stdout(crate::process::StdioSpec::from_pipe_writer(writer));
            Ok(None)
        }
        ByteOut::Parent => {
            // Inherit ral's real fd 1 so a pager or `ls` still sees a TTY.
            let inherit = shell.io.terminal.startup_stdout_tty && group.holds_terminal();
            let plan = shell
                .io
                .stdout
                .child_stdout(inherit)
                .map_err(super::route::pipe_error)?;
            cmd.stdout(plan.stdio);
            Ok(plan.pump)
        }
    }
}

/// Wire a direct external stage's stdin, stdout, and stderr from its route.
pub(super) fn wire_stage_stdio(
    cmd: &mut crate::process::Launch,
    stdin: ByteIn,
    stdout: ByteOut,
    group: &PipelineGroup,
    shell: &Shell,
) -> Settled<command::ExternalPlumbing> {
    let inbound = match stdin {
        ByteIn::Upstream(r) => command::StdinRoute::Reader(SourceReader::pipe(r)),
        ByteIn::Parent => route_parent_stdin(group, shell)?,
    };
    cmd.stdin(inbound.into_stdio());
    let stdout_pump = wire_stage_stdout(cmd, stdout, group, shell)?;
    let stderr_plan = shell
        .io
        .stderr
        .child_stderr()
        .map_err(super::route::pipe_error)?;
    cmd.stderr(stderr_plan.stdio);
    Ok(command::ExternalPlumbing {
        stdout_pump,
        stderr_pump: stderr_plan.pump,
    })
}

pub(super) struct LaunchCx<'a> {
    pub(super) mooring: &'a Mooring,
    pub(super) shell: &'a mut Shell,
    /// The pipeline node's own lexical environment — a thread stage's captured
    /// closure env, distinct from `shell.env` inside a nested machine.
    pub(super) env: &'a Env,
    pub(super) group: &'a PipelineGroup,
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
        StageLaunch::Direct { id, args } => StageKind::External(launch_external_stage_direct(
            id, args, stdin, stdout, cx, &slot,
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
    id: &CommandIdentity,
    args: &[Value],
    stdin: ByteIn,
    stdout: ByteOut,
    cx: &mut LaunchCx<'_>,
    slot: &Slot,
) -> Settled<ExternalStage> {
    let rc = command::vet(id, args, cx.shell)?;
    let mut cmd = command::build_command(
        &rc,
        crate::sandbox::Ownership::Kept,
        cx.shell,
        cx.mooring.cancel.as_scope(),
    )?;
    // Confinement may have taken seconds since the caller's own poll, so poll
    // again rather than spawn into an expired wall.
    crate::process::check(cx.mooring)?;

    let plumbing = wire_stage_stdio(&mut cmd, stdin, stdout, cx.group, cx.shell)?;

    let confinement = cmd.confinement();
    let (mut child, leader, jail) = cmd
        .spawn(PgidPolicy::Join(cx.group.leader_pgid()))
        .map_err(|e| command::spawn_error(confinement, &rc.shown, &e))?;
    if cx.shell.has_active_capabilities() {
        crate::sandbox::apply_child_limits_in_pipeline(&child, cx.group.leader_pgid());
    }
    let pumps = command::Pumps::spawn(plumbing, &mut child);
    // Behind an envelope the payload leads a session of its own (§3.2), out
    // of the pipeline group's reach, so its address is kept.
    let envelope = confinement.and(leader);
    Ok(ExternalStage {
        watch: slot.watch(child),
        name: rc.shown,
        args: rc.args,
        jail,
        pumps,
        envelope,
    })
}
