//! An agent's ral engine, in-process or over a wire.

use crate::agent::fleet::{Tenancy, TenantId};
use crate::app::Scratch;
use crate::bus::{Emitter, Mailbox};
use crate::shell_eval::{builtins, deferred_sink};
use crate::site::{Silent, site};
use ral_core::carrier::{IdentityTransport, ProbeError, Severed, Transport};
use ral_core::engine::EngineInstaller;
use ral_core::path::EnginePath;
use ral_core::process::{CancelCause, Scope, ScopeNode};
use ral_core::protocol::{Attach, EngineSpec, Root};
use std::ops::Deref;
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
    fn install_deferred(&self, sink: Arc<dyn ral_core::shell::runtime::DeferredSink>) {
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
    /// End this engine; `Err` if its teardown is uncertified.
    pub(crate) fn end(&self) -> Result<(), Severed> {
        match self {
            Self::Identity { transport, .. } => transport.end(),
            Self::Wire { transport, .. } => transport.end(),
        }
    }
    /// Boot without touching the live transport, recipe, or cancellation scope.
    pub(crate) fn prepare_rebirth(&self) -> Result<Self, String> {
        let Self::Identity {
            rebirth: Some(born),
            ..
        } = self
        else {
            return Err(Self::CANNOT_REBOOT.to_string());
        };
        let fresh =
            IdentityTransport::boot(born.installers, &born.attach).map_err(|s| s.to_string())?;
        Ok(Self::Identity {
            transport: Arc::new(fresh),
            rebirth: Some(born.clone()),
        })
    }
    pub(crate) const CANNOT_REBOOT: &str = "/clear cannot start this conversation over: its engine was not booted here; start a new conversation instead";
}
/// A tenancy bound to a life; deferred batches are vouched by `scope`.
pub(crate) struct Incarnation {
    tenancy: Tenancy,
    life: Scope,
    scope: ScopeNode,
}
impl Incarnation {
    pub(crate) fn new(tenancy: Tenancy, life: Scope) -> Self {
        Self {
            scope: life.child(),
            tenancy,
            life,
        }
    }
    pub(crate) fn install(&self, emit: &Emitter, mailbox: &Mailbox) {
        self.install_deferred(deferred_sink(
            emit,
            mailbox.vouched_by((*self.scope).clone()),
        ));
    }
    /// Certify an engine prepared for this incarnation but never installed.
    pub(crate) fn discard(&self, prepared: Engine) {
        self.tenancy.discard(prepared);
    }
    /// Fence the outgoing producers, and move the life's binding to `fresh`
    /// before the outgoing engine's certified teardown, which may sever.
    pub(crate) fn install_rebirth(&mut self, fresh: Engine) {
        self.scope.cancel(CancelCause::Cancelled);
        self.scope = self.life.child();
        // `None`: the outgoing engine was lost, which ended this life.
        if let Some(life) = self.tenancy.transport().unbind() {
            fresh.transport().bind(life);
        }
        self.tenancy.install(fresh);
    }
    pub(crate) fn tenant(&self) -> TenantId {
        self.tenancy.claim().id()
    }
    pub(crate) fn publish_death_on_release(&mut self, sink: Box<dyn crate::record::Publish>) {
        self.tenancy.publish_death_on_release(sink);
    }
    #[cfg(test)]
    pub(crate) fn install_raw(&self, sink: Arc<dyn ral_core::shell::runtime::DeferredSink>) {
        self.install_deferred(sink);
    }
    #[cfg(test)]
    pub(crate) fn scope(&self) -> Scope {
        (*self.scope).clone()
    }
}
impl Deref for Incarnation {
    type Target = Engine;
    fn deref(&self) -> &Engine {
        &self.tenancy
    }
}
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
