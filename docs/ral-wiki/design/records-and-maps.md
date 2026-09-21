# Records and maps

**ral keeps one runtime carrier for string-keyed data and two static types over
it: a *record* whose labels the checker knows, and a *map* whose keys are
runtime values.** Both are the same bag of `String → Value` pairs at run time;
the type discipline draws the line between *heterogeneous data on statically
known labels* and *homogeneous data on a runtime-determined keyset*. The whole
design is that duality and nothing more.

## One carrier

At run time there is no record/map distinction at all. A `[…]` value is a single
string-keyed ordered map — `Value::Map` (`core/src/types/value.rs`); there is no
`Value::Record`. Keys are always strings: numbers index *lists*, never maps.
So the set of possible keys is identical for both; what differs is everything
the checker is allowed to say about them.

## Two types

The split lives in `Ty` (`core/src/typecheck/ty.rs`):

- ***Map*** — `Map<α>`: a homogeneous, string-keyed collection. Every value has
  the one type `α`; *which* keys are present is unknown to the checker, because
  they are computed at run time.
- ***Record*** — `Record(Row)`: a row `[l₁: A₁, …, lₙ: Aₙ | ρ]` of statically
  known labels, each carrying its *own* type, with an open tail `ρ` for unknown
  further fields (the [[design/row-types|row-types]] discipline,
  [[related/scoped-labels|Leijen 2005]]).

They answer different static questions over the same pairs:

| | keyset | value types | access |
|---|---|---|---|
| **Record** `∏ₗ Aₗ` | static labels (+ open tail `ρ`) | per-label, heterogeneous | by literal label, resolved at compile time |
| **Map** `String ⇀ α` | runtime, unbounded | uniform `α` | by a key computed at run time |

A record is a dependent product over a fixed label set — its fibres vary. A map
is a finitely-supported function `String ⇀ α` — its fibres are uniform but its
domain is a run-time value. Neither contains the other.

## What can be a key

A key is a `String` either way (`docs/SPEC.md` §4.5). The *form* you write it in
decides which type the literal takes:

- **Bare word** — `host: 5432` — a static label.
- **Quoted string** — `'content-type': "json"` — a static label that may carry
  characters a bare word cannot.
- **Deref / computed** — `[$k: $v]` — the key is evaluated at run time and must
  be a `String`.

The first two are *labels*: known at elaboration, so they make a record. They
are the **bare** alphabet, and it is the only one a key may draw on. Backtick
labels are the *tag* alphabet, and a tag names a constructor rather than a
part, so it keys nothing. Each alphabet belongs to one type former: bare to
`Record`, tag to `Variant` and to the `case` arms that eliminate one. In the
row that carries them the alphabet is a constructor of `Label`
([[design/row-types|row-types]]), so a field's *spelling* cannot make it a
tag.

## Which one a literal becomes

**The classification is syntax, not inference.** The parser reads the keys and
emits `Ast::Record` or `Ast::Map` (`core/src/syntax/parser.rs`), and each has
its own entry type, so a record literal cannot hold a computed key:

- **`[:` marks a map.** `[:]` is the empty one and `[:, a: 1, b: 2]` a map whose
  keys are written out: `Map<Int>`.
- **Every key a static label** → `Record`. `[host: "db", port: 5432]` infers
  `[host: String, port: Int]`.
- **Any key computed** → `Map<α>`. `[$k: 1, $j: 2]` infers `Map<Int>` — one
  computed key collapses the whole literal to homogeneous, because a runtime
  keyset cannot carry per-label types.
- **Spreads alone settle nothing**, so a literal built only of them is a list.
  A spread in a record literal must be a record and in a map literal a map; the
  two never merge into one another.
- **A spread in a record literal shadows by prepending.** `[...$cfg, port: 9090]`
  unifies `$cfg` as a row — see [[design/row-types|row-types]].

The checker then has two rules rather than one — `infer_record_val` and
`infer_map_val` (`core/src/typecheck/infer.rs`) — and no order-dependent
classification to make.

This is why your two everyday cases land where they do. `audit { … }` returns a
record — its labels (`kind`, `status`, `stdout`, `value`, …) are fixed in the
builtin's scheme (`core/src/typecheck/builtins.rs`). `from-json` returns an
*unconstrained* result — a quantified type variable in its scheme — and a JSON
object decodes to the `Map` carrier
at run time (`json_to_value`, `core/src/builtins/util.rs`). The decoded value's
*type* is then fixed by how you use it: index it with a computed key and it
pins to `Map<α>`; read a literal field off it and it pins toward `Record`.

## Indexing makes the duality operational

The projection rule splits on *how the key is given*, not only on the target
(`core/src/typecheck/infer.rs`):

- **Static label** — `$r[host]` — is field selection *whatever the target is*:
  it unifies the target with `[host: α | ρ]` and returns that field's own type
  `α`. The key's form decides before the target's type is read, so a literal
  read and the same read extracted into a block (`{ |r| $r[host] } $x`) cannot
  reach different verdicts. A target already known to be a map is refused, with
  the way to read it: *"`host` is a field name, and this is a map — read a map's
  key with `get $m host <default>`, or bind the key and write `$m[$k]`."*
- **Computed key** — `$m[$k]` — is well-typed against `Map<α>`, returns the
  uniform `α`, and is the one place the target's own type still selects the
  rule: a `List` takes an `Int`, a `Map` a `String`, and a still-free target is
  pinned by the key's type. Indexing a concretely-known scalar by a runtime key
  is a type error: *"only lists (key: Integer) and maps (key: String) accept a
  key computed at runtime — for a record field, use a static name."*

So `$ENV[$name]` is sound precisely because the computed key pins `$ENV` to a
`Map`: every value shares one type, so a key you don't know until run time
still has a known result type. `$ENV[HOME]` pins that occurrence to a record
with a `HOME` field instead — each `$ENV` types on its own, the register
carrying no scheme of its own (`core/src/typecheck/infer.rs`).

## Neither stands in for the other

A record and a map never unify — not directly, and not under `List`, `Thunk`,
`Fun` or `Handle`. The map-keyed builtins are typed on `Map` — `keys :: ∀α. Map<α> → F [Str]`,
`has :: ∀α. Map<α> → Str → F Bool` (`core/src/typecheck/builtins.rs`) — so they
take a map and only a map: `keys [a: 1, b: 2]` is a type error and
`keys [:, a: 1, b: 2]` is the program that was meant.

A forgetful `Record → Map` reading is definable — collapse every field type onto
one element and forget the labels — but it is a *coercion*, and a unifier
expresses only equalities. Held as an equality it runs backwards too: a map
would then stand where a record is expected, closing that record's row and
answering for fields nobody wrote, and which of the two a literal is asked to
be would depend on the order the solver reached it in. The sound direction — a
homogeneous record where a map is wanted — is recoverable by writing the
literal as a map (`[:, …]`), which says it in the source rather than in the
unifier.
## Order is not data

One carrier, one order: `Map` is an `imbl::OrdMap` (`core/src/types/map.rs`), so
both views iterate **sorted by key** and the order a literal was written in is
not observable. That is what lets `values_equal` settle map equality with a
pointwise zip and makes `Value::PartialEq` order-independent for free — and it
is why neither view can carry a sequence. Data that must keep an order is a
list; data needing an order *and* a type per key is neither, and ral has no
third carrier for it.

## Why both, not one

Neither type subsumes the other, so collapsing to one loses something real.

- **"Just records" loses runtime-keyed access.** `$m[$k]` for a computed `$k` has
  a result type only when every value shares a type; over a heterogeneous record
  there is no principal type to give. To make records cover it you would promote
  labels to first-class runtime values — at which point `$r[host]` can no longer
  be resolved to a field type, and you have collapsed records *down* into untyped
  dictionaries, discarding the static field-typing that is their entire point.
  You would also have no type for data whose keys arrive at run time:
  `from-json` of an arbitrary object, `[$k: v]`, the [[design/scoping|environment]].
- **"Just maps" loses heterogeneity.** Under one `Map<α>` every field is `α`, so
  `[host: String, port: Int]` could not distinguish `$r[host] : String` from
  `$r[port] : Int`, and the shell's own control-flow records would be untypable:
  `try`'s `[status: Int, cmd: String, message: String, …]`, `await`'s
  `[value: α, stdout: Bytes, stderr: Bytes]`, the `audit` node
  (`core/src/typecheck/builtins.rs`). `await`'s `value: α` is polymorphic in the
  block's return type *beside* `stdout: Bytes` — no `Map<?>` types that.

These are exactly the two shapes a shell handles: the structured result of a
known command has a fixed heterogeneous form (a record — the
[[design/builtins|builtins]] thesis of no bytes→text→structured round-trip),
while the environment, a parsed JSON object, or a computed-key literal is a
homogeneous association on a runtime keyset (a map). The split is forced by the
data, not chosen for tidiness.

A *set* is the degenerate homogeneous map `Map<Unit>` — keys present, values
carrying no information — written `[:, alice: (), bob: ()]`, with `has` for
membership and `union` / `intersection` / `difference` in the prelude
(`docs/SPEC.md` §4.5).

**Realised in** [[internals/type-inference|type-inference]] (literal inference
and the record/map projection split).

Cite: `docs/SPEC.md` §4.5; `core/src/typecheck/{ty,infer,unify,builtins}.rs`.
