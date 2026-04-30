//! Shell state.
//!
//! [`Shell`] is the central interpreter state: lexical env, dynamic context,
//! control-flow counters, IO sinks, plugin registry, module loader, and audit
//! collector.  [`HeritableSnapshot`] is the `Send + Clone` subset that every
//! child computation (same-thread thunk or spawned thread) inherits.
//!
//! The inheritance protocol is:
//! - [`Shell::child_of`] / [`Shell::with_child`]: same-thread thunk body —
//!   move read-once bits (IO stack, audit tree, plugin context) out of parent,
//!   fold back via [`Shell::return_to`].
//! - [`Shell::spawn_thread`]: thread-spawn — snapshot heritable state, each
//!   thread owns its own IO.
//! - [`Shell::child_from`]: REPL aside (prompt, hook) — clone heritable state
//!   without touching IO; no flow-back.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::io::Write as _;
use crate::io::Io;
use crate::path::tilde::{TildePath, expand_tilde_path};
use super::value::Value;
use super::error::{EvalSignal, Error, ErrorKind};
use super::env::Env;
use super::dynamic::Dynamic;
use super::control::ControlState;
use super::repl::ReplScratch;
use super::capability::{Capabilities, SandboxProjection};
use super::audit::{Audit, ExecNode, Location};
use super::registry::{Registry, Modules};

/// Default cap on non-tail closure-call depth.  Insurance against
/// stack-guard SIGABRT from runaway recursion the typechecker can't
/// catch.  Tail calls are landed in the trampoline loop and don't
/// count.  Overridable via rc / CLI; in practice never tuned.
pub const DEFAULT_RECURSION_LIMIT: usize = 1024;

/// The state a child computation inherits from its parent — whether that
/// child is a thunk body (same thread) or a spawned thread (`spawn`, `par`,
/// pipeline stage).  Owning & `Send + Clone`, so it can be moved across
/// threads without borrowing the parent's `Shell`.
///
/// `inherit_from` installs this bundle plus four same-thread-only bits
/// (`io`, `in_tail_position`, `audit.tree` move, `plugin_context` move).
/// Thread-spawn sites install it and set up IO themselves.
///
/// Deliberately excluded:
/// - `io`: per-spawn sinks/sources — each child constructs its own.
/// - `audit.tree`: thread-local; cross-thread audit flows back through
///   the handle, not through shell state.
/// - `plugin_context`: REPL-local editor state, not meaningful off-thread.
/// - `last_status` / `in_tail_position`: a spawned thread starts fresh.
#[derive(Debug, Clone, Default)]
pub struct HeritableSnapshot {
    pub dynamic: Dynamic,
    pub registry: Registry,
    pub modules: Modules,
    pub location: Location,
}

pub struct Shell {
    /// Lexical environment (ρ).  Closure-captured; doesn't flow through
    /// `inherit_from`/`spawn_thread`.  See `types/env.rs`.
    pub env: Env,
    /// Dynamic context (σ): shell vars, cwd, capabilities stack, handler
    /// stack, script_args.  Clones whole into children, drops on return.
    /// See `types/dynamic.rs`.
    pub dynamic: Dynamic,
    /// Evaluator control-flow counters: `last_status`, `in_tail_position`,
    /// `call_depth`, `recursion_limit`.  Different fields obey different
    /// flow rules — see `Shell::inherit_from` / `Shell::return_to` and
    /// `types/control.rs`.
    pub control: ControlState,
    /// Source-position tracking: where we are, where we were called from,
    /// and the cached source text for structured spans.
    pub location: Location,
    /// Pipeline-stage IO: streams, value channel, terminal state, flags.
    pub io: Io,
    /// Plugin registry: registered aliases, loaded plugins, and a generation
    /// counter bumped on every load/unload so child envs can signal changes
    /// back to the parent via `return_to`.
    pub registry: Registry,
    /// Module-loader state (`use`, `source`): cache, active-load stack, depth.
    pub modules: Modules,
    /// Audit collector: execution tree plus captured stdout/stderr from the
    /// most recent external command, so `record_exec` can attach them.
    pub audit: Audit,
    /// REPL-only scratch state (editor plugin context + queued chpwd
    /// notification).  Doesn't flow across threads or IPC; moved on
    /// same-thread thunk boundary.  See `types/repl.rs`.
    pub repl: ReplScratch,
    /// Exit-code hint table — loaded once at startup from the data directory.
    pub exit_hints: crate::exit_hints::ExitHints,
    /// Structured-concurrency cancel scope.  `signal::check` consults
    /// this between effectful steps; setting the scope's flag (e.g. via
    /// `RunningPipeline::Drop` on the abort path) unwinds every thread
    /// that inherited the scope at its next poll point.  Default is a
    /// never-cancelled root scope, so non-pipeline code is unaffected.
    pub cancel: crate::signal::CancelScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandHead {
    Alias,
    Builtin,
    GrantDenied,
    External,
}

impl Shell {
    /// Create a new environment with the given terminal state.
    ///
    /// The terminal state must be provided explicitly so that callers cannot
    /// accidentally leave it at the default (all-false) — which would cause
    /// external commands to see piped I/O instead of the real terminal.
    pub fn new(terminal: crate::io::TerminalState) -> Self {
        Shell {
            env: Env::new(),
            dynamic: Dynamic {
                capabilities_stack: vec![Capabilities::root()],
                ..Dynamic::default()
            },
            control: ControlState::default(),
            io: crate::io::Io {
                terminal,
                ..Default::default()
            },
            repl: ReplScratch::default(),
            exit_hints: crate::exit_hints::ExitHints::default(),
            location: Location::default(),
            registry: Registry::default(),
            modules: Modules::default(),
            audit: Audit::default(),
            cancel: crate::signal::CancelScope::root(),
        }
    }

    /// Run `f` with `capabilities` pushed on the capabilities stack for its
    /// dynamic extent.  The single gate for every entry into capability-checked
    /// code: user `grant { ... }` blocks and plugin hook/keybinding/alias
    /// dispatch all funnel through here, so no one forgets to push/pop.  Pushed
    /// on top of the caller's stack, so effective authority is always
    /// caller ∩ this layer.
    pub fn with_capabilities<R>(
        &mut self,
        capabilities: Capabilities,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.dynamic.capabilities_stack.push(capabilities);
        let r = f(self);
        self.dynamic.capabilities_stack.pop();
        r
    }

    /// Run code registered by `plugin_name` under that plugin's manifest
    /// capabilities.  Missing plugin state is treated as deny-all; unload
    /// should remove aliases first, but stale registry entries must fail
    /// closed if that invariant is ever broken.
    pub fn with_registered_plugin_capabilities<R>(
        &mut self,
        plugin_name: &str,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let capabilities = self
            .registry
            .plugins
            .iter()
            .find(|p| p.name == plugin_name)
            .map(|p| p.capabilities.clone())
            .unwrap_or_else(Capabilities::deny_all);
        self.with_capabilities(capabilities, f)
    }

    /// True when a non-root capabilities layer is active.
    pub fn has_active_capabilities(&self) -> bool {
        self.dynamic
            .capabilities_stack
            .iter()
            .any(Capabilities::is_restrictive)
    }

    /// Run `f` with `overrides` merged into the ambient shell vars.  Pair
    /// of the `within [shell: ...]` keyword.
    pub fn with_env<R>(
        &mut self,
        overrides: HashMap<std::string::String, std::string::String>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let saved = self.dynamic.env_vars.clone();
        self.dynamic.env_vars.extend(overrides);
        let r = f(self);
        self.dynamic.env_vars = saved;
        r
    }

    /// Run `f` with `cwd` set as the ambient working directory.  Pair of
    /// the `within [dir: ...]` keyword.
    pub fn with_cwd<R>(&mut self, cwd: std::path::PathBuf, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.dynamic.cwd.replace(cwd);
        let r = f(self);
        self.dynamic.cwd = saved;
        r
    }

    /// Run `f` with `frame` pushed onto the handler stack.  Pair of the
    /// `within [handlers: ..., handler: ...]` keywords.
    pub fn with_handlers<R>(
        &mut self,
        frame: super::value::HandlerFrame,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.dynamic.handler_stack.push(frame);
        let r = f(self);
        self.dynamic.handler_stack.pop();
        r
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.env.get(name)
    }

    /// Look up in local scopes only (skipping the prelude).
    pub fn get_local(&self, name: &str) -> Option<&Value> {
        self.env.get_local(name)
    }

    /// Look up in the prelude scope only.
    pub fn get_prelude(&self, name: &str) -> Option<&Value> {
        self.env.get_prelude(name)
    }

    /// Construct an `EvalSignal::Error` located at the current source position.
    pub fn err(&self, msg: impl Into<String>, status: i32) -> EvalSignal {
        EvalSignal::Error(Error::new(msg, status).at(self.location.line, self.location.col))
    }

    /// Like `err`, with an additional hint.
    pub fn err_hint(
        &self,
        msg: impl Into<String>,
        hint: impl Into<String>,
        status: i32,
    ) -> EvalSignal {
        EvalSignal::Error(
            Error::new(msg, status)
                .at(self.location.line, self.location.col)
                .with_hint(hint),
        )
    }

    /// Like `err_hint`, but with `ErrorKind::PatternMismatch` so `try_apply` can catch it.
    pub fn pm_err(
        &self,
        msg: impl Into<String>,
        hint: impl Into<String>,
        status: i32,
    ) -> EvalSignal {
        EvalSignal::Error(
            Error::new(msg, status)
                .at(self.location.line, self.location.col)
                .with_hint(hint)
                .with_kind(ErrorKind::PatternMismatch),
        )
    }

    /// Resolve pseudo-variables (`$env`, `$args`, `$script`, `$nproc`) and
    /// names of registered builtins at value-position lookup.  A bare builtin
    /// name `$foo` synthesises a thunk `U(λx₁…λxₙ. Builtin(foo, x⃗))` so the
    /// reference is callable like any user thunk and pinned to the primitive
    /// regardless of later aliasing.
    pub fn resolve_builtin(&self, name: &str) -> Option<Value> {
        match name {
            "env" => {
                let mut merged: HashMap<String, String> = std::env::vars().collect();
                merged.extend(self.dynamic.env_vars.clone());
                let mut pairs: Vec<_> = merged
                    .into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect();
                pairs.sort_by(|a, b| a.0.cmp(&b.0));
                Some(Value::Map(pairs))
            }
            "args" => Some(Value::List(
                self.dynamic.script_args.iter().cloned().map(Value::String).collect(),
            )),
            // $script: path of the currently-executing file.  Empty in the REPL,
            // under `-c`, and during prelude loading.
            "script" => match self.location.script.as_str() {
                "" | "-c" | "<prelude>" => None,
                s => Some(Value::String(s.to_string())),
            },
            "nproc" => Some(Value::Int(
                std::thread::available_parallelism()
                    .map(|n| n.get() as i64)
                    .unwrap_or(1),
            )),
            _ => crate::builtins::synthesize_builtin_thunk(name),
        }
    }

    pub fn set(&mut self, name: std::string::String, value: Value) {
        self.env.set(name, value);
    }

    pub fn push_scope(&mut self) {
        self.env.push_scope();
    }

    pub fn pop_scope(&mut self) {
        self.env.pop_scope();
    }

    #[inline]
    pub fn set_status_from_bool(&mut self, ok: bool) {
        self.control.last_status = if ok { 0 } else { 1 };
    }

    /// Write `bytes` to the current stdout sink.
    ///
    /// `BrokenPipe` is treated as a clean shutdown: the downstream reader has
    /// closed its end of the pipe (e.g. `fzf` accepted a selection, `head`
    /// took its quota), so further writes are pointless but not an error.
    /// This matches traditional Unix tools, which exit silently on `SIGPIPE`,
    /// and prevents the pipeline supervisor from interpreting an EPIPE on a
    /// builtin writer as a failure that warrants tearing the pgid down with
    /// `SIGKILL` — a teardown that would surface as exit status 137 on
    /// sibling stages that had themselves exited cleanly.
    pub fn write_stdout(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self.io.stdout.write_all(bytes) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Look up the innermost handler for `name` across all active `within` frames.
    ///
    /// Walks `handler_stack` innermost-first (last element = innermost).  Within
    /// each frame, a per-name match takes priority over the catch-all.  If neither
    /// matches in a frame, falls through to the next outer frame.
    ///
    /// Returns `(thunk, is_catch_all, depth)`.  `depth` is the number of frames
    /// from the top of the stack that include and precede the matched frame; the
    /// caller strips them before invoking (shallow-handler semantics).
    pub fn lookup_handler(&self, name: &str) -> Option<(Value, bool, usize)> {
        for (depth, frame) in self.dynamic.handler_stack.iter().rev().enumerate() {
            if let Some((_, thunk)) = frame.per_name.iter().find(|(k, _)| k == name) {
                return Some((thunk.clone(), false, depth + 1));
            }
            if let Some(thunk) = &frame.catch_all {
                return Some((thunk.clone(), true, depth + 1));
            }
        }
        None
    }

    /// Forwarder — see [`Dynamic::should_audit_capabilities`].
    pub fn should_audit_capabilities(&self) -> bool {
        self.dynamic.should_audit_capabilities(&self.audit)
    }

    /// Forwarder — see [`Dynamic::check_editor_read`].
    pub fn check_editor_read(&self, subcmd: &str) -> Result<(), EvalSignal> {
        self.dynamic.check_editor_read(subcmd)
    }

    /// Forwarder — see [`Dynamic::check_editor_write`].
    pub fn check_editor_write(&self, subcmd: &str) -> Result<(), EvalSignal> {
        self.dynamic.check_editor_write(subcmd)
    }

    /// Forwarder — see [`Dynamic::check_editor_tui`].
    pub fn check_editor_tui(&self) -> Result<(), EvalSignal> {
        self.dynamic.check_editor_tui()
    }

    /// Forwarder — see [`Dynamic::check_shell_chdir`].
    pub fn check_shell_chdir(&self) -> Result<(), EvalSignal> {
        self.dynamic.check_shell_chdir()
    }

    /// Change the process working directory, updating `PWD`/`OLDPWD` in both
    /// `env_vars` and the ral scope.  Returns `(old_path, new_path)` on
    /// success so the caller can fire the `chpwd` lifecycle hook.
    ///
    /// Tilde expansion is delegated to `expand_tilde_path`; an empty `target`
    /// is treated as `~`.
    pub fn apply_chdir(&mut self, target: &str) -> Result<(String, String), EvalSignal> {
        let old = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        let home = self.dynamic.home();
        let home = if home.is_empty() { ".".into() } else { home };
        let resolved = if target.is_empty() {
            expand_tilde_path(None, None, &home)
        } else if let Some(path) = TildePath::parse(target) {
            expand_tilde_path(path.user.as_deref(), path.suffix.as_deref(), &home)
        } else {
            target.into()
        };

        std::env::set_current_dir(&resolved)
            .map_err(|e| EvalSignal::Error(Error::new(format!("{resolved}: {e}"), 1)))?;

        let new = std::env::current_dir()
            .map_or_else(|_| resolved.clone(), |p| p.to_string_lossy().into_owned());

        self.dynamic.env_vars.insert("OLDPWD".into(), old.clone());
        self.dynamic.env_vars.insert("PWD".into(), new.clone());
        self.set("OLDPWD".into(), Value::String(old.clone()));
        self.set("PWD".into(), Value::String(new.clone()));
        self.control.last_status = 0;

        Ok((old, new))
    }

    /// Forwarder — see [`Dynamic::check_exec_args`].
    pub fn check_exec_args(
        &mut self,
        display_name: &str,
        policy_names: &[&str],
        args: &[String],
    ) -> Result<(), EvalSignal> {
        self.dynamic
            .check_exec_args(display_name, policy_names, args, &mut self.audit, &self.location)
    }

    /// Forwarder — see [`Dynamic::resolve_path`].
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        self.dynamic.resolver().lex(path)
    }

    /// Forwarder — see [`Dynamic::check_fs_read`].
    pub fn check_fs_read(&mut self, path: &str) -> Result<(), EvalSignal> {
        self.dynamic.check_fs_read(path, &mut self.audit, &self.location)
    }

    /// Forwarder — see [`Dynamic::check_fs_write`].
    pub fn check_fs_write(&mut self, path: &str) -> Result<(), EvalSignal> {
        self.dynamic.check_fs_write(path, &mut self.audit, &self.location)
    }

    /// Forwarder — see [`Dynamic::sandbox_projection`].
    pub fn sandbox_projection(&self) -> Option<SandboxProjection> {
        self.dynamic.sandbox_projection()
    }

    /// Locate `name` on disk via the shell's effective `PATH` and
    /// `cwd`.  Thin Shell-aware wrapper over [`crate::path::locate`];
    /// returns the absolute path of the executable file the shell
    /// would run for `name`, or `None` if no such file exists.
    ///
    /// Distinct from [`Self::classify_command_head`]: that consults
    /// only the policy ("would this command be admitted?"), this
    /// consults only the filesystem ("is this command installed?").
    /// Together they let `which` and the dispatch error path tell
    /// "denied but installed" apart from "not installed."
    pub fn locate_command(&self, name: &str) -> Option<std::path::PathBuf> {
        let env_path = self.dynamic.env_vars.get("PATH").cloned();
        let env_path = env_path.or_else(|| std::env::var("PATH").ok());
        crate::path::locate(name, env_path.as_deref(), self.dynamic.cwd.as_deref())
    }

    /// Resolve the command-side lookup order for a head name after
    /// elaboration has ruled out local/prelude value bindings.
    pub fn classify_command_head(&self, name: &str) -> CommandHead {
        if self.registry.aliases.contains_key(name) {
            CommandHead::Alias
        } else if crate::builtins::is_builtin(name) {
            CommandHead::Builtin
        } else {
            let names = self.bare_policy_names(name);
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            if self.dynamic.is_exec_denied_for(&refs) {
                CommandHead::GrantDenied
            } else {
                CommandHead::External
            }
        }
    }

    /// Names by which a bare command head matches an `exec` capability key:
    /// the bare name plus its `PATH`-resolved path (when distinct).  When
    /// the active scope's `PATH` redirects resolution away from the system
    /// path, the bare name is dropped — an outer grant keyed by the bare
    /// name must not silently allow a spoofed binary.  Mirrors the bare-
    /// case logic of [`evaluator::exec::exec_policy_names`] so classify and
    /// check agree on which keys gate a command.
    fn bare_policy_names(&self, name: &str) -> Vec<String> {
        let mut names = vec![name.to_string()];
        if name.contains('/') {
            return names;
        }
        let active = self
            .dynamic
            .env_vars
            .get("PATH")
            .cloned()
            .or_else(|| std::env::var("PATH").ok());
        let Some(path) = active else {
            return names;
        };
        let Some(resolved) = crate::path::resolve_in_path(name, &path) else {
            return names;
        };
        let baseline = std::env::var("PATH")
            .ok()
            .and_then(|p| crate::path::resolve_in_path(name, &p));
        if baseline.as_deref() != Some(&resolved) {
            names.clear();
        }
        names.push(resolved);
        names
    }

    /// Get the innermost scope (for `use` to collect bindings).
    pub fn top_scope(&self) -> &HashMap<std::string::String, Value> {
        self.env.top_scope()
    }

    /// Get all bindings across all scopes (innermost wins).
    pub fn all_bindings(&self) -> Vec<(std::string::String, Value)> {
        self.env.all_bindings()
    }

    /// Snapshot the current scope chain for closure capture.  Returns
    /// an `Arc<Env>` so multiple closures (e.g. a `letrec` bank) created
    /// from one snapshot share one allocation; subsequent thunk clones
    /// are refcount bumps.
    pub fn snapshot(&self) -> Arc<Env> {
        Arc::new(self.env.clone())
    }

    /// Build a fresh runtime [`Shell`] whose lexical environment is a
    /// clone of `captured`.  Other components are defaulted — `dynamic`,
    /// `registry`, `location`, etc. start at their `Default::default()`.
    /// This is a building block for [`child_of`](Self::child_of),
    /// [`child_from`](Self::child_from), and
    /// [`spawn_thread`](Self::spawn_thread); external callers want one of
    /// those, since a defaulted shell has no inherited grants, env vars,
    /// or call-site location.
    fn from_captured(captured: &Env) -> Self {
        let mut shell = Self::new(Default::default());
        shell.env = captured.clone();
        shell
    }

    /// Thunk body: inherit heritable state from `parent` *and* move the
    /// read-once same-thread bits (pipe stdin, audit tree, plugin
    /// context) out of parent for the duration of the child's life.
    /// Pair with [`Shell::return_to`] to fold the mutations back.
    pub fn child_of(captured: &Env, parent: &mut Shell) -> Self {
        let mut child = Self::from_captured(captured);
        child.inherit_from(parent);
        child
    }

    /// Aside eval (prompt, REPL hook shell): clone heritable state from
    /// `parent` without touching its IO / audit / plugin context.  The
    /// child is an independent sibling; no flow-back is needed.
    pub fn child_from(captured: &Env, parent: &Shell) -> Self {
        let mut child = Self::from_captured(captured);
        child.install_heritable_snapshot(parent.heritable_snapshot());
        child
    }

    /// Run `f` in a child shell derived from `captured` and this shell (via
    /// `child_of`), then fold side-effects back via `return_to`.  The
    /// canonical same-thread thunk call.
    pub fn with_child<R>(&mut self, captured: &Env, f: impl FnOnce(&mut Shell) -> R) -> R {
        let mut child = Shell::child_of(captured, self);
        let result = f(&mut child);
        child.return_to(self);
        result
    }

    /// Spawn `f` on a fresh OS thread with a cloned child shell.  The caller
    /// supplies `scopes` — the thunk's captured closure scope for `spawn`
    /// / `par`, or the caller's own scope for pipeline stages — and this
    /// env's `HeritableSnapshot` is snapshotted and installed on the new
    /// thread.  Per-fork IO setup lives inside `f`.  The one and only
    /// thread-spawn primitive.
    pub fn spawn_thread<F, R>(&self, scopes: Arc<Env>, f: F) -> std::thread::JoinHandle<R>
    where
        F: FnOnce(&mut Shell) -> R + Send + 'static,
        R: Send + 'static,
    {
        let heritable = self.heritable_snapshot();
        std::thread::spawn(move || {
            let mut child = Self::from_captured(&scopes);
            child.install_heritable_snapshot(heritable);
            f(&mut child)
        })
    }

    /// Snapshot the `Send + Clone` bundle that any child computation
    /// (thunk or thread) inherits from this shell.  See
    /// [`HeritableSnapshot`].
    pub fn heritable_snapshot(&self) -> HeritableSnapshot {
        HeritableSnapshot {
            dynamic: self.dynamic.clone(),
            registry: self.registry.clone(),
            modules: self.modules.clone(),
            location: self.location.clone(),
        }
    }

    /// Install a previously-built `HeritableSnapshot`.
    pub fn install_heritable_snapshot(&mut self, s: HeritableSnapshot) {
        self.dynamic = s.dynamic;
        self.registry = s.registry;
        self.modules = s.modules;
        self.location = s.location;
    }

    /// Propagate runtime state from `parent` into this child shell for
    /// a same-thread thunk body.  Each line is one cell of the STT-in
    /// column of the flow matrix; pair with [`Self::return_to`].
    pub fn inherit_from(&mut self, parent: &mut Shell) {
        // Heritable bundle: dynamic, registry, modules, loc — clone-in.
        self.install_heritable_snapshot(parent.heritable_snapshot());
        // Control: STT-clone-in for tail flag, depth, limit.  Same OS
        // stack as parent — depth and limit both keep climbing.
        // `last_status` is *not* inherited; it starts at default and
        // flows back via `return_to`.
        self.control.in_tail_position = parent.control.in_tail_position;
        self.control.call_depth = parent.control.call_depth;
        self.control.recursion_limit = parent.control.recursion_limit;
        // IO: move-rich install (parent's pushed redirections become
        // the child's; restored in return_to).
        self.io.install_from_parent(&mut parent.io);
        // Audit tree: moved out of parent for the duration of the child.
        self.audit.tree = parent.audit.tree.take();
        // Plugin context: moved out of parent — editor scratch must not
        // be visible on both sides simultaneously.
        self.repl.plugin_context = parent.repl.plugin_context.take();
    }

    /// Flow mutations made by a child computation back to `parent`.
    /// Each line is one cell of the STT-out column of the flow matrix —
    /// the inverse of [`Self::inherit_from`].
    pub fn return_to(&mut self, parent: &mut Shell) {
        // Control: STT-rejoin only for last_status.  Tail flag, depth,
        // limit stay parent's.
        parent.control.last_status = self.control.last_status;
        // Registry: conditional merge (only if generation advanced).
        parent.registry.merge_from(&self.registry);
        // Modules: clone-replace.  Child cache wholesale wins.
        parent.modules.clone_from(&self.modules);
        // Audit: append captured streams; move tree back.
        parent.audit.append_from(&self.audit);
        parent.audit.tree = self.audit.tree.take();
        // Plugin context: moved back into parent.
        parent.repl.plugin_context = self.repl.plugin_context.take();
        // IO: stack-restore parent's pushed redirections.
        parent.io.return_to_parent(&mut self.io);
    }

    /// Run `f` inside a fresh audit scope.  The current `audit.tree` is
    /// swapped out, `f` runs with an empty tree visible to the body, and
    /// the collected children plus `f`'s result are returned while the
    /// parent tree is restored — even on panic would require catch_unwind,
    /// but the closure shape at least makes the restore structural rather
    /// than hand-rolled at each call site.
    pub fn with_audit_scope<F, R>(&mut self, f: F) -> (Vec<ExecNode>, R)
    where
        F: FnOnce(&mut Shell) -> R,
    {
        let saved = self.audit.tree.replace(Vec::new());
        let result = f(self);
        let children = std::mem::replace(&mut self.audit.tree, saved).unwrap_or_default();
        (children, result)
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

pub(crate) fn unique_strings(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = values.into_iter().collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod grant_policy_tests {
    use std::collections::BTreeMap;
    use super::{Shell};
    use crate::types::{Capabilities, CommandHead, ExecPolicy, FsPolicy};

    #[test]
    fn explicit_grant_denies_omitted_exec() {
        let mut shell = Shell::default();
        let head = shell.with_capabilities(Capabilities::deny_all(), |shell| {
            shell.classify_command_head("/bin/echo")
        });
        assert_eq!(head, CommandHead::GrantDenied);
    }

    /// `exec_dirs` admits a command whose resolved absolute path is
    /// under one of the listed prefixes, even when the per-name
    /// `exec` map has no entry.
    #[test]
    fn exec_dirs_allows_resolved_path_under_prefix() {
        let mut shell = Shell::default();
        let grant = Capabilities {
            exec: Some(BTreeMap::new()),
            exec_dirs: Some(vec!["/usr/bin".into()]),
            ..Capabilities::root()
        };
        shell
            .with_capabilities(grant, |shell| {
                shell.check_exec_args("ls", &["ls", "/usr/bin/ls"], &[])
            })
            .expect("ls under /usr/bin should be admitted by exec_dirs");
    }

    /// `exec_dirs` does not allow a binary outside any listed prefix.
    #[test]
    fn exec_dirs_denies_outside_prefixes() {
        let mut shell = Shell::default();
        let grant = Capabilities {
            exec: Some(BTreeMap::new()),
            exec_dirs: Some(vec!["/usr/bin".into()]),
            ..Capabilities::root()
        };
        let result = shell.with_capabilities(grant, |shell| {
            shell.check_exec_args("evil", &["evil", "/tmp/evil"], &[])
        });
        assert!(result.is_err());
    }

    /// Per-name `exec` policy wins over `exec_dirs`: a `Subcommands`
    /// restriction on a named entry must not be relaxed by a
    /// directory match.
    #[test]
    fn exec_dirs_does_not_relax_named_subcommands() {
        let mut shell = Shell::default();
        let grant = Capabilities {
            exec: Some(BTreeMap::from([(
                "cargo".into(),
                ExecPolicy::Subcommands(vec!["build".into()]),
            )])),
            exec_dirs: Some(vec!["/opt/homebrew/bin".into()]),
            ..Capabilities::root()
        };
        let result = shell.with_capabilities(grant, |shell| {
            shell.check_exec_args(
                "cargo",
                &["cargo", "/opt/homebrew/bin/cargo"],
                &["install".into()],
            )
        });
        assert!(
            result.is_err(),
            "named subcommand restriction should beat exec_dirs"
        );
    }

    /// A layer that declares only `exec` and misses should abstain so an
    /// enclosing `exec_dirs` layer can still allow the resolved path.
    #[test]
    fn exec_name_only_layer_abstains_and_outer_exec_dirs_allows() {
        let mut shell = Shell::default();
        let outer = Capabilities {
            exec_dirs: Some(vec!["/usr/bin".into()]),
            ..Capabilities::root()
        };
        let inner = Capabilities {
            exec: Some(BTreeMap::from([("git".into(), ExecPolicy::Allow)])),
            ..Capabilities::root()
        };
        shell.with_capabilities(outer, |shell| {
            shell.with_capabilities(inner, |shell| {
                shell.check_exec_args("ls", &["ls", "/usr/bin/ls"], &[])
            })
        })
        .expect("inner name-only miss should not override outer exec_dirs allow");
    }

    /// `exec_dirs = []` is an explicit opinion that no directory match is
    /// allowed, so a layer that also declares `exec` stays strict.
    #[test]
    fn explicit_empty_exec_dirs_keeps_single_layer_strict() {
        let mut shell = Shell::default();
        let outer = Capabilities {
            exec_dirs: Some(vec!["/usr/bin".into()]),
            ..Capabilities::root()
        };
        let inner = Capabilities {
            exec: Some(BTreeMap::from([("git".into(), ExecPolicy::Allow)])),
            exec_dirs: Some(Vec::new()),
            ..Capabilities::root()
        };
        let result = shell.with_capabilities(outer, |shell| {
            shell.with_capabilities(inner, |shell| {
                shell.check_exec_args("ls", &["ls", "/usr/bin/ls"], &[])
            })
        });
        assert!(
            result.is_err(),
            "explicit empty exec_dirs should keep the inner layer restrictive"
        );
    }

    #[test]
    fn exec_path_override_requires_resolved_path_authority() {
        let mut shell = Shell::default();
        let grant = Capabilities {
            exec: Some(BTreeMap::from([("git".into(), ExecPolicy::Allow)])),
            ..Capabilities::root()
        };
        let args = vec!["status".into()];
        let result = shell.with_capabilities(grant, |shell| {
            shell.check_exec_args("git", &["/tmp/fake-bin/git"], &args)
        });
        assert!(result.is_err());
    }

    #[test]
    fn exec_path_override_allows_explicit_resolved_path() {
        let mut shell = Shell::default();
        let grant = Capabilities {
            exec: Some(BTreeMap::from([("/tmp/fake-bin/git".into(), ExecPolicy::Allow)])),
            ..Capabilities::root()
        };
        let args = vec!["status".into()];
        shell.with_capabilities(grant, |shell| {
            shell.check_exec_args("git", &["/tmp/fake-bin/git"], &args)
        })
        .expect("resolved-path grant should allow the substituted executable");
    }

    #[test]
    fn sandbox_projection_intersects_path_components() {
        let mut shell = Shell::default();
        let outer = Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec!["/tmp/ral-prefix-a".into()],
                write_prefixes: Vec::new(),
                deny_paths: Vec::new(),
            }),
            ..Capabilities::root()
        };
        let inner = Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec!["/tmp/ral-prefix-ab".into()],
                write_prefixes: Vec::new(),
                deny_paths: Vec::new(),
            }),
            ..Capabilities::root()
        };
        let projection = shell.with_capabilities(outer, |shell| {
            shell.with_capabilities(inner, |shell| shell.sandbox_projection().unwrap())
        });
        assert!(projection.check_spec(&shell).read_prefixes.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_projection_does_not_leak_outer_raw_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let inner_dir = real.join("inner");
        let link = temp.path().join("link");
        std::fs::create_dir_all(&inner_dir).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let mut shell = Shell::default();
        let outer = Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![link.to_string_lossy().into_owned()],
                write_prefixes: Vec::new(),
                deny_paths: Vec::new(),
            }),
            ..Capabilities::root()
        };
        let inner = Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![inner_dir.to_string_lossy().into_owned()],
                write_prefixes: Vec::new(),
                deny_paths: Vec::new(),
            }),
            ..Capabilities::root()
        };

        let projection = shell.with_capabilities(outer, |shell| {
            shell.with_capabilities(inner, |shell| shell.sandbox_projection().unwrap())
        });
        let bind_spec = projection.bind_spec();
        let check_spec = projection.check_spec(&shell);
        let canonical_inner = shell
            .dynamic
            .resolver()
            .check(&inner_dir.to_string_lossy())
            .to_string_lossy()
            .into_owned();
        assert!(
            !bind_spec
                .read_prefixes
                .contains(&link.to_string_lossy().into_owned())
        );
        assert!(
            bind_spec
                .read_prefixes
                .contains(&inner_dir.to_string_lossy().into_owned())
        );
        assert!(check_spec.read_prefixes.contains(&canonical_inner));
    }
}
