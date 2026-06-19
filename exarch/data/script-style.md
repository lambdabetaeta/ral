# `ral` style guide

A ral tool call should be a small program, not a nervous probe. Gather, transform, and answer in one script when the facts belong together. Define what you might use again.

## Define, then query

Capture every query in a `let`, then read from the definition:

    let hits = grep-files 'register_handler'
    let live = filter { |h| $[not !{re-match '^tests/' $h[file]}] } $hits
    let few  = take 5 $live
    [total: !{length $hits}, live: !{length $live}, sample: $few]

In later turns you can reuse all of `hits`, `live` and `few` again.

## Define reusable blocks

Define useful functions, such as predicates, mappers, file lists, path filters, that you may re-use again later. For example, `in-src` can be re-used later:

    let in-src = { |h| re-match '^src/' $h[file] }
    let live   = filter $in-src $hits

Define a parameterised block when later calls only vary arguments:

    let cargo-in = { |root args| within [dir: $root] { cargo ...$args } }
    cargo-in 'exarch' ['test', '--test', 'session_apply']
    cargo-in 'exarch' ['test', '--test', 'session_apply', 'queued_prompt_steers_before_next_tool_call']
    cargo-in 'ral'    ['test', '--test', 'pipeline']

## Use records for named knobs

When a helper has several independent options, pass a record and override only the field that changes:

    let inspect-change = { |cfg|
        within [dir: $cfg[root]] {
            let hits  = grep-files $cfg[needle]
            let focus = filter { |h| re-match $cfg[file] $h[file] } $hits
            let shown = take $cfg[limit] $focus
            each { |h| view-around $h[line] $cfg[peek] < $h[file] } $shown
        }
    }

    let cfg = [root: 'exarch', needle: 'ToolCall', file: '^src/', peek: 4, limit: 3]

    inspect-change $cfg
    inspect-change [...$cfg, needle: 'run_shell']

## Pass blocks as policy

When the skeleton is stable but the phase behaviour changes, make the phases explicit block arguments:

    let run-with = { |collect decide act cfg|
        let facts = $collect $cfg
        let plan  = $decide $cfg $facts
        $act $cfg $plan
    }

    let collect = { |cfg| within [dir: $cfg[root]] { grep-files $cfg[needle] } }
    let decide  = { |cfg facts| take $cfg[limit] $facts }
    let act     = { |cfg plan|
        each { |h| view-around $h[line] $cfg[peek] < $h[file] } $plan
    }

    run-with $collect $decide $act [root: 'exarch', needle: 'Session', limit: 5, peek: 3]

Use `$collect`, `$decide`, and `$act` because the helpers are values. Passing helpers explicitly also makes replacement unambiguous: a later call can pass a new `decide` without editing the source of `run-with`.

## Shadow

Bindings are immutable, but shadowable. When behaviour should change, re-bind with a new block:

    let cargo-in = { |root args| within [dir: $root] { cargo ...$args } }

    # Later: quieter default.
    let cargo-in = { |root args| within [dir: $root] { cargo -q ...$args } }

Blocks are lexical and capture environments. If a block uses a helper, e.g. `plan`, shadowing `plan` later DOES NOT ALTER the behaviour of `plan` in previous blocks. Either re-bind the helper, or use higher-order arguments in the first place (as in `run-with` above).

## Long-running tasks

Scripts that run longer than 30s belong in a `spawn { … }`, which run on a worker and return a handle immediately. `await` the handle for a blocking wait: it returns the value the moment the block settles. Should the block outlast 30s, the turn fails but the worker keeps running; await again. Do not busy-wait using `poll`.

For example, stand up a package server and verify it like this:

[turn 1]

    ...
    python -m build               # ≤30s, its own call

[turn 2]

    let srv = spawn { pypi-server run -p 8080 ./pkgs }   # detaches, survives
    python -m venv /tmp/v; /tmp/v/bin/pip install --index-url http://localhost:8080/simple mypkg

[turn 3] 

    /tmp/v/bin/python -c 'import mypkg'   # the server from turn 2 is still up


## Examples

Survey a symbol: locate, drop tests, deduplicate files, and sample in one turn:

    let hits  = within [dir: 'src'] { grep-files 'register_handler' }
    let live  = filter { |h| $[not !{re-match '^tests/' $h[file]}] } $hits
    let files = fold { |acc h| if !{elem $h[file] $acc} { $acc } else { [...$acc, $h[file]] } } [] $live
    [total: !{length $hits}, live: !{length $live}, files: $files, sample: !{take 3 $live}]

Grep and read together: every `grep-files` hit carries its line and witness hash. Sample before reading context so output stays bounded:

    let hits = grep-files 'fn parse_'
    let few  = take 5 $hits
    each { |h| view-around $h[line] 3 < $h[file]; echo '---' } $few
    [total: !{length $hits}, shown: !{length $few}]

Overlap slow work: start the suite, read the implementation while it runs, and use `audit` inside the worker when failure bytes matter:

    let suite = spawn { audit { cargo test -q 2>&1 } }
    let impl  = within [dir: 'src/widget'] { grep-files 'fn render' }
    let tests = await $suite
    let log   = lines !{from-string $tests[value][children][0][stdout]}
    [impl: !{take 4 $impl}, ok: $[$tests[value][status] == 0], log: !{take 20 $log}]

Follow-ups index into the bindings — no re-grep, no re-run.

## Reading large output

An elision in value/stdout/stderr means a command succeeded, but you asked to see too much. Narrow the call:
- Scope the query: Use `within [dir: 'src'] { grep-files … }`, a tighter glob, or a `filter` before rendering.
- Bind, then slice: `length $hits`, `take 20 $hits`, `filter { … } $hits`. Never echo a large value whole.
- Pre-size files, then read windows: `line-count $f` first; then `view A B < $f` or `view-around LINE PEEK < $f`.
- For tests, ask for less: Name the single failing test or capture and slice the final log lines before returning them.

## Writing large files

A script that carries an entire long file in one raw string can exhaust the visible-output budget before the tool call is delivered. For any file beyond about 150 lines, write in chunks. 

In the first turn:

    echo #'<lines 1..N>'# > path/to/file

In the second turn:

    echo #'<next section>'# >> path/to/file

Choose the hash count (e.g. `##'…'##`) so the closing delimiter does not occur in the content.
