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
unit } | cat` therefore writes `a` into `cat` by exactly this clause, with no
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

**Failure flushes.** If a captured computation fails, bytes it produced before
failing are flushed visibly rather than lost — a partial write from
`echo half; exit 3` stays on the terminal. That is handler semantics rather
than decoding, so it is the node's clause, not the composed step's.

See also [[design/types|types]], [[design/cbpv|cbpv]],
[[design/pipelines|pipelines]], [[design/codecs|codecs]].

Cite: `docs/SPEC.md` §20.4 ("Within a captured block, only the last command's
byte output becomes the block value"; "Captures nest. Earlier output goes to the
nearest enclosing visible stream").
