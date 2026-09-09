---
generated_at_commit: 6686e770
generated_at_date: 2026-09-09
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
  range reader and differ only in the column.
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
  write site (`Shell::atomic_write`). Resolving against one snapshot makes the
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
through core's atomic write site under the run's pushed [[design/grant|grant]]
frame ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).

## Legibility by lease class — `service`, `service-handle`

There is no model-facing listing over the worker registry at all —
`workers` was retired: a listing carrying live `Value::Handle`s cannot cross
the engine protocol (`SerialValue`'s decoder rejects them), and returning the
registry as a language value was mislayered in the first place — enumeration,
reaping, and caps belong to the host and the lease layer, never this door.
Legibility now
splits by class instead:

- An ordinary `spawn`-born worker (`class: Worker`) gets no listing at all.
  Its idle-observation lease already bounds a forgotten spawn's harm to at
  most an hour of one seat out of the cap, so a rail card at birth and a
  reap card at death are the whole story
  ([[map/exarch/shell-eval|shell-eval]]).
- A `service`-born worker (`class: Durable`) is bound only by legibility, and
  that bound is now the same one an ordinary worker gets, plus one aggregate:
  the birth trail card (`worker #id cmd durable`,
  [[map/exarch/shell-eval|shell-eval]]) shown at the moment of the `service`
  call, and `/resources`' `workers.running[durable]` count. The register
  once carried a protected `services` pin the host reconciled — one row per
  live service, unwritable by the program — but that mechanism is deleted
  outright, on no rationale beyond the operator's own: protected pins should
  not exist
  ([[decisions/260719_agent-names-and-schedule-labels|names-and-schedule-labels]]'s
  2026-08-27 amendment). There is no per-service listing left; a durable
  worker's id must be read off its birth card or kept from the `Handle` the
  `service` call returned.

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
only, by the id its birth trail card named. An id naming an ephemeral
`spawn`/`watch` worker is refused exactly like an unknown one — an ephemeral
worker's rediscovery path is the binding lease, not enumeration by id. A bare
top-level `service-handle N` result cannot cross the engine protocol (a `Handle` is
not ground) — it exists to be composed with an eliminator in the same run:
`await (service-handle 3)`, `cancel (service-handle 3)`.

Carried only on `builtins::host_surface()`, alongside the search/edit
atoms above: a bare REPL shell, whose boot never carries `EXARCH_BUILTINS` (nor
`SERVICE_BUILTIN`), has neither `service` nor `service-handle`; background
work there is `spawn`, as everywhere else.

## ral helpers — `agent.ral`

Sourced into the shell at boot:

- `view-text-around path line peek` / `view-hash-around path line peek` — the two
  thin helpers over the atoms: the `2*peek + 1` lines centred on `line`, clamped at the top of
  the file.
- `pin-set <key> <card>` / `pin-clear <key>` — the model-facing write pair,
  thin wrappers over `surface `` `pin ``/`` `unpin ``, completing the
  `pin-*` family the two enquiries below start
  ([[decisions/260803_register-is-read-write|register-is-read-write]]).
- the **tasks kit** — `task-new`/`tasks-add`/`tasks-status` and friends, a
  pure-ral task list that reads its own state back through `pin-read` and
  writes the rendered rollup forward through `pin-set`/`pin-clear`
  (`tasks-sync`) rather than threading a bound list through every mutator;
  `tasks-list` is the read point, and the kit ships no query functions —
  `filter`/`first` over `tasks-list` is how you query
  ([[map/exarch/cards|cards]]).
- `goal-set` / `goal-clear` — `pin-set`/`pin-clear` under the `goal`
  register key, kept visible by the [[map/exarch/agent|nudge]] reminder.

## Harness verbs — context, spawn, schedule, reply

Every verb below is a `BuiltinEntry` in
`exarch/src/shell_eval/builtins/harness.rs`
(`HARNESS_BUILTINS`, carried on `host_surface()` beside the atoms above — one
surface for the boot install and the prompt's `builtin_index` alike), landed by
[[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]
over the rail [[map/core/engine-protocol|engine-protocol]] built. A
verb's body validates its arguments engine-side and calls
`shell.enquire(class)`; `exarch/src/fleet/desk.rs`'s `ExarchDesk` decodes the class
label and answers from shared handles (`HostServices`) captured at
install — never `&mut Agent` — installed per `ral` call in `Agent::run_shell`
and swapped back to an absent desk immediately after. A closed label set the
retiring JSON tools validated as a schema enum is an open row checked at the
door instead of a closed variant type: an unknown label errors before any
enquiry crosses, naming the legal set, rather than a static row-unification
error with no room for a didactic message.

**The desk answers six classes: four families and two singletons.**
`` `agents ``, `` `schedules ``, `` `context `` and `` `transcript `` each carry
a tag naming what to do (`family_tag`), and an unrecognised tag is as loud one
level down as an unrecognised class is at the top (`unknown_tag`) — never a
silent default. The two singletons — `` `pin-read ``, `` `pin-list `` —
take a bare payload read positionally (`payload_list` and the scalar
accessors); a family tag's own **record** crosses by field name, through
`FOValue::try_from(&Value)` and out through `Fields`, `` `evict ``'s and
`` `grep ``'s included. The desk's decode is not a
duplicate of the builtin's door but the **trust boundary**: the door checks
engine-side so a bad value reaches the model with the parser's own message, and
the desk checks again because a guest can send whatever it likes — which is why
`CronSchedule::parse` runs on both sides deliberately.

### Context stewardship

Two verbs over two things: `context` is **what the provider is sent** and the
model pays for, and `transcript` is **the record**, every turn this session or
its ancestors ever recorded
([[decisions/260906_context-rollover|context-rollover]],
[[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]). `context`
edits and surveys; `transcript` only reads. Both speak **turns**: a user turn
is a prompt (or an import's opening) and anything before the first reply, an
assistant turn is the assistant message with the tool results it called for,
and an exchange is the run of turns from a user turn, carrying that turn's id.

- **`context <tag>`** → `∀ρ1 ρ2. <survey | drop [Int] | evict [through: Int |
  ρ1] | ρ2> → F [rows: [[id: Int, exchange: Int, kind: Str, label: Str,
  bytes: Int]], evicted: Int, total-bytes: Int]`.
  One verb per addressable state: the tag selects the transition, and **every**
  tag answers the survey afterwards. That is not a shared prefix collapsed but
  the rule the registries already follow, and it fits this surface better than
  either, because an edit changes *what is addressable* — an evicted turn
  stops being nameable — so the edit is also the resurvey the
  next edit must be written against.
  - `` `survey `` describes the context, one row per resident turn (`turn_row`,
    the very shape `` transcript `index `` answers with), beside `evicted`, the
    count of turns that have left it, and `total-bytes`, which is what is
    actually sent rather than the sum of the rows: an abandoned exchange's
    turns report their own weights while the context carries only its one-line
    note. It changes nothing.
  - `` `drop <exchanges> `` sheds whole closed exchanges; they remain in the
    transcript. The live, already-gone, duplicate, or empty selection
    is refused with an explanation; a user-shaped rewind is the same
    closed-range operation.
  - `` `evict [through, note] `` removes every turn through the one named, and
    with it every exchange wholly before them, replaced at the head by the
    harness's index of what
    left; a user turn whose exchange still has a resident turn above the cut
    stays with it, so a cut into an exchange keeps its prompt, and the newest
    turn can never be named at all. `note` is the model's own line to its
    future self, rendered beside
    that index; the harness's own eviction writes none. The `evict` row is
    **open** precisely because `note` is optional and a closed row cannot say
    so: `context_evict_payload` checks its type at the door, and the desk
    refuses a note it will not draw — a marker reading
    `Your note at eviction: ""` is a defect the type should prevent, and
    making `note` required would invite exactly that. Its size and shape are
    the desk's too — at most `NOTE_CAP` (240) bytes, and no line break, since
    the marker draws one row per exchange fragment the eviction names and a
    note that could add a row would unbound the one message an eviction never
    reclaims.

  Each edit records a `ContextEdited` model event at the desk immediately,
  under `DeskAct::ContextEvict` or `ContextDrop`. There is no byte-delta
  receipt: the decision-relevant number is `total-bytes` now against the
  budget ([[decisions/260812_context-is-a-projection|context-is-a-projection]]).
- **`transcript <tag>`** → `∀α ρ1 ρ2 ρ3. <index | read [ρ1] | grep [pattern: Str
  | ρ2] | ρ3> → F α`. Read-only: no tag records a protocol event, though each
  records a `Display::HarnessCall` for the screen. The answer type is a bare
  `α` because the three tags answer three shapes. `` `read ``'s row is open
  *outright* because both of its fields are optional and a record row can
  anchor only a required one; `transcript_read_payload` checks whichever
  arrive, and naming neither is the desk's to refuse.
  - `` `index `` → the survey's own rows over every turn the transcript holds,
    plus `held: Str` — `resident`, `evicted`, or `dropped` — oldest first.
    `kind` is `exchange`, `import`, or `inherited` (an ancestor's). A departed
    turn is listed at the weight it carried when it left.
  - `` `read [exchanges: [Int], turns: [Int, Int]] `` →
    `[[exchange: Int, turns: [Int], messages: [Message]]]`, one record per
    exchange the read touched, in transcript order, each addressed by its
    own `exchange` field rather than by a `=== … ===` header a reader had to
    re-parse, and naming the turns it covered. `exchanges` names whole closed
    exchanges, `turns` is one inclusive range (`[n, n]` is the single turn
    `n`), and the two **compose as a union**. An exchange the read reaches
    whole renders through `closed_messages` — what the model was sent, whether
    resident, departed to this log's file, or an ancestor's — while a range
    that reaches only part of one answers those turns' own material; a
    fork-split exchange renders per turn, since a mixture of two files' records
    would fold as abandoned.
  - `` `grep [pattern, exchanges, turns] `` → `[hits: [[exchange: Int,
    turn: Int, role: Str, line: Int, text: Str]], total: Int]`. A Rust regex —
    ral's own `re-*`
    dialect, compiled at the desk so a bad pattern is refused in the regex
    crate's words — over prompts, programs, results, and reasoning, per line.
    Both narrowings are optional and compose as the same union; with neither,
    the whole transcript is searched. At most `GREP_HITS` (100)
    hits, oldest first, each line clipped at 200 bytes, with `total` the true
    count so a large one says *narrow*, not *page*.

  Only the turn being written *now* is unreadable: the earlier turns of the
  exchange in hand have closed and read back like any other, which is what the
  live-exchange refusal names when it points at them.

  `` `read `` is the one harness answer whose size is the size of the thing it
  describes: the survey spends a few hundred bytes to describe a 200 KB
  context, and this returns the 200 KB. That is why it is a tag of `transcript`
  and not of `context` — the distinct name is the cheapest safety mechanism a
  model-facing surface has, and the only one that acts before the call rather
  than after — and why the docstring points a long search at a `mnemon` child,
  which shares the transcript and spends its own context on it.

  A `Message` is `[role: `system|`user|`assistant|`tool, parts: [Part]]`, one
  per message the turns hold — the meta half of taking a turn carries no
  message of its own, the same as it contributes none to a live
  provider request. A `Part` is a variant, one arm per
  `genai::chat::ContentPart` — `` `text ``, `` `program `` (the ral tool call
  itself: the script source for exarch's own tool, or a name and argument
  keys for any other), `` `result ``, `` `reasoning ``, `` `binary `` (media
  metadata only), `` `custom `` — matched exhaustively in
  `exarch/src/record/model/transcript.rs`, so a genai variant this arm list
  has not met is a compile error rather than a serialization of the provider's
  struct leaking through as content. `` `ThoughtSignature `` carries no part at all:
  an opaque continuation token, dropped rather than rendered. Narrowing this
  material — truncation, elision, byte caps — is deliberately not this
  builtin's job: it is `filter`/`take`/`view-text` over the records, the way
  `tasks-list` puts querying on the caller rather than the kit
  ([[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]]
  for the transcript-as-value law this reads under: a turn's own recorded
  `Protocol` material, converted rather than re-rendered). `` `grep `` searches that
  same narrowing — `` `text ``,
  `` `program ``'s source, `` `result ``, `` `reasoning ``, a binary payload
  and a provider extension carrying no text a pattern could mean — so nothing
  is searchable that is not readable.

- **`agents <tag>`** → `∀α. F α`. One verb for the fleet, over an **open** row
  of six tags — `` `list ``, `` `start ``, `` `message ``, `` `cancel ``,
  `` `reply <value> ``, `` `read <name> `` — each taking one argument. Every
  tag but `` `read `` answers with the roster *afterwards*,
  `[[name: Str, state: <busy|waiting-on-agents|replied|waiting>, idle-s: Int, elapsed-s: Int, log-dir: Str]]`,
  rather than a receipt of its own; `` `read `` answers `[name: Str, reply: α]`,
  the value a replied child deposited, which is why the family's answer type is
  a bare `α` (the `pin-read` precedent) rather than the roster it once was
  ([[decisions/260826_reply-parks|reply-parks]]). `` `reply `` is the sole
  return path of a returning agent — first-orderness checked at the door,
  refused on every non-returning agent with the desk's own didactic text, last
  write wins within a run, deposited once the enclosing `ral` batch drains. The
  outer row is open so an unknown tag reaches a door that enumerates the six; each known
  tag's payload keeps its exact type, so the closed record inside `` `start ``
  still makes a missing or misspelled field a static error naming it, while the
  `type`/`grant`/`provider`/`model` rows *inside* that record stay open for the
  same reason one level down.
  `` `start [prompt: …, name: …, type: …, grant: …, search: …, provider: …, model: …] `` is the one
  spawn: launch-only and always asynchronous, a one-line notice arriving
  through the inbox when the child replies, and the child's row in the answer carrying the `name` and
  `log-dir` the old receipt did. `name` is the child's identity — the tab-bar
  contract (`check_name`, in `fleet.rs` beside the rule it belongs
  with), unique among live agents or the call is refused; this door refuses a
  malformed one early and in the model's own words, and `Fleet::enrol` refuses it
  again for peers that never came through a door
  ([[map/exarch/agent|agent]]);
  `type` is `` `amnemon `` (blank context) or `` `mnemon `` (imports the
  parent's model-visible context before the fresh final prompt); `grant` is one
  of the five spawnable [[map/exarch/policy|base]] names (`confined`,
  `read-only`, `edit-only`, `reasonable`, `dangerous`); `search` is a `Bool`
  admitting the provider's own hosted web search, clamped at the desk to at
  most the caller's own bit, which the trunk takes from the IT network policy's
  `search` verdict ([[map/exarch/agent|agent]]). `provider` and `model` are
  each `` `inherit `` or `` `named <Str> ``, and both are always written:
  ral has no optional field, so absence is *data* carried by a variant
  ([[invariants/optionality-via-variants|optionality-via-variants]]) and the
  record row stays closed. `` `inherit ``/`` `inherit `` shares the parent's
  `Arc<Provider>` verbatim; a named `model` alone keeps the parent's account
  and credential; a named `provider` alone runs the parent's model when it
  resolves to the parent's own account, else that account's service default
  model, refused naming `model` when the service publishes none. Because
  `` `inherit `` *states* which account the child is on, a named model never
  has to be attributed to one, so the desk resolves a provider name with
  `resolve_pinned_provider` and never touches the catalog: a spawn can never
  block the fleet on a model-list round trip. Fuel bounds delegation depth,
  not fan-out — refused only once the caller's own `fuel` reaches zero.
  `` `message [to: …, text: …] `` and `` `cancel <name> `` are descendant-only,
  resolved by name and enforced at the desk; a scope violation raises. Where a
  removed schedule is simply gone from its answer, a cancelled agent is not:
  `Agent::cancel_tree` only sets the cooperative token (and stamps the eval
  reach), so **a successful
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
  that names the tag the model actually typed — never a spelling it was never
  taught.

  Both verbs issue their transition and then the list, so a raise does not
  imply nothing happened — the act may have landed and the re-read failed.
  The audit is unchanged: `` `list `` commits no `DeskAct` in either family,
  so each transition is still one act.
- **`pin-read <key>`** → `∀α. F α`. Enquiry over the caller's own pin
  register: the card pinned at `key`, canonically re-encoded
  ([[map/exarch/cards|cards]]) so a kit can destructure it whether or not the
  bytes it wrote match what comes back; `()` on a miss or an absent
  register. Typed on the `from-json` precedent — trusted, not checked —
  because the register is schemaless by design
  ([[decisions/260803_register-is-read-write|register-is-read-write]]).
- **`pin-list`** → `F [String]`. Silent; the keys currently occupied on the
  caller's register, in `BTreeMap` order — a key names a slot for
  `pin-read`, not its content.

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
