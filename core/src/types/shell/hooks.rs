//! The hook table: a session-lived namespace of named run roots the host
//! (rc file, plugin loader) registers and the engine dispatches at lifecycle
//! moments — prompt render, startup, plugin events, keybindings.
//!
//! A hook is a function the host already holds — a compiled block or lambda,
//! or a builtin; running one is a `Program::Hook` dispatch through
//! [`Shell::run`],
//! which looks it up here and applies it. The table is a namespace apart from
//! the lexical scope and the handler stack: a hook is never resolved by
//! `$name` and never consulted at command position.

use crate::source::Span;
use crate::types::Binding;
use crate::types::Shell;
use crate::types::Value;

use serde::{Deserialize, Serialize};
use std::fmt;
// ── Hook identity ───────────────────────────────────────────────────────

/// The unique name a plugin was loaded under.
pub(crate) type PluginId = String;

/// Which namespace a hook lives in.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Namespace {
    Session,
    Plugin(PluginId),
}

/// Fully-qualified name of a registered hook.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HookName {
    pub(crate) namespace: Namespace,
    pub name: String,
}

impl HookName {
    pub fn session(name: impl Into<String>) -> Self {
        Self {
            namespace: Namespace::Session,
            name: name.into(),
        }
    }

    pub fn plugin(plugin_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            namespace: Namespace::Plugin(plugin_id.into()),
            name: name.into(),
        }
    }

    /// How a fault in this hook's run is attributed.
    pub(crate) fn fault_label(&self) -> String {
        match &self.namespace {
            Namespace::Session => self.name.clone(),
            Namespace::Plugin(id) => format!("plugin '{id}' hook '{}'", self.name),
        }
    }
}

impl fmt::Display for HookName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.namespace {
            Namespace::Session => write!(f, "{}", self.name),
            Namespace::Plugin(id) => write!(f, "{}/{}", id, self.name),
        }
    }
}

// ── Hook signature ──────────────────────────────────────────────────────

/// A hook kind's fixed arity and diagnostic label, checked at registration so
/// a wrongly-shaped hook is rejected at load time rather than at dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookSig {
    /// A parameterless `Block`: the prompt body and the startup block.
    Prompt,
    Hook {
        kind: String,
    },
    /// Applied in place inside the caller's run frame rather than as a fresh
    /// run root — `pre-exec`, `post-exec`, `chpwd`.
    Lifecycle {
        kind: String,
    },
}

impl HookSig {
    /// Parameters a thunk must take to register under this signature.
    pub(crate) fn expected_arity(&self) -> usize {
        match self {
            Self::Prompt => 0,
            Self::Hook { .. } | Self::Lifecycle { .. } => 1,
        }
    }

    /// Human-readable label for diagnostics.
    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Prompt => "prompt body",
            Self::Hook { kind } | Self::Lifecycle { kind } => kind.as_str(),
        }
    }
}

// ── Per-hook policy ─────────────────────────────────────────────────────

/// Whether a hook's runs may hand the controlling terminal to a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalPolicy {
    Denied,
    Leased,
}

/// The host-stated policy for a hook's runs.
///
/// [`Shell::run`] applies it over the dispatching request: `terminal` replaces
/// the requested authority, `capture` can only tighten it, and `aside` runs it
/// on [`Shell::join_session`], so nothing it does flows back.
#[derive(Debug, Clone)]
pub struct DefaultPolicy {
    pub terminal: TerminalPolicy,
    pub(crate) capture: bool,
    pub(crate) aside: bool,
}

impl DefaultPolicy {
    pub const fn denied() -> Self {
        Self {
            terminal: TerminalPolicy::Denied,
            capture: false,
            aside: false,
        }
    }

    pub const fn leased() -> Self {
        Self {
            terminal: TerminalPolicy::Leased,
            ..Self::denied()
        }
    }

    pub const fn denied_capture() -> Self {
        Self {
            capture: true,
            ..Self::denied()
        }
    }

    /// This policy, its runs taken aside.
    #[must_use]
    pub const fn aside(self) -> Self {
        Self {
            aside: true,
            ..self
        }
    }
}

// ── Hook ────────────────────────────────────────────────────────────────

/// One entry in the hook table: a named, typechecked, policy-tagged run root.
#[derive(Debug, Clone)]
pub struct Hook {
    /// Built by the same scheme-inference path an ordinary session `let` uses.
    pub binding: Binding,
    pub(crate) sig: HookSig,
    pub(crate) policy: DefaultPolicy,
    /// Declaration site, for diagnostics.
    pub(crate) origin: Span,
}
impl Hook {
    /// The single registration gate: the bound value must be a function — a
    /// `Block`, a `Lambda`, or a builtin — of the arity `sig` expects.
    ///
    /// # Errors
    /// [`RegisterError::NotFunction`] if it is neither,
    /// [`RegisterError::ArityMismatch`] if the parameter count differs.
    pub(crate) fn validate(&self, name: &HookName) -> Result<(), RegisterError> {
        let expected = self.sig.expected_arity();
        let actual = match &self.binding.value {
            Value::Thunk(c) if c.comp.arrow().is_none() => 0,
            Value::Thunk(_) => self.binding.value.lambda_arity().unwrap_or(0),
            Value::Native { entry, applied } => entry.fixed_arity().saturating_sub(applied.len()),
            other => {
                return Err(RegisterError::NotFunction {
                    name: name.clone(),
                    origin: self.origin,
                    actual: format!("{other}"),
                });
            }
        };
        if actual != expected {
            return Err(RegisterError::ArityMismatch {
                name: name.clone(),
                origin: self.origin,
                expected,
                actual,
                sig_label: self.sig.label().into(),
            });
        }
        Ok(())
    }
}
// ── Registration errors ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum RegisterError {
    NotFunction {
        name: HookName,
        origin: Span,
        actual: String,
    },
    ArityMismatch {
        name: HookName,
        origin: Span,
        expected: usize,
        actual: usize,
        sig_label: String,
    },
    AlreadyRegistered {
        name: HookName,
        origin: Span,
    },
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFunction { name, actual, .. } => {
                write!(
                    f,
                    "cannot register '{name}' as a hook: \
                     expected a Block, Lambda, or builtin, got {actual}"
                )
            }
            Self::ArityMismatch {
                name,
                expected,
                actual,
                sig_label,
                ..
            } => {
                write!(
                    f,
                    "cannot register '{}' as a {}: \
                     expected {} parameter{}, got {}",
                    name,
                    sig_label,
                    expected,
                    if *expected == 1 { "" } else { "s" },
                    actual
                )
            }
            Self::AlreadyRegistered { name, .. } => {
                write!(f, "hook '{name}' is already registered")
            }
        }
    }
}

// ── The session's registration surface ──────────────────────────────────

impl Shell {
    /// Register a function [`Value`] — a `Block`, `Lambda`, or builtin — as a
    /// named run root in the session hook table.
    ///
    /// It fires only on a host-dispatched `Program::Hook` run, never as `$name`
    /// and never as a command.  On failure the caller renders the
    /// [`RegisterError`] as a diagnostic at `origin`.
    ///
    /// # Errors
    /// [`RegisterError::AlreadyRegistered`] if `name` is taken, otherwise
    /// whatever [`Hook::validate`] raises.
    pub fn register_hook(
        &mut self,
        name: HookName,
        value: Value,
        sig: HookSig,
        policy: DefaultPolicy,
        origin: Span,
    ) -> Result<(), RegisterError> {
        // Short-circuit before any scheme inference.
        if self.context.hooks.contains_key(&name) {
            return Err(RegisterError::AlreadyRegistered { name, origin });
        }

        // The same scheme inference an ordinary session `let` uses.  A
        // non-thunk gets no scheme; `validate` below is the gate that rejects it.
        let scheme = self.value_scheme(&value);
        let binding = Binding { value, scheme };

        let hook = Hook {
            binding,
            sig,
            policy,
            origin,
        };
        hook.validate(&name)?;

        self.context.hooks.insert(name, hook);
        Ok(())
    }

    /// Whether a hook named `name` is registered.
    pub fn has_hook(&self, name: &HookName) -> bool {
        self.context.hooks.contains_key(name)
    }

    /// The hook registered under `name`, for a host applying it directly
    /// inside an existing command frame rather than through a dispatch door.
    pub fn hook(&self, name: &HookName) -> Option<&Hook> {
        self.context.hooks.get(name)
    }

    /// Remove one hook by name, reporting whether it was there — the inverse of
    /// [`Self::register_hook`] for a spent one-shot entry point, such as a
    /// plugin factory.
    pub fn unregister_hook(&mut self, name: &HookName) -> bool {
        self.context.hooks.remove(name).is_some()
    }

    /// Drop every hook under a plugin's namespace, returning the count.  One
    /// sweep at unload, so no dispatchable entry point outlives the plugin that
    /// owned it; also the rollback path for a load that fails partway.
    pub fn remove_plugin_hooks(&mut self, plugin_id: &str) -> usize {
        let before = self.context.hooks.len();
        self.context
            .hooks
            .retain(|name, _| !matches!(&name.namespace, Namespace::Plugin(id) if id == plugin_id));
        before - self.context.hooks.len()
    }
}
