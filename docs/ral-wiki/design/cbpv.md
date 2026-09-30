# Call-by-push-value: values and commands

**ral's organising principle is the call-by-push-value separation of *values*
from *commands*:**

- *values* are inert data: strings, integers, lists, records, thunks;
- *commands* are computations that read, write, fail, or return;
- a thunk `{M}` suspends a command as a value; `!` forces it back into a
  command, exactly — `force(thunk M) = M`, one thunk value and no bracket
  around the body ([[design/scoping|scoping]]).

The typed calculus is ordinary call-by-push-value ([[design/types|types]]). A
command is a computation `F^w Unit`: its value is its output, and `F` carries a
grade saying so. Every form
that suspends a command — `if`, `case`, `try`, `guard`, `within`, `grant` —
takes a thunk and forces the one it chooses.

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
- A thunk value is `⟨M, ρ|occ(M)⟩` by construction: it holds only the session
  bindings its body mentions. The machine's `⟨M, ρ⟩` in focus is a
  *computation closure*, transient and holding the whole environment `M` was
  reached in
  ([[decisions/260926_a-closure-keeps-only-what-it-mentions|a-closure-keeps-only-what-it-mentions]]).
- Every value is a closure over what it mentions, not a thunk alone: a list,
  record or map literal is `⟨V, ρ|occ(V)⟩` too, formed by `form(val, env,
  sig)` and read apart one layer at a time — `⟨[V₁…Vₙ], ρ⟩`'s *i*th element
  is `⟨Vᵢ, ρ⟩` — a name lent from ρ, a constant built, a nested literal or
  thunk closing over that same ρ rather than restricting again
  ([[internals/evaluator-machine|evaluator-machine]]).
- ρ is a finite map from names to values; Σ, the natives and the frozen
  prelude, is a shell's own constant and is never part of any ρ — a name
  resolves in ρ, then Σ. A `Literal` value closure's inspection reads ρ
  alone, so a name only Σ answers is never one of its entries; `form` checks
  that before choosing the closure over building the literal eagerly.
- **The cost model:** value operations cost O(program text) — occ is
  computed once, where a node is built, and inspecting a literal shares its
  ρ rather than recomputing; only computations cost O(data).

See also [[design/syscalls-are-effects|syscalls-are-effects]] (commands are the effect half this splits off),
[[design/scoping|scoping]], [[design/control-operators|control-operators]],
[[invariants/fixed-arity|fixed-arity]], [[related/system-c|system-c]] (box = thunk, unbox = force),
[[related/call-by-push-value|call-by-push-value]] (the substrate at its source).

**Realised in** [[internals/compilation-ladder|compilation-ladder]] and [[internals/evaluator-machine|evaluator-machine]].

Cite: RATIONALE §"Values and commands", §"One form, one meaning";
`docs/SPEC.md` §2, §5.
