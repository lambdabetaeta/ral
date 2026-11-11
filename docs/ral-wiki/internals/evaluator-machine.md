---
verified_at_commit: 22b85799e5e435dff38ea1a2e7ec56d443d9f62b
verified_at_date: 2026-11-11
anchors: [Machine, step_eval, eval_rules, step_return, step_halt, Frame, Focus, Terminal, Closure, Closure::new, Node::new, Env, restrict, Signature, lookup, form, run_phrases, Phrase, evaluate, apply, reserve, PipeNode, NESTED_MACHINE_LIMIT, force, force_val, Occ::of, Frame::Gather, BroodGuard, exit_brood, Frame::Acquire, Frame::Guard, Frame::Cleanup, guard_release, exit_case, ExitCase]
---

# The evaluator: a CEK machine for CBPV

**The machine steps a computation closure against a continuation; lexical
bindings travel with code, while shell effects act on the caller's live state.**
This is a *CEK machine*: control, environment and continuation. For
[[design/cbpv|call-by-push-value]], control must distinguish returning a value
from reaching a lambda that awaits an argument.

- The rules below describe checked `Val` / `CompKind` IR, **not surface ral**.
  Elaboration and checking have already made operand sequencing, capture and
  returned-thunk application explicit
  ([[internals/compilation-ladder|compilation ladder]],
  [[invariants/ir-pure-cbpv|IR invariant]]).
- `Machine { focus, stack }` steps against `&mut Shell` and `&Mooring`.
  Its three modes are `Eval`, `Return` and `Halt`; `Return` has three terminals:
  `Value`, `Lambda` and `Native`.
- Read the rules as an operational account of current Rust, not a proof that
  every Rust execution refines Agda. The substitution-based CK machine in
  `dev/agda/Extrinsic/Semantics/Machine.agda` is the core correspondence
  boundary; environments here implement lexical binding rather than literal
  substitution. Host operations, dynamic shell state and resource limits need
  their own account. Residual-function Box awaits an Agda follow-up, and native
  terminals are not mechanised. The acquiring `guard` is owned Agda divergence D4
  ([[decisions/261111_a-guard-acquires|a-guard-acquires]]). See the
  [[decisions/261102_the-checked-ir-is-the-calculus|correspondence ADR]].

## Syntax and notation

**Values are inert; computations either produce a value or await an argument.**
The following is a reading key for the control fragment, not a replacement for
`core/src/ir.rs` or `docs/SPEC.md` §17.

```text
Value types       A, B ::= Unit | Bool | Integer | String | Bytes
                        | List A | Map A | Record r | Variant r
                        | U C | Handleᵍ A | …
Computation types C    ::= Fᵍ A | A → C

Value terms       V, W ::= c | x | thunk M | [V₁,…,Vₙ]
                        | record{ℓᵢ:Vᵢ} | map{ℓᵢ:Vᵢ} | tag ℓ V | …
Computations      M, N ::= return V | force V | λx. M | M · (V₁,…,Vₙ)
                        | M to p. N | M ⨟ N | rec G i
                        | if V then Wₜ else W𝒻 | case V of {ℓᵢ ↦ Wᵢ}
                        | prim q(V⃗) | try V W | guard A V W | capture M
                        | shell forms
```

- `U C` is the value type of a suspended computation; `Fᵍ A` is a computation
  producing `A`; `A → C` is a computation awaiting an `A`, **not a value type**.
  A lambda is therefore not a thunk. `g` is a producer grade (`p`, `w`, or a
  variable); it does not classify all effects. Output grade `w` pairs only
  with `Unit` ([[design/capture|grades and capture]]).
- `c` is a literal; `x` is a lexical name, with no surface `$` here. `p` is a
  pattern (wildcard, name, list with optional rest, or record). `G` is a shared
  group `[(x₀,M₀), …, (xₙ₋₁,Mₙ₋₁)]`. Static labels and primitive metadata are
  implicit in `q`; protection sites and spans are omitted from this grammar.
- Arms of `if` are values of type `U C`; a `case` arm is a thunk of a
  payload-taking function. `try` and `guard` likewise take suspended bodies.
  `capture`, `Bind`, `Seq`, `App` and redirects instead contain computation
  syntax where their IR constructors say so.

A configuration makes the three distinct stores of information visible:

```text
⟨E(M,ρ), K; σ⟩       Eval: computation M in lexical environment ρ
⟨R(t),   K; σ⟩       Return: terminal t meeting its continuation
⟨H(b),   K; σ⟩       Halt: break b climbing its continuation

t ::= Value(v) | Lambda(λx.M,ρ) | Native(b,v⃗)
b ::= Error(e) | Escape(a)
K ::= ε | F :: K                  leftmost frame is the TOP
```

- Lowercase `v` is a *formed runtime value*, uppercase `V` a value term.
  `ρ[x ↦ v]` is a persistent extension, not mutation of any captured `ρ`.
  Runtime `Binding` also carries an optional type scheme; the rules omit it.
- `Σ` is the fixed `Signature` of natives and frozen prelude. Every rule is
  parameterised by `Σ`; it is neither part of `ρ` nor mutable store `σ`.
  `μ` denotes the run's fixed `Mooring` when cleanup grace or host work needs it.
- `σ` abbreviates the relevant live `Shell` state: cwd, environment overrides,
  handlers, grants, I/O sinks, scopes, workers, audit and session state.
  It is not a heap captured in a closure. External effects are not rolled back
  merely because a frame is popped.
- `F :: K` is written top-first, opposite Rust's `Vec` display order: Rust
  pushes at the end and pops the end. Unless shown, `σ` is unchanged.

**The arrows expose control, not a cycle-accurate trace.** `→` is a displayed
transition after erasing spans and provenance bookkeeping. `⇒*` combines
transitions. `enter(v,K,σ)` and `enterᵥ(V,ρ,K,σ)` below are helper results,
not additional `Focus` constructors or necessarily extra calls to `step`.
Native and shell operations are opaque host computations: one `eval_rules`
arm may do substantial work or re-enter another machine.

- Success rules presume formation, reservation and polling succeed.
  The sections below state their failure behaviour, not an implicit rollback.
- `step` pops a frame **before** `step_return` or `step_halt`. If any helper
  returns a `Break`, `step` installs `H(b)` with the stack and shell state
  **as they then stand**. Already pushed frames remain; already popped ones
  do not reappear. This matters for failed forcing, matching and cleanup.

## Values and environments

**Forming a value does not execute a computation or read ambient shell state.**
Write `⟦V⟧ρ,Σ = v` for `form(V, &env, &sig)`; formation can fail, notably on
an undefined name.

```text
⟦c⟧ρ,Σ         = c
⟦x⟧ρ,Σ         = ρ(x), otherwise Σ(x), otherwise Error(undefined x)
⟦thunk M⟧ρ,Σ   = Thunk(M, ρ↾occ(M))
⟦tag ℓ V⟧ρ,Σ   = Variant(ℓ, ⟦V⟧ρ,Σ)
```

- `Node::new` computes `occ` once through `Occ::of`: the names mentioned,
  including bound names, an over-approximation rather than exact free variables. `Closure::new`
  restricts the captured environment to that set. The transient `E(M,ρ)`
  instead holds the whole environment in which `M` was reached.
- Plain list, record and map literals are value closures too. Inspection
  distributes their saved environment one layer at a time. A directly
  mentioned name answered only by `Σ` requires eager formation, because
  literal inspection reads `ρ` alone. `Σ` lookup prefers prelude to natives.
- `Val::Protected` wraps a formed value with a delayed obligation; admission
  belongs at its boundary. `Prim` computes on values whose operand computations
  have already run in surrounding binds. Neither mechanism forces arbitrary
  thunks as part of ordinary value formation.
- Sinks, cwd, handlers, worker ownership and leases never enter a closure.
  Forcing restores lexical bindings, **not shell state**
  ([[design/scoping|scoping]]).

## Application and force

**Lambda and native terminals consume arguments; no returned value is auto-forced.**
A residual function is boxed when it finds no argument.

```text
⟦V⟧ρ,Σ = v
──────────────────────────────────────────────────────── return
⟨E(return V,ρ), K; σ⟩ → ⟨R(Value(v)), K; σ⟩

⟨E(λx.M,ρ), K; σ⟩ → ⟨R(Lambda(λx.M,ρ)), K; σ⟩        lambda

enter(Thunk(M,ρc), K,σ) = ⟨E(M,ρc), K; σ⟩
enter(NativeValue(b,v⃗),K,σ) = ⟨R(Native(b,v⃗)), K; σ⟩   |v⃗| < arity(b)
enter(NativeValue(b,[]),K,σ) = invoke(b,[],K,σ)            arity(b) = 0
enter(v,K,σ) = ⟨H(Error(cannot-force)), K; σ⟩             otherwise

enterᵥ(thunk M,ρ,K,σ) = ⟨E(M,ρ), K; σ⟩                 literal force
enterᵥ(V,ρ,K,σ) = enter(⟦V⟧ρ,Σ,K,σ)                    otherwise

⟨E(force V,ρ), K; σ⟩ → enterᵥ(V,ρ,K,σ)                  force

vᵢ = ⟦Vᵢ⟧ρ,Σ  (i = 1,…,n)
──────────────────────────────────────────────────────── app
⟨E(M · (V₁,…,Vₙ),ρ), K; σ⟩
  → ⟨E(M,ρ), Arg(v₁) :: … :: Arg(vₙ) :: K; σ⟩

⟨R(Lambda(λx.M,ρc)), Arg(v) :: K; σ⟩
  → ⟨E(M,ρc[x ↦ v]), K; σ⟩                             β

⟨R(Native(b,v⃗)), Arg(w) :: K; σ⟩
  → ⟨R(Native(b,(v⃗,w))), K; σ⟩                         |v⃗| + 1 < arity(b)
  → invoke(b,(v⃗,w),K,σ)                                |v⃗| + 1 = arity(b)

⟨R(Value(v)), Arg(w) :: K; σ⟩
  → ⟨H(Error(not-a-function)), K; σ⟩                    not β

box(Value(v))         = v
box(Lambda(λx.M,ρ))   = Thunk(λx.M,ρ↾occ(λx.M))
box(Native(b,v⃗))     = NativeValue(b,v⃗)

⟨R(Lambda(λx.M,ρc)), To(y,N,ρ) :: K; σ⟩
  → ⟨E(N,ρ[y ↦ Thunk(λx.M,ρc↾occ(λx.M))]), K; σ⟩      Box
```

- `force_val` recognises a **literal** `Val::Thunk` and enters its body in
  the current `ρ`, without allocating a stored closure. `force` on an already
  formed thunk enters its saved `ρc`. Neither pushes an argument or a shell
  restoration frame. Provenance frames are discussed below.
- `App` has `n ≥ 1` arguments. It forms them in order, pushes them
  right-to-left, then evaluates its computation head. Operand effects have
  already been sequenced by the checker. A call with `n` arguments reserves
  all `n` frames together.
- `Terminal::into_value` implements `box` for every value demand and the
  end of a run. A residual λ holds the parameters already bound, restricted
  by `Occ::of`; its body has not started. Pass-through frames preserve the
  terminal. A tail call still consumes its caller's remaining arguments.
- A native holds its entry and, for positive arity, fewer arguments than its
  arity. Zero arity holds none and runs on force. Saturation invokes its body
  without constructing IR. Boundary builtins instead stay
  η-expanded by inference: a partial boundary is a λ-closure over an `Exec`
  carrying its per-use checked site.
- Calling a formed value is the helper `call(f,[v₁,…,vₙ])`: push arguments,
  then `enter(f,…)`. A force failure leaves those arguments to unwind.
  Bound command heads, handlers and host `apply` share this rule. A resolved
  native `Exec` can dispatch directly when held plus new arguments saturate it.
- A returned thunk is a `Value`, not a lambda. The checked split is
  `M to h. (force h) · (V₁,…,Vₙ)`, never application through a returned
  value. Every `let` runs its right-hand side; `To` receives the residual
  function boxed ([[invariants/ir-pure-cbpv|IR invariant]]).

Native invocation is deliberately not a value-only primitive:

```text
run_native(b,v⃗,μ,σ) = (Ok(v),σ′)
──────────────────────────────────────────────────────── native
invoke(b,v⃗,K,σ) = ⟨R(Value(v)), K; σ′⟩
```

A native may write, fail, escape or apply a callback on a nested machine.
A `Break` result gives `H(b)` with the state after that host operation.
`invoke` locates failures at the saturating call, named by the builtin.
A partial native seals by name with its arguments; no builtin code enters IR.

## Continuations

**Syntax that resumes later carries its own environment.** `To` and `Seq`
frames save the caller's `ρ`; bindings made while evaluating their first
computation do not silently become the rest's environment.

```text
⟨E(M to p. N,ρ), K; σ⟩ → ⟨E(M,ρ), To(p,N,ρ) :: K; σ⟩

match(p,box(t)) = θ
──────────────────────────────────────────────────────── bind
⟨R(t), To(p,N,ρ) :: K; σ⟩ → ⟨E(N,ρ ⊕ θ), K; σ⟩

match(p,box(t)) = Error(e)
──────────────────────────────────────────────────────── mismatch
⟨R(t), To(p,N,ρ) :: K; σ⟩ → ⟨H(Error(e)), K; σ⟩

⟨E(M ⨟ N,ρ), K; σ⟩ → ⟨E(M,ρ), Seq(N,ρ) :: K; σ⟩
⟨R(t), Seq(N,ρ) :: K; σ⟩ → ⟨E(N,ρ), K; σ⟩
```

- `match` means `pattern::destructure`: it collects **all** bindings before
  installing any; `ρ ⊕ θ` extends in pattern order. A list without rest
  requires exact length, a list with rest at least the named prefix. A length
  mismatch can occur in checked code because list types do not encode length.
- A mismatch occurs after `To` was popped: the tail is not run, no partial
  binding is installed, and no resurrected `To` adds an abandoned-tail fact.
  Destructuring lambda parameters is compiled as body binds at their own spans.
- `To` binds `box(t)`; `Seq` discards it. Checked sequences demand producers,
  so they do not receive residual functions. A written `let _` remains `Bind`
  and therefore has the normal binding capture rule at checking time.
- On an incoming halt, `Arg` and `Protect` simply propagate it; `To` and
  `Seq` abandon the rest. Only written lets and sequences mark the
  error's abandoned-tail fact; compiler-generated binds do not. Escapes have
  no such report-only facts.

## Choice, recursion and primitives

**Choice selects suspended code; recursion re-enters a shared group rather
than substituting rebuilt bodies.**

```text
⟦V⟧ρ,Σ = true
⟨E(if V then T else F,ρ), K; σ⟩ → enterᵥ(T,ρ,K,σ)

⟦V⟧ρ,Σ = false
⟨E(if V then T else F,ρ), K; σ⟩ → enterᵥ(F,ρ,K,σ)

⟦V⟧ρ,Σ = Variant(ℓ,v)       arm(ℓ) = W
──────────────────────────────────────────────────────── case
⟨E(case V of arms,ρ), K; σ⟩ → enterᵥ(W,ρ,Arg(v) :: K,σ)

G = [(xⱼ,Mⱼ)]ⱼ       ρ₀ = ρ↾occ(G)
ρG = ρ₀[xⱼ ↦ Thunk(rec G j,ρ₀)]ⱼ
──────────────────────────────────────────────────────── rec
⟨E(rec G i,ρ), K; σ⟩ → ⟨E(Mᵢ,ρG), K; σ⟩

v⃗ = ⟦V⃗⟧ρ,Σ       δq(v⃗) = Ok(v)
──────────────────────────────────────────────────────── prim
⟨E(prim q(V⃗),ρ), K; σ⟩ → ⟨R(Value(v)), K; σ⟩
```

- `if` checks for `Bool` and forces only its chosen arm; it does not form
  the unchosen arm. `case` requires a variant and a matching label, pushes
  the payload, then forces its arm. A bare tag supplies `Unit`. A malformed
  condition, scrutinee or missing arm is an error; a failure forcing a
  selected case arm retains the newly pushed `Arg`.
- The recursive siblings share `ρ₀` and `G`; they do **not** capture a
  cyclic environment containing themselves. Forcing a sibling enters
  `rec G j`, which reinstalls the group bindings. The selected member reuses
  its `Rec` node; other members get nodes referring to the same group.
- `δq` abstracts arithmetic, comparison, indexing, rendering, decoding and
  collection assembly. It may fail (`H(Error(e))` on the current stack) and
  may cost O(data), but does not execute an operand computation. A primitive
  is not command dispatch or native invocation. Runtime cancellation checks
  inside value-processing helpers are also outside this pure `δq` notation.
- `Decode` strips one trailing LF or CRLF and checks UTF-8 strictly. It drops
  the environment before taking over the byte buffer. Capture insertion and
  decoding policy belong to [[design/capture|grades and capture]], not to
  an implicit action of `To`.

## Failure, recovery and cleanup

**Errors and escapes share unwinding, but only errors are caught by `Try`.**
`Error(e)` carries the language's failure; `Escape(a)` carries such control
as exit or cancellation. Neither is an IR computation constructor.

```text
v = ⟦V⟧ρ,Σ       h = ⟦W⟧ρ,Σ
⟨E(try V W,ρ), K; σ⟩ → enter(v,Try(h) :: K,σ)

⟨R(t), Try(h) :: K; σ⟩ → ⟨R(t), K; σ⟩
⟨H(Error(e)), Try(h) :: K; σ⟩
  → enter(h,Arg(record(e,σ)) :: K,σ)
⟨H(Escape(a)), Try(h) :: K; σ⟩ → ⟨H(Escape(a)), K; σ⟩
```

- Both body and handler are formed **before** installing `Try`. Once
  installed, it can catch a failure forcing its body. On a catch, `Try` is
  already popped before reserving the argument and forcing the handler:
  **a handler's own failure is not caught by the same `Try`**.
- `record(e,σ)` is the public `[frame, message, status, site]` value, a
  witnessed map from which `raise` recovers the fact, not discarded bytes or
  abandoned-tail accounting. `Try` passes a `Lambda` terminal
  through unchanged on success, as it does `Value` or `Native`. A block that
  leaves `try` unrun runs outside it; its later failures are not caught there.

**Guard shields acquisition and installs release before a body poll.** Let
`o ::= Ok(v) | Err(b)`, `resume(Ok(v)) = R(Value(v))`,
`resume(Err(b)) = H(b)`. Let `shield(σ,μ.grace) = (σs,s₀)` install a
shielded scope and return the displaced scope `s₀`. `call` is the
push-arguments-then-force helper above.

```text
room(3,K)       poll(σ) = ()
a = ⟦A⟧ρ,Σ     k = ⟦V⟧ρ,Σ     r = ⟦W⟧ρ,Σ
shield(σ,μ.grace) = (σs,s₀)
⟨E(guard A V W,ρ), K; σ⟩ → enter(a,Acquire(k,r,s₀) :: K,σs)

v = box(t)
⟨R(t), Acquire(k,r,s₀) :: K; σ⟩
  → call(k,[v],Guard(r,v) :: K,σ[scope := s₀])
⟨H(b), Acquire(k,r,s₀) :: K; σ⟩ → ⟨H(b), K; σ[scope := s₀]⟩

shield(σ,μ.grace) = (σs,s₀)       v = box(t)
⟨R(t), Guard(r,u) :: K; σ⟩
  → call(r,[u,exit_case(Ok(v))],Cleanup(Ok(v),s₀) :: K,σs)

shield(σ,μ.grace) = (σs,s₀)
⟨H(b), Guard(r,u) :: K; σ⟩
  → call(r,[u,exit_case(Err(b))],Cleanup(Err(b),s₀) :: K,σs)

⟨R(t), Cleanup(o,s₀) :: K; σ⟩
  → ⟨resume(o), K; σ[scope := s₀]⟩
⟨H(b′), Cleanup(o,s₀) :: K; σ⟩
  → ⟨H(b′), K; σ[scope := s₀]⟩
```

- Entry reserves room for the deepest stack: `Cleanup` and two `Arg`s.
  Popping `Acquire` or `Guard` returns to entry depth; provenance frames do
  not count. Every later guard reservation asserts that room rather than
  providing a fallible exit while holding the resource.
- A pending stop runs no acquisition. A halting acquisition restores scope
  and runs no release. On success, scope restoration and `Guard` installation
  have **no poll between them**; `force` polls nothing and the body's β-step
  polls after its `Arg` is popped, with `Guard` already below it.
- `guard_release` is shared by return and halt. `exit_case` borrows the saved
  outcome and constructs `ExitCase<ErrorFact>` once: `ok` has no body value,
  `err` is witnessed, `exited` carries the byte and `stopped` the cause.
  Release cannot consume the ending; any release error or escape overrides it.
- `Acquire` and `Guard` receive `box(t)`; `Cleanup` ignores its returned
  terminal. Checked acquisition, body and release operands require producers,
  so none receives a residual function in a well-typed execution.
- `Acquire` and `Cleanup` restore scope on return, halt and abandonment.
  Acquisition and release share one grace from the strike; a late acquisition
  can leave release no time, and either phase can leak a held resource.
  The shield is cooperative, not protection from terminal signals.
- Shielding changes `scope`, not `cradle`: stages follow the current scope,
  but a worker follows its owner's entry scope. A `gather` opened inside
  acquisition or release captures the shield and owns masked workers;
  births into an enclosing gather, worker's body or session do not acquire
  this shield. See [[decisions/261111_a-guard-acquires|a-guard-acquires]] and
  [[internals/cancellation|cancellation]].

## Capture

**Capture yields bytes or no captured output: it never exposes a partial prefix.**
Let `freshbuf()` return a buffer sink `β`; `s` is the displaced stdout sink.
`take(β)` reports retained bytes, whether they are whole, and total byte count.

```text
s = σ.stdout       β = freshbuf()
⟨E(capture M,ρ), K; σ⟩
  → ⟨E(M,ρ), Capture(s,β) :: K; σ[stdout := β]⟩

take(β) = (bytes, whole, total)
⟨R(t), Capture(s,β) :: K; σ⟩
  → ⟨R(Value(Bytes(bytes))), K; σ[stdout := s]⟩          if whole
  → ⟨H(Error(capture-overflow)), K; σ[stdout := s]⟩      otherwise

⟨H(b), Capture(s,β) :: K; σ⟩
  → ⟨H(discard(b,total(β))), K; σ[stdout := s]⟩
```

- `Capture` ignores the entire returned terminal `t`, not just a value's
  payload. Even a bare `Lambda` reaches this Rust return branch, although
  checked capture operands are output computations. It captures **writes**,
  not rendering of the body's result; returning a string writes no bytes.
- On halt, the buffer is dropped without replay to `s`. `discard` adds the
  actual byte count saturatingly to an error's unwind report; on an escape
  it is the identity. No partial output enters the handler's error record.
- On success with an overflowed buffer, stdout is restored first and a
  capture error is raised; the prefix is not returned or replayed. The
  `Capture` frame has already gone, so this is not a second pass through
  that frame's halt rule.
- This frame concerns one computation's byte channel. Process draining,
  detached pumps and worker output have separate lifetimes; see
  [[internals/output-capture-and-detachment|output capture and detachment]].

## Shell frames and terminal handling

**A shell frame owns its undo, not a snapshot of the whole shell.** Installing
`Within`, `Grant`, `Audit` or `Gather` forms its inputs, reserves its frame,
installs the dynamic state, pushes the undo and then forces the body. A force
failure therefore sees that frame. `Redirect` evaluates computation syntax
directly after installing nonempty redirects.

| Frame | On `R(t)` after pop | On `H(b)` after pop |
| --- | --- | --- |
| `Redirect` | Settle with commit; pass `t` if settlement succeeds | Settle with abort; ignore settlement failure and preserve `b` |
| `Within`, `Grant`, `Unmask` | Apply undo; pass `t` | Apply undo; preserve `b` |
| `Protect` | Admit/wrap `box(t)` at the site | Preserve `b` |
| `Audit` | Close trail; return report of `box(t)` | Close trail; turn error into report, propagate escape |
| `Gather` | Restore cradle; join `box(t)` through `exit_brood` | Restore cradle; end brood; preserve `b` |
| `CallSite` | Restore provenance; pass `t` | Locate break if needed, restore provenance; preserve break |

- `Within` restores environment overrides, cwd and handlers; `Grant` removes
  its own layer by position; `Unmask` restores the temporarily removed handler
  ([[internals/handler-dispatch|deep, self-masking dispatch]]). A lambda
  passes through these frames and `Redirect`; value-demanding frames box it.
  Checked scope bodies require producers, so an under-applied call cannot
  leave them unrun. An explicitly returned block runs later outside the scope.
- Redirect settlement does not undo bytes already streamed to a file. Atomic
  writes can abort; streaming writes are already committed. Return-time
  settlement failure becomes a halt on the remaining stack.
- `Gather` holds a `BroodGuard` and displaced cradle, not a different evaluator.
  It captures the current scope as the new cradle's `under`; its body runs on
  this machine. Joining/stopping follows
  [[invariants/a-worker-ends-with-its-children|worker ownership]].
- `Exec` dispatch, `Pipeline` launch/join, `Spawn`, ambient `Tilde` and native
  bodies are host operations rather than reductions of pure CBPV syntax.
  `Pipeline` runs a `PipeNode` in one shell arm; ral stages get fresh machines
  on their threads, external stages are processes
  ([[internals/pipeline-execution|pipeline execution]]).

## Two reductions

**Following the stack shows both lexical scope and outcome precedence.**
These traces suppress unchanged `σ`, provenance, successful reservations and
polls, using the helper conventions above.

### A saved thunk keeps its lexical name

Let `ρ₀ = [x ↦ 7]` and form `f = Thunk(λy.return x, [x ↦ 7])`.
Now let `ρ₁ = ρ₀[x ↦ 99, f ↦ f]`:

```text
⟨E((force f) · (1),ρ₁), ε⟩
→ ⟨E(force f,ρ₁), Arg(1) :: ε⟩
→ ⟨E(λy.return x,[x ↦ 7]), Arg(1) :: ε⟩
→ ⟨R(Lambda(λy.return x,[x ↦ 7])), Arg(1) :: ε⟩
→ ⟨E(return x,[x ↦ 7,y ↦ 1]), ε⟩
→ ⟨R(Value(7)), ε⟩
```

- The caller's `x = 99` never substitutes for the captured `x = 7`.
  A literal `force (thunk (return x))` in `ρ₁`, by contrast, immediately
  enters `return x` in `ρ₁` and returns `99`.
- If that saved thunk instead invokes `cwd`, it reads the **caller's current**
  cwd from `σ`, not the directory at formation. A `cd` in the forced body
  persists just as in a lambda body, unless an enclosing dynamic scope owns
  an explicit restoration. Lexical closure is not an implicit subshell.

### Release overrides the body before a catch

Suppose a body has failed with `e₁` inside `guard`, inside `try`. Resource
`u` is held; release `r` fails with `e₂` after receiving it and the ending:

```text
⟨H(Error(e₁)), Guard(r,u) :: Try(h) :: K; σ⟩
→ call(r,[u,exit_case(Err(Error(e₁)))],
        Cleanup(Err(Error(e₁)),s₀) :: Try(h) :: K,σs)
⇒* ⟨H(Error(e₂)), Cleanup(Err(Error(e₁)),s₀) :: Try(h) :: K; σs′⟩
→ ⟨H(Error(e₂)), Try(h) :: K; σs′[scope := s₀]⟩
→ enter(h,Arg(record(e₂,σs′[scope := s₀])) :: K,σs′[scope := s₀])
```

Successful release would restore `Error(e₁)` instead. The handler sees the
winning error, with the old scope restored and `Try` absent. If an enclosing
capture is still on `K`, release and handler writes still go to that buffer;
its eventual return/halt decides whether the bytes become a value or vanish.

## Run boundaries and space

**The top level is a sequence of closed machines, not one growing continuation.**
`run_phrases` runs `Phrase::{Define, Run}` against the session environment.
Each landed `Define` is installed immediately; a later failure keeps earlier
definitions. A block instead nests `Bind` and `Seq`, whose lexical bindings
do not escape.

```text
evaluate(M,ρ) starts at ⟨E(M,ρ), ε; σ⟩
apply(f,v⃗) starts by pushing v⃗ and forcing f, even when v⃗ is empty

⟨R(Value(v)), ε; σ⟩           finishes with Ok(v)
⟨R(Lambda(λx.M,ρ)), ε; σ⟩     finishes with Ok(Thunk(λx.M,ρ↾occ(λx.M)))
⟨R(Native(b,v⃗)), ε; σ⟩       finishes with Ok(NativeValue(b,v⃗))
⟨H(b), ε; σ⟩                 finishes with Err(b)
```

- An empty stack with `Eval` is **not** finished. The loop stops only at
  `Return` or `Halt` with no frames. Returning a thunk finishes with that
  thunk; it is never implicitly forced at the run boundary.
- β consumes `Arg` before entering the body. Forcing and recursive entry
  retain no consumed call frame. A tail-recursive loop can therefore use
  bounded computational stack; a call under a pending bind, cleanup,
  capture or other continuation still retains that pending work. This is a
  stack claim, not a bound on data or captured environments.
- Every computational push goes through `Slot`, checked by
  `reserve`/`reserve_n` against `session.stack_limit` (default 100 000,
  configurable by `--recursion-limit`). Installation is checked before its
  effect; `push_args` reserves the whole argument vector. Cap failures enter
  ordinary error unwinding on the then-current stack.
- Cancellation is cooperative: binds, sequences, application, β, recursion
  and guard entry poll at their designated points, as do host operations. The equations are
  not a promise of one poll or constant work per displayed arrow.
- Higher-order natives can enter nested `evaluate`/`apply` machines on the
  **host** stack. `NESTED_MACHINE_LIMIT` bounds that independently of the
  continuation cap. No native receives the caller's lexical environment.
  No frame crosses a thread or sealed-engine boundary.

**Provenance and panic undo are real implementation work erased by the core
rules.** They cannot be inferred from the pure continuation equations alone.

- Raising points locate errors with a visible operation name/span;
  `Exec`/`App` use their innermost dispatch/admission rather than a fallback
  stamp. The `frame` field names that operation, not necessarily a command.
- `CallSite` restores the caller's provenance register on return, halt and
  panic. Adjacent tail-call restorations coalesce and do not count toward the
  computational stack cap. Synchronous operations use `at_site`; a run base
  completes otherwise unclaimed sites
  ([[decisions/261105_the-error-record|the error record]]).
- The loop catches panics, abandons remaining frames top-down and resumes the
  panic. `Frame::abandon` restores sinks, scopes, handlers, grants and cradle,
  abandons redirect state and closes audit scopes. It does **not** evaluate
  arbitrary `Guard` release code during a panic. The framed run entry restores
  its checkpoint; panics remain defects, not catchable language errors.

## Source anchors

**The implementation map is the place to follow symbols beyond these rules.**
See [[map/core/evaluator|evaluator]], [[map/core/ir|IR]] and
[[map/core/shell-state|shell state]]; the load-bearing source seams are:

- `core/src/ir.rs`: `Val`, `CompKind`, `Prim`, `Node` and `GroupNode`.
- `core/src/shell/evaluator/machine.rs`: `Focus`, `Terminal`, `Frame`,
  `eval_rules`, `step_return`, `step_halt`, `force_val`, `force` and run entry.
- `core/src/shell/evaluator/val.rs` and `pattern.rs`: formation and
  all-or-nothing destructuring.
- `core/src/shell/runtime/{env,closure,signature}.rs`: persistent `ρ`,
  occurrence-restricted closures and separate `Σ`.

The reason for the split is in [[design/cbpv|CBPV]]; the user-visible contract
is `docs/SPEC.md` §§5, 7, 13 and 17.
