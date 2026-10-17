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

The frozen Agda CK core in `dev/agda/CK` defines contextual equivalence by program tests
returning `Unit`: a test must use a thunk to observe it. The relation is
closed under substitution contexts. Its checked laws are:

- thunk/force β: `force (thunk M) ≈ M`;
- thunk/force η for closed values: `thunk (force V) ≈ V`;
- bind β: `return V to N ≈ N[V/x]`;
- bind η: `M to return x ≈ M`;
- bind associativity: `(M to N) to P ≈ M to (N to shift-under P)`;
- function β: `(λx. M) V ≈ M[V/x]`;
- function η: `λx. M x ≈ M`, for fresh `x`.

Administrative simulations preserve exact finite ground observations in both
directions, including byte routing, failure and pipeline scheduling. The
bind and function laws also hold under arbitrary closing substitutions.
The reverse step theorem ends at a value-returning type `F A`, so a function
focus necessarily has an argument frame; it makes no claim about an
unapplied function computation at an empty stack.
These are finite may-observation equivalences, without fairness or
must-convergence claims. The separate stack relation compares returned value
syntax; `CK.Contextual.StackCounterexample` proves why its laws alone do
not establish higher-order congruence.

Statement associativity follows from bind associativity, preserving byte
flow and failure even under stored code. The independent finite-behaviour
algebra now provides sequencing and capture operations. Capture closes its
image under finite prefixes to include observations after a flush but before
a halt; this process has checked soundness and completeness for finite
observable views at the empty stack.
Continuous process operators have least fixed points given by the union of
finite unfoldings. Checked unfolding and induction laws give recursion a
compositional meaning. Repetition of a closed body agrees with this construction
in the term semantics and, at base scope when the body is complete, with
machine execution. Finite-execution induction justifies the recursive call.
Capture of repetition agrees with the prefix-closed capture image of that
fixed point. The external-command instance may emit arbitrarily many command
events or fail, but cannot return successfully. A silent self-loop has exactly
the bottom machine process. Recursive adequacy across the whole syntax and
ambient scope remains open.
The fundamental theorem now gives exact machine agreement for the CBPV
fragment with general recursion, higher-order values, external commands,
halts, capture, recovery, cleanup, data elimination, codecs and register
reads at base scope.
Its logical relation retains computability
of returned thunks and functions at the remaining execution bound; closing
substitutions preserve that relation. Coverage is a predicate over the existing
syntax. Cleanup preserves the saved higher-order result; ground-data types
supply the returned-value property for codecs and register reads. Sum, product
and list elimination preserve computability of suspended code in their data.
Their β laws equate a match with its selected body after substitution in the
term semantics, at any scope/world. Ambient
layers and remaining primitives still need cases;
the theorem does not require separate completeness assumptions for subterms
inside its covered fragment.
The scope laws in `CK.Semantics.Scoping` establish distribution over bind
and capture for valid environment overlays and handlers. Returning a thunk
does not capture that temporary dynamic scope; the corresponding machine law
holds for arbitrary bodies and surrounding stacks
([[design/scoping|lexical and dynamic scope]]).
The term-directed interpretation now covers an initial sequential fragment
with explicit scope and world responses, and checks its basic β laws.
Every finite behaviour derived by the current term interpretation has checked
machine soundness, including capture beneath sequencing and scoped handlers.
The term interpretation also proves terminal `try` laws,
identity cleanup and precedence of a halting cleanup over a halted body.
Reverse agreement is checked for terminal terms and unconstrained external
commands at base scope. It composes through forcing, capture, bind,
application, try and cleanup under explicit completeness premises. The
execution decomposition preserves exact events, including output before
failure; an application needs completeness only for functions its body can
return. Separate scoped rules establish agreement for environment/handler
entry, reads, dispatch and codecs under finite layer stacks. Checked examples
return both inner and restored environments and execute a self-masking
handler under capture. Uniform composition across those scoped and sequential
rules, general recursion, complete syntax coverage, contextual capture/failure
laws and alignment with producer grades remain outstanding.

The active `dev/agda/CEK` foundation has graded intrinsic syntax with bounded
type variables and proves closure under simultaneous type/grade substitution.
Substitution preserves kinds, including recursive data bounds and intersections.
Ground kind views classify the types available to primitive interpretations;
closed data types contain no thunks. Instantiation leaves
erased code unchanged: unit upcast disappears, capture/decode remain. Direct
adaptation has a unique action for its source and target types. These proofs
check with `--safe --without-K`; they do not yet give operational erasure or
graded contextual βη laws. Sorted rows, schemes and the environment machine
remain to be built. The theorem maps are `dev/agda/CEK/README.md` and
`dev/agda/CK/README.md`; the frontier is `dev/docs/agda-progress.md`.

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
