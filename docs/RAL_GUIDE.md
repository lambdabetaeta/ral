# ral — a guide for writing scripts

ral is a shell language for running external commands, piping them
together, reading files, and deploying things — the same work done in
bash or zsh, but with a type system, immutable bindings, and no word
splitting.  This guide assumes familiarity with programming but no prior
exposure to ral.

## Running commands

Commands work as in any shell:

```ral
ls -la /tmp
echo hello
make -j4
curl -s https://api.example.com
ls | grep foo | wc -l
```

`|` pipes stdout to stdin, and `?` runs the next command only if the
previous one failed (a fallback chain):

```ral
curl $primary ? curl $fallback
make ? make test ? make install
```

Redirects follow the usual conventions:

```ral
cmd > out.txt
cmd >> out.txt
cmd < in.txt
cmd 2> err.txt
cmd > out.txt 2>&1
```

## Bindings

`let` introduces a binding.  The right-hand side is a command context:
bare words run commands, and quoted words are data.

```ral
let name = 'hello'                   # String
let max = 42                         # Int
let opts = [host: prod, port: 8080]  # Map
let host = hostname                  # runs hostname, binds the result as String
```

`let x = hello` does *not* assign the string "hello" — it runs the
command `hello` and binds its output.  To bind a string, quote it:
`let x = 'hello'`.

All bindings are immutable; a second `let` to the same name shadows the
original within the current scope, but does not modify it.

## Dereference and force

Three prefix operators:

- **`$`** retrieves data: `$x` looks up a variable, `$x[key]` indexes
  into the result, and `$[x + 1]` evaluates arithmetic.
- **`!`** runs a stored command: `!{hostname}` forces a block literal,
  and `!$b` forces the block stored in variable `b`.
- **`^`** bypasses builtins and prelude: `^grep` resolves `grep` as an
  external command only, even if a local binding shadows it.

```ral
echo $x                    # dereference: print the value of x
let name = !{hostname}     # force: run hostname, capture stdout
echo $name                 # dereference: print the captured name
^ls -la                    # run external ls, not any prelude binding
```

These compose transparently: `!$b` is force(deref(b)).

## No word splitting

`$var` is always one argument, regardless of whether its value contains
spaces or other whitespace.  Defensive quoting around variables is never
necessary.

```ral
let file = 'my report.txt'
rm $file                   # removes one file, not two
```

## Paths need double quotes

Outside quotes, `$name` is a separate atom; `$dir/file` is two
arguments.  Use double-quoted interpolation to concatenate:

```ral
echo "$dir/file.txt"       # one argument
cp $src "$dst/backup"      # correct
curl "$host:$port/api"     # correct
```

`~` expands to `$env[HOME]`; `~/bin` expands to the user's `bin`
directory as a single atom.

## Strings

Single-quoted strings are literal: no escapes, no interpolation.
Double-quoted strings support interpolation with `$` and `!`, plus
escape sequences (`\n`, `\t`, `\\`, `\0`, `\e`, `\"`, `\$`, `\!`,
`\xNN`, `\u{X..}`).

```ral
'literal — no interpolation, no escapes'
"interpolation: $name and !{hostname}"
"escapes: \n \t \\ \0 \" \$ \!"
"numeric: \x41 is 'A'; \u{1F600} is an emoji"
"delimited dereference: $(name)_suffix"
```

Note that `\n` in a double-quoted string is a real newline character,
not the two characters `\` and `n`.  Use single quotes when you need
the literal backslash-n, e.g. for passing format strings to external
commands.

**Embedding `'` — hash bumping.**  To embed a single quote in the body,
raise the hash level: `#'…'#` closes on `'#`, `##'…'##` closes on `'##`,
and so on.  A `'` in the body followed by fewer than the opening
`#`-count is literal.  Pick the smallest level whose close pattern
does not appear in the body.

```ral
let greeting = #'it's working'#
let sql      = #'SELECT name FROM t WHERE val = "O'Brien"'#
let py       = #'
import sys
print('hello', 'world', file=sys.stderr)
'#
```

For generated or LLM-authored content this is the form to reach for:
the body is verbatim, no character is special, and any level mismatch
is a hard lex error rather than a silent semantic mistake.

Only strings can be interpolated.  To embed a list, map, or other
non-string value, convert explicitly with `str`:

```ral
echo "items: !{str $my_list}"      # correct
echo "items: $my_list"             # error — List is not a string
```

## Collections

```ral
let items = [a, b, c]                    # list
let opts = [host: prod, port: 8080]      # map (insertion order preserved)
let empty_list = []
let empty_map = [:]
let merged = [port: 9090, ...$defaults]  # spread (explicit keys win)
let dynamic = [$key: $val]              # computed key (must be String)
```

The spread operator `...` also works in function calls, expanding a
list into positional arguments:

```ral
let extra = ['--verbose', '--color']
curl ...$extra $url                     # curl --verbose --color https://...
```

## Destructuring

```ral
let [first, ...rest] = $args
let [host: h, port: p] = $opts
let [host: h, port: p = 8080] = $opts    # default if key missing
let [name: n, addr: [city: c]] = $person # nested
```

A mismatch is a runtime error, catchable by `try`.

## Blocks and lambdas

A block `{...}` is a suspended command; it does not run until forced
with `!`.

```ral
let b = {echo hello}       # stored, not run
!$b                         # forced — prints "hello"
!{echo hello}               # create and force in one step
```

A lambda takes parameters, and multi-parameter lambdas are curried:

```ral
let greet = { |name| echo "hello $name" }
greet alice                 # prints "hello alice"

let add = { |x y| return $[x + y] }
let add5 = add 5           # partial application — add5 waits for y
add5 3                     # 8
add 3 4                    # 7
```

## Expression blocks

`$[...]` is an expression language over numbers, comparisons, and
booleans.

```ral
$[$x + 1]          $[$a * $b]          $[$count - 1]
$[$x / 2]          $[$x % 3]           $[$a == $b]
$[$x > 5]          $[$x <= 10]         $[-$x]
$[$x > 0 && $x < 10]     $[$ok || !{recover}]     $[not $done]
```

`/` on two integers is integer division, and `%` requires integers.
Comparisons return `Bool`.  For string comparison, use `lt` and `gt`.
Logical `&&`, `||`, and `not` require `Bool` operands strictly — no
truthiness coercion — and `&&` / `||` short-circuit.  `!` inside
`$[...]` is force (as elsewhere); use the keyword `not` for logical
negation.

## Branching

`if` takes a `Bool` value, not a command:

```ral
if $[x > 5] { echo big } { echo small }
if !{is-empty $list} { echo none } { echo some }
```

`if` does not accept a block — passing `{cmd}` to `if` is a type error.
`?` is for command fallback, not boolean branching.

To branch on whether a command succeeds, use `_try`:

```ral
let r = _try {grep -q pattern file}
if $r[ok] { echo found } { echo missing }
```

`_try` returns a record:

| Field | Type | Meaning |
|---|---|---|
| `ok` | `Bool` | `true` on success |
| `value` | any | block's return value on success |
| `status` | `Int` | exit status on failure |
| `cmd` | `String` | command that failed |
| `stderr` | `Bytes` | stderr of the failing command |
| `line`, `col` | `Int` | source location |

## Error handling

Failure propagation is always on: if a command fails, execution stops.
`try` is the way to catch failures:

```ral
try { make -j4 } { |err|
    echo "failed: $err[cmd] status $err[status]"
}
```

Only runtime errors count as failure: a command exiting nonzero, an
explicit `fail`, or a pattern mismatch.  A body that returns `false` (or
any other value) is *successful* — `try` will not invoke the handler:

```ral
try { return false } { |err| echo unreachable }  # returns false
```

`guard` guarantees that cleanup runs even on failure or signal:

```ral
let tmp = !{mktemp}
guard {
    curl -o $tmp $url
    process $tmp
} { rm $tmp }
```

`retry` retries a body up to `n` times:

```ral
retry 3 { curl -s $url | from-json }
```

## Iteration

`for` iterates a list, and `fold` accumulates values (since bindings
are immutable, there are no mutable loop counters):

```ral
for $targets { |host| echo "deploying to $host" }
let total = fold { |acc x| return $[acc + x] } 0 $items
```

## Scoped overrides

`within` applies scoped overrides to a block.  It takes a map of
options — `env` for environment variables, `dir` for the working
directory — and all code inside inherits them, including called
functions:

```ral
within [env: [CC: clang]] { make -j4 }
within [dir: build] { cmake ..; make }
within [dir: build, env: [CC: clang]] { cmake ..; make }
```

## Capabilities

`grant` installs a deny-by-default capability context for a block.
Commands not listed in `exec` are denied; filesystem and network access
can also be restricted (`net: true` / `net: false`):

```ral
grant [
    exec: [git: [], make: []],
    fs:   [read: ['/home/project'], write: ['/tmp/build']],
] {
    git clone $repo     # permitted
    make build          # permitted
    curl $url           # denied — not in exec
}
```

To mock a command, use `within [handlers:]`:

```ral
within [handlers: [curl: { |args| echo '{"ok": true}' }]] {
    deploy $config      # calls the handler instead of real curl
}
```

Nested `grant` blocks can only reduce authority, never expand it.

## Modules

```ral
let str = use 'lib/string.ral'        # cached, returns a map
$str[upper] hello

source 'helpers.ral'                   # merges into current scope
```

`use` excludes names beginning with `_` from the returned map.
`use-reload` re-evaluates a path and refreshes its cache entry.

## Concurrency

```ral
let a = spawn { curl 'http://a.com' }
let b = spawn { curl 'http://b.com' }
let result_a = await $a
let result_b = await $b
```

`race` returns the first to finish and cancels the rest; `par` runs a
function over a list with a concurrency limit.

```ral
let winner = race [$a, $b]
par { |f| convert $f } !{glob '*.wav'} $nproc
```

## Streaming

```ral
cat data.txt | map-lines upper > out.txt
cat log.txt | filter-lines { |l| match error $l }
let n = !{fold-lines { |acc _| return $[acc + 1] } 0}
```

`map-lines`, `filter-lines`, `each-line`, and `fold-lines` process
stdin line by line without buffering the entire input.

To decode structured data from a byte stream, use the `from-X` commands:

```ral
let obj   = curl -s $url | from-json
let s     = find . | from-lines
let raw   = curl -s $url | from-bytes
```

Each codec has its own named command: `from-line` (one stripped line),
`from-string` (exact UTF-8), `from-lines` (Step stream of lines),
`from-json`, `from-bytes`.

For encoding a value back to bytes, use `to-X`: `to-json $data`,
`to-lines $list`.  These are first-class functions and can be partially
applied or passed to higher-order functions.

To read directly from a file, attach `<` to the decoder:

```ral
let body  = from-string < $path     # whole file as String
let s     = from-lines  < $path     # Step String
let lines = from-lines-list $path   # list of lines
let cfg   = from-json   < $path     # JSON value
```

To write, attach `>` to an encoder.  `>` is **atomic**: either the old
or new contents are observed, never a partial write.  Use `>~` for the
streaming truncate (POSIX `>` semantics) and `>>` to append.

```ral
to-string $body > $path             # atomic
to-json   $cfg  > $path             # atomic
to-lines  $list > $path             # atomic
echo $line     >> $log              # append
```

For a `Bytes` value already in hand (e.g. `$r[stdout]` from `await`),
route it through the `to-bytes` encoder stage to feed a decoder:
`to-bytes $b | from-string`.  Direct `$b | from-X` is a value-into-byte-stage
type error — that is intentional.

## Scripts

```ral
#!/usr/bin/env ral
let [target, port] = $args
echo "deploying to $target on $port"
within [dir: $target] { git pull ? within [env: [PORT: $port]] { make deploy } }
```

`$args` is a list of the arguments passed to the script (not including
the script path itself).  `$script` is the path to the current script
file.

`$env` is a map of the environment; use `$env[PATH]`, `$env[HOME]`,
etc.  Setting environment variables for a command or block is done via
`within [env:]`, not by mutating `$env`.

`return` ends the script (or current lambda) with status 0.  `fail`
aborts with a nonzero status; the argument is an error record:
`fail [status: N]` (status only) or `fail [status: N, message: M]`
(with a custom message). `fail $e` re-raises a caught error verbatim.
`fail [status: 0]` is a compile-time error — use `return` for clean
exits.  `return` inside a `for`/`map` body exits
the current iteration (the body is a lambda).

## Complete example

A deployment script that reads config, deploys to multiple hosts with
cleanup, and handles failures:

```ral
#!/usr/bin/env ral

let [env_name, ...targets] = $args
if !{is-empty $targets} {
    echo "usage: deploy <env> <host>..."
    fail 1
} {}

let config = from-json < "config/$env_name.json"
let [image: img, tag: tag = latest] = $config

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
```

## Practical advice

- **Quote your strings.**  `let x = foo` runs the command `foo`;
  `let x = 'foo'` binds the string.  Unquoted words are commands,
  and quoted words are data.
- **External command results are strings.**  `let host = hostname`
  binds a `String` with the trailing newline stripped; for binary
  output, pipe through `from-bytes`.
- **Use `fail` to abort.**  There is no `exit` builtin; `fail N`
  terminates with status N, and `return` exits with success.
- **Shadowing looks like mutation.**  `let count = $[count + 1]` inside
  a loop does not update the outer `count`; use `fold` to accumulate.
- **Keep non-head references explicit.**  Outside head position, use
  `$name` for values; bare names are strings.
- **Only strings interpolate.**  `"list: $xs"` is an error if `$xs`
  is a list.  Use `"list: !{str $xs}"` to convert first.
- **`try` catches errors, not falsehood.**  `try { return false }`
  succeeds; only failed commands and `fail` trigger the handler.
- **Spread for forwarding arguments.**  `cmd ...$args` expands a list
  into separate arguments — essential for wrapper scripts.
- **Small helpers pay off.**  A one-line lambda like
  `let s = { |c text| return "$c$text$RESET" }` can eliminate
  repetition across a whole script.

## Quick reference

### Control flow

| Function | Arguments | Purpose |
|---|---|---|
| `if` | `Bool then else` | Branch on Bool |
| `while` | `cond body` | Loop while condition holds |
| `for` | `items body` | Iterate a list (data-first) |
| `case` | `value handlers` | Dispatch by map key |

### Error handling

| Function | Arguments | Purpose |
|---|---|---|
| `try` | `body handler` | Catch failure |
| `retry` | `n body` | Retry up to n times |
| `guard` | `body cleanup` | Guaranteed cleanup |
| `audit` | `body` | Return execution tree |
| `fail` | `status` | Abort with status (0 = early success) |
| `_try` | `body` | Suppress failure; return error record |
| `return` | `[value]` | Exit lambda/file |

### List operations

| Function | Arguments | Purpose |
|---|---|---|
| `map` | `f items` | Transform each element |
| `each` | `f items` | Side effect per element (data-last) |
| `filter` | `f items` | Keep matching elements |
| `fold` | `f init items` | Left fold |
| `reduce` | `f items` | Fold from first element |
| `flat-map` | `f items` | Map then flatten one level |
| `first` | `pred items` | First matching element; fails if none |
| `enumerate` | `items` | List of `[index: Int, item: T]` records |
| `concat` | `xss` | Flatten a list of lists |
| `sum` | `items` | Sum a list of numbers |
| `sort-list` | `items` | Sort (numeric or lexicographic) |
| `sort-list-by` | `f items` | Sort by key function |
| `reverse` | `items` | Reverse a list |
| `take` | `n items` | First n elements |
| `drop` | `n items` | Remove first n elements |
| `take-while` | `pred items` | Take while predicate holds |
| `drop-while` | `pred items` | Drop while predicate holds |
| `zip` | `a b` | Pair elements from two lists |
| `chain` | `init fns` | Thread value through a list of functions |
| `seq` | `start end` | Integer range [start, end) |

### Map operations

| Function | Arguments | Purpose |
|---|---|---|
| `keys` | `map` | Keys in insertion order |
| `entries` | `map` | List of [key, value] pairs |
| `values` | `map` | List of values |
| `get` | `map key default` | Lookup with default |
| `has` | `map key` | Key exists (Bool) |
| `union` | `a b` | Merge; b's entries win on conflict |
| `intersection` | `a b` | Keys present in both, with a's values |
| `difference` | `a b` | Keys in a not in b, with a's values |

### Streaming (stdin)

| Function | Arguments | Purpose |
|---|---|---|
| `map-lines` | `f` | Transform each line (byte output) |
| `filter-lines` | `pred` | Keep matching lines (byte output) |
| `each-line` | `f` | Side effect per line |
| `fold-lines` | `f init` | Accumulate over lines |

### Boolean logic

Use expression blocks: `$[$a && $b]`, `$[$a || $b]`, `$[not $a]`.
`&&` and `||` short-circuit; operands must be `Bool`.

### Strings

| Function | Arguments | Purpose |
|---|---|---|
| `upper` | `s` | Uppercase |
| `lower` | `s` | Lowercase |
| `length` | `s` | Length (also lists/maps/bytes) |
| `slice` | `s start n` | Substring (n characters) |
| `replace` | `s from to` | Replace exactly one occurrence (error on 0 or >1) |
| `replace-all` | `s from to` | Replace every occurrence (literal) |
| `split` | `pattern s` | Regex split into list |
| `join` | `sep list` | Join list into string |
| `match` | `pattern s` | Regex match (Bool) |
| `lines` | `s` | Split string into lines |
| `words` | `s` | Split string into whitespace words |
| `int` | `s` | Parse integer |
| `float` | `s` | Parse float |
| `str` | `v` | Convert to string |

### Paths

| Function | Arguments | Purpose |
|---|---|---|
| `stem` | `path` | Filename without extension |
| `ext` | `path` | Extension without dot |
| `dir` | `path` | Parent directory |
| `base` | `path` | Filename with extension |
| `path-join` | `parts` | Join path components from a list |
| `resolve-path` | `path` | Resolve to absolute path |

### Filesystem

File reads/writes are redirect-based — see "Redirects" above. The
filesystem builtins below cover everything else.

| Function | Arguments | Purpose |
|---|---|---|
| `copy-file` | `src dest` | Copy a file |
| `move-file` | `src dest` | Move or rename a file |
| `remove-file` | `path` | Remove file or directory |
| `make-dir` | `path` | Create directory (and parents) |
| `line-count` | `path` | Number of lines |
| `file-size` | `path` | Size in bytes |
| `file-mtime` | `path` | Modification time (epoch seconds) |
| `file-empty` | `path` | True if file is zero bytes or directory has no entries |
| `temp-dir` | | Create a temporary directory |
| `temp-file` | | Create a temporary file |
| `glob` | `pattern` | Matching paths (sorted list) |
| `list-dir` | `path` | Directory entries (List of Map) |
| `grep-files` | `pattern paths` | Search files; returns List of Map with `file`, `line`, `text` |

### JSON

Use the codec stages with redirects (see above): `from-json < $path` to
load, `to-json $value > $path` to save.  For JSON in a String already
in hand, route through `to-string`: `to-string $body | from-json`.

JSON null maps to `unit`, integers to `Int`, other numbers to `Float`,
booleans to `Bool`, strings to `String`, arrays to `List`, and objects
to `Map`.

### Predicates (all return Bool)

| Function | Purpose |
|---|---|
| `exists` | Path exists |
| `is-file` | Regular file |
| `is-dir` | Directory |
| `is-link` | Symbolic link |
| `is-readable` | Readable |
| `is-writable` | Writable |
| `is-empty` | Empty list, map, bytes, or string |
| `equal` | Structural equality |
| `lt` | String less than |
| `gt` | String greater than |

### Concurrency

| Function | Arguments | Purpose |
|---|---|---|
| `spawn` | `body` | Start concurrent child |
| `await` | `handle` | Wait for result (cached) |
| `race` | `handles` | First to finish wins |
| `cancel` | `handle` | Cancel a running task |
| `par` | `f items n` | Parallel map with concurrency limit |
| `disown` | `handle` | Detach from script lifetime |

### Environment and scope

| Function | Arguments | Purpose |
|---|---|---|
| `within` | `options body` | Scoped env/directory override |
| `grant` | `capabilities body` | Scoped capability restriction |
| `use` | `path` | Load module (cached) |
| `use-reload` | `path` | Reload and refresh cached module |
| `source` | `path` | Evaluate into current scope |

### Miscellaneous builtins

| Builtin | Arguments | Purpose |
|---|---|---|
| `echo` | `...args` | Write to stdout |
| `ask` | `prompt` | Print to tty, read one line |
| `cwd` | | Current working directory |
| `which` | `name` | Resolve command lookup target |
| `length` | `value` | Length of string, list, map, or bytes |
| `has` | `map key` | Key membership test |
| `keys` | `map` | Map keys in insertion order |
| `clear-use-cache` | | Clear all cached `use` modules |
