---
status: active
---

# A failing cleanup pre-empts

**`guard M { N }`: any halt of the cleanup pre-empts the body's outcome — an
ordinary error exactly as a control escape already did. A cleanup that cannot
fail the computation is a cleanup whose failures are unreportable.**

## Decision

- Precedence, in full. Body returns and cleanup halts → the cleanup's halt is
  the outcome. Body halts and cleanup halts → the *cleanup's* signal wins,
  whichever kinds the two are. Body halts and cleanup returns → the body's
  signal survives, as before.
- One rule for both signals, so `eval_guard`
  ([[map/core/evaluator|core evaluator]]) stops reading the cleanup's `Break`:
  it runs the cleanup, and the body's result is reachable only through the
  cleanup's `Ok`.
- Log-and-continue keeps its spelling and loses its privilege.
  `guard M { try N { |err| … } }` reports the cleanup's failure and carries on;
  `guard M { attempt N }` swallows it silently. **Primitives strict, sugar
  soft** — the soft reading is one keystroke away and is the caller's to ask
  for.
- Typing is untouched. A halt carries no value, so `guard` still takes its
  value and route from the body alone and the cleanup still contributes
  neither.

## Why the split rule had to go

The escape half was already this rule: an `exit` or a stopped job raised in the
cleanup propagated, because discarding a stop orphans a process group whose
pgid is then lost — never resumable, never reapable. The error half was
trap-EXIT folklore. The cleanup's `Err` went to stderr as `guard: cleanup
failed: …` and the body's result stood.

Its cost is the thing this language exists to refuse. A `guard` whose cleanup
releases a lease, commits a staging file, or unmounts a directory is doing work
the program depends on; when that work failed, a caller reading the exit status
saw success, `try` could not catch it, `?` could not fall through on it, and
`audit` recorded no failure — the diagnostic went to a stream with no structure
and no reader. A shell that reports a failure only in prose has not reported
it.

The asymmetry was unmotivated besides. The two signals differ in what *catches*
them, not in whether the cleanup got to the end of its work.

## The kernel already said this

`guard M ∶ N` reduces by resuming its cleanup in front of the outcome it found,
as a discarded statement — `βguard-halt`, whose whole content is the detour
through `؛`. One rule covers both signals there because the rule never inspects
the signal: the cleanup runs for its effects, and the halt it found is back in
focus when `β؛` fires. This was the Agda kernel's one standing *owned*
divergence from the shell, and the shell's side is the side that moved.
Retiring it also retires the mortgage the divergence carried — a machine-own
stderr writer plus a rendering `Err → 𝔹*` that the abstract `Err` declines to
give, owed to no other clause.

## Consequences

- A failing cleanup is now catchable by `try`, visible in an `audit` tree, and
  present in the process exit status: the three places a failure belongs.
- A program that wants its old behaviour says so in its own text, where a
  reader can see the decision instead of inferring it from a form's folklore.
- `docs/SPEC.md` §8.7 and §17.8 state the single rule, and
  [[design/failure|failure]] states its precedence beside `try` and `attempt`.
- `guard_cleanup_error_pre_empts_the_body` (`core/tests/scope_escapes.rs`)
  carries the new meaning, beside the escape test it is now a second reading
  of.

See [[design/failure|failure]], [[design/control-operators|control-operators]],
[[design/capture|capture]] (a cleanup's bytes are chatter, unchanged by this)
and [[map/core/evaluator|core evaluator]].
