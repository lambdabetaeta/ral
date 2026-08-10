# ral — a tutorial

Like every shell, `ral` runs commands:

    ls
    cat notes.txt | wc -l
    echo "hello" > greeting.txt

It also has values, functions, structured data, typed pipelines, concurrency,
and scoped authority. You do not need those ideas all at once.

This tutorial starts with the shell you need today. Each section adds one idea.
The last sections explain the model underneath.

## 1  Commands

A command name comes first. Its arguments follow:

    ls -la /tmp
    git status --short
    make -j4

`|` connects commands. External commands send bytes through the pipe:

    cat access.log | grep ' 500 ' | wc -l

Newlines and `;` sequence commands. Failure stops the sequence, so `make` runs
only if `./configure` succeeds:

    ./configure
    make
    make install

`?` supplies a fallback:

    curl -fsS $primary ? curl -fsS $fallback
    cat VERSION ? echo unversioned

There is no command-level `&&` or `||`. Sequencing already stops on failure;
`?` handles the failure.

Redirects look familiar:

    command > output.txt
    command >> log.txt
    command < input.txt
    command 2> errors.txt
    command > output.txt 2>&1

`>` is atomic for regular files: a reader sees the old file or the complete new
file, never a half-written file. `>~` is the streaming form for live logs and
FIFOs.

## 2  Values and bindings

`let` gives a value a name. Use `$name` to retrieve it:

    let name  = 'Ada'
    let port  = 8080
    let debug = true

    echo "$name is listening on $port"

The first important rule is:

> A bare word in command position runs a command. A quoted word is data.

These definitions do different things:

    let greeting = 'hello'   # bind the String "hello"
    let greeting = hello     # run hello and bind its stdout

Capturing a byte-producing command gives you its UTF-8 stdout as a `String`,
with one trailing newline removed:

    let branch = git branch --show-current
    let host   = hostname
    let count  = wc -l < notes.txt

    echo "$branch on $host has $count lines of notes"

A bare word runs a command only at the head of a command. In a list, record,
argument, or `return`, it is a string:

    let colours = [red, green, blue]
    echo green

Bindings are immutable. A later `let` shadows an earlier binding; it does not
change it:

    let n = 1
    let n = $[$n + 1]

ral never word-splits a value:

    let file = 'my report.txt'
    rm $file                       # exactly one argument

Quote when you want to join values and text into one argument:

    echo "$dir/report.txt"
    curl "$host:$port/api"

Outside quotes, `$dir/file` is not concatenation.

### Strings

Single quotes are literal. Double quotes interpolate:

    'literal: $name \n !{date}'
    "hello $name"
    "host !{hostname}, sum $[2 + 3]"

Inside double quotes, `$name` inserts a binding, `$record[field]` inserts a
field, `!{command}` runs a command, and `$[…]` computes an expression. The
usual explicit escapes include `\n`, `\t`, `\\`, `\"`, `\xNN`, and `\u{…}`.

Only scalars interpolate directly. Convert collections, bytes, blocks, and
handles explicitly:

    echo "items: !{str $items}"

`$(name)` marks the end of a name:

    echo "$(name)[old]"
    echo "$(os)-$arch"

Raise the hash level when literal text contains apostrophes:

    let message = #'it's working'#
    let code = ##'
    print('hello')
    '##

Both string forms may span lines. `dedent` removes common indentation:

    let query = dedent '
        SELECT name
        FROM users
    '

There are no heredocs. `<<` feeds a string to stdin:

    sqlite3 app.db << $query
    cat << #'
    first line
    second line
    '#

## 3  Blocks and functions

A block `{ … }` stores a command without running it:

    let clock = { date +%s }

Force it to run:

    clock
    !$clock
    !{date +%s}

A bound block in head position is forced implicitly. `!$clock` is explicit.
`!{…}` makes and forces an anonymous block, which is useful inside an argument
or expression.

`!` means **force**, never logical negation. Logical negation is `not`.

Blocks may take space-separated parameters:

    let greet = { |name|
        echo "hello, $name"
    }

    greet Ada
    greet 'Grace Hopper'

Commas do not separate parameters:

    { |left right| $[$left + $right] }   # two parameters
    { |left, right| unit }               # parse error

A block returns its last command's result. `return` leaves early:

    let absolute = { |x|
        if $[$x < 0] { return $[-$x] }
        return $x
    }

Blocks are lexically scoped. They remember the values visible at definition,
and their local bindings disappear at the closing brace:

    let outer = 'kept'

    !{
        let temporary = 'gone soon'
        echo "$outer, $temporary"
    }

    echo $temporary   # error: not bound here

Multi-parameter blocks are curried:

    let add      = { |x y| return $[$x + $y] }
    let add-five = add 5
    add-five 3                              # 8

A function in ral is a parameterised, stored command.

## 4  Expressions and control flow

Arithmetic, comparison, and Boolean logic live in `$[…]`:

    $[$x + 1]
    $[$width * $height]
    $[$count % 2]
    $[$x >= 0 && $x < 10]
    $[not $done]

Arithmetic requires numbers. `&&`, `||`, and `not` require booleans; there is no
truthiness conversion. `/` on two integers is integer division.

`if` takes a `Bool` and block branches:

    if $debug { echo 'debug logging enabled' }

    if !{equal $mode 'check'} {
        echo checking
    } elsif !{equal $mode 'build'} {
        echo building
    } else {
        fail [status: 2, message: "unknown mode: $mode"]
    }

A two-armed `if` returns a value:

    let sign =
        if $[$n < 0] { return negative }
        else         { return nonnegative }

`equal` compares values. `lt` and `gt` compare strings lexicographically.

Failure and falsehood are different:

    if !{exists $path} { echo present }   # branch on Bool
    cat $path ? echo missing              # recover from failure

`exists` returning `false` is a successful computation. `?` reacts only to
failure.

## 5  Structured values

Lists and records share square brackets:

    let hosts  = [web-1, web-2, web-3]
    let server = [host: 'db.example.com', port: 5432]

    $hosts[0]
    "$server[host]:$server[port]"

Entries use commas. `[]` is an empty list; `[:]` is an empty map. A record has
known fields, possibly of different types. A map has computed string keys and
values of one type:

    let key   = 'staging'
    let ports = [$key: 8080, production: 443]

`...` spreads collections and command arguments:

    let defaults = [host: localhost, port: 8080]
    let config   = [...$defaults, port: 9090]

    let flags = [-l, -a]
    ls ...$flags ...$directories

Explicit record fields win over spread fields, wherever they appear.

Patterns take values apart:

    let [first, ...rest] = $ARGS
    let [host: host, port: port = 8080] = $config
    let [name: name, address: [city: city]] = $person

A mismatch is a catchable runtime error. `range 1 5` returns `[1, 2, 3, 4]`.

### Collection functions

Blocks are values, so functions can take them:

    map    { |x| return $[$x * 2] } [1, 2, 3]   # [2, 4, 6]
    filter { |x| return $[$x > 2] } [1, 2, 3]   # [3]
    fold   { |s x| return $[$s + $x] } 0 [1, 2, 3]   # 6

`for` puts the data first:

    for $hosts { |host|
        echo "checking $host"
        ping -c 1 $host
    }

There are no mutable loop counters. Carry state through `fold`, or use a
tail-recursive block. Tail calls reuse the current frame.

A named function used as data needs `$`:

    map $upper [hello, world]   # [HELLO, WORLD]
    map upper  [hello, world]   # passes the string "upper"

Only head position invokes a bound block implicitly. Use `explain NAME` for
the current signature of `map`, `first`, `sort-list-by`, `group-by`, and the
rest of the prelude.

### Variants

A variant says that a value is one of several alternatives:

    let result =
        if !{exists $path} { return `found !{file-info $path} }
        else              { return `missing }

`case` handles each alternative:

    case $result [
        `found:   { |info| echo "$path: $info[size] bytes" },
        `missing: { |_| echo "$path: not found" },
    ]

The type checker verifies the arms. A tag without a payload passes `unit`;
`_` ignores it.

## 6  Pipelines, codecs, and files

ral has one pipeline. `|` is an operating-system byte pipe: the stage on its
left writes its standard output into the standard input of the stage on its
right. Position is the whole of the wiring:

    cat data.txt | sort | uniq

Neither side has to use the wire. A stage may write nothing, and a stage may
never read. What `|` never carries is a value: a non-final stage's return value
is simply discarded, and the pipeline's value is the final stage's.

Each stage must be a command ready to run. A function still waiting for an
argument is a type error, because there is nothing there to start:

    cat notes.txt | fold-lines { |n line| return $[$n + 1] }     # error
    cat notes.txt | fold-lines { |n line| return $[$n + 1] } 0   # runs

Apply such a stage to its argument rather than piping into it. The second line
runs and returns the number of lines.

Codecs cross between bytes and values:

| Decoder | Bytes become |
|---|---|
| `from-line` | one `String`, trailing newline removed |
| `from-string` | one `String` |
| `from-lines` | a lazy stream of strings |
| `from-json` | a ral value decoded from JSON |
| `from-csv` | a list of header-keyed records |
| `from-bytes` | a `Bytes` value |

The encoders are `to-line`, `to-string`, `to-lines`, `to-json`, `to-csv`, and
`to-bytes`.

Decode while bytes are flowing, and let the decoder be the last stage. Its
returned value is the pipeline's, and `let` binds it:

    let branch = git branch --show-current | from-line
    let config = curl -fsS $url | from-json
    let rows   = curl -fsS $csv-url | from-csv

A value already in hand is not on any pipe. Encode it into one first, or read
the file directly:

    let config = to-string $text | from-json   # right
    let config = from-json < $path             # right

    let config = $text | from-json             # decodes an empty pipe

The last line is a legal program that does the wrong thing. `$text` is a
perfectly good first stage, but it writes no bytes, so `from-json` reads end of
input and fails when the program runs. The string never reached it.

`|` moves bytes; `let` binds the final stage's payload. The type checker checks
every stage before a process starts.

Values compose by application, not by `|`:

    let evens   = filter { |n| return $[$n % 2 == 0] } !{range 1 10}
    let doubled = map { |n| return $[$n * 2] } $evens

File I/O is a redirect plus a codec:

    let body   = from-string < $path
    let lines  = from-lines-list $path
    let config = from-json < $path

    to-string $body > $path
    to-json $config > $path
    echo $line >> $log

Mutations use `cp`, `mv`, `rm`, `mkdir`, and the other bundled tools. Queries
return values:

    glob #'src/**/*.rs'#
    list-dir $path
    file-info $path
    line-count $path
    exists $path
    is-file $path

`map-lines`, `filter-lines`, `each-line`, and `fold-lines` read the byte pipe a
line at a time, in bounded memory, so they belong in a pipeline:

    cat access.log | filter-lines { |line| re-match ' 500 ' $line } | wc -l

`from-lines` decodes the pipe into a lazy stream instead, and a stream must be
eliminated explicitly:

    let commits = git log --oneline | from-lines
    stream-each { |line| echo $line } $commits

Use `stream-map`, `stream-fold`, and `stream-to-list` for the rest of the lazy
work, and `from-lines-list $path` for a materialised list.

## 7  Scripts, scope, and modules

A script begins:

    #!/usr/bin/env ral
    let [target, port] = $ARGS
    echo "deploying to $target on $port"

`$ARGS` contains only user arguments. Forward them with `...$ARGS`. `$SCRIPT`
is the current file's path, `$ENV` is a read-only map of environment variables,
and `$NPROC` is the CPU count.

Use `within` for a scoped directory or environment:

    within [dir: build] {
        cmake ..
        make -j $NPROC
    }

    within [env: [RUST_LOG: debug, PORT: 8080]] {
        cargo run
    }

The old directory and environment return at the closing brace. Functions
called inside the block see the active overrides, even if defined elsewhere.

`use` evaluates a file in its own scope and returns its public bindings:

    let strings = use 'lib/strings.ral'
    $strings[trim] '  hello  '

Bindings beginning with `_` stay private. `source 'config.ral'` evaluates into
the current scope and includes every binding. Paths are relative to the
containing file; `RAL_PATH` adds search directories.

## 8  Failure and audit

Failure propagation is always on. A nonzero exit, `fail`, or a runtime error
stops the current computation unless something handles it.

`try` catches a failure:

    let config = try {
        return !{curl -fsS $primary | from-json}
    } { |error|
        echo "primary failed: $error[message]"
        return !{curl -fsS $fallback | from-json}
    }

Keep the handler on the same line as the body's closing brace: `} {`.
The handler receives an error record with `status`, `cmd`, `message`, `line`,
and `col` fields. `message` describes the failure; it is not the command's
stderr.

Raise and re-raise with an error record:

    fail [status: 2, message: 'usage: deploy HOST']
    try { risky-work } { |error| fail $error }

`guard` always runs cleanup, then lets the original failure continue:

    let temporary = temp-file
    guard {
        curl -fsS -o $temporary $url
        consume $temporary
    } {
        rm -f $temporary
    }

The prelude contains common policies:

    retry 3 { curl -fsS $url }
    attempt { rm stale.lock }
    succeeds { cargo check -q }             # Bool

`audit` turns success or failure into an execution report:

    let report = audit { make -j4 }
    echo $report[children][0][stderr]

`report[children]` is the flat list of what ran during the body: each
command's `argv`, status, origin, stdout, stderr, value, source location, and
timing, plus any redirect reads or writes and capability-check decisions.
`ral --audit script.ral` records the whole script; add `--pretty` for indented
JSON.

## 9  Concurrency

`spawn` runs a block on another thread and returns a handle:

    let tests = spawn { cargo test -q }
    let lint  = cargo clippy -q &

    # do other work

    let test-result = await $tests
    let lint-result = await $lint

`&` is the pipeline form of `spawn`. `await` returns a record with `value`,
`stdout: Bytes`, and `stderr: Bytes` fields. A worker's failure is re-raised at
`await`, so put the `await` inside `try` to recover.

`defer` is the prelude form for long work whose failure should be data. It
spawns `audit { … }`, so the awaited `value` is an audit report:

    let suite = defer { cargo test -q }

    # make progress elsewhere

    let result = await $suite
    let tree   = $result[value]

    if $[$tree[status] == 0] { echo passed } else { echo failed }

Handles cache their result and may be awaited again. `poll` checks without
blocking: it returns `` `pending `` with output so far, or `` `settled `` with
output and an `` `ok `` / `` `err `` outcome.

The other concurrency tools use the same handles:

    race [$first, $second]   # first result; cancel the rest
    cancel $handle
    par { |file| convert $file } $files $NPROC
    watch build { make -j $NPROC }

`par` is a bounded parallel `map` and preserves input order. `watch` streams
labelled output. Spawned blocks inherit immutable values, so there is no shared
mutable state or data race.

Some agent hosts also provide `service` for work lasting the session and
`detach` for an OS process that may outlive it. They are absent when the host
cannot provide those semantics; ask `explain` before relying on them.

## 10  Reinterpreting commands

`within` can change what an external command means for one block. A named
handler receives its argument list:

    within [
        handlers: [
            curl: { |args| echo #'{"status": "ok", "revision": "abc123"}'# },
        ],
    ] {
        let response = curl -fsS https://example.test/deploy | from-json
        echo "deployed $response[revision]"
    }

No request occurs: the handler supplies `curl`'s meaning inside the block.
Handlers are deep, so functions called by the body see them too.

Handlers are self-masking. They may call the same command to forward it without
recursing:

    within [handlers: [git: { |args|
        echo "+ git !{intercalate ' ' $args}"
        git ...$args
    }]] {
        git status --short
    }

A catch-all `handler:` receives `|name args|`. Bindings are lexical, but
directory, environment, handlers, and capabilities follow the call site for
the whole block.

## 11  Capabilities

`grant` removes authority for one block:

    grant [
        exec: [git: [status, diff]],
        fs: [
            read: ['cwd:'],
            write: ['tempdir:'],
        ],
        net: false,
    ] {
        git status --short       # allowed
        git push                 # denied: push was not granted
    }

Each mentioned dimension becomes deny-by-default:

- `exec:` names commands, paths, directory prefixes, or subcommands;
- `fs:` names readable, writable, and denied regions;
- `net:` permits or refuses network access;
- `detach:` permits or refuses work that outlives the session;
- `audit:` adds capability decisions to an audit tree.

An omitted dimension keeps the caller's authority. A nested `grant` can only
reduce authority, never restore it.

Filesystem policies use stable prefixes such as `cwd:`, `tempdir:`, `gitdir:`,
`xdg:config`, and `~/project`. They are fixed when the policy is loaded, so a
later directory or environment change cannot retarget them.

Filesystem rules cover redirects, structured queries, and bundled tools;
execution and network rules cover external processes. If the operating system
cannot enforce a requested external restriction, ral fails closed. Start a
script under saved profiles with:

    ral --capabilities read-only.ral script.ral

## 12  Types and the model underneath

ral infers types for values, functions, records, variants, pipelines, and
handles. Check a script without performing any command:

    ral --check script.ral

The checker catches a pipeline stage that is still waiting for an argument,
non-scalar interpolation, wrong function arguments, incompatible `if` branches,
missing `case` arms, and impossible record fields. Every type is inferred: the
language has no syntax for writing one.

The deeper model is now small:

- a **value** is inert data;
- a **command** may work, return a value, emit bytes, or fail;
- `{ command }` turns a command into a value;
- `!block` runs that stored command;
- an external command call is an effect;
- `within [handlers: …]` changes an effect's interpretation;
- `grant` limits which effects may occur;
- `audit` records the effects that occurred.

This is call-by-push-value with one practical effect: executing commands. The
name is optional; the separation between values and commands is not.

## 13  A complete script

This script writes a JSON manifest of a directory's regular files:

    #!/usr/bin/env ral

    if $[!{length $ARGS} != 2] {
        fail [status: 2, message: 'usage: manifest ROOT OUTPUT.json']
    }

    let [root, output] = $ARGS

    if $[not !{is-dir $root}] {
        fail [status: 2, message: "not a directory: $root"]
    }

    let manifest = within [dir: $root] {
        let paths = glob #'**/*'#
        let files = filter { |path| return !{is-file $path} } $paths

        return !{map { |path|
            let info = file-info $path
            return [path: $path, bytes: $info[size]]
        } $files}
    }

    to-json $manifest > $output
    echo "wrote !{length $manifest} files to $output"

Check it, then run it:

    ral --check manifest.ral
    ral manifest.ral ./src manifest.json

There is no text parsing for metadata, temporary file for the atomic write,
mutable loop state, or directory change that can leak past its block.

## 14  Coming from bash

ral keeps commands, arguments, pipes, and redirects. It removes the features
that turn data back into source text.

| bash | ral |
|---|---|
| `x=foo` | `let x = 'foo'` |
| `x=$(command)` | `let x = command` |
| `$(command)` inline | `!{command}` |
| `"$value"` to prevent splitting | `$value` — values never split |
| `"$dir/$file"` | `"$dir/$file"` — quotes join atoms |
| `${name}` | `$(name)` |
| `a && b` | `a` then `b`; failure already stops the sequence |
| <code>a &#124;&#124; b</code> | `a ? b` |
| `set -e` | always on |
| `$@` | `$ARGS`; forward with `...$ARGS` |
| `export KEY=value` | `within [env: [KEY: value]] { … }` |
| `cd dir` in a script | `within [dir: dir] { … }` |
| `trap cleanup EXIT` | `guard { body } { cleanup }` |
| heredoc | `command << #'…'#` |
| `$((x + 1))` | `$[$x + 1]` |
| `[ -f "$path" ]` | `is-file $path` |
| mutable loop state | `fold` or tail recursion |

Remember six things:

1. `let x = foo` runs `foo`; `let x = 'foo'` stores text.
2. `$file` is already one argument.
3. Decode before capture: `let config = curl … | from-json`.
4. `?` and `try` handle failures; `if` handles booleans.
5. Pass a named function as `$function` when it is an argument.
6. Use `within` for directory and environment changes.

The running shell knows its own surface:

    help
    explain map
    explain from-json
    explain grant

The [examples](https://github.com/lambdabetaeta/ral/tree/main/examples) contain
complete programs paired with the bash failure mode they replace. The
[specification](SPEC.md) is the precise language definition, and the
[rationale](RATIONALE.md) explains the design.
