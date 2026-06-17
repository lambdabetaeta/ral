//! Serialisable mirror of `Value` and `Env`.
//!
//! [`SerialValue`] and [`SerialEnvSnapshot`] are serde-round-trippable
//! representations of their runtime counterparts.  Shared scopes are
//! deduplicated via an interning table ([`InternCtx`]) so the O(2^N)
//! tree-unfolding hazard cannot occur regardless of the captured-env
//! shape.
//!
//! Used by the child-eval / pipeline-stage helper IPC (`child_eval`,
//! framed by `subprocess_codec`) to send a computation, its captured
//! closure, and the relevant parent state across a process boundary as
//! JSON.

use crate::ir::Comp;
use crate::types::{Binding, Env, Error, Value};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Transport floats by their IEEE-754 bit pattern.  JSON's number type
/// has no representation for NaN or ±∞ — `serde_json` writes them as
/// `null` and rejects `null` on the way back, breaking the totality
/// contract for the value wire.  A `f64`'s bits are a `u64`, which JSON
/// carries exactly, so every float — finite or not — round-trips
/// losslessly.
mod float_bits {
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(value: &f64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(value.to_bits())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        u64::deserialize(d).map(f64::from_bits)
    }
}

/// Serde mirror of [`Value`].  `Handle` values cannot cross the wire and
/// produce an error when encountered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SerialValue {
    Unit,
    Bool {
        value: bool,
    },
    Int {
        value: i64,
    },
    Float {
        #[serde(with = "float_bits")]
        value: f64,
    },
    String {
        value: std::string::String,
    },
    Bytes {
        value: Vec<u8>,
    },
    List {
        items: Vec<SerialValue>,
    },
    Map {
        entries: Vec<(std::string::String, SerialValue)>,
    },
    Variant {
        label: std::string::String,
        payload: Option<Box<SerialValue>>,
    },
    Lambda(SerialLambda),
    Block(SerialThunk),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SerialLambda {
    pub(crate) param: crate::ir::IrPattern,
    pub(crate) body: Arc<Comp>,
    pub(crate) captured: SerialEnvSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SerialThunk {
    pub(crate) body: Arc<Comp>,
    pub(crate) captured: SerialEnvSnapshot,
}

/// A shell snapshot in serialised form.  Each element of `scopes` is an
/// index into a companion scope table (owned by the request/response
/// envelope — see `child_eval`).  The table is a flat `Vec` of scope
/// entries, serialised at most once per `Arc`-shared allocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SerialEnvSnapshot {
    pub(crate) scopes: Vec<u32>,
}

/// Serde mirror of a scope [`Binding`]: the value in wire form, the
/// scheme as itself (already serde-round-trippable — it is what the
/// prelude bake serialises).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SerialBinding {
    pub(crate) value: SerialValue,
    pub(crate) scheme: Option<crate::typecheck::Scheme>,
}

/// The interned scope table: one row of `(name, SerialBinding)` pairs
/// per `Arc`-shared scope allocation, in DFS intern order.  Owned by
/// the request/response envelope and rebuilt into [`ScopeArcs`] by
/// [`build_arcs`] on the receiving side.
pub(crate) type ScopeTable = Vec<Vec<(String, SerialBinding)>>;

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
    pub(crate) scope_table: ScopeTable,
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
        scope: &Arc<HashMap<std::string::String, Binding>>,
    ) -> Result<u32, Error> {
        let ptr = Arc::as_ptr(scope) as usize;
        if let Some(&id) = self.ptr_to_id.get(&ptr) {
            return Ok(id);
        }
        if self.in_progress.contains(&ptr) {
            return Err(Error::new("cyclic scope reference cannot be serialised", 1));
        }
        self.in_progress.insert(ptr);
        let id = self.scope_table.len() as u32;
        self.ptr_to_id.insert(ptr, id);
        self.scope_table.push(Vec::new()); // placeholder
        let mut entries = Vec::with_capacity(scope.len());
        for (k, b) in scope.iter() {
            // Bindings that transitively contain a live in-process resource
            // (a `Handle`) cannot cross a process boundary.  Drop them here
            // rather than failing the whole snapshot: unrelated handles in
            // scope must not poison a stage that does not reference them.
            // A stage that *does* reach for the dropped name will get a
            // clean unbound-name error from the helper.  We don't descend
            // through `Thunk` boundaries: a thunk's captured env is interned
            // scope-by-scope through this same routine, which independently
            // drops handle-bearing bindings inside the captured env.
            if value_carries_handle(&b.value) {
                continue;
            }
            entries.push((
                k.clone(),
                SerialBinding {
                    value: SerialValue::from_runtime(&b.value, self)?,
                    scheme: b.scheme.clone(),
                },
            ));
        }
        self.scope_table[id as usize] = entries;
        self.in_progress.remove(&ptr);
        Ok(id)
    }
}

/// True when `value` transitively contains a `Handle` reachable without
/// crossing a `Thunk` boundary.  Thunks carry their own captured env, which
/// `intern_scope` sanitizes recursively, so a handle deep inside a thunk's
/// closure does not require dropping the *thunk* binding.
///
/// The match over `Value` is intentionally exhaustive: a new variant must
/// be classified here as scalar (no handle), container (descend), or
/// closure (don't descend) rather than silently passing as handle-free.
fn value_carries_handle(value: &Value) -> bool {
    match value {
        Value::Handle(_) => true,
        Value::List(items) => items.iter().any(value_carries_handle),
        Value::Map(entries) => entries.iter().any(|(_, v)| value_carries_handle(v)),
        Value::Variant {
            payload: Some(p), ..
        } => value_carries_handle(p),
        Value::Variant { payload: None, .. } => false,
        // Closures carry their own captured env, sanitized separately by
        // `intern_scope`, so we do not descend into them here.
        Value::Lambda { .. } | Value::Block { .. } => false,
        Value::Unit
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::Bytes(_) => false,
    }
}

/// Reconstruct one `Arc<HashMap>` per scope from a scope table.
///
/// Walks the dependency graph (scope X depends on every id reachable
/// through closures captured in its entries) and builds a scope only
/// once all of its dependencies have been built.  A cycle in the graph
/// is reported rather than silently producing a dangling reference.
pub(crate) type ScopeArcs = Vec<Option<Arc<HashMap<String, Binding>>>>;

pub(crate) fn build_arcs(scope_table: &ScopeTable) -> Result<ScopeArcs, Error> {
    let n = scope_table.len();
    let mut arcs: ScopeArcs = vec![None; n];
    let deps: Vec<HashSet<u32>> = scope_table
        .iter()
        .map(|entries| {
            let mut set = HashSet::new();
            for (_, b) in entries {
                collect_scope_deps(&b.value, &mut set);
            }
            set
        })
        .collect();
    for set in &deps {
        for &d in set {
            if d as usize >= n {
                return Err(Error::new(
                    format!("serial: scope ref {d} out of range or unresolved"),
                    1,
                ));
            }
        }
    }
    let mut built = 0usize;
    while built < n {
        let before = built;
        for id in 0..n {
            if arcs[id].is_some() {
                continue;
            }
            if !deps[id].iter().all(|&d| arcs[d as usize].is_some()) {
                continue;
            }
            let mut entries = HashMap::new();
            for (k, b) in &scope_table[id] {
                entries.insert(
                    k.clone(),
                    Binding {
                        value: b.value.clone().into_runtime(&arcs)?,
                        scheme: b.scheme.clone(),
                    },
                );
            }
            arcs[id] = Some(Arc::new(entries));
            built += 1;
        }
        if built == before {
            return Err(Error::new("serial: cyclic scope dependencies", 1));
        }
    }
    Ok(arcs)
}

/// The match over `SerialValue` is intentionally exhaustive: a new variant
/// must declare here whether it carries scope references (a closure) or
/// nested values (a container) so its dependency edges are never silently
/// dropped from the topological build in [`build_arcs`].
fn collect_scope_deps(value: &SerialValue, out: &mut HashSet<u32>) {
    match value {
        SerialValue::Lambda(l) => {
            for id in &l.captured.scopes {
                out.insert(*id);
            }
        }
        SerialValue::Block(t) => {
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
        SerialValue::Variant {
            payload: Some(p), ..
        } => {
            collect_scope_deps(p, out);
        }
        SerialValue::Variant { payload: None, .. } => {}
        SerialValue::Unit
        | SerialValue::Bool { .. }
        | SerialValue::Int { .. }
        | SerialValue::Float { .. }
        | SerialValue::String { .. }
        | SerialValue::Bytes { .. } => {}
    }
}

// ── Value conversions ─────────────────────────────────────────────────────

impl SerialValue {
    pub(crate) fn from_runtime(value: &Value, ctx: &mut InternCtx) -> Result<Self, Error> {
        Ok(match value {
            Value::Unit => Self::Unit,
            Value::Bool(v) => Self::Bool { value: *v },
            Value::Int(v) => Self::Int { value: *v },
            Value::Float(v) => Self::Float { value: *v },
            Value::String(v) => Self::String { value: v.clone() },
            Value::Bytes(v) => Self::Bytes { value: v.clone() },
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
                    .collect::<Result<_, Error>>()?,
            },
            Value::Variant { label, payload } => Self::Variant {
                label: label.clone(),
                payload: match payload {
                    Some(p) => Some(Box::new(Self::from_runtime(p, ctx)?)),
                    None => None,
                },
            },
            Value::Lambda {
                param,
                body,
                captured,
            } => Self::Lambda(SerialLambda {
                param: param.clone(),
                body: Arc::clone(body),
                captured: SerialEnvSnapshot::from_runtime(captured, ctx)?,
            }),
            Value::Block { body, captured } => Self::Block(SerialThunk {
                body: Arc::clone(body),
                captured: SerialEnvSnapshot::from_runtime(captured, ctx)?,
            }),
            Value::Handle(_) => {
                // Handles are local, process-local references to a
                // worker thread; the sandbox child cannot ship one
                // back to the parent because the parent has no way to
                // join a thread that lives in a different address space.
                // Surface this as a clear evaluation error (rather than
                // a generic IPC failure) so the user can fix the
                // body — typically by `await`ing inside the confined
                // block.
                return Err(
                    Error::new("cannot return a handle from sandboxed evaluation", 1)
                        .with_hint("await the handle before leaving the confined block"),
                );
            }
        })
    }

    pub(crate) fn into_runtime(self, arcs: &ScopeArcs) -> Result<Value, Error> {
        Ok(match self {
            Self::Unit => Value::Unit,
            Self::Bool { value } => Value::Bool(value),
            Self::Int { value } => Value::Int(value),
            Self::Float { value } => Value::Float(value),
            Self::String { value } => Value::String(value),
            Self::Bytes { value } => Value::Bytes(value),
            Self::List { items } => Value::list(
                items
                    .into_iter()
                    .map(|v| v.into_runtime(arcs))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Self::Map { entries } => Value::Map(
                entries
                    .into_iter()
                    .map(|(k, v)| Ok((k, v.into_runtime(arcs)?)))
                    .collect::<Result<_, Error>>()?,
            ),
            Self::Variant { label, payload } => Value::Variant {
                label,
                payload: match payload {
                    Some(p) => Some(Box::new((*p).into_runtime(arcs)?)),
                    None => None,
                },
            },
            Self::Lambda(lam) => Value::Lambda {
                param: lam.param,
                body: lam.body,
                captured: Arc::new(lam.captured.into_runtime(arcs)?),
            },
            Self::Block(thunk) => Value::Block {
                body: thunk.body,
                captured: Arc::new(thunk.captured.into_runtime(arcs)?),
            },
        })
    }
}

impl SerialEnvSnapshot {
    pub(crate) fn from_runtime(env: &Env, ctx: &mut InternCtx) -> Result<Self, Error> {
        let scopes = env
            .scope_iter()
            .map(|scope| ctx.intern_scope(scope))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { scopes })
    }

    pub(crate) fn into_runtime(self, arcs: &ScopeArcs) -> Result<Env, Error> {
        let scopes = self
            .scopes
            .into_iter()
            .map(|id| {
                arcs.get(id as usize)
                    .and_then(|o| o.clone())
                    .ok_or_else(|| {
                        Error::new(
                            format!("serial: scope ref {id} out of range or unresolved"),
                            1,
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Env::from_scope_iter(scopes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{CompKind, IrPattern};

    /// The body the sandbox child runs is a thunk's [`Arc<Comp>`], carried
    /// inside a [`SerialValue::Lambda`] / [`SerialValue::Block`] and framed
    /// by the IPC codec as JSON (`subprocess_codec::write_frame`).  A
    /// lambda whose body holds interior mode annotations — a [`Wire`] per
    /// pipeline stage, a ground RHS output mode on each `Bind` — must keep
    /// those verdicts across the wire, since the child reads modes off the
    /// node rather than re-inferring (there is no thunk-root wire, so the
    /// only annotations to preserve are these interior ones).
    ///
    /// This builds the annotated body through the checker, ships a lambda
    /// carrying it through the same `serde_json` codec the IPC frame uses,
    /// and asserts the deserialised body is bit-identical — `Comp`'s
    /// `PartialEq` is structural, so the assertion covers the `wires` and
    /// `rhs_output` slots on the nested nodes.
    fn annotated_lambda_body() -> Arc<Comp> {
        // A `Bind` (the `let y` RHS records a ground output mode) followed
        // by a two-stage `Pipeline` (each stage records a ground wire),
        // both interior to the lambda body — the shape the ADR names.
        let src = r#"let f = { |x| let y = /bin/echo $x; /bin/cat | /bin/cat }"#;
        let ast = crate::parse(src).expect("parse");
        let comp = crate::elaborate(&ast, Default::default());
        let annotated =
            crate::typecheck(&comp, crate::SessionSchemes::default()).expect("typecheck");
        let mut body = None;
        walk_comp(&annotated, &mut |c| {
            if let CompKind::Lam { body: b, .. } = &c.item {
                body = Some(Arc::clone(b));
            }
        });
        body.expect("lambda body in annotated comp")
    }

    /// Visit every `Comp` in the tree, descending into thunk bodies so a
    /// lambda nested inside a `{ … }` block is reached.
    fn walk_comp(comp: &Comp, visit: &mut impl FnMut(&Comp)) {
        visit(comp);
        let mut sub = |c: &Arc<Comp>| walk_comp(c, visit);
        match &comp.item {
            CompKind::Seq(parts) | CompKind::Chain(parts) => parts.iter().for_each(&mut sub),
            CompKind::Pipeline { stages, .. } => stages.iter().for_each(&mut sub),
            CompKind::Lam { body, .. } => sub(body),
            CompKind::Bind {
                comp: rhs, rest, ..
            } => {
                sub(rhs);
                sub(rest);
            }
            CompKind::App { head, .. } => sub(head),
            CompKind::If { then, else_, .. } => {
                sub(then);
                sub(else_);
            }
            CompKind::Force(crate::ir::Val::Thunk(c))
            | CompKind::Return(crate::ir::Val::Thunk(c)) => walk_comp(c, visit),
            _ => {}
        }
    }

    /// `(byte pipeline wire found, byte bind rhs_output found)` over a
    /// body.  The elaborator's placeholder is all-`Empty`, so a `Bytes`
    /// edge is the checker's verdict written onto the node.
    fn interior_annotations(body: &Comp) -> (bool, bool) {
        use crate::mode::ByteMode;
        let (mut wires, mut rhs) = (false, false);
        walk_comp(body, &mut |c| match &c.item {
            CompKind::Pipeline { wires: ws, .. }
                if ws
                    .iter()
                    .any(|w| w.output == ByteMode::Bytes || w.input == ByteMode::Bytes) =>
            {
                wires = true;
            }
            CompKind::Bind {
                rhs_output: ByteMode::Bytes,
                ..
            } => rhs = true,
            _ => {}
        });
        (wires, rhs)
    }

    #[test]
    fn lambda_body_round_trips_with_interior_annotations() {
        let body = annotated_lambda_body();
        let (wires, rhs) = interior_annotations(&body);
        assert!(wires, "body's pipeline carries a Bytes wire annotation");
        assert!(rhs, "body's bind carries a Bytes rhs_output annotation");

        let lambda = SerialValue::Lambda(SerialLambda {
            param: IrPattern::Name("x".to_string()),
            body: Arc::clone(&body),
            captured: SerialEnvSnapshot { scopes: Vec::new() },
        });

        // The codec the child-eval / pipeline-stage helper frame uses:
        // `serde_json` (`subprocess_codec::write_frame`).
        let json = serde_json::to_vec(&lambda).expect("serialise lambda");
        let back: SerialValue = serde_json::from_slice(&json).expect("deserialise lambda");

        let SerialValue::Lambda(back) = back else {
            panic!("round-trip changed the value variant");
        };
        assert_eq!(
            *back.body, *body,
            "the deserialised body must equal the original, annotations and all"
        );
        let (wires, rhs) = interior_annotations(&back.body);
        assert!(wires, "pipeline wires survive the round-trip");
        assert!(rhs, "bind rhs_output survives the round-trip");
    }

    /// A scope whose captured closure references an id past the end of
    /// the table is an out-of-range reference, not a cycle: the build is
    /// unsatisfiable for a reason `into_runtime` already names precisely,
    /// and `build_arcs` reports it with that same wording rather than the
    /// misleading "cyclic scope dependencies".
    #[test]
    fn out_of_range_scope_ref_is_not_reported_as_cyclic() {
        use crate::ir::Val;
        use crate::source::Spanned;
        let lambda = SerialValue::Lambda(SerialLambda {
            param: IrPattern::Name("x".to_string()),
            body: Arc::new(Spanned::synthetic(CompKind::Return(Val::Unit))),
            captured: SerialEnvSnapshot { scopes: vec![5] },
        });
        let table: ScopeTable = vec![vec![(
            "f".to_string(),
            SerialBinding {
                value: lambda,
                scheme: None,
            },
        )]];
        let err = build_arcs(&table).expect_err("out-of-range ref must fail the build");
        assert_eq!(
            err.message,
            "serial: scope ref 5 out of range or unresolved"
        );
    }

    /// Non-finite floats cross the value wire by bits.  JSON has no
    /// number for NaN or ±∞; serialising the `f64` directly would write
    /// `null` and reject it on decode, contradicting the totality
    /// contract.  Each case round-trips through the same `serde_json`
    /// codec the IPC frame uses and is compared by bits — `NaN != NaN`
    /// under `f64`'s `PartialEq`, so the assertion must inspect the
    /// representation, not the value.
    #[test]
    fn non_finite_floats_round_trip_by_bits() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0, 1.5] {
            let serial = SerialValue::Float { value };
            let json = serde_json::to_vec(&serial).expect("serialise float");
            let back: SerialValue = serde_json::from_slice(&json).expect("deserialise float");
            let SerialValue::Float { value: back } = back else {
                panic!("round-trip changed the value variant");
            };
            assert_eq!(
                back.to_bits(),
                value.to_bits(),
                "float {value} must round-trip bit-for-bit",
            );
        }
    }
}
