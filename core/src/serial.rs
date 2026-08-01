//! Serialisable mirrors of `Value` and `Env`.
//!
//! [`FOValue`] is first-order by construction — data all the way down — over
//! an extension slot uninhabited by default ([`NoExt`]).  [`SerialValue`]
//! fills the slot with [`Closure`] so the re-exec'd child IPC (`child_eval`,
//! `subprocess`, the pipeline stage helper) can ship a captured environment
//! as JSON.  Scopes intern by `Arc` identity ([`InternCtx`]) and rebuild
//! topologically ([`WireDecoder::for_shell`]), so sharing survives the
//! crossing rather than unfolding into an exponential tree.  `into_runtime`,
//! given a [`WireDecoder`], is the sole wire→runtime conversion.

use crate::ir::Comp;
use crate::types::{Binding, BuiltinTable, Env, Error, Shell, Value};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Floats cross as their IEEE-754 bits.  JSON has no number for NaN or ±∞:
/// `serde_json` writes them as `null` and then refuses to read it back.
mod float_bits {
    use serde::{Deserialize, Deserializer, Serializer};

    #[allow(
        clippy::trivially_copy_pass_by_ref,
        reason = "signature dictated by serde's `serialize_with`/`with` contract: `fn(&T, S)`"
    )]
    pub(super) fn serialize<S: Serializer>(value: &f64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(value.to_bits())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        u64::deserialize(d).map(f64::from_bits)
    }
}

/// A first-order ral value — data all the way down.  The extension slot `X`
/// is uninhabited by default, so a bare `FOValue` is first-order by
/// construction rather than by a checked invariant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FOValue<X = NoExt> {
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
        items: Vec<Self>,
    },
    Map {
        entries: Vec<(std::string::String, Self)>,
    },
    Variant {
        label: std::string::String,
        payload: Option<Box<Self>>,
    },
    Ext(X),
}

/// Uninhabited: a bare `FOValue` has no `Ext` arm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoExt {}

/// What the re-exec'd child IPC adds to [`FOValue`]: closures over interned
/// scopes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Closure {
    Lambda(SerialLambda),
    Block(SerialThunk),
    Native(SerialNative),
}

/// [`FOValue`] with closures, for the re-exec'd child IPC.  A `Handle` has no
/// wire form; encoding one is an error.
pub type SerialValue = FOValue<Closure>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerialLambda {
    pub param: crate::ir::IrPattern,
    pub body: Arc<Comp>,
    pub captured: SerialEnvSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerialThunk {
    pub body: Arc<Comp>,
    pub captured: SerialEnvSnapshot,
}

/// Wire mirror of a [`Value::Native`](crate::types::Value::Native): the body
/// cannot cross, so hydration re-links the name against the receiving
/// shell's manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerialNative {
    pub name: std::string::String,
    pub applied: Vec<SerialValue>,
}

/// An [`Env`] in wire form: one [`ScopeTable`] index per scope, resolved
/// against the table carried on the enclosing request/response envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SerialEnvSnapshot {
    pub scopes: Vec<u32>,
}

/// Wire mirror of a [`Binding`]: the value converted, the scheme as itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerialBinding {
    pub value: SerialValue,
    pub scheme: Option<crate::typecheck::Scheme>,
}

/// One row per `Arc`-shared scope, in DFS intern order;
/// [`WireDecoder::for_shell`] rebuilds it into scope arcs on the receiving
/// side.
pub type ScopeTable = Vec<Vec<(String, SerialBinding)>>;

// ── Interning context ─────────────────────────────────────────────────────
//
// Scopes intern in DFS order, but their references are unordered: a scope may
// hold a closure whose captured env points back at an earlier-interned sibling.
// `WireDecoder::for_shell` therefore sorts by dependency rather than trusting
// id order.

pub struct InternCtx {
    pub scope_table: ScopeTable,
    pub ptr_to_id: HashMap<usize, u32>,
}

impl InternCtx {
    pub fn new() -> Self {
        Self {
            scope_table: Vec::new(),
            ptr_to_id: HashMap::new(),
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
        #[allow(
            clippy::cast_possible_truncation,
            reason = "serialised scope id; the table holds a handful of scopes, far below 2^32"
        )]
        let id = self.scope_table.len() as u32;
        self.ptr_to_id.insert(ptr, id);
        self.scope_table.push(Vec::new()); // reserve the row: recursion may reach this id
        let mut entries = Vec::with_capacity(scope.len());
        for (k, b) in scope.iter() {
            // A binding reaching a `Handle` cannot cross a process boundary.
            // Drop it rather than fail the snapshot: an unrelated handle must
            // not poison a stage that never names it, and one that does name
            // it gets a clean unbound-name error instead.
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
        Ok(id)
    }
}

impl Default for InternCtx {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `value` reaches a `Handle` without crossing a closure boundary — a
/// closure's captured env is interned through `intern_scope`, which drops
/// handle-bearing bindings there.  The match is exhaustive on purpose: a new
/// `Value` variant must be classified rather than pass as handle-free.
fn value_carries_handle(value: &Value) -> bool {
    match value {
        Value::Handle(_) => true,
        Value::List(items) => items.iter().any(value_carries_handle),
        Value::Map(entries) => entries.iter().any(|(_, v)| value_carries_handle(v)),
        Value::Variant {
            payload: Some(p), ..
        } => value_carries_handle(p),
        Value::Native { applied, .. } => applied.iter().any(value_carries_handle),
        Value::Variant { payload: None, .. }
        | Value::Lambda { .. }
        | Value::Block { .. }
        | Value::Unit
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::Bytes(_) => false,
    }
}

/// One reconstructed `Arc<HashMap>` per scope table row (`None` until built).
type ScopeArcs = Vec<Option<Arc<HashMap<String, Binding>>>>;

/// Decode capability for one wire envelope: the rebuilt scope arcs plus the
/// [`BuiltinTable`] a captured [`Value::Native`](crate::types::Value::Native)
/// re-links its name against.
///
/// Constructible only from the [`Shell`] that will run the decoded values,
/// so no call site can pick a manifest of its own.
#[derive(Debug)]
pub struct WireDecoder {
    arcs: ScopeArcs,
    manifest: BuiltinTable,
}

impl WireDecoder {
    /// Rebuild one `Arc<HashMap>` per row of `scope_table`, each once its
    /// dependencies are built, resolving natives against `shell`'s manifest.
    ///
    /// # Errors
    /// A scope reference out of range or unresolved, a binding that fails to
    /// decode, or a cycle — a pass in which no scope makes progress.
    pub(crate) fn for_shell(shell: &Shell, scope_table: &ScopeTable) -> Result<Self, Error> {
        let n = scope_table.len();
        let mut dec = Self {
            arcs: vec![None; n],
            manifest: shell.session.builtins.clone(),
        };
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
                if dec.arcs[id].is_some() {
                    continue;
                }
                if !deps[id].iter().all(|&d| dec.arcs[d as usize].is_some()) {
                    continue;
                }
                let mut entries = HashMap::new();
                for (k, b) in &scope_table[id] {
                    entries.insert(
                        k.clone(),
                        Binding {
                            value: b.value.clone().into_runtime(&dec)?,
                            scheme: b.scheme.clone(),
                        },
                    );
                }
                dec.arcs[id] = Some(Arc::new(entries));
                built += 1;
            }
            if built == before {
                return Err(Error::new("serial: cyclic scope dependencies", 1));
            }
        }
        Ok(dec)
    }
}

/// The match is exhaustive on purpose: a new [`SerialValue`] variant must
/// declare whether it carries scope references, or its dependency edges go
/// silently missing from [`WireDecoder::for_shell`].
fn collect_scope_deps(value: &SerialValue, out: &mut HashSet<u32>) {
    match value {
        SerialValue::Ext(Closure::Lambda(l)) => {
            for id in &l.captured.scopes {
                out.insert(*id);
            }
        }
        SerialValue::Ext(Closure::Block(t)) => {
            for id in &t.captured.scopes {
                out.insert(*id);
            }
        }
        SerialValue::Ext(Closure::Native(n)) => {
            for v in &n.applied {
                collect_scope_deps(v, out);
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
        SerialValue::Variant { payload: None, .. }
        | SerialValue::Unit
        | SerialValue::Bool { .. }
        | SerialValue::Int { .. }
        | SerialValue::Float { .. }
        | SerialValue::String { .. }
        | SerialValue::Bytes { .. } => {}
    }
}

// ── Value conversions ─────────────────────────────────────────────────────

impl FOValue<Closure> {
    /// Encode a runtime [`Value`], interning captured closure environments
    /// through `ctx`.
    ///
    /// # Errors
    /// `value` is or reaches a `Value::Handle`, which has no wire form.
    pub fn from_runtime(value: &Value, ctx: &mut InternCtx) -> Result<Self, Error> {
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
            } => Self::Ext(Closure::Lambda(SerialLambda {
                param: param.clone(),
                body: Arc::clone(body),
                captured: SerialEnvSnapshot::from_runtime(captured, ctx)?,
            })),
            Value::Block { body, captured } => Self::Ext(Closure::Block(SerialThunk {
                body: Arc::clone(body),
                captured: SerialEnvSnapshot::from_runtime(captured, ctx)?,
            })),
            Value::Native { entry, applied } => Self::Ext(Closure::Native(SerialNative {
                name: entry.name.as_ref().to_string(),
                applied: applied
                    .iter()
                    .map(|v| Self::from_runtime(v, ctx))
                    .collect::<Result<_, _>>()?,
            })),
            Value::Handle(_) => {
                // A handle names a worker thread in this process, and no
                // receiver can join a thread in another address space.
                return Err(
                    Error::new("cannot return a handle from sandboxed evaluation", 1)
                        .with_hint("await the handle before leaving the confined block"),
                );
            }
        })
    }

    /// Decode back into a runtime [`Value`], resolving captured environments
    /// and native names against `dec`.
    ///
    /// # Errors
    /// A nested value fails to decode, a captured environment names a scope id
    /// out of range or unresolved, or a native's name is unknown to the
    /// manifest.
    pub fn into_runtime(self, dec: &WireDecoder) -> Result<Value, Error> {
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
                    .map(|v| v.into_runtime(dec))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Self::Map { entries } => Value::Map(
                entries
                    .into_iter()
                    .map(|(k, v)| Ok((k, v.into_runtime(dec)?)))
                    .collect::<Result<_, Error>>()?,
            ),
            Self::Variant { label, payload } => Value::Variant {
                label,
                payload: match payload {
                    Some(p) => Some(Box::new((*p).into_runtime(dec)?)),
                    None => None,
                },
            },
            Self::Ext(Closure::Lambda(lam)) => Value::Lambda {
                param: lam.param,
                body: lam.body,
                captured: Arc::new(lam.captured.into_runtime(dec)?),
            },
            Self::Ext(Closure::Block(thunk)) => Value::Block {
                body: thunk.body,
                captured: Arc::new(thunk.captured.into_runtime(dec)?),
            },
            Self::Ext(Closure::Native(n)) => {
                let entry = dec.manifest.get(&n.name).ok_or_else(|| {
                    Error::new(
                        format!("serial: unknown native '{}' in receiving manifest", n.name),
                        1,
                    )
                })?;
                Value::Native {
                    entry: Arc::new(entry),
                    applied: n
                        .applied
                        .into_iter()
                        .map(|v| v.into_runtime(dec))
                        .collect::<Result<_, _>>()?,
                }
            }
        })
    }
}

impl TryFrom<&Value> for FOValue {
    type Error = Error;

    /// Recursion over the data variants *is* the host seam's first-orderness
    /// check, rather than a separate test followed by a hopeful re-encode.
    fn try_from(v: &Value) -> Result<Self, Error> {
        Ok(match v {
            Value::Unit => Self::Unit,
            Value::Bool(v) => Self::Bool { value: *v },
            Value::Int(v) => Self::Int { value: *v },
            Value::Float(v) => Self::Float { value: *v },
            Value::String(v) => Self::String { value: v.clone() },
            Value::Bytes(v) => Self::Bytes { value: v.clone() },
            Value::List(items) => Self::List {
                items: items.iter().map(Self::try_from).collect::<Result<_, _>>()?,
            },
            Value::Map(items) => Self::Map {
                entries: items
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), Self::try_from(v)?)))
                    .collect::<Result<_, Error>>()?,
            },
            Value::Variant { label, payload } => Self::Variant {
                label: label.clone(),
                payload: match payload {
                    Some(p) => Some(Box::new(Self::try_from(p.as_ref())?)),
                    None => None,
                },
            },
            Value::Lambda { .. } | Value::Block { .. } | Value::Native { .. } | Value::Handle(_) => {
                return Err(Error::new(
                    "value is not first-order: the host seam carries only data, \
                     not closures or handles",
                    1,
                ));
            }
        })
    }
}

impl From<FOValue> for Value {
    /// Total: first-order values are a subset of `Value`, and `Ext` is
    /// unreachable at `X = NoExt`.
    fn from(fo: FOValue) -> Self {
        match fo {
            FOValue::Unit => Self::Unit,
            FOValue::Bool { value } => Self::Bool(value),
            FOValue::Int { value } => Self::Int(value),
            FOValue::Float { value } => Self::Float(value),
            FOValue::String { value } => Self::String(value),
            FOValue::Bytes { value } => Self::Bytes(value),
            FOValue::List { items } => Self::list(items.into_iter().map(Self::from).collect()),
            FOValue::Map { entries } => Self::Map(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, Self::from(v)))
                    .collect(),
            ),
            FOValue::Variant { label, payload } => Self::Variant {
                label,
                payload: payload.map(|p| Box::new(Self::from(*p))),
            },
            FOValue::Ext(x) => match x {},
        }
    }
}

impl SerialEnvSnapshot {
    /// Intern every scope of `env` into `ctx`, recording their table ids.
    ///
    /// # Errors
    /// Encoding one of a scope's bindings fails; handle-bearing bindings are
    /// dropped rather than raised.
    pub fn from_runtime(env: &Env, ctx: &mut InternCtx) -> Result<Self, Error> {
        let scopes = env
            .scope_iter()
            .map(|scope| ctx.intern_scope(scope))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { scopes })
    }

    /// Rebuild an [`Env`] from this snapshot's scope ids; `dec`'s manifest
    /// seeds its base native scope ([`BuiltinTable::natives_arc`]), since
    /// natives never ride the wire.
    ///
    /// # Errors
    /// A recorded scope id is out of range or unresolved.
    pub fn into_runtime(self, dec: &WireDecoder) -> Result<Env, Error> {
        let scopes = self
            .scopes
            .into_iter()
            .map(|id| {
                dec.arcs
                    .get(id as usize)
                    .and_then(std::clone::Clone::clone)
                    .ok_or_else(|| {
                        Error::new(
                            format!("serial: scope ref {id} out of range or unresolved"),
                            1,
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Env::from_scope_iter(scopes, dec.manifest.natives_arc()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{CompKind, IrPattern};

    /// The child reads modes off the node rather than re-inferring, so a
    /// lambda body must carry its interior verdicts across the wire — a wire
    /// per pipeline stage, a ground RHS output mode on each `Bind`.  There is
    /// no thunk-root wire, so those interior slots are the whole of it.
    fn annotated_lambda_body() -> Arc<Comp> {
        let src = r"let f = { |x| let y = /bin/echo $x; /bin/cat | /bin/cat }";
        let ast = crate::parse(src).expect("parse");
        let comp = crate::elaborate(&ast, HashSet::default(), "").expect("elaborate");
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

    /// `(byte pipeline wire, byte bind rhs_output)`.  The elaborator's
    /// placeholder is all-`Empty`, so a `Bytes` edge can only be the checker's.
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

        let lambda = SerialValue::Ext(Closure::Lambda(SerialLambda {
            param: IrPattern::Name("x".to_string()),
            body: Arc::clone(&body),
            captured: SerialEnvSnapshot { scopes: Vec::new() },
        }));

        // The same `serde_json` codec `subprocess_codec` frames with.
        let json = serde_json::to_vec(&lambda).expect("serialise lambda");
        let back: SerialValue = serde_json::from_slice(&json).expect("deserialise lambda");

        let SerialValue::Ext(Closure::Lambda(back)) = back else {
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

    /// A reference past the end of the table is out of range, not a cycle:
    /// the build must say so rather than blame the fallthrough case.
    #[test]
    fn out_of_range_scope_ref_is_not_reported_as_cyclic() {
        use crate::ir::Val;
        use crate::source::Spanned;
        let lambda = SerialValue::Ext(Closure::Lambda(SerialLambda {
            param: IrPattern::Name("x".to_string()),
            body: Arc::new(Spanned::synthetic(CompKind::Return(Val::Unit))),
            captured: SerialEnvSnapshot { scopes: vec![5] },
        }));
        let table: ScopeTable = vec![vec![(
            "f".to_string(),
            SerialBinding {
                value: lambda,
                scheme: None,
            },
        )]];
        let err = WireDecoder::for_shell(&Shell::default(), &table)
            .expect_err("out-of-range ref must fail the build");
        assert_eq!(
            err.message,
            "serial: scope ref 5 out of range or unresolved"
        );
    }

    /// Compared by bits, not by value: `NaN != NaN` under `f64`'s
    /// `PartialEq`, so the assertion must inspect the representation.
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

    #[test]
    fn ipc_value_roundtrips_simple_values() {
        let value = Value::map(vec![
            ("a".into(), Value::Int(1)),
            ("b".into(), Value::String("x".into())),
        ]);
        let mut ctx = InternCtx::new();
        let ipc = SerialValue::from_runtime(&value, &mut ctx).expect("to serial");
        let dec = WireDecoder::for_shell(&Shell::default(), &ctx.scope_table).expect("decoder");
        assert_eq!(ipc.into_runtime(&dec).expect("from serial"), value);
    }
}
