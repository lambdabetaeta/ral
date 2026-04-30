//! Wire-format types for the sandboxed grant IPC protocol.
//!
//! All types here are `Serialize + Deserialize` and represent exactly
//! what crosses the socket.  None carry runtime `Value`s directly —
//! those go through `SerialValue` in `crate::serial`.
//!
//! Conversion impls (`from_runtime`, `into_runtime`, `install_into`,
//! `from_dynamic`) live here alongside the types so the wire ↔ runtime
//! mapping stays collocated and symmetric.

use crate::serial::{InternCtx, SerialEnvSnapshot, SerialValue};
use crate::types::{
    AliasEntry, Env, EvalSignal, ExecNode, ExecNodeKind, Shell, Value,
};
use crate::ir::Comp;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Wire types ────────────────────────────────────────────────────────────

/// Plugin-registered aliases — wire form of `types::Registry`.
///
/// Loaded plugins themselves are not carried: plugin bodies capture
/// runtime thunks and the subprocess reconstructs aliases from the wire.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpcRegistry {
    pub aliases: Vec<(String, SerialValue, Option<String>)>,
}

/// Module-loader state — wire form of `types::Modules`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpcModules {
    pub cache: Vec<(String, SerialValue)>,
    pub stack: Vec<String>,
    pub depth: usize,
}

/// Wire mirror of `ExecNode`, using `SerialValue` for the `value` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcExecNode {
    pub kind: String,
    pub cmd: String,
    pub args: Vec<String>,
    pub status: i32,
    pub script: String,
    pub line: usize,
    pub col: usize,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub value: SerialValue,
    pub children: Vec<IpcExecNode>,
    pub start: i64,
    pub end: i64,
    pub principal: String,
}

/// Wire mirror of the ambient cluster of `Dynamic`: env_vars, cwd,
/// capabilities_stack.  Excludes `handler_stack` (Value thunks not
/// transmissible) and `script_args` (separate wire field).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpcAmbient {
    pub env_vars: std::collections::HashMap<String, String>,
    pub cwd: Option<std::path::PathBuf>,
    pub capabilities_stack: Vec<crate::types::Capabilities>,
}

/// Request sent from the parent ral process to the sandbox child.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxedBlockRequest {
    /// Interned scope table shared across every `SerialValue` /
    /// `SerialEnvSnapshot` in this request.
    pub scope_table: Vec<Vec<(String, SerialValue)>>,
    pub body: Comp,
    pub captured: SerialEnvSnapshot,
    /// Ambient cluster: env_vars, cwd, capabilities_stack.
    pub ambient: IpcAmbient,
    pub registry: IpcRegistry,
    pub modules: IpcModules,
    pub loc: crate::types::Location,
    pub script_args: Vec<String>,
    pub pipe_value: Option<SerialValue>,
    pub audit: bool,
}

/// Response sent from the sandbox child back to the parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SandboxedBlockResponse {
    Ok {
        scope_table: Vec<Vec<(String, SerialValue)>>,
        value: SerialValue,
        last_status: i32,
    },
    Exit {
        code: i32,
    },
    Error {
        message: String,
        status: i32,
        hint: Option<String>,
    },
}

/// One message from the child to the parent.
///
/// Audit frames stream eagerly; `Final` terminates the session and
/// carries the body's outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChildFrame {
    Audit {
        scope_table: Vec<Vec<(String, SerialValue)>>,
        node: Box<IpcExecNode>,
    },
    Final(SandboxedBlockResponse),
}

// ── Conversions ──────────────────────────────────────────────────────────

impl IpcAmbient {
    pub fn from_dynamic(d: &crate::types::Dynamic) -> Self {
        Self {
            env_vars: d.env_vars.clone(),
            cwd: d.cwd.clone(),
            capabilities_stack: d.capabilities_stack.clone(),
        }
    }
}

impl IpcExecNode {
    pub fn from_runtime(node: ExecNode, ctx: &mut InternCtx) -> Result<Self, EvalSignal> {
        let mut children = Vec::with_capacity(node.children.len());
        for child in node.children {
            children.push(IpcExecNode::from_runtime(child, ctx)?);
        }
        Ok(Self {
            kind: node.kind.to_string(),
            cmd: node.cmd,
            args: node.args,
            status: node.status,
            script: node.script,
            line: node.line,
            col: node.col,
            stdout: node.stdout,
            stderr: node.stderr,
            value: SerialValue::from_runtime(&node.value, ctx)?,
            children,
            start: node.start,
            end: node.end,
            principal: node.principal,
        })
    }

    pub fn into_runtime(
        self,
        arcs: &[Option<std::sync::Arc<HashMap<String, Value>>>],
    ) -> Result<ExecNode, EvalSignal> {
        let mut children = Vec::with_capacity(self.children.len());
        for child in self.children {
            children.push(child.into_runtime(arcs)?);
        }
        Ok(ExecNode {
            kind: if self.kind == "capability-check" {
                ExecNodeKind::CapabilityCheck
            } else {
                ExecNodeKind::Command
            },
            cmd: self.cmd,
            args: self.args,
            status: self.status,
            script: self.script,
            line: self.line,
            col: self.col,
            stdout: self.stdout,
            stderr: self.stderr,
            value: self.value.into_runtime(arcs)?,
            children,
            start: self.start,
            end: self.end,
            principal: self.principal,
        })
    }
}

impl IpcRegistry {
    pub fn from_runtime(shell: &Shell, ctx: &mut InternCtx) -> Result<Self, EvalSignal> {
        let mut aliases = Vec::with_capacity(shell.registry.aliases.len());
        for (name, entry) in shell.registry.aliases.iter() {
            aliases.push((
                name.clone(),
                SerialValue::from_runtime(&entry.value, ctx)?,
                entry.source.clone(),
            ));
        }
        Ok(Self { aliases })
    }

    pub fn install_into(
        self,
        shell: &mut Shell,
        arcs: &[Option<std::sync::Arc<HashMap<String, Value>>>],
    ) -> Result<(), EvalSignal> {
        for (name, value, source) in self.aliases {
            let value = value.into_runtime(arcs)?;
            let entry = match source {
                Some(src) => AliasEntry::with_source(value, src),
                None => AliasEntry::new(value),
            };
            shell.registry.aliases.insert(name, entry);
        }
        Ok(())
    }
}

impl IpcModules {
    pub fn from_runtime(shell: &Shell, ctx: &mut InternCtx) -> Result<Self, EvalSignal> {
        let mut cache = Vec::with_capacity(shell.modules.cache.len());
        for (name, value) in shell.modules.cache.iter() {
            cache.push((name.clone(), SerialValue::from_runtime(value, ctx)?));
        }
        Ok(Self {
            cache,
            stack: shell.modules.stack.clone(),
            depth: shell.modules.depth,
        })
    }

    pub fn install_into(
        self,
        shell: &mut Shell,
        arcs: &[Option<std::sync::Arc<HashMap<String, Value>>>],
    ) -> Result<(), EvalSignal> {
        for (name, value) in self.cache {
            shell.modules.cache.insert(name, value.into_runtime(arcs)?);
        }
        shell.modules.stack = self.stack;
        shell.modules.depth = self.depth;
        Ok(())
    }
}

// ── Request builder ──────────────────────────────────────────────────────

/// Reify `shell` and the grant body into a wire-ready request.
///
/// Inverse of `unpack` in `runner.rs`.
pub fn pack(
    body: Comp,
    captured: &Env,
    shell: &Shell,
) -> Result<SandboxedBlockRequest, EvalSignal> {
    let mut ctx = InternCtx::new();
    let captured = SerialEnvSnapshot::from_runtime(captured, &mut ctx)?;
    let registry = IpcRegistry::from_runtime(shell, &mut ctx)?;
    let modules = IpcModules::from_runtime(shell, &mut ctx)?;
    let pipe_value = shell
        .io
        .value_in
        .as_ref()
        .map(|v| SerialValue::from_runtime(v, &mut ctx))
        .transpose()?;
    Ok(SandboxedBlockRequest {
        scope_table: ctx.scope_table,
        body,
        captured,
        ambient: IpcAmbient::from_dynamic(&shell.dynamic),
        registry,
        modules,
        loc: shell.location.clone(),
        script_args: shell.dynamic.script_args.clone(),
        pipe_value,
        audit: shell.audit.tree.is_some(),
    })
}
