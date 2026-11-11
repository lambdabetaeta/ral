//! The seed's scope table: serialisable mirrors of `Value` and `Env`.
//!
//! [`SerialValue`] fills [`FOValue`]'s extension slot with closures so the
//! sealed child ([`super::EngineSeed`]) can ship a
//! captured environment as JSON.  This module interns *environments*, one row
//! per distinct allocation, by [`Env::ptr_eq`] identity ([`InternCtx`]),
//! rebuilt topologically ([`WireDecoder::for_runtime`]).  Σ never crosses: a
//! captured environment carries only ρ.  `into_runtime`, given a
//! [`WireDecoder`], is the sole wire→runtime conversion.

use crate::data::FOValue;
use crate::data::json::{drop_value, on_json_stack};
use crate::fact::{InvariantFailure, ProtocolFailure, Reason};
use crate::ir::{Comp, Name};
use crate::shell::runtime::{Binding, BuiltinTable, Closure, Env, Error, Runtime, Value};
use crate::ty::Closed;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;

/// What the re-exec'd child IPC adds to [`FOValue`]: closures over interned
/// scopes.
///
/// One wire shape for the one runtime thunk value.
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
    pub(super) comp: Arc<Comp>,
    pub env: SerialEnvSnapshot,
}

/// Wire mirror of a [`Value::Native`]: the body
/// cannot cross, so hydration re-links the name against the receiving
/// shell's manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SerialNative {
    pub name: std::string::String,
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
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SerialBinding {
    pub value: SerialValue,
    pub(super) scheme: Option<Closed>,
}

impl Drop for SerialBinding {
    fn drop(&mut self) {
        drop_value(std::mem::replace(&mut self.value, SerialValue::Unit));
    }
}

/// One row per interned environment, in discovery order;
/// [`WireDecoder::for_runtime`] rebuilds it into session-tier maps on the
/// receiving side.
pub(crate) type ScopeTable = Vec<Vec<(String, SerialBinding)>>;

// ── Interning context ─────────────────────────────────────────────────────
//
// Environment references are unordered: an environment may hold a closure
// whose captured env points at a row interned before or after it.
// `WireDecoder::for_runtime` therefore sorts by dependency rather than trusting
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
    /// A binding's value reaches a handle, which [`crate::Shell::bequeath`]
    /// should have scrubbed before any shell reached this wire.
    pub(crate) fn finish(mut self) -> Result<ScopeTable, Error> {
        while let Some((id, bindings)) = self.pending.pop() {
            self.scope_table[id as usize] = bindings
                .iter()
                .map(|(k, b)| {
                    Ok((
                        k.to_string(),
                        SerialBinding {
                            value: SerialValue::from_runtime(&b.value, &mut self)?,
                            scheme: b.scheme.clone(),
                        },
                    ))
                })
                .collect::<Result<_, Error>>()?;
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
    Error::new(Reason::Protocol(Box::new(
        ProtocolFailure::UnresolvedScope(u64::from(id).into()),
    )))
}

/// One reconstructed environment per scope table row (`None` until built).
type EnvRows = Vec<Option<Env>>;

/// Decode capability for one wire envelope: the rebuilt environment rows,
/// and the [`BuiltinTable`] a captured [`Value::Native`] re-links its name
/// against.  Σ never rides the wire, so no tier crosses here either.
///
/// Constructible only from the [`Runtime`] that will run the decoded values,
/// so no call site can pick a manifest of its own.
#[derive(Debug)]
pub(crate) struct WireDecoder {
    rows: EnvRows,
    manifest: BuiltinTable,
}

impl Drop for WireDecoder {
    fn drop(&mut self) {
        for env in self.rows.drain(..).flatten() {
            env.drop_seed();
        }
    }
}

/// Completed bindings remain owned until their row is ready to transfer.
#[derive(Default)]
struct SeedBindings(Vec<(Name, Binding)>);

impl Drop for SeedBindings {
    fn drop(&mut self) {
        for (_, Binding { value, .. }) in self.0.drain(..) {
            value.drop_seed();
        }
    }
}

impl WireDecoder {
    /// Rebuild one environment per row of `scope_table`, each once its
    /// dependencies are built: Kahn's algorithm, so a stream chain decodes in
    /// time linear in its length.
    ///
    /// # Errors
    /// A row reference out of range or unresolved, a binding that fails to
    /// decode, or a cycle — rows left waiting when no row is ready.
    pub(crate) fn for_runtime(runtime: &Runtime, scope_table: &ScopeTable) -> Result<Self, Error> {
        let n = scope_table.len();
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
        let mut dependents = vec![Vec::new(); n];
        for (id, set) in deps.iter().enumerate() {
            for &d in set {
                dependents
                    .get_mut(d as usize)
                    .ok_or_else(|| unresolved_scope_ref(d))?
                    .push(id);
            }
        }
        let mut waiting: Vec<usize> = deps.iter().map(HashSet::len).collect();
        let mut ready: Vec<usize> = (0..n).filter(|&id| waiting[id] == 0).collect();
        let mut dec = Self {
            rows: vec![None; n],
            manifest: runtime.builtins.clone(),
        };
        let mut built = 0;
        while let Some(id) = ready.pop() {
            let mut bindings = SeedBindings::default();
            for (name, binding) in &scope_table[id] {
                bindings.0.push((
                    Name::from(name.as_str()),
                    Binding {
                        value: binding.value.clone().into_runtime(&dec)?,
                        scheme: binding.scheme.clone(),
                    },
                ));
            }
            let (env, displaced) = Env::from_seed_bindings(std::mem::take(&mut bindings.0));
            for Binding { value, .. } in displaced {
                value.drop_seed();
            }
            dec.rows[id] = Some(env);
            built += 1;
            for &next in &dependents[id] {
                waiting[next] -= 1;
                if waiting[next] == 0 {
                    ready.push(next);
                }
            }
        }
        if built < n {
            return Err(Error::new(Reason::Protocol(Box::new(
                ProtocolFailure::CyclicScopes,
            ))));
        }
        Ok(dec)
    }
}

/// The match is exhaustive on purpose: a new [`SerialClosure`] variant must
/// declare whether it carries scope references, or its dependency edges go
/// silently missing from [`WireDecoder::for_runtime`].
fn collect_scope_deps(value: &SerialValue, out: &mut HashSet<u32>) {
    for_each_closure(value, |closure| match closure {
        SerialClosure::Thunk(thunk) => {
            out.insert(thunk.env.bindings);
        }
        SerialClosure::Native(_) => {}
    });
}

fn for_each_closure(value: &SerialValue, mut visit: impl FnMut(&SerialClosure)) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            SerialValue::List { items } => pending.extend(items.iter().rev()),
            SerialValue::Map { entries } => {
                pending.extend(entries.iter().rev().map(|(_, value)| value));
            }
            SerialValue::Variant { payload, .. } => pending.extend(payload.as_deref()),
            SerialValue::Ext(closure) => visit(closure),
            SerialValue::Unit
            | SerialValue::Bool { .. }
            | SerialValue::Int { .. }
            | SerialValue::Float { .. }
            | SerialValue::String { .. }
            | SerialValue::Bytes { .. } => {}
        }
    }
}

// ── Value conversions ─────────────────────────────────────────────────────

impl FOValue<SerialClosure> {
    /// Encode a value without recursing through captured environments.
    pub(crate) fn from_runtime(value: &Value, ctx: &mut InternCtx) -> Result<Self, Error> {
        on_json_stack(|| match value {
            Value::Unit => Ok(Self::Unit),
            Value::Bool(value) => Ok(Self::Bool { value: *value }),
            Value::Int(value) => Ok(Self::Int { value: *value }),
            Value::Float(value) => Ok(Self::Float { value: *value }),
            Value::String(value) => Ok(Self::String {
                value: value.to_string(),
            }),
            Value::Bytes(value) => Ok(Self::Bytes {
                value: value.to_vec(),
            }),
            Value::List(items) => {
                let mut values = Vec::with_capacity(items.len());
                for value in items {
                    match Self::from_runtime(&value, ctx) {
                        Ok(value) => values.push(value),
                        Err(error) => {
                            values.into_iter().for_each(drop_value);
                            return Err(error);
                        }
                    }
                }
                Ok(Self::List { items: values })
            }
            Value::Map(items) => {
                let mut values = Vec::with_capacity(items.len());
                for (name, value) in items {
                    match Self::from_runtime(&value, ctx) {
                        Ok(value) => values.push((name.to_string(), value)),
                        Err(error) => {
                            for (_, value) in values {
                                drop_value(value);
                            }
                            return Err(error);
                        }
                    }
                }
                Ok(Self::Map { entries: values })
            }
            Value::Variant { label, payload } => Ok(Self::Variant {
                label: label.to_string(),
                payload: payload
                    .as_deref()
                    .map(|value| Self::from_runtime(value, ctx).map(Box::new))
                    .transpose()?,
            }),
            Value::Thunk(closure) => Ok(Self::Ext(SerialClosure::Thunk(SerialThunk {
                comp: Arc::clone(closure.comp()),
                env: SerialEnvSnapshot::from_runtime(closure.env(), ctx),
            }))),
            Value::Native(entry) => Ok(Self::Ext(SerialClosure::Native(SerialNative {
                name: entry.decl.name.as_ref().to_string(),
            }))),
            Value::Handle(_) => Err(Error::new(Reason::Invariant(Box::new(
                InvariantFailure::HandleInSeed,
            )))),
        })
    }

    /// Check all fallible references before building any runtime value.
    pub(crate) fn validate_runtime(&self, dec: &WireDecoder) -> Result<(), Error> {
        let mut result = Ok(());
        for_each_closure(self, |closure| {
            if result.is_err() {
                return;
            }
            result = match closure {
                SerialClosure::Thunk(thunk) => thunk.env.clone().into_runtime(dec).map(|_| ()),
                SerialClosure::Native(native) if dec.manifest.value(&native.name).is_none() => {
                    Err(Error::new(Reason::Protocol(Box::new(
                        ProtocolFailure::UnknownNative(native.name.clone()),
                    ))))
                }
                SerialClosure::Native(_) => Ok(()),
            };
        });
        result
    }

    /// Resolve against the receiving runtime, then dispose the wire spine iteratively.
    pub(crate) fn into_runtime(self, dec: &WireDecoder) -> Result<Value, Error> {
        let result = self.validate_runtime(dec).map(|()| self.hydrate(dec));
        drop_value(self);
        result
    }

    fn hydrate(&self, dec: &WireDecoder) -> Value {
        on_json_stack(|| match self {
            Self::Unit => Value::Unit,
            Self::Bool { value } => Value::Bool(*value),
            Self::Int { value } => Value::Int(*value),
            Self::Float { value } => Value::Float(*value),
            Self::String { value } => Value::string(value.as_str()),
            Self::Bytes { value } => Value::bytes(value.clone()),
            Self::List { items } => {
                Value::list(items.iter().map(|value| value.hydrate(dec)).collect())
            }
            Self::Map { entries } => Value::Map(crate::shell::runtime::Map::from_seed_values(
                entries
                    .iter()
                    .map(|(name, value)| (name.clone(), value.hydrate(dec))),
            )),
            Self::Variant { label, payload } => Value::variant(
                label.as_str(),
                payload.as_deref().map(|value| value.hydrate(dec)),
            ),
            Self::Ext(SerialClosure::Thunk(thunk)) => {
                let env = thunk
                    .env
                    .clone()
                    .into_runtime(dec)
                    .expect("validated scope reference");
                Value::Thunk(Closure::captured(Arc::clone(&thunk.comp), env))
            }
            Self::Ext(SerialClosure::Native(native)) => Value::Native(Arc::new(
                dec.manifest
                    .value(&native.name)
                    .expect("validated native name"),
            )),
        })
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
        let ast = crate::syntax::parser::parse(src).expect("parse");
        let top = crate::elaborator::elaborate(&ast, [], None).expect("elaborate");
        let annotated =
            crate::typecheck(&top, crate::test_helper::core_schemes(), None).expect("typecheck");
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
            CompKind::Pipeline { stages } => stages.iter().find_map(find_lam_node),
            CompKind::Bind {
                comp: rhs, rest, ..
            }
            | CompKind::Seq { first: rhs, rest } => {
                find_lam_node(rhs).or_else(|| find_lam_node(rest))
            }
            CompKind::App { head, .. } => find_lam_node(head),
            CompKind::Prim(prim) => prim.operands().find_map(|value| match value {
                crate::ir::Val::Thunk(node) => find_lam_node(node.shape()),
                _ => None,
            }),
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
            CompKind::Pipeline { stages } => stages.iter().for_each(&mut sub),
            CompKind::Lam { body, .. } => sub(body),
            CompKind::Bind {
                comp: rhs, rest, ..
            }
            | CompKind::Seq { first: rhs, rest } => {
                sub(rhs);
                sub(rest);
            }
            CompKind::App { head, .. } => sub(head),
            CompKind::Prim(prim) => {
                for value in prim.operands() {
                    if let crate::ir::Val::Thunk(node) = value {
                        walk_comp(node.shape(), visit);
                    }
                }
            }
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

        let json = crate::data::json::to_vec(&lambda).expect("serialise lambda");
        let back: SerialValue = crate::data::json::from_slice(&json).expect("deserialise lambda");

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
    fn boundary_sites(comp: &Comp) -> Vec<Arc<crate::ty::Site>> {
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
    /// IR, and the sites of one unit keep their unit there, so two decodes the
    /// program compares still share a fixing in the engine child.
    #[test]
    fn a_thunk_ships_its_boundary_sites_under_one_unit() {
        let src = "let f = { |p|\n  let a = from-json < $p\n  let b = from-json < $p\n  return $[$a[x] < $b[x]]\n}";
        let ast = crate::syntax::parser::parse(src).expect("parse");
        let top = crate::elaborator::elaborate(&ast, [], None).expect("elaborate");
        let annotated =
            crate::typecheck(&top, crate::test_helper::core_schemes(), None).expect("typecheck");
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
        let json = crate::data::json::to_vec(&lambda).expect("serialise lambda");
        let back: SerialValue = crate::data::json::from_slice(&json).expect("deserialise lambda");
        let SerialValue::Ext(SerialClosure::Thunk(back)) = back else {
            panic!("round-trip changed the value variant");
        };
        assert_eq!(*back.comp, *node, "sites survive the wire");
        let received = boundary_sites(&back.comp);
        assert!(
            received[0].same_unit(&received[1]),
            "the unit's sites keep their unit in the child"
        );
        assert_eq!(received, sent, "the received sites equal the sent ones");
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
        let err = WireDecoder::for_runtime(&crate::test_helper::core_runtime(), &table)
            .expect_err("out-of-range ref must fail the build");
        assert_eq!(
            &err.reason,
            &Reason::Protocol(Box::new(ProtocolFailure::UnresolvedScope(5u64.into())))
        );
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
                let chain = crate::shell::runtime::deep_block_chain(50_000, Value::Unit);
                let mut ctx = InternCtx::new();
                SerialValue::from_runtime(&chain, &mut ctx).expect("encode");
                let table = ctx.finish().expect("finish");
                assert_eq!(table.len(), 50_000, "one scope row per link");
            })
            .expect("spawn")
            .join()
            .expect("a deep chain must encode without exhausting the stack");
    }

    /// The deferred table still decodes: `for_runtime` resolves the row
    /// dependencies and `into_runtime` rebuilds every link.
    #[test]
    fn deep_stream_chain_round_trips() {
        let chain = crate::shell::runtime::deep_block_chain(500, Value::Unit);
        let mut ctx = InternCtx::new();
        let ipc = SerialValue::from_runtime(&chain, &mut ctx).expect("encode");
        let table = ctx.finish().expect("finish");
        let dec =
            WireDecoder::for_runtime(&crate::test_helper::core_runtime(), &table).expect("decoder");
        let back = ipc.into_runtime(&dec).expect("decode");
        let mut depth = 0;
        let mut cur = &back;
        while let Value::Thunk(closure) = cur {
            depth += 1;
            cur = closure.env().get("tail").expect("each link binds the next");
        }
        assert_eq!(depth, 500, "every link survives the round-trip");
    }

    /// A pipeline-stage request built from a shell with the prelude
    /// installed carries no prelude binding in its table: the prelude is a
    /// constant tier every process rebuilds by running the same source, not
    /// a row on the wire.
    #[test]
    fn pipeline_stage_request_carries_no_prelude_binding() {
        let mut shell = crate::shell::boot::boot_shell(
            crate::io::terminal::TerminalState::default(),
            crate::shell::boot::BakedPrelude::runtime(),
            &crate::shell::boot::HostSurface::default(),
            crate::test_helper::process_cwd(),
        )
        .expect("a test host resolves its baseline");
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
