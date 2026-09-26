//! Map value (the inner of `Value::Map`).
//!
//! Newtype over `imbl::OrdMap`, whose structural sharing keeps `Value::clone`
//! cheap and which stays hidden from the rest of the tree.  Iteration is sorted
//! by key: that is what lets `values_equal` in `core/src/builtins/util.rs` settle
//! map equality with a pointwise zip, and what makes `Value::PartialEq`
//! order-independent for free.

use super::value::Value;
use imbl::shared_ptr::DefaultSharedPtr;
use std::borrow::Cow;
use std::sync::Arc;

/// A persistent string-keyed map of `Value`s.  Cheap to clone, O(log n) to look
/// up, sorted by key on iteration.
#[derive(Debug, Clone, Default)]
pub struct Map(Repr);

#[derive(Debug, Clone, Default)]
struct Repr(Arc<imbl::OrdMap<String, Value>>);

impl Map {
    pub fn new() -> Self {
        Self::default()
    }

    fn built(&self) -> &imbl::OrdMap<String, Value> {
        &self.0.0
    }

    fn built_mut(&mut self) -> &mut imbl::OrdMap<String, Value> {
        Arc::make_mut(&mut self.0.0)
    }

    pub fn len(&self) -> usize {
        self.built().len()
    }

    pub fn is_empty(&self) -> bool {
        self.built().is_empty()
    }

    pub fn get(&self, key: &str) -> Option<Cow<'_, Value>> {
        self.built().get(key).map(Cow::Borrowed)
    }

    pub(crate) fn contains_key(&self, key: &str) -> bool {
        self.built().contains_key(key)
    }

    pub(crate) fn insert(&mut self, key: String, v: Value) {
        self.built_mut().insert(key, v);
    }

    pub fn iter(&self) -> Iter<'_> {
        Iter(self.built().iter())
    }

    pub fn keys(&self) -> Keys<'_> {
        Keys(self.built().keys())
    }
}

impl PartialEq for Map {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl FromIterator<(String, Value)> for Map {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        Self(Repr(Arc::new(iter.into_iter().collect())))
    }
}

impl From<Vec<(String, Value)>> for Map {
    fn from(v: Vec<(String, Value)>) -> Self {
        v.into_iter().collect()
    }
}

/// Lending iterator over a [`Map`]'s entries, sorted by key.
pub struct Iter<'a>(imbl::ordmap::Iter<'a, String, Value, DefaultSharedPtr>);

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a str, Cow<'a, Value>);
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(k, v)| (k.as_str(), Cow::Borrowed(v)))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl ExactSizeIterator for Iter<'_> {}

pub struct Keys<'a>(imbl::ordmap::Keys<'a, String, Value, DefaultSharedPtr>);

impl<'a> Iterator for Keys<'a> {
    type Item = &'a str;
    fn next(&mut self) -> Option<&'a str> {
        self.0.next().map(String::as_str)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl ExactSizeIterator for Keys<'_> {}

impl<'a> IntoIterator for &'a Map {
    type Item = (&'a str, Cow<'a, Value>);
    type IntoIter = Iter<'a>;
    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}
