//! Persistent lists being built, and one-layer views of list values.

use super::env::Env;
use super::value::{ValClos, read};
use crate::ir::Val;
use crate::source::Spanned;
use imbl::shared_ptr::DefaultSharedPtr;
use std::borrow::Cow;
use std::sync::Arc;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct List(pub(super) Arc<imbl::Vector<ValClos>>);

impl List {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn get(&self, index: usize) -> Option<&ValClos> {
        self.0.get(index)
    }
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &ValClos> + ExactSizeIterator {
        self.0.iter()
    }
    pub(crate) fn push_back(&mut self, v: ValClos) {
        Arc::make_mut(&mut self.0).push_back(v);
    }
    pub(crate) fn push_front(&mut self, v: ValClos) {
        Arc::make_mut(&mut self.0).push_front(v);
    }
    pub(crate) fn append(&mut self, other: Self) {
        Arc::make_mut(&mut self.0).append(Arc::unwrap_or_clone(other.0));
    }
    /// `self` keeps `[0, index)`; the result takes `[index, len)`.
    pub(crate) fn split_off(&mut self, index: usize) -> Self {
        Self(Arc::new(Arc::make_mut(&mut self.0).split_off(index)))
    }
}

impl FromIterator<ValClos> for List {
    fn from_iter<I: IntoIterator<Item = ValClos>>(iter: I) -> Self {
        Self(Arc::new(iter.into_iter().collect()))
    }
}
impl From<Vec<ValClos>> for List {
    fn from(v: Vec<ValClos>) -> Self {
        v.into_iter().collect()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ListRef<'a>(ListRepr<'a>);
#[derive(Clone, Copy, Debug)]
enum ListRepr<'a> {
    Syn(&'a [Spanned<Val>], &'a Env),
    Built(&'a Arc<imbl::Vector<ValClos>>),
}

impl<'a> ListRef<'a> {
    pub(super) fn syn(elems: &'a [Spanned<Val>], env: &'a Env) -> Self {
        Self(ListRepr::Syn(elems, env))
    }
    pub(super) fn built(items: &'a Arc<imbl::Vector<ValClos>>) -> Self {
        Self(ListRepr::Built(items))
    }
    pub fn len(self) -> usize {
        match self.0 {
            ListRepr::Syn(elems, _) => elems.len(),
            ListRepr::Built(items) => items.len(),
        }
    }
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    pub fn get(self, index: usize) -> Option<Cow<'a, ValClos>> {
        match self.0 {
            ListRepr::Syn(elems, env) => elems.get(index).map(|e| read(&e.item, env)),
            ListRepr::Built(items) => items.get(index).map(Cow::Borrowed),
        }
    }
    pub fn iter(self) -> Iter<'a> {
        Iter(match self.0 {
            ListRepr::Syn(elems, env) => IterRepr::Syn(elems.iter(), env),
            ListRepr::Built(items) => IterRepr::Built(items.iter()),
        })
    }
    pub fn to_list(self) -> List {
        match self.0 {
            ListRepr::Built(items) => List(Arc::clone(items)),
            ListRepr::Syn(..) => self.iter().map(Cow::into_owned).collect(),
        }
    }
}

pub struct Iter<'a>(IterRepr<'a>);
enum IterRepr<'a> {
    Syn(std::slice::Iter<'a, Spanned<Val>>, &'a Env),
    Built(imbl::vector::Iter<'a, ValClos, DefaultSharedPtr>),
}
impl<'a> Iterator for Iter<'a> {
    type Item = Cow<'a, ValClos>;
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            IterRepr::Syn(it, env) => it.next().map(|e| read(&e.item, env)),
            IterRepr::Built(it) => it.next().map(Cow::Borrowed),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            IterRepr::Syn(it, _) => it.size_hint(),
            IterRepr::Built(it) => it.size_hint(),
        }
    }
}
impl DoubleEndedIterator for Iter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            IterRepr::Syn(it, env) => it.next_back().map(|e| read(&e.item, env)),
            IterRepr::Built(it) => it.next_back().map(Cow::Borrowed),
        }
    }
}
impl ExactSizeIterator for Iter<'_> {}
impl<'a> IntoIterator for ListRef<'a> {
    type Item = Cow<'a, ValClos>;
    type IntoIter = Iter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::runtime::{Binding, Signature};

    #[test]
    fn a_literal_mutates_by_copy_on_write() {
        let mut env = Env::EMPTY;
        for (name, n) in [("a", 1), ("b", 2)] {
            env.bind(
                name.into(),
                Binding {
                    value: ValClos::int(n),
                    scheme: None,
                },
            );
        }
        let val = Val::list(
            ["a", "b"]
                .into_iter()
                .map(|name| Spanned::synthetic(Val::Variable(name.into())))
                .collect::<Vec<_>>(),
        );
        let literal = ValClos::close(&val, &env, &Signature::default()).unwrap();
        let items = literal.as_list("test").unwrap();
        let mut mutated = items.to_list();
        let tail = mutated.split_off(1);
        assert_eq!(
            mutated.iter().cloned().collect::<Vec<_>>(),
            vec![ValClos::int(1)]
        );
        assert_eq!(
            tail.iter().cloned().collect::<Vec<_>>(),
            vec![ValClos::int(2)]
        );
        assert_eq!(
            items.iter().map(Cow::into_owned).collect::<Vec<_>>(),
            vec![ValClos::int(1), ValClos::int(2)]
        );
        assert!(literal.env().unwrap().ptr_eq(&env));
        let mut pushed = items.to_list();
        pushed.push_back(ValClos::int(3));
        assert_eq!(
            pushed.iter().cloned().collect::<Vec<_>>(),
            vec![ValClos::int(1), ValClos::int(2), ValClos::int(3)]
        );
    }
}
