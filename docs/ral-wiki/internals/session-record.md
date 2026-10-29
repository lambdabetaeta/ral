---
verified_at_commit: 8cc1d0d9
verified_at_date: 2026-10-29
anchors: [Commit, Emitter::commit, Emitter::emit, Sum, Log::append, Log::read, Log::complete_len, Log::record_at, Signal::Commit, Signal::Transient, Record, Protocol, Display, Forensic, ChildStatus, Forensic::Status, Transient, Model::step, Blocks::step, BLOCKS_WINDOW, Delta, Sink::fact, Blocks::model, Selection, replay, model::resume, Mirror::step, Scrollback::commit, export_to, AgentLog::clear, AgentLog::rewind, Context, Turn, Body, Pointer, Residue, SpanRow, Held, Locus, Table::evict, Table::withdraw, Table::frontier, Context::step, read_seeds, render_residue, Context::plan_forced_eviction, Context::resolve_cut, Context::place, Context::rewindable, Unavailable, Emitter::bind, Uncommitted, Fleet::lodge, Fatal]
---

# Session record: one seam, one log

**A session has one durable source: `sessions/<id>/record.jsonl`.** Workers
author facts through one cloneable `record::Emitter`; the model projection, the
scrollback projection, and an exported transcript are folds or presentations
of that record, and a fact with a protocol form is recorded once. The bus is delivery, not a second authority.
This is the
operational path behind the [[map/exarch/frontend|frontend]],
[[map/exarch/cards|cards]], and [[map/exarch/io-surface|io-surface]] maps.

## The seam

**One commit is one sealed line**
([[decisions/261029_a-line-is-a-commit|a-line-is-a-commit]]). `Emitter::commit`
takes `NonEmpty<Record>` and returns its `Commit` or `Unavailable`; `emit`
commits one `Protocol`, `Display`, or `Forensic` record and returns its
`Recorded<Record>`. `Log::append` holds its mutex across:

1. Serialize the records under one timestamp and seal the commit with its `Sum`.
2. Write the one line to the owned `File`.
3. Advance the commit count and byte cursor and build the `Commit`; each
   `Locus` names `(commit, index)`, the line's bytes and sum.
4. Meter usage per record and publish the whole commit as one `Signal::Commit`
   through the attached fleet sink before releasing the lock.

**One `Fatal<Unavailable>` holds the writer's first failure.** It is a leaf lock
beside the append mutex and retains the original `io::Error`; the first
failure wins. Every uncommitted append latches, including encoding failures;
append-lock poison is latched when first met. A durable record refused by
the live fold latches too. Later appends return that same failure before
serialization, writing or publication. Availability observation takes no
append lock; append checks the latch under its lock. Failed appends advance
neither the commit count nor the byte cursor, and the owned `File` has no
user-space buffer to flush on drop.

The latch ends the life node `Fleet::lodge` mints, handed to the writer by
`Emitter::bind`, from whichever thread latches it. No producer reports the
failure. The owner's terminus — `settle`, the headless run or
`SettledConversation::run` — announces it once as
`Transient::Fault` on the independent fleet sink, which the failed writer
cannot block, and makes it the outcome. Announcement precedes the terminal
death cue. A failure first met by `Agent::drop`'s `SessionEnded`, after its
owner concluded, latches but is announced by no one; what was recorded
before it stays resumable. Records are surrendered to the OS, not `fsync`ed:
process-crash durability is not a power-loss guarantee.

Protocol mutations distinguish `Uncommitted::Refused`, which leaves the
session usable without recording, from `Uncommitted::Unavailable`, which
means the writer failed and the session ended.

Append-then-publish makes channel order log order, so a missing or slow
receiver cannot lose the fact: it is already in the file and a later replay
can catch up. The sink is attachable because the session log outlives a TUI
session bus and the headless per-exchange buses that are attached to it in
turn. What a log appends with no sink attached reaches no channel; a mirror
reads it from the file with `Emitter::since`. Redelivery is safe because the
view fold ignores a `Seq` it has already folded: a resumed memo seeded from
the file is not stepped a second time by the same head records.

`Emitter::transient` uses the same log mutex for ordering but never writes a
line or takes a commit number. It publishes the other channel passenger,
`Signal::Transient(AgentId, Transient)`. A `Signal::Commit` is the recorded,
stamped, file-backed half; a `Signal::Transient` is live-only and has no
replay path.

## Three durable classes, one live edge

The outer `Record` vocabulary is closed, and **a fact is recorded in the one
class that can say it.**

- `Protocol` is the provider-facing history, and every input of every request:
  the system prompt (`System`), prompts and steering lines each tagged with the
  `Voice` that spoke them (`Human`, `Wakeup`, `Peer`, `Harness`), a
  harness-authored `Import` opening a turn of its own (the resume note), the
  ancestry link (`Inherited`), assistant messages, tool results, and the context
  edits `Evicted`, `Rewound` and `Cleared`. The model fold consumes this class
  alone and every record in it acts on the fold; the view fold reads it too, for
  the facts the screen draws from it (below).
- `Display` is worker-authored presentation data that has *no* protocol form:
  chopped prose and reasoning, tool calls and each call's `Verdict`, one
  observation per observation, cards, notices, done outcomes, and other committed
  rows. `record/stream.rs` does the chopping
  before the fact reaches the seam; grouping a call's effects is the frontend's,
  derived online ([[map/exarch/io-surface|io-surface]]). Cards and observations
  carry their round-trippable data, not a pre-rendered terminal image; the view
  fold builds the marks again. `Answer`, `Thinking`, `ToolCall` and `Verdict`
  stay here though a protocol record covers the same ground, because each is
  *recorded at a different moment* and so is not its twin: `Answer` and
  `Thinking` land line by line while the model streams, before any
  `AssistantMessage` exists — a cancelled or failed step never commits one, and
  some providers never return reasoning in the message at all, so they are the
  only durable form of what was shown; a ral call's `ToolCall` lands before
  the call executes, and a failed row prevents it. The scrollback groups effects
  under it, where the assistant message lands every call at once; `Verdict`
  lands per call as it finishes, where `ToolResults` lands only when the batch
  ends and carries no
  `failed`. A verdict is a line count and a flag; the result text is
  `ToolResults`' alone.
- `Forensic` is durable evidence that is not model context: the session
  bookends (`SessionStarted`, which names the session and its tree parent,
  `SessionResumed`, `SessionEnded`) and `SessionClosed`, the one record that
  says `/close` ended a session for good, so resume skips it (a record that
  cannot be read at all is an error naming its session, never a silent skip);
  a child's status transitions (`Status`); the shape a turn's request goes
  out under — sampling and routing both, beside the id the request will
  produce (`TurnStarted`), so a retaken request
  shows the same id twice; usage deltas, cancellation, an overflow-learned window (`WindowNarrowed`),
  provider/stall/error rows, nudges, and other breadcrumbs. The view fold
  admits the rows that belong on scrollback; the model fold ignores them.

Byte strings use lossless base64 in JSON. Audit observations preserve the
captured prefix and its retention metadata exactly; replay adds no further
truncation.

`Transient` is deliberately outside `Record`: token and thinking deltas,
state changes, boundaries, child lifecycle, stop reasons, live pins/resources,
and terminal recording failures. These are drawn or routed while the process
is alive. They are not a hidden fourth record class, and no resume attempts
to reconstruct them.

## A child's status is a fold

**A returning agent's latest recorded status is a fold of its own log**
([[decisions/261029_a-child-is-a-status|a-child-is-a-status]]).

- `ChildStatus::fold` yields Working until the first `Forensic::Status`,
  then the last status recorded. The parent reads the shared live cell;
  the fold defines its recorded meaning, not a second live register.
- Parent follow-up and child finish hold `Child.record` across decision,
  record and publication. `Report::transition` appends before showing the
  status, outside every inbox lock; a failed append still shows it, since
  the failure has ended life and the parent must hear the finish.
- A follow-up is posted before its status is recorded: refusal leaves no
  transition. Retirement takes the record seam and permits no later
  transition. A normal Working owner's Failed record precedes `SessionEnded`.
- An unwinding Working owner shows Failed without a status record or
  `SessionEnded`, leaving its log Working. A crash commits no terminal
  status; the last record is the evidence that remains.
- `Forensic::Status` carries the child's reply value; model and presentation
  folds ignore it. The parent records its own notice uptake as
  `Display::SubagentDone`, carrying only the value-free `Settlement`.

## The two folds

`record::Fold` gives both projections one driver. `record::Log::read` reads
one commit at a time and flattens it into its records; `record::replay` calls
the fold's exhaustive `step` for each. A fold sees no commit boundary; a parse
error or unrecognised record is a `Refusal`, never a silent skip.

`Fold::step` takes each record by value, and `replay` hands over the ones it
reads. The model path is `record::Model::step` over `Protocol` records only.
During a live turn, `AgentLog::advance` applies that same step immediately after
the seam returns from `emit`. `AgentLog` authors `Protocol` and its own
bookends and nothing else; every other worker emits through the agent's
`Emitter`.

### One structure, one fold, everything else a projection

The log is the durable structure; `Context` is its fold in memory — the
vocabulary in `record/model.rs`, the turns and residues in `model/table.rs`, the
automaton in `model/state.rs`, the fold in `model/fold.rs`, the render in
`model/render.rs`, the transcript reader in `model/transcript.rs`, resume in
`model/resume.rs` — and it holds **every turn the lineage has recorded, and
where each one is**
([[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]):

- `turns: Vec<Turn>` in id order — `{ id, role, kind, label, bytes, body }` —
  `role` being `user` or `assistant`, and `kind` `own`, `import` or
  `inherited`; rows are never removed; which assistant turns answer a given
  prompt is a function of role and order, held nowhere, and the live prompt —
  the newest user-role turn, `current_prompt` — is what a nudge continues
  ([[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]);
- a turn's `body` is either `Here { records, origin }`, its records in memory,
  or `There { at, withdrawn }`, a `Pointer` to the session and byte ranges that
  hold them: the session by its `AgentId`, resolved against the run's held
  `sessions/` directory, so a moved run still reads back. A turn that left has left one of two ways: by an eviction, which
  puts it *under* a residue, or by a *withdrawal*, which is never drawn.
  Whether a `There` turn is under a residue is a function of `residues`, not
  stored;
- `residues: Vec<Residue>`, one `Residue { span, note, by }` per eviction in
  the order recorded, never removed — exactly what its `Protocol::Evicted {
  residue }` carries: a span of turn ids and the model's note, `None` for the
  harness's own ([[decisions/261019_an-eviction-is-a-span|an-eviction-is-a-span]]);
- `source`, this session's own `AgentId`, and `sessions`, the run's `sessions/`
  directory, held: a turn recorded here points at `source` once it leaves, and
  every pointer's session is read from `sessions/<id>/record.jsonl` through that
  handle; the system prompt in force (`Context::system`, set by
  `Protocol::System`); the protocol's resting state; and the count of records
  folded, beside the index of the newest context edit.

Those first three live in a `Table` private to `model/table.rs`, which hands
the rest of the module a `&[Turn]` and nothing writable. A turn or a residue
therefore changes where it is only through `Table::evict` and
`Table::withdraw`, the two functions that move an item from here to there,
and no row or residue ever leaves the table.

**Residues nest by containment, location is stored, and the context is the
frontier.** A residue *covers* every turn whose id lies in its span, and every
earlier residue whose span lies within its own, an equal span included — so
the same span again rewrites a note. Spans are *laminar*, any two nested or
disjoint, because an eviction's ends must be item boundaries. Hence:

- an item's location is stamped on it as it departs, a residue's as a
  turn's: the table holds each residue as a `Standing { residue, departure }`,
  `departure` saying what a turn's `Body` says without records — `None`
  while resident, else a `Departure`, `Under(span)` or `Withdrawn`, which a
  departed turn's `Body::There` carries too. `Departure` is a `Location`
  that cannot be `Resident`. `Table::evict` stamps `Under` the new span on
  every resident turn *and residue* within it, a rewritten one included,
  and stands the new residue resident; `Table::withdraw` stamps `Withdrawn`
  on every item from its anchor on, whatever held it;
- the stamp cannot go stale. The residue an item departs under is its
  *holder*, the smallest later residue covering it, for good: any residue
  recorded later that covers the item covers that one whole, by laminarity,
  so it is wider, or equal for a rewrite, and an equal span names the same
  holder. Withdrawal dominates and is permanent;
- `Table::frontier` is the resident items in index order — disjoint, so in id
  order. Its first item is user-role by construction: the first
  non-withdrawn turn is a prompt or an import, since nothing leaves without a
  residue standing where it began, or the first item is that residue, a
  user-voice message.

**A span is one type.** `Span { from, to }`, both ends inclusive, has
private fields: `Span::new` refuses `from > to`, and deserialisation goes
through it, so no span anywhere is inverted. It owns `contains`, `within`,
`width` and `hull`, and its `Display` is the one spelling of a span, `turn 12`
or `turns 12–40` (`capitalised` for a sentence's opening). The model's
`` `evict `` input is checked once, where `enquiry::Evict` is decoded; at the
ral surface a span stays the named fields `from` and `to`.

`Turn` and `Body` are private to the module: a turn holds records, so it is
not a value to hand out. Every reader sees an item as one line, a span:
`SpanRow { span, role, kind, label, bytes, location }`, `kind` an `ItemKind`
— `own`, `import`, `inherited` or `residue`. A turn's row has its own id alone;
a residue's has its span, role `user`, its note (or `forced eviction,
without a note`) as label, and its rendered message's weight. Its `Location`
is the item's own stamp, read and never derived (`Item::location`): a
resident item is `Resident`, a withdrawn one `Withdrawn`, and any other
`Under(span)`, its holder's span. `Context::linked` yields one `Linked { row, at }`
per turn — a `TurnRow` and the address of its transcript copy — for a fork's
link. `Residue`, `SpanRow`, `ItemKind`, `Location`, `Pointer` and `Linked` are
the public shapes; `Display::Context` carries `Vec<SpanRow>`.

Everything the model or the human reads off the structure is a pure function
of it, computed on call and memoised nowhere: `rendered`, `history_bytes`,
`context_survey` (the frontier's rows), `transcript_index` (every item, by
`from`, wider span first, later-recorded first, so a residue precedes what it
covers), `plan_forced_eviction`, and the transcript reader's `locate_read` and
`locate_grep`. A read holds only turns; a search holds turns and noted
residues in index order, so `` `grep ``'s hits come in `` `index ``'s order,
and the first hundred kept are the oldest.
`Context::step` is the one
fold — it judges a record before applying it, so a hand-edited or foreign file
is refused by the same function that folds a live one, and a resume has
nothing to compare its result with. `Table::evict` and
`Table::withdraw` are the two places a turn moves from here to there.

**A recorded eviction is a span.** `Protocol::Evicted { residue }` is
admitted when both ends of its span are recorded and not withdrawn, neither end falls
strictly inside a residue — *a run that reaches into a residue must cover it
whole* — and the unclosed turn is not within. `Context::resolve_cut` states
those rules at the writer, each refusal naming the fix, and `judge` states
them again at replay. The span is the resolved identity: the turns it covers
are the recorded ids in it at that point of the log, the residues it covers
the recorded ones within it, and nothing is re-derived from later records.
`Table::evict` moves every resident turn and residue within the span there,
stamped `Under` the span, and pushes the new residue, resident; an item
already departed keeps its stamp. `plan_forced_eviction(keep)`
walks back over the frontier from the newest item, which is never a
candidate, spending `keep`; the first item that does not fit ends the one
span it answers, a prefix from the frontier's first item. The live prompt is
an item like any other, so the span may take it. A prefix that is one residue
alone is no plan — the harness never rewrites a note — while two or more
items, residues included, merge
([[decisions/261019_an-eviction-is-a-span|an-eviction-is-a-span]]).

**Leaving the context is a change of address, not a deletion.** The table sets
each departing turn's body to `There { at, departure }`, where `at` is its `origin`
if it has one and otherwise this session's `source` id beside the `Locus` of every
record it held; the records leave memory and nothing durable is touched. The id,
not a path, is what is stored (and persisted in an `Inherited` link), so a run
directory can be moved and every pointer in it still reads back. Every
read-back through `Log::record_at` checks that the range still holds a line
sealed by the `Sum` its `Locus` carries, *before* parsing records, so a range
that no longer names what it measured — a copied session directory, an edited
log — is
refused as the mismatch it is rather than answered with whatever now lies at
those offsets. The reader reads a turn at a time, so a search never holds more
than one turn, and it holds one descriptor, reopening only where
consecutive turns name different sessions. The log is thereby the model's
transcript as well as its identity
([[decisions/260906_context-rollover|context-rollover]]).

**A turn's transcript copy is where it was first recorded.** A fork's link
(`Protocol::Inherited { turns: Vec<Linked>, residues }`) carries an address for
every row, resident, under a residue or withdrawn, and the parent's residues
by value, and the child records no copy of
the parent's messages: its resident rows are *read back through those
addresses*, each row's `origin` keeping the original copy, so evicting one in
the child points there. The lineage is flattened at each fork — a grandchild's
row already names the grandparent's session — so nothing walks an ancestry, and
`Context::sourced` is the whole of "seek in the structure, then the file".
A residue is rendered from its own record, so the link carries nothing
rendered; the link's judge checks that its residues are laminar and lie
within its ids, and that every row's `location` agrees with them —
`location` is redundant with `residues`, and the judge keeps the redundancy
honest. `Table::install` takes each turn's location from its row. The link
carries no residue's location: `Table::install` derives each as the fold
stamped it, withdrawn with its `from` and otherwise under its `holder`, if
any. Carrying it instead would add a second redundancy for the judge to
check, and `holder` would serve that check anyway; it lives in
`model/table.rs` for these two readings of a link, and the fold never calls
it.

**A link is the one record whose meaning lies in another file.** Its step reads
the parent's records through sum-checked pointers (`read_seeds`, after
`judge` and before any mutation, so a failed read leaves the structure as it
was), keeps the *admissible prefix* of every resident row —
a parent's spawning batch, or an ancestor's turn cut the same way, loses its
unanswered tail, so no dangling `tool_use` reaches the child's wire — and
weighs each row by those messages. That file only ever grows, which is what
makes the pointers safe. A child whose parent's file no longer reads back
refuses with `Refusal::Unreadable`, naming the file; a live fork then fails the
spawn, and a resumed `/branch` root, which inherits from its source session,
needs that session's file to read back.

### Withdrawal: `/clear` and `/rewind`

**A clear and a rewind are one operation: the turns leave the context, leaving
no residue, and stay in the transcript.** `Protocol::Cleared` is
`Table::withdraw(0)` and `Protocol::Rewound { anchor }` is
`Table::withdraw(anchor)`; both keep the system prompt.

- Every turn with `id >= from` becomes `There { at, withdrawn: true }`. A
  resident turn gets the address `Table::evict` would give it; a turn under a
  residue keeps its address and sets the flag. Every residue whose span's
  `from` is at least `from` is withdrawn the same way, so no note survives
  for turns the model is meant to have forgotten.
- **A rewind anchors at an item start.** `Context::rewindable` refuses an
  anchor strictly inside a resident residue's span, naming the residue's
  `from` and the turn after its `to`. Hence a residue's turns are all
  withdrawn or its `from` is not, so withdrawing by `from` takes a residue
  exactly when it takes its turns. A turn under a residue is rewindable only
  at that residue's `from`.
- **A withdrawn turn may lie inside a span.** A rewind leaves withdrawn ids
  mid-sequence and a forced eviction must span them, so only a span's
  ends must be non-withdrawn; a withdrawn turn inside stays `Withdrawn` —
  withdrawal dominates — and the frontier passes over it.
- A withdrawn turn stays readable: `transcript_index`, `` `read `` and
  `` `grep `` walk every turn through its pointer, withdrawn ones included, and
  `` exarch-transcript `` says so.
- **Ids stay lineage-monotone.** The next prompt after either takes `reach + 1`;
  a rewind does not mint `anchor` again. `AgentLog::rewind` refuses an anchor
  already withdrawn, naming the earliest turn still rewindable, and
  `Context::step` refuses a replayed `Rewound`
  of a withdrawn anchor as foreign. `AgentLog::clear` refuses while tool
  results are outstanding. After a clear the automaton rests at its default;
  after a rewind it rests where the newest resident turn below the anchor leaves
  it.
- `unclosed_turn` names the newest row only while it is resident: after a rewind
  that lands past an abandoned exchange the newest row is withdrawn, and no turn
  is being written, so `` `read `` does not refuse it as "being written now".
- A cleared child keeps its parentage: `Cleared` changes the context, not the
  lineage, so a cleared fork resumes as a fork and not as a root.
- Old logs are refused: the reader fails on the first line that is not a
  commit this exarch writes, asking whether an older exarch recorded it.

Withdrawal keeps the law of
[[decisions/260906_context-rollover|context-rollover]]: every closed record is
in the transcript for the life of the log.

### The provider-facing context is a pure function of the structure

`Context::rendered()` builds an owned `Vec<ChatMessage>`, with no render memo
behind it. Assembly is one walk over the frontier: a turn yields its resident
messages — a prompt whose answers were interrupted short of a reply included,
as they lie ([[invariants/turn-ends-ready|exchange-ends-ready]]) — and a
residue its one message, `ChatMessage::user(render_residue(r))`. `history_bytes`
is the same walk over the per-turn sums and each residue's message weight,
with no turn rendered at all, and `context_survey` reports it as
`total-bytes` beside the rows rather than summing them.

`render_residue` is a pure function of the record —
`[EXARCH // Turns 41–60 have left your context.` (singular for one turn, and
`… in a forced eviction, without a note.` for a forced eviction), then `Your note:`
and the note `Debug`-quoted, then the read-back line naming
`` exarch-transcript `read [turns: !{range 41 61}] ``, `` `index `` and
`` `grep `` — no rows, no byte sums, no clock. Its size does not depend on
what it covers. A cut changes the context from its `from` onward, so the
provider re-reads from there; a forced eviction rewrites everything. A
message stays byte-stable between edits not because it is cached but because
it is a function of a structure that did not change. This is the model fold's
recompute invariant taken to its end: there is no memo to read as authority,
and nothing to keep in step.

The price is one build of the context per provider request — once per turn,
never per loop iteration — on top of the clone per HTTP attempt at the wire
request, which is where
[[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]] put
it and where it stays: `manufacture` takes `&[ChatMessage]` inside
`retry_with_backoff`'s per-attempt closure.
[[map/exarch/provider|the provider map]] describes that one place,
`provider/wire.rs`, where those messages become an owned `genai::ChatRequest`.

**The system prompt is a record, not a field.** `Protocol::System { text }` sets
`Context::system`; it is sequencing-neutral (`advance` leaves the state alone
and `admissible` answers `true`), and `into_chat_messages` never renders it, since
the prompt travels in `Request.system`. A round-trip reads the prompt off the
fold, so the log holds every input of every request. A log opens in one fixed
order — `SessionStarted`, then `Inherited` when there is one, then `System` —
which keeps the link the first protocol record of a fork, and a resume records
`System` again only when the freshly built prompt differs from the one in force.
The price is the prompt's size, once per log (about 50 KB), and again on each
change.

**Resume folds before quarantine, so a refusal leaves the file unchanged.**
`record::model::resume` checks the root and folds records through `Model::step`
and `Selection`; `Log::complete_len` answers where whole commits end for reader
and quarantine.

- An unterminated last line, or one with invalid JSON or a failed sum, is
  *torn* and never read; resume quarantines it.
- Earlier damage is refused as *corrupt*.
- Valid JSON of the wrong shape and sealed commits with unreadable records
  are refused as *foreign* wherever they stand.

Read-back is guarded by the sum at the moment of the read. A failed live
append leaves at most one terminal partial line; live catch-up is bounded by
the committed byte cursor. A complete valid line written before a crash but
not yet published is recovered by replay. A missing `record.jsonl` is a named
refusal, not an invitation to start an empty resumed session. `AgentLog::resume` takes
the session id alone: the name is `live_roots`', read off the same head record,
and `live_roots` refuses a session whose record does not parse rather than
hiding it.

**`Selection` is the model in force, folded once.** `SessionStarted` and
`ModelChanged` name the `(model, provider label)` and the window the catalog
listed, clearing any narrower window an overflow taught; `SessionResumed` names
them and keeps what was taught only when the model is unchanged;
`WindowNarrowed` sets the narrowed window. Resume reads `learned()` off it and
the view fold reads `model()` and `window()`, so neither keeps a copy.

The view path is `Blocks::step` over all three classes, with one exhaustive match
over `Protocol`: a `UserPrompt` or `Steering` draws a prompt block iff its
`Voice` is not `Harness` (nudges, reminders, child results and surface notices
are spoken for elsewhere); a forced `Evicted` draws one line naming its
span (`[turns 1–40 left the context (harness)]`), while the model's own is drawn
by its `HarnessCall` act row; `Rewound` truncates the blocks
from its anchor and opens `BlockKind::Rewound`; `Cleared` drops every block and
opens `BlockKind::Cleared`, returning `Delta::Cleared`, while usage, the
selection and the sequences already folded persist; `System`, `Import`,
`Inherited`, `AssistantMessage` and `ToolResults` are `Quiet`. A
`Display::Verdict` patches its call, and `Forensic::TurnStarted { turn }` opens
the turn rule. `Blocks::push` joins consecutive records of one lane
into a block, while a different kind opens the next block. `Block` construction
is private to the fold, and the memo keeps a bounded resident window
(`BLOCKS_WINDOW`) — the one window, for every frontend.

Each step reports what it did as a `Delta`: `Opened` a block, `Grew` the lane
the tail held, `Patched` a call with its verdict, `Rewound` or `Cleared` the blocks, or `Quiet`. A frontend is a
`bus::Sink`: it owns its own `Blocks` memo, steps it through
`Sink::fact(id, rec)` for each record of a commit within one `Sink::accept` —
the witnessed record, since a fold is stepped by the record *and* its locus —
and draws that increment, so a frontend is itself a
fold over the one log, and cannot invent a third block projection: it renders
from `BlockKind` off its memo, never from the record vocabulary
([[decisions/260909_the-fold-reports-the-printer-mirrors|the-fold-reports-the-printer-mirrors]]).
Beside the blocks the memo holds the ambient facts none of them draws:
cumulative usage from the forensic deltas, and the model in force, a
`Selection` stepped by the bookends and `ModelChanged`, so `Blocks::model` is
total from a session's first record and the context floor a frontend stamps
prose with reads both of its terms off the fold rather than being told the
denominator.
The TUI keeps a 1:1 mirror of its memo (`tui::Mirror`), one `tui::Block` per
incident, with deliberation and the work it ordered collapsed into a group as
they land; headless keeps one memo per source agent and prints each opened
block.

Live and replay use the same view fold. The TUI's `App::commit` and
`App::transient` are the two distinct routes: durable blocks are fold-backed,
while an open answer/thinking line or chrome row remains provisional until a
later record or boundary resolves it.

## Resume, clear and export

**Resume and a live tab are one construction.** A tab's `Scrollback` is a
cursor counted in commits: `Scrollback::commit` steps a commit that is next,
ignores one heard twice, and otherwise catches up from the file, so birth, a gap and a
resumed session all read the history exactly as if it had arrived live. A
resumed root's tab reads only the bytes that predate this run and then draws the
"resumed" note at the true boundary. Cumulative usage comes from the replayed
forensic deltas.

**`/clear` prepares, commits, then installs.** Its separately requested
interrupt cancels the in-flight exchange. The owner preflights context
readiness and engine rebirth eligibility, captures a non-destructive inbox
boundary, and boots a replacement without touching the live engine. Only a
committed `Protocol::Cleared` permits discarding old-boundary input,
abandoning work and schedules, replacing the engine and resetting pins and
readings. Refusal or failed preparation leaves those live resources
untouched. A failed commit latches the writer and ends the session;
`Tenancy::discard` certifies the unused prepared engine. The current engine,
record and registers stay as they were. Human input arriving after the
captured boundary survives a successful clear, including a suffix coalesced
with an older prompt; cancelled old producer scopes cannot deliver into the
new context. The inbox boundary is
applied before outgoing engine teardown. No fallible boot remains after the
commit. The frontend learns of it from the record itself: the mirror steps
`Delta::Cleared`, retires every block it held, resets its open lines and scroll
state, redraws the opening, and `App` retires the cleared agent's descendant
tabs. No file is replaced, so existing `Emitter` clones and the attached bus keep
publishing without a handoff.

**A transcript is rendered on demand, not written.** `tui::export_to(record,
dest, format)` is the one place a transcript file is created, and it never
overwrites; both `/export` and `exarch export <session> [--out FILE]` call it.
`export` replays `record.jsonl` through the same `Mirror` the scrollback uses and
hands each block it lets go to a `Render`er — `Plain` forces every block whole,
the script and prose the screen reduces — behind a `Format` seam that admits
HTML later. Because the mirror retires its blocks on a clear, an export keeps the
transcript from before it, then `[cleared]`, then what follows. A torn tail is
never read.

The resulting trust boundary is small: `record.jsonl` is the durable fact
stream, `Record` classes say which fold may project each fact, `Signal::Commit`
delivers whole commits live, and `Transient` carries only process-lifetime
edges.

See [[decisions/261029_a-line-is-a-commit|a-line-is-a-commit]] for the commit
boundary, [[decisions/260814_one-seam-one-log|one-seam-one-log]] for the seam
and fold law, [[decisions/260814_a-trace-is-a-fold|a-trace-is-a-fold]] for the
single durable record, [[decisions/260816_the-window-is-not-the-transcript|the-window-is-not-the-transcript]]
for the window, and
[[decisions/260621_session-lifetime-event-bus|session-lifetime-event-bus]] for
the live bus lifetime. The broader accumulator/fold distinction is in
[[design/residency|residency]], and the visual projection discipline is in
[[decisions/260618_tui-transcript-as-graphic|tui-transcript-as-graphic]].
[[decisions/260827_the-transcript-is-a-value|the-transcript-is-a-value]] is the
ADR for the one wire request, and
[[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]] for the
structure every view above projects — the turn as the atom of eviction, and
the turn's own body as the one statement of where it is, and
[[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]
for the one edit that moves a turn, and
[[decisions/261019_an-eviction-is-a-span|an-eviction-is-a-span]] for the
residue it leaves and the frontier the context is;
[[map/exarch/provider|provider]] covers the one place that turns those messages into an owned wire
request.
