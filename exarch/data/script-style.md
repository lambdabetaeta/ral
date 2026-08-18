# `ral` style guide

A ral tool call should be a full program: gather, transform, and compute as much as possible in one script

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

Locate, drop, deduplicate, and sample in one turn:

    let hits  = within [dir: #'src'#] { grep-files #'register_handler'# }
    let live  = filter { |h| $[not !{re-match #'^tests/'# $h[file]}] } $hits
    let files = fold { |acc h| if !{elem $h[file] $acc} { $acc } else { [...$acc, $h[file]] } } [] $live
    [total: !{length $hits}, live: !{length $live}, files: $files, sample: !{take 3 $live}]

`grep-files` hit carries its file and line; a `view-text-around` over the hit shows the place with the witness `edit-hash` checks:

    let hits = grep-files #'fn parse_'#
    let few  = take 5 $hits
    each { |h| view-text-around $h[file] $h[line] 3; echo #'---'# } $few
    [total: !{length $hits}, shown: !{length $few}]

`defer` long work **early**, then fill the waiting turns with real progress: read the next file, prepare the next edit, write the test you will run afterwards. When nothing useful remains, `await` the handle. It persists across turns, so awaiting it again later is always safe.

    let suite = defer { cargo test -q 2>&1 }
    let impl  = within [dir: #'src/widget'#] { grep-files #'fn render'# }
    let tests = await $suite
    let log   = lines !{bytes-to-string $tests[value][children][0][stdout]}
    [impl: !{take 4 $impl}, ok: $[$tests[value][status] == 0], log: !{take 20 $log}]

## Exception: writing files

A static error in a `ral` script aborts it, including the writing of long files. Keep writing files to a single-command script:

    to-string #####'…body, verbatim, at its final indentation…'##### > path/to/file

Use at least five hashes to avoid clashing with notation.

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
- Bind, then slice: `length $hits`, `take 20 $hits`, `filter { … } $hits`. Never echo a large value whole.
- Pre-size files, then read windows: `line-count $f` first; then `view-text $f A B` or `view-text-around $f LINE PEEK`.
- For tests, ask for less: Name the single failing test or capture and slice the final log lines before returning them.

As a general rule capture every stdout you might need to read.
