# ral(1) — language specification

## 0  Overview

ral is a statically typed shell in which *values* and *commands* are
formally distinct. Values are data; commands are computations that
may read input, emit bytes, return a value, or fail. A block `{M}`
suspends a command as a value (a thunk); `!` forces one. A
**parameterised block** `{ |x| M }` is a block that binds arguments
when applied. `$` dereferences; `^` bypasses internal lookup for bare
command names. In head position, bare names participate in ambient
lookup, path-shaped heads (`./x`, `/x`, `~/x`) execute exact paths,
and explicit value heads such as `$f` stay in the value/function
world. Elsewhere, words are data.

```
let host     = hostname                # runs hostname, binds String
let greeting = 'hello'                 # binds the string
let deploy   = { |h| echo $h }         # stores the parameterised block
deploy 'prod'                          # applies it
```

The formal model is call-by-push-value; see §20.9.

## 1  Grammar

```
program       = stmt*
stmt          = bg-pipeline (NL? '?' bg-pipeline)* NL?
bg-pipeline   = pipeline '&'?
pipeline      = stage (NL? '|' NL? stage)*
stage         = binding | return-stage | if-stage | command | atom-stage
binding       = 'let' pattern '=' binding-rhs
binding-rhs   = pipeline (NL? '?' pipeline)* '&'?
return-stage  = 'return' atom?
if-stage      = 'if' atom atom ('elsif' atom atom)* ('else' atom)?
command       = '^' NAME (arg | redir)*
              | implicit-head (arg | redir)*
              | explicit-head (arg | redir)+
atom-stage    = VALUE_NAME | explicit-head
implicit-head = NAME | SLASH_WORD | TILDE_WORD
explicit-head = nonword-head index* | implicit-head index+
arg           = atom | '...' atom
primary       = word | block | list | map
atom          = primary index*
index         = '[' word ']'
block         = '{' stmt* '}' | '{' '|' pattern+ '|' stmt* '}'
pattern       = '_' | IDENT | plist | pmap
plist         = '[' (pattern (',' pattern)* (',' '...' IDENT)?)? ']'
pmap          = '[' pentry (',' pentry)* ']'
pentry        = IDENT ':' pattern ('=' atom)?
list          = '[' ']' | '[' elem (',' elem)* ']'
elem          = atom | '...' atom
map           = '[' ':' ']' | '[' entry (',' entry)* ']'
entry         = mapkey ':' atom | '...' atom
mapkey        = IDENT | QUOTED | deref
word          = WORD | QUOTED | INTERP | deref | force | expr-block
nonword-head  = QUOTED | INTERP | deref | force | expr-block | block | list | map
deref         = '$' IDENT | '$(' IDENT ')'
force         = '!' primary
expr-block    = '$[' e ']'
redir         = NUMBER? '>' word | NUMBER? '<' word | NUMBER? '>>' word | NUMBER '>&' NUMBER
e             = orexpr
orexpr        = andexpr ('||' andexpr)*
andexpr       = cmpexpr ('&&' cmpexpr)*
cmpexpr       = addexpr (('==' | '!=' | '<' | '>' | '<=' | '>=') addexpr)?
addexpr       = mulexpr (('+' | '-') mulexpr)*
mulexpr       = unary (('*' | '/' | '%') unary)*
unary         = deref | force | NUMBER | 'true' | 'false' | 'not' unary | '-' unary | '(' e ')'
```

The grammar is written without `NL` tokens for clarity; see the
newline-handling rule below.  The RHS of `binding` is a pipeline, hence a
command context; head-form dispatch applies (§4). `VALUE_NAME` is a
plain `NAME` whose spelling is a numeric literal, `true`, `false`, or
`unit`. A pipeline terminates at newline, `;`, `}`, or `)`. Trailing
`&` on a pipeline spawns it in the background and yields a `Handle`
(§13.1); every arm of a statement-level `?`-chain may carry its own
`&`, while a `let` RHS may only carry a final trailing `&`.

**Newline handling.**  A newline terminates a statement unless it appears:

1. Inside a bracket pair `[ ]` (newlines are whitespace inside collection
   literals).
2. After a **continuation token**: `|`, `?`, `=` (in `let`), `if`,
   `elsif`, `else`, `,`.

A newline *before* `|` or `?` also continues: the parser peeks across
the newline. Command arguments are never continued across newlines —
long commands should bind argument lists to variables and spread them.
`return` is not a continuation token: `return\n42` is two statements.

### 1.1  Lexer

```
IDENT      = [a-zA-Z_][a-zA-Z0-9_-]*
NAME       = [^ \t\n|{}[\]$<>"',();!~^/]+
SLASH_WORD = NAME? ('/' NAME?)+
TILDE_WORD = '~' NAME? ('/' NAME?)*
WORD       = NAME | SLASH_WORD | TILDE_WORD
QUOTED     = '\'' ( [^'] | '\'\'' )* '\''
INTERP     = '"' (ICHAR | ESCAPE | deref ('[' word ']')* | force | expr)* '"'
ICHAR      = [^"\\$!]
ESCAPE     = '\' [nte\\0"$![\n]
NUMBER     = [0-9]+ ('.' [0-9]+)?
NL         = '\n' | ';'
COMMENT    = '#' .* (NL | EOF)
```

`IDENT` names variables, map keys, and dereference targets. `WORD` is a
single token family with three lexer-determined shapes:

- `NAME` — a plain slash-free word such as `git` or `-DFOO=bar`;
- `SLASH_WORD` — a slash-bearing word such as `/tmp`, `./script`, or
  `http://host:port`;
- `TILDE_WORD` — a tilde-shaped word such as `~`, `~user`, `~/path`,
  or `~user/path`.

The lexer classifies the shape once; later phases use that structure
directly and do not rediscover slash or tilde structure from raw text.
`!`, `~`, and `^` are excluded so that `!{…}` (force), `TILDE_WORD`,
and `^name` tokenise without lookahead — quote if you need either
character inside a word (`'foo!bar'`). A literal `!` must be quoted
(`'!'`): unquoted `!` is always the force prefix. `\` has no special
meaning outside double-quoted strings: `C:\Users\foo` is one `NAME`
token.

`#` starts a comment only when it appears at the start of a new token
(after whitespace or a delimiter).  Mid-word, it is an ordinary
character: `curl http://host:8080/foo#anchor` is a single
`SLASH_WORD`.
This matches the POSIX rule: "a word beginning with `#` causes that word
and all subsequent characters up to the next newline to be ignored."

`:` becomes its own token only when followed by EOF, whitespace, or
`]`. Thus `host: val` tokenises as `NAME ':' NAME`, while
`localhost:5432` remains a single `NAME`.

Postfix `[` requires adjacency: `$r[k]` indexes, `$r [k: v]` is two
atoms. The lexer is single-pass and modeless; the parser never alters
its behaviour. Because `force = '!' primary`, the postfix index in
`!{cmd}[k]` is applied by the outer `atom` rule to the forced result,
not to the block itself; `(!{cmd})[k]` and `!{cmd}[k]` are identical.

## 2  Values

```
Value ::= Unit | Bytes | String | Int | Float | Bool
        | List Value | Map String Value | Block | Handle
```

`String` is UTF-8 text. `Bytes` is an opaque finite byte sequence,
possibly containing NUL; equality is bytewise, `length` counts bytes.
There is no bytes literal. `Bytes` values arise from `from-bytes`
(terminating a byte pipeline), from encoders (`to-X`, §15), and from
I/O builtins whose declared return type is `Bytes`. External commands
and byte-output builtins return `String`; to retain their output as
`Bytes`, finish the pipeline with `| from-bytes`.

Expression blocks `$[…]` are a unified expression language over
numbers, booleans, and comparisons.  Arithmetic operators `+ - * / %`
require `Int` or `Float` operands; `Int/Int → Int` (truncated toward
zero), any `Float` → `Float`, `%` requires `Int`.  Comparisons
(`== != < > <= >=`) produce `Bool`.  Logical operators `&& || not`
require `Bool` operands strictly — no truthiness coercion — and
produce `Bool`; `&&` and `||` short-circuit, with the right-hand
side evaluated only when the left-hand side does not determine the
result.  Precedence (low → high): `||`, `&&`, comparisons, `+ -`,
`* / %`, unary `-` / `not`, atoms.  Atoms include `true`, `false`,
numeric literals, `$name` / `$name[k]`, forced commands `!{…}`, and
parenthesised sub-expressions.  `!` inside `$[…]` is always force —
the `not` keyword is the logical negation.  String comparison is
`lt`/`gt` (§16.2).

Maps preserve insertion order. Sets are `Map String Unit` by
convention; `has` and `diff` are builtins, `union` and `intersect`
prelude functions. `Handle α` is opaque, parameterised by the
return type of the spawned block: only `await`, `race`, `cancel`,
`disown` apply; printed as `<handle:PID>`. Handles arise from trailing
`&` on a pipeline (§13.1) and from `par` / `_fork`. `await` of a
`Handle α` yields a record carrying `value: α` along with the block's
captured stdout, stderr, and exit status (§13.3).

The literals `true`, `false`, `unit`, and numeric NAME tokens (matching
`NUMBER`) are recognised as values before any name lookup. The words
`if`, `elsif`, `else`, `let`, `return`, `true`, `false`, and `unit` are
reserved: the parser rejects them as binding names in `let` patterns and
lambda parameters.

## 3  Binding

`let p = cmd` binds the pattern `p` to the result of evaluating `cmd` in
command context. Hence

- `let x = foo`      runs `foo`;
- `let x = 'foo'`    binds the string;
- `let f = { … }`    stores the block;
- `let n = 42`       binds the number.

Value forms on the RHS (the literal and explicit value forms from §2 and
§4: quoted strings, `$`-derefs, `$[…]`, blocks, lists, and maps)
receive an implicit `return`, so `let x = 42` is the same as
`let x = return 42`. Plain non-literal words, slash words, and tilde
words are not value forms.

All bindings are immutable; re-`let` shadows within the current scope.
Scoping is lexical; closures capture at definition. A name is resolved
implicitly only in head position (§4); elsewhere use `$name`. Tail
calls reuse the current frame.

**Recursion and generalisation.** A maximal run of consecutive named
`let`s in the same scope forms a **group**. The elaborator builds a
dependency graph on the group (edge `i → j` when `let_j`'s name appears
free in `let_i`'s RHS) and partitions it into strongly connected
components:

- a singleton SCC with no self-edge emits as a plain `let`, in
  topological order within the group — so forward references to a
  later name are legal as long as they do not form a cycle;
- a cyclic SCC whose members are **all lambdas** emits as `letrec`,
  supporting self- and mutual recursion, monomorphic within the group
  (§20.5);
- a cyclic SCC containing any non-lambda falls back to plain `let` in
  topological order: the runtime's letrec binding only meaningfully
  applies to lambdas, so a non-lambda cycle would bind to an
  uninitialised slot.

```
let f = { |x| $[$x + 1] }
let g = { |x| $[$y * 2] }    -- forward ref to y, resolved by topo
let y = 10                   -- (non-cyclic: f, g, y in order)

let even = { |n| if $[$n == 0] { return true } else { odd $[$n - 1] } }
let odd  = { |n| if $[$n == 0] { return false } else { even $[$n - 1] } }
-- even, odd form a lambda SCC → letrec
```

A second `let` for an already-bound name in the same group **splits**
the group at the shadow point; each half is analysed independently,
preserving source-order semantics across the split:

```
let x = 1
let y = $x           -- y depends on the first x
let x = 2            -- shadow: group splits here
let z = $x           -- z depends on the second x
```

### 3.1  Scoped execution contexts

Three execution aspects are **dynamic**, inherited from the call site
rather than captured lexically: working directory, environment, and
capability restriction (`grant`). They scope to a block, are
inherited by callees defined elsewhere, and compose by nesting —
`within` overrides outward, `grant` attenuates by intersection
(§11.5). The set is fixed. `_audit` is an observability wrapper, not
an execution context.

### 3.2  `within`

`within` is a unified scoping primitive for directory, environment,
and effect handlers. Its argument is a map; the body is a block.

```
within [dir: PATH] { body }
within [env: [KEY: VAL, ...]] { body }
within [dir: PATH, env: [KEY: VAL]] { body }
```

Keys:

- `dir:` — set the working directory for the body;
- `env:` — overlay environment variables for the body; values must be
  scalars (string, int, float, or bool); lists and maps are rejected;
- `handlers:` — a map from command names to handler blocks (per-name
  effect handlers);
- `handler:` — a single catch-all handler block that intercepts
  **external commands only**. Builtins and aliases are
  language-internal and run normally; any external calls they make
  will still hit the handler.

Per-name handlers (`handlers:`) fire unconditionally — naming a
builtin is explicit intent to intercept it.

Handler semantics are **shallow**: the handler body does not see its
own frame, preventing infinite recursion. A handler receives the
command name and arguments and may return a value, fail, or delegate
to the next enclosing frame.

`^name` (external-only bypass, §4) still respects `within` handler
frames: the lookup skips builtins and prelude but the call is
contained by any enclosing handler.

`within` frames nest and compose: inner `dir:` and `env:` override
outer ones; inner `handlers:` shadow outer handlers for the same
name; `handler:` is consulted only when no `handlers:` entry matches.

## 4  Execution

There are two syntactic categories: **command contexts** (statement
position, `let` RHS) and **value contexts** (argument position,
`return`, list/map entries, interpolation, `$[…]`). Ordinary **value
forms** are: literals (`42`, `'hello'`, `"…"`, `true`, `false`,
`unit`), `$`-derefs (`$name`, `$(name)`, `$name[k]`), expression
blocks `$[…]`, and blocks `{…}` (with or without parameters), lists,
and maps. Unquoted words are lexed once as `NAME`, `SLASH_WORD`, or
`TILDE_WORD`; head interpretation uses that shape directly. A lone
value form in command context receives an implicit `return`.

Multiple atoms form an application; the first atom is the **head**.
Head interpretation is syntactic:

1. **Bare head.** If the name is bound in the value namespace, that
   value is applied (for a parameterised block) or forced (for a
   nullary block); any other value type in head position is an error.
   If the name is unbound, command lookup is used: aliases
   (interactive only), then builtins, then `$env[PATH]`.
2. **Path head.** A `SLASH_WORD` (`./x`, `../x`, `/x`) or a
   `TILDE_WORD` (`~`, `~/x`, `~user/x`) executes that exact path.
   Tilde expansion happens at the process boundary. Path heads never
   consult aliases, builtins, or `PATH`.
3. **Explicit value head.** Any other head form (for example `$map`,
   `!$f`, or a block literal) stays in the value/function world:
   external command lookup is never performed for it, and if it does
   not evaluate to a callable block value, it is an error.

Outside head position, plain and slash words are strings; tilde words
are string-typed path values:

```
return ok          # the string "ok"
return $ok         # the bound value
map $upper $items  # the function as data
```

**Prefix operators.** `$` dereferences (`$name`, `$(name)`,
`$name[k]`) and never performs command lookup by itself; `$[…]`
evaluates an expression (arithmetic, comparison, or logical). `!`
forces a block, literal (`!{M}`) or stored (`!$b`); in command
context `!{M}` is the same as `M`, in value context it yields the block's return
value. `^name` resolves `name` as an external command only, bypassing
value, alias, and builtin lookup; the operand must be a plain
slash-free word (`NAME`). The `^name` form is valid only in
head position, where `^` must be the first token of the command.

The `!{…}` form is the idiomatic way to inline a call inside a
larger command: the block is evaluated and its return value
substitutes for the `!{…}` atom. Multiple `!{…}` atoms in one
command are hoisted and evaluated left to right, before the
containing command runs. For example, `if !{$pred $head}
{ … } else { … }` applies `$pred` to `$head` and passes the resulting
`Bool` to `if`. Without the braces, `!$pred $head` is two separate
atoms (the forced `$pred` and the bare `$head`), which is usually
not what is wanted.

### 4.1  Head-form lookup

Head lookup is determined by head syntax:

1. **Bare head** — local scope, then prelude; if found, the value is
   applied/forced per the head rule above. Otherwise aliases
   (interactive only), then builtins, then `$env[PATH]`.
2. **Path head** — no lookup; execute the exact path after tilde
   expansion (if any).
3. **`^name`** — skip value, alias, and builtin lookup; resolve via
   `$env[PATH]` only.
4. **Explicit value head** — evaluate as a value and apply it; no
   command-side lookup occurs.

Under `grant`, external dispatch is filtered by `exec` (§11.1). `$name`
uses only the value namespace. Bare non-head words do not consult
either namespace; map-key positions are not head positions.

### 4.2  Pipelines and command results

Each stage has an **output channel** (bytes, structured values, or
nothing) and an independent **return value** materialised at the
`let` boundary. Pipeline composition connects only the output
channel: the non-final stage's return is **always** discarded. On a
byte edge the channel carries bytes; on a value edge it carries
structured values, delivered to the next stage as its final
argument, keeping pipelines data-last (`items | map $f` is the same as
`map $f items`).

Principal return types at the `let` boundary:

| Stage                                              | Return              |
|----------------------------------------------------|---------------------|
| buffering byte-output (externals, `echo`, `grep`)  | decoded `String`    |
| streaming reducer (`map-lines`, `filter-lines`, `each-line`) | `Unit` |
| encoder (`to-X`)                                   | `Bytes`             |
| decoder (`from-X`), value builtin, ordinary function | its structured value |

A returned `String` is data, never re-lexed, split, or globbed. For
binary, finish the pipeline with `| from-bytes`. Mode mismatches
between stages are type errors caught before execution. Named
functions-as-data on a value edge must be explicit with `$`; only
the head is implicit.

The final stage's disposition:

- statement position — bytes to the terminal (unless redirected);
  return discarded;
- `let` RHS — return bound; bytes still flow
  (`let x = echo hi > f` redirects bytes to `f` and binds `"hi"`);
- `spawn` — buffered in the handle (§13).

Adjacent external stages share a direct OS pipe. Byte stages run
concurrently; value edges are sequential and intra-evaluator.

### 4.3  Block return

A block returns its last command's result. If that last command is
byte-output, the block yields the decoded `String` for it alone.
`{}` yields `Unit`.

```
let b = { echo one; echo two }
let v = !$b                       # "two"
```

Only the **last** command's bytes are captured as the block's value.
Non-final byte-output commands flush to the surrounding visible stream
in real time, so their side-effects remain observable:

```
let v = { echo visible; echo captured }
#         ^ prints to stdout        ^ v == "captured"
```

Captures nest: each block saves the outer stream and restores it on
exit, so an inner capture's non-final bytes reach the nearest enclosing
visible stream rather than being silently dropped.

### 4.4  Bool vs failure

`if` is a syntactic form (not a function) that branches on a `Bool`:

```
if <cond> <then> [elsif <cond> <then>]* [else <else>]
```

Branches are arbitrary atoms — blocks `{ … }`, force expressions
`!{…}`, variables, etc. The typechecker requires each branch to be a
thunk `U C` for the same computation type `C`.  One-armed `if` (no
`else`, no `elsif`) has type `F Unit`; both sides of a two-armed form
must agree on their computation type.  Newlines between condition,
branches, `elsif`, and `else` are permitted.  A bare `{` on the same line
following a complete `if`-expression — without a preceding `else` or `elsif`
keyword — is a parse error; write `else { … }` instead.

```
if $ok { echo yes }                    # one-armed: type F Unit
if $ok { echo yes } else { echo no }   # two-armed
if $a { echo a } elsif $b { echo b } else { echo c }
if $flag !{f 'x'} else !{f 'y'}
if $is-mac                             # multiline
    { /bin/ls -G ...$args }
    else { /bin/ls --color=auto ...$args }
```

`if` takes a `Bool`; `?` reacts to failure. They do not cross: `if`
rejects non-Bool conditions (a type error). A predicate returning
`false` is still a successful command. When success must be inspected
as data, use `try`:

```
let r = _try { grep -q p f }
if $r[ok] { echo found } else { echo missing }
```

| Need                    | Mechanism |
|-------------------------|-----------|
| fallback on failure     | `?`       |
| two-branch on `Bool`    | `if`      |
| inspect success/failure | `try`     |
| multi-way pattern dispatch | `case`    |

### 4.5  Currying

`{ |x y z| M }` is the same as `{ |x| { |y| { |z| M } } }`. Under-application returns
the inner block; exact application runs `M`; over-application is an
arity error. Outside head position, curried blocks are explicit
with `$`. `_` discards a parameter. `{ || M }` is a syntax error; use
`{}` for a zero-argument block. The linter warns when an
under-applied block is discarded.

### 4.6  `return`

`return` is parsed before command dispatch; it evaluates at most one
value argument, producing `unit` if absent. Inside a parameterised
block it exits the enclosing block; at file scope it exits the file
with status 0. There is no non-local control flow.

### 4.7  Argument spreading

`...$xs` spreads a list into positional arguments. When arguments are
passed to an external command, `Int`/`Float` are formatted decimally
and `Bool` as `"true"`/`"false"`; any non-scalar value (`Bytes`,
`List`, `Map`, `Block`, `Handle`) is an error — argv is
textual.

## 5  Strings and bytes

Single quotes are literal; embed `'` by doubling (`'it''s'`). Double
quotes support `$`-interpolation, `!`-force, and the escapes
`\n \t \\ \0 \e \" \$ \!`. A bare `!` not followed by `{` or `$` is
literal (`\!` is explicit). To prevent `$name` from consuming a
following `[` as an index, use `$(name)[…]` to delimit the variable. Both string forms may span lines; the REPL
prompts for continuation when a quote is still open.  Use `dedent` to
strip common leading indentation from a multiline literal:

```
let msg = dedent '
    SELECT *
    FROM users
    WHERE active = true
'
```

Interpolation coerces scalars: `Int`/`Float` decimally, `Bool`
`"true"`/`"false"`, `Unit` `""`. Interpolating a non-scalar (`Bytes`,
`List`, `Map`, `Block`, `Handle`) is a type error.

Outside quotes, `$name` is a separate atom; concatenation is by
interpolation: `"$dir/file.txt"`, `"$host:$port/api"`.

## 6  Collections

`[a, b, c]` is a list; `[k: v, ...]` a map; `[]` empty list; `[:]`
empty map. Commas required; trailing commas permitted; newlines
inside `[…]` are insignificant.

`...` spreads. In a map, explicit entries take priority over spread
entries **regardless of source order**:

```
let cfg = [host: 'db', port: 5432]
let r   = [...$cfg, port: 9090]    # r : [port: Int, host: String]
```

The typing is scoped-label rows (§20.8).

Map keys may be bare words, quoted strings, or derefs; `[$k: $v]`
computes. Computed keys must be `String` at runtime.

## 7  Destructuring

Patterns appear on the LHS of `let` and as parameters of parameterised
blocks (including the blocks that make up `case` clauses, §17.1).
Pattern forms:

- `_` matches anything, binds nothing;
- `IDENT` matches anything, binds the name;
- list `[p₁, p₂, …, ...rest]` matches a list of sufficient length;
- map `[k₁: p₁, k₂: p₂ = default]` matches a map containing those
  keys (defaults fill missing keys).

Patterns are purely structural: there are no literal patterns. A
mismatch is a runtime error, catchable by `try` (and by the
`_try-apply` primitive that `case` uses).

```
let [first, ...rest] = $args
let [host: h, port: p = 8080] = $opts
let [name: n, addr: [city: c]] = $p
```

## 8  Modules

`use p` evaluates `p` and returns a map of its top-level bindings
excluding `_`-prefixed names. Paths resolve relative to the containing
file; `RAL_PATH` provides additional search paths.

Results are cached per-process by canonical absolute path (symlinks
resolved where available); the same file reached via different paths
is one entry. The cache is not invalidated on file change; to pick up
changes, restart the process. Module side effects run on first load
only.

`source p` evaluates into the current scope (not a child), merging
all bindings including `_`-prefixed ones; results are not cached.

Both detect circular references and report an error.

## 9  Environment

`$env` is a read-only map of environment variables; `$nproc` is CPU
count (`Int`). `within [env: …]` scopes overrides (§3.2); there is
no `setenv`.

`~/.ralrc` is a ral script whose last expression is a map with
optional keys `env`, `prompt`, `bindings`, `aliases`, `edit_mode`
(`"emacs"` | `"vi"`), `plugins`, `theme`. `bindings` populates the
interactive value namespace; `aliases` the interactive command
namespace; `plugins` is a list of plugin names loaded at startup
(§18.1).

The `theme` key is a map with optional fields `value_prefix` (string
prepended to every printed value; default `"=> "`) and `value_color`
(one of `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`,
`white`, `none`; default `yellow`).  Color is suppressed when stdout
is not a tty, `NO_COLOR` is set, or `RAL_INTERACTIVE_MODE=minimal`.

## 10  Error handling

### 10.0  Failure propagation

A nonzero exit or runtime error is a failure; propagation is always
on.

- sequential `a; b; c` — first failure halts;
- `?` chain `a ? b ? c` — first success wins (all arms must have the same return type);
- pipeline `a | b | c` — any non-SIGPIPE failure fails the pipeline;
- `try` — catches; handler runs; on success the body value is
  returned;
- `for`/`map` body — failure stops iteration; `return` exits the
  current iteration (the body is a parameterised block);
- `spawn` — failure captured in the handle, surfaced on `await`;
- top level — unhandled failure terminates with that status.

`try` suppresses; `guard` runs cleanup but does not suppress; `attempt`
(prelude) runs a thunk and discards both the result and any failure.

### 10.1  `_try`, `_audit`, `try`

`_try B` runs `B` and returns

```
[ok: Bool, value: α, status: Int, cmd: String, message: String,
 stdout: Bytes, line: Int, col: Int]
```

On success, `ok = true`, `status = 0`, `value` holds the result, `cmd`
is `""`, `message` is empty, `stdout` carries every byte the body wrote
to fd 1 during evaluation, and `line`/`col` point at the `_try` call
site. On failure, `ok = false`, `status` carries the failing exit code,
`cmd` names the failing command, `message` is the failure text (the
runtime error's own message, or the failing external command's stderr
decoded as UTF-8), `stdout` carries any bytes written before the
failure, and `line`/`col` point at the failing command's position in
source. The record's shape is the input shape `fail` accepts (modulo
`stdout`, which `fail` ignores), so `try { ... } { |e| fail $e }`
re-raises verbatim. A body that returns `false` is *successful* —
`ok = true`, `value = false`. Only runtime errors count as failure.

`_try` is a *complete* capture: §4.3's non-final-flush rule is
suppressed inside the body, so multi-stage bodies leave their full
transcript in `stdout` rather than streaming intermediate stages to
the terminal. Use `&` + `await` (§13.3) when you also need the raw
fd 2 bytes — `_try` rolls stderr into `message` as text.

`_audit B` runs `B` and returns its full execution tree (§10.3)
regardless of outcome. `grant […, audit: true]` does not build a tree;
it requests that capability-check events be included in whatever
tree is already being built.

`try` is the user-facing form, dispatching to a handler block on
failure:

```
try { make -j4 } { |err| echo $err[cmd] $err[status] }
let v = try { curl $primary } { |err| curl $fallback | from-json }
```

When a `try` catches an error, debug builds echo a one-line summary
to stderr (`ral: try caught error (line:col): message`). Release
builds stay silent unless `RAL_DEBUG` is set in the environment.
This surfaces errors that would otherwise be swallowed silently by
probes like `try { return $env[X] } { |_| return '' }` and by
library wrappers such as `_ed-tui` (§17) that use `try` to recover
from TUI failures.

### 10.2  `_guard`

`_guard body cleanup` runs `body`, then `cleanup` regardless of
outcome. Original failures propagate unchanged; cleanup failure is
logged and discarded. `guard` is the prelude wrapper.

### 10.3  Execution tree

A recursive record with a common prefix

```
Common = [kind: String, script: String, line: Int, col: Int,
          children: [Node], start: Int, end: Int, principal: String]
```

and kind-specific extensions. `Node` is open in its fields: consumers
read `kind` and `equal`-dispatch, accessing further fields through
row polymorphism (§20.1). V1 defines three shapes.

**Command** (`kind = 'command'`):

```
[cmd: String, args: [String], status: Int,
 stdout: Bytes, stderr: Bytes, value: α, …Common]
```

**Scope** (`kind = 'scope'`):

```
[scope: String, status: Int, value: α, …Common]
```

**Capability-check** (`kind = 'capability-check'`, emission rules
§11.4):

```
[resource: String, decision: String, granted: String, …Common]
```

Resource-specific fields: `exec` — `name: String`, `args: [String]`;
`fs` — `op: String`, `path: String`.

`script` is the source path, `""` for stdin, `"<prelude>"` for
prelude internals. Prelude wrappers record the user's call site.
`stdout` is emitted bytes; `value` is the returned datum (§4.2). For
pure-value builtins, `stdout` is empty. `start`/`end` are microseconds
since epoch; `principal` is `$USER` at record time, per-node so
extracted subtrees are self-describing.

Leaves are typically `command` or `capability-check`; interiors are
`scope` nodes for `spawn`, `grant`, `try`, `for`, `map`, `audit`,
`within`, `guard`. Tail-recursive calls are flattened: a
`while` of *N* iterations produces one node with *N* children.

Construction is lazy: plain execution builds nothing; `_try` builds
only the flat record; `_audit` builds its subtree; `ral --audit`
builds for the whole script. Capability-check nodes appear only
under `grant [… audit: true]`. Inside `_audit`, `stderr` per node is
capped at 64 KB; outside, `stderr` flows to the terminal.

### 10.4  Debugging

`ral --audit script.ral` writes the tree as JSON to stdout and script
output to stderr. No step-through debugger in v1.

### 10.5  Error messages

Runtime errors report mismatch, expected type or key set, received
value, source location, and a hint when obvious.

### 10.6  Accessing stderr

`stderr` flows to the terminal during normal execution.  Three
boundaries surface it differently:

- `try` / `_try` puts the failing command's stderr (decoded as UTF-8)
  into `message: String`.  No raw bytes — wrap the body in `&` if you
  need the bytes.
- `audit` / `_audit` records each external command's stderr as `Bytes`
  in its tree node, indexed by position.
- `await` of a `Handle α` (§13.3) returns the spawned block's full
  fd 2 capture as `stderr: Bytes` in the result record.

```
try { make } { |err| echo $err[message] }
let report = audit { make -j4 }
let text   = to-bytes $report[children][0][stderr] | from-string
let r      = make -j4 &
let r      = await $r
echo !{to-bytes $r[stderr] | from-string}
```

### 10.7  Signals

SIGINT/SIGTERM/SIGHUP set a flag; the evaluator checks the flag
between statements and begins unwinding. `guard` cleanup runs during
unwinding. A second signal is deferred until cleanup completes; a
third terminates immediately.

## 11  Capabilities (`grant`)

`grant C { B }` installs a deny-by-default capability context for
`B` using the dynamic-context mechanism of §3.1. Outside `grant`,
evaluation is ambient; inside, every omitted capability is denied.
`C` admits six keys: `exec`, `fs`, `net`, `audit`, `editor`, `shell`.

```
grant [
    exec: [git: [], make: []],
    fs:   [read: ['/home/project'], write: ['/tmp/build']],
    net:  true,
    audit: true,
] { … }
```

### 11.1  `exec`

Keyed by command name; each value is a list of allowed first arguments:

- `[]` — allow with any arguments;
- `[s₁, …]` — allow only when `argv[0] ∈ {sᵢ}`;

Unlisted names are denied.

### 11.2  `fs`

Governs ral builtins that touch the filesystem (`glob`, `list-dir`,
redirects, the `_fs` family). Two
sub-keys `read`, `write`, each a list of path prefixes. Paths are
checked against canonical absolute paths after resolution against the
active `within [dir: …]`; `.`/`..` are collapsed; symlinks are
resolved when the OS supplies that information. A `within [dir: …]`
inside `grant` cannot escape: only the resolved path matters.

`fs: [read: ['/data']]` is read-only. `fs: [:]` denies filesystem
access for ral builtins. `/dev/null` is exempt from both checks: it is
a discard device with no observable side effect.

`fs` does not restrict external programs' own I/O — they need their
binary, linker, and system libraries. Use `exec` and `within [handlers:]`
to shape that surface. Where OS sandboxing is available, write policy and
non-system read paths are enforced for externals as defence in depth.

### 11.3  `net`

Boolean. ral has no in-process network primitives, so `net` governs
only the network access of external programs spawned inside the grant.
Enforcement is OS-level: Seatbelt on macOS, a network-namespace unshare
via bubblewrap on Linux. Both are all-or-nothing — there is no
endpoint-level policy. On Windows there is no enforcement; `net: false`
emits a one-time stderr warning that the restriction will not be
applied.

### 11.4  `audit`

`audit: true` requests inclusion of capability-check events in any
active execution tree. It is additive: once enabled, it remains
enabled for nested grants. It does not itself build a tree. When an
execution tree is being collected *and* an enclosing `grant` has
`audit: true`, each `exec` and `fs` check emits a `capability-check`
node immediately before the gated action (or alone on denial). `net`
checks do not emit nodes. Field shape follows §10.3; `resource` is
`"exec"` | `"fs"`; `decision` is `"allowed"` | `"denied"`; `granted`
records the matched prefix on `fs` allows.

### 11.5  `editor`

Gates access to the line editor API (`_editor`, §18.1). Three
booleans:

- `read` — `get`, `history`, `parse`;
- `write` — `set`, `push`, `accept`, `ghost`, `highlight`, `state`;
- `tui` — `tui`.

```
grant [editor: [read: true, write: true, tui: false]] {
    _editor 'get'          # allowed
    _editor 'tui' { … }   # denied
}
```

Omitting `editor` entirely denies all sub-commands. The `editor`
capability is the primary gate for plugin code: plugin handlers are
wrapped in a `grant` derived from their declared manifest
capabilities (§18.1).

### 11.6  `shell`

Gates shell builtins that modify persistent process state beyond the
current command's lifetime.  Currently one boolean:

- `chdir` — `cd`.

```
grant [shell: [chdir: true]] {
    cd '/tmp'   # allowed
}
```

Omitting `shell` (or setting `shell: [chdir: false]`) denies `cd`.
`cd` is an ordinary core builtin: it is parsed and evaluated like any
other call, and obeys the capability gate uniformly in interactive,
script, and agent contexts.  Bare `cd` (no argument) means `cd ~`.

### 11.7  Attenuation

Nested grants can only reduce authority:

- `exec` — names intersect across layers; for a name allowed at every
  layer the subcommand lists intersect; a name not present in some
  outer layer's allow set cannot be re-introduced inside;
- `fs` — narrow by path containment (and for externals under OS
  sandboxing);
- `net` — boolean AND;
- `audit` — logical OR;
- `editor` — per-boolean AND (inner can only disable).

Authority may be restricted but never amplified. `grant` affects
ral-dispatched actions; if a permitted external program internally
spawns another, that inner spawn is constrained by the OS sandbox
when available, not by ral head lookup.

### 11.8  Platform support

In-process `exec`/`fs`/`net` checks apply everywhere. OS-level
enforcement:

- **macOS** — Seatbelt (`sandbox_init_with_parameters`); ral
  re-executes itself inside Seatbelt when `fs:` is present or `net`
  is `false`, so builtins are also kernel-enforced.
- **Linux** — bubblewrap + seccomp BPF (x86-64, AArch64); same
  re-execution strategy.
- **Windows** — no filesystem or network enforcement; in-process only.
  Each external inside a `grant` is assigned to a Job Object capping
  its process tree at 512.

## 12  Testing

Mock commands with `within [handlers:]`, inspect with `audit`, assert with
user-defined helpers:

```
let assert_eq = { |name expected actual|
    if !{equal $expected $actual} {} else {
        echo "FAIL: $name\n  expected: $expected\n  actual: $actual" 1>&2
        fail 1
    }
}

within [handlers: [deploy: { |args| echo ok }]] {
    let result = deploy prod
    assert_eq 'deploy prints ok' "ok" $result
}
```

`ral --audit test.ral` provides a structured report.

## 13  Concurrency

### 13.1  Model

`spawn B` creates an isolated copy of the evaluator state and runs
`B` concurrently. The child cannot affect the parent; communication
is only through `await`. Immutability makes the copy safe without
synchronisation. The implementation may use `fork(2)` or threads;
both satisfy the isolation contract.

The surface syntax is a trailing `&` on a pipeline:

```
let h = long-task arg &           # spawns, binds Handle α to h
grep pat file & ? echo fallback   # either arm of a ?-chain may be &
```

`&` attaches to a pipeline (§1), returns a `Handle α` immediately
(where α is the pipeline's value-output type), and does not wait;
`await h` (§13.3) resolves the handle to a record. `par` and `_fork`
produce the same kind of `Handle` programmatically.

### 13.2  `par` vs `map`

`par` and `spawn` are for I/O-bound work; `map` for in-process
transformation.

```
par { |f| convert $f } !{glob '*.wav'} $nproc
let results = map { |line| upper $line } $lines
```

`par` is the one exception in the concurrency family: it is `map`
parallelised, so it returns a list of *values* (with the await
envelope stripped) rather than a list of records.  Workers' stdout
and stderr are buffered per-task and discarded after the value is
extracted.  If you need the bytes, build the parallelism out of
`spawn`+`await` directly.

### 13.3  `await` and `race`

`await h` blocks until `h` completes and returns a record:

```
{ value:  α        # the block's return value
, stdout: Bytes    # everything the block wrote to fd 1
, stderr: Bytes    # everything the block wrote to fd 2
, status: Int      # 0 on success
}
```

`Handle` is parameterised: `spawn { B } : Handle α` where α is `B`'s
return type, and `await : Handle α → { value: α, … }` ties the two
together statically. A wrong-type consumer is a compile-time error.

`race [h₁,…]` returns the same record for the first completion and
marks the rest cancelled; awaiting a cancelled handle fails (catch
with `try`). Cancellation is handle-level; losers may continue in
the background on some platforms.

Each handle owns independent stdout and stderr buffers. During
execution, the spawned block's stdout and stderr write into these
buffers; nothing reaches the caller's terminal or capture context.
`await` does not auto-replay these bytes — they sit in `value.stdout`
and `value.stderr` until the user reads them. To restore the
bare-shell experience explicitly:

```
let h = make &
let r = await $h
echo !{to-bytes $r[stdout] | from-string}   # render bytes as a string and print
```

A redirect on the backgrounded pipeline (e.g. `cmd > log &`,
`cmd 2> err &`) sends bytes to the redirect target and leaves the
record's buffer empty. Buffers drain on first await; the record is
cached so a second `await` returns the same fields. Each buffer is
capped at 16 MiB; past the cap, a one-line truncation marker is
appended and further bytes are dropped, so high-volume spawns should
use an explicit redirect.

If the block raised, `await` re-raises rather than producing a
record. Wrap with `try` to recover:

```
let r = _try { await $h }
if $r[ok] { use $r[value] } else { recover $r[status] }
```

`par` is the exception in this family: it is `map` parallelised,
returning a list of values (with the envelope stripped) rather than a
list of records.

### 13.4  Child lifetime

Spawned children die when the host process exits. `disown h` detaches
a handle from script-level tracking; a subsequent `await h` fails.

### 13.5  Live watching: `watch`

`watch "LABEL" B` is a prelude function — not a keyword — that
spawns block `B` as a watched handle whose stdout and stderr flow
line-framed to the caller's stdout in real time, rather than being
buffered until `await`. Each emitted line is prefixed `[LABEL] `
for stdout and `[LABEL:err] ` for stderr. `watch` returns a
`Handle`, so awaiting, racing, and cancelling apply as for `&`.

The label is mandatory and may be any expression evaluating to a
String: a literal (`"build"`), an interpolation (`"job-$i"`), or a
variable deref (`$name`). The body is a `{ ... }` block passed as
a thunk.

```
let h = watch "build" { cargo build }
watch "deploy" { step-1; step-2 }
let target = "prod"
watch "build-$target" { make }     # interpolation
_await $h
```

`watch` is a one-line prelude alias over the `_watch` builtin:

```
let watch = { |label body| _watch $label $body }
```

Semantics relative to `&`:

- `watch` streams live; the handle's buffers remain empty, so the
  awaited record's `stdout`/`stderr` fields are empty. `&` buffers
  in the handle and surfaces the bytes in the record on `await`
  (§13.3).
- Each line is emitted atomically to the caller's stdout — the
  framing sink serialises complete `prefix + line + \n` writes
  through a single underlying stdout, so sibling watchers' lines
  interleave but never tear.
- Stderr of a watched handle also streams to the caller's stdout
  (prefixed `[LABEL:err] `); this differs from `&`, which buffers
  stderr and surfaces it in the record's `stderr` field on `await`.
- Under the interactive REPL the caller's stdout is routed through
  rustyline's external printer, so lines from backgrounded watchers
  appear above the active prompt rather than corrupting the line
  editor.

Caveats:

- **Child-side line buffering.** When a child's stdout is a pipe,
  most libcs block-buffer rather than line-buffer. `watch "py" { python … }`
  arrives in chunks rather than live lines unless the child flushes;
  `stdbuf -oL cmd` or the language's own line-buffer flag is the
  escape hatch.
- **Pipe backpressure.** The kernel pipe holds ~16–64 KiB; a slow
  fd 1 consumer will eventually block the child. This is the same
  behaviour as `cmd | slow-reader`.
- **`_race` and cancellation.** A cancelled watched handle's thread
  exits when the child's writes return; any partial line carried
  inside the framing sink is flushed at thread teardown.
- **`--audit` interaction.** Audit reserves fd 1 for structured
  output. Under `--audit`, watched output is still written to
  stdout; redirect the audit log with `--audit-file` if both are
  wanted.

## 14  Scripts

`$args` is the argument list — user-supplied arguments only, with no
program name in `$args[0]`.  `$script` is the path of the file
currently executing, as handed to the interpreter.  Inside a loaded
module or plugin `$script` refers to *that* file, matching the scope
used for module-relative path resolution (§8).  Under `ral -c`, in
the REPL, and while the prelude is loading, `$script` is unbound —
reading it fails like any undefined variable.

```
#!/usr/bin/env ral
let [target, port] = $args
echo "deploying to $target on $port"
within [dir: $target] { git pull ? within [env: [PORT: $port]] { make deploy } }
```

A common idiom is to self-locate relative to `$script`:

```
let here = dir $script
let repo_root = resolve-path "$here/.."
```

## 15  Unix interface

`ask "prompt"` reads one line from `/dev/tty` (not stdin) and returns
it as a `String`. Fails on EOF; an empty line is the empty string
`""`, distinct from EOF.

**Codecs.** A codec is a pair `from-X` (decoder) / `to-X` (encoder).
Decoders read bytes from the pipeline and return a structured value;
encoders take a value, emit those bytes on the pipe, and return them
as `Bytes`.

| Decoder        | In      | Out                              |
|----------------|---------|----------------------------------|
| `from-line`    | `Bytes` | `String` (trailing `\n` dropped) |
| `from-string`  | `Bytes` | `String`                         |
| `from-lines`   | `Bytes` | `[String]`                       |
| `from-json`    | `Bytes` | JSON value                       |
| `from-bytes`   | `Bytes` | `Bytes`                          |

All text decoders fail on invalid UTF-8; `from-json` additionally
fails on invalid JSON; `from-bytes` cannot fail.

| Encoder      | In                | Out     |
|--------------|-------------------|---------|
| `to-string`  | `String`          | `Bytes` |
| `to-lines`   | `[String]`        | `Bytes` |
| `to-json`    | JSON-serialisable | `Bytes` |
| `to-bytes`   | `Bytes`           | `Bytes` |

`to-bytes` accepts only `Bytes`; encode a string with `to-string`.
Encoders are first-class; partial application works (`map to-json
$values`).  There is no explicit-argument decoder.  When a value is in
hand, route it through the matching encoder and pipe into the
decoder: `to-bytes $b | from-string`, `to-string $s | from-json`.

`split` and `match` take explicit arguments rather than reading from
the pipeline. `glob` returns matching paths as a sorted list (empty
on no match).

**Redirects.** `>`, `>~`, `>>`, `2>`, `2>&1`, `<`. Stage modifiers, not
values. `within [dir: …]` scopes directory changes (§3.2); `cd` exists only
in the interactive layer. `cwd` returns the current directory as
`String`.

`>` writes atomically to a regular file: the file appears in one step
and a concurrent reader observes either the old contents or the new,
never a partial write. Non-regular targets (TTYs, `/dev/null`, named
pipes, sockets) cannot be replaced atomically; for those `>` falls
back to streaming truncate-and-write.

`>~` is the streaming truncate redirect — POSIX `>` semantics. Bytes
land as they arrive; readers may observe a half-written file. Use
`>~` when streaming visibility is needed (logs, FIFOs that should not
be replaced) or when `>` would refuse the target.

`>>` appends. `<` reads.

**File I/O.** File reads and writes are redirect-and-codec: a decoder
on `< $path` for reads, an encoder on `> $path` for writes.

```
let body = from-string < $p          # read string
let xs   = from-lines  < $p          # read list of lines
let v    = from-json   < $p          # read JSON
let b    = from-bytes  < $p          # read raw bytes

to-string $body > $p                 # atomic write
to-json   $v    > $p                 # atomic write
to-string $body >~ $p                # streaming write
echo done       >> $p                # append
```

### 15.1  Terminal capability and minimal mode

The interactive frontend decides once, at startup, whether the terminal
accepts ANSI escape sequences and whether terminal round-trip queries
(cursor-position report, device attributes) are worth attempting.  The
decision is recorded in a `TerminalState` value and exposed to user code
as the binding `$TERMINAL`.

```
$ ral
ral $ echo $TERMINAL[supports_ansi]
true
ral $ echo $TERMINAL[is_tmux]
false
```

Fields of `$TERMINAL`:

| Name            | Type   | Meaning                                         |
|-----------------|--------|-------------------------------------------------|
| `stdin_tty`     | Bool   | `isatty(0)` at startup                          |
| `stdout_tty`    | Bool   | `isatty(1)` at startup                          |
| `stderr_tty`    | Bool   | `isatty(2)` at startup                          |
| `supports_ansi` | Bool   | stdout is a tty and TERM accepts ANSI           |
| `no_color`      | Bool   | `NO_COLOR` is set (and not overridden)          |
| `is_tmux`       | Bool   | `TMUX` is set                                   |
| `is_asciinema`  | Bool   | `ASCIINEMA_REC` is set                          |
| `is_ci`         | Bool   | heuristic CI detection                          |
| `ui_ansi_ok`    | Bool   | convenience: may the UI emit ANSI?              |
| `mode`          | String | resolved `RAL_INTERACTIVE_MODE` (see below)     |

The environment variable `RAL_INTERACTIVE_MODE` forces a mode:

| Value       | Behaviour                                                |
|-------------|----------------------------------------------------------|
| unset, `auto`  | capability detection decides                          |
| `minimal`, `dumb`, `plain` | no ANSI from the UI, no CPR query         |
| `full`      | emit ANSI even when stdout is piped                      |

Under `minimal` the highlighter returns input unchanged, ghost-text hints
carry no dim styling, and the per-prompt cursor-position query is
skipped.  An RC prompt hook that wants to degrade cleanly should read
`$TERMINAL[supports_ansi]` rather than hard-coding colour escapes:

```
prompt: {
    if $TERMINAL[supports_ansi] { return "\e[32m$CWD\e[0m $ " }
    return "$CWD $ "
}
```

Map access uses the `$m[key]` form (§6), so the same rule applies to
`$TERMINAL`.

The point is that a terminal that cannot render ANSI, or a session that
the user has told us to treat as dumb, must not see escape sequences at
all — not in the prompt, not in syntax highlighting, and not in the
form of a CPR query that will never be answered.  Everything else stays
the same: the language is unchanged and scripts run identically under
any mode.

## 16  Builtins

Names in `_` are implementation primitives consumed by the prelude,
not the preferred user surface. Return-type rules follow §4.2.

### 16.1  User-facing

| Builtin | Purpose |
|---|---|
| `fail` | Raise a failure with an error record `fail [status: N, message?: M, ...]`; `fail $e` re-raises a caught error verbatim (`fail [status: 0]` is an error) |
| `echo` | Write UTF-8 bytes to stdout; return as `String` |
| `source`, `use` | §8 |
| `within`, `grant` | §3.2, §11 |
| `glob` | Sorted path glob |
| `length` | Length of list, map, string, or bytes |
| `keys` | Map keys in insertion order |
| `has` | Test map membership |
| `ask` | `/dev/tty` prompt; fails on EOF |
| `write-json` | Write a value as JSON to a file |
| `which` | Resolve lookup target; `String` or failure |
| `cwd` | Current directory as `String` |
| `grep-files` | Regex over files; returns `[[file: String, line: Int, text: String]]` (1-based lines, ordered by input file then match order) |

### 16.2  Predicates

All return `Bool`; a `false` return is successful.

`exists`, `is-file`, `is-dir`, `is-link`, `is-readable`,
`is-writable`, `is-empty` (List, Map, Bytes, or String),
`equal` (structural), `lt`, `gt` (lexicographic on `String`).

### 16.3  Map operations

`diff` — keys in `a` not in `b`.

### 16.4  Private substrate

`_each`, `_map`, `_filter`, `_fold`, `_sort-list`,
`_sort-list-by`, `_fold-lines`; `_fork`, `_await`, `_race`, `_cancel`,
`_disown`, `_par`; `_try`, `_try-apply`, `_audit`, `_guard`;
`_decode`, `_encode` (used via the `from-X`/`to-X` wrappers, §15);
families `_str` (`upper`, `lower`, `replace`, `replace-all`, `find-match`,
`find-matches`, `join`, `slice`, `split`, `match`, `shell-quote`, `shell-split`, `dedent`), `_path` (`stem`, `ext`, `dir`, `base`,
`resolve`, `join`), `_fs` (`read`, `lines`, `size`, `mtime`, `empty`, `write`,
`copy`, `rename`, `remove`, `mkdir`, `list`, `tempdir`, `tempfile`),
`_convert` (`int`, `float`, `string`), `_editor` (`get`, `set`,
`push`, `accept`, `tui`, `history`, `parse`, `ghost`, `highlight`,
`state`; §18.1), `_plugin` (`load`, `unload`; §18.1).

`_try-apply f val` applies `f` to `val`, catching *only*
pattern-mismatch failures from destructuring `f`'s parameter; it
returns `[ok: true, value: r]` on success and `[ok: false, value:
unit]` on mismatch. Any other failure in `f`'s body propagates.

The surface mechanism is a distinguished error kind, `PatternMismatch`,
raised by destructuring — a block parameter `{ |pat| … }`, an explicit
`let pat = …`, or a list/map index shape mismatch — when the value does
not fit the pattern. `_try-apply` catches this kind **at the parameter
bind step only**. If `f`'s body itself raises `PatternMismatch` (e.g. a
failing inner `let [a, b] = …`), it propagates like any other error, so
`_try-apply` never silently swallows a bug. `case` (§17.1) is built on
top of this: it tries each clause's parameter pattern in turn and
commits to the first one that matches.

### 16.5  Bundled coreutils

With `--features coreutils`, ≈ 60 GNU-compatible utilities
(`ls`, `cat`, `wc`, `head`, `tail`, `cp`, `mv`, `rm`, `sort`,
`tr`, `uniq`, …) are in-process byte-output builtins. On Unix
they are normally on `PATH` and the feature is unnecessary; on
Windows it produces a self-contained binary. `diffutils` adds `diff`
and `cmp`. All are byte-output commands.

`--features grep` enables all regex-backed builtins using ripgrep's
engine: `grep-files`, `match`, `split`, `replace`, `replace-all`,
`find-match`, `find-matches`. Without this feature these builtins are
present but raise an error at runtime. For byte-stream grep, use the
system `grep` on `PATH`.

## 17  Prelude

The prelude is ral. All names are ordinary bindings in scope before
user code runs, elaborated by the SCC rule of §3. The linter warns
on shadowing. Names are implicit in head position and explicit
elsewhere with `$` (§4); currying (§4.5) supports partial
application.

### 17.1  Definitions

```
# Strings (via _str)
let upper   = { |s|          _str 'upper' $s }
let lower   = { |s|          _str 'lower' $s }
let replace      = { |pattern repl s|  _str 'replace' $pattern $repl $s }
let replace-all  = { |pattern repl s|  _str 'replace-all' $pattern $repl $s }
let find-match   = { |pattern s|       _str 'find-match' $pattern $s }
let find-matches = { |pattern s|       _str 'find-matches' $pattern $s }
let join    = { |sep items|  _str 'join' $sep $items }
let slice   = { |s start n|  _str 'slice' $s $start $n }
let split   = { |pattern s|  _str 'split' $pattern $s }
let match   = { |pattern s|  _str 'match' $pattern $s }
let lines   = { |s|          split '\n' $s }
let words   = { |s|          split '\s+' $s }
let dedent  = { |s|          _str 'dedent' $s }

# Paths (via _path)
let stem         = { |p|     _path 'stem' $p }
let ext          = { |p|     _path 'ext' $p }
let dir          = { |p|     _path 'dir' $p }
let base         = { |p|     _path 'base' $p }
let resolve-path = { |p|     _path 'resolve' $p }
let path-join    = { |parts| _path 'join' $parts }

# Filesystem (via _fs) — read-only
let line-count  = { |p| _fs 'lines' $p }
let file-size   = { |p| _fs 'size' $p }
let file-mtime  = { |p| _fs 'mtime' $p }
let file-empty  = { |p| _fs 'empty' $p }
# File reads use redirects: `from-string < $p`, `from-lines < $p`,
# `from-json < $p`, `from-bytes < $p`, `from-line < $p`.
# `read-file-range PATH START COUNT` (1-indexed slice) and
# `read-file-numbered PATH` (cat -n shape) are agent-citation sugars
# defined later in the prelude (they need `enumerate`, `take`, `drop`,
# `map`); both delegate to `from-lines < $p` for CRLF handling and
# trailing-newline normalisation.

# Filesystem (via _fs) — mutating
# File writes use redirects: `to-string $s > $p`, `to-json $v > $p`,
# `to-lines $xs > $p`, `to-bytes $b > $p`.  `>` is atomic: either the
# old or the new contents are observed, never a half-written file.
# `>~` is streaming truncate (POSIX `>` semantics); `>>` is append.
let copy-file   = { |src dest|  _fs 'copy' $src $dest }
let move-file   = { |src dest|  _fs 'rename' $src $dest }
let remove-file = { |p|         _fs 'remove' $p }
let make-dir    = { |p|         _fs 'mkdir' $p }
let list-dir    = { |p|         _fs 'list' $p }
# list-dir returns List of Map with name (String),
# type ("file"/"dir"/"symlink"/"other"), size (Int bytes),
# mtime (Int, Unix epoch seconds).  Sorted by name.
let temp-dir   = { _fs 'tempdir' }
let temp-file  = { _fs 'tempfile' }
# temp-dir / temp-file create fresh paths in the system temporary
# directory and return the created path as a String.

# Value coercions (via _convert)
let int   = { |v| _convert 'int' $v }
let float = { |v| _convert 'float' $v }
let str   = { |v| _convert 'string' $v }
# str does not accept Bytes; decode bytes with `from-string < $p`
# (path) or `to-bytes $b | from-string` (Bytes value already in hand).

# Byte-channel encoders
let to-json   = { |v| _encode json $v }
let to-lines  = { |v| _encode lines $v }
let to-line   = { |s| _encode line $s }
let to-string = { |v| _encode string $v }
let to-bytes  = { |v| _encode bytes $v }

# Byte-channel decoders (read pipe / `< PATH` redirect)
let from-json   = { _decode json }
let from-lines  = { _decode lines }
let from-string = { _decode string }
let from-bytes  = { _decode bytes }
let from-line   = { _decode line }
# JSON mapping: object→Map, array→List, string→String,
# integer→Int, other number→Float, boolean→Bool, null→unit.
# `from-line` strips one trailing newline; `to-line` appends one.
# A `Bytes` value already in hand can be fed to a decoder via the
# `to-bytes` encoder stage: `to-bytes $b | from-X`.

# Control flow.  Logical connectives are syntax — use `$[...]` (§2).
let for   = { |items body| _each $items $body }

# Functional combinators — data-last
let each         = { |f items|      _each $items $f }
let map          = { |f items|      _map $f $items }
let filter       = { |f items|      _filter $f $items }
let fold         = { |f init items| _fold $items $init $f }
let reduce       = { |f items|
    if !{is-empty $items} { fail 1 } else {
        let [head, ...tail] = $items
        fold $f $head $tail
    }
}
let sort-list    = { |items|   _sort-list $items }
let sort-list-by = { |f items| _sort-list-by $f $items }

let reverse = { |items|
    let _rev = { |xs acc|
        if !{is-empty $xs} { return $acc } else {
            let [h, ...t] = $xs
            _rev $t [$h, ...$acc]
        }
    }
    _rev $items []
}

let get = { |m key default|
    if !{has $m $key} { return $m[$key] } else { return $default }
}

# List combinators (all tail-recursive)
let take-while = { |pred items|
    let _go = { |xs acc|
        if !{is-empty $xs} { return !{reverse $acc} } else {
            let [head, ...rest] = $xs
            if !{$pred $head} { _go $rest [$head, ...$acc] } else {
                return !{reverse $acc}
            }
        }
    }
    _go $items []
}
let drop-while = { |pred items|
    if !{is-empty $items} { return [] } else {
        let [head, ...rest] = $items
        if !{$pred $head} { drop-while $pred $rest } else { return $items }
    }
}
let take = { |n items|
    let _go = { |k xs acc|
        if $[$k <= 0 || !{is-empty $xs}] { return !{reverse $acc} } else {
            let [head, ...rest] = $xs
            _go $[$k - 1] $rest [$head, ...$acc]
        }
    }
    _go $n $items []
}
let drop = { |n items|
    if $[$n <= 0 || !{is-empty $items}] { return $items } else {
        let [_, ...rest] = $items
        drop $[$n - 1] $rest
    }
}
let zip = { |a b|
    let _go = { |xs ys acc|
        if $[!{is-empty $xs} || !{is-empty $ys}] { return !{reverse $acc} } else {
            let [xh, ...xt] = $xs
            let [yh, ...yt] = $ys
            _go $xt $yt [[$xh, $yh], ...$acc]
        }
    }
    _go $a $b []
}

# Error handling
let retry = { |n body|
    try $body { |err|
        if $[$n > 1] { retry $[$n - 1] $body } else { fail $err[status] }
    }
}

# Dispatch
# case tries each clause (a parameterised block) in order, applying
# it to val.
# The first clause whose parameter pattern destructures val successfully
# wins; its body runs with the bound names in scope.  case is purely
# structural: it dispatches on the shape of val (list length, map keys,
# nesting), not on value equality.  For value tests, use equal inside
# a clause body or an explicit if.  If no clause matches, case fails.
# A trailing `{ |_| … }` is the catch-all.
#
#   case $msg [
#       { |[]|                  empty }
#       { |[x]|                 single $x }
#       { |[head, ...tail]|     cons $head $tail }
#       { |[kind: k, body: b]|  tagged $k $b }
#       { |_|                   fail 2 }
#   ]
let case = { |val clauses|
    if !{is-empty $clauses} { fail 1 } else {
        let [clause, ...rest] = $clauses
        let r = _try-apply $clause $val
        if $r[ok] { return $r[value] } else { case $val $rest }
    }
}

# Concurrency
let spawn  = { |body|      _fork $body }
let await  = { |handle|    _await $handle }
let race   = { |handles|   _race $handles }
let cancel = { |handle|    _cancel $handle }
let par    = { |f items j| _par $f $items $j }
let disown = { |handle|    _disown $handle }

# Failure suppression
let attempt = { |body| let _ = _try $body }

# Cleanup
let guard = { |body cleanup| _guard $body $cleanup }

# Observability
let audit = { |body| _audit $body }

# Map utilities
let entries   = { |m| map { |k| return [$k, $m[$k]] } !{keys $m} }
let values    = { |m| map { |k| return $m[$k] } !{keys $m} }
let union     = { |a b| fold { |m k| if !{has $m $k} { return [...$m, $k: $b[$k]] } else { return $m } } $a !{keys $b} }
let intersect = { |a b| fold { |m k| return [...$m, $k: $a[$k]] } [:] !{filter { |k| has $b $k } !{keys $a}} }

# Utilities
let sum   = { |items| fold { |acc x| return $[$acc + $x] } 0 $items }
# seq produces the half-open range [start, end).
let seq = { |start end|
    let _go = { |i acc|
        if $[$i < $start] { return $acc } else {
            _go $[$i - 1] [$i, ...$acc]
        }
    }
    _go $[$end - 1] []
}
let flat-map = { |f items| concat !{map $f $items} }
let first = { |pred items|
    if !{is-empty $items} { fail 1 } else {
        let [head, ...rest] = $items
        if !{$pred $head} { return $head } else { first $pred $rest }
    }
}
let enumerate = { |items|
    map { |i| return [index: $i, item: $items[$i]] } !{seq 0 !{length $items}}
}
let concat = { |xss|
    let _go = { |remaining acc|
        if !{is-empty $remaining} { return $acc } else {
            let [head, ...rest] = $remaining
            _go $rest [...$acc, ...$head]
        }
    }
    _go $xss []
}
let chain = { |init fns| fold { |acc f| $f $acc } $init $fns }

# Streaming (stdin, line by line)
let fold-lines = { |f init| _fold-lines $f $init }
# map-lines and filter-lines emit bytes and return Unit.
let map-lines = { |f|
    _fold-lines { |_ line| echo !{$f $line}; return unit } unit
}
let filter-lines = { |pred|
    _fold-lines { |_ line|
        if !{$pred $line} { echo $line }
        return unit
    } unit
}
let each-line = { |f|
    _fold-lines { |_ line| $f $line; return unit } unit
}

# String quoting
let shell-quote = { |s| _str 'shell-quote' $s }
let shell-split = { |s| _str 'shell-split' $s }

# Editor state — plugin API (interactive-only; _editor errors in scripts)
let _ed-get    = { _editor 'get' }
let _ed-set    = { |s| _editor 'set' $s }
let _ed-text   = { let s = _editor 'get'; return $s[text] }
let _ed-cursor = { let s = _editor 'get'; return $s[cursor] }
let _ed-keymap = { let s = _editor 'get'; return $s[keymap] }
let _ed-lbuffer = {
    let s = _editor 'get'
    slice $s[text] 0 $s[cursor]
}
let _ed-set-lbuffer = { |l|
    let s = _editor 'get'
    let r = slice $s[text] $s[cursor] $[!{length $s[text]} - $s[cursor]]
    _editor 'set' [text: "$l$r", cursor: !{length $l}]
}
let _ed-insert = { |str|
    let s = _editor 'get'
    let l = slice $s[text] 0 $s[cursor]
    let r = slice $s[text] $s[cursor] $[!{length $s[text]} - $s[cursor]]
    _editor 'set' [text: "$l$str$r", cursor: $[$s[cursor] + !{length $str}]]
}
let _ed-tui = { |body|
    try {
        let output = _editor 'tui' $body
        return [output: $output, status: 0]
    } { |e|
        return [output: !{to-bytes $e[stderr] | from-string}, status: $e[status]]
    }
}

# Membership test
let elem = { |x items|
    $[not !{equal unit !{first { |y| equal $y $x } $items}}]
}

# Plugin lifecycle
let load-plugin   = { |name options| _plugin 'load' $name $options }
let unload-plugin = { |name| _plugin 'unload' $name }
```

## 18  Interactive layer

Line editing, history, completion, and prompt rendering are
host-language features.

| Builtin | Purpose |
|---|---|
| `cd` | Change working directory (persistent) |
| `jobs`, `fg`, `bg` | Job control |
| `quit` | Exit (≡ Ctrl-D) |
| Ctrl-Z | Suspend foreground |

**SIGINT.** Foreground command: delivered to process group. Prompt:
discards current line. Script: unwinding (§10.7).

### 18.1  Plugins

A plugin is an ordinary ral module that returns either a manifest
map or a block taking its configuration as explicit parameters and
returning a manifest map. No new language constructs are needed; a
plugin's knobs are ordinary block parameters, not a magic `$config`
binding.

**Manifest schema:**

```
[
    name: Str,
    capabilities: [exec: …, fs: …, net: …, editor: …, shell: …],
    hooks: [event-name: {handler}],
    keybindings: [[key: Str, handler: {F Bool}]],
    aliases: [name: {[Str] → F Any}],
]
```

All fields except `name` are optional. `capabilities` is parsed into a
deny-by-default `grant` context (§11). Each hook, keybinding handler, and
plugin-registered alias runs with that grant pushed on top of the caller's
current capabilities stack. Therefore the effective authority is the intersection
of the caller's current authority and the plugin's manifest:

```
effective(plugin call) = caller capabilities stack ∩ plugin capabilities
```

A plugin can never exceed what its manifest declares, and it also cannot
escape an enclosing user grant.  `aliases` are registered into the shell's
alias namespace at load time and removed at unload.  An alias name
collision with an existing `rc` or plugin-registered alias is a load-time
error.

**`_plugin 'load' <name-or-path> [<options-map>]`** resolves a plugin
file (`~/.config/ral/plugins/$name.ral`, `$RAL_PATH`, or a literal
path), evaluates it, and registers the resulting plugin. If the
module's return value is a block, the options map is applied to it
as a single argument to obtain the manifest; if omitted it defaults
to `[:]`. If the module returns a manifest map directly, a non-empty
options map is a load-time error.  **`_plugin 'unload' <name>`**
removes it.

**`_str 'shell-quote' <s>`** quotes a single argument for a POSIX-style
shell. The exact quoting form is implementation-defined; it is chosen
to preserve one argument when re-read by a compatible shell parser and
to round-trip with `shell-split` for ordinary text arguments.

**`_str 'shell-split' <s>`** tokenizes a shell-quoted string into a
list of arguments using POSIX rules (`'`, `"`, and `\` are honored).
Errors on unterminated quotes. Example: `'bat --color=always {}'`
becomes a single element. The inverse of `shell-quote` when round-
tripping whitespace-sensitive arguments.

**`grep-files <pattern> <files>`** searches each file in order and
returns a list of maps `[file: String, line: Int, text: String]` for
every matching line. `line` is 1-based; `text` is the matched line
without its trailing newline. This is a structured value builtin, not a
byte-stream command like `grep`.

**`_editor`** provides ten sub-commands for interacting with the
line editor from plugin handlers. All are gated by the `editor`
capability (§11.5).

| Sub-command | Gate | Description |
|---|---|---|
| `get` | read | `→ [text: Str, cursor: Int, keymap: Str]` |
| `set` | write | `[text: Str, cursor: Int] →` update buffer |
| `push` | write | save buffer to stack, clear |
| `accept` | write | mark buffer for immediate execution |
| `tui` | tui | suspend editor, run `{block}`, capture its stdout as `Str` |
| `history` | read | `<prefix> <limit> → [Str]` prefix search |
| `parse` | read | tokenize buffer at cursor |
| `ghost` | write | set/clear ghost (hint) text |
| `highlight` | write | set highlight spans `[{start, end, style}]` |
| `state` | write | per-plugin persistent state: `<default> <updater>` |

`_editor 'tui'` installs a capture buffer around the body — the same
mechanism `let` uses at a byte-mode boundary (§4.3). External commands
inside the body write their stdout into that buffer; stderr still goes
to the TTY, so a curses-style UI (fzf, etc.) renders normally. On
return, if the body produced a non-`Unit` value it wins; otherwise the
captured bytes are UTF-8-decoded (one trailing newline stripped) and
returned as a `Str`. This gives the idiomatic zsh pattern
`result=$(fzf)` directly inside a handler, without the plugin needing
to know about pipes.

**Hooks.** Five events; handlers are thunks called with the real
`Env` (not a snapshot), wrapped in `grant`:

| Event | Signature | When |
|---|---|---|
| `buffer-change` | `{Str → Str → Int → F Unit}` | keystroke changes buffer |
| `pre-exec` | `{Str → F Unit}` | before command evaluation |
| `post-exec` | `{Str → Int → F Unit}` | after command; receives exit status |
| `chpwd` | `{Str → Str → F Unit}` | after `cd`; old → new path |
| `prompt` | `{Str → F Str}` | before prompt; receives base prompt |

`buffer-change` hooks run inside the line editor (from
`Hinter::hint()`). The runtime lock is released before calling
the evaluator: handlers write to `env.plugin_context` rather than
shared state, avoiding reentrancy. `_editor 'tui'` is rejected
inside `buffer-change` handlers (`in_readline` flag).

**Keybinding dispatch.** Handlers are tried in reverse load order.
Return `true` to consume the key, `false` to pass it to the next
handler (or built-in editing). On error, the key is treated as
consumed and the error logged.

`_editor 'accept'` marks the buffer for immediate execution after
the handler returns, instead of re-entering the line editor. This
is the plugin equivalent of zsh's `zle accept-line`.

`_editor 'push'` saves the current buffer and clears it. On the
next prompt, the saved buffer is restored (a stack, so nested pushes
work). Combined with `accept`, this gives the zsh `push-line` +
`accept-line` pattern used by fzf-cd.

**`~/.ralrc` integration.** The RC map gains an optional `plugins`
key, a list loaded at startup. Each entry is a map
`[plugin: Str, options?: Map]`.  `options` (if present) is passed as
the single argument to the plugin's top-level block; for plugins
that take no configuration the key is omitted:

```
return [
    plugins: [
        [plugin: 'syntax-highlight'],
        [plugin: 'fzf-files',   options: [key: 'ctrl-t']],
        [plugin: 'fzf-cd',      options: [key: 'alt-c']],
        [plugin: 'fzf-history', options: [key: 'ctrl-r']],
    ],
    …
]
```

Equivalent to calling `_plugin 'load' name options` for each entry.
Unknown top-level keys in an entry are warned and ignored so the
schema can grow (e.g. `enabled:`, `when:`) without breaking parsers.

## 19  Miscellaneous rules

- **SIGPIPE exception.** Exit 141 on a non-final pipeline stage is
  treated as success; the pipeline does not fail.
- **`~` expansion.** Bare `~` and `~/…` expand to `$env[HOME]`.
- **`[` adjacency.** Postfix indexing (§1.1) requires no whitespace
  before `[`.
- **`_` prefix visibility.** `use` (§8) excludes `_`-prefixed names
  from the returned module map.

## 20  Type system

Hindley–Milner with let-polymorphism, extended with row polymorphism
for records.

### 20.1  Value types

```
A ::= Unit | Bytes | Bool | Int | Float | String
    | [A]                     — homogeneous list
    | [String:A]              — homogeneous map
    | [l₁:A₁, …, lₙ:Aₙ]       — closed record
    | [l₁:A₁, …, lₙ:Aₙ | ρ]   — open record (row variable ρ)
    | {B}                     — thunk
    | Handle
    | α                       — type variable
    | ρ                       — row variable
```

`[String:A]` is homogeneous in values. Record types carry one type per
label. Both have the same runtime representation; the distinction is
type-level only.

Open records arise in polymorphic contexts: a function reading field
`name` from any record has argument type `[name:α | ρ]`. Records with
a `kind: String` discriminant (e.g. `_audit` tree nodes, §10.3) are
consumed by reading `kind` and dispatching with `equal`; kind-specific
fields are typed through row polymorphism.

### 20.2  Command types

```
B ::= F[I,O] A  |  A → B  |  β
I,O ::= ∅ | Bytes | Values(A) | μ
```

`F[∅,∅] A` is abbreviated `F A`. A nullary block has type `{B}`; a
parameterised block has type `{A → B}` (a thunked function).

### 20.3  Byte-output commands

Principal signatures:

| Kind                                | Shape            |
|-------------------------------------|------------------|
| External command                    | `F[I, Bytes] String` |
| Byte-output builtin (`echo`, `grep`)| `F[I, Bytes] String` |
| Streaming reducer (`map-lines`, `filter-lines`, `each-line`) | `F[Bytes, Bytes] Unit` |
| Encoder (`to-X`)                    | `F[∅, Bytes] Bytes` |
| Decoder (`from-X`)                  | `F[Bytes, ∅] A` |
| Value builtin (`length`, `line-count`) | `F[∅, ∅] A` |

For external commands and byte-output builtins, bytes flow on the
output channel but the return type is `String`; decoding of the
emitted stream happens at materialisation (at the `let` binding), not
inside the pipe. Streaming reducers emit line by line without
buffering; they have no accumulated result and return `Unit`. Codec
pairs are the sole dual-channel commands: encoders emit on the pipe
and also return the bytes as `Bytes`; decoders consume from the pipe
and return a structured value. `from-bytes` has `A = Bytes` with
output mode `∅`.

### 20.4  Pipelines

A stage has type `F[I,O] A`; connection requires `O_left = I_right`:

```
ls | grep foo | wc -l
F[∅,Bytes] String   F[Bytes,Bytes] String   F[Bytes,Bytes] String

ls | from-lines | map { |line| … }
F[∅,Bytes] String   F[Bytes,∅] [String]    F[∅,∅] […]
```

Mismatches are caught at type-check time. The non-final return is not
threaded on byte edges; composition follows the output mode (§4.2).

### 20.5  Polymorphism

Types are inferred; no annotations. Generalisation occurs at
`let`:

```
let id = { |x| return $x }         -- id : {α → F α}
id 42                              -- F Int
id 'hello'                         -- F String
```

Generalisation follows the SCC elaboration of §3: a non-recursive SCC
is generalised at the binding point; a mutually-recursive SCC is
monomorphic within the group and generalised after its fixed point is
reached.

### 20.6  Type errors

Abort with exit status 1. Messages include source position, expected
type, and inferred type:

```
script.ral:12:5: type error: type mismatch: Int vs String
```

### 20.7  Implementation

Inference runs on the command/value IR after elaboration, not the
surface AST. Source positions travel through `Comp::Pos` markers.
The algorithm is Algorithm W with Rémy-style row polymorphism;
unification uses path-compressing union-find over four variable
kinds: value (α), command (β), mode (μ), and row (ρ).
Let-generalisation uses the standard free-variable calculation,
skipping variables free in the outer environment.

### 20.8  Row polymorphism and record types

Row typing follows Leijen (2005) with scoped labels: duplicate
labels are permitted in rows; selection returns the first
occurrence; extension prepends, shadowing earlier entries without a
restriction operator. A map literal with a single spread flows the
spread source's field types through into the result type; with
multiple spreads the result is open but imprecise.

Literal maps with static keys infer as closed records:

```
let r = [host: 'prod', port: 8080]
-- r : [host: String, port: Int]
```

Non-integer literal key access constrains the target to carry that
field:

```
$r[host]     -- String
$r[port]     -- Int
$r[stattus]  -- type error
```

Row polymorphism permits accepting any record with at least a given
field set:

```
let greet = { |x| echo "hello $x[name]" }
-- greet : ∀α ρ. {[name:String | ρ] → F String}
```

A record is usable where `[String:A]` is expected when all its fields
have type `A`; a heterogeneous record is not.

Record-returning builtins:

| Builtin | Return record |
|---|---|
| `_try`   | `[ok:Bool, value:α, status:Int, cmd:String, stderr:Bytes, line:Int, col:Int]` |
| `_audit` | `Node` (§10.3) |

Dynamic keys fall back to the homogeneous map rule; list indexing
(`$xs[0]`) is unaffected.

### 20.9  CBPV correspondence

The formal model is call-by-push-value.

| Source            | Type     | Rule              |
|-------------------|----------|-------------------|
| `{M}`             | `{B}`    | thunk(M)          |
| `!V`              | `B`      | force(V) if `V : {B}` |
| `{ \|x\| M }`     | `{A → B}`| thunk(λx.M)       |
| `return V`        | `F A`    | return V          |
| `let x = M; N`    | command  | bind result of M to x, then N |

`!{M}` is therefore the identity on commands: thunk then force.

## 21  Interop and login-shell use

### 21.1  ral-sh dispatcher

`ral` is not POSIX-compatible (§ Rationale). To use `ral` as a login shell
without breaking POSIX-assuming tooling (scp, rsync, git-over-ssh, ansible,
`ssh host cmd`), install the `ral-sh` companion binary as the registered
login shell.

`ral-sh` is a thin dispatcher: it never interprets ral or POSIX syntax.

| Invocation | Dispatch |
|---|---|
| Interactive (stdin and stdout are ttys, no arguments) | `exec ral` |
| All other cases (`-c`, script path, piped stdin, unknown flags) | `exec /bin/sh` |

Registration (both platforms):

```
sudo sh -c 'echo /usr/local/bin/ral-sh >> /etc/shells'
chsh -s /usr/local/bin/ral-sh
```

### 21.2  Login-shell semantics

When `ral` is invoked as a login shell — either via argv[0] starting with
`-` (the Unix convention) or via `ral --login` — it sources the following
files in order before loading the RC:

1. `/etc/ral/profile` (system-wide, optional)
2. `~/.ral_profile` (per-user, optional)

Both files are evaluated as ral source.  They may return a configuration
map (same format as the RC file; see §18) or `unit` (for files that set
environment variables purely for their side effects).

The RC file (`$XDG_CONFIG_HOME/ral/rc` or `~/.ralrc`) is loaded after the
profiles on every interactive session, login or otherwise.

### 21.3  Non-interactive stdin (script-pipe mode)

When `ral` is invoked with no arguments and stdin is not a tty, it reads
stdin to EOF and executes the result as a ral script.  This allows:

```
curl https://example.com/setup.ral | ral
```

### 21.4  `-c` and POSIX compatibility

`ral -c CODE` interprets `CODE` as ral syntax, not POSIX shell syntax.
Any tool that invokes `$SHELL -c POSIX_CMD` will receive POSIX behaviour
only if `$SHELL` refers to `ral-sh` (which forwards `-c` invocations to
`/bin/sh`).

Do not set `$SHELL=ral` on a system where other tools may shell out via
`$SHELL -c`.  Set `$SHELL=ral-sh` instead.

### 21.5  Environment variables seeded at startup

On every startup `ral` ensures the following variables are present in the
environment, falling back to platform defaults if not inherited:

`HOME`, `USER`, `LOGNAME`, `PATH`, `SHELL`, `TERM`, `LANG`, `PWD`,
`OLDPWD`, `SHLVL` (always incremented from the inherited value).
