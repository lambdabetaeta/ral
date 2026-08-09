# Pipelines: bytes between stages, values at boundaries

**`|` is a byte conduit.** A pipeline connects the payload of one stage to the
input of the next, and every interior connection is `Bytes` on both sides. The
last stage may return an ordinary ral value; that value is the pipeline's final
result, not another interior edge.

- A stage's `result` is its payload. For every non-final stage it is `Bytes`.
- A following stage consumes that payload through `input = Bytes`.
- A stage's `output` is visible chatter. It is not the payload carried to the
  next stage.
- The final stage's result mode is free: `Bytes` gives the byte result, while
  `∅` gives the returned value.

Value composition is ordinary call-by-push-value composition. Use application
to pass a value to a function and `let`/`to` to bind a computation's result;
there is no pipeline-style combinator for values. A decoder can therefore end
a pipeline — `cat data.json | from-json` — while a value produced by that
decoder is composed by binding or application:

```ral
let document = cat data.json | from-json
length $document
```

**Every multi-stage pipeline is process-staged.** The checker establishes the
byte edges before evaluation, and the runtime launches the stages as one
process group:

- every stage, including ral-written stages, executes in a helper or external
  child;
- operating-system pipes carry every interior payload;
- the parent ral process is not a member of the stage group;
- the final value, when the last stage returns one, comes home in the
  `ChildEvalResponse` selected by `FinalValue::Report`.

The final-value report is deliberately helper-staged for now. Moving a
value-returning tail into the parent would change job control, failure
precedence, audit ordering, cancellation, input restoration, and capability
enforcement; it is a separate decision.

Out-of-process stages are subshells with respect to mutation: a helper stage's
`cd`, environment, alias, or module changes do not flow back to the parent.
Only pipe contents, the final result, and recorded observations cross the
boundary. This keeps terminal ownership coherent: a shell computation inside
its own foreground process group cannot both own the terminal and remain the
parent's session.

Failure is a separate axis. A pipeline propagates a stage's failure, but the
pipe never reacts to it: recovering from failure is `?`'s and `try`'s job, and
branching is on `Bool`, never on command success
([[design/failure|failure]]).

The terminal-handoff and process-containment machinery is transport detail, not
surface semantics. Unix uses process groups, a foreground guard, and helper
job-frame gates where a tty handoff must settle before user code runs; Windows
uses Job Objects and a creation-time launch path to close its handle-inheritance
window. The moving parts live in the [[map/core/runtime|runtime]]'s `pipeline/`
and [[map/core/io-process|process]] maps.

See also [[design/types|types]], [[design/cbpv|cbpv]],
[[design/codecs|codecs]], [[design/scoping|scoping]].

**Realised in** [[internals/pipeline-execution|pipeline-execution]].

Cite: RATIONALE §"Pipelines follow their edges", §"Failure is not truth",
§"Lexical data, dynamic authority"; `docs/SPEC.md` §4, §13, §20.4.
