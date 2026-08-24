# Capture: a command's value is its stdout

**One rule, in three clauses.** A command's value *is* its stdout: an external
call is typed `F[Bytes] Unit`, its [[design/types|payload route]] sending a
value boundary to the byte stream rather than to the returned value — one
payload, never both ([[design/types|WF-2]]). A block's value is its *last*
statement's value; every earlier statement stands in **statement position**,
where a value is discarded. And discarding bytes means letting them go where a
command's bytes go by default: out.

```ral
let answer = !{ echo visible ; echo captured }
# prints "visible"; answer is "captured"
```

Nothing here is special-cased for bytes. A discarded `$[1 + 1]` in statement
position is discarded too. The only difference is what discarding *means* for a
byte stream rather than for a value, and for stdout it means the terminal.

**Why not slurp the whole block.** `$(setup; work)` in a POSIX shell glues
setup's noise onto the result, which is why shell scripts are littered with
`2>/dev/null` and why reading a chatty tool is a research project. Ral needs no
annotation for it: diagnostics reach the terminal, the payload reaches the
binding, and the statement boundary does the work a redirect would otherwise do
by hand. This is the [[design/types|one coercion]] earning its keep — `capture`
retains the *tail*, not the transcript.

**Discarded bytes leave the whole capture chain.** Not one level of it. The
destination is the nearest enclosing **visible** stream, and a capture buffer is
not visible: a discarded statement's bytes pass every enclosing capture and
reach the terminal. Inside a [[design/pipelines|pipeline]] "visible" is
stage-relative — a non-final stage's own stdout *is* the wire, and a stage runs
in a fresh child shell with no enclosing capture at all, so a flush there
bottoms out at the wire rather than escaping the pipeline. `!{ echo a; return
() } | cat` therefore writes `a` into `cat` by exactly this clause, with no
pipeline rule involved.

This is the load-bearing clause, and the one an implementation can get subtly
wrong: routing a discarded write to the *immediately* enclosing sink agrees with
the rule at one level of capture and diverges at two, where the immediately
enclosing sink is another buffer.

**Two mechanisms realise the one rule.** Which one applies depends on whether
the elaborator can see the sequence.

- *Static.* `Capture` insertion is a demand walk that pushes the wrap down to
  the leaf that owns the payload. A sequence's non-final parts are walked at
  `Discard` and never wrapped, so they execute against the ambient stdout
  directly — no flush is involved at all, because those statements were never
  inside a capture bracket.
- *Dynamic.* Across a thunk boundary the walk cannot see the sequence: a forced
  block is opaque, so the wrap lands on the whole force and the sequence really
  does run inside the bracket. The runtime then flushes at each boundary to the
  sink saved when the bracket was entered.

The static path is the common one and is correct by construction. The dynamic
path is the one that must name the *visible* sink rather than the enclosing one.

**Why `;` is not a bind.** A bind at a byte payload installs the buffer — that
is what having a bound variable to decode into means — so `M to (ignore x. N)`
*destroys* `M`'s bytes. A statement boundary installs no buffer and lets them
out. The two have the same type: a payload route says where a value boundary
looks, never whether anything was written, so `F[Value] Unit` is honest about a
computation that writes and about one that could but doesn't. Only the
evaluator distinguishes them, which is why the boundary has to be its own former
rather than sugar for a bind on a dropped variable.

**The node returns bytes; the text is composed.** `capture M : F[Value] Bytes`
is total and exact — precisely the bytes the handler collected, nothing
stripped and nothing decoded. Reading them as the `String` a value boundary
wants is a second step the checker composes over it, `decode (capture M)`, and
that step owns both things that can go wrong: one trailing terminator is
dropped, and output that is not valid UTF-8 fails there, naming `| from-bytes`
as the way to keep it. Each is its own term in the IR, and each is syntax: a
step the checker writes into a program cannot be a name the program's session
resolves ([[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]],
[[design/types|types]]).

**Exactness is kept by refusal.** The buffer behind a capture is capped at
16 MiB (`SINK_BUFFER_CAP`), and a bounded buffer is what keeps a detached
worker from growing without end ([[internals/output-capture-and-detachment|output-capture-and-detachment]]).
Past the cap it appends a truncation marker and drops the rest — bytes the
program could not tell from the command's own. So a capture that reaches the
cap *fails*: `eval_capture` reads the buffer's overflow flag once every writer
has joined, and refuses rather than bind a prefix. Nothing on the write path
can raise it — a pump hands back `()` from its own thread — which is why the
flag rides on the buffer and is read where the bytes become a value.

Nothing is destroyed by the refusal. It is a failure like any other, so the
prefix takes the road the next clause describes — out to the visible stream —
and the error, which names the cap and asks whether a file was meant, is
catchable by `try`. The human keeps the bytes; the binding does not happen.

**Failure flushes.** If a captured computation fails, bytes it produced before
failing are flushed visibly rather than lost — a partial write from
`echo half; exit 3` stays on the terminal. That is handler semantics rather
than decoding, so it is the node's clause, not the composed step's.

The kernel model proves the clause as of 2026-08-19. `βflush` in `dev/agda`
pops the capture frame and writes the buffer *as chatter*, so the flush goes
where a discarded statement's bytes go — past every enclosing capture, into the
nearest wire, or out. Writing it as a payload instead would feed one buffer into
the next, which is the nearest-sink reading this page warns against; nested
captures cascade, inner buffer first, each under its own remaining stack. The
theorem is `flush-payload`, the counterpart to the model's tail-scoping one: a
run that fails escapes whole, so a capture over it reads the same stdin, writes
the same bytes and reports the same failure as the body alone — nothing is
retained, because the payload the buffer was collecting is what the failure says
will not be delivered.

Where "visibly" points is the enclosing scope's business, and a sink redirect
moves it. Under `!{ … } > f` the file is the visible stream, so a flush inside
that scope lands in `f` and not on the terminal — the same entry that sends a
discarded statement's bytes there, since a redirect's frame takes a word under
either claim. The model runs that composite: the buffer fills, the flush hands
it outward as chatter, the redirect takes it, and the word appears in the file
event the pop fires rather than in what an observer saw.

Both theorems hold of bodies with no handler in them, and that premise is a real
limit rather than a modelling convenience.
`let x = !{ guard { cmd } { echo clean } }` buffers `cmd`'s payload, then runs
the cleanup, which prints `clean` — visibly, after the buffer already has
content — and then binds the payload. Standalone the two words appear in the
order `payload`, `clean`; captured, `clean` appears and the payload is kept. So
what a capture retains is a *suffix* of what the same body writes alone exactly
when no handler resumes the run mid-flight, and that is the shape both theorems
state. `dev/agda` records the two runs beside the theorems.

See also [[design/types|types]], [[design/cbpv|cbpv]],
[[design/pipelines|pipelines]], [[design/codecs|codecs]].

Cite: `docs/SPEC.md` §7.2 ("Within a captured block, only the last command's
byte output becomes the block value"; "Captures nest. Earlier output goes to the
nearest enclosing visible stream").
