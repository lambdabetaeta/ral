---
generated_at_commit: 668499f
generated_at_date: 2026-07-12
covers_paths: [exarch/src/agent_builtins.rs, exarch/src/agent_builtins/, exarch/data/agent.ral]
---

# Map: exarch / builtins

exarch's **resident host atoms and the thin ral helpers over them** — the
search, line-witness, and edit surface the model reaches through the `ral`
tool. The Rust atoms register above ral-core and core never inspects them
([[internals/builtins-registry|builtins-registry]];
[[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]). The
*why* of witnessed editing is
[[design/hash-addressed-editing|hash-addressed-editing]].

The shared identity is the *witness*: the letter `h` followed by six hex of a
Blake3 digest (trailing whitespace stripped), computed over the smallest
symmetric window of neighbouring lines — at least ±`MIN_RADIUS` (5), grown until
it names the line uniquely, falling back to the absolute index past
`MAX_RADIUS` — with the target's offset and the radius folded in
(`window_hashes`, an *adaptive-context* witness). The hashing is private Rust:
the model never constructs a witness, only copies one from `view-text` into
`edit-hash`, and both derive identical witnesses from identical content. The `h`
prefix keeps the witness un-lexable as an integer, so a hash never elaborates to
`Val::Int` and silently fails to compare against the recomputed `String`
([[decisions/260608_witness-hash-h-prefix|witness-hash-h-prefix]]).

## Rust atoms — `agent_builtins.rs`

`EXARCH_BUILTINS` is the slice `agent_builtins::install_on` registers into the
shell, together with core's host-selected `SERVICE_BUILTIN`
(`AGENT_BUILTIN_SETS`, the one source of truth); it is called by
`bootstrap::boot_shell` and named as the wire engine child's builtin installer.
The bulk-I/O atoms read in Rust, **below the redirect frame**, so each is one
logical operation with one surface ([[map/exarch/io-surface|io-surface]]).

- `view-text <path> <start> <end>` → `[{line, hash, text}]`. The read primitive:
  the half-open line range `[start, end)`, each row carrying its 1-based line
  number, its witness, and its text. Reads the whole file (the witness depends
  on file-wide uniqueness) and surfaces one `{io:"read", path}` card.
- `grep-files <pattern>` → `[{ file, line, text }]`. An ignore-aware Rust
  regex walk of the cwd (`search_tree`, binary detection quits at NUL, each file
  gated by `check_fs_read`, the walk polling the cancel check per entry via the
  one sanctioned `cancellable` door). Emits exactly one `{io:"grep", scope,
  pattern}` surface for the whole search ([[map/exarch/io-surface|io-surface]]).
- `edit-hash <path> <edits>` → `Unit`. The edit verb: `edits` is a list of
  `[hash: …, line: …]` records. Read the file once, resolve each hash to the one
  line whose witness matches (zero matches, several matches, a stale witness, or
  two records on one line all fail before any write), splice every named line in
  a single pass over the original rows (a real newline in the replacement splits
  the line, an empty string deletes it), and write back through core's atomic
  write door (`Shell::atomic_write`). Resolving against one snapshot makes the
  batch atomic and non-interfering. The Rust read raises no read card; the atomic
  write surfaces one committed `write` io event whose old/new snapshots the write
  card renders as a whole-file diff ([[map/exarch/cards|cards]]). A stderr note
  names the replaced lines and warns on suspicious `\n`-style escapes (the
  replacement text is verbatim).
- `edit-replace <path> <from> <to>` → `Unit`. The string-replace sibling:
  replace the one literal occurrence of `from`, erroring (file untouched) on
  zero or several matches; same silent read, same atomic write, same write card.
- `explore-dir <n>` → `[String]`. List directory entries to depth `n`,
  ignore-aware, skipping the root and any denied path.
- `skill-list` / `skill <name>` — Agent Skills with progressive disclosure: list
  the available skills (fresh scan each call, filtered by the grant), then load
  one skill's full `SKILL.md` body on demand.
- `fff <query>` → `[String]`. Frecency-ranked fuzzy filename search over the
  working tree (`fff_index`, the `fff-search` crate); the per-directory index is
  cached process-globally, so forked children sharing the cwd reuse it.

Reads resolve through `checked_read_path` / `check_fs_read`; the edit writes go
through core's atomic door under the turn's pushed [[design/grant|grant]]
frame ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).

## Legibility by lease class — `service`, `service-handle`

There is no model-facing listing over the worker registry at all —
`workers` was retired: a listing carrying live `Value::Handle`s cannot cross
the host seam (`SerialValue`'s decoder rejects them), and returning the
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
  retired at the same ready-boundary pass `reap_bindings` runs at
  (`Agent::reconcile_service_pins`, `card::services_pin_card`). The pin is
  unwritable by the program, the same way a `commitment:*` pin is
  ([[decisions/260703_protected-commitment-pins|protected-commitment-pins]]).

`service <desc> <thunk>` → `Handle`. The durable-birth verb: an ordinary
buffered spawn registered under the durable class, which arms no lease chain
— no idle reap, no 24 h backstop. `desc` is a mandatory, non-empty,
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

Sourced into the shell at boot:

- `view-text-around path line peek` — the one thin helper over the atoms:
  `view-text` of the `2*peek + 1` lines centred on `line`, clamped at the top of
  the file.
- the **tasks kit** — `mk-task`/`add-task`/`transition`/`surface-progress` and
  friends, the pure-ral task list whose status changes surface a pinned card
  ([[map/exarch/cards|cards]]).
- `set-goal` / `clear-goal` — pin or drop a session goal under the `goal`
  register key, kept visible by the [[map/exarch/agent|nudge]] reminder.

## Where to look

- `exarch/src/agent_builtins.rs` (+ `agent_builtins/fff_index.rs`) — the Rust
  atoms, their type schemes, and `EXARCH_BUILTINS`.
- `exarch/data/agent.ral` — the helper library; seeded by `boot_shell`
  ([[map/exarch|exarch]] hub).
- The model-facing tool that carries these calls is
  [[map/exarch/tools|`ral`]].
