use std::io;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use guest_abi::boot::Boot;
use rustix::process::Pid;

use crate::{engine, reap, vsock};

const RESOURCE_BACKOFF: Duration = Duration::from_millis(100);

/// Admission closes atomically with the shutdown snapshot.
pub(super) struct Fleet {
    pids: Vec<Pid>,
    open: bool,
}

impl Fleet {
    pub(super) fn new(root: Pid) -> Self {
        Self {
            pids: vec![root],
            open: true,
        }
    }

    pub(super) fn admit<E>(
        &mut self,
        spawn: impl FnOnce() -> Result<Pid, E>,
    ) -> Option<Result<Pid, E>> {
        self.open
            .then(|| spawn().inspect(|&pid| self.pids.push(pid)))
    }

    pub(super) fn close(&mut self) -> Vec<Pid> {
        self.open = false;
        self.pids.clone()
    }

    pub(super) fn forget(&mut self, pid: Pid) -> bool {
        let before = self.pids.len();
        self.pids.retain(|&p| p != pid);
        self.pids.len() != before
    }
}

pub(super) type Engines = Arc<Mutex<Fleet>>;

fn accept_backoff(err: &io::Error) -> Option<Duration> {
    match err.raw_os_error()? {
        libc::ECONNABORTED | libc::EPROTO | libc::EAGAIN => Some(Duration::ZERO),
        libc::EMFILE | libc::ENFILE | libc::ENOMEM | libc::ENOBUFS => Some(RESOURCE_BACKOFF),
        _ => None,
    }
}

/// Non-host peers are dropped; fatal accept errors end the session.
pub(super) fn make_engines(listener: &OwnedFd, boot: &Boot, engines: &Engines) {
    loop {
        match vsock::accept_from_host(listener) {
            Ok(None) => eprintln!("ral-guest: refused a dial that was not the host's"),
            Ok(Some(conn)) => {
                // Spawn under the lock so reaping and shutdown cannot miss an unrecorded child.
                let spawned = live(engines).admit(|| engine::spawn(boot, &conn));
                match spawned {
                    None => return eprintln!("ral-guest: the machine is stopping; dial dropped"),
                    Some(Ok(pid)) => eprintln!(
                        "ral-guest: engine running as pid {} for the host's dial",
                        pid.as_raw_nonzero()
                    ),
                    Some(Err(err)) => eprintln!("ral-guest: {err}"),
                }
            }
            Err(err) => match accept_backoff(&err) {
                Some(pause) => {
                    eprintln!("ral-guest: the engine factory will retry an accept: {err}");
                    std::thread::sleep(pause);
                }
                None => {
                    return reap::fail(format!(
                        "the engine factory stopped accepting the host's dials: {err}"
                    ));
                }
            },
        }
    }
}

/// A panicked pusher leaves the fleet consistent; poison recovery is safe.
pub(super) fn live(engines: &Engines) -> MutexGuard<'_, Fleet> {
    engines.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(n: i32) -> Pid {
        Pid::from_raw(n).unwrap()
    }

    #[test]
    fn admission_closes_with_the_snapshot() {
        let mut fleet = Fleet::new(pid(2));
        assert_eq!(fleet.admit(|| Ok::<_, ()>(pid(3))), Some(Ok(pid(3))));
        assert_eq!(fleet.close(), [pid(2), pid(3)]);
        assert_eq!(fleet.admit(|| -> Result<Pid, ()> { unreachable!() }), None);
        assert_eq!(fleet.pids, [pid(2), pid(3)]);
    }

    #[test]
    fn forgetting_says_whether_it_was_an_engine() {
        let mut fleet = Fleet::new(pid(2));
        assert!(fleet.forget(pid(2)));
        assert!(!fleet.forget(pid(2)));
    }
}
