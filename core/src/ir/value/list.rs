//! Lists: one-layer views, and the lists built from other lists.

use super::{VClosRef, ValClos};
use crate::ir::{Env, Node, Val};
use crate::source::Spanned;

type Items = imbl::Vector<Spanned<Val>>;

/// A list's elements and the environment closing them.
#[derive(Debug, Clone, Copy)]
pub struct ListRef<'a>(&'a Items, &'a Env);

impl ValClos {
    pub fn list(items: Vec<Self>) -> Self {
        let mut env = Env::EMPTY;
        let items = items
            .into_iter()
            .map(|item| Spanned::synthetic(env.place(item)))
            .collect();
        Self::rooted(items, env)
    }
    fn rooted(items: Items, env: Env) -> Self {
        Self(Val::List(Node::new(items)), env)
    }
}

impl<'a> ListRef<'a> {
    pub(super) fn new(items: &'a Items, env: &'a Env) -> Self {
        Self(items, env)
    }
    pub fn len(self) -> usize {
        self.0.len()
    }
    pub fn is_empty(self) -> bool {
        self.0.is_empty()
    }
    pub fn get(self, index: usize) -> Option<VClosRef<'a>> {
        self.0
            .get(index)
            .map(|elem| VClosRef::sub(&elem.item, self.1))
    }
    pub fn iter(self) -> impl DoubleEndedIterator<Item = VClosRef<'a>> + ExactSizeIterator {
        self.0
            .iter()
            .map(move |elem| VClosRef::sub(&elem.item, self.1))
    }
    /// `[...self, last]`, in O(log n).
    pub fn snoc(self, last: ValClos) -> ValClos {
        let mut env = self.1.clone();
        let mut items = self.0.clone();
        items.push_back(Spanned::synthetic(env.place(last)));
        ValClos::rooted(items, env)
    }
    /// `[...self, ...other]`: the larger side keeps its environment and takes
    /// the smaller side's elements, in O(smaller side).
    pub fn concat(self, other: ListRef<'_>) -> ValClos {
        if self.1.ptr_eq(other.1) {
            let mut items = self.0.clone();
            items.append(other.0.clone());
            ValClos::rooted(items, self.1.clone())
        } else if self.len() >= other.len() {
            let (env, tail) = self.graft(other);
            let mut items = self.0.clone();
            items.append(tail);
            ValClos::rooted(items, env)
        } else {
            let (env, mut items) = other.graft(self);
            items.append(other.0.clone());
            ValClos::rooted(items, env)
        }
    }
    /// The elements from `from` on, in the same environment, in O(log n). The
    /// result keeps the bindings of the elements it drops.
    pub fn suffix(self, from: usize) -> ValClos {
        ValClos::rooted(self.0.skip(from), self.1.clone())
    }
    /// `guest`'s elements as elements closed over this environment, extended.
    fn graft(self, guest: ListRef<'_>) -> (Env, Items) {
        let mut env = self.1.clone();
        let items = guest
            .iter()
            .map(|elem| Spanned::synthetic(env.place(elem.into_owned())))
            .collect();
        (env, items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{View, block_over};

    fn list(value: &ValClos) -> ListRef<'_> {
        let View::List(items) = value.view() else {
            panic!("a list")
        };
        items
    }
    fn ints(value: &ValClos) -> Vec<Option<i64>> {
        list(value).iter().map(VClosRef::as_int).collect()
    }
    fn block() -> ValClos {
        block_over(&Env::EMPTY)
    }
    fn handles(count: usize) -> ValClos {
        ValClos::list((0..count).map(|_| ValClos::withheld("h")).collect())
    }
    fn first_is_variable(value: &ValClos) -> bool {
        let (Val::List(node), _) = value.parts() else {
            panic!("a list")
        };
        matches!(node.shape()[0].item, Val::Variable(_))
    }

    #[test]
    fn a_built_list_binds_what_it_cannot_inline_under_item_names() {
        let value = ValClos::list(vec![ValClos::withheld("h"), ValClos::int(1), block()]);
        let (_, env) = value.parts();
        let mut names: Vec<_> = env.names().map(ToString::to_string).collect();
        names.sort();
        assert_eq!(names, ["%item0", "%item1"]);
        let shapes: Vec<_> = list(&value).iter().map(VClosRef::type_name).collect();
        assert_eq!(shapes, ["Handle", "Int", "Block"]);
    }
    #[test]
    fn a_nested_list_is_bound_never_inlined() {
        let value = ValClos::list(vec![ValClos::int(1), ValClos::list(vec![ValClos::int(2)])]);
        let (Val::List(node), env) = value.parts() else {
            panic!("a list")
        };
        assert!(matches!(node.shape()[1].item, Val::Variable(_)));
        assert_eq!(env.len(), 1);
        assert_eq!(value.to_string(), "[1, [2]]");
    }
    #[test]
    fn snoc_and_suffix_keep_the_environment_and_leave_the_original_alone() {
        let base = ValClos::list(vec![ValClos::int(1), ValClos::int(2), ValClos::int(3)]);
        let (snoced, tail) = (list(&base).snoc(ValClos::int(4)), list(&base).suffix(1));
        for derived in [&snoced, &tail] {
            assert!(derived.parts().1.ptr_eq(base.parts().1));
        }
        assert_eq!(ints(&snoced), [1, 2, 3, 4].map(Some));
        assert_eq!(ints(&tail), [2, 3].map(Some));
        assert_eq!(ints(&base), [1, 2, 3].map(Some));
        assert_eq!(list(&base).suffix(0).to_string(), base.to_string());
        assert_eq!(list(&base).suffix(3).to_string(), "[]");
    }
    #[test]
    fn a_short_list_joins_a_longer_one_by_binding_in_its_environment() {
        let base = handles(2);
        let head = ValClos::list(vec![ValClos::withheld("new")]);
        let consed = list(&head).concat(list(&base));
        let (_, env) = consed.parts();
        assert_eq!(env.len(), base.parts().1.len() + 1);
        assert!(first_is_variable(&consed));
        assert!(env.binding("%item2").is_some());
        assert!(base.parts().1.binding("%item2").is_none());
        assert_eq!(list(&consed).len(), 3);
    }
    #[test]
    fn a_tail_keeps_the_bindings_of_the_elements_it_drops() {
        let base = handles(3);
        let tail = list(&base).suffix(2);
        assert_eq!(list(&tail).len(), 1);
        assert!(tail.parts().1.ptr_eq(base.parts().1));
        assert_eq!(tail.parts().1.len(), 3);
    }
    #[test]
    fn concatenating_lists_with_one_environment_appends_the_nodes() {
        let (left, right) = (
            ValClos::list(vec![ValClos::int(1)]),
            ValClos::list(vec![ValClos::int(2)]),
        );
        let joined = list(&left).concat(list(&right));
        assert!(joined.parts().1.is_empty());
        assert_eq!(ints(&joined), [1, 2].map(Some));
    }
    #[test]
    fn concatenation_binds_the_smaller_side_in_the_larger_ones_environment() {
        let (small, large) = (handles(1), handles(2));
        for (joined, size) in [
            (list(&small).concat(list(&large)), 3),
            (list(&large).concat(list(&small)), 3),
        ] {
            assert_eq!(list(&joined).len(), size);
            assert_eq!(joined.parts().1.len(), 3);
            assert!(
                joined
                    .parts()
                    .1
                    .iter()
                    .all(|(_, binding)| matches!(binding.value.view(), View::Handle(_)))
            );
        }
        assert!(
            list(&small)
                .concat(list(&large))
                .parts()
                .1
                .binding("%item2")
                .is_some()
        );
    }
    #[test]
    fn concatenation_inlines_atoms_and_never_a_list_or_a_thunk() {
        let large = handles(2);
        let atoms = ValClos::list(vec![ValClos::int(7)]);
        let joined = list(&atoms).concat(list(&large));
        assert_eq!(joined.parts().1.len(), 2);
        assert!(!first_is_variable(&joined));
        assert_eq!(list(&joined).get(0).unwrap().as_int(), Some(7));

        let lists = ValClos::list(vec![ValClos::list(vec![ValClos::int(7)])]);
        let blocks = ValClos::list(vec![block()]);
        for (guest, type_name) in [(lists, "List"), (blocks, "Block")] {
            let joined = list(&guest).concat(list(&large));
            assert_eq!(joined.parts().1.len(), 3);
            assert!(first_is_variable(&joined));
            assert_eq!(list(&joined).get(0).unwrap().type_name(), type_name);
        }
    }
}
