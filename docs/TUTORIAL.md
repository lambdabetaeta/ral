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

Each stream (stdin, stdout, stderr) takes at most one redirect, and `2>&1` sends stderr wherever stdout goes, wherever it is written; `cmd > a > b` and `cmd 2> e 2>&1` are parse errors.

`>` is atomic for regular files: a reader sees the old file or the complete new
file, never a half-written file. `>~` is the streaming form for live logs and
FIFOs.

There is no `1>&2`. To tell the human something without putting it on the byte
channel a caller may be binding, say it:

    warn "no matches under $dir"

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

A `let` captures the command that produces its value. A function, a block in
hand, or a handle in that position binds what it *returns*; add `| from-line`
to bind what it writes:

    let f    = { hostname }
    let x    = f               # prints the host name; x is ()
    let host = f | from-line   # binds it

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

Only text, numbers, and `true`/`false` interpolate directly; `()` is nothing to
print. Convert collections, bytes, blocks, and handles explicitly:

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
    { |left, right| () }                 # parse error

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

    if $[$mode == 'check'] {
        echo checking
    } elsif $[$mode == 'build'] {
        echo building
    } else {
        fail [status: 2, message: "unknown mode: $mode"]
    }

A two-armed `if` returns a value:

    let sign =
        if $[$n < 0] { return negative }
        else         { return nonnegative }

`==` inside `$[…]` compares data structurally. `lt` and `gt` compare strings lexicographically.

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
known fields, possibly of different types. A map has string keys that are data
and values of one type; a `"quoted"` key is data, as a computed one is:

    let key   = 'staging'
    let ports = [$key: 8080, "production": 443]

`...` spreads collections and command arguments:

    let defaults = [host: localhost, port: 8080]
    let config   = [...$defaults, port: 9090]

    let flags = [-l, -a]
    ls ...$flags ...$directories

A spread of a record comes first, and the fields written after it replace the ones
it already has; it never adds one.

Patterns take values apart:

    let [first, ...rest] = args
    let [host: host, port: port] = $config
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
the current signature of `map`, `first-where`, `sort-list-by`, `group-by`, and the
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

The arms are written out at the `case` itself, one per tag, which is what lets
the type checker prove they cover every alternative the value can be. A tag
without a payload binds `()`; `_` ignores it. An arm may also name what to
run — `` `missing: $explain `` runs `$explain` on the payload — but the list of
arms is always there to read.

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
| `from-lines` | a list of strings, one per line |
| `from-json` | a ral value decoded from JSON |
| `from-jsonl` | a list of values, one per line of JSON |
| `from-csv` | a list of header-keyed maps of text |
| `from-bytes` | a `Bytes` value |

The encoders are `to-line`, `to-string`, `to-lines`, `to-json`, `to-jsonl`,
`to-csv`, and `to-bytes` — each takes one value and writes its encoded bytes. `to-bytes` is
`from-bytes`'s inverse and takes a `Bytes` value; ral has no byte literal, so
`ints-to-bytes` writes bytes given as numbers, and
`ints-to-bytes [104, 105] | from-bytes` is how a `Bytes` value is minted.

Decode while bytes are flowing, and let the decoder be the last stage. Its
returned value is the pipeline's, and `let` binds it:

    let branch = git branch --show-current | from-line
    let config = curl -fsS $url | from-json
    let rows   = curl -fsS $csv-url | from-csv

A value already in hand is not on any pipe. Encode it into one first, or read
the file directly:

    let config = to-string $text | from-json   # right
    let config = from-json < $path             # right

    let config = $text | from-json             # refused

The last line is refused before anything runs. `$text` is a value, and a value
in stage position writes nothing to the pipe, so `from-json` would read end of
input. The string never reached it.

`|` moves bytes: a stage feeds the next by writing, and the pipeline's value is
its final stage's. The type checker checks every stage before a process starts.

Values compose by application, not by `|`:

    let evens   = filter { |n| return $[$n % 2 == 0] } !{range 1 10}
    let doubled = map { |n| return $[$n * 2] } $evens

File I/O is a redirect plus a codec:

    let body   = from-string < $path
    let rows   = from-lines < $path
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

`from-lines` instead reads to the end and returns a list, so it ends the
pipeline like any other decoder:

    let commits = git log --oneline | from-lines
    take 5 $commits

Every line reader splits the same way: a line ends at `\n` or `\r\n`, and the
last line need not end at all. `to-lines` ends every line with `\n`.

## 7  Scripts, scope, and modules

A script begins:

    #!/usr/bin/env ral
    let [target, port] = args
    echo "deploying to $target on $port"

`args` returns only user arguments. Forward them with `...!{args}`. `$SCRIPT`
is the current file's path. The shell's own state is read by builtins, not
variables: `env` is a map of environment variables, `nproc` is the CPU count,
and `cwd`, `home`, and `user` are what they say.

Use `within` for a scoped directory or environment:

    within [dir: build] {
        cmake ..
        make -j !{nproc}
    }

    within [env: [RUST_LOG: debug, PORT: 8080]] {
        cargo run
    }

The old directory and environment return at the closing brace. Functions
called inside the block see the active overrides, even if defined elsewhere.

`use` evaluates a file in its own scope and returns its public bindings:

    let strings = use 'lib/strings.ral'
    $strings[trim] '  hello  '

Bindings beginning with `_` stay private. Paths are relative to the
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
The handler receives an error record with `status`, `cmd`, `message` and
`site` fields, where `site` is `` `some [script: String, line: Int, col: Int] ``
or `` `none ``. `message` describes the failure; it is not the command's stderr.

Raise and re-raise with an error record:

    fail [status: 2, message: 'usage: deploy HOST']
    try { risky-work } { |error| fail $error }

`guard` always runs cleanup, then lets the original failure continue — and a
cleanup that fails in turn pre-empts it, since a cleanup that could not fail
the run would have nowhere to report from:

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

`audit` turns success or failure into a report — `[outcome, trail]`:

    let report = audit { make -j4 }
    echo !{bytes-to-string !{commands $report}[0][stderr]}

`outcome` is `` `ok `` of the body's value or `` `err `` of the very record
`try` would have handed a handler, so failure reads the same either way:

    if !{succeeded $report} { echo built } else { echo 'build failed' }

    case $report[outcome] [
        `ok:  { |value| return $value },
        `err: { |error| echo $error[message] },
    ]

`trail` is the flat list of what happened during the body, each entry an
envelope — source location, timing, principal — around one `what`, a variant
tagged `` `command ``, `` `write ``, `` `read ``, `` `check ``, or one of
their kin. Entries arrive in settlement order, so a command lands after
everything it ran; their `start`/`end` intervals say which contained which.
`commands $report` picks out the `` `command `` payloads — `argv`, status,
origin, stdout, stderr, error — and is what you want whenever only commands
matter.

`ral --audit script.ral` prints the same report for a whole script as JSON on
stderr; add `--pretty` for indented JSON.

## 9  Concurrency

`spawn` runs a block on another thread and returns a handle at once. The
string before the block is its title, which says what the work is:

    let tests = spawn "tests" { cargo test -q }
    let lint  = spawn "lint" { cargo clippy -q }

    # do other work

    await $tests
    await $lint

`await` behaves as if the block had run on that line: its output appears
here, its value is returned, and its failure is raised here, so put the
`await` inside `try` to recover. `let log = await $tests` binds the output, as
`let log = { cargo test -q }` would. The interactive shell lets you leave the
title out, `spawn { cargo test -q }`; an agent host requires it. There is no
trailing `&`: a POSIX `&` makes a job for `fg`/`bg`, while `spawn` makes a
worker you `await`, so ral asks you to say which you meant.

A handle's result is claimed once: a second `await` of it fails, as does an
`await` after `race` or `cancel` claimed it. `poll` checks without blocking,
printing or claiming, and never fails merely because the result was claimed:
it returns `` `running `` with the output so far and the seconds elapsed,
`` `stopping `` with the elapsed time while a cancelled worker finishes,
`` `ready `` with the output, the time it took, and an `` `ok `` / `` `err ``
outcome, or `` `consumed `` with the time it took after the result was claimed.
A `` `ready `` handle can still be awaited after `poll`. `is-done $tests`
returns `false` while `` `running `` or `` `stopping `` and `true` once
`` `ready `` or `` `consumed ``. `cancel $tests` stops a worker, and everything
it started. A `spawn` inside a spawned block belongs to that block, which
finishes only after it has, its output and failure folded into its own.

Every worker has a numeric id beside its title. `workers` lists the live and
unclaimed ready workers, one record each with its id, title, owner, state and
timing:

    workers

`workers` is status data, not handles: an id cannot reconstruct a handle, so
keep the handle returned by `spawn` or `watch` in a binding when you need to
`await`, `poll`, `is-done`, or `cancel` that worker.

Fan-out that must leave nothing running is a `gather`, a block that owns
the workers spawned inside it:

    gather {
        let t = spawn "tests" { cargo test -q }
        let l = spawn "lint"  { cargo clippy -q }
        return [tests: !{await $t}, lint: !{await $l}]
    }

When the block returns, every worker in it that no one has awaited is awaited
in the order it was born; when it fails or is interrupted, they are all
cancelled. The prelude builds the common shapes on it:

    par { |file| convert $file } $files !{nproc}
    both { cargo test -q } { cargo clippy -q }
    first-of [{ fetch-from a }, { fetch-from b }]
    race [$first, $second]

`par` is a bounded parallel `map` that keeps input order, output included.
`both` runs two blocks at once and returns `[fst: …, snd: …]`; it awaits `fst`
then `snd`, so a failure in either is raised when its turn comes and the other
is cancelled. `first-of` runs several blocks and behaves as the first to
finish, cancelling the rest. `race` does the same for handles you already hold:
it claims the first to finish and cancels the others. In `par`, `first-of` and
`race` the first failure cancels the others and is raised, and Ctrl-C cancels
them all. A gather leaves nothing running when it
returns.

`watch build { make -j !{nproc} }` is a spawn that streams labelled output.
Spawned blocks inherit immutable values, so there is no shared mutable state
or data race.

Some agent hosts also provide `detach` for an OS process that may outlive the
session. It is absent when the host cannot provide those semantics; ask
`explain` before relying on it.

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
        exec: [allow: [`name git, `name rg]],
        fs: [
            read: [`cwd .],
            write: [`tempdir .],
        ],
        net: `offline,
        gpu: `withheld,
    ] {
        git status --short       # allowed
        curl $url                # denied: curl was not granted
    }

Each mentioned dimension becomes deny-by-default:

- `exec:` lists `allow` and `deny` targets: `` `name git ``, a place such as
  `` `absolute /opt/tools/lint ``, or `` `dir `system `` (or `` `dir `path ``, or
  a package manager such as `` `dir `rustup `` or `` `dir `homebrew ``) for a
  directory. An allowed target runs with any
  arguments; ral does not filter arguments;
- `fs:` lists the places that are readable, writable, and denied;
- `net:` is `` `offline `` or omitted; a grant cannot ask for the network;
- `gpu:` is `` `withheld `` or omitted; a grant cannot ask for the GPU;
- `editor:` and `shell:` list verbs, for example `` shell: [`chdir] ``.
  `` `detach `` is a `shell` verb: it permits work that outlives the session,
  so a `shell` list without it refuses that work.

An omitted dimension keeps the caller's authority. A nested `grant` can only
reduce authority, never restore it.

Three rules decide what a grant means:

- An exclusion (`deny`) beats every allowance, however specific: an exact file
  or a deeper directory inside an excluded directory does not run. A refusal
  says which exclusion applies; no allowance overrides it. An exclusion is a
  guardrail, not a wall: it names programs by path and name, so a copy of one,
  or a script fed to an allowed interpreter, is a different request.
- Whatever a grant lets run by path, directory or name, or lets be written, may
  be read. A nested `fs` grant therefore does not hide a tool an enclosing
  grant runs; exclude it to hide it.
- An allowance by path names what it named when it was granted: a symlink put
  in its place later redirects nothing. An exclusion follows its name, and a
  bare program name follows `PATH` afresh at each run.

Every dimension grants or refuses authority; none decides what is written
down. Whenever an audit trail is open, a *denial* is recorded as a `` `check ``
entry and an allowed check never is, so no inner `grant` can hide a refusal
from the trail you opened around it.

Places are a tag and a relative subpath: `` `cwd src ``, `` `tempdir . ``,
`` `scratch . ``, `` `gitdir . ``, `` `xdg-config tools ``, `` `home project ``,
or `` `absolute /usr/share ``. (`` `scratch `` is the session's own scratch
directory, which an embedding host such as exarch makes; plain ral has none and
refuses it.) They are fixed when the policy is loaded, so a
later directory or environment change cannot retarget them. Every list is a
set, so naming something twice changes nothing. Tags are checked before the
program runs: ``net: true`` and a bare string where a place belongs are errors.

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

    if $[!{length !{args}} != 2] {
        fail [status: 2, message: 'usage: manifest ROOT OUTPUT.json']
    }

    let [root, output] = args

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
| `$@` | `args`; forward with `...!{args}` |
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
