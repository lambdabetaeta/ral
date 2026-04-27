//! Serialisable mirror of `Value` and `Env`.
//!
//! [`SerialValue`] and [`SerialEnvSnapshot`] are serde-round-trippable
//! representations of their runtime counterparts.  Shared scopes are
//! deduplicated via an interning table ([`InternCtx`]) so the O(2^N)
//! tree-unfolding hazard cannot occur regardless of the captured-env
//! shape.
//!
//! Used by the sandbox IPC layer (`sandbox::ipc`) to send a computation,
//! its captured closure, and the relevant parent state across a process
//! boundary as JSON.

use crate::ir::Comp;
use crate::types::{Env, Error, EvalSignal, Value};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Serde mirror of [`Value`].  `Handle` values cannot cross the wire and
/// produce an error when encountered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SerialValue {
    Unit,
    Bool { value: bool },
    Int { value: i64 },
    Float { value: f64 },
    String { value: std::string::String },
    Bytes { value: Vec<u8> },
    List { items: Vec<SerialValue> },
    Map { entries: Vec<(std::string::String, SerialValue)> },
    Thunk(SerialThunk),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SerialThunk {
    pub(crate) body: Comp,
    pub(crate) captured: SerialEnvSnapshot,
}

/// An shell snapshot in serialised form.  Each element of `scopes` is an
/// index into a companion scope table (owned by the request/response
/// envelope — see `sandbox::ipc`).  The table is a flat `Vec` of scope
/// entries, serialised at most once per `Arc`-shared allocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SerialEnvSnapshot {
    pub(crate) scopes: Vec<u32>,
}

// ── Interning context ─────────────────────────────────────────────────────
//
// `InternCtx` tracks Arc pointer identity so a scope shared by multiple
// closures is serialised once and referenced by index everywhere else.
//
// Scopes are interned DFS but their references are unordered: a scope may
// hold a closure whose captured shell points back at an earlier-interned
// sibling (common when inner scopes carry functions captured from outer
// ones).  `build_arcs` therefore topologically sorts by dependency
// instead of trusting id order.

pub(crate) struct InternCtx {
    pub(crate) scope_table: Vec<Vec<(std::string::String, SerialValue)>>,
    ptr_to_id: HashMap<usize, u32>,
    in_progress: HashSet<usize>,
}

impl InternCtx {
    pub(crate) fn new() -> Self {
        Self {
            scope_table: Vec::new(),
            ptr_to_id: HashMap::new(),
            in_progress: HashSet::new(),
        }
    }

    fn intern_scope(
        &mut self,
        scope: &Arc<HashMap<std::string::String, Value>>,
    ) -> Result<u32, EvalSignal> {
        let ptr = Arc::as_ptr(scope) as usize;
        if let Some(&id) = self.ptr_to_id.get(&ptr) {
            return Ok(id);
        }
        if self.in_progress.contains(&ptr) {
            return Err(EvalSignal::Error(Error::new(
                "cyclic scope reference cannot be serialised",
                1,
            )));
        }
        self.in_progress.insert(ptr);
        let id = self.scope_table.len() as u32;
        self.ptr_to_id.insert(ptr, id);
        self.scope_table.push(Vec::new()); // placeholder
        let mut entries = Vec::with_capacity(scope.len());
        for (k, v) in scope.iter() {
            entries.push((k.clone(), SerialValue::from_runtime(v, self)?));
        }
        self.scope_table[id as usize] = entries;
        self.in_progress.remove(&ptr);
        Ok(id)
    }
}

/// Reconstruct one `Arc<HashMap>` per scope from a scope table.
///
/// Walks the dependency graph (scope X depends on every id reachable
/// through closures captured in its entries) and builds a scope only
/// once all of its dependencies have been built.  A cycle in the graph
/// is reported rather than silently producing a dangling reference.
pub(crate) fn build_arcs(
    scope_table: &[Vec<(std::string::String, SerialValue)>],
) -> Result<Vec<Option<Arc<HashMap<std::string::String, Value>>>>, EvalSignal> {
    let n = scope_table.len();
    let mut arcs: Vec<Option<Arc<HashMap<std::string::String, Value>>>> = vec![None; n];
    let deps: Vec<HashSet<u32>> = scope_table
        .iter()
        .map(|entries| {
            let mut set = HashSet::new();
            for (_, v) in entries {
                collect_scope_deps(v, &mut set);
            }
            set
        })
        .collect();
    let mut built = 0usize;
    while built < n {
        let before = built;
        for id in 0..n {
            if arcs[id].is_some() {
                continue;
            }
            if !deps[id]
                .iter()
                .all(|&d| arcs.get(d as usize).and_then(|o| o.as_ref()).is_some())
            {
                continue;
            }
            let mut entries = HashMap::new();
            for (k, v) in &scope_table[id] {
                entries.insert(k.clone(), v.clone().into_runtime(&arcs)?);
            }
            arcs[id] = Some(Arc::new(entries));
            built += 1;
        }
        if built == before {
            return Err(EvalSignal::Error(Error::new(
                "serial: cyclic scope dependencies",
                1,
            )));
        }
    }
    Ok(arcs)
}

fn collect_scope_deps(value: &SerialValue, out: &mut HashSet<u32>) {
    match value {
        SerialValue::Thunk(t) => {
            for id in &t.captured.scopes {
                out.insert(*id);
            }
        }
        SerialValue::List { items } => {
            for v in items {
                collect_scope_deps(v, out);
            }
        }
        SerialValue::Map { entries } => {
            for (_, v) in entries {
                collect_scope_deps(v, out);
            }
        }
        _ => {}
    }
}

// ── Value conversions ─────────────────────────────────────────────────────

impl SerialValue {
    pub(crate) fn from_runtime(value: &Value, ctx: &mut InternCtx) -> Result<Self, EvalSignal> {
        Ok(match value {
            Value::Unit      => Self::Unit,
            Value::Bool(v)   => Self::Bool   { value: *v },
            Value::Int(v)    => Self::Int    { value: *v },
            Value::Float(v)  => Self::Float  { value: *v },
            Value::String(v) => Self::String { value: v.clone() },
            Value::Bytes(v)  => Self::Bytes  { value: v.clone() },
            Value::List(items) => Self::List {
                items: items
                    .iter()
                    .map(|v| Self::from_runtime(v, ctx))
                    .collect::<Result<_, _>>()?,
            },
            Value::Map(items) => Self::Map {
                entries: items
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), Self::from_runtime(v, ctx)?)))
                    .collect::<Result<_, EvalSignal>>()?,
            },
            Value::Thunk { body, captured } => Self::Thunk(SerialThunk {
                body: body.as_ref().clone(),
                captured: SerialEnvSnapshot::from_runtime(captured, ctx)?,
            }),
            Value::Handle(_) => {
                return Err(EvalSignal::Error(Error::new(
                    "Handle values cannot be serialised",
                    1,
                )));
            }
        })
    }

    pub(crate) fn into_runtime(
        self,
        arcs: &[Option<Arc<HashMap<std::string::String, Value>>>],
    ) -> Result<Value, EvalSignal> {
        Ok(match self {
            Self::Unit             => Value::Unit,
            Self::Bool   { value } => Value::Bool(value),
            Self::Int    { value } => Value::Int(value),
            Self::Float  { value } => Value::Float(value),
            Self::String { value } => Value::String(value),
            Self::Bytes  { value } => Value::Bytes(value),
            Self::List   { items } => Value::List(
                items
                    .into_iter()
                    .map(|v| v.into_runtime(arcs))
                    .collect::<Result<_, _>>()?,
            ),
            Self::Map { entries } => Value::Map(
                entries
                    .into_iter()
                    .map(|(k, v)| Ok((k, v.into_runtime(arcs)?)))
                    .collect::<Result<_, EvalSignal>>()?,
            ),
            Self::Thunk(thunk) => Value::Thunk {
                body: Arc::new(thunk.body),
                captured: Arc::new(thunk.captured.into_runtime(arcs)?),
            },
        })
    }
}

impl SerialEnvSnapshot {
    pub(crate) fn from_runtime(
        env: &Env,
        ctx: &mut InternCtx,
    ) -> Result<Self, EvalSignal> {
        let scopes = env
            .scopes()
            .iter()
            .map(|scope| ctx.intern_scope(scope))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { scopes })
    }

    pub(crate) fn into_runtime(
        self,
        arcs: &[Option<Arc<HashMap<std::string::String, Value>>>],
    ) -> Result<Env, EvalSignal> {
        let scopes = self
            .scopes
            .into_iter()
            .map(|id| {
                arcs.get(id as usize)
                    .and_then(|o| o.clone())
                    .ok_or_else(|| {
                        EvalSignal::Error(Error::new(
                            format!("serial: scope ref {id} out of range or unresolved"),
                            1,
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Env::from_scopes(scopes))
    }
}
