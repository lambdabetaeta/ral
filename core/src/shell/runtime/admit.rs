//! Checked boundaries: a door admits a value against the type the checker
//! solved at its call.
//!
//! The checker builds the [`Site`] (`typecheck::site`); this is the runtime
//! walk over it.  A door that admits never converts: the value is what its
//! source says, and admission only says whether the program may have it.

use crate::fact::{
    AdmissionCause, AdmissionExport, AdmissionExtraField, AdmissionFailure, AdmissionShares,
    AdmissionUses, Expected, Found, InvalidCause, ValueShape,
};
use crate::shell::runtime::invalid;
use crate::shell::runtime::{Break, Map, Settled, Shell, Value};
use crate::source::Span;
use crate::sync::LockExt as _;
use crate::ty::{Closed, Fields, Fixed, Fixings, Id, Kind, Shape, Side, Site, Tail, UnitId};
use std::fmt;

// ── Fixings ─────────────────────────────────────────────────────────────

/// Which side of the number/text line `value` is on, if it is on one.
fn side(value: &Value) -> Option<Side> {
    match value {
        Value::Int(_) | Value::Float(_) => Some(Side::Number),
        Value::String(_) => Some(Side::Text),
        _ => None,
    }
}

impl Fixings {
    /// Fix `var` to `side` if it is not fixed yet; otherwise, what it was
    /// fixed to when that is the other side.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the read and the insert are one step under the lock"
    )]
    pub(crate) fn fix(
        &self,
        unit: UnitId,
        var: u32,
        side: Side,
        pointer: &str,
    ) -> Result<(), Fixed> {
        let mut table = self.0.lock_ignore_poison();
        match table.get(&(unit, var)) {
            Some(first) if first.side != side => Err(first.clone()),
            Some(_) => Ok(()),
            None => {
                table.insert(
                    (unit, var),
                    Fixed {
                        side,
                        pointer: pointer.to_owned(),
                    },
                );
                Ok(())
            }
        }
    }
}

// ── Mismatch ────────────────────────────────────────────────────────────

/// Why a value is refused, and where: an RFC 6901 pointer into it, and the use
/// in the script that imposed what it met.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    failure: AdmissionFailure,
    witness: Option<Span>,
}

impl Mismatch {
    /// The use that imposed what the value met, when one is known.
    pub fn witness(&self) -> Option<Span> {
        self.witness
    }

    /// Portable data, without the source witness.
    pub fn failure(&self) -> &AdmissionFailure {
        &self.failure
    }

    /// Advice is derived from the admitted shapes, never cached.
    pub fn hint(&self) -> Option<&'static str> {
        self.failure()
            .advice()
            .map(crate::fact::AdmissionAdvice::text)
    }

    /// The portable refusal and its report-only use witness.
    pub fn refusal(&self, door: &str) -> Break {
        invalid(door, InvalidCause::Admission(self.failure.clone()))
            .with_witness(self.witness)
            .into()
    }
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.failure().describe())
    }
}

/// A reference token as RFC 6901 writes it inside a pointer.
pub(crate) fn pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn pointer_of(path: &[Step]) -> String {
    path.iter()
        .map(|step| match step {
            Step::Key(key) => format!("/{}", pointer_token(key)),
            Step::At(index) => format!("/{index}"),
        })
        .collect()
}

// ── Site ────────────────────────────────────────────────────────────────

impl Site {
    /// Whether `value` may be what the program decided this site is.
    ///
    /// # Errors
    /// The first place the value and the type disagree.
    pub fn admit(&self, value: &Value, fixings: &Fixings) -> Result<(), Mismatch> {
        self.walk(self.root, value, fixings)
    }

    /// `value` let in at `door`, or `door`'s refusal of it.
    ///
    /// # Errors
    /// As [`Self::admit`], as [`Mismatch::refusal`] words it.
    pub fn admitted(&self, door: &str, value: Value, shell: &Shell) -> Settled<Value> {
        self.admit(&value, shell.fixings())
            .map_err(|mismatch| mismatch.refusal(door))?;
        Ok(value)
    }

    /// [`Self::admit`] for a module's export record, whose closures are also
    /// held to the schemes the module was checked at: each export's scheme is
    /// instantiated in one scratch unifier that covers the whole record, since
    /// field types share variables, and unified with the type the script uses
    /// it at.
    ///
    /// # Errors
    /// As [`Self::admit`], or an export whose scheme cannot be that type.
    pub fn admit_module(
        &self,
        record: &Value,
        schemes: &[(String, Closed)],
        fixings: &Fixings,
    ) -> Result<(), Mismatch> {
        self.admit(record, fixings)?;
        let Value::Map(fields) = record else {
            return Ok(());
        };
        let mut exports = self.exports();
        for (name, scheme) in schemes {
            let is_block = matches!(
                fields.get(name).as_deref(),
                Some(Value::Thunk(_) | Value::Native(_))
            );
            if !is_block {
                continue;
            }
            if let Some(Err((found, expected))) = exports.fits(name, scheme) {
                return Err(Mismatch {
                    failure: AdmissionFailure {
                        pointer: pointer_of(&[Step::Key(name.clone())]),
                        cause: AdmissionCause::Export(AdmissionExport { found, expected }),
                    },
                    witness: self.nodes[self.root].witness,
                });
            }
        }
        Ok(())
    }

    fn walk(&self, id: Id, value: &Value, fixings: &Fixings) -> Result<(), Mismatch> {
        Walk {
            site: self,
            fixings,
            path: Vec::new(),
        }
        .run(id, value, None)
    }
}

// ── Admission ───────────────────────────────────────────────────────────

enum Step {
    Key(String),
    At(usize),
}

struct Walk<'a> {
    site: &'a Site,
    fixings: &'a Fixings,
    path: Vec<Step>,
}

impl Walk<'_> {
    fn below<T>(&mut self, step: Step, f: impl FnOnce(&mut Self) -> T) -> T {
        self.path.push(step);
        let out = f(self);
        self.path.pop();
        out
    }

    fn mismatch(&self, witness: Option<Span>, why: AdmissionCause) -> Mismatch {
        Mismatch {
            failure: AdmissionFailure {
                pointer: pointer_of(&self.path),
                cause: why,
            },
            witness,
        }
    }

    fn uses(&self, value: &Value, expected: Expected, witness: Option<Span>) -> Mismatch {
        self.mismatch(
            witness,
            AdmissionCause::Uses(AdmissionUses {
                found: value.found(),
                expected,
            }),
        )
    }

    fn run(&mut self, id: Id, value: &Value, inherited: Option<Span>) -> Result<(), Mismatch> {
        let node = &self.site.nodes[id];
        let at = node.witness.or(inherited);
        match (&node.shape, value) {
            (Shape::Unit, Value::Unit)
            | (Shape::Bool, Value::Bool(_))
            | (Shape::Int, Value::Int(_))
            | (Shape::Float, Value::Float(_))
            | (Shape::String, Value::String(_))
            | (Shape::Bytes, Value::Bytes(_))
            | (Shape::Handle(..), Value::Handle(_))
            | (Shape::Thunk(_), Value::Thunk(_) | Value::Native(_)) => Ok(()),
            (Shape::List(elem), Value::List(items)) => {
                for (index, item) in items.iter().enumerate() {
                    self.below(Step::At(index), |w| w.run(*elem, &item, at))?;
                }
                Ok(())
            }
            (Shape::Map(elem), Value::Map(entries)) => {
                for (key, item) in entries {
                    self.below(Step::Key(key.to_string()), |w| w.run(*elem, &item, at))?;
                }
                Ok(())
            }
            (Shape::Record(fields), Value::Map(entries)) => self.record(fields, entries, at),
            (Shape::Variant(fields), Value::Variant { label, payload }) => {
                self.variant(fields, label, payload.as_deref(), at)
            }
            (Shape::Free { var, kind, weak }, _) => self.free(*var, *kind, *weak, value, at),
            (shape, _) => Err(self.uses(value, expected(shape), at)),
        }
    }

    fn record(&mut self, fields: &Fields, entries: &Map, at: Option<Span>) -> Result<(), Mismatch> {
        for (name, id) in &fields.labels {
            let Some(item) = entries.get(name) else {
                return Err(self.mismatch(at, AdmissionCause::NoField(name.clone())));
            };
            self.below(Step::Key(name.clone()), |w| w.run(*id, &item, at))?;
        }
        for (key, item) in entries {
            if fields.labels.iter().any(|(name, _)| name == key) {
                continue;
            }
            match fields.tail {
                Tail::Closed => {
                    return Err(self.mismatch(
                        at,
                        AdmissionCause::ExtraField(AdmissionExtraField {
                            field: key.to_string(),
                            fields: fields.labels.iter().map(|(name, _)| name.clone()).collect(),
                        }),
                    ));
                }
                Tail::Open { deep: true, .. } => {
                    self.below(Step::Key(key.to_string()), |w| w.data(&item, at))?;
                }
                Tail::Open { deep: false, .. } => {}
            }
        }
        Ok(())
    }

    fn variant(
        &mut self,
        fields: &Fields,
        label: &str,
        payload: Option<&Value>,
        at: Option<Span>,
    ) -> Result<(), Mismatch> {
        let unit = Value::Unit;
        let payload = payload.unwrap_or(&unit);
        match fields.labels.iter().find(|(name, _)| name == label) {
            Some((_, id)) => self.below(Step::Key("payload".into()), |w| w.run(*id, payload, at)),
            None => match fields.tail {
                Tail::Open { deep: false, .. } => Ok(()),
                Tail::Open { deep: true, .. } => {
                    self.below(Step::Key("payload".into()), |w| w.data(payload, at))
                }
                Tail::Closed => Err(self.mismatch(
                    at,
                    AdmissionCause::Uses(AdmissionUses {
                        found: Found::Variant(label.to_owned()),
                        expected: Expected::Tags(
                            fields.labels.iter().map(|(name, _)| name.clone()).collect(),
                        ),
                    }),
                )),
            },
        }
    }

    fn free(
        &mut self,
        var: u32,
        kind: Kind,
        weak: bool,
        value: &Value,
        at: Option<Span>,
    ) -> Result<(), Mismatch> {
        if !value.heads().iter().any(|&head| kind.admits(head)) {
            return Err(self.uses(value, Expected::Kind(kind.into()), at));
        }
        if kind.is_deep() {
            self.data(value, at)?;
        }
        if weak
            && kind == Kind::COMPARABLE
            && let Some(side) = side(value)
        {
            let pointer = pointer_of(&self.path);
            self.fixings
                .fix(self.site.unit, var, side, &pointer)
                .map_err(|first| {
                    self.mismatch(
                        at,
                        AdmissionCause::Shares(AdmissionShares {
                            other: first.pointer,
                            other_side: first.side.into(),
                            side: side.into(),
                        }),
                    )
                })?;
        }
        Ok(())
    }

    /// No block or handle anywhere in `value`.
    fn data(&mut self, value: &Value, at: Option<Span>) -> Result<(), Mismatch> {
        match value {
            Value::Thunk(_) | Value::Native(_) | Value::Handle(_) => {
                Err(self.uses(value, Expected::Kind(Kind::DATA.into()), at))
            }
            Value::List(items) => {
                for (index, item) in items.iter().enumerate() {
                    self.below(Step::At(index), |w| w.data(&item, at))?;
                }
                Ok(())
            }
            Value::Map(entries) => {
                for (key, item) in entries {
                    self.below(Step::Key(key.to_string()), |w| w.data(&item, at))?;
                }
                Ok(())
            }
            Value::Variant {
                payload: Some(payload),
                ..
            } => self.below(Step::Key("payload".into()), |w| w.data(payload, at)),
            Value::Unit
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::String(_)
            | Value::Bytes(_)
            | Value::Variant { payload: None, .. } => Ok(()),
        }
    }
}

/// The solved value expectation, without compiler or diagnostic state.
fn expected(shape: &Shape) -> Expected {
    match shape {
        Shape::Unit => Expected::Shape(ValueShape::Unit),
        Shape::Bool => Expected::Shape(ValueShape::Bool),
        Shape::Int => Expected::Shape(ValueShape::Int),
        Shape::Float => Expected::Shape(ValueShape::Float),
        Shape::String => Expected::Shape(ValueShape::String),
        Shape::Bytes => Expected::Shape(ValueShape::Bytes),
        Shape::List(_) => Expected::Shape(ValueShape::List),
        Shape::Map(_) => Expected::Shape(ValueShape::Map),
        Shape::Record(fields) => {
            Expected::Record(fields.labels.iter().map(|(name, _)| name.clone()).collect())
        }
        Shape::Variant(_) => Expected::Shape(ValueShape::Variant),
        Shape::Thunk(_) => Expected::AnyOf(vec![ValueShape::Block, ValueShape::Function]),
        Shape::Handle(..) => Expected::Shape(ValueShape::Handle),
        Shape::Free { kind, .. } => Expected::Kind((*kind).into()),
        Shape::Return(..) | Shape::Fun(..) | Shape::FreeComp { .. } => {
            unreachable!("a value position is never a computation")
        }
    }
}

#[cfg(test)]
mod tests;
