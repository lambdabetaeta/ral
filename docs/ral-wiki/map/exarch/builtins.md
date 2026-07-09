---
generated_at_commit: d501492
generated_at_date: 2026-07-06
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
  window to shift. Shared by `view`, `grep-files`, and `edit-hash`, so a read and an
  edit always compute the same hash.
- `grep-files <pattern>` → `[{ file, line, text, hash }]`. An ignore-aware Rust
  regex walk of the cwd (`search_tree`, binary detection quits at NUL, each file
  gated by `check_fs_read`), reading every matched file once and stamping each hit
  with its `window-hash` from those same rows — the very row list `edit-hash` will
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
  run in Rust below the redirect frame, so `edit-hash` raises no read or write io card;
  it surfaces only one whole-file `diff` card — the original-vs-final line-level
  diff grouped into hunks by the `similar` crate with ±2 lines of context, built
  by `diff_card_value` and handed to the core `surface` builtin
  ([[map/exarch/cards|cards]]). A no-op edit yields no hunks and surfaces nothing.
- `explore-dir <n>` → `[String]`. List directory entries to depth `n`,
  ignore-aware (`git_global(false)`), skipping the root and any denied path.

Reads resolve through `checked_read_path` / `check_fs_read`; the `edit-hash` write
goes through `check_fs_write` under the turn's pushed [[design/grant|grant]]
frame ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).

## Legibility by lease class — `service`, `service-handle`

There is no model-facing listing over the worker registry at all —
`workers` was retired: a listing carrying live `Value::Handle`s cannot cross
the host seam (`SerialValue::from_ground` rejects them), and returning the
registry as a language value was mislayered in the first place — enumeration,
reaping, and caps belong to the host and the lease layer, never this door
([[decisions/260705_leases-and-budgets|leases-and-budgets]]). Legibility now
splits by class instead:

- An ordinary `spawn`-born worker (`class: Worker`) gets no listing at all.
  Its idle-observation lease already bounds a forgotten spawn's harm to at
  most an hour of one seat out of the cap, so a rail card at birth and a
  reap card at death are the whole story
  ([[map/exarch/shell-eval|shell-eval]]).
- A `service`-born worker (`class: Durable`) is bound only by legibility, so
  that bound is structural: the host reconciles a protected `services` pin —
  one row per live service, keyed by id and its birth description, born and
  retired at the same boundary pass `drain_worker_reaps` runs at
  (`Agent::reconcile_service_pins`, `card::services_pin_card`). The pin is
  unwritable by the program, the same way a `commitment:*` pin is
  ([[decisions/260703_protected-commitment-pins|protected-commitment-pins]]).

`service <desc> <thunk>` → `Handle`. The durable-birth verb: an ordinary
buffered spawn registered under the durable class, which arms no lease chain
— no idle reap, no 24 h backstop. `desc` is now a mandatory, non-empty,
single-line `String` — the whole legibility bound a durable birth declares,
so it cannot be absent — and lands verbatim (trimmed) as the registry
entry's `cmd`, which is what the `services` pin renders. Cancellable through
its handle, dead with `/clear` or the process. Length is declared at birth,
never promoted into after the fact. The atom itself lives in core
(`SERVICE_BUILTIN`, the `watch` mechanism with the hosts swapped —
[[map/core/builtins|map: core builtins]]); exarch is the host that installs
it, because only under exarch's lease frame does a durable birth distinguish
anything.

`service-handle <id>` → `Handle`. The one narrow door back to a never-bound
service's handle: looked up among this shell's `LeaseClass::Durable` entries
only, by the id shown on the `services` pin. An id naming an ephemeral
`spawn`/`watch` worker is refused exactly like an unknown one — an ephemeral
worker's rediscovery path is the binding lease, not enumeration by id. A bare
top-level `service-handle N` result cannot cross the host seam (a `Handle` is
not ground) — it exists to be composed with an eliminator in the same turn:
`await (service-handle 3)`, `cancel (service-handle 3)`.

Registered only by `agent_builtins::install_on`, alongside the search/edit
atoms above: a bare REPL shell, which never installs `EXARCH_BUILTINS` (nor
`SERVICE_BUILTIN`), has neither `service` nor `service-handle` — its own job
control is [[map/repl/jobs|repl/jobs]].

## ral helpers — `agent.ral`

Sourced into the shell at boot; the line readers over the `window-hash` builtin,
plus the faithful split they share:

- `_rows s` — split a body on raw `\n`, keeping the trailing empty a terminal
  newline produces, so rejoining with `\n` reproduces the body exactly. The
  byte-faithful split (unlike the edge-trimming `lines`) is what lets a trailing
  newline survive an edit and the window-hashes be computed over the file's actual
  line structure — the ral twin of the Rust `rows_of`.
- `view-text start end` — the read primitive: materialise stdin's lines, tag each in
  `[start, end)` as `<line-no>\t<window-hash>\t<text>`. Bounds `< 1` fail; an empty
  range yields nothing.
- `view-text-around line peek` — `view-text` of the `2*peek + 1` lines centred on `line`,
  clamped at the top of the file.

## Where to look

- `exarch/src/agent_builtins.rs` — the Rust atoms, their type schemes, and
  `EXARCH_BUILTINS`.
- `exarch/data/agent.ral` — the helper library; seeded by `boot_shell`
  ([[map/exarch|exarch]] hub).
- The model-facing tools that carry these calls are
  [[map/exarch/tools|shell / agent / fff]].
