//! Connect shell streams to child descriptors and own their output drains.
//!
//! A connection owns both ends of every drain pipe: installation gives the
//! writer to the launch and retains the reader with its destination. Starting
//! drains therefore needs no access to the child or its standard streams.

use super::{Sink, Source};
use crate::process::{Launch, POLL_SLICE, Scope, StdioSpec};
use std::io::{self, Read};
use std::thread::JoinHandle;

/// Prepared child endpoints and the drains belonging to exactly those endpoints.
pub(crate) struct Connection {
    stdin: StdioSpec,
    stdout: StdioSpec,
    stderr: Stderr,
    drains: PendingDrains,
}

/// Stderr either has its own endpoint or aliases the final stdout endpoint.
enum Stderr {
    Separate(StdioSpec),
    Stdout,
}

#[derive(Clone, Copy)]
enum OutFd {
    Stdout,
    Stderr,
}

impl Connection {
    /// Borrow the shell's streams, merging outputs that share a destination.
    pub(crate) fn new(stdin: &Source, stdout: &Sink, stderr: &Sink) -> io::Result<Self> {
        let stdin = stdin.child_stdio()?;
        let mut drains = PendingDrains::default();
        let out = drains.bind(stdout, OutFd::Stdout)?;
        let stderr = if stderr.same_destination(stdout) {
            Stderr::Stdout
        } else {
            Stderr::Separate(drains.bind(stderr, OutFd::Stderr)?)
        };
        Ok(Self {
            stdin,
            stdout: out,
            stderr,
            drains,
        })
    }

    /// Consume a downstream pipe; only stderr can require a drain.
    pub(crate) fn to_pipe(
        stdin: &Source,
        stdout: os_pipe::PipeWriter,
        stderr: &Sink,
    ) -> io::Result<Self> {
        let stdin = stdin.child_stdio()?;
        let mut drains = PendingDrains::default();
        let stderr = Stderr::Separate(drains.bind(stderr, OutFd::Stderr)?);
        Ok(Self {
            stdin,
            stdout: StdioSpec::from_owned(stdout),
            stderr,
            drains,
        })
    }

    /// Transfer child-only endpoints to the launch. All preparation that can
    /// fail is complete; the caller keeps the launch and drains together.
    pub(crate) fn install(self, launch: &mut Launch) -> PendingDrains {
        launch.stdin(self.stdin).stdout(self.stdout);
        match self.stderr {
            Stderr::Separate(stderr) => {
                launch.stderr(stderr);
            }
            Stderr::Stdout => launch.dup_stdout_to_stderr(),
        }
        self.drains
    }
}

/// Lend the shell's own standard stream, so a child writing to the other
/// one reaches it directly, as inheritance would.
#[cfg(unix)]
fn lend(stream: &impl std::os::fd::AsFd) -> io::Result<StdioSpec> {
    stream
        .as_fd()
        .try_clone_to_owned()
        .map(StdioSpec::from_owned)
}

#[cfg(windows)]
fn lend(stream: &impl std::os::windows::io::AsHandle) -> io::Result<StdioSpec> {
    stream
        .as_handle()
        .try_clone_to_owned()
        .map(StdioSpec::from_owned)
}

/// Readers paired with their destinations before any drain thread exists.
#[derive(Default)]
pub(crate) struct PendingDrains(Vec<PendingDrain>);

struct PendingDrain {
    reader: os_pipe::PipeReader,
    sink: Sink,
}

impl PendingDrains {
    /// Inherit the matching standard stream, lend the other standard stream,
    /// files and pipes, and drain all other destinations through an
    /// explicitly owned pipe.
    fn bind(&mut self, sink: &Sink, fd: OutFd) -> io::Result<StdioSpec> {
        match (sink, fd) {
            (Sink::Stdout, OutFd::Stdout) | (Sink::Stderr, OutFd::Stderr) => Ok(StdioSpec::Inherit),
            (Sink::Stdout, OutFd::Stderr) => lend(&io::stdout()),
            (Sink::Stderr, OutFd::Stdout) => lend(&io::stderr()),
            (Sink::File(file), _) => Ok(StdioSpec::from_owned(file.try_clone()?)),
            (Sink::Pipe { writer, .. }, _) => Ok(StdioSpec::from_owned(writer.try_clone()?)),
            _ => {
                let (reader, writer) = crate::process::cloexec_pipe()?;
                self.0.push(PendingDrain {
                    reader,
                    sink: sink.clone(),
                });
                Ok(StdioSpec::from_owned(writer))
            }
        }
    }

    /// Start every drain under the launching computation's scope, before
    /// its child exists. If a thread cannot start, dropping the launch
    /// closes every writer: the drains already started end at EOF, having
    /// carried nothing, and no child is left to tear down.
    pub(crate) fn start(self, scope: &Scope) -> io::Result<Drains> {
        self.0
            .into_iter()
            .map(|drain| {
                let scope = scope.clone();
                std::thread::Builder::new().spawn(move || drain.run(&scope))
            })
            .collect::<io::Result<Vec<_>>>()
            .map(Drains)
    }
}

impl PendingDrain {
    /// Drain to EOF even after a destination write fails or stops: closing
    /// the pipe under the child would change its ending. Flush watch tails
    /// only at EOF. Windows teardown may interrupt either a read or a write.
    fn run(self, scope: &Scope) {
        let Self {
            mut reader,
            mut sink,
        } = self;
        let mut buf = [0u8; 8 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = sink.write_under(scope, &buf[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                #[cfg(windows)]
                Err(e) if super::aborted(&e) => {}
                Err(_) => break,
            }
        }
        sink.flush_pending();
    }
}

/// Running drains. Ordinary settlement joins them after owner teardown and
/// before reporting success or failure, including a failed start.
/// Dropping stays non-blocking for unwinding: it detaches rather than waiting
/// for descendants that may still hold a pipe.
#[derive(Debug, Default)]
pub(crate) struct Drains(Vec<JoinHandle<()>>);

impl Drains {
    /// On cancellation, kill once to release writers. Windows also needs
    /// repeated interruption of drains blocked writing to an undrained edge.
    pub(crate) fn join_under(self, scope: &Scope, kill: impl FnOnce()) {
        let mut kill = Some(kill);
        while !self.0.iter().all(JoinHandle::is_finished) {
            if scope.is_cancelled() {
                if let Some(kill) = kill.take() {
                    kill();
                }
                #[cfg(windows)]
                self.0.iter().for_each(super::interrupt);
            }
            std::thread::sleep(POLL_SLICE);
        }
        for handle in self.0 {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
