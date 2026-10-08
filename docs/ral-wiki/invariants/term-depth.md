# Term depth

**A term's depth is not bounded by its syntax, and the language puts no limit
on it. Every walk that recurses over a term, parse to typecheck and the
printers, runs on the compile stack: `compile::on_compile_stack`, a thread
with a 64 MiB reservation, through which `compile_and_typecheck` and the
static halts run. A new recursive pass over the AST or the IR runs under that
door, or it is a stack overflow waiting for a long enough program.**

- **Why syntax does not bound it.** The parser's `NESTING_DEPTH_LIMIT = 64`
  caps delimiter and prefix nesting, which is the parser's own recursion. A
  sum `$[1+…+1]`, a curried lambda `{|x1 … xN| …}`, a `?` chain, an `elsif`
  chain and a block's statement list each nest one level per element in the
  AST or the IR: the Pratt loop builds a left spine, `parse_block` curries,
  `Chain` lowers to right-nested `try`, `elsif` folds, and a block body folds
  into a `Bind` chain.
- **Why not a measure.** A block *is* a `Bind` chain, walked by the checker,
  the annotator and the drop glue alike, so no depth measure rejects a deep
  term without rejecting a long block; and a limit safe on Windows' 1 MB main
  stack would reject a few hundred statements.
- **Why not `stacker`.** Tried and declined: it protects only the walks that
  are wrapped, while derived `Drop`, `Clone`, `Debug` and the type walks
  (`apply_ty`, `free_ty`, `generalize`) stay recursive, and a recursion inside
  a grown segment gets that segment's 1 MB, less than it had before. One
  thread, one reservation, one site.
- **What is still recursive, and where.** The parser, bounded by its cap. The
  evaluator is an explicit-step machine and recurses over nothing. The checked
  `Toplevel` is dropped on the caller's thread, where drop glue at about 200
  bytes a level handles 40 000 levels in 8 MB and about 5 000 in Windows' 1 MB
  main stack: the one edge the door does not cover.
- **Measured.** Release binary, 8 MB stack, before the door: 5 000 parameters,
  `?` arms or `elsif` branches overflowed the checker; a 5 000-statement block
  or 5 000-term sum survived and 20 000 did not. A debug build spends about
  13 KB of stack a level in the elaborator. `core/tests/deep_terms.rs` holds
  the five shapes at 2 000 on the 2 MB test thread.
- **Unification alone is budgeted.** A depth budget belongs to unification,
  not to every traversal of a type: `MAX_UNIFY_DEPTH = 512` (`unify.rs`),
  charged by `deeper()` on each descent into a strictly deeper subterm, since
  a substitution composes types the program never wrote beside one another.
  `apply_ty`, `free_ty` and `generalize` carry none, and `row_occurs` spends
  the caller's budget.
- **Where.** `compile::on_compile_stack` and `COMPILE_STACK`
  (`core/src/compile.rs`); `ral/src/batch.rs::halted` for `--dump-ast` and
  `--dump-ir`; `syntax::NESTING_DEPTH_LIMIT`; `typecheck::unify::MAX_UNIFY_DEPTH`.
