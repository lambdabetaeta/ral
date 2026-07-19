//! Wire helpers for re-exec'd ral subprocesses.
//!
//! This module holds the serialisable mirrors of the *mobile* half of a
//! ral shell — the [`Mobile`](crate::types::Mobile) bundle that crosses
//! an evaluation boundary.  `serial.rs` already owns value / closure
//! transport; this module owns the surrounding mobile envelope.
//!
//! ## Tree shape
//!
//! Each wire type mirrors one subtree of the runtime tree.  Conversions
//! compose: a parent's `from_X` calls its children's `from_X`, never
//! reaches past them.
//!
//! ```text
//! WireMobile { scope, control, context }
//!   ↳ WireContext { env_overrides, dir, grants, handlers, args, modules, cwd }
//! ```
//!
//! Turn-local state (IO sinks, surface sink, foreground scope, source cursor)
//! and session state (durable root, source registry, exit hints, builtin
//! table) are host-local — the child constructs its own.  In particular the
//! builtin table is never wired: its entries hold host fn pointers, so the
//! receiver supplies its own from its booted session.  Hooks are likewise
//! host-local — they fire only inside the REPL — so a child reconstructs an
//! empty hook map.  Audit policy is carried on the eval-request envelope, not
//! here, because it is an instruction to the child rather than a property of
//! the shell state.

use crate::serial::{InternCtx, ScopeArcs, SerialEnvSnapshot, SerialValue};
use crate::typecheck;
use crate::types::{
    Context, ControlState, Error, GrantStack, HandlerEntry, HandlerFrame, HandlerStack, Mobile,
    Shell,
};
use serde::{Deserialize, Serialize};

/// Wire mirror of a user-installed [`HandlerFrame`].
///
/// Builtins are not wired as handlers: the receiver installs its own
/// builtin table during shell construction.  Per-name arity is not
/// re-derived at the receiver — a per-name entry is always
/// [`HandlerArity::Unary`](crate::types::HandlerArity::Unary) by
/// construction ([`HandlerEntry::ral_per_name`]), and hydration does not
/// re-validate:
/// the values already passed install-time arity validation on the sender.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct WireHandlerFrame {
    pub entries: Vec<(String, SerialValue, Option<typecheck::Scheme>)>,
    pub catch_all: Option<SerialValue>,
    #[serde(default)]
    pub removable_by_unalias: bool,
}

impl WireHandlerFrame {
    fn from_runtime(frame: &HandlerFrame, ctx: &mut InternCtx) -> Result<Self, Error> {
        let mut entries = Vec::with_capacity(frame.entries.len());
        for entry in &frame.entries {
            let value = &entry.thunk;
            entries.push((
                entry.name.as_ref().to_string(),
                SerialValue::from_runtime(value, ctx)?,
                entry.scheme.clone(),
            ));
        }
        let catch_all = frame
            .catch_all
            .as_ref()
            .map(|value| SerialValue::from_runtime(value, ctx))
            .transpose()?;
        Ok(Self {
            entries,
            catch_all,
            removable_by_unalias: frame.removable_by_unalias,
        })
    }

    fn into_runtime(self, arcs: &ScopeArcs) -> Result<HandlerFrame, Error> {
        let entries = self
            .entries
            .into_iter()
            .map(|(name, value, scheme)| {
                value.into_runtime(arcs).map(|v| {
                    let mut entry = HandlerEntry::ral_per_name(name, v);
                    entry.scheme = scheme;
                    entry
                })
            })
            .collect::<Result<_, _>>()?;
        // handle is assigned by HandlerStack::from(Vec<HandlerFrame>)
        // when the frames are loaded into the stack — leave it as a
        // sentinel here; From<Vec<HandlerFrame>> overwrites it.
        Ok(HandlerFrame {
            entries,
            catch_all: self.catch_all.map(|v| v.into_runtime(arcs)).transpose()?,
            handle: crate::types::FrameHandle(u64::MAX),
            removable_by_unalias: self.removable_by_unalias,
        })
    }
}

/// Wire mirror of [`ControlState`].
///
/// Every counter the parent observes across an IPC round trip is here:
/// `last_status` (the body's exit), `call_depth` (so the child does not
/// start with a stale depth budget), and `recursion_limit` (so a child
/// observes the rc / CLI-overridden ceiling rather than the
/// compile-time default).  The receiver hydrates these into a fresh
/// [`ControlState`] without re-deriving any defaults.  Tail-ness is not
/// carried: the child evaluates the body to a value and absorbs any
/// terminal tail call locally, so the body's tail position has no
/// observable effect across the boundary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct WireControl {
    pub last_status: i32,
    pub call_depth: usize,
    pub recursion_limit: usize,
}

impl WireControl {
    fn from_runtime(c: &ControlState) -> Self {
        Self {
            last_status: c.last_status,
            call_depth: c.call_depth,
            recursion_limit: c.recursion_limit,
        }
    }

    fn into_runtime(self) -> ControlState {
        ControlState {
            last_status: self.last_status,
            call_depth: self.call_depth,
            recursion_limit: self.recursion_limit,
        }
    }
}

/// Serialisable mirror of [`Mobile`].
///
/// Mirrors the runtime tree one-for-one: `scope` and `control` ride
/// directly, the rest goes through [`WireContext`].  Local machinery
/// (IO, audit, REPL, exit hints, cancel) is host-local and the
/// receiver constructs its own.
///
/// Pair with [`Self::from_runtime`] / [`Self::into_runtime`] to cross the
/// wire; the inverses are total (modulo handle-bearing values, which
/// the serial layer drops with a clean error).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireMobile {
    /// Lexical scope chain (`Mobile::scope`), interned through the
    /// enclosing envelope's scope table.
    pub scope: SerialEnvSnapshot,
    pub control: WireControl,
    pub context: WireContext,
}

/// Wire mirror of [`Context`].
///
/// One field per runtime field — the type system witnesses the
/// flattened shape directly with no intermediate wrappers.  Handler
/// frames carry closures (`Value`) that must be interned through
/// `serial::InternCtx`, so `handlers` is reified into a
/// `Vec<WireHandlerFrame>` here and rehydrated on the receiving side.
/// All other fields ride their runtime types directly: `grants` is
/// `#[serde(transparent)]` over `Vec<Capabilities>`, `modules` and `cwd`
/// are `Serialize` themselves, and the bare fields (`env_overrides`, dir,
/// args) are serde-friendly already.  `hooks` is deliberately dropped —
/// they fire only inside the REPL — so the receiver reconstructs an empty
/// hook map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireContext {
    pub env_overrides: crate::types::EnvVars,
    pub dir: Option<std::path::PathBuf>,
    pub grants: GrantStack,
    pub handlers: Vec<WireHandlerFrame>,
    pub args: Vec<String>,
    pub modules: crate::types::Modules,
    pub cwd: crate::types::Cwd,
}

impl WireMobile {
    /// Reify a [`Mobile`] into wire form against the supplied
    /// intern context.  Inverse of [`Self::into_runtime`].
    pub(crate) fn from_runtime(mobile: &Mobile, ctx: &mut InternCtx) -> Result<Self, Error> {
        Ok(Self {
            scope: SerialEnvSnapshot::from_runtime(&mobile.scope, ctx)?,
            control: WireControl::from_runtime(&mobile.control),
            context: WireContext::from_runtime(&mobile.context, ctx)?,
        })
    }

    /// Hydrate a [`Mobile`] from wire form against the scope-arc
    /// table produced by `build_arcs` on the enclosing scope table.
    pub(crate) fn into_runtime(self, arcs: &ScopeArcs) -> Result<Mobile, Error> {
        Ok(Mobile {
            scope: self.scope.into_runtime(arcs)?,
            control: self.control.into_runtime(),
            context: self.context.into_runtime(arcs)?,
        })
    }
}

impl WireContext {
    pub(crate) fn from_runtime(h: &Context, ctx: &mut InternCtx) -> Result<Self, Error> {
        let mut handlers = Vec::new();
        for frame in &h.handlers {
            handlers.push(WireHandlerFrame::from_runtime(frame, ctx)?);
        }
        Ok(Self {
            env_overrides: h.env_overrides().clone(),
            dir: h.dir.clone(),
            grants: h.grants.clone(),
            handlers,
            args: h.args.clone(),
            modules: h.modules.clone(),
            cwd: h.cwd.clone(),
        })
    }

    pub(crate) fn into_runtime(self, arcs: &ScopeArcs) -> Result<Context, Error> {
        let handlers: Vec<HandlerFrame> = self
            .handlers
            .into_iter()
            .map(|frame| frame.into_runtime(arcs))
            .collect::<Result<_, _>>()?;
        Ok(Context {
            env_overrides: self.env_overrides,
            dir: self.dir,
            grants: self.grants,
            handlers: HandlerStack::from(handlers),
            hooks: std::collections::HashMap::default(),
            args: self.args,
            modules: self.modules,
            cwd: self.cwd,
        })
    }
}

/// Install a wire mobile snapshot onto a fresh shell, splicing the wire's
/// handler frames atop the receiver's existing layers.
///
/// The builtin table is session state and never rides the wire, so
/// installing a wire mobile cannot clobber it — the receiver's booted
/// dispatch survives untouched.  The wire's handler frames are pushed on top
/// using [`HandlerStack::push_frame`], which mints a fresh handle from the
/// receiver's counter while preserving every other field the wire carries —
/// so an alias frame stays removable by `unalias` in the child exactly as it
/// is in the parent.
pub(crate) fn install_shell_mobile(
    state: WireMobile,
    shell: &mut Shell,
    arcs: &ScopeArcs,
) -> Result<(), Error> {
    let mut mobile = state.into_runtime(arcs)?;
    // The wire's stack was hydrated into a temporary HandlerStack.
    // Drain those frames and push them atop the receiver's existing
    // layers using the receiver's handle counter.
    let wire_frames: Vec<HandlerFrame> = std::mem::take(&mut mobile.context.handlers).into();
    mobile.context.handlers = std::mem::take(&mut shell.mobile.context.handlers);
    for frame in wire_frames {
        mobile.context.handlers.push_frame(frame);
    }
    shell.install_mobile(mobile);
    Ok(())
}

/// Build the shell for a re-exec'd helper child: a fresh `Shell::new`
/// (which carries only `CORE_BUILTINS`), the host's
/// [`HostSurface`](crate::driver::HostSurface) reinstalled through the
/// child-shell-extension hook, then the wire
/// mobile snapshot overlaid.  The pipeline-stage / child-eval helper —
/// the remaining re-exec path — constructs its shell here, so it cannot
/// omit the host builtins: [`install_shell_mobile`]
/// preserves the receiver's builtin table, so the hook's entries
/// survive the overlay.
pub(crate) fn reexec_child_shell(state: WireMobile, arcs: &ScopeArcs) -> Result<Shell, Error> {
    let mut shell = Shell::new(crate::io::TerminalState::default());
    crate::sandbox::run_child_shell_extension(&mut shell);
    install_shell_mobile(state, &mut shell, arcs)?;
    Ok(shell)
}
