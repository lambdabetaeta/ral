---
verified_at_commit: fc0c9fbb
verified_at_date: 2026-10-30
anchors: [Commit, Emitter::commit, Emitter::emit, Sum, Log::append, Log::read, Log::complete_len, Log::extent, log::Reader, Reader::record, Signal::Commit, Signal::Transient, Record, Transcript, Control, Display, Forensic, ChildStatus, Control::Status, Transient, Context::step, Blocks::step, Blocks::try_step, Model::narrows, BLOCKS_WINDOW, Delta, Sink::fact, Blocks::model, Model, Mail, Schedules, Folds, Replay, replay, context::resume, Mirror::try_step, Scrollback::commit, export_to, AgentLog::clear, AgentLog::rewind, Context, Turn, Body, Pointer, Eviction, SpanRow, Held, Locus, Table::evict, Table::withdraw, Table::frontier, read_seeds, render_eviction, Context::plan_forced_eviction, Context::resolve_cut, Context::place, Context::rewindable, Unavailable, Emitter::bind, Uncommitted, AgentMailbox, AgentAddress, Control::JobOpened, Control::JobSettled, Forensic::EngineSevered, Forensic::TeardownFailed, Fatal]
---

# Session record: one seam, one log

**A session has one durable source: `sessions/<id>/record.jsonl`.** Workers
author facts through one cloneable `record::Emitter`; the context, shared
agent facts, scrollback and exported transcript are folds or presentations
of that record. A posted input and its later transcript admission are distinct
facts. The bus is delivery, not a second authority.
This is the
operational path behind the [[map/exarch/frontend|frontend]],
[[map/exarch/cards|cards]], and [[map/exarch/io-surface|io-surface]] maps.

## The seam

**One commit is one sealed line**
([[decisions/261029_a-line-is-a-commit|a-line-is-a-commit]]). `Emitter::commit`
takes non-control records and returns their `Commit` or `Unavailable`;
`emit` commits one `Transcript`, `Display`, or `Forensic` record.
`Emitter::begin` and `Emitter::commit_to` are the only doors for control
lines. `begin` appends `SessionStarted` and returns the `Mail`, `Schedules`
and `Model` that `control::seed(head)` makes. Every later line uses
`commit_to(Folds, line, tail)`: append, then step the three borrowed folds; a
failed append steps nothing. `Folds` borrows three mutable references for
one commit. The cell holds that capability under its lock; reopen holds it
before cell construction. `Control` has no `Into<Record>`, and ordinary
`commit` rejects even a hand-built `Record::Control`.
`Log::append` holds its mutex across:

1. Serialize the records under one timestamp and seal the commit with its `Sum`.
2. Write the one line to the owned `File`.
3. Advance the commit count and byte cursor and build the `Commit`; each
   `Locus` names `(commit, index)`, the line's bytes and sum.
4. Meter usage per record and publish the whole commit as one `Signal::Commit`
   through the attached fleet sink before releasing the lock.

**Records are externally tagged, and a line has no depth limit.** `Record`,
`Transcript`, `Control`, `Display` and `Forensic` use serde's default external tagging: an
internally tagged enum buffers through serde's `Content` and replays it on the
native stack, bypassing the stack-grown reader, so a deep reply value would
make its session un-resumable. Reader and writer are both stack-grown
(`serde_stacker`): `seal` serializes through an adapter into a `RawValue` and
never re-parses. The types still internally tagged inside records
(`Settlement`, `DoneOutcome`, `Mark`, diff rows) carry no `FOValue`, so their
depth is bounded by their type. Lines and bodies read as JSON objects alone;
an array is foreign. Reports, schedule fires, mailbox clear and reopen
commit their related control records together on one line.

**One `Fatal<Unavailable>` holds the writer's first failure.** It is a leaf lock
beside the append mutex and retains the original `io::Error`; the first
failure wins. Every uncommitted append latches, including encoding failures;
append-lock poison is latched when first met. A durable record refused by
the live fold latches too. Later appends return that same failure before
serialization, writing or publication. Availability observation takes no
append lock; append checks the latch under its lock. Failed appends advance
neither the commit count nor the byte cursor, and the owned `File` has no
user-space buffer to flush on drop.

The latch ends the life supplied by agent construction and bound through
`Emitter::bind`, from whichever thread latches it. Producers do not announce
it. After owner drop, terminal finalization announces it once as
`Transient::Fault` on the independent fleet sink and includes it in
`Died { failures }` and the join result. This includes failure of the final
`SessionEnded` append; the recorded prefix remains resumable. Records
are surrendered to the OS, not `fsync`ed: process-crash durability is not a
power-loss guarantee.

Transcript mutations distinguish `Uncommitted::Refused`, which leaves the
session usable without recording, from `Uncommitted::Unavailable`, which
means the writer failed and the session ended.

Append-then-publish makes channel order log order, so a missing or slow
receiver cannot lose the fact: it is already in the file and a later replay
can catch up. Each root or helper attaches its log to the host session bus.
What a log appends with no sink attached reaches no channel; a mirror
reads it from the file with `Emitter::since`. Redelivery is safe because the
view fold ignores a `Seq` it has already folded: a resumed memo seeded from
the file is not stepped a second time by the same head records.

`Emitter::transient` uses the same log mutex for ordering but never writes a
line or takes a commit number. It publishes the other channel passenger,
`Signal::Transient(AgentId, Transient)`. A `Signal::Commit` is the recorded,
stamped, file-backed half; a `Signal::Transient` is live-only and has no
replay path.

## Four durable classes, one live edge

The outer `Record` vocabulary is closed, and **a fact is recorded in the one
class that can say it.**

- `Transcript` is the provider-facing history, and every input of every request:
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
- `Control` is the agent's recorded state: the head `SessionStarted`,
  `SessionResumed`, jobs, status, posts taken or withdrawn, schedule bookings
  and retirements, model changes and learned windows. `Mail`, `Schedules` and
  `Model` fold it under the cell's lock, not in the writer or owner body; no
  type stands over the three. `Transcript` is the class `Context` folds, and
  `Selected` is nested in records under `provider`. Old log shapes are
  refused, not migrated.
- `Forensic` is evidence, not state: `SessionEnded`, `SessionClosed`, request
  shape (`TurnStarted`, including tuning and route), usage, cancellation,
  provider/stall/error rows, nudges and diagnostics. No agent-state fold
  reads it. `live_roots` scans permanent closure directly; an unreadable
  session is an error naming it, never a silent skip. The view fold admits
  the rows that belong on scrollback.

Byte strings use lossless base64 in JSON. Audit observations preserve the
captured prefix and its retention metadata exactly; replay adds no further
truncation.

`Transient` is deliberately outside `Record`: token and thinking deltas,
state changes, boundaries, child lifecycle, stop reasons, live pins/resources,
and terminal recording failures. These are drawn or routed while the process
is alive. They are not a hidden fifth record class, and no resume attempts
to reconstruct them.

## Jobs, reports and teardown evidence

**The cell holds `Mail`, `Schedules`, `Model` and queued payloads under one
lock; the owner holds `Context` and resources without a lock.** Transactions
are closures, never escaping guards. Poison latches the journal and ends life;
debug builds detect re-entry. Only reporting nests locks, descendant before
its fixed ancestor; journal and bus channel follow. No transaction spans a
request, dispatch, join or `Context` access.

- `Control::JobOpened`'s commit number names the mail's `JobId`.
  `JobSettled { job, stopped }` closes it under the lock that finds no input,
  pending helper or returning-agent booking. Stop marks the job and cancels
  exchange under that lock; human gestures also commit `Withdrawn` text.
- Each accepted input or command commits `Posted(Post)` and queues its
  payload. The commit number names `PostId`. `next` and `drain` commit
  `Taken` before handing payloads over; recall and clear commit `Withdrawn`.
  At every live-cell unlock, queued posts equal `mail.pending()` in order.
  `Gone` is unrecorded join plumbing and does not split a human post run.
- `Mail::owes()` derives debt from taken-but-unanswered parent input or
  pending parent posts, gated by `returns`. A helper owes from birth; a
  returning root owes at human post time. `Taken` keeps debt, `Withdrawn`
  takes unread debt away. Human input to helpers and peer messages add none.
- `Txn::report` commits `Control::Status`, all `Unscheduled`, and withdrawal
  of queued wakeups (all pending posts after life ends). Still holding the
  helper cell, it posts `Child` to its parent iff it owed. Observing paid
  debt cannot overtake delivery. An ended reported helper owes nothing.
- `Mail::status()` is `Replied(FOValue)`, `Failed(String)` or `Stopped`.
  Working is a view of debt or open job, not a status variant.
  Parent uptake records value-free `Display::SubagentDone`; `read` fetches
  the faithful value from mail, including after departure. A poisoned helper
  reports `Failed` directly to its parent through its surviving handle.
- Timers only ring, recording nothing. The owner answers rings before uptake
  and each drain, committing a coalesced wakeup plus booking update or
  retirement. Reports and clear retire bookings before dropping timers;
  late rings cannot post stale wakeups. `Schedules` is the authority, the
  owner's unlocked timer registry its resource.
- `EngineSevered` records engine loss before intentional end;
  `TeardownFailed` records uncertified retirement once in its owner's log.
  Joins propagate failures without re-recording them. `Option<Engine>` makes
  retirement consumptive. These remain forensic, not control state.

See [[design/agents|agents]] for F1–F6 and the cell's rules, and
[[decisions/261029_an-agent-is-a-thread|an-agent-is-a-thread]] for joins.

## Context, control and view folds

`record::Fold` drives context and view projections; `control::Replay`
owns the three shared folds for a file read and rebuilds them from the head:
seed from `SessionStarted`, step every later `Control`, refuse a second head or
a transition the live fold would refuse. Replay equals live (F6). `record::Log::read` reads
one commit at a time and flattens it into its records; `record::replay` calls
the fold's exhaustive `step` for each. A fold sees no commit boundary; a parse
error or unrecognised record is a `Refusal`, never a silent skip.

`Fold::step` takes each record by value, and `replay` hands over the ones it
reads. The context path is `Context::step` over `Transcript` records only.
During a live turn, `AgentLog::advance` applies that same step immediately after
the seam returns from `emit`. `AgentLog` authors transcript changes and
bookends, seeds control folds at birth and reconciles them at reopen. Live control operations belong to
the cell; other workers emit non-control facts through the same seam.

### One structure, one fold, everything else a projection

The log is the durable structure; `Context` is its fold in memory — the
vocabulary in `record/context.rs`, the turns and evictions in `context/table.rs`, the
automaton in `context/state.rs`, the fold in `context/fold.rs`, the render in
`context/render.rs`, the transcript reader in `context/transcript.rs`, resume in
`context/resume.rs` — and it holds **every turn the lineage has recorded, and
where each one is**
([[decisions/260907_the-turn-is-the-atom|the-turn-is-the-atom]]):

- `turns: Vec<Turn>` in id order — `{ id, role, kind, label, bytes, body }` —
  `role` being `user` or `assistant`, and `kind` `own`, `import` or
  `inherited`; rows are never removed; which assistant turns answer a given
  prompt is a function of role and order, held nowhere, and the live prompt —
  the newest user-role turn, `current_prompt` — is what a nudge continues
  ([[decisions/260917_an-eviction-is-a-set-of-turns|an-eviction-is-a-set-of-turns]]);
- a turn's `body` has three shapes: `Here { records, origin }`,
  `Evicted { at: Pointer, under: Span }`, or `Withdrawn { at: Pointer }`.
  A pointer names the session by `AgentId` and the records by their loci,
  resolved against the run's held `sessions/` directory. Evicted turns keep
  their smallest covering eviction's span; withdrawal dominates;
- `evictions: Vec<(Eviction, Location)>`, in recorded order, never removed.
  Each `Eviction { span, note, by }` is what `Transcript::Evicted(Eviction)`
  carries; its note is absent for the harness's own. An eviction can itself
  be evicted, so the pair keeps its full location;

- `source`, this session's own `AgentId`, and `sessions`, the run's `sessions/`
  directory, held: a turn recorded here points at `source` once it leaves, and
  every pointer's session is read from `sessions/<id>/record.jsonl` through that
  handle; the system prompt in force (`Context::system`, set by
  `Transcript::System`); the protocol's resting state; and the count of records
  folded, beside the index of the newest context edit.

Those first three live in a `Table` private to `context/table.rs`, which hands
the rest of the module read-only turns and eviction items. A turn or an eviction
therefore changes where it is only through `Table::evict` and
`Table::withdraw`, the two functions that move an item from here to there,
and no row or eviction ever leaves the table.

**Evictions nest by containment, location is stored, and the context is the
frontier.** An eviction *covers* every turn whose id lies in its span, and every
earlier eviction whose span lies within its own, an equal span included — so
the same span again rewrites a note. Spans are *laminar*, any two nested or
disjoint, because an eviction's ends must be item boundaries. Hence:

- an item's location is stamped on departure. A turn's `Body` says where
  its records are; an eviction's table pair says `Resident`, `Evicted(span)`
  or `Withdrawn`. `Table::evict` stamps `Evicted` on every resident turn and
  eviction within the new span and installs the new eviction resident;
  `Table::withdraw` stamps `Withdrawn` from its anchor on, whatever held it;

- the stamp cannot go stale. The eviction an item departs under is its
  *holder*, the smallest later eviction covering it, for good: any eviction
  recorded later that covers the item covers that one whole, by laminarity,
  so it is wider, or equal for a rewrite, and an equal span names the same
  holder. Withdrawal dominates and is permanent;
- `Table::frontier` is the resident items in index order — disjoint, so in id
  order. Its first item is user-role by construction: the first
  non-withdrawn turn is a prompt or an import, since nothing leaves without an
  eviction standing where it began, or the first item is that eviction, a
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
— `own`, `import`, `inherited` or `eviction`. A turn's row has its own id alone;
an eviction's has its span, role `user`, its note (or `forced eviction,
without a note`) as label, and its rendered message's weight. Its `Location`
is the item's own stamp, read and never derived (`Item::location`): a
resident item is `Resident`, a withdrawn one `Withdrawn`, and any other
`Evicted(span)`, its holder's span. `Context::linked` yields one `Linked { row, at }`
per turn — a `TurnRow` and the address of its transcript copy — for a fork's
link. `Eviction`, `SpanRow`, `ItemKind`, `Location`, `Pointer` and `Linked` are
the public shapes; `Display::Context` carries `Vec<SpanRow>`.

Everything the model or the human reads off the structure is a pure function
of it, computed on call and memoised nowhere: `rendered`, `history_bytes`,
`context_survey` (the frontier's rows), `transcript_index` (every item, by
`from`, wider span first, later-recorded first, so an eviction precedes what it
covers), `plan_forced_eviction`, and the transcript reader's `locate_read` and
`locate_grep`. A read holds only turns; a search holds turns and noted
evictions in index order, so `` `grep ``'s hits come in `` `index ``'s order,
and the first hundred kept are the oldest.
`Context::step` is the one
fold — it judges a record before applying it, so a hand-edited or foreign file
is refused by the same function that folds a live one, and a resume has
nothing to compare its result with. `Table::evict` and
`Table::withdraw` are the two places a turn moves from here to there.

**A recorded eviction is a span.** `Transcript::Evicted(Eviction)` is
admitted when both ends of its span are recorded and not withdrawn, neither end falls
strictly inside an eviction — *a run that reaches into an eviction must cover it
whole* — and the unclosed turn is not within. `Context::resolve_cut` states
those rules at the writer, each refusal naming the fix, and `judge` states
them again at replay. The span is the resolved identity: the turns it covers
are the recorded ids in it at that point of the log, the evictions it covers
the recorded ones within it, and nothing is re-derived from later records.
`Table::evict` moves every resident turn and eviction within the span there,
stamped `Location::Evicted(span)`, and pushes the new eviction, resident; an item
already departed keeps its stamp. `plan_forced_eviction(keep)`
walks back over the frontier from the newest item, which is never a
candidate, spending `keep`; the first item that does not fit ends the one
span it answers, a prefix from the frontier's first item. The live prompt is
an item like any other, so the span may take it. Only material items count
for progress: an eviction is material, a turn
only if its `Body::Here` contains records. An empty inherited turn is not
material, regardless of its weight. A prefix with no material or just one
material eviction is no plan — the harness never rewrites a lone note —
while ordinary material or two evictions can merge
([[decisions/261019_an-eviction-is-a-span|an-eviction-is-a-span]]).

**Leaving the context is a change of address, not a deletion.** The table sets
each departing turn's body to `Evicted { at, under }` or `Withdrawn { at }`,
where `at` is its `origin`
if it has one and otherwise this session's `source` id beside the `Locus` of every
record it held; the records leave memory and nothing durable is touched. The id,
not a path, is what is stored (and persisted in an `Inherited` link), so a run
directory can be moved and every pointer in it still reads back. Every
read-back through `log::Reader` (`Reader::record`) checks that the range still
holds a line sealed by the `Sum` its `Locus` carries, *before* parsing records,
and parses each addressed line once across consecutive loci, so a range
that no longer names what it measured — a copied session directory, an edited
log — is
refused as the mismatch it is rather than answered with whatever now lies at
those offsets. The reader reads a turn at a time, so a search never holds more
than one turn, and it holds one descriptor, reopening only where
consecutive turns name different sessions. A binary part is metadata only:
its byte estimate is exact for canonical padded base64, floored at zero
when malformed, and its payload is never searched by grep. The log is
thereby the model's
transcript as well as its identity
([[decisions/260906_context-rollover|context-rollover]]).

**A turn's transcript copy is where it was first recorded.** A fork's link
(`Transcript::Inherited { turns: Vec<Linked>, evictions }`) carries an address for
every row, resident, under an eviction or withdrawn, and the parent's evictions
by value, and the child records no copy of
the parent's messages: its resident rows are *read back through those
addresses*, each row's `origin` keeping the original copy, so evicting one in
the child points there. The lineage is flattened at each fork — a grandchild's
row already names the grandparent's session — so nothing walks an ancestry, and
`Context::sourced` is the whole of "seek in the structure, then the file".
An eviction is rendered from its own record, so the link carries nothing
rendered; the link's judge checks that its evictions are laminar and lie
within its ids, and that every row's `location` agrees with them —
`location` is redundant with `evictions`, and the judge keeps the redundancy
honest. `Table::install` takes each turn's location from its row. The link
carries no eviction's location: `Table::install` derives each as the fold
stamped it, withdrawn with its `from` and otherwise under its `holder`, if
any. Carrying it instead would add a second redundancy for the judge to
check, and `holder` would serve that check anyway; it lives in
`context/table.rs` for these two readings of a link, and the fold never calls
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
no eviction, and stay in the transcript.** `Transcript::Cleared` is
`Table::withdraw(0)` and `Transcript::Rewound { anchor }` is
`Table::withdraw(anchor)`; both keep the system prompt.

- Every turn with `id >= from` becomes `Withdrawn { at }`. A
  resident turn gets the address `Table::evict` would give it; a turn under an
  eviction keeps its address. Every eviction whose span's
  `from` is at least `from` is withdrawn the same way, so no note survives
  for turns the model is meant to have forgotten.
- **A rewind anchors at an item start.** `Context::rewindable` refuses an
  anchor strictly inside a resident eviction's span, naming the eviction's
  `from` and the turn after its `to`. Hence an eviction's turns are all
  withdrawn or its `from` is not, so withdrawing by `from` takes an eviction
  exactly when it takes its turns. A turn under an eviction is rewindable only
  at that eviction's `from`.
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
as they lie ([[invariants/turn-ends-ready|exchange-ends-ready]]) — and an
eviction its one message, `ChatMessage::user(render_eviction(r))`. `history_bytes`
is the same walk over the per-turn sums and each eviction's message weight,
with no turn rendered at all, and `context_survey` reports it as
`total-bytes` beside the rows rather than summing them.

`render_eviction` is a pure function of the record —
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

**The system prompt is a record, not a field.** `Transcript::System { text }` sets
`Context::system`; it is sequencing-neutral (`advance` leaves the state alone
and `admissible` answers `true`), and `into_chat_messages` never renders it, since
the prompt travels in `Request.system`. A round-trip reads the prompt off the
fold, so the log holds every input of every request. A log opens in one fixed
order — `SessionStarted`, then `Inherited` when there is one, then `System` —
which keeps the link the first transcript record of a fork, and a resume records
`System` again only when the freshly built prompt differs from the one in force.
The price is the prompt's size, once per log (about 50 KB), and again on each
change.

**Resume folds before quarantine, so a refusal leaves the file unchanged.**
`record::context::resume` checks the head and folds `Context` and `control::Replay` in one
read-only pass; `Log::complete_len` answers where whole commits end for reader
and quarantine.

- An unterminated last line, or one with invalid JSON or a failed sum, is
  *torn* and never read; resume quarantines it.
- Earlier damage is refused as *corrupt*.
- Well-formed JSON objects of the wrong shape and sealed commits with
  unreadable records are refused as *foreign* wherever they stand. A shape
  error is re-checked for well-formedness: a line that is not well-formed is
  damage, and a well-formed `{sum, commit}` whose sum is not 16 lowercase hex
  digits is a failed seal, also damage. A bit-flipped key stays foreign.
- `Log::append_to` refuses a log whose tail is not a whole sealed commit
  (`Log::extent`), naming the torn byte range: resume quarantines first.

Read-back is guarded by the sum at the moment of the read. A failed live
append leaves at most one terminal partial line; live catch-up is bounded by
the committed byte cursor. A complete valid line written before a crash but
not yet published is recovered by replay. A missing `record.jsonl` is a named
refusal, not an invitation to start an empty resumed session. `AgentLog::reopen` takes
the session id, launch choice, return role, system prompt and time. The name
is `live_roots`', read off the same head record,
and `live_roots` refuses a session whose record does not parse rather than
hiding it.

**Reopen records the state it cannot revive.** It is one pass and one resume
line: the single read-only fold above, then the commits below. After quiescing the context,
`AgentLog::reopen` commits `SessionResumed { selected, returns, at_unix_ms }`,
`Withdrawn` for every pending post, `JobSettled` for an open job and every
`Unscheduled`, with an optional forensic `SystemNote` naming withdrawn inputs.
Then `System` is recorded if changed. The returned folds have no posts, job
or bookings. Debt for what was taken but not answered survives; debt for
withdrawn posts does not. No helpers, shell workers or timer resources revive.

**`Model` is the model in force, folded once.** The head seeds it from
`provider::Selected`; `ModelChanged` installs a choice and clears narrowing.
`SessionResumed` installs the launch choice, retaining narrowing only for the
same model name. `WindowNarrowed` must name that model and tighten its window; `Model::narrows`
is that one rule, used by `Txn::narrow` and the fold alike. Keying by name,
not account and route, is a deliberate approximation.
The effective window is the minimum of catalog and narrowing. The cell is the
agent's authority; the view folds its own model from the same records. Clients
are immutable resources and one agent's overflow cannot narrow another's.

The view path is `Blocks::try_step` over all four classes (`Blocks::step` is its
live form and panics on refusal, as every live fold refusal is an exarch bug); control
records are judged by `Model`'s own step. One exhaustive match
over `Transcript`: a `UserPrompt` or `Steering` draws a prompt block iff its
`Voice` is not `Harness` (nudges, reminders, child results and surface notices
are spoken for elsewhere); a forced `Evicted` draws one line naming its
span (`[turns 1–40 left the context (harness)]`), while the model's own is drawn
by its `HarnessCall` act row; `Rewound` truncates the blocks
from its anchor and opens `BlockKind::Rewound`; `Cleared` drops every block and
opens `BlockKind::Cleared`, returning `Delta::Cleared`, while usage, the
model and the sequences already folded persist; `System`, `Import`,
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
`Model` seeded by the head and stepped by control records, so `Blocks::model` is
total from a session's first record and the context floor a frontend stamps
prose with reads both of its terms off the fold rather than being told the
denominator.
The TUI keeps a 1:1 mirror of its memo (`tui::Mirror`), one `tui::Block` per
incident, with deliberation and the work it ordered collapsed into a group as
they land; headless keeps one root memo and prints its opened blocks.

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

**`/clear` prepares, joins helpers, commits, then installs.** Its queued
rewrite runs at the exchange boundary. Context readiness and engine rebirth
are checked before mutation; preparation failure preserves helpers, queue
and engine. Then the owner dismisses and joins helpers and commits
`Transcript::Cleared`. A failed commit ends life through the writer latch and
retires the unused prepared engine. Success transfers the life binding to
the fresh engine, replaces and retires the outgoing engine once, advances
batch generation and resets pins, timers and readings. `Txn::clear`
withdraws queued reports, wakeups and surfaces and retires every booking in
one commit; it drops `Gone` notices already joined. Human text, messages and
commands remain. Generation checks reject outgoing-engine surfaces; retired
timers cannot post after clear.
No fallible boot remains after the commit. The mirror learns from the record,
retires blocks and resets open lines/scroll state; descendant tabs retire
through their death signals. The log file and attached bus stay in place.

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
eviction it leaves and the frontier the context is;
[[map/exarch/provider|provider]] covers the one place that turns those messages into an owned wire
request.
