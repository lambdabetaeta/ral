//! Audit collector and execution tree.
//!
//! Audit is lexical: a scope-introducing operator (`grant`, `within`, `guard`,
//! `try`, `audit`) owns every node its body produces, sandboxed subprocess and
//! pipeline-stage nodes included.  A process boundary only transports
//! fragments; the wrapping scope decides where they land in the tree.

use super::error::Error;
use super::flow::{Break, Settled};
use super::value::Value;
use crate::diagnostic::CallSite;
use crate::source::Span;
use serde::{Deserialize, Serialize};

/// Cap on one node's recorded `stderr`; `evaluator::audit` truncates to it.
pub const STDERR_CAP_BYTES: usize = 64 * 1024;

/// Bytes captured for one node under `CapturePolicy::Bytes`, empty otherwise.
#[derive(Clone, Debug, Default)]
pub struct AuditIo {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// A node's wall-clock window, in microseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, Default)]
pub struct AuditTime {
    pub start: i64,
    pub end: i64,
}

/// Whether per-command bytes are teed into audit nodes.  `Off` lets fd 1 and
/// fd 2 stream live, unbuffered; `Bytes` installs the tee that
/// `evaluator::capture` wraps each command in.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapturePolicy {
    #[default]
    Off,
    Bytes,
}

/// Backing storage for the in-flight tree; reachable only through [`Audit`].
#[derive(Default, Debug)]
pub struct AuditTrail {
    nodes: Vec<ExecNode>,
}

/// Nodes detached from a trail — a sandbox child, a pipeline helper, or a
/// closed lexical scope hands one up, and the receiving side merges it into
/// the surrounding scope.
///
/// Same shape as [`AuditTrail`], but in transit.
#[derive(Default, Debug, Clone)]
pub struct AuditFragment {
    nodes: Vec<ExecNode>,
}

impl AuditFragment {
    pub fn empty() -> Self {
        Self::default()
    }
    pub fn from_nodes(nodes: Vec<ExecNode>) -> Self {
        Self { nodes }
    }
    pub fn into_nodes(self) -> Vec<ExecNode> {
        self.nodes
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// Audit collector — one per `Shell`, collecting exactly while `trail` is
/// `Some`.
#[derive(Default, Debug)]
pub struct Audit {
    trail: Option<AuditTrail>,
    capture: CapturePolicy,
    /// Where the command now running was dispatched from — the register every
    /// node and capability check resolves its site against, `None` before a
    /// run's first dispatch.  `run_call` in `runtime::command_call` skips the
    /// write for `_`-prefixed names, so a prelude wrapper's nodes name the
    /// user's call rather than the wrapper's; `IoLoan` in `crate::run` clears
    /// the register per run and restores it on drop.
    pub(crate) call_site: Option<Span>,
}

impl Audit {
    /// True when a scope is collecting.
    pub fn active(&self) -> bool {
        self.trail.is_some()
    }

    /// True when the tee should record each command's bytes.
    pub fn captures_bytes(&self) -> bool {
        matches!(self.capture, CapturePolicy::Bytes)
    }

    /// Overwrite the capture policy.  A scope wants `with_capture_policy` in
    /// [`crate::evaluator::audit`], whose merge is monotonic: an inner `try`
    /// must not silence an outer `audit`.
    pub fn set_capture(&mut self, policy: CapturePolicy) {
        self.capture = policy;
    }

    /// The current capture policy.
    pub fn capture_policy(&self) -> CapturePolicy {
        self.capture
    }

    /// The policy to inherit across a process boundary, `Some` iff a scope is
    /// collecting — a helper learns in one answer whether to open a trail and
    /// which policy to install.  Rides in `ChildEvalRequest`'s own
    /// `audit_policy` field: it instructs the child, it is not snapshot state.
    pub fn active_policy(&self) -> Option<CapturePolicy> {
        self.active().then_some(self.capture)
    }

    /// Inverse of [`Self::active_policy`]: open a trail and set the policy on
    /// `Some`, stay inactive on `None`.  An already-open trail keeps its nodes.
    pub fn install_active_policy(&mut self, policy: Option<CapturePolicy>) {
        if let Some(policy) = policy {
            self.trail.get_or_insert_default();
            self.capture = policy;
        }
    }

    /// Append a node; no-op when inactive, so the dispatcher need not ask.
    pub fn push(&mut self, node: ExecNode) {
        if let Some(trail) = self.trail.as_mut() {
            trail.nodes.push(node);
        }
    }

    /// Merge a fragment into the trail; when inactive the fragment is dropped.
    pub fn merge(&mut self, fragment: AuditFragment) {
        if let Some(trail) = self.trail.as_mut() {
            trail.nodes.extend(fragment.into_nodes());
        }
    }

    /// Move the parent trail aside for a fresh child.  Pairs with
    /// [`Self::leave_child`]; does nothing when audit is off, so an unaudited
    /// context leaves the body unaudited too.
    pub fn enter_child(&mut self) -> Option<AuditTrail> {
        if self.trail.is_some() {
            self.trail.replace(AuditTrail::default())
        } else {
            None
        }
    }

    /// Install a child trail whatever the parent's state — `try` needs the
    /// subtree to name the failing command, `audit` to return it.
    pub fn enter_forced_child(&mut self) -> Option<AuditTrail> {
        self.trail.replace(AuditTrail::default())
    }

    /// Restore `parent` and hand back what the child collected.
    pub fn leave_child(&mut self, parent: Option<AuditTrail>) -> AuditFragment {
        let child = self.trail.take().unwrap_or_default();
        self.trail = parent;
        AuditFragment::from_nodes(child.nodes)
    }

    /// Drain the trail, leaving it open but empty — how a sandbox or pipeline
    /// child ships its audit home.  Empty fragment when inactive.
    pub fn take_fragment(&mut self) -> AuditFragment {
        match self.trail.as_mut() {
            Some(trail) => AuditFragment::from_nodes(std::mem::take(&mut trail.nodes)),
            None => AuditFragment::empty(),
        }
    }

    /// STT-in for a same-thread thunk body.  The trail and policy move in and
    /// the call site is copied in, but it never flows back on
    /// [`Self::return_to`]: the asymmetry keeps a body's own dispatches from
    /// leaking their site into the caller's next one.
    pub fn inherit_from(&mut self, parent: &mut Self) {
        self.trail = parent.trail.take();
        self.capture = parent.capture;
        self.call_site = parent.call_site;
    }

    /// STT-out: the trail the body extended, and the policy, go back to the
    /// parent.  The call site does not.
    pub fn return_to(&mut self, parent: &mut Self) {
        parent.trail = self.trail.take();
        parent.capture = self.capture;
    }
}

/// Microseconds since the Unix epoch.
pub fn epoch_us() -> i64 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "microseconds-since-epoch stays below i64::MAX until year 294276"
    )]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64
    }
}

/// The two kinds of execution-tree node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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

/// A node in the execution tree; both kinds share this one shape.
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
    /// The runtime's own account of why the node failed — `Some` iff the
    /// outcome was a runtime error.  `stdout` and `stderr` hold only bytes
    /// observed on the child's file descriptors; this field is the one place
    /// ral speaks in its own voice.
    pub error: Option<String>,
    pub value: Value,
    pub children: Vec<Self>,
    pub start: i64,        // microseconds since the Unix epoch
    pub end: i64,          // microseconds since the Unix epoch
    pub principal: String, // $USER at the time of recording
}

/// What a node records of its body's outcome: the exit status, the value,
/// and the runtime's error message when the body failed.
pub(crate) struct NodeOutcome {
    pub status: i32,
    pub value: Value,
    pub error: Option<String>,
}

impl NodeOutcome {
    /// A body that produced a value: the caller supplies the status it
    /// observed (usually `last_status`), and there is no error.
    pub(crate) fn of_value(status: i32, value: Value) -> Self {
        Self {
            status,
            value,
            error: None,
        }
    }

    /// A body that failed: the error's own exit code, no value, and the
    /// runtime's message.  [`Self::with_partial`] refines the no-value rule
    /// for a combinator that salvages its accumulation.
    pub(crate) fn of_error(e: &Error) -> Self {
        Self {
            status: e.exit_code(),
            value: Value::Unit,
            error: Some(e.message.clone()),
        }
    }

    /// Replace the recorded value — a failed combinator records what it had
    /// accumulated where a scope records `Unit`.
    pub(crate) fn with_partial(self, value: Value) -> Self {
        Self { value, ..self }
    }
}

impl ExecNode {
    /// Wrap a run's collected fragment as the tree root.  The root is
    /// scope-shaped — no args, no I/O — and sits at a sentinel site
    /// (script = run name, line 0, col 0), since a run has no dispatch
    /// site of its own.  `exit_code` is the process's exit status as the
    /// host resolved it, not the error's own code; an escape (`exit`)
    /// is not a failure, so it records no error.
    pub fn run_root(
        name: &str,
        result: &Settled<Value>,
        exit_code: i32,
        fragment: AuditFragment,
        start: i64,
        principal: String,
    ) -> Self {
        let outcome = match result {
            Ok(v) => NodeOutcome::of_value(exit_code, v.clone()),
            Err(Break::Error(e)) => NodeOutcome {
                status: exit_code,
                ..NodeOutcome::of_error(e)
            },
            Err(Break::Escape(_)) => NodeOutcome::of_value(exit_code, Value::Unit),
        };
        Self::command(
            name,
            Vec::new(),
            outcome.status,
            CallSite {
                script: name.to_string(),
                line: 0,
                col: 0,
            },
            AuditIo::default(),
            outcome.error,
            outcome.value,
            fragment.into_nodes(),
            AuditTime {
                start,
                end: epoch_us(),
            },
            principal,
        )
    }

    /// Build a command node.  Every node in the tree — builtin, external,
    /// scope, or the run's own root from [`Self::run_root`] — comes through
    /// here; the one struct literal elsewhere is `WireExecNode::into_runtime`
    /// in `crate::child_eval`, rehydrating a node off the wire.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn command(
        cmd: impl Into<String>,
        args: Vec<String>,
        status: i32,
        site: CallSite,
        io: AuditIo,
        error: Option<String>,
        value: Value,
        children: Vec<Self>,
        time: AuditTime,
        principal: String,
    ) -> Self {
        Self {
            kind: ExecNodeKind::Command,
            cmd: cmd.into(),
            args,
            status,
            script: site.script,
            line: site.line,
            col: site.col,
            stdout: io.stdout,
            stderr: io.stderr,
            error,
            value,
            children,
            start: time.start,
            end: time.end,
            principal,
        }
    }

    /// Build a capability-check node.  `fields` splices into the same map as
    /// `resource` and `decision`; a check is leaf-shaped, so no I/O, no
    /// children, and start and end are the same instant.
    pub fn capability_check(
        resource: &str,
        decision: &str,
        site: CallSite,
        principal: String,
        fields: super::map::Map,
    ) -> Self {
        let now = epoch_us();
        let mut value_pairs = vec![
            ("resource".into(), Value::String(resource.into())),
            ("decision".into(), Value::String(decision.into())),
        ];
        for (k, v) in fields {
            value_pairs.push((k, v));
        }
        Self {
            kind: ExecNodeKind::CapabilityCheck,
            cmd: resource.into(),
            args: Vec::new(),
            status: i32::from(decision == "denied"),
            script: site.script,
            line: site.line,
            col: site.col,
            stdout: Vec::new(),
            stderr: Vec::new(),
            error: None,
            value: Value::map(value_pairs),
            children: Vec::new(),
            start: now,
            end: now,
            principal,
        }
    }

    /// Render the node as a map.  A capability check splices `self.value`'s
    /// fields into the top level as well, so `resource` and `decision` sit
    /// beside `cmd` and `status`.  `error` renders as a string, empty when
    /// the node did not fail — a record field is always present, and a
    /// runtime error message is never empty.
    pub fn to_value(&self) -> Value {
        let args_list: Vec<Value> = self.args.iter().map(|a| Value::String(a.clone())).collect();
        let children_list: Vec<Value> = self.children.iter().map(Self::to_value).collect();
        #[allow(
            clippy::cast_possible_wrap,
            reason = "line/col are source positions bounded by source size, far below i64::MAX"
        )]
        let mut pairs = vec![
            ("kind".into(), Value::String(self.kind.to_string())),
            ("cmd".into(), Value::String(self.cmd.clone())),
            ("args".into(), Value::list(args_list)),
            ("status".into(), Value::Int(i64::from(self.status))),
            ("script".into(), Value::String(self.script.clone())),
            ("line".into(), Value::Int(self.line as i64)),
            ("col".into(), Value::Int(self.col as i64)),
            ("stdout".into(), Value::Bytes(self.stdout.clone())),
            ("stderr".into(), Value::Bytes(self.stderr.clone())),
            (
                "error".into(),
                Value::String(self.error.clone().unwrap_or_default()),
            ),
            ("value".into(), self.value.clone()),
            ("children".into(), Value::list(children_list)),
            ("start".into(), Value::Int(self.start)),
            ("end".into(), Value::Int(self.end)),
            ("principal".into(), Value::String(self.principal.clone())),
        ];
        if self.kind == ExecNodeKind::CapabilityCheck
            && let Value::Map(extra) = &self.value
        {
            pairs.extend(extra.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        Value::map(pairs)
    }
}
