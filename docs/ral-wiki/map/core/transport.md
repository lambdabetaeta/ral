---
generated_at_commit: 8aa9355b
generated_at_date: 2026-10-14
covers_paths: [core/src/first_order.rs, core/src/first_order/, core/src/types/value/first_order.rs, core/src/seed.rs, core/src/seed/, core/src/frame.rs, core/src/guard/grant.rs]
---

# Map: core / transport

The wire layer that carries a shell across a process boundary. Its one consumer
is the sealed child engine a wire-seat agent spawns: a `Bequest` — the parent's `env`, `context`,
sources, terms, fixings and the spawn's grant — is serialised to JSON, framed, and sealed into bytes, and reconstituted in a fresh
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
round-trips through a string with a catch-all decode arm.** A sealed child is
indistinguishable from an in-process fork exactly when no hop drops a field or
collapses a variant. The discipline is mechanical — an exhaustive match makes a
new variant fail the build, a field-complete struct literal a new field:

- *value walks* (`types/value/first_order.rs`) match `Value` / `FOValue` exhaustively, once forward and once back;
- *hydration* installs a complete `HandlerFrame` through
  `HandlerStack::push_frame` rather than re-deriving fields like
  its `FrameKind`, so a hydrated alias stays removable by `unalias`; a
  per-name entry is unary by construction and hydration does not re-check its
  arity, which the sender's install already validated
  ([[decisions/260619_handlers-and-aliases-are-lambdas|handlers-and-aliases-are-lambdas]]);
- *kinds* ride as serde enums (`SerialClosure`, `SpawnGrant`), and *floats* by
  their IEEE-754 bits, total and exact where JSON's number turns NaN and ±∞
  into `null`.

## Values — `core/src/first_order.rs`, `core/src/seed/table.rs`

**`FOValue<X>` (`first_order.rs`, which knows no `Value`) is the one value
vocabulary every seam speaks: first-order by construction, over an extension
slot `X` uninhabited by default (`NoExt`).** Externally tagged on the wire, with
typed accessors (`field`, `as_str`, `as_int`, …) for a host reading one.

- `SerialValue = FOValue<SerialClosure>` (`seed/table.rs`) fills the slot with what this wire
  adds: `SerialClosure::Thunk(SerialThunk)` — the closure's `comp` and a
  `SerialEnvSnapshot` naming its scope's row — and `SerialClosure::Native`,
  a builtin's name and applied arguments, re-linked against the receiver's own
  table.
- `SerialBinding` mirrors a scope entry, value *and* scheme, so a sealed child
  keeps each binding's type
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).
- `first_order/datum.rs` — `Datum`, the one strict first-order codec every typed
  protocol payload goes through (`encode`; a `decode` naming what arrived
  ill-shaped), with `tag` / `untag` / `field` / `exact_keys` and the `record!`
  macro, which derives a strict record: exact keys, each once, an unknown one
  answered with the key it most likely meant, and whose encoding is canonical
  (keys sorted). `variant!` does the same for a payload enum, `label!` for an
  enum of bare tags (kebab-case labels off `strum`'s `IntoStaticStr` and
  `VariantArray`). With `typed`, each also implements `ty::Typed`, so a record
  declared once yields both its encoding and its ral type; the
  records ral reads back live in `fact` (`ErrorRecord`, `Observation`, `Io`,
  `Receipt`, ...), below the checker, which reads their types off the
  declarations (`ErrorRecord::ty()`); `poll`'s polymorphic shape is
  `Io::ty().extend(..)` over them. The doors that mint them sit above
  (`types::error`, `types::audit`).
- `first_order/byte_encoding.rs` — byte strings serialize to base64 in JSON
  and native bytes in binary codecs. Binary decoding copies or takes the
  whole byte buffer. Each decoder requires its encoding.

**One walk each way joins `Value` and `FOValue`
(`types/value/first_order.rs`).** `FOValue::walk` folds a `Value`, handing each
leaf that is not data to the caller as a borrowed `Leaf`; `unwalk` and
`for_each_ext` are its converses. `from_runtime`, `try_from` and `scrubbed`
differ only in that leaf function, and so do `into_runtime` and `From<FOValue>`.

**A leaf that is not data has three treatments, one per seam.** `Value::opacity`
classifies it — a block, a function, a handle:

- `TryFrom<&Value> for FOValue` refuses it, naming the first such leaf and
  whether it was nested (`NotData`);
- `FOValue::scrubbed` makes the conversion total by writing every such leaf as
  its `` `opaque [type: …] `` placeholder — the flat wire's treatment, taken by
  `Mooring::surface`;
- the fork's scrub (`core/src/types/shell/scrub.rs`,
  [[map/core/shell-state|shell-state]]) replaces only `Handle`s, with the same
  placeholder, and keeps closures live, since this wire interns them. It
  reaches every handle the fork holds — in a binding's data, in every scope a
  closure captured, in a native's applied arguments, in a handler frame's
  arms — and empties the fork's hooks.

## Scopes — `InternCtx` and `WireDecoder`

**Environments cross as rows of a table, one per distinct allocation, by
`Env::ptr_eq` identity** — so a captured environment with shared structure
cannot unfold into an O(2^N) tree. A closure keeps only the bindings it
mentions, so rows are per closure and small: every empty capture is one row,
a `Rec` group's siblings share one, and the linear root scan makes interning
O(roots²). A value two closures mention is written in both rows
([[decisions/260926_a-closure-keeps-only-what-it-mentions|a-closure-keeps-only-what-it-mentions]]).
Σ never crosses: a row is ρ alone, and the receiving shell's own natives and
prelude answer for a decoded closure exactly as `crate::types::lookup` reads
them for any other, through the receiver's `Signature`.

- `InternCtx::intern_env` only *reserves* a row and queues the environment;
  `finish`, the table's sole accessor, encodes the queue as a worklist, so
  encoder stack depth is bounded by data nesting within one environment,
  never by the length of a chain of closures
  ([[decisions/260806_depth-proof-env-seam|depth-proof-env-seam]]). `finish`
  encodes every binding: every path onto this wire is a scrubbed fork, so a
  handle reaching `SerialValue::from_runtime` is a fault in ral, and its
  error says so.
- `WireDecoder::for_runtime` rebuilds the rows in dependency order by Kahn's
  algorithm over `collect_scope_deps`' edges, so a stream chain decodes in
  linear time, refusing an out-of-range reference or a cycle. It
  carries the receiver's builtin manifest, for a decoded `Native` to re-link
  its name against — nothing else, since neither Σ tier rides the wire.
- `SerialEnvSnapshot::into_runtime`, given a `WireDecoder`, is the sole
  wire→runtime conversion of an environment.

## The seed — `core/src/seed.rs`, `core/src/guard/grant.rs`

**`EngineSeed` is a `Bequest` in wire form, and a codec and nothing else:
`scope_table`, `env`, `context`, the parent's `sources`, `terms`, `fixings` and
the spawn's `grant`.** No frame crosses: a
sealed child's [[internals/evaluator-machine|machine]] starts over the empty
stack, so what rides is store, never continuation. The environment rides once,
as the row of one [[design/scoping|`Env`]], ρ alone; the context is
`Context<Vec<WireHandlerFrame>>`, the live `Context` with its handler stack
swapped for interned frames (`Context::try_map_handlers`), so nothing mirrors
a field and every other field crosses as itself — `env_overrides`, `cwd`,
`grants`, `args`, `modules`. The fixings ride as the session's table of
`(UnitId, var, Fixed)` triples, never inside a `Site`, which names only its
unit: a shipped closure's sites name the parent's units, so their entries
govern them in the child as in the parent. The `SourceDb` rides whole, so a
parent closure's `FileId` resolves in the child to the file it named, and
`terms` is the same `Terms` type the constructor destructures, so a term added
to it rides or the build fails. A bequest cannot express a hook, and none
rides.
`WireHandlerFrame` is a [[internals/handler-dispatch|handler stack]] frame,
each alias arm with its scheme
([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]).

- `EngineSeed::encode(&Bequest)` builds one from a bequest `Shell::bequeath`
  produced — the package both seats take, so an identity fork and a sealed child
  snapshot the same fragment and `` exarch-agents `start `` means one thing
  regardless of seat ([[design/agents|agents]]'s one-snapshot law). `encode`
  trusts that scrub rather than re-checking it. `decode(self, &Runtime)`
  returns the `Bequest`, decoding everything through one `WireDecoder` built
  `for_runtime`: constructible only from the `Runtime` whose builtin table
  re-links the natives by name, so no call site picks a manifest of its own.
  The decoded bequest goes to `Shell::child_of`, which pushes the grant
  exactly as for a parked one; `bequeath` has already snapshotted the fixings
  and construction takes ownership of them, and a sealed child runs no recipe.
- `seal(&Bequest) -> Result<Sealed, String>` encodes and frames one
  `EngineSeed` into `Sealed`, bytes the host relays and never reads (the
  opacity law, [[decisions/261013_the-seed-rides-the-protocol-sealed|the-seed-rides-the-protocol-sealed]]);
  `Sealed::unseal` is `pub(crate)`, so only an engine can read one. `Sealed`
  is a `Datum` over `Bytes`, which is how a seal rides the enquiry as
  `FOValue::Bytes`, and is `Origin::Child`'s payload. `Engine::boot` unseals it
  against the runtime it has just derived. `Bytes` serialises as base64 under a
  human-readable serializer (`first_order/byte_encoding.rs`), so a seal also
  crosses JSON.
- `SpawnGrant` — `Inherit` (⊤), `Base(name)`, or `Restrict(record)` — crosses
  **unfrozen**: a `` `cwd `` place in a grant names the cwd of the shell it
  governs, so the freeze happens on the child's side. `SpawnGrant::narrow_onto`
  resolves it against that shell's cwd and home and pushes one session layer —
  the step `Shell::child_of` takes for an adopted identity fork
  (`IdentityTransport::adopt_parked`) and a sealed child alike.
  `Base` reaches the host's `GrantNarrower`, since core has
  no base-tag lexicon; `Restrict` goes through
  `guard::decode_capability_map`
  ([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]).

## Framing — `core/src/frame.rs`

**`encode` + `put` / `read_frame` are length-prefixed JSON frames, carrying both
the seal's one `EngineSeed` frame and the engine protocol's `WireChannel`
frames (`core/src/protocol/channel.rs`).**

- `fuse` is the one frame-size check both doors apply — `MAX_FRAME_LEN`, 256 MiB
  — before the reader allocates a body and before the writer sends one, so an
  oversized frame fails locally with a sentence instead of killing the peer
  mid-stream. The writer's check is `encode`'s alone: it hands back an
  `Encoded` (a fused length and its body), which `put` writes, failing only
  on I/O.
- Neither side caps depth: both encode and decode under `serde_stacker`, which
  grows the stack onto the heap, so a legal nest of any depth crosses and only
  the fuse bounds it.
- A body that fails to decode is dumped to an owner-only file, named in the
  error.
