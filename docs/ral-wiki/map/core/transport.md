---
generated_at_commit: 451d1ab5
generated_at_date: 2026-09-22
covers_paths: [core/src/serial.rs, core/src/serial/, core/src/subprocess.rs, core/src/subprocess_codec.rs, core/src/engine_seed.rs, core/src/spawn_grant.rs]
---

# Map: core / transport

The wire layer that carries a shell across a process boundary. The one
consumer left is the wire-seat agent hatch: when a run seats an engine on a
remote transport, a forked shell's mobile state — `env`, `context`, the
relevant parent state — is serialised to JSON, framed, and reconstituted on
the other side ([[map/core/shell-state|shell-state]]). A pipeline stage
never rides this wire — it runs on a thread of the parent process,
sharing the parent's memory directly
([[decisions/260902_stages-are-threads|stages-are-threads]]). (A
[[design/grant|grant]] does
not ride this wire either: its body evaluates locally, and external children are
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
— and the engine protocol's shared value vocabulary, externally tagged on the
wire. `SerialValue = FOValue<Closure>`
fills that slot with closures, the mirror of the runtime `Value` this wire
carries. `serial/datum.rs` types it: `Datum` (`encode`, a strict `decode`
naming what arrived ill-shaped) is the one first-order codec every typed
protocol payload goes through, with `tag`/`untag`/`field`/`exact_keys` and
the `record!` macro, which derives a strict record — exact keys, each once,
an unknown one answered with the key it most likely meant. Around it:

- `SerialLambda` / `SerialThunk` for closures, `SerialEnvSnapshot` for an `Env`;
  `SerialBinding` mirrors a scope entry — value *and* scheme — so a wire-hatched
  engine child preserves the binding's scheme across the round-trip
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
- `WireHandlerFrame` — a [[internals/handler-dispatch|handler stack]] frame,
  carrying each alias arm's scheme so a wire-hatched engine child does not strip it
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).

`install_wire_shell` reinstates a received `WireShell` into a child `Shell`,
splicing the wire's handler frames atop the receiver's own so the receiver's
own builtin table survives, never having ridden the wire: a wire-hatched
engine child installs the state onto the shell its own installer booted, so it
cannot drop the host builtins (`bare_child_shell` is the tests' stand-in for
that boot). All conversions share the `InternCtx` from `serial.rs`.

`core/src/engine_seed.rs` carries `EngineSeed` — a forked shell reified
for a wire-seat hatch (`scope_table`, `shell: WireShell`,
`captured: SerialEnvSnapshot`, and the spawn's `grant: SpawnGrant` —
`` `inherit ``, a base name, or a restriction record carried **unfrozen**, so
its sigils resolve against the child's own cwd on the far side
([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]])), the one
type in that module, since a pipeline stage never crosses a wire
([[decisions/260902_stages-are-threads|stages-are-threads]]). `pack_seed` builds one from a `Shell`, and
`seed_from_env` takes it before the engine waits for `Attach` — striking the env
var as it takes the fd, so no descendant inherits a number that has stopped being
one — and after `Attach` selects an installer and boots the shell,
`Engine::boot` hands it to `EngineSeed::apply`, which hydrates it through
`WireDecoder::for_shell` plus `install_wire_shell`, then pushes the seed's
grant as the child's one layer through `SpawnGrant::narrow_onto`. Taking and applying
are split for one reason each: the take must not wait on the host, and the
application needs the booted installer's shell. The scope it carries is never the
parent's whole lexical scope: `Shell::fork_scrubbed` strips every
handle-carrying binding (`Value::Handle` has no wire form, `serial.rs`'s
`value_carries_handle`), and it is the one door both seats pass through, so an
in-process identity fork and a wire hatch's `EngineSeed` snapshot the same
serialisable fragment and
`` exarch-agents `start `` means one thing regardless of seat
([[design/agents|agents]]'s one-snapshot law).

`core/src/spawn_grant.rs` carries `SpawnGrant`, `SpawnGrant::layer`, and
`SpawnGrant::narrow_onto` — the layer resolved against the shell's own cwd and
home and pushed as a session frame, the one step an adopted identity fork
(`IdentityTransport::adopt_parked`) and a hatched seed (`EngineSeed::apply`)
share, so neither holds a narrowing decision of its own, both under the
installer's `narrow`: `Inherit` is ⊤, `Base` reaches the host's
`GrantNarrower` (core has no base-tag lexicon), and `Restrict` walks the record
through `capability::decode_capability_map` against the child's cwd. A record
rather than a `Capabilities` is exactly what lets the freeze happen there,
keeping "every path already resolved" a construction invariant of the type the
wire never carries ([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]).

## Framing codec — `core/src/subprocess_codec.rs`

`write_frame` / `read_frame` are length-prefixed JSON frames (a `u32` length
followed by the `serde_json` body). One codec carries the wire-seat hatch's
one-shot `EngineSeed` frame and the engine protocol's front-end⇄engine
`WireChannel` frames (`core/src/wire.rs`).

`fuse` is the frame fuse both doors judge a body by — `MAX_FRAME_LEN`, 256
MiB, checked on the read side before the body is allocated and on the write
side before anything reaches the wire. One enforcement point, so an oversized
frame fails locally with a sentence instead of being written happily and then
killing the peer mid-stream.

Neither side caps depth: every frame encodes and decodes under
`serde_stacker`, which grows the stack onto the heap, so a legal nest of any
depth crosses and only the fuse bounds it.

This layer is the mechanism behind the mobile/local split — `env` /
`context` cross a re-exec boundary, `io` / `session` / `local`
do not ([[map/core/shell-state|shell-state]]) — that a wire-hatched engine
child relies on to boot from a snapshot of its parent's scope.
