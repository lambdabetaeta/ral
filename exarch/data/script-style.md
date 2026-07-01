# `ral` style guide

A ral tool call should be a small program, not a nervous probe. Gather, transform, and answer in one script when the facts belong together. Define what you might use again.

## Define, then query

Capture every query in a `let`, then read from the definition:

    let hits = grep-files #'register_handler'#
    let live = filter { |h| $[not !{re-match #'^tests/'# $h[file]}] } $hits
    let few  = take 5 $live
    [total: !{length $hits}, live: !{length $live}, sample: $few]

In later turns you can reuse all of `hits`, `live` and `few` again.

## Define reusable blocks

Define useful functions, such as predicates, mappers, file lists, path filters, that you may re-use again later. For example, `in-src` can be re-used later:

    let in-src = { |h| re-match #'^src/'# $h[file] }
    let live   = filter $in-src $hits

Define a parameterised block when later calls only vary arguments:

    let cargo-in = { |root args| within [dir: $root] { cargo ...$args } }
    cargo-in #'exarch'# [#'test'#, #'--test'#, #'session_apply'#]
    cargo-in #'exarch'# [#'test'#, #'--test'#, #'session_apply'#, #'queued_prompt_steers_before_next_tool_call'#]
    cargo-in #'ral'#    [#'test'#, #'--test'#, #'pipeline'#]

## Use records for named knobs

When a helper has several independent options, pass a record and override only the field that changes:

    let inspect-change = { |cfg|
        within [dir: $cfg[root]] {
            let hits  = grep-files $cfg[needle]
            let focus = filter { |h| re-match $cfg[file] $h[file] } $hits
            let shown = take $cfg[limit] $focus
            each { |h| view-text-around $h[file] $h[line] $cfg[peek] } $shown
        }
    }

    let cfg = [root: #'exarch'#, needle: #'ToolCall'#, file: #'^src/'#, peek: 4, limit: 3]

    inspect-change $cfg
    inspect-change [...$cfg, needle: #'run_shell'#]

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
        each { |h| view-text-around $h[file] $h[line] $cfg[peek] } $plan
    }

    run-with $collect $decide $act [root: #'exarch'#, needle: #'Session'#, limit: 5, peek: 3]

Use `$collect`, `$decide`, and `$act` because the helpers are values. Passing helpers explicitly also makes replacement unambiguous: a later call can pass a new `decide` without editing the source of `run-with`.

## Shadow

Bindings are immutable, but shadowable. When behaviour should change, re-bind with a new block:

    let cargo-in = { |root args| within [dir: $root] { cargo ...$args } }

    # Later: quieter default.
    let cargo-in = { |root args| within [dir: $root] { cargo -q ...$args } }

Blocks are lexical and capture environments. If a block uses a helper, e.g. `plan`, shadowing `plan` later DOES NOT AFFECT the behaviour of `plan` in previous blocks. Either re-bind the helper, or use higher-order arguments in the first place (as in `run-with` above).

## Examples

Survey a symbol: locate, drop tests, deduplicate files, and sample in one turn:

    let hits  = within [dir: #'src'#] { grep-files #'register_handler'# }
    let live  = filter { |h| $[not !{re-match #'^tests/'# $h[file]}] } $hits
    let files = fold { |acc h| if !{elem $h[file] $acc} { $acc } else { [...$acc, $h[file]] } } [] $live
    [total: !{length $hits}, live: !{length $live}, files: $files, sample: !{take 3 $live}]

Grep and read together: a `grep-files` hit carries its file and line; a `view-text-around` over the hit shows the place with the witness `edit` checks. Sample before reading context so output stays bounded:

    let hits = grep-files #'fn parse_'#
    let few  = take 5 $hits
    each { |h| view-text-around $h[file] $h[line] 3; echo #'---'# } $few
    [total: !{length $hits}, shown: !{length $few}]

Overlap slow work: start the suite, read the implementation while it runs, and use `audit` inside the worker when failure bytes matter:

    let suite = defer { cargo test -q 2>&1 }
    let impl  = within [dir: #'src/widget'#] { grep-files #'fn render'# }
    let tests = await $suite
    let log   = lines !{bytes-to-string $tests[value][children][0][stdout]}
    [impl: !{take 4 $impl}, ok: $[$tests[value][status] == 0], log: !{take 20 $log}]

Follow-ups index into the bindings — no re-grep, no re-run.

## One program, not a nervous probe

The instinct is to probe the machine in separate turns:

    uptime
    sysctl vm.swapusage
    memory_pressure | tail -1
    ps -Aco pid,pmem,rss,comm -m | head
    pkill -i zulip ; pkill -i "Microsoft Edge"
    sysctl vm.swapusage ; uptime          # ...did that help?

But there are only two questions here: how is memory, and what does killing the hogs buy back? Thus there should be two programs, each ending in one value. The first gathers every reading at once: `grab` lifts a capture group out of any line, and the heaviest processes parse into typed records — so `Code Helper (Renderer)` survives its spaces, the field-splitting that the bash `awk`-on-`split` idiom trips over:

    let grab = { |pat s| re-replace "(?s).*$pat.*" #'$1'# $s }

    let load  = map $float !{words !{grab #'averages: (.*)'# !{uptime}}}
    let swap  = sysctl -n vm.swapusage
    let used  = float !{grab #'used = ([0-9.]+)'#  $swap}
    let total = float !{grab #'total = ([0-9.]+)'# $swap}
    let freep = int   !{grab #'free percentage: ([0-9]+)'# !{memory_pressure 2>/dev/null}}
    let ram   = $[!{int !{sysctl -n hw.memsize}} / 1073741824]

    let hog = { |l|
        let w = words $l
        [name: !{intercalate #' '# !{drop 3 $w}}, mem-pct: !{float $w[1]}, rss-mb: $[!{int $w[2]} / 1024]]
    }
    let hogs = map $hog !{take 6 !{drop 1 !{lines !{ps -Aco pid,pmem,rss,comm -m}}}}

    [ram-gb: $ram, load: [m1: $load[0], m5: $load[1], m15: $load[2]],
     swap-mb: [used: $used, total: $total], free-pct: $freep, hogs: $hogs]

The reading is a value now: index it, `take` from it, diff it against a later snapshot.  The second program acts, and reports what the action bought — the re-check is not a third step but a measurement taken either side of the kill (the file adds an argument guard):

    let free   = { int !{grab #'free percentage: ([0-9]+)'# !{memory_pressure 2>/dev/null}} }

    let census = map { |l|
        let w = words $l
        [rss: !{int $w[0]}, comm: !{intercalate #' '# !{drop 1 $w}}]
    } !{drop 1 !{lines !{ps -Aco rss,comm}}}

    let weigh = { |pat|
        let hits = filter { |p| re-match "(?i)$pat" $p[comm] } $census
        let kb   = sum !{map { |p| $p[rss] } $hits}
        [app: $pat, procs: !{length $hits}, rss-mb: $[$kb / 1024]]
    }

    let before = !$free
    let reaped = map $weigh $args
    for $args { |p| attempt { pkill -i $p } }
    let after  = !$free

    [reaped: $reaped, total-mb: !{sum !{map { |r| $r[rss-mb] } $reaped}}, free-pct: [before: $before, after: $after]]

The census is taken once and every app weighed against it before anything dies.  The kills cannot simply be sequenced — `pkill` exits nonzero on an empty match, which would halt the rest — so `attempt` absorbs the miss.  And `grab`, bound in the first call, is still in scope for the second: a session accretes vocabulary.  The answer to "did that help?" is in the returned record.

The full runnable scripts are in `examples/mac-memory/{vitals,reap}.ral`.

## Mock with handlers

`within [handlers: …]` rebinds a command by name for the extent of a block: inside, every call to that name runs your block instead of the real program. 

**Pin nondeterminism.**  Replace a clock, a random source, or a remote with a fixed answer, and a result becomes reproducible:

    within [handlers: [date: { |args| echo #'2026-01-01T00:00:00Z'# }]] {
        let stamp = date -u +%Y-%m-%dT%H:%M:%SZ        # always the pinned value
        process-with $stamp
    }

**Exercise a destructive program without consequence.**  `reap` above kills processes; with its commands rebound it reads a census you supply and kills nothing, yet still returns its full record — so the logic is tested in isolation:

    within [handlers: [
        ps:              { |args| echo ##'  RSS COMM
    1200000 Zulip
     600000 Zulip'## },
        memory_pressure: { |args| echo #'System-wide memory free percentage: 40%'# },
        pkill:           { |args| echo "would kill !{str $args}" 1>&2 },
    ]] {
        source #'examples/mac-memory/reap.ral'#          # nothing dies; record still computed
    }

**Trace, by wrapping and forwarding.**  Handlers are *deep* — they hold for the whole body, nested calls included — and *self-masking*: inside a handler's own body a same-name call reaches the real command, so wrap-and-forward does not loop:

    within [handlers: [git: { |args| echo "+ git !{str $args}" 1>&2 ; git ...$args }]] {
        deploy            # every git inside is logged, then run for real
    }

`handler:` (singular) is a catch-all binary block `{ |name args| … }` intercepting every external command in the body — a whole-script dry run:

    within [handler: { |name args| echo "[would run: $name !{str $args}]" }] {
        deploy            # prints the plan; runs none of it
    }

## Reading large output

An elision in value/stdout/stderr means a command succeeded, but you asked to see too much. Narrow the call:
- Scope the query: Use `within [dir: #'src'#] { grep-files … }`, a tighter glob, or a `filter` before rendering.
- Bind, then slice: `length $hits`, `take 20 $hits`, `filter { … } $hits`. Never echo a large value whole.
- Pre-size files, then read windows: `line-count $f` first; then `view-text $f A B` or `view-text-around $f LINE PEEK`.
- For tests, ask for less: Name the single failing test or capture and slice the final log lines before returning them.
