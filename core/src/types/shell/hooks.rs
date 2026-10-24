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

use crate::io::RunIo;
use crate::process::RequestedTerminalAccess;
use crate::text::plural;
use crate::types::Shell;
use crate::types::Value;
use crate::types::{Binding, FrameHandle};

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, hash_map::Entry};
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
    Block,
    Hook {
        kind: String,
    },
}

impl HookSig {
    /// Parameters a thunk must take to register under this signature.
    pub(crate) fn expected_arity(&self) -> usize {
        match self {
            Self::Block => 0,
            Self::Hook { .. } => 1,
        }
    }

    /// Human-readable label for diagnostics.
    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Block => "block",
            Self::Hook { kind } => kind.as_str(),
        }
    }
}

// ── Per-hook policy ─────────────────────────────────────────────────────

/// The host-stated policy for a hook's runs.
///
/// [`Shell::run`] applies it over the dispatching request: `terminal` replaces
/// the requested authority, `io` can only tighten it to capture, and `aside`
/// runs it on [`Shell::join_session`], so nothing it does flows back.
#[derive(Debug, Clone)]
pub struct DefaultPolicy {
    pub terminal: RequestedTerminalAccess,
    pub(crate) io: RunIo,
    pub(crate) aside: bool,
}

impl DefaultPolicy {
    pub const fn denied() -> Self {
        Self {
            terminal: RequestedTerminalAccess::Denied,
            io: RunIo::Inherit,
            aside: false,
        }
    }

    pub const fn leased() -> Self {
        Self {
            terminal: RequestedTerminalAccess::Leased,
            ..Self::denied()
        }
    }

    pub const fn denied_capture() -> Self {
        Self {
            io: RunIo::Capture,
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
}
impl Hook {
    /// The single registration gate: the bound value must be a function — a
    /// `Block`, a `Lambda`, or a builtin — taking as many parameters as `sig`
    /// expects, a block's read off its type (`typecheck::takes`).
    ///
    /// # Errors
    /// [`RegisterError::NotFunction`] if it is neither,
    /// [`RegisterError::ArityMismatch`] if the parameter count differs.
    pub(crate) fn validate(&self, name: &HookName) -> Result<(), RegisterError> {
        let expected = self.sig.expected_arity();
        let actual = match &self.binding.value {
            Value::Thunk(_) => {
                let scheme = self
                    .binding
                    .scheme
                    .as_ref()
                    .expect("vet_hook infers every block's scheme");
                if crate::typecheck::takes(scheme, expected) {
                    return Ok(());
                }
                scheme.arity()
            }
            Value::Native { entry, applied } => {
                entry.decl.fixed_arity().saturating_sub(applied.len())
            }
            other => {
                return Err(RegisterError::NotFunction {
                    name: name.clone(),
                    actual: format!("{other}"),
                });
            }
        };
        if actual != expected {
            return Err(RegisterError::ArityMismatch {
                name: name.clone(),
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
        actual: String,
    },
    ArityMismatch {
        name: HookName,
        expected: usize,
        actual: usize,
        sig_label: String,
    },
    AlreadyRegistered {
        name: HookName,
    },
    NoSession,
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
                     expected {}, got {}",
                    name,
                    sig_label,
                    plural(*expected, "parameter"),
                    actual
                )
            }
            Self::AlreadyRegistered { name, .. } => {
                write!(f, "hook '{name}' is already registered")
            }
            Self::NoSession => write!(
                f,
                "hooks and plugins belong to the session, but this shell runs beside it \
                 (an aside hook, a spawned worker, a pipeline stage): \
                 did you mean to register it from the rc or the prompt itself?"
            ),
        }
    }
}

// ── The session's hook table ────────────────────────────────────────────

/// What the booted session owns: its hooks, and the plugin ledger beside them.
#[derive(Debug, Default)]
pub(crate) struct HookTable {
    hooks: HashMap<HookName, Hook>,
    plugins: Vec<PluginEntry>,
}

/// What unload must undo beyond the plugin's hooks: the alias frames it
/// installed, by handle, so an alias the user replaced survives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginEntry {
    pub name: String,
    pub aliases: Vec<FrameHandle>,
}

impl Shell {
    /// The read-only half of registration: infer the scheme and check `value`
    /// is a function of the arity `sig` expects.
    ///
    /// # Errors
    /// Whatever [`Hook::validate`] raises.
    pub fn vet_hook(
        &self,
        name: &HookName,
        value: Value,
        sig: HookSig,
        policy: DefaultPolicy,
    ) -> Result<Hook, RegisterError> {
        let scheme = self.value_scheme(&value);
        let hook = Hook {
            binding: Binding { value, scheme },
            sig,
            policy,
        };
        hook.validate(name).map(|()| hook)
    }

    /// Register a function [`Value`] — a `Block`, `Lambda`, or builtin — as a
    /// named run root in the session hook table.
    ///
    /// It fires only on a host-dispatched `Program::Hook` run, never as `$name`
    /// and never as a command.
    ///
    /// # Errors
    /// Whatever [`Self::vet_hook`] raises, [`RegisterError::NoSession`] on a
    /// child shell, [`RegisterError::AlreadyRegistered`] if `name` is taken.
    pub fn register_hook(
        &mut self,
        name: HookName,
        value: Value,
        sig: HookSig,
        policy: DefaultPolicy,
    ) -> Result<(), RegisterError> {
        let hook = self.vet_hook(&name, value, sig, policy)?;
        let table = self
            .session
            .hooks
            .as_mut()
            .ok_or(RegisterError::NoSession)?;
        let Entry::Vacant(slot) = table.hooks.entry(name.clone()) else {
            return Err(RegisterError::AlreadyRegistered { name });
        };
        slot.insert(hook);
        Ok(())
    }

    /// Install already-vetted hooks, replacing any of the same name.  Fails
    /// only on a child shell, before changing anything.
    ///
    /// # Errors
    /// [`RegisterError::NoSession`] on a child shell.
    pub fn set_hooks(
        &mut self,
        hooks: impl IntoIterator<Item = (HookName, Hook)>,
    ) -> Result<(), RegisterError> {
        let table = self
            .session
            .hooks
            .as_mut()
            .ok_or(RegisterError::NoSession)?;
        table.hooks.extend(hooks);
        Ok(())
    }

    /// Whether a hook named `name` is registered.
    pub fn has_hook(&self, name: &HookName) -> bool {
        self.hook(name).is_some()
    }

    /// The hook registered under `name`, for a host applying it directly
    /// inside an existing command frame rather than through a dispatch door.
    pub fn hook(&self, name: &HookName) -> Option<&Hook> {
        self.session.hooks.as_ref()?.hooks.get(name)
    }

    /// Remove one hook by name, reporting whether it was there — the inverse of
    /// [`Self::register_hook`] for a spent one-shot entry point, such as a
    /// plugin factory.
    pub fn unregister_hook(&mut self, name: &HookName) -> bool {
        let table = self.session.hooks.as_mut();
        table.is_some_and(|t| t.hooks.remove(name).is_some())
    }

    /// Drop every hook under a plugin's namespace, returning the count.  One
    /// sweep at unload, so no dispatchable entry point outlives the plugin that
    /// owned it; also the rollback path for a load that fails partway.
    pub fn remove_plugin_hooks(&mut self, plugin_id: &str) -> usize {
        let Some(table) = self.session.hooks.as_mut() else {
            return 0;
        };
        let before = table.hooks.len();
        table
            .hooks
            .retain(|name, _| !matches!(&name.namespace, Namespace::Plugin(id) if id == plugin_id));
        before - table.hooks.len()
    }

    /// Whether the session has a plugin loaded under `name`.
    ///
    /// # Errors
    /// [`RegisterError::NoSession`] on a child shell.
    pub fn plugin_loaded(&self, name: &str) -> Result<bool, RegisterError> {
        let table = self
            .session
            .hooks
            .as_ref()
            .ok_or(RegisterError::NoSession)?;
        Ok(table.plugins.iter().any(|p| p.name == name))
    }

    /// Record a fully loaded plugin on the ledger.
    ///
    /// # Errors
    /// [`RegisterError::NoSession`] on a child shell.
    pub fn commit_plugin(&mut self, entry: PluginEntry) -> Result<(), RegisterError> {
        let table = self
            .session
            .hooks
            .as_mut()
            .ok_or(RegisterError::NoSession)?;
        table.plugins.push(entry);
        Ok(())
    }

    /// Take a plugin off the ledger, for unload to undo.
    pub fn take_plugin(&mut self, name: &str) -> Option<PluginEntry> {
        let plugins = &mut self.session.hooks.as_mut()?.plugins;
        Some(plugins.remove(plugins.iter().position(|p| p.name == name)?))
    }
}
