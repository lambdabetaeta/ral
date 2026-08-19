# exarch: context-hash line editing

**To mutate a file, an agent names the lines it read by their content and the
content around them.** The address of a line is its *witness* — a digest of the
line together with the smallest window of neighbours that makes it unique in the
file. An edit is a batch of `[hash: …, line: …]` records, each naming one line
and the text that replaces it.

The witness has two jobs ([[map/exarch/builtins|builtins]] is the *where*):

- *Content + context* identifies the line by what it is and what surrounds it,
  and checks that the live file still contains both, modulo trailing whitespace.
- *Uniqueness* is the address. The witness resolves to exactly one line, by
  construction — the window grows until it does — so the model never meets an
  ambiguous hash.

## The witness: adaptive context

**`window_hashes` addresses each line by an *adaptive-context witness* — the
smallest context that makes it unique.** For line `i` the witness is the `line_hash` of the neighbours in a
symmetric window, prefixed by that window's radius and the line's offset within
it (clamped at the file's ends). `line_hash` is the letter `h` followed by six
hex characters of a Blake3 digest of one line with trailing whitespace stripped.

The window is **adaptive**, not fixed:

- *It starts at a freshness floor.* Every witness folds in at least ±5 lines of
  context, even a line unique on its own, so an edit anywhere within that window
  invalidates the witness and forces a re-read. The floor is the dial for "how
  much surrounding drift should a witness notice."
- *It grows only where a line repeats.* A line distinctive within ±5 resolves at
  the floor; a line whose ±5 window also occurs elsewhere grows its window — ±6,
  ±7, … — until it reaches whatever tells it apart: the function header above it,
  the differing call below it. The window grows exactly as far as the ambiguity
  demands and no further, so a distinctive line stays locally addressed and only
  a genuinely repeated region pays for wider context.
- *Its one floor is the absolute index.* A verbatim run longer than twice the cap
  (`MAX_RADIUS = 64`) cannot be separated by any window; its interior lines fold
  in their absolute index instead — the honest positional floor, reached only in
  that degenerate case and never for real code.

**No line number rides in the witness.** Because uniqueness comes from *context*,
not position, a witness goes stale only on a *local* change — the line or its
window — not on every insertion elsewhere in the file, which a line number would
suffer. The index floor above is the single, deliberate exception.

**It is computed by partition refinement**, the shape of DFA minimisation. Group
the lines by their ±5 window; then repeatedly split only the still-colliding
classes by one more line of context on each side. A line that becomes a singleton
is resolved at that radius and never re-examined, so the total work is bounded by
the size of the collision classes across radii — linear on real files, where
almost every line is unique at the floor. The lines are hashed once into an array
and reused, so a wider window costs concatenation, not re-hashing.

### Influences

The *adaptive-context witness* is exarch's own term, not a published technique;
it is a synthesis of ordinary, well-understood parts, and the application —
context grown to uniqueness as an anchor for LLM code edits — is not one we can
point to prior art for. The parts it stands on:

- **Shortest unique identifier.** Git shows the shortest commit-hash *prefix*
  still unique in a repository — "grow the identifier only until it
  disambiguates," the same instinct applied to a hash prefix rather than a line's
  context. The stringology of **shortest / minimal unique substrings** (SUS
  queries) studies the same shape directly.
- **Uniqueness as an anchor.** **Patience diff** aligns two files on the lines
  that occur *exactly once* — uniqueness-as-addressability for source lines, the
  property the growing window manufactures when a line is not unique on its own.
- **Partition refinement.** Computing each line's resolving radius by repeatedly
  splitting only the still-colliding classes is the shape of **Hopcroft's
  DFA-minimisation algorithm** and of radix/trie construction.

The two anchor harnesses below ([hashline](#comparison-anchor-based-edit-harnesses),
Dirac) are the direct lineage for the *witnessed-anchor* idea; the adaptive
context is what exarch adds to it.

## The surface: three tools, one witness, one door

`view-text`, `view-text-around`, and `edit-hash` are the whole editing
surface. All three are **path-based**: each takes a path and reads the *whole*
file in Rust, through one grant-checked read door, and computes witnesses with
the same `window_hashes`. Nothing in the surface takes a line list or a stream.

- `view-text PATH START END` — show `[START, END)` as one record per line,
  `[line: Int, hash: Str, text: Str]`. Records rather than tagged text, so one
  read serves both eyes and script: the rows a sweep means to change go straight
  into `edit-hash`, which is why no programmatic twin stands beside it.
- `view-text-around PATH LINE PEEK` — the `2*PEEK + 1` lines centred on `LINE`,
  in the same records. A thin ral helper over `view-text`.
- `edit-hash PATH EDITS` — apply the batch.

This shape is forced, and it is what makes the witness trustworthy:

- **A witness is only valid for the bytes it was computed over.** Because every
  tool reads the same whole on-disk file through the same door, the hash a read
  shows is *always* a valid handle for the `edit-hash` that follows. A rows-taking or
  stdin-taking witness function would let a caller hash a *slice* (or a different
  version, via a pipe) and get witnesses that look like handles but resolve
  against nothing — a quiet, confusing footgun. Path-based reads close it: there
  is no slice to get wrong.
- **Each tool is one logical surface.** The reads sink below the ral line;
  `view-text` and `view-text-around` raise exactly one `read` card, and
  `edit-hash` raises exactly one diff card — never a separate read or write card. This is why `edit-hash`
  is a path builtin and not a `< file > file` stream filter: a filter's read and
  write would each ride the redirect frame and surface their own card, fracturing
  one conceptual edit into three rail surfaces, and `edit-hash` would lose the path it
  needs to label its diff.
- **The diff is taken at the edit, not at the card.** Having read the original
  and built the final text, the builtin holds both sides, so it diffs them there
  and surfaces the hunks. Only the hunks then cross the surface, the wire, and
  the record — never the two whole files, whose size therefore bounds nothing.
  This is what separates an edit from a committed `>`, which never holds both
  sides: that path surfaces snapshots and is diffed at the card, and so reads as
  a diff only for a file small enough to have been snapshotted
  ([[map/exarch/io-surface|io-surface]]).

## Properties

**The witness lexes as a string.** A bare `edit-hash` argument that parses as an
integer elaborates to `Val::Int`, not `Val::String`, before the type checker
runs; an all-digit digest at the hash position would then fail its `equal`
against the recomputed string hash. `line_hash` — and therefore every witness,
which ends in a `line_hash` — always begins with `h`, keeping the token
un-numeric at any call site ([[decisions/260608_witness-hash-h-prefix|witness-hash-h-prefix]]).

**The check is stateless.** Every witness is recomputed from the file's live
content at the moment of edit; there is no map of anchors to keep in sync.

- **Fresh content passes.** If exactly one line's witness equals the one given,
  the splice proceeds.
- **A moved or changed line fails.** If none matches, the error asks the agent to
  re-read before editing. A witness goes stale only when the line *or its window*
  changed — bounded locality, not the global invalidation a line number would
  suffer on every insertion.

**Uniqueness is by construction, so the model never sees an ambiguous hash.** The
window grows until each line's witness is unique, so a `view-text`
read hands back handles that each name exactly one line. The only residual is a
24-bit accidental Blake3 collision (a stale-edit check, not an authority
boundary — the actual authority is ral's [[design/grant|grant]] frame) or the
deep interior of a verbatim run, which the index floor still makes unique.

**Editing is one atomic, named pass.** `edit-hash PATH EDITS` takes `EDITS` as a list
of `[hash: HASH, line: NEWTEXT]` records.

- **Replacement** names a line by its witness and gives its new text.
- **Deletion** is replacement with the empty string.
- **Splitting** rides on `NEWTEXT` carrying real newlines: one line becomes
  several. (`dedent` over a raw block is the clean way to write an indented
  multi-line replacement.)
- **The batch is the unit, and it is atomic.** Every hash resolves against one
  read of the file *before* anything is written, then all the named lines are
  spliced in a single pass and committed by an atomic rename. Because resolution
  precedes mutation, no edit can invalidate another's witness — adjacent lines
  included — so a region is read once and rewritten without re-reading. The batch
  writes nothing unless every hash names exactly one line and no two records name
  the same one.
- **Patch reporting** carries the literal removed and added rows through
  `surface`, one hunk per change, so the rail renders text, not opaque handles.

**A grep hit is a location, not an edit handle.** `grep-files` returns
`[{file, line, text}]` with no witness: search finds *where*, and turning a
location into an edit handle is a deliberate second step — `view-text-around` over the hit to read its witness by eye.

**The witness layer is ral over small atoms.** `line_hash` and `window_hashes`
are private Rust — the witness is never something the model constructs, only one
it copies out of a read — and `_search-files` (the ignore-aware ripgrep walk) is
`_`-prefixed so `help` hides it. `view-text`, `grep-files`, and
`edit-hash` are the host builtins; `view-text-around` is the one thin ral helper. This
keeps exarch's [[design/exarch-architecture|thin architecture]]: editing is a few
host atoms reading whole files below the ral line, not a separate edit protocol.

## Comparison: anchor-based edit harnesses

**Two published coding-agent harnesses reach the same conclusion — name the line
you read, do not reproduce it — and exarch sits past both.** Both replace the
reproduced search block of a `str_replace` tool with a short *anchor*, cutting an
edit's output from `O(search + replacement)` to `O(replacement)`; the divergence
is in the anchor's identity, and in how it disambiguates repeats.

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
- *Disambiguates repeats by the line number* — the hash is only a staleness
  check.

**Dirac — [“Hash Anchors, Myers Diff, single-token edits”](https://dirac.run/posts/hash-anchors-myers-diff-single-token)**
pushes the idea to a stateful extreme, optimising the anchor for tokens rather
than for content-derivation.

- *Diagnosis.* Output tokens cost 5–6× input, so regenerating a whole block to
  edit lines 101–150 is `O(search + replacement)` of wasted output.
- *Proposal — single-token anchors over a state machine.* Despite the title, the
  anchors are **not** content hashes: ~1,700 pre-generated single-*token* words
  ("Moderator", "Qualifier"), assigned per-file per-session, separated from code
  by a `§` delimiter. Five components carry it — an anchor pool, the delimiter, a
  validator, a state manager (line→anchor, no reuse), and a reconciler that runs
  **Myers diff** after each edit to reassign changed lines' anchors. An edit gives
  `{start_anchor, end_anchor, replacement}` → `O(replacement)`. ~60% cheaper.
- *Disambiguates repeats by construction* — anchors are never reused — at the cost
  of state that can desync from the file.

All three share the win and the safety property: the model emits only the new
text plus an anchor, and an anchor that no longer matches the live file rejects
the edit rather than corrupting it. Where they diverge is the anchor's identity.

| | anchor | derivation | disambiguates repeats by | state |
|---|---|---|---|---|
| **hashline** | `line#:hash` | content hash | the line number | none |
| **Dirac** | single-token word | pre-generated, tracked | construction (no reuse) | five-component machine |
| **exarch** | `h` + 6 hex | adaptive-context window | growing the window | none |

**Better than hashline on identity.** Hashline leans on the *line number* for
uniqueness, and a line number is invalidated by any insertion above it. Exarch
carries no line number into the anchor: the context fold *is* the disambiguator,
grown only as far as a repeat demands, so the witness goes stale on a local
change rather than on every edit elsewhere.

**Dirac's uniqueness, without Dirac's machine.** Both exarch and Dirac guarantee
a unique anchor so the model never faces an ambiguous edit — the failure mode
hashline still has, where a line buried in a repeated run is unaddressable. Dirac
buys that guarantee with a stateful reconciler (anchor pool, state manager, Myers
diff) that survives across edits but can desync from the file. Exarch buys it
*statelessly*: the witness is a pure function of file content, the window grown to
uniqueness by the same `window_hashes` on the read and the edit side — no pool, no
reconciler, no state to drift. The price is a wider anchor (`h`+6 hex ≈ 3–4 input
tokens vs. Dirac's single token) and no cross-edit stability — editing a line
shifts its neighbours' witnesses — which exarch absorbs *within* a batch, where
every hash resolves against one snapshot before any write, so a multi-line change
needs no re-read; only *separate* `edit-hash` calls require a fresh read.

Net: exarch takes hashline's witnessed-anchor idea, drops the brittle line number
for a context fold that *grows to uniqueness*, and reaches Dirac's
ambiguity-free, corruption-safe editing without Dirac's stateful machine — an
anchor that is a pure function of content, an atomic batch that blunts the re-read
cost, and no background state to drift out of sync with the file.

See also [[map/exarch/builtins|builtins]], [[map/exarch/shell-eval|shell-eval]].
