//! One borrowed DAG walk over environments, with row identities assigned after
//! their dependencies.

use super::wire::{self, Frame, Row, RowId, Value, Wire};
use crate::fact::JsonEncodeFailure;
use crate::ir::{Env, RuntimeData, Val, ValClos};
use crate::shell::grant::{RawSpawnGrant, SpawnGrant};
use crate::shell::runtime::{Image, RawContext};
use std::collections::HashMap;
use std::num::NonZeroUsize;

pub(super) fn wire(
    image: Image,
    grant: &SpawnGrant,
) -> Result<Wire<RuntimeData>, JsonEncodeFailure> {
    let Image {
        env,
        context,
        sources,
        terms,
        fixings,
    } = image;
    let mut writer = Writer::default();
    writer.visit(&env);
    for frame in &context.handlers {
        for value in frame
            .entries
            .iter()
            .map(|entry| &entry.thunk)
            .chain(frame.catch_all.as_ref())
        {
            writer.visit(value.parts().1);
        }
    }
    let raw_context = RawContext::from_value_with(&context, |frames| {
        Ok(frames
            .iter()
            .map(|frame| Frame {
                entries: frame
                    .entries
                    .iter()
                    .map(|entry| (entry.name.to_string(), writer.value(&entry.thunk)))
                    .collect(),
                catch_all: frame.catch_all.as_ref().map(|value| writer.value(value)),
                kind: frame.kind,
            })
            .collect())
    })
    .map_err(|error| error.field("context"))?;
    Ok(Wire {
        env: writer.id(&env),
        rows: writer.rows,
        context: raw_context,
        sources,
        terms,
        fixings,
        grant: RawSpawnGrant::from_value(grant),
    })
}

enum Visit<'a> {
    Open(&'a Env),
    Close(&'a Env, NonZeroUsize),
}

#[derive(Default)]
struct Writer {
    ids: HashMap<NonZeroUsize, RowId>,
    rows: Vec<Row<RuntimeData>>,
}

impl Writer {
    fn visit(&mut self, root: &Env) {
        let mut stack = vec![Visit::Open(root)];
        while let Some(visit) = stack.pop() {
            match visit {
                Visit::Open(env) => {
                    let Some(addr) = env.addr() else { continue };
                    if self.ids.contains_key(&addr) {
                        continue;
                    }
                    stack.push(Visit::Close(env, addr));
                    stack.extend(
                        env.iter()
                            .map(|(_, binding)| Visit::Open(binding.value.parts().1)),
                    );
                }
                // Captures form a DAG: all edges have closed.
                Visit::Close(env, addr) => {
                    let row = self.row(env);
                    let id = RowId::try_from(self.rows.len())
                        .expect("a marshalled image has fewer than 2^32 rows");
                    self.rows.push(row);
                    self.ids.insert(addr, id);
                }
            }
        }
    }

    /// The row of `env`; `None` for ∅.
    fn id(&self, env: &Env) -> Option<RowId> {
        env.addr().map(|addr| self.ids[&addr])
    }

    fn row(&self, env: &Env) -> Row<RuntimeData> {
        Row::Env(
            env.iter()
                .map(|(name, binding)| {
                    (
                        name.to_string(),
                        wire::Binding {
                            value: self.value(&binding.value),
                            scheme: binding.scheme.clone(),
                        },
                    )
                })
                .collect(),
        )
    }

    fn value(&self, value: &ValClos) -> Value<RuntimeData> {
        let (val, env) = value.parts();
        match val {
            Val::Builtin(entry) => Value::Builtin(entry.decl.name.to_string()),
            Val::Handle(handle) => Value::Handle(handle.title().into()),
            _ => {
                if matches!(val, Val::Thunk(_))
                    && let Some((entry, remaining)) = value.residual()
                {
                    return Value::Residual {
                        builtin: entry.decl.name.to_string(),
                        env: self.id(env).expect("a residual captures its builtin"),
                        supplied: u32::try_from(entry.decl.fixed_arity() - remaining)
                            .expect("a builtin has fewer than 2^32 arguments"),
                    };
                }
                let mut tags = Vec::new();
                let mut foot = val;
                while let Val::Variant { label, payload } = foot {
                    tags.push(label.to_string());
                    foot = payload;
                }
                Value::Closure {
                    tags,
                    val: foot.clone(),
                    env: self.id(env),
                }
            }
        }
    }
}
