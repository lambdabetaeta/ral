//! Checked boundaries: a door admits a value against the type the checker
//! solved at its call.
//!
//! The checker builds the [`Site`] (`typecheck::site`); this is the runtime
//! walk over it.  A door that admits never converts: the value is what its
//! source says, and admission only says whether the program may have it.

use crate::fact::{
    AdmissionCause, AdmissionExtraField, AdmissionFailure, AdmissionShares, AdmissionUses,
    Expected, Found, InvalidCause, ValueShape,
};
use crate::shell::runtime::invalid;
use crate::shell::runtime::{Break, MapRef, Settled, Shell, ValClos, View};
use crate::source::Span;
use crate::sync::LockExt as _;
use crate::ty::{Fields, Fixed, Fixings, Head, Id, Kind, Shape, Side, Site, Tail, UnitId};
use std::fmt;

// ── Fixings ─────────────────────────────────────────────────────────────

/// Which side of the number/text line `value` is on, if it is on one.
fn side(value: &ValClos) -> Option<Side> {
    match value.view() {
        View::Int(_) | View::Float(_) => Some(Side::Number),
        View::String(_) => Some(Side::Text),
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
    pub fn admit(&self, value: &ValClos, fixings: &Fixings) -> Result<(), Mismatch> {
        self.walk(self.root, value, fixings)
    }

    /// `value` let in at `door`, or `door`'s refusal of it.
    ///
    /// # Errors
    /// As [`Self::admit`], as [`Mismatch::refusal`] words it.
    pub fn admitted(&self, door: &str, value: ValClos, shell: &Shell) -> Settled<ValClos> {
        self.admit(&value, shell.fixings())
            .map_err(|mismatch| mismatch.refusal(door))?;
        Ok(value)
    }

    fn walk(&self, id: Id, value: &ValClos, fixings: &Fixings) -> Result<(), Mismatch> {
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

    fn uses(&self, value: &ValClos, expected: Expected, witness: Option<Span>) -> Mismatch {
        self.uses_found(value.found(), expected, witness)
    }

    fn uses_found(&self, found: Found, expected: Expected, witness: Option<Span>) -> Mismatch {
        self.mismatch(
            witness,
            AdmissionCause::Uses(AdmissionUses { found, expected }),
        )
    }

    fn run(
        &mut self,
        mut id: Id,
        value: &ValClos,
        inherited: Option<Span>,
    ) -> Result<(), Mismatch> {
        let (labels, terminal) = value.peel_variants();
        let saved = self.path.len();
        let result = (|| {
            let mut at = inherited;
            for (index, label) in labels.iter().enumerate() {
                let node = &self.site.nodes[id];
                at = node.witness.or(at);
                match &node.shape {
                    Shape::Variant(fields) => {
                        if let Some((_, next)) = fields
                            .labels
                            .iter()
                            .find(|(name, _)| name == label.as_ref())
                        {
                            self.path.push(Step::Key("payload".into()));
                            id = *next;
                            continue;
                        }
                        match fields.tail {
                            Tail::Closed => {
                                return Err(self.uses_found(
                                    Found::Variant(label.to_string()),
                                    Expected::Tags(
                                        fields
                                            .labels
                                            .iter()
                                            .map(|(name, _)| name.clone())
                                            .collect(),
                                    ),
                                    at,
                                ));
                            }
                            Tail::Open { deep: false, .. } => return Ok(()),
                            Tail::Open { deep: true, .. } => {}
                        }
                    }
                    Shape::Free { kind, .. } => {
                        if !kind.admits(Head::Variant) {
                            return Err(self.uses_found(
                                Found::Variant(label.to_string()),
                                Expected::Kind((*kind).into()),
                                at,
                            ));
                        }
                        if !kind.is_deep() {
                            return Ok(());
                        }
                    }
                    shape => {
                        return Err(self.uses_found(
                            Found::Variant(label.to_string()),
                            expected(shape),
                            at,
                        ));
                    }
                }
                self.path
                    .extend((index..labels.len()).map(|_| Step::Key("payload".into())));
                return self.data(&terminal, at);
            }
            let node = &self.site.nodes[id];
            let at = node.witness.or(at);
            match (&node.shape, terminal.view()) {
                (Shape::Unit, View::Unit)
                | (Shape::Bool, View::Bool(_))
                | (Shape::Int, View::Int(_))
                | (Shape::Float, View::Float(_))
                | (Shape::String, View::String(_))
                | (Shape::Bytes, View::Bytes(_))
                | (Shape::Handle(..), View::Handle(_))
                | (Shape::Thunk(_), View::Thunk { .. } | View::Builtin(_)) => Ok(()),
                (Shape::List(elem), View::List(items)) => {
                    for (index, item) in items.iter().enumerate() {
                        self.below(Step::At(index), |w| w.run(*elem, &item, at))?;
                    }
                    Ok(())
                }
                (Shape::Map(elem), View::Map(entries)) => {
                    for (key, item) in entries {
                        self.below(Step::Key(key.to_string()), |w| w.run(*elem, &item, at))?;
                    }
                    Ok(())
                }
                (Shape::Record(fields), View::Map(entries)) => self.record(fields, entries, at),
                (Shape::Free { var, kind, weak }, _) => {
                    self.free(*var, *kind, *weak, &terminal, at)
                }
                (shape, _) => Err(self.uses(&terminal, expected(shape), at)),
            }
        })();
        self.path.truncate(saved);
        result
    }

    fn record(
        &mut self,
        fields: &Fields,
        entries: MapRef<'_>,
        at: Option<Span>,
    ) -> Result<(), Mismatch> {
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

    fn free(
        &mut self,
        var: u32,
        kind: Kind,
        weak: bool,
        value: &ValClos,
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
    fn data(&mut self, value: &ValClos, at: Option<Span>) -> Result<(), Mismatch> {
        let (labels, terminal) = value.peel_variants();
        let saved = self.path.len();
        self.path
            .extend(labels.iter().map(|_| Step::Key("payload".into())));
        let result = (|| match terminal.view() {
            View::Thunk { .. } | View::Builtin(_) | View::Handle(_) => {
                Err(self.uses(&terminal, Expected::Kind(Kind::DATA.into()), at))
            }
            View::List(items) => {
                for (index, item) in items.iter().enumerate() {
                    self.below(Step::At(index), |w| w.data(&item, at))?;
                }
                Ok(())
            }
            View::Map(entries) => {
                for (key, item) in entries {
                    self.below(Step::Key(key.to_string()), |w| w.data(&item, at))?;
                }
                Ok(())
            }
            View::Variant { .. } => unreachable!("peeling tags leaves a nonvariant terminal"),
            View::Unit
            | View::Bool(_)
            | View::Int(_)
            | View::Float(_)
            | View::String(_)
            | View::Bytes(_) => Ok(()),
        })();
        self.path.truncate(saved);
        result
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
