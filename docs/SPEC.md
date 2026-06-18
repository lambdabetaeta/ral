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
stmt          = binding | chain NL?
binding       = 'let' pattern '=' pipeline (NL? '?' pipeline)* '&'?
chain         = bg-pipeline (NL? '?' bg-pipeline)*
bg-pipeline   = pipeline '&'?
pipeline      = stage (NL? '|' NL? stage)*
stage         = return-stage | if-stage | case-stage | scope-stage | command
return-stage  = 'return' atom?
if-stage      = 'if' atom atom ('elsif' atom atom)* ('else' atom)?
case-stage    = 'case' atom atom
scope-stage   = ( 'within' atom atom
                | 'grant'  atom atom
                | 'try'    atom atom
                | 'guard'  atom atom
                | 'audit'  atom         ) redir*
command       = head (arg | redir)*
head          = '^' NAME | atom
arg           = atom | '...' atom
atom          = primary index*
primary       = word | tag | block | collection
tag           = TAG atom?
index         = '[' word ']'
block         = '{' stmt* '}' | '{' '|' pattern+ '|' stmt* '}'
pattern       = '_' | IDENT | plist | pmap
plist         = '[' (pattern (',' pattern)* (',' '...' IDENT)?)? ']'
pmap          = '[' pentry (',' pentry)* ','? ']'
pentry        = pkey ':' pattern ('=' atom)?
pkey          = IDENT | QUOTED | TAG
collection    = list | map
list          = '[' ']' | '[' elem (',' elem)* ','? ']'
map           = '[' ':' ']'
              | '[' spread-entry* key-entry (',' entry)* ','? ']'
elem          = atom | spread-entry
entry         = key-entry | spread-entry
key-entry     = mapkey ':' atom
spread-entry  = '...' atom
mapkey        = IDENT | QUOTED | deref | TAG
word          = WORD | QUOTED | INTERP | deref | force | expr-block
deref         = '$' IDENT | '$(' IDENT ')'
force         = '!' primary
expr-block    = '$[' expr ']'
redir         = NUMBER? '>'  word
              | NUMBER? '>~' word
              | NUMBER? '<'  word
              | NUMBER? '>>' word
              | NUMBER  '>&' NUMBER
expr          = orexpr
orexpr        = andexpr (LOGOR  andexpr)*
andexpr       = cmpexpr (LOGAND cmpexpr)*
cmpexpr       = addexpr (('==' | '!=' | '<' | '>' | '<=' | '>=') addexpr)?
addexpr       = mulexpr (('+' | '-') mulexpr)*
mulexpr       = unary  (('*' | '/' | '%') unary)*
unary         = deref | force | NUMBER | 'true' | 'false'
              | 'not' unary | '-' unary | '(' expr ')'
```

The grammar is written without `NL` tokens for clarity; see the
newline-handling rule below.  A `binding` is a statement, not a stage:
`let` may only appear at the start of a `stmt` (or as the RHS of an
enclosing `let`).  It cannot appear after `|` or after `?` —
`cmd | let x = …` and `cmd ? let x = …` are parse errors.

The RHS of `binding` is a pipeline, hence a command context; head-form
dispatch applies (§4).  Note the deliberate asymmetry between a
top-level `chain` and a `binding`: every arm of a statement-level
`?`-chain may carry its own `&` (`bg-pipeline`), while a `let` RHS
admits at most one final trailing `&` that backgrounds the whole RHS.
This avoids the ambiguity where `let x = a ? b &` would otherwise be
read as backgrounding only `b`.

`head = atom` is a syntactic catch-all: §4.1 specifies how a parsed
atom is interpreted as a Bare head, a Path head, an explicit value
head, or a `^name` caret head, and which forms require at least one
argument.  §4.1 also restates the rule that a `NAME` whose spelling
is a numeric literal or one of `true`, `false`, `unit` suppresses
implicit head dispatch.

`scope-stage` is the five **control operators** `within`, `grant`,
`try`, `guard`, `audit`.  They are reserved in `let`-binding position
and in bare-head command position; each takes a fixed arity of
callable atoms (blocks, lambdas, or variable references) and
optionally trailing redirects.  `^within`/`^grant`/`^try`/`^guard`/
`^audit` bypass the reservation and look up an external command on
`PATH`; `$within`/`$grant`/`$try`/`$guard`/`$audit` in value position
are compile-time errors.  See §10.1 (`try`/`audit`), §10.2 (`guard`),
§3.2 (`within`), and §11 (`grant`) for typing rules and runtime
semantics.

A pipeline terminates at newline, `;`, `}`, or `)`.  Trailing `&` on a
pipeline spawns it in the background and yields a `Handle` (§13.1).

**Token fusion in `$[…]`.**  The lexer emits `&` and `|` as single
tokens (they are pipeline punctuation outside expression blocks).
Inside an `expr-block` the lexer itself fuses adjacent `&&` and `||`
pairs into the logical operators shown above as `LOGAND` and `LOGOR` —
the context (we are inside `$[…]`) is lexical, so the rewrite belongs
to the lexer.  No other context fuses these tokens; outside `$[…]`
the standalone `&` and `|` retain their pipeline meaning.

**Newline handling.**  `\n` and `\r\n` line endings are equivalent. A
newline terminates a statement unless it appears:

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
TAG        = '`' IDENT
NAME       = [^ \t\n|{}[\]$<>"'();!~^/`]+
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

`IDENT` names variables, map keys, and dereference targets. `TAG`
introduces variant labels and tag-keyed record rows. `WORD` is a
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
Dots are ordinary bare-word characters: `.env`, `.gitignore`, and
`foo.bar` are all plain words.

`#` starts a comment only at the start of a new token, after
whitespace or a delimiter; mid-word it is an ordinary character, so
`curl http://host:8080/foo#anchor` remains one `SLASH_WORD`.  This
mirrors the POSIX comment rule.

`,` is punctuation only while the lexer is currently inside `[...]`.
Outside bracket depth, commas are ordinary `NAME` characters: `a,b,c`
and `--features x,y` are single words.

`:` becomes its own token only when followed by EOF, whitespace, or `]`.
Thus `host: val` tokenises as `NAME ':' NAME`, while `localhost:5432`
remains a single `NAME`.

Postfix `[` requires adjacency: `$r[k]` indexes, `$r [k: v]` is two
atoms. The lexer is single-pass and keeps delimiter depth; the parser
never alters its behaviour. Because `force = '!' primary`, the postfix index in
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

Map keys iterate in sorted order; map equality is structural and
order-independent.  Sets are `Map String Unit` by convention with
`has` as a builtin and `union`, `intersection`, and `difference` as
prelude functions.

Variants are tagged sums.  A constructor is written `` `label `` (nullary)
or `` `label payload ``, where the payload is the next value atom; the
label is the bare identifier with a leading backtick.  A variant value's
type is row-polymorphic: `` `ok 5 `` has type [`ok: Int | ρ] for some
free row `ρ`, so the same value flows through code that knows about
`ok, `err, or any other constructors that may co-exist.  Records
also accept tag keys: [`dev: 8080, `prod: 443] is a closed record
whose row labels begin with a backtick.  The two row alphabets — bare keys
for ordinary records, tag keys for variants and tag-keyed records —
do not unify; mixing them in one literal is a parse error.

The eliminator is `case`:

```
case <scrutinee> [
  `ok:  { |x| … },
  `err: { |m| … }
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
let nats = { |n| stream-cons $n { !{nats $[$n + 1]} } }
```

— receives a cyclic computation type μβ. `Int → F (Stream Int)`.  No
type annotations are needed; the union-find slot for β closes on
itself and every traversal that descends through the cycle is guarded
by a visited set.  Value types are equi-recursive on the same
discipline, so a streaming consumer that pattern-matches its input
back into itself (`drain $p[tail]`) typechecks too.

The prelude exposes a small Stream library — `stream-cons`,
`stream-nil`, `stream-take`, `stream-drop`, `stream-map`,
`stream-fold`, `stream-each`, `stream-to-list` — for ergonomic
demand-driven streams over a typed protocol.

A Stream value flowing into a pipeline consumer is ordinary data:
`producer | f` is `f !{producer}` at every value edge, so the
consumer receives the `Stream τ` value whole.  Per-element
consumption is explicit, through the eliminators — `producer |
stream-each { |x| … }` runs the body once per `more head, forces
the tail, and terminates on `done; `stream-map`, `stream-fold`,
and `stream-to-list` are the transforming, reducing, and
materialising forms of the same elimination.

`Handle α` is opaque and
parameterised by the return type of the spawned block: only `await`,
`poll`, `race`, and `cancel` apply, and a handle prints as
`<handle:PID>`.  Handles arise from a trailing `&` on a pipeline
(§13.1) and from `par` and `spawn`; their await semantics are
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
(§11.5). The set is fixed. `audit` is an observability wrapper, not
an execution context.

A scoped frame holds for the **whole dynamic extent** of its body,
including every tail-recursive landing inside it.  All five control
operators (`try`, `audit`, `grant`, `within`, `guard`) share this
property uniformly: their body is invoked through the tail-call
trampoline, which absorbs `TailCall` locally, so a tail-recursive
function inside the body cannot unwind past the frame.  Capability narrowing, cwd / env overlays, effect
handlers, and cleanup thunks all remain in force across each
iteration.

### 3.2  `within`

`within` is a control operator (§1) and a unified scoping primitive
for directory, environment, and effect handlers.  Its first operand
is a map; its second is a block.  `^within` keeps PATH-lookup
semantics; `$within` in value position is a compile-time error.

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
  **external commands only**. Lexical names and builtins are
  language-internal and run normally; any external calls they make
  will still hit the handler.

Per-name handlers (`handlers:`) may handle only names not claimed by
the lexical/prelude/builtin binding namespace at the installation
site. Installing a handler for a builtin such as `length` is an error.

Handlers are **deep with self-masking**. *Deep*: the installation
persists across the dynamic extent of `within`, so successive calls in
the body each trigger the handler. *Self-masking*: during the
evaluation of a handler's own body, the matched frame is lifted off
the dynamic stack, so a same-name call inside the handler reaches the
next outer frame (or the OS), not the handler itself. ral has no
`resume`; self-masking is the operational rule that keeps the
wrap-and-forward idiom (`within [handlers: [git: { |args| my-git ...$args }]]`)
free of infinite recursion. A handler receives the command name and
arguments and may return a value, fail, or delegate to the next
enclosing frame.

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
   If the name is a builtin binding, the builtin runs. Otherwise,
   command lookup consults installed handler frames (alias or active
   `within`) and finally `$env[PATH]`.
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
evaluates an expression (arithmetic, comparison, or logical). It is
binding-only: aliases, handlers, catch-alls, and external commands are
not values. `!`
forces a block, literal (`!{M}`) or stored (`!$b`); in command
context `!{M}` is the same as `M`, in value context it yields the block's return
value. `^name` skips lexical/prelude/builtin binding lookup but
remains contained by active user handlers; if no handler matches, it
resolves `name` as an external command. The operand must be a plain
slash-free word (`NAME`). The `^name` form is valid only in head
position, where `^` must be the first token of the command.

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

The grammar gives `head = '^' NAME | atom`.  At dispatch time the
parsed atom is classified into one of four interpretive forms, and
the form chooses the lookup path:

- **Bare head** — a single `WORD` of `NAME` shape (e.g. `git`,
  `--features`).  Triggers the full lookup chain described below.
- **Path head** — a single `WORD` of `SLASH_WORD` or `TILDE_WORD`
  shape (e.g. `./build`, `/usr/bin/env`, `~/bin/script`).  Executes
  the exact path; no value-namespace step.
- **Explicit value head** — any non-word primary (`QUOTED`, `INTERP`,
  `deref`, `force`, `expr-block`, `block`, `collection`, `tag`)
  optionally followed by indices, or a `WORD` head bearing at least
  one trailing index (`$r[k]`, `cmds[deploy]`).  Stays in the value
  namespace; never falls through to alias / builtin / PATH.
- **Caret head** — `^name`.  Same fall-through as a bare head with
  the binding step skipped.  User handler entries (aliases,
  `within` handlers) still fire, so a `within [handlers: [cat: …]]`
  block intercepts `^cat` just as it does `cat`.

An additional rule suppresses implicit dispatch even for bare-shaped
words: a `NAME` whose spelling is a numeric literal, or one of `true`,
`false`, `unit`, never resolves as a head.  A standalone such atom is
the value, not a command (it falls into the explicit value-head path
when used in head position).

A bare-head lookup walks local scope then the prelude; if the name
is unbound there, dispatch consults builtin bindings before user
handler frames (aliases and active `within` frames, innermost-first),
then `$env[PATH]`.  `^name` skips the binding namespace and resolves
to a user handler if one is active; otherwise it resolves to an
external command.  An external dispatch reached this way is
further filtered by `exec` whenever a `grant` is in force (§11.1).
The value namespace is consulted only through `$name` and through the
implicit head step; bare non-head words and map-key positions never
trigger either kind of lookup.

A `command` whose `head` is an explicit value head must carry at least
one argument or redirection — a bare `$f` standing alone is a value,
not a call.  Bare and path heads with zero arguments are still
commands (`hostname`, `./build`).

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
concurrently. Value edges are sequential and intra-evaluator only in
pure value pipelines; inside a process-staged byte pipeline, ral
helper subprocesses pass values over helper value channels.

#### 4.2.1  Value pipelines and byte pipelines

A pipeline takes one of two execution shapes, fixed by its stages:

- **Value pipeline** — every stage operates on values and every edge is
  a value edge.  The whole pipeline collapses to a sequential
  data-last fold inside the parent evaluator: `x | f` is `f !{x}`.  No
  process is spawned, no byte pipe exists, and ordinary shell-state
  mutation (cwd, env, aliases, modules, registry) is visible to the
  enclosing scope as for any other expression.

- **Byte pipeline** — any pipeline with at least one external stage or
  one byte edge.  It is a Unix-style process pipeline: every stage
  executes in its own subprocess, all stages share one process group,
  and the parent evaluator never appears in that group.  Stages
  implemented in ral run as helper subprocesses on the same footing
  as external commands.  On interactive launches the parent hands the
  controlling terminal to the pipeline group as a unit before any
  stage runs user code; on non-interactive launches there is no
  terminal handoff.  How the implementation realises that ordering
  guarantee — exec trampolines, gate frames, anchor processes — is a
  platform implementation detail; SPEC only
  commits to the observable result.

A `<` redirect on a *compound* command (a function call, block, or
group) whose body is a pipeline feeds the pipeline boundary stdin —
that is, the first byte-consuming stage's stdin.  The redirect on the
outer call binds before stage routing, so `f < $path` in `let f = {
cat | head }` makes `$path` the input to `cat`.  An explicit
stage-level stdin redirect (`< other`) on a stage overrides this
boundary value with a stage-local one.

A captured pipeline (`!{ … }`, or any pipeline whose stdout is bound
into a value) does not claim foreground: bytes flow into the capture
buffer rather than the terminal, so the parent retains terminal
ownership.

**Platform.** Byte pipelines have full job-control semantics on Unix
(Linux, macOS).  On Windows there is no controlling-terminal concept
in the POSIX sense; pipelines run without a foreground handoff (the
console is shared between attached processes), but the helper /
value-edge protocol works the same as on Unix, so external-only,
mixed, and pure-value pipelines all work.  Stopped-state job control
is unreachable on Windows: there is no SIGTSTP analogue, so `fg`
blocks on whole-job completion, `bg` is a no-op (no job is ever in
the `Stopped` state), and `disown` strips the kill-on-job-close limit
on the group's Job Object before forgetting the children.  Ctrl-C
escalates: first delivery fans `CTRL_BREAK_EVENT` to every active
member, a second `TerminateJobObject`s every live group, a third
forces ral to exit.

A ral stage in a byte pipeline is therefore a subshell with respect to
mutation: changes it makes to its `cwd`, environment, aliases,
modules, or registry stay local to the helper.  Only the pipeline's
pipe contents and final value flow back; nothing else.  The same rule
applies when an alias or handler that runs an external command appears
mid-pipeline: it executes inside its own helper subprocess.

```
let r = 1 | { |x| return $[$x + 1] }       # value pipeline → r is 2
```

Values cannot silently cross a byte edge into an external command.
Encode them first (`to-json`, `to-lines`, `to-bytes`) and decode on the
other side (`from-json`, `from-lines`, `from-string`); the typechecker
rejects the implicit conversion.

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

#### 4.3.1  Top-level vs block evaluation boundary

Two evaluation modes are observable to user code: a **top-level turn**
and a **block**.  They differ in what they hand back to their caller.

A top-level turn — a script file, a `-c` argument, a REPL line, an
exarch tool call — runs a computation and **persists** every
program-state effect it produced.  `let` bindings, `cd`, `env-set`,
alias registrations, module loads, and the recorded last status all
remain visible to the next top-level turn.  The same applies on
failure and on `exit`: bindings entered before a failing command stay
bound for the next turn, and a `try` handler running at top level
observes the partial state.  This is why a REPL session is
incremental.

A block — the body of `{ … }` evaluated by `!`, of `grant`, `within`,
`try`, `guard`, `audit`, or the bare `!{ … }` form — runs a
computation and returns only an **outcome** (a value or a failure)
plus a fixed set of **observations** (captured bytes, audit fragment,
last-status update).  Any `let`, `cd`, environment overlay, alias
registration, module load, or registry mutation performed inside the
block is discarded at the closing brace.  An inner `cd` does not
escape the block; a `within [dir: …] { cd elsewhere }` neither
disturbs the parent cwd nor the `within` overlay.

`spawn`'s argument is a block in this sense: its bindings and cwd
mutations are private to the spawned thread.

The split is observable, not implementation:

```
let x = 1
{ let x = 2; let y = 3 }    # block; runs for its effects
echo $x                       # 1 — outer x unchanged
echo $y                       # error: y unbound
```

```
# at the REPL, one turn per line
▸ let x = 1
▸ let x = 2
▸ echo $x        # 2 — top-level turns persist
```

`core::evaluator` reaches these two modes through `eval_top_level`
(top-level turn) and `apply` on a `Value::Thunk` (block, via the
trampoline's value dispatch).  Both entries carry the boundary's
transport semantics — mobile persistence and the local-vs-confined
transport selection in a single call.  The distinction is part of
the language, not the implementation: every frontend exposes these
semantics to user code (a REPL line *is* a top-level turn; a `grant`
body *is* a block), and any conforming implementation must respect
them.  Today the implementation is staged: exarch's tool-call
evaluator and the tests route through `evaluator::eval_top_level`;
the ral REPL line evaluator and the `ral` script / `-c` driver
implement the same top-level semantics through their local
evaluator, because their resident-side concerns (REPL
`pending_chpwd`, signal-handler ownership, prompt hooks, spawned
threads) need a separate migration before the top-level confined
transport is safe to use without losing those observations.

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
success itself must drive a branch, use `try`:

```
try { grep -q p f; echo found } { |_| echo missing }
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
blocks (including the handler thunks that make up a `case`'s arms, §2).
Pattern forms:

- `_` matches anything, binds nothing;
- `IDENT` matches anything, binds the name;
- list `[p₁, p₂, …, ...rest]` matches a list of sufficient length;
- map `[k₁: p₁, k₂: p₂ = default]` matches a map containing those
  keys (defaults fill missing keys).

Patterns are purely structural: there are no literal patterns. A
mismatch is a runtime error, catchable by `try`.

```
let [first, ...rest] = $args
let [host: h, port: p = 8080] = $opts
let [name: n, addr: [city: c]] = $p
```

## 8  Modules

`use p` evaluates `p` and returns a map of its top-level bindings,
excluding any `_`-prefixed names; paths resolve relative to the
containing file, with `RAL_PATH` providing additional search paths.
Each `use` re-reads and re-evaluates the file, so its side effects run
on every load and an edit to the file is picked up on the next call.

`source p` evaluates into the current scope rather than a child,
merging every binding including `_`-prefixed ones.  Both forms detect
and reject circular references and bound recursion depth.

## 9  Environment

`$env` is a read-only map of environment variables and `$nproc` the
CPU count as an `Int`.  Overrides are scoped through `within [env: …]`
(§3.2); there is no `setenv`.

`~/.ralrc` is a ral script whose last expression is a configuration
map with optional keys `env`, `prompt`, `bindings`, `aliases`,
`edit_mode` (`"emacs"` or `"vi"`), `plugins`, and `theme`.  Both
`bindings` and `aliases` route by value shape: thunk entries install
as alias handler frames (command-callable, persist past `within`
blocks), every other value lands in the interactive value namespace.
`plugins` lists the plugins to load at startup (§18.1).  Aliases can
also be installed or removed at runtime via the `alias NAME { |args|
BODY }` and `unalias NAME` builtins. Alias names may not claim a
lexical/prelude/builtin binding name.

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
  pipeline.  A job-control stop in an external stage is reported as a
  stopped-command failure, not as a synthetic SIGKILL exit;
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

### 10.1  `try`, `audit`

`try` and `audit` are **control operators** with dedicated grammar
arms (§1).  `try B H` and `audit B` parse as `scope-stage`, not as
generic applications: `B` and `H` are callable atoms (blocks,
lambdas, or variable references holding either) and arity is enforced
at parse time.  Trailing redirects attach to the operator as a
whole — `try { … } { … } > out` writes the body's stdout to `out`.
`^try` and `^audit` keep PATH-lookup semantics; `$try` and `$audit`
in value position are compile-time errors.

`try B H` runs `B`.  On success it returns `B`'s value.  On failure it
calls `H` with an error record and returns `H`'s value:

```
ErrorRec = [status: Int, cmd: String, message: String,
            line: Int, col: Int]
```

`status` is the reduced exit status, `cmd` is the command that failed,
and `line`/`col` are the failing command's position in source.
`message` is synthetic: for runtime errors it is the text passed to
`fail` (or produced by the runtime), and for failing external commands
it names the actual process outcome, for example `"<cmd>: exited with
status <N>"`, `"<cmd>: killed by signal 9 (SIGKILL)"`, or a
job-control stop message.  It is *not* the failing command's fd 2
bytes — those streamed live to the terminal during execution.  For
per-command stderr/stdout as data, wrap in `audit` and inspect the
returned tree (§10.3).  The record's shape is the input shape `fail`
accepts, so `try { … } { |e| fail $e }` re-raises verbatim.  Only
runtime errors count as failure: a body that returns
`false` is still a success and `H` is not called.

`try` catches **recoverable runtime errors** and nothing else.
`exit N` (which terminates the process), a job-control stop
(SIGTSTP / SIGSTOP — see §10.7 and the REPL job table), and the
tail-call trampoline all bypass `H` and continue propagating to their
respective boundaries.  In particular, a `for`/`map` body that
issues a tail-recursive call from inside `try`'s body still
trampolines normally: the trampoline lives inside `try`'s body
invocation, so `TailCall` never escapes the wrapper.

```
try { make -j4 } { |err| echo $err[cmd] $err[status] }
let v = try { curl $primary } { |err| curl $fallback | from-json }
```

`try` does not redirect fd 1 or fd 2: bytes follow §4.3 normally, so
side-effects inside the body remain observable as they happen.  When
forensic per-command bytes are wanted, wrap in `audit` (§10.3):
`audit { try { … } { … } }` records each command's `stdout`/`stderr`
on the execution tree.  `err.message` carries only the synthetic
status text; for the failing command's actual fd 2 bytes, an audit
scope is required.

`audit B` runs `B` and returns its full execution tree (§10.3)
regardless of outcome.  External commands' fd 1 and fd 2 stream live
to the surrounding terminal AND are recorded on each per-command
node's `stdout`/`stderr`: byte capture under `audit` is non-suppressing.
`grant […, audit: true]` does not build a tree; it requests that
capability-check events be included in whatever tree is already
being built.

When a `try` catches an error, debug builds echo a one-line summary
to stderr (`ral: try caught error (line:col): message`); release
builds stay silent.  This surfaces errors that would otherwise be
swallowed silently by
probes like `try { return $env[X] } { |_| return '' }` and by
builtins such as `_ed-tui` (§18.1) that catch body failures
internally and return them as a `[output, status]` record.

### 10.2  `guard`

`guard B C` is a control operator (§1).  It runs `B`, then runs `C`
regardless of outcome.  Original failures from `B` propagate
unchanged; a failure in `C` is logged and discarded.  Both operands
are callable atoms; trailing redirects attach to the form as a whole.
`^guard` keeps PATH-lookup semantics; `$guard` in value position is a
compile-time error.

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

**Scope nodes carry empty `args`.**  Nodes emitted by the five control
operators (`within`, `grant`, `try`, `guard`, `audit`) record their
structural operands — handler frames, capability maps, body/handler
thunks, cleanup thunks — in the IR-node fields themselves, not in
`args`.  The `args: [String]` array on a scope node is therefore the
empty list; the structural record is the audit information.  This is
the opposite convention from a `command` node, whose `args` is the
evaluated argv of the wrapped command.

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
`try` builds only the flat record for its body, `audit` builds the
subtree for its body, and `ral --audit` builds the tree for the whole
script.  Capability-check nodes appear only when an enclosing `grant`
sets `audit: true`.  Inside `audit`, each node's `stderr` is capped
at 64 KB; outside `audit`, `stderr` flows to the terminal as usual.

The tree is *lexical*: every control operator (`grant`, `within`,
`guard`, `try`, `audit`) owns the audit nodes its body produces, and
they appear as direct children of the scope node — not as siblings of
it at the surrounding level.  Process boundaries (each external or
bundled command confined under an `fs`/`net`-restricting `grant`,
each pipeline stage helper) only *transport* audit fragments back to
the parent; the wrapping scope decides where they land.  Pipeline
stage fragments are merged in stage order.  `grant` owns its
confined children's nodes the same way `within` owns its body's.
`try` and `audit` are the only scopes that always build a record
regardless of an outer audit; the others build a node only when an
outer audit scope is collecting.

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

- `try` puts the failing command's stderr (decoded as UTF-8)
  into `message: String`.  No raw bytes — wrap the body in `&` if you
  need the bytes.
- `audit` records each command's stderr as `Bytes` in its tree node,
  indexed by position; `ral --audit`'s JSON output renders them as
  lossy UTF-8.
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

`grant` is a control operator (§1).  `grant C { B }` attenuates
authority for `B` using the dynamic-context mechanism of §3.1.
`^grant` keeps PATH-lookup semantics; `$grant` in value position is
a compile-time error.  Each capability dimension `C` mentions is
deny-by-default within the grant; dimensions `C` *omits* keep ambient
authority — `grant [exec: …] body` tightens exec but leaves fs, net,
editor, shell at whatever the caller had.  Six keys are accepted:
`exec`, `fs`, `net`, `audit`, `editor`, `shell`.

```
grant [
    exec: ['git': [], 'make': [], '/usr/bin/': 'allow'],
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

- `'allow'` (or `[]`, equivalent — empty subcommand list) — allow with
  any arguments;
- `[s₁, …]` — allow only when `argv[0] ∈ {sᵢ}`;
- `'deny'` — sticky veto.

Subpath keys carry only `'allow'` or `'deny'` — subcommand lists are
name-shaped and rejected on a subpath key at policy load.

Strings are lowercase on the ral surface.  The capitalised
`Allow`/`Deny`/`Subcommands` forms are reserved for the internal IPC
wire format between cooperating ral processes (`--sandbox-projection`
JSON) and never appear in user-written profiles or grant blocks.

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
queries (`glob`, `list-dir`, `file-info`, the `is-*` predicates), redirects (`<`,
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

An absolute entry passes through unchanged.  A non-sigil *relative*
entry is rejected at load: it would otherwise anchor to the live
working directory at check time, so the same grant would authorise a
different region after a `cd`.  To name a location relative to the
working directory in force when the grant is frozen, write `cwd:sub`
(§11.9).  A bare command name in `exec` (no `/`, no sigil) is a name,
not a path, and is exempt.

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
grant.  There is therefore **no in-process gate** for `net` — unlike
`exec`, `fs`, `editor`, and `shell`, which ral checks before the gated
action, a `net: false` is enforced *solely* by the OS sandbox (§11.8),
all-or-nothing, with no endpoint-level policy.  On a platform without
an OS sandbox backend a `net`-restricting grant therefore fails closed
(§11.8) rather than running unconfined.

### 11.4  `audit`

`audit: true` requests inclusion of capability-check events in any
execution tree that is already being collected; it does not itself
build a tree, and once enabled it stays enabled across nested
grants.  When such a tree is active, each `exec` or `fs` check emits
a `capability-check` node (§10.3) just before the gated action — or
alone if the action is denied — while `net` checks emit no nodes.

### 11.5  `editor`

Gates access to the line editor API (the `_ed-*` family, §18.1).
Three booleans:

- `read` — `_ed-get`, `_ed-text`, `_ed-cursor`, `_ed-keymap`, `_ed-lbuffer`, `_ed-history`, `_ed-parse`;
- `write` — `_ed-set`, `_ed-set-lbuffer`, `_ed-insert`, `_ed-push`, `_ed-accept`, `_ed-ghost`, `_ed-highlight`, `_ed-state`;
- `tui` — `_ed-tui`.

```
grant [editor: [read: true, write: true, tui: false]] {
    _ed-get          # allowed
    _ed-tui { … }    # denied
}
```

Omitting `editor` entirely denies all sub-commands.  Plugin handlers
do not push a capability frame at hook time — they run with host
authority (§18.1) — so the `editor` capability is whatever the
enclosing scope grants; wrap a plugin call in `grant { editor: … }
{ … }` to restrict it explicitly.

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

The dimensions are **independent**.  Each is a separate field whose
absence is the meet identity (`None` = inherit = ⊤), so restricting one
dimension leaves the others untouched: `grant [fs: [read: ['/tmp']]] {
… }` narrows filesystem reads but does not touch `net`, `exec`,
`editor`, or `shell`, which stay at the caller's authority.  A grant
that means to confine network access must say `net: false` itself;
tightening `fs` alone does not imply a network restriction.  The
attenuation table above composes each dimension's field separately,
never one from another.

Authority may be restricted but never amplified. `grant` affects
ral-dispatched actions; if a permitted external program internally
spawns another, that inner spawn is constrained by the OS sandbox
when available, not by ral head lookup.

The attenuated frame holds across every tail-recursive landing in
the body.  A tail-recursive function called from inside
`grant [caps] { … }` sees the narrowed capabilities on each
iteration, not just the first: the tail-call trampoline is invoked
*inside* the body's frame, so `TailCall` is absorbed before it could
unwind past the capability layer.  The same holds for the dynamic
frames installed by `within` (cwd, env, handlers) and the cleanup
thunk registered by `guard` — see §3.1.

### 11.8  Platform support

In-process `exec` and `fs` checks apply on every platform; `net` has
no in-process gate (§11.3) and is enforced by the OS sandbox alone.
OS-level enforcement varies:

- **macOS** — Seatbelt (`sandbox_init_with_parameters`); each
  external or bundled command spawned inside the `grant` is launched
  inside Seatbelt when `fs:` is present, when `net` is `false`, or
  when `exec:` is present.  Under exec
  attenuation the Seatbelt profile renders the meet-folded admit
  set as a path allow-list, so the OS layer also gates spawns
  that the in-process check can't see — including binaries
  re-execed by interpreters like `sh -c "…"`, `xargs CMD`, or
  `find -exec`.  When `fs:` is absent the OS layer passes fs
  through (the user's working tree, HOME, etc. stay reachable).
- **Linux** — bubblewrap with seccomp BPF (x86-64, AArch64).
  A spawned command is wrapped in bwrap when `fs:` is present or
  `net` is `false`; pure exec attenuation does not enter the
  OS sandbox because bwrap has no path-based exec filter.
  In-process exec checks still apply.
- **Windows / non-Unix** — in-process `exec` / `fs` checks still
  apply (and `net` has no in-process gate), but OS-level fs/net
  confinement is unavailable (no bubblewrap, no Seatbelt).  Consequently, when an evaluation
  boundary observes an active `SandboxProjection` (any non-empty `fs`
  policy, or `net: false`) and the OS backend reports itself
  unavailable, the boundary **fails closed**: it returns
  `Break::Error` whose message reads
  `sandbox confinement unavailable: <reason>`, and the body never
  runs.  A silent local fallback would weaken the user's explicit
  restriction; the implementation refuses to run instead.  Each
  external command inside a `grant` is still assigned to a Job
  Object capping its process tree at 512 — that limit operates
  through the process-spawn machinery and does not depend on the
  fs/net sandbox backend.

### 11.9  Capability profiles (`.ral` files)

A capability profile is an ordinary ral script whose terminal expression
is a map shaped exactly like the argument of `grant [...] { body }`.
The same six keys (`exec`, `fs`, `net`, `audit`, `editor`, `shell`)
with the same lowercase string conventions.  Loading runs the file
through the standard parse + elaborate + evaluate pipeline; the
returned map walks into a `Capabilities` via the same parser the
inline `grant` operator uses, resolving every `~` / `xdg:` / `cwd:` /
`tempdir:` sigil against the load-time home and working directory.

```
# my-profile.ral
return [
    fs:   [read: ['cwd:'], write: ['/tmp']],
    exec: ['/usr/bin/': 'allow', 'bash': 'deny'],
    net:  false,
]
```

**Loading at the CLI.** `ral --capabilities a.ral[,b.ral,...]` loads
each profile, left-to-right `meet`s the raw policies, freezes once
against `FreezeCtx { home, cwd }`, and pushes the result as a
permanent session frame above `Capabilities::root()`.  Repeated
`--capabilities` invocations append.  `meet` is commutative, so order
doesn't change correctness — but each successive file narrows
authority, never widens, and `audit: true` in any file makes the
session audit (logical OR per §11.7).

**Loading at runtime.** `source 'my-profile.ral'` returns the
terminal-expression map, suitable for direct use:

```
grant (source 'my-profile.ral') { … }
```

The `source` evaluation runs under the *current* authority; a profile
file that touches anything the active grant denies will fail at the
gate.  Profiles intended for both startup (`--capabilities`) and
mid-session use are easiest to keep effect-free.

## 12  Testing

Mock commands with `within [handlers:]`, inspect with `audit`, assert with
user-defined helpers:

```
let assert_eq = { |name expected actual|
    if !{equal $expected $actual} {} else {
        echo "FAIL: $name\n  expected: $expected\n  actual: $actual" 1>&2
        fail [status: 1, message: "assertion failed"]
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

A concurrent block is a thunk evaluated on a worker thread.
`spawn B` and `watch "L" B` are the two primitives: each schedules
`B` — a block / thunk — to run concurrently and returns a
`Handle α` immediately.  The body executes against the worker's own
`Shell`, built from the captured environment the spawning thunk
carries; mobile mutations made by the body die with the worker
thread, so "blocks discard their mobile" is satisfied by lifecycle
rather than by an explicit discard.  Observations — the return
value, exit status, audit nodes, buffered stdout/stderr — cross
back through the handle's `await` record.  Immutability of values
makes the shared captured environment safe without synchronisation.

The surface syntax is a trailing `&` on a pipeline, which yields a
`Handle α` immediately (where α is the pipeline's value-output
type) without waiting:

```
let h = long-job arg &            # spawns, binds Handle α to h
grep pat file & ? echo fallback   # either arm of a ?-chain may be &
```

`par`, `spawn`, and `watch` all produce the same kind of `Handle`.
`par` is *not* a primitive: it is prelude code over `spawn` and
`await`.

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
stdout and stderr are buffered per handle and discarded once the
value is extracted; for byte-level access, build the parallelism
out of `spawn` + `await` directly.

### 13.3  `await`, `poll`, and `race`

`await h` blocks until `h` completes and returns a record:

```
{ value:  α        # the block's return value
, stdout: Bytes    # everything the block wrote to fd 1
, stderr: Bytes    # everything the block wrote to fd 2
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
try { let r = await $h; use $r[value] } { |e| recover $e[status] }
```

`poll h` is the non-blocking dual of `await`, and it is *total* over a
finished block: it never waits and never re-raises.  It returns one of
two arms — `` `settled `` once the block has finished, `` `pending ``
while it runs:

```
let p = poll $h
case $p [
    `settled: { |s|
        case $s[outcome] [
            `ok:  { |v| use $v },                # returned a value
            `err: { |e| recover $e[status] }     # raised — the caught-error record
        ]
    },
    `pending: { |_| keep-working }               # still running
]
```

- `` `settled `` carries `{ stdout, stderr, outcome }`.  `stdout` and
  `stderr` are the bytes the block wrote; `outcome` is the closed variant
  `` <ok: α | err: ErrorRec> ``, where `` `ok `` holds the block's return
  value and `` `err `` holds the same `ErrorRec` (§10) `try` hands its
  handler — its `status` is the block's exit code.  The bytes drain once on
  completion, so `await` and a later `poll` observe the same buffers.
- `` `pending `` (no payload) while the block is still running.

Where `await` *blocks* and *re-raises* a failed block, `poll` neither
waits nor raises: it reports the terminal outcome — value or error — as
data, never as a failure of `poll` itself.  It still fails on a cancelled
or forgotten handle, exactly as `await` does — a detached handle has no
outcome to sample.  The prelude predicate `is-done h` reduces `poll` to a
`Bool`: `true` once `` `settled ``, `false` while `` `pending ``.

### 13.4  Child lifetime

A spawned worker is *detached*: it hangs under the session's durable
cancel root, not under the turn's foreground scope, so it outlives the
turn that launched it and a foreground cancel — a turn deadline or an
interrupt — does not reach it. It is stopped only by `cancel h`, by a
root abort, by host-process exit, or — under a host that arms one — by a
lifetime ceiling that reaps an abandoned worker after a fixed bound.
There is no detach operator: dropping a handle binding is
fire-and-forget, and the worker keeps running until one of those stops it.

### 13.5  Live watching: `watch`

`watch "LABEL" B` is a builtin — not a keyword — that
spawns block `B` as a watched handle whose stdout and stderr flow
line-framed to the caller's stdout in real time, rather than being
buffered until `await`. Each emitted line is prefixed `[LABEL] `
for stdout and `[LABEL:err] ` for stderr. `watch` returns a
`Handle`, so awaiting, racing, and cancelling apply as for `&`.

Because a watched worker is detached yet keeps writing as it runs, its
output sink must outlive the turn. `watch` is therefore a builtin the
**host installs**, not one the language ships everywhere: a host whose
stdout is a durable sink (the `ral` shell, interactive or batch, writing
to a terminal or pipe) installs it; a host whose active streams are
per-call capture buffers — an embedding that collects each turn's output —
does not, so there `watch` is simply not a builtin and naming it resolves
as any other unknown command would.

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

### 13.6  Concurrent blocks and the sandbox boundary

`spawn`/`watch` do **not** re-dispatch the body into a confined
evaluation.  The worker thread runs the body in-process; the OS
sandbox, if any, wraps the worker by virtue of wrapping the parent
process (process-level OS confinement is inherited by every thread
in that process), and the in-ral capability stack still gates the
body through the usual in-process checks.  No confined re-exec is
attempted from a worker thread, and none is needed.

Nested forced blocks *inside* a concurrent block — anything that
calls `force`/`!{…}`, drives a branch, or evaluates a `grant`/`within`
body — evaluate locally in the same process; a `grant`/`within` body
is never re-dispatched into a confined child.  The grant frame is
folded into the live capability stack, so a `spawn` inside an active
`grant` cannot escape the grant's restrictions through nested code:
every nested ral-owned effect is checked in-process against the
inherited frame, and every nested external or bundled command is
launched under the inherited `SandboxProjection`.

Handles are resident, process-local references to a worker thread, so
a locally-evaluated `grant`/`within` body may return one freely.  What
a handle cannot do is cross a child-eval IPC boundary: a value
serialised to a child-eval helper (such as a pipeline-stage subshell)
that is a handle errors with `cannot return a handle from sandboxed
evaluation`, since the worker it names does not exist in that child.
`await` the handle before its value leaves the process.

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
| `from-lines`   | `Bytes` | `Stream String`                  |
| `from-json`    | `Bytes` | JSON value                       |
| `from-bytes`   | `Bytes` | `Bytes`                          |

All text decoders fail on invalid UTF-8; `from-json` additionally
fails on invalid JSON; `from-bytes` cannot fail.  `from-lines` is
stream-shaped; materialise with `stream-to-list` (or prelude
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

`re-split` and `re-match` take explicit arguments rather than reading
from the pipeline, and `glob` returns the matching paths as a sorted
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

Most builtins are registered with their clean names directly and are
the canonical user-facing commands.  A few carry a leading `_` to mark
them as implementation primitives consumed by the prelude (§16.3).
Return-type rules follow §4.2.

Hosts may also register static host builtins: Rust atoms owned by the
embedding process rather than by `ral-core`.  A host entry carries its
names, call function, computation hint, arity, documentation, and
optional type scheme together.  Registration rejects collisions with
core names or earlier host names, while re-registering the same static
table is a no-op.  The `ral` REPL uses this for `_ed-*`; `exarch` uses
it for agent search atoms such as `grep-files`, `line-hash`, and
`explore-dir`.  Exarch's `edit` helper is sourced ral code over those
atoms and ordinary redirects.  Those exarch names are not core ral
features.

### 16.1  User-facing

| Builtin | Purpose |
|---|---|
| `fail` | Raise a failure with an error record `fail [status: N, message?: M, ...]`; `fail $e` re-raises a caught error verbatim (`fail [status: 0]` is an error) |
| `echo` | Write UTF-8 bytes to stdout; return as `String` |
| `source`, `use` | §8 |
| `glob` | Sorted path glob |
| `length` | Length of list, map, string, or bytes |
| `keys` | Map keys in sorted order |
| `has` | Test map membership |
| `ask` | `/dev/tty` prompt; fails on EOF |
| `which` | Resolve lookup target; `String` or failure |
| `cwd` | Current directory as `String` |

### 16.2  Predicates

All return `Bool`, and a `false` return is itself successful:
`exists`, `is-file`, `is-dir`, `is-link`, `is-readable`,
`is-writable`, `is-empty` (over List, Map, Bytes, or String),
`equal` (structural), and `lt` / `gt` (lexicographic on `String`).

### 16.3  Underscore-prefixed builtins

A few builtins carry a leading `_` to mark them as implementation
internals — the prelude wraps them, and user scripts should not call
them directly:

| Builtin | Wrapped by | Purpose |
|---|---|---|
| `_type` | — | Compile-time type-annotation passthrough |
| `_ed-*` | — | REPL line-editor interface (§18.1) |
| `_plugin` | Prelude `load-plugin` / `unload-plugin` | Plugin lifecycle (§18.1) |
| `_ansi-ok` | — | Probe for ANSI terminal support |

Everything else (collections, strings, codecs, filesystem queries,
control flow, concurrency, predicates, shell state) is registered
with its clean name directly — `map`, `filter`, `fold`, `each`,
`sort-list`, `sort-list-by`, `range`, `upper`, `lower`, `re-split`,
`re-match`, `re-replace`, `string-replace`, `glob`, `exists`,
`int`, `float`, `str`, `spawn`,
`await`, `poll`, `fail`, `par`, and so on.  The five scope operators
(`within`, `grant`, `try`, `guard`, `audit`) are *not* builtins; they
are control-operator keywords with dedicated grammar arms (§1) and
dedicated typing rules.  The regex-backed builtins are namespaced with the `re-` prefix
because their first argument is a regex, not a literal substring;
`string-replace` is the literal-string counterpart of `re-replace`,
requiring an exactly-once match.  The prelude may still provide convenience
wrappers (e.g. `for` calls `each`, `lines` calls `re-split '\n'`), but
the builtins themselves are the canonical user-facing names.

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

A bundled command is an *executable image*, not a path-rewriting
wrapper (§ "Bundled tools are executable images").  A clean-terminal
call may run `uumain` in-process under a tiny uucore-global lock; every
other invocation — redirects, capture/audit, an env/cwd mismatch, a
byte pipeline stage, or any active sandbox projection — spawns ral
itself as `ral --ral-bundled-tool <tool> <args…>`.  That child gets its
cwd, env, and stdio from the ordinary command-spawn plumbing
(`apply_env` installs the scoped CWD and env; the child's `Command`
owns fd 0/1/2), so the parent never temporarily rewires its own process
state to make a library call look like an exec.  Under an
`fs`-restricting `grant`, upstream uutils opens paths internally rather
than through ral's `check_fs_op`, so the bundled child is *child-owned*:
the parent first checks exec authority, then launches it under the same
effective OS sandbox an external command would receive.  Bypassing the
in-process gate by reaching for `cp` instead of a primitive is therefore
not possible — the confinement is the kernel sandbox around the child.

The `diffutils` feature bundles `cmp` and `diff` as bundled-tool exec
images on the same footing as coreutils, and
the `grep` feature enables the regex-backed builtins `re-match`,
`re-split`, `re-replace`, `re-replace-all`, `re-find-match`, and
`re-find-matches` using ripgrep's engine.  Without `grep`, those
builtins are present but raise at runtime; for byte-stream `grep`,
fall back to the system one on `PATH`.  The `ripgrep` feature bundles
an external-style `rg` command on the same exec-image path: a bundled
`rg` runs in-process on the clean-terminal fast path, otherwise as a
`ral --ral-bundled-tool rg` child carrying a vendored ripgrep core.

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
| `jobs`, `fg`, `bg`, `disown` | Job control |
| `quit` | Exit (≡ Ctrl-D) |
| Ctrl-Z | Suspend foreground |

**SIGINT.** With a foreground command running, SIGINT is delivered to
its process group.  At the prompt, it discards the current line and
redraws.  In a non-interactive script, it begins the unwinding
process described in §10.7.

### 18.x  Job control (Ctrl-Z, `fg`, `bg`, `jobs`, `disown`)

Pressing Ctrl-Z while a foreground external command is running parks
its process group as a stopped job.  The terminal is returned to ral,
the prompt re-appears, and a notification of the form
`[N] stopped\t<cmd> (SIGTSTP)` is printed where `N` is the job number.

`jobs` lists every parked or backgrounded job: id, state
(`running`/`stopped`), pgid, and the original command line.  `fg [N]`
resumes job `N` (or the most recent if omitted) in the foreground:
SIGCONT is sent to the whole pgid, the controlling terminal is handed
back, and the wait drains exits and re-stops via `waitpid(-pgid,
WUNTRACED)`.  `bg [N]` resumes a stopped job in the background.
`disown [N]` removes the job from the table without signalling it; on
shell exit, surviving jobs are sent SIGTERM, given five seconds, then
SIGKILLed.

What ral deliberately does *not* do, contrary to bash:

  - There is no `huponexit`-style guardrail at exit, no "There are
    stopped jobs" prompt-time refusal, and no async `[N] Done <cmd>`
    lines spliced into readline.  Job state is observed by typing
    `jobs`, not by being interrupted at the prompt.
  - There is no `%1` / `%+` / `%-` syntax: jobs are addressed by
    decimal id, defaulting to the most recent.

Both standalone foreground externals (vim, less, man, top, …) and
multi-stage process-staged pipelines park as one job.  When any
stage of a pipeline stops, ral SIGSTOPs the rest of the pgid so the
whole group is parked together, abandons the per-stage handles
(they survive in stopped state), and tells the pipeline anchor to
exit cleanly via SIGCONT.  `fg` SIGCONTs the surviving pgid and
re-waits the pipeline to completion.

Pure-value pipelines (§4) are sequential folds in the parent
evaluator: there is no kernel pipeline, no signal delivery, and
nothing to suspend.

Non-foreground pipelines (scripts, captured-output contexts) keep
the legacy kill-on-stop behavior: the structured
`stopped by signal …; ral killed the pipeline` diagnostic is
preserved.  There is no REPL to register a job in those contexts,
and no terminal to hand back, so parking would have nothing to do.

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

All fields except `name` are optional.

**Plugins run with host authority.**  The manifest's `capabilities:`
key is **advisory documentation only** — it is parsed-and-ignored at
load time and is not enforced at runtime.  Hooks, keybinding handlers,
and plugin-registered aliases execute under whatever capabilities the
caller's stack already grants.  To confine a plugin call, wrap the
call site in `grant { … }` (§11) — that is the one syntactic form
that demands OS-level enforcement, and it composes with plugin calls
the same way it composes with any other code.  Authors *should* still
state a plausible capability set in the manifest so users can inspect
what the plugin claims to need, but ral does not treat the claim as
load-bearing.

`aliases` are registered into the shell's alias namespace at load
time and removed at unload.  An alias name collision with an existing
`rc` or plugin-registered alias is a load-time error. An alias whose
name is a lexical/prelude/builtin binding is a load-time error.

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

**The `_ed-*` family** provides the line-editor interface for plugin
handlers.  Every op is its own builtin (no string-op dispatch); each
is inert in non-interactive contexts (a call from a non-REPL script
raises).  All are gated by the `editor` capability (§11.5).

| Builtin | Gate | Description |
|---|---|---|
| `_ed-get` | read | `→ [text: Str, cursor: Int, keymap: Str]` |
| `_ed-text` | read | `→ Str` — current buffer text |
| `_ed-cursor` | read | `→ Int` — current cursor offset (chars) |
| `_ed-keymap` | read | `→ Str` — current keymap name |
| `_ed-lbuffer` | read | `→ Str` — text to the left of the cursor |
| `_ed-set` | write | `[text?: Str, cursor?: Int] →` partial buffer update; unknown fields ignored |
| `_ed-set-lbuffer` | write | `<text> →` replace left-of-cursor; right side preserved |
| `_ed-insert` | write | `<text> →` insert at cursor; cursor advances past insertion |
| `_ed-push` | write | save buffer to stack, clear |
| `_ed-accept` | write | mark buffer for immediate execution |
| `_ed-tui` | tui | suspend editor, run `{block}`, return `[output: Str, status: Int]` |
| `_ed-history` | read | `<prefix> <limit> → [Str]` prefix search; `limit=0` for unbounded |
| `_ed-parse` | read | tokenize buffer at cursor → `[words, current, offset]` |
| `_ed-ghost` | write | `<text> →` set/clear ghost (hint) text |
| `_ed-highlight` | write | `<spans> →` set highlight spans `[{start, end, style}]`; empty list clears |
| `_ed-state` | write | `<default> <updater> →` per-plugin persistent cell, read-modify-write |

`_ed-tui` installs a capture buffer around the body using the same
mechanism `let` applies at a byte-mode boundary (§4.3).  External
commands inside the body write their stdout into that buffer while
stderr still reaches the TTY, so a curses-style UI such as fzf
renders normally.  On return, a non-`Unit` value from the body wins
outright; otherwise the captured bytes are UTF-8-decoded (stripping
one trailing newline) and returned in the `output` field.  Body
failures are caught internally — the resulting record carries the
error message in `output` and the failure status in `status` —
so plugins can switch on `status` without wrapping the call in
`try`.

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
rather than shared state to avoid reentrancy; `_ed-tui` is rejected
from inside one.

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

- **SIGPIPE exception.** A non-final pipeline stage terminated by
  SIGPIPE is treated as success; the pipeline does not fail.
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
a `kind: String` discriminant (e.g. `audit` tree nodes, §10.3) are
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

ls | from-lines | stream-each { |line| … }
F[∅,Bytes] String   F[Bytes,∅] Step String   Step String → F[∅,∅] Unit
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
| `audit` | `Node` (§10.3) |

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

### 21.6  POSIX flags

`ral` accepts the following POSIX-standard flags so that it can be
registered as `$SHELL` without breaking tools (`tmux`, `ssh`, terminal
emulators) that pass them blindly:

| Flag | Effect                                                         |
|------|----------------------------------------------------------------|
| `-i` | Force interactive (REPL) mode even when stdin is not a tty.   |
| `-s` | Force reading stdin as a batch script regardless of tty status or positionals. `-s` takes precedence over `-i` when both are given. |
| `-e` | Accepted; no effect.  (POSIX: exit on error — ral has no equivalent.) |
| `-u` | Accepted; no effect.  (POSIX: error on unset variables — ral variables are always bound.) |

Long options also accept `--flag=value` as a separator in addition to
the usual space-separated form.

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

`HOME`, `USER`, `LOGNAME`, `PATH`, `SHELL`, `TERM`, `LANG`, `SHLVL`
(always incremented from the inherited value).

`PWD` and `OLDPWD` are deliberately *not* kept in `$env`.  They live in
shell-owned state alongside the logical cwd: `cd` updates the shell's
record of "where we are" and "where we just were" without touching the
process cwd, and child commands receive the right values through
`Command::env("PWD", …)` / `Command::env("OLDPWD", …)` on each spawn.
Within ral code, the current directory is always read via `cwd`; reading
`$env[PWD]` returns nothing.

### 21.6  Current working directory

The shell tracks the working directory as a logical value, not as the
OS process cwd.  `cd` mutates the logical value; the kernel's
`getcwd(3)` keeps whatever it had at ral startup (parallel threads
sharing one process cwd is exactly the race the logical model
eliminates).

Three values participate, in precedence order:

1. `within [dir: PATH] { … }` — the dynamic override, in effect only
   for the body of the `within` block.  Rolls back at scope exit.
2. The persistent shell cwd, mutated by `cd`.  Persists across thunks
   in the same thread; spawned threads inherit a snapshot at their
   spawn point and any later `cd` they perform stays private to the
   thread.
3. The OS process cwd, used as a fallback (e.g. for shells created
   without the standard startup seeding).

`cwd` returns whichever of these is in effect.  Every cwd-sensitive
builtin (`glob`, `exists`, redirection of relative paths, …) resolves
against the same value.  Child commands receive it via
`Command::current_dir` on every spawn, so `within [dir: PATH] { ls }`
and a previously-issued `cd PATH` both produce a child that reads `ls`'s
output from `PATH`.
