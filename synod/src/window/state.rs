use crate::wire::WindowReport;
use exarch::port::Port;
use exarch::provider::Holdings;
use exarch::record::AgentId;
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

    pub(crate) fn owned_by_mut(&mut self, generation: u64) -> Option<&mut Handle> {
        self.open()?.as_mut().filter(|h| h.generation == generation)
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

/// Where a message goes: held while the machine boots, then steered.
pub(crate) enum Inlet {
    Booting(Vec<String>),
    Live { port: Port, agent: AgentId },
}

impl Inlet {
    pub(crate) fn steer(&mut self, text: String) -> Result<(), String> {
        match self {
            Self::Booting(messages) => {
                messages.push(text);
                Ok(())
            }
            Self::Live { port, agent } => port
                .submit(*agent, text)
                .map_err(|_| "The assistant has already finished; nothing was sent.".to_string()),
        }
    }

    pub(crate) fn queued(&self) -> Vec<String> {
        match self {
            Self::Booting(messages) => messages.clone(),
            Self::Live { port, agent } => port.queued(*agent).unwrap_or_default(),
        }
    }

    pub(crate) fn recall(&mut self) -> Vec<String> {
        match self {
            Self::Booting(messages) => std::mem::take(messages),
            Self::Live { port, agent } => port.recall(*agent).unwrap_or_default(),
        }
    }

    pub(crate) fn install(&mut self, port: Port, agent: AgentId) -> Vec<String> {
        match std::mem::replace(self, Self::Live { port, agent }) {
            Self::Booting(messages) => messages,
            Self::Live { .. } => Vec::new(),
        }
    }

    pub(crate) fn end(self) {
        if let Self::Live { port, agent } = self {
            let _ended = port.end(agent);
        }
    }
}

/// Generation gates publication; the window's attempt gates incoming messages.
pub(crate) struct Handle {
    pub(crate) generation: u64,
    pub(crate) attempt: String,
    pub(crate) inlet: Inlet,
    pub(crate) join: JoinHandle<()>,
    /// Restart and close stop the baseline before joining its worker.
    pub(crate) baseline_stop: crate::folder::manifest::Stop,
}

impl Handle {
    pub(crate) fn stop(self) -> JoinHandle<()> {
        self.inlet.end();
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
