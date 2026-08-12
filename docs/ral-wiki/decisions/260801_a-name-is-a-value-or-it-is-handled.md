---
status: active
---

# A name is a value or it is handled

**The builtin is not a kind of name. A manifest entry with *fixed arity* has a
function type, so it is a first-class *native* value seeded into the base env
scope; an entry with an *open argv* has no function type — nothing to curry,
no meaning for partial application — so it is only interpretable as command
syntax, and it lives where command syntax is interpreted, as a *base frame*
at the bottom of the handler stack. Resolution is therefore
`env → handlers → external` with no builtin arm, and resolution order is the
only arbiter of interception: no install path admits or refuses a name.**

## Context

Two questions met. *Can a handler intercept a builtin?* — the runtime said no
by consulting a builtin table between the env and the handler stack, while the
typechecker refused the install outright (`CannotRedefineBuiltin` T0043,
`HandlerShadowedByBinding` T0044). And *what is `$upper`?* — a builtin in value
position was η-expanded into a curried lambda over a name-dispatched command,
so a reified primitive was a closure that re-entered dispatch by name.

Both answers were consequences of the same premise: that "builtin" names a
third mechanism beside the value and the handler. Two earlier positions tried
to keep the premise and grade it — unseal builtins by *effect class* (pure
ones are values, effectful ones are handleable), then seal everything on the
grounds that in call-by-push-value every builtin is a computation. Both
mis-locate the distinction. Effectfulness disqualifies nothing: a function *is*
a computation, and a computation may act. What disqualifies a name from being a
value is having no function type to inhabit.

## Decision

### 1. The partition is arity, read off the type rule

`BuiltinEntry::fixed_arity` (`core/src/types/builtin.rs:79`) is the whole
classification, derived rather than declared: a `Sig`'s arity is structural
(`ArgSig::Exact` gives its slot count, `ArgSig::Any` gives none),
a `Scheme`'s is the curry-spine depth of its value form
(`scheme_curry_depth`, `core/src/typecheck/builtins.rs:51`). `native_value`
(`core/src/types/builtin.rs:219`) is the one door both boot and wire hydration
pass through, so nothing can classify an entry two ways.

- **Fixed arity → a native.** `Value::Native { entry, applied }` — nearly the
  whole manifest, the nullary entries included: the `from-*` decoders are
  arity-0 natives, so `$from-json` exists as a thunk whose forcing reads the
  ambient channel, dynamic state like the cwd.
- **An open argv → a base frame.** `detach` and `echo` (`ArgSig::Any`). This
  decision also read `cd` and the REPL's `fg`/`bg`/`disown` on this side, on an
  optional-argument third class of arity that is since deleted
  ([[decisions/260812_no-value-has-an-optional-argument|no-value-has-an-optional-argument]]):
  each declares its one argument, so each is a native, and the two frames above
  are the whole base layer.

`detach`'s command-only constraint — a partial application must not promise a
process outliving the session — stops being a special case and becomes a
consequence of the rule.

### 2. Natives are ordinary values

A native curries by collecting arguments until `fixed_arity` is reached;
application is the single arity gate, so no body checks its own count.
Under-application yields the partial value, over-application is an arity error,
and arity-0 natives force like blocks — exactly the lambda's behaviour. It
prints as `<native NAME>`, is equal by name plus collected arguments, is
refused by the first-order projection and by string coercion at the syscall
boundary, and crosses the scope envelope by name, re-linked against the
receiving shell's manifest.

The derived value scheme is *uncurried all the way*, as a lambda's is: `map
$round` is ill-typed because `round` is 2-ary, and the typed idiom for partial
application is the Bind rethunk (`let p = $round 1.4`, then `map $p`) or an
explicit lambda. Runtime currying serves unchecked sites only.

### 3. Base frames are the stack's floor, not a flag

The handler stack keeps its run frames and grows a base layer *below* them,
holding manifest rows rather than `HandlerFrame`s. Permanence is
representational: `unalias` and `strip_matched` index run frames only, so no
operation's index space contains a base frame. Lookup keeps its two passes —
per-name over run frames then the base layer, then catch-all — so "any per-name
handler beats any catch-all, whatever their relative depth" survives verbatim,
and a catch-all never sees `echo` or `detach`. A user frame stacks above
a base frame and self-masked forwarding reaches it; a base hit calls the native
body directly with the argv slice, with no adapter and no masking, because a
native body never self-forwards (`run_base_frame`,
`core/src/runtime/command_call.rs:156`).

### 4. No name admission anywhere

T0043 and T0044 die with no successor. A handler or alias under any taken name
installs; it is dead at bare heads wherever the env wins and live under
`^name`, which skips the env by definition. The two sides agree by
construction: `resolve` is env → handlers → external
(`core/src/runtime/command_call.rs:39`) and `exec_comp_ty` is binding → native
rule → handler → external (`core/src/typecheck/infer.rs:819`). What survives of
the handler vet is what is actually about the arm — the unary-lambda shape check
and the mode pin, the latter now pinning a stacked arm over a base-frame head to
that frame's `Sig` modes, so an `echo` arm that breaks `None → Bytes` is refused
at install.

Interception is lexical shadowing, and `^name` is the escape hatch: `^clear`
still reaches the ncurses binary, while `^detach` and `^echo` reach their base
frames rather than a `PATH` binary of that name — accepted.

### 5. The manifest is a boot manifest

`CORE_BUILTINS` and the host sets seed the base env scope, the base frames, the
checker's rule table, and `help`/`explain`; runtime dispatch never consults a
builtin table again. Natives reach the checker *solely* through that rule table
— every harvest walks user scopes only — which is load-bearing twice: a native
in the harvest would silence the `Sig` diagnostics, and a native visible to the
elaborator's `is_bound` would lower every builtin head to application, going
dark for both the rule table and the audit. `true` and `false` join the base
scope as language constants (`core/src/types/builtin.rs:209`), since a
language-given name belongs in the language's own scope.

### 6. Derive, do not assert

Hand-maintained restatements of the type rule are deleted rather than guarded:
the registry's `arity:` field with its consistency check, and the hand-written
value schemes, which now derive from the signature through the projections that
already existed (`derive_sig_scheme`, `core/src/typecheck/builtins.rs:1058`).
The deriver is total on `Exact` and undefined on `Any`, so "fixed arity ⇒ a
value scheme, an open argv ⇒ none" holds by construction. One
override survived at the time of this decision: `_type`, whose scheme
correlated an argument and a result (`α → F α`) — a fact the template
vocabulary could not state, since it names an occurrence's *instantiated*
type where every other signature is closed before generalisation.

`_type` is since deleted
([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]);
`explain <name>` answers the name-level question `_type` used to answer at an
occurrence, at the cost of generality — a name's declared scheme, not the
type an argument happened to instantiate it to during inference. With `_type`
gone, the override this section describes — `Sig::value: Option<fn(&mut
Unifier) -> Scheme>` — has no entry left that sets it to `Some`. Whether the
override mechanism itself should now come out, or stays as the honest escape
hatch for the next signature the template vocabulary can't state, is a
separate question this page does not answer.

## Consequences

- Every fixed-arity entry is `$`-referenceable and composes like a user
  function; every open-argv entry is a frame a user frame can stack on.
- A user or prelude binding named after a native seeds and shadows it, closing a
  divergence the old seed-skip created: the checker typed such a name by the
  builtin rule while env-first dispatch honoured the binding.
- `detach` no longer consults a table to reject a builtin head, so
  `detach "d" clear` detaches the `PATH` binary — the image `^clear` names.
- `echo` stops being the elaborator's one name-interpreting macro; the single
  remaining name-keyed rewrite is `exit`/`quit`'s zero-arg sugar, which supplies
  an argv default and decides nothing about resolution.
- The bundled coreutils move out of the manifest module to `core/src/uutils.rs`:
  they were always exec-side, and the manifest module now holds manifest things
  only.
- A native's audit frame sits on application itself, so a command head and a
  value-position application record identically as the entry's name. Grants
  still govern exec alone.

## Alternatives considered

- **Unseal by effect class** — pure builtins become values, effectful ones stay
  handleable. Rejected: it grades the wrong axis. Effectful bodies are ordinary
  functions in CBPV; the axis that matters is whether a function type exists.
- **Keep the seal** — everything is a computation, so nothing is a value.
  Rejected: it leaves `$upper` explained by η-expansion into a name-dispatched
  command, and leaves the builtin table as a third resolution arm.
- **Install-time name guards, in any form** — refuse, warn, or flag a handler
  under a taken name. Rejected: resolution order already decides which arm runs,
  and a second, static opinion about it can only disagree with the first.
- **A `permanent: bool` on the base frames.** Rejected: a flag can be cleared,
  and every operation that mutates the stack would have to consult it. Keeping
  base frames out of the run-frame index space makes removal unexpressible.
- **A thunked deriver spine**, so `map $round` typechecks by partial
  application. Rejected: types would then depend on provenance, a native being
  typed unlike the η-equivalent lambda. The Bind rethunk is the idiom.

## See also

[[decisions/260622_functions-and-handlers|functions-and-handlers]] (superseded
in part: its "every builtin is a function, no builtin is a handler" resolves the
other way, and `echo` is a base frame rather than elaborator sugar — the
function/handler dichotomy itself stands, and this decision is what makes it
exhaustive),
[[decisions/260812_no-value-has-an-optional-argument|no-value-has-an-optional-argument]]
(narrows the partition to two classes by deleting the optional one),
[[invariants/fixed-arity|fixed-arity]] (the invariant this makes structural),
[[design/builtins|builtins]], [[design/name-resolution|name-resolution]],
[[internals/builtins-registry|builtins-registry]],
[[internals/handler-dispatch|handler-dispatch]],
[[map/core/runtime|runtime]], [[map/core/builtins|builtins]];
`docs/SPEC.md` §14, §16.7, RATIONALE §"Values and commands".
