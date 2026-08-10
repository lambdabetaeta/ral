# Codecs: the typed crossing between bytes and values

**The `from-X` / `to-X` builtins name the typed crossing between stdin bytes
and structured values.** A decoder reads stdin and returns a value. An encoder
takes one value and writes the encoded bytes, and those bytes are what a value
boundary sees:

- every decoder has the computation type `from-X : F[Value] A`;
- every encoder has the type `to-X : A → F[Bytes] Unit`.

The encoder's [[design/types|payload route]] is `Bytes`, so by WF-2 its return
type is `Unit`. The two types are inverse: an encoder writes what the matching
decoder reads.

## The route makes the crossing legible

A codec's declaration says which of its two products a value boundary should
observe, and nothing more ([[design/types|types]]). Three shapes classify the
builtins:

- `F[Value] A` — a pure value builtin, and also a decoder: its answer is its
  returned value.
- `F[Bytes] Unit` — an external command, a byte filter, or an encoder: its
  answer is what it wrote.
- `F[ρ] A` with `ρ` forwarded from a supplied thunk — the streaming reducer.

`ret` and `ret_bytes` in `core/src/typecheck/builtins.rs` build the two ground
shapes, and `external_exec_comp_ty` in `core/src/typecheck/infer.rs` gives every
external command the second. Nothing about the [[design/pipelines|pipe]] reads
these: a `|` is a positional byte wire, and asks of a stage only that it be a
computation ready to run, never what its route is. What the
route buys is the *boundary*: `let x = cat f` binds captured text, while
`let x = cat f | from-bytes` binds the bytes exactly, and the ordinary return
type alone cannot tell those apart. A program crosses between bytes and values
only when it names a codec. A misspelled codec fails at command lookup
([[design/builtins|why each codec is its own builtin]]).

## The two directions

A decoder takes no value argument. It reads stdin, whether that comes from a
`< file` redirect, the left stage of a pipeline, or the terminal. Each decoder
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

**A decoder is the natural pipeline tail.** `cat data.json | from-json` returns
a decoded value. Putting a stage *after* a decoder is legal and useless: the
decoded value goes nowhere, and the next stage reads the EOF the decoder left
behind. Bind the decoder's result and apply the next function to it instead —
`let document = cat data.json | from-json` followed by `length $document`.

An encoder takes one value and writes its encoded form to stdout.
`to-bytes`, `to-string`, `to-lines` (which joins a list with newlines),
`to-json`, `to-csv`, and `to-line` (the line writer that `echo` uses) all
return `Unit`; the written bytes are the payload (`write_encoded` in
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

- `fold-lines <fn> <init>` folds over stdin line by line, forwarding its
  callback's boundary behaviour:
  `fold-lines : ∀ α ρ. U (α → String → F[ρ] α) → α → F[ρ] α`.
  A value-returning fold returns its accumulator. A callback that emits per
  line makes the fold byte-routed, so a value boundary captures the emitted
  lines instead — which is what `map-lines` is. `each-line` deliberately
  returns `Unit`, leaving its callback's writes visible. The one route variable
  is the caller's, read off the supplied thunk and handed back paired with the
  value type it came with; the inferencer needs no declaration
  (`scheme::fold_lines` in `core/src/typecheck/builtins.rs`).

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
