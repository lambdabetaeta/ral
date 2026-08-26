---
generated_at_commit: 68f1964e
generated_at_date: 2026-08-26
covers_paths: [core/src/serial.rs, core/src/subprocess.rs, core/src/subprocess_codec.rs, core/src/hatch.rs]
---

# Map: core / transport

The wire layer that carries a shell across a process boundary. When a pipeline
stage runs in a re-exec'd helper, the shell's mobile state — `env`,
`last_status`, `context`, the relevant parent state — is serialised to JSON,
framed, and reconstituted on the other side of a re-exec of this
[[invariants/single-binary|same binary]] ([[map/core/shell-state|shell-state]]).
(A [[design/grant|grant]] does
not ride this wire: its body evaluates locally, and external children are
confined per-command — see
[[decisions/260617_sandbox-external-children|sandbox-external-children]].
Distinct from all of this is the crate-root `core/src/transport.rs`, the
transport-parametric *host seam* — the frame algebra between a front-end and
the engine, with `engine.rs` and the `wire.rs` socket channel —
[[decisions/260628_host-seam-transport-parametric|host-seam-transport-parametric]].
A wire-seat child's hatch (`core/src/hatch.rs`,
[[map/exarch/agent|exarch / agent]]) sits below both seams and reuses this
one's machinery rather than the host seam's. The connection is opened from the
*host's* side: the parent binds an ephemeral guest port for one spawn's
duration and names it in its enquiry, the host dials it and writes eight
little-endian token bytes, and the listener thread checks them before handing
the connection to `hatch_over`. Core opens no socket for this — `AF_VSOCK` is a
VM concept below the language, so the bind lives in exarch
(`shell_eval/builtins/guest_port.rs`, the one endpoint exarch opens itself) and
`listen_for_hatch` is handed a listening descriptor. Nothing under
`core/src/hatch.rs` names a transport, which is also why the tests substitute a
`UnixListener` on the production path rather than beside it. Every partial
token read polls beside the wake pipe, so a peer that sends a prefix and stalls
cannot pin cancellation — though it does hold the accept loop, and so denies that
one spawn. `hatch_over` re-execs `current_exe` on the recipe the hatch carries —
`--engine` in production, one exact fixture under test, so the spawn itself never
branches on `cfg(test)` — with the dial on fd 3 and a
seed channel named by `RAL_ENGINE_SEED_FD`; the child drains the framed
`EngineSeed` before waiting for `Attach`, the parent writes it after `spawn`, and
only then sends one `HATCH_ACK` byte back. That ordering is what lets a seed
outgrow the socketpair's bounded buffer without deadlocking creation, and
`spawn_engine` takes the child's end of that pair *by value*, so no write can
precede the drop that turns a dead child into `EPIPE` rather than a blocked
writer. The write is bounded by the same stall the engine allows its own protocol
writes, and a seed that only partly crosses kills its child instead of recording
it: half a frame would leave it blocked in a read forever, and the `HATCHED`
table's sweep waits on exits, not on silence — it is `waitpid` and nothing else,
since a child closes that seed channel the moment it has hydrated and so falls
quiet long before it dies. The ack
goes out only once `spawn` has returned and the seed has crossed, so a host that
hears it has a live child holding its whole seed; the frame algebra offers no
substitute, since the host speaks first and `Attach` is its only legal opening
frame. Neither the token nor the ack is a
`Frame`, so a hatch never touches `PROTOCOL_VERSION` at all — no new frame, no
version bump. `HATCH_ACK` sits in the platform-neutral
`core/src/transport.rs` because the two ends need not share an operating
system, while spawning and seed hydration are Unix guest machinery. A seeded
child's ceiling is narrowed by the `GrantNarrower` its `EngineInstaller`
carries: core has no base-tag lexicon of its own, and a field rather than a
registered hook means an engine whose seeded children have no policy is
unrepresentable rather than merely unlikely. The seed a hatch carries is this
page's `EngineSeed`, below.)

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
— and the host seam's shared value vocabulary. `SerialValue = FOValue<Closure>`
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
envelope — the wire mirror of the `env`/`last_status`/`context` fields that
cross an evaluation boundary ([[map/core/shell-state|shell-state]]). No frame
ever crosses: a stage's [[internals/evaluator-machine|machine]] starts over
the empty stack, so what rides the wire is store, never continuation. Each
`Wire*` type mirrors one subtree of the runtime tree and its conversions
compose strictly (a parent's `from_X` calls its children's, never reaching
past them):

- `WireShell { env, last_status, stack_limit, context: WireContext }` — the
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
— the single re-exec'd eval protocol — and the host seam's front-end⇄engine
`WireChannel` frames (`core/src/wire.rs`).

This layer is the mechanism behind the mobile/local split — `env` /
`last_status` / `context` cross a re-exec boundary, `io` / `session` / `local`
do not ([[map/core/shell-state|shell-state]]) — that the pipeline-stage helper
relies on for out-of-process stage evaluation.
