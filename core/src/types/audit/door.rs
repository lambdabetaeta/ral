//! The fan-out door: one observation, broadcast to whoever is listening.
//!
//! [`Shell::observe_stamped`] reports to both consumers and judges neither's
//! interest: the host decides what the rail draws, and `audit { }` decides
//! what the trail keeps.  It judges only whether anything happened at all:
//! a redirect onto the discard device is not a write.  Every door that
//! settles a fact routes through it; one holding no [`Mooring`] reaches the
//! trail alone.
//!
//! With nobody listening the recorders are no-ops, so a door can call them
//! unconditionally.

use super::epoch_us;
use crate::fact::{Check, Observation, Observed};
use crate::source::CallSite;
use crate::types::{Mooring, Shell};

/// Where a command was called from and when it began: the two halves of an
/// observation's stamp, paired so the dispatch site carries one local.
#[derive(Clone, Debug, Default)]
pub(crate) struct AuditStart {
    pub site: Option<CallSite>,
    pub(crate) time: i64,
}

impl Shell {
    /// `what`, stamped now at the current dispatch site and principal.  `pub`
    /// for a host door that builds its own fact (a grep walk, a read outside
    /// any redirect): it needs the stamp core's own doors carry.
    pub fn observation(&self, what: Observed) -> Observation {
        Observation::instant(self.call_site(), self.context.principal(), what)
    }

    /// Whether an observation would reach anyone: a trail collecting it, or a
    /// host on the other end of the sink.  Doors whose facts cost something
    /// to gather, the write door's before/after snapshots, ask this first.
    pub(crate) fn listening(&self, mooring: &Mooring) -> bool {
        self.local.audit.active() || mooring.has_surface()
    }

    /// Report one observation to the surface sink and the open trail
    /// (`Audit::push` is already a no-op with no trail open; the surface is
    /// asked first because projecting costs more than the question).
    ///
    /// A redirect onto the [discard device](crate::path::LexicalPath::is_discard)
    /// left the world as it found it, so there is nothing to report: no card,
    /// no rail barrier, and no trail line claiming a file was written.  The
    /// predicate `Shell::check_fs_read` asks, of the same resolver, is
    /// asked here, at the one door every seam passes through.
    pub(crate) fn observe_stamped(&mut self, mooring: Option<&Mooring>, obs: Observation) {
        if let Observed::Write(w) = &obs.what
            && self.resolve(&w.path).is_discard()
        {
            return;
        }
        if let Some(m) = mooring.filter(|m| m.has_surface()) {
            m.surface_data(&obs.to_surface());
        }
        self.local.audit.push(obs);
    }

    /// An instantaneous door: stamped now, at the current dispatch site.
    pub(crate) fn observe(&mut self, mooring: &Mooring, what: Observed) {
        if self.listening(mooring) {
            let obs = self.observation(what);
            self.observe_stamped(Some(mooring), obs);
        }
    }

    /// Open one command's audit stamp.  With nobody listening the stamp is
    /// empty and costs neither the `script` clone nor the `epoch_us` syscall;
    /// should the command's own body then open a trail, the observation
    /// carries that empty stamp rather than a late one.
    pub(crate) fn audit_start(&self, mooring: &Mooring) -> AuditStart {
        if !self.listening(mooring) {
            return AuditStart::default();
        }
        AuditStart {
            site: self.call_site(),
            time: epoch_us(),
        }
    }

    /// Record a check worth reporting: a refusal, or a deputy admitted but
    /// flagged.  An admitted check is never recorded.  A door holding no
    /// `Mooring` passes `None`, and the check reaches the trail alone.
    pub(crate) fn record_check(&mut self, mooring: Option<&Mooring>, check: Check) {
        if self.local.audit.active() || mooring.is_some_and(Mooring::has_surface) {
            let obs = self.observation(Observed::Check(check));
            self.observe_stamped(mooring, obs);
        }
    }
}
