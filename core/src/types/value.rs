//! Runtime values.
//!
//! [`Value`] is the runtime representation of every ral value.
//! [`fmt_lambda`] renders a lambda as a compact human-readable string.

use super::env::Env;
use super::handle::HandleInner;
use super::list::List;
use super::map::Map;
use std::fmt;
use std::sync::Arc;

/// The runtime representation of every ral value.
///
/// The interpreter passes `Value` between computations; it is what a variable
/// holds, what a pipeline stage produces, and what a builtin returns.
///
/// `Lambda` is a first-class function value (the elaboration of
/// `{ |params| body }`); `Block` is a suspended nullary computation
/// (`{ body }`).  Both carry a captured scope snapshot.  The split
/// makes the calling discipline visible in the type: `apply` dispatches
/// on the variant rather than introspecting a comp body shape, and
/// `Force` always forces (it runs a `Block`; a `Lambda` is already a
/// value and is returned as-is — see [`crate::evaluator::comp::step_force`]).
///
/// `Map` is an opaque newtype around `imbl::OrdMap<String, Value>` (see
/// `types/map.rs`).  Keys iterate in sorted order, lookup is O(log n),
/// and structural equality is order-independent.
///
/// `Thunk::captured` is `Arc<Env>` so a `Value::clone` on a thunk is a
/// single refcount bump rather than a `Vec`-clone of the scope chain;
/// many closures sharing one capture site share one allocation.
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
    /// A variant: a constructor `` `label `` carrying an optional payload.
    /// The label is stored without its leading backtick; the `Display` impl
    /// prints it as `` `label `` for consistency with the surface syntax.
    Variant {
        label: std::string::String,
        payload: Option<Box<Self>>,
    },
    /// A first-class function value.  `body` is the lambda's inner
    /// computation (the result-producing comp after the parameter has
    /// been bound); for curried lambdas, `body.item` itself is
    /// `CompKind::Lam` and currying flattens through the elaborator.
    Lambda {
        param: crate::ir::IrPattern,
        body: std::sync::Arc<crate::ir::Comp>,
        captured: Arc<Env>,
    },
    /// A suspended nullary computation (`{ body }`).  Forcing it runs
    /// `body` under `captured`.
    Block {
        body: std::sync::Arc<crate::ir::Comp>,
        captured: Arc<Env>,
    },
    /// Handle to a spawned subprocess.
    Handle(HandleInner),
}

impl Value {
    /// Convert to i64 for arithmetic, if possible.
    ///
    /// Accepts `Int` and whole `Float` values only — strings are never
    /// silently parsed.  Use the `int` builtin for explicit conversion.
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

    /// Convert to f64 for arithmetic, if possible.
    ///
    /// Accepts `Int` and `Float` values only — strings are never silently
    /// parsed.  Use the `float` builtin for explicit conversion.
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

    /// Build a `Value::List` from an owned `Vec<Value>`.  Every list-construction
    /// site goes through this so the persistent-vector wrapping stays invisible
    /// to callers.  Sites that already hold a `List` use `Value::List(v)` directly.
    pub fn list(items: Vec<Self>) -> Self {
        Self::List(items.into())
    }

    /// Build a `Value::Map` from an owned `Vec<(String, Value)>`.  The pair-list
    /// representation is what every construction site naturally produces (literals,
    /// JSON, REPL config); this wraps it once into the persistent `Map`.  On
    /// duplicate keys the *last* pair wins — callers that need first-wins (e.g.
    /// `eval_map`'s explicit-before-spread priority) must dedup before calling.
    pub fn map(pairs: Vec<(String, Self)>) -> Self {
        Self::Map(pairs.into())
    }

    /// Human-readable runtime type name used in diagnostics.
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
            Self::Lambda { .. } => "Lambda",
            Self::Block { .. } => "Block",
            Self::Handle(_) => "Handle",
        }
    }

    /// Curry-chain depth of a lambda value — the number of arguments
    /// `apply` will consume — or `None` if this is not a lambda.
    ///
    /// The outer [`Value::Lambda`] counts as one; each nested
    /// [`crate::ir::CompKind::Lam`] reached through `body.item` adds
    /// another.  This is exactly the operational arity that
    /// [`crate::evaluator::apply`] consumes before reaching the body, so
    /// it is the principled arity to validate against at the install
    /// boundary.
    pub fn lambda_arity(&self) -> Option<usize> {
        let Self::Lambda { body, .. } = self else {
            return None;
        };
        let mut arity = 1;
        let mut comp = body;
        while let crate::ir::CompKind::Lam { body, .. } = &comp.item {
            arity += 1;
            comp = body;
        }
        Some(arity)
    }

    /// A structural *shallow* size estimate, in bytes — the binding-lease
    /// ledger's large-binding warning
    /// (`decisions/260629_agent-binding-reaping`,
    /// `decisions/260705_leases-and-budgets` §"Shell residency is lexical
    /// state plus host leases"). Exact for `String`/`Bytes` (their byte
    /// length); recurses into the *elements* of `List`/`Map`/`Variant`, so a
    /// large collection of small values is counted honestly. `Lambda`,
    /// `Block`, and `Handle` count as one small fixed constant and are
    /// **never** descended: chasing a closure's captured `Arc<Env>` or a
    /// handle's buffered output is the retained-size graph walk this design
    /// refuses throughout — [`pins_running_work`](super::handle::pins_running_work)
    /// makes the identical refusal for the same reason. The estimate is a residency nudge, not
    /// an accounting promise: two values sharing structure under `Arc`
    /// count twice, and captured state is invisible by construction.
    pub fn shallow_size(&self) -> usize {
        /// Stand-in cost for a `Lambda`/`Block`/`Handle` — small and fixed
        /// rather than zero, so a binding full of closures still nudges the
        /// estimate without pretending to measure what they capture.
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
            Self::Lambda { .. } | Self::Block { .. } | Self::Handle(_) => OPAQUE_CONSTANT,
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
            // Closures and handles are never structurally equal.
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unit => write!(f, ""),
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
                None => write!(f, "`{label}"),
                Some(p) => write!(f, "`{label} {p}"),
            },
            Self::Lambda { param, body, .. } => write!(f, "{}", fmt_lambda(param, body)),
            Self::Block { .. } => write!(f, "<block>"),
            Self::Handle(h) => write!(f, "<handle:{}>", h.cmd),
        }
    }
}

/// Render one pattern as a compact param string.
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

/// Walk a lambda's parameter chain (curried lambdas elaborate to a
/// nested `Lam` body) and format as `<|a b ...| block>`.
pub fn fmt_lambda(param: &crate::ir::IrPattern, body: &crate::ir::Comp) -> String {
    let mut params = vec![fmt_param(param)];
    let mut comp = body;
    while let crate::ir::CompKind::Lam { param, body } = &comp.item {
        params.push(fmt_param(param));
        comp = body;
    }
    format!("<|{}| block>", params.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// String/Bytes byte lengths are exact; a nested `List`/`Map` sums its
    /// elements' own estimates recursively, so a large collection of small
    /// values is counted honestly rather than treated as one opaque blob.
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

        // A list of lists sums all the way down.
        let nested = Value::list(vec![flat.clone(), flat]);
        assert_eq!(nested.shallow_size(), 2 * (2 + 3));

        // A map counts its keys' bytes alongside its values' estimates.
        let map = Value::map(vec![
            ("k1".to_string(), Value::String("v1".into())),
            ("k22".to_string(), Value::String("v2345".into())),
        ]);
        assert_eq!(
            map.shallow_size(),
            ("k1".len() + "v1".len()) + ("k22".len() + "v2345".len())
        );

        // A map of lists recurses through both layers.
        let map_of_lists = Value::map(vec![("k".to_string(), nested.clone())]);
        assert_eq!(
            map_of_lists.shallow_size(),
            "k".len() + nested.shallow_size()
        );

        // A variant counts its label plus its payload's estimate; a
        // payload-less variant is just its label.
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

    /// `Lambda`/`Block`/`Handle` count as one small fixed constant — their
    /// captures are never chased, so a closure over an enormous captured
    /// scope reads the same as one over an empty scope. A closure nested
    /// inside a list contributes only that constant, not its capture's size.
    #[test]
    fn shallow_size_never_descends_into_closure_captures() {
        let empty_capture = std::sync::Arc::new(crate::types::Env::new());
        let block = Value::Block {
            body: std::sync::Arc::new(crate::source::Spanned::synthetic(
                crate::ir::CompKind::Return(crate::ir::Val::Unit),
            )),
            captured: empty_capture,
        };
        let block_size = block.shallow_size();
        assert!(block_size > 0, "a closure is a small nonzero constant");

        // Build a second, larger capture and confirm the estimate is
        // identical — captures are invisible to the estimate by
        // construction, not merely "usually small".
        let mut heavy_env = crate::types::Env::new();
        heavy_env.set("heavy".into(), Value::String("x".repeat(10_000)));
        let heavy_block = Value::Block {
            body: std::sync::Arc::new(crate::source::Spanned::synthetic(
                crate::ir::CompKind::Return(crate::ir::Val::Unit),
            )),
            captured: std::sync::Arc::new(heavy_env),
        };
        assert_eq!(
            heavy_block.shallow_size(),
            block_size,
            "a closure's captured scope must never affect the estimate"
        );

        // Nested inside a list, a closure contributes only the constant.
        let list_of_one_closure = Value::list(vec![block]);
        assert_eq!(list_of_one_closure.shallow_size(), block_size);
    }
}
