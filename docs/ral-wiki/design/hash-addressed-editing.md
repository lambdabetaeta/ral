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
line with trailing whitespace stripped. `view-text`, `view-text-around`, and `grep-files`
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
`line-hash` over a slice; `view-text` and `edit` compute it directly; `grep-files`
reads each matched file once and stamps `_search-files`'s hits with it. The same `window-hash` is shared by all three, so
a read and an edit always agree. This keeps exarch's
[[design/exarch-architecture|thin architecture]]: editing is source ral over a
few host atoms and ordinary redirects, not a separate edit protocol.

## Comparison: anchor-based edit harnesses

**Two published coding-agent harnesses reach the same conclusion — name the line
you read, do not reproduce it — and exarch sits between them.** Both replace the
reproduced search block of a `str_replace` tool with a short *anchor*, cutting an
edit's output from `O(search + replacement)` to `O(replacement)`; the divergence
is in the anchor's identity.

**Can Bölük — [“The Harness Problem”](https://blog.can.ac/2026/02/12/the-harness-problem/)
(2026-02-12)** is the framing reference for this shape. Its thesis: the edit
harness is a dominant *uncontrolled* variable in agent performance, more so than
which model runs.

- *Diagnosis.* `str_replace` (Claude Code, Gemini) demands a whitespace-perfect
  reproduction of the old text and rejects on zero or multiple matches — the
  "String to replace not found" failure. Codex's patch format is tuned to one
  model's token biases and collapses on others (Grok 4 50.7% patch-failure,
  GLM-4.7 46.2%). Cursor papers over it with a fine-tuned 70B merge model —
  conceding the harness, not solving it.
- *Proposal — hashline.* Read and grep results tag each line `line#:hash|text`
  with a 2–3 character content hash. Edits name single lines, ranges, or insertion
  points by those tags; a stale hash rejects the edit before corruption. Output
  drops ~20%; the weakest models gain most (Grok Code Fast 1 6.7% → 68.3%).

**Dirac — [“Hash Anchors, Myers Diff, single-token edits”](https://dirac.run/posts/hash-anchors-myers-diff-single-token)**
pushes the same idea to its stateful extreme, optimising the anchor for tokens
rather than for content-derivation.

- *Diagnosis.* Output tokens cost 5–6× input, so regenerating a whole block to
  edit lines 101–150 is `O(search + replacement)` of wasted output.
- *Proposal — single-token anchors over a state machine.* Despite the title, the
  anchors are **not** content hashes: ~1,700 pre-generated single-*token* words
  ("Moderator", "Qualifier"), assigned per-file per-session, separated from code
  by a `§` delimiter. Five components carry it — an anchor pool, the delimiter, a
  validator (full-line string match against stored state), a state manager
  (line→anchor, no reuse, overflow to 2-token anchors), and a reconciler that runs
  **Myers diff** after each edit to reassign changed lines' anchors. An edit gives
  `{start_anchor, end_anchor, replacement}` → `O(replacement)`. Reported ~60%
  cheaper, 8/8 tasks.

All three share the win and the safety property: the model emits only the new
text plus an anchor, and an anchor that no longer matches the live file rejects
the edit rather than corrupting it. Where they diverge is the anchor's identity.

| | anchor | derivation | disambiguates repeats by | width |
|---|---|---|---|---|
| **hashline** | `line#:hash` | content hash (stateless) | the line number | 2–3 chars |
| **Dirac** | single-token word | pre-generated, state-tracked | construction (no reuse) | 1 token |
| **exarch** | `h` + 6 hex | ±3-context window-hash (stateless) | folded-in neighbourhood | 7 chars |

**Better than hashline on identity.** Hashline leans on the *line number* for
uniqueness, with the hash a staleness check — and a line number is invalidated by
any insertion above it. Exarch carries no line number into the anchor: the
±3-context fold *is* the disambiguator, and a genuinely repeated neighbourhood is
rejected by name rather than silently guessed. The witness goes stale only on a
*local* change, not on every edit elsewhere in the file.

**A deliberate trade against Dirac — the leaner side of it.** Both hit
`O(replacement)` and both refuse to corrupt on drift; the split is statefulness.

- *Dirac buys anchor stability.* Its anchors survive across edits because a Myers
  reconciler reassigns them after every change, backed by a state manager and
  anchor pool. The model edits repeatedly without re-reading — at the cost of a
  five-component machine that can desync from the file.
- *Exarch stays pure.* The witness is a function of file content alone, computed
  by the same `window-hash` on the read and the edit side — no state manager, no
  reconciler, no pool. The whole surface is one stateless read→resolve→rebuild→
  write pass in source ral over a few host atoms, which keeps the
  [[design/exarch-architecture|thin architecture]] thin.

The price exarch pays for that purity is two-fold: a wider anchor (`h`+6 hex ≈
3–4 input tokens vs. Dirac's single token), and no cross-edit stability — because
the witness is context-sensitive, editing a line shifts its neighbours'
witnesses. Exarch absorbs this *within* a batch, where every hash resolves
against one snapshot before any write, so a multi-line change needs no re-read;
only *separate* `edit` calls require a fresh `view-text`. Dirac's reconciler is
the one mechanism here that avoids even that.

Net: exarch takes hashline's witnessed-anchor idea, drops the brittle line number
for a context fold, and reaches Dirac's output efficiency and corruption-safety
without Dirac's stateful machine — an anchor that is a pure function of content,
an atomic batch that blunts the re-read cost, and no background state to drift out
of sync with the file.

See also [[map/exarch/builtins|builtins]], [[map/exarch/shell-eval|shell-eval]].
