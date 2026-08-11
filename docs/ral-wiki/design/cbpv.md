# Call-by-push-value: values and commands

**ral's organising principle is the call-by-push-value separation of *values*
from *commands*:**

- *values* are inert data: strings, integers, lists, records, thunks;
- *commands* are computations that read, write, fail, or return;
- a thunk `{M}` suspends a command as a value; `!` forces it back into a
  command.

The typed calculus is ordinary call-by-push-value with one annotation on the
returner: `F[ρ] A` records which of a command's two products — its returned
value or its stdout — a value boundary observes ([[design/types|types]]).

Two sigils keep retrieval and forcing distinct:

- `$name` dereferences a binding to its value without side effect;
- a bare word in head position is looked up and, if it names a thunk, forced.

**Once captured, data is never re-lexed, split, or globbed.** So the shell
collapse — every datum at once a string, a command name, and re-lexable
source — never arises, and neither do the escaping bugs that follow from it.

The parameterised block `{ |x| M }` is the language's single abstraction
mechanism. It subsumes what conventional shells splinter across functions,
aliases, `eval`, subshells, and trap handlers: a block can be bound, passed,
returned, or installed as an effect handler. Currying desugars to nested
lambdas, so partial and under-application are uniform.

Bindings are immutable. Re-`let` introduces a fresh binding that shadows the
outer one within its lexical scope; closures capture at definition time. This
buys equational reasoning in the pure fragment and makes `spawn` safe — a
spawned block shares nothing mutable.

**Invariants.**
- `$` consults only the value namespace and never triggers command lookup or
  handler dispatch.
- Captured external output is decoded UTF-8 *strictly* — invalid bytes fail
  with a hint to keep them via `| from-bytes` — and the decoded text is never
  re-lexed.
- Closures observe the bindings in force where they were defined, not where they
  run.

See also [[design/syscalls-are-effects|syscalls-are-effects]] (commands are the effect half this splits off),
[[design/scoping|scoping]], [[design/control-operators|control-operators]],
[[invariants/fixed-arity|fixed-arity]], [[related/system-c|system-c]] (box = thunk, unbox = force),
[[related/call-by-push-value|call-by-push-value]] (the substrate at its source).

**Realised in** [[internals/compilation-ladder|compilation-ladder]] and [[internals/evaluator-machine|evaluator-machine]].

Cite: RATIONALE §"Values and commands", §"One form, one meaning";
`docs/SPEC.md` §2, §5.
