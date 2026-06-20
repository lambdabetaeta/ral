//! Interactive read-eval-print loop for the `ral` shell.
//!
//! Public surface is one function: [`run_interactive`].  The real work
//! lives in [`session::Session`], which owns the long-lived state and
//! runs the loop.  This file is the entry point and module-tree root.
//!
//! Submodules factor out orthogonal concerns:
//! - [`completion`] -- Frontend-neutral completion engine (classification,
//!   candidate sources, fuzzy/prefix ranking) shared by every surface.
//! - [`complete`] -- The rustyline adapter over [`completion`]: completer,
//!   ghost-text hinter, and plugin syntax highlighting.
//! - [`config`]   -- RC file discovery, parsing, and application.
//! - [`cursor`]   -- ANSI cursor-position queries (Unix only).
//! - [`errfmt`]   -- REPL-specific error formatting helpers.
//! - [`exec`]     -- Single-input parse/typecheck/eval cycle.
//! - [`frontend`] -- The `Frontend` trait and its two implementations.
//! - [`keybinding`] -- Plugin keybinding dispatch.
//! - [`plugin`]   -- Plugin runtime state and hook machinery.
//! - [`plugin_editor`] -- Plugin context / editor state types (host of
//!   the type-erased `Box<dyn Any>` core stores in `ReplScratch`).
//! - [`plugin_ed_builtins`] -- The `_ed-*` builtin family, registered
//!   into core's static host-builtin table.
//! - [`host_handlers`] -- Captured builtins for job-control and
//!   plugin-lifecycle commands; installed into the session builtin table
//!   at boot.
//! - [`prompt`]   -- Prompt construction and thunk evaluation.
//! - [`session`]  -- The REPL state machine driving the loop.
//! - [`theme`]    -- REPL value-output styling (configurable from rc).
//! - [`worksheet`] -- The REPL-side worksheet model: per-binding
//!   dependency edges and the pure/effectful verdict, retained across turns
//!   for the structural surface's reactive worksheet.

mod complete;
mod completion;
mod config;
mod plugin;
mod plugin_ed_builtins;
mod plugin_editor;

mod cursor;
mod errfmt;
mod exec;
mod frontend;
mod host_handlers;
mod keybinding;
mod prompt;
mod session;
mod theme;
#[cfg(feature = "structural")]
mod worksheet;

pub(crate) use frontend::Surface;
use session::Session;
use std::process::ExitCode;

/// Register the `ral` host's builtins with core: the `_ed-*` editor
/// family and `watch`.  Must run before the type checker consults
/// `builtin_names()` and before either is invoked.  `watch` lives in core
/// but is the host's to install — the ral binary has a durable stdout sink
/// a detached watcher can outlive the turn writing to, where an agent host
/// does not.  Idempotent.
pub(crate) fn register_host_surface() {
    ral_core::builtins::register_builtins(plugin_ed_builtins::ED_BUILTINS);
    ral_core::builtins::register_builtins(ral_core::builtins::WATCH_BUILTIN);
}

/// Enter the interactive REPL.
///
/// Boots a [`Session`] (signals, terminal, builtins, profiles, capabilities,
/// frontend) and drives its loop to completion.  Returns the exit code
/// to hand back to the OS.
pub(crate) fn run_interactive(is_login: bool, opts: &super::InteractiveOpts) -> ExitCode {
    match Session::boot(is_login, opts) {
        Ok(session) => session.run(),
        Err(code) => code,
    }
}
