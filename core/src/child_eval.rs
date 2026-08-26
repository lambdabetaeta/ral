//! One wire shape and one child runner for the re-exec'd pipeline stage.
//!
//! A process-staged stage (`runtime/pipeline/`) packs a [`Comp`] body plus a
//! [`WireShell`] snapshot, re-execs, and gets one response back: outcome,
//! `last_status`, audit.  Strictly one frame each way — the child drains its
//! audit fragment after eval rather than streaming it live, so no per-node
//! frame loop exists.

use crate::evaluator::machine;
use crate::io::TerminalState;
use crate::ir::Comp;
use crate::serial::{
    InternCtx, ScopeTable, SerialEnvSnapshot, SerialValue, WireDecoder, is_handle, scrub,
};
use crate::source::{FileId, SourceDb, Span};
use crate::subprocess::{WireShell, bare_child_shell, install_shell_mobile};
use crate::types::{
    Break, CapturePolicy, Closure, Env, Error, Escape, Mooring, Observation, Settled, Shell,
    Status, Value,
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
    pub mobile: WireShell,
    /// Stage closure env: the child runs `body` as a closed machine over it
    /// (`evaluator::machine::evaluate`).
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

/// A forked shell's scope, wire-ready for `hatch` — the
/// [`ChildEvalRequest`] shape minus a body, since there is no stage to run:
/// the child engine that hydrates this one *becomes* the shell it seeds.
///
/// What it deliberately does not carry — `Value::Handle` bindings (scrubbed
/// upstream, at `Shell::fork_scrubbed`, the one place both an identity
/// fork and a wire seed pass through), terminal authority, the parent's
/// inbox or cancel token, its provider handle — is the parity argument for
/// shipping a seed at all: a fork and a seed must mean the same thing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EngineSeed {
    pub scope_table: ScopeTable,
    pub mobile: WireShell,
    pub captured: SerialEnvSnapshot,
    /// The spawn's validated base tag, meet-narrowed against the receiving
    /// engine's own ceiling once hydrated.
    pub grant: String,
}

/// Reify a forked shell into a wire-ready [`EngineSeed`] — `hatch`'s only
/// producer. `shell` is expected already scrubbed by `Shell::fork_scrubbed`;
/// this function trusts that law rather than re-checking it.
///
/// `hatch` is Linux-only, and `crate::hatch`'s own tests are its only other
/// caller, so a plain non-Linux, non-test build sees this as unreachable —
/// accurate, not a bug.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn pack_seed(shell: &Shell, grant: String) -> Settled<EngineSeed> {
    let mut ctx = InternCtx::new();
    let captured = SerialEnvSnapshot::from_runtime(&shell.env, &mut ctx);
    let mobile = WireShell::from_runtime(
        &shell.env,
        shell.last_status,
        shell.session.stack_limit,
        &shell.context,
        &mut ctx,
    )?;
    Ok(EngineSeed {
        scope_table: ctx.finish()?,
        mobile,
        captured,
        grant,
    })
}

/// Structured body outcome returned by the child.
///
/// `pgid` / `signal` cross as raw `i32` because `Pgid` / `Signal` derive no
/// serde impls; [`decode_response`] rebuilds the newtypes.  No tail call has
/// a wire variant — the child's machine settles fully before encoding.
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
    /// `pending` crosses as paths, not as open files: the staged temps live in
    /// the filesystem both sides share, and the parent's job table is what
    /// finishes them.
    #[cfg(unix)]
    Stopped {
        pgid: i32,
        signal: i32,
        cmd: String,
        pending: Vec<crate::PendingWrite>,
    },
}

/// Wire mirror of one [`Observation`].
///
/// `CommandOrigin`/`Decision`/`WriteOutcome` carry no serde impls of their
/// own, and a `Command`'s `value` or a `Capability`'s `fields` may nest a
/// native or closure that only decodes against a scope table — so rather
/// than mirror `Observed`'s variants field by field, this rides the same
/// canonical projection every other consumer reads
/// ([`Observation::to_value`]/[`Observation::from_value`]), interned as one
/// [`SerialValue`] exactly as a stage's report value is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireObservation {
    projected: SerialValue,
}

impl WireObservation {
    /// A `Handle` is scrubbed before `SerialValue::from_runtime` sees it, so
    /// one untransportable value cannot fail the whole fragment.  Closures
    /// stay: they intern against the fragment's own scope table and decode
    /// back live, which is this seam's advantage over a flat one.
    pub(crate) fn from_runtime(obs: &Observation, ctx: &mut InternCtx) -> Result<Self, Error> {
        Ok(Self {
            projected: SerialValue::from_runtime(&scrub(&obs.to_value(), &is_handle), ctx)?,
        })
    }

    pub(crate) fn into_runtime(self, dec: &WireDecoder) -> Result<Observation, Error> {
        let value = self.projected.into_runtime(dec)?;
        Observation::from_value(&value)
            .ok_or_else(|| Error::new("audit observation did not decode off the wire", 1))
    }
}

/// One observation plus the scope table it interns against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireAuditObservation {
    pub scope_table: ScopeTable,
    pub observation: WireObservation,
}

/// Full response emitted by one child.  `audit_observations` survives a
/// semantic failure, so work recorded before the failure still reaches the
/// parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChildEvalResponse {
    pub scope_table: ScopeTable,
    pub outcome: WireOutcome,
    pub last_status: i32,
    pub audit_observations: Vec<WireAuditObservation>,
}

/// Decoded response ready for the pipeline-stage parent.  `signal` is the
/// body's own outcome ([`None`] on success), returned beside
/// `audit_observations` so the parent records audit before surfacing a
/// failure.
pub(crate) struct DecodedResponse {
    pub value: Option<Value>,
    pub last_status: i32,
    pub audit_observations: Vec<Observation>,
    pub signal: Option<Break>,
}

/// Re-phrase a value-serialization failure as a process-boundary error,
/// keeping the value's own hint when it has one (a `Handle`'s already points
/// at `await`).  Shared with the stage report boundary in
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

/// Reify `shell`'s mobile state and a body into a wire-ready
/// [`ChildEvalRequest`].  `span` is the stage body's own, resolved against
/// `shell.sources()` so the child sees the same source under the same
/// [`FileId`].
pub(crate) fn pack_request(
    body: Arc<Comp>,
    shell: &Shell,
    captured: Option<&Env>,
    audit_policy: Option<CapturePolicy>,
    wants_value: bool,
    span: Option<Span>,
) -> Settled<ChildEvalRequest> {
    let mut ctx = InternCtx::new();
    let captured = captured.map(|env| SerialEnvSnapshot::from_runtime(env, &mut ctx));
    let mobile = WireShell::from_runtime(
        &shell.env,
        shell.last_status,
        shell.session.stack_limit,
        &shell.context,
        &mut ctx,
    )?;
    Ok(ChildEvalRequest {
        scope_table: ctx.finish()?,
        body,
        mobile,
        captured,
        audit_policy,
        wants_value,
        script: WireScriptContext::capture(span, &shell.session.sources),
    })
}

/// One request evaluated in the child.  `result` is already settled: the
/// machine has no tail call that could escape, so nothing crosses the
/// process boundary but a value or a halt.
struct EvalOutcome {
    result: Settled<Value>,
    audit_observations: Vec<Observation>,
    last_status: i32,
}

/// Evaluate one stage in a freshly hydrated child shell.  The stage's value
/// ships as evaluated: it is the pipeline's own result, so forcing it here
/// would run a suspension the program never forced and contradict the type
/// the checker read off the final stage.  An outer `Err` is a *pre-eval*
/// fault — arc rebuild or mobile hydration, the body never ran — which
/// [`run_child_eval`] folds into a [`break_response`].
fn eval_request(request: ChildEvalRequest, prelude: &crate::boot::BakedPrelude) -> Settled<EvalOutcome> {
    let ChildEvalRequest {
        scope_table,
        body,
        mobile,
        captured,
        audit_policy,
        script,
        ..
    } = request;

    let mut shell = bare_child_shell(prelude);
    let dec = WireDecoder::for_shell(&shell, &scope_table)?;
    install_shell_mobile(mobile, &mut shell, &dec)?;
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
        .into_runtime(&dec)?;
    let mut child = Shell::child_of(&captured, &mut shell);
    let closure = Closure { comp: body, env: captured };
    let result = machine::evaluate(closure, &mooring, &mut child);
    // Before `return_to`, which would merge the fragment into the outer shell.
    let audit_observations = child.local.audit.take_fragment().into_observations();
    child.return_to(&mut shell);

    crate::dbg_trace!(
        "child-eval",
        "post-eval: result_ok={} audit_observations={}",
        result.is_ok(),
        audit_observations.len()
    );
    let last_status = shell.last_status;
    Ok(EvalOutcome {
        result,
        audit_observations,
        last_status,
    })
}

/// Each observation interns against its own scope table, independent of the
/// response's.
fn pack_audit_observations(observations: Vec<Observation>) -> Settled<Vec<WireAuditObservation>> {
    let mut out = Vec::with_capacity(observations.len());
    for obs in observations {
        let mut ctx = InternCtx::new();
        let observation = WireObservation::from_runtime(&obs, &mut ctx)?;
        out.push(WireAuditObservation {
            scope_table: ctx.finish()?,
            observation,
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
        Break::Escape(Escape::Stopped {
            pgid,
            signal,
            cmd,
            pending,
        }) => WireOutcome::Stopped {
            pgid: pgid.as_raw(),
            signal: signal.number(),
            cmd,
            pending,
        },
    }
}

/// The one child runner.  A value that `wants_value` asked for but cannot be
/// serialized becomes a [`WireOutcome::Error`], not a transport fault.
pub(crate) fn run_child_eval(
    request: ChildEvalRequest,
    prelude: &crate::boot::BakedPrelude,
) -> ChildEvalResponse {
    let wants_value = request.wants_value;
    let outcome = match eval_request(request, prelude) {
        Ok(outcome) => outcome,
        // The body never ran: no audit, no output value.
        Err(b) => return break_response(b),
    };
    let EvalOutcome {
        result,
        audit_observations,
        last_status,
    } = outcome;

    let audit_observations = match pack_audit_observations(audit_observations) {
        Ok(observations) => observations,
        Err(b) => return break_response(b),
    };

    let mut ctx = InternCtx::new();

    let outcome = match result {
        Ok(value) => {
            let packed = if wants_value {
                match SerialValue::from_runtime(&value, &mut ctx) {
                    Ok(serial) => Some(serial),
                    Err(e) => {
                        let outcome = break_to_outcome(Break::Error(transfer_error(&e)));
                        return finish(ctx, outcome, last_status, audit_observations);
                    }
                }
            } else {
                None
            };
            WireOutcome::Ok(packed)
        }
        Err(b) => break_to_outcome(b),
    };

    finish(ctx, outcome, last_status, audit_observations)
}

/// Assemble the response from its fully-packed parts.  A scope table that
/// fails to encode voids the outcome the same way an unserialisable value
/// does: the response carries the transfer error and no value crosses.
fn finish(
    ctx: InternCtx,
    outcome: WireOutcome,
    last_status: i32,
    audit_observations: Vec<WireAuditObservation>,
) -> ChildEvalResponse {
    let (scope_table, outcome) = match ctx.finish() {
        Ok(table) => (table, outcome),
        Err(e) => (
            ScopeTable::default(),
            break_to_outcome(Break::Error(transfer_error(&e))),
        ),
    };
    ChildEvalResponse {
        scope_table,
        outcome,
        last_status,
        audit_observations,
    }
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
        audit_observations: Vec::new(),
    }
}

/// Rehydrate a child's response for its parent.  An outer `Err` is a *decode
/// fault* — the payload would not turn back into runtime values — as distinct
/// from `signal`, the body's own outcome, which crossed the wire intact.
pub(crate) fn decode_response(
    response: ChildEvalResponse,
    shell: &Shell,
) -> Settled<DecodedResponse> {
    let mut audit_observations = Vec::with_capacity(response.audit_observations.len());
    for entry in response.audit_observations {
        let dec = WireDecoder::for_shell(shell, &entry.scope_table)?;
        audit_observations.push(entry.observation.into_runtime(&dec)?);
    }
    let dec = WireDecoder::for_shell(shell, &response.scope_table)?;
    let (value, signal) = match response.outcome {
        WireOutcome::Ok(value) => {
            let value = match value {
                Some(value) => Some(value.into_runtime(&dec)?),
                None => None,
            };
            (value, None)
        }
        WireOutcome::Exit { code } => (None, Some(Break::Escape(Escape::Exit(code)))),
        #[cfg(unix)]
        WireOutcome::Stopped {
            pgid,
            signal,
            cmd,
            pending,
        } => (
            None,
            Some(Break::Escape(Escape::Stopped {
                pgid: crate::process::Pgid::from_raw(pgid)
                    .expect("a stopped child's pgid is positive"),
                signal: crate::process::Signal::new(signal),
                cmd,
                pending,
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
        audit_observations,
        signal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot::BakedPrelude;
    use crate::{Shell, elaborate, parse};
    use std::sync::OnceLock;

    fn prelude() -> &'static BakedPrelude {
        static P: OnceLock<BakedPrelude> = OnceLock::new();
        P.get_or_init(BakedPrelude::bake_runtime)
    }

    fn compile_one(source: &str) -> Arc<Comp> {
        let ast = parse(source).expect("parse");
        let top = elaborate(&ast, std::collections::HashSet::default(), "").expect("elaborate");
        let [phrase] = top.phrases.as_slice() else {
            panic!("expected one phrase, got {:?}", top.phrases);
        };
        let crate::ir::Phrase::Run(comp) = &phrase.item else {
            panic!("expected a Run phrase, got {:?}", phrase.item);
        };
        comp.clone()
    }

    fn eval_value(source: &str, shell: &mut Shell) -> Value {
        let closure = Closure { comp: compile_one(source), env: shell.env.clone() };
        machine::evaluate(closure, &Mooring::adrift(), shell).expect("eval")
    }

    /// A stage request from a freshly captured snapshot.
    fn pack_stage(stage: Arc<Comp>, shell: &Shell, wants_value: bool) -> ChildEvalRequest {
        let captured = Arc::new(shell.env.clone());
        let span = stage.span;
        pack_request(
            stage,
            shell,
            Some(&captured),
            shell.local.audit.active_policy(),
            wants_value,
            span,
        )
        .expect("pack")
    }

    #[test]
    fn stage_report_carries_audit_even_on_helper_error() {
        // A response carrying both audit and a failure must surface both, so
        // the user sees what ran before things went wrong.
        let mut ctx = InternCtx::new();
        let obs = Observation::spanning(
            crate::types::CallSite {
                script: "<test>".into(),
                line: 1,
                col: 1,
            },
            0,
            0,
            None,
            crate::types::Observed::Command {
                argv: vec!["/bin/echo".into(), "hi".into()],
                status: 0,
                origin: crate::types::CommandOrigin::External,
                io: crate::types::AuditIo::default(),
                error: None,
                value: Value::Unit,
            },
        );
        let observation = WireObservation::from_runtime(&obs, &mut ctx).expect("wire");
        let response = ChildEvalResponse {
            scope_table: ScopeTable::default(),
            outcome: WireOutcome::Error {
                message: "helper failed".into(),
                status: 1,
                hint: None,
                span: None,
            },
            last_status: 1,
            audit_observations: vec![WireAuditObservation {
                scope_table: ctx.finish().expect("finish"),
                observation,
            }],
        };
        let decoded = decode_response(response, &Shell::default()).expect("decode");
        assert!(
            matches!(decoded.signal, Some(Break::Error(ref e)) if e.message == "helper failed"),
            "expected structured error in signal; got {:?}",
            decoded.signal
        );
        assert_eq!(
            decoded.audit_observations.len(),
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

        let mut ctx = InternCtx::new();
        let wire = WireShell::from_runtime(
            &parent.env,
            parent.last_status,
            parent.session.stack_limit,
            &parent.context,
            &mut ctx,
        )
        .expect("to wire");
        let request = ChildEvalRequest {
            scope_table: ctx.finish().expect("finish"),
            body: compile_one("return ()"),
            mobile: wire,
            captured: None,
            audit_policy: None,
            wants_value: false,
            script: None,
        };

        // Cross the actual codec, not just the wire types.
        let json = serde_json::to_vec(&request).expect("serialise request");
        let request: ChildEvalRequest = serde_json::from_slice(&json).expect("deserialise request");

        let mut child = bare_child_shell(prelude());
        let dec = WireDecoder::for_shell(&child, &request.scope_table).expect("decoder");
        install_shell_mobile(request.mobile, &mut child, &dec).expect("install mobile");
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
    fn wire_observation_kind_survives_a_json_round_trip() {
        // A capability check's `kind` and `decision` cross intact, so a
        // denial does not silently decode back as some other observation.
        let mut ctx = InternCtx::new();
        let obs = Observation::instant(
            crate::types::CallSite {
                script: "<test>".into(),
                line: 1,
                col: 1,
            },
            None,
            crate::types::Observed::Capability {
                resource: "net".into(),
                decision: crate::types::Decision::Denied,
                fields: crate::types::Map::default(),
            },
        );
        let wire = WireObservation::from_runtime(&obs, &mut ctx).expect("wire");

        let json = serde_json::to_vec(&wire).expect("serialise observation");
        let back: WireObservation = serde_json::from_slice(&json).expect("deserialise observation");
        let table = ctx.finish().expect("finish");
        let dec = WireDecoder::for_shell(&Shell::default(), &table).expect("decoder");
        let runtime = back.into_runtime(&dec).expect("into runtime");
        assert_eq!(runtime.what.kind(), "capability-check");
        assert!(
            matches!(
                runtime.what,
                crate::types::Observed::Capability {
                    decision: crate::types::Decision::Denied,
                    ..
                }
            ),
            "the observation's decision must survive the wire round-trip"
        );
    }

    /// A `Handle`-bearing observation ships scrubbed rather than
    /// failing the whole fragment — the bug `pack_audit_observations`
    /// propagates as a `break_response` when any one observation cannot be
    /// interned.
    #[test]
    fn wire_observation_scrubs_a_handle_instead_of_dying() {
        use std::sync::Mutex;
        let handle = Value::Handle(Box::new(crate::types::HandleInner {
            result: Arc::new(Mutex::new(None)),
            cached: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(crate::types::HandleState::Running)),
            stdout_buf: crate::io::ByteBuffer::default(),
            stderr_buf: crate::io::ByteBuffer::default(),
            surface_buf: Arc::new(Mutex::new(Vec::new())),
            joined: Arc::new(Mutex::new(false)),
            last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
            cmd: "<test>".into(),
            cancel: crate::process::CancelScope::default(),
        }));
        let obs = Observation::instant(
            crate::types::CallSite {
                script: "<test>".into(),
                line: 1,
                col: 1,
            },
            None,
            crate::types::Observed::Command {
                argv: vec!["spawn".into()],
                status: 0,
                origin: crate::types::CommandOrigin::Builtin,
                io: crate::types::AuditIo::default(),
                error: None,
                value: handle,
            },
        );

        let mut ctx = InternCtx::new();
        let wire = WireObservation::from_runtime(&obs, &mut ctx).expect("must not die on a handle");

        let table = ctx.finish().expect("finish");
        let dec = WireDecoder::for_shell(&Shell::default(), &table).expect("decoder");
        let runtime = wire.into_runtime(&dec).expect("into runtime");
        let crate::types::Observed::Command { value, .. } = runtime.what else {
            panic!("expected a command")
        };
        assert!(
            matches!(value, Value::Variant { ref label, .. } if label == "opaque"),
            "the handle must cross as its opaque placeholder, got {value:?}"
        );
    }

    /// A closure reachable through an observation still interns against the
    /// fragment's own scope table and decodes back live — the seam's
    /// advantage over the flat wire, which opaques closures too.
    #[test]
    fn wire_observation_keeps_a_closure_rich() {
        let mut shell = Shell::default();
        let thunk = eval_value("{ |x| return $x }", &mut shell);
        let obs = Observation::instant(
            crate::types::CallSite {
                script: "<test>".into(),
                line: 1,
                col: 1,
            },
            None,
            crate::types::Observed::Command {
                argv: vec!["alias".into()],
                status: 0,
                origin: crate::types::CommandOrigin::Builtin,
                io: crate::types::AuditIo::default(),
                error: None,
                value: thunk,
            },
        );

        let mut ctx = InternCtx::new();
        let wire = WireObservation::from_runtime(&obs, &mut ctx).expect("wire");

        let table = ctx.finish().expect("finish");
        let dec = WireDecoder::for_shell(&shell, &table).expect("decoder");
        let runtime = wire.into_runtime(&dec).expect("into runtime");
        let crate::types::Observed::Command { value, .. } = runtime.what else {
            panic!("expected a command")
        };
        assert!(
            matches!(&value, Value::Thunk(c) if c.comp.arrow().is_some()),
            "the closure must decode back live, got {value:?}"
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
        let response = run_child_eval(request, prelude());
        assert!(
            matches!(response.outcome, WireOutcome::Ok(None)),
            "response value should be skipped"
        );
    }

    #[test]
    fn stage_job_round_trip_preserves_alias_binding() {
        let mut shell = Shell::default();
        let thunk = eval_value("{ |args| echo ok; return $[$args[0] * 2] }", &mut shell);
        shell.install_alias("twice".into(), thunk).unwrap();

        let stage = compile_one("twice 21");
        let request = pack_stage(stage, &shell, true);
        let response = run_child_eval(request, prelude());
        let _ = decode_response(response, &shell).expect("decode");
    }

    #[test]
    fn transfer_error_phrases_non_transferable_values_for_the_boundary() {
        // A `Handle` fails the gate before the response is built, and reads
        // as a boundary error rather than a transport fault.
        use std::sync::Mutex;
        let handle = Value::Handle(Box::new(crate::types::HandleInner {
            result: Arc::new(Mutex::new(None)),
            cached: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(crate::types::HandleState::Completed)),
            stdout_buf: crate::io::ByteBuffer::default(),
            stderr_buf: crate::io::ByteBuffer::default(),
            surface_buf: Arc::new(Mutex::new(Vec::new())),
            joined: Arc::new(Mutex::new(false)),
            last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
            cmd: "dummy".into(),
            cancel: crate::process::CancelScope::default(),
        }));
        let mut ctx = InternCtx::new();
        let err =
            transfer_error(&SerialValue::from_runtime(&handle, &mut ctx).expect_err("must fail"));
        assert!(err.message.contains("cannot cross the process boundary"));
        assert!(err.message.contains("value"));
    }

    /// A captured (non-core) native riding home in a stage's report value
    /// must re-link against the receiving shell's own manifest.
    #[test]
    fn captured_native_decodes_against_the_receiving_shells_manifest() {
        #[allow(
            clippy::unnecessary_wraps,
            reason = "registered as a `BuiltinBody::Static` fn pointer; `Settled<Value>` is the shape the builtin table dispatches through"
        )]
        fn body_stub(
            _args: &[Value],
            _mooring: &Mooring,
            _shell: &mut Shell,
        ) -> crate::types::Settled<Value> {
            Ok(Value::Unit)
        }
        fn scheme_stub(_u: &mut crate::typecheck::Unifier) -> crate::typecheck::Scheme {
            use crate::typecheck::builtins::{mk_scheme, pure, thunk};
            mk_scheme(&[], &[], &[], thunk(pure(crate::typecheck::Ty::Unit)))
        }
        let captured: Arc<[crate::types::BuiltinEntry]> =
            Arc::from(vec![crate::types::BuiltinEntry::new(
                std::borrow::Cow::Borrowed("test-captured-native"),
                scheme_stub,
                "test-only captured native.",
                crate::types::BuiltinBody::Static(body_stub),
            )]);
        let mut shell = Shell::default();
        shell.install_captured_builtins(&captured);

        let response = ChildEvalResponse {
            scope_table: ScopeTable::default(),
            outcome: WireOutcome::Ok(Some(SerialValue::Ext(crate::serial::SerialClosure::Native(
                crate::serial::SerialNative {
                    name: "test-captured-native".to_string(),
                    applied: Vec::new(),
                },
            )))),
            last_status: 0,
            audit_observations: Vec::new(),
        };

        let decoded = decode_response(response, &shell)
            .expect("a captured native must decode against the receiving shell's own manifest");
        match decoded.value {
            Some(Value::Native { entry, .. }) => {
                assert_eq!(entry.name.as_ref(), "test-captured-native");
            }
            other => panic!("expected a decoded native, got {other:?}"),
        }
    }

    /// The one snapshot law: an identity fork and a wire-seeded child, both
    /// read out of the same nursery slot, resolve every name to the same
    /// value — and the same absence — because `fork_into_nursery` scrubs
    /// `Value::Handle` bindings before either arm ever sees the scope.
    #[test]
    fn identity_fork_and_wire_seed_agree_on_the_scrubbed_scope() {
        use crate::types::{Fork, Mooring, Nursery};
        use std::sync::Mutex;

        let mut parent = Shell::default();
        parent.set_var("kept".to_string(), Value::Int(7));
        parent.set_var(
            "live".to_string(),
            Value::Handle(Box::new(crate::types::HandleInner {
                result: Arc::new(Mutex::new(None)),
                cached: Arc::new(Mutex::new(None)),
                state: Arc::new(Mutex::new(crate::types::HandleState::Running)),
                stdout_buf: crate::io::ByteBuffer::default(),
                stderr_buf: crate::io::ByteBuffer::default(),
                surface_buf: Arc::new(Mutex::new(Vec::new())),
                joined: Arc::new(Mutex::new(false)),
                last_observed: Arc::new(Mutex::new(std::time::Instant::now())),
                cmd: "<test>".into(),
                cancel: crate::process::CancelScope::default(),
            })),
        );

        let nursery = Nursery::default();
        let mooring = Mooring {
            fork: Some(Fork::Park(nursery.clone())),
            ..Mooring::adrift()
        };

        // Arm A: identity — adopt straight out of the nursery.
        let id_a = parent
            .fork_into_nursery(&mooring)
            .expect("a nursery is installed");
        let identity_child = nursery.adopt(id_a).expect("adopt the parked fork");

        // Arm B: wire — pack the (separately parked, equally scrubbed) fork
        // into an `EngineSeed` and hydrate a fresh shell from it, exactly as
        // `hatch::apply_seed` does.
        let id_b = parent
            .fork_into_nursery(&mooring)
            .expect("a nursery is installed");
        let nursery_shell = nursery.adopt(id_b).expect("adopt the parked fork");
        let seed = pack_seed(&nursery_shell, "confined".to_string()).expect("pack seed");
        let mut wire_child = bare_child_shell(prelude());
        let dec = WireDecoder::for_shell(&wire_child, &seed.scope_table).expect("decoder");
        install_shell_mobile(seed.mobile, &mut wire_child, &dec).expect("install mobile");
        wire_child.env = seed.captured.into_runtime(&dec).expect("decode captured");

        assert_eq!(
            identity_child.env.get("kept"),
            wire_child.env.get("kept"),
            "both arms must resolve a kept binding to the same value"
        );
        assert_eq!(
            identity_child.env.get("absent"),
            wire_child.env.get("absent"),
            "both arms must agree on the same absence"
        );
        let opaque = |v: Option<&Value>| matches!(v, Some(Value::Variant { label, .. }) if label == crate::serial::OPAQUE_TAG);
        assert!(
            opaque(identity_child.env.get("live")),
            "an identity fork must scrub a handle-carrying binding"
        );
        assert!(
            opaque(wire_child.env.get("live")),
            "a wire seed must scrub the same binding the same way"
        );
    }
}
