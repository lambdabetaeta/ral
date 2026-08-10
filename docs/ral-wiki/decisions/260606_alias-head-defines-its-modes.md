---
status: superseded
superseded_by: decisions/260809_pipes-are-positional-byte-wires
landed: [02ee909f]
---

# A fresh head defines its own modes; preservation binds only a known head

A handler or alias reinterprets a head, so where a head already has modes the
arm must agree with them — but a brand-new alias or handler name *is* the head:
there is nothing prior to preserve. **`head_pipe_spec`'s unknown case yields a
fully fresh `F[μ, ν]` and the arm defines the head's modes; preservation is
retained exactly where a known head's modes exist to be reinterpreted, and the
byte-channel discipline that does the real work moves to the connection point.**

- **An unknown head is no longer pinned to the external byte default.** `head_pipe_spec`'s
  `None` arm (`core/src/typecheck/infer.rs`) mints a fresh input and output mode
  rather than the external `F[μ, Bytes]`. `pin_arm_to_head` unifies the arm's
  spec against that fresh spec — a no-op constraint — so `handler_comp_scheme`
  lets the arm's own modes stand. An alias whose body iterates values
  (`… | from-lines | { |x| … }`) installs as a value-output command.
- **A known head still constrains the arm.** When `lookup_handler` finds a scheme
  whose `CompTy` resolves to a `Return`, its `PipeSpec` is the head's, and pinning
  an incompatible arm against it is a positioned `ModeMismatch`. Reinterpreting an
  existing handler with a value output under a byte-output head is unchanged: still
  rejected, still at the definition.
- **The byte-channel discipline is the real invariant, and it is enforced at the
  connection.** `O_left = I_right` (`docs/SPEC.md` §4.2.1) holds where pipeline
  channels meet, not at the alias definition. A value-output head piped into a
  byte consumer is still a mismatch — now reported at the *use site* where the
  edges connect rather than eagerly at the head's definition. The rule moved from
  definition-time pinning to connection-point checking; soundness is unchanged.
- **A `?` fallback chain carries its arms' modes.** `infer_chain` unions the arms'
  input and output modes via `union_mode` — the same widening `merge_branches`
  gives `if`, since only one arm runs — while the chain's value type stays a fresh
  variable (the arms' value union is unspellable). A chain of byte-output arms
  (`tmux a ? tmux b`) reads as byte-output and so installs as an alias over a byte
  head, where the prior value-edge default discarded the arms' modes.

Why now: [[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]]
made plugin files and rc aliases fatally checked by the single static checker.
Under eager definition-time pinning, ordinary value-output aliases and `?`-chain
aliases could not install. Letting an alias or handler be a command that defines
its own modes — keeping preservation only where a head exists to be
reinterpreted — is the correct resolution, not a weakening: the byte discipline
still binds, at the edge.

This supersedes
[[decisions/260603_handler-alias-mode-preservation|handler-alias-mode-preservation]],
which pinned every unknown head to the external byte default and so read a
value-output arm as a static mismatch at the definition.

See also
[[decisions/260603_unconditional-mode-pass|unconditional-mode-pass]],
[[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]],
[[map/core/typecheck|typecheck]],
[[design/effects-handlers|effects-handlers]];
the byte-edge judgment is `docs/SPEC.md` §4.2.1.
