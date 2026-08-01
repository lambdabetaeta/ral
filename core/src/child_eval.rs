//! One wire shape and one child runner for the re-exec'd pipeline stage.
//!
//! A process-staged stage (`runtime/pipeline/`) packs a [`Comp`] body plus a
//! [`WireMobile`] snapshot, re-execs, and gets one response back: outcome,
//! `last_status`, audit.  Strictly one frame each way — the child drains its
//! audit fragment after eval rather than streaming it live, so no per-node
//! frame loop exists.

use crate::evaluator::absorb_tail;
use crate::evaluator::call;
use crate::io::TerminalState;
use crate::ir::Comp;
use crate::serial::{InternCtx, ScopeArcs, ScopeTable, SerialEnvSnapshot, SerialValue, build_arcs};
use crate::source::{FileId, SourceDb, Span};
use crate::subprocess::{WireMobile, reexec_child_shell};
use crate::types::{
    Break, CapturePolicy, Env, Error, Escape, ExecNode, ExecNodeKind, Mobile, Mooring, Settled,
    Shell, Status, Tail, Value,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The source a stage body's span points into.  Registered in the child
/// under the parent's own [`FileId`] ([`Shell::install_remote_context`]),
/// never re-minted, so a [`Span`] resolves to the same source in both
/// processes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireScriptContext {
    pub file: FileId,
    pub name: String,
    pub text: String,
}

impl WireScriptContext {
    /// `None` when the body carries no span, or its source is unregistered.
    pub(crate) fn capture(span: Option<Span>, sources: &SourceDb) -> Option<Self> {
        let file = span?.file;
        let source = sources.get(file)?;
        Some(Self {
            file,
            name: source.name().to_string(),
            text: source.as_str().to_string(),
        })
    }
}

/// Serialized one-body evaluation request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChildEvalRequest {
    /// Interned once for the whole request, including the env scopes nested
    /// inside `mobile` and `captured`.
    pub scope_table: ScopeTable,
    pub body: Arc<Comp>,
    pub mobile: WireMobile,
    /// Stage closure env: the child pushes a child scope and applies `body`
    /// via [`call::invoke`] with the upstream value data-last.
    pub captured: Option<SerialEnvSnapshot>,
    /// An instruction to the child rather than snapshot state, hence its own
    /// field — see [`Audit::active_policy`](crate::types::Audit::active_policy).
    pub audit_policy: Option<CapturePolicy>,
    /// Whether the child serializes its return value.  Only the final
    /// value-typed stage (`FinalValue::Report`) asks: a byte-mode stage must
    /// not fail the run over an incidental non-transferable retained value.
    pub wants_value: bool,
    /// So spans the child resolves locally — audit call sites — name the
    /// parent's source instead of `Shell::site_of`'s empty fallback.
    pub script: Option<WireScriptContext>,
}

/// Structured body outcome returned by the child.
///
/// `pgid` / `signal` cross as raw `i32` because `Pgid` / `Signal` derive no
/// serde impls; [`decode_response`] rebuilds the newtypes.  [`Tail`] has no
/// wire variant — the child's trampoline absorbs it before encoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum WireOutcome {
    Ok(Option<SerialValue>),
    Error {
        message: String,
        status: i32,
        hint: Option<String>,
        /// Its [`FileId`] was minted in the parent, so the parent resolves
        /// it against its own [`SourceDb`] and the caret lands correctly.
        span: Option<Span>,
    },
    Exit {
        code: i32,
    },
    #[cfg(unix)]
    Stopped {
        pgid: i32,
        signal: i32,
        cmd: String,
    },
}

/// Wire mirror of [`ExecNode`], its value field interned as a [`SerialValue`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireExecNode {
    pub kind: ExecNodeKind,
    pub cmd: String,
    pub args: Vec<String>,
    pub status: i32,
    pub script: String,
    pub line: usize,
    pub col: usize,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub value: SerialValue,
    pub children: Vec<Self>,
    pub start: i64,
    pub end: i64,
    pub principal: String,
}

impl WireExecNode {
    pub(crate) fn from_runtime(node: ExecNode, ctx: &mut InternCtx) -> Result<Self, Error> {
        let mut children = Vec::with_capacity(node.children.len());
        for child in node.children {
            children.push(Self::from_runtime(child, ctx)?);
        }
        Ok(Self {
            kind: node.kind,
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

    pub(crate) fn into_runtime(
        self,
        arcs: &ScopeArcs,
        manifest: &crate::types::BuiltinTable,
    ) -> Result<ExecNode, Error> {
        let mut children = Vec::with_capacity(self.children.len());
        for child in self.children {
            children.push(child.into_runtime(arcs, manifest)?);
        }
        Ok(ExecNode {
            kind: self.kind,
            cmd: self.cmd,
            args: self.args,
            status: self.status,
            script: self.script,
            line: self.line,
            col: self.col,
            stdout: self.stdout,
            stderr: self.stderr,
            value: self.value.into_runtime(arcs, manifest)?,
            children,
            start: self.start,
            end: self.end,
            principal: self.principal,
        })
    }
}

/// One audit node plus the scope table it interns against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireAuditNode {
    pub scope_table: ScopeTable,
    pub node: WireExecNode,
}

/// Full response emitted by one child.  `audit_nodes` survives a semantic
/// failure, so work recorded before the failure still reaches the parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChildEvalResponse {
    pub scope_table: ScopeTable,
    pub outcome: WireOutcome,
    pub last_status: i32,
    pub audit_nodes: Vec<WireAuditNode>,
}

/// Decoded response ready for the pipeline-stage parent.  `signal` is the
/// body's own outcome ([`None`] on success), returned beside `audit_nodes`
/// so the parent records audit before surfacing a failure.
pub(crate) struct DecodedResponse {
    pub value: Option<Value>,
    pub last_status: i32,
    pub audit_nodes: Vec<ExecNode>,
    pub signal: Option<Break>,
}

/// Re-phrase a value-serialization failure as a process-boundary error,
/// keeping the value's own hint when it has one (a `Handle`'s already points
/// at `await`).  Shared with the pipeline value edge in
/// `runtime/pipeline/helper`.
pub(crate) fn transfer_error(err: &Error) -> Error {
    let hint = err.hint.clone().unwrap_or_else(|| {
        "encode the value first, or avoid transferring live handles across a process boundary"
            .to_string()
    });
    Error::new(
        format!("value cannot cross the process boundary: {}", err.message),
        err.exit_code(),
    )
    .with_hint(hint)
}

/// Reify a [`Mobile`] and a body into a wire-ready [`ChildEvalRequest`].
/// `span` is the stage body's own, resolved against `sources` so the child
/// sees the same source under the same [`FileId`].
pub(crate) fn pack_request(
    body: Arc<Comp>,
    mobile: &Mobile,
    captured: Option<&Env>,
    audit_policy: Option<CapturePolicy>,
    wants_value: bool,
    span: Option<Span>,
    sources: &SourceDb,
) -> Settled<ChildEvalRequest> {
    let mut ctx = InternCtx::new();
    let captured = match captured {
        Some(env) => Some(SerialEnvSnapshot::from_runtime(env, &mut ctx)?),
        None => None,
    };
    let mobile = WireMobile::from_runtime(mobile, &mut ctx)?;
    Ok(ChildEvalRequest {
        scope_table: ctx.scope_table,
        body,
        mobile,
        captured,
        audit_policy,
        wants_value,
        script: WireScriptContext::capture(span, sources),
    })
}

/// One request evaluated in the child.  `result` is already settled: the
/// local trampoline absorbed any terminal [`Tail`], which cannot cross a
/// process boundary.
struct EvalOutcome {
    result: Settled<Value>,
    audit_nodes: Vec<ExecNode>,
    last_status: i32,
}

/// Evaluate one stage in a freshly hydrated child shell.  `force_output`
/// applies the value edge's `!{x}` before the value ships.  An outer `Err`
/// is a *pre-eval* fault — arc rebuild or mobile hydration, the body never
/// ran — which [`run_child_eval`] folds into a [`break_response`].
fn eval_request(
    request: ChildEvalRequest,
    upstream: Option<Value>,
    force_output: bool,
) -> Settled<EvalOutcome> {
    let ChildEvalRequest {
        scope_table,
        body,
        mobile,
        captured,
        audit_policy,
        script,
        ..
    } = request;

    // The child shell does not exist yet, so re-link against the same
    // core-plus-hook manifest `reexec_child_shell` is about to boot with.
    let arcs = build_arcs(&scope_table, &crate::sandbox::wire_manifest())?;
    let mut shell = reexec_child_shell(mobile, &arcs)?;
    shell.local.audit.install_active_policy(audit_policy);
    if let Some(ctx) = script {
        shell.install_remote_context(&ctx.name, ctx.file, &ctx.text);
    }
    // No sink to replay to, yet the body may still call `surface`: `()` is
    // the `EventSink` that discards.
    let mooring = Mooring::for_stage(shell.durable_root(), Arc::new(()));
    crate::dbg_trace!(
        "child-eval",
        "pre-eval: audit.active={}",
        shell.local.audit.active(),
    );

    shell.io.terminal = TerminalState::probe();
    shell.io.launch_role = crate::io::LaunchRole::PipelineStage;
    let captured = captured
        .ok_or_else(|| {
            Break::Error(Error::new(
                "pipeline stage request carried no captured env",
                1,
            ))
        })?
        .into_runtime(&arcs, &shell.session.builtins)?;
    let mut child = Shell::child_of(&captured, &mut shell);
    // A tail call cannot cross the boundary — the parent's callee and args
    // are meaningless in this address space — so settle it here.
    let result = absorb_tail(
        call::invoke(&body, upstream, Tail::No, &mooring, &mut child),
        &mooring,
        &mut child,
    );
    let result = if force_output {
        result.and_then(|value| {
            crate::runtime::pipeline::force_pipe_value(value, &mooring, &mut child)
        })
    } else {
        result
    };
    // Before `return_to`, which would merge the fragment into the outer shell.
    let audit_nodes = child.local.audit.take_fragment().into_nodes();
    child.return_to(&mut shell);

    crate::dbg_trace!(
        "child-eval",
        "post-eval: result_ok={} audit_nodes={}",
        result.is_ok(),
        audit_nodes.len()
    );
    let last_status = shell.mobile().control.last_status;
    Ok(EvalOutcome {
        result,
        audit_nodes,
        last_status,
    })
}

/// Each node interns against its own scope table, independent of the
/// response's.
fn pack_audit_nodes(nodes: Vec<ExecNode>) -> Settled<Vec<WireAuditNode>> {
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        let mut ctx = InternCtx::new();
        let node = WireExecNode::from_runtime(node, &mut ctx)?;
        out.push(WireAuditNode {
            scope_table: ctx.scope_table,
            node,
        });
    }
    Ok(out)
}

/// Project a non-`Ok` body outcome onto the wire.  `Stopped` keeps its own
/// variant so the parent REPL parks the job rather than seeing a generic
/// error; the `Ok` arm stays with the caller, gated on `wants_value`.
fn break_to_outcome(b: Break) -> WireOutcome {
    match b {
        Break::Error(err) => {
            let status = err.exit_code();
            WireOutcome::Error {
                message: err.message,
                status,
                hint: err.hint,
                span: err.span,
            }
        }
        Break::Escape(Escape::Exit(code)) => WireOutcome::Exit { code },
        #[cfg(unix)]
        Break::Escape(Escape::Stopped { pgid, signal, cmd }) => WireOutcome::Stopped {
            pgid: pgid.as_raw(),
            signal: signal.number(),
            cmd,
        },
    }
}

/// The one child runner.  The returned [`Value`] is the in-process value for
/// the pipeline's value-out edge — independent of `wants_value`, which gates
/// only the value serialized into the response.  A value that `wants_value`
/// asked for but cannot be serialized becomes a [`WireOutcome::Error`], not a
/// transport fault.
pub(crate) fn run_child_eval(
    request: ChildEvalRequest,
    upstream: Option<Value>,
    force_output: bool,
) -> (ChildEvalResponse, Option<Value>) {
    let wants_value = request.wants_value;
    let outcome = match eval_request(request, upstream, force_output) {
        Ok(outcome) => outcome,
        // The body never ran: no audit, no output value.
        Err(b) => return (break_response(b), None),
    };
    let EvalOutcome {
        result,
        audit_nodes,
        last_status,
    } = outcome;

    let audit_nodes = match pack_audit_nodes(audit_nodes) {
        Ok(nodes) => nodes,
        Err(b) => return (break_response(b), None),
    };

    let mut ctx = InternCtx::new();

    let (outcome, output_value) = match result {
        Ok(value) => {
            let packed = if wants_value {
                match SerialValue::from_runtime(&value, &mut ctx) {
                    Ok(serial) => Some(serial),
                    Err(e) => {
                        let outcome = break_to_outcome(Break::Error(transfer_error(&e)));
                        return finish(ctx, outcome, last_status, audit_nodes, None);
                    }
                }
            } else {
                None
            };
            (WireOutcome::Ok(packed), Some(value))
        }
        Err(b) => (break_to_outcome(b), None),
    };

    finish(ctx, outcome, last_status, audit_nodes, output_value)
}

/// Assemble the response from its fully-packed parts.
fn finish(
    ctx: InternCtx,
    outcome: WireOutcome,
    last_status: i32,
    audit_nodes: Vec<WireAuditNode>,
    output_value: Option<Value>,
) -> (ChildEvalResponse, Option<Value>) {
    (
        ChildEvalResponse {
            scope_table: ctx.scope_table,
            outcome,
            last_status,
            audit_nodes,
        },
        output_value,
    )
}

/// A response for a failure that fires before, or instead of, a real eval:
/// empty audit, the signal projected onto the outcome.
pub(crate) fn break_response(signal: Break) -> ChildEvalResponse {
    // `$?` reports what the body would have set; other signals fall back to 1.
    let last_status = match &signal {
        Break::Error(err) => err.exit_code(),
        Break::Escape(_) => 1,
    };
    ChildEvalResponse {
        scope_table: ScopeTable::default(),
        outcome: break_to_outcome(signal),
        last_status,
        audit_nodes: Vec::new(),
    }
}

/// Rehydrate a child's response for its parent.  An outer `Err` is a *decode
/// fault* — the payload would not turn back into runtime values — as distinct
/// from `signal`, the body's own outcome, which crossed the wire intact.
pub(crate) fn decode_response(response: ChildEvalResponse) -> Settled<DecodedResponse> {
    let manifest = crate::sandbox::wire_manifest();
    let mut audit_nodes = Vec::with_capacity(response.audit_nodes.len());
    for entry in response.audit_nodes {
        let arcs = build_arcs(&entry.scope_table, &manifest)?;
        audit_nodes.push(entry.node.into_runtime(&arcs, &manifest)?);
    }
    let arcs = build_arcs(&response.scope_table, &manifest)?;
    let (value, signal) = match response.outcome {
        WireOutcome::Ok(value) => {
            let value = match value {
                Some(value) => Some(value.into_runtime(&arcs, &manifest)?),
                None => None,
            };
            (value, None)
        }
        WireOutcome::Exit { code } => (None, Some(Break::Escape(Escape::Exit(code)))),
        #[cfg(unix)]
        WireOutcome::Stopped { pgid, signal, cmd } => (
            None,
            Some(Break::Escape(Escape::Stopped {
                pgid: crate::process::Pgid::from_raw(pgid)
                    .expect("a stopped child's pgid is positive"),
                signal: crate::process::Signal::new(signal),
                cmd,
            })),
        ),
        WireOutcome::Error {
            message,
            status,
            hint,
            span,
        } => (
            None,
            Some(Break::Error(Error {
                message,
                status: Status::Code(status),
                span,
                hint,
            })),
        ),
    };
    Ok(DecodedResponse {
        value,
        last_status: response.last_status,
        audit_nodes,
        signal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Shell, elaborate, evaluate, parse};

    fn compile_one(source: &str) -> Arc<Comp> {
        let ast = parse(source).expect("parse");
        Arc::new(elaborate(&ast, std::collections::HashSet::default(), "").expect("elaborate"))
    }

    fn eval_value(source: &str, shell: &mut Shell) -> Value {
        evaluate(&compile_one(source), &Mooring::adrift(), shell).expect("eval")
    }

    /// A stage request from a freshly captured snapshot.
    fn pack_stage(stage: Arc<Comp>, shell: &Shell, wants_value: bool) -> ChildEvalRequest {
        let captured = shell.snapshot();
        let span = stage.span;
        pack_request(
            stage,
            &shell.mobile,
            Some(&captured),
            shell.local.audit.active_policy(),
            wants_value,
            span,
            &shell.session.sources,
        )
        .expect("pack")
    }

    #[test]
    fn stage_job_round_trip_applies_upstream_data_last() {
        let shell = Shell::default();
        let stage = compile_one("{ |x| return $[$x + 1] }");
        let request = pack_stage(stage, &shell, true);
        let (response, value) = run_child_eval(request, Some(Value::Int(41)), false);
        let decoded = decode_response(response).expect("decode");
        assert_eq!(value, Some(Value::Int(42)));
        assert_eq!(decoded.value, Some(Value::Int(42)));
        assert_eq!(decoded.last_status, 0);
        assert!(decoded.audit_nodes.is_empty());
        assert!(decoded.signal.is_none());
    }

    #[test]
    fn stage_report_carries_audit_even_on_helper_error() {
        // A response carrying both audit and a failure must surface both, so
        // the user sees what ran before things went wrong.
        let mut ctx = InternCtx::new();
        let node = WireExecNode::from_runtime(
            ExecNode::command(
                "/bin/echo",
                vec!["hi".into()],
                0,
                crate::types::CallSite {
                    script: "<test>".into(),
                    line: 1,
                    col: 1,
                },
                crate::types::AuditIo::default(),
                Value::Unit,
                Vec::new(),
                crate::types::AuditTime::default(),
                String::new(),
            ),
            &mut ctx,
        )
        .expect("wire");
        let response = ChildEvalResponse {
            scope_table: ScopeTable::default(),
            outcome: WireOutcome::Error {
                message: "helper failed".into(),
                status: 1,
                hint: None,
                span: None,
            },
            last_status: 1,
            audit_nodes: vec![WireAuditNode {
                scope_table: ctx.scope_table,
                node,
            }],
        };
        let decoded = decode_response(response).expect("decode");
        assert!(
            matches!(decoded.signal, Some(Break::Error(ref e)) if e.message == "helper failed"),
            "expected structured error in signal; got {:?}",
            decoded.signal
        );
        assert_eq!(
            decoded.audit_nodes.len(),
            1,
            "audit must survive the helper failure"
        );
    }

    #[test]
    fn alias_stays_removable_across_the_mobile_wire() {
        // An alias frame carries `removable_by_unalias`, the flag `unalias`
        // filters on.  Lose it in hydration and a stage cannot `unalias` a
        // name the parent aliased, diverging from local evaluation.
        let mut parent = Shell::default();
        let thunk = eval_value("return { |args| echo aliased }", &mut parent);
        parent
            .install_alias("ll".to_string(), thunk)
            .expect("install alias");
        assert!(parent.has_alias("ll"), "parent installs a removable alias");

        let mobile = parent.mobile();
        let mut ctx = InternCtx::new();
        let wire = WireMobile::from_runtime(&mobile, &mut ctx).expect("to wire");
        let request = ChildEvalRequest {
            scope_table: ctx.scope_table,
            body: compile_one("return unit"),
            mobile: wire,
            captured: None,
            audit_policy: None,
            wants_value: false,
            script: None,
        };

        // Cross the actual codec, not just the wire types.
        let json = serde_json::to_vec(&request).expect("serialise request");
        let request: ChildEvalRequest = serde_json::from_slice(&json).expect("deserialise request");

        let arcs = build_arcs(&request.scope_table, &crate::builtins::core_builtin_table())
            .expect("arcs");
        let mut child = reexec_child_shell(request.mobile, &arcs).expect("child shell");
        assert!(
            child.has_alias("ll"),
            "the hydrated child must see the alias as removable"
        );
        assert!(
            child.remove_alias("ll"),
            "`unalias ll` must succeed in the child, mirroring local eval"
        );
    }

    #[test]
    fn wire_exec_node_kind_survives_a_json_round_trip() {
        // The kind crosses as a serde enum, not a string with a defaulting
        // decode arm, so a new `ExecNodeKind` variant fails the build here
        // rather than silently degrading to `Command`.
        let mut ctx = InternCtx::new();
        let node = WireExecNode::from_runtime(
            ExecNode::capability_check(
                "net",
                "denied",
                crate::types::CallSite {
                    script: "<test>".into(),
                    line: 1,
                    col: 1,
                },
                String::new(),
                crate::types::Map::default(),
            ),
            &mut ctx,
        )
        .expect("wire");
        assert_eq!(node.kind, crate::types::ExecNodeKind::CapabilityCheck);

        let json = serde_json::to_vec(&node).expect("serialise node");
        let back: WireExecNode = serde_json::from_slice(&json).expect("deserialise node");
        let manifest = crate::builtins::core_builtin_table();
        let arcs = build_arcs(&ctx.scope_table, &manifest).expect("arcs");
        let runtime = back.into_runtime(&arcs, &manifest).expect("into runtime");
        assert_eq!(
            runtime.kind,
            crate::types::ExecNodeKind::CapabilityCheck,
            "the node's kind must survive the wire round-trip"
        );
    }

    #[test]
    fn stage_job_skips_report_value_when_parent_does_not_need_it() {
        // The parent reads bytes off stdout and never asks for a value.
        // Without the gate, an incidental non-transferable retained value
        // would fail the whole pipeline while building the response.
        let shell = Shell::default();
        let stage = compile_one("return 7");
        let request = pack_stage(stage, &shell, false);
        let (response, value) = run_child_eval(request, None, false);
        assert!(
            matches!(response.outcome, WireOutcome::Ok(None)),
            "response value should be skipped"
        );
        // The value-out edge stays usable regardless.
        assert_eq!(value, Some(Value::Int(7)));
    }

    #[test]
    fn stage_job_round_trip_preserves_alias_binding() {
        let mut shell = Shell::default();
        let thunk = eval_value("{ |args| echo ok; return $[$args[0] * 2] }", &mut shell);
        shell.install_alias("twice".into(), thunk).unwrap();

        let stage = compile_one("twice 21");
        let request = pack_stage(stage, &shell, true);
        let (response, value) = run_child_eval(request, None, false);
        let _ = decode_response(response).expect("decode");
        assert_eq!(value, Some(Value::Int(42)));
    }

    #[test]
    fn transfer_error_phrases_non_transferable_values_for_the_boundary() {
        // A `Handle` fails the gate before the response is built, and reads
        // as a boundary error rather than a transport fault.
        use std::sync::Mutex;
        let handle = Value::Handle(crate::types::HandleInner {
            result: Arc::new(Mutex::new(None)),
            cached: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(crate::types::HandleState::Completed)),
            stdout_buf: Arc::new(Mutex::new(Vec::new())),
            stderr_buf: Arc::new(Mutex::new(Vec::new())),
            surface_buf: Arc::new(Mutex::new(Vec::new())),
            joined: Arc::new(Mutex::new(false)),
            last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
            cmd: "dummy".into(),
            cancel: crate::process::CancelScope::default(),
        });
        let mut ctx = InternCtx::new();
        let err =
            transfer_error(&SerialValue::from_runtime(&handle, &mut ctx).expect_err("must fail"));
        assert!(err.message.contains("cannot cross the process boundary"));
        assert!(err.message.contains("value"));
    }
}
