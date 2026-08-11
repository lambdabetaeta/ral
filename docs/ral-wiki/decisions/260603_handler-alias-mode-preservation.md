---
status: superseded
superseded_by: decisions/260606_alias-head-defines-its-modes
---

> Superseded by [[decisions/260606_alias-head-defines-its-modes|a fresh head defines its own modes]].

> **2026-06-19.** The bare-block handler and alias examples below
> (`{ return 3 }`, `{ echo hi }`) are no longer valid surface forms.
> A per-name handler and every alias must be a unary lambda
> `{ |args| … }`, and a catch-all must be a binary lambda
> `{ |name args| … }`; a bare block, a non-lambda, or a wrong-arity
> lambda is rejected at install time (`docs/SPEC.md` §9.4,
> [[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]).
> The mode-preservation analysis here is unaffected — read its examples
> with the argument-binding form supplied.

# Handlers and aliases preserve a head's pipe signature

A handler reinterprets an operation; it does not retype it. The
[[design/syscalls-are-effects|operation/interpretation separation]] puts the
calling convention on the operation, so the OS reading and any `within`/`alias`
reading must agree on the same signature — exactly as a handler clause in the
algebraic-effects literature must match its operation's signature. **A handler
(or alias) for head `h` must preserve `h`'s `PipeSpec`: the mode pair `[input,
output]` is pinned by unification against `h`'s known spec, while the arm's
*value* type stays whatever scheme inference yields.**

**Narrowed by the payload-route change: exactly one thing is preserved, and the
pin now carries an obligation.**
[[decisions/260809_pipes-are-positional-byte-wires|Pipes are positional byte
wires]] deletes `input` and `output`, so the mode pair this page pins has no
members left. What survives is the reason the pin existed at all: a handler
reinterprets an operation without retyping it, so an arm installed under `h`
must agree with `h` about **where its payload lives** — the returned value, or
stdout. `pin_arm_to_head` unifies the arm's `PayloadRoute` against the head's,
and the two failure modes are `PinFailure::Route` and
`PinFailure::ByteHeadReturnsValue`.

The second is new, and it is the correction of a live defect this page's
implementation carried. Pinning the *bare* route while discarding the arm's
value type let an arm with an open route be grounded `Bytes` under a byte-routed
head while keeping a concrete non-`Unit` value the checker never re-examined —
silent type confusion, a release build printing `""` where the recorded type
said `Int`. WF-2 (`Bytes` implies the value is `Unit`) leaves exactly one
byte-routed computation, and this pin is one of the two operations that land
on it. So the verdict is **narrowed, not reversed**: one route preserved
instead of a mode pair, and the pin discharges the `Unit` pairing in the same
breath as the grounding.

This is the second of four decisions whose end state is a single mode-inference
engine. The arc: session-scheme-continuity
([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]) →
handler/alias mode preservation (here) → IR PipeSpec annotation
([[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]) → the mode pass
made unconditional ([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]).
The through-line is **net code removal**: this page's machinery is one extra
unification per arm, and what it buys is the deletion of the runtime engine's
handler-first lookup in decision three.

## Why the verdict must be transportable

For the checker's mode verdict to reach the evaluator (ADR 3), no dynamic
rebinding may change a head's signature after the check. Today it can, and the
runtime engine compensates:

- `core/src/ty.rs::env_binding_type` consults `shell.lookup_handler` **before**
  scope lookup — it re-infers a head's modes from the live handler arm body,
  because a handler can rebind a head to a body with *different* modes than the
  head's declared signature.
- The static checker reflects the same freedom. A handler arm's scheme is
  inferred from the arm body alone (`typecheck/infer.rs::infer_handler_comp`,
  `handler_comp_scheme`), with *no* unification against the head's spec. A
  byte-output arm gets a byte-output scheme; a value-output arm a value-output
  one.
- Heads that take handlers are exactly the external operations
  ([[design/effects-handlers|effects-handlers]]: handlers intercept external
  commands only; the install sites in `core/src/evaluator/scope.rs::WithinScope::parse`
  and `core/src/types/shell/scope.rs::install_alias` both reject any name that
  is a lexical binding or builtin). An external head's declared spec is
  `F[μ, Bytes]` (`typecheck/infer.rs::external_exec_comp_ty`); a session-bound
  head carries its harvested scheme (ADR 1).

The hazard this leaves in the language, independent of the series: a handler
that flips a head from byte- to value-output silently re-wires pipelines
compiled before the handler existed — spooky action at a distance through the
dynamic context. The rule removes it.

## The rule, by installation taxonomy

The two ways a name acquires a handler frame differ in lifetime, and the
enforcement site follows the lifetime:

- **`within [handlers: …]` frames are dynamic-extent within one turn.**
  `Shell::with_handlers` pushes a frame and removes it by handle on return
  (`core/src/types/value.rs::HandlerStack::push` / `remove_by_handle`); a frame
  never outlives the `eval_top_level` turn that installed it. The checker sees
  the install lexically when the opts map is literal.
- **Aliases install persistent frames.** `install_alias` calls
  `HandlerStack::push_alias` with no paired pop; the frame lives until `unalias`
  (`core/src/types/shell/scope.rs`). An alias *is* cross-turn dynamic rebinding.

### Literal `within` opts → statically

`typecheck/scope.rs::collect_within_handler_bindings` already walks a literal
`handlers: [name: { comp }, …]` map, infers each arm under the runtime calling
convention, and binds the generalised scheme for the body's scope (with the
`reject_handler_for_binding` guard; a catch-all `handler: { comp }` is inferred
for side-effects only, since it names no specific head). Extend it to **unify
the arm's `PipeSpec` against `h`'s known spec** before binding — the external
default `F[μ, Bytes]`, or the session scheme for `h` (ADR 1). A
mode-changing arm fails that unification as a `ModeMismatch`
([[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]]:
modes are equality-constrained, `none` and `bytes` do not unify).

### Alias statements → statically, same unification

A literal `alias name { body }` is already recognised at IR shape and bound as
a generalised handler scheme in the type environment
(`typecheck/infer.rs::alias_statement_shape`, `infer_seq_with_alias_bindings`).
The same `PipeSpec` unification applies: an alias body is an *alternative
interpretation of the same head*, so it preserves the head's modes. **Aliases
are mode-preserving by the same rule, not mode-changing.** This is the simpler
choice and the right one: an alias whose value type is recorded by the existing
generalisation still typechecks, and ADR 1's session-scheme mechanism is *not*
recruited to record a mode change that the rule forbids in the first place.

### Computed opts and runtime-installed aliases → checked at install

A non-literal `within [handlers: $h]` opts value, and an alias installed by a
path the checker never witnesses (rc-config `aliases:` maps in
`ral/src/repl/config.rs`, plugin loads in `ral/src/repl/plugin/load.rs`), are
*invisible to the static check*. The design pressure says install-time
unification of the stored arm's spec against the head's spec; the simpler
choice — consistent with **net code removal** — is to **reject a computed
handler/alias arm whose modes do not match the head**, rather than grow a second
mode-inference path at install time.

- A repository search finds **no computed `within` opts map**: every `within
  [handlers: …]` in the prelude and tests is a literal map, and
  `within_handler_non_literal_value_falls_through` (`core/tests/typecheck.rs`)
  is the lone non-literal case — a *value*, `$h`, not a computed map. Computed
  opts maps are an empty set; rejecting a mode mismatch in them costs nothing
  real.
- Runtime-installed aliases (rc, plugins) carry external-only bodies that
  forward to a command and so are already byte-output `F[μ, Bytes]` — the
  external default. The alias half needs no ADR 3 annotation: ADR 1 already
  computes the arm's *closed scheme at install* (`alias_arm_scheme`, the static
  engine's arm inference under the runtime handler calling convention, seeded
  from the live session) and stores it on the persistent frame's
  `HandlerEntry.scheme` for all three install paths. A closed scheme carries the
  arm's `PipeSpec`, so the mode-preservation check is one unification of the
  stored scheme's `PipeSpec` against the head's spec — implementable now. The
  one inference run at install is the static engine itself, invoked once to
  produce the frame scheme (ADR 1's mechanism); no *second* engine and no
  mode-only re-derivation exists or is added.

**Open point, resolved.** The choice was whether computed `within` opts should
be *rejected outright* (no computed handler maps at all) versus *mode-checked at
install*. The maintainer chose the install-time mode check: **a computed
`within [handlers: $h]` opts map STAYS LEGAL and is mode-checked at install**,
not rejected outright. This is uniform with the runtime-alias case and does not
remove a feature that has no users yet but is cheap to keep honest. The computed-
`within`-opts arm is mode-checked by exactly ADR 1's install-time engine, the
same one the alias half runs — `WithinScope::parse` calls the head-aware
`alias_arm_scheme` seeded from the live session and discards the computed scheme
(`HandlerEntry.scheme` stays `None`), where `install_alias` stores it. Both halves
read no IR node, so this consumer needs no IR annotation; ADR 3 carries no
thunk-root `Wire` slot. [[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]
records the same conclusion from its side.

## What this unlocks

With the rule in force, a head's modes are its declared or scheme modes
*regardless of any live handler* — so `env_binding_type`'s `lookup_handler` arm
loses its reason to consult the handler stack first. That arm is the precondition
ADR 3 names for deleting `env_binding_type` outright. This page does not delete
it; it removes the invariant that justified it, leaving the deletion to land
where the IR carries modes the evaluator reads directly.

**Verify:** a mode-changing handler arm
(`within [handlers: [foo: { return 3 }]] { foo | from-json }`, a value-output
arm under a byte-output head) is rejected at typecheck for literal opts and at
install for a computed opts map or an rc/plugin alias; a mode-preserving handler
(`within [handlers: [foo: { echo hi }]] { foo }`) still typechecks and runs;
`alias myecho { |a| /bin/echo ...$a }` typechecks and installs (byte-output
forwarding preserves the external `F[μ, Bytes]` spec), while
`alias foo { return 3 }` used where `foo` is piped into a byte stage is the same
`∅`-into-`Bytes` mismatch reported statically.

See also
[[decisions/260603_session-scheme-continuity|session-scheme-continuity]],
[[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]],
[[design/effects-handlers|effects-handlers]],
[[design/syscalls-are-effects|syscalls-are-effects]],
[[decisions/260530_handlers-deep-self-masking|handlers-deep-self-masking]],
[[internals/handler-dispatch|handler-dispatch]].
