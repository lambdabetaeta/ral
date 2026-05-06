//! Runtime mode inference for `let`-bind capture and pipeline analysis.
//!
//! Two questions only:
//!
//!   - Output mode of `m` (used by `eval_bind_rhs` to decide whether to
//!     install a stdout-capture buffer).
//!   - Input/output mode of each pipeline stage (used by `pipeline::analyze`
//!     to validate adjacency and by `pipeline::launch` to wire channels).
//!
//! The full Hindley-Milner checker in `typecheck/` runs ahead-of-time on
//! parsed source.  This module runs per-bind at evaluation time, with
//! access to the live `Shell` so locals, aliases, and effect-handlers
//! contribute their stored mode pairs.  Modes only — payload value types
//! belong to the static checker.

use crate::classify::{HeadKind, head_kind};
use crate::ir::{Comp, CompKind, ExecName, Val};
use crate::types::{Shell, Value};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModeVar(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Bytes,
    None,
    Var(ModeVar),
}

impl Mode {
    pub fn is_var(&self) -> bool {
        matches!(self, Mode::Var(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompType {
    pub input: Mode,
    pub output: Mode,
}

impl CompType {
    pub const fn val() -> Self {
        Self { input: Mode::None, output: Mode::None }
    }

    pub const fn ext() -> Self {
        Self { input: Mode::Bytes, output: Mode::Bytes }
    }

    pub const fn decode() -> Self {
        Self { input: Mode::Bytes, output: Mode::None }
    }

    pub const fn encode() -> Self {
        Self { input: Mode::None, output: Mode::Bytes }
    }
}

impl std::fmt::Display for CompType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn m(m: Mode) -> &'static str {
            match m {
                Mode::Bytes => "bytes",
                Mode::None => "none",
                Mode::Var(_) => "var",
            }
        }
        write!(f, "F_{{{}, {}}}", m(self.input), m(self.output))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeSlot {
    Free,
    Ground(Mode),
    Parent(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeError {
    pub left: Mode,
    pub right: Mode,
}

pub struct ModeUnifier {
    slots: Vec<ModeSlot>,
}

impl ModeUnifier {
    pub fn new() -> Self {
        Self { slots: Vec::new() }
    }

    pub fn fresh(&mut self) -> ModeVar {
        let id = self.slots.len();
        self.slots.push(ModeSlot::Free);
        ModeVar(id)
    }

    fn fresh_pair(&mut self) -> CompType {
        CompType {
            input: Mode::Var(self.fresh()),
            output: Mode::Var(self.fresh()),
        }
    }

    fn find(&mut self, i: usize) -> usize {
        match self.slots[i] {
            ModeSlot::Parent(p) => {
                let root = self.find(p);
                if root != p {
                    self.slots[i] = ModeSlot::Parent(root);
                }
                root
            }
            _ => i,
        }
    }

    pub fn resolve(&mut self, m: Mode) -> Mode {
        match m {
            Mode::Bytes | Mode::None => m,
            Mode::Var(ModeVar(i)) => {
                let root = self.find(i);
                match self.slots[root] {
                    ModeSlot::Free => Mode::Var(ModeVar(root)),
                    ModeSlot::Ground(g) => g,
                    ModeSlot::Parent(_) => unreachable!(),
                }
            }
        }
    }

    pub fn unify(&mut self, a: Mode, b: Mode) -> Result<(), ModeError> {
        let ra = self.resolve(a);
        let rb = self.resolve(b);
        match (ra, rb) {
            (Mode::Bytes, Mode::Bytes) | (Mode::None, Mode::None) => Ok(()),
            (Mode::Bytes, Mode::None) | (Mode::None, Mode::Bytes) => Err(ModeError {
                left: ra,
                right: rb,
            }),
            (Mode::Var(ModeVar(aid)), Mode::Var(ModeVar(bid))) => {
                if aid != bid {
                    self.slots[aid] = ModeSlot::Parent(bid);
                }
                Ok(())
            }
            (Mode::Var(ModeVar(vid)), g @ (Mode::Bytes | Mode::None))
            | (g @ (Mode::Bytes | Mode::None), Mode::Var(ModeVar(vid))) => {
                let root = self.find(vid);
                self.slots[root] = ModeSlot::Ground(g);
                Ok(())
            }
        }
    }
}

impl Default for ModeUnifier {
    fn default() -> Self {
        Self::new()
    }
}

pub struct HeadResolution {
    pub comp_type: CompType,
    pub internal: bool,
}

fn constrain_comp(comp: &Comp, unifier: &mut ModeUnifier) -> CompType {
    match &comp.kind {
        CompKind::Exec { name, args, .. } => match name {
            ExecName::Bare(name) => head_sig(name, args, unifier),
            ExecName::Path(_) | ExecName::TildePath(_) => CompType::ext(),
        },
        CompKind::Builtin { name, args } => head_sig(name, args, unifier),
        CompKind::Return(_)
        | CompKind::PrimOp(..)
        | CompKind::Interpolation(_)
        | CompKind::Background(_)
        | CompKind::Index { .. }
        | CompKind::Lam { .. }
        | CompKind::Rec { .. }
        | CompKind::LetRec { .. } => CompType::val(),
        CompKind::Seq(cs) => cs
            .last()
            .map(|c| constrain_comp(c, unifier))
            .unwrap_or_else(CompType::val),
        CompKind::Bind { rest, .. } => constrain_comp(rest, unifier),
        CompKind::Pipeline(stages) => stages
            .last()
            .map(|c| constrain_comp(c, unifier))
            .unwrap_or_else(CompType::ext),
        CompKind::Chain(parts) => infer_chain(parts, unifier),
        CompKind::App { head, args, .. } => infer_app(head, args, unifier),
        CompKind::Force(val) => infer_force(val, unifier),
        CompKind::If { then, else_, .. } => {
            infer_branches(&[then.clone(), else_.clone()], unifier)
        }
    }
}

pub fn infer_comp(comp: &Comp) -> CompType {
    constrain_comp(comp, &mut ModeUnifier::new())
}

pub fn head_sig(name: &str, args: &[Val], unifier: &mut ModeUnifier) -> CompType {
    match head_kind(name) {
        HeadKind::Bytes | HeadKind::StreamingReducer | HeadKind::External => CompType::ext(),
        HeadKind::Value | HeadKind::Never => CompType::val(),
        HeadKind::Branches => infer_branches(args, unifier),
        HeadKind::LastThunk => infer_last_thunk(args, unifier),
        HeadKind::DecodeToValue => CompType::decode(),
        HeadKind::EncodeToBytes => CompType::encode(),
    }
}

fn head_is_internal(name: &str) -> bool {
    !matches!(head_kind(name), HeadKind::External)
}

fn infer_app(head: &Comp, args: &[Val], unifier: &mut ModeUnifier) -> CompType {
    match &head.kind {
        CompKind::Force(Val::Variable(name)) => head_sig(name, args, unifier),
        CompKind::Force(Val::Thunk(comp)) => constrain_comp(comp, unifier),
        _ => unifier.fresh_pair(),
    }
}

fn infer_force(val: &Val, unifier: &mut ModeUnifier) -> CompType {
    match val {
        Val::Thunk(comp) => constrain_comp(comp, unifier),
        _ => unifier.fresh_pair(),
    }
}

/// Output mode of a chain `a ? b ? …`: bytes if any arm produces bytes,
/// else the last arm's type.  A string-literal fallback (`? ''`) must not
/// demote the mode to None when other arms emit bytes.
fn infer_chain(parts: &[Comp], unifier: &mut ModeUnifier) -> CompType {
    let types: Vec<_> = parts.iter().map(|c| constrain_comp(c, unifier)).collect();
    let any_bytes = types
        .iter()
        .any(|ct| matches!(unifier.resolve(ct.output), Mode::Bytes));
    if any_bytes {
        return CompType {
            input: Mode::None,
            output: Mode::Bytes,
        };
    }
    types.into_iter().last().unwrap_or_else(CompType::val)
}

fn infer_branches(args: &[Val], unifier: &mut ModeUnifier) -> CompType {
    let mut branches: Vec<CompType> = Vec::new();
    for a in args {
        if let Val::Thunk(c) = a {
            branches.push(constrain_comp(c, unifier));
        }
    }
    let mut iter = branches.into_iter();
    let Some(mut acc) = iter.next() else {
        return unifier.fresh_pair();
    };
    for ty in iter {
        if unifier.unify(acc.input, ty.input).is_err() {
            acc.input = Mode::Var(unifier.fresh());
        }
        if unifier.unify(acc.output, ty.output).is_err() {
            acc.output = Mode::Var(unifier.fresh());
        }
    }
    acc
}

fn infer_last_thunk(args: &[Val], unifier: &mut ModeUnifier) -> CompType {
    for arg in args.iter().rev() {
        if let Val::Thunk(c) = arg {
            return constrain_comp(c, unifier);
        }
    }
    unifier.fresh_pair()
}

// ── InferCtx — shell-aware traversal for evaluator/pipeline ────────────────

pub struct InferCtx {
    pub unifier: ModeUnifier,
}

impl InferCtx {
    pub fn new() -> Self {
        Self {
            unifier: ModeUnifier::new(),
        }
    }

    pub fn comp_type(&mut self, comp: &Comp, shell: Option<&Shell>) -> CompType {
        if let Some(shell) = shell
            && let Some((name, _)) = named_head(last_effective(comp))
            && let Some(ct) = env_binding_type(shell, name, &mut self.unifier)
        {
            return ct;
        }
        constrain_comp(comp, &mut self.unifier)
    }

    pub fn resolve_head(&mut self, name: &str, shell: Option<&Shell>) -> HeadResolution {
        if let Some(shell) = shell {
            if let Some(ct) = env_binding_type(shell, name, &mut self.unifier) {
                return HeadResolution {
                    comp_type: ct,
                    internal: true,
                };
            }
            if shell.get_prelude(name).is_some() || crate::builtins::is_builtin(name) {
                return HeadResolution {
                    comp_type: head_sig(name, &[], &mut self.unifier),
                    internal: head_is_internal(name),
                };
            }
            return HeadResolution {
                comp_type: CompType::ext(),
                internal: false,
            };
        }
        HeadResolution {
            comp_type: head_sig(name, &[], &mut self.unifier),
            internal: head_is_internal(name),
        }
    }

    /// Output mode of `comp` for bind-RHS decisions.  When the resolved
    /// output is polymorphic, consults call-site arguments to instantiate
    /// it.  Unresolved vars default to `Mode::None`.
    pub fn output_mode(&mut self, comp: &Comp, shell: Option<&Shell>) -> Mode {
        let eff = last_effective(comp);
        let ct = if let (Some(shell), Some((name, _))) = (shell, named_head(eff)) {
            self.resolve_head(name, Some(shell)).comp_type
        } else {
            let out = constrain_comp(comp, &mut self.unifier).output;
            return match self.unifier.resolve(out) {
                m if m.is_var() => Mode::None,
                m => m,
            };
        };
        let out = self.unifier.resolve(ct.output);
        if !out.is_var() {
            return out;
        }
        if let (Some(shell), Some((_, args))) = (shell, named_head(eff))
            && let Some(m) = instantiate_from_args(args, shell, &mut self.unifier)
        {
            return m;
        }
        Mode::None
    }
}

impl Default for InferCtx {
    fn default() -> Self {
        Self::new()
    }
}

fn last_effective(comp: &Comp) -> &Comp {
    match &comp.kind {
        CompKind::Seq(cs) => cs.last().unwrap_or(comp),
        _ => comp,
    }
}

fn named_head(comp: &Comp) -> Option<(&str, &[Val])> {
    match &comp.kind {
        CompKind::Exec { name, args, .. } => name.bare().map(|n| (n, args.as_slice())),
        CompKind::Force(Val::Variable(name)) => Some((name.as_str(), &[])),
        CompKind::App { head, args, .. } => match &head.as_ref().kind {
            CompKind::Force(Val::Variable(name)) => Some((name.as_str(), args.as_slice())),
            _ => None,
        },
        _ => None,
    }
}

fn env_binding_type(shell: &Shell, name: &str, u: &mut ModeUnifier) -> Option<CompType> {
    let handler = shell.lookup_handler(name).map(|(v, _, _)| v);
    [
        shell.get_local(name).cloned(),
        shell.registry.aliases.get(name).map(|e| e.value.clone()),
        handler,
    ]
    .into_iter()
    .flatten()
    .find_map(|v| match v {
        Value::Thunk { body, .. } => Some(thunk_body_type(&body, u)),
        _ => None,
    })
}

/// CompType of a stored thunk's body, in the live unifier.
/// `Lam`-wrapped bodies are unwrapped to expose the lambda body's
/// type — what the thunk yields when called.
fn thunk_body_type(body: &Comp, u: &mut ModeUnifier) -> CompType {
    let inner = match &body.kind {
        CompKind::Lam { body: lam_body, .. } => lam_body.as_ref(),
        _ => body,
    };
    constrain_comp(inner, u)
}

fn instantiate_from_args(args: &[Val], shell: &Shell, u: &mut ModeUnifier) -> Option<Mode> {
    for arg in args.iter().rev() {
        let body: std::sync::Arc<Comp> = match arg {
            Val::Thunk(body) => body.clone(),
            Val::Variable(name) => match shell.get_local(name) {
                Some(Value::Thunk { body, .. }) => body.clone(),
                _ => continue,
            },
            _ => continue,
        };
        let out = thunk_body_type(&body, u).output;
        let m = u.resolve(out);
        if !m.is_var() {
            return Some(m);
        }
    }
    None
}
