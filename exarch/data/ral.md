Like every shell, `ral` runs commands:

    ls
    cat foo.txt | wc -l
    echo "hello" > /tmp/out

Commands are sequenced by newlines or `;` (there is no `&&`). An uncaught failure aborts the whole script: `mkdir out; cp notes.txt out/` runs `cp` only when `mkdir` succeeds. `?` runs the second command when the first failed: `cat VERSION ? #'unversioned'#` (there is no `||`). There is no trailing `&` either: background work is `defer { … }`, below. A `&` inside a word is an ordinary character, so `curl https://h/?a=1&b=2` needs no quotes. `\` is ordinary too, never an escape: `C:\Users\me` is one word.

`ral` is essentially call-by-push-value with recursion, recursive types, and one effect: an exec call. Its value types are `Unit`, `Bool`, `Int`, `Float`, String, Bytes, lists, records, maps, variants, thread handles, and blocks (= parameterized, thunked commands). A command may not be used as a value. Should you wish to use one inline, you must make it into an anonymous block and force it: `!{cmd}`.

## Definitions

`let x = 42` defines `x` to be `42`. Use it as `$x`. The right-hand side is a computation: if it returns a value, `let` binds that value; if it is a command, `let` captures its stdout:

    let today = date +%F                 # captured stdout: String
    let body  = from-string < notes.txt  # returned value: String
    let n     = line-count notes.txt     # returned value: Int
    echo "notes.txt ($n lines) as of $today"

<CRITICAL>
Bound variables are available for the rest of the session; you do not need to redefine them:

    echo "notes.txt now has !{line-count notes.txt} while it had $n before"
</CRITICAL>

Captured stdout from an external command is a `String`. `let n = line-count $file` binds an `Int` and `let files = list-dir #'.'#` binds a list.

A command's value is its output: `let host = hostname` binds what it writes, and so does `let host = f` when `f` is a block or function that runs a command. `| from-lines`, `| from-json` choose another decoding.

A binding shadows the command of the same name for the rest of the session, so don't name a binding `ls`, `head` or `tail`; `^ls` runs the command.

The shell's own state is read by six builtins: `cwd`, `env`, `args`, `user`, `home`, `nproc`. Use them as `let [a, b] = args`, `!{env}[HOME]`, `"!{cwd}/x"`.

A turn ending in `let` returns nothing; end with what you mean to see as `VALUE`.


## Blocks

A block packages a command as a value:

    let d = { date +%s }
    d                # runs date, forcing the block, even without !
    !$d              # the same, explicitly - NOT A NEGATION, SIMPLY FORCING A BLOCK
    !{date +%s}      # forcing an anonymous block, used in interpolation

Blocks may take space-separated, lexically-scoped, curried parameters:

    { ls }                                # thunk
    let print-file = { |path| cat $path } # one parameter
    let f = { |a b| $[$a + $b] }          # two parameters
    { |a, b| … }                          # PARSE ERROR — no commas

Blocks can be used with and as recursive higher-order functions, such as `map`, `filter`, `each`, `fold`, ...:

    map { |f| line-count $f } !{glob #'**/*.txt'#}
    filter { |h| re-match #'^src/'# $h[file] } $hits
    fold { |acc x| $[$acc + $x[size]] } 0 !{list-dir #'.'#}
    for $hits { |h| echo "$h[file]:$h[line]" }
    let in-src = { |h| re-match #'^src/'# $h[file] }
    filter $in-src $hits

Omitting the `$` in `$in-src` makes `in-src` just a string argument in the above.

You have the standard prelude found in functional programming, listed below; call `help` to find out more about a function.

Bindings are immutable but shadowable. Blocks capture lexically: earlier blocks keep the bindings they captured, so shadowing a helper does not change those blocks.

`let` accepts a computation on its right; an argument or record field needs a value. Force an anonymous block to use a computation there:

    let xs = [a, b, c]
    let n = length $xs
    [count: !{length $xs}]

`$xs` retrieves a value, `{ length $xs }` packages a computation, and `!{length $xs}` forces it. `$[…]` evaluates an arithmetic or Boolean expression; Boolean negation is `not`, as in `$[not !{is-file $path}]`.

## Strings

- Double quotes may be used to interpolate variables, fields, and forces:

      echo "hi $first-name $(last-name): $h[file] line $h[line], today !{date +%F | from-line}, sum $[2 + 3]"

- Parentheses delimit a variable inside an interpolated word: `"$(today)-checkout"`.
- Words never glue: `$dir/file` is a parse error; interpolate instead: `"$dir/file"`.
- In double quotes, a leading `~` or `~/` expands to the home directory, e.g. `ls "~/Work Invoices"`. `!{home}` is the same value.
- Escapes are a fixed set (`\n`, `\r`, `\t`, `\\`, `\"`, `\$`, `\!`, `\~`, `\0`, `\e`, `\xNN` for ASCII, `\u{…}`, and backslash-newline continuation).
- A raw string is written `#'…'#`: no escapes, no interpolation. If the content itself contains `'#`, add more hashes, e.g. `#####'…'#####`. Prefer this form for literal payloads passed to an external tool, e.g. `sed -i #'s|^old_value$|new_value|'# config.txt`. Pass computed values directly as `$arg`, or interpolate when constructing text.
- `dedent` strips the common leading indentation from a multiline string.
- There are no `<<EOF` heredocs. `cmd << $body` or `cmd << #'…'#` feeds a string to `cmd`'s stdin. One newline at the very start of the string is dropped, so the body can start on the line under the command.
- There is no `1>&2`. Say it with `warn "…"`, which puts one line on stderr and returns unit: a note for the human, off the byte channel a caller may be binding. `2> f` and `2>&1` are unchanged, for an external command's own stderr. Those, with `< f`, `<< str`, `> f`, `>> f` and `>~ f`, are the whole of ral's fd vocabulary: there is no fd plumbing, so `1< f`, `1> f`, `0< f`, `0> f` and any fd past 2 are parse errors rather than something quietly reinterpreted, and `echo 1>file` is refused rather than writing an empty file. Each stream takes at most one redirect (`cmd > a > b` and `2> e 2>&1` are parse errors), and `2>&1` means "stderr goes wherever stdout goes" whichever side of `> f` it is written.
- Split captured text explicitly with `lines $text`.
- Parse numeric text with `int $text` or `float $text`.

Search and replacement are regex builtins (Rust regex syntax — #'a|b'#, DO NOT USE ESCAPES `\|`):

    if !{re-match #'^WARN'# $line} { echo $line }
    re-replace-all #'\s+'# #' '# $s
    string-replace #'old_name'# #'new_name'# $s    # literal, must match exactly once

## Numbers and booleans

A bare word that looks like a number IS that number: `echo 007` prints `7`, `echo 1.50` prints `1.5`, and `echo #'3.100'#` prints `3.100`:

    let ver = '3.10'                 # a version is text; bare, it would be 3.1

Arithmetic and Boolean expressions must be in `$[…]` blocks: `$[$x == 0]`, `$[$a + $b]`, `$[$x > 0 && $x < 10]`, `$[not !{re-match #'x'# $s}]` are computed to values. Note that boolean negation is `not` (`!` forces). An operand may be any value — `$[$s == 'quit']` compares strings, and `==` is structural on every kind — but arithmetic and `<`/`>` want numbers. Such blocks do not nest; one layer suffices.

`if` takes a Boolean value and blocks, written out or in hand:

    if $[$s == #'quit'#] { #'bye'# } else { #'continuing'# }
    if    $[$x == 0] { #'zero'#     }
    elsif $[$x > 0 ] { f #'positive'# }
    else             { g #'negative'# }

`if` with both branches returns a value, so it can sit on the right of a `let`; anywhere else a value is expected, such as a collection entry, write `!{if …}`.

## Structured values

    let xs = [a, b, c]                   # list, with elements separated by commas
    $xs[0]                               # #'a'#
    let r = [host: #'h'#, port: 80]        # record — fixed, heterogeneous fields
    $r[host]                             # bare-key indexing
    let [first, ...rest] = $xs           # destructuring
    let wide = [...$xs, d, e]            # consing by spreading
    let at_end = [d, e, ...$xs]          # appending by spreading
    ls ...$flags ...$dirs                # …and to splice arguments

Indexing `$h[key]` works in any context (pipelines, blocks, double quoted): e.g. `view-text-around $h[file] $h[line] 3`.

An entry is a value: `[n: !{length $xs}, s: !{str $x}]`; `let` takes a command, a literal does not.

A map is not a record: its keys are runtime data and its values all share one type. A literal is a map if any key is data: `"quoted"` (`["a": 1, "b": 2]`) or computed (`[$k: 1]`). A bare or `'single-quoted'` key is a label, and labels alone make a record; `[:]` is the empty map. Only maps support `keys`, `values`, `has`, `get` (with default), `union`, `entries`, and only a map is read by a computed key (`$m[$k]`); a bare key like `$r[host]` always reads a record field.

When you write an rc file, a plugin manifest, or a capability profile — anything whose terminal `return` a host checks against a fixed keyset — write a record: `return [edit-mode: 'vi']`. A misspelled key is then a type error before the file runs at all. `return ["edit-mode": 'vi']` is a map, not a record, and `[]` is the empty *list*; both are type errors too. A file with nothing to set returns `()`.

A variant is a value tagged by a `` `tag ``, recording one of several outcomes along with some data, e.g. `` `file [bytes: 4096] ``:

    let probe = { |p|
      if    $[not !{exists $p}] { `absent }
      elsif !{is-dir $p}        { `dir  [entries: !{length !{list-dir $p}}] }
      else                      { `file [bytes: !{file-info $p}[size]] }
    }

`case` eliminates a variant. Its arms are syntax: one arm per tag, written out in place — a table of handlers held in a variable, or spliced in with `...`, is a parse error, because `case` must see every arm to prove it covers every tag. Each arm binds the tag's payload:

    case !{probe $path} [
      `absent: { |_| "$path: not found" },
      `dir:    { |d| "$path: directory, $d[entries] entries" },
      `file:   { |f| "$path: file, $f[bytes] bytes" }
    ]

A nullary tag still binds a value (`()`) — ignore it with `_`. An arm is a block `{ |p| … }` or a name holding a function: naming a function (`` `dir: $describe ``) applies it to the payload, and types and behaves exactly as `` `dir: { |d| $describe $d } `` does. Nothing else may stand in an arm, or in an `if` branch, because it would run before the form chose.

`range 1 11` returns the list `[1, …, 10]` (`seq` is the external coreutil, and prints bytes).


## Pipelines

`ral` pipes carry bytes from the stdout of a script to the stdin of another (UNIX-style). The *last* stage in a pipeline may also return a `ral` value.

Codecs bridge bytes to values: `from-line` takes `Bytes` to a `String` with no trailing `\n`, and `from-string` with it; `from-lines` gives a list of lines; `from-json` turns JSON bytes into a `ral` value:

    let cfg     = curl -s https://api.example.com/cfg | from-json
    let first   = head -n1 notes.txt
    let commits = git log --oneline | from-lines

There are also corresponding `to-line`, `to-string`, `to-lines`, `to-json` that take values to bytes. Text decoders require UTF-8; use `from-bytes` when bytes are not text.

Decoders read from the byte channel.  To decode bytes in a definition, use `bytes-to-string $r[stdout]`.

`from-json` and `from-jsonl` results are checked against how the script uses the value: a read of a field the file lacks, or a number used as text, fails at the decode, naming the JSON pointer (`/items/3/size`) and the line of the use. To read one value out of a document, `from-json-at ['items', '3', 'size'] < $file` takes the path as a list of keys and indexes, never as a spliced string.

A decoder ends the byte pipeline: its value never travels down a further `|`, so `git log --oneline | from-lines | head -n5` is refused — a stage feeds the next by writing, and `from-lines` returns a list. Bind the list, then `take 5 $commits`.

## I/O

Read with `from-X < PATH`, write with `to-X $v > PATH`:

    let body  = from-string < $file    # String
    let rows  = from-lines < $file     # [String]
    let cfg   = from-json < $file      # record
    to-string $report >  $file         # write (atomic)
    to-string $report >> $file         # append
    to-json   $cfg    >  $file         # JSON write

Write a file with `echo #'…'# > path`.

<critical>
For a literal multi-line file body, prefer a raw string, ideally with many hashes:

    echo #####'first line
    second 'quoted' line'##### > $file
</critical>

## Within

`within` is an effect handler that runs a block with a changed directory, environment, or handling of a command call:

    within [dir: #'exarch'#] { cargo check }
    let h = defer { within [env: [LOG_LEVEL: #'debug'#]] { convert-all } }
    within [ env : [ API_KEY : #''# ], handlers: [curl: { |args| echo #'offline stub'# }]] { fetch-all }
    let all_stubbed = { |name args| echo "stubbed: $name ...$args" }
    within [handler: $all_stubbed ] { run-all }
    within [handlers: [ curl: { |args| echo "stubbed" } ] ] { !$job }

A per-command `handlers:` entry is a one-arg function receiving argvs. Like `case`'s arms, the `handlers:` list is syntax: it is written out in place, because each name in it is bound in the body — a table held in a variable is a parse error, though an individual arm may be. The catch-all `handler:` is a two-arg function that intercepts EVERY external command, and is ordinary data, so it may be written out or bound. A handler stands in for a bare command name and writes what the command would write; `^name` runs the binary regardless. A handler is a stub, not a sandbox.

`within`'s brackets are the form's own, not a collection literal: `[]` is the empty option set, `[:]` names no options at all, and the option names are always written in the bracket — a bundle bound elsewhere (`within $o { … }`) or spread in is a parse error — while their values may be computed: `within [dir: $d] { … }`. A misspelled option is a static error naming the form's own list.

`grant` runs a block with reduced authority; it can only narrow what the surrounding frame permits:

    grant [net: `offline] { cargo check --offline }

Call `explain grant` for the capability fields, tagged paths, and examples.

Use `within` instead of `cd`. Paths in results are relative to the `within` directory, so consume them under the same `within`. `env:` values must be scalars.

## Failure

A failed call ends a `ral` script unless its failure is caught. Definitions that completed before the failure are still bound. Resume from the step that failed; do not replay the script from the top, which re-runs the writes and spawns that already happened.

`try` catches a failed command; without it, a non-zero exit aborts the entire script. Its handler receives an error record with fields `status`, `reason`, `cmd`, `message`, `site` (`` `some [script, line, col] `` or `` `none ``) — the same record an `audit` report's `` `err `` outcome carries, so failure reads the same either way:

    let log =
      try { sort in.txt 2>&1 | from-string } { |err|
        "sort failed: exited $err[status], $err[message]"
      }

`$err[reason]` says why it failed — `` `exited n ``, `` `signaled n ``, `` `cancelled ``, `` `timed-out ``, `` `not-found ``, `` `not-runnable ``, `` `raised `` — so branch on it rather than decoding `status`.

The handler block must start on the same line as the body's closing brace — `} { |err| … }`. `$err[message]` is synthetic status text, not the failing command's stderr; wrap a failing call in `audit` when you need to see stdout.

Do NOT use `try` for tools that report through their exit codes (`grep`, `diff`, `test`): wrap them in `audit` to read its output as data instead of raising, or `attempt { …  }` to merely suppress the error.

Prelude functions cover common cases:

    if !{succeeds { grep -q ERROR log.txt }} { echo #'clean'# } else { echo ###'broken'### }
    attempt { rm stale.lock }          # suppress any failure
    retry 3 { curl -s $url }           # up to 3 attempts

`guard BODY CLEANUP` runs the cleanup block whether the body succeeds or fails, then hands back the body's own result — so a failure keeps propagating. A cleanup that fails or exits pre-empts that result and becomes the outcome; write `guard BODY { attempt { … } }` if the cleanup's own failure should be ignored.

An error may be raised deliberately using e.g. `fail [status: 2, message: #'failure caused by x'#]`.


## Audit

`audit { … }` evaluates its body and returns a report with two fields: `outcome`, which is `` `ok `` of the value the body returned or `` `err `` of an error record, and `trail`, a flat list of everything the body did. `audit` turns recoverable runtime failures into data; `exit`, Ctrl-C, and the tool timeout stop the run instead. It keeps each command's stdout and stderr apart, so you need not `2>&1` to capture stderr.

Read a report with two prelude functions: `succeeded $r` is a `Bool`, and `commands $r` gives the trail's exec calls in order, each with its own `argv`, exit `status`, `stdout`, `stderr` and `error`. This is how you read a tool whose exit code is data, e.g. `grep` exit 1 meaning no match:

    let r   = audit { grep -c ERROR app.log }
    let out = bytes-to-string !{commands $r}[0][stdout]
    if !{succeeded $r} { $out } else { #'no matches'# }

Each command retains at most 1 MiB per stream. `stdout-total` and
`stderr-total` count everything written, so the bytes are complete exactly
when the total equals `length $c[stdout]`. Capture includes intermediate
pipeline output; all bytes still flow to their destination. Redirect large
output to a file and read a bounded window when you need more.

Prefer `!{commands $r}` to a `case` on the trail whenever only commands matter. When you want the failure itself, `case` on the outcome — the `` `err `` record is exactly what `try` hands its handler:

    case $r[outcome] [
      `ok:  { |v| $v },
      `err: { |e| echo $e[message] },
    ]

## Running several commands

If an exec call in a script fails, it aborts the script. This may be stopped by wrapping a call in `audit { … }`. For example, to see if three tools exist and what status they return:

    let probe = audit { attempt { cat --version }; attempt { sed --version }; attempt { python3 --version } }
    map { |c| [tool: $c[argv][0], status: $c[status]] } !{commands $probe}

One row per command, each with its own `argv`, `status`, `stdout` and `stderr`; always prefer this to one blob of text you must re-parse. `succeeds` answers a bare true/false:

    let has_python = !{succeeds { python3 --version > /dev/null }}

When what you want is the merged text of every step, put the block on the byte channel — a pipe or a redirect takes what *every* part writes:

    let steps = { attempt { sort in.txt > sorted.txt }; attempt { wc -l sorted.txt } }
    let text  = !$steps | from-string     # every step's stdout, as one String
    !$steps > log.txt                     # or straight to a file

In summary: `;` sequences, `attempt` tolerates a failure, `?` supplies a fallback, `2>` and `>` redirect, and `within [dir: …]` changes directory. Do not use `sh -c`, as e.g. `sh -c 'a; b; c'` payload throws away what `ral` would have told you — three commands collapse into one opaque child with one undifferentiated stdout, and a failure in the middle becomes invisible.

## Concurrency

`defer { … }` runs its block on new thread, returning a handle at once. As a general rule you should `defer` all long work:

    let b = { sort huge.txt > sorted.txt }
    let h = defer $b        # keep the handle!
    ##'sort started'##

If you truly have nothing else to do, `await` the handle with a long timeout.

`await` returns `[value, stdout, stderr]`. Its `stdout` and `stderr` are the thread's whole output, already merged across every command in it; its `value` is an `audit` report on the block, which you can examine to find what ran. An `audit` report has no top-level `stdout`; a thread's result does.

    let r = await $h
    let ok         = !{succeeded $r[value]}                             # did the block succeed?
    let thread_out = bytes-to-string $r[stdout]                         # everything the thread printed
    let cmd_stdout = bytes-to-string !{commands $r[value]}[0][stdout]   # stdout of first command
    case $r[value][outcome] [                                           # the block's own return value
      `ok:  { |v| $v },
      `err: { |e| fail $e },
    ]

Like `audit`, deferred blocks turn errors into data. `try` cannot catch Ctrl-C or the tool timeout: they stop the run. A worker you `cancel` reports `` `cancelled `` on `await` and in `poll`'s settled outcome; a worker that `exit`s ends the run that awaits or polls it. They are idempotent, so you can await the same handle across turns.

Use `cancel $h` to stop a thread that is no longer required.

`service` keeps work running for as long as this session lasts:

    let h = service #'watch the log file'# { tail -f app.log }

The first argument is a description of the task; the second is a block to run as a server. `service-handle ID` can be used to acquire a durable worker's handle by its id, so you can `await` or `cancel` it if you have forgotten the binding.

Important: The lifetime of a `defer` or `service` ends when the session ends.

Should you wish for a service that runs *after* the session is over, use `detach`:

    let d = within [dir: #'/data'#] { detach #'background export job'# python3 export.py }

`detach` takes a description of the task, a binary to call (not a block!), and some arguments. It then asks the OS to run this binary with these arguments, returning a receipt `[pid, desc]`. Stdin, stdout and stderr are `/dev/null`, so you will not receive any updates. Polling and killing can happen only through the OS — and, from inside a grant, only through what the survivor serves: each grant's children live in their own process namespace, so a later grant cannot `ps` or `kill` a survivor an earlier one detached.

## Exploring

The exploration tools skip dotfiles and `.gitignore`d files, unless stated otherwise.

Look at a directory with `ls`. To compute over its entries, `list-dir $path` returns every one as records `[name, type, size, mtime]`; `file-info`, `line-count` and `is-file`/`is-dir`/`exists` answer structured questions about a single path. To survey or search the tree:

- `glob #'**/*.txt'#` returns matching paths as a list. `.gitignore`d files match; a wildcard never matches a dotfile, so name one literally. Spread into a command: `mv ...!{glob …} out/`.
- `explore-dir 2 src` lists the entries beneath a directory to depth 2 as a list.
- `grep-files #'TODO'# src` searches a file, or a directory recursively.
- `grep-hidden #'API_KEY'# .` also searches hidden and `.gitignore`d files, but skips `.git`.
- `find-files #'query'# src` returns the files under a file or directory as `[String]`, fuzzy-ranked by path, best first and truncated. Use it to find files by name without a glob pattern.

Scope any of these with `within [dir: …]`. A walk skips only what it finds: a path you name is always searched, so `grep-files #'x'# target` searches a gitignored directory and `grep-files #'x'# .env` a dot file.

## Coreutils

All Rust uutils are available.

## Help

When you are unsure how to use something, call `explain <name>`. It covers language forms such as `grant` and `within` as well as builtins and prelude functions, and gives a standalone guide followed by labelled type and source facts. Do not reach for `which`, as that only covers executables in the PATH.
