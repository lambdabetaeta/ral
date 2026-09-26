//! List value (the inner of `Value::List`).
//!
//! The persistent `imbl::Vector` shares its spine on `clone` and `split_off`
//! rather than copying elements: that is what lets `eval_list` cons onto a
//! spread and `...rest` patterns bind a tail for free, both in `evaluator/`.
//! The newtype keeps `imbl` from leaking past `types/`.
//!
//! `Repr` is behind an `Arc`, mutated by `Arc::make_mut`: cheap to clone,
//! copy-on-write when a unique owner mutates.

use super::value::Value;
use imbl::shared_ptr::DefaultSharedPtr;
use std::borrow::Cow;
use std::sync::Arc;

/// A persistent list of `Value`s.
#[derive(Debug, Clone, Default)]
pub struct List(Repr);

#[derive(Debug, Clone, Default)]
struct Repr(Arc<imbl::Vector<Value>>);

impl List {
    pub fn new() -> Self {
        Self::default()
    }

    fn built(&self) -> &imbl::Vector<Value> {
        &self.0.0
    }

    /// Copy-on-write.
    fn built_mut(&mut self) -> &mut imbl::Vector<Value> {
        Arc::make_mut(&mut self.0.0)
    }

    pub fn len(&self) -> usize {
        self.built().len()
    }

    pub fn is_empty(&self) -> bool {
        self.built().is_empty()
    }

    pub fn get(&self, index: usize) -> Option<Cow<'_, Value>> {
        self.built().get(index).map(Cow::Borrowed)
    }

    pub fn iter(&self) -> Iter<'_> {
        Iter(self.built().iter())
    }

    pub(crate) fn push_back(&mut self, v: Value) {
        self.built_mut().push_back(v);
    }

    pub(crate) fn push_front(&mut self, v: Value) {
        self.built_mut().push_front(v);
    }

    pub(crate) fn set(&mut self, index: usize, v: Value) {
        self.built_mut().set(index, v);
    }

    pub(crate) fn append(&mut self, other: &Self) {
        self.built_mut().append(other.built().clone());
    }

    /// `self` keeps `[0, index)`; the returned list takes `[index, len)`.
    pub(crate) fn split_off(&mut self, index: usize) -> Self {
        Self(Repr(Arc::new(self.built_mut().split_off(index))))
    }
}

impl PartialEq for List {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl FromIterator<Value> for List {
    fn from_iter<I: IntoIterator<Item = Value>>(iter: I) -> Self {
        Self(Repr(Arc::new(iter.into_iter().collect())))
    }
}

impl From<Vec<Value>> for List {
    fn from(v: Vec<Value>) -> Self {
        v.into_iter().collect()
    }
}

/// Lending iterator over a [`List`].
pub struct Iter<'a>(imbl::vector::Iter<'a, Value, DefaultSharedPtr>);

impl<'a> Iterator for Iter<'a> {
    type Item = Cow<'a, Value>;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(Cow::Borrowed)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl DoubleEndedIterator for Iter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back().map(Cow::Borrowed)
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl<'a> IntoIterator for &'a List {
    type Item = Cow<'a, Value>;
    type IntoIter = Iter<'a>;
    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}
