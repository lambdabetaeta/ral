---
status: accepted
generated_at_commit: 9c8aa5d9
---

# A written key is a name

**How a key is written decides what it reads. Quotes never decide a type.**
A written name reads a record field, a whole number a list element, and a
computed string a map value. Data arriving from outside takes the type the
script uses, checked at the door.

## What was decided

- **One rule per read.** `$r[host]`, `$r['user name']`, `$r["user name"]`
  and `$r[true]` require `[label: α | ρ]`. `$xs[0]` requires `[α]`.
  `$m[$k]`, `$m["k$i"]`, `$m[!{f}]` and `$m[$[…]]` require `Map α` and a
  `String` key. Each unifies immediately; none defers, defaults a shape or
  marks a variable weak.
- **Computed list positions use `nth`.** `nth : ∀α. Int → [α] → F α`,
  written `nth $i $xs`, shares the list-index runtime checks. Written
  positions are whole numbers from zero; negative or fractional numerals are
  parse errors with a hint to quote a numeric field name. Paths and tilde
  words must be quoted as names too; a tag keys nothing.
- **Literals and patterns share the alphabet.** Words and constant quoted
  strings name fields, so `[a: 1, "b c": 2, 'd': 3]` is a record. Any
  computed key makes the literal a map, its written keys becoming data.
  `[:]` is the empty map; `[:, a: 1, b: 2]` explicitly builds a map on
  written keys, with spreads permitted. Unkeyed entries and `[:,]` are
  refused. Patterns accept written keys only; a spliced one is refused.
- **Options and contracts follow suit.** A constant `"dir":` names a
  `within` or `grant` option; computed names remain refused.
  `return ["edit-mode": 'vi']` is a record checked against the rc table,
  as quoted fields are against manifest and profile tables.
- **`env : ∀α. F α` is a boundary.** `!{env}[HOME]` checks the named field
  at that call; `keys !{env}` or `has !{env} X` uses a map. Every call has
  its own site. Values remain text.
- **`from-csv : ∀α. F [α]` is a boundary.** Label reads use record rows,
  computed reads map rows. Cells remain text; numeric use fails at decode,
  naming the column and line of use.
- **`to-csv : ∀α:data. [α] → writes` accepts record or map rows.** Every
  row uses the same runtime carrier with the first row's keys. Cells are
  `String`, `Int`, `Float` or `Bool`, rendered as `str` renders them.
  Any other cell is refused with its zero-based row index and key, before
  any bytes are written.
- **The boundary set is closed:** `from-json`, `from-jsonl`, `from-json-at`,
  `from-csv`, `env`, `use`, `exarch-agents`, `exarch-pins`,
  `exarch-transcript` and `_ed-state`.
- **Key form is represented, not rediscovered.** `IndexKey` in the AST and
  `Key` in the IR are `Label`, `Position` or `Data`. Only `Data` contains an
  expression to traverse. Read errors are ordinary unification failures;
  their reasons and the heads found select the help, including `nth` for a
  computed list position ([[design/records-and-maps|records and maps]]).

## Why

A read with one typing rule preserves principal types. A record helper stays
row-polymorphic and a map helper element-polymorphic, whether its body is
inline or named. No disjunct must be chosen from later uses, so no index-made
weakness or value-shape default is needed. Surface reads correspond one to
one with the formal core's **`proj`, `index` and `lookup`**.

A boundary already knows both the incoming value and the type its script
needs. Checking there preserves typed decoding before downstream effects
without asking outside data to choose records or maps in advance.

## Rejected

- **An opaque `Json` type:** loses typed decoding before effects; exarch
  replies use `FOValue`, which also carries variants.
- **`$r.host`:** collides with interpolation such as `"$f.txt"`.
- **Quotes marking data:** makes `$x["HOME"]` differ from `$x[HOME]` for no
  useful reason.
- **Keeping `Idx` for computed list positions:** retains weak variables to
  serve four call sites; `nth` expresses the operation directly.

## Effect on earlier decisions

This supersedes the `Lbl` and `Idx` clauses and the CSV-row refusal of
[[decisions/260930_operators-are-kinded|operators are kinded]], and the
index-made weakness and pending-label clauses of
[[decisions/260930_a-let-generalises-what-is-not-weak|a let generalises what is not weak]].
It amends the closed set of
[[decisions/260930_a-boundary-is-checked-against-its-type|a boundary is checked against its type]]
and the `env` scheme of
[[decisions/261005_ambient-reads-are-builtins|ambient reads are builtins]].
The kind discipline and boundary weakness remain unchanged.

Cite: `docs/SPEC.md` §4.5–4.7, §7.3, §17.3.
