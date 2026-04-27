ral runs commands like a shell:

    ls
    cat foo.txt
    cat foo.txt | wc -l
    echo hello > /tmp/out
    echo more  >> /tmp/out

A bare command is an *expression*; its value is its captured stdout.
Bind it directly with `let`:

    let dir    = pwd
    let body   = from-string < 'foo.txt'
    let nlines = wc -l < foo.txt

There is **no parenthesised command-substitution** in ral.  `(cmd)` is
a parse error; `(x)` does not group; `f(x)` is not a call.  The only
place `(...)` appears is `$(name)` inside an interpolating string, to
delimit a variable name from adjacent characters:

    echo "$(prefix)_log.txt"

`!{cmd}` exists, but only inside an interpolating string:

    echo "hi $name, cwd=!{pwd}, lines=!{wc -l < foo.txt}"

Quoting:

    'literal'        # single-quoted, no interpolation, no escapes
    "interp $name"   # double-quoted, $var and !{cmd} expand here

Escapes inside double-quoted strings: `\n` `\t` `\\` `\"` `\0` `\e`,
plus `\$` and `\!` to write a literal `$` / `!` without interpolation.
There are no heredocs (`<<EOF`).  Multi-line strings are just literal
newlines inside the quotes.

Single-quoted strings are **fully literal**: `'\n'` is a backslash
followed by `n`, not a newline.  To embed real newlines or tabs in a
string, use double quotes (`"line1\nline2"`) or a literal newline
inside the quotes.

String equality is `!{equal a b}`, not `==`.  `==` lives only inside
`$[…]` arithmetic / boolean blocks: `$[$x == 0]`, `$[$a + $b]`,
`$[$x > 0 && $x < 10]`, `$[not $p]`.

Data:

    let xs = [a, b, c]              # list
    let m  = [host: 'h', port: 80]  # record / map
    $m[host]                        # index — bare key OK

Control flow.  `if` takes a condition and a then-block, with optional
`elsif` and `else` branches — each introduced by its keyword.  A bare
second `{...}` without `else` is a parse error (the old three-block
syntax `if c { a } { b }` is gone):

    if !{equal $s 'quit'} { exit 0 }
    if !{equal $s 'quit'} { exit 0 } else { echo "continuing" }
    if   $[$x == 0] { 'zero'     }
    elsif $[$x > 0] { 'positive' }
    else            { 'negative' }

`if` is an *expression*, so it can appear on the RHS of `let` —
the cleanest way to compute a conditional value:

    let kind = if $[$x == 0] { 'zero' } else { 'nonzero' }

`for` over a list:

    for $args { |a| echo $a }

`try` catches a failed command.  Without `try`, a non-zero exit raises
an error effect that aborts the current `grant` body:

    try { cat /no } { |err| echo $err[status] }

Persistent state across tool calls:

    cd '/tmp'          # change directory; persists into the next call

## Blocks and parameterised blocks

A **block** `{ stmts }` is a suspended computation — runs when invoked.
A **parameterised block** `{ |a b c| stmts }` takes arguments.  The
parameter list is *space-separated*, never comma-separated.

    { ls }                         # block
    { |path| cat $path }           # parameterised, one arg
    { |a b| $[$a + $b] }           # parameterised, two args
    { |a, b| ... }                 # PARSE ERROR — no commas

Higher-order functions take parameterised blocks.  The argument order
is **not** consistent across them; memorise:

    map    block list           data-last
    filter block list           data-last
    each   block list           data-last
    fold   block init list      data-last
    sort-list-by block list     data-last
    for    list block           data-FIRST  (the only one)

## Pitfalls

- `!{cmd}` evaluated inside a record / list literal returns `Cmd α`,
  not `α`; the type leaks out and the inferred type of the
  surrounding binding becomes a function.  If you see
  `expected [_], got {_ → Cmd _}` at a `let`, suspect a stray
  `!{...}` inside a literal.
- Type errors point at the binding, not the offending sub-expression.
- `has $list $item` is list membership, **not** substring.  For
  substring matching, use `match 'needle' $haystack` — `match` is
  regex but does **substring search by default** (`is_match`
  semantics), so no `.*…\*` wrapping is needed.  Anchor with `^` /
  `$` when you want a full-match.
- `split` and `match` patterns are **regex**, not literal.  Escape
  special chars: `split '\|' $s` for a pipe-separated string.
- `path-join` takes a **list**, not varargs: `path-join ['a', 'b',
  'c']`, not `path-join 'a' 'b' 'c'`.
- `which NAME` reports where a name resolves (`name: prelude`,
  `name: builtin`, `name: alias …`, `name: local`, or an external
  path).  Use it to tell whether you'll get a builtin or a shadowed
  external binary.
- `remove-file` raises on a missing path; wrap in `try` or guard with
  `is-file` for idempotency.
- To edit a file, prefer `edit-file PATH OLD NEW` (one call, prints a
  `-/+` diff).  `OLD` must appear exactly once in the file: zero matches
  raises `pattern not found` (re-read the file — anchor whitespace or
  newlines may differ); >1 raises `pattern matches N times (lines …)`
  (widen `OLD` with surrounding context).  For raw substitution, `replace
  s from to` (exactly-one) and `replace-all s from to` (every match) are
  also available as pure string ops.
- File I/O is **redirect-based**: read with `from-X < PATH`, write with
  `to-X $v > PATH`.  `>` is **atomic** (the file appears whole or not at
  all); use `>~` for streaming truncate, `>>` for append.  Common forms:
    - `let body  = from-string < $p`        # whole file as String
    - `let lines = from-lines  < $p`        # whole file as List Str
    - `let cfg   = from-json   < $p`        # JSON config
    - `to-string $body > $p`                # atomic write
    - `to-json   $cfg  > $p`                # atomic save
- For citation-style line work, prefer `read-file-range PATH START
  COUNT` (1-indexed slice) or `read-file-numbered PATH` (cat -n shape —
  use this when citing line numbers back).  Both delegate to `from-lines
  < $p` so they handle CRLF and skip the trailing-newline empty; line
  counts agree with `line-count`.
- For a `Bytes` value already in hand (e.g. `$r[stdout]` from `await`),
  pipe through `to-bytes` to feed a decoder: `to-bytes $b | from-string`.
  Direct `$b | from-X` is rejected as a value/byte-stage mismatch.
- "command X denied by active grant" often means X is not a builtin
  and doesn't exist as an external either — check the prelude
  reference below for the actual name (e.g. `length`, not `len`).

The full prelude is listed below as `name : type — purpose`.  These
are the only user-facing names; do not invent others.  Underscore
names like `_str`, `_fs`, `_path` are internal — call the wrappers.

Big tool outputs are summarised in history per section: STDOUT,
STDERR, VALUE, and AUDIT are capped independently, each with a
`[elided N bytes]` marker if it overflowed.  When any section was cut,
a single `[full output spilled to /tmp/exarch-*/*.out (use
head/tail/rg)]` line at the end points to the unmodified original on
disk; reach for `head`, `tail`, `rg`, or `cat` on it when the elided
middle matters.

For diagnosis — exact argv handed to `execve`, capability decisions,
per-stage stdout/stderr in a pipeline — set `audit: true` on the
shell tool call.  The result will include an `AUDIT:` JSON block.
Use it when something is silently failing, when a denial is
unexpected, or when you want to see what a pipeline actually did.
Skip it for routine commands; the JSON is verbose.
