//! An agent's ral engine, in-process or over a wire.

use crate::agent::Body;
use crate::app::Scratch;
use crate::bus::AgentAddress;
use crate::record::Forensic;
use crate::shell_eval::{builtins, deferred_sink};
use crate::site::{Silent, site};
use ral_core::carrier::{IdentityTransport, ProbeError, Severed, Transport};
use ral_core::engine::EngineInstaller;
use ral_core::path::EnginePath;
use ral_core::protocol::{Attach, EngineSpec, Root};
use std::sync::Arc;

pub(crate) enum EngineKind {
    Identity(Arc<IdentityTransport>),
    Wire { engine: EngineSpec },
}
pub(crate) enum Engine {
    Identity {
        transport: Arc<IdentityTransport>,
        rebirth: Option<Rebirth>,
    },
    Wire {
        transport: Box<ral_core::carrier::WireTransport>,
        engine: EngineSpec,
    },
}
#[derive(Clone)]
pub(crate) struct Rebirth {
    installers: &'static [EngineInstaller],
    attach: Attach,
    _scratch: Arc<Scratch>,
}
impl Rebirth {
    pub(crate) fn boot(&self) -> Result<Engine, Severed> {
        Ok(Engine::Identity {
            transport: Arc::new(IdentityTransport::boot(self.installers, &self.attach)?),
            rebirth: Some(self.clone()),
        })
    }
}
impl Engine {
    pub(crate) fn root(
        installers: &'static [EngineInstaller],
        cwd: ral_core::path::CwdPath,
        grants: ral_core::authority::policy::GrantStack,
        terminal: ral_core::io::terminal::TerminalState,
        scratch: Arc<Scratch>,
    ) -> Result<Self, Severed> {
        let home = site!(Silent::EngineHome, ral_core::host::home()).unwrap_or_default();
        let attach = Attach::root(
            EngineSpec::new(builtins::INSTALLER_TAG, EnginePath::Host(home.into())),
            Root::new(EnginePath::Host(cwd.into_inner()))
                .with_env(scratch.env())
                .with_grants(grants),
        )
        .with_terminal(terminal);
        let transport = Arc::new(IdentityTransport::boot(installers, &attach)?);
        Ok(Self::Identity {
            transport,
            rebirth: Some(Rebirth {
                installers,
                attach,
                _scratch: scratch,
            }),
        })
    }
    pub(crate) fn adopted(transport: IdentityTransport) -> Self {
        Self::Identity {
            transport: Arc::new(transport),
            rebirth: None,
        }
    }
    pub(crate) fn wire(
        transport: ral_core::carrier::WireTransport,
        attach: Attach,
    ) -> Result<Self, Severed> {
        let engine = attach.engine.clone();
        transport.attach(attach);
        transport.await_attached()?;
        Ok(Self::Wire {
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
    pub(crate) fn kind(&self) -> EngineKind {
        match self {
            Self::Identity { transport, .. } => EngineKind::Identity(transport.clone()),
            Self::Wire { engine, .. } => EngineKind::Wire {
                engine: engine.clone(),
            },
        }
    }
    pub(crate) fn install(&self, address: &AgentAddress, generation: u64) {
        self.transport()
            .set_deferred_sink(deferred_sink(address, generation));
    }
    pub(crate) fn inherit(&self, outgoing: &Self) {
        if let Some(life) = outgoing.transport().unbind() {
            self.transport().bind(life);
        }
    }
    pub(crate) fn severed(&self) -> Option<Severed> {
        self.transport().severed()
    }
    pub(crate) fn read<T>(
        &self,
        probe: impl FnOnce(&dyn Transport) -> Result<T, ProbeError>,
    ) -> Result<T, Severed> {
        let transport = self.transport();
        probe(transport).map_err(|e| match e {
            ProbeError::Severed(cause) => cause,
            ProbeError::Rejected(why) => transport.sever(Severed::Faulted(why)),
        })
    }
    /// End this engine; `Err` if its teardown is uncertified.
    pub(crate) fn end(&self) -> Result<(), Severed> {
        match self {
            Self::Identity { transport, .. } => transport.end(),
            Self::Wire { transport, .. } => transport.end(),
        }
    }
    pub(crate) fn rebirth(&self) -> Option<Rebirth> {
        match self {
            Self::Identity { rebirth, .. } => rebirth.clone(),
            Self::Wire { .. } => None,
        }
    }
    /// Boot without touching the live transport, recipe, or cancellation scope.
    pub(crate) fn prepare_rebirth(&self) -> Result<Self, String> {
        self.rebirth()
            .ok_or_else(|| Self::CANNOT_REBOOT.to_string())?
            .boot()
            .map_err(|cause| cause.to_string())
    }
    pub(crate) const CANNOT_REBOOT: &str = "/clear cannot start this conversation over: its engine was not booted here; start a new conversation instead";
}
impl Body {
    /// A11: retire each outgoing engine once, retaining uncertified teardown.
    pub(crate) fn retire_engine(&mut self, name: &str, engine: &Engine) {
        if let Err(cause) = engine.end() {
            let failure =
                format!("agent '{name}' ended without certified engine teardown: {cause}");
            self.log
                .record_emitter()
                .emit_or_end(Forensic::TeardownFailed {
                    failure: failure.clone(),
                });
            self.failures.push(failure);
        }
    }
}
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
