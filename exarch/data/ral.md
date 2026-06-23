Like every shell, `ral` runs commands:

    ls
    cat foo.txt | wc -l
    echo hello > /tmp/out
    echo 'more'  >> /tmp/out

Commands are sequenced by newlines or `;`, and an uncaught failure aborts the whole script: `./configure; make` runs `make` only if configuration succeeded. `?` runs the second command when the first failed: `cat VERSION ? 'unversioned'`. THERE IS NO `&&` NOR `||`.

Under the hood `ral` is really a version of call-by-push-value with full recursion, tail call optimisation, recursive types, and commands/exec-as-effects. 

`ral` has value types and computation types. The basic value types are: Unit, Bool, Int, Float, String, Bytes, lists of values, records and maps, variants, blocks (= commands packaged as values), and concurrent handles. A command may not be used as a value. Should you wish to use one inline, you must make it into an anonymous block and force it: `!{cmd}`.

## Definitions

`let x = 42` is an immutable (but shadowable) definition, which can be used as `$x`. 

A script whose last line is a `let` returns nothing; end with the value you mean to see.

When used with a command a binding captures stdout:

    let branch = git branch --show-current
    let body   = from-string < notes.txt
    let n      = line-count notes.txt
    echo "$branch has $n lines of notes"

Captured output is a `String`: split it with `lines`, parse it with `int`/`float`, or decode it with a codec (`from-json $s`).

## Blocks

A block packages a command as a value; forcing runs it. A block in head position is forced; otherwise use `!`:

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

Blocks can be used with higher-order functions, such as `map`, `filter`, `each`, `fold`, `flat-map`, `sort-list-by`, …. Examples:

    map { |f| line-count $f } !{glob 'src/**/*.rs'}
    filter { |h| re-match '^src/' $h[file] } $hits
    fold { |acc x| $[$acc + $x[size]] } 0 !{list-dir '.'}
    for $hits { |h| echo "$h[file]:$h[line]" }

You have the standard prelude found in functional programming: `take`, `drop`, `length`, `elem`, `concat`, `intercalate`, `sum`, `zip`, `enumerate`, `first`, `reverse`, `sort-list`. For example, use `fold { |acc x| if !{elem $x $acc} { $acc } else { [...$acc, $x] } } [] $xs` for de-duplication.

Blocks are ordinary values. Define reusable functions pass them with `$`:

    let in-src = { |h| re-match '^src/' $h[file] }
    filter $in-src $hits

NB: omitting the `$` in `$in-src` makes `in-src` just a string argument in the above.

Finally, blocks support recursive definitions.

## Pipelines

`ral` has pipelines. Some pipes carry bytes from one command to the next (external, UNIX-style). Others pipe values from one `ral` script to another; then the equation `x | f = f !{x}` holds.

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

- Single quotes ('…') are verbatim; NO ESCAPES, NO INTERPOLATION.
- Double quotes may be used to interpolate variables, fields, and forces:

      echo "hi $name: $h[file] line $h[line], host !{hostname | from-line}, sum $[2 + 3]"

  A composite path must be one quoted word: `echo hi > "$dir/file"` (a bare `$dir/file` does not work).

  `$(name)` delimits a variable from adjacent text that would otherwise be glued to it; it interpolates the whole value, so index with `$h[file]`, not `$(h)[file]`. Escapes are a fixed set (`\n`, `\r`, `\t`, `\\`, `\"`, `\$`, `\!`, `\0`, `\e`, `\xNN` for ASCII, `\u{…}`, and backslash-newline continuation).
- Raw strings `#'…'#` are verbatim (with more hashes as needed: `##'…'##`, `###'…'###` and so on). These must be used for multiline inputs, with real newlines instead of `\n`. Use enough hashes that the closing run is not in the body. Note that a `#` run *not* followed by `'` instead marks everything to the end of the line as a comment.
- `dedent` strips the common leading indentation from a multiline string.
- `ral` has no heredocs (`<<EOF …`). Raw strings `#'…'#` are multiline: write a file with `echo #'…'# > path`, or feed a program's stdin with `echo #'…'# | cmd`.

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

Indexing `$h[key]` works in any context (pipelines, blocks, double quoted): e.g. `view-text-around $h[line] 3 < $h[file]`.

A *map* is the homogeneous cousin (all values one type); only maps support `keys`, `values`, `has`, `get` (with default), `union`, `entries`. `[:]` is the empty map. 

`range 1 11` returns the list `[1, …, 10]` (`seq` is the external coreutil, and prints bytes).

## Variants and case

A variant is a backtick-tagged value. `` `absent `` is a bare (nullary) tag; `` `file [bytes: 4890] `` is a tag carrying a payload — usually a record. A tag records which of several outcomes occurred and its payload carries that outcome's data, so a function that probes the system records its finding as a variant:

    let probe = { |p|
      if    $[not !{exists $p}] { `absent }
      elsif !{is-dir $p}        { `dir  [entries: !{length !{list-dir $p}}] }
      else                      { `file [bytes: !{file-info $p}[size]] }
    }

`case` eliminates a variant: a tag-keyed table of handler blocks, each binding the matched tag's payload; only that label's block runs.

    case !{probe $path} [
      `absent: { |_| "$path: not found" },
      `dir:    { |d| "$path: directory, $d[entries] entries" },
      `file:   { |f| "$path: file, $f[bytes] bytes" }
    ]

The table must cover every tag. A nullary tag still hands its block a value — ignore it with `_`. Handlers nest: when a payload is itself a variant, match it with a `case` inside its block.

## Failure

`try` catches a failed command; without it, a non-zero exit aborts the entire script. When a tool reports through its exit code rather than failing (`grep`, `diff`, `test`, `valgrind --error-exitcode`), wrap it in `audit` to read its output as data instead of raising.

Its handler receives an error record of with fields `status`, `cmd`, `message`, `line`, `col`:

    let log =
      try { make 2>&1 | from-string } { |err| 
        "make failed: exited $err[status], $err[message]"
      }

The handler block must start on the same line as the body's closing brace — `} { |err| … }`. `err[message]` is synthetic status text, not the failing command's stderr; wrap in `audit` when output is the data you need.

Prelude functions cover common cases:

    if !{succeeds { cargo check -q }} { echo 'clean' } else { echo 'broken' }
    attempt { rm stale.lock }          # suppress any failure
    retry 3 { curl -s $url }           # up to 3 attempts

`guard BODY CLEANUP` runs the cleanup block if the body fails, then propagates the failure. `fail [status: 2, message: '…']` raises deliberately.

## Concurrency

`spawn { … }` runs a block on a worker and returns a handle at once; `await $h` blocks until the worker settles and returns a [value, stdout, stderr] record. Awaiting is cheap and returns the moment the block settles. A `spawn` you do not await is not stranded: let the turn return and the host notifies you at the next turn boundary when the worker settles, rendering its surfaced output on the rail; `await $h` then, on that later turn, only when you want the value record. Use like this:

    let b = { cargo build }
    let h = spawn $b
    let x = await $h                      # blocks until the worker returns
    [out: $x[stdout], errs: $x[stderr]]   # as Bytes

Wrap in `audit` to read the recorded tree:

    let suite = spawn { audit { cargo test -q } }
    # … other work in this turn or a later one — handles persist across turns …
    let r      = await $suite
    let report = $r[value]
    let log    = lines !{from-string $report[children][0][stdout]}
    [ok: $[$report[status] == 0], log: !{take 20 $log}]

Use `cancel $h` to stop a worker that is no longer needed.

A spawned failure raises at `await` before returning its buffered bytes; put `audit` or `try` inside the worker when logs matter. 

There is also a bounded parallel `map` and a `race`; use `help` to find out more about them. 

## Within

`within` runs a block with a changed directory, environment, or handling of a command call:

    within [dir: 'src'] { grep-files 'TODO' }
    spawn { within [env: [RUST_LOG: 'debug']] { cargo run } }
    within [handlers: [curl: { |args| 'offline stub' }]] { fetch-all }
    let blocked_make = { |name args| echo "blocked: $name ...$args" }
    within [handler: $blocked_make ] { make deploy }

All keys to `within` are optional, but multiple ones may be used together.

A per-command `handlers:` entry is a unary lambda `{ |args| … }` and receives that command's argument list. The catch-all `handler:` is a binary lambda `{ |name args| … }`; it intercepts EVERY external command, receiving its name and its args.

Use `within` instead of `cd`; paths in results are relative to the `within` directory, so consume them under the same `within`. `env:` values must be scalars. (It is an effect handler; avoid mentioning that to the user.)

## Audit

`audit { … }` evaluates its body and returns the execution tree as a ral value: each external command's argv, stdout, stderr, exit code, and timing. `audit` does not raise errors: it turns them into record data. It also keeps stdout/stderr apart, so you need not `2>&1` to capture stderr. Example use:

    let tree = audit { cargo build }         # never raises, even on a failed build
    $tree[status]                            # the exit code, as data
    from-string $tree[children][0][stderr]   # cargo writes diagnostics to fd 2

A build is slow, so you usually run it on a worker and read the same tree off the handle once it joins (see Concurrency):

    let trace      = spawn { audit { cargo build } }
    let cargo-build = await $trace   # the returned value is the audit tree

This is how you read a tool whose exit code is *data* (e.g. `grep` exit 1 meaning no match), deliberate signal like `valgrind --error-exitcode=77`. Wrapping such a tool in `audit` captures the output and lets you branch on the code:

    let r      = audit { valgrind --error-exitcode=77 --leak-check=full ./a.out }
    let report = from-string $r[children][0][stderr]
    if $[ $r[status] == 77 ] { "leaks:\n$report" } else { 'clean' }

## I/O

Read with `from-X < PATH`, write with `to-X $v > PATH`:

    let body  = from-string < $file    # String
    let rows  = from-lines-list $file  # [String]
    let cfg   = from-json < $file      # record
    to-string $report >  $file         # write (atomic)
    to-string $report >> $file         # append
    to-json   $cfg    >  $file         # JSON write

Multi-line text with awkward quotes goes through a raw string:

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

- `view-text START END < PATH` shows the half-open line range `[START, END)`, each line tagged `<line-no>\t<hash>\t<text>`. Pipe from anything: `git show HEAD:f.rs | view-text 100 150`.
- `view-text-around LINE PEEK < PATH` shows the `2*PEEK + 1` lines centred on `LINE`, tagged the same way.

The hash is a freshness witness: it identifies the line by its content and neighbourhood. Every `view-text`, `view-text-around`, and `grep-files` line carries it, as it is required to edit.

`edit PATH EDITS` applies a batch of `[HASH, NEW-TEXT]` pairs in one pass. Each pair replaces **only the unique line identified by the hash**; to replace an multiline block provide all hashes and replacement strings (one pair per line). `NEW-TEXT` is inserted verbatim, so newlines become actual newlines; pass the empty string `''` to delete a line. . Raw `#'…'#` is useful for replacements; never use interpolating double quotes for editing. All hashes resolve against the file as read before any write, so adjacent edits are safe and the edit is atomic. Example:

    view-text 80 120 < src/lib.rs       # this allows you to view hashes
    edit 'src/lib.rs' [
      [h1b2c3, '    let n = 42
        let scaled = n * 2'],
      [h4e5f6, '    let m = 0'],
      [h7a8b9, '']
    ]

`edit` can also be used programmaticaly:

    let hits = grep-files …
    let mine = filter { |h| equal $h[file] 'src/lib.rs' } $hits
    edit 'src/lib.rs' !{map { |h| [$h[hash], '    // resolved'] } $mine}

## Surfacing

`surface CARD` shows the user a render document on the rail. Surface your own card only when a result is worth the user seeing it (a build summary, a test matrix, a captured output). You declare both the *data* and its *level of measurement*; the host owns the appearance, so you name a role or a magnitude, never a colour.

A card `` `card LIST-OF-MARKS` `` is an ordered stack of marks drawn top-to-bottom. There are five marks:

- `` `text [spans: [[role: "…", text: "…"], …]] `` — a run of spans. Every span carries `role` — one of `path`, `code`, `ok`, `warn`, `bad`, `muted`, `strong` (identity, mapped to a hue), or `""` for plain ink. A heading is a `strong` span.
- `` `measure [label: "…", value: N, max: M, unit: "…"] `` — a magnitude. With `max`, it reads as a proportional bar (`value/max`); without, as a `log2` size bar. `max`/`unit` may be omitted.
- `` `fields [rows: [[label: "…", value: VALUE], …]] `` — an aligned `(label, value)` table; rows are records (a positional `[label, value]` list would force label and value to one type). A `VALUE` is a `` `text `` or `` `measure `` mark; use the same kind across the rows.
- `` `diff [path: "…", start: N, before: […], del: […], add: […], after: […]] `` — one located hunk (`del` rewritten to `add` at line `start`, with context). Pass `hunks: [[…], …]` for several. The host renders the size bar, add/del grain, and graded disclosure.
- `` `raw [bytes: "…"] `` — pre-formed bytes appended verbatim, for output outside the grammar. Honest about being un-encoded ink.

A `` `card `` may stack marks of different kinds, but within one homogeneous list — a span list, a `fields` row list — every element is one type, so give every span a `role` and keep a table's values one kind. `edit` builds and surfaces its own diff card; the tasks kit adds `task-card`/`meter-card` constructors. Compose marks directly for anything else:

    surface `card [
      `text    [spans: [[role: "strong", text: "tests "], [role: "ok", text: "42 passed"]]],
      `measure [label: "crates", value: 7, max: 12],
      `fields  [rows: [[label: "suite",  value: `text [spans: [[role: "",   text: "unit" ]]]],
                       [label: "status", value: `text [spans: [[role: "ok", text: "green"]]]]]]
    ]

## Help

When you are unsure of the signature of something you always call `help <name>`. This can be done as part of a turn:

    let h = spawn { audit { make } }
    let x = help 'view-text-around'
    [view-text-around-help: $x]

Many builtins are not covered above; call `help` on any of: `ask`, `watch`, `alias`/`unalias`, `source`, `use`, `shell-quote`/`shell-split`, `upper`/`lower`/`slice`, `str`, `re-split`/`re-find-match`/`re-find-matches`/`re-replace`, `resolve-path`/`cwd`/`cd`/`temp-dir`/`temp-file`, `is-link`/`is-readable`/`is-writable`/`is-empty`, `fold-lines`, `clear`/`reset`, `reduce`, `last`, `take-while`/`drop-while`, `words`, `intersection`/`difference`, `stream-cons`/`stream-nil`/`stream-take`/`stream-drop`, `map-lines`/`filter-lines`/`each-line`, `file-empty`, `par`, `ansi-…`/`styled`.
