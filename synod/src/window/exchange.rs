use crate::wire::ExchangeId;
use ral_core::sync::{CondvarExt as _, LockExt as _};
use std::sync::{Arc, Condvar, Mutex};

#[derive(Default)]
pub(super) struct Exchanges {
    state: Mutex<Sequence>,
    delivered: Condvar,
    #[cfg(test)]
    waiting: Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
}

#[derive(Default)]
struct Sequence {
    last: u32,
    phase: Phase,
}

#[derive(Default)]
enum Phase {
    #[default]
    Idle,
    Running,
    Stopping {
        delivered: bool,
    },
}

impl Sequence {
    fn begin(&mut self) -> ExchangeId {
        assert!(matches!(self.phase, Phase::Idle));
        self.last = self.last.checked_add(1).expect("exchange IDs exhausted");
        self.phase = Phase::Running;
        ExchangeId(self.last)
    }

    fn stop(&mut self, id: ExchangeId) -> bool {
        // A stopped exchange may take up text sent after the stop; it stays stoppable.
        if id.0 != self.last
            || !matches!(
                self.phase,
                Phase::Running | Phase::Stopping { delivered: true }
            )
        {
            return false;
        }
        self.phase = Phase::Stopping { delivered: false };
        true
    }

    fn delivered(&mut self, id: ExchangeId) {
        assert_eq!(id.0, self.last);
        assert!(matches!(self.phase, Phase::Stopping { delivered: false }));
        self.phase = Phase::Stopping { delivered: true };
    }

    fn finish(&mut self, id: ExchangeId) -> Option<bool> {
        assert_eq!(id.0, self.last);
        let stopped = match self.phase {
            Phase::Idle => panic!("exchange already completed"),
            Phase::Running => false,
            Phase::Stopping { delivered: false } => return None,
            Phase::Stopping { delivered: true } => true,
        };
        self.phase = Phase::Idle;
        Some(stopped)
    }
}

impl Exchanges {
    pub(super) fn begin(&self) -> ExchangeId {
        self.state.lock_ignore_poison().begin()
    }

    pub(super) fn stop(self: &Arc<Self>, id: ExchangeId) -> Option<Delivery> {
        self.state.lock_ignore_poison().stop(id).then(|| Delivery {
            exchanges: self.clone(),
            id,
        })
    }

    /// Wait outside the slot lock; the next exchange cannot overtake delivery.
    pub(super) fn finish(&self, id: ExchangeId) -> bool {
        let mut state = self.state.lock_ignore_poison();
        loop {
            if let Some(stopped) = state.finish(id) {
                drop(state);
                return stopped;
            }
            #[cfg(test)]
            {
                let waiting = self.waiting.lock_ignore_poison().take();
                if let Some(waiting) = waiting {
                    waiting.send(()).unwrap();
                }
            }
            state = self.delivered.wait_ignore_poison(state);
        }
    }
}

pub(super) struct Delivery {
    exchanges: Arc<Exchanges>,
    id: ExchangeId,
}

impl Drop for Delivery {
    fn drop(&mut self) {
        self.exchanges.state.lock_ignore_poison().delivered(self.id);
        self.exchanges.delivered.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_stop_never_reserves_idle_or_the_next_exchange() {
        let exchanges = Arc::new(Exchanges::default());
        let a = exchanges.begin();
        assert!(!exchanges.finish(a));
        assert!(exchanges.stop(a).is_none());
        let b = exchanges.begin();
        assert!(b.0 > a.0);
        assert!(exchanges.stop(a).is_none());
        assert!(!exchanges.finish(b));
    }

    #[test]
    fn a_delivered_stop_can_be_reissued_but_never_duplicated_in_flight() {
        let mut state = Sequence::default();
        let a = state.begin();
        assert!(state.stop(a));
        assert!(!state.stop(a));
        assert_eq!(state.finish(a), None);
        state.delivered(a);
        assert!(state.stop(a));
        assert_eq!(state.finish(a), None);
        state.delivered(a);
        assert_eq!(state.finish(a), Some(true));
        assert!(!state.stop(a));
        let b = state.begin();
        assert_eq!(state.finish(b), Some(false));
    }

    #[test]
    fn finish_waits_until_delivery_drops() {
        use std::sync::mpsc::{self, TryRecvError};
        use std::time::Duration;

        let exchanges = Arc::new(Exchanges::default());
        let a = exchanges.begin();
        let delivery = exchanges.stop(a).expect("running exchange accepts a stop");
        assert!(exchanges.stop(a).is_none());
        let (waiting_tx, waiting_rx) = mpsc::sync_channel(0);
        *exchanges.waiting.lock_ignore_poison() = Some(waiting_tx);
        let (finished_tx, finished_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let exchanges = &exchanges;
            scope.spawn(move || {
                finished_tx.send(exchanges.finish(a)).unwrap();
            });
            waiting_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            // Reacquiring state proves finish entered its condvar wait.
            assert!(matches!(
                exchanges.state.lock_ignore_poison().phase,
                Phase::Stopping { delivered: false }
            ));
            assert_eq!(finished_rx.try_recv(), Err(TryRecvError::Empty));
            drop(delivery);
            let completed = finished_rx.recv_timeout(Duration::from_secs(5));
            // Release the worker even if Delivery failed to notify it.
            exchanges.delivered.notify_all();
            assert!(completed.unwrap());
        });
        assert!(exchanges.stop(a).is_none());
        let b = exchanges.begin();
        assert!(exchanges.stop(a).is_none());
        assert!(!exchanges.finish(b));
    }

    #[test]
    fn completion_does_not_wait_for_command_acknowledgement() {
        let exchanges = Arc::new(Exchanges::default());
        let a = exchanges.begin();
        drop(exchanges.stop(a).expect("running exchange accepts a stop"));
        assert!(exchanges.finish(a));
        let b = exchanges.begin();
        assert!(exchanges.stop(a).is_none());
        assert!(!exchanges.finish(b));
    }

    #[test]
    fn superseded_attempts_have_independent_delivery() {
        let old = Arc::new(Exchanges::default());
        let a = old.begin();
        let delivery = old.stop(a).expect("running exchange accepts a stop");
        let new = Exchanges::default();
        let b = new.begin();
        drop(delivery);
        assert!(old.finish(a));
        assert!(!new.finish(b));
    }
}
