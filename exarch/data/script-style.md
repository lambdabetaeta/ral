# `ral` style guide

A ral tool call should be a small program, not a nervous probe. Gather, transform, and answer in one script when the facts belong together. Ordinary shell one-liners are fine when one fact is all you need — the sin is a *sequence* of turns that could have been one script.

## Define, then query

Capture every query in a `let`, then read from the definition:

    let hits = grep-files #'register_handler'#
    let live = filter { |h| $[not !{re-match #'^tests/'# $h[file]}] } $hits
    let few  = take 5 $live
    [total: !{length $hits}, live: !{length $live}, sample: $few]

In later turns you can reuse all of `hits`, `live` and `few` again — no re-grep, no re-run.

## Define on the second use, not the first

Bind a block when a pattern **recurs**, not speculatively: a helper that is never called again was pure overhead. Inline the first occurrence; when you catch yourself writing it a second time, name it:

    let cargo-in = { |root args| within [dir: $root] { cargo ...$args } }
    cargo-in #'exarch'# [#'test'#, #'--test'#, #'session_apply'#]
    cargo-in #'ral'#    [#'test'#, #'--test'#, #'pipeline'#]

Bindings are immutable but shadowable: re-bind `let cargo-in = …` when the behaviour should change. Blocks capture lexically — earlier blocks keep the helper they captured, so shadowing does not rewrite the past.

## Examples

Survey a symbol: locate, drop tests, deduplicate files, and sample in one turn:

    let hits  = within [dir: #'src'#] { grep-files #'register_handler'# }
    let live  = filter { |h| $[not !{re-match #'^tests/'# $h[file]}] } $hits
    let files = fold { |acc h| if !{elem $h[file] $acc} { $acc } else { [...$acc, $h[file]] } } [] $live
    [total: !{length $hits}, live: !{length $live}, files: $files, sample: !{take 3 $live}]

Grep and read together: a `grep-files` hit carries its file and line; a `view-text-around` over the hit shows the place with the witness `edit-hash` checks. Sample before reading context so output stays bounded:

    let hits = grep-files #'fn parse_'#
    let few  = take 5 $hits
    each { |h| view-text-around $h[file] $h[line] 3; echo #'---'# } $few
    [total: !{length $hits}, shown: !{length $few}]

Overlap slow work: start the suite, read the implementation while it runs, and await when there is nothing left to prepare:

    let suite = defer { cargo test -q 2>&1 }
    let impl  = within [dir: #'src/widget'#] { grep-files #'fn render'# }
    let tests = await $suite
    let log   = lines !{bytes-to-string $tests[value][children][0][stdout]}
    [impl: !{take 4 $impl}, ok: $[$tests[value][status] == 0], log: !{take 20 $log}]

## Long jobs span turns — work while you wait

A command that is simply slow but has nothing to overlap it should run inline with a raised `timeout_secs`, not deferred. Defer is for work you can hide behind other progress.

`defer` long work **early**, then fill the waiting turns with real progress: read the next file, prepare the next edit, write the test you will run afterwards. When nothing useful remains, `await` the handle. It persists across turns, so awaiting it again later is always safe.

Never submit a script that merely sleeps and checks. A turn spent purely waiting has cost the full round trip and bought nothing; if the job is the only work left, `await` it instead of guessing sleep durations.

Long *within* the session and alive *after* it are different problems. `defer` and `service` answer the first, and their threads are meant to end with the process that hosts them. A server that must still answer when a grader, a test, or a person looks once you have finished is `detach`ed instead — handed to the OS, with a receipt naming its pid in place of a handle to await. It is mute once born, so confirm it by probing what it serves rather than by trusting the receipt.

## Leave the tree in a finished state, always

Whatever grades or ships is the **disk**, not your conversation. As soon as a first complete version of an artifact exists — a script, a config, an answer file — write it out, then iterate on it in place:

    to-string $draft > solution.py    # first working version, on disk now
    # …later turns: edit solution.py in place, re-run the checker

Work interrupted at an arbitrary moment should leave behind its best attempt so far, not an empty directory and a plan. Prefer many small improvements to one perfect artifact delivered late.

## Verify proportionately

Run the task's own success criteria (its tests, its checker, its stated acceptance) once, when you believe the work is complete. Between edits, a targeted check of the thing you changed suffices; re-running the full suite after every step burns the clock for reassurance, not information.

## Mock with handlers

`within [handlers: …]` rebinds a command by name for the extent of a block — pin nondeterminism, or dry-run something destructive:

    within [handlers: [date: { |args| echo #'2026-01-01T00:00:00Z'# }]] {
        let stamp = date -u +%Y-%m-%dT%H:%M:%SZ        # always the pinned value
        process-with $stamp
    }

    within [handler: { |name args| echo "[would run: $name !{str $args}]" }] {
        deploy            # prints the plan; runs none of it
    }

Handlers are deep (they hold for nested calls) and self-masking (inside a handler's body, the same name reaches the real command, so wrap-and-forward does not loop).

## Reading large output

An elision in value/stdout/stderr means a command succeeded, but you asked to see too much. Narrow the call:
- Scope the query: Use `within [dir: #'src'#] { grep-files … }`, a tighter glob, or a `filter` before rendering.
- Bind, then slice: `length $hits`, `take 20 $hits`, `filter { … } $hits`. Never echo a large value whole.
- Pre-size files, then read windows: `line-count $f` first; then `view-text $f A B` or `view-text-around $f LINE PEEK`.
- For tests, ask for less: Name the single failing test or capture and slice the final log lines before returning them.
