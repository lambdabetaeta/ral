 Findings

  - High: Map-pattern defaults keep raw Ast in runtime patterns and re-elaborate with an empty binding set at assignment time. This can misclassify lexical names in defaults and keeps parser
    syntax alive in evaluation. See core/src/ast.rs:143 and core/src/evaluator/pattern.rs:69. Defaults should be elaborated once, with the surrounding lexical context, into IR.
  - High: LetRec grouping says blocks participate, but only lambdas are collected. The docs say “lambda/block RHS” at core/src/group.rs:13, StmtGroup::LetRec says lambda or block at core/src/
    group.rs:88, and Ast::is_lambda includes blocks at core/src/ast.rs:267. The actual collector only matches Ast::Lambda at core/src/group.rs:108. Use value.is_lambda() or change the rule.
  - High: File-descriptor redirect parsing silently maps overflow to fd 0 or 1. 999999999999999999999> becomes stdout, not an error. See core/src/lexer.rs:919, core/src/lexer.rs:923, and core/
    src/lexer.rs:967. This should be a lex error with the offending fd text.
  - Medium: Ast::Pos is a semantic smell. The AST doc admits nodes carry no spans and position markers are interleaved at core/src/ast.rs:6; parser injects them at core/src/parser.rs:221 and
    core/src/parser.rs:274; elaboration then filters them at core/src/elaborator.rs:181. There is already span infrastructure. A Spanned<T> or Stmt { span, kind } would shorten parser/elaborator
    and remove sentinel nodes from semantic traversals.
  - Medium: Command policy-name logic is duplicated by design. Shell::bare_policy_names says it mirrors execution logic at core/src/types/shell.rs:484; the execution path is core/src/evaluator/
    exec.rs:475. This should be one command-identity function, probably near path/ or capability checking.
  - Medium: Builtins are not actually one source of truth. The registry claims name, hint, doc, dispatch cannot drift at core/src/builtins.rs:3, but full schemes live separately in core/src/
    typecheck/builtins.rs:79, and thunk synthesis depends on that table at core/src/builtins.rs:402. Add scheme/arity to builtin descriptors or generate both tables from one descriptor list.
  - Medium: The parser repeats ? chain parsing. Top-level statements do it at core/src/parser.rs:231; let RHS repeats the same shape at core/src/parser.rs:431. A small parse_chain(parse_item)
    helper would remove grammar drift and shorten the parser.
  - Medium: Interpolation/index/expression parsing uses raw strings and re-lexing. StringPart::Force, Expr, and Index store raw source at core/src/lexer.rs:54; parser reparses at core/src/
    parser.rs:959, core/src/parser.rs:1120, and core/src/parser.rs:1134. The &&/|| fusing workaround at core/src/parser.rs:1141 is a sign the boundary is wrong.
  - Medium: eval_comp is the operational core, but it is too crowded. The single match spans source tracking, dispatch, tail calls, pipeline execution, sequencing, capture flushing, and LetRec
    fixpoints from core/src/evaluator.rs:170. Split only along semantic rules, not mechanically: eval_letrec, eval_bind, eval_seq, eval_if would read closer to the rules.
  - Medium: exec.rs and pipeline staging are organized but oversized. exec.rs contains command identity, spawning, redirects, fd guards, and atomic writes; the atomic-write rationale alone runs
    from core/src/evaluator/exec.rs:1272. pipeline/stages.rs explicitly contains analysis, launch, and collect in one 1100-line file at core/src/evaluator/pipeline/stages.rs:1. These already
    have clean seams.
  - Medium: Shell is still a god object. It owns lexical env, dynamic state, control, location, IO, registry, modules, audit, REPL scratch, hints, and cancellation at core/src/types/shell.rs:62.
    The child-state flow is carefully documented but heavy at core/src/types/shell.rs:550. Move policy, child inheritance, and path resolution out behind smaller types.
  - Medium: Several dynamic-scope helpers manually restore state and are not panic-safe. Examples: core/src/types/shell.rs:145, core/src/types/shell.rs:185, core/src/types/shell.rs:199, core/
