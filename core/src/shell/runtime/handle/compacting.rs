//! A birth-ordered list of handles that forgets consumed ones on an amortized
//! schedule: a sweep visits every entry, and the next waits until the list has
//! doubled, so a push costs amortized O(1) handle samples.

use super::{HandleInner, Sample};
use std::ops::Deref;

/// Minimum and initial length at which a push sweeps.
pub(crate) const MIN_SWEEP_AT: usize = 32;

pub(crate) struct Compacting<T> {
    items: Vec<T>,
    sweep_at: usize,
    /// Entries every sweep has visited, so tests can bound the amortized cost.
    #[cfg(test)]
    visits: usize,
}

impl<T> Default for Compacting<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            sweep_at: MIN_SWEEP_AT,
            #[cfg(test)]
            visits: 0,
        }
    }
}

impl<T: AsRef<HandleInner>> Compacting<T> {
    /// Append `item`, first forgetting consumed entries once the list has
    /// reached its sweep threshold.
    pub(crate) fn push(&mut self, item: T) {
        if self.items.len() >= self.sweep_at {
            self.prune();
        }
        self.items.push(item);
    }

    /// Forget every consumed entry now, and set the next sweep at twice what
    /// remains.
    pub(crate) fn prune(&mut self) {
        self.prune_observing(|_| ());
    }

    /// [`Self::prune`], handing `seen` each entry's one sample, so a caller
    /// can measure the list in the same pass.
    pub(crate) fn prune_observing(&mut self, mut seen: impl FnMut(Sample<()>)) {
        #[cfg(test)]
        {
            self.visits += self.items.len();
        }
        self.items.retain(|item| {
            let sample = item.as_ref().sample();
            seen(sample);
            !matches!(sample, Sample::Consumed)
        });
        self.sweep_at = self.items.len().saturating_mul(2).max(MIN_SWEEP_AT);
    }
}

impl<T> Compacting<T> {
    /// Remove, in order, the entries `pred` selects; `pred` may update the
    /// ones it keeps.
    pub(crate) fn extract_if<F: FnMut(&mut T) -> bool>(
        &mut self,
        pred: F,
    ) -> impl Iterator<Item = T> {
        self.items.extract_if(.., pred)
    }

    pub(crate) fn into_vec(self) -> Vec<T> {
        self.items
    }

    #[cfg(test)]
    pub(crate) fn visits(&self) -> usize {
        self.visits
    }
}

impl<T> Deref for Compacting<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.items
    }
}

impl AsRef<Self> for HandleInner {
    fn as_ref(&self) -> &Self {
        self
    }
}
