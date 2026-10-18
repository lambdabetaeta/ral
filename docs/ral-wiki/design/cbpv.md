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
type variables and proves closure under simultaneous type/row/grade substitution.
Substitution preserves kinds, including recursive data bounds and intersections.
Ground kind views classify closed types; runtime data values contain no
thunks even under open type scopes. Open record and variant rows are sorted
and duplicate-free; selection, type-changing updates and exhaustive cases
preserve typing under instantiation. `Error` is the ordinary five-field
record, and a typed `try` handler can select its status. Instantiation leaves
erased code unchanged: unit upcast disappears, capture/decode remain. Direct
adaptation has a unique action for its source and target types. These proofs
check with `--safe --without-K`; they do not yet give operational erasure or
graded contextual βη laws. Scoped open-row substitution preserves exclusion/deep
bounds and satisfies identity and composition throughout the type grammar.
Projection, injection and type-changing update preserve typing and erased code
through tail substitution. Prenex scheme substitution and bounded instantiation
also check, including lifting under each quantifier sort. Contexts bind schemes;
occurrences carry bounded instantiations, and bind generalises computations
under fresh quantifiers and a weakened outer context, without a value
restriction. Substitution preserves typing and erased code through these
rules; recursive self-bindings remain monomorphic. Whole-core operational
soundness remains open.
The data foundations include canonical sorted-map construction, checked i64
and unbounded arithmetic, and an abstract float interface. Signed division
truncates toward zero; the division equation, remainder bound, range checks
and zero-divisor precedence are proved. Float numeric equality identifies
signed zeros without identifying their representations. Text is a list of
Unicode scalar values, ordered by code point; byte trimming removes exactly
one LF or CRLF. Named primitive calls preserve typing and opcode under
instantiation. Their Σ interface requires typed results and interpretation
independent of typing witnesses, yielding deterministic primitive reduction.
Runtime values and arithmetic, codec, map and indexing interpretations
check. Primitive failures construct ordinary raised-error records; successful
map insertion preserves existing entries. The complete current primitive
dispatcher includes structural data equality, scalar ordering and shallow
sizing under arbitrary scopes, and proves independence from typing witnesses.
Type equality at any fixed scope is decidable, yielding the equality-proof uniqueness needed by that theorem
without assuming K. Typed lexical closures and scheme-indexed environments
support total value evaluation. Instantiating a stored polymorphic value
preserves its erased data and captured code; typed and untyped value evaluation
agree. The CEK machine executes structural/control forms, including
polymorphic bind, recursion, recovery and cleanup. Primitives execute beneath
generalisation and raise ordinary catchable records. Generalisation evaluates
its body once and packages the returned value. Internal steps are deterministic.
Direct external commands thread mutable world state and consume stdin prefixes;
stdout/stderr words survive failures. Only process exit zero succeeds; other
outcomes become structured errors with syntax-stamped sites. The world relation
need not be deterministic or total. Byte feeds share a closed input queue and
restore caller stdin on return or escape, preserving world changes and output.
Environment entry validates sorted overlays, refuses PWD/OLDPWD before the
body, and restores the caller map on every outcome. Commands observe the
active map; returned closures retain lexical bindings only. Directory entry
rejects empty paths locally and uses a stateful world judgment for other paths.
The saved path restores on every outcome; resolution and OS/authority
validation remain obligations of that judgment.
Capture accumulates stdout in its continuation frame and passes stderr through.
It returns exact bytes on success; a halt flushes, and a refused flush preserves
the delivered prefix while replacing the original halt. Pipelines and authority
scopes remain pending; file redirects execute in the protocol machine below.
Contextual transfer remains open. Typed continuation
extension relates every CEK next action and lifts finite internal runs;
capture extension preserves inner buffers and absorbs outward stdout. General
forward simulation now compares capture with a body run under accepting
stdout, preserving stderr, stdin and body world changes. It
returns the complete byte word on success and follows the real outer flush
on halt. Reverse completeness reconstructs a reference-body run from every
terminating capture at the empty stack, with exact stderr and a witness to
the returned bytes or actual flush response. This characterises finite
termination; finite-prefix and contextual characterisations remain open.
Handler selection now has checked named/base/catch-all priority and exact
single-frame masking with a permanent base table. Typed lexical arms apply
argv/name in fixed order, and a command-result frame returns outcomes across
differing type scopes. Configurations now execute installation, named/catch-all
dispatch, owner masking/restoration, external fallback and unmasked host base
calls, retaining native outcomes and bounded byte traffic.
`CEK.TermEvaluation` defines independent finite evaluation of terms, closures,
applications, polymorphic builders and arms. Computation results distinguish
returned values, lexical functions and escapes. Builders execute once under
fresh scopes and package their values; subsequent occurrences only specialise.
Capture changes the body's stdout interpretation, then handles the real outer
flush reply. Checked derivations cover handler forwarding and one-time builder
effects; a recursive forcing loop has no finite derivation.
`CEK.TermSoundness` refines every finite derivation to CEK execution at arbitrary
type scopes and continuations. Results and final contexts agree exactly;
stdout enters the nearest enclosing capture and stderr passes through. At the
empty continuation this gives termination with identical output and outcome.
`CEK.TermContinuation` interprets pending frames through independent evaluations.
`CEK.TermReflection` preserves this meaning backward through every internal step
and scope restoration; terminating internal runs therefore yield independent
term derivations. `CEK.TermEffectsReflection` covers command replies, scope entry,
handler dispatch and capture flushes. `CEK.TermCompleteness` proves exact
finite-run agreement with independent evaluation at the empty continuation:
output, typed result and every final context field coincide. No world
determinism or global termination assumption is needed. Contextual handler
laws and the remaining pipeline, redirection and authority forms remain open.
`CEK.RecursiveGroup` derives heterogeneous recursive groups as a unary fixed
point returning a product of thunks. Formation runs no member. Invocation
unfolds to the selected closure at any typed continuation, and independent
evaluation satisfies the unfolding law in both directions. The closure keeps
monomorphic lookup's type specialisation; its erased code and captured
environment are proved equal to the member under the recursive environment.
Mutual calls, mixed grades, lexical capture and an unselected halt have checked
examples. Source-binder elaboration and Rust's capture minimisation and node
sharing remain outside this result.
`CEK.Boundary` checks erased data against a ground type. Every successful read
carries a proof that the returned value erases to exactly its input, and every
typed data value is admitted. `BoundaryCompleteness.decide` decides whether
such a representation exists. Closed records require the exact field set;
maps require the canonical sorted dictionary representation and valid Unicode
keys. Admission performs no numeric conversion or field loss. The byte-input
`from-json` primitive fixes its ground result in the opcode and uses a supplied
parser. Its result embeds beneath arbitrary binders with exact erasure
preservation; the Σ interpreter remains independent of typing witnesses.
Parse/refusal failures raise ordinary status-1 errors with the command and
site. Checked machine examples exercise recovery, generalisation and cleanup.
JSON grammar, stdin acquisition/elaboration, detailed diagnostic paths, weak
existential types and closure/module contracts remain outside this result.
The separate CEK file protocol distinguishes staged replacement from streaming
writes. Its byte ledger retains exactly delivered prefixes. Settlement visits
every intent despite commit failure, retains each path/mode/word and reports
the first error. Body halts abandon staged writes; streamed bytes remain
committed. Total settlement assumes available world responses, without assuming
determinism. Ordered entry opens stdin, stdout and stderr, aborting earlier
output intents on a later refusal while retaining world effects. Merged stderr
shares the final stdout destination without a second intent. Input restoration
is guarded by an installed input redirect, preserving inherited consumption.
Entry is total under explicit read/open/cleanup availability. Routing keeps a
ledger indexed by the opened sinks: file writes retain delivered prefixes,
inherited writes delegate, and discard owns no intent. Merged stderr updates
stdout's account, giving one settlement fact with both channels' delivered
bytes in trace order. A checked counterexample proves that per-channel aggregate
words cannot reconstruct every merged trace. External and base replies now
carry ordered transcripts proving their exact aggregate output. Each CEK run
has an outward transcript: capture filters stdout while preserving stderr
order, and later flushes retain only delivered bytes. Existing adequacy remains
an aggregate-observation theorem. A separate command protocol interleaves opaque
process steps with actual routed writes, feeding each response back before the
command continues. Its finite runs preserve exact file ledgers and merged
settlement; replies forward only parent output, keeping file/discard traffic
internal. Nested sink stacks enforce scope order: an inner capture precedes
an outer file, an inner file precedes an outer capture, and stderr changes to
stdout only at the merge binding it reaches. Scoped writes preserve the
bindings and reach at most one outward endpoint; they are total under explicit
operation availability. Commands execute against these stacks, with the
single-frame model embedded exactly. Scope data is independent of the world:
an opening snapshot fixes the bindings indexing its current ledger. Typed
scope exit settles every owner, restores guarded stdin and applies body/commit
outcome priority, with totality under settlement availability.
Machine modules now parameterise handles abstractly. A routing bridge views
the current typed continuation as nested capture/file/root sinks and reconstructs it
after shape-preserving I/O updates. Its control skeleton preserves all values,
handlers, cleanup closures and saved scope data, forgetting capture buffers
and file-ledger words while retaining the opening snapshot and bindings. Every finite scoped command run from such a view lifts to a typed
final continuation with that exact view and unchanged skeleton.
The typed protocol machine now interleaves external and selected base process
steps with routed writes, feeding back each actual response before continuing.
Completion resumes ordinary CEK value/error/exit control and bounded input
consumption. Structural, scope and handler transitions are shared with the
atomic machine; atomic command and flush transitions are excluded from this
embedding. Capture-halt flushing uses the same typed router. Finite scoped
runs compile with exact outward events. Conversely, a finite active-command
path reaching a ready state yields its scoped run to first completion and the
remaining path, with exact event concatenation and preserved result, world,
input and control metadata. Existing whole-term adequacy remains for
`CommandMachine`'s atomic interface. Typed file delimiters execute source
stdin/stdout/stderr/merge scopes, preserving stdout's grade change. Entry
refusal resumes an ordinary error; return or escape settles the current ledger,
restores guarded input and applies body/commit-error priority. Machine paths
retain exact settlement facts, including opening rollback. Source examples
check capture/file precedence, partial refusal, stream retention, commit/open
failure, staged abort, one merged fact in write order and input restoration
after a nested commit error. Independent protocol term semantics now carries
these sinks through structural evaluation, recursion, polymorphic builders,
selected arms, cleanup and all implemented scopes. Capture uses a buffer;
redirect bodies update a ledger which is settled on exit. Ordered outward
writes and file facts are retained separately. Mutual induction proves that
every finite derivation preserves enclosing sink bindings and restores caller
handlers. Independent examples distinguish capture/file order, merged refusal
feedback, commit-error recovery and refused halt flushing. Whole-term agreement
with the protocol machine, transcript-sensitive adequacy and full audit
chronology remain open.
Primitive erasure agreement and compatibility
with runtime instantiation remain to be proved.
The theorem maps are `dev/agda/CEK/README.md` and
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
