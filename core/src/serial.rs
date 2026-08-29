//! Serialisable mirrors of `Value` and `Env`.
//!
//! [`FOValue`] is first-order by construction — data all the way down — over
//! an extension slot uninhabited by default ([`NoExt`]).  [`SerialValue`]
//! fills the slot with [`Closure`] so the re-exec'd child IPC (`child_eval`,
//! `subprocess`, the pipeline stage helper) can ship a captured environment
//! as JSON.  `serial.rs` interns *environments*, not scopes: one row per
//! distinct session-tier root, by [`imbl::GenericHashMap::ptr_eq`] identity
//! ([`InternCtx`]), rebuilt topologically ([`WireDecoder::for_shell`]) and
//! seated under the receiver's own natives and prelude — those two constant
//! tiers never cross.  `into_runtime`, given a [`WireDecoder`], is the sole
//! wire→runtime conversion.

use crate::ir::Comp;
use crate::types::{Binding, BuiltinTable, Env, Error, Shell, Value};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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
///
/// One wire shape for the one runtime thunk value (S10): `Comp::arrow`
/// on the decoded `comp` tells `Lambda` from `Block` back apart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SerialClosure {
    Thunk(SerialThunk),
    Native(SerialNative),
}

/// [`FOValue`] with closures, for the re-exec'd child IPC.  A `Handle` has no
/// wire form; encoding one is an error.
pub type SerialValue = FOValue<SerialClosure>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerialThunk {
    pub comp: Arc<Comp>,
    pub env: SerialEnvSnapshot,
}

/// Wire mirror of a [`Value::Native`]: the body
/// cannot cross, so hydration re-links the name against the receiving
/// shell's manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerialNative {
    pub name: std::string::String,
    pub applied: Vec<SerialValue>,
}

/// An [`Env`] in wire form: the [`ScopeTable`] row holding its session
/// tier, resolved against the table carried on the enclosing
/// request/response envelope.
///
/// The natives and prelude tiers never ride the wire, so they need no row
/// of their own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SerialEnvSnapshot {
    pub bindings: u32,
}

/// Wire mirror of a [`Binding`]: the value converted, the scheme as itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerialBinding {
    pub value: SerialValue,
    pub scheme: Option<crate::typecheck::Scheme>,
}

/// One row per interned environment, in discovery order;
/// [`WireDecoder::for_shell`] rebuilds it into session-tier maps on the
/// receiving side.
pub type ScopeTable = Vec<Vec<(String, SerialBinding)>>;

// ── Interning context ─────────────────────────────────────────────────────
//
// Environment references are unordered: an environment may hold a closure
// whose captured env points at a row interned before or after it.
// `WireDecoder::for_shell` therefore sorts by dependency rather than trusting
// id order.

pub struct InternCtx {
    scope_table: ScopeTable,
    /// Every root interned in this message so far, scanned linearly by
    /// [`imbl::GenericHashMap::ptr_eq`] — a message holds a handful, so the scan
    /// costs nothing a hash lookup would save.
    roots: Vec<(crate::types::BindingMap, u32)>,
    /// Rows with an id but no encoding yet.  A stream is a chain of closures —
    /// block → captured env → binding → block — so encoding an environment's
    /// bindings inside `intern_env` would recurse once per link and bound
    /// stream length by the stack.  Interning only *reserves*; [`Self::finish`]
    /// encodes from this queue, a worklist in place of that recursion.
    pending: Vec<(u32, crate::types::BindingMap)>,
}

impl InternCtx {
    pub fn new() -> Self {
        Self {
            scope_table: Vec::new(),
            roots: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Intern `bindings` — an [`Env`]'s session tier — by its persistent
    /// root's identity, reserving a row for [`Self::finish`] to encode.
    fn intern_env(&mut self, bindings: &crate::types::BindingMap) -> u32 {
        if let Some(&(_, id)) = self.roots.iter().find(|(root, _)| root.ptr_eq(bindings)) {
            return id;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "serialised row id; the table holds a handful of environments, far below 2^32"
        )]
        let id = self.scope_table.len() as u32;
        self.roots.push((bindings.clone(), id));
        self.scope_table.push(Vec::new()); // reserve the row; `finish` fills it
        self.pending.push((id, bindings.clone()));
        id
    }

    /// Encode every pending environment's bindings — each may intern further
    /// environments, which join the queue rather than the stack — and yield
    /// the table.  Nothing ships without passing through here: the table has
    /// no other accessor.
    ///
    /// # Errors
    /// Encoding one of a binding's value fails; handle-bearing bindings are
    /// dropped rather than raised.
    pub fn finish(mut self) -> Result<ScopeTable, Error> {
        while let Some((id, bindings)) = self.pending.pop() {
            let mut entries = Vec::with_capacity(bindings.len());
            for (k, b) in &bindings {
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
                        value: SerialValue::from_runtime(&b.value, &mut self)?,
                        scheme: b.scheme.as_deref().cloned(),
                    },
                ));
            }
            self.scope_table[id as usize] = entries;
        }
        Ok(self.scope_table)
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
        Value::Variant { payload: None, .. } | Value::Thunk(_) | Value::Unit
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::Bytes(_) => false,
    }
}

/// One reconstructed session-tier map per scope table row (`None` until
/// built).
type EnvRows = Vec<Option<crate::types::BindingMap>>;

/// Decode capability for one wire envelope: the rebuilt environment rows
/// and the tiers a captured value re-links against.
///
/// The rows sit beside the receiver's own natives and prelude tiers every
/// row is seated under, and the [`BuiltinTable`] a captured
/// [`Value::Native`] re-links its name against.
///
/// Constructible only from the [`Shell`] that will run the decoded values,
/// so no call site can pick a manifest — or a prelude — of its own.
#[derive(Debug)]
pub struct WireDecoder {
    rows: EnvRows,
    manifest: BuiltinTable,
    natives: Arc<crate::types::NativeMap>,
    prelude: Arc<crate::types::PreludeMap>,
}

impl WireDecoder {
    /// Rebuild one session-tier map per row of `scope_table`, each once its
    /// dependencies are built; every row is later seated under `shell`'s own
    /// natives and prelude ([`SerialEnvSnapshot::into_runtime`]), never the
    /// sender's — those two tiers never ride the wire.
    ///
    /// # Errors
    /// A row reference out of range or unresolved, a binding that fails to
    /// decode, or a cycle — a pass in which no row makes progress.
    pub(crate) fn for_shell(shell: &Shell, scope_table: &ScopeTable) -> Result<Self, Error> {
        let n = scope_table.len();
        let mut dec = Self {
            rows: vec![None; n],
            manifest: shell.session.builtins.clone(),
            natives: shell.env.natives_arc(),
            prelude: shell.env.prelude_arc(),
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
                if dec.rows[id].is_some() {
                    continue;
                }
                if !deps[id].iter().all(|&d| dec.rows[d as usize].is_some()) {
                    continue;
                }
                let mut entries = crate::types::BindingMap::default();
                for (k, b) in &scope_table[id] {
                    entries.insert(
                        k.clone(),
                        Binding {
                            value: b.value.clone().into_runtime(&dec)?,
                            scheme: b.scheme.clone().map(Arc::new),
                        },
                    );
                }
                dec.rows[id] = Some(entries);
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
        SerialValue::Ext(SerialClosure::Thunk(t)) => {
            out.insert(t.env.bindings);
        }
        SerialValue::Ext(SerialClosure::Native(n)) => {
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

impl<X> FOValue<X> {
    /// The value's shape, named without quoting it.
    ///
    /// For a diagnostic that must say what arrived where something else was
    /// expected. A value that crossed a seam is foreign and unbounded, and the
    /// text goes on to be read by a model with a finite context, so quoting it
    /// would let the sender choose how much of that context to spend. Structure
    /// is what a shape error is about, so structure is all this renders.
    #[must_use]
    pub fn shape(&self) -> String {
        match self {
            Self::Unit => "()".to_string(),
            Self::Bool { .. } => "a Bool".to_string(),
            Self::Int { .. } => "an Int".to_string(),
            Self::Float { .. } => "a Float".to_string(),
            Self::String { .. } => "a Str".to_string(),
            Self::Bytes { value } => format!("{} bytes", value.len()),
            Self::List { items } => format!("a list of {}", plural(items.len(), "element")),
            Self::Map { entries } => format!("a record of {}", plural(entries.len(), "field")),
            // The label is the host's own alphabet, not caller-supplied text of
            // arbitrary size, so naming it costs nothing and is what a wrong tag
            // needs to hear.
            Self::Variant {
                label,
                payload: None,
            } => format!("the bare tag `{label}`"),
            Self::Variant {
                label,
                payload: Some(p),
            } => format!("`{label}` carrying {}", p.shape()),
            Self::Ext(_) => "a value that is not first-order".to_string(),
        }
    }
}

/// `n` of `noun`, agreeing in number: the one spelling of a count in prose,
/// shared with the type errors' own sentences in `typecheck::explain`.
pub(crate) fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

// ── Value conversions ─────────────────────────────────────────────────────

impl FOValue<SerialClosure> {
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
            Value::Thunk(closure) => Self::Ext(SerialClosure::Thunk(SerialThunk {
                comp: Arc::clone(&closure.comp),
                env: SerialEnvSnapshot::from_runtime(&closure.env, ctx),
            })),
            Value::Native { entry, applied } => Self::Ext(SerialClosure::Native(SerialNative {
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
            Self::Ext(SerialClosure::Thunk(thunk)) => Value::Thunk(crate::types::Closure {
                comp: thunk.comp,
                env: thunk.env.into_runtime(dec)?,
            }),
            Self::Ext(SerialClosure::Native(n)) => {
                // The value half only: no `Value::Native` was ever built from
                // a base frame, so a wire name that reaches one is not a
                // native we could rebuild.
                let entry = dec.manifest.value(&n.name).ok_or_else(|| {
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

    /// Recursion over the data variants *is* the protocol's first-orderness
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
            Value::Thunk(_) | Value::Native { .. } | Value::Handle(_) => {
                return Err(Error::new(
                    "value is not first-order: the protocol carries only data, \
                     not closures or handles",
                    1,
                ));
            }
        })
    }
}

/// The label a placeholder carries — a `Variant`, never a bare string, so no
/// genuine string can impersonate one.
pub const OPAQUE_TAG: &str = "opaque";

/// The leaves [`FOValue::try_from`] rejects.
pub fn no_wire_form(v: &Value) -> bool {
    matches!(v, Value::Handle(_) | Value::Thunk(_) | Value::Native { .. })
}

/// Just the leaf a `Handle` has no wire form at all: unlike [`no_wire_form`],
/// a closure is not one of them, since a fork or a wire seed keeps closures
/// rich rather than scrubbing them.
pub(crate) fn is_handle(v: &Value) -> bool {
    matches!(v, Value::Handle(_))
}

/// Recursively replace every leaf `p` accepts with `` `opaque {type: …} ``.
///
/// Every other leaf crosses untouched.  The seams differ only in `p`: a flat
/// wire scrubs closures as well as handles, the fragment wire keeps them,
/// since they intern against its scope table and decode back live.
pub fn scrub(v: &Value, p: &impl Fn(&Value) -> bool) -> Value {
    if p(v) {
        return Value::Variant {
            label: OPAQUE_TAG.to_string(),
            payload: Some(Box::new(Value::map(vec![(
                "type".to_string(),
                Value::String(v.type_name().to_lowercase()),
            )]))),
        };
    }
    match v {
        Value::List(items) => Value::list(items.iter().map(|i| scrub(i, p)).collect()),
        Value::Map(entries) => Value::map(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), scrub(v, p)))
                .collect(),
        ),
        Value::Variant { label, payload } => Value::Variant {
            label: label.clone(),
            payload: payload.as_ref().map(|q| Box::new(scrub(q, p))),
        },
        other => other.clone(),
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
    /// Intern `env`'s session tier into `ctx`, recording its row id.
    /// Infallible: interning reserves the id, and any encoding failure
    /// surfaces at [`InternCtx::finish`].
    pub fn from_runtime(env: &Env, ctx: &mut InternCtx) -> Self {
        Self {
            bindings: ctx.intern_env(env.bindings_root()),
        }
    }

    /// Rebuild an [`Env`] from this snapshot's row, seated under `dec`'s
    /// natives and prelude — the receiver's own, since neither tier rides
    /// the wire.
    ///
    /// # Errors
    /// The recorded row id is out of range or unresolved.
    pub fn into_runtime(self, dec: &WireDecoder) -> Result<Env, Error> {
        let bindings = dec
            .rows
            .get(self.bindings as usize)
            .and_then(std::clone::Clone::clone)
            .ok_or_else(|| {
                Error::new(
                    format!("serial: scope ref {} out of range or unresolved", self.bindings),
                    1,
                )
            })?;
        Ok(Env::from_parts(
            Arc::clone(&dec.natives),
            Arc::clone(&dec.prelude),
            bindings,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::CompKind;

    /// The child reads the checker's verdicts off the node rather than
    /// re-inferring, so a lambda's comp must carry its interior annotations
    /// across the wire — a pipeline's yield marker, and the `Capture` node a
    /// byte-payload `Bind` RHS elaborates to. There is no thunk-root
    /// annotation, so those interior slots are the whole of it.
    fn annotated_lambda_node() -> Arc<Comp> {
        let src = r"let f = { |x| let y = /bin/echo $x; /bin/cat | /bin/cat }";
        let ast = crate::parse(src).expect("parse");
        let top = crate::elaborate(&ast, HashSet::default(), "").expect("elaborate");
        let annotated =
            crate::typecheck(&top, crate::SessionSchemes::default()).expect("typecheck");
        let [phrase] = annotated.phrases.as_slice() else {
            panic!("expected one phrase, got {:?}", annotated.phrases);
        };
        let crate::ir::Phrase::Define { comp, .. } = &phrase.item else {
            panic!("expected a Define phrase, got {:?}", phrase.item);
        };
        find_lam_node(comp).expect("lambda in annotated comp")
    }

    /// Depth-first search for the first `Lam` node, cloning its `Arc` rather
    /// than a caller-picked field — a `SerialThunk` wraps the whole comp
    /// `close` would have, not just its body.
    fn find_lam_node(comp: &Arc<Comp>) -> Option<Arc<Comp>> {
        if matches!(comp.item, CompKind::Lam { .. }) {
            return Some(Arc::clone(comp));
        }
        match &comp.item {
            CompKind::Chain(parts) => parts.iter().find_map(find_lam_node),
            CompKind::Pipeline { stages, .. } => stages.iter().find_map(find_lam_node),
            CompKind::Bind {
                comp: rhs, rest, ..
            } => find_lam_node(rhs).or_else(|| find_lam_node(rest)),
            CompKind::App { head, .. } => find_lam_node(head),
            CompKind::If { then, else_, .. } => {
                find_lam_node(then).or_else(|| find_lam_node(else_))
            }
            CompKind::Force(crate::ir::Val::Thunk(c))
            | CompKind::Return(crate::ir::Val::Thunk(c))
            | CompKind::Capture(c)
            | CompKind::Decode(c) => find_lam_node(c),
            _ => None,
        }
    }

    /// Visit every `Comp` in the tree, descending into thunk bodies so a
    /// lambda nested inside a `{ … }` block is reached.
    fn walk_comp(comp: &Comp, visit: &mut impl FnMut(&Comp)) {
        visit(comp);
        let mut sub = |c: &Arc<Comp>| walk_comp(c, visit);
        match &comp.item {
            CompKind::Chain(parts) => parts.iter().for_each(&mut sub),
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
            | CompKind::Return(crate::ir::Val::Thunk(c))
            | CompKind::Capture(c)
            | CompKind::Decode(c) => walk_comp(c, visit),
            _ => {}
        }
    }

    /// `(a unit-yielding pipeline, a `Capture` node)`. The elaborator never
    /// emits either, so finding one is proof the checker wrote it.
    fn interior_annotations(body: &Comp) -> (bool, bool) {
        let (mut unit_yield, mut capture) = (false, false);
        walk_comp(body, &mut |c| match &c.item {
            CompKind::Pipeline {
                yields: crate::ir::PipeYield::Unit,
                ..
            } => {
                unit_yield = true;
            }
            CompKind::Capture(_) => capture = true,
            _ => {}
        });
        (unit_yield, capture)
    }

    #[test]
    fn lambda_body_round_trips_with_interior_annotations() {
        let node = annotated_lambda_node();
        let (unit_yield, capture) = interior_annotations(&node);
        assert!(
            unit_yield,
            "body's pipeline yields unit, not its last value"
        );
        assert!(
            capture,
            "body's byte-payload bind RHS carries a Capture node"
        );

        let lambda = SerialValue::Ext(SerialClosure::Thunk(SerialThunk {
            comp: Arc::clone(&node),
            env: SerialEnvSnapshot { bindings: 0 },
        }));

        // The same `serde_json` codec `subprocess_codec` frames with.
        let json = serde_json::to_vec(&lambda).expect("serialise lambda");
        let back: SerialValue = serde_json::from_slice(&json).expect("deserialise lambda");

        let SerialValue::Ext(SerialClosure::Thunk(back)) = back else {
            panic!("round-trip changed the value variant");
        };
        assert_eq!(
            *back.comp, *node,
            "the deserialised comp must equal the original, annotations and all"
        );
        let (unit_yield, capture) = interior_annotations(&back.comp);
        assert!(unit_yield, "the pipeline's yield survives the round-trip");
        assert!(capture, "the Capture node survives the round-trip");
    }

    /// A reference past the end of the table is out of range, not a cycle:
    /// the build must say so rather than blame the fallthrough case.
    #[test]
    fn out_of_range_scope_ref_is_not_reported_as_cyclic() {
        use crate::ir::Val;
        use crate::source::Spanned;
        let lambda = SerialValue::Ext(SerialClosure::Thunk(SerialThunk {
            comp: Arc::new(Spanned::synthetic(CompKind::Return(Val::Unit))),
            env: SerialEnvSnapshot { bindings: 5 },
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

    /// Encoding walks the chain as a queue, so a quarter-megabyte stack
    /// encodes fifty thousand links.  (Regression: `intern_scope` and
    /// `from_runtime` recursed into each other once per link, and a helper
    /// stage died on a few hundred lines of `from-lines`.)
    #[test]
    fn deep_stream_chain_encodes_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let chain = crate::types::deep_block_chain(50_000);
                let mut ctx = InternCtx::new();
                SerialValue::from_runtime(&chain, &mut ctx).expect("encode");
                let table = ctx.finish().expect("finish");
                assert_eq!(table.len(), 50_000, "one scope row per link");
            })
            .expect("spawn")
            .join()
            .expect("a deep chain must encode without exhausting the stack");
    }

    /// The deferred table still decodes: `for_shell` resolves the row
    /// dependencies and `into_runtime` rebuilds every link.
    #[test]
    fn deep_stream_chain_round_trips() {
        let chain = crate::types::deep_block_chain(500);
        let mut ctx = InternCtx::new();
        let ipc = SerialValue::from_runtime(&chain, &mut ctx).expect("encode");
        let table = ctx.finish().expect("finish");
        let dec = WireDecoder::for_shell(&Shell::default(), &table).expect("decoder");
        let back = ipc.into_runtime(&dec).expect("decode");
        let mut depth = 0;
        let mut cur = &back;
        while let Value::Thunk(closure) = cur {
            depth += 1;
            cur = closure.env.get("tail").expect("each link binds the next");
        }
        assert_eq!(depth, 500, "every link survives the round-trip");
    }

    #[test]
    fn ipc_value_roundtrips_simple_values() {
        let value = Value::map(vec![
            ("a".into(), Value::Int(1)),
            ("b".into(), Value::String("x".into())),
        ]);
        let mut ctx = InternCtx::new();
        let ipc = SerialValue::from_runtime(&value, &mut ctx).expect("to serial");
        let table = ctx.finish().expect("finish");
        let dec = WireDecoder::for_shell(&Shell::default(), &table).expect("decoder");
        assert_eq!(ipc.into_runtime(&dec).expect("from serial"), value);
    }

    /// A pipeline-stage request built from a shell with the prelude
    /// installed carries no prelude binding in its table: the prelude is a
    /// constant tier every process rebuilds by running the same source, not
    /// a row on the wire (§6.2).
    #[test]
    fn pipeline_stage_request_carries_no_prelude_binding() {
        let mut shell = crate::boot::boot_shell(
            crate::io::TerminalState::default(),
            &crate::boot::BakedPrelude::bake_runtime(),
            &crate::boot::HostSurface::default(),
        );
        shell.env.bind(
            "only_mine".to_string(),
            Binding {
                value: Value::Int(1),
                scheme: None,
            },
        );

        let mut ctx = InternCtx::new();
        let _ = SerialEnvSnapshot::from_runtime(&shell.env, &mut ctx);
        let table = ctx.finish().expect("finish");

        assert_eq!(table.len(), 1, "one row: the session tier alone");
        let names: Vec<&str> = table[0].iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["only_mine"],
            "the prelude's own bindings never ride the wire"
        );
    }
}
