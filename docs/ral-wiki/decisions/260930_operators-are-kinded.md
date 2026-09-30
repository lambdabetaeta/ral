---
status: accepted
generated_at_commit: 8d868e18
---

# Operators are kinded

**A type variable carries a kind: a set of admissible heads and a deep bit.**
The operators, the interpolation and the builtins that take "anything" state
what they take, so that a helper keeps its generality and a program that
cannot run is refused where it is written.

## What was decided

- **Five named kinds**, printed `∀α:number.`:

  | kind | heads | constrains |
  |---|---|---|
  | `number` | Int, Float | `+ - * /`, unary `-` |
  | `comparable` | Int, Float, String | `< > <= >=`, `lt`, `gt`, `sort-list`, `sort-list-by`'s key, `int`, `float` |
  | `scalar` | Bool, Int, Float, String | each interpolation part |
  | `sized` | String, Bytes, List, Map | `length`, `is-empty` |
  | `data` (deep) | everything but a block and a handle | `==`, `!=`, `equal`, `str`, the encoders, the host doors that serialise their argument |

  `%` is `Int → Int → Int`. A record has no size (`length` on one is refused),
  and `()` is neither a word nor an interpolant.
- **Two variables unite at the meet** (`Kind::meet`): the heads both admit, the
  deep bits or-ed. An empty meet is refused; a meet with one nullary head binds
  the variable to it, so `sized ∩ comparable` is `String`.
- **Binding checks the head** (`Unifier::admit`), and a deep kind imposes
  `data` on every component: a list's or map's element, a record's fields and a
  variant's payloads, and the tail row variable's deep bit, which imposes `data`
  on each field later added through it. The walk carries a visited set, so a
  cyclic type ends.
- **A kind is where it was imposed.** The kind, and the span that last narrowed
  it, sit on the free variable's root (`Kinded`); a refusal cites the span, so
  the report draws a second caret there: *"used as something `length` measures
  here"*. `InferCtx` sets the span in force (`Unifier::at`) before it unifies or
  instantiates.
- **A scheme carries its kinds**: `ty_vars: Vec<(TyVar, Kind)>`, `row_vars:
  Vec<(RowVar, bool)>`, and a weak residual keeps its kind too, so a stored
  scheme refuses in the next unit what it refused in its own.
  `builtins::mk_scheme` takes them; `mk_plain_scheme` is the unkinded case.
- **A bare-label read is settled by a kind as well as a head** (Amendment A): a
  kind admitting Map and not Record reads the key, one admitting Record and not
  Map reads the field, one admitting neither is refused at the read, naming what
  else the variable was used as. `{ |m| length $m; $m[a] }` and its transpose
  are both `Map α → Command α`.
- **One code, `T0074` `KindMismatch`**, for a head the kind does not admit and
  for an empty meet. The headline is the kind's; the guidance is the operator's:
  *to join text, interpolate*; *sort with `sort-list-by` and a key*; *a block has
  no equality — use what it computes*; *`()` is nothing to print*.
- **The host doors that serialise their argument** type their variables `data`
  and their row variables deep, since `first_order` refuses a block or a handle
  at any depth; `_ed-state`'s cell is `α:data`; `_ed-highlight` takes spans.
- **`()` is refused as a word and as an interpolant**, from one declaration
  (`RefusedArg::Unit`) read by the checker, `vet`, `interpolate_piece` and
  `echo`.
- **An identical report at the same span is made once**: two operands of one
  type, both refused, are one complaint.

## Why

This is Elm's `number` and `comparable` and SML's equality types: a closed, fixed
set of predicates that generalise, with no dictionaries. A helper such as
`let log = { |n| echo "n: $n" }` stays `∀α:scalar`, and `max`, `min`,
`maximum` and `minimum` infer their kinds from the prelude. Without them
`$["a" + "b"]`, `length 5` and `$[$f == $f]` were accepted and failed at run
time, because `+`, `length` and `==` only unified their operands, or nothing.

Indexing is a relation between two variables, not a predicate on one, which is
why it gets its own settlement and not a kind. What `sized` says of a label read
is the one place the two meet.

## The one relation

A computed index `$c[$k]` is the only deferred constraint the checker has:
`Idx(c, k, e)` is `c = [e] ∧ k = Int ∨ c = Map e ∧ k = String`, beside the
label read's `Lbl` in `typecheck/index.rs`.

- **A head or a kind settles it, never a guess.** It fires at creation and again
  at the unit's end: a list target, an `Int` key, a key whose kind admits `Int`
  and not `String` (`k:number`), or a target whose kind admits `List` and not
  `Map` selects the list case, and the mirror image the map case. A target of
  any other head is refused (`DynamicIndexOnScalar`, `IndexIntoThunk`), a target
  whose kind admits neither (`c:comparable`) or a key that is not an `Int` or a
  `String` is `KindMismatch` (T0074), naming the other use or the key's type.
- **An integer literal is the list rule and never waits**, so `{ |xs| $xs[0] }`
  is `∀e. [e] → Command e` and is used at two element types.
- **The three variables are weak for the unit.** Nothing settles at a
  generalisation, because settling could change nothing: the variables are weak
  whether it has settled or not. A helper like `{ |m k| $m[$k] }` is therefore
  accepted at the one container type its unit uses, and its verdict, like every
  type `explain` prints, is the same under any reordering of independent
  statements.
- **What is pending at the unit's end is refused**, `IndexContainerUnknown`
  (T0075): *"is `$m` a list or a map? …, but nothing in the program fixes
  which"*, with a second caret on the binding that holds the index. Labels and
  indexes drain together, since either can decide the other's target.
- **The price** is that a helper whose computed key is fixed by its own body
  (`{ |xs i| let j = $[$i + 1]; $xs[$j] }`) is monomorphic all the same, and that
  a block's parameter indexed by a computed key is checked against the call that
  fixes it, inline or let-bound alike: `map { |x| $x[$x] } [1]` is refused both
  ways.

## Rejected

SML '97's resolution, which makes every helper monomorphic. No overloading at
all. The status quo.

A CSV row as a record: a row is a map from header to text, since its keys are
data. `from-csv` is `F [Map String]` and `to-csv` takes `[Map String]`, so the
round trip type-checks and a record-literal row is refused.

Refines [[decisions/260606_unify-one-sided-obligations|unify-one-sided-obligations]]
and [[decisions/260930_a-let-generalises-what-is-not-weak|a-let-generalises-what-is-not-weak]].
