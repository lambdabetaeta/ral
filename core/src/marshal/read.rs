//! Reconstruct rows in wire order; only completed rows can be referenced.

use super::wire::{self, RawData, Row, RowId, Value, Wire};
use crate::builtin::BuiltinEntry;
use crate::data::json::JsonRef;
use crate::fact::{
    DecodeCause, DecodeFailure, Expected, InvalidResidual, RowDefect, RowKind, ShapeFailure,
    UnresolvedRow, ValueShape,
};
use crate::shell::grant::SpawnGrant;
use crate::shell::runtime::{
    Binding, Env, HandlerEntry, HandlerFrame, Image, Runtime, Signature, ValClos, View,
};
use crate::ty::Closed;
use std::collections::HashSet;
use std::sync::Arc;

mod ir;

/// Levels of JSON nesting an image may hold, scalars counted.
// 100 linked files (`MAX_SOURCE_DEPTH`) through `Use`, each at most 64 source levels
// (`NESTING_DEPTH_LIMIT`) at under 8 JSON levels each, bounds every image a parent writes;
// `STACK` holds the recursive drop of a raw tree this deep.
pub(super) const NESTING_LIMIT: usize = 65_536;

/// Stack for decoding and dropping a raw tree up to [`NESTING_LIMIT`] deep, whatever
/// the thread's own.
pub(super) const STACK: usize = 64 << 20;

/// Refuse a document nested past [`NESTING_LIMIT`], before any recursive walk
/// or drop meets it.
///
/// # Errors
/// [`DecodeCause::TooDeep`] at the root.
pub(super) fn nesting(root: JsonRef<'_, '_>) -> Result<(), DecodeFailure> {
    let mut pending = vec![(root, 1)];
    while let Some((node, depth)) = pending.pop() {
        if depth > NESTING_LIMIT {
            return Err(DecodeFailure::new(DecodeCause::TooDeep(
                NESTING_LIMIT.into(),
            )));
        }
        let items = node.items().into_iter().flatten();
        let members = node.members().into_iter().flatten().map(|(_, value)| value);
        pending.extend(items.chain(members).map(|child| (child, depth + 1)));
    }
    Ok(())
}

pub(super) fn image(
    runtime: &Runtime,
    wire: Wire<RawData>,
) -> Result<(Image, SpawnGrant), DecodeFailure> {
    let mut reader = Reader {
        sig: &runtime.sig,
        total: wire.rows.len(),
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
    sig: &'a Signature,
    /// Every row of the document, read or not.
    total: usize,
    rows: Vec<Read>,
}

fn unresolved(row: RowId, wanted: RowKind, defect: RowDefect) -> DecodeFailure {
    DecodeFailure::new(DecodeCause::UnresolvedRow(UnresolvedRow {
        row: u64::from(row).into(),
        wanted,
        defect,
    }))
}

impl Reader<'_> {
    fn earlier(&self, row: RowId) -> Result<&Read, RowDefect> {
        let index = usize::try_from(row).unwrap_or(usize::MAX);
        self.rows.get(index).ok_or_else(|| {
            if index < self.total {
                RowDefect::Forward(self.rows.len().into())
            } else {
                RowDefect::Absent(self.total.into())
            }
        })
    }

    fn environment(&self, row: Option<RowId>) -> Result<Env, DecodeFailure> {
        let Some(row) = row else {
            return Ok(Env::EMPTY);
        };
        match self.earlier(row) {
            Ok(Read::Env(env)) => Ok(env.clone()),
            Ok(Read::Built(_)) => Err(unresolved(
                row,
                RowKind::Environment,
                RowDefect::WrongKind(RowKind::Built),
            )),
            Err(defect) => Err(unresolved(row, RowKind::Environment, defect)),
        }
    }

    fn built(&self, row: RowId) -> Result<ValClos, DecodeFailure> {
        match self.earlier(row) {
            Ok(Read::Built(value)) => Ok(value.clone()),
            Ok(Read::Env(_)) => Err(unresolved(
                row,
                RowKind::Built,
                RowDefect::WrongKind(RowKind::Environment),
            )),
            Err(defect) => Err(unresolved(row, RowKind::Built, defect)),
        }
    }

    /// Σ's own value for `name`, so a residual's identity check holds.
    fn builtin(&self, name: &str) -> Result<(&ValClos, &Arc<BuiltinEntry>), DecodeFailure> {
        self.sig
            .native(name)
            .and_then(|value| match value.view() {
                View::Builtin(entry) => Some((value, entry)),
                _ => None,
            })
            .ok_or_else(|| DecodeFailure::new(DecodeCause::UnknownBuiltin(name.into())))
    }

    fn row(&self, row: &Row<RawData>) -> Result<Read, DecodeFailure> {
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

    fn binding(&self, binding: &wire::Binding<RawData>) -> Result<Binding, DecodeFailure> {
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

    fn value(&self, value: &Value<RawData>) -> Result<ValClos, DecodeFailure> {
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
                .map(|(value, _)| value.clone())
                .map_err(|error| error.payload("Builtin")),
            Value::Residual {
                builtin,
                env,
                supplied,
            } => {
                let (_, entry) = self
                    .builtin(builtin)
                    .map_err(|error| error.field("builtin").payload("Residual"))?;
                let env = self
                    .environment(Some(*env))
                    .map_err(|error| error.field("env").payload("Residual"))?;
                entry
                    .residual(&env, usize::try_from(*supplied).unwrap_or(usize::MAX))
                    .map_err(|defect| {
                        DecodeFailure::new(DecodeCause::InvalidResidual(InvalidResidual {
                            name: builtin.clone(),
                            arity: entry.decl.fixed_arity().into(),
                            supplied: u64::from(*supplied).into(),
                            defect,
                        }))
                        .payload("Residual")
                    })
            }
            Value::Handle(title) => Ok(ValClos::withheld(title.as_str())),
            Value::Failure(raw) => raw
                .validate()
                .map(ValClos::failure)
                .map_err(|error| error.payload("Failure")),
            Value::Built(row) => self.built(*row).map_err(|error| error.payload("Built")),
        }
    }

    /// A handler thunk, which `HandlerEntry::ral_per_name` trusts to be a block.
    fn arm(&self, value: &Value<RawData>) -> Result<ValClos, DecodeFailure> {
        let value = self.value(value)?;
        // `within` installs any thunk its checker admitted, builtins included.
        match value.shape() {
            ValueShape::Block | ValueShape::Function => Ok(value),
            _ => Err(DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
                expected: Expected::AnyOf(vec![ValueShape::Block, ValueShape::Function]),
                found: value.found(),
            }))),
        }
    }

    fn frame(&self, frame: &wire::Frame<RawData>) -> Result<HandlerFrame, DecodeFailure> {
        let mut names = HashSet::with_capacity(frame.entries.len());
        let mut entries = Vec::with_capacity(frame.entries.len());
        for (index, (name, value)) in frame.entries.iter().enumerate() {
            if !names.insert(name) {
                return Err(
                    DecodeFailure::new(DecodeCause::DuplicateField(name.clone()))
                        .index(0usize.into())
                        .index(index.into())
                        .field("entries"),
                );
            }
            let arm = self.arm(value).map_err(|error| {
                error
                    .index(1usize.into())
                    .index(index.into())
                    .field("entries")
            })?;
            entries.push(HandlerEntry::ral_per_name(name.clone(), arm));
        }
        let catch_all = frame
            .catch_all
            .as_ref()
            .map(|value| self.arm(value))
            .transpose()
            .map_err(|error| error.field("catch_all"))?;
        Ok(HandlerFrame {
            entries,
            catch_all,
            kind: frame.kind,
        })
    }
}
