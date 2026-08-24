---
status: active
---

# Diagnostics are a builtin

**`1>&2` leaves the surface. A message to the human is not standard output
pointed somewhere else; it is its own act, and `warn` is the name for it.**

## Decision

- `warn : String → F[Value] Unit` writes the string and one newline to the
  shell's stderr sink and returns `()`. It is an ordinary table entry in
  `builtin_registry!`, bodied in `core/src/builtins/misc.rs` beside `surface`,
  whose plain-text sibling it is.
- The route is `Value`, not `Bytes`. A diagnostic does not join the byte
  channel, so a caller binding the computation's payload never picks the
  message up — the property `1>&2` could not have, since it worked by making
  the two streams one.
- `1>&2`, and its short spelling `>&2`, are refused in the lexer
  (`scan_redirect_gt`), which sees both descriptors and knows a bare `>` means
  fd 1. The message names `warn`, and names `2>&1` as well, because a program
  holding the exchange backwards means that one.
- `2> f` and `2>&1` stay. An external command's stderr is not ral's to
  re-author: it genuinely needs binding and filing.
- The fd-target admission in `install_sink_redirects`
  ([[map/core/evaluator|core evaluator]]) narrows with the surface, keeping the
  2→1 direction and the two identity dups. `classify_redirects`, the external
  path, already modelled `2>&1` alone.

## Why the redirect had to go

`1>&2` has exactly one honest use, and it is a workaround: bash gives a script
no way to say "this line is for the human", so a script says "standard output,
but over there". Everything about the spelling is machinery for that one
sentence — two descriptor numbers, an ampersand, an order the reader must
decode — and the sentence itself never appears.

It also lies about what happens. `1>&2` does not mark a line as diagnostic; it
moves the *payload* stream, so every byte the command was going to hand its
caller goes with it. Inside a capture that is the whole binding; inside a
pipeline, the whole wire. The idiom is only safe where nothing is listening,
which is a fact about the call site, not about the redirect.

The golden rule applies straightforwardly: fifty years of shells have taught
people to write two-descriptor arithmetic where one verb would do.

## What it buys the model

The Agda kernel's stderr-redirect parcel, revised the same day, is smaller by
exactly this redirect:

- no `cross!` frame and no `crosses` constructor of `AnswerErr` — the
  fd-1-onto-fd-2 answer had no other client;
- the two routing walks stay **one-directional**. `answer-err`'s surviving
  `rejoins` sends the merged word on through `route` on the *tail*, so `route`
  never calls back into the err walk. With `crosses` present the two would have
  had to be mutually recursive, and every proof over them a mutual induction.

The retirement lands before that parcel, so the measured corpus and the
calculus agree on what exists rather than the model carrying a former the shell
has stopped accepting.

## Consequences

- The corpus is swept: every `echo … 1>&2` in `examples/`, `tests/`,
  `scripts/`, `exarch/data/` and `exarch/examples/` is a `warn`. The `.sh`
  counterpart beside each example keeps `>&2` — it is bash, shown for
  comparison, and bash has no `warn`.
- `warn` takes *one* `String`, where `echo` took an argv, so a multi-argument
  diagnostic becomes one interpolated string. That is the same trade `echo`
  versus `to-line` already makes.
- Reachability, not defence: with the lexer refusing, no `EvalRedirectV`
  carrying fd 1 → fd 2 can be built from source, so the narrowed admission in
  `install_sink_redirects` closes a case the surface no longer opens.
- `docs/SPEC.md` §7.4 drops the table row and says why; §14.6 states `warn`'s
  meaning beside `surface` and `ask`.

See [[design/builtins|builtins]], [[internals/builtins-registry|builtins-registry]],
[[design/capture|capture]],
[[internals/output-capture-and-detachment|output-capture-and-detachment]],
[[internals/surface-syntax|surface-syntax]] and
[[map/core/evaluator|core evaluator]].
