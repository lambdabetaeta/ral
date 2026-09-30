//! Checked boundaries: a door admits a value against the type the checker
//! solved at its call.
//!
//! A [`Site`] is that type frozen, as a graph of nodes rather than a `Ty`,
//! because the walk wants two things a `Ty` loses once the unifier has
//! resolved it: the span that imposed each structure, and the identity of each
//! free variable, which is what lets two decodes the program treats as one
//! `comparable` type agree ([`Fixings`]).  A door that admits never converts:
//! the value is what its source says, and admission only says whether the
//! program may have it.

use crate::source::Span;
use crate::sync::LockExt as _;
use crate::typecheck::{CompTy, Grade, Kind, Label, Row, Scheme, Ty, Unifier, fmt_scheme, fmt_ty};
use crate::types::{Break, Error, Map, Shell, Value};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

type Id = usize;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Node {
    shape: Shape,
    /// The use that imposed this structure, or this kind.
    witness: Option<Span>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum Shape {
    Unit,
    Bytes,
    Bool,
    Int,
    Float,
    String,
    List(Id),
    Map(Id),
    Handle(Id),
    Record(Fields),
    Variant(Fields),
    Thunk(Id),
    Free { var: u32, kind: Kind },
    Return(GradeShape, Id),
    Fun(Id, Id),
    FreeComp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum GradeShape {
    Value,
    Output,
    Free,
}

impl GradeShape {
    fn of(grade: Grade) -> Self {
        match grade {
            Grade::Value => Self::Value,
            Grade::Output => Self::Output,
            Grade::Var(_) => Self::Free,
        }
    }

    fn instantiate(self, u: &mut Unifier) -> Grade {
        match self {
            Self::Value => Grade::Value,
            Self::Output => Grade::Output,
            Self::Free => u.fresh_grade(),
        }
    }
}

/// A row's labels in first-appearance order, and how it ends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Fields {
    labels: Vec<(String, Id)>,
    tail: Tail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Tail {
    Closed,
    Open { deep: bool },
}

// ── Fixings ─────────────────────────────────────────────────────────────

/// What a `comparable` variable was fixed to: the one run-time operation that
/// fails on two admissible values of different types is ordering a number
/// against text, so that is the one distinction a door must keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Side {
    Number,
    Text,
}

impl Side {
    fn of(value: &Value) -> Option<Self> {
        match value {
            Value::Int(_) | Value::Float(_) => Some(Self::Number),
            Value::String(_) => Some(Self::Text),
            _ => None,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Number => "a number",
            Self::Text => "text",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Fixed {
    side: Side,
    pointer: String,
}

/// What the unit's `comparable` variables were fixed to.
///
/// One per unit, shared by every [`Site`] the unit builds, so two decodes the
/// program treats as one `comparable` type agree for as long as the compiled
/// program lives.  A `SerialThunk` ships a copy to an engine child; what the
/// child fixes stays there.
#[derive(Debug)]
pub struct Fixings {
    id: u64,
    table: Mutex<HashMap<u32, Fixed>>,
}

impl Fixings {
    pub(crate) fn new() -> Arc<Self> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let id =
            u64::from(std::process::id()) << 32 | u64::from(NEXT.fetch_add(1, Ordering::Relaxed));
        Arc::new(Self {
            id,
            table: Mutex::default(),
        })
    }

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

impl PartialEq for Fixings {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

/// Wire form of a unit's fixings: the table, under its id, so every site of
/// one unit that reaches a process meets the one `Arc` there.
mod shared {
    use super::{Fixed, Fixings, HashMap, Mutex, OnceLock, Weak};
    use crate::sync::LockExt as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::sync::Arc;

    type Live = Mutex<HashMap<u64, Weak<Fixings>>>;

    fn live() -> &'static Live {
        static LIVE: OnceLock<Live> = OnceLock::new();
        LIVE.get_or_init(Live::default)
    }

    pub(super) fn serialize<S: Serializer>(
        fixings: &Arc<Fixings>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let table = fixings.table.lock_ignore_poison();
        (fixings.id, &*table).serialize(serializer)
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "lookup and registration are one step under the lock"
    )]
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Arc<Fixings>, D::Error> {
        let (id, table) = <(u64, HashMap<u32, Fixed>)>::deserialize(deserializer)?;
        let mut live = live().lock_ignore_poison();
        live.retain(|_, weak| weak.strong_count() > 0);
        if let Some(known) = live.get(&id).and_then(Weak::upgrade) {
            return Ok(known);
        }
        let fixings = Arc::new(Fixings {
            id,
            table: Mutex::new(table),
        });
        live.insert(id, Arc::downgrade(&fixings));
        Ok(fixings)
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
        if let Some(site) = shell.site_of(self.witness) {
            let _ = write!(message, " — line {}", site.line);
        }
        let mut error = Error::new(message, 1).with_witness(self.witness);
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

/// The type a boundary's result was solved at, and what a door needs to admit
/// a value against it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Site {
    nodes: Vec<Node>,
    root: Id,
    #[serde(with = "shared")]
    fixings: Arc<Fixings>,
}

impl Site {
    /// Freeze `ty` as `u` has solved it.
    pub(crate) fn snapshot(u: &Unifier, ty: &Ty, fixings: Arc<Fixings>) -> Self {
        let mut build = Build {
            u,
            nodes: Vec::new(),
            tys: HashMap::new(),
            comps: HashMap::new(),
        };
        let root = build.ty(ty);
        Self {
            nodes: build.nodes,
            root,
            fixings,
        }
    }

    /// Whether both sites answer to one unit's [`Fixings`].
    #[cfg(test)]
    pub(crate) fn shares_fixings_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.fixings, &other.fixings)
    }

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
        let mut u = Unifier::new();
        let expected = self.instantiate(&mut u);
        let Ty::Record(row) = u.resolve_ty(&expected) else {
            return Ok(());
        };
        for (name, scheme) in schemes {
            let is_block = matches!(
                fields.get(name).as_deref(),
                Some(Value::Thunk(_) | Value::Native { .. })
            );
            let Some(want) = is_block.then(|| field_ty(&u, &row, name)).flatten() else {
                continue;
            };
            let scheme = crate::typecheck::reseed_weak(&mut u, Arc::clone(scheme));
            let have = crate::typecheck::instantiate(&mut u, &scheme);
            let wanted = fmt_ty(&u.apply_ty(&want));
            if u.unify_ty(&have, &want).is_err() {
                return Err(Mismatch {
                    pointer: pointer_of(&[Step::Key(name.clone())]),
                    witness: self.nodes[self.root].witness,
                    why: Why::Export {
                        found: fmt_scheme(&scheme),
                        expected: wanted,
                    },
                });
            }
        }
        Ok(())
    }

    /// The root as a type over `u`'s fresh variables: one variable per node,
    /// so sharing and cycles survive.
    fn instantiate(&self, u: &mut Unifier) -> Ty {
        let (tys, comps): (HashMap<Id, u32>, HashMap<Id, u32>) = {
            let mut tys = HashMap::new();
            let mut comps = HashMap::new();
            for (id, node) in self.nodes.iter().enumerate() {
                match node.shape {
                    Shape::Return(..) | Shape::Fun(..) | Shape::FreeComp => {
                        comps.insert(id, u.fresh_comp_root());
                    }
                    _ => {
                        tys.insert(id, u.fresh_ty_root());
                    }
                }
            }
            (tys, comps)
        };
        let ty = |id: Id| Ty::Var(crate::typecheck::TyVar(tys[&id]));
        let comp = |id: Id| CompTy::Var(crate::typecheck::CompTyVar(comps[&id]));
        for (id, node) in self.nodes.iter().enumerate() {
            match &node.shape {
                Shape::Free { kind, .. } => {
                    let free = u.fresh_kinded_var(*kind);
                    u.bind_ty_root(tys[&id], Ty::Var(free));
                }
                Shape::FreeComp => {
                    let free = u.fresh_comp_ty();
                    u.bind_comp_root(comps[&id], free);
                }
                Shape::Return(grade, inner) => {
                    let grade = grade.instantiate(u);
                    u.bind_comp_root(comps[&id], CompTy::Return(grade, Box::new(ty(*inner))));
                }
                Shape::Fun(arg, body) => {
                    u.bind_comp_root(
                        comps[&id],
                        CompTy::Fun(Box::new(ty(*arg)), Box::new(comp(*body))),
                    );
                }
                structure => {
                    let built = match structure {
                        Shape::Unit => Ty::Unit,
                        Shape::Bytes => Ty::Bytes,
                        Shape::Bool => Ty::Bool,
                        Shape::Int => Ty::Int,
                        Shape::Float => Ty::Float,
                        Shape::String => Ty::String,
                        Shape::List(e) => Ty::List(Box::new(ty(*e))),
                        Shape::Map(e) => Ty::Map(Box::new(ty(*e))),
                        Shape::Handle(e) => Ty::Handle(Box::new(ty(*e))),
                        Shape::Thunk(c) => Ty::Thunk(Box::new(comp(*c))),
                        Shape::Record(fields) => Ty::Record(row_of(u, fields, &ty, Label::Field)),
                        Shape::Variant(fields) => Ty::Variant(row_of(u, fields, &ty, Label::Case)),
                        Shape::Free { .. }
                        | Shape::Return(..)
                        | Shape::Fun(..)
                        | Shape::FreeComp => {
                            unreachable!("bound above")
                        }
                    };
                    u.bind_ty_root(tys[&id], built);
                }
            }
        }
        ty(self.root)
    }

    fn walk(&self, id: Id, value: &Value) -> Result<(), Mismatch> {
        Walk {
            site: self,
            path: Vec::new(),
        }
        .run(id, value, None)
    }
}

/// `row`'s field `name`, if the row names it.
fn field_ty(u: &Unifier, row: &Row, name: &str) -> Option<Ty> {
    let mut cur = u.resolve_row(row);
    loop {
        match cur {
            Row::Extend(label, ty, rest) => {
                if label == Label::Field(name.to_owned()) {
                    return Some(*ty);
                }
                cur = u.resolve_row(&rest);
            }
            Row::Var(_) | Row::Empty => return None,
        }
    }
}

fn row_of(
    u: &mut Unifier,
    fields: &Fields,
    ty: &impl Fn(Id) -> Ty,
    label: fn(String) -> Label,
) -> Row {
    let tail = match fields.tail {
        Tail::Closed => Row::Empty,
        Tail::Open { deep } => Row::Var(u.fresh_deep_row_var(deep)),
    };
    fields.labels.iter().rev().fold(tail, |rest, (name, id)| {
        Row::Extend(label(name.clone()), Box::new(ty(*id)), Box::new(rest))
    })
}

// ── Snapshot ────────────────────────────────────────────────────────────

struct Build<'a> {
    u: &'a Unifier,
    nodes: Vec<Node>,
    tys: HashMap<u32, Id>,
    comps: HashMap<u32, Id>,
}

impl Build<'_> {
    fn alloc(&mut self) -> Id {
        self.nodes.push(Node {
            shape: Shape::Unit,
            witness: None,
        });
        self.nodes.len() - 1
    }

    /// One node per variable root, allocated before its structure is walked,
    /// so a cycle closes on it.
    fn ty(&mut self, ty: &Ty) -> Id {
        let u = self.u;
        let root = match ty {
            Ty::Var(v) => Some(u.ty_root(v.0)),
            _ => None,
        };
        if let Some(&id) = root.and_then(|r| self.tys.get(&r)) {
            return id;
        }
        let id = self.alloc();
        if let Some(r) = root {
            self.tys.insert(r, id);
        }
        let (shape, witness) = match &*u.head_ty(ty) {
            Ty::Var(v) => {
                let kinded = u.kinded(*v);
                (
                    Shape::Free {
                        var: v.0,
                        kind: kinded.kind,
                    },
                    kinded.witness,
                )
            }
            Ty::Unit => (Shape::Unit, None),
            Ty::Bytes => (Shape::Bytes, None),
            Ty::Bool => (Shape::Bool, None),
            Ty::Int => (Shape::Int, None),
            Ty::Float => (Shape::Float, None),
            Ty::String => (Shape::String, None),
            Ty::List(a) => (Shape::List(self.ty(a)), None),
            Ty::Map(a) => (Shape::Map(self.ty(a)), None),
            Ty::Handle(a) => (Shape::Handle(self.ty(a)), None),
            Ty::Record(r) => (Shape::Record(self.fields(r)), None),
            Ty::Variant(r) => (Shape::Variant(self.fields(r)), None),
            Ty::Thunk(c) => (Shape::Thunk(self.comp(c)), None),
        };
        let witness = witness.or_else(|| root.and_then(|r| u.bound_witness(r)));
        self.nodes[id] = Node { shape, witness };
        id
    }

    fn comp(&mut self, cty: &CompTy) -> Id {
        let u = self.u;
        let root = match cty {
            CompTy::Var(v) => Some(u.comp_root(v.0)),
            _ => None,
        };
        if let Some(&id) = root.and_then(|r| self.comps.get(&r)) {
            return id;
        }
        let id = self.alloc();
        if let Some(r) = root {
            self.comps.insert(r, id);
        }
        let shape = match &*u.head_comp_ty(cty) {
            CompTy::Var(_) => Shape::FreeComp,
            CompTy::Return(grade, a) => {
                Shape::Return(GradeShape::of(u.resolve_grade(*grade)), self.ty(a))
            }
            CompTy::Fun(a, b) => Shape::Fun(self.ty(a), self.comp(b)),
        };
        self.nodes[id].shape = shape;
        id
    }

    fn fields(&mut self, row: &Row) -> Fields {
        let u = self.u;
        let mut labels: Vec<(String, Id)> = Vec::new();
        let mut cur = u.resolve_row(row);
        let tail = loop {
            match cur {
                Row::Extend(label, ty, rest) => {
                    if !labels.iter().any(|(name, _)| name == label.name()) {
                        let id = self.ty(&ty);
                        labels.push((label.name().to_owned(), id));
                    }
                    cur = u.resolve_row(&rest);
                }
                Row::Var(v) => {
                    break Tail::Open {
                        deep: u.is_deep_row(v),
                    };
                }
                Row::Empty => break Tail::Closed,
            }
        };
        Fields { labels, tail }
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
        if !kind.admits_value(value) {
            return Err(self.uses(value, kind_phrase(kind), at, None));
        }
        if kind.is_deep() {
            self.data(value, at)?;
        }
        if kind == Kind::COMPARABLE
            && let Some(side) = Side::of(value)
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
mod tests {
    use super::*;
    use crate::source::FileId;
    use crate::typecheck::builtins::{closed_record, closed_variant, open_record};

    fn site(u: &Unifier, ty: &Ty) -> Site {
        Site::snapshot(u, ty, Fixings::new())
    }

    fn ground(ty: &Ty) -> Site {
        site(&Unifier::new(), ty)
    }

    fn map(entries: &[(&str, Value)]) -> Value {
        Value::map(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect::<Vec<_>>(),
        )
    }

    fn refusal(site: &Site, value: &Value) -> String {
        site.admit(value)
            .expect_err("the value must be refused")
            .to_string()
    }

    #[test]
    fn a_value_of_the_solved_type_is_admitted() {
        let ty = closed_record(&[("a", Ty::Int), ("b", Ty::List(Box::new(Ty::String)))]);
        let value = map(&[
            ("a", Value::Int(1)),
            ("b", Value::list(vec![Value::string("x")])),
        ]);
        assert!(ground(&ty).admit(&value).is_ok());
    }

    #[test]
    fn a_deep_mismatch_names_its_pointer() {
        let ty = Ty::List(Box::new(closed_record(&[("size", Ty::Int)])));
        let value = Value::list(vec![
            map(&[("size", Value::Int(1))]),
            map(&[("size", Value::string("12kB"))]),
        ]);
        assert_eq!(
            refusal(&ground(&ty), &value),
            "the value at `/1/size` is text (`'12kB'`), but this script uses it as an Int"
        );
    }

    #[test]
    fn a_key_with_a_slash_or_a_tilde_is_escaped_in_its_pointer() {
        let ty = Ty::Map(Box::new(Ty::Int));
        let value = map(&[("a/b~c", Value::string("x"))]);
        assert!(refusal(&ground(&ty), &value).contains("`/a~1b~0c`"));
    }

    #[test]
    fn an_open_row_admits_extra_fields_and_a_closed_one_names_them() {
        let mut u = Unifier::new();
        let tail = u.fresh_row_var();
        let value = map(&[("a", Value::Int(1)), ("extra", Value::Bool(true))]);
        let open = open_record(&[("a", Ty::Int)], tail);
        assert!(site(&u, &open).admit(&value).is_ok());
        let closed = closed_record(&[("a", Ty::Int)]);
        assert!(refusal(&ground(&closed), &value).contains("has a field `extra`"));
    }

    #[test]
    fn a_missing_field_is_named() {
        let ty = closed_record(&[("verdict", Ty::String)]);
        let message = refusal(&ground(&ty), &map(&[]));
        assert_eq!(
            message,
            "the value has no field `verdict`, which this script reads"
        );
    }

    #[test]
    fn a_kinded_variable_admits_only_its_heads() {
        let mut u = Unifier::new();
        let v = u.fresh_kinded(Kind::NUMBER);
        let s = site(&u, &Ty::List(Box::new(v)));
        assert!(
            s.admit(&Value::list(vec![Value::Int(1), Value::Float(2.5)]))
                .is_ok()
        );
        assert_eq!(
            refusal(&s, &Value::list(vec![Value::string("x")])),
            "the value at `/0` is text (`'x'`), but this script uses it as a number"
        );
    }

    #[test]
    fn a_free_variable_admits_anything_a_parametric_use_could_hold() {
        let mut u = Unifier::new();
        let v = u.fresh_ty();
        let s = site(&u, &Ty::List(Box::new(v)));
        assert!(
            s.admit(&Value::list(vec![Value::Int(1), Value::string("a")]))
                .is_ok()
        );
    }

    #[test]
    fn a_comparable_variable_is_fixed_to_numbers_or_to_text() {
        let mut u = Unifier::new();
        let v = u.fresh_kinded(Kind::COMPARABLE);
        let ty = closed_record(&[("a", v.clone()), ("c", v)]);
        let s = site(&u, &ty);
        assert!(
            s.admit(&map(&[("a", Value::Int(1)), ("c", Value::Float(2.5))]))
                .is_ok()
        );
        let mixed = map(&[("a", Value::Int(1)), ("c", Value::string("x"))]);
        assert_eq!(
            refusal(&s, &mixed),
            "`/a` is a number and `/c` is text, and this script compares them"
        );
    }

    #[test]
    fn fixings_are_shared_by_every_site_of_a_unit() {
        let mut u = Unifier::new();
        let v = u.fresh_kinded(Kind::COMPARABLE);
        let fixings = Fixings::new();
        let first = Site::snapshot(&u, &v, Arc::clone(&fixings));
        let second = Site::snapshot(&u, &v, fixings);
        assert!(first.admit(&Value::Int(1)).is_ok());
        assert!(refusal(&second, &Value::string("x")).contains("compares them"));
    }

    #[test]
    fn a_recursive_type_admits_a_nested_document_and_refuses_a_scalar_leaf() {
        let mut u = Unifier::new();
        let v = u.fresh_tyvar();
        u.unify_ty(&Ty::Var(v), &Ty::Map(Box::new(Ty::Var(v))))
            .expect("a cycle through a map is data");
        let s = site(&u, &Ty::Var(v));
        let nested = map(&[("a", map(&[("b", map(&[]))]))]);
        assert!(s.admit(&nested).is_ok());
        let leaf = map(&[("a", map(&[("b", Value::Int(1))]))]);
        assert_eq!(
            refusal(&s, &leaf),
            "the value at `/a/b` is a whole number (`1`), but this script uses it as a map"
        );
    }

    #[test]
    fn a_json_number_keeps_its_kind_at_a_ground_site() {
        let float = refusal(&ground(&Ty::Float), &Value::Int(3));
        assert!(float.contains("a whole number (`3`)") && float.contains("as a Float"));
        let int = ground(&Ty::Int)
            .admit(&Value::Float(2.5))
            .expect_err("a fraction is not an Int");
        assert!(
            int.hint()
                .is_some_and(|hint| hint.contains("`float` or `int`"))
        );
    }

    #[test]
    fn a_variant_site_needs_a_tag_of_its_row() {
        let ty = closed_variant(&[("none", Ty::Unit), ("some", Ty::Int)]);
        let s = ground(&ty);
        let some = Value::Variant {
            label: "some".into(),
            payload: Some(Box::new(Value::Int(1))),
        };
        assert!(s.admit(&some).is_ok());
        let none = Value::Variant {
            label: "none".into(),
            payload: None,
        };
        assert!(s.admit(&none).is_ok());
        let other = Value::Variant {
            label: "other".into(),
            payload: None,
        };
        assert!(refusal(&s, &other).contains("one of the tags `none`, `some`"));
        assert!(refusal(&s, &map(&[])).contains("uses it as a variant"));
    }

    #[test]
    fn a_handle_site_admits_the_value_the_worker_settled_with() {
        let s = ground(&Ty::Handle(Box::new(Ty::Int)));
        assert!(s.admit_handled(&Value::Int(1)).is_ok());
        assert!(refusal_of(s.admit_handled(&Value::string("x"))).contains("uses it as an Int"));
    }

    fn refusal_of(result: Result<(), Mismatch>) -> String {
        result.expect_err("the value must be refused").to_string()
    }

    #[test]
    fn the_witness_is_the_use_that_fixed_the_structure() {
        let mut u = Unifier::new();
        let at = Span::new(FileId::DUMMY, 3, 9);
        u.at = Some(at);
        let v = u.fresh_tyvar();
        u.unify_ty(&Ty::Var(v), &Ty::Int)
            .expect("a free variable meets Int");
        let mismatch = site(&u, &Ty::Var(v))
            .admit(&Value::string("x"))
            .expect_err("text is not an Int");
        assert_eq!(mismatch.witness(), Some(at));
    }

    #[test]
    fn a_site_round_trips_the_wire_and_its_units_sites_meet_one_fixings() {
        let mut u = Unifier::new();
        let v = u.fresh_kinded(Kind::COMPARABLE);
        let fixings = Fixings::new();
        let (a, b) = (
            Site::snapshot(&u, &v, Arc::clone(&fixings)),
            Site::snapshot(&u, &v, fixings),
        );
        let wire = |site: &Site| postcard::to_allocvec(site).expect("a site serialises");
        let (a2, b2): (Site, Site) = (
            postcard::from_bytes(&wire(&a)).expect("and reads back"),
            postcard::from_bytes(&wire(&b)).expect("and reads back"),
        );
        assert_eq!(a2.nodes, a.nodes);
        assert!(Arc::ptr_eq(&a2.fixings, &b2.fixings));
        assert!(a2.admit(&Value::Int(1)).is_ok());
        assert!(b2.admit(&Value::string("x")).is_err());
    }

    #[test]
    fn a_module_closure_is_held_to_its_scheme() {
        use crate::typecheck::builtins::{fun, mk_plain_scheme, pure, thunk};
        let mut u = Unifier::new();
        let block = |param: Ty, result: Ty| thunk(fun(param, pure(result)));
        let ty = closed_record(&[("f", block(Ty::Int, Ty::Int))]);
        let site = site(&u, &ty);
        let record = map(&[("f", crate::types::block_over(&crate::types::Env::new()))]);
        let scheme_of = |param: Ty, result: Ty| {
            vec![(
                "f".to_owned(),
                Arc::new(mk_plain_scheme(&[], &[], block(param, result))),
            )]
        };
        assert!(
            site.admit_module(&record, &scheme_of(Ty::Int, Ty::Int))
                .is_ok()
        );
        let err = site
            .admit_module(&record, &scheme_of(Ty::String, Ty::Int))
            .expect_err("a block over text is no block over Int");
        assert!(
            err.to_string()
                .starts_with("the value at `/f` is a block of type")
        );
        let _ = &mut u;
    }
}
