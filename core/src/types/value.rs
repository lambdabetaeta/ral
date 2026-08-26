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
use crate::syntax::tag::TAG_PREFIX;
use std::fmt;
use std::sync::Arc;

/// The runtime representation of every ral value.
///
/// There is one thunk value, `Thunk(Closure)`: a computation closed over the
/// environment it was captured against. `{ |params| body }` and `{ body }`
/// are told apart only by the closure's own comp shape —
/// `Comp::arrow` answers `Some` for a `Lam`, so `apply` and `step_force` read
/// the shape rather than a separate variant (S10, the CEK plan §1.1).
#[derive(Debug, Clone)]
pub enum Value {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(std::string::String),
    Bytes(Vec<u8>),
    List(List),
    Map(Map),
    /// `label` is stored without its leading backtick; `Display` puts it back.
    Variant {
        label: std::string::String,
        payload: Option<Box<Self>>,
    },
    Thunk(Closure),
    /// A builtin, curried through `applied`: applying appends arguments until
    /// `entry.fixed_arity()` is reached, then the host body runs with the
    /// full slice. Under-application yields the partial `Native` back;
    /// over-application is an arity error, mirroring a `Lambda`.
    Native {
        entry: Arc<BuiltinEntry>,
        applied: Vec<Self>,
    },
    /// A computation spawned onto a worker thread, not a subprocess.
    Handle(HandleInner),
}

impl Value {
    /// `Int` and whole `Float` only: a numeric-looking string is never
    /// silently parsed, since that is the `int` builtin's job.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(n) => Some(*n),
            #[allow(
                clippy::float_cmp,
                reason = "integral test: exact by construction, comparing f to its own floor"
            )]
            Self::Float(f)
                if *f == f.floor()
                    && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(f) =>
            {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "guard restricts f to integral values in [-2^63, 2^63); the cast is exact"
                )]
                Some(*f as i64)
            }
            _ => None,
        }
    }

    /// `Int` and `Float` only; strings go through the `float` builtin.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            #[allow(
                clippy::cast_precision_loss,
                reason = "Int→Float coercion; loss beyond 2^53 is intrinsic to representing i64 in an f64 mantissa"
            )]
            Self::Int(n) => Some(*n as f64),
            Self::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Every list-construction site goes through here, so the persistent-vector
    /// wrapping stays invisible to callers.
    pub fn list(items: Vec<Self>) -> Self {
        Self::List(items.into())
    }

    /// Render an argv: every element through the total text conversion
    /// [`Display`](fmt::Display) performs and `str` exposes.
    ///
    /// The one rendering every argv boundary *inside* the shell shares —
    /// `echo`'s write, a handler arm's argument list, the audit trail's record
    /// of a call — so an argv is a list of strings wherever it is read.  It is
    /// total on purpose, and so is unlike the exec boundary, which refuses the
    /// shapes [`super::RefusedArg`] names because it is heading for `execve(2)`:
    /// total inside, gated at the OS call.
    pub fn render_argv(args: &[Self]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    /// On duplicate keys the *last* pair wins; a caller needing first-wins
    /// dedups beforehand, as `eval_map` does to keep explicit entries ahead
    /// of spreads.
    pub fn map(pairs: Vec<(String, Self)>) -> Self {
        Self::Map(pairs.into())
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
            Self::Thunk(c) if c.comp.arrow().is_some() => "Lambda",
            Self::Thunk(_) => "Block",
            Self::Native { .. } => "Native",
            Self::Handle(_) => "Handle",
        }
    }

    /// How many arguments `apply` consumes before reaching the body: the outer
    /// `Lam` counts one, each nested `Lam` another. Being the operational
    /// arity, it is what `validate_handler_arity` in `types/handler.rs` checks
    /// a handler against at its install site. `None` for a `Thunk` whose
    /// `comp.arrow()` is `None` — a block, not a lambda.
    pub fn lambda_arity(&self) -> Option<usize> {
        let Self::Thunk(c) = self else {
            return None;
        };
        let (_, mut body) = c.comp.arrow()?;
        let mut arity = 1;
        while let crate::ir::CompKind::Lam { body: inner, .. } = &body.item {
            arity += 1;
            body = inner;
        }
        Some(arity)
    }

    /// A *shallow* size estimate in bytes, weighed against
    /// `BindingLease::large_binding_bytes` whenever a session-scope name is
    /// installed, so a heavy binding earns a residency nudge. Exact for
    /// `String`/`Bytes`, and recursive through the *elements* of
    /// `List`/`Map`/`Variant`, so a large collection of small values is
    /// counted honestly. `Lambda`, `Block`, and `Handle` are never descended:
    /// chasing a captured `Arc<Env>` or a handle's buffers is the retained-size
    /// walk this design refuses throughout, `pins_running_work` in
    /// `types/handle.rs` refusing it identically. A nudge, then, not an
    /// account — structure shared under `Arc` counts twice, captures not at all.
    pub fn shallow_size(&self) -> usize {
        /// Small and fixed rather than zero, so a binding full of closures
        /// still moves the estimate without pretending to measure captures.
        const OPAQUE_CONSTANT: usize = 32;
        match self {
            Self::Unit => 0,
            Self::Bool(_) => 1,
            Self::Int(_) | Self::Float(_) => 8,
            Self::String(s) => s.len(),
            Self::Bytes(b) => b.len(),
            Self::List(items) => items.iter().map(Self::shallow_size).sum(),
            Self::Map(pairs) => pairs.iter().map(|(k, v)| k.len() + v.shallow_size()).sum(),
            Self::Variant { label, payload } => {
                label.len() + payload.as_deref().map_or(0, Self::shallow_size)
            }
            // `entry` is opaque like a closure's capture; `applied` counts
            // like a list's elements.
            Self::Native { applied, .. } => {
                OPAQUE_CONSTANT + applied.iter().map(Self::shallow_size).sum::<usize>()
            }
            Self::Thunk(_) | Self::Handle(_) => OPAQUE_CONSTANT,
        }
    }
}

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
            // A name is an intensional identity a closure lacks, so natives
            // compare where closures never do.
            (
                Self::Native {
                    entry: ea,
                    applied: aa,
                },
                Self::Native {
                    entry: eb,
                    applied: ab,
                },
            ) => ea.name == eb.name && aa == ab,
            // Closures and handles are never structurally equal.
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
            Self::Float(n) => f.write_str(&fmt_float(*n)),
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
                    write!(f, "{v}")?;
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
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "]")
            }
            Self::Variant { label, payload } => match payload {
                None => write!(f, "{TAG_PREFIX}{label}"),
                Some(p) => write!(f, "{TAG_PREFIX}{label} {p}"),
            },
            Self::Thunk(c) => match c.comp.arrow() {
                Some((param, body)) => write!(f, "{}", fmt_lambda(param, body)),
                None => write!(f, "<block>"),
            },
            Self::Native { entry, applied } => write!(f, "{}", fmt_native(&entry.name, applied)),
            Self::Handle(h) => write!(f, "<handle:{}>", h.cmd),
        }
    }
}

fn fmt_param(p: &crate::ir::IrPattern) -> String {
    match p {
        crate::ir::IrPattern::Wildcard => "_".into(),
        crate::ir::IrPattern::Name(s) => s.clone(),
        crate::ir::IrPattern::List { elems, rest } => {
            let mut parts: Vec<String> = elems.iter().map(fmt_param).collect();
            if let Some(r) = rest {
                parts.push(format!("...{r}"));
            }
            format!("[{}]", parts.join(" "))
        }
        crate::ir::IrPattern::Map(entries) => {
            let parts: Vec<String> = entries
                .iter()
                .map(|entry| {
                    let label = entry.key.row_label();
                    let v = fmt_param(&entry.pattern);
                    if matches!(&entry.pattern, crate::ir::IrPattern::Name(n) if n == &label) {
                        label
                    } else {
                        format!("{label}: {v}")
                    }
                })
                .collect();
            format!("[{}]", parts.join(" "))
        }
    }
}

/// The one printed spelling of a `Float`.
///
/// ryu's shortest round trip, with the point restored to a bare exponent
/// mantissa (`1e300` → `1.0e300`) so the printer's image stays inside the
/// numeral grammar.  A `Float` is finite by construction; ryu's `NaN`/`inf`
/// are the honest last resort.
pub fn fmt_float(n: f64) -> String {
    let mut buf = ryu::Buffer::new();
    let s = buf.format(n);
    match s.split_once('e') {
        Some((mantissa, exp)) if !mantissa.contains('.') => format!("{mantissa}.0e{exp}"),
        _ => s.to_owned(),
    }
}

/// Format as `<native NAME>`, or `<native NAME +N>` for a partial with `N`
/// collected arguments.
pub fn fmt_native(name: &str, applied: &[Value]) -> String {
    if applied.is_empty() {
        format!("<native {name}>")
    } else {
        format!("<native {name} +{}>", applied.len())
    }
}

/// Format as `<|a b ...| block>`, flattening the curried `Lam` chain.
pub fn fmt_lambda(param: &crate::ir::IrPattern, body: &crate::ir::Comp) -> String {
    let mut params = vec![fmt_param(param)];
    let mut comp = body;
    while let crate::ir::CompKind::Lam { param, body } = &comp.item {
        params.push(fmt_param(param));
        comp = body;
    }
    format!("<|{}| block>", params.join(" "))
}

/// A chain of `n` blocks, each capturing the next in a one-binding env — the
/// skeleton of a `from-lines` stream.  Fixture for the two walks that cross
/// the captured-env seam once per link: the serial encoder and `Env`'s drop.
#[cfg(test)]
pub(crate) fn deep_block_chain(n: usize) -> Value {
    let body = Arc::new(crate::source::Spanned::synthetic(
        crate::ir::CompKind::Return(crate::ir::Val::Unit),
    ));
    let mut v = Value::Unit;
    for _ in 0..n {
        let mut env = Env::new();
        env.bind(
            "tail".into(),
            Binding {
                value: v,
                scheme: None,
            },
        );
        v = Value::Thunk(Closure {
            comp: Arc::clone(&body),
            env,
        });
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shallow_size_counts_nested_lists_and_maps() {
        assert_eq!(Value::Unit.shallow_size(), 0);
        assert_eq!(Value::String("hello".into()).shallow_size(), 5);
        assert_eq!(Value::Bytes(vec![0u8; 10]).shallow_size(), 10);

        let flat = Value::list(vec![
            Value::String("ab".into()),
            Value::String("cde".into()),
        ]);
        assert_eq!(flat.shallow_size(), 2 + 3);

        let nested = Value::list(vec![flat.clone(), flat]);
        assert_eq!(nested.shallow_size(), 2 * (2 + 3));

        // Keys' bytes count alongside their values' estimates.
        let map = Value::map(vec![
            ("k1".to_string(), Value::String("v1".into())),
            ("k22".to_string(), Value::String("v2345".into())),
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
            payload: Some(Box::new(Value::String("payload".into()))),
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
        let empty_env = crate::types::Env::new();
        let block = Value::Thunk(Closure {
            comp: std::sync::Arc::new(crate::source::Spanned::synthetic(
                crate::ir::CompKind::Return(crate::ir::Val::Unit),
            )),
            env: empty_env,
        });
        let block_size = block.shallow_size();
        assert!(block_size > 0, "a closure is a small nonzero constant");

        let mut heavy_env = crate::types::Env::new();
        heavy_env.bind(
            "heavy".into(),
            Binding {
                value: Value::String("x".repeat(10_000)),
                scheme: None,
            },
        );
        let heavy_block = Value::Thunk(Closure {
            comp: std::sync::Arc::new(crate::source::Spanned::synthetic(
                crate::ir::CompKind::Return(crate::ir::Val::Unit),
            )),
            env: heavy_env,
        });
        assert_eq!(
            heavy_block.shallow_size(),
            block_size,
            "a closure's captured scope must never affect the estimate"
        );

        let list_of_one_closure = Value::list(vec![block]);
        assert_eq!(list_of_one_closure.shallow_size(), block_size);
    }
}
