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
- **Quoted string** — `"content-type": "json"` — a static label that may carry
  characters a bare word cannot.
- **Tag** — `` `dev: 8080 `` — a static label in the *tag* alphabet (leading
  backtick).
- **Deref / computed** — `[$k: $v]` — the key is evaluated at run time and must
  be a `String`.

The first three are *labels*: known at elaboration, so they make a record. A
record draws its labels from two alphabets — **bare** for ordinary records,
**tag** (backtick) for tag-keyed records and [[invariants/optionality-via-variants|variants]].
The two do not unify, and mixing them in one literal is a parse error
(`docs/SPEC.md` §4.5).

## Which one a literal becomes

The checker reads the keys (`infer_map_val`, `core/src/typecheck/infer.rs`):

- **Every key a static label** → `Record`. `[host: "db", port: 5432]` infers
  `[host: String, port: Int]`.
- **Any key computed** → `Map<α>`. `[$k: 1, $j: 2]` infers `Map<Int>` — the
  whole literal collapses to homogeneous, because a runtime keyset cannot carry
  per-label types.
- **A spread keeps it a record.** `[...$cfg, port: 9090]` unifies `$cfg` as a
  row and shadows by prepending — see [[design/row-types|row-types]].

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

- **Static label** — `$r[host]` — unifies the target with `[host: α | ρ]` and
  returns *that field's own type* `α`. Heterogeneity is fine, because the access
  is resolved at elaboration.
- **Computed key** — `$m[$k]` — is well-typed only against `Map<α>`, and returns
  the uniform `α`. Indexing a concretely-known non-map by a runtime key is a
  type error: *"only lists (key: Integer) and maps (key: String) accept a key
  computed at runtime — for a record field, use a static name."*

So `$env[$name]` is sound precisely because `env` is a `Map`
(`core/src/typecheck/builtins.rs`): every value shares one type, so a key you
don't know until run time still has a known result type.

## The coercion: record → map, forgetful and one-way

A record may be used where a `Map<α>` is expected. Unification carries the one
coercion (`unify_map_record`, `core/src/typecheck/unify.rs`): it unifies *every*
field type of the row with the map's element `α` and **closes the tail** (`ρ ↦
[]`). Consequences:

- The map-keyed builtins are typed on `Map` — `keys :: ∀α. Map<α> → F [Str]`,
  `has :: ∀α. Map<α> → Str → F Bool` (`core/src/typecheck/builtins.rs`) — yet
  they accept a **homogeneous** record through the coercion. `keys [a: 1, b: 2]`
  type-checks: the row `[a: Int, b: Int]` collapses to `Map<Int>`.
- A **heterogeneous** record is rejected: `keys [host: "x", port: 8080]` forces
  `String ~ Int` and fails. There is no uniform `α`.
- The coercion is **forgetful** — it discards the labels-as-types and keeps only
  the uniform fibre — and **one-way**: there is no total `Map → Record`, because
  static labels cannot be recovered from runtime keys. Passing an *open* record
  into a map position closes its row; the record view, with its tail, is not
  recoverable downstream.

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
carrying no information — with `has` for membership and `union` / `intersection`
/ `difference` in the prelude (`docs/SPEC.md` §4.5).

**Realised in** [[internals/type-inference|type-inference]] (literal inference,
the record/map projection split, the `unify_map_record` coercion).

Cite: `docs/SPEC.md` §4.5; `core/src/typecheck/{ty,infer,unify,builtins}.rs`.
