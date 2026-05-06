//! Audit collector, execution tree, and source positions.
//!
//! [`Audit`] accumulates exec-tree nodes and raw stdout/stderr during an
//! `audit { ... }` scope.  [`ExecNode`] is one node in the tree.  [`Location`]
//! and [`CallSite`] carry source-position information for diagnostics.

use serde::{Deserialize, Serialize};
use super::value::Value;

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

/// Audit collector.  `tree` is `Some` when `_audit { ... }` has installed a
/// node list; `captured_stdout`/`_stderr` buffer the most recent external
/// command's output so `record_exec` can attach it to the tree node.
#[derive(Default, Debug)]
pub struct Audit {
    pub tree: Option<Vec<ExecNode>>,
    pub captured_stdout: Vec<u8>,
    pub captured_stderr: Vec<u8>,
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
