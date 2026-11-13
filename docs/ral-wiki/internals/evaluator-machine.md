---
verified_at_commit: 9290eb03
verified_at_date: 2026-11-13
anchors: [Machine, Stack, Stack::step, Stack::settle, Stack::eval, Stack::eval_rules, Stack::resume, Stack::unwind, Stack::call, Stack::reserve, Stack::guard_release, CompClosure, Next, Frame, Frame::abandon, Frame::Gather, Frame::Acquire, Frame::Guard, Frame::Cleanup, ValClos, ValClos::close, ValClos::thunk, View, BuiltinEntry::eta, BuiltinEntry::residual, CompKind::Builtin, Node::new, Occ::of, Env, restrict, Signature, lookup, run_phrases, Phrase, evaluate, apply, NESTED_MACHINE_LIMIT, force, force_val, BroodGuard, exit_brood, exit_case, ExitCase]
---

# The evaluator: a CEK machine for CBPV

**The machine steps a computation closure or unwinds a break; a produced
value meets its continuation in the same step.** Lexical bindings travel with
code; shell effects act on the caller's live state.

- The rules below describe checked `Val` / `CompKind` IR, not surface ral
  ([[internals/compilation-ladder|compilation ladder]],
  [[invariants/ir-pure-cbpv|IR invariant]]).
- `Machine { cclos, stk }` is the configuration. `CompClosure =
  Settled<(Arc<Comp>, Env)>` is ⟨M, ρ⟩ or `Err(Break)`; there is no value
  register and no runtime `return V` node. `Stack` is K plus the run's
  `Mooring`; `step`, `settle`, `eval`, `resume`, `unwind` and `call` are its
  methods, so `Machine::run` keeps ownership of every frame across a panic.
- Value representation: [[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]].
  Symbol map: [[map/core/evaluator|evaluator]].

## Notation

```text
⟨M,ρ | K;σ⟩        computation closure        ⟨raise b | K;σ⟩     Err(b)
K ::= ε | F :: K    leftmost frame is the top
b ::= Error(e) | Escape(a)
```

- `v` is a value closure ⟨V,ρ⟩; `ρ[x ↦ v]` is persistent extension. `Σ` is the
  fixed `Signature` (prelude, then builtins); lookup is ρ then Σ. `μ` is the
  run's `Mooring`; `σ` is live `Shell` state: cwd, env overrides, handlers,
  grants, sinks, scopes, workers, audit. Popping a frame does not roll back an
  external effect.
- Rule results are not configurations: `C(M,ρ)` is the next closure, `D(v)` a
  value handed down now. `S(n,K;σ)` is `settle`; `meet(t,F,K;σ)` is `resume`
  after popping `F`, for `t` either `C(λx.M,ρ)` or `D(v)`.

```text
S(C(M,ρ),K;σ) = ⟨M,ρ | K;σ⟩            S(Err(b),K;σ) = ⟨raise b | K;σ⟩
S(D(v),ε;σ) = finish Ok(v)             S(D(v),F :: K;σ) = meet(D(v),F,K;σ)
⟨λx.M,ρ | F :: K;σ⟩ → meet(C(λx.M,ρ),F,K;σ)
⟨λx.M,ρ | ε;σ⟩ → finish Ok(boxed)      ⟨raise b | F :: K;σ⟩ → unwind(F,b,K;σ)
⟨raise b | ε;σ⟩ → finish Err(b)
```

- `step` pops before `resume` or `unwind`. A helper failure installs `Err(b)`
  with stack and shell as they then stand.
- `boxed(C(λx.M,ρ)) = ⟨thunk (λx.M),ρ↾occ(λx.M)⟩` and `boxed(D(v)) = v`.
  Pass-through frames preserve `t`; value-demanding frames use `boxed(t)`.

## Values and environments

**Closing a value executes nothing and reads no ambient shell state.**

```text
close(x,ρ,Σ) = lookup(x,ρ,Σ), or Error(undefined x)
close(V,ρ,Σ) = ⟨V,ρ↾occ(V) + Σ's answers for missing direct names⟩    V not a variable
```

- A node's `Occ` (`Occ::of`, computed on first use) over-approximates the names
  it mentions. `close` and `ValClos::thunk` restrict at formation; a component
  read shares the whole saved ρ and never consults Σ, and `View` reads a
  head's constructor only. Names under a thunk resolve ρ then Σ when it runs.
  A list or map built at run time is syntax over `%itemK` variables bound in
  ρ ([[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]]).
- `Env::EMPTY` is refcount-free; small environments are `ThinArc`. Compiled
  bounds: `Env ≤ 16`, `ValClos ≤ 48`, `Frame ≤ 128` bytes.
- No sink, cwd, handler stack, worker ownership or lease enters a capture:
  forcing restores bindings, not shell state ([[design/scoping|scoping]]).

## Application and force

**A builtin forces to a λ-chain ending in δ; partial application is ordinary
β.** For a builtin `c` of arity `n`, `ηc = λa₀.…λaₙ₋₁.δ(b; a⃗)` with `b ↦ c`.

```text
⟨return V,ρ | K;σ⟩ → S(D(close(V,ρ,Σ)),K;σ)
enter(⟨thunk M,ρc⟩,K;σ) = ⟨M,ρc | K;σ⟩
enter(⟨builtin c,ρc⟩,K;σ) = ⟨ηc,{b ↦ ⟨builtin c,ρc⟩} | K;σ⟩
enter(v,K;σ) = ⟨raise Error(cannot-force) | K;σ⟩          otherwise
⟨force V,ρ | K;σ⟩ → enter(close(V,ρ,Σ),K;σ)               literal thunk: ⟨M,ρ | K;σ⟩

vᵢ = close(Vᵢ,ρ,Σ)
⟨M · (V₁,…,Vₙ),ρ | K;σ⟩ → ⟨M,ρ | Arg(v₁) :: … :: Arg(vₙ) :: K;σ⟩
meet(C(λx.M,ρc),Arg(v),K;σ) = ⟨M,ρc[x ↦ v] | K;σ⟩              β
meet(D(v),Arg(w),K;σ) = ⟨raise Error(not-a-function) | K;σ⟩

ρ(b) = builtin c       v⃗ = ρ(a⃗)       run_native(c,v⃗,μ,σ) = (Ok(v),σ′)
⟨δ(b; a⃗),ρ | K;σ⟩ → S(D(v),K;σ′)
```

- `force_val` enters a literal thunk in the current ρ without forming a value
  closure; `force` enters a saved thunk or builtin η. Neither pushes a
  restoration frame or polls.
- `App` closes arguments in order and pushes them right-to-left, reserving all
  frames at once. β polls after consuming its argument. A tail call consumes
  the caller's remaining arguments, so no call frame survives. A value
  meeting `Arg` is an error, never an implicit force.
- `Stack::call` runs a builtin directly when given exactly its arity, else
  pushes the arguments and forces. Arity zero runs on force. Bound heads,
  handlers and host `apply` share it. δ reads the captured entry, never a
  name-dispatched Σ row.
- Boundary builtins run saturated with the checker's `Site`; inference
  expands partial boundaries over `Exec`.
- A residual λ is boxed only at a value demand or run end. A builtin residual
  prints `<native NAME, k of n arguments>`, has type name `Lambda` and crosses
  as builtin name, environment row and supplied count;
  `BuiltinEntry::residual` rebuilds the receiver's η suffix, `ValClos::residual`
  recognises it ([[map/core/transport|transport]]).
- `run_native` may write, fail, escape or apply a callback in a nested
  machine; its break is located at the builtin call.
- A returned thunk is data: checked application through one is
  `M to h. (force h) · V`. Every `let` runs its RHS.

## Sequencing, choice and recursion

**Only `To` and `Seq` carry a computation with its saved ρ.**

```text
⟨M to p.N,ρ | K;σ⟩ → ⟨M,ρ | To(p,N,ρ) :: K;σ⟩
match(p,boxed(t)) = θ:   meet(t,To(p,N,ρ),K;σ) = ⟨N,ρ ⊕ θ | K;σ⟩
match(p,boxed(t)) = Error(e):  meet(t,To(p,N,ρ),K;σ) = ⟨raise Error(e) | K;σ⟩
⟨M ⨟ N,ρ | K;σ⟩ → ⟨M,ρ | Seq(N,ρ) :: K;σ⟩      meet(t,Seq(N,ρ),K;σ) = ⟨N,ρ | K;σ⟩

close(V,ρ,Σ) = Bool(b)      ⟨if V then T else F,ρ | K;σ⟩ → force_val of the chosen arm
close(V,ρ,Σ) = Variant(ℓ,v), arm(ℓ) = W
⟨case V of arms,ρ | K;σ⟩ → ⟨W,ρ | Arg(v) :: K;σ⟩   (W forced in place)

G = [(xⱼ,Mⱼ)]    ρ₀ = ρ↾occ(G)    ρG = ρ₀[xⱼ ↦ ⟨thunk (rec G j),ρ₀⟩]
⟨rec G i,ρ | K;σ⟩ → ⟨Mᵢ,ρG | K;σ⟩               (polls first)

v⃗ = close(V⃗,ρ,Σ)    δq(v⃗) = Ok(v)    ⟨prim q(V⃗),ρ | K;σ⟩ → S(D(v),K;σ)
```

- Destructuring collects all bindings before installing any. A list pattern
  without rest needs exact length. A mismatch occurs after `To` is popped:
  no partial binding, no resurrected frame.
- `To` boxes its input; `Seq` discards it. On a break `Arg` propagates and
  `To`/`Seq` abandon the rest; only errors through written `let`s and
  sequences gain the abandoned-tail fact, compiler binds do not.
- `case` pushes the payload before forcing; a bare tag supplies unit. A
  malformed head or a missing arm is an error.
- Recursive siblings share `G` and `ρ₀`; no environment is cyclic. Force
  reinstalls the group.
- `δq` is value-only: arithmetic, indexing, rendering, decoding, assembly. It
  may fail or cost O(data) and runs no operand computation. `Decode` drops its
  environment before taking the bytes, removes one trailing LF or CRLF and
  checks UTF-8 strictly.

## Failure, recovery and cleanup

**Errors and escapes share unwinding; only errors are caught by `Try`.**
Neither is an IR constructor.

```text
h = close(W,ρ,Σ)       V = thunk M       ⟨try V W,ρ | K;σ⟩ → ⟨M,ρ | Try(h) :: K;σ⟩
meet(t,Try(h),K;σ) = S(t,K;σ)
unwind(Try(h),Error(e),K;σ) = call(h,[record(e,σ)],K;σ)
unwind(Try(h),Escape(a),K;σ) = ⟨raise Escape(a) | K;σ⟩
```

- The handler is closed and the body forced before `Try` is pushed; the popped
  `Try` cannot catch its handler's failure. `record` is the plain
  `[frame, message, status, site]` data, after which the error no longer
  exists; a returned block runs later, outside the catch.

**`guard` shields acquisition and installs release before the body's first
poll.** With `o ::= Ok(v) | Err(b)`, `restore(Ok(v)) = D(v)`,
`restore(Err(b)) = Err(b)`, and `shield(σ,μ.grace) = (σs,s₀)` saving the
displaced scope `s₀`:

```text
room(3,K)   poll(σ) = ()   a,k,r = close(A,ρ,Σ),close(V,ρ,Σ),close(W,ρ,Σ)
⟨guard A V W,ρ | K;σ⟩ → enter(a,Acquire(k,r,s₀) :: K;σs)

meet(t,Acquire(k,r,s₀),K;σ) = call(k,[boxed t],Guard(r,boxed t) :: K;σ[scope := s₀])
unwind(Acquire(k,r,s₀),b,K;σ) = ⟨raise b | K;σ[scope := s₀]⟩
meet(t,Guard(r,u),K;σ) = call(r,[u,exit_case(Ok(boxed t))],Cleanup(Ok(boxed t),s₀) :: K;σs)
unwind(Guard(r,u),b,K;σ) = call(r,[u,exit_case(Err(b))],Cleanup(Err(b),s₀) :: K;σs)
meet(t,Cleanup(o,s₀),K;σ) = S(restore(o),K;σ[scope := s₀])
unwind(Cleanup(o,s₀),b′,K;σ) = ⟨raise b′ | K;σ[scope := s₀]⟩
```

- Entry reserves the deepest need, `Cleanup` plus two `Arg`s; popping
  `Acquire` or `Guard` returns to entry depth, so later pushes cannot fail.
  Provenance frames do not count.
- A pending stop runs no acquisition; a failing acquire runs no release. On
  success, scope restoration and pushing `Guard` have no intervening poll;
  the body's β polls with `Guard` below its argument.
- `exit_case` borrows the saved outcome: `ok` omits the body value, `err` is
  the plain record, `exited` carries the byte, `stopped` the cause. A successful
  release restores the saved outcome; a release error or escape overrides it.
- Acquisition and release share one grace from the strike, so a late
  acquisition can leave release no time. Shielding is cooperative, not
  protection from terminal signals.
- Shields change `scope`, not `cradle`. Stages follow scope; workers follow
  their owner's entry scope; a `gather` opened inside a shield owns masked
  workers ([[internals/cancellation|cancellation]]).

## Capture and shell frames

**Capture returns whole bytes or fails; it exposes no partial prefix.**

```text
s = σ.stdout    β = freshbuf()
⟨capture M,ρ | K;σ⟩ → ⟨M,ρ | Capture(s,β) :: K;σ[stdout := β]⟩
meet(t,Capture(s,β),K;σ) = S(D(⟨bytes,∅⟩),K;σ[stdout := s])           β whole
                         = ⟨raise Error(capture-overflow) | K;σ[stdout := s]⟩   otherwise
unwind(Capture(s,β),b,K;σ) = ⟨raise discard(b,total(β)) | K;σ[stdout := s]⟩
```

- Capture ignores its input, a λ included. On a break it drops the bytes;
  `discard` adds their count, saturating, to an error's unwind report and
  leaves escapes unchanged.
- Pumps, process drains and worker output have their own lifetimes
  ([[internals/output-capture-and-detachment|output capture and detachment]]).

**A shell frame owns its undo, not a snapshot of the shell.** Installation
closes inputs, reserves, installs state, pushes the undo, then forces the body.

| Frame | `meet(t,…)` after pop | `unwind(…,b,…)` after pop |
| --- | --- | --- |
| `Redirect` | commit; `S(t,K;σ′)` | abort; keep `b` |
| `Within`, `Grant`, `Unmask` | undo; `S(t,K;σ′)` | undo; keep `b` |
| `Audit` | close trail; report of `boxed(t)` | close; report an error, pass an escape |
| `Gather` | restore cradle; join `boxed(t)` through `exit_brood` | restore cradle; end brood; keep `b` |
| `CallSite` | restore provenance; `S(t,K;σ′)` | locate; restore provenance; keep `b` |

- `Within` restores cwd, overrides and handlers; `Grant` removes its layer by
  position; `Unmask` restores a removed handler
  ([[internals/handler-dispatch|handler dispatch]]). A scope body's λ passes
  through.
- Redirect commit cannot unstream bytes; atomic writes can abort. A commit
  failure is a break on the remaining stack.
- `Gather` owns a `BroodGuard` and the displaced cradle, with the current
  scope as the new cradle's `under`
  ([[invariants/a-worker-ends-with-its-children|worker ownership]]).
- `Exec`, `Pipeline`, `Spawn`, `Tilde`, `Use` and builtin bodies are host
  operations. A `PipeNode` runs in one shell arm; ral stages enter fresh
  machines on threads and external stages are processes
  ([[internals/pipeline-execution|pipeline execution]]).

## Two reductions

### A saved thunk keeps its lexical name

With `ρ₀ = [x ↦ ⟨7,∅⟩]`, `f = ⟨thunk (λy.return x),ρ₀⟩` and
`ρ₁ = ρ₀[x ↦ ⟨99,∅⟩, f ↦ f]`:

```text
⟨(force f) · (1),ρ₁ | ε⟩ → ⟨force f,ρ₁ | Arg(⟨1,∅⟩) :: ε⟩
→ ⟨λy.return x,ρ₀ | Arg(⟨1,∅⟩) :: ε⟩ → ⟨return x,ρ₀[y ↦ ⟨1,∅⟩] | ε⟩
→ finish Ok(⟨7,∅⟩)
```

Caller `x = 99` never replaces captured `x = 7`; a literal
`force (thunk (return x))` enters in `ρ₁` and returns `99`. A `cd` in the
thunk persists unless an enclosing scope owns restoration: a lexical closure
is not a subshell.

### Release overrides the body before a catch

A body fails with `e₁` holding `u`; release fails with `e₂`:

```text
⟨raise Error(e₁) | Guard(r,u) :: Try(h) :: K;σ⟩
→ call(r,[u,exit_case(Err(Error(e₁)))],Cleanup(Err(Error(e₁)),s₀) :: Try(h) :: K;σs)
⇒* ⟨raise Error(e₂) | Cleanup(Err(Error(e₁)),s₀) :: Try(h) :: K;σs′⟩
→ ⟨raise Error(e₂) | Try(h) :: K;σs′[scope := s₀]⟩
→ call(h,[record(e₂,σs′[scope := s₀])],K;σs′[scope := s₀])
```

A successful release restores `e₁`. The handler sees the winning error with
scope restored; an enclosing capture still receives release and handler
writes.

## Run boundaries and space

**The top level is a sequence of closed machines, not a growing
continuation.**

- `run_phrases` readmits imported residual data at its solved type, then runs
  each `Phrase::{Define, Run}` as its own machine. A landed `Define`
  survives a later failure; block-local binds do not escape.
- `evaluate(M,ρ)` starts `C(M,ρ)` over `ε`. `apply(f,args)` runs `call` first
  and starts a machine only when `call` yields a closure. A value finishes in
  `settle`, a λ finishes boxed in `step`, a break finishes after every frame
  unwinds.
- β consumes its `Arg`, and force retains no consumed frame, so tail
  recursion needs bounded stack, not bounded data. Pending binds, cleanup and
  captures retain their work.
- `Slot`, from `reserve`, guards every computational push against
  `session.stack_limit` (default 100 000, `--recursion-limit`). The check
  precedes installation; a refusal unwinds the stack as it stands.
- Cancellation is cooperative at binds, sequences, application, β, recursion,
  guard entry and host poll points. No step promises constant work or one poll.
- A builtin that applies a function nests a machine on the host stack;
  `NESTED_MACHINE_LIMIT` bounds the nesting. Builtin bodies receive no caller
  ρ, and no frame crosses a thread or marshalling boundary.

**Provenance and panic undo are implementation work the equations erase.**

- Named operations run under `at_site`; `Exec`, `App` and `Force` push a
  `CallSite` that restores provenance on success, break and panic. Adjacent
  call sites coalesce and cost no stack budget
  ([[decisions/261105_the-error-record|the error record]]).
- On a panic `run` abandons frames top-down through `Frame::abandon`, which
  restores sinks, scopes, handlers, grants, cradle and provenance and aborts
  redirects and audits. It does not run guard release code; a panic is a
  defect, not a catchable error.

The rationale is [[design/cbpv|CBPV]]; SPEC §§5, 7, 13 and 17 state the
user-visible contract.
