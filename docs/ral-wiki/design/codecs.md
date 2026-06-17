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

`from-X` reads stdin with zero arguments (the `< file` or pipeline case) or
decodes the one value it is given:

- `from-bytes` → `Bytes`, the raw escape hatch (a non-`Bytes` argument is an
  error, not a silent stringify);
- `from-string` → `String`, **strict** UTF-8;
- `from-line` → `String`, strict, one trailing `\n` / `\r\n` stripped;
- `from-json` → a decoded value, strict UTF-8 then JSON;
- `from-lines` → a line stream (below).

`to-X` takes one value, writes its encoded form to the byte channel, and returns
the `Bytes` it wrote: `to-bytes`, `to-string`, `to-line` (trailing newline),
`to-lines` (newline-join a list), `to-json`. All five share the `encode` mode.

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

## Strict in, lossy across the line boundary

The structured decoders are **strict**: invalid UTF-8 is an error that points at
`from-bytes`, because a `String` or JSON value you will compute with must not
silently carry a replacement character. The line path is **lossy, hence total** —
`from-lines` decodes with replacement, matching how an [[design/types|external
command's captured stdout]] decodes at a `let`. The contrast is deliberate:
scanning text tolerates a `�` and wants totality; building a value does not.

See also [[design/builtins|builtins]], [[design/pipelines|pipelines]],
[[design/types|types]], [[design/cbpv|cbpv]]; [[map/core/builtins|map: builtins]],
[[map/core/io-process|io-process]].
Cite: RATIONALE §"External commands return strings",
§"Byte pipelines are processes; value pipelines are folds";
`docs/SPEC.md` §4.2, §16, §20.
