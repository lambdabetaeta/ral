---
generated_at_commit: 7ba500b
generated_at_date: 2026-06-17
covers_paths: [exarch/src/agent_builtins.rs, exarch/data/agent.ral]
---

# Map: exarch / builtins

exarch's **resident host atoms and sourced helpers** — the search, line-witness,
and edit surface the model reaches through the `shell` tool. The Rust atoms
register above ral-core and core never inspects them
([[internals/builtins-registry|builtins-registry]];
[[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]). The
*why* of witnessed editing is
[[design/hash-addressed-editing|hash-addressed-editing]].

## Rust atoms — `agent_builtins.rs`

`EXARCH_BUILTINS` is the slice `agent_builtins::install_on` registers into the
shell (called by `bootstrap::boot_shell` and by the sandbox-IPC child's shell
extension). The shared identity is `line_hash`: the letter `h` followed by six
hex of a Blake3 digest of a line with trailing whitespace stripped. The `h`
prefix keeps the witness un-lexable as an integer, so a hash never elaborates to
`Val::Int` and silently fails to compare against the recomputed `String`.

- `line-hash <s>` → `String`. The `h`-tagged six-hex Blake3 of a single line —
  the irreducibly-Rust digest the whole witness layer is built from. The
  `window-hash` helper folds it over a line and its ±3 neighbours.
- `_search-files <pattern>` → `[{ file, line, text }]`. A `RegexMatcher` walk of
  the cwd (`WalkBuilder`, ignore-aware; binary detection quits at NUL), each file
  gated by `check_fs_read`. The raw search — no witness; `_`-prefixed so `help`
  hides it, reached only through the `grep-files` prelude helper, which stamps each
  hit with its `window-hash`.
- `explore-dir <n>` → `[String]`. List directory entries to depth `n`,
  ignore-aware (`git_global(false)`), skipping the root.

Reads resolve through `checked_read_path` / `check_fs_read`; writes go through
ral redirection under the turn's pushed [[design/grant|grant]] frame.

## ral helpers — `agent.ral`

Sourced into the shell at boot:

- `window-hash rows i` — the witness: `line-hash` of the ±3 neighbours' own
  `line-hash`es, prefixed by the target's offset within the (edge-clamped) window.
  Context distinguishes repeated lines; the offset distinguishes lines in a file
  too short for the window to shift. Shared by `view`, `grep-files`, and `edit`,
  so a read and an edit always compute the same hash.
- `view start end` — the read primitive: materialise stdin's lines, tag each in
  `[start, end)` as `<line-no>\t<window-hash>\t<text>`. Bounds `< 1` fail; an empty
  range yields nothing.
- `view-around line peek` — `view` of the `2*peek + 1` lines centred on `line`,
  clamped at the top of the file.
- `grep-files <pattern>` → `[{ file, line, text, hash }]`. Calls `_search-files`,
  groups hits by file, reads each matched file once, and stamps every hit with its
  `window-hash` — the same line list `edit` will rebuild, so a search result feeds
  straight into a batch.
- `edit path edits` — the edit verb: `edits` is a list of `[hash, new-text]`
  pairs. Read the file once, resolve each hash to the one line whose `window-hash`
  matches (zero matches, several matches, or two pairs on one line all fail before
  any write), splice every named line in a single pass (newlines split `new-text`;
  empty deletes), write back through `>`, and hand one `` `patch `` tagged variant
  per edit — a located hunk carrying `path`, `start`, the literal `del` / `add`
  rows, and the `before` / `after` context lines around the change — to the core
  `surface` builtin directly. Resolving against one snapshot makes the batch
  atomic and non-interfering. That is the identity under a bare REPL, lifted into
  a typed rail `Patch` event when the exarch host has installed its per-turn
  [[map/exarch/shell-eval|surface sink]].

## Where to look

- `exarch/src/agent_builtins.rs` — the three Rust atoms, their type schemes, and
  `EXARCH_BUILTINS`.
- `exarch/data/agent.ral` — the helper library; seeded by `boot_shell`
  ([[map/exarch|exarch]] hub).
- The model-facing tools that carry these calls are
  [[map/exarch/tools|shell / agent / fff]].
