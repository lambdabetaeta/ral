---
generated_at_commit: 61c288f
generated_at_date: 2026-10-21
covers_paths: [exarch/src/shell_eval/builtins.rs, exarch/src/shell_eval/builtins/, exarch/src/skill.rs, exarch/src/agent/desk.rs, exarch/src/agent/desk/, exarch/src/enquiry.rs, exarch/src/library.rs, exarch/data/agent.ral]
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
harness verbs. Core's `boot_shell`
takes the surface and installs it at construction (a half-dressed production
shell is unrepresentable), and the wire engine boots the same dressing
through its `EngineInstaller`'s two halves
(`boot::engine_runtime` and `boot::engine_root`), named on the wire by `INSTALLER_TAG` in
`Frame::Attach`.
The bulk-I/O atoms read in Rust, **below the redirect frame**, so each is one
logical operation with one surface ([[map/exarch/io-surface|io-surface]]).

The readers and editors share one line-ending policy (`Eol`): a file whose
every `\n` follows a `\r` is CRLF, read as `\n` text and written back in CRLF;
any other file, mixed ones included, is taken raw. So an unedited round trip is
byte-exact, a multi-line `from` matches a CRLF file, and an edit never leaves
mixed endings behind.

- `view-text <path> <start> <end>` → `[{line, text}]`. The read primitive: the
  half-open line range `[start, end)`, each row carrying its 1-based line number
  and its text, verbatim — which is what `edit-replace` matches on. Surfaces one
  `Observed::Read { path }` observation.
- `view-hash <path> <start> <end>` → `[{line, hash, text}]`. The same range with
  each row's witness, the handle `edit-hash` checks. Reads *and hashes* the whole
  file, since the witness depends on file-wide uniqueness; both readers share one
  range reader and differ only in the column.
- `grep-files <pattern> <path>` → `[{ file, line, text }]`. An ignore-aware
  Rust regex search of `path`, a file or a directory walked recursively
  (`search_tree`; a hit names its file in the shape `path` was written, so a
  reader takes it back unchanged; a missing path is an error,
  not an empty search; CRLF-aware, so `$` anchors before `\r\n` and a hit's text
  is the row `view-text` shows; binary detection quits at NUL, the walk the
  shared one below, kept to `path`'s filesystem). Each file is streamed, never
  read whole: every read polls the shell's scope, the line ending is learnt as
  the bytes pass, and a line past 16 MiB skips its file with a note on stderr.
  Surfaces exactly one
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
- `edit-replace <path> <from> <to>` → `Unit`. Replace
  the one literal occurrence of `from`, erroring (file untouched) on zero or
  several matches; same silent read, same atomic write, same diff card. Counting
  is overlap-aware (core's `occurrence_starts`), so a needle that overlaps itself
  cannot pass for unique. It speaks its own diagnostics rather than relabelling
  another builtin's: several matches name the lines, and a miss names the
  mangling it can prove — a literal `\n`-style escape in `from`, or a line
  matching apart from its indentation.
- `explore-dir <n> <path>` → `[String]`. List the entries beneath `path` to
  depth `n`, ignore-aware, skipping the root and any denied path.
- `find-files <query> <path>` → `[String]`. The files under PATH (resolved and
  existence-checked as `grep-files` does), walked as `grep-files` walks,
  fuzzy-ranked by path through core's `text::rank` (nucleo), cut to
  `FIND_LIMIT`. It filters before the cut: a denied match neither takes a
  slot nor shows in the count. No index is kept, so every name the model
  learns has passed the door this call.

Each walk resolves its `path` through `walk_root` (grant-checked; missing is
an error) and runs core's admitted `Walk` over it
([[map/core/capabilities|capabilities]]), as `glob` does: cancel is polled per
entry, each entry is anchored by its own path (so a file named `~` or `xdg:x`
is that file, not a sigil's expansion) and admitted silently, and a denied or
unreadable entry is skipped, so neither a refusal nor an error names a withheld
path. `grep-files` then reads each listed file through `locate_if_admitted`,
judging the object it opens.
- `skill-list` / `skill <name>` — Agent Skills with progressive disclosure: list
  the available skills (fresh scan each call, filtered by the grant), then load
  one skill's full `SKILL.md` body on demand; the scan and frontmatter parse
  live in `skill.rs`. Startup discovery skips invalid files quietly; `skill-list`
  lists valid skills, then names each skipped `SKILL.md` by path with its cause;
  its count covers only valid skills. `skill` reports a read or frontmatter
  failure with the skill's name and path.

Reads resolve through `Shell::locate::<Reading>`; the edit writes go
through core's atomic write site under the run's pushed [[design/grant|grant]]
frame ([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).

## Listing workers: `workers`

**A listing is data; capabilities are lexical values.** `workers` is core's
verb, present in every host ([[map/core/builtins|map: core builtins]]), so
exarch declares nothing of its own here. Every worker is legible by its id:
the birth trail card (`errand #7 started · building`) and the completion
notice name it ([[map/exarch/shell-eval|shell-eval]]), and `/resources`
counts the `running`, `stopping`, and `ready` entries.

- `workers` → `[WorkerRow]`. The registry as records — `id`, `title`,
  `owner`, `state`, `started`, `elapsed`, `settled` — one per worker
  (`running`, `stopping`, or `ready`), and never a handle, so a top-level
  `workers` crosses the engine protocol and the `VALUE` section prints it as
  aligned rows, by the printer's own rule. Reading it claims nothing; no
  builtin mints a `Handle` from an id.

## ral helpers — `agent.ral`

Sourced into the shell at boot:

- `view-text-around path line peek` / `view-hash-around path line peek` — the two
  thin helpers over the atoms: the `2*peek + 1` lines centred on `line`, clamped at the top of
  the file.
- the **tasks kit** — `task-new` and friends, a pure-ral task list that
  reads its own state back through `` exarch-pins `read `` and writes the
  rendered rollup forward through `` exarch-pins `set ``/`` `clear ``
  (`tasks-sync`) rather than threading a bound list through every mutator;
  `exarch-tasks` `` `list `` is the read point, and the kit ships no query
  functions — `filter`/`first-where` over it is how you query
  ([[map/exarch/cards|cards]]).
- `exarch-goal` `` `set ``/`` `clear `` — `exarch-pins` `` `set ``/`` `clear ``
  under the `goal` register key, kept visible by the
  [[map/exarch/agent|nudge]] reminder.

## Harness verbs — context, spawn, schedule, reply

Every verb below is a `BuiltinEntry` in
`exarch/src/shell_eval/builtins/harness.rs`
(`HARNESS_BUILTINS`, carried on `host_surface()` beside the atoms above — one
surface for the boot install and the prompt's `builtin_index` alike), landed by
[[decisions/260702_agent-tool-to-exarch-builtin|agent-tool-to-exarch-builtin]]
over the rail [[map/core/engine-protocol|engine-protocol]] built. A verb's
body reads the model's argument, as first-order data, into a `Request` and
calls `shell.enquire` with it; `exarch/src/agent/desk.rs`'s `ExarchDesk`
decodes the same `Request` and matches it exhaustively, answering from shared
handles (`HostServices`) captured at install — never `&mut Agent` — installed
per `ral` call in `Agent::run_shell` and swapped back to an absent desk
immediately after. The door then admits the host's answer only in the shape
the request owes (`Request::owed`). A closed label set is an open row checked
at the door instead of a closed variant type: an unknown label errors before
any enquiry crosses, naming the legal set, rather than a static
row-unification error with no room for a didactic message.

**One vocabulary, typed once: `exarch/src/enquiry.rs`.** The desk
answers five classes, each a `Family` of tags — `` `exarch-agents ``,
`` `exarch-schedules ``, `` `exarch-context ``, `` `exarch-transcript `` and
`` `exarch-pins `` — every class, tag, field and answer a `Datum`, with one
`encode` and one strict, total `decode`. An unrecognised tag is as loud one
level down as an unrecognised class is at the top, the refusal offering the
tags the family takes; a tag's record crosses by field name, strict in its
keys (`record!`), `` `evict ``'s and `` `grep ``'s included. Because door and
desk decode with the one function, they refuse in the one wording. The
desk's decode is still not a duplicate of the door's but the **trust
boundary**: the door refuses engine-side so a bad value reaches the model with
the parser's own message, and the desk refuses again because a guest can send
whatever it likes — which is why a cron expression is parsed
(`CronSchedule::parse`, inside the one decode) on both sides.

### Context stewardship

The `explain` guides in `shell_eval/builtins/harness.rs` document each
operation with examples, return shapes, limits, and recovery advice.
`data/context.md` keeps the working strategy: evict irrelevant retrievals and
failed attempts once retried, each as a run `[from, to]` under a required
note; cut at the bottom or in batch; cover a residue whole or not at all;
expect the a forced eviction to take the current prompt too, so evict
first under one's own note, saying what was asked; and read
residues back through `exarch-transcript`. The command reference explicitly
distinguishes bytes from tokens, requires nonempty single-line UTF-8
eviction notes, and warns that transcript reads are unbounded.

Two verbs over two things: `exarch-context` is **what the provider is sent** and the
model pays for, and `exarch-transcript` is **the record**, every turn this session or
its ancestors ever recorded
([[decisions/260906_context-rollover|context-rollover]],
[[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]). `exarch-context`
edits and surveys; `exarch-transcript` only reads. Both speak **turns**, and every
turn carries a **role**: a `user` turn is a prompt (or an import's opening) and
anything before the first reply, an `assistant` turn is the assistant message
with the tool results it called for. The turns *answering* a prompt — those
after it, up to the next prompt — are a function of role and order, named in
no answer
([[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]).
An eviction leaves a **residue** standing for its run: a span `[from, to]` of
turn ids, both inclusive, and the model's note, addressed by the ids it covers
and never by an id of its own ([[decisions/261019_an-eviction-is-a-span|an-eviction-is-a-span]]).
Every `ral` tool result ends with `TURN: <id>`, the id of the turn it closes,
so the model has the address before it asks for one.

- **`exarch-context <tag>`** → `∀ρ. <survey | evict [from: Int, to: Int, note: Str] | ρ> →
  F [rows: [[from: Int, to: Int, role: Str, kind: Str, label: Str, bytes: Int]],
  total-bytes: Int]`.
  One verb per addressable state: the tag selects the transition, and **every**
  tag answers the survey afterwards. That is not a shared prefix collapsed but
  the rule the registries already follow, and it fits this surface better than
  either, because an edit changes *what is addressable* — an evicted run
  becomes one residue — so the edit is also the resurvey the
  next edit must be written against.
  - `` `survey `` describes the context, one span row per frontier item — a
    resident turn (`from == to`, `kind` `own`, `import` or `inherited`) or a
    resident residue (`kind` `residue`, role `user`, its note as `label`) —
    the very rows `` exarch-transcript `index `` answers with, less
    `location`, beside `total-bytes`, which is what is actually sent rather
    than the sum of the rows. What lies under a residue is
    `` transcript `index ``'s to say. It changes nothing.
  - `` `evict [from, to, note] `` is the one context edit: it takes one
    contiguous run of the context — turns and residues alike, both ends
    inclusive — and leaves one bracketed user-voice residue standing for it,
    carrying the note and how to read the run back
    ([[decisions/261019_an-eviction-is-a-span|an-eviction-is-a-span]]).
    `[from: 41, to: 60]` names turns 41–60 and is the address of the residue
    it leaves. A run that reaches into a residue must cover it whole; a
    larger run that does replaces the old notes with the new one, and the
    same span over one residue rewrites its note. Refused by name: an end
    the transcript never recorded, an end already withdrawn, a run with the
    **unclosed** turn — the one being written — within, and a run cutting a
    residue (`turns 41–60 stand together under one note: name from: 41, to:
    60 whole, or leave it`). A prompt may be evicted, the live one included;
    the note should say what was asked. `note` is the model's own line to
    its future self and is **required**: a forced eviction is this
    same edit under another authority and the only one without a note;
    `/rewind` is a different record, `Protocol::Rewound`, which withdraws
    turns and leaves no residue at all. The desk refuses an empty note, and
    its size and shape are the desk's too — at most `Note::CAP` (240) bytes
    and no line break, since the residue keeps one short line, the whole of
    what an eviction leaves in the context.

  The edit records a `Protocol::Evicted { residue }` model event
  (`Residue { span, note, by }`) at the desk immediately, under `DeskAct::ContextEvict`, whose subject is the span —
  `turns 41–60` — and whose result says the turns now stand under the note,
  with the context's serialized bytes. There is no byte-delta
  receipt: the decision-relevant number is `total-bytes` now against the
  budget ([[decisions/260812_context-is-a-projection|context-is-a-projection]]).
- **`exarch-transcript <tag>`** → `∀α ρ. <index | read [turns: [Int]] |
  grep [pattern: Str, turns: <all | only [Int]>] | ρ> → F α`. Read-only: no tag records a protocol
  event, though each
  records a `Display::HarnessCall` for the screen. The answer type is a bare
  `α` because the three tags answer three shapes. Every narrowing is a list of
  turn ids, and the ids are the ones `` `evict ``'s `from` and `to` name: the
  context family speaks a run by its ends, the transcript family a `[Int]`
  ([[decisions/261019_an-eviction-is-a-span|an-eviction-is-a-span]]).
  Both records are **closed**; `` `grep ``'s `turns` is `` `all `` or
  `` `only [Int] ``, absence being data.
  - `` `index `` → the survey's own rows over every turn and every residue
    the transcript holds, plus `location` — `` `resident ``, `` `under [from:
    Int, to: Int] `` naming the innermost residue over the item, a named
    record like every other span, or `` `withdrawn `` — ordered
    by `from`, wider span first, later-recorded first, so a residue precedes
    what it covers. `kind` is `own`, `import`, `inherited` (an ancestor's) or
    `residue`. A departed turn is listed at the weight it carried when it
    left.
  - `` `read [turns: [Int]] `` →
    `[[turn: Int, role: Str, messages: [Message]]]`, one element per turn
    named, in transcript order, each addressed by its own `turn` field and
    carrying the `role` it was taken in rather than a `=== … ===` header a
    reader had to re-parse. A turn's material is what the model was sent,
    whether it is resident, departed to this log's file, or an ancestor's
    (`Placed::of` finds its records, `turn_messages` renders them). `!{range a b}` is half-open, so the turns under
    `[from: 41, to: 60]` are `!{range 41 61}`; a residue itself is never
    read, its note being the whole of it.
  - `` `grep [pattern, turns] `` → `[hits: [[from: Int, to: Int, kind: Str,
    role: Str, line: Int, text: Str]], total: Int]`. A Rust regex —
    ral's own `re-*`
    dialect, compiled at the desk so a bad pattern is refused in the regex
    crate's words — over prompts, programs, results, and reasoning, per line,
    and over every model residue's note (`line: 1`; never the residue's
    boilerplate, and a harness residue has nothing to search). A hit names
    the item it lies in, a turn hit being `from == to`.
    `` `turns: `all `` searches every turn but the one in hand and every
    noted residue; `` `only `` narrows it to the turns named and the residues
    whose ids are all named. At
    most `GREP_HITS` (100)
    hits, in `` `index ``'s order, each line clipped at 200 bytes, with `total` the true
    count so a large one says *narrow*, not *page*.

  Only the turn being written *now* is unreadable: the earlier turns of the
  work in hand have closed and read back like any other, which is what the
  unclosed-turn refusal names when it points at them.

  `` `read `` is the one harness answer whose size is the size of the thing it
  describes: the survey spends a few hundred bytes to describe a 200 KB
  context, and this returns the 200 KB. That is why it is a tag of `exarch-transcript`
  and not of `exarch-context` — the distinct name is the cheapest safety mechanism a
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
  `` exarch-tasks `list `` puts querying on the caller rather than the kit
  ([[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]]
  for the transcript-as-value law this reads under: a turn's own recorded
  `Protocol` material, converted rather than re-rendered). `` `grep `` searches that
  same narrowing — `` `text ``,
  `` `program ``'s source, `` `result ``, `` `reasoning ``, a binary payload
  and a provider extension carrying no text a pattern could mean — so nothing
  is searchable that is not readable.

The `exarch-agents` guide documents its operations, grant profiles,
write restrictions with inherited execution, role selection, and fan-out
with literal binding references. `data/agents.md`
keeps point/call guidance for delegation, disjoint write grants, waiting,
reading, follow-ups, and cancellation. Each child's system prompt renders its
own grant, including the parent's restriction. Replying parks a child and starts
its one-hour follow-up lease; working time is outside that lease
([[map/exarch/agent|agent]]).

- **`exarch-agents <tag>`** → `∀α. F α`. One verb for the fleet, over an **open** row
  of six tags — `` `list ``, `` `start ``, `` `message ``, `` `cancel ``,
  `` `reply <value> ``,
  `` `read <name> ``. The acting tags answer `[live: Int, replied: Int]` *afterwards* — other live agents in the reader's tree, and how many of
  its own children hold a deposited reply (reading does not consume it) — rather than a
  receipt of its own. The rule is unchanged (the answer is the world after the
  transition); what changed is its cost, since a roster on every spawn of a
  fan-out is O(fleet) restating what the caller mostly chose, while `replied` is
  the one count it could not derive and the one asking for an action.
  `` `list `` answers the roster rows,
  `[[name: Str, spawner: <root|agent Str>, state: <busy|waiting-on-agents|replied|waiting>, idle-s: Int, elapsed-s: Int, log-dir: Str]]`
  — the reader's whole tree, itself among them, since what it may *message* is
  wider than what it spawned. The climb stops at a root, so one root session
  never lists another's. `` `read `` answers `[name: Str, reply: α]`,
  the value a replied child deposited, which is why the family's answer type is
  a bare `α` (the `` exarch-pins `read `` precedent) rather than a roster
  ([[decisions/260826_reply-parks|reply-parks]]). `` `reply `` is the sole
  return path of a returning agent — first-orderness checked at the door,
  refused on every non-returning agent with the desk's own didactic text, last
  write wins within a run, deposited once the enclosing `ral` batch drains. The
  outer row is open so an unknown tag reaches a door that enumerates the tags; each known
  tag's payload keeps its exact type, so the closed record inside `` `start ``
  still makes a missing or misspelled field a static error naming it, while the
  `type`/`grant`/`model` rows *inside* that record stay open for the
  same reason one level down. Every family door refuses a block or a handle at
  any depth of its argument (`first_order`), so the type variables its argument
  mentions are `data` and its row variables deep (`door` in `harness.rs`;
  [[decisions/260930_operators-are-kinded|operators-are-kinded]]).
  `` `start [prompt: …, name: …, type: …, grant: …, model: …] `` is the one
  spawn: launch-only and always asynchronous, a one-line notice arriving
  through the inbox when the child replies, and the child's row in the answer carrying the `name` and
  `log-dir` the old receipt did. `name` is the child's identity — the tab-bar
  contract (`check_name`, in `enquiry.rs` beside the rule it belongs
  with), unique among live agents or the call is refused; this door refuses a
  malformed one early and in the model's own words, and `Fleet::enrol` refuses it
  again for peers that never came through a door
  ([[map/exarch/agent|agent]]);
  `type` is `` `amnemon `` (blank context) or `` `mnemon `` (imports the
  parent's model-visible context before the fresh final prompt); `grant` is the
  one layer the child gets — `` `inherit ``, one of the four spawnable
  [[map/exarch/policy|base]] names (`confined`, `read-only`, `edit-only`,
  `reasonable`), or `` `restrict R `` with `R` a capability record in
  `grant [...] { body }`'s own vocabulary; the engine reads it off `Launch::split`, and the desk's `Start` carries only the remaining `Spec`. `Grant::decode` (`enquiry.rs`) closes the row,
  naming all six shapes and `R`'s own keys in its refusal and holding `R` to
  first-order data, since the ceiling crosses to the far side as data; the
  record itself is decoded by `decode_capability_map` off the `Form::Grant`
  table when `SpawnGrant::layer` resolves it against the child's cwd, so an
  unknown key is refused in that table's words
  ([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]). There is
  no `search` field: hosted web search is a tool, present in a request or not,
  so the model never reasons about it in prose; a child inherits its parent's
  bit verbatim ([[map/exarch/agent|agent]]). `model` is
  `` `inherit `` or `` `role <Str> ``, and is always written: ral has no
  optional field, so absence is *data* carried by a variant
  ([[invariants/optionality-via-variants|optionality-via-variants]]) and the
  record row stays closed. `` `inherit `` shares the parent's `Arc<Provider>`
  verbatim; `` `role `` takes the provider the human bound to that name at
  launch (`--role NAME=MODEL`, [[map/exarch/provider|provider]]), and is
  refused when none is configured or none bears the name. The agent never
  names a provider or a model, so it cannot discover one. Fuel bounds delegation depth,
  not fan-out — refused only once the caller's own `fuel` reaches zero.
  `` `cancel <name> `` is descendant-only, resolved by name and enforced at
  the desk; a scope violation raises. `` `message [to: …, text: …] `` is not:
  the fleet is one mailbox space, so a note runs in any direction across the
  tree and only a note to oneself refuses. The roster is the fleet to match —
  every live agent, the reader included — so what a model may list and what it
  may message are one set rather than two that drift. Each row's `spawner`
  (`` `root `` for one a human started, `` `agent <name> `` otherwise) carries
  the scope `` `cancel `` and `` `read `` still enforce: the listing states the
  rule it does not impose. Where a
  removed schedule is simply gone from its answer, a cancelled agent is not:
  `Agent::cancel_tree` only sets the cooperative token (and stamps the eval
  reach), so **a successful
  cancel answers with a roster that still lists the target** — a request, not a
  transaction, and the one place the rule needs a sentence of its own.
- **`exarch-schedules <tag>`** → `F [[label: Str, trigger: Str, next-s: Int,
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
- **`exarch-pins <tag>`** → `∀α. F α`. One family over the caller's own pin
  register: `` `set ``/`` `clear `` write it, `` `read ``/`` `list `` read it
  back, and it is the register's only door — with the same
  foreground-only discipline `exarch-agents` and `exarch-context` already have: a call
  inside `spawn "…" { … }` errors rather than degrading.
  - `` `set [key: Str, body: Card] `` → `Unit`.
  - `` `clear <key> `` → `Unit`.
  - `` `read <key> `` → `∀α. F α`. `` `some `` the card pinned at `key`, canonically
    re-encoded ([[map/exarch/cards|cards]]) so a kit can destructure it
    whether or not the bytes it wrote match what comes back; `` `none `` on a miss
    or an absent register. Its answer type is the script's to decide, because the
    register is schemaless by design
    ([[decisions/260803_register-is-read-write|register-is-read-write]]): a boundary,
    admitted at the door against how the script uses the card
    ([[decisions/260930_a-boundary-is-checked-against-its-type|a-boundary-is-checked-against-its-type]]).
  - `` `list `` → `F [String]`. Silent; the keys currently occupied on the
    caller's register, in `BTreeMap` order — a key names a slot for
    `` `read ``, not its content.

Receipts and listings are ral records the model can bind, filter, and fan out
over, rather than stringly-typed JSON it re-parses — the composability a
provider-advertised tool's JSON lacks. Acting verbs render as *acts* — the
`Display::HarnessCall`/`Forensic::HarnessResult` rail pair
([[decisions/260720_harness-calls-are-acts|harness-calls-are-acts]]; a spawn
additionally derives a child tab);
listings stay silent, since their value *is* the returned record.
[[map/exarch/tools|`ral`]] is what remains a tool for all of them; `thinking`
is a second, narration-only tool outside this harness/tool boundary
([[map/exarch/agent|agent]]).

## Where to look

- `exarch/src/shell_eval/builtins.rs` — the Rust
  atoms, their type schemes, and `EXARCH_BUILTINS`.
- `exarch/src/shell_eval/builtins/harness.rs` — the harness verbs above,
  `HARNESS_BUILTINS`.
- `exarch/src/skill.rs` — the skill scan behind
  `skill-list`/`skill`.
- `exarch/src/agent/desk.rs` and `agent/desk/` — `HostServices`, `ExarchDesk`, and the handler
  for each enquiry class.
- `exarch/data/agent.ral` — the helper library; installed by `library::install_agent_library`, which `boot::engine_root` calls
  ([[map/exarch|exarch]] hub).
- The model-facing tool that carries every one of these calls is
  [[map/exarch/tools|`ral`]].
- There is no model-facing network builtin: a guest reaches the network
  itself, policed host-side — [[design/egress|egress]],
  [[map/exarch/agent|agent]], [[map/synod|synod]].
