//! Audit collector and execution tree.
//!
//! Only two kinds of node are ever built: a real command (builtin or
//! external) and a capability check.  `within`, `grant`, `guard`, `try`, and
//! `audit` are collection boundaries, not tree nodes — none of them owns or
//! wraps a node of its own; their bodies' real commands land flat in
//! whichever trail is open.  A sandboxed subprocess or a pipeline-stage
//! helper only *transports* its fragment back to the parent process; nothing
//! decides where the nodes "belong" beyond that flat merge.

use super::error::Error;
use super::value::Value;
use crate::diagnostic::CallSite;
use crate::source::Span;
use serde::{Deserialize, Serialize};

/// Cap on one node's recorded `stderr`; `evaluator::audit` truncates to it.
pub const STDERR_CAP_BYTES: usize = 64 * 1024;

/// Bytes captured for one node under `CapturePolicy::Bytes`, empty otherwise.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
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
struct AuditTrail {
    nodes: Vec<ExecNode>,
}

/// Nodes detached from a trail — a sandboxed child or a pipeline helper
/// hands one up across a process boundary, and the receiving side merges it
/// into the surrounding trail.
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

    /// Open a trail if none is open yet, returning its current length as a mark.
    pub fn force_open(&mut self) -> usize {
        self.trail.get_or_insert_default().nodes.len()
    }

    /// The nodes pushed since `mark`.
    pub fn since(&self, mark: usize) -> Vec<ExecNode> {
        self.trail
            .as_ref()
            .map_or_else(Vec::new, |t| t.nodes[mark..].to_vec())
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
    /// runtime's message.
    pub(crate) fn of_error(e: &Error) -> Self {
        Self {
            status: e.exit_code(),
            value: Value::Unit,
            error: Some(e.message.clone()),
        }
    }
}

impl ExecNode {
    /// Build a command node.  Every node in the tree — builtin or external —
    /// comes through here; the one struct literal elsewhere is
    /// `WireExecNode::into_runtime` in `crate::child_eval`, rehydrating a
    /// node off the wire.
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

/// The record `audit { … }` returns: the body's own outcome plus the flat
/// list of real nodes its dynamic extent produced.
///
/// This is not a node — `audit` runs no command and owns no site of its own,
/// only the outcome of what it forced into being recorded.  Mirrored in the
/// typechecker by `audit_record` in `core/src/typecheck/builtins.rs`.
pub fn tree_value(
    status: i32,
    value: Value,
    error: Option<String>,
    children: &[ExecNode],
) -> Value {
    let children_list: Vec<Value> = children.iter().map(ExecNode::to_value).collect();
    Value::map(vec![
        ("status".into(), Value::Int(i64::from(status))),
        ("value".into(), value),
        ("error".into(), Value::String(error.unwrap_or_default())),
        ("children".into(), Value::list(children_list)),
    ])
}
