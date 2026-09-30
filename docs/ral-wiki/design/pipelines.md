# Pipelines: positional byte wires, values at boundaries

**`|` connects the left stage's stdout to the right stage's stdin: a stage
feeds the next by writing.** Every interior edge is an operating-system byte
pipe, allocated from stage position alone. Every stage but the last is a
command whose value is unused (`F^ε Unit`, any grade); the pipeline's value is
its final stage's.

```text
Γ ⊢ M : F^ε Unit       Γ ⊢ N : F^ε' B
──────────────────────────────────────
        Γ ⊢ M | N : F^ε' B
```

Operationally: connect `stdout(M)` to `stdin(N)`, run the stages under the
process-group discipline below, and take the pipeline's value from `N`.

**This is not a value pipe.** No returned `Bytes`, `String`, record, or other
value is ever serialised onto an interior edge; what crosses is what a stage
writes. A consumer need not read, and an empty stream is still a byte stream:

```ral
!{ return () } | cat              # cat reads EOF
!{ echo hi } | cat                # cat reads "hi"
echo hi | !{ return 5 }           # the consumer ignores stdin; the pipeline returns 5
yes | !{ return 5 }               # terminates: yes's next write finds its reader gone
```

**Three static rules, each about one stage.** A stage must have shape `F^ε A` — a
computation ready to run, not a function still waiting for an argument;
`echo hi | !{ |x| echo $x }` is a type error whose help says to apply it rather
than pipe into it. A stage before the last must write: it is accepted at `Unit` in any grade, but
a decoder (`from-*`, marked `BuiltinDiagnostic::Decoder`) is refused by its own
mark (`stage_decoder`, `DecoderMidPipeline`, T0078: "a decoder ends the byte
pipeline: `from-json` returns a value and writes nothing, so nothing reaches
`cat`", with the help to bind the value first), and a value or block literal in
stage position writes nothing to the pipe (`Reason::PipelineStageWrites`,
T0011). `each { … } $xs | cat` is accepted: `each` streams what its body writes.
And a stage after a `|` may not bind standard input at its
own root: `a | b < f` and `a | b << w` are refused, because the feed answers
every read `b` makes for the stage's whole run and leaves `a` writing for
nobody — a producer that, concurrently, blocks for nothing until its next
write finds its reader gone. Each rewrite keeps every command already written: drop the pipe, run the
producer as its own statement, or `spawn` it.

**The refusal reads the stage's root and nothing deeper**, which is the whole of
what the pipeline rule can see, and the whole of what answers a stage's reads
for its entire run. A read one level in — inside a block, or on one command
among several — supplies that command alone, is not statically dead, and stays
legal. A stage's own redirects never collide: a stream takes one binding, so
`from-string < /dev/null << #'won'#` is refused at parse
([[decisions/260930_redirects-are-bindings|redirects-are-bindings]]).

The final stage carries no such rule, so one that *returns* a thunk is
accepted: `cat f | { from-line }` typechecks, runs nothing, leaves `f` unread,
and is the thunk. This footgun is admitted deliberately — a syntax-directed
rejection is not stable under naming the subterm
([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).

Value composition is ordinary call-by-push-value composition: application passes
a value to a function, `let` / `to` binds a computation's result. A decoder
therefore ends a pipeline, and what it decodes is composed by binding:

```ral
let document = cat data.json | from-json
length $document
```

**A ral-written stage is a thread; only an external is a process.** Every
multi-stage pipeline shares one process group:

- a stage whose head resolves to ral runs its own CEK machine on an OS thread,
  over a cloned `Shell`, wired to its neighbours by ordinary kernel pipes;
- an external command is a process in the group, spawned directly when no
  redirect or byte-capturing audit rules that out, else spawned from inside
  the stage thread that hosts it;
- operating-system pipes carry every interior edge, all of them alike, whether
  the stage writing or reading one is a thread or a process;
- the parent ral process is not a member of the group; a stable anchor process
  holds the pgid joinable for the pipeline's whole life;
- the final value is simply the last stage's thread returning it — no frame,
  no wire, crosses back. A captured final stage (`let x = a | b`) is a
  `Capture` node, so it runs as a thread stage whose stdin is the pipe and
  whose buffer takes what `b` writes
  ([[decisions/260930_graded-f|graded-f]]).

Only an external is ever isolated by a process boundary. A stage thread's panic
is caught at the pipeline boundary and folded as that stage's own failure,
carrying the stage's span, its own stack having none to attribute it to; a Rust
stack overflow is not an unwind and still takes the whole shell.

A stage is a subshell with respect to mutation regardless of whether it is a
thread or a process: its `cd`, environment, alias, or module changes do not
flow back to the parent, since a thread's cloned `Shell` is simply dropped at
the stage's end. Only pipe contents, the final result, and recorded
observations cross the boundary. This keeps terminal ownership coherent: a
shell computation inside its own foreground process group cannot both own the
terminal and remain the parent's session — and a thread in the parent's own
process cannot read a terminal whose foreground belongs to the externals'
group either, which is why a ral-written stage 0 with nothing else to read
from sees EOF on an interactive terminal
(`!{ from-line } | cat` at the prompt) rather than falling through to the
controlling tty.

Failure is a separate axis. A pipeline propagates a stage's failure, but the
pipe never reacts to it: recovering from failure is `?`'s and `try`'s job, and
branching is on `Bool`, never on command success
([[design/failure|failure]]). **The stage-feed refusal above is static; the
cut it anticipates is dynamic, and lands at the write.** An interior edge is
dead once its reader stage has ended, and a stage feels that death at exactly
one place — its next write to the dead edge — and nowhere else: everything it
did before that write, on its own account, runs to completion regardless.
That break is the pipeline's only forgiven death — every other exit status,
whatever it is, is kept. A stage whose own redirect diverts every byte of its
stdout to a file is a corollary of the same rule, not an exception to it: it
never performs the write the cut watches for, so `cmd > file | next` runs
`cmd`'s redirect to completion regardless of when `next` settles. Every
semantic arrow in a pipeline already points tail-ward — value, report —
and the cut points the same way: past a `|`, a stage speaks only while its
reader is there to hear it, and is otherwise left to its own account
([[decisions/260905_the-cut-is-at-the-write|the-cut-is-at-the-write]]).

The terminal-handoff and process-containment machinery is transport detail, not
surface semantics. Unix uses process groups and a foreground guard claimed
before any stage runs; ral does not suspend, so a stopped child — by
`SIGTSTP`, `SIGTTIN`, `SIGTTOU`, or an external `kill -STOP` — is answered
with `SIGCONT` at once by whoever is waiting on it, the same rule for a
direct external and a stage thread's own child alike
([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]). Windows uses
Job Objects and a creation-time launch path to close its handle-inheritance
window. The moving parts live in the [[map/core/runtime|runtime]]'s
`pipeline/` and [[map/core/io-process|process]] maps.

See also [[design/types|types]], [[design/cbpv|cbpv]],
[[design/codecs|codecs]], [[design/scoping|scoping]].

**Realised in** [[internals/pipeline-execution|pipeline-execution]].

Cite: RATIONALE §"Pipelines follow their edges", §"Failure is not truth",
§"Lexical data, dynamic authority"; `docs/SPEC.md` §7, §11, §17.4.
