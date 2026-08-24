---
status: active
---

# The edit witness is `h` plus six hex, so it lexes as a string

**The exarch line-hash witness is the letter `h` followed by six hex characters,
because a bare witness must round-trip through ral's value model as a `String`,
and an all-hex token can be all digits.** Bare-word elaboration classifies a
token syntactically before the type checker runs: `Val::from_word` sends any
word that parses as `i64` to `Val::Int` and only the rest to `Val::String`
([[internals/surface-syntax|surface-syntax]]). An agent reads a witness from
`view` and types it back as a bare `edit` argument, so an all-digit digest like
`152347` elaborates to `Val::Int(152347)` while the hash `edit` recomputes is
`Val::String("152347")`. `equal` is structural with no Int×String arm, so the
endpoint check rejects a correct witness and the agent loops re-issuing the same
edit. The `h` prefix removes the ambiguity at the source: the witness can never
parse as a number, so it is a `String` at every call site and the comparison
compares like with like.

Alternatives, and why they lose:

- **String-coerce the comparison in `edit`** (`to-string $start-hash`). Incomplete,
  not merely inelegant: a leading-zero digest is already `Val::Int(12345)` by the
  time `edit` runs — the zero is gone at elaboration — so `to-string` yields
  `"12345" ≠ "012345"`. The string-ness must be preserved *before* classification,
  not reconstructed after. Correctness rules this out, not taste.
- **An Int×String arm in `equal`.** A wrong repo-wide semantic — `equal 1 "1"`
  would become true. The fault is the witnessed-edit layer reading a string token
  as a number, not equality.
- **Type-directed literal elaboration** — let a numeric-shaped word stay a
  `String` when it lands in a `String`-typed hole. The principled language-level
  fix and not a coercion at all (bidirectional elaboration, `fromInteger`-style).
  But elaboration is syntax-directed and precedes typing: `elab_expr` calls
  `Val::from_word` with no expected type in scope. Achieving it needs bidirectional
  elaboration threading expected types to leaves, or a polymorphic literal with
  defaulting rules — disproportionate to a witness format. Not taken here, and
  since **declined** for the language as a whole: a numeral denotes its number in
  every position and a token meaning bytes is quoted, so nothing type-directed
  decides a leaf
  ([[decisions/260824_a-numeral-denotes-its-number|a-numeral-denotes-its-number]],
  which also leaves qualified literals with evidence open). The `h` prefix is
  therefore the permanent shape of a witness, not a way-station.
- **Drop bare-word Int classification** (write `!{int 50}`). Relocates the
  coercion to every numeric builtin boundary as an implicit `String→Int`, and ral
  is a shell where bare integers are the common case. Strictly worse on the very
  axis it was meant to clean up.

The change is the witness *format*, owned by exarch: the producer (`view`,
`grep-files`) and the consumer (`edit`) both call `line_hash`, so the prefix is
internally self-consistent; only the documented six-hex contract moves with it.

See also [[design/hash-addressed-editing|hash-addressed-editing]],
[[map/exarch/builtins|builtins]], [[map/exarch/shell-eval|shell-eval]].
