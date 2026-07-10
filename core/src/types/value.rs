//! Runtime values.
//!
//! [`Value`] is the runtime representation of every ral value.
//! [`HandleInner`] is the shared state for a spawned computation.
//! [`HandlerFrame`] is one frame of the user handler stack.  All frames
//! share one flat `Vec<HandlerFrame>` in [`HandlerStack`], ordered
//! innermost-last (last-pushed-wins).  Scoped `within` handlers are removed
//! by handle; aliases are removed by name and carry an explicit
//! `removable_by_unalias` bit.  Builtin command bindings live separately in
//! [`BuiltinTable`].
//! [`fmt_lambda`] renders a lambda as a compact human-readable string.

use super::env::Env;
use super::list::List;
use super::map::Map;
use crate::io::ByteBuffer;
use crate::typecheck;
use crate::typecheck::builtins::BuiltinTypeRule;
use std::borrow::Cow;
use std::fmt;
use std::sync::{Arc, Mutex};

/// Runtime closure backing a captured builtin body, carrying host state.
pub type CapturedBuiltinFn =
    Arc<dyn Fn(&[Value], &mut crate::types::Shell) -> Settled<Value> + Send + Sync>;

/// Host implementation of a builtin command binding.
#[derive(Clone)]
pub enum BuiltinBody {
    /// Process-static function pointer.
    Static(fn(&[Value], &mut crate::types::Shell) -> Settled<Value>),
    /// Runtime closure with host state captured by the frontend.
    Captured(CapturedBuiltinFn),
}

impl BuiltinBody {
    /// Call the body with the given arguments and shell.
    ///
    /// # Errors
    /// Returns `Err` if the invoked body raises a runtime `Break`.
    pub fn call(&self, args: &[Value], shell: &mut crate::types::Shell) -> Settled<Value> {
        match self {
            Self::Static(f) => f(args, shell),
            Self::Captured(f) => f(args, shell),
        }
    }
}

impl fmt::Debug for BuiltinBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Static(_) => f.write_str("BuiltinBody::Static(<fn>)"),
            Self::Captured(_) => f.write_str("BuiltinBody::Captured(<closure>)"),
        }
    }
}

/// A builtin command binding: a name implemented by host Rust code.
#[derive(Clone)]
pub struct BuiltinEntry {
    pub name: Cow<'static, str>,
    pub type_rule: BuiltinTypeRule,
    pub doc: &'static str,
    pub body: BuiltinBody,
}

impl BuiltinEntry {
    /// Fixed value-arg count for `$name` η-expansion and typecheck.
    /// `None` for variadic or command-only entries.
    pub fn fixed_arity(&self) -> Option<usize> {
        match &self.type_rule {
            BuiltinTypeRule::Scheme(arity, _) => *arity,
            BuiltinTypeRule::Sig(sig) => sig.fixed_arity(),
        }
    }
}

impl fmt::Debug for BuiltinEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltinEntry")
            .field("name", &self.name)
            .field("body", &self.body)
            .finish_non_exhaustive()
    }
}

/// Per-shell builtin command bindings.
#[derive(Debug, Clone, Default)]
pub struct BuiltinTable {
    sets: imbl::Vector<Arc<[BuiltinEntry]>>,
}

impl BuiltinTable {
    /// Install a group of builtin entries for this shell.
    pub fn install_static(&mut self, entries: &'static [BuiltinEntry]) {
        self.install_arc(Arc::from(entries));
    }

    /// Install runtime-owned builtin entries for this shell.
    pub fn install_arc(&mut self, entries: Arc<[BuiltinEntry]>) {
        self.sets.push_back(entries);
    }

    /// Look up a builtin, newest installed set first.
    pub fn get(&self, name: &str) -> Option<BuiltinEntry> {
        self.sets
            .iter()
            .rev()
            .flat_map(|set| set.iter())
            .find(|entry| entry.name == name)
            .cloned()
    }

    /// Names of installed builtins, newest installed set first.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.sets
            .iter()
            .rev()
            .flat_map(|set| set.iter().map(|entry| entry.name.as_ref()))
    }
}

// `Shell` and `Settled<Value>` cross-references for host thunks.
use super::flow::Settled;

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
    /// refuses throughout — [`pins_running_work`] makes the identical
    /// refusal for the same reason. The estimate is a residency nudge, not
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

/// Whether `v` structurally reaches a handle whose state is
/// [`HandleState::Running`] — the binding-lease reaper's pin check
/// (`decisions/260629_agent-binding-reaping`): a name whose value still
/// reaches a running worker is never pruned. Recurses through `List`,
/// `Map`, and `Variant` payloads; a `Lambda` or `Block`'s captured
/// `Arc<Env>` is deliberately **never** descended — the same graph chase
/// [`Value::shallow_size`] refuses, and a handle reachable only through a
/// closure capture is not "the name of live work": the worker registry
/// retains the handle itself regardless of whether any top-level name
/// still reaches it, so nothing can be stranded either way.
pub(crate) fn pins_running_work(v: &Value) -> bool {
    match v {
        Value::Handle(h) => *h.state.lock().unwrap() == HandleState::Running,
        Value::List(items) => items.iter().any(pins_running_work),
        Value::Map(pairs) => pairs.iter().any(|(_, v)| pins_running_work(v)),
        Value::Variant { payload, .. } => payload.as_deref().is_some_and(pins_running_work),
        Value::Unit
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::Bytes(_)
        | Value::Lambda { .. }
        | Value::Block { .. } => false,
    }
}

/// Shared handle to a spawned computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleState {
    Running,
    Completed,
    Cancelled,
}

/// The observable outcome of a finished concurrent block: the bytes it
/// wrote, drained exactly once from the handle's buffers, paired with its
/// raw result.
///
/// Every eliminator (`await`, `race`, `poll`) projects this
/// one cached value rather than re-reading the live buffers, so a failed
/// block's bytes are captured on completion and repeated observations stay
/// consistent.  `outcome` is `Ok(value)` for a block that returned and
/// `Err(break)` for one that raised or whose worker panicked.
#[derive(Debug, Clone)]
pub struct CompletedHandle {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Structured-event values a *detached* worker deferred, drained once
    /// from [`HandleInner::surface_buf`].  Replayed through the awaiting
    /// turn's surface by `await`/`race` (once), never by `poll`.
    pub surface: Vec<crate::serial::FOValue>,
    pub outcome: super::flow::Settled<Value>,
}

/// Bounded buffer of structured-event values a *detached* worker defers
/// instead of emitting live.
///
/// The turn that spawned the worker may already
/// have ended, so its `surface` events are buffered here and replayed through
/// the awaiting turn's surface on the first `await`/`race`.  Bounded so a
/// runaway detached emitter cannot grow it without limit (see
/// `builtins::concurrency`'s `DeferredSurface`).
pub type SurfaceBuffer = Arc<Mutex<Vec<crate::serial::FOValue>>>;

/// Shared handle to a spawned computation.
#[derive(Debug, Clone)]
#[allow(clippy::type_complexity)]
pub struct HandleInner {
    /// Result channel: worker sends [`Settled<Value>`] (the worker's
    /// trampoline absorbs any terminal tail call before the value
    /// reaches the channel — `Tail` cannot cross a thread boundary).
    pub result: Arc<Mutex<Option<std::sync::mpsc::Receiver<super::flow::Settled<Value>>>>>,
    /// Cached outcome after first observation (§13.3: a second await
    /// returns the cached bytes + result).  `None` until the block
    /// completes, then the buffers are drained into it exactly once.
    pub cached: Arc<Mutex<Option<CompletedHandle>>>,
    /// Lifecycle state for handle-level APIs.
    pub state: Arc<Mutex<HandleState>>,
    /// Buffered stdout from the spawned block (§13.3 replay rule).  Bytes
    /// accumulate here during execution and are drained on `await`.  Always
    /// empty for watched handles — bytes flow live through `Sink::LineFramed`.
    pub stdout_buf: ByteBuffer,
    /// Buffered stderr from the spawned block (§13.3 replay rule).  Always
    /// empty for watched handles.
    pub stderr_buf: ByteBuffer,
    /// Bounded deferred surface events from the worker: a *detached*
    /// worker's `surface` builtin lands here rather than on the
    /// (possibly-ended) spawning turn's live sink.  Drained once on
    /// completion into [`CompletedHandle::surface`].
    pub surface_buf: SurfaceBuffer,
    /// The deliver-once test-and-set latch for the worker's deferred surface,
    /// shared between the two renderers and set by whichever renders the batch
    /// first — an eliminator's replay (`await`/`race`) or the host's boundary
    /// delivery.  Whoever wins renders; the other sees it set and skips, so a
    /// batch never renders twice and a never-awaited worker still delivers
    /// exactly once at the boundary.
    pub joined: Arc<Mutex<bool>>,
    /// The moment an eliminator last named this handle: written at
    /// construction and renewed by `poll` and by each `await`/`race` wait
    /// sweep, read by the idle-observation lease chain
    /// (`builtins::concurrency`).  `cancel` and listing never touch it —
    /// only an observation renews the lease.
    pub last_observed: Arc<Mutex<std::time::Instant>>,
    pub cmd: std::string::String,
    /// The worker's cancel scope, a child of the spawning shell's scope.
    /// `cancel` and `race`-of-losers fire it so the worker stops at its
    /// next poll rather than running to completion detached.
    pub cancel: crate::process::CancelScope,
}

impl PartialEq for HandleInner {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.result, &other.result)
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

/// Opaque handle returned by [`HandlerStack::push`].
///
/// Generational — allocated from a monotonic counter on [`HandlerStack`],
/// one per push.  Passing the handle back to [`HandlerStack::remove_by_handle`]
/// locates the frame by identity rather than index, so removal is robust
/// to sibling alias removals that would shift array indices between push
/// and paired pop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHandle(pub(crate) u64);

/// Calling convention of a handler invocation — fixed by the surface
/// form at install time, never inferred from the value's runtime shape.
///
/// A per-name handler (`within [handlers: …]`) and an alias are always
/// [`Unary`]: a unary lambda `{ |args| … }` invoked with the command's
/// argument list.  A catch-all (`within [handler: …]`) is always
/// [`CatchAll`]: a binary lambda `{ |name args| … }` invoked with the
/// command name and the argument list.  The install boundary rejects any
/// value that does not match the required arity.
///
/// [`Unary`]: HandlerArity::Unary
/// [`CatchAll`]: HandlerArity::CatchAll
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HandlerArity {
    /// Catch-all: thunk receives `(name, args)`.
    CatchAll,
    /// Per-name lambda: thunk receives `(args)`.
    Unary,
}

/// One user handler entry — the unit of installation in a
/// [`HandlerFrame`].  Builtins are represented by [`BuiltinEntry`], not
/// by this type.
#[derive(Clone)]
pub struct HandlerEntry {
    pub name: Cow<'static, str>,
    /// Calling convention for the dispatch site.  Read directly off the
    /// entry rather than inferred from the thunk shape per call.
    pub arity: HandlerArity,
    pub thunk: Value,
    /// The arm's closed scheme, stored at install for persistent (alias)
    /// frames so the next turn's check sees the alias as the installing
    /// turn did.  `None` on `within [handlers: …]` entries, whose frames
    /// never outlive their turn.
    pub scheme: Option<crate::typecheck::Scheme>,
}

impl HandlerEntry {
    /// Build a per-name entry for a user-defined `within [handlers: …]`
    /// or `alias` thunk.  Always [`HandlerArity::Unary`]: a per-name
    /// handler's calling convention is fixed by its surface form, so its
    /// thunk is a unary lambda `{ |args| … }` invoked with the command's
    /// argument list.  The caller validates at the install boundary that
    /// the thunk is in fact a unary lambda.
    pub fn ral_per_name(name: String, thunk: Value) -> Self {
        Self {
            name: Cow::Owned(name),
            arity: HandlerArity::Unary,
            thunk,
            scheme: None,
        }
    }

    /// Vet `thunk` as the handler body for `name` and build the entry to
    /// install — the single gate shared by `alias` and `within [handlers:
    /// …]` so the name-conflict check, the shape check, and the
    /// mode-preservation check are each written once regardless of which
    /// install path is calling.
    ///
    /// Checked in order: `name` must be free of both a lexical binding
    /// and a builtin (a handler needs a head of its own to dispatch on);
    /// `thunk` must be a unary lambda `{ |args| … }`, enforced by
    /// [`validate_handler_arity`]; its body must preserve the head's
    /// pipeline mode, enforced by [`crate::typecheck::alias_arm_scheme`].
    /// `role` names the diagnostic and picks whether the inferred scheme
    /// is persisted on the entry — an alias frame outlives its
    /// installing turn and needs it seeded for the next turn's check; a
    /// `within [handlers: …]` frame is popped before the turn ends and
    /// needs none.
    ///
    /// # Errors
    /// Returns `Err` if `name` is already a lexical binding in scope, if
    /// `name` is a builtin, if `thunk` is not a unary lambda (per
    /// [`validate_handler_arity`]), or if the arm's body changes the head's
    /// pipeline mode (per [`crate::typecheck::alias_arm_scheme`]).
    pub fn vet(
        name: String,
        thunk: Value,
        session_schemes: crate::typecheck::SessionSchemes,
        role: HandlerRole,
    ) -> Settled<Self> {
        let label = role.label();
        if session_schemes.bindings.iter().any(|(n, _)| n == &name) {
            return Err(super::coerce::sig(format!(
                "{label}: `{name}` is a lexical binding in this scope; handler names must \
                 be free of lexical bindings and builtins"
            )));
        }
        if crate::builtins::is_builtin(&name) {
            return Err(super::coerce::sig(format!(
                "{label}: `{name}` is a builtin; handler names must be free of lexical \
                 bindings and builtins"
            )));
        }
        validate_handler_arity(&thunk, 1, &format!("{label}: `{name}`"))?;
        let Value::Lambda { param, body, .. } = &thunk else {
            unreachable!("validate_handler_arity guarantees a unary lambda");
        };
        let scheme = crate::typecheck::alias_arm_scheme(&name, param, body, session_schemes)
            .map_err(|m| {
                use crate::typecheck::fmt_mode;
                super::coerce::sig(format!(
                    "{label}: `{name}`'s body changes the head's pipeline mode ({} vs {}); \
                     a handler reinterprets a head and must preserve its modes — match the \
                     existing head's modes or add a codec",
                    fmt_mode(&m.left),
                    fmt_mode(&m.right),
                ))
            })?;
        let mut entry = Self::ral_per_name(name, thunk);
        if role.persists_scheme() {
            entry.scheme = Some(scheme);
        }
        Ok(entry)
    }
}

/// Which install path is vetting a handler entry through
/// [`HandlerEntry::vet`] — picks the diagnostic's label and whether the
/// arm's inferred scheme is persisted on the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerRole {
    /// `alias NAME { |args| … }` — the frame outlives its installing
    /// turn, so its scheme is persisted for the next turn's check.
    Alias,
    /// `within [handlers: …]` — the frame is popped before the turn
    /// ends, so no scheme needs to survive it.
    Scoped,
}

impl HandlerRole {
    fn label(self) -> &'static str {
        match self {
            Self::Alias => "alias",
            Self::Scoped => "within handlers",
        }
    }

    fn persists_scheme(self) -> bool {
        matches!(self, Self::Alias)
    }
}

/// Validate that a handler thunk's surface form matches the required
/// calling convention: it must be a lambda of exactly `arity` arguments.
///
/// The calling convention of a handler is fixed by its surface form, not
/// inferred from its runtime shape, so this is the single gate at every
/// install boundary (`alias`, `within [handlers: …]`, `within [handler:
/// …]`).  A non-lambda value or a lambda of the wrong arity is rejected
/// with a message that names what was wrong and `context` (e.g. ``alias:
/// `greet` ``) so the diagnostic points at the offending install site.
///
/// # Errors
/// Returns `Err` if `value` is not a lambda, or is a lambda whose
/// curry-chain arity is not exactly `arity`.
pub fn validate_handler_arity(value: &Value, arity: usize, context: &str) -> Settled<()> {
    let form = match arity {
        1 => "a unary lambda `{ |args| ... }`",
        2 => "a binary lambda `{ |name args| ... }`",
        n => unreachable!("handler arity must be 1 or 2, got {n}"),
    };
    match value.lambda_arity() {
        Some(found) if found == arity => Ok(()),
        Some(found) => Err(super::coerce::sig(format!(
            "{context} must be {form}, got a lambda taking {found} argument(s)"
        ))),
        None => Err(super::coerce::sig(format!(
            "{context} must be {form}, got a {}",
            value.type_name()
        ))),
    }
}

impl fmt::Debug for HandlerEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HandlerEntry")
            .field("name", &self.name)
            .field("arity", &self.arity)
            .field("thunk", &self.thunk)
            .finish_non_exhaustive()
    }
}

/// One frame of the handler stack.
///
/// Per-name entries are checked before the catch-all within the same
/// frame.  If a frame has neither a matching entry nor a catch-all,
/// command lookup falls through to the next handler frame.  Scoped
/// handlers and aliases share this frame shape, with an explicit
/// removability bit for `unalias`.
#[derive(Debug, Clone)]
pub struct HandlerFrame {
    pub entries: Vec<HandlerEntry>,
    /// Catch-all handler: `within [handler: thunk]`.  `None` on alias
    /// frames.  Its arity is implicit ([`HandlerArity::CatchAll`]).
    pub catch_all: Option<Value>,
    /// Opaque identity for paired push / remove.
    pub handle: FrameHandle,
    /// True only for frames installed by `alias` and removable by
    /// `unalias`; scoped `within` frames are removed by handle.
    pub removable_by_unalias: bool,
}

impl HandlerFrame {
    /// Whether this frame is the alias frame for `name`: it carries the
    /// `removable_by_unalias` bit and has exactly one per-name entry
    /// matching `name` with no catch-all.  The single shape predicate
    /// shared by alias removal and alias presence queries.
    pub fn is_alias_for(&self, name: &str) -> bool {
        self.removable_by_unalias
            && self.catch_all.is_none()
            && self.entries.len() == 1
            && self.entries[0].name == name
    }
}

/// The handler stack.
///
/// A flat `Vec<HandlerFrame>` with last-pushed-wins ordering — the
/// innermost frame is at the highest index.  Scoped `within` handlers
/// are removed by handle; aliases are removed by walking for the
/// matching removable name.
///
/// The two-pass [`HandlerStack::lookup`] rule (`per-name across all
/// frames, then catch-all across all frames`) ensures any per-name
/// handler beats any catch-all regardless of stack position.
///
/// No `Serialize` / `Deserialize`: frames carry `Value`, which holds
/// closures that must be interned through `serial::InternCtx` at IPC
/// boundaries; the wire mirror in `subprocess::WireHandlerFrame` handles
/// that conversion field-by-field.
#[derive(Debug, Clone, Default)]
pub struct HandlerStack {
    frames: Vec<HandlerFrame>,
    next_handle: u64,
}

impl HandlerStack {
    /// Allocate a new handle, append a frame, and return the handle.
    ///
    /// Covers scoped `within` installation.  The caller is responsible
    /// for removing the frame via [`Self::remove_by_handle`].
    pub fn push(&mut self, entries: Vec<HandlerEntry>, catch_all: Option<Value>) -> FrameHandle {
        self.push_frame(HandlerFrame {
            entries,
            catch_all,
            handle: FrameHandle(u64::MAX),
            removable_by_unalias: false,
        })
    }

    /// Install an alias frame removable by [`Self::remove_alias`].
    pub fn push_alias(&mut self, entries: Vec<HandlerEntry>) -> FrameHandle {
        self.push_frame(HandlerFrame {
            entries,
            catch_all: None,
            handle: FrameHandle(u64::MAX),
            removable_by_unalias: true,
        })
    }

    /// Append a complete [`HandlerFrame`], minting a fresh handle from
    /// this stack's counter and preserving every other field — notably
    /// `removable_by_unalias`, so a wire-hydrated alias frame stays
    /// removable by `unalias`.  The frame's incoming `handle` is
    /// discarded; identity belongs to the receiving stack.
    pub fn push_frame(&mut self, mut frame: HandlerFrame) -> FrameHandle {
        let handle = FrameHandle(self.next_handle);
        self.next_handle += 1;
        frame.handle = handle;
        self.frames.push(frame);
        handle
    }

    /// Remove the frame with the given handle (walk innermost-first;
    /// usually near the top).  Returns the removed frame, or `None` if
    /// no frame carries that handle.  Used by `with_handlers`'s paired
    /// pop.
    pub fn remove_by_handle(&mut self, handle: FrameHandle) -> Option<HandlerFrame> {
        let pos = self.frames.iter().rposition(|f| f.handle == handle)?;
        Some(self.frames.remove(pos))
    }

    /// Remove the innermost alias frame for `name` (see
    /// [`HandlerFrame::is_alias_for`]).  Returns the removed frame, or
    /// `None` if no such frame is installed.
    ///
    /// Selection turns on the `removable_by_unalias` bit, which `push`
    /// clears on scoped `within` frames; only frames installed by
    /// `alias` carry it.  A `within [handlers: [foo: t]]` frame is thus
    /// excluded by construction even when it shares the one-entry,
    /// no-catch-all shape of an alias for `foo`.
    pub fn remove_alias(&mut self, name: &str) -> Option<HandlerFrame> {
        let pos = self.frames.iter().rposition(|f| f.is_alias_for(name))?;
        Some(self.frames.remove(pos))
    }

    /// Walk the stack in two passes, returning the winning handler for
    /// `name`.  Returns the matched entry together with `depth` — the
    /// count of frames from the top to (and including) the matched
    /// frame, used by self-masking invocation to locate and lift the
    /// matched frame for the dynamic extent of the body.
    ///
    /// **Pass 1 — per-name:** scan all frames innermost-first.  The
    /// first frame that has an explicit entry whose name equals `name`
    /// wins immediately.
    ///
    /// **Pass 2 — catch-all:** if no per-name entry was found anywhere,
    /// scan all frames innermost-first again.  The first frame that
    /// carries a catch-all thunk wins; the synthesized `HandlerEntry`
    /// has `arity = CatchAll` and `thunk` set to the catch-all value.
    ///
    /// Returning `None` means the name is not handled by the stack at
    /// all (the caller falls through to external command lookup).
    pub fn lookup(&self, name: &str) -> Option<(HandlerEntry, usize)> {
        // Pass 1: per-name match across all frames.
        for (depth, frame) in self.frames.iter().rev().enumerate() {
            if let Some(entry) = frame.entries.iter().find(|e| e.name == name) {
                return Some((entry.clone(), depth + 1));
            }
        }
        // Pass 2: catch-all match across all frames.
        for (depth, frame) in self.frames.iter().rev().enumerate() {
            if let Some(thunk) = &frame.catch_all {
                return Some((
                    HandlerEntry {
                        name: Cow::Owned(name.to_string()),
                        arity: HandlerArity::CatchAll,
                        thunk: thunk.clone(),
                        scheme: None,
                    },
                    depth + 1,
                ));
            }
        }
        None
    }

    /// All per-name handler entries installed across the stack,
    /// innermost first.  Duplicates are not de-duplicated.
    pub fn entries(&self) -> impl Iterator<Item = &HandlerEntry> {
        self.frames.iter().rev().flat_map(|f| f.entries.iter())
    }

    /// The (name, scheme) pairs of installed alias arms, outermost first
    /// — the alias half of the next turn's check seed.
    pub fn alias_schemes(&self) -> Vec<(String, typecheck::Scheme)> {
        self.frames
            .iter()
            .filter(|f| f.removable_by_unalias)
            .flat_map(|f| f.entries.iter())
            .filter_map(|entry| {
                entry
                    .scheme
                    .clone()
                    .map(|scheme| (entry.name.as_ref().to_string(), scheme))
            })
            .collect()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, HandlerFrame> {
        self.frames.iter()
    }

    /// Lift the matched frame off the stack and return it.  Pair with
    /// [`Self::restore_matched`].  Only the matched frame is removed —
    /// frames newer or older than it stay in place, so outer handlers
    /// for *other* names remain visible inside the running body.
    /// `depth` is the value returned alongside the match by
    /// [`Self::lookup`].  The frame carries its own `handle`, which
    /// `restore_matched` reads to find the insertion point.
    pub fn strip_matched(&mut self, depth: usize) -> HandlerFrame {
        let index = self.frames.len() - depth;
        self.frames.remove(index)
    }

    /// Re-insert the frame previously taken by [`Self::strip_matched`]
    /// at its correct position, using its handle to find the insertion
    /// point that preserves the original relative ordering.
    ///
    /// The frame must go back *under* any frames newer than it (frames
    /// with higher handle values) and *over* anything older.  Since
    /// handles are monotonically allocated, we find the rightmost frame
    /// whose handle is strictly older and insert after it.  In practice
    /// this walks at most a few entries — only frames pushed during the
    /// matched body's own execution will have newer handles.
    pub fn restore_matched(&mut self, frame: HandlerFrame) {
        let insert_at = self
            .frames
            .iter()
            .rposition(|f| f.handle.0 < frame.handle.0)
            .map_or(0, |i| i + 1);
        self.frames.insert(insert_at, frame);
    }
}

impl<'a> IntoIterator for &'a HandlerStack {
    type Item = &'a HandlerFrame;
    type IntoIter = std::slice::Iter<'a, HandlerFrame>;
    fn into_iter(self) -> Self::IntoIter {
        self.frames.iter()
    }
}

impl From<Vec<HandlerFrame>> for HandlerStack {
    /// Build a `HandlerStack` from a raw frame vec, assigning new handles.
    /// Used at IPC boundaries where deserialized frames arrive without
    /// handles (the wire format does not carry them).
    fn from(v: Vec<HandlerFrame>) -> Self {
        let mut stack = Self::default();
        for frame in v {
            stack.push_frame(frame);
        }
        stack
    }
}

impl From<HandlerStack> for Vec<HandlerFrame> {
    fn from(s: HandlerStack) -> Self {
        s.frames
    }
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
