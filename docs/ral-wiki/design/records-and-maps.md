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

The split lives in `Ty` (`core/src/ty.rs`):

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

- **Bare word** — `host: 5432` — a label.
- **Single-quoted word** — `'content-type': "json"` — a label that may carry
  characters a bare word cannot.
- **Double-quoted string** — `["a": 1]` — data: a string expression, whether
  or not it interpolates.
- **Deref** — `[$k: $v]` — data: the key is evaluated at run time and must be
  a `String`.

The first two are *labels*: known at elaboration, so they make a record. They
are the **bare** alphabet, and it is the only one a key may draw on. The last
two are *data*: the split is the one the shell already draws between a quoted
word and a string that is built, and the one a map's index draws, since
`$m["a"]` and `$m[$k]` both read a map where `$r[a]` reads a record's field.
(Elixir's `%{a: 1}` against `%{"a" => 1}` is the precedent.)

Backtick labels are the *tag* alphabet, and a tag names a constructor rather
than a part, so it keys nothing. Each label alphabet belongs to one type
former: bare to `Record`, tag to `Variant` and to the `case` arms that
eliminate one. In the row that carries them the alphabet is a constructor of
`Label` ([[design/row-types|row-types]]), so a field's *spelling* cannot make
it a tag.

## Which one a literal becomes

**The classification is syntax, not inference.** The parser reads the keys and
emits `Ast::Record` or `Ast::Map` (`core/src/syntax/parser.rs`), and each has
its own entry type, so a record literal cannot hold a data key:

- **`[:]` is the empty map**, the one entry with both sides erased, and the
  only literal that opens with `:`. There is no map *marker*: `[:, a: 1]` was
  one, and went when the quoted key arrived, so that every colon in a bracket
  is an entry's.
- **A form's bracket is the form's own.** `within`'s and `grant`'s first
  operand is read by the form, not by the collection rule, so `[]` there is the
  empty *option set* and `[:]` names no options at all. That is the one place
  the `[]`/`[:]` ambiguity bites, and closing it there is what makes
  `grant [] { … }` mean what it reads as.
- **Every key a label** → `Record`. `[host: "db", port: 5432]` infers
  `[host: String, port: Int]`.
- **Any key data** → `Map<α>`. `["a": 1, "b": 2]` and `[$k: 1, $j: 2]` infer
  `Map<Int>` — one data key collapses the whole literal to homogeneous, because
  a keyset that is data cannot carry per-label types.
- **Spreads alone settle nothing**, so a literal built only of them is a list.
  A spread in a record literal must be a record and in a map literal a map; the
  two never merge into one another, and two maps merge by `union`, not by a
  literal.
- **A spread in a record literal shadows by prepending.** `[...$cfg, port: 9090]`
  unifies `$cfg` as a row — see [[design/row-types|row-types]].

The checker then has two rules rather than one — `infer_record_val` and
`infer_map_val` (`core/src/typecheck/infer.rs`) — and no order-dependent
classification to make.

This is why your two everyday cases land where they do. `audit { … }` returns a
record — its labels (`kind`, `status`, `stdout`, `value`, …) are fixed in the
builtin's scheme (`core/src/typecheck/builtins.rs`). `from-json` returns a
result the script decides — a weak type variable, one per unit, checked at the
decode ([[decisions/260930_a-boundary-is-checked-against-its-type|a-boundary-is-checked-against-its-type]])
— and a JSON object decodes to the `Map` carrier
at run time (`json_to_value`, `core/src/builtins/codecs.rs`). The decoded value's
*type* is then fixed by how you use it: index it with a computed key and it
pins to `Map<α>`; read a literal field off it and it pins toward `Record`.

## Indexing makes the duality operational

The projection rule splits on *how the key is given*, not only on the target
(`core/src/typecheck/infer.rs`):

- **Static label** — `$r[host]` — is a deferred constraint over the target's
  head, `c = [host: α | ρ] ∨ c = Map α`. A record target reads the field at its
  own type; a map target reads that key at the map's element type; a target
  still unknown waits, and is read as a record where the binding that owns it
  is generalised, or at the end of the unit. Only the head decides, never
  statement order: `{ |m| $m[a]; keys $m }` and its transpose are both
  `Map α → Command α`, while `{ |r| $r[host] }` is generic over records and is
  refused when given a map.
- **Integer literal** — `$xs[0]` — indexes a list, `c = [α]`, with no
  constraint, so `first = { |xs| $xs[0] }` is generic over element types.
- **Computed key** — `$m[$k]` — is `c = [α] ∧ k = Int ∨ c = Map α ∧ k = String`,
  returning the uniform `α`. The target's head, the key's head, or either's
  kind picks the disjunct; nothing else does, so the verdict is the same under
  any order of statements. The three types are weak for the whole unit: a helper
  `{ |m k| $m[$k] }` has one container type per program, accepted at the one
  type its program uses and refused, *"is `$m` a list or a map?"*, when nothing
  fixes which. Indexing a concretely-known scalar by a runtime key is a type
  error: *"only lists (key: Integer) and maps (key: String) accept a key
  computed at runtime — for a record field, use a static name."*

`env` is `F (Map String)`, so `!{env}[$name]` and `!{env}[HOME]` alike are `String`:
every value shares one type, and a key you don't know until run time still has
a known result type.

## Neither stands in for the other

A record and a map never unify — not directly, and not under `List`, `Thunk`,
`Fun` or `Handle`. The map-keyed builtins are typed on `Map` — `keys :: ∀α. Map<α> → F [Str]`,
`has :: ∀α. Map<α> → Str → F Bool` (`core/src/typecheck/builtins.rs`) — so they
take a map and only a map: `keys [a: 1, b: 2]` is a type error and
`keys ["a": 1, "b": 2]` is the program that was meant.

The refusal is derived from the two types' shapes rather than from the site
that raised it (`core/src/typecheck/explain.rs`), so the same sentence answers
an argument, a spread and a branch join alike — the last being the commonest
way in, as in `if c { ["k": $v] } else { [k: ''] }`, where the fix is `[:]`
for the empty map, there being no empty-record literal. A map type prints as
`Map α`, so the diagnosis names the two kinds before the help does.

**A form's options are names, so a map is not one.** `within` and `grant`
declare their options — `dir`, `env` and `handler`; `exec`, `fs`, `net`,
`detach`, `editor` and `shell` — and the options are *syntax*: the names are
written in the form's own bracket and each value is checked at the type its
name declares, key by written key. A bundle bound elsewhere, a genuine map (one
off `from-json`, a plugin's configuration), a spread and a repeated name are
parse errors, each with its own sentence: *`within` takes its options written
in its own bracket. The values may be bound — `within [dir: $d]` — but the
option names are written here, the way `case` writes its arms.* The runtime
still takes a `Map`, records being maps at run time, and the machine builds one
from the written pairs; what the type system never sees is a record standing
for the options.

A forgetful `Record → Map` reading is definable — collapse every field type onto
one element and forget the labels — but it is a *coercion*, and a unifier
expresses only equalities. Held as an equality it runs backwards too: a map
would then stand where a record is expected, closing that record's row and
answering for fields nobody wrote, and which of the two a literal is asked to
be would depend on the order the solver reached it in. The sound direction — a
homogeneous record where a map is wanted — is recoverable by writing the
literal as a map (`["k": …]`), which says it in the source rather than in the
unifier.
## Order is not data

One carrier, one order: `Map` is an `imbl::OrdMap` (`core/src/types/map.rs`), so
both views iterate **sorted by key** and the order a literal was written in is
not observable. That is what lets `Value::equals` settle map equality with a
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
carrying no information — written `["alice": (), "bob": ()]`, with `has` for
membership and `union` / `intersection` / `difference` in the prelude
(`docs/SPEC.md` §4.5).

## The empty-record gap

There is no empty-record literal. `[]` is the empty *list* — no entries means
no keys, static or computed, so the parser has nothing to classify by and
falls back to the third literal kind rather than to either keyed one
(`core/src/syntax/parser.rs`). A record with its fields removed one at a time
does not converge on `[]`; it converges on the empty map `[:]`, because
`[a: 1]` losing its last field has, at that point, no static key left to be
a record *of*. The gap is real, not a documentation oversight: there is
nothing to add to the grammar that would make `[]` mean "record, zero
fields" without also making it ambiguous with "list, zero elements" at every
other use of `[]` in the language — the list reading is needed far more
often, and disambiguating by expected type would make a literal's kind
depend on where it appears rather than on what it says, which is exactly the
property [[design/row-types|row-types]] classification does not have.

This is not a cosmetic asymmetry. A record's keyset is part of its *type* —
that is the entire content of the record/map split above — so a record with
no keyset is not a smaller record, it is a value with no type to check
statically. Concretely: shrinking `[edit-mode: 'vi']` to nothing does not
leave a `[]` a checker could still hold to a table; it leaves `[:]`, a map,
which meets that table only when something applies it at run time. Three
places in `ral` hand a script's returned value straight to a fixed table
this way — an rc file, a plugin manifest, a capability profile
(`docs/SPEC.md` §15.3, §15.5, §12.12) — and in each one, emptying the
returned record is indistinguishable, at the keystroke that does it, from
opting the whole file out of static checking. A user who deletes a config's
last field to "leave it empty" has, without any warning at the deletion
site, also decided that every key they type next is checked only when the
file is next applied, not when it is next read.

The user has chosen to leave this as-is rather than special-case `[]` in a
contract file's return position, the way it is already special-cased inside
a form's option bracket (`within []`, `grant []`). That exception is local to
one syntactic position a form owns outright; a contract file's return
position is an ordinary expression whose value flows through the same
record/map classification as everywhere else, and giving it a second,
context-dependent meaning for `[]` would reopen exactly the ambiguity the
form-bracket exception was built to avoid in the first place, one level up.

**Superseded for contract files.** A contract file's return is now ascribed its
table after inference, in a scratch copy of the unifier, so `[]` stays the empty
list everywhere but a list or a map returned at a contract boundary is refused
statically, with the spelling to use: `["k": v]` is a map whose keys are data,
not the table's labels, and `[]` is not the empty record. A file with nothing
to set returns `()`, which the checker and each door read as the empty keyset —
the exception is a value's own, not a second meaning for `[]`. The cliff above
is closed in the sense that matters: emptying a config no longer opts the file
out of static checking. A return the checker cannot type — `from-json` — and a
plugin manifest's factory stay on the runtime door.

**Realised in** [[internals/type-inference|type-inference]] (literal inference
and the record/map projection split).

Cite: `docs/SPEC.md` §4.5, §12.12, §15.3, §15.5; `core/src/typecheck/{ty,infer,unify,builtins,contract}.rs`.
