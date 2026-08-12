---
status: active
---

# Argv is a list of strings, and everything else is lambda calculus

**ral has two argument-passing mechanisms and they share nothing. A handler, a
base frame, or an external is *variadic over a list of strings*: it takes an
argv, every element rendered, and it is intercepted, stacked, and reached by
`^name`. Everything else is lambda calculus: curried application at a declared
arity, first-class, no argv anywhere.** `ArgSig` existed only because the type
layer never committed to that split, so it is deleted — along with the
machinery whose whole occupation was re-deriving which side of the split a name
was on.

## Context

[[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]
found the two mechanisms and read a manifest entry's side off its arity;
[[decisions/260812_no-value-has-an-optional-argument|no-value-has-an-optional-argument]]
left that reading two-valued, `Exact` or `Any`. What neither did was let the
*types* say which mechanism a name is. So the split was real and rederived
everywhere: a classifier at seed time, a fallback in the pipe-route rule, a
guard on the spread refusal, an `Option` on the scheme deriver, and — at the
argv paths — a walk that inferred each argument and threw the result away.

**Both argv boundaries already had a type. Neither was written down.**

Internally an argv is `List String`. A base frame renders every element through
the total `to-string` — `Value`'s `Display`, which `builtin_echo` maps over
its arguments (`core/src/builtins/codecs.rs`) — which is why
`echo hello [a: 1]` prints `hello [a: 1]` and `echo $f` prints `<|x| block>`
instead of either being an error. At the exec boundary an argv is bytes, and
*there* the predicate is partial: `reject_exec_arg`
(`core/src/runtime/command/vet.rs:105`) refuses maps, lists, blocks, functions,
native callables, handles, and bytes with **R0001**. So: **`List String` inside, bytes at the OS call.** Nowhere the
checker could read said so — `docs/SPEC.md` §6.6 said an argv's elements
"remain ral values", and the checker's argv paths said nothing at all.

## Decision

### 1. `ArgSig` dissolves into its one remaining payload

`echo` and `detach` were `ArgSig::Any`'s last two entries. They leave the
builtin table for the base-frame manifest, typed as what they are:

| name | type |
| --- | --- |
| `echo` | `List String -> Return(Bytes, Unit)` |
| `detach` | `List String -> F Any` |

At runtime they were base frames already; what moves is their *typing* — their
schemes go into the typecheck env at seed time, so `lookup_handler` sees a base
frame where it used to see nothing. Every surviving table entry is then
`Exact`, so `BuiltinSig.args` flattens to the bare `&'static [ArgTemplate]`:
the enum is one payload with a wrapper around it, and the wrapper goes. No
`Option`, no third `BuiltinTypeRule` arm.

Four partial functions become total in the same stroke, each of which was
partial only to encode the split. `BuiltinEntry::fixed_arity`
(`core/src/types/builtin.rs`) returns `usize`. `native_value` beside it becomes
total, so there is no longer such a thing as a manifest entry with no value
form. `seed_natives_and_base` (`core/src/types/shell/host.rs`) stops
partitioning and becomes two installs, reading the manifest it is given rather
than sorting one. `derive_sig_scheme` (`core/src/typecheck/builtins.rs`) stops
returning `Option<Scheme>` — 260801 §6 wanted a total deriver and now it has
one, with no case to exclude.

### 2. The wart dies by the coercion, not by weakening the check

The one place argv elements crossed unrendered was the handler arm — §6.6's
"remain ral values". `apply_alias_arm` (`core/src/typecheck/infer.rs`) unifies
every element of a call's argv against a single `elem` variable, so a
heterogeneous argv was a **T0010**:

```ral
within [handlers: [mycmd: { |args| echo 'got' ...$args }]] { mycmd hello 1 true }
```

— "couldn't match Integer with String", for a call an external of the same
name would have taken without comment. Under the rule the elements are rendered
*before* they are unified, so `elem` settles at `String` and the call
typechecks.

**The wart dies by the coercion, not by weakening the check.** The two are easy
to confuse, and only one of them is sound. Weakening means unifying each
element against a fresh variable, or against nothing: the arm then accepts an
argv whose element type its own body contradicts, and the clash reappears at
runtime — which is the failure `apply_alias_arm`'s unification exists to
prevent. Rendering keeps the unification and makes it *true*: the boundary
converts, and `String` is what arrives. The conversion is the argv boundary's
own act and not a name, so nothing in scope and no installed frame can
intercept it, and
[[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]]'s line holds
here unchanged.

Handlers, base frames, `echo`, `detach`, and externals are now one convention
with one typing rule, differing only in the result they name.

### 3. The price is the rule: an arm consumes what an exec call would

Rendering is a runtime change as well as a typing one. An arm receives its
arguments' renderings, not the values, at the point `run_handler`
(`core/src/runtime/command_call.rs`) packs the argv into the list it forces the
arm on. Call sites are always fine, `to-string` being total: every value has a
rendering, so no call that typechecks today stops. An arm that *consumed* typed
values off its argv is not fine — `elem` pinned to `Int`, arithmetic on
`$args[0]` — and stops typechecking.

That is the rule, not a loss absorbed, and the reason is **interchangeability**.
A handler stands in for a command. If it consumes something an exec call could
not deliver, the two are not substitutable, and substitutability is the entire
point of a handler standing in for a command. So no internally-defined handler
keeps homogeneous non-`String` argv unification, and no exception is held open
for one: an arm that wants a number parses it, exactly as it would have to if
the argv had come from a process.

§6.6 moves a second time. The spread rule narrowed "remain ral values" to the
argv side; this repeals it. Elements render to `String` inside and to bytes at
the exec boundary, and there is no third reading.

### 4. What deletes itself rather than relocating

A check goes, a shrug is answered, a guard changes shape, and one refusal
stays. Nothing here relocates.

- **`head_pipe_route`'s `Sig` fallback** (`core/src/typecheck/infer.rs`) existed
  *only* because base frames were invisible to `lookup_handler`. Its arm is
  guarded on `fixed_arity().is_none()`, true of `echo` and `detach` alone once
  the optional class was gone. With their schemes in the typecheck env the
  handler arm above it answers first, and the fallback has nothing left to
  catch.
- **`infer_args`** — walk the arguments and constrain nothing — is not moved
  but *answered*. The argv rule constrains precisely what the shrug ignored, at
  every site that called it, `external_exec_comp_ty` included, which was that
  same shrug plus `CompTy::bytes()`.
- **The command-head half of the spread refusal** stops being a guard and
  becomes the shape of `exec_comp_ty`, whose four arms were already in the
  right order: a binding and a builtin refuse a spread because they are values,
  a handler and an external allow one because they have an argv for it to be.
- **The `apply_args` refusal stays**, because it *is* the lambda-calculus side
  of the thesis: a value takes its arguments by application at an arity its own
  type declares, so it has no argv, and `...` has nothing to spread into.

## Where this stops, and that stopping is a judgement

After this the value world still holds `BuiltinSig`, `ArgTemplate`,
`TyTemplate`, `CompTemplate`, `sig_route`, `sig_comp_ty`, `unify_arg_template`,
and `apply_builtin_sig` — none of it lambda calculus. Taken to its end the
thesis deletes all of it: every value builtin becomes a `Scheme`, and
`BuiltinTypeRule` stops being an enum. The case for stopping short is short and
exact, so it is reproduced here rather than gestured at. Each row is something
the templates hold and a scheme does not:

| lost | to whom |
| --- | --- |
| **T0050** arity | a scheme gives arity by curry depth, but under-application would become legal for a builtin as it is for a lambda — bare `explain` would stop being an error |
| **T0054** | `DecoderTakesNoArgument`, the `from-*` diagnostic |
| **T0055** | `check_error_message`, reachable only through `ArgTemplate::ErrorRecord` |
| `OneOf` precision | `to-bytes 3` and `to-bytes [1.5]` would become statically legal |
| per-argument spans | `unify_arg_template` underlines the offending argument; a scheme mismatch underlines the call |

The templates are cruft, and their shape says so: `BlockOrLambda` (`alias`),
`OneOf` (`to-bytes`), and `ErrorRecord` (`fail`) now have exactly one user
each. Read it as such — and then take it as the templates' own plan, with that
price list as its agenda. Five diagnostics are not to be smuggled out under a
refactor.

## The prize is reachable, and deliberately not taken

With renderability a static predicate, R0001 *could* become a compile-time
diagnostic — which `docs/SPEC.md` §6.5's promise that "ral checks known arity
and argument-type errors before execution" already implies, and does not
currently deliver for arguments, because the exec boundary inferred each
argument and discarded the result. This decision makes the diagnostic reachable
and **does not take it**; it lands in the commit after.

The deferral is a matter of packaging, not of doubt. `reject_exec_arg` is a
match on a value's *shape* — `List`, `Map`, `Lambda`, `Block`, `Native`,
`Handle`, `Bytes` — and shape is exactly what a type states, so once the
checker holds each argument's type in order to type the call at all, the same
refusal can fire before execution. But a diagnostic carries its own test
surface, and that surface should fail on its own terms rather than inside the
commit that states the thesis.

Two things about it are settled in advance, so the deferral is not later
mistaken for an open design question:

- **First-orderness is the wrong predicate.** `FOValue` (`core/src/serial.rs`)
  is data all the way down, so `List String` is first-order and is still
  refused at the exec boundary — spreading is a list's idiom, encoding is
  bytes'. A first-order obligation would be sound and incomplete.
- **No class machinery is needed for the exact obligation.** The checker
  already carries constraints it cannot settle where it meets them and revisits
  them at each generalisation boundary (`solve_at_boundary`,
  `core/src/typecheck/route_solver.rs`), which is how a payload route's
  grounding is deferred. A renderability obligation on a type variable is that
  same machinery, not a new feature. What stays open is only the policy for a
  variable that never grounds — `{ |x| /bin/echo $x }` kept fully polymorphic
  — and the answer is to refuse the concrete cases first and leave R0001 as
  the backstop for what polymorphism hides, so nothing that runs today stops
  running.

## The 260801 amendment

[[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]'s
thesis survives untouched: two mechanisms, and effectfulness partitions
nothing. What changes is its §1, and the matching sentence in `index.md`, both
of which have **arity** partition the manifest.

Arity was the right reading while the manifest was one table with two policies
in it. It is the wrong one now. The manifest is *authored* as two — a native
table and a base-frame manifest — arity is a consequence of which half a name
is in, and the argv half has no arity at all. `fixed_arity` classifies nothing
any more: it answers a `usize` for every entry that has one, which is every
entry left in the table.

The derive-do-not-assert discipline of that page's §6 is not weakened by this;
it is finished. There is no longer a reading to get wrong, because there is no
longer one table to be read two ways.

## Alternatives considered

- **Keep the unification and drop the rendering.** Unify each argv element
  against a fresh variable, or against nothing, and the heterogeneous handler
  call typechecks with no coercion anywhere. Rejected: it does not remove the
  clash, it defers it to runtime, and it hands an arm an argv its own body
  contradicts. The check was right; what was missing was the boundary telling
  the truth about what crosses it.
- **Let an arm keep a non-`String` element type where its body wants one.**
  Rejected: the exception is the whole cost. A handler that consumes what no
  exec call can deliver is not interchangeable with the command it stands for,
  and an argv convention with an exception in it derives nothing — the same
  argument that took `ArgSig::Any` away from `cd`.
- **Leave `echo` and `detach` in the builtin table as `ArgSig::Any`.** Cheapest
  of all, and it keeps the enum, the classifier, the pipe-route fallback, and
  the spread guard alive to serve two names. Rejected: the two mechanisms would
  still be one table read twice, which is the thing being deleted.

## See also

[[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]
(amended: the manifest is authored as two rather than partitioned by arity, and
its §1 arity reading is a consequence rather than the classifier),
[[decisions/260812_no-value-has-an-optional-argument|no-value-has-an-optional-argument]]
(left `ArgSig` two-valued; this deletes it),
[[decisions/260811_a-coercion-is-syntax|a-coercion-is-syntax]] (why the argv
rendering is the boundary's act and not a name),
[[decisions/260622_functions-and-handlers|functions-and-handlers]] (its
handler-takes-`List α` convention narrows to `List String`),
[[invariants/fixed-arity|fixed-arity]] (arity now binds application alone),
[[design/builtins|builtins]], [[design/name-resolution|name-resolution]],
[[internals/handler-dispatch|handler-dispatch]],
[[internals/builtins-registry|builtins-registry]],
[[map/core/builtins|builtins]]; `docs/SPEC.md` §6.5, §6.6, §14.
