Keep requirements and current work in context; recover discarded evidence from the transcript. Every result ends with `TURN: N`: N is that call's turn id. Ids below are placeholders.

    exarch-context `survey / `evict [turns: [Int], note: `none|`some Str]

**Evict each failed call in the next one, alongside the correction.** Failed means a diagnostic, a failing command, or not what you meant. Kept, it is re-sent on every request; the newest turn costs almost nothing in cache to cut. Note the lesson and side effects. Earlier writes and spawns survive: resume, don't replay.

    exarch-context `evict [turns: [42], note: `some #'from-lines ends a byte pipeline. No file changes.'#]
    let commits = git log -5 --oneline | from-lines

**Batch completed work into one cut.** Keep conclusions and next steps in the note; name a binding for longer material. Each cut breaks cache reuse from the earliest changed turn. On a pressure warning, cut before the harness evicts without a note.

    exarch-context `evict [
      turns: !{range 31 37},
      note: `some #'Duplicate check added in parse_options. Regression test needed; checks not run.'#
    ]

**Evict spent `explain` results when unlikely to be needed soon.** Fetch them again later.

    exarch-context `evict [turns: [48], note: `none]

**Search before reading history.** If `total` exceeds the returned hits, narrow the pattern or turn ids; matches are oldest first.

    exarch-transcript `grep [pattern: #'duplicate flag'#, turns: `all]

    exarch-transcript `grep [pattern: #'parse_options.*duplicate'#, turns: `only [31, 35]]

**Use the index when you lack a search term.** It lists labels and sizes.

    exarch-transcript `index

**Bind large reads; inspect only relevant messages or parts.** Reads are unbounded. Check current files before editing from historical evidence.

    let old-turn = exarch-transcript `read [turns: [35]]

**Look up limits and refusal rules as needed:** `explain exarch-context`, `explain exarch-transcript`.
