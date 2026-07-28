//! List value (the inner of `Value::List`).
//!
//! The persistent `imbl::Vector` shares its spine on `clone` and `split_off`
//! rather than copying elements: that is what lets `eval_list` cons onto a
//! spread and `...rest` patterns bind a tail for free, both in `evaluator/`.
//! The newtype keeps `imbl` from leaking past `types/`.

use super::value::Value;
use imbl::shared_ptr::DefaultSharedPtr;

/// A persistent list of `Value`s: cheap to clone, copy-on-write on mutation.
#[derive(Debug, Clone, Default)]
pub struct List(imbl::Vector<Value>);

impl List {
    pub fn new() -> Self {
        Self(imbl::Vector::new())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> Iter<'_> {
        Iter(self.0.iter())
    }

    pub fn get(&self, index: usize) -> Option<&Value> {
        self.0.get(index)
    }

    pub fn push_back(&mut self, v: Value) {
        self.0.push_back(v);
    }

    pub fn push_front(&mut self, v: Value) {
        self.0.push_front(v);
    }

    pub fn append(&mut self, other: Self) {
        self.0.append(other.0);
    }

    /// `self` keeps `[0, index)`; the returned list takes `[index, len)`.
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

/// Owning iterator over a [`List`], newtyped so imbl's pointer-kind generic
/// stays off the public signature.
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
