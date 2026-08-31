Like every shell, `ral` runs commands:

    ls
    cat foo.txt | wc -l
    echo "hello" > /tmp/out

Commands are sequenced by newlines or `;` (there is no `&&`). An uncaught failure aborts the whole script: `./configure; make` runs `make` only when configuration succeeds. `?` runs the second command when the first failed: `cat VERSION ? #'unversioned'#` (there is no `||`). There is no trailing `&` either: background work is `defer { … }`, below.

`ral` is essentially call-by-push-value with recursion, recursive types, and one effect: an exec call. Its value types are `Unit`, `Bool`, `Int`, `Float`, String, Bytes, lists, records, maps, variants, thread handles, and blocks (= parameterized, thunked commands). A command may not be used as a value. Should you wish to use one inline, you must make it into an anonymous block and force it: `!{cmd}`.

## Definitions

`let x = 42` defines `x` to be `42`. Use it as `$x`. When used with a command it captures stdout:

    let branch = git branch --show-current
    let body   = from-string < notes.txt
    let n      = line-count notes.txt
    echo "$branch has $n lines of notes"

<critical>
Bound variables are **AVAILABLE IN EVERY TURN, FOR THE REST OF THE SESSION**. **YOU DO NOT NEED TO RE-DEFINE THEM IN THE NEXT TURN, JUST USE THEM AGAIN.** In the following turn do NOT re-bind `n`, just use it again:

    echo "notes.txt now has !{line-count notes.txt} while it had $n before"
</critical>

Captured stdout from an external command is a `String`; ral heads may instead return structured values. For example, `let text = git log --oneline` binds a `String`, while `let n = line-count $file` binds an `Int` and `let files = list-dir #'.'#` binds a list. Split captured text explicitly with `lines $text`, and parse numeric text with `int $text` or `float $text`.

Top-level value names may not collide with commands reachable on `PATH`. Avoid names such as `head`, `tail`, `test`, and `date`; prefer descriptive names such as `commit-sha`, `tag-lines`, and `release-date`.

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

Blocks can be used with higher-order functions, such as `map`, `filter`, `each`, `fold`, ...:

    map { |f| line-count $f } !{glob #'src/**/*.rs'#}
    filter { |h| re-match #'^src/'# $h[file] } $hits
    fold { |acc x| $[$acc + $x[size]] } 0 !{list-dir #'.'#}
    for $hits { |h| echo "$h[file]:$h[line]" }
    let in-src = { |h| re-match #'^src/'# $h[file] }
    filter $in-src $hits

Omitting the `$` in `$in-src` makes `in-src` just a string argument in the above.

You have the standard prelude found in functional programming, listed below; call `help` to find out more about a function.

Blocks support recursive definitions.

## Pipelines

`ral` pipes carry bytes from the stdout of a script to the stdin of another (UNIX-style). The *last* stage in a pipeline may also return a `ral` value.

Codecs bridge bytes to values: `from-line` takes `Bytes` to a `String` with no trailing `\n`, and `from-string` with it; `from-lines` gives a lazy stream of `String`; `from-json` turns JSON bytes into a `ral` value:

    let cfg = curl -s https://api.example.com/cfg | from-json
    let os  = !{uname -s | from-line}

There are also corresponding `to-line`, `to-string`, `to-lines`, `to-json` that take values to bytes. Text decoders require UTF-8; use `from-bytes` when bytes are not text. 

Decoders read from the byte channel.  To decode bytes in a definition, use `bytes-to-string $r[stdout]`.

`from-lines` yields a lazy stream, which no function iterates implicitly. A decoder ends the byte pipeline, so its resulting stream is a value: bind or force it, then pass it to a stream eliminator:

    let stream  = !{git log | from-lines}
    let commits = stream-to-list $stream

Do not write `git log | from-lines | stream-to-list`: the second pipe expects bytes, but `from-lines` has already produced a ral value. For small finite output, prefer `lines`; `from-lines-list PATH` reads a file directly as a materialised list of lines:

    let commits = lines !{git log --oneline -5 | from-string}
    let src     = from-lines-list #'src/main.rs'#

## Audit

`audit { … }` evaluates its body and returns a report with four fields: exit `status`, the ral `value` the body returned, an `error` string, and `children`. `children` is a flat list of exec calls the body made, including its `argv`, exit `status`, `stdout` and `stderr`. `audit` turns any errors into record data, so it never fails, and it keeps each command's stdout and stderr apart, so you need not `2>&1` to capture stderr. This is how you read a tool whose exit code is data, e.g. `grep` exit 1 meaning no match, or a `valgrind --error-exitcode=77`:

    let r      = audit { valgrind --error-exitcode=77 --leak-check=full ./a.out }
    let report = bytes-to-string $r[children][0][stdout]
    if $[ $r[status] == 77 ] { "leaks:\n$report" } else { #'clean'# }

## Running several commands

If an exec call in a script fails, it aborts the script. This may be stopped by wrapping a call in `audit { … }`. For example, to see if three binaries exist and what status they return:

    let probe = audit { attempt { git --version }; attempt { cmake --version }; attempt { nvcc --version } }
    map { |c| [tool: $c[argv][0], status: $c[status]] } $probe[children]

One row per command, each with its own `argv`, `status`, `stdout` and `stderr`; always prefer this to one blob of text you must re-parse. `succeeds` answers a bare true/false:

    map { |t| [tool: $t, ok: !{succeeds { which $t > /dev/null }}] } [#'git'#, #'cmake'#]

When what you want is the merged text of every step, put the block on the byte channel — a pipe or a redirect takes what *every* part writes:

    let steps = { attempt { make }; attempt { make test } }
    let text  = !$steps | from-string     # every step's stdout, as one String
    !$steps > build.log                   # or straight to a file

Beware: `let text = !$steps` only binds the *final* command's stdout (i.e. `make test`), dropping the stdout of earlier steps. Reach for `| from-string` whenever a block has more than one part.

In summary: `;` sequences, `attempt` tolerates a failure, `?` supplies a fallback, `2>` and `>` redirect, and `within [dir: …]` changes directory. Do not use `sh -c`, as e.g. `sh -c 'a; b; c'` payload throws away what `ral` would have told you — three commands collapse into one opaque child with one undifferentiated stdout, and a failure in the middle becomes invisible.

## Strings

- Double quotes may be used to interpolate variables, fields, and forces:

      echo "hi $first-name $(last-name): $h[file] line $h[line], host !{hostname | from-line}, sum $[2 + 3]"

  `$(name)` delimits variables from post-fixes that do not belong to them. A composite path must be one quoted word: `echo hi > "$dir/file"`.

* Escapes are a fixed set (`\n`, `\r`, `\t`, `\\`, `\"`, `\$`, `\!`, `\0`, `\e`, `\xNN` for ASCII, `\u{…}`, and backslash-newline continuation).
- Raw strings `#'…'#` are the verbatim string form; they need NO escapes and carry NO interpolation. They can contain any character including apostrophes. If the content itself contains `'#`, add more hashes, e.g. `#####'…'#####`. A hash not followed by a single-quote starts a comment to end of line.

  A raw string is also how an argument thick with metacharacters reaches an external tool: ral passes it through as one word, untouched, so a `sed` script needs no escaping at all and there is never a reason to hand it to a shell instead.

      sed -i #'s|^INCLUDE_DIRS := $(PYTHON_INCLUDE)|INCLUDE_DIRS := /usr/include/opencv4|'# Makefile.config

- `dedent` strips the common leading indentation from a multiline string.
- There are no `<<EOF` heredocs. `cmd << #'…'#` (space after `<<` required) feeds the string to `cmd`'s stdin (a stored string works too: `cmd << $body`). One newline at the very front of the string is dropped, so the body can start on the line under the command. Write a file with `echo #'…'# > path`.
- There is no `1>&2`. Say it with `warn "…"`, which puts one line on stderr and returns unit: a note for the human, off the byte channel a caller may be binding. `2> f` and `2>&1` are unchanged, for an external command's own stderr.

Search and replacement are regex builtins (Rust regex syntax — #'a|b'#, DO NOT USE ESCAPES `\|`):

    if !{re-match #'^WARN'# $line} { echo $line }
    re-replace-all #'\s+'# #' '# $s
    string-replace #'old_name'# #'new_name'# $s    # literal, must match exactly once

## Numbers and booleans

A bare word that looks like a number IS that number, in every position — argument, value, list element — and quoting is how you ask for the text instead. Each number has one printed spelling, so a non-canonical numeral comes back normalised: `echo 007` prints `7`, `echo 1.50` prints `1.5`, and a whole float keeps its point (`3.0`). Note that `3.10` is the number 3.1: version-like tokens MUST be quoted.

    let ver = '3.10'                 # a version is text; bare, it would be 3.1

Arithmetic and Boolean expressions must be in `$[…]` blocks: `$[$x == 0]`, `$[$a + $b]`, `$[$x > 0 && $x < 10]`, `$[not !{re-match #'x'# $s}]` are computed to values. Note that boolean negation is `not` (`!` forces). `$[…]` admits numbers and booleans only — string equality is the command `equal`, ordering `lt`/`gt`. Such blocks do not nest; one layer suffices.

`if` takes a Boolean value and blocks:

    if !{equal $s #'quit'#} { #'bye'# } else { #'continuing'# }  # WARNING: ! HERE IS NOT NEGATION, IT IS FORCING A BLOCK
    if    $[$x == 0] { #'zero'#     }
    elsif $[$x > 0 ] { f #'positive'# }
    else             { g #'negative'# }

`if` with both branches returns a value, so it can sit on the right of a `let`.

## Structured values

    let xs = [a, b, c]                   # list — commas, not spaces
    $xs[0]                               # #'a'#
    let r = [host: #'h'#, port: 80]        # record — fixed, heterogeneous fields
    $r[host]                             # bare-key indexing
    let [head, ...rest] = $xs            # destructuring
    let wide = [...$xs, d, e]            # consing by spreading
    let at_end = [d, e, ...$xs]          # appending by spreading
    ls ...$flags ...$dirs                # …and to splice arguments

Indexing `$h[key]` works in any context (pipelines, blocks, double quoted): e.g. `view-text-around $h[file] $h[line] 3`.

A map is a homogeneous record; only maps support `keys`, `values`, `has`, `get` (with default), `union`, `entries`. `[:]` is the empty map. 

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

A nullary tag still binds a value (`()`) — ignore it with `_`. An arm's body may be any expression, not only `{ |p| … }`: naming a function (`` `dir: $describe ``) applies it to the payload, and types and behaves exactly as `` `dir: { |d| $describe $d } `` does. 

`range 1 11` returns the list `[1, …, 10]` (`seq` is the external coreutil, and prints bytes).


## Failure

A failed call ends a `ral` script, much like `set -euo pipefail` in `bash`.

`try` catches a failed command; without it, a non-zero exit aborts the entire script. Its handler receives an error record with fields `status`, `cmd`, `message`, `line`, `col`:

    let log =
      try { make 2>&1 | from-string } { |err| 
        "make failed: exited $err[status], $err[message]"
      }

The handler block must start on the same line as the body's closing brace — `} { |err| … }`. `$err[message]` is synthetic status text, not the failing command's stderr; wrap a failing call in `audit` when you need to see stdout.

Do NOT use `try` for tools that report through their exit codes (`grep`, `diff`, `test`, `valgrind --error-exitcode`): wrap them in `audit` to read its output as data instead of raising, or `attempt { …  }` to merely suppress the error.

Prelude functions cover common cases:

    if !{succeeds { cargo check -q }} { echo #'clean'# } else { echo ###'broken'### }
    attempt { rm stale.lock }          # suppress any failure
    retry 3 { curl -s $url }           # up to 3 attempts

`guard BODY CLEANUP` runs the cleanup block whether the body succeeds or fails, then hands back the body's own result — so a failure keeps propagating. A cleanup that fails or exits pre-empts that result and becomes the outcome; write `guard BODY { attempt { … } }` if the cleanup's own failure should be ignored. 

An error may be raised deliberately using e.g. `fail [status: 2, message: #'failure caused by x'#]`.


## Concurrency

`defer { … }` runs its block on new thread, returning a handle at once. As a general rule you should `defer` all long work:

    let b = { make } 
    let h = defer $b        # keep the handle!
    ##'build started'##

If you truly have nothing else to do, `await` the handle with a long timeout.

`await` returns `[value, stdout, stderr]`. Its `stdout` and `stderr` are the thread's whole output, already merged across every command in it; its `value` is an `audit` report on the block, which you can examine to find what ran. An `audit` report has no top-level `stdout`; a thread's result does.

    let r = await $h
    let ok         = $[$r[value][status] == 0]                       # did the block succeed?
    let thread_out = bytes-to-string $r[stdout]                      # everything the thread printed
    let cmd_stdout = bytes-to-string $r[value][children][0][stdout]  # stdout of first command
    let result     = $r[value][value]                                # the block's own return value

Like `audit`, deferred blocks turn errors into data. They are idempotent, so you can await the same handle across turns.

Use `cancel $h` to stop a thread that is no longer required.

`service` keeps work running for as long as this session lasts:

    let h = service #'watch the test log'# { tail -f test.log }

The first argument is a description of the task; the second is a block to run as a server. `service-handle ID` can be used to acquire a durable worker's handle by its id, so you can `await` or `cancel` it if you have forgotten the binding.

Important: The lifetime of a `defer` or `service` ends when the session ends.

Should you wish for a service that runs *after* the session is over, use `detach`: 

    let d = within [dir: #'/app'#] { detach #'gRPC KV store on port 5328'# python server.py }

`detach` takes a description of the task, a binary to call (not a block!), and some arguments. It then asks the OS to run this binary with these arguments, returning a receipt `[pid, desc]`. Stdin, stdout and stderr are `/dev/null`, so you will not receive any updates. Polling and killing can happen only through the OS. 

## Within

`within` is an effect handler that runs a block with a changed directory, environment, or handling of a command call:

    within [dir: #'src'#] { grep-files ##'#TODO'## }
    let h = defer { within [env: [RUST_LOG: #'debug'#]] { cargo run } }
    within [ env : [ API_KEY : #''# ], handlers: [curl: { |args| #'offline stub'# }]] { fetch-all }
    let all_blocked = { |name args| echo "blocked: $name ...$args" }
    within [handler: $all_blocked ] { make deploy }
    within [handlers: [ git: { |args| echo "git blocked" } ] ] { !$deploy }

A per-command `handlers:` entry is a one-arg function receiving argvs. The catch-all `handler:` is a two-arg function that intercepts EVERY external command.

Use `within` instead of `cd`. Paths in results are relative to the `within` directory, so consume them under the same `within`. `env:` values must be scalars. 

## I/O

Read with `from-X < PATH`, write with `to-X $v > PATH`:

    let body  = from-string < $file    # String
    let rows  = from-lines-list $file  # [String]
    let cfg   = from-json < $file      # record
    to-string $report >  $file         # write (atomic)
    to-string $report >> $file         # append
    to-json   $cfg    >  $file         # JSON write

<critical>
Writing a multi-line file must use a raw string, ideally with many hashes:

    to-string #####'first line
    second 'quoted' line'##### > $file
</critical>

## Exploring

Use the following instead of `rg`/`find`/`ls` to search for files; all are `.gitignore` sensitive. 

- `glob #'src/**/*.rs'#` — matching paths as a ral list; skips dot files. Spread into a command: `mv ...!{glob …} out/`.
- `explore-dir n` — entries of the current directory to depth `n` as a `ral` list; `.gitignore`-aware
- `grep-files #'fn \w+_test'#` — recursive grep of the current directory (Rust regex syntax); returns `ral` list of records `[file, line, text]`.
- `fff #'query'#` — fuzzy file-name search (frecency-ranked) over the working tree, returning `[String]`. Use to find files by name without a glob pattern.
- `list-dir`, `file-info`, `line-count`, `is-file`/`is-dir`/`exists` — structured metadata without parsing `ls`.

Scope any of these with `within [dir: …]`.

<critical>
For dot files and gitignored files you must use `rg` bundled.
</critical>

## Help

When you are unsure of the signature of something you always call `explain <name>`.

Do not reach for `which`. It is a PATH lookup, so it cannot see anything ral
itself provides: it reports `/bin/echo` for `echo`, which never runs, and fails
outright on `length` or any other builtin — a failure that stops the rest of
the block. `explain <name>` names the frame that actually runs and, on its last
line, whatever that shadows, the PATH binary included.
