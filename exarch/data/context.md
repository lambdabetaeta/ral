Your context is costly, as it is re-sent on every subsequent turn. You have a primitive that you may use to survey and edit it:

    exarch-context `survey / `evict [from: Int, to: Int, note: Str]

You need not call survey often, as every `ral` call ends with `TURN: N` where N is that call's turn id.

An eviction takes one contiguous run of turns, `from` to `to`, both inclusive, and stands in their place as a message carrying your note. The note is required: one line, at most 240 bytes. An eviction is addressed by the ids it covers, so `[from: 41, to: 60]` names turns 41–60 and their eviction.

There are two chief candidates for eviction:
- Turns that retrieved irrelevant information. These may be evicted with a short note on what was retrieved.
- Turns that contained failed tool calls, e.g. with `ral` syntax that was incorrect or did not typecheck. In such cases it is best to retry; once you get it right, evict the failed attempts and their results. The note may be a few words, or may teach the idiom further:

    let commits = [last-commits: git log -5 --oneline]    # TURN: 42
    let commits = [last-commits: !{git log -5 --oneline}] # TURN: 43
    exarch-context `evict [from: 42, to: 42, note: #'a command as a record field needs !{}'#]

You may also habitually evict calls to `explain` near the end of the context with a short note. A prompt may be evicted like any other turn; say in the note what was asked.

Bear in mind that evictions earlier in the transcript may invalidate the cache. You should try to evict turns only at the bottom of the conversation, or in batch whenever you are warned the context is getting large:

    exarch-context `evict [
      from: 31, to: 36,
      note: #'Duplicate check added in parse_options. Regression test needed; checks not run.'#
    ]
    exarch-context `evict [from: 37, to: 49, note: #'Refactor complete and tests passed.'#]
    ...

A run that reaches into an eviction must cover it whole. After the two evictions above, `[from: 35, to: 40]` is refused; `[from: 31, to: 49]` would cover both evictions and replace their notes with yours; `[from: 31, to: 36]`, the same span as the first eviction, would rewrite its note.

If you do nothing once warned, a forced eviction takes the oldest turns at a turn boundary once the context passes its trigger, a reserve beyond the warning, and carries no note. It may take your current prompt too, so when warned, evict under your own note first, saying what was asked.

You may use `exarch-transcript` to search the entire conversation, including evicted turns. `index` lists every turn and every eviction, an eviction before the turns it covers, each saying where it is:

    exarch-transcript `index
    # [..., [from: 31, to: 36, role: #'user'#, kind: #'eviction'#, label: #'Duplicate check added in parse_options. Regression test needed; checks not run.'#, bytes: 417, location: `resident],
    #       [from: 31, to: 31, role: #'user'#, kind: #'own'#, label: #'Reject duplicate flags in parse_options'#, bytes: 1208, location: `evicted [from: 31, to: 36]], ...]

`grep` searches turns and notes alike; a hit in a note names its eviction by `from` and `to`:

    exarch-transcript `grep [pattern: #'duplicate flag'#, turns: `all]
    exarch-transcript `grep [pattern: #'parse_options.*duplicate'#, turns: `only [31, 35]]

You may also read turns back in full. `range` is half-open, so the turns under `[from: 31, to: 36]` are `!{range 31 37}`:

    let old = exarch-transcript `read [turns: !{range 31 37}]

Eviction does not change the state of the shell.
