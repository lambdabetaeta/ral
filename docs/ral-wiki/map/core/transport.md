---
generated_at_commit: 5803377b
generated_at_date: 2026-09-24
covers_paths: [core/src/serial.rs, core/src/serial/, core/src/subprocess.rs, core/src/subprocess_codec.rs, core/src/engine_seed.rs, core/src/spawn_grant.rs]
---

# Map: core / transport

The wire layer that carries a shell across a process boundary. Its one consumer
is the wire-seat agent hatch: a forked shell's mobile state — `env`, `context`,
the spawn's grant — is serialised to JSON, framed, and reconstituted in a fresh
engine process ([[map/core/shell-state|shell-state]]). A pipeline stage never
rides it — stages are threads sharing the parent's memory
([[decisions/260902_stages-are-threads|stages-are-threads]]) — and neither does
a [[design/grant|grant]] body, which evaluates locally while its external
children are confined per command
([[decisions/260617_sandbox-external-children|sandbox-external-children]]). The
front-end⇄engine protocol is a separate wire,
[[map/core/engine-protocol|engine-protocol]], though it shares this layer's
value vocabulary and codec.

**Every wire↔runtime hop is an exhaustive, field-complete map: no hop passes
through a constructor that defaults a field the wire carries, and no kind
round-trips through a string with a catch-all decode arm.** A hatched child is
indistinguishable from an in-process fork exactly when no hop drops a field or
collapses a variant. The discipline is mechanical — an exhaustive match makes a
new variant fail the build, a field-complete struct literal a new field:

- *value walks* (`serial.rs`) match `Value` / `SerialValue` exhaustively (one
  exception, below);
- *hydration* installs a complete `HandlerFrame` through
  `HandlerStack::push_frame` rather than re-deriving fields like
  `removable_by_unalias`, so a hydrated alias stays removable by `unalias`; a
  per-name entry is unary by construction and hydration does not re-check its
  arity, which the sender's install already validated
  ([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]);
- *kinds* ride as serde enums (`SerialClosure`, `SpawnGrant`), and *floats* by
  their IEEE-754 bits, total and exact where JSON's number turns NaN and ±∞
  into `null`.

## Values — `core/src/serial.rs`

**`FOValue<X>` is the one value vocabulary every seam speaks: first-order by
construction, over an extension slot `X` uninhabited by default (`NoExt`).**
Externally tagged on the wire, with typed accessors (`field`, `as_str`,
`as_int`, …) for a host reading one.

- `SerialValue = FOValue<SerialClosure>` fills the slot with what this wire
  adds: `SerialClosure::Thunk(SerialThunk)` — the closure's `comp` and a
  `SerialEnvSnapshot` naming its scope's row — and `SerialClosure::Native`,
  a builtin's name and applied arguments, re-linked against the receiver's own
  table.
- `SerialBinding` mirrors a scope entry, value *and* scheme, so a hatched child
  keeps each binding's type
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).
- `serial/datum.rs` — `Datum`, the one strict first-order codec every typed
  protocol payload goes through (`encode`; a `decode` naming what arrived
  ill-shaped), with `tag` / `untag` / `field` / `exact_keys` and the `record!`
  macro, which derives a strict record: exact keys, each once, an unknown one
  answered with the key it most likely meant.

**A leaf that is not data has three treatments, one per seam.** `Opaque::of`
classifies it — a block, a function, a handle:

- `TryFrom<&Value> for FOValue` refuses it, naming the first such leaf and
  whether it was nested (`NotData`);
- `FOValue::scrubbed` makes the conversion total by writing every such leaf as
  its `` `opaque [type: …] `` placeholder — the flat wire's treatment, taken by
  `Mooring::surface`;
- `scrub_handles` replaces only `Handle`s and keeps closures live, since this
  wire interns them — the fork's treatment, applied to the session scope's
  bindings alone. It is the one walk that is not exhaustive: it descends lists,
  maps and variant payloads and passes every other value through unchanged, so
  a handle in a closure's captured scope or a native's applied arguments
  survives it, as does one in a handler arm, which the fork's context carries
  unscrubbed.

## Scopes — `InternCtx` and `WireDecoder`

**Scopes cross as rows of a table, one per distinct session-tier root, by
`imbl` `ptr_eq` identity** — so a captured environment with shared structure
cannot unfold into an O(2^N) tree.

- `InternCtx::intern_env` only *reserves* a row and queues the scope; `finish`,
  the table's sole accessor, encodes the queue as a worklist, so encoder stack
  depth is bounded by data nesting within one scope, never by the length of a
  chain of closures ([[decisions/260806_depth-proof-env-seam|depth-proof-env-seam]]).
  `finish` drops any binding whose value carries a handle
  (`value_carries_handle`), so the name arrives unbound.
- `WireDecoder::for_shell` rebuilds the rows in dependency order
  (`collect_scope_deps`), refusing an out-of-range reference or a cycle, and
  seats each under the *receiver's* natives and prelude: those two constant
  tiers never cross.
- `SerialEnvSnapshot::into_runtime`, given a `WireDecoder`, is the sole
  wire→runtime conversion of a scope.

## The mirrored shell — `core/src/subprocess.rs`

**`serial.rs` carries values and scopes; this module carries the envelope
around them.** No frame crosses: a hatched engine's
[[internals/evaluator-machine|machine]] starts over the empty stack, so what
rides is store, never continuation. Each `Wire*` type mirrors one subtree of the
runtime tree, and a parent's `from_runtime` calls only its children's:

- `WireShell { env, stack_limit, context }` — `env` is the row of one
  [[design/scoping|`Env`]]'s session tier;
- `WireContext` mirrors `Context` — `env_overrides`, `dir`, `cwd`, `grants`,
  `handlers`, `args`, `modules`; `hooks` stays behind and the receiver starts
  with an empty table;
- `WireHandlerFrame` — a [[internals/handler-dispatch|handler stack]] frame,
  each alias arm with its scheme
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).

`install_wire_shell` splices the wire's handler frames atop the receiver's own,
so the builtin table the child's installer booted survives, never having ridden
the wire (`bare_child_shell` is the tests' stand-in for that boot).

## The seed — `core/src/engine_seed.rs`, `core/src/spawn_grant.rs`

**`EngineSeed` is a forked shell reified for a hatch: `scope_table`, `shell`,
`captured`, and the spawn's `grant`.**

- `pack_seed` builds one from a shell that `Shell::fork_scrubbed` produced — the
  fork both seats take, so an identity fork and a hatch snapshot the same
  fragment and `` exarch-agents `start `` means one thing regardless of seat
  ([[design/agents|agents]]'s one-snapshot law). That fork replaces each handle
  in a session binding with its placeholder (`Env::scrub_handles`, through
  `scrub_handles` above).
- `seed_from_env` (in `hatch.rs`) takes the seed before the engine waits for
  `Attach`, striking the fd's env var as it takes the fd; after `Attach`,
  `Engine::boot` hands it to `EngineSeed::apply`, which hydrates through
  `WireDecoder::for_shell` and `install_wire_shell`, then pushes the grant. The
  take must not wait on the host; the application needs the booted installer's
  shell.
- `SpawnGrant` — `Inherit` (⊤), `Base(name)`, or `Restrict(record)` — crosses
  **unfrozen**: a `cwd:` sigil in a grant names the cwd of the shell it
  governs, so the freeze happens on the child's side. `SpawnGrant::narrow_onto`
  resolves it against that shell's cwd and home and pushes one session layer —
  the step an adopted identity fork (`IdentityTransport::adopt_parked`) and a
  hatched seed share. `Base` reaches the host's `GrantNarrower`, since core has
  no base-tag lexicon; `Restrict` goes through
  `capability::decode_capability_map`
  ([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]).

## Framing — `core/src/subprocess_codec.rs`

**`write_frame` / `read_frame` are length-prefixed JSON frames, carrying both
the hatch's one `EngineSeed` frame and the engine protocol's `WireChannel`
frames (`core/src/wire.rs`).**

- `fuse` is the one frame-size check both doors apply — `MAX_FRAME_LEN`, 256 MiB
  — before the reader allocates a body and before the writer sends one, so an
  oversized frame fails locally with a sentence instead of killing the peer
  mid-stream.
- Neither side caps depth: both encode and decode under `serde_stacker`, which
  grows the stack onto the heap, so a legal nest of any depth crosses and only
  the fuse bounds it.
- A body that fails to decode is dumped to an owner-only file, named in the
  error.
