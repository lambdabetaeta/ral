---
status: active
---

# No value has an optional argument

**`ArgSig::Optional` is deleted. A builtin signature declares an exact arity or
an open argv, and there is nothing between the two: a value's arity is the depth
of its curry spine, so there is no arrow a caller may decline to supply.** The
four entries that carried a defaulted parameter lose it — bare `cd` and bare
`fg` are arity errors — and all four flip from base frames to natives, because
that is what the derived classifier now says of them. This is
[[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]
read once more.

## Context

`BuiltinEntry::fixed_arity` is the whole native-vs-base-frame classification,
and it is *derived* rather than declared: `ArgSig::Exact` gives its slot count,
`ArgSig::Any` gives none. `Optional` had to give none as well — a zero-or-one
argument policy names no function type, and there is nothing to curry — so it
was a third answer to a two-valued question, sitting in the one table whose
entire point is that first-class-ness *follows* from arity. A policy that could
never have a first-class form does not belong in the reading that decides which
names do.

Its four users were all defaulted parameters surviving on bash compatibility,
and each default is a shell habit rather than a typed convenience:

| entry | a bare call meant | signature now |
| --- | --- | --- |
| `cd` | `$HOME` | `sig::CHDIR` — `ArgSig::Exact` of one `String` |
| `fg`, `bg`, `disown` | the most recent job, POSIX's "current job" | `sig::INT_TO_UNIT` — `ArgSig::Exact` of one `Int` |

## Decision

### 1. `Exact`, and the arity message learns whose it is

Bare `cd` and bare `fg` are now **T0050** `BuiltinArity`. The diagnostic loses
its `at_most` field, which the `Optional` arm was the only writer of, and gains
a `name` in the same stroke, so the message reads

```text
`cd` expected 1 argument, got 0
```

rather than the nameless "builtin expected 1 argument(s), got 0" it read
before — fixing the anonymity for all ~80 builtins rather than special-casing
the two names this decision moves. That is the shape of the removal throughout:
the special case does not relocate, it is answered by something already general.

### 2. All four become natives — the rule executing, not a regression

`fixed_arity()` now answers `Some(1)` for all four, and `native_value`
(`core/src/types/builtin.rs:219`) is a `map` over exactly that answer, so
`seed_natives_and_base` (`core/src/types/shell/host.rs:483`) seeds them into the
base env scope instead of the base layer of the handler stack. They gain `$cd`
and `$fg` value forms; a handler no longer intercepts the bare name, since
resolution reaches the env first; `^cd` still reaches an installed arm, exactly
as `^jobs` does today. `echo` and `detach` — genuinely open argv — are the
only base frames left.

### 3. The break is the removal of the ability to opt back in

The break is stronger than a changed default. After the flip,
bare-`cd`-goes-home is not a default a user may restore: no handler intercepts
the bare head — only `^cd` reaches one — and no value can have an optional
argument, that being the thesis. What a user gets is `let go-home = { cd ~ }`,
or both behaviours under two names. Never both under one.

### 4. Requiring an explicit job id needs no value-level source for one

`fg`, `bg`, and `disown` are interactive-only. `docs/SPEC.md` §11.6 lists them
under "the interactive builtins are", and §11.7's availability table gives them
`no` in core, `no` in batch, and `no` in the exarch agent host. So no *program*
needs a job id: there is nothing to plumb a default into, and the deleted
default was never load-bearing for composition. What remains is a human reading
their own terminal, where `jobs` has just printed the designators, and a human
reading a listing is not parsing command output. "You must now name the job"
costs one glance, not a rewrite.

### 5. `$fg` is not a new surface

`jobs` is already `ArgSig::Exact` of no arguments (`sig::TERMINAL_CONTROL`),
hence already a native with a `$jobs` value. REPL-only-and-first-class is the
status quo, not something this decision introduces; `$fg` merely joins it.
`jobs` is otherwise completely untouched — same signature, same rendering,
same `Unit` return.

## The route pin loses its subject

`head_pipe_route` (`core/src/typecheck/infer.rs`) falls back to a builtin's
`Sig` route only when `fixed_arity().is_none()`, so after the flip an alias over
`cd` is no longer pinned to `cd`'s route: **T0012** `RouteMismatch` and its WF-2
follow-up (`PinFailure::ByteHeadReturnsValue`) stop firing there, and
`alias_arm_scheme` (`core/src/typecheck.rs`) stops refusing installs it refuses
today.

That loss is required, not merely acceptable. The pin exists so that an arm
standing in for a head agrees with the head about which of a computation's two
products a pipe reads. After the flip the bare head resolves to the native, so
an installed `cd` arm is reachable only through `^cd` and no longer stands for
`cd` anywhere a pipe route matters; pinning it to the native's `Sig` route would
be a category error — asserting agreement with a head it can never be. The
check does not relocate: its subject ceases to exist.

## Alternatives considered

Both of the cheaper routes were weighed and lost. Recorded here so the break
reads as a choice rather than an oversight.

- ***`ArgSig::Any` instead of `Exact`.*** This reaches the same endpoint —
  `Optional` gone, two variants left — with no classifier flip at all: `cd`
  stays a base frame, `$cd` stays a **T0042** `BuiltinNotFirstClass`, an alias
  over `cd` keeps its route pinning, and nothing a bash user types breaks. It
  loses twice. `cd 3` stops being a type error and becomes a runtime "expected a
  String path" from `builtin_chdir` (`core/src/builtins/shell.rs`), which is the
  checker declining a fact it holds. And, worse, the open-argv class would stop
  meaning "genuinely open argv" and start meaning "open argv, plus four we
  declined to change". **The partition is only worth having while it is
  derived**; a class with an exception in it derives nothing.
- ***Elaborator sugar, as `exit`/`quit` have.*** Rewriting bare `cd` to
  `cd $HOME` would keep the default, keep `Exact`, and break nothing for anyone.
  It loses because the elaborator keeps **exactly one** name-keyed rewrite,
  which supplies an argv default and decides nothing about resolution
  ([[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]);
  a second is a permanent special case, and special cases keyed on names are the
  fifty years this shell exists to avoid. It also does nothing for
  `fg`/`bg`/`disown`, whose default is a runtime lookup in the job table and
  not an argv the elaborator could write, so `Optional` would survive
  regardless — a third of the problem, bought with a precedent.

## Runtime residue

Two bodies stop reading an argument that may be absent, and both shrink.

`builtin_chdir`'s `None => String::new()` arm goes, and with it
`Shell::apply_chdir`'s `target.is_empty() ⇒ home` branch
(`core/src/types/shell/cwd.rs`). An empty path is now rejected beside the
existing non-`String` rejection: the checker guarantees a `String` and not a
non-empty one, and `resolve_path(Some(&old), "")` normalises straight back to
`old`, which would make `cd $d` a silent successful no-op. Tilde handling is
unaffected — the `home` prologue stays for the surviving `TildePath::parse`
branch, so `cd ~` and `cd ~user` read as before.

`job_id_arg` (`ral/src/repl/host_handlers.rs`) collapses to reading `args[0]`.
The "no current job" paths go, and `JobTable::most_recent_id` with them. The
"no such job" elaboration stays: `fg`/`bg`/`disown` are pgid-only, so an id that
resolves no pgid job still points the user at a worker handle's own
eliminators — `await` is its `fg`, `cancel` its kill.

## See also

[[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]
(amended: `cd` and the REPL's `fg`/`bg`/`disown` move from its base-frame list
to its native list, and its "variadic or optional" arity reads "an open argv"),
[[invariants/fixed-arity|fixed-arity]] (the invariant this makes exhaustive),
[[invariants/optionality-via-variants|optionality-via-variants]] (where an
absent argument goes instead — into the value, as an open variant),
[[design/builtins|builtins]], [[internals/builtins-registry|builtins-registry]],
[[map/repl/jobs|jobs]]; `docs/SPEC.md` §10.2 (`cd path`), §11.6, §11.7,
§14.6.
