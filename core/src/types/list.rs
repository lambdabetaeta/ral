//! List value (the inner of `Value::List`).
//!
//! The persistent `imbl::Vector` shares its spine on `clone` and `split_off`
//! rather than copying elements: that is what lets `eval_list` cons onto a
//! spread and `...rest` patterns bind a tail for free, both in `evaluator/`.
//! The newtype keeps `imbl` from leaking past `types/`.
//!
//! A literal (`[1, $x, ...]` with no spread) is the value closure `⟨V,
//! ρ|occ(V)⟩` instead: `Repr::Literal` holds the node and the environment
//! `form` restricted it to, and an element is read by [`inspect`], one layer
//! of the distributive law. The mutators materialize a literal once
//! (`built_mut`), then `Arc::make_mut`: copy-on-write, as `imbl` already is.

use super::env::Env;
use super::value::Value;
use crate::ir::{ListNode, Val};
use imbl::shared_ptr::DefaultSharedPtr;
use std::borrow::Cow;
use std::sync::Arc;

/// A persistent list of `Value`s.
#[derive(Debug, Clone, Default)]
pub struct List(Repr);

#[derive(Debug, Clone)]
enum Repr {
    Built(Arc<imbl::Vector<Value>>),
    Literal(Arc<ListNode>, Env),
}

impl Default for Repr {
    fn default() -> Self {
        Self::Built(Arc::default())
    }
}

/// Cuts the `List → Env → Binding → List → …` chain a literal's captured
/// environment can start, mirroring `Closure`'s drop — it does not pass
/// through a `Closure`, so nothing else trampolines it.
impl Drop for Repr {
    fn drop(&mut self) {
        if let Self::Literal(_, env) = self {
            env.dismantle();
        }
    }
}

impl List {
    pub fn new() -> Self {
        Self::default()
    }

    /// `env` restricted to `node`'s occ — the value closure `⟨V, ρ|occ(V)⟩`,
    /// `form`'s one caller for a list literal with no unbound Σ-only name.
    pub(crate) fn literal(node: &Arc<ListNode>, env: &Env) -> Self {
        Self::captured(Arc::clone(node), env.restrict(node.occ()))
    }

    /// Over an `env` already scoped to `node` — inspection reading a nested
    /// literal out of another (no restriction again), and the fork
    /// scrub rebuilding over a scrubbed environment with the same names.
    pub(crate) fn captured(node: Arc<ListNode>, env: Env) -> Self {
        Self(Repr::Literal(node, env))
    }

    /// The literal's node and the environment it closed over, for the scrub's
    /// door into any handle it holds. `None` for built data.
    pub(crate) fn literal_parts(&self) -> Option<(&Arc<ListNode>, &Env)> {
        match &self.0 {
            Repr::Literal(node, env) => Some((node, env)),
            Repr::Built(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn literal_env(&self) -> Option<&Env> {
        self.literal_parts().map(|(_, env)| env)
    }

    /// The built spine, lent when `self` already is one, materialized from
    /// text otherwise.
    fn materialized(&self) -> Cow<'_, imbl::Vector<Value>> {
        match &self.0 {
            Repr::Built(v) => Cow::Borrowed(v),
            Repr::Literal(node, env) => Cow::Owned(
                node.shape()
                    .iter()
                    .map(|e| inspect(&e.item, env).into_owned())
                    .collect(),
            ),
        }
    }

    /// Materializes a literal once (O(text)), then hands out the unique
    /// `Arc`'s contents to mutate — copy-on-write, as `imbl` already is.
    fn built_mut(&mut self) -> &mut imbl::Vector<Value> {
        if matches!(self.0, Repr::Literal(..)) {
            self.0 = Repr::Built(Arc::new(self.materialized().into_owned()));
        }
        let Repr::Built(v) = &mut self.0 else {
            unreachable!("just materialized")
        };
        Arc::make_mut(v)
    }

    pub fn len(&self) -> usize {
        match &self.0 {
            Repr::Built(v) => v.len(),
            Repr::Literal(node, _) => node.shape().len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: usize) -> Option<Cow<'_, Value>> {
        match &self.0 {
            Repr::Built(v) => v.get(index).map(Cow::Borrowed),
            Repr::Literal(node, env) => node.shape().get(index).map(|e| inspect(&e.item, env)),
        }
    }

    pub fn iter(&self) -> Iter<'_> {
        match &self.0 {
            Repr::Built(v) => Iter(IterRepr::Built(v.iter())),
            Repr::Literal(node, env) => Iter(IterRepr::Literal(node.shape().iter(), env)),
        }
    }

    pub(crate) fn push_back(&mut self, v: Value) {
        self.built_mut().push_back(v);
    }

    pub(crate) fn push_front(&mut self, v: Value) {
        self.built_mut().push_front(v);
    }

    pub(crate) fn set(&mut self, index: usize, v: Value) {
        self.built_mut().set(index, v);
    }

    pub(crate) fn append(&mut self, other: &Self) {
        let other = other.materialized().into_owned();
        self.built_mut().append(other);
    }

    /// `self` keeps `[0, index)`; the returned list takes `[index, len)`.
    pub(crate) fn split_off(&mut self, index: usize) -> Self {
        Self(Repr::Built(Arc::new(self.built_mut().split_off(index))))
    }
}

impl PartialEq for List {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl FromIterator<Value> for List {
    fn from_iter<I: IntoIterator<Item = Value>>(iter: I) -> Self {
        Self(Repr::Built(Arc::new(iter.into_iter().collect())))
    }
}

impl From<Vec<Value>> for List {
    fn from(v: Vec<Value>) -> Self {
        v.into_iter().collect()
    }
}

/// ⟨V, ρ⟩ one layer down, shared between [`List`] and [`super::map::Map`]: a
/// name is read from ρ and lent; a constant is built; a nested literal or
/// thunk closes over the *same* ρ — two refcount bumps, not a restriction.
/// Infallible: `form` builds a `Literal` only when ρ binds its every name.
pub(super) fn inspect<'a>(val: &'a Val, env: &'a Env) -> Cow<'a, Value> {
    match val {
        Val::Unit => Cow::Owned(Value::Unit),
        Val::Int(n) => Cow::Owned(Value::Int(*n)),
        Val::Float(f) => Cow::Owned(Value::Float(*f)),
        Val::Bool(b) => Cow::Owned(Value::Bool(*b)),
        Val::String(s) => Cow::Owned(Value::string(s.clone())),
        Val::Variable(name) => Cow::Borrowed(
            env.get(name)
                .expect("a literal's name is bound in ρ by construction (form's check)"),
        ),
        Val::Thunk(node) => Cow::Owned(Value::Thunk(super::Closure::captured(
            Arc::clone(node.shape()),
            env.clone(),
        ))),
        Val::List(node) => Cow::Owned(Value::List(List::captured(Arc::clone(node), env.clone()))),
        Val::Record(node) | Val::Map(node) => Cow::Owned(Value::Map(super::Map::captured(
            Arc::clone(node),
            env.clone(),
        ))),
        Val::Variant { label, payload } => Cow::Owned(Value::Variant {
            label: label.clone(),
            payload: payload
                .as_deref()
                .map(|p| Box::new(inspect(p, env).into_owned())),
        }),
    }
}

/// Lending iterator over a [`List`].
pub struct Iter<'a>(IterRepr<'a>);

enum IterRepr<'a> {
    Built(imbl::vector::Iter<'a, Value, DefaultSharedPtr>),
    Literal(std::slice::Iter<'a, crate::source::Spanned<Val>>, &'a Env),
}

impl<'a> Iterator for Iter<'a> {
    type Item = Cow<'a, Value>;
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            IterRepr::Built(it) => it.next().map(Cow::Borrowed),
            IterRepr::Literal(it, env) => it.next().map(|e| inspect(&e.item, env)),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            IterRepr::Built(it) => it.size_hint(),
            IterRepr::Literal(it, _) => it.size_hint(),
        }
    }
}

impl DoubleEndedIterator for Iter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            IterRepr::Built(it) => it.next_back().map(Cow::Borrowed),
            IterRepr::Literal(it, env) => it.next_back().map(|e| inspect(&e.item, env)),
        }
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl<'a> IntoIterator for &'a List {
    type Item = Cow<'a, Value>;
    type IntoIter = Iter<'a>;
    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Binding;

    fn literal_node(names: &[&str]) -> Arc<ListNode> {
        let elems: Box<[crate::source::Spanned<Val>]> = names
            .iter()
            .map(|n| crate::source::Spanned::synthetic(Val::Variable((*n).into())))
            .collect();
        ListNode::new(elems)
    }

    /// `split_off` and `push_back` on a literal leave it intact and
    /// yield built lists with the right contents.
    #[test]
    fn a_literal_mutates_by_copy_on_write() {
        let mut env = Env::new();
        env.bind(
            "a".into(),
            Binding {
                value: Value::Int(1),
                scheme: None,
            },
        );
        env.bind(
            "b".into(),
            Binding {
                value: Value::Int(2),
                scheme: None,
            },
        );
        let node = literal_node(&["a", "b"]);
        let literal = List::literal(&node, &env);
        assert!(literal.literal_parts().is_some());

        let mut mutated = literal.clone();
        let tail = mutated.split_off(1);
        assert_eq!(
            mutated.iter().map(Cow::into_owned).collect::<Vec<_>>(),
            vec![Value::Int(1)]
        );
        assert_eq!(
            tail.iter().map(Cow::into_owned).collect::<Vec<_>>(),
            vec![Value::Int(2)]
        );
        assert!(
            literal.literal_parts().is_some(),
            "the original literal is untouched by mutating its clone"
        );
        assert_eq!(
            literal.iter().map(Cow::into_owned).collect::<Vec<_>>(),
            vec![Value::Int(1), Value::Int(2)]
        );

        let mut pushed = literal;
        pushed.push_back(Value::Int(3));
        assert_eq!(
            pushed.iter().map(Cow::into_owned).collect::<Vec<_>>(),
            vec![Value::Int(1), Value::Int(2), Value::Int(3)]
        );
    }
}
