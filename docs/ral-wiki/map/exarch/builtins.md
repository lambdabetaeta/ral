---
generated_at_commit: cbeb5457
generated_at_date: 2026-08-17
covers_paths: [exarch/src/shell_eval/builtins.rs, exarch/src/shell_eval/builtins/, exarch/src/shell_eval/skill.rs, exarch/src/fleet/desk.rs, exarch/data/agent.ral]
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
the model never constructs a witness, only copies one from `view-hash` into
`edit-hash`, and both derive identical witnesses from identical content. The `h`
prefix keeps the witness un-lexable as an integer, so a hash never elaborates to
`Val::Int` and silently fails to compare against the recomputed `String`
([[decisions/260608_witness-hash-h-prefix|witness-hash-h-prefix]]).

## Rust atoms — `shell_eval/builtins.rs`

`EXARCH_BUILTINS` is the largest set on `builtins::host_surface()` — the one
`HostSurface` value declaring exarch's builtins beyond core's, alongside the
harness verbs and core's host-selected `SERVICE_BUILTIN`. Core's `boot_shell`
takes the surface and installs it at construction (a half-dressed production
shell is unrepresentable), and the wire engine boots the same dressing
through its `EngineInstaller` *boot recipe*
(`bootstrap::engine_boot_shell`), named on the wire by `INSTALLER_TAG` in
`Frame::Attach`.
The bulk-I/O atoms read in Rust, **below the redirect frame**, so each is one
logical operation with one surface ([[map/exarch/io-surface|io-surface]]).

- `view-text <path> <start> <end>` → `[{line, text}]`. The read primitive: the
  half-open line range `[start, end)`, each row carrying its 1-based line number
  and its text, verbatim — which is what `edit-replace` matches on. Surfaces one
  `Observed::Read { path }` observation.
- `view-hash <path> <start> <end>` → `[{line, hash, text}]`. The same range with
  each row's witness, the handle `edit-hash` checks. Reads *and hashes* the whole
  file, since the witness depends on file-wide uniqueness; both readers share one
  range door and differ only in the column.
- `grep-files <pattern>` → `[{ file, line, text }]`. An ignore-aware Rust
  regex walk of the cwd (`search_tree`, binary detection quits at NUL, each file
  gated by `check_fs_read`, the walk polling the cancel check per entry via the
  one sanctioned `cancellable` door). Surfaces exactly one
  `Observed::Grep { scope, pattern }` observation for the whole search
  ([[map/exarch/io-surface|io-surface]]).
- `edit-hash <path> <edits>` → `Unit`. The edit verb: `edits` is a list of
  `[hash: …, line: …]` records. Read the file once, resolve each hash to the one
  line whose witness matches (zero matches, several matches, a stale witness, or
  two records on one line all fail before any write), splice every named line in
  a single pass over the original rows (a real newline in the replacement splits
  the line, an empty string deletes it), and write back through core's atomic
  write door (`Shell::atomic_write`). Resolving against one snapshot makes the
  batch atomic and non-interfering. The Rust read raises no read card and the
  atomic write observes nothing; the builtin surfaces one whole-file diff card
  of the original against the final text ([[map/exarch/cards|cards]]), and
  nothing at all if the two agree. A stderr note names the replaced lines and
  warns on suspicious `\n`-style escapes (the replacement text is verbatim).
- `edit-replace <path> <from> <to>` → `Unit`. The default taught edit: replace
  the one literal occurrence of `from`, erroring (file untouched) on zero or
  several matches; same silent read, same atomic write, same diff card. Counting
  is overlap-aware (core's `occurrence_starts`), so a needle that overlaps itself
  cannot pass for unique. It speaks its own diagnostics rather than relabelling
  another builtin's: several matches name the lines, and a miss names the
  mangling it can prove — a literal `\n`-style escape in `from`, or a line
  matching apart from its indentation.
- `explore-dir <n>` → `[String]`. List directory entries to depth `n`,
  ignore-aware, skipping the root and any denied path.
- `skill-list` / `skill <name>` — Agent Skills with progressive disclosure: list
  the available skills (fresh scan each call, filtered by the grant), then load
  one skill's full `SKILL.md` body on demand; the scan and frontmatter parse
  live in `shell_eval/skill.rs`.
- `fff <query>` → `[String]`. Frecency-ranked fuzzy filename search over the
  working tree (`fff_index`, the `fff-search` crate); the per-directory index is
  cached process-globally, so forked children sharing the cwd reuse it.

Reads resolve through `checked_read_path` / `check_fs_read`; the edit writes go
through core's atomic door under the run's pushed [[design/grant|grant]]
frame ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).

## Legibility by lease class — `service`, `service-handle`

There is no model-facing listing over the worker registry at all —
`workers` was retired: a listing carrying live `Value::Handle`s cannot cross
the host seam (`SerialValue`'s decoder rejects them), and returning the
registry as a language value was mislayered in the first place — enumeration,
reaping, and caps belong to the host and the lease layer, never this door.
Legibility now
splits by class instead:

- An ordinary `spawn`-born worker (`class: Worker`) gets no listing at all.
  Its idle-observation lease already bounds a forgotten spawn's harm to at
  most an hour of one seat out of the cap, so a rail card at birth and a
  reap card at death are the whole story
  ([[map/exarch/shell-eval|shell-eval]]).
- A `service`-born worker (`class: Durable`) is bound only by legibility, so
  that bound is structural: the host reconciles a protected `services` pin —
  one row per live service, keyed by id and its birth description, born and
  retired at the attend loop's ready-boundary pass
  (`Agent::reconcile_service_pins`, `card::services_pin_card`). The pin is
  the one host-owned, write-protected register slot — unwritable by the
  program
  ([[decisions/260719_agent-names-and-schedule-labels|names-and-schedule-labels]]).

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
not ground) — it exists to be composed with an eliminator in the same run:
`await (service-handle 3)`, `cancel (service-handle 3)`.

Carried only on `builtins::host_surface()`, alongside the search/edit
atoms above: a bare REPL shell, whose boot never carries `EXARCH_BUILTINS` (nor
`SERVICE_BUILTIN`), has neither `service` nor `service-handle` — its own job
control is [[map/repl/jobs|repl/jobs]].

## ral helpers — `agent.ral`

Sourced into the shell at boot:

- `view-text-around path line peek` / `view-hash-around path line peek` — the two
  thin helpers over the atoms: the `2*peek + 1` lines centred on `line`, clamped at the top of
  the file.
- `pin-set <key> <card>` / `pin-clear <key>` — the model-facing write pair,
  thin wrappers over `surface `` `pin ``/`` `unpin ``, completing the
  `pin-*` family the two enquiries below start
  ([[decisions/260803_register-is-read-write|register-is-read-write]]).
- the **tasks kit** — `mk-task`/`add-task`/`transition` and friends, a
  pure-ral task list that reads its own state back through `pin-read` and
  writes the rendered rollup forward through `pin-set`/`pin-clear`
  (`sync-tasks`) rather than threading a bound list through every mutator
  ([[map/exarch/cards|cards]]).
- `set-goal` / `clear-goal` — `pin-set`/`pin-clear` under the `goal`
  register key, kept visible by the [[map/exarch/agent|nudge]] reminder.

## Harness verbs — context, spawn, schedule, reply

Every verb below is a `BuiltinEntry` in
`exarch/src/shell_eval/builtins/harness.rs`
(`HARNESS_BUILTINS`, carried on `host_surface()` beside the atoms above — one
surface for the boot install and the prompt's `builtin_index` alike), landed by
[[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]
over the rail [[decisions/260706_enquiry-channel|enquiry-channel]] built. A
verb's body validates its arguments engine-side and calls
`shell.enquire(class)`; `exarch/src/fleet/desk.rs`'s `ExarchDesk` decodes the class
label and answers from shared handles (`HostServices`) captured at
install — never `&mut Agent` — installed per `ral` call in `Agent::run_shell`
and swapped back to an absent desk immediately after. A closed label set the
retiring JSON tools validated as a schema enum is an open row checked at the
door instead of a closed variant type: an unknown label errors before any
enquiry crosses, naming the legal set, rather than a static row-unification
error with no room for a didactic message.

### Context stewardship

The context verbs address the model view by the exchange and digest reaches
reported by `context`; they do not expose the forensic event ledger as a
queryable store.

- **`context`** → `F [spans: [[exchange: Int, kind: Str, prompt: Str, bytes: Int,
  steps: Int, live: Bool]], total-bytes: Int, total-steps: Int, cache: Str]`.
  A silent survey of the finite view. A digest is named by the last exchange it
  reaches; `cache` says whether editing before the provider cache watermark
  will make the next request reread the prefix.
- **`context-read <exchanges>`** → `F Str`. Reads named closed exchanges as a
  role-marked, step-delimited transcript. It may name a digest by its reach,
  but not an exchange swallowed by that digest. Binding the result is silent;
  stdout and the final value of a shell call are model material, so large reads
  should be sliced rather than printed whole.
- **`context-drop <exchanges>`** → `F [bytes-delta: Int]`. Sheds whole closed
  exchanges immediately and records a `ContextEdited` model event. The live,
  unknown, folded, duplicate, or empty selection is refused with an explanation;
  a user-shaped rewind is the same closed-range operation. A negative byte
  delta is honest when the remaining digest is larger than what it replaced.
- **`context-fold [through: Int, digest: Str]`** → `F [bytes-delta: Int]`.
  Replaces the visible prefix through a closed exchange with the supplied
  digest, recording the edit immediately. The reach may extend the current
  digest but cannot cross the live exchange; folding is curation, not a promise
  of compression, so the byte delta may be negative.

- **`agents <tag>`** →
  `F [[name: Str, elapsed-s: Int, log-dir: Str]]`. One verb for the fleet, over
  an **open** row of four tags — `` `list ``, `` `start ``, `` `message ``,
  `` `cancel `` — each taking one argument, and every one of them answering
  with the roster *afterwards* rather than a receipt of its own. The outer row
  is open so an unknown tag reaches a door that enumerates the four; each known
  tag's payload keeps its exact type, so the closed record inside `` `start ``
  still makes a missing or misspelled field a static error naming it, while the
  `type`/`grant` rows *inside* that record stay open for the same reason one
  level down.
  `` `start [prompt: …, name: …, type: …, grant: …, search: …] `` is the one
  spawn: launch-only and always asynchronous, the reply arriving later through
  the inbox, and the child's row in the answer carrying the `name` and
  `log-dir` the old receipt did. `name` is the child's identity — the tab-bar
  contract (`valid_name`), unique among live agents or the call is refused;
  `type` is `` `amnemon `` (blank context) or `` `mnemon `` (imports the
  parent's model-visible context before the fresh final prompt); `grant` is one
  of the five spawnable [[map/exarch/policy|base]] names (`confined`,
  `read-only`, `edit-only`, `reasonable`, `dangerous`); `search` is a `Bool`
  admitting the provider's own hosted web search, clamped at the desk to at
  most the caller's own bit, which the trunk takes from the IT network policy's
  `search` verdict ([[map/exarch/agent|agent]]). Fuel bounds delegation depth,
  not fan-out — refused only once the caller's own `fuel` reaches zero.
  `` `message [to: …, text: …] `` and `` `cancel <name> `` are descendant-only,
  resolved by name and enforced at the desk; a scope violation raises. Where a
  removed schedule is simply gone from its answer, a cancelled agent is not:
  `AgentRegistry::cancel` only sets the cooperative token, so **a successful
  cancel answers with a roster that still lists the target** — a request, not a
  transaction, and the one place the rule needs a sentence of its own.
- **`schedules <tag>`** → `F [[label: Str, trigger: Str, next-s: Int,
  fires: Int]]`. The same shape over `` `list ``, `` `add ``, `` `remove ``.
  `` `add [trigger: …, label: …, prompt: …] `` takes a closed record — a record
  literal infers an exact row, so a missing or surplus field is a static error
  naming it, which also sidesteps ral's grammar footgun where a nullary tag
  would otherwise absorb the following positional atom; `trigger` is
  `` `cron '<expr>' `` or `` `after '<dur>' `` over an open row, `label` a
  required `Str` and the schedule's identity. The receipt's `next-s` is not
  lost but recomputed: the new schedule's row in the answer carries it, which
  is still how a mis-meant cron is caught at arm time. `` `remove <label> ``
  really does answer by absence, since `ScheduleRegistry::unschedule` removes
  the entry under the lock — the half of the rule the fleet cannot honour.
  `next-s` saturates to `i64::MAX` for a cron with no next occurrence. Every
  tag is gated on the `--allow-schedule` grant, refused with a didactic text
  otherwise.

  Both verbs issue their transition and then the list, so a raise does not
  imply nothing happened — the act may have landed and the re-read failed.
  The audit is unchanged: `agent-list` and `schedule-list` commit no `DeskAct`,
  so each transition is still one act.
- **`pin-read <key>`** → `∀α. F α`. Enquiry over the caller's own pin
  register: the card pinned at `key`, canonically re-encoded
  ([[map/exarch/cards|cards]]) so a kit can destructure it whether or not the
  bytes it wrote match what comes back; `unit` on a miss or an absent
  register. Typed on the `from-json` precedent — trusted, not checked —
  because the register is schemaless by design
  ([[decisions/260803_register-is-read-write|register-is-read-write]]).
- **`pin-list`** → `F [String]`. Silent; the keys currently occupied on the
  caller's register, in `BTreeMap` order — a key names a slot for
  `pin-read`, not its content.
- **`reply <value>`** — `∀α. α → F Unit`, first-orderness checked at the
  door. The sole return path for a returning agent; last write wins within a
  run. Refused on every non-returning agent — the interactive trunk and
  each `/branch` child alike, keyed on a `returns` bit fixed at
  construction — with the desk's own didactic text. The run ends only once
  the enclosing `ral` call's whole batch of statements drains, not at this
  call.

Receipts and listings are ral records the model can bind, filter, and fan out
over, rather than stringly-typed JSON it re-parses — the composability the
retired tool form lacked. Acting verbs render as *acts* — the
`Display::HarnessCall`/`Forensic::HarnessResult` rail pair
([[decisions/260720_harness-calls-are-acts|harness-calls-are-acts]]; a spawn
additionally derives a child tab);
listings stay silent, since their value *is* the returned record.
[[map/exarch/tools|tools]] is what remains a tool.

## Where to look

- `exarch/src/shell_eval/builtins.rs` (+ `builtins/fff_index.rs`) — the Rust
  atoms, their type schemes, and `EXARCH_BUILTINS`.
- `exarch/src/shell_eval/builtins/harness.rs` — the harness verbs above,
  `HARNESS_BUILTINS`.
- `exarch/src/shell_eval/skill.rs` — the skill scan behind
  `skill-list`/`skill`.
- `exarch/src/fleet/desk.rs` — `HostServices`, `ExarchDesk`, and the handler
  for each enquiry class.
- `exarch/data/agent.ral` — the helper library; seeded by `boot_shell`
  ([[map/exarch|exarch]] hub).
- The model-facing tool that carries every one of these calls is
  [[map/exarch/tools|`ral`]].
- There is no model-facing network builtin: a guest reaches the network
  itself, policed host-side — [[design/egress|egress]],
  [[map/exarch/agent|agent]], [[map/synod|synod]].
