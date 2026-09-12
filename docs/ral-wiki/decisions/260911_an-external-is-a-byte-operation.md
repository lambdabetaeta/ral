---
status: active
generated_at_commit: 1a02a2ed
---

# An external is a byte operation

**An external command is an operation whose signature the operating system
fixes at `List String → Bytes`; a handler *interprets* that operation and
cannot *re-declare* its type.** The checker read a bare head it could not
resolve as this external, cashed the byte route into the tree at that point
(a `Capture` node, a `PipeYield`), and the machine later resolved the same
name against a different environment — one where a `source`d file, a
reinterpreting arm, or a literal catch-all had made it something else. Where
the two disagreed, WF-2's promise (`ρ = Bytes` implies the value is `Unit`)
broke, and only a runtime check caught it — `c6077c72` replaced a silent
`assert!` with the diagnostic this decision keeps ([[design/types|types]]
narrates WF-2 and this check). This decision closes the disagreement at its
source: it
forces the byte route uniformly on every reinterpretation of an unresolved
name, makes `^name` a uniform escape to the path-head class, and deletes the
one binder that could install a name behind the checker's back. What the
runtime check now guards is no longer a known user mistake — it is an
invariant the statics are supposed to hold everywhere.

## Context

Four things could make the checker's route and the machine's outcome for one
bare head disagree: `head_pipe_route` answering a *fresh* route for a name
that turned out to be a native or a binding; the literal catch-all
reinterpreting every external in its extent without its type being checked; a
per-name or catch-all arm returning a value where the checker had cashed
`Bytes`; and `source`, which installs a binding *while the run is already
under way*, long after the whole unit was checked, so a name the checker read
as external could resolve to a block returning an `Int`. The user's own
diagnosis was blunt: *"handlers mock binaries. binaries return bytes. so they
should return bytes."* Closing three of these four statically and deleting the
fourth, the un-checkable `source`, is this decision.

A second ruling landed alongside the first and reverses part of
[[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]:
`^name` was defined there as an escape hatch that "skips the env by
definition" while still reaching an installed handler. The user's ruling —
*"^ should reach external"* — makes `^name` skip the handler stack too, base
frames included.

## Decision

### The route is forced uniformly, at install

`head_pipe_route` (`core/src/typecheck/infer.rs`) answers `PayloadRoute::Bytes`
for every name with no handler scheme in scope, unconditionally — no carve-out
for a name that happens to be a native or a binding, because under ruling 2
(below) no arm installed under such a spelling is reached by any bare head at
all, so there is nothing for it to define a route *for*. An arm that
reinterprets an unhandled name inherits the byte route rather than declaring
one, and is refused at install (`pin_arm_to_head`) if its body is not
`Unit`-valued — the same refusal `echo` itself has always enforced. The
literal catch-all is pinned the same way, since it stands in for every
external in its extent; a computed catch-all is vetted at its runtime install,
as a computed per-name arm already was. Base frames' own routes (`echo`
`Bytes`, `detach` `Value α`) and alias-over-alias pinning are unchanged: an arm
over a base frame is still reached at the bare head, and still pins to that
frame's own scheme.

### `^name` joins the path-head class

`^name` now means exactly what `./tool` and `/usr/bin/tool` mean: it skips the
value world *and* the handler stack — run frames and base frames alike — and
executes the external of that name (SPEC §9.5). This reverses
[[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]
§4's "`^detach` and `^echo` reach their base frames rather than a `PATH`
binary of that name — accepted": under this ruling **`^echo` is `/bin/echo`
and `^detach` is not found**, everywhere. §4's first leg is untouched and
restated here: resolution order is the only arbiter of interception, no
install path admits or refuses a name, and this does not resurrect T0043
(`CannotRedefineBuiltin`) or T0044 (`HandlerShadowedByBinding`) — a handler or
alias under any taken name still installs; whether it is ever *reached* is
answered by the same order both sides already share, not by a second opinion
at install. The checker's `Exec` arm and the runtime's `resolve_command_word`
must move in lockstep for this: both now send every non-bare head —
`CommandWord::External(_)` and every path head alike — to the one path-head
arm, `external_exec_comp_ty` on the checker's side and
`CommandIdentity::resolve` on the runtime's; changing one side without the
other would reopen exactly the check/run divergence this whole decision
exists to close, so both land in one commit with a test pinning agreement.

An arm installed under a *native*'s or a *session/local binding*'s spelling is
therefore unreachable from every bare head — globally for a native, and
exactly where the binding is in scope for a binding, since a closure that does
not capture it never sees it shadowed. This is not a new refusal: `explain`
already reports the winner at a bare head and lists a shadowed arm under
`shadows:`, so `explain length` inside an arm over the native `length` reads
`length: builtin` / `shadows: handler` — the discoverability a dead arm needs,
in the tool already built for "what runs here." No install-time gate, and no
new non-fatal diagnostic class: ral has none, and inventing one here would be
scope creep the runtime's own resolution order already makes unnecessary.

### One type for every user arm

A handler substitutes a new body for an external's operation, never a new
type: every user handler arm — per-name or catch-all — now has exactly one
type, `U(List String → F[Bytes] Unit)` (an arm installed over the name
`detach` itself is the one exception, below). **A value-returning command of
variable arity is therefore not expressible.** `alias three { |args| return
3 }` is refused at install (`[T0012]`), and a binding of the same shape is
refused too, for the unrelated reason that a binding is fixed-arity
(`[T0011]`). This is the accepted cost of ruling 1: bindings are the value
world — fixed arity, typed values; handlers are the command world — argv in,
bytes out, the OS's own contract. A name is a value or it is handled; if it is
handled, it is a command, and a command writes.

The replacement spellings all still work: a value function taking a list,
`let three = { |xs| … }; three [a, b, c]`; a fixed-arity value function,
unaffected; a variadic *command* — forwarder, byte mock, `fail`-bodied arm —
unaffected. If a bash-style `f a b c` calling convention for value functions is
ever wanted, the route back is recorded rather than built here: `alias` as a
*lexical* argv-convention binding, distinct from `alias`-as-handler, which
[[decisions/260622_functions-and-handlers|functions-and-handlers]] already
wants to retire — out of scope for this decision.

### `detach`'s two identities

Two things named `detach` must not be conflated, and this decision changes
only one of them. An arm installed **over the spelling `detach`**
(`within [handlers: [detach: …]]`) pins to `detach`'s own scheme,
`∀α. [Str] → F α` (`typecheck/builtins.rs:757`), and may return anything — it
is reached at the bare head like any base-frame arm, unaffected by ruling 1.
Separately, `detach` **intercepting another head** (`detach "d" my-server …`)
runs that head's arm through `apply_handler` under the run's own sinks, with
no `Capture`, and reports the arm's value as its own — and that arm is now
uniformly `List String → F[Bytes] Unit`, so it contributes only `Unit` to
`detach`'s `α`. `detach` never forwards an intercepted arm's *value*; it dies
with the capability ruling 1 accepts, not as a separate decision. What now
inhabits `detach`'s `α`: the `{pid, desc}` receipt on a real birth; `Unit` when
the head it ran is intercepted by a user arm or is itself a base frame
(`detach "d" echo hi`); and whatever an arm installed over `detach` itself
returns.

### `source` is deleted; `use` is the one loader

`source` ran a file's phrases into the caller's own environment while the run
was already under way, installing names the checker had no way to see. It
could not be fixed by checking harder: a `source` path is a runtime value
resolved against the runtime cwd and the runtime grant, so a static reading of
what it installs would have to read the filesystem in the checker — the same
divergence a `PATH` oracle would reintroduce, against `within [dir:]` and
`grant [fs:]`. The verb was intrinsically un-checkable and, by the time of this
decision, vestigial: two call sites, both inside its own test. Its whole
phrase spine — `Phrase::Source`, `CompKind::Source`, `Frame::Source`, the
registry entry, the elaborator's source arms — is deleted; `use` is the one
loader left in the language, typed `String → Map a`, and every host loading
door — `evaluate_source`, `evaluate_checked`, `compile_toplevel`,
`module_phrases`, their cycle and depth guards — is untouched, still serving
rc, plugins, exarch, and the capability files.

### `seed_env` binds scheme-less names monomorphically

A binding installed with no scheme — a host seed var, an rc `env:`/`prompt:`
key — used to be dropped from the type environment entirely, which made an
existing name of that kind read as unbound, and so, at a bare head, as an
external. `seed_env` now binds it at one fresh **monomorphic** type variable
instead. Generalising it to `∀a. a` would be unsound — a wildcard that could
stand in for any type at all — where a single fresh variable merely says "one
value, one type not yet observed," which is what it is.

### The lease ledger loses a seam

Exarch's binding-lease ledger
([[decisions/260629_agent-binding-reaping|agent-binding-reaping]]) renewed a
name's lease at three seams sharing one exhaustive walker over referenced
names: the turn's own compiled program; runtime-compiled loads through
`check_source`/`compile_toplevel`; and a dispatch-time touch in
`classify_command`'s `Resolution::Env` arm, for a bare head that resolved to a
binding the elaborator's bound set never saw. Only `source` could produce that
last case — a name the elaborator *does* see compiles its reference as an
`App`, never an `Exec`, so it never reaches that arm at all. With `source`
gone, the seam has no argument left to renew: `classify_command`'s
dispatch-time touch and `BindingLedger::renew_one`, which existed only to
serve it, are deleted together. Two seams remain, not three.

## Consequences

- The runtime backstop, `machine::bytes_promise_broken`
  (`core/src/evaluator/machine.rs`), stays at its two cash sites,
  `Frame::Capture` and `PipeYield::Unit` — the check was always correct, only
  its hint was wrong. It named `source` as the cause and claimed `use` hands a
  file's names back "where the checker can see them" (false: `use` is
  `String → Map a`, dynamically typed, each projection instantiating a fresh
  variable, so a bad key or a mismatched use surfaces as a runtime `[R0001]`,
  never a `T`-code, and never through this check). The hint now says what is
  true: every producer this decision knows of is refused statically, so a
  script reaching this check has found a checker/runtime divergence with no
  known cause, and should be reported rather than diagnosed as a mistake in
  the script.
- `explain` reads in bare-head order unconditionally: the comment that
  justified probing handlers before the manifest by "`^name` reaches it" is
  deleted along with the reason, since `^name` reaches nothing installed under
  a native's or a binding's name any more.
- Grants are unaffected: `classify_command` admits the `External` arm exactly
  as it did before ruling 2, whether the head arrived bare, by path, or by
  `^name`.
- Self-masking (`Frame::Unmask`) is unaffected: a forwarder inside an arm still
  reaches its own outer frame by writing the *bare* name, never `^name`, which
  no longer forwards to anything installed.
- Every test that observed the old reversed route rule, or observed `^name`
  reaching an installed arm, is converted to observe the ruled semantics
  instead, none deleted as coverage: what such a test showed before — "which
  arm won" — is now read off a captured value rather than off the type the
  checker gave the winning arm, since that type no longer varies by winner.
- **Windows.** `echo` is not a bundled coreutil, so `^echo` resolves to
  `/bin/echo` on Unix and to nothing at all on Windows, which has no
  standalone `echo.exe`. No in-repo program needed the old reading once its
  four `^echo`-in-an-arm tests were rewritten to bare `echo` (self-masking
  already reaches the frame from inside the arm), so this is a platform gap
  noted, not one anything here papers over.

## Alternatives considered

- **Narrow the fresh-route rule to a `within`'s dynamic extent, or to a name
  that is not a native or binding.** Rejected: a frame's lifetime does not
  change what it interprets, and under ruling 2 an arm over a native or
  binding spelling is reached by *no* bare head at all, so there is nothing
  left for a narrowing to except.
- **A `PATH` probe at handler-install time**, refusing an arm whose name
  resolves to nothing on `PATH`. Rejected: a dead-PATH oracle, and
  [[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]
  already rejected static admission gates on the same grounds — resolution
  order is the one arbiter, and a second static opinion can only disagree with
  it.
- **A base-frame carve-out keeping `^detach`/`^echo` at their frames.**
  Rejected: it would restore exactly the asymmetry ruling 2 removes, and
  reads incoherently beside "`^name` is the binary" — `^echo` would name
  ral's `echo` while `^cat` names the OS's `cat`. Nothing in the repository
  needed the old reading; every site that spelled `^echo` inside an `echo` arm
  did so incidentally and reaches the frame through self-masking regardless.
- **Finish `source` as a checked binder** (staging its target as a typed unit,
  or weakening the whole-program check to admit it). Rejected on both counts:
  the first is unsound for the same reason the runtime check exists — the
  checker's filesystem/cwd/grant view cannot agree with the machine's without
  becoming a `PATH`-oracle in another shape; the second weakens WF-2's own
  guarantee for every caller to rescue one vestigial verb.
- **A non-fatal diagnostic class for a dead arm under a native or binding
  name.** Rejected as scope creep: ral has no such class, one was floated
  before and never built, and `explain`'s existing `shadows:` line already
  answers the discoverability question a warning would exist for.

## Known limitation

`use`'s typing, `String → Map a`, is dynamic: a module's names are not typed
individually, each projection instantiates a fresh variable, and a bad key or
a type mismatch through it is a runtime error, not a static one. A typed
module signature — checking a `use`d file's exports against a declared shape
— is a separate, larger design and explicitly out of scope here.

## See also

[[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]
(superseded in part: §4's "`^detach`/`^echo` reach their base frames" reverses;
its first leg — resolution order as the only arbiter, no name-admission gate —
stands),
[[decisions/260812_no-value-has-an-optional-argument|no-value-has-an-optional-argument]]
(superseded in part: every "`^cd` still reaches an installed arm" reading
reverses — `^cd` now reaches the `PATH` binary or nothing),
[[decisions/260629_agent-binding-reaping|agent-binding-reaping]] (superseded in
part: the binding-lease ledger's three renewal seams narrow to two, `source`'s
seam having no producer left),
[[decisions/260622_functions-and-handlers|functions-and-handlers]] (the
alias-as-lexical-argv-binding route back, still open there),
[[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]
(the byte-route encoding this decision leans on but does not change),
[[design/types|types]] (WF-2 and the payload route),
[[design/effects-handlers|effects-handlers]],
[[design/name-resolution|name-resolution]],
[[design/builtins|builtins]],
[[internals/handler-dispatch|handler-dispatch]],
[[internals/binding-leases|binding-leases]],
[[invariants/fixed-arity|fixed-arity]],
[[map/core/runtime|runtime]].

Cite: RATIONALE §"Values and commands", §"Effect handlers reinterpret external
names"; `docs/SPEC.md` §6.3, §9.1, §9.5, §17.
