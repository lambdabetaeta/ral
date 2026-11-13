//! Runtime values are ⟨V, ρ⟩: syntax and the environment closing it.

use crate::builtin::BuiltinEntry;
use crate::data::print::tag_run;
use crate::data::{Bytes, Finite, Str};
use crate::fact::{Found, TextPreview, ValueShape};
use crate::handle::{Handle, HandleInner};
use crate::io::terminal::{InteractiveMode, TerminalState};
use crate::ir::{Binding, Comp, CompKind, Env, Name, Node, Val};
use crate::ty::{Binders, Closed, Head, Ty, closed_record};
use either::Either;
use std::fmt;
use std::sync::Arc;

mod first_order;
mod list;
mod map;

pub use list::ListRef;
pub use map::MapRef;

/// A runtime value: ⟨V, ρ⟩, ρ binding every direct name of V. V's root is
/// never a variable.
#[derive(Debug, Clone)]
pub struct ValClos(Val, Env);

const _: () = assert!(
    std::mem::size_of::<ValClos>() <= 48,
    "ValClos must fit in 48 bytes"
);

/// The first direct name of `val` that `env` does not bind.
fn first_unbound(val: &Val, env: &Env) -> Result<(), Name> {
    val.for_each_direct(&mut |name| env.get(name).map(|_| ()).ok_or_else(|| name.clone()))
}

/// A borrowed [`ValClos`]: the same invariant, V's root is never a variable.
#[derive(Debug, Clone, Copy)]
pub struct VClosRef<'a>(&'a Val, &'a Env);

/// What a dying value alone owns.
enum Owned {
    Env(Env),
    Val(Val),
}

/// Whether dropping `val` can recurse into nested values.
fn nests(val: &Val) -> bool {
    matches!(
        val,
        Val::Variant { .. } | Val::List(_) | Val::Record(_) | Val::Map(_)
    )
}

impl Drop for ValClos {
    fn drop(&mut self) {
        let val = std::mem::replace(&mut self.0, Val::Unit);
        if nests(&val) {
            dismantle(Owned::Val(val));
        }
        let env = std::mem::take(&mut self.1);
        if !env.is_empty() {
            dismantle(Owned::Env(env));
        }
    }
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
            Owned::Val(val) => take_apart(val),
        }
        next = DISMANTLE_QUEUE.with(|slot| slot.borrow_mut().as_mut().and_then(Vec::pop));
    }
}

/// Queue the nested values of `val` that nothing else holds.
fn take_apart(mut val: Val) {
    while let Val::Variant { payload, .. } = val {
        val = *payload;
    }
    let children = match val {
        Val::List(node) => Arc::into_inner(node)
            .map(|node| Either::Left(node.into_shape().into_iter().map(|elem| elem.item))),
        Val::Record(node) | Val::Map(node) => Arc::into_inner(node)
            .map(|node| Either::Right(node.into_shape().into_iter().map(|(_, elem)| elem.item))),
        _ => None,
    };
    for child in children.into_iter().flatten().filter(nests) {
        dismantle(Owned::Val(child));
    }
}

/// A value's head and nothing below it.
#[derive(Debug, Clone, Copy)]
pub enum View<'a> {
    Unit,
    Bool(bool),
    Int(i64),
    Float(Finite),
    String(&'a Str),
    Bytes(&'a Bytes),
    List(ListRef<'a>),
    Map(MapRef<'a>),
    /// A bare tag's payload is `()`.
    Variant {
        label: &'a Name,
        payload: VClosRef<'a>,
    },
    Thunk {
        comp: &'a Arc<Comp>,
        env: &'a Env,
    },
    Builtin(&'a Arc<BuiltinEntry>),
    Handle(&'a Handle),
}

impl ValClos {
    pub const UNIT: Self = Self(Val::Unit, Env::EMPTY);
    pub fn bool(b: bool) -> Self {
        Self(Val::Bool(b), Env::EMPTY)
    }
    pub fn int(n: i64) -> Self {
        Self(Val::Int(n), Env::EMPTY)
    }
    pub fn float(f: Finite) -> Self {
        Self(Val::Float(f), Env::EMPTY)
    }
    pub fn string(s: impl Into<Str>) -> Self {
        Self(Val::String(s.into()), Env::EMPTY)
    }
    pub fn bytes(b: impl Into<Bytes>) -> Self {
        Self(Val::Bytes(b.into()), Env::EMPTY)
    }
    /// `None` is the unit payload. The payload keeps its environment, except a
    /// builtin, handle or η residual, which is bound to a name.
    pub fn variant(label: impl Into<Name>, payload: Option<Self>) -> Self {
        let (payload, env) = match payload {
            None => (Val::Unit, Env::EMPTY),
            Some(mut p) => {
                if matches!(p.0, Val::Handle(_)) || VClosRef::from(&p).residual().is_some() {
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
                } else {
                    (
                        std::mem::replace(&mut p.0, Val::Unit),
                        std::mem::take(&mut p.1),
                    )
                }
            }
        };
        Self(
            Val::Variant {
                label: label.into(),
                payload: Box::new(payload),
            },
            env,
        )
    }
    pub fn builtin(entry: Arc<BuiltinEntry>) -> Self {
        Self(Val::Builtin(entry), Env::EMPTY)
    }
    pub(crate) fn handle(inner: HandleInner) -> Self {
        Self(Val::Handle(Handle::Live(Box::new(inner))), Env::EMPTY)
    }
    pub(crate) fn withheld(title: impl Into<Str>) -> Self {
        Self(Val::Handle(Handle::Withheld(title.into())), Env::EMPTY)
    }
    pub fn thunk(comp: Arc<Comp>, env: &Env) -> Self {
        let node = Node::new(comp);
        let env = env.restrict(node.occ());
        Self(Val::Thunk(node), env)
    }
    /// ⟨V, ρ⟩, which the caller guarantees: V's root is not a variable and ρ
    /// binds every direct name of V.
    pub(crate) fn from_parts(val: Val, env: Env) -> Self {
        debug_assert!(!matches!(val, Val::Variable(_)), "a root is not a variable");
        debug_assert!(
            first_unbound(&val, &env).is_ok(),
            "ρ binds every direct name"
        );
        Self(val, env)
    }
    /// The syntax and environment, for the marshaller.
    pub(crate) fn parts(&self) -> (&Val, &Env) {
        (&self.0, &self.1)
    }
    /// The syntax, if it is an atom; otherwise `self`.
    pub(crate) fn into_inline(mut self) -> Result<Val, Self> {
        if matches!(
            self.0,
            Val::Unit | Val::Bool(_) | Val::Int(_) | Val::Float(_) | Val::String(_) | Val::Bytes(_)
        ) {
            Ok(std::mem::replace(&mut self.0, Val::Unit))
        } else {
            Err(self)
        }
    }
    /// ⟨`t₁ … `tₙ foot, env⟩, outermost tag first, `env` kept as given. A bare
    /// variable is its binding. Refused with the first direct name of `foot`
    /// that `env` does not bind.
    pub(crate) fn closure(tags: Vec<Name>, foot: Val, env: Env) -> Result<Self, Name> {
        if tags.is_empty()
            && let Val::Variable(name) = &foot
        {
            return env.get(name).cloned().ok_or_else(|| name.clone());
        }
        first_unbound(&foot, &env)?;
        let val = tags
            .into_iter()
            .rev()
            .fold(foot, |payload, label| Val::Variant {
                label,
                payload: Box::new(payload),
            });
        Ok(Self(val, env))
    }
    pub fn view(&self) -> View<'_> {
        VClosRef::from(self).view()
    }
    pub(crate) fn into_computation(mut self) -> Result<(Arc<Comp>, Env), Self> {
        if let Val::Thunk(node) = &self.0 {
            let comp = Arc::clone(node.shape());
            return Ok((comp, std::mem::take(&mut self.1)));
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
    pub(crate) fn residual(&self) -> Option<(&Arc<BuiltinEntry>, usize)> {
        VClosRef::from(self).residual()
    }
    pub(crate) fn block(&self) -> Option<(&Arc<Comp>, &Env)> {
        VClosRef::from(self).block()
    }
    pub fn shape(&self) -> ValueShape {
        VClosRef::from(self).shape()
    }
    pub fn found(&self) -> Found {
        VClosRef::from(self).found()
    }
    pub fn as_int(&self) -> Option<i64> {
        VClosRef::from(self).as_int()
    }
    pub(crate) fn as_float(&self) -> Option<f64> {
        VClosRef::from(self).as_float()
    }
    pub(crate) fn heads(&self) -> &'static [Head] {
        VClosRef::from(self).heads()
    }
    pub fn scalar_text(&self) -> Option<String> {
        VClosRef::from(self).scalar_text()
    }
    pub fn type_name(&self) -> &'static str {
        VClosRef::from(self).type_name()
    }
    pub(crate) fn shallow_size(&self) -> usize {
        VClosRef::from(self).shallow_size()
    }
    /// Render every argument through the total text conversion.
    pub(crate) fn render_argv(args: &[Self]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }
}

impl<'a> From<&'a ValClos> for VClosRef<'a> {
    fn from(value: &'a ValClos) -> Self {
        Self(&value.0, &value.1)
    }
}

impl<'a> VClosRef<'a> {
    /// The component `val` of ⟨V, ρ⟩ as a value: a variable is its binding,
    /// found in one lookup.
    pub(crate) fn sub(val: &'a Val, env: &'a Env) -> Self {
        match val {
            Val::Variable(name) => Self::from(
                env.get(name)
                    .expect("a value closure binds its direct names"),
            ),
            _ => Self(val, env),
        }
    }
    /// ⟨V′, ρ⟩ sharing ρ; ∅ when V′ mentions no name.
    pub fn into_owned(self) -> ValClos {
        let env = if self.1.is_empty() || self.0.mentions_nothing() {
            Env::EMPTY
        } else {
            self.1.clone()
        };
        ValClos(self.0.clone(), env)
    }
    pub fn view(self) -> View<'a> {
        let Self(val, env) = self;
        match val {
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
            Val::List(node) => View::List(ListRef::new(node.shape(), env)),
            Val::Record(node) | Val::Map(node) => View::Map(MapRef::new(node.shape(), env)),
            Val::Variant { label, payload } => View::Variant {
                label,
                payload: Self::sub(payload, env),
            },
            Val::Variable(_) => unreachable!("a value closure's root is not a variable"),
        }
    }
    /// `()`, which is also a bare tag's payload.
    pub fn is_unit(self) -> bool {
        matches!(self.0, Val::Unit)
    }
    /// Read a whole tag spine, outermost tag first, and the value under it.
    pub(crate) fn peel_variants(self) -> (Vec<&'a Name>, Self) {
        let mut labels = Vec::new();
        let mut at = self;
        while let Val::Variant { .. } = at.0 {
            let mut val = at.0;
            while let Val::Variant { label, payload } = val {
                labels.push(label);
                val = payload;
            }
            at = Self::sub(val, at.1);
        }
        (labels, at)
    }
    /// The builtin this value is, and the arguments its η residual still awaits.
    pub(crate) fn residual(self) -> Option<(&'a Arc<BuiltinEntry>, usize)> {
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
                let View::Builtin(entry) = Self::from(env.get(builtin)?).view() else {
                    return None;
                };
                Some((entry, remaining))
            }
            _ => None,
        }
    }
    /// A thunk whose body the checker reads: not a builtin's η residual.
    pub(crate) fn block(self) -> Option<(&'a Arc<Comp>, &'a Env)> {
        match self.view() {
            View::Thunk { comp, env } if self.residual().is_none() => Some((comp, env)),
            _ => None,
        }
    }
    /// The one classification: a builtin is classified as its η-expansion,
    /// so arity 0 is a block and arity ≥ 1 a function.
    pub fn shape(self) -> ValueShape {
        match self.0 {
            Val::Unit => ValueShape::Unit,
            Val::Bool(_) => ValueShape::Bool,
            Val::Int(_) => ValueShape::Int,
            Val::Float(_) => ValueShape::Float,
            Val::String(_) => ValueShape::String,
            Val::Bytes(_) => ValueShape::Bytes,
            Val::List(_) => ValueShape::List,
            Val::Record(_) | Val::Map(_) => ValueShape::Map,
            Val::Variant { .. } => ValueShape::Variant,
            Val::Thunk(node) if node.shape().lam().is_none() => ValueShape::Block,
            Val::Builtin(entry) if entry.decl.fixed_arity() == 0 => ValueShape::Block,
            Val::Thunk(_) | Val::Builtin(_) => ValueShape::Function,
            Val::Handle(_) => ValueShape::Handle,
            Val::Variable(_) => unreachable!("a value closure's root is not a variable"),
        }
    }
    pub fn found(self) -> Found {
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
            View::Thunk { .. } | View::Builtin(_) => match self.shape() {
                ValueShape::Block => Found::Block,
                _ => Found::Function,
            },
            View::Handle(_) => Found::Handle,
        }
    }
    pub fn as_int(self) -> Option<i64> {
        match self.view() {
            View::Int(n) => Some(n),
            _ => None,
        }
    }
    pub(crate) fn as_float(self) -> Option<f64> {
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
    pub(crate) fn heads(self) -> &'static [Head] {
        match self.shape() {
            ValueShape::Unit => &[Head::Unit],
            ValueShape::Bool => &[Head::Bool],
            ValueShape::Int => &[Head::Int],
            ValueShape::Float => &[Head::Float],
            ValueShape::String => &[Head::String],
            ValueShape::Bytes => &[Head::Bytes],
            ValueShape::List => &[Head::List],
            ValueShape::Map => &[Head::Map, Head::Record],
            ValueShape::Variant => &[Head::Variant],
            ValueShape::Block | ValueShape::Function => &[Head::Thunk],
            ValueShape::Handle => &[Head::Handle],
        }
    }
    pub fn scalar_text(self) -> Option<String> {
        matches!(
            self.view(),
            View::String(_) | View::Int(_) | View::Float(_) | View::Bool(_)
        )
        .then(|| self.to_string())
    }
    pub fn type_name(self) -> &'static str {
        match self.shape() {
            ValueShape::Unit => "Unit",
            ValueShape::Bool => "Bool",
            ValueShape::Int => "Int",
            ValueShape::Float => "Float",
            ValueShape::String => "String",
            ValueShape::Bytes => "Bytes",
            ValueShape::List => "List",
            ValueShape::Map => "Map",
            ValueShape::Variant => "Variant",
            ValueShape::Block => "Block",
            ValueShape::Function => "Lambda",
            ValueShape::Handle => "Handle",
        }
    }
    /// Estimate data bytes; never descends a capture or a handle's buffers.
    pub(crate) fn shallow_size(self) -> usize {
        let (labels, terminal) = self.peel_variants();
        labels.into_iter().map(|label| label.len()).sum::<usize>()
            + match terminal.view() {
                View::Unit => 0,
                View::Bool(_) => 1,
                View::Int(_) | View::Float(_) => 8,
                View::String(s) => s.len(),
                View::Bytes(b) => b.len(),
                View::List(items) => items.iter().map(Self::shallow_size).sum(),
                View::Map(entries) => entries
                    .iter()
                    .map(|(k, v)| k.len() + v.shallow_size())
                    .sum(),
                View::Variant { .. } => unreachable!("the tag spine was peeled above"),
                View::Thunk { .. } | View::Builtin(_) | View::Handle(_) => 32,
            }
    }
}

/// Structural first-order equality: a block, function or handle equals nothing.
/// ral's `==` refuses them instead.
impl PartialEq for VClosRef<'_> {
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
impl PartialEq for ValClos {
    fn eq(&self, other: &Self) -> bool {
        VClosRef::from(self) == VClosRef::from(other)
    }
}

impl fmt::Display for VClosRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (labels, terminal) = self.peel_variants();
        if !labels.is_empty() {
            f.write_str(&tag_run(labels))?;
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
                    write!(f, "{v}")?;
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
                    write!(f, "{k}: {v}")?;
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
impl fmt::Display for ValClos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        VClosRef::from(self).fmt(f)
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
                .map(|(name, field)| ((*name).to_owned(), field.value(terminal))),
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
/// `n` lists, each holding the one before.
#[cfg(test)]
pub(crate) fn deep_list_chain(n: usize, foot: ValClos) -> ValClos {
    (0..n).fold(foot, |inner, _| ValClos::list(vec![inner]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Spanned;

    fn on_small_stack(work: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(work)
            .expect("spawn")
            .join()
            .expect("the work finishes without exhausting the stack");
    }
    #[test]
    fn closure_checks_direct_names_in_order_and_keeps_the_given_env() {
        let env = Env::from_bindings(vec![
            (
                "bound".into(),
                Binding {
                    value: ValClos::int(7),
                    scheme: None,
                },
            ),
            (
                "unused".into(),
                Binding {
                    value: ValClos::UNIT,
                    scheme: None,
                },
            ),
        ])
        .unwrap();
        let foot = Val::list(vec![
            Spanned::synthetic(Val::thunk(Arc::new(Spanned::synthetic(CompKind::Return(
                Val::Variable("inside".into()),
            ))))),
            Spanned::synthetic(Val::Variable("bound".into())),
        ]);
        let value =
            ValClos::closure(vec!["outer".into(), "inner".into()], foot, env.clone()).unwrap();
        let (Val::Variant { label, payload }, held_env) = value.parts() else {
            panic!("a tagged syntax closure")
        };
        assert_eq!(label.as_ref(), "outer");
        assert!(
            matches!(payload.as_ref(), Val::Variant { label, .. } if label.as_ref() == "inner")
        );
        assert!(held_env.ptr_eq(&env));
        assert_eq!(
            ValClos::closure(vec![], Val::Variable("bound".into()), env.clone()).unwrap(),
            ValClos::int(7),
        );
        let foot = Val::record(vec![(
            "key".into(),
            Spanned::synthetic(Val::list(vec![
                Spanned::synthetic(Val::Variant {
                    label: "tag".into(),
                    payload: Box::new(Val::Variable("first".into())),
                }),
                Spanned::synthetic(Val::Variable("second".into())),
            ])),
        )]);
        assert_eq!(
            ValClos::closure(vec![], foot, env).unwrap_err().as_ref(),
            "first",
        );
    }
    #[test]
    fn a_tag_binds_a_residual_instead_of_embedding_its_syntax() {
        let mut shell = crate::test_helper::core_shell();
        let entry = Arc::new(shell.lookup_builtin("has").unwrap());
        let partial = crate::shell::evaluator::apply(
            &ValClos::builtin(Arc::clone(&entry)),
            vec![ValClos::string("key")],
            &crate::shell::runtime::Mooring::adrift(),
            &mut shell,
        )
        .unwrap();
        let tagged = ValClos::variant("ok", Some(partial));
        let (Val::Variant { payload, .. }, env) = tagged.parts() else {
            panic!("a tagged syntax closure")
        };
        let Val::Variable(name) = payload.as_ref() else {
            panic!("a residual is bound, not embedded")
        };
        let (held_entry, remaining) = env.get(name).unwrap().residual().unwrap();
        assert!(Arc::ptr_eq(held_entry, &entry));
        assert_eq!(remaining, entry.decl.fixed_arity() - 1);
    }
    #[test]
    fn sub_resolves_a_variable_in_one_lookup_and_borrows_the_binding() {
        let bound = ValClos::list(vec![ValClos::int(1)]);
        let mut env = Env::EMPTY;
        env.bind(
            "x".into(),
            Binding {
                value: bound,
                scheme: None,
            },
        );
        let variable = Val::Variable("x".into());
        let found = VClosRef::sub(&variable, &env);
        let (val, _) = env.get("x").unwrap().parts();
        assert!(std::ptr::eq(found.0, val));
        let literal = Val::Int(3);
        assert!(std::ptr::eq(
            VClosRef::sub(&literal, &env).0,
            &raw const literal
        ));
    }
    #[test]
    fn a_variants_view_borrows_its_payload_without_forming_a_closure() {
        let item = ValClos::withheld("h");
        let tagged = ValClos::variant("ok", Some(item));
        let (_, env) = tagged.parts();
        let View::Variant { label, payload } = tagged.view() else {
            panic!("a tag")
        };
        let bound = env.get("%payload0").unwrap();
        assert_eq!(label.as_ref(), "ok");
        assert!(std::ptr::eq(payload.0, bound.parts().0));
        assert!(matches!(
            ValClos::variant("bare", None).view(),
            View::Variant { payload, .. } if matches!(payload.view(), View::Unit)
        ));
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
    fn an_owned_subterm_shares_the_capture_unless_it_mentions_nothing() {
        let mut env = Env::EMPTY;
        env.bind(
            "a".into(),
            Binding {
                value: ValClos::int(1),
                scheme: None,
            },
        );
        let thunk = Val::thunk(Arc::new(Spanned::synthetic(CompKind::Return(
            Val::Variable("a".into()),
        ))));
        let literal = ValClos::from_parts(
            Val::list([thunk, Val::Int(2)].map(Spanned::synthetic)),
            env.clone(),
        );
        let View::List(items) = literal.view() else {
            panic!("a list")
        };
        assert!(items.get(0).unwrap().into_owned().parts().1.ptr_eq(&env));
        assert!(items.get(1).unwrap().into_owned().parts().1.is_empty());
    }
    #[test]
    fn a_deep_list_chain_drops_on_a_small_stack() {
        on_small_stack(|| drop(deep_list_chain(1_000_000, ValClos::UNIT)));
    }
    #[test]
    fn deeply_nested_source_literals_drop_on_a_small_stack() {
        on_small_stack(|| {
            for nest in [
                |inner| Val::list([Spanned::synthetic(inner)]),
                |inner| Val::map([("k".into(), Spanned::synthetic(inner))]),
            ] {
                drop(ValClos::from_parts(
                    (0..100_000).fold(Val::Unit, |inner, _| nest(inner)),
                    Env::EMPTY,
                ));
            }
        });
    }
    #[test]
    fn a_deep_list_chain_over_a_handle_drops_on_a_small_stack() {
        on_small_stack(|| drop(deep_list_chain(100_000, ValClos::withheld("h"))));
    }
    #[test]
    fn a_deep_tag_chain_drops_on_a_small_stack() {
        on_small_stack(|| {
            drop((0..1_000_000).fold(ValClos::UNIT, |inner, _| ValClos::variant("t", Some(inner))));
        });
    }
    #[test]
    fn a_deep_map_chain_drops_on_a_small_stack() {
        on_small_stack(|| {
            drop((0..100_000).fold(ValClos::UNIT, |prev, _| {
                ValClos::map(vec![("k".into(), prev)])
            }));
        });
    }
    #[test]
    fn a_list_holding_a_deep_tag_chain_clones_and_drops_on_a_small_stack() {
        on_small_stack(|| {
            let chain =
                (0..100_000).fold(ValClos::UNIT, |inner, _| ValClos::variant("t", Some(inner)));
            let list = ValClos::list(vec![chain]);
            drop(list.clone());
            drop(list);
        });
    }
}
