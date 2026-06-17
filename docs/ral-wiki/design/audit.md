# Audit

**ral records execution as a structural tree.** The `audit` operator runs its
body and returns, alongside the body's value, a tree of what ran — commands,
scopes, capability checks — so a run can be inspected after the fact. It is one
of the five [[design/control-operators|control operators]] for the same reason
as the others: the tree it threads lives in `Shell` state, not in any value the
body could construct.

**Ownership of the tree is lexical, not temporal.**

- Each scope-producing site — `audit`, and the other operators `within` /
  `grant` / `guard` / `try` — *owns* the nodes its body produces, recording them
  as the direct children of its own scope node.
- Source nesting decides the tree's shape, regardless of which process or thread
  executed which node.
- A process boundary — the OS-sandbox child a `grant` re-execs into
  ([[design/grant|grant]]), a pipeline-stage helper — only *transports* its
  audit fragment back to the owning evaluator, which merges it into place; the
  boundary never decides structure.

So a sandboxed `grant { … }` shows its body's commands as direct children, not
loose at the root, and an extracted subtree stays self-describing.

Each node is self-describing about who and how it ran:

- each node carries the `principal` in force where it ran, so the tree records
  *who* as well as *what*, and a detached subtree still names its actor;
- scope nodes carry no serialised arguments — their structural content (handler
  frames, capability maps) lives in the IR node, not in an args list;
- tail-recursive iteration flattens: a loop is one scope node with a child per
  iteration, not a chain as deep as the iteration count, matching how
  [[design/scoping|dynamic scope]] persists across tail calls.

What builds a tree is itself scoped:

- plain execution records nothing;
- `try` keeps only its flat error record;
- a `grant` contributes capability-check nodes only when it sets `audit: true`;
- `audit` builds the full subtree.

See also [[design/syscalls-are-effects|syscalls-are-effects]] — an audit tree is
a trace of the operations performed and the scopes that framed them.

Recording lives in `core/src/evaluator/audit.rs` ([[map/core/evaluator|evaluator]]);
`docs/SPEC.md` §10.1 and §10.3 give the formal account.
