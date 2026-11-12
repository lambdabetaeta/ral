//! Runtime values are value closures over the IR.

use super::Error;
use super::env::{Binding, Env};
use super::list::{List, ListRef};
use super::map::{Map, MapRef};
use super::signature::{Signature, lookup};
use crate::builtin::BuiltinEntry;
use crate::data::datum::Datum as _;
use crate::data::{Bytes, FOValue, Finite, Str};
use crate::fact::{ErrorFact, EvalFailure, Found, Reason, TextPreview, ValueShape};
use crate::handle::{Handle, HandleInner};
use crate::io::terminal::{InteractiveMode, TerminalState};
use crate::ir::{Comp, CompKind, Name, Occ, Val};
use crate::ty::{Binders, Closed, Head, TAG_PREFIX, Ty, closed_record};
use std::borrow::Cow;
use std::fmt;
use std::ops::ControlFlow;
use std::sync::Arc;

mod compare;
mod first_order;
mod seed;
pub(super) use seed::{drop_seed_env, drop_seed_value};

/// A runtime value: ⟨V, ρ⟩, every direct name of V bound in ρ.
#[derive(Debug, Clone)]
pub struct ValClos(Repr);

#[derive(Debug)]
enum Repr {
    Syn(Val, Env),
    List(Arc<imbl::Vector<ValClos>>),
    Map(Arc<imbl::OrdMap<String, ValClos>>, Option<Arc<ErrorFact>>),
}

const _: () = assert!(
    std::mem::size_of::<ValClos>() <= 48,
    "ValClos must fit in 48 bytes"
);

impl Clone for Repr {
    fn clone(&self) -> Self {
        match self {
            Self::Syn(val, env) => Self::Syn(clone_tag_spine(val), env.clone()),
            Self::List(items) => Self::List(Arc::clone(items)),
            Self::Map(entries, fact) => Self::Map(Arc::clone(entries), fact.clone()),
        }
    }
}

impl Drop for Repr {
    fn drop(&mut self) {
        let owned = match self {
            Self::Syn(val, env) => {
                let mut val = std::mem::replace(val, Val::Unit);
                while let Val::Variant { payload, .. } = val {
                    val = *payload;
                }
                (!env.is_empty()).then(|| Owned::Env(std::mem::take(env)))
            }
            Self::List(items) => Arc::get_mut(items)
                .filter(|items| !items.is_empty())
                .map(|items| Owned::List(std::mem::take(items))),
            Self::Map(entries, _) => Arc::get_mut(entries)
                .filter(|entries| !entries.is_empty())
                .map(|entries| Owned::Map(std::mem::take(entries))),
        };
        if let Some(owned) = owned {
            dismantle(owned);
        }
    }
}

/// A dying value's sole owner of a capture or built container.
enum Owned {
    Env(Env),
    List(imbl::Vector<ValClos>),
    Map(imbl::OrdMap<String, ValClos>),
}

thread_local! {
    static DISMANTLE_QUEUE: std::cell::RefCell<Option<Vec<Owned>>> =
        const { std::cell::RefCell::new(None) };
}

/// While one drop runs, a nested one queues its owner, bounding the stack.
fn dismantle(owned: Owned) {
    let Ok(Some(owned)) = DISMANTLE_QUEUE.try_with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(queue) = slot.as_mut() {
            queue.push(owned);
            None
        } else {
            *slot = Some(Vec::new());
            Some(owned)
        }
    }) else {
        return;
    };
    struct Disarm;
    impl Drop for Disarm {
        fn drop(&mut self) {
            let _ = DISMANTLE_QUEUE.try_with(|slot| slot.borrow_mut().take());
        }
    }
    let _disarm = Disarm;
    let mut next = Some(owned);
    while let Some(owned) = next {
        match owned {
            Owned::Env(env) => drop(env),
            Owned::List(items) => drop(items),
            Owned::Map(entries) => drop(entries),
        }
        next = DISMANTLE_QUEUE.with(|slot| slot.borrow_mut().as_mut().and_then(Vec::pop));
    }
}

/// A value's head, one layer read.
#[derive(Debug)]
pub enum View<'a> {
    Unit,
    Bool(bool),
    Int(i64),
    Float(Finite),
    String(&'a Str),
    Bytes(&'a Bytes),
    List(ListRef<'a>),
    Map(MapRef<'a>),
    Variant {
        label: &'a Name,
        /// `None` is the unit payload.
        payload: Option<Cow<'a, ValClos>>,
    },
    Thunk {
        comp: &'a Arc<Comp>,
        env: &'a Env,
    },
    Builtin(&'a Arc<BuiltinEntry>),
    Handle(&'a Handle),
}

impl ValClos {
    pub const UNIT: Self = Self(Repr::Syn(Val::Unit, Env::EMPTY));
    pub fn bool(b: bool) -> Self {
        Self(Repr::Syn(Val::Bool(b), Env::EMPTY))
    }
    pub fn int(n: i64) -> Self {
        Self(Repr::Syn(Val::Int(n), Env::EMPTY))
    }
    pub fn float(f: Finite) -> Self {
        Self(Repr::Syn(Val::Float(f), Env::EMPTY))
    }
    pub fn string(s: impl Into<Str>) -> Self {
        Self(Repr::Syn(Val::String(s.into()), Env::EMPTY))
    }
    pub fn bytes(b: impl Into<Bytes>) -> Self {
        Self(Repr::Syn(Val::Bytes(b.into()), Env::EMPTY))
    }
    pub fn list(items: Vec<Self>) -> Self {
        List::from(items).into()
    }
    /// The last pair wins on a duplicate key.
    pub fn map(pairs: Vec<(String, Self)>) -> Self {
        Map::from(pairs).into()
    }
    /// `None` is the unit payload. A builtin or handle never sits under a tag.
    pub fn variant(label: impl Into<Name>, payload: Option<Self>) -> Self {
        let (payload, env) = match payload {
            None => (Val::Unit, Env::EMPTY),
            Some(mut p) => match &mut p.0 {
                Repr::Syn(Val::Builtin(_) | Val::Handle(_), _) | Repr::List(_) | Repr::Map(..) => {
                    let name = crate::ir::synthetic("payload", 0);
                    let mut env = Env::EMPTY;
                    env.bind(
                        name.clone(),
                        Binding {
                            value: p,
                            scheme: None,
                        },
                    );
                    (Val::Variable(name), env)
                }
                Repr::Syn(val, env) => (std::mem::replace(val, Val::Unit), std::mem::take(env)),
            },
        };
        Self(Repr::Syn(
            Val::Variant {
                label: label.into(),
                payload: Box::new(payload),
            },
            env,
        ))
    }
    /// The record a program reads of `fact`; `raise` recovers `fact` from it.
    pub fn failure(fact: ErrorFact) -> Self {
        let FOValue::Map { entries } = fact.record().encode() else {
            unreachable!("a record encodes as a map")
        };
        let entries = entries
            .into_iter()
            .map(|(k, v)| (k, Self::from(v)))
            .collect();
        Self(Repr::Map(Arc::new(entries), Some(Arc::new(fact))))
    }
    pub fn builtin(entry: Arc<BuiltinEntry>) -> Self {
        Self(Repr::Syn(Val::Builtin(entry), Env::EMPTY))
    }
    pub(crate) fn handle(inner: HandleInner) -> Self {
        Self(Repr::Syn(
            Val::Handle(Handle::Live(Box::new(inner))),
            Env::EMPTY,
        ))
    }
    pub(crate) fn withheld(title: impl Into<Str>) -> Self {
        Self(Repr::Syn(
            Val::Handle(Handle::Withheld(title.into())),
            Env::EMPTY,
        ))
    }
    pub fn thunk(comp: Arc<Comp>, env: &Env) -> Self {
        let val = Val::thunk(comp);
        let Val::Thunk(node) = &val else {
            unreachable!()
        };
        let env = env.restrict(node.occ());
        Self(Repr::Syn(val, env))
    }
    /// Close one layer, pulling Σ's answers into ρ for direct names only.
    pub(crate) fn close(val: &Val, env: &Env, sig: &Signature) -> Result<Self, Error> {
        if let Val::Variable(name) = val {
            return lookup(name, env, sig)
                .cloned()
                .ok_or_else(|| missing(name, env, sig));
        }
        let mut captured = match val {
            Val::Thunk(node) => env.restrict(node.occ()),
            Val::List(node) => env.restrict(node.occ()),
            Val::Record(node) | Val::Map(node) => env.restrict(node.occ()),
            Val::Variant { .. } => {
                let mut terminal = val;
                while let Val::Variant { payload, .. } = terminal {
                    terminal = payload;
                }
                env.restrict(&Occ::of(terminal))
            }
            Val::Unit
            | Val::Bool(_)
            | Val::Int(_)
            | Val::Float(_)
            | Val::String(_)
            | Val::Bytes(_)
            | Val::Builtin(_)
            | Val::Handle(_) => Env::EMPTY,
            Val::Variable(_) => unreachable!("variables are resolved above"),
        };
        pull_direct(val, &mut captured, env, sig)?;
        Ok(Self(Repr::Syn(clone_tag_spine(val), captured)))
    }
    pub fn view(&self) -> View<'_> {
        match &self.0 {
            Repr::List(items) => View::List(ListRef::built(items)),
            Repr::Map(entries, fact) => View::Map(MapRef::built(entries, fact.as_deref())),
            Repr::Syn(val, env) => match val {
                Val::Unit => View::Unit,
                Val::Bool(b) => View::Bool(*b),
                Val::Int(n) => View::Int(*n),
                Val::Float(f) => View::Float(*f),
                Val::String(s) => View::String(s),
                Val::Bytes(b) => View::Bytes(b),
                Val::Builtin(entry) => View::Builtin(entry),
                Val::Handle(handle) => View::Handle(handle),
                Val::Thunk(node) => View::Thunk {
                    comp: node.shape(),
                    env,
                },
                Val::List(node) => View::List(ListRef::syn(node.shape(), env)),
                Val::Record(node) | Val::Map(node) => View::Map(MapRef::syn(node.shape(), env)),
                Val::Variant { label, payload } => View::Variant {
                    label,
                    payload: (!matches!(payload.as_ref(), Val::Unit))
                        .then(|| read(payload, env))
                        .filter(|p| !matches!(&p.as_ref().0, Repr::Syn(Val::Unit, _))),
                },
                Val::Variable(_) => unreachable!("a value closure's root is not a variable"),
            },
        }
    }
    /// Read a whole tag spine without materialising its successive suffixes.
    pub(crate) fn peel_variants(&self) -> (Vec<&Name>, Cow<'_, Self>) {
        let mut labels = Vec::new();
        let mut value = self;
        loop {
            let Repr::Syn(val @ Val::Variant { .. }, env) = &value.0 else {
                return (labels, Cow::Borrowed(value));
            };
            let mut val = val;
            while let Val::Variant { label, payload } = val {
                labels.push(label);
                val = payload;
            }
            match val {
                Val::Variable(name) => {
                    value = env
                        .get(name)
                        .expect("a value closure binds its direct names");
                }
                _ => return (labels, read(val, env)),
            }
        }
    }
    pub(crate) fn into_computation(mut self) -> Result<(Arc<Comp>, Env), Self> {
        if let Repr::Syn(Val::Thunk(node), env) = &mut self.0 {
            return Ok((Arc::clone(node.shape()), std::mem::take(env)));
        }
        if let View::Builtin(entry) = self.view() {
            let (comp, name) = entry.eta();
            let (comp, name) = (Arc::clone(comp), name.clone());
            let mut env = Env::EMPTY;
            env.bind(
                name,
                Binding {
                    value: self,
                    scheme: None,
                },
            );
            return Ok((comp, env));
        }
        Err(self)
    }
    pub(crate) fn captured_envs(&self, out: &mut Vec<Env>) {
        match &self.0 {
            Repr::Syn(_, env) => out.push(env.clone()),
            Repr::List(items) => items.iter().for_each(|v| v.captured_envs(out)),
            Repr::Map(entries, _) => entries.values().for_each(|v| v.captured_envs(out)),
        }
    }
    pub(crate) fn scrubbed(&self, scope: &mut impl FnMut(&Env) -> Option<Env>) -> Option<Self> {
        match &self.0 {
            Repr::Syn(val, env) => {
                let changed_val = withhold(val);
                let changed_env = scope(env);
                if changed_val.is_none() && changed_env.is_none() {
                    return None;
                }
                Some(Self(Repr::Syn(
                    changed_val.unwrap_or_else(|| clone_tag_spine(val)),
                    changed_env.unwrap_or_else(|| env.clone()),
                )))
            }
            Repr::List(items) => patched(
                items,
                items
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| v.scrubbed(scope).map(|v| (i, v))),
                |items, i, v| {
                    Arc::make_mut(items).set(i, v);
                },
            )
            .map(|items| Self(Repr::List(items))),
            Repr::Map(entries, fact) => patched(
                entries,
                entries
                    .iter()
                    .filter_map(|(k, v)| v.scrubbed(scope).map(|v| (k.clone(), v))),
                |entries, k, v| {
                    Arc::make_mut(entries).insert(k, v);
                },
            )
            .map(|entries| Self(Repr::Map(entries, fact.clone()))),
        }
    }
    /// The builtin this value is, and the arguments its η residual still awaits.
    pub(crate) fn residual(&self) -> Option<(&Arc<BuiltinEntry>, usize)> {
        match self.view() {
            View::Builtin(entry) => Some((entry, entry.decl.fixed_arity())),
            View::Thunk { comp, env } => {
                let mut comp = comp.as_ref();
                let mut remaining = 0;
                while let CompKind::Lam { body, .. } = &comp.item {
                    remaining += 1;
                    comp = body;
                }
                let CompKind::Builtin { builtin, .. } = &comp.item else {
                    return None;
                };
                let View::Builtin(entry) = env.get(builtin)?.view() else {
                    return None;
                };
                Some((entry, remaining))
            }
            _ => None,
        }
    }
    /// A thunk whose body the checker reads: not a builtin's η residual.
    pub(crate) fn block(&self) -> Option<(&Arc<Comp>, &Env)> {
        match self.view() {
            View::Thunk { comp, env } if self.residual().is_none() => Some((comp, env)),
            _ => None,
        }
    }
    pub(crate) fn try_for_each_child<B>(
        &self,
        f: &mut impl FnMut(&Self) -> ControlFlow<B>,
    ) -> ControlFlow<B> {
        match self.view() {
            View::List(items) => items.iter().try_for_each(|v| f(&v)),
            View::Map(entries) => entries.iter().try_for_each(|(_, v)| f(&v)),
            View::Variant { payload, .. } => payload.iter().try_for_each(|v| f(v)),
            _ => ControlFlow::Continue(()),
        }
    }
    #[cfg(test)]
    pub(crate) fn env(&self) -> Option<&Env> {
        match &self.0 {
            Repr::Syn(_, env) => Some(env),
            _ => None,
        }
    }
    pub(crate) fn drop_seed(self) {
        drop_seed_value(self);
    }
    pub fn shape(&self) -> ValueShape {
        match self.view() {
            View::Unit => ValueShape::Unit,
            View::Bool(_) => ValueShape::Bool,
            View::Int(_) => ValueShape::Int,
            View::Float(_) => ValueShape::Float,
            View::String(_) => ValueShape::String,
            View::Bytes(_) => ValueShape::Bytes,
            View::List(_) => ValueShape::List,
            View::Map(_) => ValueShape::Map,
            View::Variant { .. } => ValueShape::Variant,
            View::Thunk { comp, .. } if comp.lam().is_none() => ValueShape::Block,
            View::Thunk { .. } | View::Builtin(_) => ValueShape::Function,
            View::Handle(_) => ValueShape::Handle,
        }
    }
    pub fn found(&self) -> Found {
        match self.view() {
            View::Unit => Found::Unit,
            View::Bool(b) => Found::Bool(b),
            View::Int(n) => Found::Int(n),
            View::Float(f) => Found::Float(f),
            View::String(s) => Found::Text(TextPreview::new(s)),
            View::Bytes(_) => Found::Bytes,
            View::List(_) => Found::List,
            View::Map(_) => Found::Map,
            View::Variant { label, .. } => Found::Variant(label.to_string()),
            View::Thunk { comp, .. } if comp.lam().is_none() => Found::Block,
            View::Thunk { .. } | View::Builtin(_) => Found::Function,
            View::Handle(_) => Found::Handle,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self.view() {
            View::Int(n) => Some(n),
            _ => None,
        }
    }
    pub(crate) fn as_float(&self) -> Option<f64> {
        match self.view() {
            #[allow(
                clippy::cast_precision_loss,
                reason = "Int→Float loses precision beyond 2^53"
            )]
            View::Int(n) => Some(n as f64),
            View::Float(f) => Some(f.get()),
            _ => None,
        }
    }
    pub(crate) fn heads(&self) -> &'static [Head] {
        match self.view() {
            View::Unit => &[Head::Unit],
            View::Bool(_) => &[Head::Bool],
            View::Int(_) => &[Head::Int],
            View::Float(_) => &[Head::Float],
            View::String(_) => &[Head::String],
            View::Bytes(_) => &[Head::Bytes],
            View::List(_) => &[Head::List],
            View::Map(_) => &[Head::Map, Head::Record],
            View::Variant { .. } => &[Head::Variant],
            View::Thunk { .. } | View::Builtin(_) => &[Head::Thunk],
            View::Handle(_) => &[Head::Handle],
        }
    }
    pub fn scalar_text(&self) -> Option<String> {
        matches!(
            self.view(),
            View::String(_) | View::Int(_) | View::Float(_) | View::Bool(_)
        )
        .then(|| self.to_string())
    }
    /// Render every argument through the total text conversion.
    pub(crate) fn render_argv(args: &[Self]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }
    pub fn type_name(&self) -> &'static str {
        match self.view() {
            View::Unit => "Unit",
            View::Bool(_) => "Bool",
            View::Int(_) => "Int",
            View::Float(_) => "Float",
            View::String(_) => "String",
            View::Bytes(_) => "Bytes",
            View::List(_) => "List",
            View::Map(_) => "Map",
            View::Variant { .. } => "Variant",
            View::Thunk { comp, .. } if comp.lam().is_some() => "Lambda",
            View::Thunk { .. } => "Block",
            View::Builtin(_) => "Native",
            View::Handle(_) => "Handle",
        }
    }
    /// Estimate data bytes; never descends a capture or a handle's buffers.
    pub(crate) fn shallow_size(&self) -> usize {
        let (labels, terminal) = self.peel_variants();
        labels.into_iter().map(|label| label.len()).sum::<usize>()
            + match terminal.view() {
                View::Unit => 0,
                View::Bool(_) => 1,
                View::Int(_) | View::Float(_) => 8,
                View::String(s) => s.len(),
                View::Bytes(b) => b.len(),
                View::List(items) => items.iter().map(|v| v.shallow_size()).sum(),
                View::Map(entries) => entries
                    .iter()
                    .map(|(k, v)| k.len() + v.shallow_size())
                    .sum(),
                View::Variant { .. } => unreachable!("the tag spine was peeled above"),
                View::Thunk { .. } | View::Builtin(_) | View::Handle(_) => 32,
            }
    }
}

impl From<List> for ValClos {
    fn from(list: List) -> Self {
        Self(Repr::List(list.0))
    }
}
impl From<Map> for ValClos {
    fn from(map: Map) -> Self {
        Self(Repr::Map(map.0, None))
    }
}

/// Component V′ of ⟨V, ρ⟩, read as ⟨V′, ρ⟩.
pub(super) fn read<'a>(val: &'a Val, env: &'a Env) -> Cow<'a, ValClos> {
    match val {
        Val::Variable(name) => Cow::Borrowed(
            env.get(name)
                .expect("a value closure binds its direct names"),
        ),
        _ => Cow::Owned(ValClos(Repr::Syn(clone_tag_spine(val), env.clone()))),
    }
}

fn clone_tag_spine(mut val: &Val) -> Val {
    let mut labels = Vec::new();
    while let Val::Variant { label, payload } = val {
        labels.push(label.clone());
        val = payload;
    }
    labels
        .into_iter()
        .rev()
        .fold(val.clone(), |payload, label| Val::Variant {
            label,
            payload: Box::new(payload),
        })
}

fn missing(name: &str, env: &Env, sig: &Signature) -> Error {
    let hint = match name {
        "STATUS" => None,
        _ => crate::typecheck::hyphen_boundary(name, |p| lookup(p, env, sig).is_some())
            .map(crate::typecheck::hyphen_boundary_hint),
    };
    let error = Error::new(Reason::Evaluation(Box::new(EvalFailure::MissingBinding(
        name.to_string(),
    ))));
    match hint {
        Some(hint) => error.with_hint(hint),
        None => error,
    }
}

fn pull_direct(mut val: &Val, captured: &mut Env, env: &Env, sig: &Signature) -> Result<(), Error> {
    while let Val::Variant { payload, .. } = val {
        val = payload;
    }
    match val {
        Val::Variable(name) if captured.get(name).is_none() => {
            let value = lookup(name, env, sig)
                .cloned()
                .ok_or_else(|| missing(name, env, sig))?;
            captured.bind(
                name.clone(),
                Binding {
                    value,
                    scheme: None,
                },
            );
        }
        Val::Variant { .. } => unreachable!("the tag spine was peeled above"),
        Val::List(node) => {
            for elem in node.shape() {
                pull_direct(&elem.item, captured, env, sig)?;
            }
        }
        Val::Record(node) | Val::Map(node) => {
            for (_, elem) in node.shape() {
                pull_direct(&elem.item, captured, env, sig)?;
            }
        }
        Val::Unit
        | Val::Bool(_)
        | Val::Int(_)
        | Val::Float(_)
        | Val::String(_)
        | Val::Bytes(_)
        | Val::Builtin(_)
        | Val::Handle(_)
        | Val::Thunk(_)
        | Val::Variable(_) => {}
    }
    Ok(())
}

fn withhold(mut val: &Val) -> Option<Val> {
    let mut labels = Vec::new();
    while let Val::Variant { label, payload } = val {
        labels.push(label);
        val = payload;
    }
    let Val::Handle(Handle::Live(h)) = val else {
        return None;
    };
    Some(labels.into_iter().rev().fold(
        Val::Handle(Handle::Withheld(h.title.clone().into())),
        |payload, label| Val::Variant {
            label: label.clone(),
            payload: Box::new(payload),
        },
    ))
}

fn patched<C: Clone, K, V>(
    container: &C,
    changes: impl IntoIterator<Item = (K, V)>,
    mut put: impl FnMut(&mut C, K, V),
) -> Option<C> {
    let mut changes = changes.into_iter().peekable();
    changes.peek()?;
    let mut copy = container.clone();
    for (key, new) in changes {
        put(&mut copy, key, new);
    }
    Some(copy)
}

/// Representational equality; ral's `==` is [`ValClos::equals`].
impl PartialEq for ValClos {
    fn eq(&self, other: &Self) -> bool {
        let (left_labels, left) = self.peel_variants();
        let (right_labels, right) = other.peel_variants();
        if left_labels != right_labels {
            return false;
        }
        match (left.view(), right.view()) {
            (View::Unit, View::Unit) => true,
            (View::Bool(a), View::Bool(b)) => a == b,
            (View::Int(a), View::Int(b)) => a == b,
            (View::Float(a), View::Float(b)) => a == b,
            (View::String(a), View::String(b)) => a == b,
            (View::Bytes(a), View::Bytes(b)) => a == b,
            (View::List(a), View::List(b)) => a.iter().eq(b.iter()),
            (View::Map(a), View::Map(b)) => a.len() == b.len() && a.iter().eq(b.iter()),
            _ => false,
        }
    }
}
impl fmt::Display for ValClos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (labels, terminal) = self.peel_variants();
        for (i, label) in labels.iter().enumerate() {
            if i > 0 {
                write!(f, " ")?;
            }
            write!(f, "{TAG_PREFIX}{label}")?;
        }
        if !labels.is_empty() {
            if matches!(terminal.view(), View::Unit) {
                return Ok(());
            }
            write!(f, " ")?;
        }
        if let Some((entry, remaining)) = terminal.residual() {
            let arity = entry.decl.fixed_arity();
            write!(f, "<native {}", entry.decl.name)?;
            if remaining < arity {
                write!(f, ", {} of {arity} arguments", arity - remaining)?;
            }
            return write!(f, ">");
        }
        match terminal.view() {
            View::Unit => write!(f, "()"),
            View::Bool(b) => write!(f, "{b}"),
            View::Int(n) => write!(f, "{n}"),
            View::Float(n) => write!(f, "{n}"),
            View::String(s) => write!(f, "{s}"),
            View::Bytes(b) => write!(f, "{}", String::from_utf8_lossy(b)),
            View::List(items) => {
                write!(f, "[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", v.as_ref())?;
                }
                write!(f, "]")
            }
            View::Map(entries) => {
                if entries.is_empty() {
                    return write!(f, "[:]");
                }
                write!(f, "[")?;
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {}", v.as_ref())?;
                }
                write!(f, "]")
            }
            View::Variant { .. } => unreachable!("the tag spine was peeled above"),
            View::Thunk { comp, .. } => match comp.lam() {
                Some((param, body)) => write!(f, "{}", fmt_lambda(param, body)),
                None => write!(f, "<block>"),
            },
            View::Builtin(_) => unreachable!("a builtin has a residual"),
            View::Handle(h) => write!(f, "<handle:{}>", h.title()),
        }
    }
}

fn fmt_lambda(param: &Name, body: &Comp) -> String {
    use crate::ir::is_synthetic;
    let mut names = vec![param];
    let mut comp = body;
    while let CompKind::Lam { param, body } = &comp.item {
        names.push(param);
        comp = body;
    }
    let mut params: Vec<_> = names
        .iter()
        .map(|name| if is_synthetic(name) { "_" } else { name }.to_string())
        .collect();
    while let CompKind::Bind {
        comp: rhs,
        hole: (),
        pattern,
        rest,
    } = &comp.item
        && let CompKind::Return(Val::Variable(name)) = &rhs.item
        && is_synthetic(name)
    {
        if let Some(index) = names.iter().position(|param| *param == name) {
            params[index] = pattern.to_string();
        }
        comp = rest;
    }
    format!("<|{}| block>", params.join(" "))
}

enum TerminalField {
    Flag(fn(&TerminalState) -> bool),
    Mode,
}
impl TerminalField {
    fn value(&self, terminal: &TerminalState) -> ValClos {
        match self {
            Self::Flag(read) => ValClos::bool(read(terminal)),
            Self::Mode => ValClos::string(match terminal.mode {
                InteractiveMode::Auto => "auto",
                InteractiveMode::Minimal => "minimal",
                InteractiveMode::Full => "full",
            }),
        }
    }
    fn ty(&self) -> Ty {
        match self {
            Self::Flag(_) => Ty::bool(),
            Self::Mode => Ty::string(),
        }
    }
}
const TERMINAL_FIELDS: &[(&str, TerminalField)] = &[
    ("stdin-tty", TerminalField::Flag(|t| t.startup_stdin_tty)),
    ("stdout-tty", TerminalField::Flag(|t| t.startup_stdout_tty)),
    ("stderr-tty", TerminalField::Flag(|t| t.startup_stderr_tty)),
    ("supports-ansi", TerminalField::Flag(|t| t.supports_ansi)),
    ("no-color", TerminalField::Flag(|t| t.no_color)),
    ("is-tmux", TerminalField::Flag(|t| t.is_tmux)),
    ("is-asciinema", TerminalField::Flag(|t| t.is_asciinema)),
    ("is-ci", TerminalField::Flag(|t| t.is_ci)),
    ("truecolor", TerminalField::Flag(|t| t.truecolor)),
    ("hyperlinks", TerminalField::Flag(TerminalState::hyperlinks)),
    (
        "clipboard-write",
        TerminalField::Flag(TerminalState::clipboard_write),
    ),
    (
        "bracketed-paste",
        TerminalField::Flag(TerminalState::bracketed_paste),
    ),
    ("ui-ansi-ok", TerminalField::Flag(TerminalState::ui_ansi_ok)),
    (
        "ui-truecolor-ok",
        TerminalField::Flag(TerminalState::ui_truecolor_ok),
    ),
    (
        "ui-hyperlinks-ok",
        TerminalField::Flag(TerminalState::ui_hyperlinks_ok),
    ),
    (
        "ui-clipboard-write-ok",
        TerminalField::Flag(TerminalState::ui_clipboard_write_ok),
    ),
    (
        "ui-bracketed-paste-ok",
        TerminalField::Flag(TerminalState::ui_bracketed_paste_ok),
    ),
    ("mode", TerminalField::Mode),
];
impl ValClos {
    pub fn terminal_scheme() -> Closed {
        let fields = TERMINAL_FIELDS
            .iter()
            .map(|(name, field)| (*name, field.ty()))
            .collect::<Vec<_>>();
        Binders::close(|_| closed_record(&fields))
    }
}
impl From<&TerminalState> for ValClos {
    fn from(terminal: &TerminalState) -> Self {
        Self::map(
            TERMINAL_FIELDS
                .iter()
                .map(|(name, field)| ((*name).into(), field.value(terminal)))
                .collect(),
        )
    }
}

#[cfg(test)]
pub(crate) fn block_over(env: &Env) -> ValClos {
    let names = env
        .names()
        .map(|n| crate::source::Spanned::synthetic(Val::Variable(n.clone())))
        .collect::<Vec<_>>();
    ValClos::thunk(
        Arc::new(crate::source::Spanned::synthetic(CompKind::Return(
            Val::list(names),
        ))),
        env,
    )
}
#[cfg(test)]
pub(crate) fn captured(v: Option<&ValClos>) -> &Env {
    match v.map(ValClos::view) {
        Some(View::Thunk { env, .. }) => env,
        other => panic!("expected a block, got {other:?}"),
    }
}
#[cfg(test)]
pub(crate) fn deep_block_chain(n: usize, foot: ValClos) -> ValClos {
    let body = Arc::new(crate::source::Spanned::synthetic(CompKind::Return(
        Val::Variable("tail".into()),
    )));
    let mut v = foot;
    for _ in 0..n {
        let mut env = Env::EMPTY;
        env.bind(
            "tail".into(),
            Binding {
                value: v,
                scheme: None,
            },
        );
        v = ValClos::thunk(Arc::clone(&body), &env);
    }
    v
}
#[cfg(test)]
pub(crate) fn deep_list_chain(n: usize, foot: ValClos) -> ValClos {
    let val = Val::list(vec![crate::source::Spanned::synthetic(Val::Variable(
        "acc".into(),
    ))]);
    let sig = Signature::default();
    let mut v = foot;
    for _ in 0..n {
        let mut env = Env::EMPTY;
        env.bind(
            "acc".into(),
            Binding {
                value: v,
                scheme: None,
            },
        );
        v = ValClos::close(&val, &env, &sig).unwrap();
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_records_match_their_closed_scheme_in_every_mode() {
        let scheme = ValClos::terminal_scheme();
        let site = crate::ty::Site::snapshot(
            &crate::typecheck::Unifier::new(),
            &scheme.ty,
            crate::ty::UnitId::fresh(),
        );
        for (mode, text) in [
            (InteractiveMode::Auto, "auto"),
            (InteractiveMode::Minimal, "minimal"),
            (InteractiveMode::Full, "full"),
        ] {
            let terminal = TerminalState {
                mode,
                ..TerminalState::default()
            };
            let value = ValClos::from(&terminal);
            site.admit(&value, &crate::ty::Fixings::default())
                .expect("TERMINAL matches its scheme");
            assert_eq!(
                value.as_map("test").unwrap().get("mode").as_deref(),
                Some(&ValClos::string(text))
            );
        }
    }
    #[test]
    fn a_literal_naming_a_signature_name_captures_its_value() {
        let mut sig = Signature::default();
        sig.install_prelude(Arc::new(
            std::iter::once((
                "k".into(),
                Binding {
                    value: ValClos::int(7),
                    scheme: None,
                },
            ))
            .collect(),
        ));
        let val = Val::list(vec![crate::source::Spanned::synthetic(Val::Variable(
            "k".into(),
        ))]);
        let value = ValClos::close(&val, &Env::EMPTY, &sig).unwrap();
        assert_eq!(value.env().unwrap().get("k"), Some(&ValClos::int(7)));
        assert_eq!(
            value.as_list("test").unwrap().get(0).as_deref(),
            Some(&ValClos::int(7))
        );
    }
    #[test]
    fn shallow_size_never_descends_into_closure_captures() {
        let block = block_over(&Env::EMPTY);
        let size = block.shallow_size();
        assert!(size > 0);
        let mut env = Env::EMPTY;
        env.bind(
            "heavy".into(),
            Binding {
                value: ValClos::string("x".repeat(10_000)),
                scheme: None,
            },
        );
        assert_eq!(block_over(&env).shallow_size(), size);
        assert_eq!(ValClos::list(vec![block]).shallow_size(), size);
    }
    #[test]
    fn a_component_shares_the_whole_capture() {
        let mut env = Env::EMPTY;
        for (name, n) in [("a", 1), ("b", 2)] {
            env.bind(
                name.into(),
                Binding {
                    value: ValClos::int(n),
                    scheme: None,
                },
            );
        }
        let val = Val::list(vec![
            crate::source::Spanned::synthetic(Val::list(vec![crate::source::Spanned::synthetic(
                Val::Variable("a".into()),
            )])),
            crate::source::Spanned::synthetic(Val::Variable("b".into())),
        ]);
        let value = ValClos::close(&val, &env, &Signature::default()).unwrap();
        let child = value.as_list("test").unwrap().get(0).unwrap();
        assert!(child.env().unwrap().ptr_eq(value.env().unwrap()));
        assert_eq!(child.env().unwrap().get("b"), Some(&ValClos::int(2)));
    }
    #[test]
    fn a_deep_literal_chain_drops_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| drop(deep_list_chain(100_000, ValClos::UNIT)))
            .expect("spawn")
            .join()
            .expect("a deep literal chain drops without exhausting the stack");
    }
    #[test]
    fn deep_built_chains_drop_on_a_small_stack() {
        let chains: [fn(ValClos) -> ValClos; 2] = [
            |prev| ValClos::list(vec![prev]),
            |prev| ValClos::map(vec![("k".into(), prev)]),
        ];
        for link in chains {
            std::thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || drop((0..100_000).fold(ValClos::UNIT, |prev, _| link(prev))))
                .expect("spawn")
                .join()
                .expect("a deep built chain drops without exhausting the stack");
        }
    }
}
