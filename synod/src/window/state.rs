use crate::wire::WindowReport;
use exarch::provider::Holdings;
use ral_core::sync::LockExt;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

/// Credential scrub runs once before threads; every command shares its outcome.
pub(crate) struct Accounts(Result<Holdings, String>);

impl Accounts {
    pub(crate) fn new(resolved: Result<Holdings, String>) -> Self {
        Self(resolved)
    }

    pub(crate) fn resolved(&self) -> Result<&Holdings, String> {
        self.0.as_ref().map_err(Clone::clone)
    }
}

#[derive(Default)]
pub(crate) struct Running(pub(crate) Arc<Mutex<Owner>>);

pub(crate) enum Owner {
    Open(Option<Handle>),
    Closing,
}

impl Default for Owner {
    fn default() -> Self {
        Self::Open(None)
    }
}

impl Owner {
    pub(crate) fn current(&self) -> Option<&Handle> {
        match self {
            Self::Open(handle) => handle.as_ref(),
            Self::Closing => None,
        }
    }

    pub(crate) fn open(&mut self) -> Option<&mut Option<Handle>> {
        match self {
            Self::Open(handle) => Some(handle),
            Self::Closing => None,
        }
    }

    pub(crate) fn owned_by(&self, generation: u64) -> Option<&Handle> {
        self.current().filter(|h| h.generation == generation)
    }
}

/// Only the first close owns shutdown; consume it outside the owner lock.
#[must_use]
pub(crate) struct Close(Option<Handle>);

impl Close {
    pub(crate) fn wait(self) {
        let Some(handle) = self.0 else { return };
        let join = handle.stop();
        let (done, wait) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _ = join.join();
            let _ = done.send(());
        });
        let _ = wait.recv_timeout(Duration::from_secs(5));
    }
}

impl Running {
    /// Atomically forbid registration and hand shutdown to the first caller.
    pub(crate) fn begin_close(&self) -> Option<Close> {
        let previous = std::mem::replace(&mut *self.0.lock_ignore_poison(), Owner::Closing);
        match previous {
            Owner::Open(handle) => Some(Close(handle)),
            Owner::Closing => None,
        }
    }
}

/// Generation gates publication; the window's attempt gates incoming messages.
pub(crate) struct Handle {
    pub(crate) generation: u64,
    pub(crate) attempt: String,
    pub(crate) sender: mpsc::Sender<String>,
    pub(crate) join: JoinHandle<()>,
    /// Restart and close stop the baseline before joining its worker.
    pub(crate) baseline_stop: crate::folder::manifest::Stop,
}

impl Handle {
    pub(crate) fn stop(self) -> JoinHandle<()> {
        drop(self.sender);
        self.baseline_stop.stop();
        self.join
    }
}

pub(crate) struct Held {
    /// Kept after worker exit so held report cards remain openable.
    pub(crate) folder: PathBuf,
    pub(crate) report: WindowReport,
}

#[derive(Default)]
pub(crate) struct Review(pub(crate) Mutex<Option<Held>>);

/// Single-flight sign-in avoids competing for the loopback port.
#[derive(Default)]
pub(crate) struct SignIn(pub(crate) Mutex<Option<Arc<AtomicBool>>>);
