Like every shell, `ral` runs commands:

    ls
    cat foo.txt | wc -l
    echo "hello" > /tmp/out

Commands are sequenced by newlines or `;`. An uncaught failure aborts the whole script: `mkdir out; cp notes.txt out/` runs `cp` only when `mkdir` succeeds. `?` runs the second command when the first failed: `cat VERSION ? #'unversioned'#`. `&&` and `||` are Boolean operators, used inside `$[…]`. There is no trailing `&` either: background work is `spawn "title" { … }`, below.

A string argument stays one argument: its contents are never implicitly split, globbed, or interpreted as source. Spread a list explicitly with `...$args`; expand a glob with `...!{glob #'*.txt'#}`. A `&` inside a word is ordinary, so `curl https://h/?a=1&b=2` needs no quotes. Outside double quotes, `\` is ordinary too: `C:\Users\me` is one word.

`ral` is essentially call-by-push-value with recursion, recursive types, and one effect: an exec call. Its value types are `Unit`, `Bool`, `Int`, `Float`, String, Bytes, lists, records, maps, variants, thread handles, and blocks (= parameterized, thunked commands).

## Values and computations

`ral` separates values from computations. `let x = …` runs the computation on its right and binds the result; an argument or record field needs a value. The forms to remember are:

- `$x` retrieves a bound value.
- `{ … }` packages a computation as a block, without running it.
- `!{ … }` forces an anonymous block where a value is needed; `!$f` forces a bound block.
- `$[ … ]` evaluates arithmetic or Boolean expressions. Boolean negation is `not`: `$[not !{is-file $path}]`. `!` always means forcing.
- `( … )` never groups: pass a call's result as `!{…}`, e.g. `for !{take 30 $xs} { |x| … }`, or `let` it first.

A computation can return a value and can write bytes to stdout. `let` binds the returned value of a computation such as `length` or `from-json`. For a byte-writing command such as `date` or `echo`, it instead captures stdout as a `String`, removing one trailing line terminator if present:

    let today = date +%F                 # captured stdout: String
    let body  = from-string < notes.txt  # returned value: String
    let n     = line-count notes.txt     # returned value: Int
    echo "notes.txt ($n lines) as of $today"

This also works through blocks and functions: `let f = { hostname }; let host = f` captures the host name. A final decoder such as `| from-lines` or `| from-json` returns another value instead.

Use forcing to put a computation in a value position:

    let xs = [a, b, c]
    let count = length $xs
    [count: !{length $xs}]

<CRITICAL> Bound variables are available for the rest of the session; you do not need to redefine them:

    echo "notes.txt now has !{line-count notes.txt} while it had $n before"
</CRITICAL>

A binding shadows the command of the same name for the rest of the session, so don't name a binding `ls`, `head` or `tail`; `^ls` runs the command.

The shell's own state is read by six builtins: `cwd`, `env`, `args`, `user`, `home`, `nproc`. Use them as `let [a, b] = args`, `!{env}[HOME]`, `"!{cwd}/x"`.

A turn ending in `let` returns nothing; end with what you mean to see as `VALUE`. Fields live inside a record value, such as `[out: $o, err: $e]`.


## Help

When you are unsure how to use something, call `explain <name>`. It covers language forms such as `grant` and `within` as well as builtins and prelude functions, and gives a standalone guide followed by labelled type and source facts. Do not reach for `which`, as that only covers executables in the PATH.

`help` lists available forms and commands. All Rust uutils are available.

## Exploring

The exploration tools skip dotfiles and `.gitignore`d files, unless stated otherwise.

Look at a directory with `ls`. To compute over its entries, `list-dir $path` returns a record per entry; `file-info`, `line-count` and `is-file`/`is-dir`/`exists` answer structured questions about a single path. To survey or search the tree:

- `glob #'**/*.txt'#` returns matching paths as a list. `.gitignore`d files match; a wildcard never matches a dotfile, so name one literally. Spread into a command: `mv ...!{glob …} out/`.
- `explore-dir 2 src` lists the entries beneath a directory to depth 2 as a list.
- `grep-files #'TODO'# src` searches a file, or a directory recursively.
- `grep-hidden #'API_KEY'# .` also searches hidden and `.gitignore`d files, but skips `.git`.
- `find-files #'query'# src` returns the files under a file or directory as `[String]`, fuzzy-ranked by path, best first and truncated. Use it to find files by name without a glob pattern.

Scope any of these with `within [dir: …]`. A walk skips only what it finds: a path you name is always searched, so `grep-files #'x'# target` searches a gitignored directory and `grep-files #'x'# .env` a dot file.

`view-text` and `view-text-around` return their lines as values. Only the final expression becomes `VALUE`, so collect several reads into one result:

    [source: !{view-text src/main.rs 1 20}, config: !{view-text Cargo.toml 1 20}]

## Blocks

A bound block can be called by name in command position:

    let d = { date +%s }
    d                # runs date, forcing the block, even without !
    !$d              # the same, explicitly

Blocks may take space-separated, lexically-scoped, curried parameters:

    { ls }                                # thunk
    let print-file = { |path| cat $path } # one parameter
    let f = { |a b| $[$a + $b] }          # two parameters
    { |a, b| … }                          # PARSE ERROR — no commas

Pass blocks to higher-order functions:

    map { |f| line-count $f } !{glob #'**/*.txt'#}
    filter { |h| re-match #'^src/'# $h[file] } $hits
    fold { |acc x| $[$acc + $x[size]] } 0 !{list-dir #'.'#}
    for $hits { |h| echo "$h[file]:$h[line]" }
    let in-src = { |h| re-match #'^src/'# $h[file] }
    filter $in-src $hits

Omitting the `$` in `$in-src` makes `in-src` just a string argument in the above.

Bindings are immutable but shadowable. Blocks capture lexically: earlier blocks keep the bindings they captured, so shadowing a helper does not change those blocks. A block is a scope: a `let` inside it ends with the block. The bodies of `if`, `for` and `within` are blocks, so a value leaves them only by being returned: `let h = within [dir: #'/app'#] { spawn "build" { … } }`.

## Strings

- Double quotes may be used to interpolate variables, fields, and forces:

      echo "hi $first-name $(last-name): $h[file] line $h[line], today !{date +%F | from-line}, sum $[2 + 3]"

- Parentheses delimit a variable inside an interpolated word: `"$(today)-checkout"`. `$(…)` never runs a command. To capture command output, use `let` or force the command inside double quotes with `!{…}`.
- Words never glue: `$dir/file`, `-j!{nproc}` and `--format=#'%H'#` are parse errors. Quote the whole word: `"$dir/file"`, `"-j!{nproc}"`, `#'--format=%(refname:short)'#`.
- In double quotes, a leading `~` or `~/` expands to the home directory, e.g. `ls "~/Work Invoices"`. `!{home}` is the same value.
- Double quotes process escapes such as `\n`, `\t`, `\\`, `\"`, `\$`, and `\!`; any other, like `\|`, is an error. Escapes belong to the literal: `echo` writes the strings it is given, so the tab in `echo "a\tb"` is already in the string.
- Write literal text as a raw string, `#'…'#`, with no escapes or interpolation. The delimiters are mirror images: `#'` opens, `'#` closes, touching: `#'…'` and `#'…' #` never close. Any other word starting with `#` begins a comment, so `#/app/x#` is no string. A bare word is already a string value, so paths, flags and names need no quotes: `[--out, /app/x.whl, RowParallelLinear]`. If the text contains `'#`, use more hashes on both sides: `###'…'###`. Use raw strings for literal payloads passed to external tools, e.g. `sed -i #'s|^old_value$|new_value|'# config.txt`. Pass computed values directly as `$arg`, or interpolate when constructing text.
- `dedent` strips the common leading indentation from a multiline string.
- There are no heredocs. `cmd << $body` or `python3 << #'…'#` feeds a string to stdin; the raw string is the whole body, with no `EOF` label. One newline at the very start of the string is dropped, so the body can start on the line under the command.
- Split captured text explicitly with `lines $text`.
- Parse numeric text with `int $text` or `float $text`.

Search and replacement are regex builtins (Rust regex syntax — #'a|b'#, DO NOT USE ESCAPES `\|`):

    if !{re-match #'^WARN'# $line} { echo $line }
    re-replace-all #'\s+'# #' '# $s
    string-replace #'old_name'# #'new_name'# $s    # literal, must match exactly once

## Numbers and booleans

A bare word that looks like a number IS that number: `echo 007` prints `7`, `echo 1.50` prints `1.5`, and `echo #'3.100'#` prints `3.100`:

    let ver = #'3.10'#                 # a version is text; bare, it would be 3.1

Arithmetic and Boolean expressions must be in `$[…]` blocks: `$[$x == 0]`, `$[$a + $b]`, `$[$x > 0 && $x < 10]`, `$[not !{re-match #'x'# $s}]` are computed to values. An operand may be any value — `$[$s == #'quit'#]` compares strings, and `==` is structural on every kind — but arithmetic and `<`/`>` want numbers. Such blocks do not nest; one layer suffices. Force calls inside: `$[!{length $xs} == 3]`.

`if` takes a Boolean value and blocks, written out or in hand:

    if $[$s == #'quit'#] { #'bye'# } else { #'continuing'# }
    if    $[$x == 0] { #'zero'#     }
    elsif $[$x > 0 ] { f #'positive'# }
    else             { g #'negative'# }

`if` with both branches can return a value on the right of a `let`.

## Structured values

Every element of a list shares one type; use a record for fields of different types:

    let xs = [a, b, c]                   # list, with elements separated by commas
    $xs[0]                               # #'a'#
    let r = [host: #'h'#, port: 80]        # record — fixed, heterogeneous fields
    $r[host]                             # bare-key indexing
    let [first, ...rest] = $xs           # destructuring
    let at_end = [...$xs, d, e]          # appending by spreading
    let at_start = [d, e, ...$xs]        # prepending by spreading
    ls ...$flags ...$dirs                # …and to splice arguments

Collection literals contain values, so force calls inside them: `[!{length $xs}, 2]`. The same rule applies to record fields: `[count: !{length $xs}]`. A pipeline is one computation, so it is forced whole: `[mode: !{stat -c #'%a'# $f | from-line}]`.

Indexing `$h[key]` works in any context (pipelines, blocks, double quoted): e.g. `view-text-around $h[file] $h[line] 3`. It reads a list by Int, a record by label and a map by key, nothing else: not text, bytes or a handle, and no slices. Use `take 5 !{drop 2 $xs}`, `slice $s 0 80`, `!{commands $r}[0][stdout]`.

A map's keys are runtime data and its values all share one type; a record's labels are fixed. A literal is a map if any key is data: `"quoted"` (`["a": 1, "b": 2]`) or computed (`[$k: 1]`). A bare key or a raw string key (`#'label'#`) is a label, and labels alone make a record; `[:]` is the empty map. Only maps support `keys`, `values`, `has`, `get` (with default), `union`, `entries`, and only a map is read by a computed key (`$m[$k]`).

A variant is a value tagged by a `` `tag ``, recording one of several outcomes along with some data, e.g. `` `file [bytes: 4096] ``:

    let probe = { |p|
      if    $[not !{exists $p}] { `absent }
      elsif !{is-dir $p}        { `dir  [entries: !{length !{list-dir $p}}] }
      else                      { `file [bytes: !{file-info $p}[size]] }
    }

`case` selects a variant's arm. Write one arm per tag in place; each arm binds the tag's payload:

    case !{probe $path} [
      `absent: { |_| "$path: not found" },
      `dir:    { |d| "$path: directory, $d[entries] entries" },
      `file:   { |f| "$path: file, $f[bytes] bytes" }
    ]

A nullary tag passes `()` — ignore it with `_`. An arm may also name a function: `` `dir: $describe `` applies it to the payload. Call `explain case` for the arm restrictions.

`range 1 11` returns the list `[1, …, 10]` (`seq` is the external coreutil, and prints bytes).


## Pipelines

`ral` pipes carry bytes from the stdout of a script to the stdin of another (UNIX-style). The *last* stage in a pipeline may also return a `ral` value.

Codecs bridge bytes to values: `from-line` takes `Bytes` to a `String` with no trailing `\n`, and `from-string` with it; `from-lines` gives a list of lines; `from-json` turns JSON bytes into a `ral` value:

    let cfg     = curl -s https://api.example.com/cfg | from-json
    let first   = head -n1 notes.txt
    let commits = git log --oneline | from-lines

Each decoder has an encoder, `to-…`, that takes values back to bytes. Text decoders require valid UTF-8, except `from-lines`, which replaces invalid sequences with `�`. Use `from-bytes` to preserve arbitrary bytes.

Decoders read from the byte channel. To decode bytes already stored in a value, use `bytes-to-string $r[stdout]`.

`from-json` and `from-jsonl` results are checked against how the script uses the value: a read of a field the file lacks, or a number used as text, fails at the decode, naming the JSON pointer (`/items/3/size`) and the line of the use. To read one value out of a document, `from-json-at [#'items'#, #'3'#, #'size'#] < $file` takes the path as a list of keys and indexes, never as a spliced string.

Only a pipeline's last stage may return a value; every earlier stage must write bytes. So `git log | from-lines | head -n5` and `lines $t | take 5` are refused, and value functions such as `bytes-to-string`, `lines`, `take`, `map` and `filter` never read stdin. Pipes carry bytes; a value function receives values, as arguments with its input last, so nest instead: `take 5 !{lines $t}`, `map { |c| $c[status] } !{commands $r}`.

## I/O

Use `<` for input, `>` for atomic output, `>>` to append, and `2>` for stderr. `2>&1` sends a command's stderr to its stdout destination:

    sort in.txt > sorted.txt 2> errors.txt

`warn "…"` writes one line to stderr and returns unit; use it instead of `1>&2`. Call `explain redirects` for all redirect forms and restrictions.

Read with `from-X < PATH`, write with `to-X $v > PATH`:

    let body  = from-string < $file    # String
    let rows  = from-lines < $file     # [String]
    let cfg   = from-json < $file      # record
    to-string $report >  $file         # write (atomic)
    to-string $report >> $file         # append
    to-json   $cfg    >  $file         # JSON write

<CRITICAL> Write a new file's literal body as a raw string, ideally with many hashes:

    echo #####'first line
    second 'quoted' line'##### > $file

To change an existing file, edit it.
</CRITICAL>

## Within

`within` is an effect handler that runs a block with a changed directory, environment, or handling of a command call:

    within [dir: #'exarch'#] { cargo check }
    within [env: [LOG_LEVEL: #'debug'#]] { convert-all }

Call `explain within` for command handlers and the full option syntax. A handler stubs a command; confinement comes from `grant`, below.

`grant` runs a block with reduced authority; it can only narrow what the surrounding frame permits:

    grant [net: `offline] { cargo check --offline }

Call `explain grant` for the capability fields, tagged paths, and examples.

Use `within` instead of `cd`. Paths in results are relative to the `within` directory, so consume them under the same `within`. `env:` values must be scalars.

## Failure

A failed call ends a `ral` script unless its failure is caught. Definitions that completed before the failure are still bound. Resume from the step that failed; do not replay the script from the top, which re-runs the writes and spawns that already happened.

Choose how to handle a failure: `try` runs a recovery handler, `audit` records the outcome and command output for inspection, and `attempt` deliberately ignores the failure. This also applies to tools whose exit codes carry data, such as `grep`, `diff`, and `test`; distinguish expected nonzero exits from actual errors.

`try`'s handler receives an error record, the same one an `audit` report's `` `err `` outcome carries. Call `explain try` for its fields:

    let log =
      try { sort in.txt 2>&1 | from-string } { |err|
        "sort failed: exited $err[status], $err[message]"
      }

`$err[reason]` is a variant saying why it failed; branch on its tag, as the `audit` example below does.

The handler block must start on the same line as the body's closing brace — `} { |err| … }`. `$err[message]` is synthetic status text; to see the failing command's output, wrap the call in `audit`.

Prelude functions cover common cases:

    if !{succeeds { grep -q ERROR log.txt }} { echo #'ERROR found'# } else { echo #'grep did not succeed'# }
    attempt { rm stale.lock }          # suppress any failure
    retry 3 { curl -s $url }           # up to 3 attempts

`guard BODY CLEANUP` runs the cleanup block whether the body succeeds or fails, then hands back the body's own result — so a failure keeps propagating. A cleanup that fails or exits pre-empts that result and becomes the outcome; write `guard BODY { attempt { … } }` if the cleanup's own failure should be ignored.

An error may be raised deliberately using e.g. `fail [status: 2, message: #'failure caused by x'#]`.


## Audit

`audit { … }` evaluates its body and returns a report with two fields: `outcome`, which is `` `ok `` of the value the body returned or `` `err `` of an error record, and `trail`, a flat list of everything the body did. `audit` turns recoverable runtime failures into data; `exit`, Ctrl-C, and the tool timeout stop the run instead. It keeps each command's stdout and stderr apart, so you need not `2>&1` to capture stderr.

Read a report with two prelude functions: `succeeded $r` is a `Bool`, and `commands $r` gives the trail's exec calls in order, one record per call. This is how you read a tool whose exit code is data, e.g. `grep` exit 1 meaning no match:

    let r = audit { grep -c ERROR app.log }
    case $r[outcome] [
      `ok:  { |_| bytes-to-string !{commands $r}[0][stdout] },
      `err: { |e|
        if $[$e[reason] == `exited 1] { #'no matches'# } else { fail $e }
      }
    ]

Each command retains at most 1 MiB per stream. `stdout-total` and `stderr-total` count everything written, so the bytes are complete exactly when the total equals `length $c[stdout]`. Capture includes intermediate pipeline output; all bytes still flow to their destination. Redirect large output to a file and read a bounded window when you need more.

Prefer `!{commands $r}` to a `case` on the trail whenever only commands matter.

## Running several commands

`audit` stops its body at the first uncaught failure and returns a report; it does not resume the remaining commands. Wrap each call in `attempt` to run every step despite failures, while the surrounding `audit` records them. For example, to see if three tools exist and what status they return:

    let probe = audit { attempt { cat --version }; attempt { sed --version }; attempt { python3 --version } }
    map { |c| [tool: $c[argv][0], status: $c[status]] } !{commands $probe}

One row per command; always prefer this to one blob of text you must re-parse. Inspect each command's status: `succeeded $probe` can be true even when commands failed, because `attempt` caught their failures.

When what you want is the merged text of every step, put the block on the byte channel — a pipe or a redirect takes what *every* part writes:

    let steps = { attempt { sort in.txt > sorted.txt }; attempt { wc -l sorted.txt } }
    let text  = !$steps | from-string     # every step's stdout, as one String
    !$steps > log.txt                     # or straight to a file

Do not use `sh -c`, as e.g. `sh -c #'a; b; c'#` payload throws away what `ral` would have told you — three commands collapse into one opaque child with one undifferentiated stdout, and a failure in the middle becomes invisible.

## Concurrency

`spawn "title" { … }` runs its block on a new thread and returns a handle at once. The title says what the work is; it is required here. Spawn long work, keep the handle, and do other things while it runs:

    let h = spawn "sort the log" { sort huge.txt > sorted.txt }

You need not wait for it: end the turn when nothing else remains, and the host wakes you when it finishes, naming its id, title, how long it ran and how it ended. Do not poll for completion.

`await $h` behaves exactly as if the block had run on that line: whatever it printed appears on your stdout and stderr, its value is returned, and a failure in it fails your script there, so `try` around `await` is how you recover. Awaiting again, even turns later, replays the same ending; it does not run the block again. An `exit` inside the block exits the turn that awaits it.

    let out = await $h          # a command's output, as `let out = { … }` would bind it
    try { await $h } { |e| echo "failed: $e[message]" }

If the binding is gone, the notice's id brings the handle back: `await !{worker 7}`. `workers` lists every worker with its id, title, owner, state and timing. `cancel $h` stops a block that is no longer needed, and everything it started.

A block may `spawn` inside itself: those workers belong to it, and it finishes only after they have, their output and failures folded into its own.

`poll $h` samples without waiting or printing: `pending` with the output so far and the seconds elapsed, or `settled` with the output, the time it took and `ok`/`err`. Use it to read a long-running server's output; to wait, `sleep 30; poll $h`.

Fan-out that must leave nothing behind is a combinator, not a set of spawns: `par f items jobs` maps in parallel and keeps input order; `all [build: { cargo build }, lint: { cargo clippy }]` runs several blocks and returns a record of their values; `race [$a, $b]` takes the first to finish and cancels the rest. Each ends every worker it started before it returns, and the first failure cancels the others and is raised.

<CRITICAL>A worker lives as long as this session. Nothing but its own completion, `cancel`, or the session's end stops it.</CRITICAL>

Should you wish for a process that runs *after* the session is over, use `detach`:

    let d = within [dir: #'/data'#] { detach #'background export job'# python3 export.py }

`detach` takes a task description, a binary (not a block!), and arguments. It returns a receipt; `await`, `poll`, and `cancel` do not apply. Stdin, stdout and stderr are `/dev/null`, so you will not receive updates. Call `explain detach` for monitoring and grant restrictions.
