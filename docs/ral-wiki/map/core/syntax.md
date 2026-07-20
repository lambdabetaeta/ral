---
generated_at_commit: 1cff92ae8c6c493aa045926a8977195c7fb16293
generated_at_date: 2026-07-20
covers_paths: [core/src/syntax/]
---

# Map: core / syntax

`core/src/syntax/` turns source text into the surface AST — the only place that
sees raw bytes and bare words.

- `lexer.rs` — `lex(source) -> Result<Vec<(Token, Span)>, LexError>`; the
  `Token` enum.
- `parser.rs` — `parse(source) -> Result<Vec<Stmt>, ParseError>`; `parse_with`
  carries a `FileId`. A digit glued to a comparison operator inside `$[…]`
  (`$[2>3]`, lexed as the file-descriptor redirect `2>`) earns a diagnostic
  that names the shape and asks for spaces, not a bare "redirect" token.
  `<<` is the here-string redirect; the bash spellings (`<<<`, a glued
  heredoc `<<EOF`) earn targeted diagnostics naming ral's form.
- `ast.rs` — the surface AST. `Ast` is the expression node and `Stmt` the
  statement node; the enum is deliberately wide and flat
  ([[decisions/260530_ast-stays-flat|ast-stays-flat]]).
- `group.rs` — pre-pass detecting mutually recursive binding groups, consumed by
  the [[map/core/elaboration|elaborator]].
- `quote.rs` — bare-word classification (`is_bare_word`, `quote_word`,
  `quote_word_if_needed`), shared with the [[map/repl|REPL]].
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

A command's pipe modes are the modal projection of its declared type, read
once: a builtin's boundary `PipeSpec` is `sig_pipe_spec` (the streaming reducer
`reducer_spec`) in [[map/core/typecheck|typecheck::builtins]], and the static
checker walks a prelude function's body for its modes. With the runtime engine
retired ([[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]),
there is no shell-free fallback and no hand-maintained byte-mode residue: the baked
prelude's schemes and the IR's ground wires carry the real inferred modes for every
prelude export.

The five reserved [[design/control-operators|control operators]] are recognised
at this layer; everything else is a library binding. See `docs/SPEC.md` §2–§4
for the grammar.
