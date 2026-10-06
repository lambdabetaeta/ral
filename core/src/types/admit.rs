//! Checked boundaries: a door admits a value against the type the checker
//! solved at its call.
//!
//! The checker builds the [`Site`] (`typecheck::site`); this is the runtime
//! walk over it.  A door that admits never converts: the value is what its
//! source says, and admission only says whether the program may have it.

use crate::source::Span;
use crate::sync::LockExt as _;
use crate::ty::{Fields, Fixed, Fixings, Id, Kind, Scheme, Shape, Side, Site, Tail};
use crate::types::{Break, Error, Map, Shell, Value};
use std::fmt::{self, Write as _};
use std::sync::Arc;

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
    fn fix(&self, var: u32, side: Side, pointer: &str) -> Result<(), Fixed> {
        let mut table = self.table.lock_ignore_poison();
        match table.get(&var) {
            Some(first) if first.side != side => Err(first.clone()),
            Some(_) => Ok(()),
            None => {
                table.insert(
                    var,
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
#[derive(Debug, Clone, PartialEq)]
pub struct Mismatch {
    pointer: String,
    witness: Option<Span>,
    why: Why,
}

#[derive(Debug, Clone, PartialEq)]
enum Why {
    Uses {
        found: String,
        expected: String,
        hint: Option<&'static str>,
    },
    NoField(String),
    ExtraField {
        field: String,
        fields: Vec<String>,
    },
    Shares {
        other: String,
        other_side: Side,
        side: Side,
    },
    Export {
        found: String,
        expected: String,
    },
}

const JSON_NUMBERS: &str = "JSON has one number type, and ral reads a whole number as an Int and \
a number with a fraction as a Float; `float` or `int` at the use accepts the other";
const NO_JSON_VARIANTS: &str = "a decoder produces no variants: a tag is matched only on a value \
the program built, or one a host answered";

impl Mismatch {
    /// The use that imposed what the value met, when one is known.
    pub fn witness(&self) -> Option<Span> {
        self.witness
    }

    /// What to do about it, where there is one thing.
    pub fn hint(&self) -> Option<&'static str> {
        match &self.why {
            Why::Uses { hint, .. } => *hint,
            _ => None,
        }
    }

    /// The error `door` raises: the sentence, the line of the use that imposed
    /// what the value met, and a caret on that use.
    pub fn refusal(&self, door: &str, shell: &Shell) -> Break {
        let mut message = format!("{door}: {self}");
        if let Some(site) = self.witness.and_then(|w| shell.session.sources.site(w)) {
            let _ = write!(message, ", line {}", site.line);
        }
        let mut error = Error::new(message).with_witness(self.witness);
        if let Some(hint) = self.hint() {
            error = error.with_hint(hint);
        }
        Break::Error(error)
    }

    fn place(&self) -> String {
        match self.pointer.as_str() {
            "" => "the value".into(),
            p => format!("the value at `{p}`"),
        }
    }
}

fn shown(pointer: &str) -> String {
    match pointer {
        "" => "the whole value".into(),
        p => format!("`{p}`"),
    }
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.why {
            Why::Uses {
                found, expected, ..
            } => write!(
                f,
                "{} is {found}, but this script uses it as {expected}",
                self.place()
            ),
            Why::NoField(field) => write!(
                f,
                "{} has no field `{field}`, which this script reads",
                self.place()
            ),
            Why::ExtraField { field, fields } => write!(
                f,
                "{} has a field `{field}`, but this script uses it as a record with exactly the \
                 fields {}",
                self.place(),
                fields
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Why::Shares {
                other,
                other_side,
                side,
            } => write!(
                f,
                "{} is {other_side} and {} is {side}, and this script compares them",
                shown(other),
                shown(&self.pointer)
            ),
            Why::Export { found, expected } => write!(
                f,
                "{} is a block of type {found}, but this script uses it as {expected}",
                self.place()
            ),
        }
    }
}

/// How a value looks to a reader of the sentence.
fn found(value: &Value) -> String {
    match value {
        Value::Unit => "null".into(),
        Value::Bool(b) => format!("a Bool (`{b}`)"),
        Value::Int(n) => format!("a whole number (`{n}`)"),
        Value::Float(x) => format!("a number with a fraction (`{x}`)"),
        Value::String(s) => format!("text (`'{}'`)", clip(s)),
        Value::Bytes(_) => "bytes".into(),
        Value::List(_) => "a list".into(),
        Value::Map(_) => "a map".into(),
        Value::Variant { label, .. } => format!("a variant (tag `{label}`)"),
        Value::Thunk(_) | Value::Native { .. } => "a block".into(),
        Value::Handle(_) => "a handle".into(),
    }
}

fn clip(text: &str) -> String {
    const WIDTH: usize = 24;
    match text.char_indices().nth(WIDTH) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// A kind, in the words of the use that imposed it.
fn kind_phrase(kind: Kind) -> String {
    match kind {
        Kind::NUMBER => "a number".into(),
        Kind::COMPARABLE => "a number or text".into(),
        Kind::SCALAR => "a number, text or a Bool".into(),
        Kind::SIZED => "text, bytes, a list or a map".into(),
        Kind::DATA => "data, with no block or handle in it".into(),
        Kind::LABELLED => "a record or a map".into(),
        Kind::INDEXED => "a list or a map".into(),
        Kind::KEY => "a whole number or text".into(),
        other => format!("a value of kind `{other}`"),
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
    pub fn admit(&self, value: &Value) -> Result<(), Mismatch> {
        self.walk(self.root, value)
    }

    /// [`Self::admit`] for the value a `Handle α` site's worker settles with.
    ///
    /// # Errors
    /// As [`Self::admit`].
    pub fn admit_handled(&self, value: &Value) -> Result<(), Mismatch> {
        match self.nodes[self.root].shape {
            Shape::Handle(inner) => self.walk(inner, value),
            _ => Ok(()),
        }
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
        schemes: &[(String, Arc<Scheme>)],
    ) -> Result<(), Mismatch> {
        self.admit(record)?;
        let Value::Map(fields) = record else {
            return Ok(());
        };
        let mut exports = self.exports();
        for (name, scheme) in schemes {
            let is_block = matches!(
                fields.get(name).as_deref(),
                Some(Value::Thunk(_) | Value::Native { .. })
            );
            if !is_block {
                continue;
            }
            if let Some(Err((found, expected))) = exports.fits(name, scheme) {
                return Err(Mismatch {
                    pointer: pointer_of(&[Step::Key(name.clone())]),
                    witness: self.nodes[self.root].witness,
                    why: Why::Export { found, expected },
                });
            }
        }
        Ok(())
    }

    fn walk(&self, id: Id, value: &Value) -> Result<(), Mismatch> {
        Walk {
            site: self,
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
    path: Vec<Step>,
}

impl Walk<'_> {
    fn below<T>(&mut self, step: Step, f: impl FnOnce(&mut Self) -> T) -> T {
        self.path.push(step);
        let out = f(self);
        self.path.pop();
        out
    }

    fn mismatch(&self, witness: Option<Span>, why: Why) -> Mismatch {
        Mismatch {
            pointer: pointer_of(&self.path),
            witness,
            why,
        }
    }

    fn uses(
        &self,
        value: &Value,
        expected: String,
        witness: Option<Span>,
        hint: Option<&'static str>,
    ) -> Mismatch {
        self.mismatch(
            witness,
            Why::Uses {
                found: found(value),
                expected,
                hint,
            },
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
            | (Shape::Handle(_), Value::Handle(_))
            | (Shape::Thunk(_), Value::Thunk(_) | Value::Native { .. }) => Ok(()),
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
            (Shape::Free { var, kind }, _) => self.free(*var, *kind, value, at),
            (shape, _) => Err(self.uses(value, expected(shape), at, refusal_hint(shape, value))),
        }
    }

    fn record(&mut self, fields: &Fields, entries: &Map, at: Option<Span>) -> Result<(), Mismatch> {
        for (name, id) in &fields.labels {
            let Some(item) = entries.get(name) else {
                return Err(self.mismatch(at, Why::NoField(name.clone())));
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
                        Why::ExtraField {
                            field: key.to_string(),
                            fields: fields.labels.iter().map(|(name, _)| name.clone()).collect(),
                        },
                    ));
                }
                Tail::Open { deep: true } => {
                    self.below(Step::Key(key.to_string()), |w| w.data(&item, at))?;
                }
                Tail::Open { deep: false } => {}
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
                Tail::Open { deep: false } => Ok(()),
                Tail::Open { deep: true } => {
                    self.below(Step::Key("payload".into()), |w| w.data(payload, at))
                }
                Tail::Closed => Err(self.mismatch(
                    at,
                    Why::Uses {
                        found: format!("a variant (tag `{label}`)"),
                        expected: format!(
                            "a variant with one of the tags {}",
                            fields
                                .labels
                                .iter()
                                .map(|(name, _)| format!("`{name}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        hint: None,
                    },
                )),
            },
        }
    }

    fn free(
        &mut self,
        var: u32,
        kind: Kind,
        value: &Value,
        at: Option<Span>,
    ) -> Result<(), Mismatch> {
        if !value.heads().iter().any(|&head| kind.admits(head)) {
            return Err(self.uses(value, kind_phrase(kind), at, None));
        }
        if kind.is_deep() {
            self.data(value, at)?;
        }
        if kind == Kind::COMPARABLE
            && let Some(side) = side(value)
        {
            let pointer = pointer_of(&self.path);
            self.site
                .fixings
                .fix(var, side, &pointer)
                .map_err(|first| {
                    self.mismatch(
                        at,
                        Why::Shares {
                            other: first.pointer,
                            other_side: first.side,
                            side,
                        },
                    )
                })?;
        }
        Ok(())
    }

    /// No block or handle anywhere in `value`.
    fn data(&mut self, value: &Value, at: Option<Span>) -> Result<(), Mismatch> {
        match value {
            Value::Thunk(_) | Value::Native { .. } | Value::Handle(_) => {
                Err(self.uses(value, kind_phrase(Kind::DATA), at, None))
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

/// The use a position stands for, in words.
fn expected(shape: &Shape) -> String {
    match shape {
        Shape::Unit => "`()`".into(),
        Shape::Bool => "a Bool".into(),
        Shape::Int => "an Int".into(),
        Shape::Float => "a Float".into(),
        Shape::String => "text".into(),
        Shape::Bytes => "bytes".into(),
        Shape::List(_) => "a list".into(),
        Shape::Map(_) => "a map".into(),
        Shape::Record(_) => "a record".into(),
        Shape::Variant(_) => "a variant".into(),
        Shape::Thunk(_) => "a block".into(),
        Shape::Handle(_) => "a handle".into(),
        Shape::Free { kind, .. } => kind_phrase(*kind),
        Shape::Return(..) | Shape::Fun(..) | Shape::FreeComp => {
            unreachable!("a value position is never a computation")
        }
    }
}

fn refusal_hint(shape: &Shape, value: &Value) -> Option<&'static str> {
    match (shape, value) {
        (Shape::Float, Value::Int(_)) | (Shape::Int, Value::Float(_)) => Some(JSON_NUMBERS),
        (Shape::Variant(_), _) => Some(NO_JSON_VARIANTS),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
