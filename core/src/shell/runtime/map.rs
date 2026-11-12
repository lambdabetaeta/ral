//! Persistent maps being built, and one-layer views of map values.

use super::env::Env;
use super::value::{ValClos, read};
use crate::fact::ErrorFact;
use crate::ir::{Name, Val};
use crate::source::Spanned;
use imbl::shared_ptr::DefaultSharedPtr;
use std::borrow::Cow;
use std::sync::Arc;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Map(pub(super) Arc<imbl::OrdMap<String, ValClos>>);
impl Map {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn get(&self, key: &str) -> Option<&ValClos> {
        self.0.get(key)
    }
    pub(crate) fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &ValClos)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }
    pub(crate) fn insert(&mut self, key: String, v: ValClos) {
        Arc::make_mut(&mut self.0).insert(key, v);
    }
    /// Hydrate every pair in order, releasing an earlier duplicate through `drop_seed`.
    pub(crate) fn from_seed_values(values: impl IntoIterator<Item = (String, ValClos)>) -> Self {
        let mut map = imbl::OrdMap::new();
        for (key, value) in values {
            if let Some(replaced) = map.insert(key, value) {
                replaced.drop_seed();
            }
        }
        Self(Arc::new(map))
    }
}
impl FromIterator<(String, ValClos)> for Map {
    fn from_iter<I: IntoIterator<Item = (String, ValClos)>>(iter: I) -> Self {
        Self(Arc::new(iter.into_iter().collect()))
    }
}
impl From<Vec<(String, ValClos)>> for Map {
    fn from(v: Vec<(String, ValClos)>) -> Self {
        v.into_iter().collect()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MapRef<'a>(MapRepr<'a>);
#[derive(Clone, Copy, Debug)]
enum MapRepr<'a> {
    Syn(&'a [(Name, Spanned<Val>)], &'a Env),
    Built(
        &'a Arc<imbl::OrdMap<String, ValClos>>,
        Option<&'a ErrorFact>,
    ),
}
impl<'a> MapRef<'a> {
    pub(super) fn syn(entries: &'a [(Name, Spanned<Val>)], env: &'a Env) -> Self {
        Self(MapRepr::Syn(entries, env))
    }
    pub(super) fn built(
        map: &'a Arc<imbl::OrdMap<String, ValClos>>,
        fact: Option<&'a ErrorFact>,
    ) -> Self {
        Self(MapRepr::Built(map, fact))
    }
    pub fn len(self) -> usize {
        match self.0 {
            MapRepr::Syn(entries, _) => entries.len(),
            MapRepr::Built(map, _) => map.len(),
        }
    }
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    pub fn get(self, key: &str) -> Option<Cow<'a, ValClos>> {
        match self.0 {
            MapRepr::Built(map, _) => map.get(key).map(Cow::Borrowed),
            MapRepr::Syn(entries, env) => entries
                .binary_search_by(|(k, _)| k.as_ref().cmp(key))
                .ok()
                .map(|i| read(&entries[i].1.item, env)),
        }
    }
    pub(crate) fn contains_key(self, key: &str) -> bool {
        match self.0 {
            MapRepr::Built(map, _) => map.contains_key(key),
            MapRepr::Syn(entries, _) => entries
                .binary_search_by(|(k, _)| k.as_ref().cmp(key))
                .is_ok(),
        }
    }
    pub fn iter(self) -> Iter<'a> {
        Iter(match self.0 {
            MapRepr::Built(map, _) => IterRepr::Built(map.iter()),
            MapRepr::Syn(entries, env) => IterRepr::Syn(entries.iter(), env),
        })
    }
    pub fn keys(self) -> Keys<'a> {
        Keys(self.iter())
    }
    pub(crate) fn witness(self) -> Option<&'a ErrorFact> {
        match self.0 {
            MapRepr::Built(_, fact) => fact,
            MapRepr::Syn(..) => None,
        }
    }
    /// A changed record no longer projects its fact.
    pub fn to_map(self) -> Map {
        match self.0 {
            MapRepr::Built(map, _) => Map(Arc::clone(map)),
            MapRepr::Syn(entries, env) => entries
                .iter()
                .map(|(k, v)| (k.to_string(), read(&v.item, env).into_owned()))
                .collect(),
        }
    }
}

pub struct Iter<'a>(IterRepr<'a>);
enum IterRepr<'a> {
    Built(imbl::ordmap::Iter<'a, String, ValClos, DefaultSharedPtr>),
    Syn(std::slice::Iter<'a, (Name, Spanned<Val>)>, &'a Env),
}
impl<'a> Iterator for Iter<'a> {
    type Item = (&'a str, Cow<'a, ValClos>);
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            IterRepr::Built(it) => it.next().map(|(k, v)| (k.as_str(), Cow::Borrowed(v))),
            IterRepr::Syn(it, env) => it.next().map(|(k, v)| (k.as_ref(), read(&v.item, env))),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            IterRepr::Built(it) => it.size_hint(),
            IterRepr::Syn(it, _) => it.size_hint(),
        }
    }
}
impl ExactSizeIterator for Iter<'_> {}
pub struct Keys<'a>(Iter<'a>);
impl<'a> Iterator for Keys<'a> {
    type Item = &'a str;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(k, _)| k)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}
impl ExactSizeIterator for Keys<'_> {}
impl<'a> IntoIterator for MapRef<'a> {
    type Item = (&'a str, Cow<'a, ValClos>);
    type IntoIter = Iter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{FOValue, datum::Datum as _};
    use crate::fact::Reason;
    use crate::ir::FieldsNode;
    use crate::shell::runtime::{Signature, View};
    use crate::source::CallSite;

    const CASES: &[&[(&str, i64)]] = &[
        &[],
        &[("a", 1)],
        &[("b", 2), ("a", 1), ("c", 3)],
        &[("zz", 9), ("z", 8), ("", 0)],
    ];
    fn built(entries: &[(&str, i64)]) -> ValClos {
        ValClos::map(
            entries
                .iter()
                .map(|(k, n)| ((*k).into(), ValClos::int(*n)))
                .collect(),
        )
    }
    fn literal(entries: &[(&str, i64)]) -> ValClos {
        let mut fields: Vec<_> = entries
            .iter()
            .map(|(k, n)| ((*k).into(), Spanned::synthetic(Val::Int(*n))))
            .collect();
        fields.sort_by(|a: &(Name, _), b| a.0.cmp(&b.0));
        ValClos::close(
            &Val::Map(FieldsNode::new(fields.into())),
            &Env::EMPTY,
            &Signature::default(),
        )
        .unwrap()
    }
    fn pairs(map: MapRef<'_>) -> Vec<(String, ValClos)> {
        map.iter()
            .map(|(k, v)| (k.into(), v.into_owned()))
            .collect()
    }
    #[test]
    fn a_literal_reads_as_the_built_map() {
        for case in CASES {
            let (lit, blt) = (literal(case), built(case));
            let (a, b) = (lit.as_map("test").unwrap(), blt.as_map("test").unwrap());
            assert_eq!(a.len(), b.len());
            assert_eq!(a.is_empty(), b.is_empty());
            assert_eq!(pairs(a), pairs(b));
            assert!(pairs(b).windows(2).all(|w| w[0].0 < w[1].0));
            for probe in ["", "a", "b", "c", "z", "zz", "absent"] {
                assert_eq!(a.get(probe), b.get(probe));
                assert_eq!(a.contains_key(probe), b.contains_key(probe));
            }
            assert_eq!(lit, blt);
            assert_eq!(blt, lit);
        }
    }
    #[test]
    fn equality_distinguishes_values_and_keys_across_forms() {
        for other in [
            &[("a", 1), ("b", 3)][..],
            &[("a", 1), ("c", 2)],
            &[("a", 1)],
        ] {
            assert_ne!(literal(&[("a", 1), ("b", 2)]), built(other));
            assert_ne!(built(other), literal(&[("a", 1), ("b", 2)]));
        }
    }
    #[test]
    fn inserting_into_a_literal_keeps_its_entries() {
        let literal = literal(&[("a", 1), ("c", 3)]);
        let mut map = literal.as_map("test").unwrap().to_map();
        map.insert("b".into(), ValClos::int(2));
        map.insert("a".into(), ValClos::int(10));
        assert_eq!(ValClos::from(map), built(&[("a", 10), ("b", 2), ("c", 3)]));
    }
    fn fact() -> ErrorFact {
        ErrorFact::new(
            Reason::TimedOut,
            "sleep".into(),
            CallSite {
                script: "t.ral".into(),
                line: 3,
                col: 5,
            },
        )
    }
    #[test]
    fn a_witnessed_map_reads_as_the_built_record() {
        let fact = fact();
        let witnessed = ValClos::failure(fact.clone());
        let built = ValClos::from(fact.record().encode());
        assert_eq!(witnessed, built);
        assert_eq!(
            FOValue::try_from(&witnessed).expect("data"),
            fact.record().encode()
        );
        let View::Map(map) = witnessed.view() else {
            panic!("failure record")
        };
        assert_eq!(map.witness(), Some(&fact));
        assert_eq!(pairs(map), pairs(built.as_map("test").unwrap()));
    }
    #[test]
    fn inserting_into_a_witnessed_map_drops_the_witness_and_keeps_the_entries() {
        let fact = fact();
        let value = ValClos::failure(fact.clone());
        let mut map = value.as_map("test").unwrap().to_map();
        map.insert("extra".into(), ValClos::int(1));
        let rebuilt = ValClos::from(map);
        assert_eq!(rebuilt.as_map("test").unwrap().witness(), None);
        let mut expected = ValClos::from(fact.record().encode())
            .as_map("test")
            .unwrap()
            .to_map();
        expected.insert("extra".into(), ValClos::int(1));
        assert_eq!(rebuilt, ValClos::from(expected));
    }
}
