//! REPL-only scratch on `Shell.local`: no part of the language semantics, in
//! no wire format.  Every fork starts from `default()`.

#[derive(Debug, Default)]
pub struct ReplScratch {
    /// The plugins this shell's load door committed, in load order.
    pub plugins: Vec<PluginEntry>,
    /// The latest `cd`, which moves only the shell's logical cwd; a host reads
    /// it, never takes it, and fires `chpwd` on a `seq` it has not seen.
    pub last_chpwd: Option<Chpwd>,
}

/// What unload must undo beyond the plugin's hooks: the aliases it installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginEntry {
    pub name: String,
    pub aliases: Vec<String>,
}

/// One directory change: `seq` counts them, from 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chpwd {
    pub seq: u64,
    pub old: String,
    pub new: String,
}
