//! The shared remote-eval runner: one wire shape, one child runner,
//! for both re-exec'd-child eval protocols.
//!
//! Two consumers re-exec a child to evaluate a [`Comp`]: the
//! sandbox-confined eval (`sandbox/`) and the process-staged pipeline
//! stage (`evaluator/pipeline/`).  Both pack the body plus a
//! [`WireMobile`] snapshot, reconstruct a shell in the child, evaluate,
//! and report a structured outcome with audit nodes.  The two differ in
//! exactly two preludes: the *eval shape* ([`ChildKind`]) and the
//! *gating* of value and mobile serialization ([`ChildEvalRequest`]'s
//! `wants_value` / `wants_mobile`).  Everything between is shared here.
//!
//! One request frame in, one response frame out.  Neither child streams
//! audit live: both drain their audit fragment after eval and ship it in
//! the single response, so there is no per-node frame loop.  Live
//! streaming, if a future parcel wants it, reintroduces a frame loop and
//! does not belong here.

use crate::diagnostic::SourceLoc;
use crate::evaluator::absorb_tail;
use crate::evaluator::call;
use crate::evaluator::comp::eval_comp;
use crate::io::TerminalState;
use crate::ir::Comp;
use crate::serial::{InternCtx, ScopeTable, SerialEnvSnapshot, SerialValue, build_arcs};
use crate::subprocess::{WireExecNode, WireMobile, reexec_child_shell};
use crate::types::{
    Break, CapturePolicy, Env, Error, Escape, ExecNode, Mobile, Settled, Shell, Status, Tail, Value,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// Serialized one-body evaluation request.
///
/// `wants_value` / `wants_mobile` gate the two value-bearing fields of
/// the response *before* serialization: a stage that does not need its
/// return value (a byte-mode pipeline stage) must not fail the whole run
/// over an incidental non-transferable retained value, and a pipeline
/// stage (a subshell) must not ship its post-run mobile at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChildEvalRequest {
    /// Interned scope table shared across every `SerialValue` /
    /// `SerialEnvSnapshot` in this request — including the env scopes
    /// embedded in `mobile` and `captured`.
    pub scope_table: ScopeTable,
    pub body: Arc<Comp>,
    pub mobile: WireMobile,
    /// Stage closure env (pipeline only).  `Some` => the child pushes a
    /// child scope and applies `body` via [`call::invoke`] with the
    /// upstream value data-last; `None` => the mobile already carries the
    /// lexical env and the child runs [`eval_comp`] directly (sandbox).
    pub captured: Option<SerialEnvSnapshot>,
    /// Carried separately from `mobile`; see
    /// [`Audit::active_policy`](crate::types::Audit::active_policy)
    /// for why audit policy isn't part of the mobile snapshot.
    pub audit_policy: Option<CapturePolicy>,
    /// Whether the child serializes its return value into the response on
    /// success.  Sandbox: always.  Pipeline: only the final value-typed
    /// stage (`FinalValue::Report`).
    pub wants_value: bool,
    /// Whether the child serializes its post-run mobile.  Sandbox: yes
    /// (the parent installs it).  Pipeline: no (stages are subshells; the
    /// parent installs only `last_status`).
    pub wants_mobile: bool,
    /// Capability token authenticating the confinement marker.  `Some`
    /// only for the sandbox re-exec: the parent mints a fresh secret per
    /// re-exec and the confined child adopts it (records it and stamps it
    /// into `RAL_SANDBOX_ACTIVE`) so nested grants dispatch locally
    /// instead of re-entering confinement.  Delivered over this IPC
    /// channel — which an external wrapper cannot write to — rather than
    /// the inheritable env var, so a forged marker does not authenticate.
    /// `None` for a pipeline stage (which never enters the OS sandbox).
    pub sandbox_token: Option<String>,
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
/// `mobile` is `Some` only when the request set `wants_mobile` *and* the
/// post-run mobile packed; a pack failure downgrades a success outcome
/// to an error and leaves `mobile: None` (the sandbox parent substitutes
/// a local placeholder).  `audit_nodes` survives a semantic failure so
/// audit captured before the failure reaches the parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChildEvalResponse {
    pub scope_table: ScopeTable,
    pub outcome: WireOutcome,
    pub last_status: i32,
    pub mobile: Option<WireMobile>,
    pub audit_nodes: Vec<WireAuditNode>,
    /// Events the body handed to `surface` while evaluating, interned
    /// into `scope_table`.  The sandbox parent replays them through its
    /// own sink once the eval returns; the pipeline parent drops them.
    pub surface_events: Vec<SerialValue>,
}

/// The two eval shapes — the one axis the runner cannot merge.
///
/// `Sandbox` runs `eval_comp` against a shell whose mobile already
/// carries the lexical env (the contract layer shaped it).
/// `PipelineStage` hydrates `captured`, builds a `Shell::child_of`, and
/// applies the body via `call::invoke` with the upstream value
/// data-last.  See the §4 risk list in the design memo for why folding
/// the two would change checker-visible semantics.
pub(crate) enum ChildKind {
    Sandbox,
    PipelineStage {
        /// Whether the body's value is forced once before it ships
        /// (`x | f = f !{x}`).  The serving child derives this from its
        /// own value-out edge: a stage holding a value-out channel feeds
        /// a value consumer and forces; a final or byte-mode stage holds
        /// none and leaves the value as-is.
        force_output: bool,
    },
}

/// Decoded response ready for either parent.
///
/// `value` is the body's return value (present iff the child packed one);
/// `signal` carries the helper-reported semantic outcome as a [`Break`]
/// (`Some` for `Error` / `Exit` / `Stopped`, `None` for success).  The
/// two are returned side by side with `audit_nodes` so a parent can
/// record audit before surfacing a failure.  `mobile` is `Some` only
/// when the child shipped one (`wants_mobile` and packable).
pub(crate) struct DecodedResponse {
    pub value: Option<Value>,
    pub last_status: i32,
    pub mobile: Option<Mobile>,
    pub audit_nodes: Vec<ExecNode>,
    pub surface_events: Vec<Value>,
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
/// Inverse of [`decode_response`].  `captured` is `Some` for a pipeline
/// stage closure env, `None` for the sandbox (mobile carries the env).
/// `sandbox_token` is `Some` only for the sandbox re-exec, carrying the
/// per-re-exec capability token that authenticates the confinement marker
/// in the child.
pub(crate) fn pack_request(
    body: Arc<Comp>,
    mobile: &Mobile,
    captured: Option<&Env>,
    audit_policy: Option<CapturePolicy>,
    wants_value: bool,
    wants_mobile: bool,
    sandbox_token: Option<String>,
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
        wants_mobile,
        sandbox_token,
    })
}

/// Output of evaluating one request in the child: the already-settled
/// body result (the local trampoline absorbed any terminal `Tail` —
/// it cannot cross the process boundary), the audit nodes drained after
/// eval, the buffered surfaced events, the post-run mobile, and the
/// `last_status` the parent installs.
struct EvalOutcome {
    result: Settled<Value>,
    audit_nodes: Vec<ExecNode>,
    surface_events: Vec<Value>,
    mobile: Mobile,
    last_status: i32,
}

/// The common spine plus the per-kind prelude: build the child shell,
/// install policy and a buffering surface sink, evaluate per [`ChildKind`],
/// drain audit, and read `last_status`.  The outer `Err` is a *pre-eval*
/// fault (arc rebuild / mobile hydration) — the body never ran — which
/// [`run_child_eval`] folds into a [`break_response`].
fn eval_request(
    request: ChildEvalRequest,
    upstream: Option<Value>,
    kind: ChildKind,
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
    // The parent's real surface sink is unreachable from this process, so
    // events are buffered and shipped in the response rather than streamed.
    let surface_buf: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    shell.turn.surface = Some(Arc::new({
        let buf = Arc::clone(&surface_buf);
        move |v| {
            if let Ok(mut events) = buf.lock() {
                events.push(v);
            }
        }
    }));
    crate::dbg_trace!(
        "child-eval",
        "pre-eval: audit.active={}",
        shell.local.audit.active(),
    );

    let (result, audit_nodes) = match kind {
        ChildKind::Sandbox => {
            // The child evaluates the body to a value it ships back and
            // absorbs any terminal tail call locally, so the body's tail
            // position has no observable effect across the boundary;
            // [`Tail::No`] is the honest default.
            let result = absorb_tail(eval_comp(&body, &mut shell, Tail::No), &mut shell);
            let audit_nodes = shell.local.audit.take_fragment().into_nodes();
            (result, audit_nodes)
        }
        ChildKind::PipelineStage { force_output } => {
            shell.turn.io.terminal = TerminalState::probe();
            shell.turn.io.job_control = crate::io::JobControl::pipeline_child();
            let captured = captured
                .ok_or_else(|| {
                    Break::Error(Error::new(
                        "pipeline stage request carried no captured env",
                        1,
                    ))
                })?
                .into_runtime(&arcs)?;
            let mut child = Shell::child_of(&captured, &mut shell);
            // Helper-local absorption point: a tail call cannot cross
            // the process boundary (the parent's callee/args wouldn't be
            // valid in this address space), so settle it here; the
            // stage's tail-ness has no effect across the boundary
            // ([`Tail::No`]).
            let result = absorb_tail(
                call::invoke(&body, upstream, Tail::No, &mut child),
                &mut child,
            );
            // `x | f = f !{x}`: a value-edge producer's output is forced
            // once before it ships, via the shared value-edge `!{x}`.
            let result = if force_output {
                result
                    .and_then(|value| crate::runtime::pipeline::force_pipe_value(value, &mut child))
            } else {
                result
            };
            // Drain the child's audit before `return_to` merges it into
            // the outer shell.
            let audit_nodes = child.local.audit.take_fragment().into_nodes();
            child.return_to(&mut shell);
            (result, audit_nodes)
        }
    };

    let surface_events = surface_buf
        .lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default();
    crate::dbg_trace!(
        "child-eval",
        "post-eval: result_ok={} audit_nodes={}",
        result.is_ok(),
        audit_nodes.len()
    );
    let mobile = shell.mobile();
    let last_status = mobile.control.last_status;
    Ok(EvalOutcome {
        result,
        audit_nodes,
        surface_events,
        mobile,
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

/// The one child runner.  Build the shell per [`ChildKind`], evaluate,
/// and pack one [`ChildEvalResponse`].  The returned `Option<Value>` is
/// the in-process value for the pipeline's value-out edge — independent
/// of `wants_value`, which only gates the value serialized into the
/// response.
///
/// Value and mobile serialization are each gated before serialization: a
/// non-transferable `Ok` value (with `wants_value`) becomes a structured
/// `WireOutcome::Error` carrying the hinted "cannot cross the process
/// boundary" message; a mobile pack failure (with `wants_mobile`)
/// downgrades a success outcome to that same error and leaves
/// `mobile: None`.
pub(crate) fn run_child_eval(
    request: ChildEvalRequest,
    upstream: Option<Value>,
    kind: ChildKind,
) -> (ChildEvalResponse, Option<Value>) {
    let wants_value = request.wants_value;
    let wants_mobile = request.wants_mobile;
    let outcome = match eval_request(request, upstream, kind) {
        Ok(outcome) => outcome,
        // Pre-eval fault: the body never ran.  No audit, no mobile, no
        // output value; the parent reads `last_status` off the response.
        Err(b) => return (break_response(b), None),
    };
    let EvalOutcome {
        result,
        audit_nodes,
        surface_events,
        mobile,
        last_status,
    } = outcome;

    let audit_nodes = match pack_audit_nodes(audit_nodes) {
        Ok(nodes) => nodes,
        Err(b) => return (break_response(b), None),
    };

    let mut ctx = InternCtx::new();
    // A surfaced event that fails to serialize is dropped, not fatal — these
    // are side-channel observations forwarded to the host's surface sink,
    // never part of the body's outcome.
    let surface_events: Vec<SerialValue> = surface_events
        .iter()
        .filter_map(|v| SerialValue::from_runtime(v, &mut ctx).ok())
        .collect();

    let (outcome, output_value) = match result {
        Ok(value) => {
            let packed = if wants_value {
                match SerialValue::from_runtime(&value, &mut ctx) {
                    Ok(serial) => Some(serial),
                    Err(e) => {
                        let outcome = break_to_outcome(Break::Error(transfer_error(e)));
                        return finish(
                            ctx,
                            outcome,
                            last_status,
                            None,
                            audit_nodes,
                            surface_events,
                            None,
                        );
                    }
                }
            } else {
                None
            };
            (WireOutcome::Ok(packed), Some(value))
        }
        Err(b) => (break_to_outcome(b), None),
    };

    let mobile = if wants_mobile {
        match WireMobile::from_mobile(&mobile, &mut ctx) {
            Ok(wire_mobile) => Some(wire_mobile),
            Err(e) => {
                // A non-transferable retained value made the post-run
                // mobile unpackable.  Downgrade a success to an error so
                // the parent does not install a half-mobile; leave
                // `mobile: None` and let the parent substitute a placeholder.
                if matches!(outcome, WireOutcome::Ok(_)) {
                    return finish(
                        ctx,
                        break_to_outcome(Break::Error(transfer_error(e))),
                        last_status,
                        None,
                        audit_nodes,
                        surface_events,
                        None,
                    );
                }
                return finish(
                    ctx,
                    outcome,
                    last_status,
                    None,
                    audit_nodes,
                    surface_events,
                    output_value,
                );
            }
        }
    } else {
        None
    };

    finish(
        ctx,
        outcome,
        last_status,
        mobile,
        audit_nodes,
        surface_events,
        output_value,
    )
}

/// Assemble the response from its fully-packed parts.
#[allow(clippy::too_many_arguments)]
fn finish(
    ctx: InternCtx,
    outcome: WireOutcome,
    last_status: i32,
    mobile: Option<WireMobile>,
    audit_nodes: Vec<WireAuditNode>,
    surface_events: Vec<SerialValue>,
    output_value: Option<Value>,
) -> (ChildEvalResponse, Option<Value>) {
    (
        ChildEvalResponse {
            scope_table: ctx.scope_table,
            outcome,
            last_status,
            mobile,
            audit_nodes,
            surface_events,
        },
        output_value,
    )
}

/// Build a response for a pre-eval failure: empty audit, no mobile, the
/// signal projected to the outcome.  Used by both preludes for failures
/// that fire before (or instead of) a real eval.
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
        mobile: None,
        audit_nodes: Vec::new(),
        surface_events: Vec::new(),
    }
}

/// Rehydrate a [`ChildEvalResponse`] into runtime values, audit nodes,
/// the post-run mobile, and the body's pass/fail signal, returning audit
/// and signal side by side so the parent can record audit before
/// surfacing a failure.
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
    let mobile = match response.mobile {
        Some(mobile) => Some(mobile.into_mobile(&arcs)?),
        None => None,
    };
    // A surfaced event that fails to rehydrate is dropped, not fatal — these
    // are side-channel observations forwarded to the host's surface sink,
    // never part of the body's outcome.
    let surface_events: Vec<Value> = response
        .surface_events
        .into_iter()
        .filter_map(|sv| sv.into_runtime(&arcs).ok())
        .collect();
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
        mobile,
        audit_nodes,
        surface_events,
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
            false,
            None,
        )
        .expect("pack")
    }

    #[test]
    fn stage_job_round_trip_applies_upstream_data_last() {
        let shell = Shell::default();
        let stage = compile_one("{ |x| return $[$x + 1] }");
        let request = pack_stage(stage, &shell, true);
        let (response, value) = run_child_eval(
            request,
            Some(Value::Int(41)),
            ChildKind::PipelineStage {
                force_output: false,
            },
        );
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
            mobile: None,
            audit_nodes: vec![WireAuditNode {
                scope_table: ctx.scope_table,
                node,
            }],
            surface_events: Vec::new(),
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
        // `unalias` filters on.  Hydration must preserve it, or a child
        // (a confined block, a pipeline helper) cannot `unalias` a name
        // the parent aliased — confined evaluation would diverge from
        // local.  This drives the exact hydration seam: install an alias,
        // pack the mobile through `WireMobile`, JSON-frame it as the IPC
        // codec does, rebuild the child shell, and confirm the alias is
        // both visible (`has_alias`) and removable (`remove_alias`).
        let mut parent = Shell::default();
        let thunk = eval_value("return { echo aliased }", &mut parent);
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
            wants_mobile: false,
            sandbox_token: None,
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
        // round-trips through the IPC codec as `CapabilityCheck`, never
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
        let (response, value) = run_child_eval(
            request,
            None,
            ChildKind::PipelineStage {
                force_output: false,
            },
        );
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
        let (response, value) = run_child_eval(
            request,
            None,
            ChildKind::PipelineStage {
                force_output: false,
            },
        );
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
            cmd: "dummy".into(),
            cancel: crate::process::CancelScope::default(),
        });
        let mut ctx = InternCtx::new();
        let err =
            transfer_error(SerialValue::from_runtime(&handle, &mut ctx).expect_err("must fail"));
        assert!(err.message.contains("cannot cross the process boundary"));
        assert!(err.message.contains("value"));
    }

    #[test]
    fn sandbox_shape_round_trip_returns_mutated_mobile() {
        // Sandbox shape: `captured: None`, `wants_mobile: true`, the mobile
        // already carries the lexical env, and the post-run mobile comes
        // back installed with the body's `last_status`.
        let mut shell = Shell::default();
        // Seed a binding the body reads, so the env-carrying mobile is
        // exercised end to end.
        eval_value("let base = 40", &mut shell);
        let body = compile_one("return $[$base + 2]");
        let request = pack_request(
            body,
            &shell.mobile,
            None,
            shell.local.audit.active_policy(),
            true,
            true,
            None,
        )
        .expect("pack");
        let (response, _) = run_child_eval(request, None, ChildKind::Sandbox);
        assert!(response.mobile.is_some(), "sandbox must return its mobile");
        let decoded = decode_response(response).expect("decode");
        assert_eq!(decoded.value, Some(Value::Int(42)));
        assert!(decoded.signal.is_none());
        assert!(decoded.mobile.is_some());
    }
}
