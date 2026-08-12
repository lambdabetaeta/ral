Like every shell, `ral` runs commands:

    ls
    cat foo.txt | wc -l
    echo "hello" > /tmp/out

Commands are sequenced by newlines or `;`. An uncaught failure aborts the whole script: `./configure; make` runs `make` only when configuration succeeds. `?` runs the second command when the first failed: `cat VERSION ? #'unversioned'#`. There is no `&&` nor `||`.

`ral` is essentially call-by-push-value with recursion, recursive types, and one effect: an exec call. Its value types are `Unit`, `Bool`, `Int`, `Float`, String, Bytes, lists, records, maps, variants, thread handles, and blocks (= parameterized thunked commands). A command may not be used as a value. Should you wish to use one inline, you must make it into an anonymous block and force it: `!{cmd}`.

## Definitions

`let x = 42` defines `x` to be `42`. Use it as `$x`. When used with a command it captures stdout:

    let branch = git branch --show-current
    let body   = from-string < notes.txt
    let n      = line-count notes.txt
    echo "$branch has $n lines of notes"

The variables `branch`, `body`, and `n` are then **AVAILABLE IN EVERY RAL TOOL CALL, OVER EVERY TURN, FOR THE REST OF THE SESSION**. **YOU DO NOT NEED TO RE-DEFINE THEM IN THE NEXT TURN, JUST USE THEM AGAIN.**

Captured output is a `String`: split it with `lines`, parse it with `int`/`float`, or decode it by putting it back on the byte channel (`to-string $s | from-json`).

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

`ral` has one pipe, and it carries bytes from one command to the next (UNIX-style). Every interior edge is bytes on both sides; only the *last* stage may return a value. Values are combined by ordinary application (`f $x`) and `let`, never by `|`: `[1,2] | length` is a type error, `length [1,2]` is the spelling.

Codecs bridge the world of bytes to the world of values: `from-line` takes `Bytes` to a `String` with no trailing `\n`, and `from-string` with it; `from-lines` gives a lazy stream of `String`; `from-json` turns JSON into a `ral` value. Decode where the bytes flow, and capture only after decoding:

    let cfg = curl -s https://api.example.com/cfg | from-json
    let os  = !{uname -s | from-line}

There are also corresponding `to-line`, `to-string`, `to-lines`, `to-json` that take values to bytes. Text decoders require UTF-8; use `from-bytes` when bytes are not text. 

Decoders read from the byte channel.  To decode bytes in a definition, use `bytes-to-string $r[stdout]`.

`from-lines` yields a lazy stream, which no function iterates implicitly. A decoder ends its pipeline, so bind the stream and eliminate it explicitly — `stream-each { |l| … } !{git log | from-lines}`, or `stream-to-list`/`stream-map`/`stream-fold` — or skip streams: `lines` splits a String into a materialised list, and `from-lines-list PATH` reads a file as a materialised list of lines:

    let commits = lines !{git log --oneline -5 | from-string}
    let src     = from-lines-list #'src/main.rs'#

## Strings

- Double quotes may be used to interpolate variables, fields, and forces:

      echo "hi $first-name $(last-name): $h[file] line $h[line], host !{hostname | from-line}, sum $[2 + 3]"

  `$(name)` delimits variables from post-fixes that do not belong to them. A composite path must be one quoted word: `echo hi > "$dir/file"`.

* Escapes are a fixed set (`\n`, `\r`, `\t`, `\\`, `\"`, `\$`, `\!`, `\0`, `\e`, `\xNN` for ASCII, `\u{…}`, and backslash-newline continuation).
- Raw strings `#'…'#` are the verbatim string form: no escapes, no interpolation. They can contain any character including apostrophes. If the content itself contains `'#`, add more hashes. A hash not followed by a single-quote starts a comment to end of line.

  A raw string is also how an argument thick with metacharacters reaches an external tool: ral passes it through as one word, untouched, so a `sed` script needs no escaping at all and there is never a reason to hand it to a shell instead.

      sed -i #'s|^INCLUDE_DIRS := $(PYTHON_INCLUDE)|INCLUDE_DIRS := /usr/include/opencv4|'# Makefile.config

- `dedent` strips the common leading indentation from a multiline string.
- There are no `<<EOF` heredocs. `cmd << #'…'#` (space after `<<` required) feeds the string to `cmd`'s stdin (a stored string works too: `cmd << $body`). One newline at the very front of the string is dropped, so the body can start on the line under the command. Write a file with `echo #'…'# > path`.

Search and replacement are regex builtins (Rust regex syntax — #'a|b'#, DO NOT USE ESCAPES `\|`):

    if !{re-match #'^WARN'# $line} { echo $line }
    re-replace-all #'\s+'# #' '# $s
    string-replace #'old_name'# #'new_name'# $s    # literal, must match exactly once

## Numbers and booleans

Arithmetic and Boolean expressions must be in `$[…]` blocks: `$[$x == 0]`, `$[$a + $b]`, `$[$x > 0 && $x < 10]`, `$[not !{re-match #'x'# $s}]` are computed to values. Note that negation is `not` (`!` forces). `$[…]` admits numbers and booleans only — string equality is the command `equal`, ordering `lt`/`gt`. Such blocks do not nest; one layer suffices.

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

A nullary tag still binds a value (`unit`) — ignore it with `_`. An arm's body may be any expression, not only `{ |p| … }`: naming a function (`` `dir: $describe ``) applies it to the payload, and types and behaves exactly as `` `dir: { |d| $describe $d } `` does. 

`range 1 11` returns the list `[1, …, 10]` (`seq` is the external coreutil, and prints bytes).


## Failure

`try` catches a failed command; without it, a non-zero exit aborts the entire script. When a tool reports through its exit code rather than failing (`grep`, `diff`, `test`, `valgrind --error-exitcode`), wrap it in `audit` to read its output as data instead of raising.

Its handler receives an error record with fields `status`, `cmd`, `message`, `line`, `col`:

    let log =
      try { make 2>&1 | from-string } { |err| 
        "make failed: exited $err[status], $err[message]"
      }

The handler block must start on the same line as the body's closing brace — `} { |err| … }`. `err[message]` is synthetic status text, not the failing command's stderr; wrap in `audit` when output is the data you need.

Prelude functions cover common cases:

    if !{succeeds { cargo check -q }} { echo #'clean'# } else { echo #'broken'# }
    attempt { rm stale.lock }          # suppress any failure
    retry 3 { curl -s $url }           # up to 3 attempts

`guard BODY CLEANUP` runs the cleanup block if the body fails, then propagates the failure. `fail [status: 2, message: #'…'#]` raises deliberately.

## Audit

`audit { … }` evaluates its body and returns a report with exactly four fields and no others: exit `status`, the ral `value` the body returned, an `error` string, and `children`. `children` is the flat list of what the body did. Every entry carries a `kind`; a `command` entry then carries its own `argv`, `status`, `stdout` and `stderr` (the last two `bytes`), while a `read`, `write` or `capability-check` entry carries different fields — so `argv` and `stdout` project off `children` only when the body ran nothing but commands, and a redirect in the body narrows every entry to `kind` alone. The report has no `stdout` field of its own: per-command output lives in `children`, and the merged output of a whole block comes from piping the block instead (see *Running several commands*). `audit` turns any errors into record data, so it never fails, and it keeps each command's stdout and stderr apart, so you need not `2>&1` to capture stderr. This is how you read a tool whose exit code is *data* (e.g. `grep` exit 1 meaning no match), or a deliberate signal like `valgrind --error-exitcode=77`:

    let r      = audit { valgrind --error-exitcode=77 --leak-check=full ./a.out }
    let report = bytes-to-string $r[children][0][stdout]
    if $[ $r[status] == 77 ] { "leaks:\n$report" } else { #'clean'# }

## Running several commands

A block runs its parts in order and abandons the rest at the first failure, so `audit { a; b; c }` reports on the steps that ran and says nothing of the steps it never reached. To run a whole battery and see every outcome, wrap each step in `attempt`:

    let probe = audit { attempt { git --version }; attempt { cmake --version }; attempt { nvcc --version } }
    map { |c| [tool: $c[argv][0], status: $c[status]] } $probe[children]

One row per command, each with its own `argv`, `status`, `stdout` and `stderr` — always prefer this to one blob of text you must re-parse. `succeeds` answers a bare yes/no without a report at all:

    map { |t| [tool: $t, ok: !{succeeds { which $t > /dev/null }}] } [#'git'#, #'cmake'#]

When what you want is the merged text of every step, put the block on the byte channel — a pipe or a redirect takes what *every* part writes:

    let steps = { attempt { make }; attempt { make test } }
    let text  = !$steps | from-string     # every step's stdout, as one String
    !$steps > build.log                   # or straight to a file

Beware the one capture that does not merge: `let x = !$steps` binds the *final* command's output alone, and the earlier steps print visibly instead. Reach for `| from-string` whenever a block has more than one part.

So never reach for `sh -c` to sequence commands, to fall back after a failure, or to change directory first: `;` sequences, `attempt` tolerates a failure, `?` supplies a fallback, `2>` and `>` redirect, and `within [dir: …]` changes directory. A `sh -c 'a; b; c'` payload throws away what ral would have told you — three commands collapse into one opaque child with one undifferentiated stdout, and a failure in the middle becomes invisible.

## Concurrency

`defer { … }` runs its block on new thread, returning a handle at once. As a general rule you should `defer` all long work:

    let b = { make } 
    let h = defer $b        # keep the handle!
    #'build started'#

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

`detach` takes a description of the task, a binary to call (not a block!), and some arguments. It then asks the OS to run this binary with these arguments, returning a receipt `[pid, desc]`. Stdin, stdout and stderr are `/dev/null`, so you will not receive any updates. Polling that process can only happen using its own logs. Killing this process can only be done with OS primitives, e.g. `kill`. If you detach inside a `grant`, that grant's limits stay on the process for as long as it runs, and nothing later can lift them; a grant may also refuse the call outright with `detach: false`.

## Within

`within` is an effect handler that runs a block with a changed directory, environment, or handling of a command call:

    within [dir: #'src'#] { grep-files #'TODO'# }
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

Multi-line text with awkward quotes should go through a raw string:

    to-string #'first line
    second 'quoted' line'# > $file

## Exploring

Use the following to search for files; all are `.gitignore` sensitive. Use these instead of `rg`/`find`/`ls`.

- `glob #'src/**/*.rs'#` — matching paths as a ral list; skips dot files. Spread into a command: `mv ...!{glob …} out/`.
- `explore-dir n` — entries of the current directory to depth `n` as a `ral` list; `.gitignore`-aware
- `grep-files #'fn \w+_test'#` — recursive grep of the current directory (Rust regex syntax); returns `ral` list of records `[file, line, text]`.
- `fff #'query'#` — fuzzy file-name search (frecency-ranked) over the working tree, returning `[String]`. Use to find files by name without a glob pattern.
- `list-dir`, `file-info`, `line-count`, `is-file`/`is-dir`/`exists` — structured metadata without parsing `ls`.

Scope any of these with `within [dir: …]`.

For dot/ignored files you also have `rg` bundled.

## Help

When you are unsure of the signature of something you always call `explain <name>`.
