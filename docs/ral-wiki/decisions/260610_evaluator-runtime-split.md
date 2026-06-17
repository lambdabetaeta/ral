---
status: active
---

# evaluator/ is the calculus; runtime/ is the plumbing

**The CBPV machine and the OS plumbing now live in separate module
trees, so "the calculus is the cheap part" is a fact of the file layout
rather than a claim about it.** `core/src/evaluator/` had grown to ~9.2k
lines, over budget relative to the theory; inspection showed the actual
machine — `comp`, `expr`, `val`, `call`, `case`, `pattern`, `scope`,
`trampoline`, `capture`, redirect frames, `audit` — is ~2.6k, and the
other ~6.6k was command/pipeline/transport machinery already coupled to
it through clean seams. This relocates that machinery to
`core/src/runtime/`.

- **`evaluator/` is exactly the machine now.** `command/`, `command.rs`,
  `command_call.rs`, `pipeline/`, `pipeline.rs`, and `transport.rs` moved
  to `runtime/`; `evaluator/` holds only the eleven machine modules. Zero
  lines of logic changed — paths, `mod` declarations, and two visibility
  modifiers only ([[internals/evaluator-machine|evaluator-machine]],
  [[design/cbpv|cbpv]]).
- **The up-seam is three verbs.** The runtime re-enters evaluation
  through `call::invoke`, `eval_block`, and `absorb_tail` — the
  designated re-entry points. It also reaches `comp::eval_comp`, `audit`,
  `apply`, and `redirect`, each genuinely used elsewhere crate-wide
  (builtins, the crate-root child-eval runner, hosts via re-exports), so
  none narrows below `pub(crate)` without breaking a real caller. Rust's
  `pub(crate)` cannot make privacy enforce a three-function seam while
  those helpers serve other consumers; the seam is documented and as
  narrow as the existing coupling allows.
- **The down-seam is narrow, not singular.** The pipeline sub-seam is the
  single call `comp → pipeline::run_pipeline`, as intended
  ([[internals/pipeline-execution|pipeline-execution]]). Other
  evaluator→runtime edges remain — `call → command_call::run_call` and
  `command::EvalRedirect`, `redirect → command` guards, `evaluator →
  transport::dispatch` — pre-existing and not collapsible without a logic
  change. `runtime.rs` names them rather than pretending one edge exists.
- **The mutual recursion is irreducible.** A stage body contains
  closures, so the stage runner must re-enter the evaluator; the seam
  exists precisely because the recursion cannot be cut. The split makes
  the boundary visible, not absent.

Realistic end state, achieved: `evaluator/` at the machine size and
`runtime/` holding the fd/pgid/signal floor that any Unix shell pays.
