# Pipelines: byte processes and value folds

**`|` is dataflow.** The pipe threads each stage's output into the next stage.
The type of the connecting edge decides the runtime behaviour: the byte modes
of [[design/types|computation types]] select one of two execution models. The
result mode of the final stage decides where the pipeline's own result is:

- a final stage on the value edge gives the pipeline a value payload;
- a byte-wired final stage puts the pipeline's result on the wire, so the
  pipeline's type is `⟨…, Bytes, Bytes⟩ Unit` and, by WF-2, the pipeline has
  no value component.

At a bind, the boundary inserts the [[design/types|capture]] coercion, so the
binding receives the decoded text.

Failure is a separate axis. A pipeline propagates a stage's failure, but the
pipe never *reacts* to it: recovering from failure is `?`'s and `try`'s job, and
branching is on `Bool`, never on command success ([[design/failure|failure]]).

**Value pipelines are folds.** When every stage operates on the value channel,
`x | f` is typed data-last composition — it reduces to `f !{x}` and is evaluated
sequentially in the parent. No process is spawned, no pipe exists, no process
group is formed. `range 1 21 | filter $even | sum` is three function calls
threaded by the value channel.

**Byte pipelines are processes.** As soon as one edge touches bytes — an
external command, or any byte-output stage — the whole pipeline runs as a
Unix-style process pipeline:

- every stage, *including* ral-implemented ones, executes in a subprocess;
- all subprocesses share one process group;
- the parent ral process is not a member of that group.

This is what lets a terminal-touching pipeline behave exactly as a shell's does,
regardless of whether a stage is `/bin/cat`, a handler, or a ral block.

Out-of-process stages are therefore subshells with respect to mutation: a
helper stage's `cd`, env, alias, or module changes do not flow back to the
parent — only the pipe contents and the final value cross the boundary. This
matches every traditional shell and is what keeps job control coherent: a shell
process inside its own foreground pipeline cannot consistently both own the
terminal and not own it. It is the same isolation a spawned block enjoys
([[design/cbpv|immutable bindings]]).

The terminal-handoff and process-containment machinery is transport detail, not
semantics. Unix uses process groups, a foreground guard, and helper job-frame
gates where a tty handoff must settle before user code runs; Windows has no
`tcsetpgrp` analogue, so it uses Job Objects after spawn and still needs a custom
creation-time launch path to close its handle-inheritance and early-fork windows.
The moving parts live in the [[map/core/runtime|runtime]]'s `pipeline/` and
[[map/core/io-process|process]] maps.

See also [[design/types|types]], [[design/cbpv|cbpv]],
[[design/scoping|scoping]].

**Realised in** [[internals/pipeline-execution|pipeline-execution]].

Cite: RATIONALE §"Pipelines follow their edges", §"Failure is not truth",
§"Lexical data, dynamic authority"; `docs/SPEC.md` §4, §13, §20.4.
