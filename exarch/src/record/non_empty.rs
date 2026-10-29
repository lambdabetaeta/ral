//! A list with a first element: non-empty by construction, not by check.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::iter::{self, Chain, Once};
use std::{slice, vec};

/// At least one `T`, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmpty<T> {
    pub first: T,
    pub rest: Vec<T>,
}

impl<T> NonEmpty<T> {
    pub fn one(first: T) -> Self {
        Self {
            first,
            rest: Vec::new(),
        }
    }

    pub fn iter(&self) -> Chain<Once<&T>, slice::Iter<'_, T>> {
        iter::once(&self.first).chain(&self.rest)
    }
}

impl<T> IntoIterator for NonEmpty<T> {
    type Item = T;
    type IntoIter = Chain<Once<T>, vec::IntoIter<T>>;

    fn into_iter(self) -> Self::IntoIter {
        iter::once(self.first).chain(self.rest)
    }
}

impl<'a, T> IntoIterator for &'a NonEmpty<T> {
    type Item = &'a T;
    type IntoIter = Chain<Once<&'a T>, slice::Iter<'a, T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<T: Serialize> Serialize for NonEmpty<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for NonEmpty<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut elements = Vec::<T>::deserialize(deserializer)?.into_iter();
        let first = elements
            .next()
            .ok_or_else(|| de::Error::invalid_length(0, &"at least one element"))?;
        Ok(Self {
            first,
            rest: elements.collect(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
