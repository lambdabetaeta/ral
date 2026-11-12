//! The bequest's scrub: the one snapshot law's whole mechanism.
//!
//! It walks environments, not values — each ρ a bequest reaches, once, by root
//! identity, dependencies first, on an explicit stack: a stream is a chain of
//! closures, so recursing through one would bound it by the host stack.
//! Captured environments form a DAG, so there is no cycle to guard; but a
//! shared root must be remembered, or a chain of definitions is walked once
//! per path to it, exponentially.  Σ is never walked: the prelude is baked,
//! and the natives seeded, before any handle exists.

use super::Shell;
use super::construct::{Bequest, Image, Terms};
use crate::shell::grant::SpawnGrant;
use crate::shell::runtime::handler::HandlerFrame;
use crate::shell::runtime::{Binding, Env, ValClos};

impl Shell {
    /// What a child takes: this session's scope and handler frames with every
    /// handle scrubbed (the one snapshot law), its sources, terms, a snapshot
    /// of its fixings, and `grant`.  No hook and no terminal lease: those are
    /// a root's.
    ///
    /// A live handle becomes withheld; handle-free scopes stay shared.
    pub fn bequeath(&self, grant: SpawnGrant) -> Bequest {
        let mut scrub = Scrub::default();
        let env = scrub.scope(&self.env).unwrap_or_else(|| self.env.clone());
        let mut frames: Vec<HandlerFrame> = self.context.handlers.iter().cloned().collect();
        for v in frames.iter_mut().flat_map(HandlerFrame::values_mut) {
            if let Some(scrubbed) = scrub.value(v) {
                *v = scrubbed;
            }
        }
        Bequest {
            image: Image {
                env,
                context: self.context.with_handlers(frames),
                sources: self.session.sources.clone(),
                terms: Terms::of(self),
                fixings: self.session.fixings.snapshot(),
            },
            grant,
        }
    }
}

/// Every environment visited, with its replacement — `None` when it reaches
/// no handle — found by `ptr_eq` scan, as `InternCtx` finds its roots.  Σ is
/// never walked: it is baked and seeded before any handle exists.
#[derive(Default)]
struct Scrub {
    memo: Vec<(Env, Option<Env>)>,
}

impl Scrub {
    /// `root`'s replacement, `None` if it reaches no handle.
    fn scope(&mut self, root: &Env) -> Option<Env> {
        self.settle(vec![root.clone()]);
        self.replacement(root)
    }

    /// `v`'s replacement, `None` if it reaches no handle.
    fn value(&mut self, v: &ValClos) -> Option<ValClos> {
        let mut roots = Vec::new();
        v.captured_envs(&mut roots);
        self.settle(roots);
        v.scrubbed(&mut |env| self.replacement(env))
    }

    /// Memoise every environment reachable from `roots`, each after every
    /// environment it captures.
    fn settle(&mut self, roots: Vec<Env>) {
        let mut stack: Vec<Visit> = roots.into_iter().map(Visit::Open).collect();
        while let Some(visit) = stack.pop() {
            match visit {
                Visit::Open(scope) => {
                    if self.seen(&scope).is_some() {
                        continue;
                    }
                    let mut captured = Vec::new();
                    for (_, binding) in scope.iter() {
                        binding.value.captured_envs(&mut captured);
                    }
                    captured.retain(|s| self.seen(s).is_none());
                    stack.push(Visit::Close(scope));
                    stack.extend(captured.into_iter().map(Visit::Open));
                }
                // A DAG: every scope this one captures was opened above it,
                // so is settled.
                Visit::Close(scope) => {
                    let mut rebuilt = None;
                    for (name, binding) in scope.iter() {
                        if let Some(value) =
                            binding.value.scrubbed(&mut |env| self.replacement(env))
                        {
                            rebuilt.get_or_insert_with(|| scope.clone()).bind(
                                name.clone(),
                                Binding {
                                    value,
                                    scheme: binding.scheme.clone(),
                                },
                            );
                        }
                    }
                    self.memo.push((scope, rebuilt));
                }
            }
        }
    }

    fn seen(&self, scope: &Env) -> Option<&Option<Env>> {
        self.memo
            .iter()
            .find(|(root, _)| root.ptr_eq(scope))
            .map(|(_, replacement)| replacement)
    }

    /// `scope`'s memoised replacement, `None` if it reaches no handle.
    ///
    /// # Panics
    /// If `scope` is unsettled: a traversal bug.
    fn replacement(&self, scope: &Env) -> Option<Env> {
        self.seen(scope)
            .expect("a captured scope is settled before any value holding it is rebuilt")
            .clone()
    }
}

/// A scope on the walk's stack: to open, pushing every scope it captures, or
/// to close, once they are settled.
enum Visit {
    Open(Env),
    Close(Env),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::{Handle, idle_handle};
    use crate::shell::runtime::{Mooring, View, block_over, captured};
    use std::sync::Arc;

    fn is_withheld(v: Option<&ValClos>) -> bool {
        matches!(v.map(ValClos::view), Some(View::Handle(Handle::Withheld(title))) if title.as_ref() == "<test>")
    }

    fn partial(shell: &mut Shell, value: ValClos) -> ValClos {
        let builtin = ValClos::builtin(Arc::new(shell.lookup_builtin("has").expect("has builtin")));
        crate::shell::evaluator::apply(&builtin, vec![value], &Mooring::adrift(), shell)
            .expect("partial application")
    }

    #[test]
    fn residuals_scrub_handles_and_captures_without_copying_untouched_environments() {
        let mut parent = crate::test_helper::core_shell();
        let unchanged = partial(&mut parent, ValClos::int(7));
        let unchanged_env = captured(Some(&unchanged)).clone();
        crate::test_access::bind_untyped(&mut parent, "unchanged", unchanged);
        crate::test_access::bind_untyped(&mut parent, "live", idle_handle());
        let held = ValClos::list(vec![idle_handle(), block_over(&parent.env)]);
        let residual = partial(&mut parent, held);
        crate::test_access::bind_untyped(&mut parent, "partial", residual);

        let fork = parent.bequeath(SpawnGrant::Inherit).image;
        let env = captured(fork.env.get("partial"));
        let held = env.get("%arg0").expect("η binds the first argument");
        let View::List(items) = held.view() else {
            panic!("the residual must retain its held list");
        };
        assert!(is_withheld(items.get(0).as_deref()));
        assert!(is_withheld(captured(items.get(1).as_deref()).get("live")));
        assert!(captured(fork.env.get("unchanged")).ptr_eq(&unchanged_env));
    }

    #[test]
    fn a_handle_free_scope_forks_as_itself() {
        let mut parent = crate::test_helper::core_shell();
        crate::test_access::bind_untyped(&mut parent, "big", ValClos::string("x".repeat(1 << 20)));
        crate::test_access::bind_untyped(
            &mut parent,
            "list",
            ValClos::list(vec![ValClos::int(1), ValClos::string("a")]),
        );
        crate::test_access::bind_untyped(
            &mut parent,
            "map",
            ValClos::map(vec![("k".into(), ValClos::int(2))]),
        );
        let blk = block_over(&parent.env);
        crate::test_access::bind_untyped(&mut parent, "blk", blk);

        let fork = parent.bequeath(SpawnGrant::Inherit).image;
        assert!(
            fork.env.ptr_eq(&parent.env),
            "a scope reaching no handle must fork as itself"
        );
    }

    #[test]
    fn a_handle_is_scrubbed_through_every_block_and_nothing_else_is_copied() {
        let mut parent = crate::test_helper::core_shell();
        crate::test_access::bind_untyped(&mut parent, "live", idle_handle());
        let blocks = ["b1", "b2", "b3"];
        for name in blocks {
            let blk = block_over(&parent.env);
            crate::test_access::bind_untyped(&mut parent, name, blk);
        }
        crate::test_access::bind_untyped(&mut parent, "big", ValClos::string("x".repeat(1 << 20)));

        let fork = parent.bequeath(SpawnGrant::Inherit).image;
        assert!(
            is_withheld(fork.env.get("live")),
            "a handle binding must remain in place, withheld"
        );
        let (Some(View::String(ours)), Some(View::String(theirs))) = (
            parent.env.get("big").map(ValClos::view),
            fork.env.get("big").map(ValClos::view),
        ) else {
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
                !theirs.ptr_eq(ours),
                "`{name}`'s captured scope must be rebuilt"
            );
            assert!(
                is_withheld(theirs.get("live")),
                "`{name}`'s captured scope must retain the withheld handle"
            );
        }
    }

    /// Each definition doubles the paths to the first: a walk without the
    /// memo takes 2^40 steps.
    #[test]
    fn a_chain_of_definitions_is_walked_once_per_scope() {
        let mut parent = crate::test_helper::core_shell();
        for i in 0..40 {
            let blk = block_over(&parent.env);
            crate::test_access::bind_untyped(&mut parent, &format!("d{i}"), blk);
        }

        let fork = parent.bequeath(SpawnGrant::Inherit).image;
        assert!(
            fork.env.ptr_eq(&parent.env),
            "a handle-free chain of definitions must fork as itself"
        );
    }

    #[test]
    fn a_handle_under_a_deep_tag_spine_is_withheld_in_place() {
        let mut parent = crate::test_helper::core_shell();
        let tagged = (0..10_000).fold(idle_handle(), |v, _| ValClos::variant("deep", Some(v)));
        crate::test_access::bind_untyped(&mut parent, "tagged", tagged);

        let fork = parent.bequeath(SpawnGrant::Inherit).image;
        let (labels, terminal) = fork.env.get("tagged").expect("bound").peel_variants();
        assert_eq!(labels.len(), 10_000);
        assert!(is_withheld(Some(&terminal)));
    }

    /// On the default test thread's stack: the walk must not recurse per link.
    #[test]
    fn a_deep_chain_over_a_handle_scrubs_to_its_foot() {
        const LINKS: usize = 10_000;
        let mut parent = crate::test_helper::core_shell();
        crate::test_access::bind_untyped(
            &mut parent,
            "chain",
            crate::shell::runtime::deep_block_chain(LINKS, idle_handle()),
        );

        let fork = parent.bequeath(SpawnGrant::Inherit).image;
        let mut link = fork.env.get("chain");
        for _ in 0..LINKS {
            link = captured(link).get("tail");
        }
        assert!(
            is_withheld(link),
            "the chain's foot must retain the withheld handle"
        );
    }
}
