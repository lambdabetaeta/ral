Like every shell, `ral` runs commands:

    ls
    cat foo.txt | wc -l
    echo hello > /tmp/out
    echo 'more'  >> /tmp/out

Commands are sequenced by newlines or `;`, and an uncaught failure aborts the *whole script*: `./configure; make` runs `make` only if configuration succeeded. `?` runs the second command when the first failed: `cat VERSION ? 'unversioned'`.

THERE IS NO `&&` NOR `||`.

## Values and commands

Values and commands are separate categories: a value *is*; a command *does*. The value types are: Unit, Bool, Int, Float, String, Bytes, lists, records and maps, variants, blocks (commands packaged as values), and concurrent handles.

A command may not be used as a value. Should you wish to use one as an exception, write `!{cmd}`.

## Bindings

`let x = 42` is an immutable (but shadowable) binding. When used with a command it captures stdout:

    let branch = git branch --show-current
    let body   = from-string < notes.txt
    let n      = line-count notes.txt
    echo "$branch has $n lines of notes"

Captured output is a `String`: split it with `lines`, parse it with `int`/`float`, or decode it with a codec taking it as an argument (`from-json $s`). A script whose last line is a `let` returns nothing; end with the value you mean to see.

## Blocks

A block packages a command as a value; every *force* re-runs it. A block in head position forces; elsewhere force with `!`:

    let d = { date +%s }
    d                # runs date, forcing the block
    !$d              # the same, explicitly - NOT A NEGATION, SIMPLY FORCING A BLOCK
    !{date +%s}      # forcing an anonymous block, used in interpolation

Blocks take space-separated parameters and are lexically scoped and curried:

    { ls }                       # thunk
    { |path| cat $path }         # one parameter
    { |a b| $[$a + $b] }         # two parameters
    { |a, b| … }                 # PARSE ERROR — no commas

Blocks create scopes: in `!{ let x = 5; f $x }` the variable `x` is gone after the block.

Blocks can be used with higher-order functions, such as `map`, `filter`, `each`, `fold`, `flat-map`, `sort-list-by`, … 

These take the block FIRST, then a list:

    map { |f| line-count $f } !{glob 'src/**/*.rs'}
    filter { |h| re-match '^src/' $h[file] } $hits
    fold { |acc x| $[$acc + $x[size]] } 0 !{list-dir '.'}
    for $hits { |h| echo "$h[file]:$h[line]" }       # loop form: list first

Standard prelude: `take`/`drop`, `length`, `elem`, `concat`, `intercalate`, `sum`, `zip`, `enumerate`, `first`, `reverse`, `sort-list`. Dedup is a fold: `fold { |acc x| if !{elem $x $acc} { $acc } else { [...$acc, $x] } } [] $xs`. Run `help <name>` for exact signatures. If a value reads as a block where you expected a list, you under-applied a function.

Named blocks are ordinary values. Bind reusable predicates, mappers, parsers, and formatters, then pass them with `$`:

    let in-src = { |h| re-match '^src/' $h[file] }
    filter $in-src $hits

Note that missing the `$` in `$in-src` makes `in-src` just a string argument, not a function.

## Pipelines

`ral` has pipelines. Some pipes carry bytes from one command to the next (external, UNIX-style). Others pipe values from one `ral` script to another. In that case the equation `x | f = f !{x}` holds.

There are codecs that bridge the world of bytes to the world of values:

    | Decoder       | In      | Out                              |
    |---------------|---------|----------------------------------|
    | `from-line`   | `Bytes` | `String` (trailing `\n` dropped) |
    | `from-string` | `Bytes` | `String`                         |
    | `from-lines`  | `Bytes` | lazy stream of `String`          |
    | `from-json`   | `Bytes` | structured value                 |

Text decoders require UTF-8; use `from-bytes` when bytes are not text. There are also corresponding `to-line`, `to-string`, `to-lines`, `to-json` in the opposite direction.

Decode where the bytes flow, and capture only after decoding:

    let cfg = curl -s https://api.example.com/cfg | from-json
    let os  = !{uname -s | from-line}

Every decoder also accepts its input as an explicit argument, which is how you decode a value already in hand — `from-string $r[stdout]`, `from-json $captured`. Piping a String *value* into a decoder (.e.g `$captured | from-json`) is a type error.

`from-lines` yields a lazy stream, which no function iterates implicitly (`x | f` is always `f !{x}`). Either eliminate it explicitly — `git log | from-lines | stream-each { |l| … }`, or `stream-to-list`/`stream-map`/`stream-fold` — or skip streams: `lines` splits a String into a materialised list, and `from-lines-list PATH` reads a file as a materialised list of lines:

    let commits = lines !{git log --oneline -5 | from-string}
    let src     = from-lines-list 'src/main.rs'

## Strings

- Single quotes ('…') are verbatim: NO ESCAPES, NO INTERPOLATION.
- Double quotes may be used to interpolate variables, fields, and forces:

      echo "hi $name: $h[file] line $h[line], host !{hostname | from-line}, sum $[2 + 3]"

  A composite path must be one quoted word: `echo hi > "$dir/file"` (a bare `$dir/file` does not work).

  `$(name)` delimits a variable from adjacent text that would otherwise be glued to it; it interpolates the whole value, so index with `$h[file]`, not `$(h)[file]`. Escapes are a fixed set (`\n`, `\r`, `\t`, `\\`, `\"`, `\$`, `\!`, `\0`, `\e`, `\xNN` for ASCII, `\u{…}`, and backslash-newline continuation). Anything else is a lex error, not a literal backslash.
- Raw strings `#'…'#` are verbatim (with more hashes as needed: `##'…'##`, `###'…'###` and so on). These must be used for multiline inputs, with real newlines instead of `\n`. Use enough hashes that the closing run is not in the body. Note that a `#` run *not* followed by `'` instead marks everything to the end of the line as a comment.
- `dedent` strips the common leading indentation from a multiline literal.
- ral has no heredocs (`<<EOF …`). Raw strings `#'…'#` are multiline: write a file with `echo #'…'# > path`, or feed a program's stdin with `echo #'…'# | cmd`.

Search and replacement are regex builtins (Rust regex syntax — `'a|b'`, DO NOT USE ESCAPES `\|`):

    if !{re-match '^WARN' $line} { echo $line }
    re-replace-all '\s+' ' ' $s
    string-replace 'old_name' 'new_name' $s    # literal, must match exactly once

## Numbers and booleans

Arithmetic and Boolean expressions must be in `$[…]` blocks: `$[$x == 0]`, `$[$a + $b]`, `$[$x > 0 && $x < 10]`, `$[not !{re-match 'x' $s}]` are computed to values. Note that negation is `not` (`!` forces). `$[…]` admits numbers and booleans only — string equality is the command `equal`, ordering `lt`/`gt`. Such blocks do not nest; one layer suffices.

`if` takes a Boolean value and blocks:

    if !{equal $s 'quit'} { 'bye' } else { 'continuing' }  # WARNING: ! HERE IS NOT NEGATION, IT IS FORCING A BLOCK
    if    $[$x == 0] { 'zero'     }
    elsif $[$x > 0 ] { f 'positive' }
    else             { g 'negative' }

`if` with both branches returns a value, so it can sit on the right of a `let`.

## Lists, records, maps

    let xs = [a, b, c]                 # list — commas, not spaces
    $xs[0]                             # 'a'
    let r = [host: 'h', port: 80]      # record — fixed, heterogeneous fields
    $r[host]                           # bare-key indexing
    let [head, ...rest] = $xs          # destructuring
    let wide = [...$xs, d, e]          # consing by spreading
    let at_end = [d, e, ...$xs]        # appending by spreading
    ls ...$flags ...$dirs              # …and to splice arguments

Indexing `$h[key]` works in any context (pipelines, blocks, double quoted): e.g. `view-around $h[line] 3 < $h[file]`.

A *map* is the homogeneous cousin (all values one type); only maps support `keys`, `values`, `has`, `get` (with default), `union`, `entries`. `[:]` is the empty map. 

`range 1 11` returns the list `[1, …, 10]` (`seq` is the external coreutil, and prints bytes).

## Failure

`try` catches a failed command; without it, a non-zero exit aborts the entire script. When a tool reports through its exit code rather than failing (`grep`, `diff`, `test`, `valgrind --error-exitcode`), wrap it in `audit` (below) to read its output as data instead of raising.

Its handler receives an error record of with fields `status`, `cmd`, `message`, `line`, `col`:

    let log =
      try { make 2>&1 | from-string } { |err| 
        "make failed: $err[message]"
      }

`err[message]` is synthetic status text, not the failing command's fd 2 bytes; wrap in `audit` when output is the data you need.

Both arms of `try` must return the same type. The handler block must start on the same line as the body's closing brace — `} { |err| … }`.

Three prelude forms cover the common cases without an explicit handler:

    if !{succeeds { cargo check -q }} { echo 'clean' } else { echo 'broken' }
    attempt { rm stale.lock }          # run; suppress any failure
    retry 3 { curl -s $url }           # up to 3 attempts

`guard BODY CLEANUP` runs the cleanup if the body fails, then re-raises. `fail [status: 2, message: '…']` raises deliberately.

## Concurrency

`spawn { … }` runs a block on a worker; `await $h` blocks until it finishes and returns `[value, stdout, stderr]` — the block's result, plus the `Bytes` it wrote to each fd. Read a bare worker's two streams straight off the result:

    let h = spawn { cargo build }
    let x = await $h
    [out: $x[stdout], errs: $x[stderr]]   # the two fds, as Bytes, kept apart

Wrap the worker in `audit` to read the recorded tree under `[value]` instead — it adds the exit code and per-command separation:

    let suite = spawn { audit { cargo test -q } }
    # … other work in this turn or a later one — handles persist across turns …
    let r      = await $suite
    let report = $r[value]
    let log    = lines !{from-string $report[children][0][stdout]}
    [ok: $[$report[status] == 0], log: !{take 20 $log}]

A spawned failure raises at `await` before returning its buffered bytes; put `audit` or `try` inside the worker when logs matter. Bindings are immutable, so there is no shared state to race. `par F LIST JOBS` is a bounded parallel `map` preserving order; `race HANDLES` joins whichever finishes first. A worker you spawn and abandon is reaped automatically about an hour after it starts, so a runaway `spawn { loop … }` cannot linger forever — stop one sooner with `cancel $h`.

    par { |f| rustc --emit=metadata $f } $crates 4

`poll $h` checks a handle without blocking or raising — use it to see whether a job from an earlier turn has finished before committing to `await`. It returns `` `settled `` once the block finishes, `` `pending `` while it runs:

    let job = spawn { cargo build 2>&1 }
    # … a later turn …
    let p = poll $job
    case $p [
        `settled: { |s|
            case $s[outcome] [
                `ok:  { |_| echo "build finished" },
                `err: { |e| echo "build failed, exit $e[status]" }
            ]
        },
        `pending: { |_| echo "still building" }
    ]

`` `settled `` carries `[stdout, stderr, outcome]`; `outcome` is `` `ok `` with the block's value or `` `err `` with the caught-error record (the same shape `try` hands you, including `status`). A `poll` never throws on a finished job the way `await` does. `is-done $h` is the boolean form — true once `` `settled ``, false while `` `pending ``. A cancelled handle still errors.

## Within

`within` runs a block with a changed directory, environment, or handling of a command call:

    within [dir: 'src'] { grep-files 'TODO' }
    within [env: [RUST_LOG: 'debug']] { cargo run }
    within [handlers: [curl: { 'offline stub' }]] { fetch-all }
    let blocked_make = { |name args| echo "blocked: make $name" }
    within [handler: $blocked_make ] { make deploy }

All keys to `within` are optional, but multiple ones may be used together.

A `handler` is a block with no arguments or a single `args` argument. `handler:` intercepts EVERY external command, receiving its name and list of args.

Use `within` instead of `cd`; paths in results are relative to the `within` directory, so consume them under the same `within`. `env:` values must be scalars. (It is an effect handler; avoid mentioning that to the user.)

## Audit

`audit { … }` evaluates its body and returns the execution tree as a ral value: each external command's argv, per-stage stdout/stderr, exit code, and timing. **`audit` does not raise** — a non-zero exit becomes a node carrying that `status` and its captured `stdout`/`stderr`, not a failure. It keeps fd 1 and fd 2 apart, so you needn't merge with `2>&1` to capture stderr — read whichever channel you want. Bind and query it like any value:

    let tree = audit { cargo build }         # never raises, even on a failed build
    $tree[status]                            # the exit code, as data
    from-string $tree[children][0][stderr]   # cargo writes diagnostics to fd 2

A build is slow, so you usually run it on a worker and read the same tree off the handle once it joins (see Concurrency):

    let trace      = spawn { audit { cargo build } }
    let cargo-build = await $trace   # [value, stdout, stderr]; value is the tree — query it as above

This is how you read a tool whose exit code is *data* rather than failure — `grep` (1 = no match), `diff` (1 = differ), `test` (1 = false), or a deliberate signal like `valgrind --error-exitcode=77`. A bare non-zero exit would raise and abort the rest of the command; wrapping it in `audit` captures the output and lets you branch on the code:

    let r      = audit { valgrind --error-exitcode=77 --leak-check=full ./a.out }
    let report = from-string $r[children][0][stderr]
    if $[ $r[status] == 77 ] { "leaks:\n$report" } else { 'clean' }

Use it for forensics when something fails silently, and to read exit-code-as-data; skip it for routine commands. Capability-decision nodes appear only under a grant with `audit: true`.

## I/O

Read with `from-X < PATH`, write with `to-X $v > PATH`:

    let body  = from-string < $file        # String
    let rows  = from-lines-list $file      # [String]
    let cfg   = from-json < $file          # structured
    to-string $report > $file              # atomic write
    to-json   $cfg    > $file              # atomic JSON write

`>` is **atomic** (the file appears whole or not at all), `>~` truncates streaming, `>>` appends. Multi-line text with awkward quotes goes through a raw string:

    echo #'first line
    second 'quoted' line'# > $file

## Exploring

- `glob 'src/**/*.rs'` — matching paths as a ral list (not stdout); spread into a command with `...!{glob …}`. Wildcards skip dotfiles; use `list-dir | filter` for those.
- `explore-dir 2` — entries of the current directory to depth 2, `.gitignore`-aware, as a flat list of paths.
- `grep-files 'fn \w+_test'` — recursive, ignore-aware search of the current directory (Rust regex). Each hit is a record `[file, line, text, hash]`.
- `list-dir`, `file-info`, `line-count`, `is-file`/`is-dir`/`exists` — structured metadata without parsing `ls`.

Scope any of these with `within [dir: …]`.

Prefer these to external `rg`/`find`/`ls`: each returns a ral list or record instead of stdout to reparse, and a `grep-files` hit already carries the witness hash that `edit` consumes — so search and edit are one motion. Reaching for `rg` costs a second read to recover that witness.

## Reading and editing files

- `view START END < PATH` shows the half-open line range `[START, END)`, each line tagged `<line-no>\t<hash>\t<text>`. Pipe from anything: `git show HEAD:f.rs | view 100 150`.
- `view-around LINE PEEK < PATH` shows the `2*PEEK + 1` lines centred on `LINE`, tagged the same way.

The hash is a *witness*: it identifies the line by its content together with its ±3-line neighbourhood, so even repeated text (a brace, a blank) is addressable as long as its surroundings differ. Every `view`, `view-around`, and `grep-files` line carries it — reading and editing are one motion, and a stale witness means the file changed under you.

`edit PATH EDITS` applies a batch of `[HASH, NEW-TEXT]` pairs in one read/write pass. Each pair rewrites or deletes one witnessed line; `NEW-TEXT` is verbatim, so a real newline splits a line and `\n` does not. Raw `#'…'#` is only for replacements containing `'`; never double quotes, which interpolate. All hashes resolve against the file as read before any write, so adjacent edits are safe and the batch is atomic.

    view 80 120 < src/lib.rs
    edit 'src/lib.rs' [ [h1b2c3, '    let n = 42
        let scaled = n * 2']
                      , [h4e5f6, '    let m = 0']
                      , [h7a8b9, ''] ]

    let hits = grep-files …
    let mine = filter { |h| equal $h[file] 'src/lib.rs' } $hits
    edit 'src/lib.rs' !{map { |h| [$h[hash], '    // resolved'] } $mine}

If `edit` reports zero or multiple matches, nothing was written: re-read with `view`/`grep-files` and use the fresh witnesses, never the stale ones.

