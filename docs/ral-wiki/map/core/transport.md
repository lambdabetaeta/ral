---
generated_at_commit: f9ac94f8
generated_at_date: 2026-11-12
covers_paths: [core/src/data.rs, core/src/data/, core/src/shell/runtime/value/first_order.rs, core/src/marshal.rs, core/src/marshal/, core/src/io/frame.rs, core/src/shell/grant/spawn.rs, core/src/protocol/json.rs, core/src/carrier/json.rs, exarch/src/record/json.rs, exarch/src/record/json/, exarch/src/record/log/raw.rs]
---

# Map: core / transport

**Every helper shell starts from its marshalled parent**, in process or in a
VM. `Shell::marshal` writes the parent's image and the spawn's grant as one
JSON document; `Shell::child_of` reads it against the receiving runtime
([[map/core/shell-state|shell-state]]). A pipeline stage never
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
round-trips through a string with a catch-all decode arm.** Both seats read
the same parent bytes. The discipline is mechanical — an exhaustive match makes a
new variant fail the build, a field-complete struct literal a new field:

- *value walks* (`shell/runtime/value/first_order.rs`) match `View` / `FOValue` exhaustively, once forward and once back;
- *marshalling* reconstructs a complete `HandlerFrame`, retaining its
  `FrameKind`, per-name order and catch-all; a persistent frame remains
  host-managed. A per-name entry is unary by construction and the reader
  does not re-check its arity, validated by the sender's static check or
  persistent-install vet
  ([[decisions/260619_handlers-are-lambdas|handlers-are-lambdas]]);
- *kinds* ride as serde enums (`marshal::wire::Value`, `RawSpawnGrant`), and *floats* by
  their IEEE-754 bits, total and exact where JSON's number turns NaN and ±∞
  into `null`.

## Values — `core/src/data.rs`

**`FOValue<X>` (`data.rs`, which knows no `ValClos`) is the one value
vocabulary for first-order seams. The top-level `data` module is independent of
runtime values and shared by protocols and records: first-order by
construction, over an extension slot `X` uninhabited by default (`NoExt`).**
Externally tagged on the wire, with typed accessors (`field`, `as_str`, `as_int`, …) for a host reading one.

- `data/datum.rs` — `Datum`, the one strict first-order codec every typed
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
  declarations (`ErrorRecord::ty()`). That closed record is exactly
  `[frame, message, status, site]`: a frame name, the reason's wording and
  failure code, and a total `CallSite`. The wire carries the `ErrorFact` it
  projects from instead, `{frame, reason, site}`: the typed `Reason` is `Datum`
  but never `Typed` ([[decisions/261105_the-error-record|the error record]]).
  `poll`'s polymorphic shape is
  `Io::ty().extend(..)` over them. The doors that mint them sit above
  (`shell::runtime::error`, `shell::runtime::audit`).
- `data/print.rs` — the one value printer, `pretty_print` over a `Printable`
  view (`Shape`: collection children unread; consecutive variant labels
  grouped with one optional terminal child), which `FOValue` implements here
  and the shell's `ValClos` implements in `shell/builtins/print.rs`; beneath
  `authority`, so a grant prints without the shell
  ([[invariants/core-below-shell|core-below-shell]]).
- `data/string.rs` — `Str`, the shared immutable string, exported as
  `data::Str` for IR and runtime values; `FOValue::String` owns a `String`.
- `data/byte_encoding.rs` — byte strings serialize to base64 in JSON
  and native bytes in binary codecs. Binary decoding copies or takes the
  whole byte buffer. Each decoder requires its encoding.

**One walk each way joins `ValClos` and `FOValue`
(`shell/runtime/value/first_order.rs`).** `FOValue::walk` reads `View` and
hands non-data leaves to its caller as `Leaf`; `unwalk` is the reverse walk.
`try_from` and `scrubbed` differ only in their leaf function; `From<FOValue>`
uses `unwalk`. Marshalling instead reads `ValClos::held`, preserving syntax,
captures, built collections and failure witnesses. The reverse first-order
walks use `ValClos::variant`; `View::Variant` omits resolved unit payloads.

**A leaf that is not data has three treatments, one per seam.** `ValClos::opacity`
classifies it — a block, a function, a handle:

- `TryFrom<&ValClos> for FOValue` refuses it, naming the first such leaf and
  whether it was nested (`NotData`);
- `FOValue::scrubbed` makes the conversion total by writing every such leaf as
  its `` `opaque [type: …] `` placeholder — the flat wire's treatment, taken by
  `Mooring::surface`;
- the marshaller writes handles as titles wherever the image reaches them,
  including captures, residual arguments, collection positions and handler
  arms. The reader constructs `ValClos::withheld`; observation fails
  `withheld-handle`. Closures remain executable, and hooks are absent from
  the image by construction, on both seats.

## Marshalling — `core/src/marshal.rs`, `core/src/marshal/`

**`Marshalled` is one JSON document over `Image` and `SpawnGrant`; a value
names rows rather than nesting runtime values.** `wire.rs` is the structural
mirror: `rows`, root `env`, `context`, `sources`, `terms`, `fixings`, `grant`.
One `Wire`/`Row`/`Value`/`Binding`/`Frame` family is generic over `D: IrData`:
`Closure.val` is `Val<Explicit, D>`; `Binding.scheme` is `Option<D::Scheme>`.
Writing emits `Wire<RuntimeData>` directly; structural decoding and reading
use `Wire<RawData>`, with the same JSON spelling and no intermediate admitted
wire graph. Both use the [[map/core/ir|one generic IR grammar]]: runtime leaves
are `Bytes`/`Closed`, raw leaves `String`/`Arc<Scheme>`. Representation is
independent of phase; linked modules remain `Explicit` in every outer phase.
The [[internals/evaluator-machine|machine]] starts with an empty stack:
store crosses, never continuation, hooks or terminal authority.

- `write.rs` borrows the image and visits environments and built lists/maps
  dependencies-first on an explicit Open/Close stack. A `HashMap` keys
  allocations by `Env::addr` or container address; the image keeps those
  allocations alive. Each allocation gets one row at close; ∅ names no row.
  The identity table is O(rows); each node's bindings or elements are visited
  once for dependencies and once for encoding
  ([[decisions/260806_depth-proof-env-seam|depth-proof-env-seam]]).
- **Row `i` names only rows `j < i`; the root env and handler frames may
  name any row.** `Row::{Env, List, Map}` preserves sharing within the child.
  `read.rs::Reader` checks and constructs each row in wire order. References
  must have the requested environment or built kind; `Env::from_bindings`
  and map construction refuse duplicate names or keys.
- `Value::Closure { tags, val, env }` preserves held syntax and its capture,
  without materialising literal collections. The writer peels the root's
  tag spine syntactically. Before `ValClos::closure` checks the foot's direct
  names and folds the tags back, `read/ir.rs` admits raw IR through borrowed,
  exhaustive `admit_val`, `admit_comp` and `admit_module` walks.
  `Bytes::from_base64` checks byte leaves; `Closed::new` checks every
  definition scheme in every recursively linked module. Environment binding
  schemes also close through `Closed::new`. Runtime `Closed` remains checked.
  Neither Σ tier crosses; the receiving `Runtime` supplies natives and prelude
  ([[decisions/260926_a-closure-keeps-only-what-it-mentions|a-closure-keeps-only-what-it-mentions]]).
- IR admission preserves spans, origins, sites, units, admits, annotations
  and all other fields, rebuilding nodes through their constructors under
  `on_json_stack`. Explicit primitive and redirect matches retain actual
  enum payloads, fields and tuple/sequence indices in failure paths, including
  `Spanned.item`, not `Node.shape`, `Arc` or `Option`. `Prim::Binary` operands
  occupy indices 1 and 2; the phase-only `Prim::map_operands` cannot supply
  these paths. Admission introduces no new IR validation policy.
- `Value::Builtin(name)` re-links through the receiving builtin table, one
  cached entry per name per read. `Residual { builtin, env, supplied }`
  rebuilds the receiver's η suffix through `BuiltinEntry::residual`:
  `supplied < arity`, including zero, and the row binds exactly that entry
  and its first `supplied` arguments. The row's allocation is retained;
  the rebuilt suffix has η's own spans, and β uses the argument frame's span.
- Builtins, handles and η residuals sit only at a closure's root:
  `ValClos::variant` binds such payloads to fresh names. `Val::{Builtin,
  Handle}` and `CompKind::Builtin` (δ) are serde-skipped in both directions;
  ordinary closure syntax therefore contains none. This is the writer's
  structural-serialization totality argument.
- `Value::Handle(title)` arrives withheld. `Failure(fact)` carries the
  witnessed map's `ErrorFact`, validated and rebuilt with `ValClos::failure`,
  so `raise` in the child keeps its typed reason, including under a tag.
  Unwitnessed built maps and built lists cross once as rows. Captures,
  residual chains, built nesting and tag spines cross without recursive
  runtime-value nesting; shared IR is still copied once per closure.
- The context crosses through `RawContext<Vec<wire::Frame>>`; frames retain
  arms, catch-all and kind, never handler schemes. `SourceDb` keeps each
  closure's `FileId` meaningful. `Terms` carries stack/child limits, exit
  hints, scratch, lease and retention. Fixings cross as their own snapshot,
  keyed by the parent's units rather than embedded in a `Site`.
- `marshal::write` can refuse only a non-Unicode cwd, as `Unencodable` with
  operation `Marshal`. `marshal::read` parses JSON, structurally decodes
  `Wire<RawData>`, then admits IR and checks while building. Forward or
  wrong-kind rows, unknown builtins, invalid residuals, invalid IR base64,
  open closures or schemes (including nested linked modules), duplicates,
  malformed facts, syntax and shape errors all become a `DecodeFailure`
  at the refusing JSON path, wrapped once as `ProtocolFailure::Decode {
  kind: Parent, operation: "child" }`.
- `Marshalled` is a `Datum` over `Bytes`, relayed unread by the host; JSON
  carriage uses base64. `IdentityTransport::child(&parent)` shares its
  parent's `Arc<Runtime>`; `Engine::boot` reads `Origin::Child(parent)`
  against its own runtime. Both call `Shell::child_of`, run no root recipe
  and reconstruct the same image
  ([[decisions/261013_a-child-starts-from-its-parent-marshalled|a-child-starts-from-its-parent-marshalled]]).
- The reader re-holds the inherited grant stack's exclusions. An object
  replaced between write and read arrives unheld, but its old identity and
  position still refuse. `RawSpawnGrant` (`shell/grant/spawn.rs`) validates
  to `Inherit` (⊤), `Base(name)` or `Restrict(record)`, still **unfrozen**.
  `SpawnGrant::narrow_onto` freezes against the child's own env, cwd and
  scratch and pushes one layer; `Base` uses the installer's `GrantNarrower`
  and `Restrict` uses `decode_capability_map`
  ([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]).

## Framing — `core/src/io/frame.rs`

**`encode` + `put` / `read_frame` delimit the engine protocol's `WireChannel`
frames (`core/src/protocol/channel.rs`).** `Marshalled` has no inner length
prefix: the carrying enquiry or attach owns the size limit, and in-process
marshalling is unframed. `PROTOCOL_VERSION` is 47.

- `fuse` is the one frame-size check both doors apply — `MAX_FRAME_LEN`, 256 MiB
  — before the reader allocates a body and before the writer sends one, so an
  oversized frame returns typed actual/limit counts instead of killing the peer
  mid-stream. The writer's check is `encode`'s alone: it hands back an
  `Encoded` (a fused length and its body), which `put` writes, failing only
  on I/O.
- Neither side caps depth: both encode and decode under `serde_stacker`, which
  grows the stack onto the heap, so a legal nest of any depth crosses and only
  the fuse bounds it.
- `FrameReadError` separates its portable `FrameFailure` from report-only
  `FrameDump`: either the owner-only raw-dump path or the I/O error that
  prevented preserving it. Neither dump evidence nor its rendering becomes
  the decode cause.

## Typed JSON boundaries

**Syntax, structural shape and domain validation are separate steps.**
`data::json::from_slice<T: JsonDecode>(&[u8]) -> Result<T, JsonInputFailure>`
borrows the input bytes. Failure distinguishes syntax from a typed
`DecodeFailure` with its logical field/index/tag path. `JsonDocument` retains source ranges and object entries,
so validation can see duplicate keys and original numeric spellings instead
of inferring causes from serde's diagnostic text.

- `RawFOValue` and `RawDatum<T>` retain unchecked leaves until validation.
  The raw graphs in `protocol/json.rs` and `carrier/json.rs` validate their
  domain leaves before returning usable values; a wire reason uses the same
  `Datum` as an error fact, not a second reason schema. `marshal/wire.rs`
  structurally decodes raw byte strings and schemes in the same generic IR
  grammar; `marshal/read/ir.rs` admits byte and linked-module scheme leaves
  before closure construction. `marshal/read.rs` checks closure names, closes
  environment schemes and validates residuals while building rows. A non-finite
  float is `DecodeCause::FiniteBits`. Runtime teardown of capture and built-container chains is the `ValClos` drop trampoline
  ([[map/core/shell-state|shell-state]]).
- `JsonEncode` returns `JsonEncodeFailure` before writing a frame. Audited raw
  representations serialize only after fallible leaves have been checked;
  `serialize_structural` is not a blanket encoding fallback.
- Exarch's `record/json.rs` keeps record shape separate from the log's byte
  envelope. Its `Mirror::check` precedes `hydrate`, including nested
  `RawDatum`, `RawFOValue` and `RawSevered` leaves. Hydration does not turn a
  semantic decoder's message into a reason.
- `record/log/raw.rs` borrows the commit's exact source slice. It validates
  syntax and envelope, verifies the seal over those bytes, then validates the
  records. Writing encodes the body once and hashes and writes that same
  buffer. Read-back never reserializes a parsed value to check integrity.
  `RecordLineFailure` distinguishes syntax, envelope, seal and record failures.
