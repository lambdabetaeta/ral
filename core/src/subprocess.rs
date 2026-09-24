//! Serialisable mirror of the wire half of a shell — `env`,
//! `session.stack_limit`, `context` — what crosses to a re-exec'd engine
//! child (`hatch`'s [`crate::engine_seed::EngineSeed`]).  `serial.rs`
//! transports the values and closures inside; this module is the envelope
//! around them.
//!
//! Nothing host-local rides: the builtin table holds fn pointers, hooks are
//! host lifecycle entry points, and IO and session state belong to whoever
//! runs — the child constructs its own.

use crate::serial::{InternCtx, SerialEnvSnapshot, SerialValue, WireDecoder};
use crate::typecheck;
use crate::types::{
    Context, Env, Error, GrantStack, HandlerEntry, HandlerFrame, HandlerStack, Shell,
};
use serde::{Deserialize, Serialize};

/// Wire mirror of a user-installed [`HandlerFrame`].
///
/// Hydration does not re-check arity: a per-name entry is unary by
/// construction ([`HandlerEntry::ral_per_name`]) and the sender vetted the
/// thunk at install.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct WireHandlerFrame {
    pub(crate) entries: Vec<(String, SerialValue, Option<typecheck::Scheme>)>,
    pub(crate) catch_all: Option<SerialValue>,
    #[serde(default)]
    pub(crate) removable_by_unalias: bool,
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

/// Serialisable mirror of a shell's wire state (`env`, `session.stack_limit`,
/// `context`).
///
/// The inverses are total on a scrubbed fork ([`Shell::fork_scrubbed`]),
/// which holds no handle; encoding one is an error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireShell {
    /// Interned through the enclosing request envelope's scope table.
    pub env: SerialEnvSnapshot,
    /// The cap rides so the child continues the parent's rc / CLI-configured
    /// ceiling rather than the compile-time default.
    pub(crate) stack_limit: usize,
    pub(crate) context: WireContext,
}

/// Wire mirror of [`Context`].
///
/// Only `handlers` needs reshaping: its frames carry closures, which must be
/// interned through [`InternCtx`].  `hooks` is dropped outright and the
/// receiver starts with an empty table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireContext {
    pub(crate) env_overrides: crate::types::EnvVars,
    pub(crate) dir: Option<std::path::PathBuf>,
    pub(crate) grants: GrantStack,
    pub(crate) handlers: Vec<WireHandlerFrame>,
    pub(crate) args: Vec<String>,
    pub(crate) modules: crate::types::Modules,
    pub cwd: crate::types::Cwd,
}

/// A decoded [`WireShell`], as the three fields `install_wire_shell`'s
/// caller needs to write onto a `Shell`.
pub(crate) struct DecodedShell {
    pub env: Env,
    pub(crate) stack_limit: usize,
    pub(crate) context: Context,
}

impl WireShell {
    /// Inverse of [`Self::into_runtime`].
    pub(crate) fn from_runtime(
        env: &Env,
        stack_limit: usize,
        context: &Context,
        ctx: &mut InternCtx,
    ) -> Result<Self, Error> {
        Ok(Self {
            env: SerialEnvSnapshot::from_runtime(env, ctx),
            stack_limit,
            context: WireContext::from_runtime(context, ctx)?,
        })
    }

    pub(crate) fn into_runtime(self, dec: &WireDecoder) -> Result<DecodedShell, Error> {
        Ok(DecodedShell {
            env: self.env.into_runtime(dec)?,
            stack_limit: self.stack_limit,
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

/// Install a wire shell onto a fresh shell, splicing the wire's handler
/// frames atop the receiver's own.
///
/// The receiver's builtin table survives, never having ridden the wire.
/// Frames go through [`HandlerStack::push_frame`], which mints a handle from
/// the receiver's counter and keeps every other field, so an alias frame
/// stays removable by `unalias` in the child.
pub(crate) fn install_wire_shell(
    state: WireShell,
    shell: &mut Shell,
    dec: &WireDecoder,
) -> Result<(), Error> {
    let DecodedShell {
        env,
        stack_limit,
        mut context,
    } = state.into_runtime(dec)?;
    let wire_frames: Vec<HandlerFrame> = std::mem::take(&mut context.handlers).into();
    context.handlers = std::mem::take(&mut shell.context.handlers);
    for frame in wire_frames {
        context.handlers.push_frame(frame);
    }
    shell.env = env;
    shell.session.stack_limit = stack_limit;
    shell.context = context;
    Ok(())
}

/// A fresh shell wearing the host's builtin surface and `prelude`'s baked
/// tier: `Shell::new` carries core's manifest alone, so the child-shell hook
/// reinstalls the rest — and seats the prelude — before any [`WireDecoder`]
/// is built against it, since a decoder seats every hydrated environment
/// under this shell's own prelude.
///
/// Test-only scaffolding: production hydration goes through
/// [`crate::boot::boot_shell`], which takes its `HostSurface` directly.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn bare_child_shell(prelude: &crate::boot::BakedPrelude) -> Shell {
    let mut shell = Shell::new(crate::io::TerminalState::default());
    crate::sandbox::run_child_shell_extension(&mut shell);
    crate::builtins::register(&mut shell, prelude.comp());
    shell
}
