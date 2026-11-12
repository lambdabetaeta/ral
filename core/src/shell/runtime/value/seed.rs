//! Disposal of seed-created owners, not a global runtime destruction policy.

use super::{Repr, ValClos};
use crate::shell::runtime::{Binding, Env};
use std::sync::Arc;

pub(crate) fn drop_seed_value(value: ValClos) {
    drop_owned(Owned::Value(value));
}
pub(crate) fn drop_seed_env(env: Env) {
    drop_owned(Owned::Env(env));
}
enum Owned {
    Value(ValClos),
    Env(Env),
    Binding(Binding),
}
fn drop_owned(root: Owned) {
    let mut pending = vec![root];
    while let Some(owner) = pending.pop() {
        match owner {
            Owned::Binding(Binding { value, .. }) => pending.push(Owned::Value(value)),
            Owned::Env(env) => match env.into_seed_bindings() {
                Ok(bindings) => pending.extend(bindings.into_iter().map(Owned::Binding)),
                Err(env) => drop(env),
            },
            Owned::Value(mut value) => match &mut value.0 {
                Repr::List(items) => {
                    if let Some(items) = Arc::into_inner(std::mem::take(items)) {
                        pending.extend(items.into_iter().map(Owned::Value));
                    }
                }
                Repr::Map(entries, _) => {
                    if let Some(entries) = Arc::into_inner(std::mem::take(entries)) {
                        pending.extend(entries.into_iter().map(|(_, v)| Owned::Value(v)));
                    }
                }
                Repr::Syn(_, env) => pending.push(Owned::Env(std::mem::take(env))),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{CompKind, Val};
    use crate::shell::runtime::{Map, Mooring};
    use crate::source::Spanned;
    use std::sync::Barrier;

    fn seed_residual(
        value: ValClos,
        shell: &mut crate::Shell,
        entry: &Arc<crate::builtin::BuiltinEntry>,
    ) -> ValClos {
        let partial = crate::shell::evaluator::apply(
            &ValClos::builtin(Arc::clone(entry)),
            vec![value],
            &Mooring::adrift(),
            shell,
        )
        .expect("partial builtin");
        let (comp, env) = partial.expect_thunk("test").unwrap();
        let (seed, displaced) = Env::from_seed_bindings(
            env.iter()
                .map(|(name, b)| (name.clone(), b.clone()))
                .collect(),
        );
        assert!(displaced.is_empty());
        ValClos::thunk(Arc::clone(comp), &seed)
    }
    fn release_concurrently(value: ValClos) {
        let aliases = [value.clone(), value.clone(), value.clone(), value];
        let barrier = Arc::new(Barrier::new(aliases.len()));
        let threads: Vec<_> = aliases
            .into_iter()
            .map(|value| {
                let barrier = Arc::clone(&barrier);
                std::thread::Builder::new()
                    .stack_size(256 * 1024)
                    .spawn(move || {
                        barrier.wait();
                        value.drop_seed();
                    })
                    .unwrap()
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }
    #[test]
    fn seed_cleanup_releases_residual_argument_owners_concurrently_on_small_stacks() {
        let mut shell = crate::test_helper::core_shell();
        let entry = Arc::new(shell.lookup_builtin("has").expect("has builtin"));
        let marker = Arc::new(entry.as_ref().clone());
        let weak = Arc::downgrade(&marker);
        let mut value = ValClos::builtin(marker);
        for _ in 0..30_000 {
            value = seed_residual(value, &mut shell, &entry);
        }
        release_concurrently(value);
        assert!(weak.upgrade().is_none());
    }
    #[test]
    fn seed_cleanup_releases_shared_runtime_owners_concurrently_on_small_stacks() {
        let mut value = ValClos::int(1);
        for _ in 0..30_000 {
            value = ValClos::variant("deep", Some(value));
        }
        let (env, displaced) = Env::from_seed_bindings(vec![(
            "deep".into(),
            Binding {
                value,
                scheme: None,
            },
        )]);
        assert!(displaced.is_empty());
        let comp = Arc::new(Spanned::synthetic(CompKind::Return(Val::Variable(
            "deep".into(),
        ))));
        let value = ValClos::list(vec![ValClos::from(Map::from_seed_values([(
            "closure".into(),
            ValClos::thunk(comp, &env),
        )]))]);
        drop(env);
        let mut shell = crate::test_helper::core_shell();
        let entry = Arc::new(shell.lookup_builtin("has").expect("has builtin"));
        release_concurrently(seed_residual(value, &mut shell, &entry));
    }
}
