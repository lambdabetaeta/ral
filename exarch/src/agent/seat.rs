//! The agent's seat: the transport its runs go through, plus whatever
//! host-side state that seat kind owns.  Every engine-side reach is a
//! method here, so a new seat kind is one more variant, not a second agent.

use crate::app::Scratch;
use crate::cancel::InterruptTarget;
use crate::shell_eval::builtins;
use ral_core::carrier::{IdentityTransport, ProbeError, Severed, Transport};
use ral_core::engine::EngineInstaller;
use ral_core::protocol::{Attach, EngineSpec, Root};
use std::sync::Arc;

/// How a fork of this seat's engine reaches the desk that adopts it: parked in
/// the parent's own transport, or made by the host's factory across a wire.
/// `` `start `` and `/branch` choose their arm on this fact alone.
pub(crate) enum SeatKind {
    Identity(Arc<IdentityTransport>),
    /// A child is attached as this engine was, with its own seal.
    Wire {
        engine: EngineSpec,
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
    /// Out-of-process, one engine per session: a fork is made by the host's
    /// factory and attached as this seat was, plus its seal.
    Wire {
        transport: Box<ral_core::carrier::WireTransport>,
        target: InterruptTarget,
        engine: EngineSpec,
    },
}

/// What an identity root reboots from: the recipes and Attach it was born
/// from, and the scratch that Attach names, which outlives every reboot.
pub(crate) struct Rebirth {
    installers: &'static [EngineInstaller],
    attach: Attach,
    _scratch: Arc<Scratch>,
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
        let mut env = scratch.env();
        env.push((
            "EXARCH_SESSION_DIR".into(),
            session_dir.to_string_lossy().into_owned(),
        ));
        let attach = Attach::root(
            EngineSpec::new(builtins::INSTALLER_TAG, home.into()),
            Root::new(cwd).with_env(env),
        )
        .with_terminal(terminal);
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

    /// Write `attach` and wait for the verdict. Only its engine half is kept:
    /// a child's seal is always added fresh, so no seat holds its parent's
    /// bequest for its own life.
    ///
    /// # Errors
    /// The transport's severance, if the engine refuses the attach or falls
    /// silent before answering it.
    pub(crate) fn wire(
        transport: ral_core::carrier::WireTransport,
        attach: Attach,
    ) -> Result<Self, Severed> {
        let engine = attach.engine.clone();
        transport.attach(attach);
        transport.await_attached()?;
        Ok(Self::Wire {
            target: InterruptTarget::new(transport.control().clone()),
            transport: Box::new(transport),
            engine,
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
            Self::Wire { engine, .. } => SeatKind::Wire {
                engine: engine.clone(),
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
