Your context is the turns the provider is sent on each request; it is the only memory that costs you, and the only memory that can be taken away. Bindings, files, the task list and the goal survive every eviction. The transcript keeps every turn, evicted or not. So before letting a turn go, promote what it established: `let` it, `` exarch-tasks `note `` it, or `` exarch-goal `set `` it.

    exarch-context `survey / `evict [turns: [Int], note: Str]
    exarch-transcript `index / `grep [pattern: Str, turns: [Int]] / `read [turns: [Int]]

`explain exarch-context` and `explain exarch-transcript` give the full shapes and rules.

## Evicting

Evict a detour the moment it proves useless, with a note saying what it found. The note is kept verbatim in the marker left behind, so it is the only part of those turns you keep without reading them back. `!{range a b}` builds a run of ids; weigh `total-bytes` from `survey` against the window.

Near the window the harness evicts the oldest turns itself, without a note, and warns you once beforehand. Take that warning as your cue: bind the conclusions and make the cut yourself. Every eviction costs a cache miss from the earliest evicted turn onward, so cut once, not piecemeal.

## Searching

Find ids with `grep`, then `read` only those turns. A hit is `[turn, role, line, text]`; `hits` holds the 100 oldest matches, each clipped to 200 bytes, and `total` counts them all.

    let found = exarch-transcript `grep [pattern: 'test result: FAILED']
    let mine = filter { |h| equal $h[role] 'assistant' } $found[hits]
    map { |h| [$h[turn], $h[line]] } $mine     # [[40, 79], [40, 85], ...]

    let turns = exarch-transcript `read [turns: [40]]
    str $turns[0][messages][1][parts]          # parts are variants: `str` shows the tags, `keys` fails

Once ids are known, narrow `grep` with `turns:`. `read` is not clipped, so bind and slice it like any large output. A `mnemon` child shares this record and can search it in its own context.

## Recovering from failure

When a script fails — a type error, a parse error, or it simply did not do what you meant — evict its turn in your next script, alongside the corrected attempt. Every tool result ends with `TURN: <id>`, the turn to name; the note says what you learned, if anything, and what the script wrote before it failed. A failed attempt is noise to every later request, and cutting the newest turn costs almost nothing in cache.

There is no undo. When a stretch of work fails, bind the conclusion — what failed and what it means — then evict the failed turns in one call with that conclusion as the note, and resume from the last good state. Never replay a failed script from the top: its writes already happened. To try work that may fail without touching your context at all, hand it to a child — `mnemon` shares the record, `amnemon` starts clean — and cancel it if it fails.
