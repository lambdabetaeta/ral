---
verified_at_commit: 7ba500b
verified_at_date: 2026-06-17
anchors: [lex, parse, Head, scan_expr_block]
---

# Surface syntax: lexing and parsing

The front of the [[internals/compilation-ladder|ladder]]. `core/src/syntax/` is
the only code that sees raw bytes and bare words.

**Lexing has no context-dependent rules.** `lex(source)` produces
`Vec<(Token, Span)>` in one pass under one grammar — ral carries none of the
mode-switching a POSIX shell's lexer needs (no separate arithmetic / test / glob
lexers), because it has no history forcing them. The two sigils stay lexically
distinct: `$name` is a value dereference, a bare word in head position is a
command token. The arithmetic sublanguage `$[…]` is pre-scanned by a dedicated
`scan_expr_block` so its Pratt parser downstream sees exactly those tokens.

**Parsing is recursive descent with a Pratt core for expressions.**
`parse(source)` returns `Vec<Stmt>` or a `ParseError`. Statement and pipeline
productions descend recursively (`parse_stmt`, `parse_pipeline`, `parse_primary`)
under a depth counter that fails cleanly on pathological nesting rather than
overflowing the host stack. The `$[…]` sublanguage — arithmetic, comparison,
Boolean — is one Pratt parser by binding power (`parse_expr_prec`), not bash's
partitioned `(( ))` / `[[ ]]`; one grammar, again because no history forces
several.

**The AST is flat by decision.** `Ast` (expressions) and `Stmt` are wide flat
enums ([[decisions/260530_ast-stays-flat|ast-stays-flat]]); no desugaring happens
here. The surface forms are preserved verbatim for the
[[map/core/elaboration|elaborator]], the one phase that lowers them.

**Head classification refines in three stages, each deciding a different
question.** The parser fixes the *syntactic shape* of each command head
(`ast.rs`: `ExternalName` for `^name`, `Path` / `TildePath` for `./x` / `~/x`,
`Bare` for a lexical-lookup-or-`Exec` word). The
[[map/core/elaboration|elaborator]] resolves a `Bare` head against the lexical
scope — a bound name lowers to CBPV application, an unbound one to `Exec`
(forward declarations cover only the thunk-form bindings `group.rs` knots, so
the shadow agrees with what evaluation will see). A name-dispatched `Exec`
then resolves binding → handler → PATH at *evaluation* time
(`ir.rs` `CommandWord`), so handler installation and PATH state are read when
the command runs, not when it was parsed. The five reserved
[[design/control-operators|control operators]] are recognised at the parser
layer and everything else is a library binding. `group.rs` is a pre-pass
marking mutually recursive binding groups for the elaborator.

See also [[internals/compilation-ladder|compilation-ladder]]; map
[[map/core/syntax|syntax]]. Grammar: `docs/SPEC.md` §1–§4; RATIONALE §"No
context-dependent lexer rules", §"Expression blocks".
