//! Hook table: a session-lived namespace of named run-entry points
//! registered by the host (rc file, plugin loader) and dispatched by the
//! engine at lifecycle moments — prompt render, startup, plugin hooks,
//! keybindings.
//!
//! A hook is a [`Value::Block`] / [`Value::Lambda`] the host already
//! holds in compiled form.  **Registering** it = storing it by name in
//! the session-lived [`Context::hooks`] table.  **Running** it =
//! dispatching a [`Program::Hook`](crate::transport::Program) run through
//! [`Shell::run`], which looks up the hook and applies it through
//! the shared framed scaffold.
//!
//! The table is a separate namespace from both the user lexical scope
//! ([`Env`]) and the handler stack ([`HandlerStack`]): a hook is a run
//! root, never a command; it is never resolved by `$name` and never
//! consulted at command position.  This keeps host entry points out of
//! the user's value/command namespace.

use crate::source::Span;
use crate::types::Binding;
use crate::types::Value;

use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;
// ── Hook identity ───────────────────────────────────────────────────────

/// Plugin identity: the unique name a plugin was loaded under.
pub type PluginId = String;

/// Which namespace a hook lives in.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Namespace {
    /// Session-global: rc-declared prompt, startup block.
    Session,
    /// Scoped to one loaded plugin.
    Plugin(PluginId),
}

/// Fully-qualified name of a registered hook.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HookName {
    pub namespace: Namespace,
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

/// The fixed-arity signature of a hook kind — the typed contract checked
/// at registration so a hook declared for the wrong arity is rejected at
/// load time, not at dispatch time.
///
/// Each variant carries the expected input arity (0 for a prompt body
/// Block, 1 for a Lambda receiving a single record argument) and a
/// human-readable label for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookSig {
    /// Zero-input hook: `{ … }` — used for the prompt body and the
    /// startup block.  A `Block` (no parameters).
    Prompt,
    /// One-input hook receiving a ground record: `{ |ctx| … }` —
    /// prompt hook, buffer-change, keybinding, lifecycle hooks.
    Hook { kind: String },
    /// Plugin factory: one-input Lambda receiving the options map.
    PluginFactory,
    /// In-frame lifecycle hook: applied directly inside the command's
    /// run frame rather than as a fresh run root.
    Lifecycle { kind: String },
}

impl HookSig {
    /// The expected parameter count for a thunk registered under this
    /// signature: 0 for a `Prompt` (Block), 1 for everything else
    /// (Lambda).
    pub fn expected_arity(&self) -> usize {
        match self {
            Self::Prompt => 0,
            Self::Hook { .. } | Self::PluginFactory | Self::Lifecycle { .. } => 1,
        }
    }

    /// Human-readable label for diagnostics ("prompt body", "prompt hook", …).
    pub fn label(&self) -> &str {
        match self {
            Self::Prompt => "prompt body",
            Self::Hook { kind } | Self::Lifecycle { kind } => kind.as_str(),
            Self::PluginFactory => "plugin factory",
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

/// The host-stated policy for a registered hook's runs: terminal
/// access, capture regime, and optional run budget.
#[derive(Debug, Clone)]
pub struct DefaultPolicy {
    /// Terminal authority for runs from this hook.
    pub terminal: TerminalPolicy,
    /// Capture stdout/stderr for this hook's runs.
    pub capture: bool,
    /// Optional per-run wall; `None` = uncapped.
    pub budget: Option<Duration>,
}

impl DefaultPolicy {
    pub const fn denied() -> Self {
        Self {
            terminal: TerminalPolicy::Denied,
            capture: false,
            budget: None,
        }
    }

    pub const fn leased() -> Self {
        Self {
            terminal: TerminalPolicy::Leased,
            capture: false,
            budget: None,
        }
    }

    pub const fn denied_capture() -> Self {
        Self {
            terminal: TerminalPolicy::Denied,
            capture: true,
            budget: None,
        }
    }
}

// ── Hook ────────────────────────────────────────────────────────────────

/// One entry in the hook table: a named, typechecked, policy-tagged
/// run root.
#[derive(Debug, Clone)]
pub struct Hook {
    /// The lexical binding — `{ value: Block/Lambda, scheme }` — built
    /// by the same scheme-inference path an ordinary session `let` uses.
    pub binding: Binding,
    /// The engine-declared fixed-arity signature this hook was
    /// checked against at registration.
    pub sig: HookSig,
    /// The host-stated default policy for runs from this hook.
    pub policy: DefaultPolicy,
    /// Declaration site, for diagnostics.
    pub origin: Span,
}
impl Hook {
    /// The single registration gate: the bound value must be a `Block` or
    /// a `Lambda`, and its parameter count must match the signature's
    /// expected arity (0 for `Prompt`, 1 for `Hook`/`Lifecycle`/
    /// `PluginFactory`).
    ///
    /// # Errors
    /// Returns [`RegisterError::NotFunction`] if the bound value is neither
    /// a `Block` nor a `Lambda`, or [`RegisterError::ArityMismatch`] if its
    /// parameter count differs from the signature's expected arity.
    pub fn validate(&self, name: &HookName) -> Result<(), RegisterError> {
        let expected = self.sig.expected_arity();
        let actual = match &self.binding.value {
            Value::Block { .. } => 0,
            Value::Lambda { .. } => self.binding.value.lambda_arity().unwrap_or(0),
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
    /// The value is not a Block or Lambda.
    NotFunction {
        name: HookName,
        origin: Span,
        actual: String,
    },
    /// The value's arity does not match the expected signature.
    ArityMismatch {
        name: HookName,
        origin: Span,
        expected: usize,
        actual: usize,
        sig_label: String,
    },
    /// A hook with this name is already registered.
    AlreadyRegistered { name: HookName, origin: Span },
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFunction { name, actual, .. } => {
                write!(
                    f,
                    "cannot register '{name}' as a hook: \
                     expected a Block or Lambda, got {actual}"
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
