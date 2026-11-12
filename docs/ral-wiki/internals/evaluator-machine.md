---
verified_at_commit: f9ac94f8
verified_at_date: 2026-11-12
anchors: [Machine, step, settle, eval, eval_rules, resume, unwind, Frame, ValClos, ValClos::close, ValClos::thunk, View, BuiltinEntry::eta, BuiltinEntry::residual, CompKind::Builtin, Node::new, Env, restrict, Signature, lookup, run_phrases, Phrase, evaluate, apply, reserve, PipeNode, NESTED_MACHINE_LIMIT, force, force_val, Occ::of, Frame::Gather, BroodGuard, exit_brood, Frame::Acquire, Frame::Guard, Frame::Cleanup, guard_release, exit_case, ExitCase]
---

# The evaluator: a CEK machine for CBPV

**The machine steps a computation closure or unwinds a break; a produced
value meets its continuation in the same step.** Lexical bindings travel with
code, while shell effects act on the caller's live state. This is a *CEK
machine*: control, environment and continuation.

- These rules describe checked `Val` / `CompKind` IR, **not surface ral**.
  Elaboration and checking make operand sequencing, capture and returned-thunk
  application explicit ([[internals/compilation-ladder|compilation ladder]],
  [[invariants/ir-pure-cbpv|IR invariant]]).
- `Machine { state, stack, mooring, … }` holds `Settled<(Arc<Comp>, Env)>`:
  a computation closure or `Err(Break)`, never a persistent value register.
  `settle` passes values through frames until one yields a closure or break,
  or the run finishes. No rule constructs runtime `return V` syntax.
- This is an operational account of Rust, not a refinement proof. Agda's
  substitution-based CK machine is the core correspondence boundary;
  environments implement lexical binding rather than literal substitution.
  Residual-function boxing, runtime builtin η/δ and the acquiring `guard`
  need their separate correspondence follow-ups
  ([[decisions/261102_the-checked-ir-is-the-calculus|correspondence ADR]],
  [[decisions/261111_a-guard-acquires|a-guard-acquires]]).
- The value and state representation is fixed by
  [[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]].

## Syntax and notation

**Values are inert; computations produce a value or await an argument.**
This is a reading key, not a replacement for `core/src/ir.rs` or SPEC §17.

```text
Value types       A, B ::= Unit | Bool | Integer | String | Bytes
                        | List A | Map A | Record r | Variant r
                        | U C | Handleᵍ A | …
Computation types C    ::= Fᵍ A | A → C
Value terms       V, W ::= x | () | scalar | bytes | builtin c | handle h
                        | thunk M | [V₁,…,Vₙ] | record{ℓᵢ:Vᵢ}
                        | map{ℓᵢ:Vᵢ} | tag ℓ V
Computations      M, N ::= return V | force V | λx.M | M · (V₁,…,Vₙ)
                        | M to p.N | M ⨟ N | rec G i
                        | if V then Wₜ else W𝒻 | case V of {ℓᵢ ↦ Wᵢ}
                        | prim q(V⃗) | δ(b; a⃗) | try V W | guard A V W
                        | capture M | shell forms
```

- `U C` suspends a computation; `Fᵍ A` produces `A`; `A → C` awaits an
  argument and is **not a value type**. A lambda is not a thunk. The producer
  grade `g` does not classify all effects; output grade `w` pairs only with
  `Unit` ([[design/capture|grades and capture]]).
- `x` is a name, without surface `$`; `p` is a wildcard, name, list or record
  pattern. `G = [(x₀,M₀), …]` is a shared recursive group. Spans, admission
  sites and primitive metadata are omitted.
- Choice arms are thunks; a case arm takes the payload. `try` and `guard`
  also take suspended operands. `Capture`, `Bind`, `Seq`, `App` and redirects
  contain computation syntax where their constructors say so.

**Only the following are machine configurations:**

```text
⟨M,ρ | K; σ⟩             computation closure
⟨raise b | K; σ⟩         Err(b), not an IR constructor
b ::= Error(e) | Escape(a)
K ::= ε | F :: K         leftmost frame is the TOP
```

- Lowercase `v` is a runtime value closure `⟨V,ρ⟩`, with nonvariable root
  and every direct name bound in `ρ`. A direct name is outside any thunk.
  `ρ[x ↦ v]` is persistent extension. `Binding`'s optional scheme is omitted.
- `Σ` is the fixed `Signature`: prelude then builtins. Lookup is `ρ` then
  `Σ`. `μ` is the run's fixed `Mooring`; `σ` is live `Shell` state — cwd,
  environment overrides, handlers, grants, sinks, scopes, workers and audit.
  External effects are not rolled back merely because a frame is popped.
- `F :: K` is top-first, opposite Rust's `Vec` order. Unless shown, `σ`
  is unchanged.

**Rule results are not configurations.** Write `C(M,ρ)` for the next closure
and `D(v)` for a value handed down now. `S(n,K;σ)` denotes `settle` on the
result `n`; `meet(t,F,K;σ)` denotes `resume` after popping `F`, where `t` is
`C(λx.M,ρ)` or `D(v)`. These are metanotation, not extra machine states:

```text
S(C(M,ρ),K;σ) = ⟨M,ρ | K;σ⟩
S(Err(b),K;σ) = ⟨raise b | K;σ⟩
S(D(v),ε;σ) = finish Ok(v)
S(D(v),F :: K;σ) = meet(D(v),F,K;σ)

⟨λx.M,ρ | F :: K;σ⟩ → meet(C(λx.M,ρ),F,K;σ)
⟨λx.M,ρ | ε;σ⟩ → finish Ok(⟨thunk (λx.M),ρ↾occ(λx.M)⟩)
⟨raise b | F :: K;σ⟩ → unwind(F,b,K;σ)
⟨raise b | ε;σ⟩ → finish Err(b)
```

- Arrows erase spans and provenance, not failures. `⇒*` combines transitions;
  host operations may do substantial work or re-enter another machine.
- `step` pops before `resume` or `unwind`. A helper failure installs `Err(b)`
  with stack and shell **as they then stand**: pushed frames remain and popped
  ones do not reappear. Success rules presume reservation and polling succeed.
- `boxed(D(v)) = v`; `boxed(C(λx.M,ρ)) = ⟨thunk (λx.M),ρ↾occ(λx.M)⟩`.
  Pass-through frames preserve `t`; value-demanding frames use `boxed(t)`.

## Values and environments

**Closing a value executes no computation and reads no ambient shell state.**
Write `close(V,ρ,Σ)` for `ValClos::close`:

```text
close(x,ρ,Σ) = lookup(x,ρ,Σ), or Error(undefined x)
close(V,ρ,Σ) = ⟨V,ρc⟩                       V not a variable
ρc = ρ↾occ(V), extended by Σ's answers for missing direct names
read(x,ρ) = ρ(x)
read(V′,ρ) = ⟨V′,ρ⟩                         V′ not a variable
```

- `Node::new` stores `Occ::of`, an over-approximation of mentions including
  bound names. `close` and `ValClos::thunk` restrict at formation; component
  `read` shares the whole saved `ρ`, without restricting again or consulting
  `Σ`. Names under a thunk resolve `ρ` then `Σ` when it runs.
- Constructors for scalars, byte strings, builtins and handles use empty
  captures; component reads may retain the container's ρ. `Env::EMPTY`
  clones without a refcount operation; small nonempty environments use
  `ThinArc`. Compiled upper bounds are `Env ≤ 16`, `ValClos ≤ 48` and
  `Frame ≤ 128` bytes.
- Built lists and maps are value closures over implicit fresh variables,
  represented by persistent vectors and ordered maps for logarithmic updates.
  `ListRef` / `MapRef` read either syntax over `ρ` or built data through
  `Cow<ValClos>`. A variant shares its payload's syntax and capture, binding a
  built container, builtin, handle or η residual when needed. `View::Variant`
  omits a resolved unit payload, including a variable bound to unit, without
  recursively calling `view`.
- No sink, cwd, handler stack, worker ownership or lease enters a lexical
  capture. Forcing restores bindings, **not shell state**
  ([[design/scoping|scoping]]).

## Application and force

**A builtin forces to a λ-chain ending in δ; partial application is ordinary β.**
For builtin value `c` of arity `n`, let
`ηc = λa₀.…λaₙ₋₁.δ(b; a⃗)`, with `b ↦ c` in its environment.

```text
⟨return V,ρ | K;σ⟩ → S(D(close(V,ρ,Σ)),K;σ)
enter(⟨thunk M,ρc⟩,K;σ) = ⟨M,ρc | K;σ⟩
enter(⟨builtin c,ρc⟩,K;σ) = ⟨ηc,{b ↦ ⟨builtin c,ρc⟩} | K;σ⟩
enter(v,K;σ) = ⟨raise Error(cannot-force) | K;σ⟩        otherwise
enterᵥ(thunk M,ρ,K;σ) = ⟨M,ρ | K;σ⟩
enterᵥ(V,ρ,K;σ) = enter(close(V,ρ,Σ),K;σ)              otherwise
⟨force V,ρ | K;σ⟩ → enterᵥ(V,ρ,K;σ)

vᵢ = close(Vᵢ,ρ,Σ)
⟨M · (V₁,…,Vₙ),ρ | K;σ⟩
  → ⟨M,ρ | Arg(v₁) :: … :: Arg(vₙ) :: K;σ⟩
meet(C(λx.M,ρc),Arg(v),K;σ) = ⟨M,ρc[x ↦ v] | K;σ⟩   β
meet(D(v),Arg(w),K;σ) = ⟨raise Error(not-a-function) | K;σ⟩

view(ρ(b)) = Builtin(c)       v⃗ = ρ(a⃗)
run_native(c,v⃗,μ,σ) = (Ok(v),σ′)
⟨δ(b; a⃗),ρ | K;σ⟩ → S(D(v),K;σ′)
```

- `force_val` enters a literal thunk in the current environment without
  storing a value closure. `force` enters a saved thunk or builtin η-expansion;
  it pushes no shell restoration frame and polls nothing.
- `App` closes arguments in order and pushes right-to-left, reserving all
  `n ≥ 1` frames together. β polls after consuming its argument. The remaining
  λ crosses restoration frames as a computation, so a tail call consumes the
  caller's remaining arguments. A value meeting `Arg` is a clean invariant
  error, never an implicit force.
- `call(f,args)` invokes a builtin directly when given **exactly** its arity;
  otherwise it pushes arguments and forces `f`. Failure to force leaves those
  arguments for unwinding. Bound heads, handlers and host `apply` share it.
  Arity zero runs on force or the exact-arity call. `BuiltinEntry::eta` caches
  synthetic syntax; δ reads the captured entry, not a name-dispatched `Σ` row.
- Boundary calls run saturated with the checker's `Site`. Inference expands
  partial boundaries over `Exec`, retaining their per-use site.
- A residual λ is boxed only at a value demand or run end. Builtin residuals
  display `<native NAME, k of n arguments>`, have type name `Lambda`, and
  cross as a builtin name, an environment row and a supplied-argument count,
  not as thunk syntax. `BuiltinEntry::residual` rebuilds the receiving entry's
  η suffix only for a count below its arity and a row binding exactly that
  builtin and the supplied arguments; zero supplied is valid.
  `ValClos::residual` recognises them for marshalling, display, hook arity and
  rejection as handler arms. δ is neither wire syntax nor sent to the checker
  ([[decisions/261013_a-child-starts-from-its-parent-marshalled|a child starts from its parent, marshalled]]).
- `run_native` may write, fail, escape or apply a callback in a nested
  machine. Its break settles as `Err(b)` after the host operation, located at
  the builtin call. It is not a value-only primitive.
- A returned thunk is data. Checked application through one is
  `M to h. (force h) · V`, never application of the returned value itself.
  Every `let` runs its RHS; a residual function is then boxed.

## Continuations, choice and recursion

**Syntax that resumes later carries its own environment.** Only `To` and
`Seq` frames carry a computation with its saved `ρ`.

```text
⟨M to p.N,ρ | K;σ⟩ → ⟨M,ρ | To(p,N,ρ) :: K;σ⟩
match(p,boxed(t)) = θ
meet(t,To(p,N,ρ),K;σ) = ⟨N,ρ ⊕ θ | K;σ⟩
match(p,boxed(t)) = Error(e)
meet(t,To(p,N,ρ),K;σ) = ⟨raise Error(e) | K;σ⟩
⟨M ⨟ N,ρ | K;σ⟩ → ⟨M,ρ | Seq(N,ρ) :: K;σ⟩
meet(t,Seq(N,ρ),K;σ) = ⟨N,ρ | K;σ⟩

view(close(V,ρ,Σ)) = Bool(true)
⟨if V then T else F,ρ | K;σ⟩ → enterᵥ(T,ρ,K;σ)
view(close(V,ρ,Σ)) = Bool(false)
⟨if V then T else F,ρ | K;σ⟩ → enterᵥ(F,ρ,K;σ)
view(close(V,ρ,Σ)) = Variant(ℓ,v)       arm(ℓ) = W
⟨case V of arms,ρ | K;σ⟩ → enterᵥ(W,ρ,Arg(v) :: K;σ)

G = [(xⱼ,Mⱼ)]ⱼ       ρ₀ = ρ↾occ(G)
ρG = ρ₀[xⱼ ↦ ⟨thunk (rec G j),ρ₀⟩]ⱼ
⟨rec G i,ρ | K;σ⟩ → ⟨Mᵢ,ρG | K;σ⟩

v⃗ = close(V⃗,ρ,Σ)       δq(v⃗) = Ok(v)
⟨prim q(V⃗),ρ | K;σ⟩ → S(D(v),K;σ)
```

- Destructuring collects **all** bindings before installing any, then extends
  in pattern order. Lists require exact length without rest, at least the
  prefix with rest. Mismatch occurs after `To` is popped: no partial binding,
  tail execution or resurrected frame. Lambda patterns lower to body binds.
- `To` boxes its input; `Seq` discards it. Checked sequences require producers.
  On a break `Arg` propagates; `To` and `Seq` abandon the rest. Only errors
  through written lets and sequences acquire the abandoned-tail fact;
  compiler binds and escapes do not.
- `if` forces only its selected arm. `case` pushes the payload before forcing;
  a bare tag supplies unit. Malformed heads or missing arms fail, and a case
  arm's force failure leaves its `Arg` on the stack.
- Recursive siblings share `G` and `ρ₀`, not a cyclic environment containing
  themselves. Force reinstalls the group. The selected member reuses its
  `Rec` node; siblings have nodes over the same group.
- `δq` is value-only arithmetic, indexing, rendering, decoding or assembly,
  distinct from builtin δ. It may fail or cost O(data), but executes no
  operand computation. Helpers may also poll cancellation.
- `Decode` drops its environment before taking the bytes, removes one trailing
  LF or CRLF and checks UTF-8 strictly. Capture policy is explicit syntax,
  not an implicit action of `To` ([[design/capture|capture]]).

## Failure, recovery and cleanup

**Errors and escapes share unwinding; only errors are caught by `Try`.**
Neither is an IR constructor.

```text
v = close(V,ρ,Σ)       h = close(W,ρ,Σ)
⟨try V W,ρ | K;σ⟩ → enter(v,Try(h) :: K;σ)
meet(t,Try(h),K;σ) = S(t,K;σ)
unwind(Try(h),Error(e),K;σ) = call(h,[record(e,σ)],K;σ)
unwind(Try(h),Escape(a),K;σ) = ⟨raise Escape(a) | K;σ⟩
```

- Body and handler close **before** installing `Try`; thereafter it catches
  a force failure too. The popped `Try` cannot catch its handler's failure.
- `record` is a witnessed `[frame, message, status, site]` map, from which
  `raise` recovers the fact, not discarded-byte or abandoned-tail accounting.
  `Try` passes a λ unchanged. A returned block runs later outside the catch.

**Guard shields acquisition and installs release before a body poll.** Let
`o ::= Ok(v) | Err(b)`; `restore(Ok(v)) = D(v)`, `restore(Err(b)) = Err(b)`.
`shield(σ,μ.grace) = (σs,s₀)` installs a shield and saves the displaced scope.
`call(f,args,K;σ)` includes the exact-arity builtin rule above.

```text
room(3,K)       poll(σ) = ()
a = close(A,ρ,Σ)       k = close(V,ρ,Σ)       r = close(W,ρ,Σ)
shield(σ,μ.grace) = (σs,s₀)
⟨guard A V W,ρ | K;σ⟩ → enter(a,Acquire(k,r,s₀) :: K;σs)

v = boxed(t)
meet(t,Acquire(k,r,s₀),K;σ)
  = call(k,[v],Guard(r,v) :: K;σ[scope := s₀])
unwind(Acquire(k,r,s₀),b,K;σ) = ⟨raise b | K;σ[scope := s₀]⟩

shield(σ,μ.grace) = (σs,s₀)       v = boxed(t)
meet(t,Guard(r,u),K;σ)
  = call(r,[u,exit_case(Ok(v))],Cleanup(Ok(v),s₀) :: K;σs)
shield(σ,μ.grace) = (σs,s₀)
unwind(Guard(r,u),b,K;σ)
  = call(r,[u,exit_case(Err(b))],Cleanup(Err(b),s₀) :: K;σs)
meet(t,Cleanup(o,s₀),K;σ) = S(restore(o),K;σ[scope := s₀])
unwind(Cleanup(o,s₀),b′,K;σ) = ⟨raise b′ | K;σ[scope := s₀]⟩
```

- Entry checks the deepest stack requirement: `Cleanup` plus two `Arg`s.
  Popping `Acquire` or `Guard` returns to entry depth; later reservations
  assert that room. Provenance frames do not count.
- A pending stop runs no acquisition. A halting acquire restores scope and
  runs no release. On success, restoration and installing `Guard` have **no
  intervening poll**. Force polls nothing; the body's β polls with `Guard`
  below its popped argument.
- `guard_release` serves success and break. `exit_case` borrows the saved
  outcome: `ok` omits the body value, `err` is witnessed, `exited` carries the
  byte and `stopped` the cause. Release success restores the saved outcome;
  any release error or escape overrides it.
- `Acquire` and `Guard` box their inputs; `Cleanup` ignores its input.
  Checked acquisition, body and release require producers. `Acquire` and
  `Cleanup` restore scope on success, break and abandonment.
- Acquisition and release share one grace from the strike; late acquisition
  can leave release no time, and either can leak a resource. Shielding is
  cooperative, not protection from terminal signals.
- Shielding changes `scope`, not `cradle`. Stages follow scope; workers follow
  their owner's entry scope. A `gather` opened inside either shield owns
  masked workers; births into an outside owner do not inherit the shield
  ([[internals/cancellation|cancellation]]).

## Capture and shell frames

**Capture returns whole bytes or fails; it exposes no partial prefix.** Let
`freshbuf()` give sink `β`, and `take(β)` give bytes, completeness and total.

```text
s = σ.stdout       β = freshbuf()
⟨capture M,ρ | K;σ⟩ → ⟨M,ρ | Capture(s,β) :: K;σ[stdout := β]⟩
take(β) = (bytes,whole,total)
meet(t,Capture(s,β),K;σ)
  = S(D(⟨bytes,∅⟩),K;σ[stdout := s])                  if whole
  = ⟨raise Error(capture-overflow) | K;σ[stdout := s]⟩  otherwise
unwind(Capture(s,β),b,K;σ)
  = ⟨raise discard(b,total(β)) | K;σ[stdout := s]⟩
```

- Capture ignores the entire input, including a λ, though checked operands
  are output computations. It captures writes, not rendering of a result.
- On break it drops bytes without replay. `discard` adds the actual count
  saturatingly to an error's unwind report; escapes are unchanged. Overflow
  restores stdout before failing, after the capture frame has gone.
- Pumps, process drains and worker output have their own lifetimes
  ([[internals/output-capture-and-detachment|output capture and detachment]]).

**A shell frame owns its undo, not a snapshot of the whole shell.** Scope
installation closes inputs, reserves, installs state, pushes undo, then forces
its body. A force failure sees the frame. Redirects enter computation syntax.

| Frame | On `meet(t,…)` after pop | On `unwind(…,b,…)` after pop |
| --- | --- | --- |
| `Redirect` | Commit; `S(t,K;σ′)` on success | Abort; preserve `b`, ignoring settlement failure |
| `Within`, `Grant`, `Unmask` | Undo; `S(t,K;σ′)` | Undo; preserve `b` |
| `Audit` | Close trail; settle report of `boxed(t)` | Close; report error, propagate escape |
| `Gather` | Restore cradle; join `boxed(t)` through `exit_brood` | Restore cradle; end brood; preserve `b` |
| `CallSite` | Restore provenance; `S(t,K;σ′)` | Locate if needed; restore provenance; preserve break |

- `Within` restores cwd, overrides and handlers; `Grant` removes its own
  layer by position; `Unmask` restores a temporarily removed handler
  ([[internals/handler-dispatch|handler dispatch]]). These and redirects pass
  λs through. Checked scope bodies require producers; a returned block runs
  later outside the scope.
- Redirect commit cannot undo streamed bytes. Atomic writes can abort.
  Commit failure becomes a break on the remaining stack.
- `Gather` owns a `BroodGuard` and displaced cradle on this machine, capturing
  current scope as its new cradle's `under`. Joining/stopping follows
  [[invariants/a-worker-ends-with-its-children|worker ownership]].
- `Exec`, `Pipeline`, `Spawn`, ambient `Tilde` and builtin bodies are host
  operations. A `PipeNode` runs in one shell arm; ral stages enter fresh
  machines on threads and external stages are processes
  ([[internals/pipeline-execution|pipeline execution]]).

## Two reductions

**The stack exposes lexical scope and outcome precedence without a value state.**
These traces erase unchanged shell state, provenance, reservations and polls.

### A saved thunk keeps its lexical name

Let `ρ₀ = [x ↦ ⟨7,∅⟩]`, `f = ⟨thunk (λy.return x),ρ₀⟩` and
`ρ₁ = ρ₀[x ↦ ⟨99,∅⟩, f ↦ f]`:

```text
⟨(force f) · (1),ρ₁ | ε⟩
→ ⟨force f,ρ₁ | Arg(⟨1,∅⟩) :: ε⟩
→ ⟨λy.return x,ρ₀ | Arg(⟨1,∅⟩) :: ε⟩
→ ⟨return x,ρ₀[y ↦ ⟨1,∅⟩] | ε⟩
→ S(D(⟨7,∅⟩),ε) = finish Ok(⟨7,∅⟩)
```

- Caller `x = 99` never replaces captured `x = 7`. A literal
  `force (thunk (return x))` enters `return x` in `ρ₁` and returns `99`.
- If the thunk invokes `cwd`, it reads the caller's current cwd. A `cd`
  persists unless an enclosing scope owns restoration: a lexical closure
  is not a subshell.

### Release overrides the body before a catch

A body fails with `e₁` while resource `u` is held; release fails with `e₂`:

```text
⟨raise Error(e₁) | Guard(r,u) :: Try(h) :: K;σ⟩
→ call(r,[u,exit_case(Err(Error(e₁)))],
        Cleanup(Err(Error(e₁)),s₀) :: Try(h) :: K;σs)
⇒* ⟨raise Error(e₂) | Cleanup(Err(Error(e₁)),s₀) :: Try(h) :: K;σs′⟩
→ ⟨raise Error(e₂) | Try(h) :: K;σs′[scope := s₀]⟩
→ call(h,[record(e₂,σs′[scope := s₀])],K;σs′[scope := s₀])
```

Successful release restores `e₁`. The handler sees the winning error with
scope restored and `Try` absent. An enclosing capture still receives release
and handler writes; its own outcome decides whether bytes survive.

## Run boundaries and space

**The top level is a sequence of closed machines, not a growing continuation.**
`run_phrases` runs `Phrase::{Define, Run}`; landed definitions survive later
failure. Block-local binds do not escape.

- Before any phrase, imported residual data is readmitted at its solved type.
  A stored name delaying a weak residual beneath a thunk or handle is
  statically unusable in a later unit, following μ-equations. Admission
  executes no code and consumes no handle; frames check no result contract.
- `evaluate(M,ρ)` seeds `C(M,ρ)` over `ε`; `apply(f,args)` seeds `call`,
  including with no arguments. `run` settles the seed before stepping.
  A value finishes in `settle`; a λ finishes boxed in `step`; a break finishes
  only after all frames unwind. An empty stack alone does not finish code.
- β consumes its `Arg`; force and recursive entry retain no consumed call
  frame. Tail recursion can use bounded computational stack, not necessarily
  bounded data. Pending binds, cleanup and captures retain their work.
- `Slot`, checked by `reserve` / `reserve_n`, guards every computational push
  against `session.stack_limit` (default 100 000, `--recursion-limit`). Checks
  precede installation; cap failures unwind the then-current stack.
- Cancellation is cooperative at binds, sequences, application, β, recursion,
  guard entry and host poll points. Neither an arrow nor a step promises
  constant work or one poll. Fused settlement adds no poll of its own.
- Higher-order builtins nest machines on the host stack;
  `NESTED_MACHINE_LIMIT` bounds this independently. Builtin bodies receive no
  caller lexical environment. No frame crosses a thread or marshalling boundary.

**Provenance and panic undo are implementation work erased by the equations.**

- Raising points claim visible names and spans; `Exec` / `App` use innermost
  dispatch or admission. `CallSite` restores provenance on success, break and
  panic. Adjacent tail restorations coalesce and consume no computational
  budget; synchronous operations use `at_site`; the run base completes sites
  ([[decisions/261105_the-error-record|the error record]]).
- On panic, `run` abandons frames top-down and resumes the panic.
  `Frame::abandon` restores sinks, scopes, handlers, grants, cradle and
  provenance, abandons redirects and closes audits. It **does not run Guard
  release code**. The framed entry restores its checkpoint; a panic remains
  a defect, not a catchable error.

## Source anchors

See [[map/core/evaluator|evaluator]], [[map/core/ir|IR]] and
[[map/core/shell-state|shell state]] for the symbol map:

- `ir.rs`: `Val`, `CompKind`, `Prim`, `Node`, `GroupNode`.
- `shell/evaluator/machine.rs`: `State`, `Next`, `Machine`, `Frame`, `step`,
  `settle`, `eval_rules`, `resume`, `unwind`, `boxed`, `force`, `force_val`.
- `shell/evaluator/pattern.rs`: all-or-nothing destructuring.
- `shell/runtime/{value,env,signature}.rs`, `core/src/builtin.rs`: value closures, persistent
  `ρ`, separate `Σ`, cached η-expansions.

The rationale is [[design/cbpv|CBPV]]; SPEC §§5, 7, 13 and 17 state the
user-visible contract.
