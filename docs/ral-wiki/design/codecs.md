# Codecs: the typed crossing between bytes and values

**The `from-X` / `to-X` family is where a program deliberately moves a
[[design/pipelines|pipeline]] edge between the byte channel and structured
values, and the move is a *typing* fact: every `from-X` has computation type
`F[Bytes, ∅] A` and every `to-X` has `F[∅, Bytes] Bytes`.** These are the two
asymmetric [[design/types|byte modes]] — `mode.rs` names them `decode` and
`encode`, right beside `none` and `ext` — so the codecs are simply the builtins
that carry a byte mode on one side and `∅` on the other.

## Byte modes make the crossing checkable

A computation has type `F[I,O] A` over an input and output byte mode, each `∅`,
`Bytes`, or a mode variable `μ` ([[design/types|types]]). Four `PipeSpec` shapes
name themselves in `mode.rs`:

- `none` — `F[∅, ∅]` — a pure value builtin; no byte channel touched.
- `ext` — `F[Bytes, Bytes]` — an external command or byte filter.
- `decode` — `F[Bytes, ∅]` — **consume the byte channel, return a value.**
- `encode` — `F[∅, Bytes]` — **consume a value, emit on the byte channel.**

`from-X` *is* `decode`; `to-X` *is* `encode`. The asymmetry is the whole point.
Because the [[design/pipelines|pipe]] connects two stages only when the left
output mode equals the right input mode, the modes admit `cmd | from-json`
(`Bytes` meets `Bytes`) and `to-json $x | cmd` (`Bytes` meets `Bytes`) and reject
threading a value where bytes are due — the boundary cannot be crossed by
accident, only by naming a codec. Decoding is therefore typed *at* the crossing:
`from-json` yields a fresh value type the inferencer threads onward, and a
misspelled codec fails at command lookup rather than as a runtime "unknown
codec" string ([[design/builtins|why each codec is its own builtin]]).

## The two directions

`from-X` takes **no value argument** — it reads the byte channel, whether that
channel is a `< file` redirect or the left stage of a pipeline. Each decoder is
declared arity 0, so passing a value is a *type* error raised before the call
runs (`` `from-json` takes no argument — it reads the byte channel ``) whose
hint names the fix: to decode a value already in hand, pipe it through the
matching `to-X` encoder, so the bytes re-enter the channel a decoder reads —
`to-string $s | from-json` for JSON in a `String`, `to-bytes $b | from-string`
for a `Bytes` value (e.g. `$r[stdout]` from `await`). The decoders:

- `from-bytes` → `Bytes`, the raw escape hatch;
- `from-string` → `String`, **strict** UTF-8;
- `from-line` → `String`, strict, one trailing `\n` / `\r\n` stripped;
- `from-json` → a decoded value, strict UTF-8 then JSON;
- `from-csv` → a list of records keyed by the header row, every field a
  `String` (CSV is untyped — coerce with `int` / `float`); the underlying
  reader handles quoted fields, embedded commas, and embedded newlines;
- `from-lines` → a line stream (below).

`to-X` takes one value and writes its encoded form to the byte channel.
`to-bytes`, `to-string`, `to-lines` (newline-join a list), `to-json`, and
`to-csv` return the `Bytes` they wrote; `to-line` is the line-writer used by
`echo` and returns `Unit`, so a value boundary captures its final line as a
`String`. All share the `encode` mode. `to-csv` takes a list of records and emits a header plus one row each;
columns are the first record's keys in sorted order (maps are key-ordered, so
there is no original column order to recover) and a record missing a column
contributes an empty field.
`to-json` maps a ral value to JSON structurally: a record or `[String:A]` map
becomes an object, a list an array, `Unit` becomes `null`, and a variant
`` `tag payload `` becomes `{"tag": "tag", "payload": …}` — the `payload` key
omitted for a niladic tag. `Bytes` serialises as an array of byte integers; a
`Lambda`, `Block`, or `Handle` has no JSON image and is an error.

## Whole-buffer vs. streaming

The structured decoders (`from-string`, `from-line`, `from-json`) read all of
stdin and then decode — whole-buffer. So does `from-lines`, despite its
stream-shaped result: it yields a `Step` stream — a `more [head, tail]` /
`done` open variant whose tail is a thunk
([[invariants/optionality-via-variants|open variants]]) — but the whole chain
is built eagerly from stdin read to EOF, so the *interface* is incremental
while the memory profile matches the other decoders. One codec is genuinely
*streaming*, the way to process unbounded input without holding it:

- `fold-lines <fn> <init>` folds over stdin line by line. Its type is
  *parametric in the output mode*, `F[Bytes, μ] α`: a pure fold stays a
  `decode` (`μ = ∅`), but a callback that emits bytes per line lifts the whole
  stage to `F[Bytes, Bytes] α` — a decoder that re-encodes is back to `ext`.
  The mode is inferred from what the callback does, not declared.

## Strict values, lossy lines

The structured decoders and an [[design/types|external command captured by
`let`]] are **strict**: invalid UTF-8 is an error that points at `from-bytes`,
because a `String` or JSON value you will compute with must not silently carry a
replacement character. `from-lines` alone is **lossy, hence total**: scanning
lines tolerates a `�`, while constructing a scalar `String` does not.

See also [[design/builtins|builtins]], [[design/pipelines|pipelines]],
[[design/types|types]], [[design/cbpv|cbpv]]; [[map/core/builtins|map: builtins]],
[[map/core/io-process|io-process]].
Cite: RATIONALE §"Values and commands", §"Pipelines follow their edges";
`docs/SPEC.md` §4.2, §16, §20.
