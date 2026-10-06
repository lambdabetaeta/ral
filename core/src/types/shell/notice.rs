//! What a settled run's ready boundary tells its host: a worker the lease
//! chain reaped, or idle top-level bindings the ledger pruned.  Both are
//! recorded, neither drawn.

use super::bindings::Pruned;
use super::workers::ReapNotice;
use crate::first_order::FOValue;
use crate::first_order::datum::{Datum, tag, untag};
use crate::variant;

/// A `` `notice `` event's body: `` `reap [id, cmd, class, cause] `` or
/// `` `prune [[name, idle-calls, kind], ...] ``, one prune per boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    Reap(ReapNotice),
    Prune(Vec<Pruned>),
}

variant!(Notice {
    Reap(ReapNotice): "reap",
    Prune(Vec<Pruned>): "prune",
});

impl Notice {
    /// The tag a notice carries on the surface channel.
    pub const SURFACE_TAG: &str = "notice";

    /// The notice tagged for the surface channel, so a host dispatches on the
    /// tag alone.
    pub fn to_surface(self) -> FOValue {
        tag(Self::SURFACE_TAG, Some(self.encode()))
    }

    /// Inverse of [`Self::to_surface`]; `None` for any other surface event.
    pub fn from_surface(v: &FOValue) -> Option<Self> {
        match untag(v)? {
            (Self::SURFACE_TAG, Some(body)) => Self::decode(body).ok(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{LeaseClass, ReapCause, WorkerId};

    #[test]
    fn a_notice_round_trips_through_the_surface() {
        let reap = Notice::Reap(ReapNotice {
            id: WorkerId(3),
            cmd: "watch build".into(),
            class: LeaseClass::Worker,
            cause: ReapCause::Idle,
        });
        let prune = Notice::Prune(vec![Pruned {
            name: "scratch".into(),
            idle_calls: 5,
            kind: "Int".into(),
        }]);
        for notice in [reap, prune] {
            let wire = notice.clone().to_surface();
            assert_eq!(Notice::from_surface(&wire), Some(notice));
        }
    }

    #[test]
    fn another_surface_event_is_no_notice() {
        assert_eq!(Notice::from_surface(&tag("observed", None)), None);
    }
}
