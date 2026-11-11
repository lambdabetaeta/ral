---
generated_at_commit: becc34179cf764c11ef623c65e88f8f3d1006b57
generated_at_date: 2026-11-11
covers_paths: [core/src/syntax/, editors/]
---

# Map: core / syntax

`core/src/syntax/` turns source text into the surface AST — the only place that
sees raw bytes and bare words.

- `lexer.rs` — `lex(source) -> Result<Vec<Lexeme>, LexError>`; the
  `Token` enum. A UTF-8-boundary byte cursor scans borrowed slices; source
  length is checked against the `u32` span limit at entry. `next_token`
  stamps ordinary tokens once, and `operator_prefix` reads expression
  operators longest-first. `lex_open` shares the scanner under `EofPolicy::AllowOpen`:
  no source is appended or re-lexed, and no closing tokens are invented.
  `OpenLexemes` carries the written tokens and an unclosed-string span for
  editor quote stripping; all spans remain within the original source. The innermost open delimiter (`DelimKind`: `Brace`,
  `Bracket`, `Expr`) is the lexer's whole context: it decides whether a
  newline separates, whether `,` punctuates, and — inside `$[…]` only —
  whether `<` `>` `<=` `>=` `!=` `&&` `||` and the arithmetic characters are
  `Token::Op(Operator)` (a closed enum over `ir::BinaryOp` plus `And`, `Or`,
  `Assign`) rather than a redirect, refused punctuation or word characters. A
  closer that is not the innermost opener's is `LexErrorKind::Mismatched`
  (L0006), a definite error. In a level-`n ≥ 1` raw body, a non-closing `'`
  preceded by at least `n` body `#`s is `LexErrorKind::OpenerInRaw` (L0007),
  also definite. `fence_admits` defines admissible fences for both printers
  (`quote_word` and `data/print.rs`'s `quote_string`): neither opener nor closer
  may occur in the body. The bash reflexes `${…}` and `$(cmd args)` are
  refused at `scan_dollar`. Outside `$[…]` a word starting with `&`
  is refused naming `spawn { … }`, one starting `&&` naming `;`, while a `&`
  inside a word is ordinary (`?a=1&b=2`); a bare `$` is refused naming
  the three things it can open. A splice inside `"…"` (`scan_splice`) is the
  token stream the same text has outside the string, so `"!$d"` and `!$d` are
  one form; `$(name)` ends it, and every other splice continues into
  `[key]`. An unterminated string names
  the exact closer it still wants, `StringForm::closing()` — a bumped literal
  wants its `#` run back after the `'`, so the bare reflex never closes it.
  `StringForm::SingleQuoted(n)` includes ordinary quotes at level zero.
  A trailing backslash in a double-quoted string is incomplete too: the next
  newline can continue it. `RedirectOp` is `Read | HereString | Stdout(mode) |
  Stderr(mode)`, so an output stream cannot carry a read operator.
  Folding fd 1 onto fd 2 (`1>&2`, and `>&2`, which is the same redirect
  spelled short) is refused at the lexer, pointing at the `warn` builtin, and
  at `2>&1` for a program holding the direction backwards: a diagnostic is a
  verb here, not a second name for the byte channel.
  A bare word is shaped once: `Word::Tilde` by `TildePath::parse_for`,
  `Word::Path` when `path::names_a_path_for` finds a separator, else
  `Word::Plain`, both under the lexer's `PathRules` — `HOST` through
  `lex_with`, either set through `lex_for` in tests — so `C:\x` is a path
  under Windows rules and a plain word under Posix.
  A redirect list is bound stream by stream, `Redirects::bind` refusing a second
  binding of stdin, stdout or stderr at the second redirect
  ([[decisions/260930_redirects-are-bindings|redirects-are-bindings]]).
- `syntax.rs` — the module root: `CONTROL_OPERATORS` (`try`, `guard`,
  `within`, `grant`, `audit`, `spawn`, `gather`) and `FLOW_KEYWORDS` (`if`, `elsif`, `else`,
  `let`, `return`, `case`), read by the parser and, through
  `is_control_operator` / `is_keyword`, by the checker, highlighters, and the
  [[map/core/builtins|help registry's]] documentation-completeness test;
  `NESTING_DEPTH_LIMIT`.
- `parser.rs` — `parse(source) -> Result<Vec<Stmt>, ParseError>`; `parse_with`
  carries a `FileId`. A `||` after a pipe is refused naming `?`. The words rule is checked once
  per unit by `end_unit`. `[:]` is the empty map; `[:, …]` opens an explicit
  map on written keys, permits spreads and refuses `[:,]`. Otherwise keys
  decide: any computed key (`$k`, `"k$i"`, `MapKeyForm::Data`) makes a map,
  labels alone an `Ast::Record`, no keyed entry a list. `parse_static_key`
  admits words and constant single- or double-quoted strings. Patterns and
  options accept this same alphabet but refuse computed keys; a tag keys
  nothing. `parse_options(form, example)` returns one `Options` record for
  `within` or `grant`. Its values are ordinary atoms: `handlers: $table` and
  `handlers: [:, name: { |args| … }]` need no special parser. A repeated option
  uses the generic duplicate-key refusal. `parse_index_key` emits
  `IndexKey::{Label, Position, Data}`:
  written names, whole-number positions from zero, or computed expressions.
  Negative and fractional numerals, path and tilde words, and tags are refused
  with guidance about the permitted key form. The items are read in one pass, each
  lifting the literal to at least its own kind, so no token-level lookahead has
  to skip a spread's nested brackets. Every item is one atom
  (`parse_entry_value`): a command written as one — an argument, `|`, `?` or
  redirect after the value — is P0003 (`ParseErrorKind::EntryRuns`), whose
  help `syntax/report.rs` draws from `EntryReading`. Every other refusal of a
  bracket's shape is a P0001 whose help shows what to write
  (`ParseErrorKind::Plain { hint }`, set by `ParseError::with_hint`):
  `parse_bracket_items` reads the items of every `[…]` — literal, options,
  `case` arms, list and record pattern — and names them (`Bracketed`) when a `,`
  is missing or extra; `stray_colon` catches a `key: value` written outside
  `[ ]`. The `$[…]`
  body is a Pratt parser whose operands are
  `parse_atom`'s atoms, a sign touching a numeral included
  (`signed_numeral`), plus `(…)` grouping and the prefixes `-` and `not`;
  an operator in operand position, or two operands with no operator between
  them, each earn an error naming the gap; a bare non-numeral word under
  `-`, arithmetic, or ordering (`numeric_operand`) is refused as the string
  it is, asking whether `$name` was meant.
  `<<` is the here-string redirect; the bash spellings (`<<<`, a glued
  heredoc `<<EOF`) earn targeted diagnostics naming ral's form. A run of
  separators collapses to one token, `Token::Semi` when it contains a `;` and
  the soft `Token::Newline` otherwise: a `;` is *hard*, never crossed by a
  pipeline or chain continuation. `parse_pattern` is the one place a duplicate
  binder is refused — a pattern binds all its names or none, so
  `let [same, same] = [1, 2]` is a parse error at the pattern's span, while a
  repeat across curried parameters stays ordinary shadowing.
- `ast.rs` — the surface AST. `Ast` is the expression node and `Stmt` the
  statement node; the enum is deliberately wide and flat. The operator forms
  `Binary`, `Negate`, `Not`, `And`, `Or` are ordinary variants with `Ast`
  operands; `$[…]` itself leaves no node
  ([[decisions/260909_expression-block-is-a-lexical-mode|expression-block-is-a-lexical-mode]]).
  A command's head is classified once, as `Head` (`Bare`, `^`-`ExternalName`,
  `Path`, `TildePath`, or a `Value` head), so nothing downstream recovers its
  meaning from a generic `Ast`.
  `Ast::Redirected` holds a stage's trailing redirects, one node for every stage
  form; `Call` and `Scope` carry none
  ([[decisions/260930_redirects-are-bindings|redirects-are-bindings]]).
  `ScopeAst::Within { opts, body }` carries only the option record and body;
  there is no handler-specific AST node.
  `Ast::Unit` is the `()`
  literal — punctuation denoting the unit value, like `[]` and `[:]`, so not a
  word. `Ast::Case` carries `arms: Vec<CaseArm>`, a finite list of tag-and-body
  alternatives the parser hands on whole
  ([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]).
  `WordLiteral::classify` decides by the numeral grammar: purely lexical, never a round
  trip through printing, so `007` and `1.50` are numerals while `1e5`, which
  merely happens to f64-parse, is text
  ([[decisions/260824_a-numeral-denotes-its-number|a-numeral-denotes-its-number]]).
  `Pattern`, `BinaryOp` and `Redirects` are not here: the [[map/core/ir|IR]]
  owns them, and the parser builds them (`redirect_word`
  translates a redirect token).
- `numeral.rs` — the single home of the numeral grammar (`prefix`, `shape`);
  the lexer's numeral scanning, `WordLiteral::classify` and the parser's
  `signed_numeral` read it.
- `group.rs` — the scoping of a statement list: `parts` partitions it
  into statements and stretches of block definitions (SPEC §5.7), and `sccs`
  splits a stretch's dependency graph, which the
  [[map/core/elaboration|elaborator]] draws, into its recursive groups.
- `quote.rs` — bare-word classification (`is_bare_word`, `quote_word`,
  returning a `Cow`), shared with the [[map/repl|REPL]] and the
  `ral-quote` builtin. It is the dual of `WordLiteral::classify`: a string may
  go bare only where the numeral grammar declines it, since a bare `007` would
  read back as the number 7. A bidi control, which the lexer refuses even in
  quotes, sends the string to `"…"` with a `\u{…}` escape, so every string
  has a spelling.
- `highlight.rs` — `classify(src)`, token classes (`Class`) read off the lexer
  for the exarch and synod highlighters, so neither carries a second parser of
  its own.

## Editor projections

**Editor grammars approximate this surface; they do not define it.**

- `editors/tree-sitter-ral/grammar.js` and `queries/highlights.scm` preserve
  bracket-local whitespace, statement newlines in nested braces, touching
  spread/force operands, and adjacent nonempty word indices. Forced variable
  keys remain inside the force operand; delimited names take none.
- Tree-sitter bounds hash-bumped single-string fences at depths 1–3 and
  approximates positional colon splitting with colon-joined nonempty stems.
  Its `map_literal` covers both records and maps without enforcing the
  classification-dependent one-first-spread restriction on records.
- `editors/ral-syntax/ral.tmLanguage.json` and its hand-translated Sublime YAML
  use backreferences for arbitrary matching single-string fences. They scope
  whole numeral words, not numeric prefixes; numeric ranges remain runtime
  checks in every editor grammar. Regex bundles treat commas as punctuation
  even outside brackets, and arithmetic unary signs as operators.
- `test/corpus/` under the tree-sitter grammar tests parse behavior;
  `editors/ral-syntax/tests/` tests actual TextMate and syntect scope spans.
  Generated tree-sitter sources, headers and libraries are ignored outputs.
  Each grammar's README records regeneration and consumer coverage.

## Runtime nesting

**Recursion is bounded by a single shared depth cap, so adversarial nesting
rejects cleanly instead of overflowing the stack.** One `NESTING_DEPTH_LIMIT`
governs both stages.
- The lexer enforces it at `scan_token_group`, the sole recursion through
  bracketed token groups.
- The parser's three mutually-recursive sub-grammars each descend through
  exactly one guarded chokepoint, all counting against `Parser::nested`'s
  shared `depth`: values through `parse_primary`, expressions through
  `parse_expr_operand` (the unary prefixes `-` / `not` and parenthesised
  sub-expressions bottom out here), patterns through `parse_pattern` (list and
  record patterns recurse back through it per element).

The seven reserved [[design/control-operators|control operators]] are recognised
at this layer: `guard` has exactly three operands (`acquire, body, release`),
represented by `ScopeAst::Guard { acquire, body, release }`;
`spawn` has one operand or a title and body, and `gather` has one body.
`with-temp-dir` and `with-temp-file` are ordinary prelude bindings. See `docs/SPEC.md` §3,
§17.1 for the grammar.
