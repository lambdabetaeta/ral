---
status: superseded
superseded_by: decisions/260809_pipes-are-positional-byte-wires
supersedes: decisions/260609_pure-pipe-equation, decisions/260610_value-edge-locality
---

# Byte-only pipeline interiors

## Decision

The surface pipe `|` is a byte conduit. For every interior edge, the
producer's payload and the next stage's input are `Bytes`. The final stage is
different: its result is the pipeline's result and may be any ordinary ral
value. A stage's `output` remains independent chatter; it is not the payload
that the next stage consumes.

This is the model described by [[design/pipelines|pipelines]],
[[design/types|types]], and `docs/SPEC.md` §7 and §17. The checker pins each
interior producer result and consumer input to `Bytes`, records the ground
`(input, output, result)` wire for every stage, and leaves the final result
mode free. The byte-result invariant remains: a computation whose payload
conduit is `Bytes` returns `Unit` as its value (`WF-2`). There is no second
value-pipe rule and no implicit codec.

Value-style composition uses ordinary application and bind. An expression such
as `x | f` is rejected as an attempted value edge; the diagnostic should teach
`f x`, a `let` binding, or an explicit encoder followed by a byte pipeline.
This keeps the one surface pipe aligned with the operating-system boundary and
keeps value composition in the ordinary CBPV evaluator.

## Decoder tails

A decoder is a legal final stage because it consumes bytes and returns the
pipeline's final value:

```ral
let document = cat data.json | from-json
length $document
```

The same decoder cannot be followed by another `|` stage: that would put a
value on an interior edge. Bind the decoded value and apply the next function,
or encode it explicitly with `to-json`, `to-string`, or another matching
encoder. The default implicit capture seam remains byte-to-`String` with
strict UTF-8 and one trailing line terminator removed; named `from-*` decoders
remain the explicit path for structured or binary results. See
[[design/codecs|codecs]].

### A ral tail must be a computation, not a block literal

A block *literal* in stage position is a value, not a computation, so it
advertises no input channel and can never be a byte consumer — `cat f |
{ from-line }` is rejected. A ral-written tail therefore has to reach stage
position as a computation: bind it and force it there.

```ral
let reader = { from-line }
cat data.txt | !$reader
```

Builtin decoders have no such restriction, since `from-json` and its siblings
are nullary command forms rather than block literals. This is a limitation of
the surface, not of the typing rule: the rule pins the consumer's `input` to
`Bytes`, and a block literal simply has none to pin. Lifting it — letting a
literal in stage position elaborate to its body computation — is a separate
question, and until it is answered the forced-binding spelling above is the
one that works.

## Execution boundary

Every multi-stage pipeline is process-staged. Interior routes are operating-
system byte pipes, and stages run as one process group. Only the pipe bytes,
the final result, and recorded observations cross a process boundary; lexical
bindings, cwd, environment, aliases, and modules stay in their child.
[[internals/pipeline-execution|Pipeline execution]] resolves ground wires,
launches the stages, and collects them without a parent-side value fold.

The final value result remains helper-staged for now. `FinalValue::Report`
selects a helper request/response path, and the child returns the final value
in `ChildEvalResponse` along with status and observations. Moving the final
tail into the parent is explicitly deferred to a separate decision; this ADR
does not change that boundary. See [[map/core/runtime|runtime]] and
[[map/core/typecheck|typecheck]].

## Formal-model witness

The Agda model no longer presents a second surface value pipe. The retained
composite is named `bind-apply`: it derives bind-then-apply in the value
calculus while preserving the producer's `⟨input, output⟩` grade through
`bind-ty`. Its grade-action proof remains attached in `dev/agda/Core/Derived.agda`;
the rename removes the obsolete surface claim rather than deleting the proof.

The old equations in [[decisions/260609_pure-pipe-equation|pure-pipe-equation]]
and [[decisions/260610_value-edge-locality|value-edge-locality]] are superseded
by this decision. Future work may decide whether the final value tail can be
evaluated in the parent, but it must not reintroduce typed value transport on
an interior `|` edge.
