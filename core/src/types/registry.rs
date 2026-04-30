//! Alias and plugin registry.
//!
//! [`Registry`] tracks registered aliases, loaded plugins, and a generation
//! counter that child envs use to propagate changes back to the parent via
//! `return_to`.  [`Modules`] holds the `use`/`source` result cache, active-
//! load stack for cycle detection, and current recursion depth.

use std::collections::HashMap;
use super::value::Value;
use super::capability::Capabilities;

/// Where an alias came from.  Plugin-registered aliases dispatch under the
/// owning plugin's `capabilities`; user aliases run under the caller's.
#[derive(Clone, Debug, PartialEq)]
pub enum AliasOrigin {
    User,
    /// Plugin name — looked up in `Shell.plugins` at call time.
    Plugin(std::string::String),
}

/// A registered alias: the thunk to run, plus the original source text
/// captured at registration time (if available).  The source is shown by
/// `which` so users see what they wrote rather than elaborated IR.
#[derive(Clone, Debug, PartialEq)]
pub struct AliasEntry {
    pub value: Value,
    pub source: Option<std::string::String>,
    pub origin: AliasOrigin,
}

impl AliasEntry {
    pub fn new(value: Value) -> Self {
        Self {
            value,
            source: None,
            origin: AliasOrigin::User,
        }
    }

    pub fn with_source(value: Value, source: impl Into<std::string::String>) -> Self {
        Self {
            value,
            source: Some(source.into()),
            origin: AliasOrigin::User,
        }
    }

    /// Alias registered by a plugin.  Dispatches under the plugin's grant.
    pub fn from_plugin(value: Value, plugin: impl Into<std::string::String>) -> Self {
        Self {
            value,
            source: None,
            origin: AliasOrigin::Plugin(plugin.into()),
        }
    }
}

/// A loaded plugin in the plugin registry.
#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub name: std::string::String,
    pub capabilities: Capabilities,
    pub hooks: HashMap<std::string::String, Value>,
    pub keybindings: Vec<(std::string::String, Value)>,
    /// Aliases registered by this plugin; removed from `Shell.aliases` on unload.
    pub aliases: Vec<(std::string::String, AliasEntry)>,
    pub state_cell: Option<Value>,
}

/// Plugin-registered aliases and loaded plugins.  `generation` is bumped on
/// every load/unload; `return_to` uses it to detect whether a child thunk
/// mutated the registry and flow the changes back to the parent shell.
#[derive(Clone, Default, Debug)]
pub struct Registry {
    pub aliases: HashMap<std::string::String, AliasEntry>,
    pub plugins: Vec<LoadedPlugin>,
    pub generation: usize,
}

/// Module-loader state for `use` and `source`: result cache, active-load
/// stack (for cycle detection), and current recursion depth.
#[derive(Clone, Default, Debug)]
pub struct Modules {
    pub cache: HashMap<std::string::String, Value>,
    pub stack: Vec<std::string::String>,
    pub depth: usize,
}

impl Registry {
    /// Clone child into parent iff the child's generation counter advanced.
    /// Skips the clone when no plugin was loaded/unloaded, keeping the hot
    /// thunk path allocation-free.
    pub fn merge_from(&mut self, child: &Registry) {
        if self.generation != child.generation {
            self.clone_from(child);
        }
    }
}
