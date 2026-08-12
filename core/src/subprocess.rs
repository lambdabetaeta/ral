//! Serialisable mirror of the [`Mobile`] half of a shell — what crosses to a
//! re-exec'd child.  `serial.rs` transports the values and closures inside;
//! this module is the envelope around them.
//!
//! Nothing host-local rides: the builtin table holds fn pointers, hooks are
//! host lifecycle entry points, and IO and session state belong to whoever
//! runs — the child constructs its own.  Audit policy travels on
//! `child_eval`'s request envelope instead, being an instruction to the child
//! rather than a property of its shell.

use crate::serial::{InternCtx, SerialEnvSnapshot, SerialValue, WireDecoder};
use crate::typecheck;
use crate::types::{
    Context, ControlState, Error, GrantStack, HandlerEntry, HandlerFrame, HandlerStack, Mobile,
    Shell,
};
use serde::{Deserialize, Serialize};

/// Wire mirror of a user-installed [`HandlerFrame`].
///
/// Hydration does not re-check arity: a per-name entry is unary by
/// construction ([`HandlerEntry::ral_per_name`]) and the sender vetted the
/// thunk at install.
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

    fn into_runtime(self, dec: &WireDecoder) -> Result<HandlerFrame, Error> {
        let entries = self
            .entries
            .into_iter()
            .map(|(name, value, scheme)| {
                value.into_runtime(dec).map(|v| {
                    let mut entry = HandlerEntry::ral_per_name(name, v);
                    entry.scheme = scheme;
                    entry
                })
            })
            .collect::<Result<_, _>>()?;
        // Sentinel: `HandlerStack::from(Vec<HandlerFrame>)` mints the handle.
        Ok(HandlerFrame {
            entries,
            catch_all: self.catch_all.map(|v| v.into_runtime(dec)).transpose()?,
            handle: crate::types::FrameHandle(u64::MAX),
            removable_by_unalias: self.removable_by_unalias,
        })
    }
}

/// Wire mirror of [`ControlState`].
///
/// All three counters ride, so the child continues the parent's depth budget
/// under the parent's rc / CLI-configured ceiling rather than the
/// compile-time default.  Tail-ness does not: the child absorbs its body's
/// terminal tail call locally.
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
/// The inverses are total modulo handle-bearing values, which the serial
/// layer drops with a clean error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireMobile {
    /// Interned through the enclosing request envelope's scope table.
    pub scope: SerialEnvSnapshot,
    pub control: WireControl,
    pub context: WireContext,
}

/// Wire mirror of [`Context`].
///
/// Only `handlers` needs reshaping: its frames carry closures, which must be
/// interned through [`InternCtx`].  `hooks` is dropped outright and the
/// receiver starts with an empty table.
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
    /// Inverse of [`Self::into_runtime`].
    pub(crate) fn from_runtime(mobile: &Mobile, ctx: &mut InternCtx) -> Result<Self, Error> {
        Ok(Self {
            scope: SerialEnvSnapshot::from_runtime(&mobile.scope, ctx),
            control: WireControl::from_runtime(&mobile.control),
            context: WireContext::from_runtime(&mobile.context, ctx)?,
        })
    }

    pub(crate) fn into_runtime(self, dec: &WireDecoder) -> Result<Mobile, Error> {
        Ok(Mobile {
            scope: self.scope.into_runtime(dec)?,
            control: self.control.into_runtime(),
            context: self.context.into_runtime(dec)?,
        })
    }
}

impl WireContext {
    pub(crate) fn from_runtime(h: &Context, ctx: &mut InternCtx) -> Result<Self, Error> {
        let mut handlers = Vec::new();
        for frame in &h.handlers {
            handlers.push(WireHandlerFrame::from_runtime(frame, ctx)?);
        }
        let (dir, cwd) = h.wire_cwd_parts();
        Ok(Self {
            env_overrides: h.env_overrides().clone(),
            dir: dir.map(std::path::Path::to_path_buf),
            grants: h.grants.clone(),
            handlers,
            args: h.args.clone(),
            modules: h.modules.clone(),
            cwd: cwd.clone(),
        })
    }

    pub(crate) fn into_runtime(self, dec: &WireDecoder) -> Result<Context, Error> {
        let handlers: Vec<HandlerFrame> = self
            .handlers
            .into_iter()
            .map(|frame| frame.into_runtime(dec))
            .collect::<Result<_, _>>()?;
        Ok(Context::from_wire(
            self.env_overrides,
            self.dir,
            self.grants,
            HandlerStack::from(handlers),
            self.args,
            self.modules,
            self.cwd,
        ))
    }
}

/// Install a wire mobile onto a fresh shell, splicing the wire's handler
/// frames atop the receiver's own.
///
/// The receiver's builtin table survives, never having ridden the wire.
/// Frames go through [`HandlerStack::push_frame`], which mints a handle from
/// the receiver's counter and keeps every other field, so an alias frame
/// stays removable by `unalias` in the child.
pub(crate) fn install_shell_mobile(
    state: WireMobile,
    shell: &mut Shell,
    dec: &WireDecoder,
) -> Result<(), Error> {
    let mut mobile = state.into_runtime(dec)?;
    let wire_frames: Vec<HandlerFrame> = std::mem::take(&mut mobile.context.handlers).into();
    mobile.context.handlers = std::mem::take(&mut shell.mobile.context.handlers);
    for frame in wire_frames {
        mobile.context.handlers.push_frame(frame);
    }
    shell.install_mobile(mobile);
    Ok(())
}

/// A fresh shell wearing the host's builtin surface: `Shell::new` carries
/// core's manifest alone, so the child-shell hook reinstalls the rest before
/// any [`WireDecoder`] is built against it.
pub(crate) fn bare_child_shell() -> Shell {
    let mut shell = Shell::new(crate::io::TerminalState::default());
    crate::sandbox::run_child_shell_extension(&mut shell);
    shell
}
