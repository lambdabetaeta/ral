---
generated_at_commit: 6d48e9af
generated_at_date: 2026-08-26
covers_paths: [core/src/syntax/]
---

# Map: core / syntax

`core/src/syntax/` turns source text into the surface AST — the only place that
sees raw bytes and bare words.

- `lexer.rs` — `lex(source) -> Result<Vec<(Token, Span)>, LexError>`; the
  `Token` enum. An unterminated string names the exact closer it still wants,
  `StringForm::closing()` — a bumped literal wants its `#` run back after the
  `'`, so the bare reflex never closes it. Folding fd 1 onto fd 2 (`1>&2`, and
  `>&2`, which is the same redirect spelled short) is refused at the lexer,
  pointing at the `warn` builtin, and at `2>&1` for a program holding the
  direction backwards: a diagnostic is a verb here, not a second name for the
  byte channel.
- `parser.rs` — `parse(source) -> Result<Vec<Stmt>, ParseError>`; `parse_with`
  carries a `FileId`. A trailing `&` is a stage terminator only so its refusal
  can name `spawn { … }`, whose handle you `await`. A digit glued to a
  comparison operator inside `$[…]`
  (`$[2>3]`, lexed as the file-descriptor redirect `2>`) earns a diagnostic
  that names the shape and asks for spaces, not a bare "redirect" token.
  `<<` is the here-string redirect; the bash spellings (`<<<`, a glued
  heredoc `<<EOF`) earn targeted diagnostics naming ral's form. A run of
  separators collapses to one token, `Token::Semi` when it contains a `;` and
  the soft `Token::Newline` otherwise: a `;` is *hard*, never crossed by a
  pipeline or chain continuation. `parse_pattern` is the one place a duplicate
  binder is refused — a pattern binds all its names or none, so
  `let [same, same] = [1, 2]` is a parse error at the pattern's span, while a
  repeat across curried parameters stays ordinary shadowing.
- `ast.rs` — the surface AST. `Ast` is the expression node and `Stmt` the
  statement node; the enum is deliberately wide and flat
  ([[decisions/260530_ast-stays-flat|ast-stays-flat]]). `Ast::Unit` is the `()`
  literal — punctuation denoting the unit value, like `[]` and `[:]`, so not a
  word. `Ast::Case` carries `arms: Vec<CaseArm>`, a finite list of tag-and-body
  alternatives the parser hands on whole
  ([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]).
  `WordLiteral::classify` is the numeral grammar: purely lexical, never a round
  trip through printing, so `007` and `1.50` are numerals while `1e5`, which
  merely happens to f64-parse, is text
  ([[decisions/260824_a-numeral-denotes-its-number|a-numeral-denotes-its-number]]).
- `group.rs` — pre-pass detecting mutually recursive binding groups, consumed by
  the [[map/core/elaboration|elaborator]].
- `quote.rs` — bare-word classification (`is_bare_word`, `quote_word`,
  `quote_word_if_needed`), shared with the [[map/repl|REPL]]. It is the dual of
  `WordLiteral::classify`: a string may go bare only where the numeral grammar
  declines it, since a bare `007` would read back as the number 7.
- `tag.rs`, `free_refs.rs` — variant-label tagging and free-reference scans.

**Recursion is bounded by a single shared depth cap, so adversarial nesting
rejects cleanly instead of overflowing the stack.** One `NESTING_DEPTH_LIMIT`
governs both stages.
- The lexer enforces it at `scan_token_group`, the sole recursion through
  bracketed token groups.
- The parser's three mutually-recursive sub-grammars each descend through
  exactly one guarded chokepoint, all counting against `Parser::nested`'s
  shared `depth`: values through `parse_primary`, arithmetic through
  `parse_expr_atom` (the unary prefixes `-` / `not` and parenthesised
  sub-expressions bottom out here), patterns through `parse_pattern` (list and
  map patterns recurse back through it per element).

The five reserved [[design/control-operators|control operators]] are recognised
at this layer; everything else is a library binding. See `docs/SPEC.md` §3,
§17.1 for the grammar.
