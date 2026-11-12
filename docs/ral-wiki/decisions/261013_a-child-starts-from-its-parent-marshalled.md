---
status: accepted
generated_at_commit: f9ac94f8
generated_at_date: 2026-11-12
verified_at_commit: f9ac94f8
verified_at_date: 2026-11-12
anchors: [Marshalled, Shell::marshal, Shell::child_of, BuiltinEntry::residual]
amended_by: "[[261112_runtime-values-are-value-closures|runtime values are value closures]]"
---

# A child starts from its parent, marshalled

**Every helper agent's shell starts from `Marshalled` bytes, on either seat;
the host relays them unread, and the spawner makes the interpreting engines
one binary.** `Shell::marshal` writes the parent's `Image` and `SpawnGrant`;
`Shell::child_of` reads them against the receiving `Runtime`, then constructs
the child. The codec does not construct sessions.

This decision incorporates
[[261112_runtime-values-are-value-closures|runtime values are value closures]]
and the 2026-11-12 marshalling decisions below.

## Opacity and the factory

**The first-order law governs positions the host reads.** `Marshalled` is
one JSON document carried as bytes: the `start` enquiry names it `parent`,
and a child attach carries `Origin::Child(Marshalled)`. No host interprets
its contents. Reading belongs to core, against the receiving builtin table.

- **One installed binary.** The guest factory spawns every engine from one
  path on the read-only rootfs. Parent and child therefore agree structurally;
  a build-id inside the document would check what the spawner guarantees.
  Host/guest protocol skew is instead gated by `PROTOCOL_VERSION` at attach.
- **Every wire engine starts from an `Attach`.** Its `EngineSpec` names the
  engine; `Origin::Root` runs the root recipe, while `Origin::Child(parent)`
  calls `Shell::child_of` without a root recipe. The child's cwd, environment
  and session state come from its parent, not duplicate root fields.
- **The host makes engines; the daemon is the factory.** The builtin marshals
  and enquires. The wire desk dials its `Factory`, attaches with the parent's
  engine spec and marshalled parent, waits for `Attached`, then answers.
  `MachineFactory` uses `Machine::connect_guest`; the daemon listens on
  `FACTORY_PORT` for the VM's life and spawns one engine per connection.
  The root dials out as its readiness signal; the host dials children in.
- **Admission is the peer's CID.** The daemon accepts only `VMADDR_CID_HOST`;
  the guest kernel supplies the source CID, so a guest process cannot claim it.
- **Scratch belongs to the host.** The root attach supplies `$EXARCH_SCRATCH`;
  a child inherits its spelling and terms. No engine mints a scratch or moves
  its lock across a process boundary. Tool homes are shared within the agent
  tree, `/tmp` on the guest: agents in that tree trust one another. A narrowed
  child may plant tool configuration a wider relative later executes; grants
  bound access to the user's world, not trust within the tree.
- **The connection owns a wire engine's life.** EOF on fd 3 ends it; `halt`
  signals every engine's process group. An engine owns no child-engine processes.

## One route (D1, D8, D9)

**`IdentityTransport::child` and `Engine::boot` both call `Shell::child_of`.**
An in-process child shares the parent's `Runtime` (builtin table and prelude),
not its live worker capabilities. Immutable values cannot distinguish copying
from sharing; agents communicate through the desk (`message`, `reply`).

- In-process spawning pays one JSON encode/decode of the image. Code held by
  several closures is copied once per closure; code and strings have no sharing
  guarantee across this boundary.
- Both seats re-hold the grant stack's exclusions when reading. An object
  replaced between writing and reading arrives unheld, but its old identity
  and position still refuse. The new grant remains unfrozen until construction
  resolves it against the child's cwd and pushes its layer last.
- Both seats refuse a non-Unicode cwd on writing, and a builtin value absent
  from the receiving runtime on reading. The latter can arise from a host test
  binding an entry outside the runtime table.
- The vocabulary is *marshal*, `Marshalled`, and `parent`: `Start { parent, .. }`
  and `Origin::Child(Marshalled)`. It names no alternate transfer route.
  Other senses remain: boot/checker and prompt/replay seeding, `own`'s seed,
  scrollback and mail seeding, record/live-page/memfd/provider-request seals,
  the lease's baseline seal, credential/data scrubs, and exarch's and Trio's
  nurseries.
- `fork_then_enquire` refuses `NoDesk` before marshalling, using the same
  `no_desk` wording as `Shell::enquire`. There is no separate transfer facility
  whose absence needs a refusal.

## Rows, one walk each way (D2, D3)

**A row names only earlier rows; the root environment and handler frames may
name any row.** One structural serde mirror, `Wire`, holds rows, root env,
context, sources, terms, fixings and grant. Semantic checking belongs outside
serde, in the reader that constructs runtime values.

- **Nodes are allocations.** An environment, built list or built map is one
  row. `Env::addr` identifies a nonempty environment; ∅ names no row. A built
  container's address identifies it. The borrowed image owns every reached
  allocation throughout writing, so addresses cannot be reused mid-walk.
- **Dependencies first.** One explicit Open/Close stack walks the graph; one
  identity `HashMap` deduplicates nodes and assigns ids when they close. Each
  node expands once, with O(rows) identity bookkeeping. No separate handle
  scrub or ordering recovery is needed.
- **Read and check together.** Row i is built from rows 0..i, then the root
  env and handler frames from all rows. Each step returns `Result`; there is
  no validation-then-`expect`, topological sort or cycle error. The reference
  law makes a cycle unwritable.
- **Values cross as held.** Syntactic data stays syntax over its environment;
  built collections stay built rows, shared wherever the parent shared them.
  No literal is materialised. The session row holds no prelude binding.

## IR boundary amendment — 2026-11-12

**One IR grammar has independent phase and representation axes.** `IrData`
selects byte and scheme leaves: `RuntimeData` uses `Bytes` and `Closed`;
marshal-private `RawData` uses `String` and `Arc<Scheme>`. `Phase`, `Serial`,
`Explicit` and `Inferring` keep their meanings. Linked modules remain
`Explicit` independently of the containing tree's checking phase.

- One generic `Wire`/`Row`/`Value`/`Binding`/`Frame` family serves both sides:
  the writer emits `Wire<RuntimeData>` directly; structural decoding and the
  reader use `Wire<RawData>`, with identical JSON spelling. There is no second
  IR grammar or intermediate admitted wire graph.
- `marshal/read/ir.rs` admits borrowed raw IR exhaustively before
  `ValClos::closure`: `Bytes::from_base64` checks byte leaves and `Closed::new`
  checks every definition scheme in every recursively linked module.
  Runtime `Closed` is not weakened, nor is a new IR validation policy added.
- Admission preserves every field and rebuilds nodes through their
  constructors under `on_json_stack`. Primitive and redirect forms are
  matched explicitly; `Prim::map_operands` remains an infallible phase mapper
  retaining the representation axis, not a wire-path traversal.
- Failures retain actual enum payloads, fields and tuple/sequence indices,
  including `Spanned.item`, without `Node.shape`, `Arc` or `Option`
  components. `Prim::Binary` operands occupy indices 1 and 2. The one
  protocol wrapper remains unchanged.

## A value holds no value (D4–D6, D10)

**Wire values name rows rather than nesting other wire values.** Only IR
remains nested; tag spines are flat lists. The vocabulary is:

- `Closure { tags, val, env }`: ⟨V, ρ⟩ with outermost tag first and the spine's
  syntactic foot V. The writer never follows ρ to peel a spine; the reader
  folds the tags back. Environments may break a spine without nesting the wire.
- `Builtin(name)`: the receiving runtime re-links the name to its entry.
- `Residual { builtin, env, supplied }`: the receiving entry's η suffix over
  an environment row, not δ syntax transported from the parent.
- `Handle(title)`: every live handle arrives withheld, retaining its binding,
  position and title, including inside captures and handler arms.
- `Failure(fact)`: a caught record's `ErrorFact` crosses intact, then
  `ValClos::failure` reconstructs its projection. Re-raising in the child
  keeps the typed reason, including beneath a tag. An unwitnessed record is
  an ordinary built map.
- `Built(row)`: a built list or map row, with its sharing preserved.

**Builtins, handles and η residuals occur only at a value closure's root.**
`ValClos::variant` binds such a payload to a fresh name, as for built data.
No tag spine ends in one. `Val::Builtin`, `Val::Handle` and
`CompKind::Builtin` skip serde in both directions. δ occurs only in η;
its suffixes are residuals over an environment binding their entry. Thus
writing an ordinary V is total, and a non-Unicode cwd is the writer's only
failure.

**A residual is an entry, an environment and a count (D5).**
`ValClos::residual` recognises it; writing records
`supplied = arity − remaining`. Zero is real: `let g = has` boxes η itself.
`BuiltinEntry::residual` rebuilds the receiver's suffix only when
`supplied < arity` and the row binds exactly δ's builtin (that entry) and
its first `supplied` arguments. Restriction is then the identity, preserving
the row's allocation. The rebuilt suffix has η's unspanned syntax; the
parent's root λ may carry the call span, but β reads the argument frame's
span, so behaviour agrees.

## Framing and refusals (D7, D11)

**`Marshalled` is the JSON bytes, not an inner length-prefixed frame.** The
carrying enquiry or attach owns its size limit and refuses an oversized
frame. In process there is no carrying frame. `PROTOCOL_VERSION` is 47:
the `start` record and fact vocabulary change together.

Every read refusal is a `DecodeFailure` at its JSON path, wrapped exactly
once as `ProtocolFailure::Decode { kind: Parent, operation: "child" }`:

- forward, out-of-range or wrong-kind row;
- unknown builtin or invalid residual;
- invalid base64 in IR, open closure or open scheme (including linked modules);
- duplicate binding or map key;
- malformed fact, structural mismatch or JSON syntax error.

The path names the binding; no extra binding-context wrapper is needed.
`RawContext::validate_with`'s handler adapter returns that same decode shape.

## Where

- `core/src/marshal.rs`, `marshal/{wire,write,read}.rs`: `Marshalled`, the
  generic structural mirror and the two graph walks; `marshal/read/ir.rs`:
  exhaustive, path-preserving IR admission.
- `core/src/shell/runtime/state/construct.rs`: `Shell::marshal`,
  `Shell::child_of`, the single session constructor.
- `core/src/builtin.rs`: `BuiltinEntry::residual`.
- `core/src/carrier/identity.rs`, `core/src/engine.rs`: the two seats' child
  entry points; `core/src/protocol.rs`: `Origin::Child` and `Attach::child`.
- `exarch/src/shell_eval/builtins/harness.rs`: `fork_then_enquire`;
  `exarch/src/agent/desk/agents.rs`: identity child or factory attach.
- `exarch/src/agent/factory.rs`, `synod/src/assistant/guest.rs`,
  `ral-guest/src/init/fleet.rs`: the factory and the one installed engine path.
