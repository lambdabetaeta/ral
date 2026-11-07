# Grades and capture: values at `let`

**Only a `let` implicitly turns a command's output into a value.** The rule reads the
right-hand side's resolved type, so it works through blocks, calls and names:
`let f = { hostname }; let host = f` binds the host name.

## Grades classify producers, not effects

A *grade* says how a computation produces its result, not whether it writes.

- `F^p A` produces a value; `F^w Unit` is a command and produces output.
  Unit values are written `()` in programs.
- **`F^w A ⇒ A = Unit`.** Each grade is paired with one value type; rules
  introduce `w` only at `Unit`, and copy the pair together.
- A grade variable can become `w` only beside `Unit`. Before binding either
  grade, unification refuses an output pair against a type other than `Unit` or
  a variable whose kind admits it. Returners and handles share this check.
- A sequence takes its tail's type, not a combination of its effects:
  `{ echo pre; return 5 }` returns an integer while writing `pre`.
- There is no grade multiplication or permissiveness order. Grades are
  producer kinds, not an effect monoid ([[related/cbpve|CBPVE]]).

## Where grades come from

- **`w`: commands.** Externals, base frames such as `echo`, and builtin
  results declared `Command`.
- **`p`: value demands and boundaries.** A `let` fixes a free grade; builtin
  parameters such as `filter`'s predicate demand a value. Checked-boundary
  decoders (JSON/CSV), `use` and other checked boundaries supply data, so
  their result grade is `p`: joining with a command must not turn that
  supplied data into `()`.
- **Fresh grades: other producers.** `return`, primitives, `~`, `capture`,
  `spawn`, `audit`, and handled `alias`/`unalias` statements; ordinary builtin
  results, except commands, boundaries and already graded results such as
  `fail`, `raise` and `await`; and stdout redirects, `M > f : F^ν A` for `M : F^g A`.
- Ordinary text/byte decoders (`from-bytes`, `from-string`, `from-line`,
  `from-lines`) have fresh result grades too; their non-unit payload types
  hide those grades when printed as `Returns`.
- Stdin/stderr redirects preserve their input grade. Scope forms such as
  `within` and `gather` preserve their body's type.

A free unit producer can therefore stand as a command by unification, with no
runtime conversion. A `let`-fixed `p` producer cannot. The rule reads types:
`return ()` and `return $u`, for `u : ()`, type alike.

## The bind rule is the one coercion

The same rule governs local binds, top-level definitions and hoisted `!{…}`.

- **Function:** bind unrun as `return (thunk M)` (Let-fun).
- **`F^w Unit`:** insert `capture M to d. decode d`; bind a `String`.
- **`F^p A`:** bind `A`.
- **Free grade or computation:** fix `F^ν A` to `F^p A`, or unify the
  computation variable with `F^p α`, then bind it. This is the sole default.

Nothing else coerces: arguments and handler arms unify with their demands;
`if`, `case`, `try` and `?` arms each meet a fresh result, then those results
unify with the join in source order. A failure blames the later arm. A tail
`Run` is never captured.

## Blocks run as written

- `map : ∀α β ν. {α → Command[ν] β} → [α] → Returns [β]` accepts either
  callback grade. `map { |f| echo $f } [1, 2]` writes `1` and `2`, returning
  `[(), ()]`; it does not collect their output.
- To collect text, write `map { |f| return !{echo $f} } [1, 2]`.
- `cross` and `par` also accept command callbacks; parallel workers' output
  is replayed in input order. `flat-map` never implicitly captures, but its
  callback must return a list: it maps, then concatenates the returned lists.
- `filter` requires a returned `Bool`; `sort-list-by` requires a returned
  comparable key. `both` requires value-returning blocks:
  `both { return !{curl $a} } { return !{curl $b} }`.
- `each`, `fold`, `fold-lines` and `audit` run either body grade and keep its
  value. `spawn`/`watch` keep the body's grade in the handle; `await` restores
  it, so `let out = await $h` captures a command worker's output.

## Reading types

`Command[g] A` names a producer. Constants and impossible command grades have
short spellings.

- `w` prints `Command`; `p` prints `Returns A`.
- A free grade prints `Returns A` when its paired type cannot be `Unit`:
  a concrete non-unit type, or a quantified variable whose kind excludes unit.
- Otherwise it prints `Command[ν] A`, using `ν ξ ο π`. A scheme quantifies
  only the grades it prints.
- `cd : ∀ν. String → Command[ν] Unit`;
  `length : ∀α:sized. α → Returns Integer`; `echo : [String] → Command`.
- `{ |x| return $x } : ∀α ν. {α → Command[ν] α}`.
- `await : ∀α ν. Handle (Command[ν] α) → Command[ν] α`.

## Programs

| program | result |
| --- | --- |
| `let h = hostname`; `let f = { hostname }; let h = f` | captures |
| `let x = if $c { echo a } else { warn b }` | captures |
| `try { mkdir d } { \|e\| warn no }` | one `Command` |
| `within [handlers: [curl: { \|a\| return () }]] { let x = curl foo }` | captures `""` |
| `try { curl $u > out } { \|e\| echo failed }` | one `Command` |
| `try { each-line { \|l\| warn $l } } { \|e\| echo failed }` | one `Command` |
| `let u = (); if $c { return $u } else { echo a }` | one `Command` |
| `each { \|b\| !$b } [{ echo hi }, { return () }]` | writes `hi` |
| `map { \|f\| echo $f } [1, 2]` | writes `1` and `2`; value `[(), ()]`, which the REPL then prints |
| `let x = map { \|f\| echo $f } [1, 2]`; likewise `map $g` for a command block `$g` | writes `1` and `2`; binds `[(), ()]` |
| `cross [1, 3] [2] { \|x y\| echo "$x$y" }` | writes `12` and `32`; value `[(), ()]` |
| `par { \|u\| curl $u } $us 4` | output replayed in order; value `[(), …]` |
| `let x = try { echo a } { \|e\| return 'none' }` | refused, join hint |
| `let x = if $c { hostname } else { return 'b' }` | refused, join hint |
| `both { echo a } { echo b }` | refused, argument hint |
| `let x = if $c { echo a } else { from-json < f }` | refused, join hint |
| `sort-list-by { \|f\| echo $f } $xs`; `group-by`, `median-by` with a command key | refused, argument hint |
| `let g = { \|f\| let y = !$f; return $y }; g { echo hi }` | refused, argument hint |
| `let twice = { \|f\| let _ = !$f; !$f }`; `if $c { twice { return () } } else { echo a }` | refused, join hint |

For a command where a value is required, return an explicit capture
(`{ return !{…} }`) or bind it first. Use `succeeds { … }` for a yes/no answer,
or decode output to the required type. In the `twice` example, end its arm
with `return ()` to supply a fresh unit producer.

## The whole computation is the buffer

**Capture preserves exactly the bytes written by its body.** An inner `let`
captures for itself; every other write, including a `guard` cleanup, goes to
the enclosing sink. `let x = !{ echo a; echo b }` binds `"a\nb"`, while
`let x = !{ let y = echo a; echo b }` binds `"b"` and `y` is `"a"`.

- `CompKind::Capture` returns `Bytes`; `Prim::Decode` drops one trailing LF
  or CRLF and strictly decodes UTF-8. Invalid text fails; use `from-bytes`
  to keep binary data. Neither term has surface syntax.
- `rhs_bound_ty` returns `Bound::Captured` or `Bound::Thunked`;
  `infer_let` applies it immediately to the right-hand side it inferred.
- A stand-in runs whole under its caller's capture. A fresh unit-returning
  stub for an external is a command and captures `""`.
- A capture reaching the 16 MiB buffer cap fails rather than bind a prefix.
  On a halt, `Frame::Capture` restores its previous sink and drops the buffer:
  a capture that does not become its value commits nothing, so
  `let x = try { return !{ cmd } } { |_| return 'x' }` writes nothing to
  stdout. On error, each capture adds its actual byte count to
  `Unwound.discarded`, summed saturatingly across nesting. One renderer shows
  one counted note, after the hints and advice (`docs/SPEC.md` §13.1) and
  before any eligible abandoned-tail note. The count is report-only and does not enter `[reason, frame, site]` or
  survive `raise` of that record; escapes carry no unwind facts. `audit`
  keeps a command's output
  ([[decisions/261105_the-error-record|the error record]]).

See [[design/types|types]], [[design/codecs|codecs]],
[[internals/output-capture-and-detachment|output capture]],
[[map/core/typecheck|typecheck]]. Cite: `docs/SPEC.md` §§7.2, 17.3–17.7.
