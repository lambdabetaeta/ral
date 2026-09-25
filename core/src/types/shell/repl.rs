//! REPL-only scratch on `Shell.local`: no part of the language semantics, in
//! no wire format.  Every fork starts from `default()`.

#[derive(Debug, Default)]
pub struct ReplScratch {
    /// The plugins this shell's load door committed, in load order.
    pub plugins: Vec<PluginEntry>,
}

/// What unload must undo beyond the plugin's hooks: the aliases it installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginEntry {
    pub name: String,
    pub aliases: Vec<String>,
}
