//! The seed's scope table: serialisable value closures and environments.
//!
//! [`SerialValue`] fills [`FOValue`]'s extension slot with closures so the
//! sealed child ([`super::EngineSeed`]) can ship a
//! captured environment as JSON.  This module interns *environments*, one row
//! per distinct allocation, by [`Env::ptr_eq`] identity ([`InternCtx`]),
//! rebuilt topologically ([`WireDecoder::for_runtime`]).  Σ never crosses: a
//! captured environment carries only ρ.  `into_runtime`, given a
//! [`WireDecoder`], is the sole wire→runtime conversion.

use crate::builtin::BuiltinTable;
use crate::data::FOValue;
use crate::data::json::{drop_value, on_json_stack};
use crate::fact::{ProtocolFailure, Reason};
use crate::ir::{Comp, Name};
use crate::shell::runtime::{Binding, Env, Error, Runtime, ValClos, View};
use crate::ty::Closed;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;

/// The non-data values child IPC adds to [`FOValue`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum SerialAtom {
    Thunk(SerialThunk),
    Builtin(String),
    Handle(String),
}

pub(crate) type SerialValue = FOValue<SerialAtom>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SerialThunk {
    /// Includes linked modules' checked tops and origins, never load requests.
    pub(super) comp: Arc<Comp>,
    pub env: SerialEnvSnapshot,
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
    pub(crate) fn finish(mut self) -> ScopeTable {
        while let Some((id, bindings)) = self.pending.pop() {
            self.scope_table[id as usize] = bindings
                .iter()
                .map(|(k, b)| {
                    (
                        k.to_string(),
                        SerialBinding {
                            value: SerialValue::from_runtime(&b.value, &mut self),
                            scheme: b.scheme.clone(),
                        },
                    )
                })
                .collect();
        }
        self.scope_table
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
/// and the [`BuiltinTable`] a captured builtin re-links its name
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

/// Every atom declares whether it carries a scope reference.
fn collect_scope_deps(value: &SerialValue, out: &mut HashSet<u32>) {
    for_each_atom(value, |atom| match atom {
        SerialAtom::Thunk(thunk) => {
            out.insert(thunk.env.bindings);
        }
        SerialAtom::Builtin(_) | SerialAtom::Handle(_) => {}
    });
}

fn for_each_atom(value: &SerialValue, mut visit: impl FnMut(&SerialAtom)) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            SerialValue::List { items } => pending.extend(items.iter().rev()),
            SerialValue::Map { entries } => {
                pending.extend(entries.iter().rev().map(|(_, value)| value));
            }
            SerialValue::Variant { payload, .. } => pending.extend(payload.as_deref()),
            SerialValue::Ext(atom) => visit(atom),
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

impl FOValue<SerialAtom> {
    /// Encode a value without recursing through captured environments.
    pub(crate) fn from_runtime(value: &ValClos, ctx: &mut InternCtx) -> Self {
        on_json_stack(|| {
            let (labels, terminal) = value.peel_variants();
            let mut encoded = match terminal.view() {
                View::Unit => Self::Unit,
                View::Bool(value) => Self::Bool { value },
                View::Int(value) => Self::Int { value },
                View::Float(value) => Self::Float { value },
                View::String(value) => Self::String {
                    value: value.to_string(),
                },
                View::Bytes(value) => Self::Bytes {
                    value: value.to_vec(),
                },
                View::List(items) => Self::List {
                    items: items
                        .iter()
                        .map(|value| Self::from_runtime(&value, ctx))
                        .collect(),
                },
                View::Map(items) => Self::Map {
                    entries: items
                        .iter()
                        .map(|(name, value)| (name.to_string(), Self::from_runtime(&value, ctx)))
                        .collect(),
                },
                View::Variant { .. } => unreachable!("the tag spine was peeled above"),
                View::Thunk { comp, env } => Self::Ext(SerialAtom::Thunk(SerialThunk {
                    comp: Arc::clone(comp),
                    env: SerialEnvSnapshot::from_runtime(env, ctx),
                })),
                View::Builtin(entry) => Self::Ext(SerialAtom::Builtin(entry.decl.name.to_string())),
                View::Handle(handle) => Self::Ext(SerialAtom::Handle(handle.title().to_string())),
            };
            for label in labels.into_iter().rev() {
                encoded = Self::Variant {
                    label: label.to_string(),
                    payload: (!matches!(encoded, Self::Unit)).then(|| Box::new(encoded)),
                };
            }
            encoded
        })
    }

    /// Check all fallible references before building any runtime value.
    pub(crate) fn validate_runtime(&self, dec: &WireDecoder) -> Result<(), Error> {
        let mut result = Ok(());
        for_each_atom(self, |atom| {
            if result.is_err() {
                return;
            }
            result = match atom {
                SerialAtom::Thunk(thunk) => thunk.env.clone().into_runtime(dec).map(|_| ()),
                SerialAtom::Builtin(name) if dec.manifest.value(name).is_none() => Err(Error::new(
                    Reason::Protocol(Box::new(ProtocolFailure::UnknownNative(name.clone()))),
                )),
                SerialAtom::Builtin(_) | SerialAtom::Handle(_) => Ok(()),
            };
        });
        result
    }

    /// Resolve against the receiving runtime, then dispose the wire spine iteratively.
    pub(crate) fn into_runtime(self, dec: &WireDecoder) -> Result<ValClos, Error> {
        let result = self.validate_runtime(dec).map(|()| self.hydrate(dec));
        drop_value(self);
        result
    }

    fn hydrate(&self, dec: &WireDecoder) -> ValClos {
        on_json_stack(|| {
            let mut labels = Vec::new();
            let mut terminal = self;
            while let Self::Variant { label, payload } = terminal {
                labels.push(label);
                terminal = payload.as_deref().unwrap_or(&Self::Unit);
            }
            let mut value = match terminal {
                Self::Unit => ValClos::UNIT,
                Self::Bool { value } => ValClos::bool(*value),
                Self::Int { value } => ValClos::int(*value),
                Self::Float { value } => ValClos::float(*value),
                Self::String { value } => ValClos::string(value.as_str()),
                Self::Bytes { value } => ValClos::bytes(value.clone()),
                Self::List { items } => {
                    ValClos::list(items.iter().map(|value| value.hydrate(dec)).collect())
                }
                Self::Map { entries } => crate::shell::runtime::Map::from_seed_values(
                    entries
                        .iter()
                        .map(|(name, value)| (name.clone(), value.hydrate(dec))),
                )
                .into(),
                Self::Variant { .. } => unreachable!("the tag spine was peeled above"),
                Self::Ext(SerialAtom::Thunk(thunk)) => {
                    let env = thunk
                        .env
                        .clone()
                        .into_runtime(dec)
                        .expect("validated scope reference");
                    ValClos::thunk(Arc::clone(&thunk.comp), &env)
                }
                Self::Ext(SerialAtom::Builtin(name)) => ValClos::builtin(Arc::new(
                    dec.manifest.value(name).expect("validated builtin name"),
                )),
                Self::Ext(SerialAtom::Handle(title)) => ValClos::withheld(title.as_str()),
            };
            for label in labels.into_iter().rev() {
                value = ValClos::variant(label.as_str(), Some(value));
            }
            value
        })
    }
}

impl SerialEnvSnapshot {
    /// Intern `env` into `ctx`, reserving its row for [`InternCtx::finish`].
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
    use crate::ir::{CompKind, Module, Phrase, Toplevel, Val};
    use crate::source::{SourceFile, SourceOrigin, Spanned};
    use crate::typecheck::Unit;

    /// The child reads the checker's verdicts off the node rather than
    /// re-inferring, so a lambda's comp must carry its interior annotations
    /// across the wire — the `Capture` node a `let` of a command elaborates to.
    /// There is no thunk-root annotation, so those interior slots are the whole
    /// of it.
    fn annotated_lambda_node() -> Arc<Comp> {
        let src = r"let f = { |x| let y = /bin/echo $x; /bin/cat | /bin/cat }";
        let ast = crate::syntax::parser::parse(src).expect("parse");
        let top = crate::elaborator::elaborate(&ast, [], None, &mut crate::compile::no_modules)
            .expect("elaborate");
        let annotated = crate::typecheck(&top, crate::test_helper::core_schemes(), Unit::Script)
            .expect("typecheck");
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

        let lambda = SerialValue::Ext(SerialAtom::Thunk(SerialThunk {
            comp: Arc::clone(&node),
            env: SerialEnvSnapshot { bindings: 0 },
        }));

        let json = crate::data::json::to_vec(&lambda).expect("serialise lambda");
        let back: SerialValue = crate::data::json::from_slice(&json).expect("deserialise lambda");

        let SerialValue::Ext(SerialAtom::Thunk(back)) = back else {
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

    #[test]
    fn closure_round_trips_with_nested_linked_modules() {
        let inner = Arc::new(Module {
            top: Toplevel {
                phrases: vec![Spanned::synthetic(Phrase::Run(Arc::new(
                    Spanned::synthetic(CompKind::Return(Val::Int(42))),
                )))],
                admits: Vec::new(),
            },
            origin: SourceOrigin::File(
                SourceFile::new("/linked-seed/inner.ral").expect("UTF-8 path"),
            ),
        });
        let outer = Arc::new(Module {
            top: Toplevel {
                phrases: vec![Spanned::synthetic(Phrase::Run(Arc::new(
                    Spanned::synthetic(CompKind::Use(inner)),
                )))],
                admits: Vec::new(),
            },
            origin: SourceOrigin::File(
                SourceFile::new("/linked-seed/outer.ral").expect("UTF-8 path"),
            ),
        });
        let comp = Arc::new(Spanned::synthetic(CompKind::Lam {
            param: "unused".into(),
            body: Arc::new(Spanned::synthetic(CompKind::Use(outer))),
        }));
        let value = ValClos::thunk(Arc::clone(&comp), &Env::EMPTY);
        let mut ctx = InternCtx::new();
        let wire = SerialValue::from_runtime(&value, &mut ctx);
        let table = ctx.finish();
        let bytes = crate::data::json::to_vec(&wire).expect("serialise closure");
        let wire: SerialValue = crate::data::json::from_slice(&bytes).expect("deserialise closure");
        let bytes = crate::data::json::to_vec(&table).expect("serialise scopes");
        let table: ScopeTable = crate::data::json::from_slice(&bytes).expect("deserialise scopes");
        let dec = WireDecoder::for_runtime(&crate::test_helper::core_runtime(), &table)
            .expect("rebuild scopes");
        let back = wire
            .into_runtime(&dec)
            .expect("hydrate closure without loading");
        let View::Thunk { comp: body, .. } = back.view() else {
            panic!("round-trip changed the value variant");
        };
        assert_eq!(
            body, &comp,
            "every linked module's checked top and origin survive the wire"
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
        let top = crate::elaborator::elaborate(&ast, [], None, &mut crate::compile::no_modules)
            .expect("elaborate");
        let annotated = crate::typecheck(&top, crate::test_helper::core_schemes(), Unit::Script)
            .expect("typecheck");
        let [phrase] = annotated.phrases.as_slice() else {
            panic!("expected one phrase, got {:?}", annotated.phrases);
        };
        let crate::ir::Phrase::Define { comp, .. } = &phrase.item else {
            panic!("expected a Define phrase, got {:?}", phrase.item);
        };
        let node = find_lam_node(comp).expect("lambda in annotated comp");
        let sent = boundary_sites(&node);
        assert_eq!(sent.len(), 2, "each decode carries its site");

        let lambda = SerialValue::Ext(SerialAtom::Thunk(SerialThunk {
            comp: Arc::clone(&node),
            env: SerialEnvSnapshot { bindings: 0 },
        }));
        let json = crate::data::json::to_vec(&lambda).expect("serialise lambda");
        let back: SerialValue = crate::data::json::from_slice(&json).expect("deserialise lambda");
        let SerialValue::Ext(SerialAtom::Thunk(back)) = back else {
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
        let lambda = SerialValue::Ext(SerialAtom::Thunk(SerialThunk {
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
        use crate::fact::{CompareFailure, EqualityOrOrder, EvalFailure, ValueShape};
        use crate::handle::{Handle, HandleInner, pins_live_work};
        use crate::shell::runtime::{Break, End, block_over};

        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let chain = crate::shell::runtime::deep_block_chain(50_000, ValClos::UNIT);
                let mut ctx = InternCtx::new();
                SerialValue::from_runtime(&chain, &mut ctx);
                let table = ctx.finish();
                assert_eq!(table.len(), 50_000, "one scope row per link");

                let assert_equality_error = |left: &ValClos,
                                             right: &ValClos,
                                             shapes: (ValueShape, ValueShape)| {
                    let Break::Error(error) = left.equals(right).expect_err("opaque equality") else {
                        panic!("equality must return a catchable error");
                    };
                    assert_eq!(
                        error.reason,
                        Reason::Evaluation(Box::new(EvalFailure::Compare(CompareFailure {
                            operation: "equality".into(),
                            relation: EqualityOrOrder::Equality,
                            left: shapes.0,
                            right: shapes.1,
                        })))
                    );
                };
                let (handle, permit) = HandleInner::new(
                    crate::fact::WorkerId::mint(),
                    "deep tags",
                    crate::process::DurableRoot::default().child(),
                    crate::io::Streams::default(),
                );
                let live = ValClos::handle(handle);
                let mut env = Env::EMPTY;
                env.bind("work".into(), Binding { value: live.clone(), scheme: None });
                let mut unit_spine = None;
                for foot in [
                    ValClos::UNIT,
                    ValClos::int(7),
                    ValClos::list(vec![ValClos::int(7)]),
                    crate::handle::idle_handle(),
                    block_over(&env),
                    live,
                ] {
                    let is_unit = matches!(foot.view(), View::Unit);
                    let is_handle = matches!(foot.view(), View::Handle(_));
                    let is_opaque = foot.opacity().is_some();
                    let pins = matches!(foot.view(), View::Handle(Handle::Live(h)) if h.sample().is_live());
                    let foot_size = foot.shallow_size();
                    let foot_shape = foot.shape();
                    let mut value = foot;
                    for depth in 0..10_000 {
                        if depth == 5_000 && !is_unit && !is_handle {
                            let mut env = Env::EMPTY;
                            env.bind(
                                "tail".into(),
                                Binding {
                                    value,
                                    scheme: None,
                                },
                            );
                            value = ValClos::close(
                                &Val::Variant {
                                    label: "deep".into(),
                                    payload: Box::new(Val::Variable("tail".into())),
                                },
                                &env,
                                &crate::shell::runtime::Signature::default(),
                            )
                            .unwrap();
                        } else {
                            value = ValClos::variant("deep", Some(value));
                        }
                    }
                    let copy = value.clone();
                    assert_eq!(copy.shape(), ValueShape::Variant);
                    assert_eq!(copy.shallow_size(), 10_000 * "deep".len() + foot_size);
                    assert_eq!(pins_live_work(&copy), pins);
                    assert_eq!(value == copy, !is_opaque);
                    if is_opaque {
                        assert_equality_error(&value, &copy, (foot_shape, foot_shape));
                    } else {
                        assert!(value.equals(&copy).unwrap());
                    }
                    if is_unit {
                        unit_spine = Some(copy.clone());
                    }
                    let unit = unit_spine.as_ref().unwrap();
                    assert_eq!(unit.equals(&copy).unwrap(), is_unit);
                    assert_eq!(copy.equals(unit).unwrap(), is_unit);
                    let longer = ValClos::variant("deep", Some(copy.clone()));
                    assert_ne!(value, longer);
                    assert!(!unit.equals(&longer).unwrap());
                    assert!(!longer.equals(unit).unwrap());
                    if is_opaque {
                        assert_equality_error(&value, &longer, (foot_shape, ValueShape::Variant));
                        assert_equality_error(&longer, &value, (ValueShape::Variant, foot_shape));
                    } else {
                        assert!(!value.equals(&longer).unwrap());
                        assert!(!longer.equals(&value).unwrap());
                    }
                    let different = ValClos::variant("different", Some(copy.clone()));
                    assert!(!value.equals(&different).unwrap());
                    assert!(!different.equals(&value).unwrap());
                    let text = copy.to_string();
                    assert_eq!(text.matches("`deep").count(), 10_000);
                    assert_eq!(text.ends_with("`deep"), is_unit);
                    let scrubbed = copy.scrubbed(&mut |env| Some(env.clone())).unwrap();
                    let (labels, terminal) = scrubbed.peel_variants();
                    assert_eq!(labels.len(), 10_000);
                    if !is_handle {
                        assert!(copy.scrubbed(&mut |_| None).is_none());
                    }
                    if is_opaque {
                        assert_equality_error(&ValClos::UNIT, &terminal, (ValueShape::Unit, foot_shape));
                        assert_equality_error(&terminal, &ValClos::UNIT, (foot_shape, ValueShape::Unit));
                        assert!(FOValue::try_from(&copy).unwrap_err().nested);
                    } else {
                        let back = ValClos::from(FOValue::try_from(&copy).expect("deep data"));
                        assert_eq!(back, copy);
                        back.drop_seed();
                    }
                    let mut ctx = InternCtx::new();
                    let wire = SerialValue::from_runtime(&copy, &mut ctx);
                    let mut cursor = &wire;
                    let mut depth = 0;
                    while let SerialValue::Variant { label, payload } = cursor {
                        assert_eq!(label, "deep");
                        depth += 1;
                        let Some(payload) = payload.as_deref() else {
                            assert!(is_unit);
                            break;
                        };
                        cursor = payload;
                    }
                    assert_eq!(depth, 10_000);
                    assert_eq!(
                        matches!(cursor, SerialValue::Variant { payload: None, .. }),
                        is_unit
                    );
                    assert_eq!(
                        matches!(cursor, SerialValue::Ext(SerialAtom::Handle(_))),
                        is_handle
                    );
                    let table = ctx.finish();
                    assert_eq!(
                        table.len(),
                        usize::from(foot_shape == ValueShape::Block),
                        "only the block captures a scope"
                    );
                    let dec = WireDecoder::for_runtime(&crate::test_helper::core_runtime(), &table)
                        .expect("deep tag decoder");
                    let back = wire.into_runtime(&dec).expect("hydrate deep tags");
                    let (labels, terminal) = back.peel_variants();
                    assert_eq!(labels.len(), 10_000);
                    assert!(labels.iter().all(|label| label.as_ref() == "deep"));
                    assert_eq!(terminal.shape(), foot_shape);
                    if is_handle {
                        assert!(matches!(terminal.view(), View::Handle(Handle::Withheld(_))));
                    }
                    if !is_opaque {
                        assert_eq!(back, copy);
                    }
                    drop(terminal);
                    back.drop_seed();
                    drop(copy);
                    value.drop_seed();
                }
                permit.finish(End::Ok(ValClos::UNIT));
            })
            .expect("spawn")
            .join()
            .expect("a deep chain must encode without exhausting the stack");
    }

    /// The deferred table still decodes: `for_runtime` resolves the row
    /// dependencies and `into_runtime` rebuilds every link.
    #[test]
    fn deep_stream_chain_round_trips() {
        let chain = crate::shell::runtime::deep_block_chain(500, ValClos::UNIT);
        let mut ctx = InternCtx::new();
        let ipc = SerialValue::from_runtime(&chain, &mut ctx);
        let table = ctx.finish();
        let dec =
            WireDecoder::for_runtime(&crate::test_helper::core_runtime(), &table).expect("decoder");
        let back = ipc.into_runtime(&dec).expect("decode");
        let mut depth = 0;
        let mut cur = &back;
        while let View::Thunk { env, .. } = cur.view() {
            depth += 1;
            cur = env.get("tail").expect("each link binds the next");
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
        crate::test_access::bind_untyped(&mut shell, "only_mine", ValClos::int(1));

        let mut ctx = InternCtx::new();
        let _ = SerialEnvSnapshot::from_runtime(&shell.env, &mut ctx);
        let table = ctx.finish();

        assert_eq!(table.len(), 1, "one row: the session tier alone");
        let names: Vec<&str> = table[0].iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["only_mine"],
            "the prelude's own bindings never ride the wire"
        );
    }
}
