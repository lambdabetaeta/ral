//! Host-program table: a session-lived namespace of named turn-entry
//! points registered by the host (rc file, plugin loader) and dispatched
//! by the engine at lifecycle moments — prompt render, startup, plugin
//! hooks, keybindings.
//!
//! A host program is a [`Value::Block`] / [`Value::Lambda`] the host
//! already holds in compiled form.  **Registering** it = storing it by
//! name in the session-lived [`Context::programs`] table.  **Running**
//! it = [`Shell::run_program`], which looks up the program and applies it
//! through the shared framed scaffold ([`Shell::run_built`]).
//!
//! The table is a separate namespace from both the user lexical scope
//! ([`Env`]) and the handler stack ([`HandlerStack`]): a program is a
//! turn root, never a command; it is never resolved by `$name` and never
//! consulted at command position.  This keeps host entry points out of
//! the user's value/command namespace.

use crate::source::Span;
use crate::types::Binding;

use std::fmt;
use std::time::Duration;

// ── Program identity ────────────────────────────────────────────────────

/// Plugin identity: the unique name a plugin was loaded under.
pub type PluginId = String;

/// Which namespace a program lives in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Namespace {
    /// Session-global: rc-declared prompt, startup block.
    Session,
    /// Scoped to one loaded plugin.
    Plugin(PluginId),
}

/// Fully-qualified name of a registered host program.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProgramName {
    pub namespace: Namespace,
    pub name: String,
}

impl ProgramName {
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

impl fmt::Display for ProgramName {
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
    /// Zero-input program: `{ … }` — used for the prompt body and the
    /// startup block.  A `Block` (no parameters).
    PromptProgram,
    /// One-input hook receiving a ground record: `{ |ctx| … }` —
    /// prompt hook, buffer-change, keybinding, lifecycle hooks.
    Hook { kind: String },
    /// Plugin factory: one-input Lambda receiving the options map.
    PluginFactory,
    /// In-frame lifecycle hook: applied directly inside the command's
    /// turn frame rather than as a fresh turn root.
    Lifecycle { kind: String },
}

impl HookSig {
    /// The expected parameter count for a thunk registered under this
    /// signature: 0 for a `PromptProgram` (Block), 1 for everything else
    /// (Lambda).
    pub fn expected_arity(&self) -> usize {
        match self {
            HookSig::PromptProgram => 0,
            HookSig::Hook { .. } | HookSig::PluginFactory | HookSig::Lifecycle { .. } => 1,
        }
    }

    /// Human-readable label for diagnostics ("prompt body", "prompt hook", …).
    pub fn label(&self) -> &str {
        match self {
            HookSig::PromptProgram => "prompt body",
            HookSig::Hook { kind } => kind.as_str(),
            HookSig::PluginFactory => "plugin factory",
            HookSig::Lifecycle { kind } => kind.as_str(),
        }
    }
}

// ── Per-program policy ──────────────────────────────────────────────────

/// Whether a program's turns may hand the controlling terminal to a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalPolicy {
    Denied,
    Leased,
}

/// The host-stated policy for a registered program's turns: terminal
/// access, capture regime, and optional turn budget.
#[derive(Debug, Clone)]
pub struct DefaultPolicy {
    /// Terminal authority for turns run from this program.
    pub terminal: TerminalPolicy,
    /// Capture stdout/stderr for this program's turns.
    pub capture: bool,
    /// Optional per-turn wall; `None` = uncapped.
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

// ── Host program ────────────────────────────────────────────────────────

/// One entry in the program table: a named, typechecked, policy-tagged
/// turn root.
#[derive(Debug, Clone)]
pub struct HostProgram {
    /// The lexical binding — `{ value: Block/Lambda, scheme }` — built
    /// by the same scheme-inference path an ordinary session `let` uses.
    pub binding: Binding,
    /// The engine-declared fixed-arity signature this program was
    /// checked against at registration.
    pub sig: HookSig,
    /// The host-stated default policy for turns run from this program.
    pub policy: DefaultPolicy,
    /// Declaration site, for diagnostics.
    pub origin: Span,
}

impl HostProgram {
    /// Marked seam for mobility enforcement.  No-op in Phase 0; in a
    /// later phase this will validate that the program's captured `Env`
    /// holds only transportable state.
    pub fn validate(&self) -> Result<(), RegisterError> {
        let _ = &self.binding;
        Ok(())
    }
}

// ── Registration errors ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum RegisterError {
    /// The value is not a Block or Lambda.
    NotCallable {
        name: ProgramName,
        origin: Span,
        actual: String,
    },
    /// The value's arity does not match the expected signature.
    ArityMismatch {
        name: ProgramName,
        origin: Span,
        expected: usize,
        actual: usize,
        sig_label: String,
    },
    /// A program with this name is already registered.
    AlreadyRegistered { name: ProgramName, origin: Span },
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegisterError::NotCallable { name, actual, .. } => {
                write!(
                    f,
                    "cannot register '{}' as a host program: \
                     expected a Block or Lambda, got {}",
                    name, actual
                )
            }
            RegisterError::ArityMismatch {
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
            RegisterError::AlreadyRegistered { name, .. } => {
                write!(f, "host program '{}' is already registered", name)
            }
        }
    }
}
