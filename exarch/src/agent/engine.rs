//! An agent's ral engine, in-process or over a wire.

use crate::app::Scratch;
use crate::shell_eval::builtins;
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
pub(crate) struct Rebirth {
    installers: &'static [EngineInstaller],
    attach: Attach,
    _scratch: Arc<Scratch>,
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
    pub(crate) fn install_deferred(&self, sink: Arc<dyn ral_core::shell::runtime::DeferredSink>) {
        self.transport().set_deferred_sink(sink);
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
    pub(crate) fn rebootable(&self) -> bool {
        matches!(
            self,
            Self::Identity {
                rebirth: Some(_),
                ..
            }
        )
    }
    pub(crate) fn clear(&mut self) -> Result<(), String> {
        let Self::Identity {
            transport,
            rebirth: Some(rebirth),
        } = self
        else {
            return Err(Self::CANNOT_REBOOT.to_string());
        };
        let fresh = IdentityTransport::boot(rebirth.installers, &rebirth.attach)
            .map_err(|s| s.to_string())?;
        transport.control().terminate();
        *transport = Arc::new(fresh);
        Ok(())
    }
    pub(crate) const CANNOT_REBOOT: &str = "/clear cannot start this conversation over: its engine was not booted here; start a new conversation instead";
}
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
