---
generated_at_commit: 3fcae877
generated_at_date: 2026-11-13
covers_paths: [exarch/src/shell_eval/builtins.rs, exarch/src/shell_eval/builtins/, exarch/src/skill.rs, exarch/src/agent/desk.rs, exarch/src/agent/desk/, exarch/src/enquiry.rs, exarch/src/library.rs, exarch/data/agent.ral]
---

# Map: exarch / builtins

**exarch's builtins are one `HostSurface`: Rust atoms that read and edit below
the redirect frame, and harness verbs that enquire of the agent's desk.**
`builtins::host_surface()` carries `EXARCH_BUILTINS` and `HARNESS_BUILTINS`;
`boot::engine_runtime` installs it on either carrier, and `INSTALLER_TAG`
(`"exarch-agent"`) names it in the wire `EngineSpec`. The prompt's builtin index
reads the same surface. Core never inspects these entries
([[internals/builtins-registry|builtins-registry]],
[[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]).

## Line witnesses

A *witness* names a line by its neighbourhood, not its number
([[design/hash-addressed-editing|hash-addressed-editing]]).

- `line_hash`: `h` plus six hex of a Blake3 digest of the line, trailing
  whitespace stripped. The `h` keeps a witness from lexing as an `Int`
  ([[decisions/260824_a-numeral-denotes-its-number|a-numeral-denotes-its-number]]).
- `window_hashes`: each line's witness hashes the smallest symmetric window,
  at least ±`MIN_RADIUS` (5), that no other line shares, folding in the
  radius and the line's offset in the clamped window. Partition refinement
  grows only still-colliding classes. Past ±`MAX_RADIUS` (64) a line is named
  by its index.
- `view-hash` and `edit-hash` both call `window_hashes` over the whole file,
  so a read and the edit after it agree. The model copies witnesses; it never
  computes them.

## Rust atoms: `shell_eval/builtins.rs`

**Each atom is one logical operation with one surface**
([[map/exarch/io-surface|io-surface]]). Reads go through `Shell::locate::<Reading>`;
writes through `Shell::atomic_write`, under the run's [[design/grant|grant]]
([[decisions/260619_surface-reads-writes-execs|surface-reads-writes-execs]]).
Bodies take `&[ValClos]` and return `Settled<ValClos>`.

- *Line endings* (`Eol`): a file is CRLF iff every `\n` follows a `\r`; it is
  read as `\n` text and re-encoded on write. Any other file is taken raw, so
  an unedited round trip is byte-exact.
- `view-text : Str → Int → Int → F [[line: Int, text: Str]]` — the half-open
  range `[start, end)`, 1-based, text verbatim. One `Observed::Read`.
- `view-hash : Str → Int → Int → F [[line: Int, hash: Str, text: Str]]` — the
  same range with witnesses.
- `grep-files : Str → Str → F [[file: Str, line: Int, text: Str]]` — Rust
  regex over a file or a recursive walk (`search_tree`). Beneath `path` the
  walk skips hidden and ignored entries and stays on `path`'s filesystem.
  CRLF-aware, so a hit's `text` is the row `view-text` shows. Binary detection
  quits at NUL. Each file is streamed and every read polls the shell's scope;
  a line past `LONGEST_LINE` (16 MiB) or a failed read skips the file with a
  stderr note. One `Observed::Grep` for the whole search.
- `grep-hidden` — `grep-files` whose walk enters hidden and ignored entries,
  except `.git`.
- `edit-hash : Str → [[hash: Str, line: Str]] → F Unit` — resolves every
  witness against one read before writing. An empty batch, an absent or
  ambiguous witness, or two records on one line fails with the file
  untouched. Replacement text is verbatim: an empty line deletes, a real
  newline splits.
- `edit-replace : Str → Str → Str → F Unit` — replaces the one literal
  occurrence of `from`, counted overlap-aware by core's `occurrence_starts`.
  Several matches name their lines; a miss names a provable mangling (a
  `\n`-style escape, or a line matching but for indentation). An empty `from`
  is refused; it never creates a file.
- Both editors read silently, surface one whole-file diff card, or nothing if
  no line changed ([[map/exarch/cards|cards]]), and note the replaced lines on
  stderr.
- `explore-dir : Int → Str → F [Str]` — entries beneath `path` to depth `n`,
  ignore-aware, the root excluded.
- `find-files : Str → Str → F [Str]` — files under `path`, ranked by core's
  `text::rank`, cut to `FIND_LIMIT` (50). The grant filters before the cut.
- `exarch-surface` — core's `builtin_surface` under exarch's name: puts a card
  or event on the rail.

Every walk starts at `walk_root`, grant-checked; a missing root is an error.
It then runs core's `Shell::admitted_walk`, which admits each entry by its own
path and skips denied or unreadable ones. `grep-files` opens each file through
`locate_if_admitted` ([[map/core/capabilities|capabilities]]).

### Skills: `skill.rs`

- `skill-list : F Str` — `name: description` per valid skill, then one line
  per skipped skill with its place and cause. Fresh scan each call, filtered by
  the grant.
- `skill : Str → F Str` — one skill's `SKILL.md` body. The name is validated
  first, which keeps the path inside its root.
- `skill_roots`, first name wins: `<cwd>/.exarch/skills`, `.agents/skills` from
  cwd up to the working-tree root, `$XDG_CONFIG_HOME/exarch/skills`,
  `~/.agents/skills`, then `skill::BUILTIN` (`exarch-sandbox`, compiled in).
- Boot's prompt section (`discover_metadata`) and both builtins share these
  roots and judge reads by the same grant stack.

## The `agent.ral` kit

`library::install_agent_library` sources `data/agent.ral` through
`extend_session` from `boot::engine_root`; `agent_library_docs` gives `help`
its entries.

- `view-text-around` / `view-hash-around path line peek` — the `2*peek + 1`
  lines centred on `line`, clamped at the top.
- `exarch-tasks` — a task list stored as a card on the `tasks` pin, read by
  `tasks-list` and written by `tasks-sync`. No Rust knows what a task is.
- `exarch-goal` `` `set ``/`` `clear `` — the `goal` pin.

## Harness verbs: `shell_eval/builtins/harness.rs`

**A harness verb decodes the model's argument into a `Request`, enquires, and
admits the answer only in the shape the request owes.**

- `first_order` refuses a block or handle at any depth of the argument, so
  argument rows are deep and type variables `Kind::DATA` (`argument_row`;
  [[decisions/260930_operators-are-kinded|operators-are-kinded]]).
- `ask` checks the answer with `Request::owed`. `exarch-agents`, `exarch-pins`
  and `exarch-transcript` are boundary builtins: their `α:data` answer is
  admitted against the script's use
  ([[decisions/260930_a-boundary-is-checked-against-its-type|a-boundary-is-checked-against-its-type]]).
- Outer tag rows are open, so an unknown tag reaches the decoder, which names
  the legal tags. Record payloads are closed, so a missing or misspelled field
  is a static error. Variant fields inside them (`type`, `grant`, `model`,
  `trigger`) stay open for the same reason.

### The vocabulary: `enquiry.rs`

**One `Datum` codec serves both sides.** `Request` has five classes, each a
`Family` with `CLASS` and `TAGS`: `agents`, `schedules`, `pins`, `context`,
`transcript`. Records decode strictly by key (`record!`). The builtin decodes
so the model sees the decoder's message; the desk decodes again because a guest
may send anything. A cron expression is parsed in that decode
(`CronSchedule::parse`), so on both sides.

### The desk: `agent/desk.rs`, `agent/desk/`

- `Agent::ral` builds `RunHost { desk, apply }` per call. `ExarchDesk` reaches
  the owner's `Body` only through `Arc<Loan<Body>>`; after dispatch returns,
  `Loan::get` fails and enquiries answer `no_desk()`. Workers and stages have
  no desk, so every harness verb fails inside `spawn`.
- `ExarchDesk::handle` decodes, dispatches exhaustively, then syncs the
  record before answering.
- `Call` holds per-call values and the committed `DeskAct`s. `commit_act`
  emits a `Display::HarnessCall` row for every attempt and keeps committed
  acts for `Call::audit`, which tells the model after a failed dispatch what
  already happened
  ([[decisions/260720_harness-calls-are-acts|harness-calls-are-acts]]).
  `DeskAct` is `Spawn`, `Stop`, `Dismiss`, `Message`, `Schedule`,
  `Unschedule`, `Reply`, `ContextEvict`. Listings and reads commit none.

## `exarch-agents`

`<list | start […] | message [to: Str, text: Str] | stop Str | dismiss Str |
reply β | read Str | ρ> → F α` ([[design/agents|agents]],
[[decisions/261029_an-agent-is-a-thread|an-agent-is-a-thread]]).

- `` `list `` — the roster `[[name, spawner, state, idle-s, elapsed-s,
  log-dir]]`, over agents sharing the reader's `founder`.
- Acting tags answer `[live: Int, replied: Int]`: other live agents in the
  tree, and own helpers holding a reply.
- `` `start [prompt, name, type, grant, model] `` — asynchronous; a report
  notice arrives through the mailbox.
  - The builtin decodes a `Launch`, which splits `grant` from the `Spec`.
    It refuses with `no_desk()` unless `Mooring::has_desk`, then calls
    `Shell::marshal(&grant)` and asks `Agents::Start(Start { spec, parent })`.
    The grant crosses inside the `Marshalled` parent; handles cross withheld
    ([[map/core/transport|transport]]).
  - `name`: `check_name` (1–24 ASCII alphanumerics, `-`, `_`) at decode;
    `Registry::name` reserves it at the desk.
  - `type`: `` `amnemon `` (blank context) or `` `mnemon `` (inherits the
    parent's context).
  - `grant`: `` `inherit ``, a `policy::spawn_bases` name (`confined`,
    `read-only`, `edit-only`, `reasonable`), or `` `restrict R ``. `Grant::decode`
    names all six shapes. `SpawnGrant::layer` decodes `R` with the grant
    vocabulary when the child is built
    ([[map/exarch/policy|policy]],
    [[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]).
  - `model`: `` `inherit `` shares the parent's `Arc<Provider>`; `` `role
    <Str> `` takes the provider bound at launch, refused if unknown
    ([[map/exarch/provider|provider]],
    [[invariants/optionality-via-variants|optionality-via-variants]]).
  - The desk refuses at zero `fuel`, builds the engine with `child_engine`
    (`IdentityTransport::child` in process, `make_engine` over the wire),
    calls `Agent::born`, launches with an `Input::Task` and adopts the thread
    ([[map/exarch/agent|agent]]).
- `` `message `` — any other live agent; a resting helper takes only its parent
  or the human. Self-messages are refused.
- `` `stop `` — descendants only; cancels the current exchange, leaving the
  target's helpers running.
- `` `dismiss `` — own helpers only; ends and joins it, keeping failures.
- `` `reply v `` — returning agents only (`Mail::returns`). A bare
  `` `reply `` is `()`. Last write in a call wins; the loop takes it after the
  batch.
- `` `read name `` — `[name, reply]` from the newest own helper of that name,
  live or ended. Refused while `Status::reply_pending`, or for a failed,
  stopped or silent helper. Reading consumes nothing.

## `exarch-schedules`

`<list | add [trigger, label: Str, prompt: Str] | remove Str | ρ> →
F [[label: Str, trigger: Str, next-s: Int, fires: Int]]`.

- Every tag answers the live table. Every tag requires `--allow-schedule`;
  the refusal names the tag the model wrote.
- `trigger` is `` `cron Str `` or `` `after Str ``. `next-s` is zero once
  overdue.
- `` `remove `` of an absent label succeeds; its table lacks the label.

## `exarch-pins`

`<set [key: Str, body: card] | clear Str | read Str | list | ρ> → F α` over the
caller's `Body.pins` ([[design/pins|pins]],
[[decisions/260803_register-is-read-write|register-is-read-write]]).

- `` `set ``/`` `clear `` answer `Unit` and record a pin surface.
- `` `read `` answers `` `some card `` (re-encoded) or `` `none ``.
- `` `list `` answers the occupied keys in order.
- Pins outlive exchanges, are emptied by `/clear`, and are not restored on
  resume.

## `exarch-context` and `exarch-transcript`

**`exarch-context` is what the provider is sent; `exarch-transcript` is
everything the lineage recorded.** Both address *turns* by id; every `ral`
result ends `TURN: <id>`
([[decisions/260906_context-rollover|context-rollover]],
[[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]],
[[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]).

- `exarch-context : <survey | evict [from: Int, to: Int, note: Str] | ρ> →
  F [rows: [[from, to, role, kind, label, bytes]], total-bytes: Int]`. Both
  tags answer the survey, the state the next edit is written against.
  - `` `survey `` — one row per resident turn (`from == to`; `kind` `own`,
    `import` or `inherited`) or eviction (`kind` `eviction`, note as `label`).
    `total-bytes` is the serialized request, not a sum of rows
    ([[decisions/260812_context-is-a-projection|context-is-a-projection]]).
  - `` `evict `` — replaces one contiguous, inclusive run of turns and
    evictions with one eviction carrying the note
    ([[decisions/261019_an-eviction-is-a-span|an-eviction-is-a-span]]).
    A run must cover any eviction it touches whole; the same span rewrites a
    note. Refused: unrecorded or withdrawn ends, a run holding the turn being
    written. `Note` must be nonempty, one line, at most `Note::CAP` (240)
    bytes. Records `Transcript::Evicted` under `DeskAct::ContextEvict`.
- `exarch-transcript : <index | read [turns: [Int]] | grep [pattern: Str,
  turns: <all | only [Int]>] | ρ> → F α`. Read-only.
  - `` `index `` — survey rows over every turn and eviction ever recorded,
    plus `location` (`` `resident ``, `` `evicted [from, to] `` for the
    innermost eviction, `` `withdrawn ``), an eviction before what it covers.
  - `` `read `` — `[[turn: Int, role: Str, messages: [Message]]]`, untruncated,
    wherever the turn lies. The unclosed turn is unreadable.
  - `` `grep `` — `[hits: [[from, to, kind, role, line, text]], total: Int]`.
    The pattern compiles in `Grep::decode`. Searches message text, programs,
    results, reasoning and eviction notes (`line: 1`); `` `only `` reaches a
    note only when it names every id the eviction covers. At most `GREP_HITS`
    (100) hits, text clipped to 200 bytes; `total` counts all.
- A `Part` is one arm per `genai::chat::ContentPart` (`` `text ``,
  `` `program ``, `` `result ``, `` `reasoning ``, `` `binary `` metadata,
  `` `custom ``), matched exhaustively in `record/context/transcript.rs`;
  `ThoughtSignature` is dropped
  ([[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]]).

## Also

- `workers` is core's ([[map/core/builtins|core builtins]]); exarch adds none.
- `ral` is the only model-facing tool that carries these calls
  ([[map/exarch/tools|tools]]).
- No builtin reaches the network; a guest does, policed host-side
  ([[design/egress|egress]], [[map/synod|synod]]).
