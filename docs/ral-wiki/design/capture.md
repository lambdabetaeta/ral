# Capture: a `let` captures the command that produces its value

**A `let` captures the command that produces its value; a function, block or
handle in that position binds what it returns, and `| from-line` turns what it
writes into a value.** A command is `F Unit`: it writes and returns nothing
([[design/types|types]]). So `let x = hostname` binds the text `hostname`
writes, because the checker wraps that command in the one coercion, `cap M to d.
decode d`, and it decides so once, from syntax, before any type is inferred
([[decisions/260930_capture-is-decided-by-syntax|capture-is-decided-by-syntax]]).

```ral
let answer = !{ echo visible ; echo captured }
# prints "visible"; answer is "captured"
```

**The walk `⟦·⟧` (`capture_sites` in `core/src/typecheck/capture.rs`) follows
the positions a `let`'s result comes from.** From the right-hand side it
descends through:

- an `Exec` that writes (`head_writes`: no binding, and no value row unless a
  `Writes` row applied at its arity) — the site;
- a pipeline's *final* stage;
- `Force` of a literal thunk;
- a `Bind`'s `rest`, hoisted binds included, never its right-hand side;
- the arms of `If`, `Case`, `Try` (body and handler), `Within` (body), `Grant`
  (body) and `Guard` (body, not cleanup) when they are literal thunks — for a
  literal `{ |p| … }` arm, its body.

It stops at `Capture`, `Redirect`, `App`, `Force` of a name, values, `Index`,
`Interpolation` and `Audit`. So `let x = f` (a function), `let x = !$t`,
`let x = time { ls }`, `let x = !{ ls } > f` and `let x = a | !$f` bind what the
thing returns, usually `()`. A redirect fused onto a command stays on its
`Exec`: `let saved = echo hi > f` binds `""`, and `let x = cmd 2>&1` binds both
streams. `let _ = cmd` captures and drops.

**A discarded statement is never captured.** `M; N` leaves `M` uncaptured, so
its bytes go where a command's bytes go: to the run's stdout, which inside a
capture is that capture's buffer and otherwise the terminal. What a captured
block writes before its tail is therefore visible, and its tail is the value;
an inner `let` captures for itself:

```ral
let x = !{ echo a; echo b }          # prints a; x is "b"
let y = !{ let z = echo a; echo b }  # prints nothing; y is "b" (z is "a")
```

A captured *stand-in* is the exception that follows from the rule: an arm for
`curl` is a command, so every statement of it is captured, and
`curl: { |a| echo note; echo body }` under `let x = curl` binds `"note\nbody"`.

**Why not slurp the whole block.** `$(setup; work)` in a POSIX shell glues
setup's noise onto the result, which is why shell scripts are littered with
`2>/dev/null` and why reading a chatty tool is a research project. Ral needs no
annotation for it: diagnostics reach the terminal, the tail reaches the binding,
and the statement boundary does the work a redirect would otherwise do by hand.

**The node returns bytes; the text is composed.** `capture M : F Bytes` is
total and exact — precisely the bytes `M` wrote, nothing stripped and nothing
decoded, its body's value ignored. Reading them as a `String` is a second step
the checker composes over it, `decode (capture M)`, and that step owns both
things that can go wrong: one trailing terminator is dropped, and output that is
not valid UTF-8 fails there, naming `| from-bytes` as the way to keep it. Each
is its own term in the IR (`CompKind::Capture`, `CompKind::Decode`), inserted by
`annotate` and with no surface syntax, so a step the checker writes into a
program cannot be a name the program's session resolves
([[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]]).

**Exactness is kept by refusal.** The buffer behind a capture is capped at
16 MiB (`SINK_BUFFER_CAP`), and a bounded buffer is what keeps a detached
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
replaced (`Shell::write_sink`) and propagates the halt, so whatever a failing
captured command wrote before it failed stays where it would have gone. Under `!{ … } > f` the file is that
sink, so the flush lands in `f`.

**The kernel proves the two halves.** `dev/agda` states capture for the
route-free calculus: a terminating `M` has `capture M` return exactly the bytes
`M` wrote and write nothing itself (`capture-returns`); a halting one halts with
the same signal after writing what `M` wrote (`capture-halts`). The kernel
covers this ruling only — no kinds, rows, weak variables or boundaries.

What a capture retains is what `⟦·⟧` marks, and a cleanup is not marked:
`let x = !{ guard { cmd } { echo clean } }` binds `cmd`'s output, and `clean`
goes where a discarded statement's bytes go.

See also [[design/types|types]], [[design/cbpv|cbpv]],
[[design/pipelines|pipelines]], [[design/codecs|codecs]].

Cite: `docs/SPEC.md` §7.2.
