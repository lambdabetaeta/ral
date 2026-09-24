//! The fork's scrub: the one snapshot law's whole mechanism.
//!
//! It walks scopes, not values — each session map a fork reaches, once, by
//! root identity, dependencies first, on an explicit stack: a stream is a
//! chain of closures, so recursing through one would bound it by the host
//! stack.  Captured scopes form a DAG, so there is no cycle to guard; but a
//! shared root must be remembered, or a chain of definitions is walked once
//! per path to it, exponentially.  Only session tiers are walked: the prelude
//! is baked, and the natives seeded, before any handle exists.

use super::Shell;
use crate::serial::opaque;
use crate::types::{Binding, BindingMap, Closure, Env, List, Map, Value};
use std::sync::Arc;

impl Shell {
    /// Fork this shell ([`Self::fork_session`]) for a child engine to inherit.
    ///
    /// The one snapshot law lands here: no handle is reachable from the fork —
    /// not from its scope at any depth, nor its handler stack, nor its hooks —
    /// so an identity-adopted child and a wire-hatched one, both forked here,
    /// resolve every name to the same value or the same absence.  A handle
    /// becomes its `` `opaque `` placeholder; hooks, the installing host's
    /// lifecycle entry points, stay with that host.  A shell reaching no handle
    /// forks as itself: the fork shares its session map.
    pub fn fork_scrubbed(&self) -> Self {
        let mut fork = self.fork_session();
        let mut scrub = Scrub::default();
        if let Some(bindings) = scrub.scope(fork.env.bindings_root()) {
            fork.env = Env::from_parts(fork.env.natives_arc(), fork.env.prelude_arc(), bindings);
        }
        for v in fork.context.handlers.values_mut() {
            if let Some(scrubbed) = scrub.value(v) {
                *v = scrubbed;
            }
        }
        fork.context.hooks.clear();
        fork
    }
}

/// Every scope visited, with its replacement — `None` when it reaches no
/// handle — found by `ptr_eq` scan, as `InternCtx` finds its roots.
#[derive(Default)]
struct Scrub {
    memo: Vec<(BindingMap, Option<BindingMap>)>,
}

impl Scrub {
    /// `root`'s replacement, `None` if it reaches no handle.
    fn scope(&mut self, root: &BindingMap) -> Option<BindingMap> {
        self.settle(vec![root.clone()]);
        self.replacement(root)
    }

    /// `v`'s replacement, `None` if it reaches no handle.
    fn value(&mut self, v: &Value) -> Option<Value> {
        let mut roots = Vec::new();
        captured_scopes(v, &mut roots);
        self.settle(roots);
        self.rebuild(v)
    }

    /// Memoise every scope reachable from `roots`, each after every scope it
    /// captures.
    fn settle(&mut self, roots: Vec<BindingMap>) {
        let mut stack: Vec<Visit> = roots.into_iter().map(Visit::Open).collect();
        while let Some(visit) = stack.pop() {
            match visit {
                Visit::Open(scope) => {
                    if self.seen(&scope).is_some() {
                        continue;
                    }
                    let mut captured = Vec::new();
                    for binding in scope.values() {
                        captured_scopes(&binding.value, &mut captured);
                    }
                    captured.retain(|s| self.seen(s).is_none());
                    stack.push(Visit::Close(scope));
                    stack.extend(captured.into_iter().map(Visit::Open));
                }
                // A DAG: every scope this one captures was opened above it,
                // so is settled.
                Visit::Close(scope) => {
                    let rebuilt = patched(
                        &scope,
                        scope.iter().filter_map(|(name, binding)| {
                            self.rebuild(&binding.value).map(|value| {
                                let scheme = binding.scheme.clone();
                                (name.clone(), Binding { value, scheme })
                            })
                        }),
                        |map: &mut BindingMap, name, binding| {
                            map.insert(name, binding);
                        },
                    );
                    self.memo.push((scope, rebuilt));
                }
            }
        }
    }

    fn seen(&self, scope: &BindingMap) -> Option<&Option<BindingMap>> {
        self.memo
            .iter()
            .find(|(root, _)| root.ptr_eq(scope))
            .map(|(_, replacement)| replacement)
    }

    /// `scope`'s memoised replacement, `None` if it reaches no handle.
    ///
    /// # Panics
    /// If `scope` is unsettled: a traversal bug.
    fn replacement(&self, scope: &BindingMap) -> Option<BindingMap> {
        self.seen(scope)
            .expect("a captured scope is settled before any value holding it is rebuilt")
            .clone()
    }

    /// `v` with every handle it reaches replaced, `None` if it reaches none.
    /// Recurses through data alone: a closure's scope is already settled.
    ///
    /// # Panics
    /// If `v` holds a closure over an unsettled scope (see [`Self::replacement`]).
    fn rebuild(&self, v: &Value) -> Option<Value> {
        match v {
            Value::Handle(_) => Some(Value::from(opaque(v))),
            Value::List(items) => patched(
                items,
                items
                    .iter()
                    .enumerate()
                    .filter_map(|(i, item)| self.rebuild(item).map(|new| (i, new))),
                List::set,
            )
            .map(Value::List),
            Value::Map(entries) => patched(
                entries,
                entries
                    .iter()
                    .filter_map(|(key, item)| self.rebuild(item).map(|new| (key.clone(), new))),
                Map::insert,
            )
            .map(Value::Map),
            Value::Variant { label, payload } => {
                let payload = self.rebuild(payload.as_deref()?)?;
                Some(Value::Variant {
                    label: label.clone(),
                    payload: Some(Box::new(payload)),
                })
            }
            Value::Native { entry, applied } => patched(
                applied,
                applied
                    .iter()
                    .enumerate()
                    .filter_map(|(i, arg)| self.rebuild(arg).map(|new| (i, new))),
                |args: &mut Vec<Value>, i, new| args[i] = new,
            )
            .map(|applied| Value::Native {
                entry: Arc::clone(entry),
                applied,
            }),
            Value::Thunk(closure) => {
                let env = &closure.env;
                let bindings = self.replacement(env.bindings_root())?;
                Some(Value::Thunk(Closure {
                    comp: Arc::clone(&closure.comp),
                    env: Env::from_parts(env.natives_arc(), env.prelude_arc(), bindings),
                }))
            }
            Value::Unit
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::String(_)
            | Value::Bytes(_) => None,
        }
    }
}

/// A scope on the walk's stack: to open, pushing every scope it captures, or
/// to close, once they are settled.
enum Visit {
    Open(BindingMap),
    Close(BindingMap),
}

/// `container` with each change put into a copy, or `None` if there is none:
/// what changed is replaced, the rest stays shared.
fn patched<C: Clone, K, V>(
    container: &C,
    changes: impl IntoIterator<Item = (K, V)>,
    mut put: impl FnMut(&mut C, K, V),
) -> Option<C> {
    let mut changes = changes.into_iter().peekable();
    changes.peek()?;
    let mut copy = container.clone();
    for (key, new) in changes {
        put(&mut copy, key, new);
    }
    Some(copy)
}

/// Push the session map of every closure `v` holds, through data and a
/// native's applied arguments, never entering a closure.
fn captured_scopes(v: &Value, out: &mut Vec<BindingMap>) {
    match v {
        Value::Thunk(closure) => out.push(closure.env.bindings_root().clone()),
        Value::List(items) => {
            for item in items {
                captured_scopes(item, out);
            }
        }
        Value::Map(entries) => {
            for (_, item) in entries {
                captured_scopes(item, out);
            }
        }
        Value::Variant {
            payload: Some(p), ..
        } => captured_scopes(p, out),
        Value::Native { applied, .. } => {
            for arg in applied {
                captured_scopes(arg, out);
            }
        }
        Value::Variant { payload: None, .. }
        | Value::Unit
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::Bytes(_)
        | Value::Handle(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{block_over, idle_handle};

    fn captured(v: Option<&Value>) -> &Env {
        match v {
            Some(Value::Thunk(closure)) => &closure.env,
            other => panic!("expected a block, got {other:?}"),
        }
    }

    fn is_placeholder(v: Option<&Value>) -> bool {
        matches!(v, Some(Value::Variant { label, .. }) if label == crate::serial::OPAQUE_TAG)
    }

    #[test]
    fn a_handle_free_scope_forks_as_itself() {
        let mut parent = Shell::default();
        parent.set_var("big".into(), Value::string("x".repeat(1 << 20)));
        parent.set_var(
            "list".into(),
            Value::list(vec![Value::Int(1), Value::string("a")]),
        );
        parent.set_var("map".into(), Value::map(vec![("k".into(), Value::Int(2))]));
        let blk = block_over(&parent.env);
        parent.set_var("blk".into(), blk);

        let fork = parent.fork_scrubbed();
        assert!(
            fork.env.bindings_root().ptr_eq(parent.env.bindings_root()),
            "a scope reaching no handle must fork as itself"
        );
    }

    #[test]
    fn a_handle_is_scrubbed_through_every_block_and_nothing_else_is_copied() {
        let mut parent = Shell::default();
        parent.set_var("live".into(), idle_handle());
        let blocks = ["b1", "b2", "b3"];
        for name in blocks {
            let blk = block_over(&parent.env);
            parent.set_var(name.into(), blk);
        }
        parent.set_var("big".into(), Value::string("x".repeat(1 << 20)));

        let fork = parent.fork_scrubbed();
        assert!(
            is_placeholder(fork.env.get("live")),
            "a handle binding must become the placeholder"
        );
        let (Some(Value::String(ours)), Some(Value::String(theirs))) =
            (parent.env.get("big"), fork.env.get("big"))
        else {
            panic!("both shells bind `big` to a string");
        };
        assert_eq!(
            ours.as_ptr(),
            theirs.as_ptr(),
            "an untouched string must share the parent's allocation"
        );
        for name in blocks {
            let (ours, theirs) = (captured(parent.env.get(name)), captured(fork.env.get(name)));
            assert!(
                !theirs.bindings_root().ptr_eq(ours.bindings_root()),
                "`{name}`'s captured scope must be rebuilt"
            );
            assert!(
                is_placeholder(theirs.get("live")),
                "`{name}`'s captured scope must hold the placeholder"
            );
        }
    }

    /// Each definition doubles the paths to the first: a walk without the
    /// memo takes 2^40 steps.
    #[test]
    fn a_chain_of_definitions_is_walked_once_per_scope() {
        let mut parent = Shell::default();
        for i in 0..40 {
            let blk = block_over(&parent.env);
            parent.set_var(format!("d{i}"), blk);
        }

        let fork = parent.fork_scrubbed();
        assert!(
            fork.env.bindings_root().ptr_eq(parent.env.bindings_root()),
            "a handle-free chain of definitions must fork as itself"
        );
    }

    /// On the default test thread's stack: the walk must not recurse per link.
    #[test]
    fn a_deep_chain_over_a_handle_scrubs_to_its_foot() {
        const LINKS: usize = 10_000;
        let mut parent = Shell::default();
        parent.set_var(
            "chain".into(),
            crate::types::deep_block_chain(LINKS, idle_handle()),
        );

        let fork = parent.fork_scrubbed();
        let mut link = fork.env.get("chain");
        for _ in 0..LINKS {
            link = captured(link).get("tail");
        }
        assert!(
            is_placeholder(link),
            "the chain's foot must be the placeholder"
        );
    }
}
