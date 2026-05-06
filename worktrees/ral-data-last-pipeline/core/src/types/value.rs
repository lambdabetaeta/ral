//! Runtime values.
//!
//! [`Value`] is the runtime representation of every ral value.
//! [`HandleInner`] is the shared state for a spawned computation.
//! [`HandlerFrame`] is one frame of the `within` effect-handler stack.
//! [`fmt_block`] renders a thunk body as a compact human-readable string.

use std::fmt;
use std::sync::{Arc, Mutex};
use super::env::Env;
use super::error::EvalSignal;

/// The runtime representation of every ral value.
///
/// The interpreter passes `Value` between computations; it is what a variable
/// holds, what a pipeline stage produces, and what a builtin returns.
///
/// `Thunk` is a suspended computation with a captured scope snapshot (a
/// closure).  Whether the thunk is a lambda (`CompKind::Lam`) or a nullary
/// block is determined by inspecting the body — the `Value` itself is
/// opaque.
///
/// `Map` uses a `Vec` of pairs rather than a hash map so that key order is
/// preserved and maps can be compared structurally.
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
    List(Vec<Value>),
    Map(Vec<(std::string::String, Value)>),
    Thunk {
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
            Value::Int(n) => Some(*n),
            Value::Float(f) if *f == f.floor() => Some(*f as i64),
            _ => None,
        }
    }

    /// Convert to f64 for arithmetic, if possible.
    ///
    /// Accepts `Int` and `Float` values only — strings are never silently
    /// parsed.  Use the `float` builtin for explicit conversion.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Human-readable runtime type name used in diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Unit => "Unit",
            Value::Bool(_) => "Bool",
            Value::Int(_) => "Int",
            Value::Float(_) => "Float",
            Value::String(_) => "String",
            Value::Bytes(_) => "Bytes",
            Value::List(_) => "List",
            Value::Map(_) => "Map",
            Value::Thunk { .. } => "Block",
            Value::Handle(_) => "Handle",
        }
    }
}

/// Shared handle to a spawned computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleState {
    Running,
    Completed,
    Cancelled,
    Disowned,
}

/// Shared handle to a spawned computation.
#[derive(Debug, Clone)]
#[allow(clippy::type_complexity)]
pub struct HandleInner {
    /// Result channel: thread sends Result<Value, EvalSignal>, await receives.
    pub result: Arc<Mutex<Option<std::sync::mpsc::Receiver<Result<Value, EvalSignal>>>>>,
    /// Cached result after first await (§13.3: second await returns cached).
    pub cached: Arc<Mutex<Option<Result<Value, EvalSignal>>>>,
    /// Lifecycle state for handle-level APIs.
    pub state: Arc<Mutex<HandleState>>,
    /// Buffered stdout from the spawned block (§13.3 replay rule).  Bytes
    /// accumulate here during execution and are drained on `await`.  Always
    /// empty for watched handles — bytes flow live through `Sink::LineFramed`.
    pub stdout_buf: Arc<Mutex<Vec<u8>>>,
    /// Buffered stderr from the spawned block (§13.3 replay rule).  Always
    /// empty for watched handles.
    pub stderr_buf: Arc<Mutex<Vec<u8>>>,
    pub cmd: std::string::String,
}

impl PartialEq for HandleInner {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.result, &other.result)
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Unit, Value::Unit) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Bytes(a), Value::Bytes(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => a == b,
            // Closures and handles are never structurally equal.
            (Value::Thunk { .. }, Value::Thunk { .. }) => false,
            (Value::Handle(_), Value::Handle(_)) => false,
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Unit => write!(f, ""),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(n) => write!(f, "{n}"),
            Value::String(s) => write!(f, "{s}"),
            Value::Bytes(b) => write!(f, "{}", String::from_utf8_lossy(b)),
            Value::List(items) => {
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
            Value::Map(pairs) => {
                if pairs.is_empty() {
                    return write!(f, "[:]");
                }
                write!(f, "[")?;
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "]")
            }
            Value::Thunk { body, .. } => write!(f, "{}", fmt_block(body)),
            Value::Handle(h) => write!(f, "<handle:{}>", h.cmd),
        }
    }
}

/// Render one pattern as a compact param string.
fn fmt_param(p: &crate::ast::Pattern) -> String {
    match p {
        crate::ast::Pattern::Wildcard => "_".into(),
        crate::ast::Pattern::Name(s) => s.clone(),
        crate::ast::Pattern::List { elems, rest } => {
            let mut parts: Vec<String> = elems.iter().map(fmt_param).collect();
            if let Some(r) = rest { parts.push(format!("...{r}")); }
            format!("[{}]", parts.join(" "))
        }
        crate::ast::Pattern::Map(entries) => {
            let parts: Vec<String> = entries.iter()
                .map(|(k, pat, _)| {
                    let v = fmt_param(pat);
                    if matches!(pat, crate::ast::Pattern::Name(n) if n == k) { k.clone() }
                    else { format!("{k}: {v}") }
                })
                .collect();
            format!("[{}]", parts.join(" "))
        }
    }
}

/// Walk nested `Lam` nodes to collect parameter names, then format as
/// `<block>` (nullary) or `<|a b| block>` (one or more params).
pub fn fmt_block(body: &crate::ir::Comp) -> String {
    let mut params: Vec<String> = Vec::new();
    let mut comp = body;
    loop {
        match &comp.kind {
            crate::ir::CompKind::Lam { param, body } => {
                params.push(fmt_param(param));
                comp = body;
            }
            _ => break,
        }
    }
    if params.is_empty() {
        "<block>".into()
    } else {
        format!("<|{}| block>", params.join(" "))
    }
}

/// One `within [handlers: …, handler: …]` frame.
///
/// Per-name entries are checked before the catch-all within the same frame.
/// If a frame has neither a matching per-name nor a catch-all, dispatch falls
/// through to the next outer frame.
#[derive(Debug, Clone)]
pub struct HandlerFrame {
    /// Per-name handlers: `within [handlers: [NAME: thunk]]`.
    pub per_name: Vec<(std::string::String, Value)>,
    /// Catch-all handler: `within [handler: thunk]`.
    pub catch_all: Option<Value>,
}
