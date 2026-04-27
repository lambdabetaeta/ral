# Writing shell scripts in ral — a tutorial

This tutorial contains what you need to port a bash script to ral without
reading the compiler or the test suite.  It is written from the perspective
of someone who knows bash and wants to get work done.

Each section has a concrete example.  When ral differs from bash the
difference is called out explicitly.

## 1. Running a script

Shebang line:

    #!/usr/bin/env ral

Invocation forms:

    ral script.ral arg1 arg2
    ral -c 'echo hello' extra args
    ral --check script.ral          # syntax + type check, no execution
    ral --help                      # full option reference

Command-line arguments are in `$args` as a list of strings — user
arguments only, no program name at index 0.  The script's own path is
in `$script` (like bash `BASH_SOURCE[0]`, Python `__file__`).  Inside
a loaded module `$script` is that module's path, not the entry
script's.  Under `ral -c` and in the REPL, `$script` is unbound —
reading it fails like any undefined variable.

    # $args is ["--dry-run", "foo"]
    for $args { |a| echo "arg: $a" }

    # self-locate relative to the script file
    let here = dir $script
    let repo_root = resolve-path "$here/.."

## 2. Let-bindings

`let` binds a name in the current scope.  There is no mutation; re-using
a name shadows the previous binding.

    let x = 1
    let x = 2          # shadow — `x` is now 2, the old 1 is still captured
                       #          by any closure created before this line

A `let` inside a block does not leak out.  If you need to accumulate
state across iterations of a loop, use `fold`, not re-assignment.

    let flags = fold { |acc a|
        if !{equal $a '--verbose'} { return [...$acc, verbose: true] }
                                    else { return $acc }
    } [verbose: false] $args

## 3. Strings

Single-quoted strings are literal.  Double-quoted strings interpolate
`$var` and `!{expr}`.

    let name = 'alice'
    echo "hello, $name"               # hello, alice
    echo "cwd: !{pwd}"                # cwd: /work

There is no `"${var}"` form.  If the variable name runs into following
text, break the string:

    echo "$prefix" "_suffix"

## 4. Commands

A bare line is an external command or builtin.  Arguments are
whitespace-separated tokens:

    echo hello world
    ls -la /tmp

**Commas tokenise.**  `foo a,b,c` is *not* three comma-separated args;
it is one token `a,b,c` unless the comma is a list separator in a ral
literal.  Inside argv position you usually have to quote:

    cargo build --features 'coreutils,diffutils,grep'

### Command substitution

Two ways to capture stdout:

    let files = ls /tmp                      # ordinary let = command
    echo "found !{ls /tmp | wc -l} files"    # !{...} splices the value

The trailing newline is stripped, as in bash `$(...)`.

### Spreading arguments

A list can be spread into argv with `...`:

    let extra = ['-v', '--timeout', '30']
    curl ...$extra https://example.com

## 5. Blocks

A block `{ ... }` is a first-class value.  Blocks with explicit
parameters are functions; blocks without parameters are thunks:

    let add   = { |a b| return $[$a + $b] }
    let greet = { |_|   echo 'hi' }          # takes a dummy arg
    let thunk = { echo 'lazy' }              # no args

To invoke a block, splice it like a builtin with `!{name args...}`:

    echo !{add 2 3}           # 5
    !{greet unit}             # hi — `unit` is the stock no-op argument

`return` in a block produces its value.  Without `return`, the block's
value is the result of its last expression (often `unit` for commands).

## 6. Conditionals

`if cond then else`.  **All three arguments are required**, even when
the else branch is empty — pass `{}`:

    if $ready { echo go } { echo wait }
    if !{is-dir /tmp} { echo 'tmp exists' } {}

The condition may be a Bool, an expression-block predicate
`$[$x == y]` / `$[$x > 0 && $x < 10]`, a builtin substitution
`!{match '\.ral$' $f}`, or a thunk that produces a Bool.

**Do not** write `if cond {} { body } {}` — that is four arguments.  If
you want "do body only when cond is false", write:

    if $cond {} { body }       # three arguments: cond, then, else

`if` is a built-in syntactic form, not a function.

### Chaining on failure

The `?` chain runs the next command only if the previous *failed*:

    mkdir -p build/
    cp binary build/ ? echo 'copy failed'

A common idiom is using `!{cmd}` with a fallback default:

    let key = !{security find-generic-password -s my-key -w} ? ''

## 7. Loops

`for $list { |x| body }`:

    for [1, 2, 3] { |n| echo $n }

    for $args { |a|
        echo "arg: $a"
    }

`while cond body` exists too but is rarely what you want — folds and
`map`/`filter` read better.

## 8. Lists and maps

    let xs = [a, b, c]
    echo $xs[0]                # a

    let m = [host: 'localhost', port: 8080]
    echo $m[host]              # localhost
    echo $m[port]              # 8080

    let empty_list = []
    let empty_map  = [:]

Spread inside a literal:

    let combined = [...[1, 2], ...[3, 4]]         # [1, 2, 3, 4]
    let with_override = [port: 9090, ...$m]       # explicit wins

Destructuring in `let`:

    let [first, ...rest] = $args

## 9. Expression blocks

Arithmetic, comparison, and logical operators live inside `$[...]`:

    if $[$x == 0] { echo zero } {}
    let n = $[$a + $b * 2]
    if $[$x > 0 && $x < 10] { echo in-range } {}
    if $[not !{is-empty $xs}] { echo nonempty } {}

Boolean operators `&&`, `||`, and `not` short-circuit and require
`Bool` operands strictly — no truthiness coercion.  `!` inside
`$[...]` is always force (it introduces an inline call like
`!{foo}`); use the keyword `not` for logical negation.

String equality is **not** `==`; use the `equal` builtin:

    if !{equal $s 'quit'} { exit 0 } {}

## 10. Error handling

`try { body } { |err| handler }` catches failures from commands and
from failed builtins:

    try {
        cat /does/not/exist
    } { |err|
        echo "failed with status $err[status]"
    }

`fail N` raises a failure with status `N`.  `exit N` terminates the
process.

**Gotcha:** `try` (and `_try`) print a diagnostic line to stderr when
they catch.  If you are using `try` as a control-flow primitive to
test whether something succeeded, expect the noise.

**Gotcha:** several prelude functions *fail* when you might expect
`false`.  In particular `elem x list` fails when `x` is not in `list`,
rather than returning `false`.  If you want a total membership
predicate, write one with `fold`:

    let has = { |x items|
        fold { |acc y|
            if $acc { return true } else { return !{equal $y $x} }
        } false $items
    }

## 11. Environment

Read with `$env[NAME]`.  `$env` behaves like a map, and reading a
missing key fails — wrap in `try` if the variable may be unset:

    let home = $env[HOME]
    let maybe = try { return $env[MAYBE] } { |_| return '' }

`within` runs a scoped block with overridden environment variables,
working directory, or effect handlers — all restored on exit.  The
argument is a map whose recognised keys are `env:` (environment
variables), `dir:` (working directory), `handlers:` (per-name effect
handlers), and `handler:` (catch-all handler):

    within [env: [CARGO_TARGET_DIR: "$cache/target-macos"]] {
        cargo build --release
    }

    within [dir: '/tmp'] {
        ls -la
    }

## 12. Pipes and redirection

Familiar from bash:

    echo hello > out.txt
    echo more >> out.txt
    cat out.txt | grep ell
    ls /nope 2> /dev/null
    ls /nope 2>&1 | head -1

## 13. Filesystem builtins

Reading and writing files is done with **redirects** plus codec stages.
The decoder reads from `< $path`; the encoder writes to `> $path`:

    let body  = from-string < $path     # read file as String
    let lines = from-lines  < $path     # read file as List Str
    let cfg   = from-json   < $path     # read file as JSON value

    to-string $body > $path             # write atomically
    to-json   $cfg  > $path             # write JSON atomically
    echo line      >> $path             # append

`>` is atomic: the file appears whole or not at all (no partial writes
on crash, `^C`, etc.).  Use `>~` for streaming truncate (POSIX `>`
semantics) when you need readers to see partial output.

The remaining filesystem builtins are in the prelude:

    copy-file $src $dst
    move-file $src $dst
    remove-file $path            # works on files *and* directories
    make-dir $path               # mkdir -p
    list-dir $path               # returns [[name: ..., type: ...], ...]

    is-file $path
    is-dir $path
    exists $path

    resolve-path $path           # -> absolute path
    path-join [$a, $b, $c]       # join with /
    temp-file                    # -> fresh path in tmpdir
    temp-dir                     # -> fresh directory

    file-size $path
    line-count $path
    file-mtime $path             # integer

## 14. String and path builtins

    upper $s        lower $s
    length $s       length $list        # polymorphic
    replace $s $from $to            # exactly one (errors on 0 or >1)
    replace-all $s $from $to        # every occurrence
    split $pattern $s               # regex split
    join $sep $list
    slice $s $start $n
    match $pattern $s               # regex test, returns bool

    words $s        lines $s            # whitespace / newline split

    stem $p    ext $p    dir $p    base $p

    shell-quote $s                  # POSIX quote one token
    shell-split $s                  # POSIX tokenize a quoted line

## 15. Collection builtins

Data-last where it matters (so `!{map f xs}` reads naturally):

    map    { |x| return $[$x * 2] }    [1, 2, 3]       # [2, 4, 6]
    filter { |x| return $[$x > 2] }    [1, 2, 3]       # [3]
    fold   { |acc x| return $[$acc+$x] } 0 [1, 2, 3]   # 6
    reduce { |a b| return $[$a*$b] }   [2, 3, 4]       # 24

    sort-list $xs            reverse $xs
    take $n $xs              drop $n $xs
    first $pred $xs          # fails if no match
    enumerate $xs            zip $a $b
    concat $xss              flat-map $f $xs
    sum $xs
    entries $map             values $map             keys $map
    has $map $key            get $map $key $default

`filter` returns `[]` when nothing matches — safe.  `first` *fails*
when nothing matches — wrap in `try`.

## 16. Logic

Logical operators are syntax inside `$[...]`:

    $[$a && $b]       $[$a || $b]       $[not $a]

`&&` and `||` short-circuit — the right operand is evaluated only
when the left does not already decide the result.  Operands must
be `Bool`: `$[1 && true]` is a type error, not truthy.  Put an
effectful command on the RHS by forcing it: `$[$ok || !{recover}]`.

## 17. Data conversions

Command output is bytes.  Pipe through a `from-X` codec to get a
typed value:

    let line = sha256sum $file | from-string
    let hash = !{words $line}[0]

JSON:

    let obj  = to-string $text | from-json   # decode in-hand JSON string
    let text = to-json $obj                  # encode (returns Bytes, also writes pipe)

Numbers:

    int $s           float $s           str $n

## 18. Building a `run` helper for dry-run

Bash scripts often have:

    run() { echo "+ $*"; [[ $DRY_RUN == 1 ]] || "$@"; }
    run docker build ...

You cannot defer argv as a value in ral — there is no equivalent of
`"$@"`.  The idiomatic shape is to echo a description and guard the
real call:

    echo '+ docker build ...'
    if $dry_run {} {
        docker build ...
    }

Or, if you want the guard only:

    let run = { |body| if $[not $dry_run] { !{body unit} } }
    run { |_| docker build ... }

## 19. Checking your work

Before running, syntax- and type-check:

    ral --check script.ral

A clean run prints nothing and exits 0.  Errors point at the offending
line and column.  The type checker catches arity mistakes (passing 4
blocks to `if`, etc.) that bash would silently mis-interpret.

## 20. A minimal worked example

    #!/usr/bin/env ral

    # Rebuild and publish release artifacts.
    # Run from repo root.

    let has = { |x items|
        fold { |acc y|
            if $acc { return true } else { return !{equal $y $x} }
        } false $items
    }

    let dry_run = has '--dry-run' $args

    let targets = [
        [triple: 'x86_64-unknown-linux-musl',  name: 'ral-linux-x86_64'],
        [triple: 'aarch64-unknown-linux-musl', name: 'ral-linux-arm64'],
    ]

    for $targets { |t|
        echo ''
        echo "==> $t[name] ($t[triple])"
        echo "+ cargo build --release --target $t[triple]"
        if $dry_run {} {
            cargo build --release --target $t[triple]
            copy-file "target/$t[triple]/release/ral" "dist/$t[name]"
        }
    }

    echo 'done'

That is roughly all you need.  The full reference for the language
lives in `docs/SPEC.md`; `docs/RAL_GUIDE.md` covers features this
tutorial glosses over (concurrency, plugins, the editor API).
