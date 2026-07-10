//! Audit collector and execution tree.
//!
//! This module owns every shape used by audit: the per-shell collector
//! [`Audit`], the storage [`AuditTrail`], the transport [`AuditFragment`],
//! the per-node value parts ([`AuditIo`], [`AuditTime`]),
//! and the node itself [`ExecNode`].  Construction of normal `command`
//! and `capability-check` nodes lives here too, so the rest of the tree
//! never reaches for raw `ExecNode { … }` syntax.
//!
//! Audit is lexical.  A scope-introducing operator (`grant`, `within`,
//! `guard`, `try`, `audit`) owns every node produced by its body —
//! including sandboxed subprocess nodes and pipeline stage nodes.
//! Process boundaries only transport audit fragments; the wrapping
//! scope is what decides where they land in the tree.

use super::value::Value;
use crate::diagnostic::CallSite;
use serde::{Deserialize, Serialize};

/// Cap applied to per-node `stderr` when bytes are being captured
/// (`CapturePolicy::Bytes`).  See SPEC §10.3.
pub const STDERR_CAP_BYTES: usize = 64 * 1024;

/// Raw bytes carried by one audit node.  `stdout` and `stderr` are the
/// per-command captures produced under `CapturePolicy::Bytes`, or the
/// empty vector when bytes are not being captured.
#[derive(Clone, Debug, Default)]
pub struct AuditIo {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Wall-clock window for one audit node.  Microseconds since the Unix
/// epoch.
#[derive(Clone, Copy, Debug, Default)]
pub struct AuditTime {
    pub start: i64,
    pub end: i64,
}

/// Byte-capture policy.
///
/// `Off` is the default — fd 1 and fd 2 stream
/// live with no buffering, the normal §4.3 path.  `Bytes` installs the
/// dispatcher-level tee that captures each command's stdout / stderr
/// into its audit node (§10.3).  Set by `audit`; inherited by nested
/// scopes through [`Audit::set_capture`].
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapturePolicy {
    #[default]
    Off,
    Bytes,
}

/// Backing storage for the in-flight audit tree.  Private to this
/// module; everything outside reads/writes through [`Audit`] / through
/// [`AuditFragment`] on a process boundary.
#[derive(Default, Debug)]
pub struct AuditTrail {
    nodes: Vec<ExecNode>,
}

impl AuditTrail {
    /// Drain the trail's accumulated nodes.  Used by sandbox helpers
    /// and pipeline helpers to ship audit back across a process
    /// boundary as an [`AuditFragment`].
    pub fn into_nodes(self) -> Vec<ExecNode> {
        self.nodes
    }
}

/// Audit nodes detached from a trail.
///
/// Process boundaries (sandbox
/// child, pipeline helper, internal builtins) produce a fragment;
/// the receiving side merges it into the surrounding scope.  Distinct
/// from [`AuditTrail`] only in ownership: the trail belongs to a live
/// `Audit`, the fragment is in transit.
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

/// Audit collector — one per `Shell`.
///
/// Two orthogonal pieces of state: a `trail` that is `Some` when audit
/// is active (a scope is collecting nodes), and a `capture` policy that
/// the dispatcher-level Tee consults when wrapping per-command bytes.
/// Callers must not reach inside; the methods on this type are the
/// API.  The internal layout is private so adding new fields (or
/// rewriting the trail representation) does not ripple through call
/// sites.
#[derive(Default, Debug)]
pub struct Audit {
    trail: Option<AuditTrail>,
    capture: CapturePolicy,
}

impl Audit {
    /// True when an audit scope is collecting.
    pub fn active(&self) -> bool {
        self.trail.is_some()
    }

    /// True when per-command bytes should be captured by the
    /// dispatcher-level Tee.
    pub fn captures_bytes(&self) -> bool {
        matches!(self.capture, CapturePolicy::Bytes)
    }

    /// Top-level activation — used by `ral --audit` and by the
    /// re-execed pipeline / sandbox children that inherit "audit on"
    /// from their parent.  No-op when already active.
    pub fn enable(&mut self) {
        if self.trail.is_none() {
            self.trail = Some(AuditTrail::default());
        }
    }

    /// Set the current byte-capture policy.  Use the scoped switch in
    /// [`crate::evaluator::audit`] instead of poking this directly.
    pub fn set_capture(&mut self, policy: CapturePolicy) {
        self.capture = policy;
    }

    /// Read the current byte-capture policy.
    pub fn capture_policy(&self) -> CapturePolicy {
        self.capture
    }

    /// Capture policy iff a scope is currently collecting.  The
    /// natural shape for a process boundary: the sandbox / pipeline
    /// child needs to know both whether to enable its trail *and*
    /// which byte-capture policy to install, and the two questions
    /// always travel together.  Returns `None` when audit is
    /// inactive (no trail to inherit).
    ///
    /// The re-exec'd-child IPC seam (`ChildEvalRequest`, the only frame
    /// that crosses to a helper now that a bundled tool rides as an
    /// ordinary external stage) carries the return value of this in a
    /// dedicated `audit_policy` field rather than embedding it in the mobile
    /// snapshot: audit policy is an **instruction** to the helper
    /// process, not a snapshot property — `WireMobile` carries
    /// mobile-only state with no local audit on it.  And the policy
    /// is `Option<CapturePolicy>` rather than a `bool` so a stage inside
    /// `try { … }` rides as `None` (live streaming) and one inside
    /// `audit { … }` rides as `Some(Bytes)` (recorded).
    pub fn active_policy(&self) -> Option<CapturePolicy> {
        self.active().then_some(self.capture)
    }

    /// Inverse of [`Self::active_policy`]: install an inherited
    /// policy on the receiver, enabling the trail when `Some` and
    /// leaving the audit inactive when `None`.  Used by the sandbox
    /// child and pipeline-helper subprocesses to mirror the parent's
    /// audit state — the two-step (enable + `set_capture`) sequence is
    /// awkward to keep in sync at every call site.
    pub fn install_active_policy(&mut self, policy: Option<CapturePolicy>) {
        if let Some(policy) = policy {
            self.enable();
            self.set_capture(policy);
        }
    }

    /// Append one node to the active trail.  No-op when audit is
    /// inactive — the dispatcher path can call this unconditionally
    /// after building a node it has decided to record.
    pub fn push(&mut self, node: ExecNode) {
        if let Some(trail) = self.trail.as_mut() {
            trail.nodes.push(node);
        }
    }

    /// Merge a fragment into the active trail.  No-op when audit is
    /// inactive (the fragment is dropped).
    pub fn merge(&mut self, fragment: AuditFragment) {
        if let Some(trail) = self.trail.as_mut() {
            trail.nodes.extend(fragment.into_nodes());
        }
    }

    /// Enter a lexical audit scope: move the parent trail aside and
    /// install a fresh child trail.  Pairs with [`Self::leave_child`];
    /// only installs when audit was already active, so an inactive
    /// surrounding context leaves the body unaudited.
    pub fn enter_child(&mut self) -> Option<AuditTrail> {
        if self.trail.is_some() {
            self.trail.replace(AuditTrail::default())
        } else {
            None
        }
    }

    /// Forced variant: install a fresh child trail regardless of
    /// parent state.  Used by `try` and `audit`, which collect
    /// children even when no surrounding `audit { … }` is active so
    /// they can name the failing command / return the full subtree.
    pub fn enter_forced_child(&mut self) -> Option<AuditTrail> {
        self.trail.replace(AuditTrail::default())
    }

    /// Leave the lexical audit scope: take the child trail, restore the
    /// saved parent, and return the child's nodes as a fragment.
    pub fn leave_child(&mut self, parent: Option<AuditTrail>) -> AuditFragment {
        let child = self.trail.take().unwrap_or_default();
        self.trail = parent;
        AuditFragment::from_nodes(child.nodes)
    }

    /// Drain the current trail's accumulated nodes as a fragment,
    /// leaving the trail empty.  Used at process boundaries (sandbox
    /// child, pipeline helper) to ship the audit accumulated during
    /// child evaluation back to the parent.  Returns the empty
    /// fragment when audit is inactive.
    pub fn take_fragment(&mut self) -> AuditFragment {
        match self.trail.as_mut() {
            Some(trail) => AuditFragment::from_nodes(std::mem::take(&mut trail.nodes)),
            None => AuditFragment::empty(),
        }
    }

    /// STT-in for a same-thread thunk body — see
    /// [`crate::types::Shell::inherit_from`].  Moves both the trail
    /// and the capture policy into the child.
    pub fn inherit_from(&mut self, parent: &mut Self) {
        self.trail = parent.trail.take();
        self.capture = parent.capture;
    }

    /// STT-out for a same-thread thunk body.  Returns the (possibly
    /// extended) trail and policy back to the parent.
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
    pub children: Vec<Self>,
    pub start: i64,        // wall-clock start: microseconds since epoch
    pub end: i64,          // wall-clock end: microseconds since epoch
    pub principal: String, // $USER at time of recording
}

impl ExecNode {
    /// Build a command node.  Every audit-tree command node — every
    /// builtin, every external command, every scope (`grant`, `try`,
    /// `audit`, …) — and the batch-mode root in `ral::batch` flow through
    /// this constructor, so the only other place that synthesises
    /// `ExecNode` by struct literal is `subprocess::WireExecNode::into_runtime`,
    /// rehydrating a wire-transported node.
    #[allow(clippy::too_many_arguments)]
    pub fn command(
        cmd: impl Into<String>,
        args: Vec<String>,
        status: i32,
        site: CallSite,
        io: AuditIo,
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
            value,
            children,
            start: time.start,
            end: time.end,
            principal,
        }
    }

    /// Build a capability-check event node.  `fields` is spliced into
    /// the same map that already carries `resource` and `decision`,
    /// per SPEC §10.3.  Capability nodes have no captured I/O and no
    /// children: a single check is leaf-shaped.
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
            value: Value::map(value_pairs),
            children: Vec::new(),
            start: now,
            end: now,
            principal,
        }
    }

    /// Convert to a `Value::Map` matching the execution tree node shape.
    /// For `capability-check` nodes the fields stored in `self.value` are
    /// also spliced into the top-level map so that `resource`, `decision`,
    /// and the resource-specific fields appear alongside `cmd`/`status`.
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
