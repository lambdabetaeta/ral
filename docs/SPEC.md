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

The formal model is call-by-push-value; see §20.8.

## 1  Grammar

```
program       = stmt*
stmt          = binding | bg-pipeline (NL? '?' bg-pipeline)* NL?
bg-pipeline   = pipeline '&'?
pipeline      = stage (NL? '|' NL? stage)*
stage         = return-stage | if-stage | command | atom-stage
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
newline-handling rule below.  A `binding` is a statement, not a stage:
`let` may only appear at the start of a `stmt` (or as the RHS of an
enclosing `let`, since `binding-rhs` itself is a pipeline-and-chain).
It cannot appear after `|` or after `?` — `cmd | let x = …` and
`cmd ? let x = …` are parse errors.  The RHS of `binding` is a
pipeline, hence a command context; head-form dispatch applies (§4).
`VALUE_NAME` is a plain `NAME` whose spelling is a numeric literal,
`true`, `false`, or `unit`. A pipeline terminates at newline, `;`,
`}`, or `)`. Trailing `&` on a pipeline spawns it in the background
and yields a `Handle` (§13.1); every arm of a statement-level
`?`-chain may carry its own `&`, while a `let` RHS may only carry a
final trailing `&`.

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
QUOTED     = '#'^n '\'' .* '\'' '#'^n   (n ≥ 0; close = '\'' followed by ≥ n '#'s)
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

The lexer classifies the shape once; later phases consume that
structure directly rather than rediscover slash or tilde shape from
raw text.  The characters `!`, `~`, and `^` are excluded so that
`!{…}`, `TILDE_WORD`, and `^name` tokenise without lookahead, and a
word that needs any of those literally must be quoted (`'foo!bar'`,
`'!'`).  Backslash has no special meaning outside `"…"`, so
`C:\Users\foo` is a single `NAME`.

`#` starts a comment only at the start of a new token, after
whitespace or a delimiter; mid-word it is an ordinary character, so
`curl http://host:8080/foo#anchor` remains one `SLASH_WORD`.  This
mirrors the POSIX comment rule.

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
        | List Value | Map String Value | Variant Label Value?
        | Block | Handle
```

`String` is UTF-8 text. `Bytes` is a finite byte sequence, possibly
containing NUL; equality is bytewise and `length` counts bytes.
`Bytes` is opaque to language operations — it has no literal form,
and string operations refuse it — but a `Bytes` value renders as
lossy UTF-8 when printed (in `echo`, the REPL, and the `ral --audit`
JSON dump), so byte fields stay readable without an explicit decode.
`Bytes` values arise from `from-bytes` (terminating a byte pipeline),
from encoders (`to-X`, §15), and from I/O builtins whose declared
return type is `Bytes`.  External commands and byte-output builtins
return `String`; to retain their output as `Bytes`, finish the
pipeline with `| from-bytes`.

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

Maps preserve insertion order, and sets are `Map String Unit` by
convention with `has` as a builtin and `union`, `intersection`, and
`difference` as prelude functions.

Variants are tagged sums.  A constructor is written `.label` (nullary)
or `.label payload`, where the payload is the next value atom; the
label is the bare identifier with a leading dot.  A variant value's
type is row-polymorphic: `.ok 5` has type `[.ok: Int | ρ]` for some
free row `ρ`, so the same value flows through code that knows about
`.ok`, `.err`, or any other constructors that may co-exist.  Records
also accept tag keys: `[.dev: 8080, .prod: 443]` is a closed record
whose row labels begin with a dot.  The two row alphabets — bare keys
for ordinary records, tag keys for variants and tag-keyed records —
do not unify; mixing them in one literal is a parse error.

The eliminator is `case`:

```
case <scrutinee> [
  .ok:  { |x| … },
  .err: { |m| … }
]
```

The handler table is a tag-keyed record of thunks.  The result of the
case is the result of forcing the matching handler on the variant's
payload.  Typing requires the handler row to cover every constructor
the scrutinee can produce; missing or extraneous arms are reported as
non-exhaustiveness.  Nullary tags pass `Unit` to their handler.

Computation types are *equi-recursive*.  A self-recursive function
whose return type cycles through `Thunk(F …)` — for example an
infinite stream producer

```
let nats = { |n| step-cons $n { !{nats $[$n + 1]} } }
```

— receives a cyclic computation type μβ. `Int → F (Step Int)`.  No
type annotations are needed; the union-find slot for β closes on
itself and every traversal that descends through the cycle is guarded
by a visited set.  Value types remain non-recursive: the occurs
check on `TyVar` is preserved, so `α = [α]` is still rejected.

The prelude exposes a small Step library — `step-cons`, `step-done`,
`step-take`, `step-map`, `step-fold`, `step-each`, `step-into-list`
— for ergonomic demand-driven streams over a typed protocol.

A Step value flowing into a pipeline consumer is iterated
element-by-element: `producer | { |x| … }` calls the consumer once
per `.more` head, forces the tail, and terminates on `.done`.  The
typechecker propagates the *element* type at this boundary, so the
consumer sees `τ` rather than `Step τ`.  The recogniser is
structural — any variant whose row carries `.more {head, tail:
Thunk(_)}` and `.done` participates, regardless of whether it came
from the prelude `step-*` family or a user-defined recursive
variant of the same shape.  Variants that don't fit this shape
(e.g. `.ok 5`) are passed through to the consumer as ordinary
single values.

`Handle α` is opaque and
parameterised by the return type of the spawned block: only `await`,
`race`, `cancel`, and `disown` apply, and a handle prints as
`<handle:PID>`.  Handles arise from a trailing `&` on a pipeline
(§13.1) and from `par` and `_fork`; their await semantics are
specified in §13.3.

The literals `true`, `false`, `unit`, and numeric NAME tokens (matching
`NUMBER`) are recognised as values before any name lookup. The words
`if`, `elsif`, `else`, `let`, `return`, `true`, `false`, `unit`, and
`case` are reserved: the parser rejects them as binding names in `let`
patterns and lambda parameters.  `case` is also a stage-form keyword
(§2 above) — the parser dispatches on the head identifier rather than
treating `case` as an ordinary callable.

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
`let`s in the same scope forms a **group**, on which the elaborator
builds a dependency graph (with an edge `i → j` whenever `let_j`'s
name appears free in `let_i`'s RHS) and partitions into strongly
connected components.  An acyclic singleton emits as a plain `let`
in topological order, so a forward reference to a later name in the
group is legal as long as it does not close a cycle.  A cyclic SCC
whose members are **all lambdas** emits as `letrec`, supporting self
and mutual recursion and remaining monomorphic within the group
(§20.5).  A cyclic SCC containing any non-lambda falls back to plain
`let` in topological order, since the runtime's `letrec` binding
applies only to lambdas and a non-lambda cycle would observe an
uninitialised slot.

```
let f = { |x| $[$x + 1] }
let g = { |x| $[$y * 2] }    -- forward ref to y, resolved by topo
let y = 10                   -- (non-cyclic: f, g, y in order)

let even = { |n| if $[$n == 0] { return true } else { odd $[$n - 1] } }
let odd  = { |n| if $[$n == 0] { return false } else { even $[$n - 1] } }
-- even, odd form a lambda SCC → letrec
```

A second `let` for a name already bound in the same group **splits**
the group at the shadow point; each half is analysed independently,
preserving source-order semantics across the divide:

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

A bare-head lookup walks local scope then the prelude before falling
through to aliases (interactive only), builtins, and `$env[PATH]` in
turn; `^name` is the same fall-through with the value, alias, and
builtin steps skipped.  An external dispatch reached this way is
further filtered by `exec` whenever a `grant` is in force (§11.1).
The value namespace is consulted only through `$name` and through the
implicit head step; bare non-head words and map-key positions never
trigger either kind of lookup.

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
if $a { echo a } elsif $b { echo b } else { echo c }
```

`if` takes a `Bool` and `?` reacts to failure; the two never cross,
since `if` rejects non-Bool conditions as a type error and a
predicate returning `false` is still a successful command.  When
success itself must be inspected as data, use `try`:

```
let r = _try { grep -q p f }
if $r[ok] { echo found } else { echo missing }
```

For multi-way pattern dispatch on a value, see `case` (§17).

### 4.5  Currying

`{ |x y z| M }` desugars to `{ |x| { |y| { |z| M } } }`, so
under-application returns the inner block, exact application runs
`M`, and over-application is an arity error.  Outside head position
a curried block must be reached explicitly with `$`, and `_` discards
a parameter; the linter warns when an under-applied block is
discarded.  `{ || M }` is a syntax error — write `{}` for the
zero-argument case.

### 4.6  `return`

`return` is parsed before command dispatch and evaluates at most one
value argument, producing `unit` when none is given.  Inside a
parameterised block it exits the enclosing block; at file scope it
exits the file with status 0.  There is no non-local control flow.

### 4.7  Argument spreading

`...$xs` spreads a list into positional arguments.  Because argv is
textual, only scalars survive the boundary: `Int` and `Float` are
formatted decimally, `Bool` as `"true"` or `"false"`, and any
non-scalar value (`Bytes`, `List`, `Map`, `Block`, `Handle`) is an
error.

## 5  Strings and bytes

Quotation comes in two complementary forms.  Single quotes denote a
literal: the body is taken verbatim, with no escape sequences and no
interpolation.  Double quotes denote an interpolating string: `$name`
substitutes a binding, `!{cmd}` substitutes the captured stdout of a
command, and the escapes `\n \t \\ \0 \e \" \$ \! \xNN \u{X..}` produce
their conventional characters.

Within a literal, an embedded `'` is admitted by raising the hash
level: `#'…'#` closes only on `'#`, `##'…'##` on `'##`, and in general
the closing delimiter is a `'` followed by exactly the opening hash
count.  A `'` in the body followed by fewer hashes than the opening
level is itself part of the body.  At top level, a run of `#`s not
followed by `'` is a comment, so the two uses of `#` do not collide.

The numeric escapes inside `"…"` are constrained.  `\xNN` requires
exactly two hex digits and must lie in `\x00..=\x7F`; for non-ASCII
bytes, use `Bytes`.  `\u{X..}` admits 1 to 6 hex digits and must denote
a valid Unicode scalar value.  Any other `\X` is a lex error rather
than a silent literal, on the principle that an unfamiliar escape is
more often a typo than a deliberate choice.  A bare `!` not followed by
`{` or `$` remains literal, with `\!` available as the explicit form.
Where `$name` would otherwise be followed by `[`, the form
`$(name)[…]` delimits the variable from the index that follows.

Both quoted forms may span multiple lines, and the REPL prompts for
continuation while a quote remains open.  `dedent` strips the common
leading indentation from a multiline literal:

```
let msg = dedent '
    SELECT *
    FROM users
    WHERE active = true
'
```

Interpolation coerces only scalar values.  `Int` and `Float` are
formatted decimally, `Bool` becomes `"true"` or `"false"`, and `Unit`
becomes the empty string.  Interpolating any other value — `Bytes`,
`List`, `Map`, `Block`, or `Handle` — is a type error, since these
have no canonical textual rendering.

Outside quotation, `$name` is a separate atom in its own right, and
strings are concatenated by writing the parts inside an interpolating
string, as in `"$dir/file.txt"` or `"$host:$port/api"`.

## 6  Collections

Lists and maps share one bracket form.  `[a, b, c]` is a list and
`[k: v, …]` a map; the empty list is `[]` and the empty map `[:]`.
Entries are separated by commas, a trailing comma is permitted, and
newlines inside the brackets are not significant.

`...` spreads one collection into another.  In a map, explicit entries
take priority over spread entries **regardless of source order**, so
the textual position of the override does not matter:

```
let cfg = [host: 'db', port: 5432]
let r   = [...$cfg, port: 9090]    # r : [port: Int, host: String]
```

The typing follows the scoped-label row discipline of §20.7.

Map keys may be bare words, quoted strings, or derefs.  The form
`[$k: $v]` computes the key at runtime, where it must be a `String`.

## 7  Destructuring

Patterns appear on the LHS of `let` and as parameters of parameterised
blocks (including the blocks that make up `case` clauses, §17).
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

`use p` evaluates `p` and returns a map of its top-level bindings,
excluding any `_`-prefixed names; paths resolve relative to the
containing file, with `RAL_PATH` providing additional search paths.
Results are cached per-process by canonical absolute path (symlinks
resolved where the OS supplies them), so the same file reached via
different paths is one entry.  The cache is not invalidated on file
change — restart the process to pick up edits — and a module's side
effects run only on first load.

`source p` evaluates into the current scope rather than a child,
merging every binding including `_`-prefixed ones, and is never
cached.  Both forms detect and reject circular references.

## 9  Environment

`$env` is a read-only map of environment variables and `$nproc` the
CPU count as an `Int`.  Overrides are scoped through `within [env: …]`
(§3.2); there is no `setenv`.

`~/.ralrc` is a ral script whose last expression is a configuration
map with optional keys `env`, `prompt`, `bindings`, `aliases`,
`edit_mode` (`"emacs"` or `"vi"`), `plugins`, and `theme`.  Of these,
`bindings` populates the interactive value namespace, `aliases`
populates the interactive command namespace, and `plugins` lists
the plugins to load at startup (§18.1).

The `theme` key is a map with two optional fields.  `value_prefix` is
a string prepended to every printed value, defaulting to `"=> "`.
`value_color` is one of `black`, `red`, `green`, `yellow`, `blue`,
`magenta`, `cyan`, `white`, or `none`, defaulting to `yellow`.  Colour
is suppressed whenever stdout is not a tty, `NO_COLOR` is set, or
`RAL_INTERACTIVE_MODE=minimal`.

## 10  Error handling

### 10.0  Failure propagation

Any nonzero exit status or runtime error counts as a failure, and
propagation is always on; the surrounding form decides what happens
next.

- sequential `a; b; c` — the first failure halts the rest;
- `?` chain `a ? b ? c` — the first success wins, and all arms must
  have the same return type;
- pipeline `a | b | c` — any non-SIGPIPE failure fails the whole
  pipeline;
- `try` — catches the failure and runs its handler; if the body
  succeeds, its value is returned;
- `for` and `map` — a failing body stops iteration, while `return`
  exits the current iteration (the body is itself a parameterised
  block);
- `spawn` — failure is captured in the handle and surfaced on `await`;
- top level — an unhandled failure terminates the process with that
  status.

The three forms that interact with cleanup are complementary.  `try`
suppresses the failure entirely; `guard` runs cleanup but lets the
original failure continue propagating; the prelude's `attempt` runs a
thunk and discards both the result and any failure.

### 10.1  `_try`, `_audit`, `try`

`_try B` runs `B` and returns the outcome variant

```
[.ok: α | .err: ErrorRec]
```

where

```
ErrorRec = [status: Int, cmd: String, message: String,
            stdout: Bytes, line: Int, col: Int]
```

On success the variant is `.ok v` where `v` is the body's value.  On
failure the variant is `.err r` where `r` carries the exit `status`,
the `cmd` that failed, a `message` (the runtime error's own message or
the failing external command's stderr decoded as UTF-8), and
`line`/`col` for the failing command's position in source.  `stdout`
holds every byte the body wrote to fd 1 during evaluation — present
on the failure side because that's where it's most useful.  The
record's shape is the input shape `fail` accepts, so
`try { … } { |e| fail $e }` re-raises verbatim.  Only runtime errors
count as failure: a body that returns `false` is still `.ok false`.

To destructure, pair `_try` with `case`:

```
let r = _try { make -j4 }
case $r [
  .ok:  { |_| echo built },
  .err: { |e| echo "failed: $e[message]" }
]
```

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
library wrappers such as `_ed-tui` (§18.1) that use `try` to recover
from TUI failures.

### 10.2  `_guard`

`_guard body cleanup` runs `body`, then `cleanup` regardless of
outcome. Original failures propagate unchanged; cleanup failure is
logged and discarded. `guard` is the prelude wrapper.

### 10.3  Execution tree

Every node has the same shape, with a `kind` discriminator selecting
how the remaining fields are read:

```
Node = [kind: String, cmd: String, args: [String], status: Int,
        script: String, line: Int, col: Int,
        stdout: Bytes, stderr: Bytes, value: α,
        children: [Node], start: Int, end: Int, principal: String]
```

Two kinds are emitted.  A `command` node records the execution of a
single command — external program, builtin, or user function — and
populates `cmd`, `args`, `status`, `stdout`, `stderr`, and `value` in
the obvious way.  A `capability-check` node records a `grant` decision
and additionally carries `resource: String` (`"exec"` or `"fs"`) and
`decision: String` (`"allowed"` or `"denied"`); for an allowed `fs`
check, the matched prefix appears as `granted: String`, and the
resource-specific fields (`name`, `args` for `exec`; `op`, `path` for
`fs`) are spliced into the same map.  `Node` is therefore open: a
consumer reads `kind`, dispatches with `equal`, and accesses the
kind-specific fields through row polymorphism (§20.1).

`script` is the source path, `""` for stdin, and `"<prelude>"` for
prelude internals; prelude wrappers record the user's call site rather
than their own.  `stdout` and `stderr` carry the raw bytes the command
emitted to fd 1 and fd 2; `ral --audit`'s JSON output decodes them as
lossy UTF-8 strings so the tree stays readable.  `value` is the
returned datum (§4.2), and for pure-value builtins `stdout` is empty.  The pair `start`/`end` are
microseconds since the Unix epoch; `principal` records `$USER` at the
moment the node was constructed, on every node, so an extracted
subtree remains self-describing.

Tail-recursive calls are flattened, so a `while` of *N* iterations
produces one node with *N* children rather than a linear chain of
depth *N*.  Construction is lazy: plain execution builds nothing,
`_try` builds only the flat record for its body, `_audit` builds the
subtree for its body, and `ral --audit` builds the tree for the whole
script.  Capability-check nodes appear only when an enclosing `grant`
sets `audit: true`.  Inside `_audit`, each node's `stderr` is capped
at 64 KB; outside `_audit`, `stderr` flows to the terminal as usual.

### 10.4  Debugging

`ral --audit script.ral` runs the script and writes the resulting
execution tree as JSON to stderr; the script's own output reaches
its usual destinations on fd 1 and fd 2 underneath that.  Pass
`--pretty` for an indented form.  There is no step-through debugger.

### 10.5  Error messages

Runtime errors report mismatch, expected type or key set, received
value, source location, and a hint when obvious.

### 10.6  Accessing stderr

`stderr` flows to the terminal during normal execution.  Three
boundaries surface it differently:

- `try` / `_try` puts the failing command's stderr (decoded as UTF-8)
  into `message: String`.  No raw bytes — wrap the body in `&` if you
  need the bytes.
- `audit` / `_audit` records each command's stderr as `Bytes` in its
  tree node, indexed by position; `ral --audit`'s JSON output renders
  them as lossy UTF-8.
- `await` of a `Handle α` (§13.3) returns the spawned block's full
  fd 2 capture as `stderr: Bytes` in the result record.

```
try { make } { |err| echo $err[message] }
let report = audit { make -j4 }
echo $report[children][0][stderr]
let r      = make -j4 &
let r      = await $r
echo $r[stderr]
```

### 10.7  Signals

SIGINT, SIGTERM, and SIGHUP set a flag that the evaluator checks
between statements; once observed, the flag begins an unwinding
during which `guard` cleanups run.  A second signal arriving in the
unwinding window is deferred until cleanup completes, and a third
terminates the process immediately.

## 11  Capabilities (`grant`)

`grant C { B }` attenuates authority for `B` using the dynamic-context
mechanism of §3.1.  Each capability dimension `C` mentions is
deny-by-default within the grant; dimensions `C` *omits* keep ambient
authority — `grant [exec: …] body` tightens exec but leaves fs, net,
editor, shell at whatever the caller had.  Six keys are accepted:
`exec`, `fs`, `net`, `audit`, `editor`, `shell`.

```
grant [
    exec: ['git': [], 'make': [], '/usr/bin/': 'Allow'],
    fs:   [read: ['/home/project'], write: ['/tmp/build']],
    net:  true,
    audit: true,
] { … }
```

### 11.1  `exec`

A unified map keyed by one of three shapes:

- **bare command name** (`git`, `kubectl`) — match by name as the
  user typed it, after PATH lookup.
- **absolute literal path** (`/usr/bin/git`) — match a specific
  resolved binary.
- **absolute subpath** (`/usr/bin/`, trailing `/`) — match any
  binary whose resolved path lies inside the directory.  Path-prefix
  sigils (§11.2.1) may appear at the head of literal-path or subpath
  keys (`xdg:bin/`, `~/.cargo/bin/`, `cwd:/`); they're rewritten to
  absolute paths at policy load.

Each value is the policy.  Bare-name and literal-path keys carry the
full lattice:

- `[]` (or `'Allow'` in TOML) — allow with any arguments;
- `[s₁, …]` (`{Subcommands = […]}` in TOML) — allow only when
  `argv[0] ∈ {sᵢ}`;
- `'Deny'` — sticky veto.

Subpath keys carry only `'Allow'` or `'Deny'` — `Subcommands` is
name-shaped and is rejected on a subpath key at policy load.

**Match precedence within a layer:**

1. **Literal hits win.**  An exact key match (bare name or
   absolute path) wins over any sibling subpath that would also
   admit the same binary.  An explicit literal `Deny` vetoes.
2. **Otherwise the longest matching subpath wins.**  Deeper prefix
   beats shallower, so `'/usr/bin/sensitive/': 'Deny'` carves a
   hole inside `'/usr/bin/': 'Allow'` for binaries under the
   inner directory.
3. **Otherwise the layer denies.**  A layer that opts into `exec`
   admits *only* what its map says; everything else is denied
   within that layer.

### 11.2  `fs`

Governs every operation that touches the filesystem — structured
queries (`glob`, `list-dir`, the `_fs` query ops), redirects (`<`,
`>`, `>>`, `>~`), and bundled coreutils (`cp`, `mv`, `rm`, `mkdir`,
`ln`, …) — through three sub-keys
`read`, `write`, and `deny`, each a list of path prefixes.  A path
is canonicalised after resolution against the active `within
[dir: …]`, with `.` and `..` collapsed and symlinks resolved when
the OS exposes them, so a `within [dir: …]` inside a `grant`
cannot escape its enclosing policy: only the resolved path
matters.  An empty map `fs: [:]` denies filesystem access
entirely; `/dev/null` is exempt from both checks as a discard
device.

A `read` or `write` succeeds when, at every layer with an `fs`
opinion, the path falls inside some entry of the corresponding
prefix list and outside every entry of `deny`.  Both prefixes
and denies are path *regions*, not exact paths: a deny on
`/etc/secrets` covers `/etc/secrets/foo`, and a read prefix on
`~/.local` (resolved at load) covers everything beneath it.
Membership is alias-aware so the macOS firmlink `/tmp` ↔
`/private/tmp` does not produce two different answers depending
on which form the policy author chose.

Deny is symmetric: the same deny region blocks reads and writes.
This is the simpler rule, and it has the right effect for the
common case — a directory the agent should not see is also one
it should not modify.  Deny is anti-monotonic in the lattice:
more layers can only add denies (composition unions them), so a
nested grant can never uncover a region the outer policy denied.
Prefixes compose by intersection, so a nested grant can only
narrow what is reachable.

`fs` does not restrict an external program's own I/O — those
need their binary, linker, and system libraries — so use `exec`
together with `within [handlers:]` to shape that surface.  Where
OS sandboxing is available, write policy and non-system read
paths are also enforced for externals as defence in depth.

#### 11.2.1  Path-prefix sigils

Two sigils are recognised at the head of a path string in any
`fs.read`, `fs.write`, `fs.deny`, or path-shaped `exec` key (literal
path or subpath), and resolved once at policy load:

- `~`, `~/sub`, `~user`, `~user/sub` — the usual shell tilde rule.
- `xdg:NAME` and `xdg:NAME/sub` — an XDG basedir, where `NAME` is
  one of `config`, `data`, `cache`, `state`, `bin`.  The first four
  are the [XDG basedir spec][xdg-basedir]; `bin` is non-spec but
  conventional.  Each maps to its `XDG_*_HOME` env var when set,
  otherwise to the Linux default — `~/.config`, `~/.local/share`,
  `~/.cache`, `~/.local/state`, `~/.local/bin` — universally, so
  cross-platform tools that respect XDG behave the same on macOS
  and Linux.

[xdg-basedir]: https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html

Any other entry passes through unchanged.

Resolution is one-shot at policy load: tokens are rewritten into
concrete absolute paths in the policy itself, so later mutation of
HOME or `XDG_*_HOME` cannot widen what was already authorised.  An
`xdg:NAME[/sub]` token whose resolved base sits outside HOME is
rejected at load — for example, with `XDG_DATA_HOME=/etc` set in
the calling environment, a policy naming `xdg:data` errors instead
of granting `/etc` read.  Unknown names (`xdg:cofnig`) error at the
same boundary, in the spirit of `deny_unknown_fields`.

### 11.3  `net`

Boolean.  ral has no in-process network primitives, so `net` governs
only the network access of external programs spawned inside the
grant.  Enforcement is OS-level (§11.8) and all-or-nothing: there is
no endpoint-level policy.

### 11.4  `audit`

`audit: true` requests inclusion of capability-check events in any
execution tree that is already being collected; it does not itself
build a tree, and once enabled it stays enabled across nested
grants.  When such a tree is active, each `exec` or `fs` check emits
a `capability-check` node (§10.3) just before the gated action — or
alone if the action is denied — while `net` checks emit no nodes.

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
`cd` is an ordinary core builtin and obeys the gate uniformly across
interactive, script, and agent contexts; bare `cd` with no argument
means `cd ~`.

### 11.7  Attenuation

Nested grants can only reduce authority.  Per dimension:

- `exec` — the literal half (bare names, absolute paths) intersects
  across layers: a literal key allowed at every opining layer
  survives, with its policies meet-folded (Subcommands lists
  intersect; `Deny` is sticky from any layer); subpath keys
  intersect by path containment (deeper survives on overlap);
  literal `Deny` and subpath `Deny` propagate even when only one
  layer names them.
- `fs` — narrow by path containment (and for externals under OS
  sandboxing).
- `net` — boolean AND.
- `audit` — logical OR.
- `editor` — per-boolean AND (inner can only disable).

A dimension that *no* layer in the stack opined on stays at ambient
authority — there is no implicit deny from omission across the stack,
only within a layer that opted into the dimension.

Authority may be restricted but never amplified. `grant` affects
ral-dispatched actions; if a permitted external program internally
spawns another, that inner spawn is constrained by the OS sandbox
when available, not by ral head lookup.

### 11.8  Platform support

In-process `exec`/`fs`/`net` checks apply on every platform.  OS-level
enforcement varies:

- **macOS** — Seatbelt (`sandbox_init_with_parameters`); ral
  re-executes itself inside Seatbelt when `fs:` is present, when
  `net` is `false`, or when `exec:` is present.  Under exec
  attenuation the Seatbelt profile renders the meet-folded admit
  set as a path allow-list, so the OS layer also gates spawns
  that the in-process check can't see — including binaries
  re-execed by interpreters like `sh -c "…"`, `xargs CMD`, or
  `find -exec`.  When `fs:` is absent the OS layer passes fs
  through (the user's working tree, HOME, etc. stay reachable).
- **Linux** — bubblewrap with seccomp BPF (x86-64, AArch64).
  Re-executes when `fs:` is present or `net` is `false`; pure
  exec attenuation does not enter the sandbox subprocess because
  bwrap has no path-based exec filter.  In-process exec checks
  still apply.
- **Windows** — no filesystem or network enforcement; only in-process
  checks apply, and `net: false` emits a one-time stderr warning that
  the restriction will not be applied.  Each external command inside
  a `grant` is assigned to a Job Object capping its process tree at
  512.

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
`B` concurrently; the child cannot affect the parent, and the only
channel of communication is `await`.  Immutability makes the copy
safe without synchronisation, so the implementation may use
`fork(2)` or threads — both satisfy the isolation contract.  The
surface syntax is a trailing `&` on a pipeline, which yields a
`Handle α` immediately (where α is the pipeline's value-output
type) without waiting:

```
let h = long-task arg &           # spawns, binds Handle α to h
grep pat file & ? echo fallback   # either arm of a ?-chain may be &
```

`par` and `_fork` produce the same kind of `Handle` programmatically.

### 13.2  `par` vs `map`

`par` and `spawn` carry I/O-bound work, while `map` is for
in-process transformation:

```
par { |f| convert $f } !{glob '*.wav'} $nproc
let results = map { |line| upper $line } $lines
```

Within the concurrency family, `par` is the one exception: it is
`map` parallelised, returning a list of *values* with the await
envelope stripped rather than a list of records.  Each worker's
stdout and stderr are buffered per task and discarded once the value
is extracted; for byte-level access, build the parallelism out of
`spawn` + `await` directly.

### 13.3  `await` and `race`

`await h` blocks until `h` completes and returns a record:

```
{ value:  α        # the block's return value
, stdout: Bytes    # everything the block wrote to fd 1
, stderr: Bytes    # everything the block wrote to fd 2
, status: Int      # 0 on success
}
```

`Handle` is parameterised, with `spawn { B } : Handle α` for α equal
to `B`'s return type and `await : Handle α → { value: α, … }` tying
the two together statically, so a wrong-type consumer fails at
compile time.

`race [h₁,…]` returns the same record for the first completion and
marks the rest cancelled — awaiting a cancelled handle fails, and
must be caught with `try` if recovery is wanted.  Cancellation is
handle-level only, so on some platforms the losing computations may
continue in the background.

Each handle owns independent stdout and stderr buffers; during
execution the spawned block writes into them and nothing reaches the
caller's terminal or capture context.  `await` does not auto-replay
those bytes, so they sit in `value.stdout` and `value.stderr` until
the user reads them — `echo $r[stdout]` suffices, since `Bytes`
prints as lossy UTF-8 (§2).  Buffers drain on the first `await` and
the record is cached, so a second `await` returns the same fields.
Each buffer is capped at 16 MiB; past the cap, a one-line truncation
marker is appended and further bytes are dropped, so high-volume
spawns should use an explicit redirect.  A redirect on the
backgrounded pipeline (`cmd > log &`, `cmd 2> err &`) sends bytes to
the target instead and leaves the corresponding record buffer empty.

If the block raised, `await` re-raises rather than producing a
record. Wrap with `try` to recover:

```
let r = _try { await $h }
if $r[ok] { use $r[value] } else { recover $r[status] }
```

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
String — a literal, an interpolation, or a deref — and the body is
the usual `{ ... }` block:

```
let h = watch "build" { cargo build }
watch "deploy" { step-1; step-2 }
let target = "prod"
watch "build-$target" { make }     # interpolation
_await $h
```

In contrast to `&`, the streamed handle's buffers stay empty, so the
awaited record's `stdout` and `stderr` fields contain nothing useful;
the bytes have already gone to the caller's stdout, with stderr
prefixed `[LABEL:err] ` rather than buffered separately.  Each
prefixed line is emitted atomically through a shared framing sink, so
sibling watchers interleave at line granularity but never tear, and
under the interactive REPL the lines route through rustyline's
external printer so they appear above the prompt rather than
corrupting the editor.

The usual pipe-buffering caveats apply: a child that block-buffers
stdout will arrive in chunks unless coaxed (`stdbuf -oL`, language
line-buffer flag), a slow consumer can backpressure the child once
the kernel pipe fills, and a cancellation flushes any partial line
in the framing sink at teardown.

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
it as a `String`, failing on EOF; an empty line is the empty string
`""` and remains distinct from end-of-file.

**Codecs.** A codec is a pair `from-X` (decoder) and `to-X` (encoder)
covering one direction each.  A decoder reads bytes from the pipeline
and returns a structured value; an encoder takes a value, emits the
corresponding bytes on the pipe, and also returns them as `Bytes`.

| Decoder        | In      | Out                              |
|----------------|---------|----------------------------------|
| `from-line`    | `Bytes` | `String` (trailing `\n` dropped) |
| `from-string`  | `Bytes` | `String`                         |
| `from-lines`   | `Bytes` | `Step String`                    |
| `from-json`    | `Bytes` | JSON value                       |
| `from-bytes`   | `Bytes` | `Bytes`                          |

All text decoders fail on invalid UTF-8; `from-json` additionally
fails on invalid JSON; `from-bytes` cannot fail.  `from-lines` is
stream-shaped (Step); materialise with `step-into-list` (or prelude
`from-lines-list`) when a list is required.

| Encoder      | In                | Out     |
|--------------|-------------------|---------|
| `to-string`  | `String`          | `Bytes` |
| `to-lines`   | `[String]`        | `Bytes` |
| `to-json`    | JSON-serialisable | `Bytes` |
| `to-bytes`   | `Bytes`           | `Bytes` |

`to-bytes` accepts only `Bytes` — encode a string with `to-string`
first — and encoders are first-class, so partial application works
(`map to-json $values`).  There is no explicit-argument decoder; to
decode a value already in hand, route it through the matching
encoder and pipe into the decoder, as in `to-bytes $b | from-string`
or `to-string $s | from-json`.

`split` and `match` take explicit arguments rather than reading from
the pipeline, and `glob` returns the matching paths as a sorted
list, empty when nothing matches.

**Redirects.** The redirect operators are `>`, `>~`, `>>`, `2>`,
`2>&1`, and `<`.  They are stage modifiers rather than values, and
they apply only to the pipeline they decorate; persistent directory
changes are scoped through `within [dir: …]` (§3.2) and `cd` exists
only in the interactive layer, while the current directory is read by
the `cwd` builtin as a `String`.

The default write redirect `>` is atomic on regular files: the
destination appears in one step, and a concurrent reader observes
either the old contents or the new but never a partial write.  When
the target is non-regular — a TTY, `/dev/null`, a named pipe, or a
socket — atomic replacement is not available, and `>` falls back to a
streaming truncate-and-write.  The variant `>~` is the streaming form
unconditionally, with POSIX `>` semantics: bytes land as they arrive
and a concurrent reader may observe a half-written file.  Use `>~`
when streaming visibility is part of the contract (logs, FIFOs that
must not be replaced) or when `>` would refuse the target.  `>>`
appends, and `<` reads.

**File I/O.** File reads and writes are redirect-and-codec: a decoder
on `< $path` for reads, an encoder on `> $path` for writes.

```
let body = from-string < $p          # read string
let s    = from-lines  < $p          # read Step String
let xs   = from-lines-list $p        # read list of lines
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
decision is recorded in a `TerminalState` value and exposed to user
code as the binding `$TERMINAL`, indexed in the usual way (§6).
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

A terminal that cannot render ANSI, or one the user has told us to
treat as dumb, must not see escape sequences at all — not in the
prompt, not in syntax highlighting, and not as a cursor-position
query that will never be answered.  The language itself is unchanged
across modes: scripts run identically under any of them.

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
| `which` | Resolve lookup target; `String` or failure |
| `cwd` | Current directory as `String` |
| `grep-files` | Regex over files; returns `[[file: String, line: Int, text: String]]` (1-based lines, ordered by input file then match order) |

### 16.2  Predicates

All return `Bool`, and a `false` return is itself successful:
`exists`, `is-file`, `is-dir`, `is-link`, `is-readable`,
`is-writable`, `is-empty` (over List, Map, Bytes, or String),
`equal` (structural), and `lt` / `gt` (lexicographic on `String`).

### 16.3  Private substrate

`_each`, `_map`, `_filter`, `_fold`, `_sort-list`,
`_sort-list-by`, `_fold-lines`; `_fork`, `_await`, `_race`, `_cancel`,
`_disown`, `_par`; `_try`, `_try-apply`, `_audit`, `_guard`;
`_decode`, `_encode` (used via the `from-X`/`to-X` wrappers, §15);
families `_str` (`upper`, `lower`, `replace`, `replace-all`, `find-match`,
`find-matches`, `join`, `slice`, `split`, `match`, `shell-quote`, `shell-split`, `dedent`), `_path` (`stem`, `ext`, `dir`, `base`,
`resolve`, `join`), `_fs` (`lines`, `size`, `mtime`, `empty`, `list`,
`tempdir`, `tempfile` — structured queries only; bytes I/O goes
through codec + redirect, effects through bundled coreutils),
`_convert` (`int`, `float`, `string`), `_editor` (`get`, `set`,
`push`, `accept`, `tui`, `history`, `parse`, `ghost`, `highlight`,
`state`; §18.1), `_plugin` (`load`, `unload`; §18.1).

`_try-apply f val` applies `f` to `val` and returns the outcome
variant `[.ok: r | .err: Unit]` — `.ok r` on success, `.err unit`
when `val` fails to destructure against `f`'s parameter pattern.  It
catches the distinguished `PatternMismatch` error kind raised by
destructuring (a block parameter, an explicit `let pat = …`, or a
list/map shape mismatch), and only at the parameter-bind step: any
other failure in `f`'s body, including a `PatternMismatch` from an
inner `let`, propagates as usual, so `_try-apply` never silently
swallows a bug.  The error tag is `unit` because pattern-mismatch
failure carries no structured record — caller distinguishes by the
tag alone.

### 16.4  Bundled coreutils

The `coreutils` Cargo feature folds a curated set of GNU-compatible
utilities — `ls`, `cat`, `wc`, `head`, `tail`, `cp`, `mv`, `rm`,
`mkdir`, `ln`, `sort`, `tr`, `uniq`, and around seventy total — into
the binary as in-process builtins.  Bare `ral` keeps the feature
optional (developers usually have system coreutils); `exarch` enables
it unconditionally so a sealed profile is reproducible without
depending on the host's `cp` or `mv`.

Filesystem effects (`cp`, `mv`, `rm`, `mkdir`, `ln`, `chmod`, …) are
the canonical way to perform mutations: there are no `copy-file` /
`make-dir` / `remove-file` primitives.  Effects don't return
structured values, so wrapping them buys nothing.

Every bundled invocation goes through a capability-checked dispatch
wrapper.  For each path-taking tool, the wrapper consults the tool's
own clap parser to identify path arguments and their roles
(read / write / both), then calls `check_fs_read` or `check_fs_write`
on each before delegating to `uumain`.  Bypassing the sandbox by
reaching for `cp` instead of a primitive is therefore not possible —
both paths land at the same chokepoint.

`within [dir: ...]` propagates by chdir under the same lock that
serialises uutils stdio redirection, so relative path arguments
resolve against ral's scoped CWD, not the host process CWD.

The `diffutils` feature bundles `cmp` and `diff` through the same
helper-subprocess path as coreutils — `resolve_command` rewrites a bare
`cmp`/`diff` to `--ral-uutils-helper`, the parent never runs them
in-process — and
the `grep` feature enables the regex-backed builtins `grep-files`,
`match`, `split`, `replace`, `replace-all`, `find-match`, and
`find-matches` using ripgrep's engine.  Without `grep`, those
builtins are present but raise at runtime; for byte-stream `grep`,
fall back to the system one on `PATH`.

## 17  Prelude

The prelude is itself written in ral.  Its names are ordinary
bindings in scope before user code runs, elaborated by the SCC rule
of §3, and the linter warns when user code shadows one.  As with any
other binding, a prelude name is implicit in head position and
explicit elsewhere through `$` (§4), and currying (§4.5) supports
partial application of any parameterised one.

The canonical source is `core/src/prelude.ral`.  A normative listing
will appear in this section once the surface is stable; until then,
treat the source file as authoritative.

## 18  Interactive layer

Line editing, history, completion, and prompt rendering are
host-language features.

| Builtin | Purpose |
|---|---|
| `cd` | Change working directory (persistent) |
| `jobs`, `fg`, `bg` | Job control |
| `quit` | Exit (≡ Ctrl-D) |
| Ctrl-Z | Suspend foreground |

**SIGINT.** With a foreground command running, SIGINT is delivered to
its process group.  At the prompt, it discards the current line and
redraws.  In a non-interactive script, it begins the unwinding
process described in §10.7.

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

**`_str 'shell-quote' <s>`** and **`_str 'shell-split' <s>`** form a
POSIX-style round-trip pair.  `shell-quote` returns one argument in a
form a compatible shell parser will re-read as a single argument;
`shell-split` tokenises a shell-quoted string back into a list of
arguments, honouring `'`, `"`, and `\` and erroring on an
unterminated quote.  The exact quoting form is implementation-defined
but stable enough to round-trip ordinary text arguments.

**`grep-files <pattern> <files>`** searches each file in order and
returns a list of maps `[file: String, line: Int, text: String]`,
one per match, with 1-based `line` and the trailing newline stripped
from `text`.  Unlike `grep`, this is a structured-value builtin
rather than a byte-stream command.

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

`_editor 'tui'` installs a capture buffer around the body using the
same mechanism `let` applies at a byte-mode boundary (§4.3).
External commands inside the body write their stdout into that
buffer while stderr still reaches the TTY, so a curses-style UI such
as fzf renders normally.  On return, a non-`Unit` value from the
body wins outright; otherwise the captured bytes are UTF-8-decoded
(stripping one trailing newline) and returned as a `Str`.

The prelude exposes a small layer of `_ed-*` helpers over `_editor`,
intended for plugin authors and inert in non-interactive contexts (any
`_editor` call from a script raises).  They factor common operations
on the buffer — reading text or the cursor, replacing the
left-of-cursor segment, inserting at the cursor — and wrap
`_editor 'tui'` so the caller receives a `[output, status]` record
even when the body fails:

```
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
```

**Hooks.** Five events fire at well-defined moments, with handlers
called as thunks against the real `Env` (not a snapshot) and wrapped
in `grant`.  `buffer-change` (`{Str → Str → Int → F Unit}`) fires on
each keystroke that changes the buffer; `pre-exec` (`{Str → F Unit}`)
runs before command evaluation and `post-exec` (`{Str → Int → F
Unit}`) runs after, receiving the exit status; `chpwd` (`{Str → Str
→ F Unit}`) fires after `cd` with the old and new paths; and
`prompt` (`{Str → F Str}`) runs just before the prompt is drawn,
receiving the base prompt and returning the rendered one.

`buffer-change` hooks fire inside the line editor with the runtime
lock released, so handlers must communicate through plugin context
rather than shared state to avoid reentrancy; `_editor 'tui'` is
rejected from inside one.

**Keybinding dispatch.** Handlers are tried in reverse load order:
returning `true` consumes the key, `false` passes it to the next
handler or to built-in editing.  An error is treated as a consume
and is logged.

The two write sub-commands `accept` and `push` interact with the
prompt's lifecycle.  `accept` marks the current buffer for immediate
execution once the handler returns, in place of re-entering the line
editor; `push` saves the buffer and clears it, restoring it on the
next prompt as a stack so that nested pushes compose.  Combining
them gives the familiar push-then-accept pattern useful for things
like fzf-driven directory hops.

**`~/.ralrc` integration.** The RC map gains an optional `plugins`
key, a list of `[plugin: Str, options?: Map]` entries loaded at
startup, equivalent to calling `_plugin 'load' name options` for
each.  `options`, when present, is passed as the single argument to
the plugin's top-level block:

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

Unknown top-level keys in an entry are warned and ignored, so the
schema can grow with `enabled:` or `when:` flags without breaking
older parsers.

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
I,O ::= ∅ | Bytes | μ
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

For externals and byte-output builtins the bytes flow on the output
channel while the return type is `String`, with decoding deferred to
the `let` boundary rather than performed inside the pipe.  Streaming
reducers process line by line without buffering and so return `Unit`.
Encoders and decoders are the sole dual-channel commands: an encoder
emits the bytes on the pipe and also returns them as `Bytes`, and a
decoder consumes bytes from the pipe to produce a structured value
(`from-bytes` is the special case `A = Bytes` with output mode `∅`).

### 20.4  Pipelines

A stage has type `F[I,O] A`; connection requires `O_left = I_right`:

```
ls | grep foo | wc -l
F[∅,Bytes] String   F[Bytes,Bytes] String   F[Bytes,Bytes] String

ls | from-lines | { |line| … }
F[∅,Bytes] String   F[Bytes,∅] Step String   F[∅,∅] […]
```

Mismatches between adjacent stages are caught at type-check time,
and the non-final return is not threaded across byte edges, since
composition follows the output mode rather than the value (§4.2).

### 20.5  Polymorphism

Types are inferred without annotations, and generalisation occurs at
the `let` boundary:

```
let id = { |x| return $x }         -- id : {α → F α}
id 42                              -- F Int
id 'hello'                         -- F String
```

The discipline follows the SCC elaboration of §3: a non-recursive
SCC is generalised at the binding point, while a mutually recursive
SCC is monomorphic within its group and generalised only after the
fixed point is reached.

### 20.6  Type errors

A type error aborts the program with exit status 1, and the message
carries the source position together with the expected and inferred
types:

```
script.ral:12:5: type error: type mismatch: Int vs String
```

### 20.7  Row polymorphism and record types

Row typing follows Leijen (2005) with scoped labels: duplicate
labels are permitted in a row, selection returns the first
occurrence, and extension prepends to shadow earlier entries without
needing a restriction operator.  A map literal with a single spread
threads the spread source's field types into the result type, while
multiple spreads yield an open but imprecise result.

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

A record is usable where `[String:A]` is expected exactly when all
of its fields share the type `A`; a heterogeneous record cannot be
treated as a homogeneous map.

Record-returning builtins:

| Builtin | Return record |
|---|---|
| `_try`   | `[.ok: α \| .err: [status:Int, cmd:String, message:String, stdout:Bytes, line:Int, col:Int]]` |
| `_audit` | `Node` (§10.3) |

Dynamic keys fall back to the homogeneous-map rule, and list
indexing such as `$xs[0]` is unaffected by all of this.

### 20.8  CBPV correspondence

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

`ral-sh` is a thin dispatcher and never interprets either ral or
POSIX syntax: with no arguments and a tty on both stdin and stdout
it `exec`s `ral`, and in every other case (`-c`, a script path,
piped stdin, unknown flags) it `exec`s `/bin/sh`.  Registration is
the usual `chsh -s /usr/local/bin/ral-sh` after adding the path to
`/etc/shells`.

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
