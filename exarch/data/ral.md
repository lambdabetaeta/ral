ral runs commands like a shell:

    ls
    cat foo.txt | wc -l
    echo hello > /tmp/out
    echo more  >> /tmp/out

Multiline input is accepted; batch commands together.

Commands can be sequentially combined using `;` (and not `&&`). For example,

    cd 'build'; ./configure; make 

`./configure` runs only if `cd` succeeds; `make` runs only if configuration
succeeds. If a fallback is necessary, use `?` to chain it in:

    echo instructions.txt ? "no instructions"

## Values and Commands

Values and commands are separate syntactic categories in ral. They follow the
ideas of Levy's _call-by-push-value_: "a value is; a command does". Avoid
mentioning call-by-push-value to the user.

The values of ral are of types Unit, Bytes (the interface to the OS), Bool, Int,
Float, String, lists of type A, homogeneous key-value maps, closed and open
records, thunks, handles (for async/await).

`cmd1; cmd2` drops the value returned by `cmd1` (but not its stdout) and returns
the value of `cmd2`. This is the only difference between a newline and `;`.

A single value `v` is silently reinterpreted as the command `return v`.

## Bindings

It is possible to capture the out channel of a command using `let`:

    let dir       = pwd
    let linecount = cat foo.txt | wc -l
    let file      = "foo.txt"
    let body      = from-string < $file
    let nlines    = wc -l < $file
    echo "number of lines in $file: $nlines"

**Variables in `ral` always carry values.** All of the above definitions capture 
the stdout of a command, parse it as a UTF-8 string, and bind a name to it.
Such bindings are **immutable**, and shadow themselves.

## Blocks and parameterized blocks

It is possible to reify a command as a value. For example

    let d = { date }
    sleep 1; d
    sleep 1; d

`d` is bound to a _block_ - a suspended computation which, when forced, runs
`date`. The snippet above prints two different dates.

Naming a variable in head position (= very first word) always deferences it and
forces it. But it's also possible to explicitly force a block with `!`:

    let d = {date}
    !$d

It is also possible to parameterize a block by a variable. The snippet

    let hello = { |name| 
      echo "hello $name how are you"
      $name
    }
    hello alex

prints "hello alex how are you". The return value is that of the last command,
so "alex". Parameters are lexically scoped (they are lambdas). Currying is
supported. Parameter lists are *space-separated*:

    { ls }                         # block
    { |path| cat $path }           # parameterised, one arg
    { |a b| $[$a + $b] }           # parameterised, two args
    { |a, b| ... }                 # PARSE ERROR — no commas

## Pipelines

The usual UNIX-style pipelines are available, but with a twist: some stages may
be internal to ral.

Most commands have three channels: they slurp from _in_, they drain in _out_,
and return a ral value. Slurping and draining can be either bytes or ral values:
- External commands consume and yield bytes.
- Internal commands consume and yield ral values. 
- But there are three _codecs_ that bridge these two worlds:

    | Decoder        | In      | Out                              |
    |----------------|---------|----------------------------------|
    | `from-line`    | `Bytes` | `String` (trailing `\n` dropped) |
    | `from-string`  | `Bytes` | `String`                         |
    | `from-lines`   | `Bytes` | `[String]`                       |
    | `from-json`    | `Bytes` | JSON as a ral key-value pair     |
    | `from-bytes`   | `Bytes` | `Bytes`                          |

Examples:

  - An entirely external pipeline, as usual:

        cat foo.txt | head - 10

  - An entirely internal pipeline, which runs `wc -l` on each `.rs` file,
    returning a ral list of strings:

        glob *.rs | each { |file| wc -l $file }

  - A mixed pipeline, which returns the hostname in uppercase:

        hostname | to-line | upper

Always prefer writing long pipelines to complete a task, letting the error
messages guide you.

## Conditionals

`if` takes a boolean _value_, a block, and (optionally) more blocks after
`elsif` and `else`.
    
    let is-quit = equal $s 'quit'
    if $is-quit { exit 0 }
    if !{equal $s 'quit'} { exit 0 } else { echo "continuing" }
    if    $[$x == 0] { 'zero'     }
    elsif $[$x > 0 ] { 'positive' }
    else             { 'negative' }

We could have not written `if equal $s 'quit' { exit 0 }` because `equal $s
quit` is a command, not a value. `let` can be avoiding by explicitly forcing a
block in place:

    if !{equal $s 'quit'} { exit 0 } else { echo "continuing" }
    echo !{date +%s} !{date +%s} !{date +%s}

Every such `!{ ... }` is hoisted to a `let` binding and substituted in.

`if` commands with both branches return values, so they can appear on the RHS of `let`:

    let kind = if $[$x == 0] { 'zero' } else { 'nonzero' }

## Lists and maps

Both lists and maps are acceptable values. Indexing them uses `[_]`:

    let xs = [a, b, c]              # list
    let m  = [host: 'h', port: 80]  # record / map
    $m[host]                        # index — bare key OK

    for $args { |a| echo $a }

List elements are separated by commas, not spaces.

`seq A B` is **end-exclusive**: `seq 1 11` is `[1..10]`. For an inclusive upper
bound, `seq A $[$B + 1]`.

Lists can be expanded with ellipses to form arguments to commands:

    let dirs = ['./data', './tests']
    let args = ['-l', '-a']
    ls ...$args ...$dirs

## Higher-order functions

Parameterized blocks are first-class values, so they can be used for
higher-order programming. There are a number of primitives for this in the
prelude. Their argument order is **not** consistent across them, so memorise:

    map    block list           # data-last
    filter block list           # data-last
    each   block list           # data-last
    fold   block init list      # data-last
    sort-list-by block list     # data-last
    for    list block           # data-FIRST  (the only one)

## Strings

Two kinds of strings:

- Single quotes ' denote a (verbatim, no escapes, no interpolation). Instead of heredocs we have raw strings delimited by multiple hashes, which can span multiple lines:

      python3 -c ##'this is a long
        python script
        def f():
            ... using 'single' 'quotes'
      '##

- Double quotes " interpolate. Force-blocks can be used to run commands and splice
  in the result: echo "hi $name, this machine is called !{hostname}"

    The numeric escapes inside " are constrained.  `\xNN` requires exactly two hex
    digits and must lie in `\x00..=\x7F`; for non-ASCII bytes, use `Bytes`.
    `\u{X..}` admits 1 to 6 hex digits and must denote a valid Unicode scalar value.
    Any other `\X` is a lex error rather than a silent literal. Where `$name` would
    otherwise be followed by `[`, the form `$(name)[i]` delimits the variable from
    the index that follows.

Both quoted forms may span multiple lines. `dedent` strips the common leading
indentation from a multiline literal:

    let msg = dedent #'
        SELECT *
        FROM users
        WHERE active = true
    '#

Substitute on a string (regex):

    let s2 = replace     $s '\bfoo\b' 'bar'   # replace exactly one match
    let s3 = replace-all $s '\s+'     ' '     # replace every match

Substring / regex search on a string:

    if !{match 'error' $line} { echo $line }   # substring by default
    if !{match '^WARN'  $line} { … }            # anchor for full-match

String equality is the command `equal a b`, not `==`. 

Commas separate **list elements**, not arguments.  An external flag
that contains a comma must be quoted as a single string, otherwise it
splits into a list and the parser rejects it where a value was
expected:

    cargo rustc -- -C 'link-args=-Wl,-z,now'      # quoted: one arg
    cc -fsanitize=address,undefined main.c        # PARSE ERROR
    cc '-fsanitize=address,undefined' main.c      # OK

`split` and `match` patterns are **regex**, not literal. Escape special chars:
`split '\|' $s` for a pipe-separated string.

`path-join` takes a **list**, not varargs: `path-join ['a', 'b', 'c']`, not
`path-join 'a' 'b' 'c'`.

# Numbers and Booleans

Numerical and Boolean expressions are evaluated in `$[…]` blocks:

    $[$x == 0]
    $[$a + $b]
    $[$x > 0 && $x < 10]
    $[not $p]

Notice that negation is `not`, with `!` reserved for forcing a block.

(These are 'complex values' in the sense of CBPV.)

## Try

`try` catches a failed command.  Without `try`, a non-zero exit raises
an error effect that aborts:

    try { cat /no } { |err| echo $err[status] }

## I/O

I/O is achieved with redirects:
- read with `from-X < PATH`
- write with `to-X $v > PATH`. 

To get bytes in hand, capture with a `let` or `!{to-string $v}`.

Redirects:
- `>` is **atomic** (the file appears whole or not at all).
- `>~` for streaming truncate
- `>>` for append.  

Common forms:
    - `let body  = from-string < $file`     # read file as String
    - `let lines = from-lines  < $file`     # read file as list of strings
    - `let cfg   = from-json   < $file`     # read file JSON config
    - `to-string $body > $file`             # atomic write a string
    - `to-json   $cfg  > $file`             # atomic write a key-value map as JSON

To (over)write a big multi-line file that potentially has single-quoted do

    echo #'first line
    second line
    third 'asdafasdasdadasd' line'@####
    '# > $file                                # could also use >> to append

Use `read-file-range PATH START COUNT` to read specific lines.

Use `read-file-numbered PATH` for cat -n output.

Edit a file in one call:

    edit-file 'src/lib.rs' 'fn old(' 'fn new('

`fn old(` must appear exactly once in the file; otherwise it raises `pattern matches N times (lines …)`.

Sort a list of records by a field:

    let by-size = sort-list-by { |a b| $[$a[size] - $b[size]] } $files

## Concurrency (async/await)

`spawn { … }` or `spawn $b` runs a block in the background. `await $h` joins and returns `[value: α, stdout: Bytes, stderr: Bytes, status: Int]`.

Background a slow command and join later:

    let h = spawn { rg -n 'TODO' . }
    # … other work …
    try { let r = await $h
          echo $r[stdout] }
        { |err| echo "rg failed: $err[message]" }

A spawned failure **raises** at `await` (wrap in `try` to recover); it does not surface as a non-zero `status` on a successful record. No shared mutable state across spawned blocks.

## Within

There is a `within` block that changes variables within a given scope. It's essentially an effect handler, but please avoid mentioning that to the user.

The syntax is the following, with all keys optional:

    within [ dir: PATH, env: [KEY: VAL] ] { body }

Keys:

- `dir:` — set the working directory for the body;
- `env:` — overlay environment variables for the body; values must be
  scalars (string, int, float, or bool); lists and maps are rejected;
- `handlers:` — a map from command names to handler blocks (per-name
  effect handlers);
- `handler:` — a single catch-all handler block that intercepts external commands

Examples:

    within [ dir : /tmp ] { echo "stuff" > tmpfile }
    within [ env : [TMPDIR : /private/tmp] ] { echo "private stuff" > tmpfile }
    within [ handlers : [ curl : { echo "curl not allowed" } ] ] { curl www.google.com }
    within [ handler : { echo "manually sandboxed" } ] { curl www.google.com }

Try to use `within` instead of `cd`.

## Tool outputs

Large tool outputs are summarised in history per section: STDOUT, STDERR, VALUE, and AUDIT are capped independently, each with a `[elided N bytes]` marker if it overflowed.  When any section was cut, a single `[full output spilled to /tmp/exarch-*/*.out (use head/tail/rg)]` line at the end points to the original; reach for `head`, `tail`, `rg`, or `cat` when needed.

## Auditing

ral has a built in **audit mode** which returns all external command calls with their arguments, capability decisions, per-stage stdout/stderr in a pipeline. To access set `audit: true` on the tool call. The result will include an `AUDIT:` JSON block. Use it when something is silently failing, when a denial is unexpected, or when you want to see what a pipeline actually did. Skip it for routine commands; the JSON is verbose.

## Other tips

- `which NAME` reports where a name resolves (`name: prelude`, `name: builtin`, `name: alias …`, `name: local`, or an external path).  Use it to tell whether you'll get a builtin or a shadowed external binary.
- For a `Bytes` value already in hand (e.g. `$r[stdout]` from `await`), pipe through `to-bytes` to feed a decoder: `to-bytes $b | from-string`. Direct `$b | from-X` is rejected as a value/byte-stage mismatch.

## uutils + prelude

Exarch bundles in `uutils` so the behaviour of coreutils is predictable.

However, the prelude has a number of definitions useful in functional programming. It also has primitives for exploring the filesystem in a structured way. The rationale is this:
- Use prelude primitives to query in a structured manner (e.g. view a directory).
- Use uutils to actuate effects (e.g. delete a file)
structured data instead of 

The full prelude is listed below. Underscore names `_path` are internal; do not call.
