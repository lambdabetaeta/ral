---
generated_at_commit: 489d125
generated_at_date: 2026-07-16
covers_paths: [exarch/src/agent_builtins.rs, exarch/src/agent_builtins/, exarch/src/desk.rs, exarch/data/agent.ral]
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

## Harness verbs — spawn, schedule, commitment, reply

Every verb below is a `BuiltinEntry` in `exarch/src/agent_builtins/harness.rs`
(`HARNESS_BUILTINS`, chained into `HOST_BUILTIN_SETS` beside the atoms above —
one registration site for `install_on` and the prompt's `builtin_index`
alike), landed by
[[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]
over the rail [[decisions/260706_enquiry-channel|enquiry-channel]] built. A
verb's body validates its arguments engine-side and calls
`shell.enquire(class)`; `exarch/src/desk.rs`'s `ExarchDesk` decodes the class
label and answers from shared handles (`HostServices`) captured at
install — never `&mut Agent` — installed per `ral` call in `Agent::run_shell`
and swapped back to an absent desk immediately after. A closed label set the
retiring JSON tools validated as a schema enum is an open row checked at the
door instead of a closed variant type: an unknown label errors before any
enquiry crosses, naming the legal set, rather than a static row-unification
error with no room for a didactic message.

- **`amnemon <prompt> <title> <permissions>` / `mnemon <prompt> <title>
  <permissions>`** → `F [id: Int, title: Str, log-dir: Str]`. Launch-only and
  always asynchronous, like the tool they replace: the receipt is the
  answer, the reply comes later through the inbox. `permissions` is one of
  the six [[map/exarch/policy|base]] names; `mnemon` additionally imports the
  parent's model-visible context before the child's fresh final prompt. Fuel
  bounds delegation depth, not fan-out — refused only once the caller's own
  `fuel` reaches zero.
- **`agents`** → `F [[id: Int, title: Str, elapsed-s: Int, log-dir: Str]]`.
  Silent; a recovery poll over live descendants.
- **`message <id> <text>`** / **`agent-cancel <id>`** → `F Unit`.
  Descendant-only, enforced at the desk; a pure confirmation, so success is
  the return and failure raises.
- **`schedule <spec>`** → `F Int`, taking a single closed **record** argument
  (`[trigger: …, label: …, prompt: …]`) rather than three positional
  arguments — a record literal infers an exact row, so a missing or surplus
  field is a static error naming it, which also sidesteps ral's grammar
  footgun where a nullary tag would otherwise absorb the following
  positional atom. `trigger` is `` `cron '<expr>' `` or `` `after '<dur>' ``;
  `label` is `` `some '<name>' `` or `` `none `` (the `sched-{id}` default).
  Gated on the `--allow-schedule` grant, refused with a didactic text
  otherwise.
- **`schedules`** → `F [[id: Int, label: Str, trigger: Str, next-s: Int,
  fires: Int]]`. Silent; `next-s` saturates to `i64::MAX` for a cron with no
  next occurrence.
- **`unschedule <id>`** → `F Unit`.
- **`commit <key> <description>`** / **`verify-commitment <key>`** →
  `F [id: Int, title: Str, log-dir: Str]`. The write and check halves of a
  protected `commitment:*` pin
  ([[decisions/260703_protected-commitment-pins|protected-commitment-pins]]):
  each forks a host-owned, read-only-narrowed child through the same spawn
  spine as `amnemon`, and the pin opens or clears only once the child's
  settled reply is the right structured shape.
- **`reply <value>`** — `∀α. α → F Unit`, first-orderness checked at the
  door. The sole return path for a returning agent; last write wins within a
  turn. Refused on every non-returning agent — the interactive trunk and
  each `/branch` child alike, keyed on a `returns` bit fixed at
  construction — with the desk's own didactic text. The run ends only once
  the enclosing `ral` call's whole batch of statements drains, not at this
  call.

Receipts and listings are ral records the model can bind, filter, and fan out
over, rather than stringly-typed JSON it re-parses — the composability the
retired tool form lacked. Acting verbs keep `Kind::HarnessCall`/
`HarnessResult` rail chrome (a spawn additionally derives a child tab);
listings stay silent, since their value *is* the returned record.
[[map/exarch/tools|tools]] is what remains a tool.

## Where to look

- `exarch/src/agent_builtins.rs` (+ `agent_builtins/fff_index.rs`) — the Rust
  atoms, their type schemes, and `EXARCH_BUILTINS`.
- `exarch/src/agent_builtins/harness.rs` — the harness verbs above,
  `HARNESS_BUILTINS`.
- `exarch/src/desk.rs` — `HostServices`, `ExarchDesk`, and the handler for
  each enquiry class.
- `exarch/data/agent.ral` — the helper library; seeded by `boot_shell`
  ([[map/exarch|exarch]] hub).
- The model-facing tool that carries every one of these calls is
  [[map/exarch/tools|`ral`]].
