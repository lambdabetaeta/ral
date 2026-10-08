//! The agent's seat: the transport its runs go through, plus whatever
//! host-side state that seat kind owns.  Every engine-side reach is a
//! method here, so a new seat kind is one more variant, not a second agent.

use crate::app::Scratch;
use crate::cancel::InterruptTarget;
use crate::shell_eval::builtins;
use ral_core::carrier::{IdentityTransport, ProbeError, Severed, Transport};
use ral_core::engine::EngineInstaller;
use ral_core::protocol::Attach;
use std::sync::Arc;

/// How a fork of this seat's engine reaches the desk that adopts it: parked in
/// the parent's own transport, or dialled across a wire. `` `start `` and
/// `/branch` choose their arm on this fact alone.
pub(crate) enum SeatKind {
    Identity(Arc<IdentityTransport>),
    /// The attach this seat was given: a hatched child is attached alike.
    /// Its seed carries the live cwd and overrides this one on hydration, so
    /// no probe is owed per call.
    Wire {
        cwd: std::path::PathBuf,
        home: std::path::PathBuf,
    },
}

/// One agent's engine-side attachment; what differs per call already lives
/// off the [`Transport`] trait, so this stays a closed enum.
pub(crate) enum Seat {
    /// In-process.  `/clear` reboots it onto the *same* `target`: the cell an
    /// interrupt reaches the run through must outlive the rebuild.
    Identity {
        transport: Arc<IdentityTransport>,
        target: InterruptTarget,
        /// `None` for an adopted fork, whose authority a fresh boot would
        /// not carry.
        rebirth: Option<Rebirth>,
    },
    /// Out-of-process, one engine per session: a fork is hatched guest-side
    /// and dialled by the desk's wire arm, attached as this seat was.
    Wire {
        transport: Box<ral_core::carrier::WireTransport>,
        target: InterruptTarget,
        cwd: std::path::PathBuf,
        home: std::path::PathBuf,
    },
}

/// What an identity root reboots from: the recipes and Attach it was born
/// from, and the scratch that Attach names, which outlives every reboot.
pub(crate) struct Rebirth {
    installers: &'static [EngineInstaller],
    attach: Attach,
    _scratch: Arc<Scratch>,
}

/// When in a session's life its engine was lost, which is the whole of what
/// decides what a person should do next.
///
/// A start failure is a thing to retry; a mid-session death is a thing to
/// abandon — telling someone who has already produced something to "start a
/// new session" is advice for a conversation that never began.  Nothing
/// else about the two cases differs, which is why this is a two-variant enum
/// and not a description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnginePhase {
    /// The engine went before the session ever took a turn — the attach was
    /// refused, or the engine died between being spawned and answering it.
    Starting,
    /// The engine went with a session already under way, taking the
    /// conversation's whole state with it.
    Running,
}

/// The one thing every edge says about a severed engine, said once.
///
/// The transport, [`Severed`]'s `Display`, the seat, and the front-end all
/// report through this single, short, fixed sentence rather than each
/// wrapping the fact in its own words.  The one thing beside it is the one
/// thing a reader can act on: the run's log directory.
///
/// Neither the engine's own words nor [`Severed::code`] belong in a window —
/// a paragraph of machinery and a token for a bug report.  Both go to the
/// log, which is what [`Self::logged`] is for and why the sentence names it.
///
/// It is an [`Error`](std::error::Error) so that the one edge that has to
/// cross an [`io::Error`](std::io::Error) — `Avatar::root`, whose other
/// failures are ordinary filesystem ones — can carry it whole rather than
/// flattened to a string.  A front-end downcasts to tell a severance from a
/// log directory it could not create, and so knows whether it has a guest
/// console worth capturing before it tears the machine down.
#[derive(Debug)]
pub struct EngineLost {
    phase: EnginePhase,
    cause: Severed,
    /// The run directory, not the session's: a start failure has no session
    /// worth naming, and the engine's captured output is a property of the
    /// run.  `None` where the caller genuinely has no log to point at, in
    /// which case the sentence simply omits the invitation rather than
    /// sending the reader somewhere that does not exist.
    log_dir: Option<std::path::PathBuf>,
}

impl EngineLost {
    /// The engine went before the session ever took a turn.
    pub fn starting(cause: &Severed, log_dir: Option<&std::path::Path>) -> Self {
        Self {
            phase: EnginePhase::Starting,
            cause: cause.clone(),
            log_dir: log_dir.map(std::path::Path::to_path_buf),
        }
    }

    /// The engine went with a session already under way.
    pub fn running(cause: &Severed, log_dir: Option<&std::path::Path>) -> Self {
        Self {
            phase: EnginePhase::Running,
            cause: cause.clone(),
            log_dir: log_dir.map(std::path::Path::to_path_buf),
        }
    }

    /// The run directory this failure invites a reader into, if there is one.
    #[must_use]
    pub fn log_dir(&self) -> Option<&std::path::Path> {
        self.log_dir.as_deref()
    }

    #[must_use]
    pub const fn phase(&self) -> EnginePhase {
        self.phase
    }

    /// Why the transport says the engine is gone, in its own words — the
    /// paragraph kept out of the user's sentence.  Written into files and
    /// durable records; never shown as the whole of a failure.
    #[must_use]
    pub fn cause(&self) -> &Severed {
        &self.cause
    }

    /// The form that belongs in a log rather than in a window: the sentence a
    /// user is shown, then the code to quote and the engine's own account of
    /// itself, so the durable record keeps what the sentence dropped.
    #[must_use]
    pub fn logged(&self) -> String {
        format!("{self}\n\n({}) {}", self.cause.code(), self.cause)
    }
}

impl std::error::Error for EngineLost {}

/// What happened and what to do, in two full stops, then a bracket holding
/// the one thing worth following: a path.  A dash would ask the reader which
/// half is the advice, and a code beside the path is a word they cannot act
/// on standing where the thing they can act on should be.
impl std::fmt::Display for EngineLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sentence = match (self.phase, &self.cause) {
            // A refusal is deterministic: trying again meets it again.
            (EnginePhase::Starting, Severed::Refused(_)) => "The assistant could not be started.",
            (EnginePhase::Starting, _) => "The assistant could not be started. Try again.",
            (EnginePhase::Running, _) => {
                "The assistant stopped, so this conversation cannot go on. Start a new one."
            }
        };
        f.write_str(sentence)?;
        match &self.log_dir {
            Some(dir) => write!(f, " (details in {})", dir.display()),
            None => Ok(()),
        }
    }
}

impl Seat {
    /// An identity root, booted through `installers` from what this session
    /// states: its cwd, terminal, scratch, and log directory.
    ///
    /// # Errors
    /// The recipe's refusal.
    pub(crate) fn root(
        installers: &'static [EngineInstaller],
        cwd: std::path::PathBuf,
        terminal: ral_core::terminal::TerminalState,
        scratch: Arc<Scratch>,
        session_dir: &std::path::Path,
    ) -> Result<Self, Severed> {
        let home = ral_core::host::home().unwrap_or_default();
        let mut attach = Attach::new(builtins::INSTALLER_TAG, cwd, home.into());
        attach.terminal = terminal;
        attach.env = scratch.env();
        attach.env.push((
            "EXARCH_SESSION_DIR".into(),
            session_dir.to_string_lossy().into_owned(),
        ));
        let transport = Arc::new(IdentityTransport::boot(installers, &attach)?);
        Ok(Self::Identity {
            target: InterruptTarget::new(transport.control().clone()),
            transport,
            rebirth: Some(Rebirth {
                installers,
                attach,
                _scratch: scratch,
            }),
        })
    }

    /// A fork its parent's transport adopted.
    pub(crate) fn adopted(transport: IdentityTransport) -> Self {
        Self::Identity {
            target: InterruptTarget::new(transport.control().clone()),
            transport: Arc::new(transport),
            rebirth: None,
        }
    }

    /// `cwd` and `home` are the caller's word, never read from this
    /// process: under a VM they are guest paths this host cannot resolve.
    ///
    /// # Errors
    /// The transport's severance, if the engine refuses the attach or falls
    /// silent before answering it.
    pub(crate) fn wire(
        transport: ral_core::carrier::WireTransport,
        cwd: std::path::PathBuf,
        home: std::path::PathBuf,
    ) -> Result<Self, Severed> {
        transport.attach(Attach::new(
            builtins::INSTALLER_TAG,
            cwd.clone(),
            home.clone(),
        ));
        transport.await_attached()?;
        Ok(Self::Wire {
            target: InterruptTarget::new(transport.control().clone()),
            transport: Box::new(transport),
            cwd,
            home,
        })
    }

    pub(crate) fn transport(&self) -> &dyn Transport {
        match self {
            Self::Identity { transport, .. } => &**transport,
            Self::Wire { transport, .. } => &**transport,
        }
    }

    pub(crate) fn kind(&self) -> SeatKind {
        match self {
            Self::Identity { transport, .. } => SeatKind::Identity(transport.clone()),
            Self::Wire { cwd, home, .. } => SeatKind::Wire {
                cwd: cwd.clone(),
                home: home.clone(),
            },
        }
    }

    /// Install the session sink a settling worker's deferred batch reaches.
    pub(crate) fn install_deferred(&self, sink: Arc<dyn ral_core::types::DeferredSink>) {
        self.transport().set_deferred_sink(sink);
    }

    /// Why no further frame will cross this seat's transport, if that has
    /// happened.
    pub(crate) fn severed(&self) -> Option<Severed> {
        self.transport().severed()
    }

    /// Take one reading through `door`. A refusal is a protocol fault here —
    /// probes are asked only at run boundaries — so it severs the seat.
    ///
    /// # Errors
    /// The engine's severance.
    pub(crate) fn read<T>(
        &self,
        door: impl FnOnce(&dyn Transport) -> Result<T, ProbeError>,
    ) -> Result<T, Severed> {
        let t = self.transport();
        door(t).map_err(|e| match e {
            ProbeError::Severed(cause) => cause,
            ProbeError::Rejected(why) => t.sever(Severed::Faulted(why)),
        })
    }

    /// Where this agent's interrupt and terminate land.
    pub(crate) fn reach(&self) -> InterruptTarget {
        match self {
            Self::Identity { target, .. } | Self::Wire { target, .. } => target.clone(),
        }
    }

    /// `/clear`'s engine half: boot afresh from the same Attach, onto the same
    /// target.  Replacing the transport drops the outgoing engine, whose
    /// teardown cancels its workers: `/clear` outranks leases.
    ///
    /// # Errors
    /// A seat with no recipe to reboot from, or the recipe's refusal.
    pub(crate) fn clear(&mut self) -> Result<(), String> {
        let Self::Identity {
            transport,
            target,
            rebirth: Some(rebirth),
        } = self
        else {
            return Err(
                "/clear cannot start this conversation over: its engine was not \
                 booted here, and starting afresh means a new conversation"
                    .to_string(),
            );
        };
        *transport = Arc::new(
            IdentityTransport::boot(rebirth.installers, &rebirth.attach)
                .map_err(|s| s.to_string())?,
        );
        target.republish(transport.control().clone());
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests;
