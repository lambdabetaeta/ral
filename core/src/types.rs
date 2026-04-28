use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::io::Io;
use crate::util::{TildePath, expand_tilde_path};
use std::io::Write as _;

// Lexical environment (ρ).  Closure-captured.  See types/env.rs.
mod env;
pub use env::Env;

// Dynamic context (σ).  Clone-into-child, drop-on-return.  See
// types/dynamic.rs.
mod dynamic;
pub use dynamic::Dynamic;

// Evaluator control-flow counters.  See types/control.rs.
mod control;
pub use control::ControlState;

// REPL-only scratch state (editor / chpwd hook).  See types/repl.rs.
mod repl;
pub use repl::ReplScratch;

// Capability layer: ExecPolicy, FsPolicy, EditorPolicy, ShellPolicy,
// SandboxPolicy/BindSpec/CheckSpec, Capabilities + meet.  See
// types/capability.rs.
mod capability;
pub use capability::{
    Capabilities, EditorPolicy, ExecPolicy, FsPolicy, SandboxBindSpec, SandboxCheckSpec,
    SandboxPolicy, ShellPolicy,
};

// ── Values ───────────────────────────────────────────────────────────────

/// The runtime representation of every ral value.
///
/// The interpreter passes `Value` between computations; it is what a variable
/// holds, what a pipeline stage produces, and what a builtin returns.
///
/// `Thunk` is a suspended computation with a captured scope snapshot (a
/// closure).  Whether the thunk is a lambda (`CompKind::Lam`) or a nullary
/// block is determined by inspecting the body — the `Value` itself is
/// opaque.
///
/// `Map` uses a `Vec` of pairs rather than a hash map so that key order is
/// preserved and maps can be compared structurally.
///
/// `Thunk::captured` is `Arc<Env>` so a `Value::clone` on a thunk is a
/// single refcount bump rather than a `Vec`-clone of the scope chain;
/// many closures sharing one capture site share one allocation.
#[derive(Debug, Clone)]
pub enum Value {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(std::string::String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Map(Vec<(std::string::String, Value)>),
    Thunk {
        body: std::sync::Arc<crate::ir::Comp>,
        captured: Arc<Env>,
    },
    /// Handle to a spawned subprocess.
    Handle(HandleInner),
}

impl Value {
    /// Convert to i64 for arithmetic, if possible.
    ///
    /// Accepts `Int` and whole `Float` values only — strings are never
    /// silently parsed.  Use the `int` builtin for explicit conversion.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            Value::Float(f) if *f == f.floor() => Some(*f as i64),
            _ => None,
        }
    }

    /// Convert to f64 for arithmetic, if possible.
    ///
    /// Accepts `Int` and `Float` values only — strings are never silently
    /// parsed.  Use the `float` builtin for explicit conversion.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Human-readable runtime type name used in diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Unit => "Unit",
            Value::Bool(_) => "Bool",
            Value::Int(_) => "Int",
            Value::Float(_) => "Float",
            Value::String(_) => "String",
            Value::Bytes(_) => "Bytes",
            Value::List(_) => "List",
            Value::Map(_) => "Map",
            Value::Thunk { .. } => "Block",
            Value::Handle(_) => "Handle",
        }
    }
}

/// Shared handle to a spawned computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleState {
    Running,
    Completed,
    Cancelled,
    Disowned,
}

/// Shared handle to a spawned computation.
#[derive(Debug, Clone)]
#[allow(clippy::type_complexity)]
pub struct HandleInner {
    /// Result channel: thread sends Result<Value, EvalSignal>, await receives.
    pub result: Arc<Mutex<Option<std::sync::mpsc::Receiver<Result<Value, EvalSignal>>>>>,
    /// Cached result after first await (§13.3: second await returns cached).
    pub cached: Arc<Mutex<Option<Result<Value, EvalSignal>>>>,
    /// Lifecycle state for handle-level APIs.
    pub state: Arc<Mutex<HandleState>>,
    /// Buffered stdout from the spawned block (§13.3 replay rule).  Bytes
    /// accumulate here during execution and are drained on `await`.  Always
    /// empty for watched handles — bytes flow live through `Sink::LineFramed`.
    pub stdout_buf: Arc<Mutex<Vec<u8>>>,
    /// Buffered stderr from the spawned block (§13.3 replay rule).  Always
    /// empty for watched handles.
    pub stderr_buf: Arc<Mutex<Vec<u8>>>,
    pub cmd: std::string::String,
}

impl PartialEq for HandleInner {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.result, &other.result)
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Unit, Value::Unit) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Bytes(a), Value::Bytes(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => a == b,
            // Closures and handles are never structurally equal.
            (Value::Thunk { .. }, Value::Thunk { .. }) => false,
            (Value::Handle(_), Value::Handle(_)) => false,
            _ => false,
        }
    }
}

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

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Unit => write!(f, ""),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(n) => write!(f, "{n}"),
            Value::String(s) => write!(f, "{s}"),
            Value::Bytes(b) => write!(f, "{}", String::from_utf8_lossy(b)),
            Value::List(items) => {
                if items.is_empty() {
                    return write!(f, "[]");
                }
                write!(f, "[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            Value::Map(pairs) => {
                if pairs.is_empty() {
                    return write!(f, "[:]");
                }
                write!(f, "[")?;
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "]")
            }
            Value::Thunk { body, .. } => write!(f, "{}", fmt_block(body)),
            Value::Handle(h) => write!(f, "<handle:{}>", h.cmd),
        }
    }
}

/// Render one pattern as a compact param string.
fn fmt_param(p: &crate::ast::Pattern) -> String {
    match p {
        crate::ast::Pattern::Wildcard => "_".into(),
        crate::ast::Pattern::Name(s) => s.clone(),
        crate::ast::Pattern::List { elems, rest } => {
            let mut parts: Vec<String> = elems.iter().map(fmt_param).collect();
            if let Some(r) = rest { parts.push(format!("...{r}")); }
            format!("[{}]", parts.join(" "))
        }
        crate::ast::Pattern::Map(entries) => {
            let parts: Vec<String> = entries.iter()
                .map(|(k, pat, _)| {
                    let v = fmt_param(pat);
                    if matches!(pat, crate::ast::Pattern::Name(n) if n == k) { k.clone() }
                    else { format!("{k}: {v}") }
                })
                .collect();
            format!("[{}]", parts.join(" "))
        }
    }
}

/// Walk nested `Lam` nodes to collect parameter names, then format as
/// `<block>` (nullary) or `<|a b| block>` (one or more params).
pub fn fmt_block(body: &crate::ir::Comp) -> String {
    let mut params: Vec<String> = Vec::new();
    let mut comp = body;
    loop {
        match &comp.kind {
            crate::ir::CompKind::Lam { param, body } => {
                params.push(fmt_param(param));
                comp = body;
            }
            _ => break,
        }
    }
    if params.is_empty() {
        "<block>".into()
    } else {
        format!("<|{}| block>", params.join(" "))
    }
}

// ── Effect handler frames (for `within`) ─────────────────────────────────

/// One `within [handlers: …, handler: …]` frame.
///
/// Per-name entries are checked before the catch-all within the same frame.
/// If a frame has neither a matching per-name nor a catch-all, dispatch falls
/// through to the next outer frame.
#[derive(Debug, Clone)]
pub struct HandlerFrame {
    /// Per-name handlers: `within [handlers: [NAME: thunk]]`.
    pub per_name: Vec<(std::string::String, Value)>,
    /// Catch-all handler: `within [handler: thunk]`.
    pub catch_all: Option<Value>,
}

/// Line editor state visible to plugins.
#[derive(Debug, Clone, Default)]
pub struct EditorState {
    pub text: std::string::String,
    pub cursor: usize,
    pub keymap: std::string::String,
}

/// A highlight span submitted by a plugin.
#[derive(Debug, Clone)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub style: std::string::String,
}

/// Execution context for `_editor` and `_plugin` builtins.
///
/// Read-only information the runtime supplies before a plugin handler runs.
#[derive(Debug, Clone, Default)]
pub struct PluginInputs {
    pub history_entries: Vec<std::string::String>,
    /// True when the handler is firing inside the readline loop (e.g. for
    /// `buffer-change`); `_editor 'tui'` is forbidden in that mode.
    pub in_readline: bool,
}

/// Effects produced by a plugin handler that the runtime applies after the
/// call returns.  Default-initialised before each call; populated only by the
/// handler via `_editor` builtins.
#[derive(Debug, Clone, Default)]
pub struct PluginOutputs {
    pub ghost_text: Option<std::string::String>,
    pub highlight_spans: Vec<HighlightSpan>,
    /// `_editor 'push'` saves the current buffer here for the runtime to
    /// stash on the buffer stack.
    pub pushed_buffer: Option<(std::string::String, usize)>,
    /// `_editor 'accept'` sets this; the runtime treats the post-call buffer
    /// as if the user pressed Enter.
    pub accept_line: bool,
}

/// Set on `Shell` before running plugin hooks/keybinding handlers.
/// The `_editor` builtins read and write through this rather than
/// touching shared REPL state directly, avoiding reentrancy.
///
/// The `inputs` / `outputs` split makes the data-flow direction visible at
/// every access site: callsites populate `inputs` before the call and inspect
/// `outputs` after.  `editor_state` is the live buffer (read and written by
/// the handler); `state_cell` and `in_tui` are internal scratch.
#[derive(Debug, Clone)]
pub struct PluginContext {
    pub inputs: PluginInputs,
    pub outputs: PluginOutputs,
    /// Live editor buffer.  Pre-populated by the runtime; the handler may
    /// mutate via `_editor 'set'`/`'push'`; the runtime reads after.
    pub editor_state: EditorState,
    /// Reentrancy guard for `_editor 'tui'`; not user-visible.
    pub in_tui: bool,
    /// Per-plugin scratch cell exposed via `_editor 'state'`.
    pub state_cell: Option<Value>,
    pub state_default_used: bool,
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

// ── Shell sub-structs ──────────────────────────────────────────────────────
//
// Fields of `Shell` that always travel together are grouped into sub-structs
// so `inherit_from` / `return_to` can clone a whole group in one line and
// readers see intent, not a wall of individual field copies.

/// A source position: script name + (line, col).  Used both for "where we
/// are now" and (via `Location::call_site`) "where we were called from".
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct CallSite {
    pub script: String,
    pub line: usize,
    pub col: usize,
}

/// Source-position tracking for diagnostics.  Holds where execution is,
/// where it was called from (saved before entering prelude wrappers so
/// `audit`/`_try` name the user's line, not the prelude's), and the
/// cached source text of the current script for structured spans.
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct Location {
    pub script: String,
    pub line: usize,
    pub col: usize,
    /// Cached source text; not serde-transmissible (Arc<str>), and the
    /// sandbox child doesn't need it for diagnostics.
    #[serde(skip)]
    pub source: Option<std::sync::Arc<str>>,
    pub call_site: CallSite,
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

/// Audit collector.  `tree` is `Some` when `_audit { ... }` has installed a
/// node list; `captured_stdout`/`_stderr` buffer the most recent external
/// command's output so `record_exec` can attach it to the tree node.
#[derive(Default, Debug)]
pub struct Audit {
    pub tree: Option<Vec<ExecNode>>,
    pub captured_stdout: Vec<u8>,
    pub captured_stderr: Vec<u8>,
}

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

// ── Environment ──────────────────────────────────────────────────────────

/// Default cap on non-tail closure-call depth.  Insurance against
/// stack-guard SIGABRT from runaway recursion the typechecker can't
/// catch.  Tail calls are landed in the trampoline loop and don't
/// count.  Overridable via rc / CLI; in practice never tuned.
pub const DEFAULT_RECURSION_LIMIT: usize = 1024;

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

/// Microseconds since the Unix epoch.
pub fn epoch_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}

/// The two kinds of execution-tree node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecNodeKind {
    Command,
    CapabilityCheck,
}

impl std::fmt::Display for ExecNodeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Command => "command",
            Self::CapabilityCheck => "capability-check",
        })
    }
}

/// A node in the execution tree. Every node has the same shape.
#[derive(Debug, Clone)]
pub struct ExecNode {
    pub kind: ExecNodeKind,
    pub cmd: String,
    pub args: Vec<String>,
    pub status: i32,
    pub script: String,
    pub line: usize,
    pub col: usize,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub value: Value,
    pub children: Vec<ExecNode>,
    pub start: i64,        // wall-clock start: microseconds since epoch
    pub end: i64,          // wall-clock end: microseconds since epoch
    pub principal: String, // $USER at time of recording
}

impl ExecNode {
    pub fn leaf(
        cmd: impl Into<String>,
        args: Vec<String>,
        status: i32,
        script: impl Into<String>,
        line: usize,
        col: usize,
    ) -> Self {
        ExecNode {
            kind: ExecNodeKind::Command,
            cmd: cmd.into(),
            args,
            status,
            script: script.into(),
            line,
            col,
            stdout: Vec::new(),
            stderr: Vec::new(),
            value: Value::Unit,
            children: Vec::new(),
            start: 0,
            end: 0,
            principal: String::new(),
        }
    }

    /// A capability-check event node.  The caller populates `node.value` with
    /// resource-specific fields (`name`/`args` for exec, `op`/`path` for fs)
    /// before pushing the node into the exec tree.
    pub fn capability_check(
        resource: &str,
        decision: &str,
        script: &str,
        line: usize,
        col: usize,
    ) -> Self {
        ExecNode {
            kind: ExecNodeKind::CapabilityCheck,
            cmd: resource.into(),
            args: Vec::new(),
            status: if decision == "denied" { 1 } else { 0 },
            script: script.into(),
            line,
            col,
            stdout: Vec::new(),
            stderr: Vec::new(),
            value: Value::Map(vec![
                ("resource".into(), Value::String(resource.into())),
                ("decision".into(), Value::String(decision.into())),
            ]),
            children: Vec::new(),
            start: epoch_us(),
            end: epoch_us(),
            principal: String::new(),
        }
    }

    /// Convert to a Value::Map matching the execution tree node shape.
    /// For `capability-check` nodes the fields stored in `self.value` are
    /// also spliced into the top-level map so that `resource`, `decision`,
    /// and the resource-specific fields appear alongside `cmd`/`status`.
    pub fn to_value(&self) -> Value {
        let args_list: Vec<Value> = self.args.iter().map(|a| Value::String(a.clone())).collect();
        let children_list: Vec<Value> = self.children.iter().map(|c| c.to_value()).collect();
        let mut pairs = vec![
            ("kind".into(), Value::String(self.kind.to_string())),
            ("cmd".into(), Value::String(self.cmd.clone())),
            ("args".into(), Value::List(args_list)),
            ("status".into(), Value::Int(self.status as i64)),
            ("script".into(), Value::String(self.script.clone())),
            ("line".into(), Value::Int(self.line as i64)),
            ("col".into(), Value::Int(self.col as i64)),
            ("stdout".into(), Value::Bytes(self.stdout.clone())),
            ("stderr".into(), Value::Bytes(self.stderr.clone())),
            ("value".into(), self.value.clone()),
            ("children".into(), Value::List(children_list)),
            ("start".into(), Value::Int(self.start)),
            ("end".into(), Value::Int(self.end)),
            ("principal".into(), Value::String(self.principal.clone())),
        ];
        if self.kind == ExecNodeKind::CapabilityCheck
            && let Value::Map(extra) = &self.value
        {
            pairs.extend(extra.iter().cloned());
        }
        Value::Map(pairs)
    }
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
    pub fn with_handlers<R>(&mut self, frame: HandlerFrame, f: impl FnOnce(&mut Self) -> R) -> R {
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

        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".into());
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
        self.dynamic.resolve_path(path)
    }

    /// Forwarder — see [`Dynamic::check_fs_read`].
    pub fn check_fs_read(&mut self, path: &str) -> Result<(), EvalSignal> {
        self.dynamic.check_fs_read(path, &mut self.audit, &self.location)
    }

    /// Forwarder — see [`Dynamic::check_fs_write`].
    pub fn check_fs_write(&mut self, path: &str) -> Result<(), EvalSignal> {
        self.dynamic.check_fs_write(path, &mut self.audit, &self.location)
    }

    /// Forwarder — see [`Dynamic::sandbox_policy`].
    pub fn sandbox_policy(&self) -> Option<SandboxPolicy> {
        self.dynamic.sandbox_policy()
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

// ── Capability policy ─────────────────────────────────────────────────────
//
// Capability checks live on `Dynamic` rather than on `Shell` so that the
// type system *prevents* policy code from reading lexical scope, REPL
// scratch, control state, or exit hints — the policy operates on the
// dynamic capability stack and emits into a separately-borrowed `Audit`,
// with diagnostic location passed as `&Location`.  `Shell::check_*` are
// thin shims that bind the right borrows.

/// One capability layer's vote on a candidate command.
enum LayerExec {
    /// Layer has no exec/exec_dirs opinion.
    NoOpinion,
    /// Layer has exec restrictions and the command matches none.
    Denied,
    /// Layer admits the command with this policy.
    Allowed(ExecPolicy),
}

/// Folded verdict across the whole capability stack.
enum ExecVerdict {
    /// No layer has any exec opinion.
    Unrestricted,
    /// At least one layer denies; the call is rejected.
    Denied,
    /// Every opining layer allowed; effective policy is the
    /// intersection of those layers' allowed policies.
    Allowed(ExecPolicy),
}

impl Dynamic {
    /// Decide a single layer's verdict on a command.  Two routes
    /// match a layer: (a) name in the layer's `exec` map, (b)
    /// resolved absolute path under one of the layer's `exec_dirs`.
    /// Name match wins if both are present (takes the named policy).
    ///
    /// `None` on either field is "no opinion" for that route.  A
    /// layer that declared only one route and missed it abstains,
    /// letting another layer admit the command by a different route.
    /// If no layer admits it, the stack-level fold denies rather than
    /// treating a restrictive exec grant as ambient authority.
    fn layer_exec_verdict(&self, ctx: &Capabilities, names: &[&str]) -> LayerExec {
        let exec_set = ctx.exec.is_some();
        let dirs_set = ctx.exec_dirs.is_some();
        if !exec_set && !dirs_set {
            return LayerExec::NoOpinion;
        }
        // Name match takes precedence.
        if let Some(exec) = &ctx.exec {
            let matched: Vec<&ExecPolicy> = exec
                .iter()
                .filter(|(k, _)| names.iter().any(|n| k == n))
                .map(|(_, p)| p)
                .collect();
            if let Some(first) = matched.first() {
                let policy = matched
                    .iter()
                    .skip(1)
                    .fold((*first).clone(), |acc, p| {
                        intersect_exec_policy(acc, (*p).clone())
                    });
                return LayerExec::Allowed(policy);
            }
        }
        // Fall back to dir match against the resolved absolute path(s).
        if let Some(dirs) = &ctx.exec_dirs
            && dirs.iter().any(|d| {
                names.iter().any(|n| {
                    let p = std::path::Path::new(n);
                    p.is_absolute()
                        && crate::path::path_within(p, std::path::Path::new(d))
                })
            })
        {
            return LayerExec::Allowed(ExecPolicy::Allow);
        }
        // Both routes declared, neither matched → strict deny.  Otherwise
        // abstain so an outer layer's opinion can decide.
        if exec_set && dirs_set {
            LayerExec::Denied
        } else {
            LayerExec::NoOpinion
        }
    }

    /// Walk the stack and combine per-layer verdicts.  Any layer that
    /// denies → command denied.  Any allowed opinions intersect.  If
    /// the stack declared exec policy but no layer admitted the command,
    /// deny; only a stack with no exec policy at all is unrestricted.
    fn evaluate_exec(&self, names: &[&str]) -> ExecVerdict {
        let mut policy: Option<ExecPolicy> = None;
        let mut any_opinion = false;
        let mut saw_exec_policy = false;
        for ctx in self.capabilities_stack.iter() {
            saw_exec_policy |= ctx.exec.is_some() || ctx.exec_dirs.is_some();
            match self.layer_exec_verdict(ctx, names) {
                LayerExec::NoOpinion => {}
                LayerExec::Denied => return ExecVerdict::Denied,
                LayerExec::Allowed(p) => {
                    any_opinion = true;
                    policy = Some(match policy.take() {
                        None => p,
                        Some(prev) => intersect_exec_policy(prev, p),
                    });
                }
            }
        }
        if any_opinion {
            ExecVerdict::Allowed(policy.unwrap_or(ExecPolicy::Allow))
        } else if saw_exec_policy {
            ExecVerdict::Denied
        } else {
            ExecVerdict::Unrestricted
        }
    }

    /// Whether the active stack denies every candidate name outright.
    /// Used by `classify_command_head` to colour the dispatch site
    /// before any args are parsed.
    fn is_exec_denied_for(&self, names: &[&str]) -> bool {
        matches!(self.evaluate_exec(names), ExecVerdict::Denied)
    }

    /// True when capability checks should emit events into the exec
    /// tree.  Requires an active tree (`audit` or `ral --audit`) AND
    /// `audit: true` on at least one enclosing capabilities layer
    /// (SPEC §11.4-11.5).
    pub fn should_audit_capabilities(&self, audit: &Audit) -> bool {
        audit.tree.is_some() && self.capabilities_stack.iter().any(|ctx| ctx.audit)
    }

    /// Walk the capabilities stack; if any level has a relevant
    /// capability set and its boolean is `false`, return a denial
    /// error.  `test` maps a `Capabilities` to `Some(allowed)` when the
    /// layer has an opinion, or `None` to abstain.
    fn check_grant_bool(
        &self,
        msg: impl Fn() -> String,
        test: impl Fn(&Capabilities) -> Option<bool>,
    ) -> Result<(), EvalSignal> {
        for ctx in &self.capabilities_stack {
            if test(ctx) == Some(false) {
                return Err(EvalSignal::Error(Error::new(msg(), 1)));
            }
        }
        Ok(())
    }

    /// Check that the `editor.read` capability is available.
    pub fn check_editor_read(&self, subcmd: &str) -> Result<(), EvalSignal> {
        self.check_grant_bool(
            || format!("denied: _editor '{subcmd}' requires editor.read"),
            |ctx| ctx.editor.as_ref().map(|ed| ed.read),
        )
    }

    /// Check that the `editor.write` capability is available.
    pub fn check_editor_write(&self, subcmd: &str) -> Result<(), EvalSignal> {
        self.check_grant_bool(
            || format!("denied: _editor '{subcmd}' requires editor.write"),
            |ctx| ctx.editor.as_ref().map(|ed| ed.write),
        )
    }

    /// Check that the `editor.tui` capability is available.
    pub fn check_editor_tui(&self) -> Result<(), EvalSignal> {
        self.check_grant_bool(
            || "denied: _editor 'tui' requires editor.tui".into(),
            |ctx| ctx.editor.as_ref().map(|ed| ed.tui),
        )
    }

    /// Check that the `shell.chdir` capability is available.
    pub fn check_shell_chdir(&self) -> Result<(), EvalSignal> {
        self.check_grant_bool(
            || "denied: cd requires shell.chdir".into(),
            |ctx| ctx.shell.as_ref().map(|sh| sh.chdir),
        )
    }

    /// Resolve a user-facing path against the scoped cwd used by builtins.
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        crate::path::resolve_path(self.cwd.as_deref(), path)
    }

    /// Resolve `path` for a capability check.  Inside the sandboxed
    /// child the OS-level Seatbelt/bwrap profile is the real gate, and
    /// `canonicalize` may fail on intermediate components or fall back
    /// to lexical form on only one side of the comparison; both lead to
    /// spurious denials.  We therefore use pure lexical resolution
    /// there, leaning on `path_within`'s firmlink-alias awareness to
    /// keep `/tmp` ↔ `/private/tmp` correct.  Outside the sandbox we
    /// keep canonicalize-based resolution so grants follow symlinks.
    fn resolve_for_check(&self, path: &str) -> PathBuf {
        if std::env::var_os(crate::sandbox::SANDBOX_ACTIVE_ENV).is_some() {
            self.resolve_path(path)
        } else {
            self.resolve_grant_path(path)
        }
    }

    /// Canonicalise a path under the scoped cwd, walking up to the
    /// nearest existing ancestor and re-appending the unresolved tail.
    /// Needed so grants written against e.g. `/tmp/` still match
    /// non-existent targets on platforms where `/tmp` is a symlink.
    fn resolve_grant_path(&self, path: &str) -> PathBuf {
        let joined = self.resolve_path(path);
        if let Ok(c) = std::fs::canonicalize(&joined) {
            return c;
        }
        let mut trail: Vec<std::ffi::OsString> = Vec::new();
        let mut cursor = joined.as_path();
        loop {
            if let Ok(c) = std::fs::canonicalize(cursor) {
                let mut resolved = c;
                for seg in trail.iter().rev() {
                    resolved.push(seg);
                }
                return resolved;
            }
            match cursor.parent() {
                Some(parent) => {
                    if let Some(name) = cursor.file_name() {
                        trail.push(name.to_os_string());
                    }
                    if parent.as_os_str().is_empty() {
                        return joined;
                    }
                    cursor = parent;
                }
                None => return joined,
            }
        }
    }

    fn path_allowed_by_prefixes(&self, path: &Path, prefixes: &[String]) -> bool {
        prefixes
            .iter()
            .any(|prefix| crate::path::path_within(path, &self.resolve_for_check(prefix)))
    }

    /// Emit a capability-check audit node into `audit`.  No-op unless
    /// `_audit { ... }` is active *and* an enclosing grant requested
    /// auditing.  `fill` attaches per-check `(key, value)` pairs.
    fn emit_capability_audit(
        &self,
        kind: &str,
        ok: bool,
        audit: &mut Audit,
        location: &Location,
        fill: impl FnOnce(&mut Vec<(std::string::String, Value)>),
    ) {
        if !self.should_audit_capabilities(audit) {
            return;
        }
        let decision = if ok { "allowed" } else { "denied" };
        let script = location.call_site.script.clone();
        let line = location.call_site.line;
        let col = location.call_site.col;
        let principal = self.env_vars.get("USER").cloned().unwrap_or_default();
        let mut node = ExecNode::capability_check(kind, decision, &script, line, col);
        node.principal = principal;
        if let Value::Map(ref mut pairs) = node.value {
            fill(pairs);
        }
        if let Some(tree) = &mut audit.tree {
            tree.push(node);
        }
    }

    /// Validate an `exec` capability check against the active stack and
    /// emit an audit node if auditing is on.
    pub fn check_exec_args(
        &self,
        display_name: &str,
        policy_names: &[&str],
        args: &[String],
        audit: &mut Audit,
        location: &Location,
    ) -> Result<(), EvalSignal> {
        let verdict = self.evaluate_exec(policy_names);

        let result: Result<(), EvalSignal> = match verdict {
            ExecVerdict::Unrestricted => Ok(()),
            ExecVerdict::Denied => Err(EvalSignal::Error(
                Error::new(format!("command '{display_name}' denied by active grant"), 1)
                    .with_hint("add the command to the grant exec map (or its directory to exec_dirs) to allow it"),
            )),
            ExecVerdict::Allowed(ExecPolicy::Allow) => Ok(()),
            ExecVerdict::Allowed(ExecPolicy::Subcommands(allowed)) => match args.first() {
                None => Err(EvalSignal::Error(
                    Error::new(
                        format!(
                            "command '{display_name}' requires an allowed subcommand under the active grant"
                        ),
                        1,
                    )
                    .with_hint(format!("allowed subcommands: {}", allowed.join(", "))),
                )),
                Some(first) => {
                    if allowed.iter().any(|candidate| candidate == first) {
                        Ok(())
                    } else {
                        Err(EvalSignal::Error(
                            Error::new(
                                format!(
                                    "command '{display_name}' subcommand '{first}' denied by active grant"
                                ),
                                1,
                            )
                            .with_hint(format!("allowed subcommands: {}", allowed.join(", "))),
                        ))
                    }
                }
            },
        };

        let has_exec_policy = self
            .capabilities_stack
            .iter()
            .any(|ctx| ctx.exec.is_some() || ctx.exec_dirs.is_some());
        if has_exec_policy {
            self.emit_capability_audit("exec", result.is_ok(), audit, location, |pairs| {
                pairs.push(("name".into(), Value::String(display_name.into())));
                if let Some(resolved_name) = policy_names
                    .iter()
                    .find(|candidate| **candidate != display_name)
                {
                    pairs.push(("resolved".into(), Value::String((*resolved_name).into())));
                }
                let args_val: Vec<Value> = args.iter().map(|a| Value::String(a.clone())).collect();
                pairs.push(("args".into(), Value::List(args_val)));
            });
        }

        result
    }

    /// Check whether a filesystem `op` (`"read"` or `"write"`) on
    /// `path` is permitted by the active capabilities stack.
    ///
    /// `consult_deny_paths` is true for write-style ops: any layer's
    /// `deny_paths` overrides every other layer's allow.  Reads ignore
    /// `deny_paths` — those are write-only denials.
    fn check_fs_op(
        &self,
        path: &str,
        op: &str,
        get_prefixes: impl Fn(&FsPolicy) -> &[String],
        consult_deny_paths: bool,
        audit: &mut Audit,
        location: &Location,
    ) -> Result<(), EvalSignal> {
        // /dev/null is a discard device; restricting it has no security value.
        if path == "/dev/null" {
            return Ok(());
        }
        let resolved = self.resolve_for_check(path);
        let mut denied = false;
        let mut granted_prefix: Option<String> = None;
        let mut has_fs_policy = false;

        for ctx in self.capabilities_stack.iter() {
            if let Some(fs) = &ctx.fs {
                has_fs_policy = true;
                if consult_deny_paths {
                    for deny in &fs.deny_paths {
                        let deny_resolved = self.resolve_for_check(deny);
                        if resolved == deny_resolved {
                            denied = true;
                            break;
                        }
                    }
                    if denied {
                        break;
                    }
                }
                let prefixes = get_prefixes(fs);
                if !self.path_allowed_by_prefixes(&resolved, prefixes) {
                    denied = true;
                    break;
                } else {
                    for prefix in prefixes {
                        if crate::path::path_within(&resolved, &self.resolve_for_check(prefix)) {
                            granted_prefix = Some(prefix.clone());
                            break;
                        }
                    }
                }
            }
        }

        if has_fs_policy {
            self.emit_capability_audit("fs", !denied, audit, location, |pairs| {
                pairs.push(("op".into(), Value::String(op.into())));
                pairs.push(("path".into(), Value::String(path.into())));
                if !denied && let Some(gp) = granted_prefix {
                    pairs.push(("granted".into(), Value::String(gp)));
                }
            });
        }

        if denied {
            Err(EvalSignal::Error(Error::new(
                format!("fs {op} denied by grant: {}", resolved.display()),
                1,
            )))
        } else {
            Ok(())
        }
    }

    pub fn check_fs_read(
        &self,
        path: &str,
        audit: &mut Audit,
        location: &Location,
    ) -> Result<(), EvalSignal> {
        self.check_fs_op(path, "read", |fs| &fs.read_prefixes, false, audit, location)
    }

    pub fn check_fs_write(
        &self,
        path: &str,
        audit: &mut Audit,
        location: &Location,
    ) -> Result<(), EvalSignal> {
        self.check_fs_op(path, "write", |fs| &fs.write_prefixes, true, audit, location)
    }

    /// Compute the effective sandbox policy for the current capabilities
    /// stack, intersecting fs prefixes and ANDing net booleans across
    /// layers.  `deny_paths` accumulate as a union: more denies = less
    /// authority, monotone with stack depth.
    pub fn sandbox_policy(&self) -> Option<SandboxPolicy> {
        let mut read_prefixes: Option<Vec<PrefixPair>> = None;
        let mut write_prefixes: Option<Vec<PrefixPair>> = None;
        let mut deny_paths: Vec<String> = Vec::new();
        let mut net_allowed = true;
        let mut saw_fs = false;
        let mut saw_net = false;

        for ctx in &self.capabilities_stack {
            if let Some(fs) = &ctx.fs {
                saw_fs = true;
                let read = canonical_prefix_pairs(self, &fs.read_prefixes);
                let write = canonical_prefix_pairs(self, &fs.write_prefixes);
                read_prefixes = Some(match read_prefixes {
                    Some(current) => intersect_prefix_pairs(&current, &read),
                    None => read,
                });
                write_prefixes = Some(match write_prefixes {
                    Some(current) => intersect_prefix_pairs(&current, &write),
                    None => write,
                });
                for p in &fs.deny_paths {
                    deny_paths.push(p.clone());
                    let resolved = self
                        .resolve_grant_path(p)
                        .to_string_lossy()
                        .into_owned();
                    deny_paths.push(resolved);
                }
            }
            if let Some(net) = ctx.net {
                saw_net = true;
                net_allowed &= net;
            }
        }

        if !saw_fs && (!saw_net || net_allowed) {
            return None;
        }

        let read_prefixes = read_prefixes.unwrap_or_default();
        let write_prefixes = write_prefixes.unwrap_or_default();
        Some(SandboxPolicy {
            fs: FsPolicy {
                read_prefixes: prefix_pair_raws(&read_prefixes),
                write_prefixes: prefix_pair_raws(&write_prefixes),
                deny_paths: unique_strings(deny_paths),
            },
            net: net_allowed,
        })
    }
}

// ── Per-group merge policies ──────────────────────────────────────────────
//
// Each sub-struct owns the rule for how its child state merges back into
// its parent.  `return_to` above is a list of these — no ad-hoc field
// twiddling.

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

impl Audit {
    /// Audit buffers are append-only: parent and child both emit bytes, and
    /// both streams belong in the parent's buffer in their native order.
    /// `tree` is thread-local and propagated by the caller when needed.
    pub fn append_from(&mut self, child: &Audit) {
        self.captured_stdout
            .extend_from_slice(&child.captured_stdout);
        self.captured_stderr
            .extend_from_slice(&child.captured_stderr);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PrefixPair {
    canonical: String,
    raw: String,
}

fn canonical_prefix_pairs(dynamic: &Dynamic, prefixes: &[String]) -> Vec<PrefixPair> {
    let mut out: Vec<PrefixPair> = prefixes
        .iter()
        .map(|prefix| {
            let canonical = dynamic
                .resolve_grant_path(prefix)
                .to_string_lossy()
                .into_owned();
            let raw = dynamic.resolve_path(prefix).to_string_lossy().into_owned();
            PrefixPair { canonical, raw }
        })
        .collect();
    out.sort_by(|a, b| {
        a.canonical
            .cmp(&b.canonical)
            .then_with(|| a.raw.cmp(&b.raw))
    });
    out.dedup();
    out
}

fn prefix_pair_raws(prefixes: &[PrefixPair]) -> Vec<String> {
    unique_strings(prefixes.iter().map(|prefix| prefix.raw.clone()))
}

fn unique_strings(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = values.into_iter().collect();
    out.sort();
    out.dedup();
    out
}

/// Intersect two exec policies.  Subcommand lists are narrowed to their common
/// subset; `Allow` defers to whichever side is more restrictive.
fn intersect_exec_policy(outer: ExecPolicy, inner: ExecPolicy) -> ExecPolicy {
    match (outer, inner) {
        (ExecPolicy::Allow, inner) => inner,
        (outer, ExecPolicy::Allow) => outer,
        (ExecPolicy::Subcommands(a), ExecPolicy::Subcommands(b)) => {
            ExecPolicy::Subcommands(a.into_iter().filter(|s| b.contains(s)).collect())
        }
    }
}

fn intersect_prefix_pairs(left: &[PrefixPair], right: &[PrefixPair]) -> Vec<PrefixPair> {
    let mut out = Vec::new();
    for a in left {
        for b in right {
            let a_path = Path::new(&a.canonical);
            let b_path = Path::new(&b.canonical);
            if a.canonical == b.canonical {
                out.push(a.clone());
                out.push(b.clone());
            } else if crate::path::path_within(a_path, b_path) {
                out.push(a.clone());
            } else if crate::path::path_within(b_path, a_path) {
                out.push(b.clone());
            }
        }
    }
    out.sort_by(|a, b| {
        a.canonical
            .cmp(&b.canonical)
            .then_with(|| a.raw.cmp(&b.raw))
    });
    out.dedup();
    out
}

impl Default for Shell {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

#[cfg(test)]
mod grant_policy_tests {
    use super::{Capabilities, Shell, FsPolicy};

    #[test]
    fn explicit_grant_denies_omitted_exec() {
        let mut shell = Shell::default();
        let head = shell.with_capabilities(Capabilities::deny_all(), |shell| {
            shell.classify_command_head("/bin/echo")
        });
        assert_eq!(head, super::CommandHead::GrantDenied);
    }

    /// `exec_dirs` admits a command whose resolved absolute path is
    /// under one of the listed prefixes, even when the per-name
    /// `exec` map has no entry.
    #[test]
    fn exec_dirs_allows_resolved_path_under_prefix() {
        let mut shell = Shell::default();
        let grant = Capabilities {
            exec: Some(Vec::new()),
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
            exec: Some(Vec::new()),
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
            exec: Some(vec![(
                "cargo".into(),
                super::ExecPolicy::Subcommands(vec!["build".into()]),
            )]),
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
            exec: Some(vec![("git".into(), super::ExecPolicy::Allow)]),
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
            exec: Some(vec![("git".into(), super::ExecPolicy::Allow)]),
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
            exec: Some(vec![("git".into(), super::ExecPolicy::Allow)]),
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
            exec: Some(vec![("/tmp/fake-bin/git".into(), super::ExecPolicy::Allow)]),
            ..Capabilities::root()
        };
        let args = vec!["status".into()];
        shell.with_capabilities(grant, |shell| {
            shell.check_exec_args("git", &["/tmp/fake-bin/git"], &args)
        })
        .expect("resolved-path grant should allow the substituted executable");
    }

    #[test]
    fn sandbox_policy_intersects_path_components() {
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
        let policy = shell.with_capabilities(outer, |shell| {
            shell.with_capabilities(inner, |shell| shell.sandbox_policy().unwrap())
        });
        assert!(policy.check_spec(&shell).read_prefixes.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_policy_does_not_leak_outer_raw_prefix() {
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

        let policy = shell.with_capabilities(outer, |shell| {
            shell.with_capabilities(inner, |shell| shell.sandbox_policy().unwrap())
        });
        let bind_spec = policy.bind_spec();
        let check_spec = policy.check_spec(&shell);
        let canonical_inner = shell
            .dynamic
            .resolve_grant_path(&inner_dir.to_string_lossy())
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

// ── Error ────────────────────────────────────────────────────────────────

/// Classification of runtime errors.  Used by `_try-apply` (SPEC §16.4) to
/// catch only pattern-mismatch failures while letting other errors propagate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorKind {
    #[default]
    Other,
    /// Destructuring a function parameter failed (wrong shape, missing key,
    /// wrong length).  Any other failure is `Other`.
    PatternMismatch,
}

#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    pub status: i32,
    pub loc: Option<crate::diagnostic::SourceLoc>,
    pub hint: Option<String>,
    pub kind: ErrorKind,
}

impl Error {
    pub fn new(message: impl Into<String>, status: i32) -> Self {
        Error {
            message: message.into(),
            status,
            loc: None,
            hint: None,
            kind: ErrorKind::Other,
        }
    }

    pub fn with_kind(mut self, kind: ErrorKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn at(mut self, line: usize, col: usize) -> Self {
        self.loc = Some(crate::diagnostic::SourceLoc {
            file: String::new(),
            line,
            col,
            len: 0,
        });
        self
    }

    pub fn at_loc(mut self, loc: crate::diagnostic::SourceLoc) -> Self {
        self.loc = Some(loc);
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

// ── EvalSignal (non-local control flow) ──────────────────────────────────

#[derive(Debug, Clone)]
pub enum EvalSignal {
    /// Runtime error or fail.
    Error(Error),
    /// exit N — clean process exit with a status code.
    Exit(i32),
    /// Tail call — propagates up to the nearest trampoline.
    /// Carries the full callee and all args so curried recursion works.
    TailCall { callee: Value, args: Vec<Value> },
}

impl From<Error> for EvalSignal {
    fn from(e: Error) -> Self {
        EvalSignal::Error(e)
    }
}

impl fmt::Display for EvalSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvalSignal::Error(e) => write!(f, "{e}"),
            EvalSignal::Exit(code) => write!(f, "exit {code}"),
            EvalSignal::TailCall { .. } => write!(f, "<tail call>"),
        }
    }
}
