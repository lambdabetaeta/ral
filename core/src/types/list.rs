//! List value (the inner of `Value::List`).
//!
//! Opaque newtype around an `imbl::Vector<Value>`.  The persistent
//! vector is chosen for O(1)-amortised structural sharing on `clone` —
//! list literals and tail-cons patterns clone the spine on every step
//! through the evaluator and through pattern matching.  Wrapping it in
//! a newtype keeps `imbl` an internal implementation detail of `types/`
//! and makes the public surface deliberate: only the operations listed
//! here are exposed to the rest of the tree.

use super::value::Value;
use imbl::shared_ptr::DefaultSharedPtr;

/// A persistent list of `Value`s.  Cheap to clone and to share across
/// scopes; copy-on-write on mutation.
#[derive(Debug, Clone, Default)]
pub struct List(imbl::Vector<Value>);

impl List {
    /// Empty list.
    pub fn new() -> Self {
        Self(imbl::Vector::new())
    }

    /// Length of the list.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// `true` if the list has no elements.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterate the elements in order.
    pub fn iter(&self) -> Iter<'_> {
        Iter(self.0.iter())
    }

    /// Element at `index`, or `None` if out of bounds.
    pub fn get(&self, index: usize) -> Option<&Value> {
        self.0.get(index)
    }

    /// Append `v` to the end.  Copy-on-write on the persistent spine.
    pub fn push_back(&mut self, v: Value) {
        self.0.push_back(v);
    }

    /// Prepend `v` to the front.
    pub fn push_front(&mut self, v: Value) {
        self.0.push_front(v);
    }

    /// Append the contents of `other` onto the end of `self`.
    pub fn append(&mut self, other: Self) {
        self.0.append(other.0);
    }

    /// Split the list at `index`: `self` keeps `[0, index)`, the
    /// returned list contains `[index, len)`.
    pub fn split_off(&mut self, index: usize) -> Self {
        Self(self.0.split_off(index))
    }
}

impl PartialEq for List {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl FromIterator<Value> for List {
    fn from_iter<I: IntoIterator<Item = Value>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// Owning iterator over a [`List`].  Newtype over the underlying imbl
/// consuming iterator so the imbl type does not leak through
/// [`IntoIterator::IntoIter`].
pub struct IntoIter(imbl::vector::ConsumingIter<Value, DefaultSharedPtr>);

impl Iterator for IntoIter {
    type Item = Value;
    fn next(&mut self) -> Option<Value> {
        self.0.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

/// Borrowing iterator over a [`List`].
pub struct Iter<'a>(imbl::vector::Iter<'a, Value, DefaultSharedPtr>);

impl<'a> Iterator for Iter<'a> {
    type Item = &'a Value;
    fn next(&mut self) -> Option<&'a Value> {
        self.0.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl IntoIterator for List {
    type Item = Value;
    type IntoIter = IntoIter;
    fn into_iter(self) -> IntoIter {
        IntoIter(self.0.into_iter())
    }
}

impl<'a> IntoIterator for &'a List {
    type Item = &'a Value;
    type IntoIter = Iter<'a>;
    fn into_iter(self) -> Iter<'a> {
        Iter(self.0.iter())
    }
}

impl std::ops::Index<usize> for List {
    type Output = Value;
    fn index(&self, i: usize) -> &Value {
        &self.0[i]
    }
}

impl Extend<Value> for List {
    fn extend<I: IntoIterator<Item = Value>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

impl From<Vec<Value>> for List {
    fn from(v: Vec<Value>) -> Self {
        Self(v.into())
    }
}
