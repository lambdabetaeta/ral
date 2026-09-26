//! Map value (the inner of `Value::Map`).
//!
//! Newtype over `imbl::OrdMap`, whose structural sharing keeps `Value::clone`
//! cheap and which stays hidden from the rest of the tree.  Iteration is sorted
//! by key: that is what lets `values_equal` in `core/src/builtins/util.rs` settle
//! map equality with a pointwise zip, and what makes `Value::PartialEq`
//! order-independent for free.
//!
//! A literal (`[key: val, …]` or `[:, key: val, …]` with no spread, no
//! computed key) is the value closure `⟨V, ρ|occ(V)⟩` instead: `Repr::Literal`
//! holds the node — entries already sorted by key — and the environment
//! `form` restricted it to. A lookup takes the first of an equal run, as
//! SPEC §4.5 requires; the checker already refuses one among a plain
//! literal's own keys, so that only matters if entries were merged upstream.

use super::env::Env;
use super::list::inspect;
use super::value::Value;
use crate::ir::{FieldsNode, Val};
use imbl::shared_ptr::DefaultSharedPtr;
use std::borrow::Cow;
use std::sync::Arc;

/// A persistent string-keyed map of `Value`s.  Cheap to clone, O(log n) to look
/// up, sorted by key on iteration.
#[derive(Debug, Clone, Default)]
pub struct Map(Repr);

#[derive(Debug, Clone)]
enum Repr {
    Built(Arc<imbl::OrdMap<String, Value>>),
    Literal(Arc<FieldsNode>, Env),
}

impl Default for Repr {
    fn default() -> Self {
        Self::Built(Arc::default())
    }
}

/// Cuts the `Map → Env → Binding → Map → …` chain a literal's captured
/// environment can start; see `list::Repr`'s drop.
impl Drop for Repr {
    fn drop(&mut self) {
        if let Self::Literal(_, env) = self {
            env.dismantle();
        }
    }
}

impl Map {
    pub fn new() -> Self {
        Self::default()
    }

    /// `env` restricted to `node`'s occ — the value closure `⟨V, ρ|occ(V)⟩`,
    /// `form`'s one caller for a record or map literal with no unbound
    /// Σ-only name.
    pub(crate) fn literal(node: &Arc<FieldsNode>, env: &Env) -> Self {
        Self::captured(Arc::clone(node), env.restrict(node.occ()))
    }

    /// Over an `env` already scoped to `node`; see `List::captured`.
    pub(crate) fn captured(node: Arc<FieldsNode>, env: Env) -> Self {
        Self(Repr::Literal(node, env))
    }

    /// The literal's node and the environment it closed over, for the scrub's
    /// door into any handle it holds. `None` for built data.
    pub(crate) fn literal_parts(&self) -> Option<(&Arc<FieldsNode>, &Env)> {
        match &self.0 {
            Repr::Literal(node, env) => Some((node, env)),
            Repr::Built(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn literal_env(&self) -> Option<&Env> {
        self.literal_parts().map(|(_, env)| env)
    }

    /// The built map, lent when `self` already is one, materialized from text
    /// otherwise.
    fn materialized(&self) -> Cow<'_, imbl::OrdMap<String, Value>> {
        match &self.0 {
            Repr::Built(m) => Cow::Borrowed(m),
            Repr::Literal(node, env) => Cow::Owned(
                node.shape()
                    .iter()
                    .map(|(k, v)| (k.to_string(), inspect(&v.item, env).into_owned()))
                    .collect(),
            ),
        }
    }

    /// Materializes a literal once (O(text)), then hands out the unique
    /// `Arc`'s contents to mutate — copy-on-write, as `imbl` already is.
    fn built_mut(&mut self) -> &mut imbl::OrdMap<String, Value> {
        if matches!(self.0, Repr::Literal(..)) {
            self.0 = Repr::Built(Arc::new(self.materialized().into_owned()));
        }
        let Repr::Built(m) = &mut self.0 else {
            unreachable!("just materialized")
        };
        Arc::make_mut(m)
    }

    pub fn len(&self) -> usize {
        match &self.0 {
            Repr::Built(m) => m.len(),
            Repr::Literal(node, _) => node.shape().len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, key: &str) -> Option<Cow<'_, Value>> {
        match &self.0 {
            Repr::Built(m) => m.get(key).map(Cow::Borrowed),
            Repr::Literal(node, env) => {
                let entries = node.shape();
                let i = entries.partition_point(|(k, _)| k.as_ref() < key);
                entries
                    .get(i)
                    .filter(|(k, _)| k.as_ref() == key)
                    .map(|(_, v)| inspect(&v.item, env))
            }
        }
    }

    pub(crate) fn contains_key(&self, key: &str) -> bool {
        match &self.0 {
            Repr::Built(m) => m.contains_key(key),
            Repr::Literal(node, _) => node
                .shape()
                .binary_search_by(|(k, _)| k.as_ref().cmp(key))
                .is_ok(),
        }
    }

    pub(crate) fn insert(&mut self, key: String, v: Value) {
        self.built_mut().insert(key, v);
    }

    pub fn iter(&self) -> Iter<'_> {
        match &self.0 {
            Repr::Built(m) => Iter(IterRepr::Built(m.iter())),
            Repr::Literal(node, env) => Iter(IterRepr::Literal(node.shape().iter(), env)),
        }
    }

    pub fn keys(&self) -> Keys<'_> {
        Keys(self.iter())
    }
}

impl PartialEq for Map {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl FromIterator<(String, Value)> for Map {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        Self(Repr::Built(Arc::new(iter.into_iter().collect())))
    }
}

impl From<Vec<(String, Value)>> for Map {
    fn from(v: Vec<(String, Value)>) -> Self {
        v.into_iter().collect()
    }
}

/// Lending iterator over a [`Map`]'s entries, sorted by key.
pub struct Iter<'a>(IterRepr<'a>);

enum IterRepr<'a> {
    Built(imbl::ordmap::Iter<'a, String, Value, DefaultSharedPtr>),
    Literal(
        std::slice::Iter<'a, (crate::ir::Name, crate::source::Spanned<Val>)>,
        &'a Env,
    ),
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a str, Cow<'a, Value>);
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            IterRepr::Built(it) => it.next().map(|(k, v)| (k.as_str(), Cow::Borrowed(v))),
            IterRepr::Literal(it, env) => {
                it.next().map(|(k, v)| (k.as_ref(), inspect(&v.item, env)))
            }
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            IterRepr::Built(it) => it.size_hint(),
            IterRepr::Literal(it, _) => it.size_hint(),
        }
    }
}

impl ExactSizeIterator for Iter<'_> {}

pub struct Keys<'a>(Iter<'a>);

impl<'a> Iterator for Keys<'a> {
    type Item = &'a str;
    fn next(&mut self) -> Option<&'a str> {
        self.0.next().map(|(k, _)| k)
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
