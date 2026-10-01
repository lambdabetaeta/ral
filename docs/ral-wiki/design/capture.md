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

**The node returns bytes; the text is composed.** `capture M : F Bytes` is
total and exact — precisely the bytes `M` wrote, nothing stripped and nothing
decoded. Reading them as a `String` is a second step the checker composes over
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

The capture is of the whole computation, so everything the body writes is
retained, a `guard`'s cleanup included: `let x = !{ guard { cmd } { echo clean } }`
binds `cmd`'s output followed by `clean`.

See also [[design/types|types]], [[design/cbpv|cbpv]],
[[design/pipelines|pipelines]], [[design/codecs|codecs]].

Cite: `docs/SPEC.md` §7.2.
