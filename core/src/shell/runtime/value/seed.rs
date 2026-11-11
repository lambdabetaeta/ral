//! Disposal of seed-created owners, not a global runtime destruction policy.

use super::Value;
use crate::shell::runtime::{Binding, Env};

pub(crate) fn drop_seed_value(value: Value) {
    drop_owned(Owned::Value(value));
}

pub(crate) fn drop_seed_env(env: Env) {
    drop_owned(Owned::Env(env));
}

enum Owned {
    Value(Value),
    Env(Env),
    Binding(Binding),
}

fn drop_owned(root: Owned) {
    let mut pending = vec![root];
    while let Some(owner) = pending.pop() {
        match owner {
            Owned::Binding(Binding { value, .. }) => pending.push(Owned::Value(value)),
            Owned::Env(env) => {
                match env.into_seed_bindings() {
                    Ok(bindings) => pending.extend(bindings.into_iter().map(Owned::Binding)),
                    // Ordinary nonempty Small is outside the seed-created owner boundary.
                    Err(env) => drop(env),
                }
            }
            Owned::Value(value) => match value {
                Value::List(list) => {
                    if let Some(values) = list.into_seed_values() {
                        pending.extend(values.into_iter().map(Owned::Value));
                    }
                }
                Value::Map(map) => {
                    if let Some(values) = map.into_seed_values() {
                        pending.extend(values.into_iter().map(|(_, v)| Owned::Value(v)));
                    }
                }
                Value::Variant { payload, .. } => {
                    pending.extend(payload.map(|value| Owned::Value(*value)));
                }
                Value::Thunk(closure) => pending.push(Owned::Env(closure.into_env())),
                Value::Unit
                | Value::Bool(_)
                | Value::Int(_)
                | Value::Float(_)
                | Value::String(_)
                | Value::Bytes(_)
                | Value::Native(_)
                | Value::Handle(_) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::runtime::{Closure, Map};
    use std::sync::{Arc, Barrier};

    #[test]
    fn seed_cleanup_releases_shared_runtime_owners_concurrently_on_small_stacks() {
        let mut value = Value::Int(1);
        for _ in 0..30_000 {
            value = Value::variant("deep", Some(value));
        }
        let (env, displaced) = Env::from_seed_bindings(vec![(
            "deep".into(),
            Binding {
                value,
                scheme: None,
            },
        )]);
        assert!(displaced.is_empty());
        let comp = Arc::new(crate::source::Spanned::synthetic(
            crate::ir::CompKind::Return(crate::ir::Val::Unit),
        ));
        let value = Value::list(vec![Value::Map(Map::from_seed_values([(
            "closure".into(),
            Value::Thunk(Closure::captured(comp, env)),
        )]))]);
        let mut aliases: Vec<_> = (0..3).map(|_| value.clone()).collect();
        aliases.push(value);
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
}
