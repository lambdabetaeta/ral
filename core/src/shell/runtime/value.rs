//! Runtime values: what a variable holds, what a pipeline stage produces,
//! what a builtin returns.

use super::builtin::BuiltinEntry;
use super::closure::Closure;
#[cfg(test)]
use super::env::Binding;
#[cfg(test)]
use super::env::Env;
use super::handle::HandleInner;
use super::list::List;
use super::map::Map;
use crate::data::Bytes;
use crate::data::Finite;
use crate::data::Str;
use crate::fact::{Found, TextPreview, ValueShape};
use crate::io::terminal::{InteractiveMode, TerminalState};
use crate::ty::{Head, TAG_PREFIX};
use std::fmt;
use std::ops::ControlFlow;
use std::sync::Arc;

mod compare;
mod first_order;
mod seed;
pub(super) use seed::{drop_seed_env, drop_seed_value};

/// The runtime representation of every ral value.
///
/// There is one thunk value, `Thunk(Closure)`: a computation closed over the
/// bindings it mentions. `{ |params| body }` and `{ body }` are one
/// variant: `apply` and the machine's `force` rule never ask which, and only a
/// description of a value reads the shape (`Comp::lam`).
///
/// Cloning copies no payload — `Str`, `Bytes`, `List`, `Map` and a closure's
/// scope are all shared — and `Env` and variable lookup rely on it.
#[derive(Debug, Clone)]
pub enum Value {
    Unit,
    Bool(bool),
    Int(i64),
    Float(Finite),
    String(Str),
    Bytes(Bytes),
    List(List),
    Map(Map),
    /// `label` is stored without its leading backtick; `Display` puts it back.
    Variant {
        label: crate::ir::Name,
        payload: Option<Box<Self>>,
    },
    Thunk(Closure),
    /// A builtin, `thunk (λx₁…λxₙ. Native(b, x₁…xₙ))`: forcing it builds that
    /// code, so a partial application is a λ closure like any other.
    Native(Arc<BuiltinEntry>),
    /// A computation spawned onto a worker thread, not a subprocess.
    /// Boxed: `HandleInner`'s eight `Arc` fields would otherwise be the
    /// largest variant, inflating every `Value` move (`Focus`'s hot path
    /// included) to its size.
    Handle(Box<HandleInner>),
}

const _: () = assert!(
    std::mem::size_of::<Value>() <= 32,
    "Value must fit in 32 bytes"
);

impl Value {
    /// Release seed-created data and captures without recursive drop glue.
    pub(crate) fn drop_seed(self) {
        drop_seed_value(self);
    }

    /// An inert head summary; closures are never traversed.
    pub fn shape(&self) -> ValueShape {
        match self {
            Self::Unit => ValueShape::Unit,
            Self::Bool(_) => ValueShape::Bool,
            Self::Int(_) => ValueShape::Int,
            Self::Float(_) => ValueShape::Float,
            Self::String(_) => ValueShape::String,
            Self::Bytes(_) => ValueShape::Bytes,
            Self::List(_) => ValueShape::List,
            Self::Map(_) => ValueShape::Map,
            Self::Variant { .. } => ValueShape::Variant,
            Self::Thunk(c) if c.comp().lam().is_none() => ValueShape::Block,
            Self::Thunk(_) | Self::Native(_) => ValueShape::Function,
            Self::Handle(_) => ValueShape::Handle,
        }
    }

    /// Bounded evidence for a refusal, independent of runtime resources.
    pub fn found(&self) -> Found {
        match self {
            Self::Unit => Found::Unit,
            Self::Bool(b) => Found::Bool(*b),
            Self::Int(n) => Found::Int(*n),
            Self::Float(f) => Found::Float(*f),
            Self::String(s) => Found::Text(TextPreview::new(s)),
            Self::Bytes(_) => Found::Bytes,
            Self::List(_) => Found::List,
            Self::Map(_) => Found::Map,
            Self::Variant { label, .. } => Found::Variant(label.to_string()),
            Self::Thunk(c) if c.comp().lam().is_none() => Found::Block,
            Self::Thunk(_) | Self::Native(_) => Found::Function,
            Self::Handle(_) => Found::Handle,
        }
    }

    /// `Int` only: the checker keeps `Int` and `Float` apart, and a
    /// numeric-looking string is the `int` builtin's job.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// `Int` and `Float` only; strings go through the `float` builtin.
    pub(crate) fn as_float(&self) -> Option<f64> {
        match self {
            #[allow(
                clippy::cast_precision_loss,
                reason = "Int→Float coercion; loss beyond 2^53 is intrinsic to representing i64 in an f64 mantissa"
            )]
            Self::Int(n) => Some(*n as f64),
            Self::Float(f) => Some(f.get()),
            _ => None,
        }
    }

    /// Every list-construction site goes through here, so the persistent-vector
    /// wrapping stays invisible to callers.
    pub fn list(items: Vec<Self>) -> Self {
        Self::List(items.into())
    }

    /// A tagged value; `Unit` is represented by an absent payload.
    pub fn variant(label: impl Into<crate::ir::Name>, payload: Option<Self>) -> Self {
        Self::Variant {
            label: label.into(),
            payload: payload.filter(|p| !matches!(p, Self::Unit)).map(Box::new),
        }
    }

    /// Every string-construction site goes through here, so the shared buffer
    /// stays invisible to callers.
    pub fn string(s: impl Into<Str>) -> Self {
        Self::String(s.into())
    }

    /// Every bytes-construction site goes through here, so the shared buffer
    /// stays invisible to callers.
    pub fn bytes(b: impl Into<Bytes>) -> Self {
        Self::Bytes(b.into())
    }

    /// The heads this value may be admitted at: a map is a `Map` or a
    /// `Record`, which the runtime does not tell apart.
    pub(crate) fn heads(&self) -> &'static [Head] {
        match self {
            Self::Unit => &[Head::Unit],
            Self::Bool(_) => &[Head::Bool],
            Self::Int(_) => &[Head::Int],
            Self::Float(_) => &[Head::Float],
            Self::String(_) => &[Head::String],
            Self::Bytes(_) => &[Head::Bytes],
            Self::List(_) => &[Head::List],
            Self::Map(_) => &[Head::Map, Head::Record],
            Self::Variant { .. } => &[Head::Variant],
            Self::Thunk(_) | Self::Native(_) => &[Head::Thunk],
            Self::Handle(_) => &[Head::Handle],
        }
    }

    /// The text of a scalar (`String`, `Int`, `Float`, `Bool`), the one
    /// conversion an interpolation, an environment override and a printer share.
    pub fn scalar_text(&self) -> Option<String> {
        matches!(
            self,
            Self::String(_) | Self::Int(_) | Self::Float(_) | Self::Bool(_)
        )
        .then(|| self.to_string())
    }

    /// Render an argv: every element through the total text conversion
    /// [`Display`](fmt::Display) performs and `str` exposes.
    ///
    /// The one rendering every argv boundary *inside* the shell shares —
    /// `echo`'s write, a handler arm's argument list, the audit trail's record
    /// of a call — so an argv is a list of strings wherever it is read.  It is
    /// total on purpose, and so is unlike the exec boundary, which refuses the
    /// shapes [`crate::ty::RefusedArg`] names because it is heading for `execve(2)`:
    /// total inside, gated at the OS call.
    pub(crate) fn render_argv(args: &[Self]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    /// The record a program reads of `fact`; `raise` recovers `fact` from it.
    pub fn failure(fact: crate::fact::ErrorFact) -> Self {
        Self::Map(Map::witnessed(fact))
    }

    /// On duplicate keys the *last* pair wins; a caller needing first-wins
    /// dedups beforehand, as `eval_map` does to keep explicit entries ahead
    /// of spreads.
    pub fn map(pairs: Vec<(String, Self)>) -> Self {
        Self::Map(pairs.into())
    }

    /// Visit each direct component of data: a list's elements, a record's
    /// values, a variant's payload.  A closure has none: its capture is never
    /// entered.
    pub(crate) fn try_for_each_child<B>(
        &self,
        f: &mut impl FnMut(&Self) -> ControlFlow<B>,
    ) -> ControlFlow<B> {
        match self {
            Self::List(items) => items.iter().try_for_each(|v| f(&v)),
            Self::Map(pairs) => pairs.iter().try_for_each(|(_, v)| f(&v)),
            Self::Variant { payload, .. } => payload.iter().try_for_each(|p| f(p)),
            Self::Unit
            | Self::Bool(_)
            | Self::Int(_)
            | Self::Float(_)
            | Self::String(_)
            | Self::Bytes(_)
            | Self::Thunk(_)
            | Self::Native(_)
            | Self::Handle(_) => ControlFlow::Continue(()),
        }
    }

    /// The name a diagnostic calls this value.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Unit => "Unit",
            Self::Bool(_) => "Bool",
            Self::Int(_) => "Int",
            Self::Float(_) => "Float",
            Self::String(_) => "String",
            Self::Bytes(_) => "Bytes",
            Self::List(_) => "List",
            Self::Map(_) => "Map",
            Self::Variant { .. } => "Variant",
            Self::Thunk(c) if c.comp().lam().is_some() => "Lambda",
            Self::Thunk(_) => "Block",
            Self::Native(_) => "Native",
            Self::Handle(_) => "Handle",
        }
    }

    /// A *shallow* size estimate in bytes, weighed against
    /// `BindingLease::large_binding_bytes` whenever a session-scope name is
    /// installed, so a heavy binding earns a residency nudge. Exact for
    /// `String`/`Bytes`, and recursive through the *elements* of
    /// `List`/`Map`/`Variant`, so a large collection of small values is
    /// counted honestly. `Lambda`, `Block`, `Native` and `Handle` are never
    /// descended: chasing a closure's captured `Env` or a handle's buffers is
    /// the retained-size walk this design refuses throughout, `pins_live_work` in
    /// `shell/runtime/handle.rs` refusing it identically. A nudge, then, not an
    /// account — structure shared under `Arc` counts twice, captures not at all.
    pub(crate) fn shallow_size(&self) -> usize {
        /// Small and fixed rather than zero, so a binding full of closures
        /// still moves the estimate without pretending to measure captures.
        const OPAQUE_CONSTANT: usize = 32;
        match self {
            Self::Unit => 0,
            Self::Bool(_) => 1,
            Self::Int(_) | Self::Float(_) => 8,
            Self::String(s) => s.len(),
            Self::Bytes(b) => b.len(),
            Self::List(items) => items.iter().map(|v| v.shallow_size()).sum(),
            Self::Map(pairs) => pairs.iter().map(|(k, v)| k.len() + v.shallow_size()).sum(),
            Self::Variant { label, payload } => {
                label.len() + payload.as_deref().map_or(0, Self::shallow_size)
            }
            Self::Thunk(_) | Self::Native(_) | Self::Handle(_) => OPAQUE_CONSTANT,
        }
    }
}

/// Representational equality, the tests' oracle; ral's own `==` is [`Value::equals`].
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Unit, Self::Unit) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a == b,
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Bytes(a), Self::Bytes(b)) => a == b,
            (Self::List(a), Self::List(b)) => a == b,
            (Self::Map(a), Self::Map(b)) => a == b,
            (
                Self::Variant {
                    label: la,
                    payload: pa,
                },
                Self::Variant {
                    label: lb,
                    payload: pb,
                },
            ) => la == lb && pa == pb,
            // Suspensions are never structurally equal.
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unit => write!(f, "()"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Int(n) => write!(f, "{n}"),
            Self::Float(n) => write!(f, "{n}"),
            Self::String(s) => write!(f, "{s}"),
            Self::Bytes(b) => write!(f, "{}", String::from_utf8_lossy(b)),
            Self::List(items) => {
                if items.is_empty() {
                    return write!(f, "[]");
                }
                write!(f, "[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", v.as_ref())?;
                }
                write!(f, "]")
            }
            Self::Map(m) => {
                if m.is_empty() {
                    return write!(f, "[:]");
                }
                write!(f, "[")?;
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {}", v.as_ref())?;
                }
                write!(f, "]")
            }
            Self::Variant { label, payload } => match payload {
                None => write!(f, "{TAG_PREFIX}{label}"),
                Some(p) => write!(f, "{TAG_PREFIX}{label} {p}"),
            },
            Self::Thunk(c) => match c.comp().lam() {
                Some((param, body)) => write!(f, "{}", fmt_lambda(param, body)),
                None => write!(f, "<block>"),
            },
            Self::Native(entry) => write!(f, "<native {}>", entry.decl.name),
            Self::Handle(h) => write!(f, "<handle:{}>", h.title),
        }
    }
}

/// Format as `<|a b ...| block>`, flattening the curried `Lam` chain.
fn fmt_lambda(param: &crate::ir::Name, body: &crate::ir::Comp) -> String {
    use crate::ir::{CompKind, Val, is_synthetic};

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

/// The `$TERMINAL` map bound for RC files and plugins.  Scripts pattern-match
/// these keys: adding one is safe, renaming or removing one breaks them.
impl From<&TerminalState> for Value {
    fn from(t: &TerminalState) -> Self {
        let mode = match t.mode {
            InteractiveMode::Auto => "auto",
            InteractiveMode::Minimal => "minimal",
            InteractiveMode::Full => "full",
        };
        let flags = [
            ("stdin-tty", t.startup_stdin_tty),
            ("stdout-tty", t.startup_stdout_tty),
            ("stderr-tty", t.startup_stderr_tty),
            ("supports-ansi", t.supports_ansi),
            ("no-color", t.no_color),
            ("is-tmux", t.is_tmux),
            ("is-asciinema", t.is_asciinema),
            ("is-ci", t.is_ci),
            ("truecolor", t.truecolor),
            ("hyperlinks", t.hyperlinks()),
            ("clipboard-write", t.clipboard_write()),
            ("bracketed-paste", t.bracketed_paste()),
            ("ui-ansi-ok", t.ui_ansi_ok()),
            ("ui-truecolor-ok", t.ui_truecolor_ok()),
            ("ui-hyperlinks-ok", t.ui_hyperlinks_ok()),
            ("ui-clipboard-write-ok", t.ui_clipboard_write_ok()),
            ("ui-bracketed-paste-ok", t.ui_bracketed_paste_ok()),
        ];
        Self::map(
            flags
                .into_iter()
                .map(|(k, b)| (k.into(), Self::Bool(b)))
                .chain([("mode".into(), Self::string(mode))])
                .collect(),
        )
    }
}

/// A block mentioning every session name of `env`, so its capture is `env`
/// itself.
#[cfg(test)]
pub(crate) fn block_over(env: &Env) -> Value {
    use crate::ir::{CompKind, ThunkNode, Val};
    use crate::source::Spanned;
    let names = env
        .names()
        .map(|n| Spanned::synthetic(Val::Variable(n.clone())))
        .collect::<Vec<_>>();
    let body = Spanned::synthetic(CompKind::Return(Val::list(names)));
    let node = ThunkNode::new(Arc::new(body));
    Value::Thunk(Closure::new(Arc::clone(node.shape()), node.occ(), env))
}

/// The scope a block `v` captured.
#[cfg(test)]
pub(crate) fn captured(v: Option<&Value>) -> &Env {
    match v {
        Some(Value::Thunk(closure)) => closure.env(),
        other => panic!("expected a block, got {other:?}"),
    }
}

/// A chain of `n` blocks over `foot`, each capturing the next in a one-binding
/// env — the skeleton of a user-built lazy list.  Fixture for the walks that
/// cross the captured-env seam once per link: the seed encoder, the fork's
/// scrub, and `Env`'s drop.
#[cfg(test)]
pub(crate) fn deep_block_chain(n: usize, foot: Value) -> Value {
    let body = Arc::new(crate::source::Spanned::synthetic(
        crate::ir::CompKind::Return(crate::ir::Val::Variable("tail".into())),
    ));
    let node = crate::ir::ThunkNode::new(Arc::clone(&body));
    let mut v = foot;
    for _ in 0..n {
        let mut env = Env::new();
        env.bind(
            "tail".into(),
            Binding {
                value: v,
                scheme: None,
            },
        );
        v = Value::Thunk(Closure::new(Arc::clone(&body), node.occ(), &env));
    }
    v
}

/// A chain of `n` one-element list literals over `foot`, each capturing the
/// next in a one-binding env — `acc = [$acc]`, run by a recursive function.
/// The literal repr's captured `Env` must be cut by the same
/// trampoline `Closure`'s drop uses, or a deep chain overflows the stack.
#[cfg(test)]
pub(crate) fn deep_list_chain(n: usize, foot: Value) -> Value {
    let node = crate::ir::ListNode::new(Box::from([crate::source::Spanned::synthetic(
        crate::ir::Val::Variable("acc".into()),
    )]));
    let mut v = foot;
    for _ in 0..n {
        let mut env = Env::new();
        env.bind(
            "acc".into(),
            Binding {
                value: v,
                scheme: None,
            },
        );
        v = Value::List(List::literal(&node, &env));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unit_variant_payload_is_absent() {
        assert!(matches!(
            Value::variant("none", Some(Value::Unit)),
            Value::Variant { payload: None, .. }
        ));
    }

    #[test]
    fn lambda_display_recovers_patterns_and_hides_synthetic_names() {
        use crate::ir::{CompKind, Phrase, Val};

        for (src, expected, partial) in [
            (
                "{ |[a, b] [c] x| () }",
                "<|[a b] [c] x| block>",
                Some("<|[c] x| block>"),
            ),
            ("{ |[x] x| () }", "<|[x] x| block>", Some("<|x| block>")),
            ("{ |_| () }", "<|_| block>", None),
        ] {
            let ast = crate::syntax::parser::parse(src).expect("parse");
            let top = crate::elaborator::elaborate(&ast, [], None).expect("elaborate");
            let Phrase::Run(comp) = &top[0].item else {
                panic!("expected a run phrase: {top:?}");
            };
            let CompKind::Return(Val::Thunk(node)) = &comp.item else {
                panic!("expected a lambda thunk: {comp:?}");
            };
            let (param, body) = node.shape().lam().expect("outer lambda");
            assert_eq!(fmt_lambda(param, body), expected);
            if let Some(expected) = partial {
                let (param, body) = body.lam().expect("inner lambda");
                assert_eq!(fmt_lambda(param, body), expected);
            }
        }
        assert_eq!(
            fmt_lambda(
                &crate::ir::synthetic("param", 0),
                &crate::source::Spanned::synthetic(CompKind::Return(Val::Unit)),
            ),
            "<|_| block>",
        );
    }

    #[test]
    fn shallow_size_counts_nested_lists_and_maps() {
        assert_eq!(Value::Unit.shallow_size(), 0);
        assert_eq!(Value::string("hello").shallow_size(), 5);
        assert_eq!(Value::bytes(vec![0u8; 10]).shallow_size(), 10);

        let flat = Value::list(vec![Value::string("ab"), Value::string("cde")]);
        assert_eq!(flat.shallow_size(), 2 + 3);

        let nested = Value::list(vec![flat.clone(), flat]);
        assert_eq!(nested.shallow_size(), 2 * (2 + 3));

        // Keys' bytes count alongside their values' estimates.
        let map = Value::map(vec![
            ("k1".to_string(), Value::string("v1")),
            ("k22".to_string(), Value::string("v2345")),
        ]);
        assert_eq!(
            map.shallow_size(),
            ("k1".len() + "v1".len()) + ("k22".len() + "v2345".len())
        );

        let map_of_lists = Value::map(vec![("k".to_string(), nested.clone())]);
        assert_eq!(
            map_of_lists.shallow_size(),
            "k".len() + nested.shallow_size()
        );

        let variant = Value::Variant {
            label: "tag".into(),
            payload: Some(Box::new(Value::string("payload"))),
        };
        assert_eq!(variant.shallow_size(), "tag".len() + "payload".len());
        let bare_variant = Value::Variant {
            label: "bare".into(),
            payload: None,
        };
        assert_eq!(bare_variant.shallow_size(), "bare".len());
    }

    /// Captures are invisible by construction, not merely usually small.
    #[test]
    fn shallow_size_never_descends_into_closure_captures() {
        let block = block_over(&Env::new());
        let block_size = block.shallow_size();
        assert!(block_size > 0, "a closure is a small nonzero constant");

        let mut heavy_env = Env::new();
        heavy_env.bind(
            "heavy".into(),
            Binding {
                value: Value::string("x".repeat(10_000)),
                scheme: None,
            },
        );
        let heavy_block = block_over(&heavy_env);
        assert_eq!(
            heavy_block.shallow_size(),
            block_size,
            "a closure's captured scope must never affect the estimate"
        );

        let list_of_one_closure = Value::list(vec![block]);
        assert_eq!(list_of_one_closure.shallow_size(), block_size);
    }

    #[test]
    fn a_cloned_string_shares_its_allocation() {
        let v = Value::string("x".repeat(1 << 20));
        let clone = v.clone();
        let (Value::String(a), Value::String(b)) = (&v, &clone) else {
            panic!("a string clones as a string");
        };
        assert_eq!(a.as_ptr(), b.as_ptr(), "a clone must not copy the bytes");
    }

    /// A 10⁵-deep `acc = [$acc]` chain drops on a 256 KiB thread:
    /// the literal repr's captured `Env` is cut by `Env::dismantle`'s
    /// trampoline, the same one `Closure`'s drop uses, even though the chain
    /// never passes through a `Closure`.
    #[test]
    fn a_deep_literal_chain_drops_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| drop(deep_list_chain(100_000, Value::Unit)))
            .expect("spawn")
            .join()
            .expect("a deep literal chain must drop without exhausting the stack");
    }
    #[test]
    fn reason_summaries_distinguish_blocks_functions_and_data_heads() {
        let block = block_over(&Env::new());
        assert_eq!(
            (block.shape(), block.found()),
            (ValueShape::Block, Found::Block)
        );
        for (value, shape, found) in [
            (Value::Unit, ValueShape::Unit, Found::Unit),
            (Value::Bool(true), ValueShape::Bool, Found::Bool(true)),
            (Value::Int(i64::MIN), ValueShape::Int, Found::Int(i64::MIN)),
            (
                Value::Float(Finite::from(2_i64)),
                ValueShape::Float,
                Found::Float(Finite::from(2_i64)),
            ),
            (Value::bytes(vec![0xff]), ValueShape::Bytes, Found::Bytes),
            (Value::list(vec![block]), ValueShape::List, Found::List),
            (Value::map(vec![]), ValueShape::Map, Found::Map),
            (
                Value::variant("label", None),
                ValueShape::Variant,
                Found::Variant("label".into()),
            ),
        ] {
            assert_eq!(value.shape(), shape);
            assert_eq!(value.found(), found);
        }
        assert_eq!(
            Value::string("x".repeat(24)).found(),
            Found::Text(TextPreview {
                preview: "x".repeat(24),
                truncated: false,
            })
        );
    }
    #[test]
    fn opaque_previews_never_retain_live_handles_or_closure_captures() {
        let handle = crate::shell::runtime::idle_handle();
        assert_eq!(handle.shape(), ValueShape::Handle);
        assert_eq!(handle.found(), Found::Handle);
        let mut shell = crate::test_helper::core_shell();
        let function =
            crate::shell::evaluator::run_source("{ |x| return $x }", &mut shell).unwrap();
        assert_eq!(function.shape(), ValueShape::Function);
        assert_eq!(function.found(), Found::Function);
    }
}
