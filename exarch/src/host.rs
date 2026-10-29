//! The fleet's owner: launch its roots, carry their signals, and join them.

use crate::agent::fleet::Fleet;
pub use crate::agent::fleet::HELPER_IDLE;
use crate::agent::spawn::LaunchError;
pub use crate::agent::{Admission, EngineLost, Factory, RootConfig, RootEngine, SPAWN_FUEL, Trunk};

use crate::agent::Agent;
use crate::bus::{BusReceiver, FleetBus};
use crate::port::Port;
use crate::provider::Provider;
use crate::record::{AgentId, Born, Resumed};
use ral_core::own::OwnDir;
use ral_core::process::{AmbientForward, CancelCause};
use std::{io, sync::Arc};

/// Owns a fleet whose roots attend on their own threads.
pub struct Host {
    roots: Vec<AgentId>,
    bus: FleetBus,
    signals: Option<AmbientForward>,
    fleet: Arc<Fleet>,
}

impl Host {
    /// Found a fleet and launch its first root.
    ///
    /// # Errors
    /// The root's engine, log, or thread could not be started.
    pub fn found(cfg: RootConfig, engine: RootEngine, provider: Arc<Provider>) -> io::Result<Self> {
        Self::adopt(vec![(Agent::root(cfg, engine, provider)?, None)])
    }

    /// Reopen and launch every open root in a recorded run.
    ///
    /// # Errors
    /// A root could not be reopened or its thread could not be started.
    pub fn resume(
        cfg: RootConfig,
        engine: RootEngine,
        provider: &Arc<Provider>,
    ) -> io::Result<Self> {
        Self::adopt(
            Agent::resume(cfg, engine, provider)?
                .into_iter()
                .map(|(root, resumed)| (root, Some(resumed)))
                .collect(),
        )
    }

    pub(crate) fn adopt(roots: Vec<(Agent, Option<Resumed>)>) -> io::Result<Self> {
        let fleet = Arc::clone(
            &roots
                .first()
                .ok_or_else(|| io::Error::other("there is no session to open"))?
                .0
                .fleet,
        );
        let mut host = Self {
            roots: roots.iter().map(|(root, _)| root.id()).collect(),
            bus: FleetBus::session(),
            signals: Some(crate::signals::face(&fleet.registry)),
            fleet,
        };
        let mut roots = roots.into_iter();
        while let Some((root, resumed)) = roots.next() {
            let id = root.id();
            let born = Born {
                log_dir: root.log_dir().clone(),
                name: root.handle().0.name.clone(),
                parent: None,
                resumed,
                reader: root.recorder().reader(),
            };
            match root.launch(born, None, &host.bus.emitter(id)) {
                Ok(thread) => host.fleet.adopt_root(thread),
                Err(LaunchError { cause, failures }) => {
                    let failures: Vec<_> =
                        std::iter::once(format!("could not start root agent {id}: {cause}"))
                            .chain(failures)
                            .chain(host.finish())
                            .chain(roots.flat_map(|(root, _)| root.discard()))
                            .collect();
                    return Err(io::Error::new(cause.kind(), failures.join("\n")));
                }
            }
        }
        Ok(host)
    }

    /// The roots it launched, in launch order.
    pub fn roots(&self) -> &[AgentId] {
        &self.roots
    }

    pub fn feed(&self) -> &BusReceiver {
        self.bus.rx()
    }

    pub fn port(&self) -> Port {
        Port::new(self.fleet.registry.clone(), self.bus.meter())
    }

    /// Admit one job on a gated root; `false` once it has ended.
    pub fn release(&self, root: AgentId) -> bool {
        self.fleet
            .registry
            .get(root)
            .is_some_and(|handle| handle.release())
    }

    pub fn run_dir(&self) -> &OwnDir {
        self.fleet.launch.run_dir()
    }

    /// End the fleet and join every root, returning their teardown failures.
    pub fn end(mut self) -> Vec<String> {
        self.finish()
    }

    #[cfg(test)]
    pub(crate) fn fleet(&self) -> &Arc<Fleet> {
        &self.fleet
    }

    fn finish(&mut self) -> Vec<String> {
        self.fleet.registry.end(CancelCause::Cancelled);
        drop(self.signals.take());
        self.fleet.join_roots()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
