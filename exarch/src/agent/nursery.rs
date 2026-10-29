//! Helpers remain owned until their threads have joined.

use crate::agent::AgentHandle;
use crate::bus::{WORKER_PANIC_PREFIX, panic_msg};
use crate::record::AgentId;
use std::thread::JoinHandle;

#[derive(Default)]
pub(crate) struct Nursery(Vec<(AgentHandle, Option<Joined<Teardown>>)>);
impl Nursery {
    pub(crate) fn adopt(&mut self, handle: AgentHandle, thread: Joined<Teardown>) {
        self.0.push((handle, Some(thread)));
    }

    pub(crate) fn named(&self, name: &str) -> Option<&AgentHandle> {
        self.0
            .iter()
            .rev()
            .find_map(|(handle, _)| (handle.0.name == name).then_some(handle))
    }

    pub(crate) fn holds(&self, id: AgentId) -> bool {
        self.0
            .iter()
            .any(|(handle, thread)| handle.id() == id && thread.is_some())
    }

    pub(crate) fn join(&mut self, id: AgentId) -> Vec<String> {
        let Some(thread) = self
            .0
            .iter_mut()
            .find(|(handle, _)| handle.id() == id)
            .and_then(|(_, thread)| thread.take())
        else {
            return Vec::new();
        };
        Teardown::from_join(thread.join()).failures
    }

    fn unjoined(&self) -> impl Iterator<Item = &AgentHandle> {
        self.0
            .iter()
            .filter_map(|(handle, thread)| thread.is_some().then_some(handle))
    }

    /// Some unjoined helper owes a report (A5).
    pub(crate) fn owing(&self) -> bool {
        self.unjoined().any(|handle| handle.status().owes_parent)
    }

    pub(crate) fn dismiss_all(&mut self) -> Vec<String> {
        self.unjoined().for_each(AgentHandle::end);
        self.0
            .iter_mut()
            .filter_map(|(_, thread)| thread.take())
            .flat_map(|thread| Teardown::from_join(thread.join()).failures)
            .collect()
    }
}

#[must_use]
pub(crate) struct Joined<T>(Option<JoinHandle<T>>);
impl<T> Joined<T> {
    pub(crate) fn new(thread: JoinHandle<T>) -> Self {
        Self(Some(thread))
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.0
            .as_ref()
            .expect("a thread is joined once")
            .is_finished()
    }

    pub(crate) fn join(mut self) -> std::thread::Result<T> {
        self.0.take().expect("a thread is joined once").join()
    }
}
impl<T> Drop for Joined<T> {
    fn drop(&mut self) {
        if let Some(thread) = self.0.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) struct Teardown {
    pub(crate) failures: Vec<String>,
}

impl Teardown {
    pub(crate) fn from_join(result: std::thread::Result<Self>) -> Self {
        result.unwrap_or_else(|payload| Self {
            failures: vec![format!("{WORKER_PANIC_PREFIX}{}", panic_msg(&payload))],
        })
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
