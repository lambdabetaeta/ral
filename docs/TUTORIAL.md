# ral — a tutorial

This tutorial is about ral, a shell: it runs external commands, pipes
them together, reads files, and deploys things.  Three ideas run
through the whole language, and we shall keep returning to them.

- **Values and commands are distinct.**  The slogan is: a value *is*; a
  command *does*.  Quoted words are data, bare words run programs, and a
  block `{ … }` suspends a command as a value — a *thunk*, run only
  when *forced* with `!`.  The formal model is call-by-push-value.
- **Everything is typed, and checked before it runs.**  Pipelines,
  functions, and collections carry types that ral infers; a stage
  whose input does not match its predecessor's output is rejected
  before any process starts.  `ral --check script.ral` type-checks
  without running.
- **No word splitting, no mutation.**  `$var` is always exactly one
  argument, whatever whitespace it holds, and a `let` binding, once
  made, never changes.

We teach ral on its own terms.  Readers arriving from bash need not
fear: every shell-to-ral difference is collected in one place near the
end, under *Coming from bash*.

## 1  Running commands

In this section we run commands and compose them.  A command runs when
we write its name and its arguments; pipes are `|`:

    ls -la /tmp
    echo hello
    make -j4
    cat foo.txt | wc -l

Sequential composition is a newline or `;`.  Failure propagates: a
command that exits nonzero halts the rest, so `./configure; make` runs
`make` just if the configuration succeeds.  For a fallback, `?` runs
its right-hand side only when the left-hand side *failed*:

    curl $primary ? curl $fallback
    make ? make test ? make install

Redirects attach to a command:

    cmd > out.txt          # write (atomic: the file appears whole or not at all)
    cmd >~ out.txt         # streaming truncate
    cmd >> out.txt         # append
    cmd < in.txt           # read
    cmd 2> err.txt         # redirect stderr
    cmd > out.txt 2>&1     # merge stderr into stdout

`>` is *atomic* on regular files: a concurrent reader observes either
the old contents or the new, never a partial write.  Use `>~` instead
when streaming visibility is part of the contract — logs and FIFOs,
say.

## 2  Bindings

This section introduces `let`, which binds a name.  Such a binding is
immutable, and its right-hand side is a command context: bare words run
commands, quoted words are data.

    let name = 'hello'                   # String
    let max  = 42                        # Int
    let opts = [host: 'prod', port: 80]  # record
    let host = hostname                  # runs hostname, binds its stdout

This trips up newcomers, so let us be explicit: `let x = hello` does
*not* bind the string "hello" — it runs the command `hello` and binds
its output.  To bind a string, quote it.

This is not as strange as it sounds.  Capturing a command binds its
stdout, parsed as UTF-8 with the trailing newline stripped:

    let dir    = pwd
    let nlines = wc -l < $file
    echo "number of lines in $file: $nlines"

A second `let` to the same name *shadows* the original within the
current scope; it does not modify it.  Closures created before the
shadow still see the old value.  Since bindings never change, an
apparent counter update like `let n = $[$n + 1]` only rebinds `n`
locally — it does not mutate the `n` an enclosing scope can see.  To
carry state forward, accumulate with `fold` or a tail-recursive
function (§11).

## 3  No word splitting

The rule of this section is simple: `$var` is always exactly one
argument, whatever whitespace its value holds:

    let file = 'my report.txt'
    rm $file                   # removes one file, not two

Outside quotes, `$name` is a separate atom, so `$dir/file` is two
arguments, not one.  To concatenate, interpolate inside double quotes:

    echo "$dir/file.txt"       # one argument
    cp $src "$dst/backup"
    curl "$host:$port/api"

`~` and `~/bin` expand to `$ENV[HOME]`-relative paths, as single
atoms.

## 4  Strings

ral has two kinds of string quote, and they differ in one respect.
Single quotes are *literal*: no escapes, no interpolation.  Double
quotes *interpolate*: `$name` substitutes a binding, `!{cmd}` splices
the captured stdout of a command, and the escapes
`\n \t \\ \0 \e \" \$ \! \xNN \u{X..}` produce their conventional
characters.

    'literal — no interpolation, no escapes'
    "hi $name, this machine is called !{hostname}"
    "numeric: \x41 is 'A'; \u{1F600} is an emoji"

The numeric escapes are constrained: `\xNN` requires exactly two hex
digits in `\x00..=\x7F`, and `\u{X..}` admits 1–6 hex digits denoting a
valid Unicode scalar.  Any other `\X` is a lex error rather than a
silent literal.  Where `$name` would otherwise be followed by `[`,
write `$(name)[…]` to delimit the variable from the index.

**Embedding `'` — hash bumping.**  To embed quote characters, raise the
hash level of a literal: `#'…'#` closes only on `'#`, `##'…'##` on
`'##`, and so on.  A `'` in the body followed by fewer hashes is
literal.  The idea is to pick the smallest level whose close pattern
does not appear in the body — the body is then verbatim, with no
special characters at all:

    let greeting = #'it's working'#
    let py = ##'
    def f():
        print('hello', 'world')
    '##

Both quoted forms may span multiple lines.  `dedent` strips the common
leading indentation from a multiline literal:

    let msg = dedent '
        SELECT *
        FROM users
    '

Only *scalar* values interpolate: `Int` and `Float` format decimally,
`Bool` as `true`/`false`, `Unit` as the empty string.  Interpolating a
list, map, or other compound value is a type error; one must convert
explicitly with `str`:

    echo "items: !{str $my-list}"      # correct
    echo "items: $my-list"             # error — List is not a string

String *equality* is the command `equal`, not `==` (which compares
numbers); string ordering is `lt` / `gt`:

    if !{equal $s 'quit'} { exit 0 }

For regex work there are `re-` builtins (Rust regex syntax — remember
to escape metacharacters: `re-split '\|' $s` for a pipe-separated
string):

    re-match    '^WARN'  $line             # Bool
    re-replace     '\bfoo\b' 'bar' $s      # first match only
    re-replace-all '\s+'     ' '   $s      # every match
    re-split       ','       $s            # split into a list

`string-replace from to s` is the literal (non-regex) counterpart; it
requires exactly one occurrence and errors on zero or several.

## 5  Blocks and force

This section is where call-by-push-value shows its hand.  A block
`{ … }` suspends a command as a value — a *thunk* that does not run
until *forced* with `!`:

    let d = { date +%s }       # stored, not run
    !$d                        # forced — runs date
    sleep 1
    !$d                        # forced again — a later timestamp

Naming a variable in head position — the very first word of a command —
dereferences and forces it, so `d` alone also runs the block.  `!{M}`
creates and forces in one step; this is the idiomatic way to inline a
call inside a larger command:

    echo "today is !{date +%A}"
    if !{equal $s 'quit'} { exit 0 }

A *parameterised block* is, in effect, a function.  Its parameters are
space-separated — commas never separate parameters — and lexically
scoped:

    { ls }                      # block
    { |path| cat $path }        # one parameter
    { |a b| $[$a + $b] }        # two parameters
    { |a, b| … }                # PARSE ERROR — no commas

    let greet = { |name|
        echo "hello $name how are you"
        $name
    }
    greet alex                  # prints the greeting; returns "alex"

A block returns its *last* command's result; `return` exits early with
a value.  Multi-parameter blocks are curried, so that under-application
is partial application:

    let add  = { |x y| return $[$x + $y] }
    let add5 = add 5            # waits for y
    add5 3                      # 8

A block is also a scoping construct.  Bindings made inside it are
discarded at the closing brace, so ephemeral state belongs in a
force-block:

    !{ let tmp = !{mktemp}; produce > $tmp; consume < $tmp }
    # tmp is no longer bound here

## 6  Expression blocks

Arithmetic, comparison, and boolean logic live inside `$[…]`, and
nowhere else:

    $[$x + 1]          $[$a * $b]          $[$count - 1]
    $[$x / 2]          $[$x % 3]           $[$a == $b]
    $[$x > 5]          $[$x <= 10]         $[-$x]
    $[$x > 0 && $x < 10]     $[$ok || !{recover}]     $[not $done]

`/` on two integers is integer division; `%` requires integers.  `&&`
and `||` short-circuit, and they require `Bool` operands strictly —
there is no truthiness coercion, so `$[1 && true]` is a type error.
Inside `$[…]`, as everywhere, `!` is *force*; logical negation is the
keyword `not`.  Expression blocks do not nest: one layer suffices.

## 7  Conditionals

In this section we branch.  `if` takes a `Bool` value and block
branches, with optional `elsif` and `else`:

    if $is-quit { exit 0 }
    if !{equal $s 'quit'} { exit 0 } else { echo 'continuing' }
    if    $[$x == 0] { 'zero'     }
    elsif $[$x > 0 ] { 'positive' }
    else             { 'negative' }

A one-armed `if` — one with no `else` — is evaluated for its side
effects.  Two-armed forms must agree on their type and produce a value,
so that `if` can sit on the right-hand side of a `let`:

    let kind = if $[$x == 0] { 'zero' } else { 'nonzero' }

The condition must be a *value* of type `Bool`.  `equal $s 'quit'` is
a command, so force it in place: `if !{equal $s 'quit'} { … }`.

`if` branches on `Bool`; `?` reacts to *failure*.  These two never
cross: a predicate returning `false` is still a successful command.
When success itself must drive a branch, reach for `try` (§14) or the
prelude's `succeeds`:

    try { grep -q pattern file; echo found } { |_| echo missing }
    if !{succeeds { cmp -s $a $b }} { echo identical }

## 8  Lists, records, and maps

Lists and key-value collections share one bracket form.  Their
elements are separated by commas — commas are list punctuation only
inside `[…]`, and elsewhere they are ordinary word characters:

    let xs = [a, b, c]              # list
    $xs[0]                          # 'a'
    let m  = [host: 'h', port: 80]  # record
    $m[host]                        # index — bare key OK
    let empty-list = []
    let empty-map  = [:]
    let dynamic    = [$key: $val]   # computed key (must be String)

`range 1 11` is the half-open list `[1, 2, …, 10]`.  The bundled
coreutil `seq` also exists, but it is an external-style command
emitting *bytes*, not a ral list.

`...` spreads one collection into another, or a list into command
arguments.  In a map, explicit entries win over spread entries,
whatever the textual order:

    let defaults = [host: 'db', port: 5432]
    let cfg      = [...$defaults, port: 9090]   # port is 9090

    let pre-args = ['-l', '-a']
    let args     = ['-h', ...$pre-args]
    ls ...$args ...$dirs

Spreading into argv requires scalars: `Int`, `Float`, and `Bool`
format textually; spreading a nested list or map is an error.

Patterns destructure, both on the left-hand side of `let` and as block
parameters:

    let [head, ...rest] = $ARGS
    let [host: h, port: p] = $opts
    let [host: h, port: p = 8080] = $opts    # default if key missing
    let [name: n, addr: [city: c]] = $person # nested

Patterns are structural; a mismatch is a runtime error, catchable by
`try`.

## 9  Variants and `case`

Variants are *tagged sums* — the language's way of saying that a value
is one of several alternatives.  A *constructor* is a backtick label,
optionally carrying one payload atom:

    let ok  = `ok 5
    let err = `err 'parse failure'
    let nothing = `none                      # nullary

The *eliminator* — the form that takes a variant apart — is `case`,
whose second operand is a tag-keyed table of handlers.  The handler row
must cover every constructor the scrutinee can produce; a missing or
extraneous arm is a type error:

    let answer = case $r [
        `ok:  { |n| return $n },
        `err: { |_| return -1 }
    ]

Nullary tags pass `unit` to their handler.  Variants compose with
`try` to give total parsers:

    let parse-int = { |s|
        try { return `ok !{int $s} } { |e| return `err $e[message] }
    }

## 10  Higher-order functions

Parameterised blocks are first-class values, and the prelude is full of
combinators that take them.  These take the function *first* and the
data last:

    map    { |x| return $[$x * 2] }      [1, 2, 3]   # [2, 4, 6]
    filter { |x| return $[$x > 2] }      [1, 2, 3]   # [3]
    fold   { |acc x| return $[$acc+$x] } 0 [1, 2, 3] # 6
    reduce { |a b| return $[$a * $b] }   [2, 3, 4]   # 24

`for` is the one data-first form, reading like a loop:

    for $targets { |host| echo "deploying to $host" }

There are no mutable loop counters; one accumulates with `fold`, or
writes a tail-recursive function.  Tail calls reuse the current frame,
so that recursion *is* the loop:

    let total = fold { |acc x| return $[$acc + $x] } 0 $items

A named function passed *as an argument* must be explicit with `$`;
only the head position dereferences implicitly:

    map $upper [hello, world]            # [HELLO, WORLD]
    map upper  [hello, world]            # WRONG — passes the string 'upper'

`return` inside a `for`/`map` body exits the current iteration — the
body is itself a block — and a failing body stops the iteration.  There
is no `break`.  For early termination, reach instead for a combinator:
`take-while` / `drop-while` for predicates, `first` for the first
matching element (which fails if none matches), or a `fold` that
threads the decision through its accumulator.  Use `explain NAME` for any
function'\''s exact signature.
type.

## 11  Pipelines and codecs

Most commands have three channels: they slurp from *in*, drain into
*out*, and return a ral value.  The in and out channels carry either
bytes or values, and which they carry depends on the command.  There
are three cases.  First, *external commands* consume and yield bytes.
Second, *internal functions* consume and yield ral values.  Third,
*codecs* bridge the two worlds:

    | Decoder       | In      | Out                              |
    |---------------|---------|----------------------------------|
    | `from-line`   | `Bytes` | `String` (trailing `\n` dropped) |
    | `from-string` | `Bytes` | `String`                         |
    | `from-lines`  | `Bytes` | `Stream String`                  |
    | `from-json`   | `Bytes` | JSON as a ral value              |
    | `from-bytes`  | `Bytes` | `Bytes`                          |

with corresponding `to-line`, `to-string`, `to-lines`, `to-json`,
`to-bytes` encoders for the other direction.  Mode mismatches between
adjacent stages are type errors, and are caught before execution.

The out channel and the return value are separate.  `|` carries a
stage's output to the next stage, whereas `let` binds a command's
*return* — and a non-final stage's return is always discarded.  Hence
piping a function call sends its bytes downstream, whereas binding it
captures its result.

    cat foo.txt | head -10                      # entirely external
    glob '*.rs' | map { |f| wc -l $f }          # entirely internal
    hostname | from-line | upper                # mixed
    let cfg = curl -s $url | from-json          # capture after decoding

**Capture after decoding, not before.**  `let x = curl …` parses the
bytes to a `String`, so a later `$x | from-json` is a value-into-byte
stage type error.  Keep it one pipeline (`curl … | from-json`) and
capture the decoded value.  For a `Bytes` value already in hand — e.g.
`$r[stdout]` from `await` — route it through the matching encoder:
`to-bytes $b | from-string`; for JSON in a `String`,
`to-string $s | from-json`.

`from-lines` yields a demand-driven *stream*, consumed explicitly with
the stream eliminators:

    find . -name '*.log' | from-lines | stream-each { |path| echo "log: $path" }

Materialise a stream into a list with `stream-to-list`, or read a file
of lines directly with `from-lines-list $path` (a *positional*
argument, not a redirect).  The prelude also has `stream-cons`,
`stream-nil`, `stream-take`, `stream-drop`, `stream-map`,
`stream-fold`, `stream-each` for programming with infinite or lazy
streams.

For line-by-line processing of stdin without buffering, reach for the
streaming reducers:

    cat data.txt | map-lines $upper > out.txt
    cat log.txt  | filter-lines { |l| re-match 'error' $l }
    let n = cat log.txt | fold-lines { |acc _| return $[$acc + 1] } 0

## 12  Files

The idea here is simple: file I/O is redirect-and-codec.  A decoder on
`< $path` reads, and an encoder on `> $path` writes.  Nothing more is
needed.

    let body  = from-string < $path      # whole file as String
    let lines = from-lines-list $path    # list of lines
    let cfg   = from-json   < $path      # JSON value

    to-string $body > $path              # atomic write
    to-json   $cfg  > $path              # atomic write
    echo $line     >> $log               # append

Filesystem *mutations* go through the bundled coreutils.  `cp`, `mv`,
`rm`, `mkdir`, `ln`, `touch`, and some seventy others are compiled into
the binary, and they dispatch through the same capability checks as
everything else does (§16):

    cp $src $dst
    mkdir -p $path
    rm -rf $dir            # the dangerous verb wears its name

Structured *queries* — operations returning a value worth piping — are,
by contrast, functions:

    glob '*.rs'            # matching paths, sorted list
    list-dir $path         # entries as a list of records
    file-info $path        # [name, type, size, mtime, atime, btime, readonly, target]
    line-count $path
    file-empty $path       # Bool
    temp-file              # fresh path in tmpdir
    temp-dir               # fresh directory
    resolve-path $path     # absolute path
    absolute-path $path    # absolute path, lexical

    exists $path           is-file $path        is-dir $path
    is-link $path          is-readable $path    is-writable $path

## 13  Error handling

Failure propagation is always on: a command exiting nonzero, an
explicit `fail`, or a pattern mismatch aborts the enclosing script.
The surrounding form then decides what happens next.

`try B H` catches a failure in `B` and runs the handler `H` with an
error record.  Its two operands sit at the brace boundary `} {` on one
line; a bare newline between them does not parse:

    try { cat /does/not/exist } { |err| echo $err[status] }
    try { make -j4 } { |err|
        echo "failed: $err[cmd] status $err[status] at line $err[line]"
    }

| Field     | Type     | Meaning                                  |
|-----------|----------|------------------------------------------|
| `status`  | `Int`    | exit status                              |
| `cmd`     | `String` | command that failed                      |
| `message` | `String` | runtime message or process outcome text  |
| `line`, `col` | `Int` | source location                         |

On success `try` returns the body's value; on failure, the handler's.
Only runtime errors count as failure — a body that returns `false`, or
any other value, is successful, and the handler is not called.  Note
that `try` is pure control flow: it does not redirect stdout or stderr,
and `message` is a synthetic summary, not the failing command's stderr
bytes.  For those, wrap in `audit` (§17), or background with `&` and
read the await record.

`guard B C` guarantees cleanup: it runs `B`, then runs `C` whatever the
outcome.  A failure in `B` continues propagating after `C` runs.  Like
`try`, its operands sit at the `} {` boundary on one line:

    let tmp = !{mktemp}
    guard {
        curl -o $tmp $url
        process $tmp
    } { rm $tmp }

The prelude rounds this out:

    retry 3 { curl -s $url | from-json }     # up to 3 attempts
    attempt { rm $scratch }                  # run; suppress any failure
    succeeds { cmp -s $a $b }                # Bool: did it exit 0?

`fail` raises a failure with an error record; `fail $e` re-raises a
caught error verbatim.  Its operand is *only* an error record
`[status: N, message?: M]`, never a bare integer.  `exit N` (alias
`quit`) terminates the process, and is *not* catchable by `try`;
`return`, by contrast, exits the current block — or the script, with
status 0:

    fail [status: 2, message: 'no targets given']
    try { … } { |e| fail $e }                # observe, then re-raise
    exit 1

## 14  Concurrency

This section is about running things at the same time.  A trailing `&`
backgrounds a pipeline and yields a *handle*; `spawn { … }` does the
same for a block.  `await $h` joins:

    let a = spawn { curl -s 'http://a.example' }
    let b = curl -s 'http://b.example' &
    let ra = await $a
    echo $ra[stdout]

`await` returns a record:

| Field    | Type    | Meaning                            |
|----------|---------|------------------------------------|
| `value`  | `α`     | the block's return value           |
| `stdout` | `Bytes` | everything written to fd 1         |
| `stderr` | `Bytes` | everything written to fd 2         |

A spawned failure raises *at `await`* — so wrap in `try` to recover:

    let h = spawn { rg -n 'FIXME|TODO' . }
    # … other work …
    try {
        let r = await $h
        echo $r[stdout]
    } { |err| echo "rg failed: $err[message]" }

To check a handle *without* blocking, `poll $h` is the non-blocking dual
of `await`: it returns `` `pending `` while the block runs and `` `settled ``
once it finishes, and it never re-raises:

    let job = spawn { rg -n 'TODO' . }
    let p   = poll $job
    case $p [
        `settled: { |s|
            case $s[outcome] [
                `ok:  { |_| echo 'scan done' },
                `err: { |e| echo "scan failed, exit $e[status]" }
            ]
        },
        `pending: { |s| echo "still scanning: $s[stdout]" }
    ]

`` `settled `` carries `[stdout, stderr, outcome]`, where `outcome` is
`` `ok `` with the block's value or `` `err `` with the caught-error record
(the same shape `try` gives) — so a failed block's status lives in
`outcome.err.status`, never on `await`'s record.  `` `pending `` carries
`[stdout, stderr]` too — the bytes the block has written *so far* — so you
can read a running job's progress without waiting for it (the headless
counterpart to `watch`).  It is a cumulative, non-destructive snapshot: the
bytes stay buffered, so a later `await` still returns them in full, and each
poll of a running job simply reports a little more.  `is-done $h` is the
boolean shortcut: `true` once settled, `false` while pending.

Because bindings are immutable, there is no shared mutable state across
spawned blocks; each runs against a snapshot of its environment.  This
is the payoff of immutability — data races are ruled out by
construction, not by discipline.

    race [$a, $b]            # first to finish wins; the rest are cancelled
    cancel $h                # cancel a running block
    par { |f| convert $f } !{glob '*.wav'} $NPROC   # parallel map, n jobs

`par` is `map` parallelised: it returns the list of *values* in input
order, with at most `jobs` blocks in flight (`$NPROC` is the CPU
count).  `watch "label" { … }` spawns a block whose output streams live
to your terminal, each line prefixed `[label]`.  This is what one wants
when running several builds at once without losing their interleaved
output.

## 15  Scoped execution: `within`

`within` runs a block under scoped overrides — working directory,
environment variables, or command handlers — all of which are restored
on exit.  All the keys are optional:

    within [dir: '/tmp']                  { echo stuff > tmpfile }
    within [env: [CC: 'clang']]           { make -j4 }
    within [dir: 'build', env: [V: '1']]  { cmake ..; make }

An override has a visible extent and composes under nesting: inner
frames shadow outer ones, and the change disappears at the closing
brace.

The `handlers:` key installs per-command *effect handlers* — blocks
that intercept calls by name.  This is the mocking and instrumentation
mechanism:

    within [handlers: [curl: { |args| echo '{"ok": true}' }]] {
        deploy $config        # any curl inside runs the handler instead
    }

Handlers are *deep* — they persist for the whole body — and
*self-masking*: inside a handler's own body, a same-name call reaches
the next outer frame or the OS, so wrap-and-forward does not recurse:

    within [handlers: [git: { |args| echo "+ git"; git ...$args }]] { … }

`handler:` (singular) is a catch-all that intercepts every *external*
command in the body.

## 16  Capabilities: `grant`

`grant` attenuates authority for a block.  Each dimension it mentions
becomes deny-by-default within the grant; a dimension it omits keeps
ambient authority:

    grant [
        exec: [git: [], make: [], '/usr/bin/': 'allow'],
        fs:   [read: ['/home/project'], write: ['/tmp/build']],
        net:  false,
    ] {
        git clone $repo      # permitted
        make build           # permitted
        curl $url            # denied — not in exec, and net is off
    }

- `exec:` — which commands may run, keyed by bare name, absolute
  path, or directory prefix; values are `'allow'`, `'deny'`, or a
  list of permitted subcommands (`[git: ['status', 'log']]`).
- `fs:` — `read:`, `write:`, and `deny:` lists of path regions,
  enforced for builtins, redirects, and the bundled coreutils alike
  (and for external programs via the OS sandbox where available).
- `net:` — boolean; enforced through the OS sandbox.

A nested grant can only *reduce* authority, never expand it.  A
*capability profile* — a `.ral` file whose last expression is a map of
this shape — can be loaded at startup with
`ral --capabilities profile.ral`, or spliced in at runtime with
`grant (source 'profile.ral') { … }`.

## 17  Auditing and `--check`

`audit { … }` runs a block and returns its full *execution tree*:
every command with its argv, status, captured stdout/stderr bytes,
timing, and any capability decisions — whatever the outcome:

    let report = audit { make -j4 }
    echo $report[children][0][stderr]

Use it when something fails silently, a denial is unexpected, or you
want to see what a pipeline actually did.  The same tree is available
for a whole script with `ral --audit script.ral` (JSON on stderr;
`--pretty` indents).  Finally, `ral --check script.ral` parses and
type-checks without executing — the type errors surface before any
process runs.

## 18  Modules

`use p` evaluates a file and returns a map of its top-level bindings,
excluding `_`-prefixed names; results are cached per canonical path,
so a module's side effects run once:

    let str = use 'lib/string.ral'
    $str[trim] '  hello  '

`source p`, by contrast, evaluates into the *current* scope, merging
every binding — including the `_`-prefixed ones — and is never cached.
Paths resolve relative to the containing file; `RAL_PATH` adds search
directories.

## 19  Scripts

    #!/usr/bin/env ral
    let [target, port] = $ARGS
    echo "deploying to $target on $port"
    within [dir: $target] { git pull ? within [env: [PORT: $port]] { make deploy } }

- `$ARGS` — list of the script's arguments (no program name at
  index 0).  Destructure it: `let [cmd, ...rest] = $ARGS`.  Forward it
  whole to another command by spreading: `cmd ...$ARGS`.
- `$SCRIPT` — path of the file currently executing; inside a loaded
  module it is that module's path.  Self-locate with the bundled
  `dirname`: `let here = dirname $SCRIPT`.
- `$ENV` — read-only map of the environment (`$ENV[PATH]`,
  `$ENV[HOME]`).  Reading a missing key fails; probe with
  `try { return $ENV[MAYBE] } { |_| return '' }`.  Overrides go
  through `within [env: …]`, never mutation.
- `$NPROC` — CPU count as an `Int`.

Invocation forms:

    ral script.ral arg1 arg2
    ral -c 'echo hello'
    ral --check script.ral        # parse + type-check only
    ral --audit script.ral        # run with execution-tree JSON on stderr
    curl https://example.com/setup.ral | ral    # script over a pipe

## 20  Interactive use

The REPL adds line editing, history, completion, and job control:
`cd` (persistent across the session), `jobs`, `fg`, `bg`, `disown`,
and Ctrl-Z to park a foreground job.  `quit` (or Ctrl-D) exits.

`alias NAME { |args| … }` installs an invocable alias;
`unalias NAME` removes it.  `help` lists all commands, `explain NAME`
shows a function'\''s signature and source location.
types.

Startup configuration lives in `~/.ralrc` (or
`$XDG_CONFIG_HOME/ral/rc`) — a ral script returning a map with optional
keys `env`, `prompt`, `aliases`, `bindings`, `edit_mode`, `plugins`,
and `theme`.  See the specification for the RC and plugin schema.

## 21  A complete example

A deployment script that reads config, deploys to multiple hosts with
cleanup, and handles failures:

    #!/usr/bin/env ral

    let [env-name, ...targets] = $ARGS
    if !{is-empty $targets} {
        echo 'usage: deploy <env> <host>...'
        fail [status: 2, message: 'no targets given']
    }

    let config = from-json < "config/$ENV-name.json"
    let [image: img, tag: tag = 'latest'] = $config

    for $targets { |host|
        echo "deploying $img:$tag to $host"
        guard {
            ssh $host "docker pull $img:$tag"
            ssh $host "docker stop app"
            ssh $host "docker run -d --name app $img:$tag"
        } {
            echo "cleaning up $host"
        }
    }

    echo "deployed to !{length $targets} hosts"

## 22  Coming from bash

ral keeps the muscle memory of a Unix shell — names, arguments, pipes,
redirects — but it is a different language underneath.  The single
biggest adjustment is **quoting discipline**: in head position a bare
word *runs a command*, and a quoted word is *data*.  Thus `let x = foo`
runs `foo` and binds its output, whereas `let x = 'foo'` binds the
string.  Everything below follows from that, and from the two
structural rules — no word splitting, no mutation.

| bash | ral |
|---|---|
| `x=foo` (assign string) | `let x = 'foo'` |
| `x=$(cmd)` (capture output) | `let x = cmd` or inline `!{cmd}` |
| `"$var"` (defensive quoting) | `$var` — already one argument, never splits |
| `"$dir/$file"` | `"$dir/$file"` — interpolate to join atoms |
| `${var}` (delimit a name) | `$(var)` — e.g. `$(name)[…]` before a `[` |
| `a && b` (then, on success) | `a` newline `b`, or `a; b` — failure halts the rest |
| `a \|\| b` (else, on failure) | `a ? b` |
| `set -e` | always in effect — failure propagation is never off |
| heredoc `<<EOF` | `cmd << #'…'#` — the raw string is the body; one leading newline is dropped |
| `$@` / `$*` | `$ARGS`; forward with the spread `...$ARGS` |
| `$0` / `BASH_SOURCE` | `$SCRIPT` |
| `export VAR=…` / env mutation | `within [env: [VAR: …]] { … }` |
| `cd dir` (in scripts) | `within [dir: 'dir'] { … }` |
| `trap … EXIT` | `guard { … } { … }` |
| `[ -f x ]` / `test -f x` | `is-file x` (also `is-dir`, `exists`, `is-readable`, …) |
| `cp` / `mv` / `rm` / `mkdir` | same names — bundled coreutils, capability-checked |
| `( subshell )` (isolated bindings) | a block `{ … }` discards its bindings at the brace |
| `.bashrc` | `~/.ralrc` |
| `seq 1 10` as a list | `range 1 11` (the coreutil `seq` emits bytes) |
| `var=$((x+1))` (arithmetic) | `$[$x + 1]` |
| `[[ "$s" == quit ]]` (string eq) | `equal $s 'quit'` (`==` compares numbers) |

A few cells need more than a row:

- **No `&&` / `||`.**  Sequencing is a newline or `;`, and failure
  stops the sequence automatically.  `?` is the only conjunction-like
  operator, and it fires on *failure*, not on truthiness.  Hence `?`
  replaces `||` for fallbacks, whereas `&&` simply disappears —
  ordinary sequencing already short-circuits on error.

- **No mutation, ever.**  There is no `x=$((x+1))` in a loop.  A
  re-`let` shadows; it never updates an outer binding.  To carry a
  running total, `fold` over the data or recurse (§10).  Likewise there
  are no global variables to `export`: scope environment changes with
  `within [env: …]`.

- **Failure vs. falsehood.**  `set -e` is always on, but it reacts to
  *failure* — a nonzero exit, `fail`, a pattern mismatch — not to a
  command *returning* `false`.  `try { return false }` succeeds and
  never reaches its handler; only a failing command does (§13).

- **Capture after decoding.**  `$(cmd)` in bash hands you raw text.  In
  ral, keep byte-producing stages and their decoder in one pipeline and
  capture the decoded value (`let cfg = curl … | from-json`).
  `let`-capturing the bytes parses them to a `String` first, and loses
  the structure (§11).

- **Only scalars interpolate.**  `"$xs"` is a type error when `$xs` is
  a list — write `"!{str $xs}"` to convert first (§4).

- **`cd` is interactive only.**  In the REPL `cd` persists across the
  session; in scripts prefer `within [dir: …]`, whose effect has a
  bounded extent and composes under nesting (§15).

- **Pass functions with `$`.**  In argument position a bare `upper` is
  the *string* `'upper'`; the function is `$upper`.  Only head position
  dereferences implicitly (§10).

## 23  Quick reference

### Control flow

| Form | Shape | Purpose |
|---|---|---|
| `if` | `if Bool {…} [elsif Bool {…}]* [else {…}]` | branch on a Bool |
| `case` | `case v [`tag: {handler}, …]` | eliminate a variant |
| `for` | `for items {…}` | iterate a list (data-first) |
| `return` | `return [v]` | exit block / script |
| `?` | `a ? b` | run `b` only if `a` failed |

### Error handling

| Function | Arguments | Purpose |
|---|---|---|
| `try` | `body handler` | catch failure |
| `guard` | `body cleanup` | guaranteed cleanup; failure propagates |
| `retry` | `n body` | retry up to n times |
| `attempt` | `body` | run; discard result and failure |
| `succeeds` | `body` | Bool: did the body succeed? |
| `audit` | `body` | return the execution tree |
| `fail` | `[status: N, message?: M]` | raise a failure (`fail $e` re-raises) |
| `exit` | `[n]` | terminate the process |

### Lists

| Function | Arguments | Purpose |
|---|---|---|
| `map` / `filter` / `each` | `f items` | transform / select / side-effect |
| `fold` | `f init items` | left fold |
| `reduce` | `f items` | fold from first element |
| `flat-map` | `f items` | map then flatten one level |
| `first` | `pred items` | first match as an Option: `` `just x `` or `` `none `` |
| `option-or` | `default opt` | payload of `` `just ``, or `default` for `` `none `` |
| `last` | `items` | last element; fails on empty |
| `elem` | `x items` | membership (total, returns Bool) |
| `contains` | `items x` | membership, collection-first (delegates to `elem`) |
| `enumerate` | `items` | `[index: Int, item: α]` records |
| `concat` | `xss` | flatten a list of lists |
| `sum` | `items` | sum of numbers |
| `range` | `start end` | integers in `[start, end)` |
| `sort-list` / `sort-list-by` | `items` / `f items` | sort |
| `reverse`, `take n`, `drop n`, `take-while p`, `drop-while p`, `zip a b` | | the usual |

### Maps

| Function | Arguments | Purpose |
|---|---|---|
| `keys` / `values` / `entries` | `map` | views, keys sorted |
| `get` | `map key default` | lookup with default |
| `has` | `map key` | membership |
| `union` | `a b` | merge; b wins on conflict |
| `intersection` / `difference` | `a b` | keyed set algebra |

### Strings

| Function | Arguments | Purpose |
|---|---|---|
| `upper` / `lower` | `s` | case |
| `length` | `s` | also lists, maps, bytes |
| `slice` | `s start count` | substring by character offset |
| `string-replace` | `from to s` | literal, exactly-once |
| `re-replace` / `re-replace-all` | `pat repl s` | regex replace |
| `re-split` / `re-match` | `pat s` | regex split / test |
| `intercalate` | `sep items` | join a list into one string |
| `lines` / `words` | `s` | split on newlines / whitespace |
| `dedent` | `s` | strip common indentation |
| `shell-quote` / `shell-split` | `s` | POSIX quoting round-trip |
| `int` / `float` / `str` | `v` | conversions |

### Streaming stdin

| Function | Arguments | Purpose |
|---|---|---|
| `map-lines` | `f` | transform each line |
| `filter-lines` | `pred` | keep matching lines |
| `each-line` | `f` | side effect per line |
| `fold-lines` | `f init` | accumulate over lines |
| `view` | `start end` | numbered lines `[start, end)` of stdin |

### Filesystem

Reads and writes are redirect-and-codec (§12); mutations are the
bundled coreutils (`cp`, `mv`, `rm`, `mkdir`, `ln`, …).  Queries:

| Function | Purpose |
|---|---|
| `glob` | matching paths, sorted list |
| `list-dir` / `file-info` | structured directory / stat records |
| `line-count` / `file-empty` | counts and emptiness |
| `temp-dir` / `temp-file` | fresh temporary paths |
| `resolve-path` | absolute path |
| `absolute-path` | absolute path, lexical (symlinks kept, path need not exist) |
| `exists`, `is-file`, `is-dir`, `is-link`, `is-readable`, `is-writable` | predicates |

### Concurrency

| Function | Arguments | Purpose |
|---|---|---|
| `&` (trailing) | | background a pipeline, yield a handle |
| `spawn` | `body` | run a block concurrently |
| `await` | `handle` | join: `[value, stdout, stderr]` (re-raises on failure) |
| `poll` | `handle` | non-blocking: `<pending: [stdout, stderr] \| settled: [stdout, stderr, outcome]>` |
| `is-done` | `handle` | `Bool`: has it settled yet? (non-blocking) |
| `race` | `handles` | first to finish wins |
| `cancel` | `handle` | cancel a running block |
| `par` | `f items jobs` | parallel map, bounded |
| `watch` | `label body` | spawn with live, labelled output |

### Environment and scope

| Form | Purpose |
|---|---|
| `within [dir:, env:, handlers:, handler:] {…}` | scoped overrides |
| `grant [exec:, fs:, net:, …] {…}` | scoped capability restriction |
| `use` / `source` | load a module / evaluate into scope |
| `cwd` | current directory |
| `$ENV` / `$NPROC` / `$ARGS` / `$SCRIPT` | ambient values |

### JSON

`from-json < $path` loads; `to-json $v > $path` saves; for a string in
hand, `to-string $s | from-json`.  JSON null maps to `unit`, integers
to `Int`, other numbers to `Float`, booleans to `Bool`, strings to
`String`, arrays to `List`, objects to `Map`.

---

The full language definition lives in the [specification](SPEC.md), and
the design history in the [rationale](RATIONALE.md).
