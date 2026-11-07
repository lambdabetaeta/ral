//! The external arm of command dispatch — `dispatch` picks it over
//! env and handler: prepare the admitted head, wire the shell's streams
//! into child stdio, start it under the canonical pgid and sandbox, and hand
//! it to the pipeline collector, every external's one waiter.
//!
//! Pipeline stages never reach [`run`] — they take
//! [`super::pipeline::PipeNode::run`] — but a direct stage shares
//! [`prepare`], [`start`], the collector and [`ending`] with it, so both
//! paths resolve, confine, start, wait on and end a call the same way.

use crate::authority::sandbox::Ownership;
use crate::io::Sink;
use crate::process::{POLL_SLICE, PgidPolicy, Scope};
use crate::shell::runtime::{Mooring, Settled, Shell, Value};

#[cfg(unix)]
mod detach;
mod foreground;
mod head;
pub(crate) mod process;
mod stdio;
mod vet;

#[cfg(unix)]
pub(crate) use detach::detach;
pub(crate) use foreground::placement;
pub(crate) use head::{Admission, Head};
pub(crate) use process::{Born, Claim, Started, ending, prepare, start};
pub(crate) use stdio::{ChildOut, boundary_stdin, wire_stdio};

/// Runs a standalone external call from vetting to reap.  A bundled
/// coreutils/diffutils/ripgrep head takes the same path as any host
/// executable: its image is `ral --ral-bundled-tool <tool>`.
pub(crate) fn run(
    head: Head<Admission>,
    args: &[Value],
    mooring: &Mooring,
    shell: &Shell,
) -> Settled<Value> {
    let mut prepared = prepare(head, args, Ownership::Kept, shell)?;
    let (policy, lending) = placement(shell, mooring, prepared.launch.enveloped());
    let readable = lending.is_some() || matches!(policy, PgidPolicy::Inherit);
    let pumps = wire_stdio(
        &mut prepared.launch,
        boundary_stdin(&shell.io, readable),
        ChildOut::Sink(shell.io.stdout.clone()),
        &shell.io.stderr,
    )?;

    announce_command_title(&prepared.shown, shell);

    // Anchor the denial-log window before the spawn, so a kernel deny the
    // child logs falls inside what `authority::sandbox::denial_hint` reads back.
    let started = std::time::Instant::now();
    // `finish_command` builds the `Command{External}` observation from
    // whatever error reaches it, so a start failure needs no emission of
    // its own here.
    let Started { shown, born, .. } = start(prepared, policy)?;
    // Its `Drop` restores ral's pgid on every path out of here, sparing the
    // next REPL tty read an EIO from a background pgroup.
    let loan = lending.and_then(|loan| loan.lend(born.spawned.child.id().cast_signed()));

    // Nothing fallible may run between `start` and here: until the collector
    // holds it the bare child leaks on an early return.
    super::pipeline::join_lone(born, pumps, loan, shown, started, shell).map(|()| Value::Unit)
}

/// A child's stdout/stderr pumps: planned as the [`Sink`] each drains into,
/// `None` where that fd is inherited or wired straight to an OS pipe, then
/// started as the drainer threads over the child's pipes.  A reader-gone kill's
/// remaining bytes are owed to nobody, and a descendant it missed could hold the
/// pipe for ever, so that ending detaches, as does every forced end; every
/// other joins, after the kill that frees the pipe.
pub(crate) struct Pumps<T = std::thread::JoinHandle<()>> {
    stdout: Option<T>,
    stderr: Option<T>,
}

impl<T> Default for Pumps<T> {
    fn default() -> Self {
        Self {
            stdout: None,
            stderr: None,
        }
    }
}

impl Pumps<Sink> {
    pub(crate) const fn new(stdout: Option<Sink>, stderr: Option<Sink>) -> Self {
        Self { stdout, stderr }
    }

    /// Start the planned drainers over `child`'s piped stdout/stderr, each
    /// reading `scope`, the launching computation's current scope.
    pub(crate) fn start(self, child: &mut crate::process::ChildHandle, scope: &Scope) -> Pumps {
        Pumps {
            stdout: self
                .stdout
                .and_then(|sink| child.take_stdout().map(|s| sink.pump(s, scope.clone()))),
            stderr: self
                .stderr
                .and_then(|sink| child.take_stderr().map(|s| sink.pump(s, scope.clone()))),
        }
    }
}

impl Pumps {
    /// Join both drainers a slice at a time; dropping `self` instead detaches
    /// them.  On a cause in force on `scope`, `kill` once: a pump a leader's
    /// survivor holds open ends only when the survivor's pipe closes.  On
    /// Windows, where a pipe write cannot be polled, the pumps are interrupted
    /// every slice too: one blocked on an edge a teardown left undrained would
    /// otherwise wait for a read end the collector holds until this very stage
    /// files.
    pub(crate) fn join_under(self, scope: &Scope, kill: impl FnOnce()) {
        let handles: Vec<_> = [self.stdout, self.stderr].into_iter().flatten().collect();
        let mut kill = Some(kill);
        while !handles.iter().all(std::thread::JoinHandle::is_finished) {
            if scope.is_cancelled() {
                if let Some(kill) = kill.take() {
                    kill();
                }
                #[cfg(windows)]
                handles.iter().for_each(crate::io::interrupt);
            }
            std::thread::sleep(POLL_SLICE);
        }
        for jh in handles {
            let _ = jh.join();
        }
    }
}

/// Announce the running command via the terminal title (OSC 0).
fn announce_command_title(cmd: &str, shell: &Shell) {
    if shell.io.interactive && shell.io.terminal.ui_title_ok() {
        use std::io::Write;
        let _ = std::io::stdout().write_all(crate::io::ansi::osc_set_title(cmd).as_bytes());
        let _ = std::io::stdout().flush();
    }
}
