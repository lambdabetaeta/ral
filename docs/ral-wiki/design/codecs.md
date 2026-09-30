# Codecs: the typed crossing between bytes and values

**The `from-X` / `to-X` builtins name the typed crossing between stdin bytes
and structured values.** A decoder reads stdin and returns a value. An encoder
takes one value and writes the encoded bytes:

- every decoder has the computation type `from-X : Returns A`;
- every encoder has the type `to-X : A → Command`.

The two types are inverse: an encoder writes what the matching decoder reads.

## The crossing is a name

A command is a computation of type `F^w Unit` (`Command`): its value is its
output. An encoder is a command with an argument, a builtin row ending in
`Command`; an external command is `[String] → Command` too. A decoder is a row
that returns a value and writes nothing, so it ends the byte pipeline. A bind
sees what a command wrote because the command's type says so:
`let x = cat f` binds captured text
([[decisions/260930_graded-f|graded-f]], [[design/capture|capture]]), while
`let x = cat f | from-bytes` binds the bytes exactly, because the decoder is
where the pipeline's value comes from. A
program crosses between bytes and values only when it names a codec. A
misspelled codec fails at command lookup
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
- `from-json` → a decoded value: strict UTF-8, then JSON; a *boundary*
  ([[decisions/260930_a-boundary-is-checked-against-its-type|checked at the
  door]]);
- `from-jsonl` → a list, one `from-json` value per non-blank line split by the
  line rule, also a boundary; each line parses alone, so a failure names the
  input's line;
- `from-json-at tokens` → the value at a path of RFC 6901 reference tokens (an
  object member by exact key, an array element by decimal), decoded and
  checked like `from-json`; a path given as tokens, never as a pointer string,
  so no key needs escaping;
- `from-csv` → a list of maps keyed by the header row, `[Map String]` and so not
  a boundary: its shape is decided; every field is a
  `String`, because CSV is untyped — coerce with `int` / `float`; the reader
  handles quoted fields, embedded commas, and embedded newlines;
- `from-lines` → `[String]`, split by the line rule (below), lossy per line.

**A decoder of undecided shape is checked.** `from-json`'s type is `∀α. Returns α`,
and its `α` would be a cast. It is instead weak (one type per unit) and the
decoder admits the value it read against the type the checker solved for it, so
what fails is the decode, naming the pointer and the line of the use that
needed the shape: `the value at /items/3/size is text, but this script uses it
as a number`. The decoder never converts: a JSON `3` is an `Int` and `2.5` a
`Float`, and `float` at the use accepts both.

**A decoder is the pipeline's last stage.** `cat data.json | from-json` returns
a decoded value, the pipeline's value being its final stage's. A decoder before
the last stage is refused: a stage feeds the next by writing, and a decoder
returns ([[design/pipelines|pipelines]]). Bind the decoder's result and apply
the next function to it instead — `let document = cat data.json | from-json`
followed by `length $document`.

An encoder takes one value and writes its encoded form to stdout.
`to-bytes` (a `Bytes` value, passed through unchanged), `ints-to-bytes` (a list
of `Int`, each 0 through 255 — ral has no byte literal, so this is how bytes are
written by number), `to-string`, `to-lines` (each element followed by `\n`),
`to-json`, `to-jsonl` (each element as `to-json` writes it, then `\n`),
`to-csv`, and `to-line` (the line writer that `echo` uses) all
write the encoded bytes (`write_encoded` in
`core/src/builtins/codecs.rs`). Each encoder names one operand type, so
`to-bytes 3` and `to-bytes hello` are ordinary unification failures rather than
a union the checker has to resolve; the operand-prefixed name is what
distinguishes the second writer, as in `bytes-to-string`. In a pipeline, the write feeds the wire:
`to-json $x | cmd` gives `cmd` the encoded bytes. At a bind, the
[[design/capture|capture]] coercion applies, the encoder being a command:
`let e = to-json $x` binds the encoded text as a `String`, and
`let e = to-json $x > f` binds `()`, the redirect having discharged the output.

`to-csv` takes a list of records and writes a header row plus one row for each
record. The columns are the first record's keys in sorted order, because maps
are key-ordered and hold no original column order. A record that misses a
column contributes an empty field.

`to-json` maps a ral value to JSON structurally. A record or a `Map A`
becomes an object; a list becomes an array; `Unit` becomes `null`. A
variant `` `tag payload `` becomes `{"tag": "tag", "payload": …}`, and the
`payload` key is absent for a niladic tag. A `Bytes` value serialises as an
array of byte integers. A `Lambda`, a `Block`, or a `Handle` has no JSON image
and is an error; `to-jsonl` encodes every element before it writes, so its
refusal names the element's index and leaves the channel empty.

## Decoders return values; a fold streams

**A decoder reads to EOF and returns a value, and a value holds no reader.**
Every decoder is whole-buffer, `from-lines` included. A lazy sequence of lines
would be a value still owing reads: once it outlived its capture it would have
to hold a live reader on a producer still running, past the boundary that ends
that producer. So the one codec that streams is a fold, whose callback runs
while the pipe is open, and it is the way to process unbounded input without
holding it:

- `fold-lines <fn> <init>` folds over stdin line by line:
  `fold-lines : ∀ε α. U (α → String → F^ε α) → α → F^p α`
  (`scheme::fold_lines` in `core/src/typecheck/builtins.rs`).
  It returns its accumulator; a callback of either grade is absorbed — its
  output streams, its value is kept. A fold is not a decoder, so it may end a
  pipeline, and precede the last stage only with a `()` accumulator.

## One line rule

**Every reader of lines agrees on where a line ends, because there is one
reader.** The rule, with its table in `docs/SPEC.md` §7.3:

- a *terminator* is `\n` or `\r\n`, and exactly one `\r` belongs to it:
  `a\r\r\n` is the line `a\r`;
- a lone `\r` is text, never a terminator, mid-line or at EOF;
- the final line need not be terminated;
- each line strips its own terminator, so mixed endings are fine.

One function measures a terminator (0, 1, or 2 bytes), and three kinds of
caller share it:

- *line readers* — `from-lines`, `from-jsonl`, `fold-lines` and the prelude
  filters over it, `line-count`, and `lines` — all split through one reader
  generic over its byte source (`core/src/builtins/util.rs`): stdin for the
  decoders, the string's bytes for `lines`, which so agrees by construction
  rather than by test;
- *one-terminator strippers* — capture, `from-line`, `ask` — remove at most one
  terminator from the end: `a\r\n\r\n` becomes `a\r\n`;
- *writers* — `to-line`, `to-lines`, `to-jsonl`, `echo` — emit `\n` only.
  Reading accepts both endings the world writes; ral's own output has one
  spelling.

`to-lines` terminates every element, as `to-line` and `to-jsonl` do, so
`from-lines ∘ to-lines = id` on every list whose elements contain no `\n` and
do not end in `\r` — such an `\r` would fuse with the written `\n` into one
terminator.

## Strict values, lossy lines

The structured decoders and an [[design/capture|external command captured by
`let`]] are **strict**: invalid UTF-8 is an error that points at `from-bytes`,
because a `String` or JSON value you will compute with must not silently carry a
replacement character. `from-lines` alone is **lossy, hence total**: scanning
lines tolerates a `�`, while constructing a scalar `String` does not.

See also [[design/builtins|builtins]], [[design/pipelines|pipelines]],
[[design/types|types]], [[design/cbpv|cbpv]]; [[map/core/builtins|map: builtins]],
[[map/core/io-process|io-process]].
Cite: RATIONALE §"Values and commands", §"Pipelines follow their edges";
`docs/SPEC.md` §7, §14, §17.5.
