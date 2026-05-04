# Backtick tags and comma-bearing bare words

Plan for 5.3-codex.  Start by replacing the grammar draft in
`docs/SPEC.md` section 1, then make the parser match that grammar.
Do not grow a second grammar in parser comments, tree-sitter, or tests.

## Goal

Two surface problems share one cause: leading punctuation is doing too
much lexical work.

- `.` should be available in bare filenames and argv words.  Tags move
  from `.ok` to `` `ok ``.
- `,` should be available in ordinary bare words such as
  `coreutils,diffutils`.  Comma remains a separator only in bracketed
  list/map/pattern syntax.

No compatibility syntax is required.  ral is still moving fast; carrying
both `.ok` and `` `ok `` would make the grammar less honest for little
benefit.

## Proposed grammar

This is the cleaned-up replacement for `docs/SPEC.md` section 1.  Keep
it close to this shape; if implementation reveals a semantic mismatch,
fix the grammar first and then fix the parser.

```text
program       = stmt*
stmt          = binding | bg-pipeline (NL? '?' bg-pipeline)* NL?
bg-pipeline   = pipeline '&'?
pipeline      = stage (NL? '|' NL? stage)*
stage         = return-stage | if-stage | case-stage | command | atom-stage

binding       = 'let' pattern '=' binding-rhs
binding-rhs   = pipeline (NL? '?' pipeline)* '&'?
return-stage  = 'return' atom?
if-stage      = 'if' atom atom ('elsif' atom atom)* ('else' atom)?
case-stage    = 'case' atom atom

command       = '^' NAME (arg | redir)*
              | implicit-head (arg | redir)*
              | explicit-head (arg | redir)+
atom-stage    = VALUE_NAME | explicit-head
implicit-head = NAME | SLASH_WORD | TILDE_WORD
explicit-head = nonword-head index* | implicit-head index+
arg           = atom | '...' atom

atom          = primary index*
primary       = word | tag | block | collection
tag           = TAG atom?
index         = '[' word ']'

block         = '{' stmt* '}' | '{' '|' pattern+ '|' stmt* '}'
pattern       = '_' | IDENT | plist | pmap
plist         = '[' (pattern (',' pattern)* (',' '...' IDENT)?)? ']'
pmap          = '[' pentry (',' pentry)* ','? ']'
pentry        = IDENT ':' pattern ('=' atom)?

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
nonword-head  = QUOTED | INTERP | deref | force | expr-block | block | collection | tag
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

Lexer draft:

```text
IDENT      = [a-zA-Z_][a-zA-Z0-9_-]*
TAG        = '`' IDENT
NAME       = one or more bare characters, slash-free
SLASH_WORD = NAME? ('/' NAME?)+
TILDE_WORD = '~' NAME? ('/' NAME?)*
WORD       = NAME | SLASH_WORD | TILDE_WORD
QUOTED     = '#'^n '\'' .* '\'' '#'^n
INTERP     = '"' (ICHAR | ESCAPE | deref ('[' word ']')* | force | expr)* '"'
NUMBER     = [0-9]+ ('.' [0-9]+)?
NL         = '\n' | ';'
COMMENT    = '#' .* (NL | EOF)
```

Bare characters now include `.` and `,`.  Backtick is not a bare
character: a literal backtick-bearing word must be quoted.  Comma is a
`','` token only while lexing inside `[ ... ]`; outside brackets it is
part of `NAME` / `SLASH_WORD`.  This is lexer state, not parser
guessing.  The lexer already tracks bracket depth for newline handling;
reuse that mechanism and document it in SPEC.

The `map` rule deliberately says a map with spreads must contain at
least one keyed entry.  Thus `[...$xs]` remains a list spread, while
`[...$cfg, port: 9090]` is a map.  That matches the current parser's
`is_map_ahead` rule, but makes the rule explicit.

## Implementation plan

### 1. Update SPEC first

Edit `docs/SPEC.md` section 1 before touching code.  The implementation
must conform to that text, not to stale parser comments.

Also update the variant prose in SPEC:

- `` `ok 5 `` constructs a variant.
- `` `ok `` is nullary at tag-payload boundaries.
- Handler records use tag keys: ``[`ok: { |x| ... }, `err: { |e| ... }]``.
- Dot is ordinary in bare words: `.env`, `.gitignore`, `foo.bar`.
- Comma is ordinary outside brackets: `a,b,c`, `--features x,y`.

Remove or rewrite the current claim that the lexer is modeless.  The
new precise statement is: the lexer is single-pass; it keeps delimiter
depth; comma is punctuation only inside bracket depth.

### 2. Lexer changes

Files:

- `core/src/lexer.rs`
- `core/src/ast.rs` only if `Word` docs need updating

Changes:

- Change the tag branch from `.` followed by ident to backtick followed
  by ident.  `Token::Tag(String)` can stay; its `Display` should render
  with backtick.
- A bare backtick not followed by `IDENT` should be a lex error with a
  useful message: "expected tag label after backtick; quote literal
  backticks".
- Make `.` an ordinary bare character.  Keep `...` as `Token::Spread`;
  it is already grammar punctuation and remains special.
- Make `,` an ordinary bare character outside brackets.
- Emit `Token::Comma` only when the current delimiter stack says the
  lexer is inside `[ ... ]`.
- Keep colon's existing context-sensitive rule: `host: val` splits,
  `host:5432` stays bare.
- Update lexer module docs and tests so they describe the current rules,
  not the old `.tag` rules.

Important edge cases:

- `echo a,b,c` => `Word("a,b,c")`
- `[a, b, c]` => comma tokens
- `echo .env` => `Word(".env")`
- `` return `ok 5 `` => `Token::Tag("ok")`, payload `5`
- `...$xs` remains spread
- `..` and `.` remain words

### 3. Parser changes

Files:

- `core/src/parser.rs`
- parser tests in the same file and/or integration tests

Changes:

- Parser logic for `Token::Tag` should mostly survive.  It now receives
  tags from backtick rather than dot.
- Keep the existing tag-payload boundary rule unless SPEC says otherwise:
  separators, closers, redirects, pipe, question, newline, EOF, and comma
  terminate a nullary tag.
- Update map parsing so `Token::Tag(label)` keys become tag keys rendered
  with the new surface label, not `.label`.
- Update errors: "expected map key: name, 'quoted', backtick tag, or $var".
- Make parser comments match the grammar exactly.  Avoid informal
  comments such as "primary = word | block | list | map" if `tag` and
  `collection` are now part of the rule.

Do not add fallback parsing from `.tag` to `` `tag ``.  `.tag` is a
word.  If a user writes `return .ok`, it should return the string `.ok`
or be treated as a word according to the existing value-literal rules,
not construct a variant.

### 4. Row-label representation

Files:

- `core/src/typecheck/infer.rs`
- `core/src/typecheck/unify.rs`
- `core/src/typecheck/fmt.rs`
- `core/src/typecheck/builtins.rs`
- `core/src/step.rs`
- `core/src/evaluator/case.rs`
- any code that formats `".{label}"` or checks `starts_with('.')`

Current code uses a leading dot inside type-row labels to mark the tag
alphabet.  Do not leave old dot labels leaking into type errors after
the surface changes.

Minimum acceptable patch:

- Introduce helpers such as `tag_row_label(label)`, `tag_map_key(label)`,
  `is_tag_label(label)`, and `render_row_label(label)`.
- Use a leading backtick sentinel internally for tag-keyed row labels,
  e.g. `` `ok ``.
- Change `is_tag_label` from `starts_with('.')` to the helper rule.
- Change `MORE_TAG` / `DONE_TAG` in `core/src/step.rs` to backtick
  labels.
- Change `case` runtime handler lookup from `.{label}` to the new
  tag-key helper.
- Change type and value formatting so variants display as `` `ok 5 ``
  and tag-keyed record rows display with backticks.

Better patch, if time permits:

- Replace string-prefix tag detection with an explicit row-label type
  (`Bare(String)` / `Tag(String)`) in the type system.
- This is cleaner but larger.  Do it only if the patch remains local and
  mechanical.  The grammar change should not become a full type-system
  refactor.

Runtime variant labels should remain bare (`ok`, `err`, `more`, `done`)
inside `Value::Variant`.  The sigil belongs to syntax and row keys, not
to the runtime constructor name.

### 5. Prelude, tests, and docs migration

Search excluding generated/vendor trees:

```text
grep -R -n --exclude-dir=target --exclude-dir=.git --exclude-dir=vendor --exclude-dir=bench --exclude-dir=node_modules '\.[A-Za-z_][A-Za-z0-9_-]*' core ral tests docs TUTORIAL.md README.md plugins dev/docs
```

Expected high-value migrations:

- `core/src/prelude.ral`: step constructors and `case` arms.
- `tests/builtins/*.ral` and `tests/lang/*.ral`: `_try`, `case`, Step.
- `docs/SPEC.md`, `docs/RATIONALE.md`, `TUTORIAL.md`, `docs/RAL_GUIDE.md`.
- Typechecker and evaluator comments/errors mentioning `.ok`, `.err`,
  `.more`, `.done`.

Be careful not to rewrite Rust method calls such as `.ok()` or prose
about file names.  Only migrate ral surface syntax and tag-row labels.

### 6. Editor and completion mirrors

Files:

- `editors/tree-sitter-ral/grammar.js`
- `editors/tree-sitter-ral/queries/highlights.scm`
- `editors/ral-syntax/*`
- `ral/src/repl/complete.rs`

Changes:

- Tree-sitter should recognise backtick tags and stop treating leading
  `.ident` as a tag.
- Bare-word rules should allow comma and dot.
- Completion quoting should no longer quote names solely because they
  contain comma or dot.
- Backtick should require quoting.

Keep these mirrors subordinate to `core/src/lexer.rs`; do not invent
slightly different editor syntax.

### 7. Regression tests

Add or update tests at the lexer, parser, and integration levels.

Lexer tests:

- `echo a,b,c` tokenises `a,b,c` as one plain word.
- `cargo build --features coreutils,diffutils,grep` keeps the feature
  list as one plain word.
- `[a, b, c]` still emits comma tokens.
- `[host: db, port: 5432]` still emits comma tokens and colon tokens.
- `echo .env .gitignore foo.bar` tokenises all three as words.
- `` `ok `` tokenises as `Token::Tag("ok")`.
- Lone backtick errors clearly.

Parser/integration tests:

- `` return `ok 5 `` constructs a variant and prints `` `ok 5 ``.
- `` case `ok 5 [`ok: { |x| return $x }, `err: { |_| return -1 }] ``
  returns `5`.
- `_try` examples use `` `ok `` / `` `err `` arms.
- Step examples use `` `more `` / `` `done ``.
- `echo a,b,c` passes one argv argument.
- `echo .env` passes one string argument, not a variant.
- `[...$xs]` remains a list spread.
- `[...$cfg, port: 9090]` remains a map spread plus keyed override.

Negative tests:

- `return .ok` does not construct a variant.
- `` return ` `` gives the new helpful lex error.
- Mixed record alphabets still fail, now with backtick tag keys:
  `` [host: 'db', `prod: 443] ``.

### 8. Verification

All compilation and tests must run inside Docker:

```text
docker exec shell-dev cargo test
docker exec shell-dev cargo test -p ral-core
docker exec shell-dev cargo test -p ral
```

If failures are too broad, narrow first with:

```text
docker exec shell-dev cargo test -p ral-core lexer
docker exec shell-dev cargo test -p ral-core parser
docker exec shell-dev cargo test -p ral variants
```

Do not use `git -A`.  Stage only files touched by this migration if a
commit is requested later.

## Non-goals

- No `.tag` compatibility mode.
- No parser retry after a `.tag` parse failure.
- No command-argument special case where `.foo` means text in commands
  but tag in values.
- No change to `$`, `!`, `^`, `~`, comments, or string interpolation.
- No broad type-system refactor unless explicit row labels stay small
  and mechanical.
