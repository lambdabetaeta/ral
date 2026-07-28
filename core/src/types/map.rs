//! Map value (the inner of `Value::Map`).
//!
//! Newtype over `imbl::OrdMap`, whose structural sharing keeps `Value::clone`
//! cheap and which stays hidden from the rest of the tree.  Iteration is sorted
//! by key: that is what lets `values_equal` in `core/src/builtins/util.rs` settle
//! map equality with a pointwise zip, and what makes `Value::PartialEq`
//! order-independent for free.

use super::value::Value;

/// A persistent string-keyed map of `Value`s.  Cheap to clone, O(log n) to look
/// up, sorted by key on iteration.
#[derive(Debug, Clone, Default)]
pub struct Map(imbl::OrdMap<String, Value>);

impl Map {
    pub fn new() -> Self {
        Self(imbl::OrdMap::new())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// Insert `key → value`, returning the prior binding if any.
    pub fn insert(&mut self, key: String, value: Value) -> Option<Value> {
        self.0.insert(key, value)
    }

    pub fn iter(&self) -> Iter<'_> {
        Iter(self.0.iter())
    }

    pub fn keys(&self) -> Keys<'_> {
        Keys(self.0.keys())
    }
}

impl PartialEq for Map {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl FromIterator<(String, Value)> for Map {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl From<Vec<(String, Value)>> for Map {
    fn from(v: Vec<(String, Value)>) -> Self {
        Self(v.into_iter().collect())
    }
}

use imbl::shared_ptr::DefaultSharedPtr;

/// Owning iterator over a [`Map`].  A newtype, like its borrowing siblings
/// below, so imbl's pointer-kind generic stays out of the public signatures.
pub struct IntoIter(imbl::ordmap::ConsumingIter<String, Value, DefaultSharedPtr>);

impl Iterator for IntoIter {
    type Item = (String, Value);
    fn next(&mut self) -> Option<(String, Value)> {
        self.0.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

pub struct Iter<'a>(imbl::ordmap::Iter<'a, String, Value, DefaultSharedPtr>);

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a String, &'a Value);
    fn next(&mut self) -> Option<(&'a String, &'a Value)> {
        self.0.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

pub struct Keys<'a>(imbl::ordmap::Keys<'a, String, Value, DefaultSharedPtr>);

impl<'a> Iterator for Keys<'a> {
    type Item = &'a String;
    fn next(&mut self) -> Option<&'a String> {
        self.0.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl IntoIterator for Map {
    type Item = (String, Value);
    type IntoIter = IntoIter;
    fn into_iter(self) -> IntoIter {
        IntoIter(self.0.into_iter())
    }
}

impl<'a> IntoIterator for &'a Map {
    type Item = (&'a String, &'a Value);
    type IntoIter = Iter<'a>;
    fn into_iter(self) -> Iter<'a> {
        Iter(self.0.iter())
    }
}
