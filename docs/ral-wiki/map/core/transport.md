---
generated_at_commit: 50388d83
generated_at_date: 2026-08-29
covers_paths: [core/src/serial.rs, core/src/subprocess.rs, core/src/subprocess_codec.rs]
---

# Map: core / transport

The wire layer that carries a shell across a process boundary. When a pipeline
stage runs in a re-exec'd helper, the shell's mobile state — `env`,
`context`, the relevant parent state — is serialised to JSON,
framed, and reconstituted on the other side of a re-exec of this
[[invariants/single-binary|same binary]] ([[map/core/shell-state|shell-state]]).
(A [[design/grant|grant]] does
not ride this wire: its body evaluates locally, and external children are
confined per-command — see
[[decisions/260617_sandbox-external-children|sandbox-external-children]].)
The front-end⇄engine protocol is a separate wire —
[[map/core/engine-protocol|engine-protocol]].

**Every wire↔runtime hop is an exhaustive, field-complete map: no hop may pass
through a constructor that defaults a field the wire carries, and no kind may
round-trip through a string with a catch-all decode arm.** This is what keeps
helper-stage evaluation indistinguishable from local — a divergence between the
two is exactly a field the hop dropped or a variant it collapsed. The discipline
is mechanical: an exhaustive match makes a new variant fail the build, and a
field-complete struct literal makes a new field fail it. Three
realisations:

- *value walks* (`serial.rs`) match `Value`/`SerialValue` exhaustively;
- *hydration* installs a complete `HandlerFrame` through
  `HandlerStack::push_frame` rather than re-deriving fields like
  `removable_by_unalias`, so a wire-hydrated alias stays removable by `unalias`;
  a per-name entry's calling convention rides as `HandlerArity::Unary` by
  construction, never re-sniffed from the thunk's shape — the values cleared
  install-time arity validation on the sender, so hydration does not re-check
  ([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]);
- *kinds* ride as serde enums — `WireObservation`'s `what` mirrors `Observed`,
  not a string — and *floats* ride by IEEE-754 bits (`f64::to_bits`/`from_bits`
  in the serde mirror), total and exact where JSON's number coerces NaN/±∞ to
  `null`.

## Value & environment mirror — `core/src/serial.rs`

`FOValue` is the serde-round-trippable *first-order* value — data all the way
down, first-order by construction via an uninhabited-by-default extension slot
— and the engine protocol's shared value vocabulary. `SerialValue = FOValue<Closure>`
fills that slot with closures, the mirror of the runtime `Value` this wire
carries. Around it:

- `SerialLambda` / `SerialThunk` for closures, `SerialEnvSnapshot` for an `Env`;
  `SerialBinding` mirrors a scope entry — value *and* scheme — so a re-exec'd
  helper stage preserves the binding's scheme across the round-trip
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).
- An interning table, `InternCtx`, deduplicates shared scopes, so a captured
  environment with shared frames cannot unfold into an O(2^N) tree. Interning
  only *reserves* an id and queues the scope; `finish` — the table's sole
  accessor — drains the queue, so encoder stack depth is bounded by data
  nesting inside one scope, not by stream length
  ([[decisions/260806_depth-proof-env-seam|depth-proof-env-seam]]).
- `from_runtime` walks a `Value`/`Env` into its serial form against the intern
  context; the inverse rebuilds runtime values from the snapshot.

All three hand-written walks match their value type exhaustively, so a new
`Value`/`SerialValue` variant fails the build at each walk rather than being
silently treated as handle-free or dependency-free:

- `from_runtime` — the serialisation walk;
- `value_carries_handle` — the handle-sanitiser;
- `collect_scope_deps` — the dependency collector.

## The mirrored shell state — `core/src/subprocess.rs`

`serial.rs` owns value and closure transport; this module owns the surrounding
envelope — the wire mirror of the `env`/`context` fields that
cross an evaluation boundary ([[map/core/shell-state|shell-state]]). No frame
ever crosses: a stage's [[internals/evaluator-machine|machine]] starts over
the empty stack, so what rides the wire is store, never continuation. Each
`Wire*` type mirrors one subtree of the runtime tree and its conversions
compose strictly (a parent's `from_X` calls its children's, never reaching
past them):

- `WireShell { env, stack_limit, context: WireContext }` — the
  top, a serialisable mirror of a shell's mobile state. `env`'s wire row is
  only the bindings tier of one [[design/scoping|`Env`]] — the persistent map
  of everything bound since the prelude — interned by the identity of its
  root; the receiving side seats it under the receiver's own `natives` and
  `prelude`, so the two constant tiers never cross the wire at all;
- `WireContext` — the [`Context`] mirror (`env_overrides`, `dir`/`cwd`,
  `grants`, `handlers`, `args`, `modules`); `hooks` is dropped outright and
  the receiver starts with an empty table;
- `WireObservation` — the [[design/audit|audit]] trail fragment, one flat list
  with no recursion, living beside the request it rides in
  `core/src/child_eval.rs` ([[map/core/runtime|runtime]]);
- `WireHandlerFrame` — a [[internals/handler-dispatch|handler stack]] frame,
  carrying each alias arm's scheme so a re-exec'd helper stage does not strip it
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).

`install_wire_shell` reinstates a received `WireShell` into a child `Shell`,
splicing the wire's handler frames atop the receiver's own so the receiver's
own builtin table survives, never having ridden the wire.
`reexec_child_shell` is the one constructor the
[[internals/pipeline-execution|pipeline-stage helper]] — the sole re-exec'd
eval path — builds its shell through (`Shell::new` + the host's `HostSurface`
reinstalled via the child-shell-extension hook + `install_wire_shell`), so it
cannot drop the host builtins. All
conversions share the `InternCtx` from `serial.rs`.

`core/src/child_eval.rs` also carries `EngineSeed` — a forked shell reified
for a wire-seat hatch, `ChildEvalRequest`'s shape minus a body
(`scope_table`, `mobile: WireShell`, `captured: SerialEnvSnapshot`, the
spawn's validated `grant` tag). `pack_seed` builds one from a `Shell`, and
`seed_from_env` takes it before the engine waits for `Attach` — striking the env
var as it takes the fd, so no descendant inherits a number that has stopped being
one — and after `Attach` selects an installer and boots the shell, `apply_seed`
hydrates it through the same `WireDecoder::for_shell` `eval_request` already uses,
before narrowing the shell's capabilities to the seed's grant. Taking and applying
are split for one reason each: the take must not wait on the host, and the
application needs the booted installer's shell. The scope it carries is never the
parent's whole lexical scope: `Shell::fork_scrubbed` strips every
handle-carrying binding (`Value::Handle` has no wire form, `serial.rs`'s
`value_carries_handle`), and it is the one door both seats pass through, so an
in-process identity fork and a wire hatch's `EngineSeed` snapshot the same
serialisable fragment and
`` agents `start `` means one thing regardless of seat
([[design/agents|agents]]'s one-snapshot law).

## Framing codec — `core/src/subprocess_codec.rs`

`write_frame` / `read_frame` are length-prefixed JSON frames (a `u32` length
followed by the `serde_json` body). One codec carries the
[[internals/pipeline-execution|pipeline-stage helper]]'s request/response frames
— the single re-exec'd eval protocol — and the engine protocol's front-end⇄engine
`WireChannel` frames (`core/src/wire.rs`).

This layer is the mechanism behind the mobile/local split — `env` /
`context` cross a re-exec boundary, `io` / `session` / `local`
do not ([[map/core/shell-state|shell-state]]) — that the pipeline-stage helper
relies on for out-of-process stage evaluation.
