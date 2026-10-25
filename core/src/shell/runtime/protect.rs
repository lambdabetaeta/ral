//! Delayed obligations at an imported weak value's membrane.

use std::sync::Arc;

use crate::ir::{Args, CompKind, Pattern, Val, ValListElem};
use crate::source::Spanned;
use crate::ty::{Id, Shape, Site};

use super::{Binding, Closure, Env, Settled, Shell, Value};

impl Site {
    fn project(&self, root: Id) -> Arc<Self> {
        Arc::new(Self {
            nodes: Arc::clone(&self.nodes),
            root,
            unit: self.unit,
        })
    }

    pub(crate) fn protect(&self, value: Value, shell: &Shell) -> Settled<Value> {
        self.admit(&value, shell.fixings())
            .map_err(|m| m.refusal("imported value", shell))?;
        Ok(self.wrap(value))
    }

    /// Install obligations without inspecting delayed results.
    pub(crate) fn wrap(&self, value: Value) -> Value {
        match (&self.nodes[self.root].shape, value) {
            (Shape::Thunk(body), raw @ (Value::Thunk(_) | Value::Native { .. })) => {
                self.wrap_thunk(*body, raw)
            }
            // Internal projections suspend the remaining computation contract.
            (Shape::Fun(..) | Shape::Return(..), raw @ Value::Thunk(_)) => {
                self.wrap_thunk(self.root, raw)
            }
            (Shape::Handle(_, payload), Value::Handle(mut handle)) => {
                handle.guard_payload(self.project(*payload));
                Value::Handle(handle)
            }
            (Shape::List(elem), Value::List(items)) => {
                let site = self.project(*elem);
                Value::list(items.iter().map(|v| site.wrap(v.into_owned())).collect())
            }
            (Shape::Map(elem), Value::Map(items)) => {
                let site = self.project(*elem);
                Value::map(
                    items
                        .iter()
                        .map(|(k, v)| (k.to_string(), site.wrap(v.into_owned())))
                        .collect(),
                )
            }
            (Shape::Record(fields), Value::Map(items)) => Value::map(
                items
                    .iter()
                    .map(|(k, v)| {
                        let value = match fields.labels.iter().find(|(label, _)| label == k) {
                            Some((_, id)) => self.project(*id).wrap(v.into_owned()),
                            None => v.into_owned(),
                        };
                        (k.to_string(), value)
                    })
                    .collect(),
            ),
            (Shape::Variant(fields), Value::Variant { label, payload }) => {
                let payload = payload.map(|v| {
                    match fields.labels.iter().find(|(l, _)| l == label.as_ref()) {
                        Some((_, id)) => Box::new(self.project(*id).wrap(*v)),
                        None => v,
                    }
                });
                Value::Variant { label, payload }
            }
            (_, value) => value,
        }
    }

    fn wrap_thunk(&self, root: Id, raw: Value) -> Value {
        if matches!(self.nodes[root].shape, Shape::FreeComp { .. }) {
            return raw;
        }
        let raw_name = crate::ir::synthetic("membrane", 0);
        let mut env = Env::new();
        env.bind(
            raw_name.clone(),
            Binding {
                value: raw,
                scheme: Some(self.project(root).scheme()),
            },
        );
        let force = Spanned::synthetic(CompKind::Force(Val::Variable(raw_name)));
        let body = match self.nodes[root].shape {
            Shape::Return(_, result) => CompKind::Protect {
                body: Arc::new(force),
                site: self.project(result),
            },
            Shape::Fun(arg, rest) => {
                let param = crate::ir::synthetic("membrane", 1);
                let checked = crate::ir::synthetic("membrane", 2);
                let args: Args = vec![ValListElem::Single(Spanned::synthetic(Val::Variable(
                    checked.clone(),
                )))]
                .into();
                let call = Val::thunk(Arc::new(Spanned::synthetic(CompKind::App {
                    head: Arc::new(force),
                    args,
                })));
                // Form the next wrapper only after beta: even an arrow cycle is guarded.
                let tail = Spanned::synthetic(CompKind::Force(Val::Protected {
                    value: Box::new(call),
                    site: self.project(rest),
                }));
                let body = CompKind::bind(
                    Pattern::Name(checked),
                    Spanned::synthetic(CompKind::Protect {
                        body: Arc::new(Spanned::synthetic(CompKind::Return(Val::Variable(
                            param.clone(),
                        )))),
                        site: self.project(arg),
                    }),
                    tail,
                );
                CompKind::Lam {
                    param: Pattern::Name(param),
                    body: Arc::new(Spanned::synthetic(body)),
                }
            }
            _ => unreachable!("a thunk's contract is a computation"),
        };
        Value::Thunk(Closure::captured(Arc::new(Spanned::synthetic(body)), env))
    }
}
