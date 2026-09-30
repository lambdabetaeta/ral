//! Serialisable mirrors of `Value` and `Env`.
//!
//! [`FOValue`] is first-order by construction — data all the way down — over
//! an extension slot uninhabited by default ([`NoExt`]).  [`SerialValue`]
//! fills the slot with [`Closure`] so the re-exec'd engine child IPC
//! (`engine_seed`, `subprocess`, `hatch`) can ship a captured environment
//! as JSON.  `serial.rs` interns *environments*, one row per distinct
//! allocation, by [`Env::ptr_eq`] identity ([`InternCtx`]), rebuilt
//! topologically ([`WireDecoder::for_shell`]).  Σ never crosses: a captured
//! environment carries only ρ.  `into_runtime`, given a [`WireDecoder`], is
//! the sole wire→runtime conversion.

pub mod datum;

use crate::ir::{Comp, Name};
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
#[serde(rename_all = "snake_case")]
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
/// One wire shape for the one runtime thunk value: `Comp::arrow`
/// on the decoded `comp` tells `Lambda` from `Block` back apart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum SerialClosure {
    Thunk(SerialThunk),
    Native(SerialNative),
}

/// [`FOValue`] with closures, for the re-exec'd child IPC.  A `Handle` has no
/// wire form; encoding one is an error.
pub(crate) type SerialValue = FOValue<SerialClosure>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SerialThunk {
    pub comp: Arc<Comp>,
    pub env: SerialEnvSnapshot,
}

/// Wire mirror of a [`Value::Native`]: the body
/// cannot cross, so hydration re-links the name against the receiving
/// shell's manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SerialNative {
    pub name: std::string::String,
    pub(crate) applied: Vec<SerialValue>,
}

/// An [`Env`] in wire form: the [`ScopeTable`] row holding its session
/// tier, resolved against the table carried on the enclosing
/// request/response envelope.
///
/// The natives and prelude tiers never ride the wire, so they need no row
/// of their own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SerialEnvSnapshot {
    pub(crate) bindings: u32,
}

/// Wire mirror of a [`Binding`]: the value converted, the scheme as itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SerialBinding {
    pub value: SerialValue,
    pub(crate) scheme: Option<crate::typecheck::Scheme>,
}

/// One row per interned environment, in discovery order;
/// [`WireDecoder::for_shell`] rebuilds it into session-tier maps on the
/// receiving side.
pub(crate) type ScopeTable = Vec<Vec<(String, SerialBinding)>>;

// ── Interning context ─────────────────────────────────────────────────────
//
// Environment references are unordered: an environment may hold a closure
// whose captured env points at a row interned before or after it.
// `WireDecoder::for_shell` therefore sorts by dependency rather than trusting
// id order.

pub(crate) struct InternCtx {
    scope_table: ScopeTable,
    /// Every root interned in this message so far, scanned linearly by
    /// [`Env::ptr_eq`]: roots number about the message's closures, so
    /// interning is O(roots²), as a stream chain always made it.
    roots: Vec<(Env, u32)>,
    /// Rows with an id but no encoding yet.  A stream is a chain of closures —
    /// block → captured env → binding → block — so encoding an environment's
    /// bindings inside `intern_env` would recurse once per link and bound
    /// stream length by the stack.  Interning only *reserves*; [`Self::finish`]
    /// encodes from this queue, a worklist in place of that recursion.
    pending: Vec<(u32, Env)>,
}

impl InternCtx {
    pub fn new() -> Self {
        Self {
            scope_table: Vec::new(),
            roots: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Intern `env` by its persistent allocation's identity, reserving a row
    /// for [`Self::finish`] to encode.  Σ never rides along: only `env`
    /// itself, ρ, is interned.
    fn intern_env(&mut self, env: &Env) -> u32 {
        if let Some(&(_, id)) = self.roots.iter().find(|(root, _)| root.ptr_eq(env)) {
            return id;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "serialised row id; a message's environments number far below 2^32"
        )]
        let id = self.scope_table.len() as u32;
        self.roots.push((env.clone(), id));
        self.scope_table.push(Vec::new()); // reserve the row; `finish` fills it
        self.pending.push((id, env.clone()));
        id
    }

    /// Encode every pending environment's bindings — each may intern further
    /// environments, which join the queue rather than the stack — and yield
    /// the table.  Nothing ships without passing through here: the table has
    /// no other accessor.
    ///
    /// # Errors
    /// A binding's value reaches a handle, which [`Shell::fork_scrubbed`]
    /// should have scrubbed before any shell reached this wire.
    pub(crate) fn finish(mut self) -> Result<ScopeTable, Error> {
        while let Some((id, bindings)) = self.pending.pop() {
            let mut entries = Vec::with_capacity(bindings.len());
            for (k, b) in bindings.iter() {
                entries.push((
                    k.to_string(),
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

/// One wording for the one fault, raised from both the ordering pass and the
/// rehydration that reads the table it built.
fn unresolved_scope_ref(id: u32) -> Error {
    Error::new(
        format!("serial: scope ref {id} out of range or unresolved"),
        1,
    )
}

/// One reconstructed environment per scope table row (`None` until built).
type EnvRows = Vec<Option<Env>>;

/// Decode capability for one wire envelope: the rebuilt environment rows,
/// and the [`BuiltinTable`] a captured [`Value::Native`] re-links its name
/// against.  Σ never rides the wire, so no tier crosses here either.
///
/// Constructible only from the [`Shell`] that will run the decoded values,
/// so no call site can pick a manifest of its own.
#[derive(Debug)]
pub(crate) struct WireDecoder {
    rows: EnvRows,
    manifest: BuiltinTable,
}

impl WireDecoder {
    /// Rebuild one environment per row of `scope_table`, each once its
    /// dependencies are built.
    ///
    /// # Errors
    /// A row reference out of range or unresolved, a binding that fails to
    /// decode, or a cycle — a pass in which no row makes progress.
    pub(crate) fn for_shell(shell: &Shell, scope_table: &ScopeTable) -> Result<Self, Error> {
        let n = scope_table.len();
        let mut dec = Self {
            rows: vec![None; n],
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
                    return Err(unresolved_scope_ref(d));
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
                let mut pairs = Vec::with_capacity(scope_table[id].len());
                for (k, b) in &scope_table[id] {
                    pairs.push((
                        Name::from(k.as_str()),
                        Binding {
                            value: b.value.clone().into_runtime(&dec)?,
                            scheme: b.scheme.clone().map(Arc::new),
                        },
                    ));
                }
                let mut env = Env::new();
                env.extend(pairs.into_iter());
                dec.rows[id] = Some(env);
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
#[cfg_attr(not(any(unix, test)), allow(dead_code))]
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

    /// A record's field; `None` on an absent key or a value that is no record.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Map { entries } => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String { value } => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int { value } => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool { value } => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes { value } => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_list(&self) -> Option<&[Self]> {
        match self {
            Self::List { items } => Some(items),
            _ => None,
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
    pub(crate) fn from_runtime(value: &Value, ctx: &mut InternCtx) -> Result<Self, Error> {
        Ok(match value {
            Value::Unit => Self::Unit,
            Value::Bool(v) => Self::Bool { value: *v },
            Value::Int(v) => Self::Int { value: *v },
            Value::Float(v) => Self::Float { value: *v },
            Value::String(v) => Self::String {
                value: v.to_string(),
            },
            Value::Bytes(v) => Self::Bytes { value: v.to_vec() },
            Value::List(items) => Self::List {
                items: items
                    .iter()
                    .map(|v| Self::from_runtime(&v, ctx))
                    .collect::<Result<_, _>>()?,
            },
            Value::Map(items) => Self::Map {
                entries: items
                    .iter()
                    .map(|(k, v)| Ok((k.to_string(), Self::from_runtime(&v, ctx)?)))
                    .collect::<Result<_, Error>>()?,
            },
            Value::Variant { label, payload } => Self::Variant {
                label: label.to_string(),
                payload: match payload {
                    Some(p) => Some(Box::new(Self::from_runtime(p, ctx)?)),
                    None => None,
                },
            },
            Value::Thunk(closure) => Self::Ext(SerialClosure::Thunk(SerialThunk {
                comp: Arc::clone(closure.comp()),
                env: SerialEnvSnapshot::from_runtime(closure.env(), ctx),
            })),
            Value::Native { entry, applied } => Self::Ext(SerialClosure::Native(SerialNative {
                name: entry.name.as_ref().to_string(),
                applied: applied
                    .iter()
                    .map(|v| Self::from_runtime(v, ctx))
                    .collect::<Result<_, _>>()?,
            })),
            Value::Handle(_) => {
                return Err(Error::new(
                    "a handle reached the seed wire, though `Shell::fork_scrubbed` should have \
                     replaced it: this is a fault in ral rather than in your program — please \
                     report the source that produced it",
                    1,
                ));
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
    pub(crate) fn into_runtime(self, dec: &WireDecoder) -> Result<Value, Error> {
        Ok(match self {
            Self::Unit => Value::Unit,
            Self::Bool { value } => Value::Bool(value),
            Self::Int { value } => Value::Int(value),
            Self::Float { value } => Value::Float(value),
            Self::String { value } => Value::string(value),
            Self::Bytes { value } => Value::bytes(value),
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
                label: label.into(),
                payload: match payload {
                    Some(p) => Some(Box::new((*p).into_runtime(dec)?)),
                    None => None,
                },
            },
            Self::Ext(SerialClosure::Thunk(thunk)) => Value::Thunk(
                crate::types::Closure::captured(thunk.comp, thunk.env.into_runtime(dec)?),
            ),
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

/// A leaf that is not data: what a seam scrubs to an `` `opaque `` placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opaque {
    Block,
    Function,
    Handle,
}

impl Opaque {
    #[must_use]
    pub fn of(v: &Value) -> Option<Self> {
        match v {
            Value::Thunk(c) if c.comp().arrow().is_none() => Some(Self::Block),
            Value::Thunk(_) | Value::Native { .. } => Some(Self::Function),
            Value::Handle(_) => Some(Self::Handle),
            _ => None,
        }
    }
}

impl std::fmt::Display for Opaque {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Block => "a block",
            Self::Function => "a function",
            Self::Handle => "a handle",
        })
    }
}

/// Why a value has no [`FOValue`]: the first opaque leaf met, and whether it
/// sat inside a list, map, or variant rather than being the value itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotData {
    pub leaf: Opaque,
    pub nested: bool,
}

impl TryFrom<&Value> for FOValue {
    type Error = NotData;

    /// Recursion over the data variants *is* the protocol's first-orderness
    /// check, rather than a separate test followed by a hopeful re-encode.
    fn try_from(v: &Value) -> Result<Self, NotData> {
        if let Some(leaf) = Opaque::of(v) {
            return Err(NotData {
                leaf,
                nested: false,
            });
        }
        let inner = |v: &Value| Self::try_from(v).map_err(|e| NotData { nested: true, ..e });
        Ok(match v {
            Value::Unit => Self::Unit,
            Value::Bool(v) => Self::Bool { value: *v },
            Value::Int(v) => Self::Int { value: *v },
            Value::Float(v) => Self::Float { value: *v },
            Value::String(v) => Self::String {
                value: v.to_string(),
            },
            Value::Bytes(v) => Self::Bytes { value: v.to_vec() },
            Value::List(items) => Self::List {
                items: items.iter().map(|v| inner(&v)).collect::<Result<_, _>>()?,
            },
            Value::Map(items) => Self::Map {
                entries: items
                    .iter()
                    .map(|(k, v)| Ok((k.to_string(), inner(&v)?)))
                    .collect::<Result<_, NotData>>()?,
            },
            Value::Variant { label, payload } => Self::Variant {
                label: label.to_string(),
                payload: payload.as_deref().map(inner).transpose()?.map(Box::new),
            },
            Value::Thunk(_) | Value::Native { .. } | Value::Handle(_) => {
                unreachable!("Opaque::of answers for every leaf that is not data")
            }
        })
    }
}

/// The label a placeholder carries — a `Variant`, never a bare string, so no
/// genuine string can impersonate one.
pub(crate) const OPAQUE_TAG: &str = "opaque";

/// `` `opaque [type: …] ``: what a scrubbed leaf crosses as, and what a fork
/// holds where a handle stood.
pub(crate) fn opaque(v: &Value) -> FOValue {
    FOValue::Variant {
        label: OPAQUE_TAG.to_string(),
        payload: Some(Box::new(FOValue::Map {
            entries: vec![(
                "type".to_string(),
                FOValue::String {
                    value: v.type_name().to_lowercase(),
                },
            )],
        })),
    }
}

impl FOValue {
    /// [`FOValue::try_from`] made total: every leaf that is not data crosses
    /// as its `` `opaque `` placeholder, as the flat wire needs.
    pub(crate) fn scrubbed(v: &Value) -> Self {
        match v {
            Value::Unit => Self::Unit,
            Value::Bool(v) => Self::Bool { value: *v },
            Value::Int(v) => Self::Int { value: *v },
            Value::Float(v) => Self::Float { value: *v },
            Value::String(v) => Self::String {
                value: v.to_string(),
            },
            Value::Bytes(v) => Self::Bytes { value: v.to_vec() },
            Value::List(items) => Self::List {
                items: items.iter().map(|v| Self::scrubbed(&v)).collect(),
            },
            Value::Map(items) => Self::Map {
                entries: items
                    .iter()
                    .map(|(k, v)| (k.to_string(), Self::scrubbed(&v)))
                    .collect(),
            },
            Value::Variant { label, payload } => Self::Variant {
                label: label.to_string(),
                payload: payload.as_deref().map(|p| Box::new(Self::scrubbed(p))),
            },
            Value::Thunk(_) | Value::Native { .. } | Value::Handle(_) => opaque(v),
        }
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
            FOValue::String { value } => Self::string(value),
            FOValue::Bytes { value } => Self::bytes(value),
            FOValue::List { items } => Self::list(items.into_iter().map(Self::from).collect()),
            FOValue::Map { entries } => Self::Map(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, Self::from(v)))
                    .collect(),
            ),
            FOValue::Variant { label, payload } => Self::Variant {
                label: label.into(),
                payload: payload.map(|p| Box::new(Self::from(*p))),
            },
            FOValue::Ext(x) => match x {},
        }
    }
}

impl SerialEnvSnapshot {
    /// Intern `env` into `ctx`, recording its row id.  Infallible: interning
    /// reserves the id, and any encoding failure surfaces at
    /// [`InternCtx::finish`].
    pub(crate) fn from_runtime(env: &Env, ctx: &mut InternCtx) -> Self {
        Self {
            bindings: ctx.intern_env(env),
        }
    }

    /// Rebuild the [`Env`] this snapshot names, from `dec`'s row.
    ///
    /// # Errors
    /// The recorded row id is out of range or unresolved.
    pub(crate) fn into_runtime(self, dec: &WireDecoder) -> Result<Env, Error> {
        dec.rows
            .get(self.bindings as usize)
            .and_then(std::clone::Clone::clone)
            .ok_or_else(|| unresolved_scope_ref(self.bindings))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::CompKind;

    /// The child reads the checker's verdicts off the node rather than
    /// re-inferring, so a lambda's comp must carry its interior annotations
    /// across the wire — the `Capture` node a `let` of a command elaborates to.
    /// There is no thunk-root annotation, so those interior slots are the whole
    /// of it.
    fn annotated_lambda_node() -> Arc<Comp> {
        let src = r"let f = { |x| let y = /bin/echo $x; /bin/cat | /bin/cat }";
        let ast = crate::parse(src).expect("parse");
        let top = crate::elaborate(&ast, HashSet::default(), "").expect("elaborate");
        let annotated =
            crate::typecheck(&top, crate::SessionSchemes::default(), None).expect("typecheck");
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
            CompKind::Pipeline { stages, .. } => stages.iter().find_map(find_lam_node),
            CompKind::Bind {
                comp: rhs, rest, ..
            } => find_lam_node(rhs).or_else(|| find_lam_node(rest)),
            CompKind::App { head, .. } => find_lam_node(head),
            CompKind::If { then, else_, .. } => {
                [then, else_].into_iter().find_map(|arm| match &arm.item {
                    crate::ir::Val::Thunk(node) => find_lam_node(node.shape()),
                    _ => None,
                })
            }
            CompKind::Force(crate::ir::Val::Thunk(node))
            | CompKind::Return(crate::ir::Val::Thunk(node)) => find_lam_node(node.shape()),
            CompKind::Capture(c) => find_lam_node(c),
            _ => None,
        }
    }

    /// Visit every `Comp` in the tree, descending into thunk bodies so a
    /// lambda nested inside a `{ … }` block is reached.
    fn walk_comp(comp: &Comp, visit: &mut impl FnMut(&Comp)) {
        visit(comp);
        let mut sub = |c: &Arc<Comp>| walk_comp(c, visit);
        match &comp.item {
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
                for arm in [then, else_] {
                    if let crate::ir::Val::Thunk(node) = &arm.item {
                        walk_comp(node.shape(), visit);
                    }
                }
            }
            CompKind::Force(crate::ir::Val::Thunk(node))
            | CompKind::Return(crate::ir::Val::Thunk(node)) => walk_comp(node.shape(), visit),
            CompKind::Capture(c) => walk_comp(c, visit),
            _ => {}
        }
    }

    /// Whether a `Capture` node is in the tree.  The elaborator never emits one,
    /// so finding it is proof the checker wrote it.
    fn has_capture(body: &Comp) -> bool {
        let mut capture = false;
        walk_comp(body, &mut |c| {
            if matches!(c.item, CompKind::Capture(_)) {
                capture = true;
            }
        });
        capture
    }

    #[test]
    fn lambda_body_round_trips_with_interior_annotations() {
        let node = annotated_lambda_node();
        assert!(
            has_capture(&node),
            "body's let of a command carries a Capture node"
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
        assert!(
            has_capture(&back.comp),
            "the Capture node survives the round-trip"
        );
    }

    /// Every boundary `Exec` under `comp`, in walk order.
    fn boundary_sites(comp: &Comp) -> Vec<Arc<crate::types::Site>> {
        let mut sites = Vec::new();
        walk_comp(comp, &mut |c| {
            if let CompKind::Exec(exec) = &c.item
                && let Some(site) = &exec.site
            {
                sites.push(Arc::clone(site));
            }
        });
        sites
    }

    /// The child admits as the parent would: a thunk ships the sites of its
    /// IR, and the sites of one unit meet the one `Fixings` there, so two
    /// decodes the program compares still agree in the engine child.
    #[test]
    fn a_thunk_ships_its_boundary_sites_and_their_shared_fixings() {
        let src = "let f = { |p|\n  let a = from-json < $p\n  let b = from-json < $p\n  return $[$a[x] < $b[x]]\n}";
        let ast = crate::parse(src).expect("parse");
        let top = crate::elaborate(&ast, HashSet::default(), "").expect("elaborate");
        let annotated =
            crate::typecheck(&top, crate::SessionSchemes::default(), None).expect("typecheck");
        let [phrase] = annotated.phrases.as_slice() else {
            panic!("expected one phrase, got {:?}", annotated.phrases);
        };
        let crate::ir::Phrase::Define { comp, .. } = &phrase.item else {
            panic!("expected a Define phrase, got {:?}", phrase.item);
        };
        let node = find_lam_node(comp).expect("lambda in annotated comp");
        let sent = boundary_sites(&node);
        assert_eq!(sent.len(), 2, "each decode carries its site");

        let lambda = SerialValue::Ext(SerialClosure::Thunk(SerialThunk {
            comp: Arc::clone(&node),
            env: SerialEnvSnapshot { bindings: 0 },
        }));
        let json = serde_json::to_vec(&lambda).expect("serialise lambda");
        let back: SerialValue = serde_json::from_slice(&json).expect("deserialise lambda");
        let SerialValue::Ext(SerialClosure::Thunk(back)) = back else {
            panic!("round-trip changed the value variant");
        };
        assert_eq!(*back.comp, *node, "sites survive the wire");
        let received = boundary_sites(&back.comp);
        assert!(
            received[0].shares_fixings_with(&received[1]),
            "the unit's sites meet one Fixings in the child"
        );
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
    /// encodes fifty thousand links.  (Regression: `intern_env` and
    /// `from_runtime` recursed into each other once per link, and a helper
    /// stage died on a lazy list a few hundred closures long.)
    #[test]
    fn deep_stream_chain_encodes_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let chain = crate::types::deep_block_chain(50_000, Value::Unit);
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
        let chain = crate::types::deep_block_chain(500, Value::Unit);
        let mut ctx = InternCtx::new();
        let ipc = SerialValue::from_runtime(&chain, &mut ctx).expect("encode");
        let table = ctx.finish().expect("finish");
        let dec = WireDecoder::for_shell(&Shell::default(), &table).expect("decoder");
        let back = ipc.into_runtime(&dec).expect("decode");
        let mut depth = 0;
        let mut cur = &back;
        while let Value::Thunk(closure) = cur {
            depth += 1;
            cur = closure.env().get("tail").expect("each link binds the next");
        }
        assert_eq!(depth, 500, "every link survives the round-trip");
    }

    #[test]
    fn ipc_value_roundtrips_simple_values() {
        let value = Value::map(vec![
            ("a".into(), Value::Int(1)),
            ("b".into(), Value::string("x")),
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
    /// a row on the wire.
    #[test]
    fn pipeline_stage_request_carries_no_prelude_binding() {
        let mut shell = crate::boot::boot_shell(
            crate::io::TerminalState::default(),
            &crate::boot::BakedPrelude::bake_runtime(),
            &crate::boot::HostSurface::default(),
        );
        shell.env.bind(
            "only_mine".into(),
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
