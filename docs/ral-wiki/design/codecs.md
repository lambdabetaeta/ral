# Codecs: the typed crossing between bytes and values

**The `from-X` / `to-X` builtins name the typed crossing between a byte
pipeline and structured values.** A decoder reads the byte channel and returns
a value. An encoder takes one value, writes the encoded bytes, and those bytes
are its result:

- every decoder has the computation type `from-X : ⟨Bytes, ∅, ∅⟩ A`;
- every encoder has the type `to-X : A → ⟨∅, Bytes, Bytes⟩ Unit`.

The encoder's result mode is `Bytes`, so by WF-2 its return type is `Unit`
([[design/types|types]]). The two types are inverse: an encoder puts on the
channel what the matching decoder reads from it.

## Byte modes make the crossing checkable

A computation has the type `⟨i, o, r⟩ A` over an input mode, an output mode,
and a result mode ([[design/types|types]]). Four spec shapes classify the
builtins:

- `⟨∅, ∅, ∅⟩` — a pure value builtin; it touches no byte channel.
- `⟨Bytes, Bytes, Bytes⟩` — an external command or a byte filter.
- `⟨Bytes, ∅, ∅⟩` — a decoder: it consumes the byte channel and returns a value.
- `⟨∅, Bytes, Bytes⟩` — an encoder: it consumes a value argument and writes the byte channel.

`PipeSpec::none` and `PipeSpec::decode` in `core/src/mode.rs` build the two
value-payload shapes; `ret_bytes` in `core/src/typecheck/builtins.rs` builds
the two byte-payload shapes for builtins, and `external_exec_comp_ty` in
`core/src/typecheck/infer.rs` builds the external-command shape. The
[[design/pipelines|pipe]] connects a producer's `result` to the next stage's
`input`, and every interior connection is `Bytes`/`Bytes`. The modes therefore
admit `cmd | from-json` and `to-json $x | cmd` (`Bytes` meets `Bytes`), and they
reject a value where bytes are due. A program crosses the boundary only when it
names a codec. The decode is typed at the final stage:
`from-json` yields a fresh value type for the pipeline's result. A misspelled
codec fails at command lookup
([[design/builtins|why each codec is its own builtin]]).

## The two directions

A decoder takes no value argument. It reads the byte channel, whether that
channel is a `< file` redirect or the left stage of a pipeline. Each decoder
is declared with arity 0, so a passed value is a type error, raised before the
call runs (`` `from-json` takes no argument — it reads the byte channel ``).
The error's hint names the fix: apply the matching encoder, then send its bytes
through the pipeline so the decoder has a channel to read —
`to-string $s | from-json` for JSON in a `String`, `to-bytes $b | from-string`
for a `Bytes` value (for example `$r[stdout]` from `await`). The decoders:

- `from-bytes` → `Bytes`; the bytes pass through with no decode;
- `from-string` → `String`, **strict** UTF-8;
- `from-line` → `String`, strict, with one trailing `\n` / `\r\n` stripped;
- `from-json` → a decoded value: strict UTF-8, then JSON;
- `from-csv` → a list of records keyed by the header row; every field is a
  `String`, because CSV is untyped — coerce with `int` / `float`; the reader
  handles quoted fields, embedded commas, and embedded newlines;
- `from-lines` → a line stream (below).

**A decoder is a legal pipeline tail.** `cat data.json | from-json` returns a
decoded value. A later stage cannot consume that value through `|`; bind it and
use ordinary application instead, for example
`let document = cat data.json | from-json` followed by `length $document`.

An encoder takes one value and writes its encoded form to the byte channel.
`to-bytes`, `to-string`, `to-lines` (which joins a list with newlines),
`to-json`, `to-csv`, and `to-line` (the line writer that `echo` uses) all
return `Unit`; the written bytes are the result (`write_encoded` in
`core/src/builtins/codecs.rs`). In a pipeline, the write feeds the wire:
`to-json $x | cmd` gives `cmd` the encoded bytes. At a bind, the
[[design/types|capture]] coercion applies: `let e = to-json $x` binds the
encoded text as a `String`.

`to-csv` takes a list of records and writes a header row plus one row for each
record. The columns are the first record's keys in sorted order, because maps
are key-ordered and hold no original column order. A record that misses a
column contributes an empty field.

`to-json` maps a ral value to JSON structurally. A record or a `[String:A]`
map becomes an object; a list becomes an array; `Unit` becomes `null`. A
variant `` `tag payload `` becomes `{"tag": "tag", "payload": …}`, and the
`payload` key is absent for a niladic tag. A `Bytes` value serialises as an
array of byte integers. A `Lambda`, a `Block`, or a `Handle` has no JSON image
and is an error.

## Whole-buffer vs. streaming

The structured decoders (`from-string`, `from-line`, `from-json`) read all of
stdin and then decode: they are whole-buffer. `from-lines` is also
whole-buffer, despite its stream-shaped result. It yields a `Step` stream — a
`more [head, tail]` / `done` open variant whose tail is a thunk
([[invariants/optionality-via-variants|open variants]]) — but it builds the
whole chain eagerly, from the stdin read to EOF. The interface is incremental;
the memory profile matches the other decoders. One codec streams, and it is
the way to process unbounded input without holding it:

- `fold-lines <fn> <init>` folds over stdin line by line. Its output mode is
  parametric:
  `fold-lines : ∀ α μ ρ. U (α → String → ⟨∅, μ, ρ⟩ α) → α → ⟨Bytes, μ, ∅⟩ α`.
  A pure fold keeps `μ = ∅` and has the decoder shape. A callback that writes
  bytes per line lifts the stage to `⟨Bytes, Bytes, ∅⟩ α`, and the fold's
  result stays the reduced value. The inferencer takes `μ` from the callback;
  no declaration is needed. The callback slot carries the quantified
  result-mode variable `ρ`, as [[design/types|types]] describes for signature
  slots (`reducer_spec` in `core/src/typecheck/builtins.rs`).

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
