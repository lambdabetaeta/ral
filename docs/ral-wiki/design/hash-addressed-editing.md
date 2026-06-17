# exarch: context-hash line editing

**To mutate a file, an agent names the lines it read by their content and the
content around them.** The address of a line is its *window-hash* — a digest of
the line together with its ±3 neighbours; an edit is a batch of
`(hash, replacement)` pairs, each naming one line and the text that replaces it.

The witness has two parts ([[map/exarch/builtins|builtins]] is the *where*):

- *Window-hash* identifies the line by its content and its neighbourhood, and
  checks that the live file still contains both, modulo trailing whitespace.
- *Uniqueness* is the address. The hash must resolve to exactly one line; the edit
  is defined only when that line-in-context occurs once.

**`window-hash` folds context into the witness.** For line `i` it is
`line_hash` of the concatenated `line_hash`es of the lines in `[i-3, i+3]`,
prefixed by the target's offset within that window. `line_hash` itself is the
letter `h` followed by six hex characters (24 bits) of a Blake3 digest of one
line with trailing whitespace stripped. `view`, `view-around`, and `grep-files`
emit the window-hash beside the 1-indexed line number, so a read result already
carries the argument shape `edit` expects.

- *Context distinguishes repeats.* Two lines with identical text but different
  neighbours — a blank line, a bare brace, a repeated header — get different
  witnesses, so each is addressable without a line number.
- *The offset distinguishes short files.* When a file is short enough that the
  window clamps to the whole of it for several lines, those lines share the same
  content; the target's offset within the window keeps their witnesses distinct.

## Properties

**The witness lexes as a string.** A bare `edit` argument that parses as an
integer elaborates to `Val::Int`, not `Val::String`, before the type checker
runs; an all-digit digest at the hash position would then fail its `equal`
against the recomputed string hash. `line_hash` — and therefore `window-hash`,
which ends in a `line_hash` — always begins with `h`, keeping the token un-numeric
at any call site, so the check compares like with like
([[decisions/260608_witness-hash-h-prefix|witness-hash-h-prefix]]).

**The check is stateless.** The digest is recomputed from the file's live content
at the moment of edit.

- **Fresh content passes.** If exactly one line's window hashes to the witness,
  the splice proceeds.
- **A moved or changed line fails.** If no line matches, the error asks the agent
  to re-read before editing. A witness goes stale only when the line *or its ±3
  neighbourhood* changed — bounded locality, not the global invalidation a line
  number would suffer on every insertion.

**Only a repeated neighbourhood is ambiguous.** A line collides with another only
when its whole window — content and context and offset — repeats, as deep inside
a run of identical lines.

- **One match is the contract.** A line distinguished by its surroundings edits
  cleanly, even if its own text recurs elsewhere.
- **Many matches are ambiguous.** The edit fails and names the matching line
  numbers rather than guess. A line buried in a long identical run is genuinely
  unaddressable by content; that is the residual the witness cannot express.

**One primitive covers the edit surface.** The helper is `edit path edits`, where
`edits` is a list of `[hash, new-text]` pairs.

- **Replacement** names a line by its window-hash and gives its new text.
- **Insertion** rides on `new-text` carrying newlines: one line becomes several.
- **Deletion** is replacement with the empty string.
- **The batch is the unit, and it is atomic.** Every hash resolves against one
  read of the file *before* anything is written, then all the named lines are
  spliced in a single pass. Because resolution precedes mutation, no edit can
  invalidate another's witness — adjacent lines included — so a region is read
  once and rewritten without re-reading. The batch writes nothing unless every
  hash names exactly one line and no two pairs name the same one. A single edit
  is a batch of one.
- **Patch reporting** carries the literal removed and added rows through
  `surface`, one hunk per edit, so the rail renders text, not opaque handles.

**The hash size is a stale-edit check, not an authority boundary.** Twenty-four
bits are enough for an interactive line witness: a collision degrades to the
check accepting two equal digests for distinct contexts, not to ambient write
authority. The actual authority remains ral's [[design/grant|grant]] frame and
filesystem checks.

**The witness layer is ral over small atoms.** Rust supplies only the
irreducible pieces — `line-hash` (one Blake3 digest), `_search-files` (the
ignore-aware ripgrep walk, returning `[file, line, text]` with no witness;
`_`-prefixed so `help` hides it), and `explore-dir`. `window-hash` composes
`line-hash` over a slice; `view` and `edit` compute it directly; `grep-files`
reads each matched file once and stamps `_search-files`'s hits with it. The same `window-hash` is shared by all three, so
a read and an edit always agree. This keeps exarch's
[[design/exarch-architecture|thin architecture]]: editing is source ral over a
few host atoms and ordinary redirects, not a separate edit protocol.

## Reference

Can Bölük's
[“The Harness Problem”](https://blog.can.ac/2026/02/12/the-harness-problem/)
is the direct reference point for this shape. It argues that the edit harness is
a major part of coding-agent performance, and presents *hashline*: read and grep
results tag each line with a line number plus short content hash; edits name
single lines, ranges, or insertion points by those tags; stale hashes reject the
edit before corruption.

Exarch uses the same witnessed-anchor idea, with a narrower surface:

- **One verb, a batch of lines.** `edit path edits` applies a list of
  `(hash, new-text)` pairs in one read/write pass; multi-line change is a batch of
  single-line replacements resolved against one snapshot, rather than a range
  primitive.
- **Context is the address.** The line number is shown for the agent's reading,
  but the edit selects by a hash that folds in ±3 lines of context, so the witness
  distinguishes repeated lines without carrying a position the file can
  invalidate.
- **Ral-level composition.** The witness layer lives in `agent.ral`; Rust supplies
  only `line-hash`, `_search-files`, and `explore-dir`.
- **Rail patches.** The helper emits literal removed and added rows through
  `surface`, so the user sees a diff-shaped event rather than an opaque anchor
  operation.
- **String-tagged hash.** Exarch's hash is the letter `h` followed by six hex
  characters — larger than the shortest possible display handle, with a collision
  budget aimed at ordinary source files, and un-lexable as a number so a bare
  witness round-trips as a string through ral's value model.

See also [[map/exarch/builtins|builtins]], [[map/exarch/shell-eval|shell-eval]].
