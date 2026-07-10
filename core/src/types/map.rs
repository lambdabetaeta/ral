//! Map value (the inner of `Value::Map`).
//!
//! Opaque newtype around an `imbl::OrdMap<String, Value>`.  The
//! persistent B+tree is chosen for O(1)-amortised structural sharing
//! on `clone` (so `Value::clone` of a record-shaped map stays cheap)
//! and O(log n) lookup at every key-access site (pattern match,
//! `has`, `keys`, the `fail` record-field reader).  Wrapping it in a
//! newtype keeps `imbl` an internal implementation detail of
//! `types/`; only the operations listed here are exposed to the rest
//! of the tree.
//!
//! Iteration order is **sorted by key**, so structural equality is
//! order-independent for free, unifying `Value::PartialEq` with the
//! semantic `equal` builtin.

use super::value::Value;

/// A persistent string-keyed map of `Value`s.  Cheap to clone;
/// copy-on-write on mutation; iterates sorted by key.
#[derive(Debug, Clone, Default)]
pub struct Map(imbl::OrdMap<String, Value>);

impl Map {
    /// Empty map.
    pub fn new() -> Self {
        Self(imbl::OrdMap::new())
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// `true` if the map has no entries.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Look up `key`.  O(log n).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// `true` if `key` is bound.  O(log n).
    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// Insert `key → value`, returning the prior binding if any.
    pub fn insert(&mut self, key: String, value: Value) -> Option<Value> {
        self.0.insert(key, value)
    }

    /// Iterate `(&key, &value)` pairs in sorted-key order.
    pub fn iter(&self) -> Iter<'_> {
        Iter(self.0.iter())
    }

    /// Iterate the keys in sorted order.
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

/// Owning iterator over a [`Map`].  Newtype over the underlying imbl
/// consuming iterator so the imbl pointer-kind generic does not leak
/// through [`IntoIterator::IntoIter`].
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

/// Borrowing iterator over a [`Map`].
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

/// Borrowing iterator over a [`Map`]'s keys.
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
