//! Map value (the inner of `Value::Map`).
//!
//! Newtype over `imbl::OrdMap`, whose structural sharing keeps `Value::clone`
//! cheap and which stays hidden from the rest of the tree.  Iteration is sorted
//! by key: that is what lets `values_equal` in `core/src/shell/builtins/util.rs` settle
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
use crate::data::FOValue;
use crate::data::datum::Datum as _;
use crate::fact::ErrorFact;
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
    /// `fact.record()`, built, and the fact it projects: what `raise` reads
    /// back.  Indistinguishable from `Built` to everything else.
    Witnessed(Arc<imbl::OrdMap<String, Value>>, Arc<ErrorFact>),
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
    /// Hydrate every pair in order, safely releasing an earlier duplicate.
    pub(crate) fn from_seed_values(values: impl IntoIterator<Item = (String, Value)>) -> Self {
        let mut map = imbl::OrdMap::new();
        for (name, value) in values {
            if let Some(replaced) = map.insert(name, value) {
                super::value::drop_seed_value(replaced);
            }
        }
        Self(Repr::Built(Arc::new(map)))
    }

    /// Consume only seed-built data; its persistent spine has no internal aliases.
    pub(crate) fn into_seed_values(mut self) -> Option<imbl::OrdMap<String, Value>> {
        match &mut self.0 {
            Repr::Built(values) => Arc::into_inner(std::mem::take(values)),
            Repr::Literal(..) | Repr::Witnessed(..) => {
                unreachable!("seed hydration creates only built maps")
            }
        }
    }

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
            Repr::Built(_) | Repr::Witnessed(..) => None,
        }
    }

    /// The record a program reads of `fact`, witnessed by it.
    pub(crate) fn witnessed(fact: ErrorFact) -> Self {
        let FOValue::Map { entries } = fact.record().encode() else {
            unreachable!("a record encodes as a map")
        };
        let map: imbl::OrdMap<String, Value> = entries
            .into_iter()
            .map(|(k, v)| (k, Value::from(v)))
            .collect();
        Self(Repr::Witnessed(Arc::new(map), Arc::new(fact)))
    }

    /// The failure this map is the unchanged record of, if any.
    pub(crate) fn witness(&self) -> Option<&ErrorFact> {
        match &self.0 {
            Repr::Witnessed(_, fact) => Some(fact),
            Repr::Built(_) | Repr::Literal(..) => None,
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
            Repr::Built(m) | Repr::Witnessed(m, _) => Cow::Borrowed(m),
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
    /// A changed record no longer projects its fact, so the witness goes.
    fn built_mut(&mut self) -> &mut imbl::OrdMap<String, Value> {
        if !matches!(self.0, Repr::Built(_)) {
            self.0 = Repr::Built(Arc::new(self.materialized().into_owned()));
        }
        let Repr::Built(m) = &mut self.0 else {
            unreachable!("just materialized")
        };
        Arc::make_mut(m)
    }

    pub fn len(&self) -> usize {
        match &self.0 {
            Repr::Built(m) | Repr::Witnessed(m, _) => m.len(),
            Repr::Literal(node, _) => node.shape().len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, key: &str) -> Option<Cow<'_, Value>> {
        match &self.0 {
            Repr::Built(m) | Repr::Witnessed(m, _) => m.get(key).map(Cow::Borrowed),
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
            Repr::Built(m) | Repr::Witnessed(m, _) => m.contains_key(key),
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
            Repr::Built(m) | Repr::Witnessed(m, _) => Iter(IterRepr::Built(m.iter())),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::Reason;
    use crate::source::{CallSite, Spanned};

    /// Keys given out of order.
    const CASES: &[&[(&str, i64)]] = &[
        &[],
        &[("a", 1)],
        &[("b", 2), ("a", 1), ("c", 3)],
        &[("zz", 9), ("z", 8), ("", 0)],
    ];

    fn built(entries: &[(&str, i64)]) -> Map {
        entries
            .iter()
            .map(|(k, n)| ((*k).to_string(), Value::Int(*n)))
            .collect()
    }

    /// Stably sorted by key, as `form` leaves a literal's node.
    fn literal(entries: &[(&str, i64)]) -> Map {
        let mut fields: Vec<_> = entries
            .iter()
            .map(|(k, n)| ((*k).into(), Spanned::synthetic(Val::Int(*n))))
            .collect();
        fields.sort_by(|a: &(crate::ir::Name, _), b| a.0.cmp(&b.0));
        Map::captured(FieldsNode::new(fields.into()), Env::new())
    }

    fn pairs(map: &Map) -> Vec<(String, Value)> {
        map.iter()
            .map(|(k, v)| (k.to_string(), v.into_owned()))
            .collect()
    }

    #[test]
    fn a_literal_reads_as_the_built_map() {
        for case in CASES {
            let (lit, blt) = (literal(case), built(case));
            assert!(lit.literal_parts().is_some() && blt.literal_parts().is_none());
            assert_eq!(lit.len(), blt.len());
            assert_eq!(lit.is_empty(), blt.is_empty());
            assert_eq!(pairs(&lit), pairs(&blt));
            assert!(pairs(&blt).windows(2).all(|w| w[0].0 < w[1].0));
            for probe in ["", "a", "b", "c", "z", "zz", "absent"] {
                assert_eq!(lit.get(probe), blt.get(probe), "get {probe:?}");
                assert_eq!(
                    lit.contains_key(probe),
                    blt.contains_key(probe),
                    "has {probe:?}"
                );
            }
            assert_eq!(lit, blt);
            assert_eq!(blt, lit);
        }
    }

    #[test]
    fn equality_distinguishes_values_and_keys_across_forms() {
        let base = [("a", 1), ("b", 2)];
        for other in [
            &[("a", 1), ("b", 3)][..],
            &[("a", 1), ("c", 2)],
            &[("a", 1)],
        ] {
            assert_ne!(literal(&base), built(other));
            assert_ne!(built(other), literal(&base));
        }
    }

    #[test]
    fn inserting_into_a_literal_keeps_its_entries() {
        let mut map = literal(&[("a", 1), ("c", 3)]);
        map.insert("b".into(), Value::Int(2));
        map.insert("a".into(), Value::Int(10));
        assert!(map.literal_parts().is_none(), "insert materialises");
        assert_eq!(map, built(&[("a", 10), ("b", 2), ("c", 3)]));
    }

    #[test]
    fn a_literal_lookup_takes_the_first_of_an_equal_run() {
        let map = literal(&[("a", 1), ("a", 2), ("b", 3)]);
        assert_eq!(map.get("a").as_deref(), Some(&Value::Int(1)));
    }

    fn fact() -> ErrorFact {
        let site = CallSite {
            script: "t.ral".into(),
            line: 3,
            col: 5,
        };
        ErrorFact::new(Reason::TimedOut, "sleep".into(), site)
    }

    /// The map a program would build by hand from `fact`'s record.
    fn record_built(fact: &ErrorFact) -> Map {
        let FOValue::Map { entries } = fact.record().encode() else {
            unreachable!("a record encodes as a map")
        };
        entries
            .into_iter()
            .map(|(k, v)| (k, Value::from(v)))
            .collect()
    }

    #[test]
    fn a_witnessed_map_reads_as_the_built_record() {
        let fact = fact();
        let (witnessed, blt) = (Map::witnessed(fact.clone()), record_built(&fact));
        assert_eq!(witnessed.len(), blt.len());
        assert_eq!(witnessed.is_empty(), blt.is_empty());
        assert_eq!(pairs(&witnessed), pairs(&blt));
        for probe in ["frame", "message", "status", "site", "reason"] {
            assert_eq!(witnessed.get(probe), blt.get(probe), "get {probe:?}");
            assert_eq!(
                witnessed.contains_key(probe),
                blt.contains_key(probe),
                "has {probe:?}"
            );
        }
        assert_eq!(witnessed, blt);
        assert_eq!(blt, witnessed);
        assert_eq!(
            FOValue::try_from(&Value::Map(witnessed)).expect("data"),
            fact.record().encode()
        );
    }

    #[test]
    fn inserting_into_a_witnessed_map_drops_the_witness_and_keeps_the_entries() {
        let fact = fact();
        let mut map = Map::witnessed(fact.clone());
        map.insert("extra".into(), Value::Int(1));
        assert_eq!(map.witness(), None);
        let mut expected = record_built(&fact);
        expected.insert("extra".into(), Value::Int(1));
        assert_eq!(map, expected);
    }
}
