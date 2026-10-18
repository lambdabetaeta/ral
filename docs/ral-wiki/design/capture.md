---
generated_at_commit: 6ca71142
---

# Capture: a command's value is its output

**A command's value is its output. Where a value is demanded of a command, the
command is captured and its output decoded as a `String`; the demand is found
by the type, not by the shape of the syntax.** `F` carries a grade
([[design/types|types]]): a command is `F^w Unit` (printed `Command`), and
`let x = hostname` binds the text `hostname` writes because the checker
resolves the right-hand side to `F^w Unit` and wraps it in the one coercion,
`cap M to d. decode d`
([[decisions/260930_graded-f|graded-f]]). So `let f = { hostname }; let x = f`
binds the host name, as does `let g = { |n| echo $n }; let x = g 5`
(`"5"`), because `f` and `g` have command types; abstraction and application no
longer hide the fact.

```ral
let answer = !{ echo first ; echo second }
# prints nothing; answer is "first\nsecond"
```

**Three places demand a value, and capture is placed at exactly those.**

- *Bind.* The whole right-hand side of a `let` (or `M to x. N`, or a named
  pattern) that resolves to `F^w Unit` is captured: `let x = M` is
  `let x = M | from-line` for every `M` that is a command — a block, a call, a
  variable holding a block, a pipeline, an `if` with command arms. A right-hand
  side whose grade is still a variable is defaulted to `p`: a `let` is a demand
  for a value, and this is the only defaulting in the system.
- *Argument.* In `apply_args_capped`, an argument whose type ends, after its own
  parameters, in `F^w Unit`, passed where the callee's parameter ends in
  `F^p β` at the same arity, sets `β := String` and is coerced: a literal block
  that writes every one of its arrows has its body wrapped under them; a block
  in hand, or a literal that does not — `{ !$f }` — is η-wrapped whole, so the
  argument still reaches the lambda the wrap would otherwise stand between. So
  `map { |f| echo $f } [1, 2]` is `["1", "2"]` and prints nothing.
- *Join.* An `F^w Unit` arm of an `if`/`case`/`try` whose join settles on `F^p`
  is wrapped like an argument ([[design/types|types]]): `try { fetch } { |e|
  return 'none' }` binds `fetch`'s output.

Anywhere else no coercion is inserted. A command flowing into a bare variable,
or a demand that resolves only later, is an ordinary mismatch of `Command`
against `Returns A`, and its hint names the fix.

**What does not capture.** A *tail* `Run` is never coerced: a turn's last
command prints and reports its value. `each`, `fold`, `fold-lines`, `spawn`,
`watch`, `service`, `audit` and `defer` absorb a command body: they run it,
stream its output and keep its value, which for a command is `()`. `warn` is
`Returns Unit`, since stderr is not the byte channel. A stdout redirect
*discharges* the grade, `M > f : F^p A`, so `let x = to-json 1 > f` binds `()`.
A `()`-producer where a command is demanded is a command by a zero-cost upcast
`F^p Unit ⇝ F^w Unit` (identity at run time); there is no path from
`F^p Unit` to `F^p String`, so `let x = f` with `f` returning `()` binds `()`.

**The whole computation is the buffer.** Capture wraps the whole right-hand
side, so nothing escapes to the terminal: `let x = !{ echo pre; echo host }`
binds `"pre\nhost"`. An inner `let` still captures for itself:

```ral
let y = !{ let z = echo a; echo b }   # prints nothing; y is "b" (z is "a")
```

A captured *stand-in* is the same rule: an arm for `curl` is a command, so
`curl: { |a| echo note; echo body }` under `let x = curl` binds `"note\nbody"`,
and a `()`-returning arm is admitted and captures `""`.

**Why the type, not a walk of the syntax.** "This computation's result is its
output" is a fact about a computation, and a walk of a right-hand side cannot
see it through abstraction, application or a variable, so β/η would not preserve
meaning. In the grade it passes through all three. The price is the rule's
placement: it is decided in checking mode, in program order
([[decisions/260930_graded-f|graded-f]] records the one order-dependence).

**The node returns bytes; the text is composed.** On success, `capture M : F Bytes`
returns precisely the bytes `M` wrote, nothing stripped and nothing decoded. Reading them as a `String` is a second step the checker composes over
it, `decode (capture M)`, and that step owns both things that can go wrong: one
trailing terminator is dropped, and output that is not valid UTF-8 fails there,
naming `| from-bytes` as the way to keep it. Each is its own term in the IR
(`CompKind::Capture`, `CompKind::Decode`), inserted by `annotate` and with no
surface syntax, so a step the checker writes into a program cannot be a name the
program's session resolves
([[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]]).
`InferCtx.captured` holds the computations a value demand captures, keyed by node
address, and `captured_vals` the values coerced in hand; `annotate` reads both.

**Exactness is kept by refusal.** The buffer behind a capture is capped at
16 MiB (`SINK_BUFFER_CAP`), and a bounded buffer is what keeps a
worker from growing without end ([[internals/output-capture-and-detachment|output-capture-and-detachment]]).
Past the cap it appends a truncation marker and drops the rest — bytes the
program could not tell from the command's own. So a capture that reaches the
cap *fails*: `Frame::Capture` reads the buffer's overflow flag
(`capture_overflowed`) once every writer has joined, and refuses rather than bind
a prefix. The error names the cap and asks whether a file was meant, and is
catchable by `try`. The human keeps the bytes; the binding does not happen.

**Failure flushes.** `Frame::Capture` pushes a fresh buffer as `shell.io.stdout`,
holding the sink it replaced. On return it restores that sink and yields the
buffer as `Bytes`; on a halt it writes what the body wrote to the sink it
replaced (`Shell::write_sink`). Its clause is `raise s ↦ write_out buf ; raise s`: an
accepted write preserves the body signal; a refused write pre-empts it. Bytes
delivered before refusal remain visible. Under `!{ … } > f` the file is that
sink, so the flush lands in `f`.

**The active CEK model has typed capture frames and a stateful flush boundary.**
`CEK.CaptureRouting` walks ordinary and generalisation frames to the nearest
buffer; its concatenation law preserves byte order, and stderr passes through.
Successful pop returns the exact buffer. A halt pops before flushing, so an
outer capture absorbs the bytes; only an external destination invokes the world.
Acceptance preserves the halt. Refusal exposes the delivered prefix and raises
the write error instead. Checked traces cover nested recovery, cleanup,
polymorphic bind, feed/environment/directory restoration and flush errors overriding both errors and
exits. `CEK.CaptureLaws` gives general command-success and halt-boundary rules;
contextual transfer remains open.
`CEK.Continuation` now lifts internal paths beneath any compatible typed
continuation, and `CEK.CaptureContinuation` proves that adding capture
preserves inner buffers and absorbs outward stdout. Internally terminating
bodies consequently return empty bytes or reach an empty-buffer halt flush.

A general bare-run comparison needs a delivery condition. A nested failure
can flush to the world before bare recovery, changing world state or replacing
the error. An enclosing capture absorbs that same flush. Checked CEK traces
exhibit the state difference for the same command response.

`CEK.BufferedWorld` supplies that accepting reference destination: command
execution is unchanged, but outward flushes accept fully without changing
world state; directory admission is unchanged too. `CEK.CaptureSimulation`
proves the forward direction for every finite reference-body execution,
including handler/base calls, environment/directory entry, refusal and
handler/scope restoration, without a
syntax coverage predicate. It preserves stderr, stdin and body world changes, while capture
accumulates reference stdout. Success returns the whole byte word. Halt uses
the actual outer flush response, including partial refusal and error
replacement. `CEK.CaptureReflection` reverses each captured step into a body
step, successful pop or outer flush. `CEK.CaptureCompleteness.capture-complete`
reconstructs a terminating reference-body run from every terminating capture
at the empty stack, retaining output, outcome, final configuration and the
actual flush response. Stderr agrees exactly. Both directions of finite
termination now check; contextual equivalence, finite-prefix characterisation
and divergence remain open.

**The frozen CK kernel proves the two halves for accepting body runs.** With unbounded
buffers, `capture-returns` retains exactly stdout and writes nothing outward.
`capture-halts-response` follows the outer write verdict: acceptance preserves
the body's signal and stdout (`capture-halts`); refusal reports the write error
and the delivered prefix. The acceptance premise concerns comparison with the
bare body at the empty stack: nested writes handled by capture cannot refuse.
Absorption by a capture buffer, pipe wire or redirect frame succeeds; file-world
refusal remains outside the model.

The independent finite-behaviour algebra groups each action's channel
traffic into one atomic event. `Core.Semantics.Process` closes sets of
behaviours under finite prefixes. The capture operation handles body writes
by an accepting buffer and permits refusal only at its outer flush, so its
characterisation and evaluation equation are unconditional. Its image needs
prefix closure too: the buffer can be flushed before an observation reports the halt. The
checked `FlushBoundary` counterexample rules out a direct-image account of
all capture observations. `CK.Machine.CaptureCompleteness.capture-characterisation`
proves exact agreement of finite views between `capture M` and the
prefix-closed capture operation on the body's process, for closed
`M : F Unit` at the empty stack. It preserves stdin, stderr and file events,
accounts for write-error precedence, and includes internal pipeline scheduling.

The term-directed interpretation has checked capture soundness with
explicit ambient scope. Its continuation argument tracks updates to enclosing
buffers, including nested failure flushes and handlers inside capture. Every
derived behaviour has a machine execution with exactly the same atomic events.
Exact reverse recovery at the empty stack preserves those action boundaries,
including empty traffic and internal flushes. Thus capture preserves term
completeness whenever its body is complete; captured external commands have
checked agreement in both directions. A complete self-masking handler also
retains agreement when captured. Completeness composes through sequential bind,
application, try and cleanup; a captured command sequence with failure
recovery and cleanup has checked agreement in both directions.

**Capture preserves ambient scope in the scoped fundamental theorem.**
`DelimitedCaptureReflection` preserves body reads and dispatch under equal-scope
boundaries while accounting for collected stdout. `DelimitedCaptureExecution`
recovers the body behaviour, including the failure-flush prefix, within the
captured execution's step budget. This gives capture closure alongside
higher-order CBPV, general recursion, recovery and cleanup in the covered
fragment. A valid environment overlay commutes with capture at the level of
whole machine processes for covered terms. A scoped external-command instance
checks success, failure and the prefix after flushing; its dispatch premise
is explicit. Arbitrary handler invocation, the remaining language rules,
contextual capture laws and Rust refinement remain open.

The capture is of the whole computation, so everything the body writes is
retained, a `guard`'s cleanup included: `let x = !{ guard { cmd } { echo clean } }`
binds `cmd`'s output followed by `clean`.

See also [[design/types|types]], [[design/cbpv|cbpv]],
[[design/pipelines|pipelines]], [[design/codecs|codecs]].

Cite: `docs/SPEC.md` §7.2.
