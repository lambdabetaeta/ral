Your context is costly, as it is re-sent on every subsequent turn. You have a primitive that you may use to survey and edit it:

    exarch-context `survey / `evict [turns: [Int], note: `none|`some Str]

You need not call survey often, as every `ral` call ends with `TURN: N` where N is that call's turn id.

There are two chief candidates for eviction:
- Turns that retrieved irrelevant information. These may be evicted with a short note on what was retrieved.
- Turns that contained failed tool calls, e.g. with `ral` syntax that was incorrect or did not typecheck. In such cases it is best to retry; once you get it right, evict the failed attempts and their results. The note may be `` `none ``, or may teach the idiom further:

    let commits = [last-commits: git log -5 --oneline]    # TURN: 42
    let commits = [last-commits: !{git log -5 --oneline}] # TURN: 43
    exarch-context `evict [turns: [42], note: `some #'a command as a record field needs !{}'#]

You may also habitually evict calls to `explain` near the end of the context with `` `none `` or a short note.

Bear in mind that evictions earlier in the transcript may invalidate the cache. You should try to evict turns only at the bottom of the conversation, or in batch whenever you are warned the context is getting large:

    exarch-context `evict [
      turns: !{range 31 37},
      note: `some #'Duplicate check added in parse_options. Regression test needed; checks not run.'#
    ]
    exarch-context `evict [
      turns: !{range 37 50},
      note: `some #'Refactor complete and tests passed.'#
    ]
    ...

You may use `exarch-transcript` to search the entire conversation, including evicted turns:

    exarch-transcript `grep [pattern: #'duplicate flag'#, turns: `all]
    exarch-transcript `grep [pattern: #'parse_options.*duplicate'#, turns: `only [31, 35]]

You may also read turns back in full:

    let old = exarch-transcript `read [turns: [31, 35]]

Eviction does not change the state of the shell.
