//! The in-process carrier: an [`Engine`] behind the session lock, run on the
//! calling thread, its rails calling the `Host` directly.

use std::sync::Arc;
use std::sync::mpsc;

use super::{
    ControlSender, Door, Ends, EventReceiver, Host, ProbeError, Severed, Transport, forbid_reentry,
};
use crate::engine::{Engine, EngineInstaller, Rails, Scopes};
use crate::first_order::FOValue;
use crate::guard::SpawnGrant;
use crate::protocol::probe::Probe;
use crate::protocol::{Attach, DispatchId, Event, Run};
use crate::sync::LockExt as _;
use crate::types::{Fork, Nursery, NurseryId};

/// A session lock that cannot poison-panic, named for the one state it
/// guards: a run that unwinds drops its guard mid-mutation, and recovering
/// the poison (via [`LockExt`]) rather than unwrapping it means such a run
/// can never wedge the session for whatever runs next.
struct SessionLock(std::sync::Mutex<Engine>);

impl SessionLock {
    fn lock(&self) -> std::sync::MutexGuard<'_, Engine> {
        self.0.lock_ignore_poison()
    }
}

/// The in-process carrier: an [`Engine`] behind the session lock, run on the
/// calling thread, its rails calling the `Host` directly.
pub struct IdentityTransport {
    engine: SessionLock,
    /// The engine's scopes, outside the lock, so a `Control` lands on a
    /// dispatch still waiting to take it.
    scopes: Arc<Scopes>,
    installer: &'static EngineInstaller,
    /// Where this transport's runs park their forks — outside the lock, so a
    /// handler can adopt one mid-dispatch.
    nursery: Nursery,
    event_tx: mpsc::Sender<(DispatchId, Event)>,
    ends: Ends,
}

impl IdentityTransport {
    /// Boot an engine from `attach`, in this process.
    ///
    /// # Errors
    /// [`Severed::Refused`], in the refusing step's own words — the verdict
    /// `WireTransport::await_attached` gives.
    pub fn boot(installers: &'static [EngineInstaller], attach: &Attach) -> Result<Self, Severed> {
        Engine::boot(installers, attach, None)
            .map(Self::over)
            .map_err(Severed::Refused)
    }

    fn over(engine: Engine) -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        let scopes = engine.scopes().clone();
        let control = ControlSender(Door::Identity(scopes.clone()));
        Self {
            installer: engine.installer(),
            scopes,
            engine: SessionLock(std::sync::Mutex::new(engine)),
            nursery: Nursery::default(),
            event_tx,
            ends: Ends::new(control, EventReceiver::new(event_rx), Arc::default()),
        }
    }

    /// Adopt the fork a run parked under `id` as an engine of its own, its
    /// grant layer narrowed against the fork's own cwd under this transport's
    /// installer.
    ///
    /// # Errors
    /// No fork parked under `id`, or whatever narrowing `grant` refuses.
    pub fn adopt_parked(&self, id: NurseryId, grant: &SpawnGrant) -> Result<Self, String> {
        let mut shell = self.nursery.adopt(id).ok_or_else(|| {
            format!(
                "no forked session is parked under nursery id {}: was it adopted already, or \
                 has the run that forked it ended?",
                id.0
            )
        })?;
        grant.narrow_onto(&mut shell, self.installer.narrow)?;
        Ok(Self::over(Engine::new(shell, self.installer, Box::new(()))))
    }

    /// Read the engine's shell under the session lock.
    #[cfg(feature = "test-util")]
    pub(crate) fn inspect<R>(&self, read: impl FnOnce(&crate::types::Shell) -> R) -> R {
        forbid_reentry(self);
        read(&self.engine.lock().shell)
    }
}

/// The identity transport's enquiry desk: a direct, same-thread wrapper
/// around the host's `Host`, installed onto each dispatch's `RunRequest`.
///
/// Draining `events` before `host.enquire` is what keeps a handler from
/// running ahead of the run's own surface output.
struct IdentityDesk {
    host: Arc<dyn Host>,
    events: Arc<EventReceiver>,
}

impl crate::types::EnquiryDesk for IdentityDesk {
    /// `_cancel` goes unpolled: [`EnquiryDesk::enquire`] is contractually
    /// blocking and short, so a `Host` answers at once and leaves nothing
    /// here to park on.
    fn enquire(
        &self,
        req: FOValue,
        _cancel: &crate::process::CancelScope,
    ) -> Result<FOValue, crate::types::Error> {
        let mut carried = std::collections::VecDeque::new();
        while let Some((did, event)) = self.events.try_recv() {
            match event {
                Event::Surface(val) => self.host.surface(&val),
                other => carried.push_back((did, other)),
            }
        }
        for item in carried {
            self.events.stash.lock_ignore_poison().push_back(item);
        }
        self.host.enquire(req).map_err(crate::types::Error::from)
    }
}

impl Transport for IdentityTransport {
    fn ends(&self) -> &Ends {
        &self.ends
    }

    fn dispatch(&self, id: DispatchId, run: Run, host: &Arc<dyn Host>) {
        if self.severed().is_some() {
            return;
        }
        let scope = self.scopes.open(id);
        let events = self.event_tx.clone();
        let rails = Rails {
            outlet: Arc::new(move |id, event| events.send((id, event)).is_ok()),
            deferred: self.ends.deferred_sink.lock_ignore_poison().clone(),
            desk: Arc::new(IdentityDesk {
                host: host.clone(),
                events: self.ends.events.clone(),
            }),
            fork: Fork::Park(self.nursery.clone()),
        };
        let report = self.engine.lock().run(id, run, rails, &scope);
        let _ = self.event_tx.send((id, Event::Report(report)));
    }

    fn probe(&self, probe: &Probe) -> Result<FOValue, ProbeError> {
        forbid_reentry(self);
        if let Some(cause) = self.severed() {
            return Err(ProbeError::Severed(cause));
        }
        Ok(self.engine.lock().probe(probe))
    }

    fn detach(&self) {
        self.scopes.strike(crate::process::CancelCause::Cancelled);
    }
}

#[cfg(test)]
mod tests;
