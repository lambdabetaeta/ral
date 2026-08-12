---
status: active
---

# The exec boundary is gated before the run, from one refused set

**Shape is exactly what a type states, so the refusal that guards `execve(2)`
fires wherever an argument's type says the shape — as T0057, before anything is
spawned — and at the spawn wherever polymorphism hides it. The set of refused
shapes is *declared once* and read from both sides of the boundary, so the two
moments cannot come to different verdicts.**

## Context

[[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]] made
the checker hold each argument's type at the exec boundary, in order to type the
call at all, and left this diagnostic reachable and untaken.
`docs/SPEC.md` §6.5 already promises that "ral checks known arity and
argument-type errors before execution"; for an external's arguments it did not
deliver, because the boundary's refusal was a match on a *runtime value*.

An argv is read at two boundaries, and only one of them is partial:

- **Inside the shell** rendering is total — `Value::render_argv`, `Display` on
  every value — so a base frame and a handler arm refuse nothing, and
  `echo [a: 1]` prints a map.
- **At the operating system** an argument is one *word*. A list is several
  arguments, a map is fields, a block has not run, a handle is still running,
  bytes are a channel. None of these is a word.

## Decision

### 1. Concrete types only

Where the resolved type is a refused shape, the checker diagnoses; where it is
still a type variable, it says nothing. The shape follows
`command_non_function_ty` — name a concretely wrong head before the general
unifier mismatch fires — and the consequence is the point: **no program that ran
before stops running.** The spawn-time refusal remains as the backstop for what a
variable hides, so the diagnostic is pure gain rather than a trade.

```ral
let r = [a: 1]; cat $r              # T0057, before the run
let show = { |v| cat $v }; show $r  # the parameter's shape is the run's business
```

### 2. One declaration, two readings

`RefusedArg` (`core/src/types/exec_arg.rs`) names the refused shapes — list,
map, block, handle, bytes — with two total maps into it and one remedy out:

- `of_value`, which `runtime::command::vet` reads at the spawn;
- `of_ty`, which the argv rule reads before it;
- `remedy`, one sentence per shape, so the static error and the pre-spawn one
  speak *one language* about the same mistake.

Both matches are wildcard-free, which is the mechanism rather than a comment: a
new `Value` or `Ty` constructor cannot compile until it has a verdict on both
sides. Two facts fall out of writing them side by side — a **record** is a map at
run time and is refused as one, and a **tagged value** is a word (`` `ok 1 ``
renders), so `Variant` is refused by neither.

### 3. The gate is the exec boundary's alone

One argv rule serves all three boundaries, and it now carries which boundary it
is at (`ArgvBoundary::InShell` / `Exec`, `core/src/typecheck/infer.rs`). A base
frame and a handler arm pass `InShell` and gate nothing: rendering there is
total, and a rule uniform across both boundaries would be wrong in one
direction. Getting this wrong permissively is a missed diagnostic; getting it
wrong restrictively breaks `echo [a: 1]`.

### 4. A spread is left to the run

A spread contributes as many argv elements as its list holds, and only the run
knows how many — an *empty* spread contributes none, and refuses nothing. So a
spread's element type is not gated, however concrete it is:

```ral
let xss = filter $p $lists            # List (List String), possibly empty
/bin/echo ...$xss                     # no diagnostic; the spawn decides
```

This is a deliberate incompleteness, and it is what §1's guarantee costs. A
static refusal here would reject a call that spawns cleanly, which is the one
thing the gate must never do.

## What is not built, and why

**No renderability class, predicate, or deferred obligation.** First-orderness
is the wrong predicate to begin with: `FOValue` (`core/src/serial.rs`) is data
all the way down, so `List String` is first-order and is still refused at the
exec boundary — spreading is a list's idiom, encoding is bytes' — which makes a
first-order obligation sound and incomplete. The *exact* obligation could be
deferred on a type variable with the machinery that already defers a payload
route's grounding (`solve_at_boundary`, `core/src/typecheck/route_solver.rs`),
and that remains available; it is a separate question, whose open half is the
policy for a variable that never grounds — `{ |x| /bin/echo $x }` kept fully
polymorphic — where the choice is to reject at generalisation, honest but with
prelude blast radius, or to leave it where it now is.

## Alternatives considered

- **Refuse a variable at generalisation.** Complete, and it makes the boundary's
  rule a property of the type rather than of the call site. Rejected for now:
  it costs programs that run, and the prelude's exposure to it has not been
  measured. Measuring that exposure is the price of admission, not this
  commit's work.
- **Gate a spread's element type.** Rejected: see §4. Soundness here is not
  worth a false refusal of an empty spread.
- **Match on `Ty` in the checker and leave `vet`'s match where it was.** Two
  statements of one boundary, kept in step by review. Rejected: the drift is
  invisible until a user meets one refusal and not the other, and the failure is
  silent in both directions.
- **Reuse the spawn-time wording verbatim for every shape.** It reads as one
  language, but the list remedy ("use `...`") is wrong for a map and a block.
  The remedy is per shape instead, in one place, so both sides gained the
  precision rather than the checker alone.

## See also

[[decisions/260812_argv-is-a-list-of-strings|argv-is-a-list-of-strings]] (made
this reachable; its "the prize is reachable" section points here),
[[invariants/exec-argv-is-words|exec-argv-is-words]] (the rule this decision
installs), [[invariants/fixed-arity|fixed-arity]] (the argv/application split
the boundary sits on),
[[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]]
(why a bundled tool is vetted exactly as a host binary is),
[[internals/type-inference|type-inference]], [[map/core/typecheck|typecheck]],
[[map/core/runtime|runtime]]; `docs/SPEC.md` §6.5, §6.6.
