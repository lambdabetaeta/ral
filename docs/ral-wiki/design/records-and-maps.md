---
generated_at_commit: 1dc7c6e715426917bb7a648cf6e67ea5945065b0
---

# Records and maps

**A written key is a name; a computed key is data.** Records carry a type per
named field, maps one type for all their values. Quotes never choose between
them. [[decisions/261102_a-written-key-is-a-name|A written key is a name]]
fixes the surface and its three read rules.

## One carrier, two types

Both use `Value::Map`, an ordered collection of `String → Value` pairs
(`core/src/shell/runtime/{value,map}.rs`). The static types never unify:

- **Record** `[l₁: A₁, …, lₙ: Aₙ | ρ]`: known labels, heterogeneous fields,
  an open row for further fields ([[design/row-types|row types]]).
- **Map** `Map α`: a runtime keyset, homogeneous values, no promise that a
  particular key is present.

Both iterate sorted by key; written order is not data. Ordered data belongs
in a list. A set is `Map Unit`, as in `[:, alice: (), bob: ()]`.

## The alphabet is shared

A bare word, a single-quoted word, or a double-quoted string without a splice
is a written label. `true` and `false` name fields too. `$k`, `"k$i"`,
`!{f}` and `$[…]` are computed keys and must produce `String`. A tag names
a constructor, never a field or key.

- **All written keys give a record:** `[a: 1, "b c": 2, 'd': 3]`.
- **Any computed key gives a map:** `[$k: 1, b: 2]`; the written keys become
  data too, and every value has one type.
- **`[:, …]` explicitly builds a map:** `[:, a: 1, b: 2]`, or
  `[:, ...$m, a: 1]`. Unkeyed entries are refused; `[:,]` is refused with
  `write [:] for the empty map`.
- **`[:]` is the empty map; `[]` is the empty list.** Spreads alone give a
  list, and there is no empty-record literal.
- **Patterns take written keys only:** `let ["user name": u] = $r`.
  A spliced key is refused: `a pattern's key is written out; "…$x…" is computed`.
- **Options name their fields:** `within ["dir": $d] { … }` is valid;
  computed names in `within` and `grant` remain refused.

Classification happens in the parser, not in inference. `infer_record_val`
and `infer_map_val` then apply their respective rules. A record spread is an
update of one base: it comes first, replaces existing fields at their written
types, and adds none. A map spread must be a map. Explicit map entries beat
all spreads; the first spread wins among spreads.

## A read has one rule

| Key | Reads | Immediate constraint |
|---|---|---|
| `$r[host]`, `$r['user name']`, `$r["user name"]`, `$r[true]` | record field | `r = [label: α \| ρ]` |
| `$xs[0]` | list element | `xs = [α]` |
| `$m[$k]`, `$m["k$i"]`, `$m[!{f}]`, `$m[$[…]]` | map value | `m = Map α`, key `String` |

A written position is a whole number from zero. Negative or fractional
numerals are parse errors, with a hint to quote a numeric field name:
`$r['1.5']`. A path or tilde word must also be quoted, as in `$r['a/b']`.
A computed list position uses **`nth : ∀α. Int → [α] → F α`**, written
`nth $i $xs`, with the same runtime checks as a written position.

The AST's `IndexKey` and IR's `Key` carry `Label`, `Position` or `Data`.
No pass recovers the form from a value's shape. Only `Data` is traversed by
annotation and name collection. Each read unifies at once: no disjunction,
shape default, deferred read constraint or index-made weak variable.
`let lookup = { |m k| $m[$k] }` therefore generalises over map element types.
These reads are the formal core's `prim` operations `index (fld ℓ)`,
`index (pos k)` and `index key`.

## Data is checked at the door

`from-json`, `from-csv` and `env` take the shape the script uses, checked at
that call ([[decisions/260930_a-boundary-is-checked-against-its-type|checked boundaries]]).

- `env : ∀α. F α`: `!{env}[HOME]` uses a record and checks that field is
  present; `keys !{env}` and `has !{env} X` use maps. Each call is its own
  door. Values remain text, so a numeric use fails there.
- `from-csv : ∀α. F [α]`: label reads make rows records, computed reads make
  them maps. Cells remain text; a numeric use fails at decode, naming the
  column and the line of use.
- `to-csv : ∀α:data. [α] → writes`: rows may be records or maps, one runtime
  carrier. Every row has the first row's keys, and cells are text, numbers or
  booleans, rendered as `str` renders them. A refusal names the row index and
  key before any bytes are written ([[design/codecs|codecs]]).

A contract file returns a record or `()`, checked against its declared table.
`return ["edit-mode": 'vi']` names the record field just as an unquoted key
does. `return [:, edit-mode: 'vi']` is a map and is refused. `()` denotes no
settings; `[]` remains a list, not an empty record.

## Refusals explain the key

The unifier's message stays primary; `explain.rs` adds help by the read's
`Reason` and the head it found:

- Written key on a map: `a key written out, like HOME, names a record field,
  and this is a map, whose keys are data: read it with a computed key
  ($m[$k]) or with get $m HOME default`.
- Written key on a list: `a list is read by position: $xs[0], or nth $i $xs
  for a computed one`.
- Computed key on a record: `a computed key reads a map, and this is a record,
  whose fields have names: write the name, as in $r[host]`.
- Computed key on a list: `a computed key reads a map; for a list element at
  a computed position, write nth $i $xs`.
- Position on a record or map: `a whole number reads a list element; a field
  whose name is digits is written quoted, as in $r['0']`.
- Computed `Int` key: `a computed key reads a map, whose keys are Strings;
  for a list position, write nth $i $xs`.
- A thunk needs forcing; a scalar gets `only records, lists and maps are
  read with […]`.

For a record/map mismatch the general help is: `if these keys are data,
build a map: [$k: v], [:, a: 1, b: 2] for keys written out, and [:] for the
empty one`. `keys [a: 1]` is refused; `keys [:, a: 1]` expresses the map.
There is no implicit record-to-map coercion: an equality in the unifier would
also run backwards and invent record fields.

## Evaluation order

**All operands run before assembly validates the keys.** Elaboration hoists
map operands into binds in written order: each key, then its value, with a
spread in place. An operand failure stops the remaining operands; a repeated
computed key is refused by `Prim::Map` only after every operand has run.
Explicit entries beat spreads, and the first spread wins.

Assembly is a primitive on values in Rust and in `Extrinsic.Prims`; operand
effects are represented by the surrounding binds, not by assembly transitions.
See [[decisions/261102_the-checked-ir-is-the-calculus|the checked IR is the calculus]].

See [[internals/type-inference|type inference]], [[design/row-types|row types]],
[[invariants/fields-are-reached-by-name|fields are reached by name]].
Cite: `docs/SPEC.md` §4.5–4.7, §7.3; `core/src/{syntax/parser,ir,typecheck/infer}.rs`.
