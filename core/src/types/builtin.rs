//! Builtin command bindings.
//!
//! A [`BuiltinEntry`] is a command name implemented by host Rust code;
//! the per-shell [`BuiltinTable`] holds the installed sets.  Builtin
//! bindings live separately from the user handler stack.

use super::flow::Settled;
use super::value::Value;
use crate::typecheck::builtins::BuiltinTypeRule;
use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

/// Runtime closure backing a captured builtin body, carrying host state.
pub type CapturedBuiltinFn = Arc<
    dyn Fn(&[Value], &crate::types::Mooring, &mut crate::types::Shell) -> Settled<Value>
        + Send
        + Sync,
>;

/// Host implementation of a builtin command binding.
///
/// The [`Mooring`](crate::types::Mooring) sits between the arguments and the
/// shell in every spelling: it is the run's fixed conditions (where events
/// go, who answers, what stops it), borrowed rather than owned, and so
/// disjoint from the `&mut Shell` the body mutates.
#[derive(Clone)]
pub enum BuiltinBody {
    /// Process-static function pointer.
    Static(fn(&[Value], &crate::types::Mooring, &mut crate::types::Shell) -> Settled<Value>),
    /// Runtime closure with host state captured by the frontend.
    Captured(CapturedBuiltinFn),
}

impl BuiltinBody {
    /// Call the body with the given arguments, mooring, and shell.
    ///
    /// # Errors
    /// Returns `Err` if the invoked body raises a runtime `Break`.
    pub fn call(
        &self,
        args: &[Value],
        mooring: &crate::types::Mooring,
        shell: &mut crate::types::Shell,
    ) -> Settled<Value> {
        match self {
            Self::Static(f) => f(args, mooring, shell),
            Self::Captured(f) => f(args, mooring, shell),
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
    ///
    /// # Panics
    /// Panics if a name in `entries` collides with an already-installed
    /// builtin, or if a name appears twice within `entries`.
    pub fn install_static(&mut self, entries: &'static [BuiltinEntry]) {
        self.install_arc(Arc::from(entries));
    }

    /// Install runtime-owned builtin entries for this shell.
    ///
    /// Idempotent: re-installing a set already installed here — by `Arc`
    /// pointer identity, or by carrying the same names as a set already
    /// installed — is a no-op. Name collisions against a *different*
    /// installed set panic: host crates must own disjoint surfaces.
    ///
    /// # Panics
    /// Panics if a name in `entries` collides with an already-installed
    /// builtin, or if a name appears twice within `entries`.
    pub fn install_arc(&mut self, entries: Arc<[BuiltinEntry]>) {
        if self
            .sets
            .iter()
            .any(|set| Arc::ptr_eq(set, &entries) || same_builtin_names(set, &entries))
        {
            return;
        }
        if let Err(e) = check_builtin_collisions(&entries, &self.sets) {
            panic!("builtin installation failed: {e}");
        }
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

fn same_builtin_names(a: &[BuiltinEntry], b: &[BuiltinEntry]) -> bool {
    a.len() == b.len()
        && a.iter()
            .map(|entry| entry.name.as_ref())
            .all(|name| b.iter().any(|entry| entry.name == name))
}

fn check_builtin_collisions(
    new_entries: &[BuiltinEntry],
    installed: &imbl::Vector<Arc<[BuiltinEntry]>>,
) -> Result<(), String> {
    let mut local = HashSet::new();
    for entry in new_entries {
        let name = entry.name.as_ref();
        if !local.insert(name) {
            return Err(format!(
                "builtin `{name}` is installed twice in one builtin set"
            ));
        }
        if installed
            .iter()
            .flat_map(|set| set.iter())
            .any(|existing| existing.name == name)
        {
            return Err(format!(
                "builtin `{name}` conflicts with an installed builtin"
            ));
        }
    }
    Ok(())
}
