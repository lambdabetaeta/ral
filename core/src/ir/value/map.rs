//! Maps: one-layer views, and building a map.

use super::{VClosRef, ValClos};
use crate::ir::{Env, Fields, Name, Node, Val};
use crate::source::Spanned;
use std::collections::BTreeMap;

/// A map's entries and the environment closing them.
#[derive(Debug, Clone, Copy)]
pub struct MapRef<'a>(&'a Fields, &'a Env);

impl ValClos {
    /// The last pair wins on a duplicate key.
    pub fn map(pairs: impl IntoIterator<Item = (String, Self)>) -> Self {
        let entries: BTreeMap<Name, Self> = pairs
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect();
        let mut env = Env::EMPTY;
        let fields = entries
            .into_iter()
            .map(|(key, value)| (key, Spanned::synthetic(env.place(value))))
            .collect();
        Self(Val::Map(Node::new(fields)), env)
    }
}

impl<'a> MapRef<'a> {
    pub(super) fn new(fields: &'a Fields, env: &'a Env) -> Self {
        Self(fields, env)
    }
    pub fn len(self) -> usize {
        self.0.len()
    }
    pub fn is_empty(self) -> bool {
        self.0.is_empty()
    }
    pub fn get(self, key: &str) -> Option<VClosRef<'a>> {
        self.0
            .get(key)
            .map(|entry| VClosRef::sub(&entry.item, self.1))
    }
    pub(crate) fn contains_key(self, key: &str) -> bool {
        self.0.contains_key(key)
    }
    /// By ascending key.
    pub fn iter(
        self,
    ) -> impl DoubleEndedIterator<Item = (&'a str, VClosRef<'a>)> + ExactSizeIterator {
        self.0
            .iter()
            .map(move |(key, entry)| (&**key, VClosRef::sub(&entry.item, self.1)))
    }
    pub fn keys(self) -> impl DoubleEndedIterator<Item = &'a str> + ExactSizeIterator {
        self.0.keys().map(|key| &**key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::View;

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
                .map(|(k, n)| ((*k).to_owned(), ValClos::int(*n))),
        )
    }
    /// Syntax with no `ValClos::map` involved.
    fn literal(entries: &[(&str, i64)]) -> ValClos {
        ValClos::from_parts(
            Val::map(
                entries
                    .iter()
                    .map(|(k, n)| (Name::from(*k), Spanned::synthetic(Val::Int(*n)))),
            ),
            Env::EMPTY,
        )
    }
    fn map(value: &ValClos) -> MapRef<'_> {
        let View::Map(entries) = value.view() else {
            panic!("a map")
        };
        entries
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
            let (a, b) = (map(&lit), map(&blt));
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
    fn equality_distinguishes_values_and_keys() {
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
    fn the_last_pair_wins_on_a_duplicate_key() {
        let value = ValClos::map(vec![
            ("k".into(), ValClos::withheld("first")),
            ("k".into(), ValClos::int(2)),
        ]);
        assert_eq!(value, built(&[("k", 2)]));
        assert!(value.parts().1.is_empty());
    }
    #[test]
    fn data_is_inlined_and_the_rest_bound_in_key_order() {
        let value = ValClos::map(vec![
            ("b".into(), ValClos::withheld("h")),
            ("a".into(), ValClos::int(1)),
            ("c".into(), ValClos::withheld("h")),
        ]);
        let (_, env) = value.parts();
        let mut names: Vec<_> = env.names().map(ToString::to_string).collect();
        names.sort();
        assert_eq!(names, ["%item0", "%item1"]);
        assert_eq!(map(&value).keys().collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(map(&value).get("a").and_then(VClosRef::as_int), Some(1));
        assert_eq!(map(&value).get("c").unwrap().type_name(), "Handle");
    }
}
