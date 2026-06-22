---
generated_at_commit: 1baac6d
generated_at_date: 2026-06-22
covers_paths: [exarch/src/agent_builtins.rs, exarch/data/agent.ral]
---

# Map: exarch / builtins

exarch's **resident host atoms and the thin ral helpers over them** — the
search, line-witness, and edit surface the model reaches through the `shell`
tool. The Rust atoms register above ral-core and core never inspects them
([[internals/builtins-registry|builtins-registry]];
[[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]). The
*why* of witnessed editing is
[[design/hash-addressed-editing|hash-addressed-editing]].

The shared identity is the *witness*: a `line_hash` is the letter `h` followed
by six hex of a Blake3 digest of a line with trailing whitespace stripped, and a
`window-hash` folds that over a line and its ±3 neighbours. The `h` prefix keeps
the witness un-lexable as an integer, so a hash never elaborates to `Val::Int`
and silently fails to compare against the recomputed `String`
([[decisions/260608_witness-hash-h-prefix|witness-hash-h-prefix]]).

## Rust atoms — `agent_builtins.rs`

`EXARCH_BUILTINS` is the slice `agent_builtins::install_on` registers into the
shell (called by `bootstrap::boot_shell` and by the sandbox-IPC child's shell
extension). The bulk-I/O atoms read in Rust, **below the redirect frame**, so
each is one logical operation that surfaces once and never raises a per-file
read or write io card ([[map/exarch/io-surface|io-surface]]).

- `line-hash <s>` → `String`. The `h`-tagged six-hex Blake3 of a single line —
  the irreducibly-Rust digest the whole witness layer is built from. Numbering,
  slicing, and tagging compose in ral; only the digest cannot.
- `window-hash <rows> <i>` → `String`. The witness for line `i` (0-indexed) of
  the row list: the `line-hash` of the ±3 neighbours' own `line-hash`es, prefixed
  by the target's offset within the (edge-clamped) window. Context distinguishes
  repeated lines; the offset distinguishes lines in a file too short for the
  window to shift. Shared by `view`, `grep-files`, and `edit`, so a read and an
  edit always compute the same hash.
- `grep-files <pattern>` → `[{ file, line, text, hash }]`. An ignore-aware Rust
  regex walk of the cwd (`search_tree`, binary detection quits at NUL, each file
  gated by `check_fs_read`), reading every matched file once and stamping each hit
  with its `window-hash` from those same rows — the very row list `edit` will
  rebuild, so a search result feeds straight into a batch. A non-UTF-8 match
  carries an empty-string witness, a value no `window-hash` produces, so it
  resolves to no line. Emits exactly one `{io:"grep", scope, pattern}` surface
  for the whole search ([[map/exarch/io-surface|io-surface]]).
- `edit <path> <edits>` → `Unit`. The edit verb: `edits` is a list of
  `[hash, new-text]` pairs. Read the file once, resolve each hash to the one line
  whose `window-hash` matches (zero matches, several matches, or two pairs on one
  line all fail before any write), splice every named line in a single pass over
  the original rows (a real newline in `new-text` splits the line, an empty string
  deletes it), and write back atomically. Resolving against one snapshot makes the
  batch atomic and non-interfering. The read door and the atomic write door both
  run in Rust below the redirect frame, so `edit` raises no read or write io card;
  it surfaces only one whole-file `diff` card — the original-vs-final line-level
  diff grouped into hunks by the `similar` crate with ±2 lines of context, built
  by `diff_card_value` and handed to the core `surface` builtin
  ([[map/exarch/cards|cards]]). A no-op edit yields no hunks and surfaces nothing.
- `explore-dir <n>` → `[String]`. List directory entries to depth `n`,
  ignore-aware (`git_global(false)`), skipping the root and any denied path.

Reads resolve through `checked_read_path` / `check_fs_read`; the `edit` write
goes through `check_fs_write` under the turn's pushed [[design/grant|grant]]
frame ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).

## ral helpers — `agent.ral`

Sourced into the shell at boot; the line readers over the `window-hash` builtin,
plus the faithful split they share:

- `_rows s` — split a body on raw `\n`, keeping the trailing empty a terminal
  newline produces, so rejoining with `\n` reproduces the body exactly. The
  byte-faithful split (unlike the edge-trimming `lines`) is what lets a trailing
  newline survive an edit and the window-hashes be computed over the file's actual
  line structure — the ral twin of the Rust `rows_of`.
- `view start end` — the read primitive: materialise stdin's lines, tag each in
  `[start, end)` as `<line-no>\t<window-hash>\t<text>`. Bounds `< 1` fail; an empty
  range yields nothing.
- `view-around line peek` — `view` of the `2*peek + 1` lines centred on `line`,
  clamped at the top of the file.

## Where to look

- `exarch/src/agent_builtins.rs` — the Rust atoms, their type schemes, and
  `EXARCH_BUILTINS`.
- `exarch/data/agent.ral` — the helper library; seeded by `boot_shell`
  ([[map/exarch|exarch]] hub).
- The model-facing tools that carry these calls are
  [[map/exarch/tools|shell / agent / fff]].
