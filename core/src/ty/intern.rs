//! Hash-consing: one allocation per structure, so that equality of two handles
//! is the equality of their addresses.
//!
//! An [`Interned<N>`] is the only way to hold an `N`: its constructor looks the
//! node up in the table of its sort and hands back the allocation already
//! there, if any.  A node's children are themselves handles, so two nodes are
//! the same structure exactly when their children are the same allocations and
//! the node comparison, which is shallow, says so.  Induction on the term
//! makes `==` on handles structural equality in O(1).
//!
//! The table holds its entries weakly: it is a cache of what is alive, never an
//! owner, so a type nothing refers to any more is freed.  The dead weaks it
//! leaves behind are swept in amortized constant time per insertion.

use crate::sync::LockExt as _;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, Weak};

/// A sort of node that can be interned: it says which table is its own, and
/// what being ground means for it.
pub(super) trait Internable: Eq + Hash + Sized + 'static {
    /// Whether the node contains no variable, given that its children's flags
    /// are already known: [`Interned::is_ground`] on each child.
    fn ground(&self) -> bool;

    /// The one table of this sort.
    fn table() -> &'static Mutex<Table<Self>>;
}

/// A node with what was computed from it once, at interning.
struct Entry<N> {
    node: N,
    /// The structural hash: the node's own, with each child contributing its
    /// cached one.  Deterministic, so a set of handles iterates in the same
    /// order in every run.
    hash: u64,
    /// The node contains no variable of any sort.
    ground: bool,
}

/// A handle on the one live allocation of a node.  `Clone` is an `Arc` clone.
pub(super) struct Interned<N>(Arc<Entry<N>>);

impl<N: Internable> Interned<N> {
    /// The handle for `node`, allocated if no equal node is alive.
    pub(super) fn new(node: N) -> Self {
        N::table().lock_ignore_poison().intern(node)
    }
}

impl<N> Interned<N> {
    pub(super) fn node(&self) -> &N {
        &self.0.node
    }

    /// Whether the node contains no variable of any sort, grade variables
    /// included.  Terms are finite DAGs, so the flag is a fold over children
    /// that were interned before their parent.
    pub(super) fn is_ground(&self) -> bool {
        self.0.ground
    }
}

impl<N> Clone for Interned<N> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// Addresses: every handle came from [`Table::intern`], so one structure has
/// one address.
impl<N> PartialEq for Interned<N> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<N> Eq for Interned<N> {}

/// The cached structural hash, not the address: the address varies between
/// runs, and with it the iteration order of every set of handles.
impl<N> Hash for Interned<N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.0.hash);
    }
}

impl<N: serde::Serialize> serde::Serialize for Interned<N> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.node().serialize(serializer)
    }
}

/// A node is read and interned, so no handle exists that the table did not
/// make.
impl<'de, N: Internable + serde::Deserialize<'de>> serde::Deserialize<'de> for Interned<N> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        N::deserialize(deserializer).map(Self::new)
    }
}

/// The nodes of one sort that are, or lately were, alive.
pub(super) struct Table<N> {
    /// Entries by structural hash; a bucket is short, and may hold dead weaks.
    buckets: HashMap<u64, Vec<Weak<Entry<N>>>>,
    /// The weaks held, live or dead.
    entries: usize,
    /// How many of them were live when the table was last swept.
    live: usize,
}

impl<N: Internable> Table<N> {
    pub(super) fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            entries: 0,
            live: 0,
        }
    }

    /// The handle for `node`: the live entry equal to it, or a new one.  A
    /// dead weak is an absent entry.
    fn intern(&mut self, node: N) -> Interned<N> {
        let hash = structural_hash(&node);
        let bucket = self.buckets.entry(hash).or_default();
        if let Some(hit) = bucket
            .iter()
            .find_map(|weak| weak.upgrade().filter(|entry| entry.node == node))
        {
            return Interned(hit);
        }
        let ground = node.ground();
        let entry = Arc::new(Entry { node, hash, ground });
        bucket.push(Arc::downgrade(&entry));
        self.entries += 1;
        if self.entries > 2 * self.live {
            self.sweep();
        }
        Interned(entry)
    }

    /// Drop every dead weak, and every bucket it leaves empty.  Run when the
    /// table has outgrown twice what survived the last sweep, so the cost of a
    /// sweep is paid for by the insertions since, and a table that churns
    /// through fresh types stays proportional to the ones alive.
    fn sweep(&mut self) {
        self.buckets.retain(|_, bucket| {
            bucket.retain(|weak| weak.strong_count() > 0);
            !bucket.is_empty()
        });
        self.live = self.buckets.values().map(Vec::len).sum();
        self.entries = self.live;
    }
}

fn structural_hash<N: Hash>(node: &N) -> u64 {
    let mut hasher = DefaultHasher::new();
    node.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;

    /// A node sort of its own, so no test shares a table with the checker's.
    /// `Leaf(0)` stands for a variable.
    #[derive(PartialEq, Eq, Hash)]
    enum Tree {
        Leaf(u32),
        Pair(Interned<Self>, Interned<Self>),
    }

    impl Internable for Tree {
        fn ground(&self) -> bool {
            match self {
                Self::Leaf(n) => *n != 0,
                Self::Pair(a, b) => a.is_ground() && b.is_ground(),
            }
        }

        fn table() -> &'static Mutex<Table<Self>> {
            static TABLE: LazyLock<Mutex<Table<Tree>>> = LazyLock::new(|| Mutex::new(Table::new()));
            &TABLE
        }
    }

    fn leaf(n: u32) -> Interned<Tree> {
        Interned::new(Tree::Leaf(n))
    }

    fn pair(a: &Interned<Tree>, b: &Interned<Tree>) -> Interned<Tree> {
        Interned::new(Tree::Pair(a.clone(), b.clone()))
    }

    fn hash_of(handle: &Interned<Tree>) -> u64 {
        let mut hasher = DefaultHasher::new();
        handle.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn the_same_structure_is_the_same_allocation() {
        let built = |n| pair(&leaf(n), &pair(&leaf(n + 1), &leaf(n)));
        let (a, b) = (built(10), built(10));
        assert!(Arc::ptr_eq(&a.0, &b.0));
        assert!(a == b);
    }

    #[test]
    fn different_structures_are_different_allocations() {
        let (x, y) = (leaf(20), leaf(21));
        assert!(x != y);
        assert!(pair(&x, &y) != pair(&y, &x));
        assert!(pair(&x, &x) != x);
    }

    #[test]
    fn equality_is_decided_by_the_children_s_addresses() {
        let (deep, other_deep) = (pair(&leaf(30), &leaf(31)), pair(&leaf(30), &leaf(31)));
        assert!(pair(&deep, &leaf(32)) == pair(&other_deep, &leaf(32)));
    }

    #[test]
    fn the_hash_is_the_structural_one_and_is_cached() {
        let (a, b) = (pair(&leaf(40), &leaf(41)), pair(&leaf(40), &leaf(41)));
        assert_eq!(hash_of(&a), hash_of(&b));
        assert_eq!(a.0.hash, structural_hash(a.node()));
        assert_ne!(hash_of(&a), hash_of(&pair(&leaf(41), &leaf(40))));
    }

    #[test]
    fn a_node_is_ground_when_it_and_every_child_are() {
        let (var, value) = (leaf(0), leaf(50));
        assert!(!var.is_ground());
        assert!(value.is_ground());
        assert!(pair(&value, &value).is_ground());
        assert!(!pair(&value, &var).is_ground());
        assert!(!pair(&pair(&value, &value), &pair(&var, &value)).is_ground());
    }

    #[test]
    fn a_dead_entry_is_absent_and_is_made_anew() {
        let mut table = Table::<Tree>::new();
        let first = table.intern(Tree::Leaf(60));
        let weak = Arc::downgrade(&first.0);
        drop(first);
        assert!(weak.upgrade().is_none());
        let second = table.intern(Tree::Leaf(60));
        assert!(matches!(second.node(), Tree::Leaf(60)));
        assert!(Arc::ptr_eq(&second.0, &table.intern(Tree::Leaf(60)).0));
    }

    #[test]
    fn dead_entries_are_purged_as_the_table_grows() {
        let mut table = Table::<Tree>::new();
        for n in 1..=1000 {
            drop(table.intern(Tree::Leaf(n)));
        }
        assert!(table.entries <= 3, "{} entries survive", table.entries);
        assert_eq!(
            table.buckets.values().map(Vec::len).sum::<usize>(),
            table.entries
        );
    }

    #[test]
    fn a_sweep_keeps_what_is_alive() {
        let mut table = Table::<Tree>::new();
        let kept: Vec<_> = (1..=50).map(|n| table.intern(Tree::Leaf(n))).collect();
        for n in 100..1100 {
            drop(table.intern(Tree::Leaf(n)));
        }
        assert!(table.entries <= 2 * (50 + 1));
        for (n, handle) in (1..=50).zip(&kept) {
            assert!(Arc::ptr_eq(&handle.0, &table.intern(Tree::Leaf(n)).0));
        }
    }
}
