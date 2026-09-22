The session keeps three tiers of memory: **turns** are the working set the provider is charged for; **bindings, the register, and the goal** outlive every turn; **the transcript** is the complete record — every closed turn, evicted or not. Promote what must survive (`let` it, `exarch-tasks `note` it, `exarch-goal `set` it) and let the record keep whatever you let go.

## Spending turns

`exarch-context `survey` — `rows`, one per resident turn (id, role, kind `own`|`import`|`inherited`, label, bytes), and `total-bytes`, the figure to weigh against the window. `exarch-context `evict [turns: [Int], note: '…']` — the named turns leave at once, wherever they lie, never the one being written; the answer is the survey after it has acted. A run of departed turns leaves one marker behind: which turns went, your `note` verbatim, how to read them back. `!{range a b}` builds the ids.

Evict the moment a detour proves useless, with a note saying what went and why. Near the window the harness cuts the oldest turns itself, noteless, and warns you first: that warning is your chance to bind the conclusions and make the cut yourself. Cost: the cache holds only the prefix before the earliest cut, so the next request re-reads from there on.

## Searching the record

`exarch-transcript `index` — every recorded turn (id, role, kind, label, bytes, held: `resident`|`evicted`), oldest first. `exarch-transcript `grep [pattern: '…', turns: [Int]]` — Rust regex over every message line of the searched turns; `turns` is optional. `exarch-transcript `read [turns: [Int]]` — the messages exactly as the provider was sent them, nothing clipped.

Work outward from `grep`, in on `read`. A hit is [turn, role, line, text] — the ids are the point, read nothing whole until you have them:

    let found = exarch-transcript `grep [pattern: 'test result: FAILED']
    $found[total]                          # 7 — `hits` holds the 100 oldest, each `text` 200 bytes
    let mine = filter { |h| equal $h[role] #'assistant'# } $found[hits]
    map { |h| [$h[turn], $h[line]] } $mine # [[40, 79], [40, 85], ...]

    let turn = exarch-transcript `read [turns: [40]]
    let parts = $turn[0][messages][0][parts]
    let peek = str $parts[0]
    # `text [content: ...] — probe the tags before projecting: `keys` fails on a variant, `str` never fails
    case $parts[0] [ `text: { |c| $c[content] }, `reasoning: { |c| #'(kept)'# } ]

Narrow `grep` to `turns:` once ids are known; `read` is uncapped, so bind and slice it like any large output. A `mnemon` child shares this record and greps it in its own context.

## Rewinding when it fails

There is no undo and no `rewind` — `exarch-context` takes `survey` and `evict`, and the transcript is read-only ("tag must be one of `survey, `evict — got rewind"). Rewinding is a protocol. When a stretch of work fails: bind the conclusion — what failed and what it means — then `evict` the failed turns in one call with that conclusion as the note, `grep` and then `read` whatever you must recover, and resume from the last good state. Bindings, files, and the register are untouched: the cut is to the working set only, and the marker carries your note forward.

Never replay a failed script from the top (Failure); the writes already happened. Refused for you, naming the turn: one being written ("turn N is being written now — an eviction keeps the work in hand"), an id never recorded, and a set that would leave a user turn unanswered. To try work that may fail without dirtying the working set at all, hand it to a child — `mnemon` shares the record, `amnemon` starts clean — and cancel it on failure.