//! The shared remote-eval runner: one wire shape, one child runner,
//! for the process-staged pipeline stage.
//!
//! A process-staged pipeline stage (`runtime/pipeline/`) re-execs a child
//! to evaluate a [`Comp`]: it packs the body plus a [`WireMobile`]
//! snapshot, reconstructs a shell in the child, evaluates, and reports a
//! structured outcome with audit nodes.  Value serialization is gated by
//! [`ChildEvalRequest`]'s `wants_value`.
//!
//! One request frame in, one response frame out.  The child does not
//! stream audit live: it drains its audit fragment after eval and ships
//! it in the single response, so there is no per-node frame loop.  Live
//! streaming, if a future parcel wants it, reintroduces a frame loop and
//! does not belong here.

use crate::diagnostic::SourceLoc;
use crate::evaluator::absorb_tail;
use crate::evaluator::call;
use crate::io::TerminalState;
use crate::ir::Comp;
use crate::serial::{InternCtx, ScopeTable, SerialEnvSnapshot, SerialValue, build_arcs};
use crate::subprocess::{WireExecNode, WireMobile, reexec_child_shell};
use crate::types::{
    Break, CapturePolicy, Env, Error, Escape, ExecNode, Mobile, Settled, Shell, Status, Tail, Value,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Serialized one-body evaluation request.
///
/// `wants_value` gates the response's return value *before*
/// serialization: a stage that does not need its return value (a
/// byte-mode pipeline stage) must not fail the whole run over an
/// incidental non-transferable retained value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChildEvalRequest {
    /// Interned scope table shared across every `SerialValue` /
    /// `SerialEnvSnapshot` in this request — including the env scopes
    /// embedded in `mobile` and `captured`.
    pub scope_table: ScopeTable,
    pub body: Arc<Comp>,
    pub mobile: WireMobile,
    /// Stage closure env: the child pushes a child scope and applies
    /// `body` via [`call::invoke`] with the upstream value data-last.
    pub captured: Option<SerialEnvSnapshot>,
    /// Carried separately from `mobile`; see
    /// [`Audit::active_policy`](crate::types::Audit::active_policy)
    /// for why audit policy isn't part of the mobile snapshot.
    pub audit_policy: Option<CapturePolicy>,
    /// Whether the child serializes its return value into the response on
    /// success.  Pipeline: only the final value-typed stage
    /// (`FinalValue::Report`).
    pub wants_value: bool,
}

/// Structured body outcome returned by the child.
///
/// `Ok` carries the body's success value when [`ChildEvalRequest`]'s
/// `wants_value` asked for it (`None` otherwise); `Error` carries a
/// structured error to re-raise as `Break::Error`; `Exit` propagates
/// `Escape::Exit`; `Stopped` propagates `Escape::Stopped` (a child
/// parked by a job-control stop signal).  `pgid` / `signal` cross the
/// wire as raw `i32` because `Pgid` / `Signal` do not derive
/// `Serialize`/`Deserialize`; [`decode_response`] reconstructs the
/// newtypes.  `Tail` has no wire variant — the child's trampoline
/// absorbs it before encoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum WireOutcome {
    Ok(Option<SerialValue>),
    Error {
        message: String,
        status: i32,
        hint: Option<String>,
        /// Source position of the error, when one was attached.  The
        /// location carries the source's [`FileId`], minted in the parent
        /// before the body crossed the wire; the parent resolves it against
        /// its own `SourceDb` at render, so the caret lands in the right
        /// source after decode.
        loc: Option<SourceLoc>,
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

/// One audit node plus the scope table it interns against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireAuditNode {
    pub scope_table: ScopeTable,
    pub node: WireExecNode,
}

/// Full response emitted by one child.
///
/// `audit_nodes` survives a semantic failure so audit captured before the
/// failure reaches the parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChildEvalResponse {
    pub scope_table: ScopeTable,
    pub outcome: WireOutcome,
    pub last_status: i32,
    pub audit_nodes: Vec<WireAuditNode>,
}

/// Decoded response ready for the pipeline-stage parent.
///
/// `value` is the body's return value (present iff the child packed one);
/// `signal` carries the helper-reported semantic outcome as a [`Break`]
/// (`Some` for `Error` / `Exit` / `Stopped`, `None` for success).  The
/// two are returned side by side with `audit_nodes` so the parent can
/// record audit before surfacing a failure.
pub(crate) struct DecodedResponse {
    pub value: Option<Value>,
    pub last_status: i32,
    pub audit_nodes: Vec<ExecNode>,
    pub signal: Option<Break>,
}

/// Re-phrase a value-serialization failure as a process-boundary error.
/// A value type that already explains *how* to avoid the crossing — a
/// `Handle`, whose serializer hints at `await` — keeps its own hint; an
/// otherwise hintless failure gets the generic boundary guidance.
///
/// Shared by both serial-boundary crossings: the remote-eval response
/// edge here, and the pipeline value edge in `runtime/pipeline/helper`.
pub(crate) fn transfer_error(err: Error) -> Error {
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
/// Inverse of [`decode_response`].  `captured` carries the pipeline stage
/// closure env.
pub(crate) fn pack_request(
    body: Arc<Comp>,
    mobile: &Mobile,
    captured: Option<&Env>,
    audit_policy: Option<CapturePolicy>,
    wants_value: bool,
) -> Settled<ChildEvalRequest> {
    let mut ctx = InternCtx::new();
    let captured = match captured {
        Some(env) => Some(SerialEnvSnapshot::from_runtime(env, &mut ctx)?),
        None => None,
    };
    let mobile = WireMobile::from_mobile(mobile, &mut ctx)?;
    Ok(ChildEvalRequest {
        scope_table: ctx.scope_table,
        body,
        mobile,
        captured,
        audit_policy,
        wants_value,
    })
}

/// Output of evaluating one request in the child: the already-settled
/// body result (the local trampoline absorbed any terminal `Tail` —
/// it cannot cross the process boundary), the audit nodes drained after
/// eval, and the `last_status` the parent installs.
struct EvalOutcome {
    result: Settled<Value>,
    audit_nodes: Vec<ExecNode>,
    last_status: i32,
}

/// Build the child shell, install policy and a buffering surface sink,
/// evaluate the pipeline stage, drain audit, and read `last_status`.
/// `force_output` forces the body's value once before it ships
/// (`x | f = f !{x}`); the serving child derives it from its own
/// value-out edge.  The outer `Err` is a *pre-eval* fault (arc rebuild /
/// mobile hydration) — the body never ran — which [`run_child_eval`]
/// folds into a [`break_response`].
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
        ..
    } = request;

    let arcs = build_arcs(&scope_table)?;
    let mut shell = reexec_child_shell(mobile, &arcs)?;
    shell.local.audit.install_active_policy(audit_policy);
    // The stage child has no surface sink to replay to, but the body may
    // still call `surface`; the no-op `()` sink discards those calls.
    shell.turn.surface = Some(Arc::new(()));
    crate::dbg_trace!(
        "child-eval",
        "pre-eval: audit.active={}",
        shell.local.audit.active(),
    );

    shell.turn.io.terminal = TerminalState::probe();
    shell.turn.io.launch_role = crate::io::LaunchRole::PipelineStage;
    let captured = captured
        .ok_or_else(|| {
            Break::Error(Error::new(
                "pipeline stage request carried no captured env",
                1,
            ))
        })?
        .into_runtime(&arcs)?;
    let mut child = Shell::child_of(&captured, &mut shell);
    // Helper-local absorption point: a tail call cannot cross the process
    // boundary (the parent's callee/args wouldn't be valid in this address
    // space), so settle it here; the stage's tail-ness has no effect
    // across the boundary ([`Tail::No`]).
    let result = absorb_tail(
        call::invoke(&body, upstream, Tail::No, &mut child),
        &mut child,
    );
    // `x | f = f !{x}`: a value-edge producer's output is forced once
    // before it ships, via the shared value-edge `!{x}`.
    let result = if force_output {
        result.and_then(|value| crate::runtime::pipeline::force_pipe_value(value, &mut child))
    } else {
        result
    };
    // Drain the child's audit before `return_to` merges it into the outer
    // shell.
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

/// Intern each runtime [`ExecNode`] into a [`WireAuditNode`] for transport.
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

/// Project a non-`Ok` body outcome into the wire-form [`WireOutcome`].
///
/// `Stopped` flows through as its own wire variant carrying pgid /
/// signal / cmd, so the parent REPL can park the job correctly rather
/// than seeing a generic error.  The `Ok` arm is handled separately by
/// the caller because its serialization is gated on `wants_value`.
fn break_to_outcome(b: Break) -> WireOutcome {
    match b {
        Break::Error(err) => {
            let status = err.exit_code();
            WireOutcome::Error {
                message: err.message,
                status,
                hint: err.hint,
                loc: err.loc,
            }
        }
        Break::Escape(Escape::Exit(code)) => WireOutcome::Exit { code },
        #[cfg(unix)]
        Break::Escape(Escape::Stopped { pgid, signal, cmd }) => WireOutcome::Stopped {
            pgid: pgid.0,
            signal: signal.number(),
            cmd,
        },
    }
}

/// The one child runner.  Build the shell, evaluate the stage, and pack
/// one [`ChildEvalResponse`].  The returned `Option<Value>` is the
/// in-process value for the pipeline's value-out edge — independent of
/// `wants_value`, which only gates the value serialized into the response.
/// `force_output` forces the body's value once before it ships
/// (`x | f = f !{x}`).
///
/// Value serialization is gated before serialization: a non-transferable
/// `Ok` value (with `wants_value`) becomes a structured `WireOutcome::Error`
/// carrying the hinted "cannot cross the process boundary" message.
pub(crate) fn run_child_eval(
    request: ChildEvalRequest,
    upstream: Option<Value>,
    force_output: bool,
) -> (ChildEvalResponse, Option<Value>) {
    let wants_value = request.wants_value;
    let outcome = match eval_request(request, upstream, force_output) {
        Ok(outcome) => outcome,
        // Pre-eval fault: the body never ran.  No audit, no output value;
        // the parent reads `last_status` off the response.
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
                        let outcome = break_to_outcome(Break::Error(transfer_error(e)));
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

/// Build a response for a pre-eval failure: empty audit, the signal
/// projected to the outcome.  Used for failures that fire before (or
/// instead of) a real eval.
pub(crate) fn break_response(signal: Break) -> ChildEvalResponse {
    // For an `Error` signal the user-visible exit code is the error's own
    // status, so `$?` reflects what the body would have set; any other
    // signal falls back to `1`.
    let last_status = match &signal {
        Break::Error(err) => err.exit_code(),
        _ => 1,
    };
    ChildEvalResponse {
        scope_table: ScopeTable::default(),
        outcome: break_to_outcome(signal),
        last_status,
        audit_nodes: Vec::new(),
    }
}

/// Rehydrate a [`ChildEvalResponse`] into runtime values, audit nodes,
/// and the body's pass/fail signal, returning audit and signal side by
/// side so the parent can record audit before surfacing a failure.
///
/// The outer `Err` is a *decode fault* — the wire payload could not be
/// turned back into runtime values — distinct from `signal`, which is the
/// body's own outcome that crossed the wire intact.
pub(crate) fn decode_response(response: ChildEvalResponse) -> Settled<DecodedResponse> {
    let mut audit_nodes = Vec::with_capacity(response.audit_nodes.len());
    for entry in response.audit_nodes {
        let arcs = build_arcs(&entry.scope_table)?;
        audit_nodes.push(entry.node.into_runtime(&arcs)?);
    }
    let arcs = build_arcs(&response.scope_table)?;
    let (value, signal) = match response.outcome {
        WireOutcome::Ok(value) => {
            let value = match value {
                Some(value) => Some(value.into_runtime(&arcs)?),
                None => None,
            };
            (value, None)
        }
        WireOutcome::Exit { code } => (None, Some(Break::Escape(Escape::Exit(code)))),
        #[cfg(unix)]
        WireOutcome::Stopped { pgid, signal, cmd } => (
            None,
            Some(Break::Escape(Escape::Stopped {
                pgid: crate::process::Pgid(pgid),
                signal: crate::process::Signal::new(signal),
                cmd,
            })),
        ),
        WireOutcome::Error {
            message,
            status,
            hint,
            loc,
        } => (
            None,
            Some(Break::Error(Error {
                message,
                status: Status::Code(status),
                loc,
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
        Arc::new(elaborate(&ast, Default::default()))
    }

    fn eval_value(source: &str, shell: &mut Shell) -> Value {
        evaluate(&compile_one(source), shell).expect("eval")
    }

    /// Pack a pipeline-stage request from a freshly captured snapshot.
    fn pack_stage(stage: Arc<Comp>, shell: &Shell, wants_value: bool) -> ChildEvalRequest {
        let captured = shell.snapshot();
        pack_request(
            stage,
            &shell.mobile,
            Some(&captured),
            shell.local.audit.active_policy(),
            wants_value,
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
        // The decode contract: a response carrying both audit nodes and a
        // structured failure must surface both to the parent.  Audit
        // captures from nested external commands (or any partial work)
        // survive the failure so the user can see what ran before things
        // went wrong.
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
                loc: None,
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
        // R3: an alias frame carries `removable_by_unalias`, the flag
        // `unalias` filters on.  Hydration must preserve it, or a pipeline
        // helper cannot `unalias` a name the parent aliased — stage
        // evaluation would diverge from local.  This drives the exact
        // hydration seam: install an alias, pack the mobile through
        // `WireMobile`, JSON-frame it as the codec does, rebuild the child
        // shell, and confirm the alias is both visible (`has_alias`) and
        // removable (`remove_alias`).
        let mut parent = Shell::default();
        let thunk = eval_value("return { |args| echo aliased }", &mut parent);
        parent
            .install_alias("ll".to_string(), thunk)
            .expect("install alias");
        assert!(parent.has_alias("ll"), "parent installs a removable alias");

        let mobile = parent.mobile();
        let mut ctx = InternCtx::new();
        let wire = WireMobile::from_mobile(&mobile, &mut ctx).expect("to wire");
        let request = ChildEvalRequest {
            scope_table: ctx.scope_table,
            body: compile_one("return unit"),
            mobile: wire,
            captured: None,
            audit_policy: None,
            wants_value: false,
        };

        // Cross the actual codec: serialize the request and read it back.
        let json = serde_json::to_vec(&request).expect("serialise request");
        let request: ChildEvalRequest = serde_json::from_slice(&json).expect("deserialise request");

        let arcs = build_arcs(&request.scope_table).expect("arcs");
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
        // The audit-tree node's kind crosses the wire as a serde enum, not
        // a string with a defaulting decode arm: a capability-check node
        // round-trips through the wire codec as `CapabilityCheck`, never
        // silently degraded to `Command`.  Adding an `ExecNodeKind`
        // variant now fails the build at the wire rather than aliasing
        // onto the catch-all.
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
        let arcs = build_arcs(&ctx.scope_table).expect("arcs");
        let runtime = back.into_runtime(&arcs).expect("into runtime");
        assert_eq!(
            runtime.kind,
            crate::types::ExecNodeKind::CapabilityCheck,
            "the node's kind must survive the wire round-trip"
        );
    }

    #[test]
    fn stage_job_skips_report_value_when_parent_does_not_need_it() {
        // Byte-mode stages do not embed their return value in the
        // response — the parent reads bytes off the stage's stdout and
        // never asks for a value.  Without this gate, an incidental
        // non-transferable retained value (a handle, etc.) would fail the
        // whole pipeline while building the response.
        let shell = Shell::default();
        let stage = compile_one("return 7");
        let request = pack_stage(stage, &shell, false);
        let (response, value) = run_child_eval(request, None, false);
        assert!(
            matches!(response.outcome, WireOutcome::Ok(None)),
            "response value should be skipped"
        );
        // Output value still flows so an inter-stage value-out edge
        // remains usable.
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
        // A non-transferable value (a `Handle`) becomes the hinted
        // process-boundary error the value-serialization gate raises
        // before the value reaches the response, rather than a transport
        // fault.
        use std::sync::Mutex;
        let handle = Value::Handle(crate::types::HandleInner {
            result: Arc::new(Mutex::new(None)),
            cached: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(crate::types::HandleState::Completed)),
            stdout_buf: Arc::new(Mutex::new(Vec::new())),
            stderr_buf: Arc::new(Mutex::new(Vec::new())),
            surface_buf: Arc::new(Mutex::new(Vec::new())),
            joined: Arc::new(Mutex::new(false)),
            cmd: "dummy".into(),
            cancel: crate::process::CancelScope::default(),
        });
        let mut ctx = InternCtx::new();
        let err =
            transfer_error(SerialValue::from_runtime(&handle, &mut ctx).expect_err("must fail"));
        assert!(err.message.contains("cannot cross the process boundary"));
        assert!(err.message.contains("value"));
    }
}
