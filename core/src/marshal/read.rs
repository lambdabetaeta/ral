//! Reconstruct rows in wire order; only completed rows can be referenced.

use super::wire::{self, RawData, Row, RowId, Value, Wire};
use crate::builtin::{BuiltinEntry, BuiltinTable};
use crate::data::datum::Datum;
use crate::data::json::{RawFOValue, drop_value};
use crate::fact::{
    DataStep, DecodeCause, DecodeFailure, ErrorFact, InvalidResidual, RowKind, UnresolvedRow,
};
use crate::shell::grant::SpawnGrant;
use crate::shell::runtime::{Binding, Env, HandlerEntry, HandlerFrame, Image, Runtime, ValClos};
use crate::ty::Closed;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

mod ir;

pub(super) fn image(
    runtime: &Runtime,
    wire: Wire<RawData>,
) -> Result<(Image, SpawnGrant), DecodeFailure> {
    let mut reader = Reader {
        table: &runtime.builtins,
        builtins: HashMap::new(),
        rows: Vec::with_capacity(wire.rows.len()),
    };
    for (index, row) in wire.rows.iter().enumerate() {
        let row = reader
            .row(row)
            .map_err(|error| error.index(index.into()).field("rows"))?;
        reader.rows.push(row);
    }
    let env = reader
        .environment(wire.env)
        .map_err(|error| error.field("env"))?;
    let mut context = wire
        .context
        .validate_with(|frames| {
            frames
                .iter()
                .enumerate()
                .map(|(index, frame)| {
                    reader
                        .frame(frame)
                        .map_err(|error| error.index(index.into()))
                })
                .collect()
        })
        .map_err(|error| error.field("context"))?;
    context.grants.rehold();
    let grant = wire
        .grant
        .validate()
        .map_err(|error| error.field("grant"))?;
    Ok((
        Image {
            env,
            context,
            sources: wire.sources,
            terms: wire.terms,
            fixings: wire.fixings,
        },
        grant,
    ))
}

enum Read {
    Env(Env),
    Built(ValClos),
}

struct Reader<'a> {
    table: &'a BuiltinTable,
    builtins: HashMap<String, Arc<BuiltinEntry>>,
    rows: Vec<Read>,
}

impl Reader<'_> {
    fn unresolved(&self, row: RowId, wanted: RowKind) -> DecodeFailure {
        DecodeFailure::new(DecodeCause::UnresolvedRow(UnresolvedRow {
            row: u64::from(row).into(),
            wanted,
            before: self.rows.len().into(),
        }))
    }

    fn environment(&self, row: Option<RowId>) -> Result<Env, DecodeFailure> {
        let Some(row) = row else {
            return Ok(Env::EMPTY);
        };
        match usize::try_from(row)
            .ok()
            .and_then(|index| self.rows.get(index))
        {
            Some(Read::Env(env)) => Ok(env.clone()),
            Some(Read::Built(_)) | None => Err(self.unresolved(row, RowKind::Environment)),
        }
    }

    fn built(&self, row: RowId) -> Result<ValClos, DecodeFailure> {
        match usize::try_from(row)
            .ok()
            .and_then(|index| self.rows.get(index))
        {
            Some(Read::Built(value)) => Ok(value.clone()),
            Some(Read::Env(_)) | None => Err(self.unresolved(row, RowKind::Built)),
        }
    }

    fn builtin(&mut self, name: &str) -> Result<Arc<BuiltinEntry>, DecodeFailure> {
        if let Some(entry) = self.builtins.get(name) {
            return Ok(Arc::clone(entry));
        }
        let entry = self
            .table
            .value(name)
            .map(Arc::new)
            .ok_or_else(|| DecodeFailure::new(DecodeCause::UnknownBuiltin(name.into())))?;
        self.builtins.insert(name.into(), Arc::clone(&entry));
        Ok(entry)
    }

    fn row(&mut self, row: &Row<RawData>) -> Result<Read, DecodeFailure> {
        match row {
            Row::Env(entries) => {
                let bindings = entries
                    .iter()
                    .enumerate()
                    .map(|(index, (name, binding))| {
                        self.binding(binding)
                            .map(|binding| (name.as_str().into(), binding))
                            .map_err(|error| error.index(1usize.into()).index(index.into()))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| error.payload("Env"))?;
                Env::from_bindings(bindings).map(Read::Env).map_err(|name| {
                    let mut error =
                        DecodeFailure::new(DecodeCause::DuplicateField(name.to_string()));
                    if let Some((index, _)) = entries
                        .iter()
                        .enumerate()
                        .filter(|(_, (bound, _))| bound.as_str() == name.as_ref())
                        .nth(1)
                    {
                        error = error.index(0usize.into()).index(index.into());
                    }
                    error.payload("Env")
                })
            }
            Row::List(items) => items
                .iter()
                .enumerate()
                .map(|(index, value)| self.value(value).map_err(|error| error.index(index.into())))
                .collect::<Result<Vec<_>, _>>()
                .map(|items| Read::Built(ValClos::list(items)))
                .map_err(|error| error.payload("List")),
            Row::Map(entries) => {
                let mut seen = HashSet::with_capacity(entries.len());
                let mut values = Vec::with_capacity(entries.len());
                for (index, (key, value)) in entries.iter().enumerate() {
                    if !seen.insert(key) {
                        return Err(DecodeFailure::new(DecodeCause::DuplicateField(key.clone()))
                            .index(0usize.into())
                            .index(index.into())
                            .payload("Map"));
                    }
                    let value = self.value(value).map_err(|error| {
                        error
                            .index(1usize.into())
                            .index(index.into())
                            .payload("Map")
                    })?;
                    values.push((key.clone(), value));
                }
                Ok(Read::Built(ValClos::map(values)))
            }
        }
    }

    fn binding(&mut self, binding: &wire::Binding<RawData>) -> Result<Binding, DecodeFailure> {
        let scheme = binding
            .scheme
            .as_ref()
            .map(|scheme| {
                Closed::new(Arc::clone(scheme))
                    .map_err(|open| DecodeFailure::new(DecodeCause::OpenScheme(open)))
            })
            .transpose()
            .map_err(|error| error.field("scheme"))?;
        Ok(Binding {
            value: self
                .value(&binding.value)
                .map_err(|error| error.field("value"))?,
            scheme,
        })
    }

    fn value(&mut self, value: &Value<RawData>) -> Result<ValClos, DecodeFailure> {
        match value {
            Value::Closure { tags, val, env } => {
                let env = self
                    .environment(*env)
                    .map_err(|error| error.field("env").payload("Closure"))?;
                let val =
                    ir::admit_val(val).map_err(|error| error.field("val").payload("Closure"))?;
                ValClos::closure(
                    tags.iter().map(|tag| tag.as_str().into()).collect(),
                    val,
                    env,
                )
                .map_err(|name| {
                    DecodeFailure::new(DecodeCause::OpenClosure(name.to_string()))
                        .payload("Closure")
                })
            }
            Value::Builtin(name) => self
                .builtin(name)
                .map(ValClos::builtin)
                .map_err(|error| error.payload("Builtin")),
            Value::Residual {
                builtin,
                env,
                supplied,
            } => {
                let entry = self
                    .builtin(builtin)
                    .map_err(|error| error.field("builtin").payload("Residual"))?;
                let env = self
                    .environment(Some(*env))
                    .map_err(|error| error.field("env").payload("Residual"))?;
                usize::try_from(*supplied)
                    .ok()
                    .and_then(|supplied| entry.residual(env, supplied))
                    .ok_or_else(|| {
                        DecodeFailure::new(DecodeCause::InvalidResidual(InvalidResidual {
                            name: builtin.clone(),
                            arity: entry.decl.fixed_arity().into(),
                            supplied: u64::from(*supplied).into(),
                        }))
                        .payload("Residual")
                    })
            }
            Value::Handle(title) => Ok(ValClos::withheld(title.as_str())),
            Value::Failure(raw) => {
                let value = raw.validate().map_err(|error| error.payload("Failure"))?;
                let result = ErrorFact::decode(&value)
                    .map(ValClos::failure)
                    .map_err(|error| fact_path(raw, error).payload("Failure"));
                drop_value(value);
                result
            }
            Value::Built(row) => self.built(*row).map_err(|error| error.payload("Built")),
        }
    }

    fn frame(&mut self, frame: &wire::Frame<RawData>) -> Result<HandlerFrame, DecodeFailure> {
        let entries = frame
            .entries
            .iter()
            .enumerate()
            .map(|(index, (name, value))| {
                self.value(value)
                    .map(|value| HandlerEntry::ral_per_name(name.clone(), value))
                    .map_err(|error| {
                        error
                            .index(1usize.into())
                            .index(index.into())
                            .field("entries")
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let catch_all = frame
            .catch_all
            .as_ref()
            .map(|value| self.value(value))
            .transpose()
            .map_err(|error| error.field("catch_all"))?;
        Ok(HandlerFrame {
            entries,
            catch_all,
            kind: frame.kind,
        })
    }
}

/// Datum paths traverse ral data; the wire wraps each step in first-order syntax.
fn fact_path(mut raw: &RawFOValue, error: DecodeFailure) -> DecodeFailure {
    let mut path = Vec::new();
    for step in error.path {
        match (raw, step) {
            (RawFOValue::Map { entries }, DataStep::Field(name)) => {
                let Some((index, (_, value))) = entries
                    .iter()
                    .enumerate()
                    .find(|(_, (key, _))| *key == name)
                else {
                    break;
                };
                path.extend([
                    DataStep::Payload("map".into()),
                    DataStep::Field("entries".into()),
                    DataStep::Index(index.into()),
                    DataStep::Index(1usize.into()),
                ]);
                raw = value;
            }
            (RawFOValue::List { items }, DataStep::Index(index)) => {
                let Some(value) = usize::try_from(index.to_u64())
                    .ok()
                    .and_then(|index| items.get(index))
                else {
                    break;
                };
                path.extend([
                    DataStep::Payload("list".into()),
                    DataStep::Field("items".into()),
                    DataStep::Index(index),
                ]);
                raw = value;
            }
            (RawFOValue::Variant { label, payload }, DataStep::Payload(tag)) if *label == tag => {
                path.extend([
                    DataStep::Payload("variant".into()),
                    DataStep::Field("payload".into()),
                ]);
                let Some(value) = payload else {
                    break;
                };
                raw = value;
            }
            _ => break,
        }
    }
    DecodeFailure {
        path,
        cause: error.cause,
    }
}
