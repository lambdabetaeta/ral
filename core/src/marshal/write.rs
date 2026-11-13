//! One borrowed DAG walk, with row identities assigned after their dependencies.

use super::wire::{self, Frame, Row, RowId, Value, Wire};
use crate::data::json::RawDatum;
use crate::fact::JsonEncodeFailure;
use crate::ir::{RuntimeData, Val};
use crate::shell::grant::{RawSpawnGrant, SpawnGrant};
use crate::shell::runtime::{Env, Held, Image, RawContext, ValClos};
use std::collections::HashMap;

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
    if let Some(root) = Node::env(&env) {
        writer.visit(root);
    }
    for frame in &context.handlers {
        for value in frame
            .entries
            .iter()
            .map(|entry| &entry.thunk)
            .chain(frame.catch_all.as_ref())
        {
            if let Some(root) = refs(value) {
                writer.visit(root);
            }
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
        env: Node::env(&env).map(|node| writer.id(node)),
        rows: writer.rows,
        context: raw_context,
        sources,
        terms,
        fixings,
        grant: RawSpawnGrant::from_value(grant),
    })
}

#[derive(Clone, Copy)]
enum Node<'a> {
    Env(&'a Env),
    List(&'a imbl::Vector<ValClos>),
    Map(&'a imbl::OrdMap<String, ValClos>),
}

impl<'a> Node<'a> {
    fn env(env: &'a Env) -> Option<Self> {
        env.addr().map(|_| Self::Env(env))
    }

    fn addr(self) -> usize {
        match self {
            Self::Env(env) => env
                .addr()
                .expect("only nonempty environments are nodes")
                .get(),
            Self::List(items) => std::ptr::from_ref(items).addr(),
            Self::Map(entries) => std::ptr::from_ref(entries).addr(),
        }
    }

    fn children(self, mut visit: impl FnMut(&'a ValClos)) {
        match self {
            Self::Env(env) => {
                for (_, binding) in env.iter() {
                    visit(&binding.value);
                }
            }
            Self::List(items) => {
                for value in items {
                    visit(value);
                }
            }
            Self::Map(entries) => {
                for (_, value) in entries {
                    visit(value);
                }
            }
        }
    }
}

/// The sole row reference a value writes, without entering its syntax or ρ.
fn refs(value: &ValClos) -> Option<Node<'_>> {
    match value.held() {
        Held::Syn(Val::Builtin(_) | Val::Handle(_), _) | Held::Failure(_) => None,
        Held::Syn(_, env) => Node::env(env),
        Held::List(items) => Some(Node::List(items)),
        Held::Map(entries) => Some(Node::Map(entries)),
    }
}

enum Visit<'a> {
    Open(Node<'a>),
    Close(Node<'a>),
}

#[derive(Default)]
struct Writer {
    ids: HashMap<usize, RowId>,
    rows: Vec<Row<RuntimeData>>,
}

impl Writer {
    fn visit(&mut self, root: Node<'_>) {
        let mut stack = vec![Visit::Open(root)];
        while let Some(visit) = stack.pop() {
            match visit {
                Visit::Open(node) => {
                    if self.ids.contains_key(&node.addr()) {
                        continue;
                    }
                    stack.push(Visit::Close(node));
                    node.children(|value| {
                        if let Some(child) = refs(value) {
                            stack.push(Visit::Open(child));
                        }
                    });
                }
                // Captures and built containers form a DAG: all edges have closed.
                Visit::Close(node) => {
                    let row = self.row(node);
                    let id = RowId::try_from(self.rows.len())
                        .expect("a marshalled image has fewer than 2^32 rows");
                    self.rows.push(row);
                    self.ids.insert(node.addr(), id);
                }
            }
        }
    }

    fn id(&self, node: Node<'_>) -> RowId {
        self.ids[&node.addr()]
    }

    fn row(&self, node: Node<'_>) -> Row<RuntimeData> {
        match node {
            Node::Env(env) => Row::Env(
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
            ),
            Node::List(items) => Row::List(items.iter().map(|value| self.value(value)).collect()),
            Node::Map(entries) => Row::Map(
                entries
                    .iter()
                    .map(|(key, value)| (key.clone(), self.value(value)))
                    .collect(),
            ),
        }
    }

    fn value(&self, value: &ValClos) -> Value<RuntimeData> {
        let reference = refs(value).map(|node| self.id(node));
        match value.held() {
            Held::Syn(Val::Builtin(entry), _) => Value::Builtin(entry.decl.name.to_string()),
            Held::Syn(Val::Handle(handle), _) => Value::Handle(handle.title().into()),
            Held::Syn(mut val, _) => {
                if matches!(val, Val::Thunk(_))
                    && let Some((entry, remaining)) = value.residual()
                {
                    return Value::Residual {
                        builtin: entry.decl.name.to_string(),
                        env: reference.expect("a residual captures its builtin"),
                        supplied: u32::try_from(entry.decl.fixed_arity() - remaining)
                            .expect("a builtin has fewer than 2^32 arguments"),
                    };
                }
                let mut tags = Vec::new();
                while let Val::Variant { label, payload } = val {
                    tags.push(label.to_string());
                    val = payload;
                }
                Value::Closure {
                    tags,
                    val: val.clone(),
                    env: reference,
                }
            }
            Held::List(_) | Held::Map(_) => {
                Value::Built(reference.expect("a built collection names its row"))
            }
            Held::Failure(fact) => Value::Failure(RawDatum::from_datum(fact.clone())),
        }
    }
}
