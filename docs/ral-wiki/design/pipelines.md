# Pipelines: positional byte wires, values at boundaries

**`|` connects the left stage's stdout to the right stage's stdin, and neither
endpoint must prove that it writes or reads.** Every interior edge is an
operating-system byte pipe, allocated from stage position alone. A non-final
stage's returned value is discarded; the final stage's
[[design/types|payload route]] decides what the pipeline as a whole reports.

```text
Γ ⊢ M : F[ρ] A       Γ ⊢ N : F[σ] B
────────────────────────────────────
          Γ ⊢ M | N : F[σ] B
```

Operationally: connect `stdout(M)` to `stdin(N)`, run the stages under the
process-group discipline below, discard `M`'s returned value, and take the
pipeline's route and value type from `N`.

**This is not a value pipe.** No returned `Bytes`, `String`, record, or other
value is ever serialised onto an interior edge. A returned value in non-final
position is simply unused, exactly as bytes written to an unread pipe are simply
unread. The symmetry is deliberate — a consumer need not read, a producer need
not write, and an empty stream is still a byte stream:

```ral
!{ return unit } | cat            # cat reads EOF
!{ echo hi; return unit } | cat   # cat reads "hi"; the Unit goes nowhere
cat f | from-bytes | grep x       # the returned Bytes is discarded; grep reads EOF
echo hi | !{ return 5 }           # the consumer ignores stdin; the pipeline returns 5
yes | !{ return 5 }               # terminates: the non-reader closes, the firehose gets EPIPE
```

**The one static rule about a stage constrains that stage alone.** A stage must
have shape `F[ρ] A` — a computation ready to run, not a function still waiting
for an argument. `echo hi | !{ |x| echo $x }` is a type error whose help says to
apply it rather than pipe into it. Nothing constrains a stage relative to its
neighbours.

That rule reads type formers, not spellings, so a stage that *returns* a thunk
is accepted: `cat f | { from-line }` typechecks, runs nothing, leaves `f`
unread, and discards the thunk. This footgun is admitted deliberately — a
syntax-directed rejection is not stable under naming the subterm, and rejecting
on the type needs a negative premise no sound decidable rule can state
([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).

Value composition is ordinary call-by-push-value composition: application passes
a value to a function, `let` / `to` binds a computation's result. A decoder
therefore ends a pipeline, and what it decodes is composed by binding:

```ral
let document = cat data.json | from-json
length $document
```

**Every multi-stage pipeline is process-staged.** The runtime launches the
stages as one process group:

- every stage, including ral-written stages, executes in a helper or external
  child;
- operating-system pipes carry every interior edge, all of them alike;
- the parent ral process is not a member of the stage group;
- the final value, when the final route is `Value`, comes home in the
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
§"Lexical data, dynamic authority"; `docs/SPEC.md` §7, §11, §17.4.
