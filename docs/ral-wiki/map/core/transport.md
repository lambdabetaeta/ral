---
generated_at_commit: 3fcae877
generated_at_date: 2026-11-13
covers_paths: [core/src/data.rs, core/src/data/, core/src/shell/runtime/value/first_order.rs, core/src/marshal.rs, core/src/marshal/, core/src/io/frame.rs, core/src/shell/grant/spawn.rs, core/src/protocol/json.rs, core/src/carrier/json.rs, exarch/src/record/json.rs, exarch/src/record/json/, exarch/src/record/log/raw.rs]
---

# Map: core / transport

**What crosses a process or seat boundary, and how it is checked on arrival.**
Three things cross: first-order values (`FOValue`) on every open protocol
position, a marshalled parent (`Marshalled`) when a helper shell starts, and
length-prefixed frames carrying both. Pipeline stages and
[[design/grant|grant]] bodies cross nothing: stages are threads
([[decisions/260902_stages-are-threads|stages-are-threads]]) and a grant body
evaluates locally ([[decisions/260617_sandbox-external-children|sandbox-external-children]]).
The envelope is [[map/core/engine-protocol|engine-protocol]]; why the host
never reads a parent is [[design/engine-protocol|the engine protocol]].

- **Syntax, structure, semantics.** Every JSON input is parsed to a
  `JsonDocument`, decoded structurally into a raw form, then validated into
  domain values. Each refusal is a typed `DecodeFailure` at its JSON path.
- **Exhaustive both ways.** Wire kinds are serde enums matched exhaustively;
  floats cross as IEEE-754 bits, so NaN and ±∞ survive.

## Values — `core/src/data.rs`, `core/src/data/`

**`FOValue<X = NoExt>` is the one first-order vocabulary**, independent of
runtime values and shared by protocols and records. `NoExt` is uninhabited,
so a default `FOValue` is data all the way down.

- `data/datum.rs` — `Datum`, the strict codec every typed payload uses
  (`encode`, `decode`), with `tag`, `untag`, `field`, `exact_keys`. `record!`
  derives an exact-key record with canonical (sorted) encoding and a
  nearest-key hint for an unknown key; `variant!` and `label!` do the same for
  payload and bare-tag enums. With `typed`, each also implements `ty::Typed`.
  Records ral reads back live in `fact` (`ErrorRecord`, `Io`, ...), below the
  checker.
- *The error record.* `ErrorRecord` is `[frame, message, status, site]`; the
  wire carries the `ErrorFact` it projects from, `{frame, reason, site}`,
  whose `Reason` is `Datum` but not `Typed`
  ([[decisions/261105_the-error-record|the error record]]).
- `data/print.rs` — `pretty_print` over a `Printable` view, implemented by
  `FOValue` here and `ValClos` in the shell; it sits below `authority`
  ([[invariants/core-below-shell|core-below-shell]]).
- `data/string.rs` — `Str`, the shared immutable string of IR and runtime
  values. `data/byte_encoding.rs` — bytes are base64 in JSON, native in binary
  codecs.

**One walk joins `ValClos` to `FOValue`
(`shell/runtime/value/first_order.rs`).** `FOValue::walk` hands each block,
function or handle (`ValClos::opacity`) to a leaf function:

- `TryFrom<&ValClos> for FOValue` refuses it with `NotData`, naming the leaf
  and whether it was nested;
- `FOValue::scrubbed` writes it as its `` `opaque [type: …] `` placeholder
  (`Mooring::surface`);
- `From<FOValue> for ValClos` is a plain fold.

## Marshalling — `core/src/marshal.rs`, `core/src/marshal/`

**`Marshalled` is one JSON document over the parent's `Image` and the spawn's
`SpawnGrant`; a wire value names rows and never nests another wire value.**
`Shell::marshal` writes it; `Shell::child_of` reads it against the receiving
`Runtime`, constructs the session and pushes the grant's layer
([[map/core/shell-state|shell-state]]). The store crosses; the continuation,
hooks and root-only terminal authority do not.

- **Seats.** `IdentityTransport::child` shares the parent's `Arc<Runtime>`;
  `Engine::boot` reads `Origin::Child(parent)` against its own. Both call
  `Shell::child_of`, run no root recipe, and read the same bytes. exarch's
  `start` refuses `NoDesk` before marshalling.
- **Representation axis.** `wire.rs` is one family `Wire`/`Row`/`Value`/
  `Binding`/`Frame` generic over `D: IrData`. The writer emits
  `Wire<RuntimeData>` (`Bytes`, `Closed` leaves); the reader decodes
  `Wire<RawData>` (`String`, `Arc<Scheme>` leaves), with identical spelling.
  The axis exists because `decode_structural` (`data/json/de.rs`) forbids
  semantic serde validators, and `Bytes` and `Closed` validate when
  deserialized. Admission is the semantic pass
  ([[map/core/ir|one IR grammar]]).
- **`Wire`** holds `rows`, the root `env`, `context`, `sources`, `terms`,
  `fixings` and `grant`.

### Writing — `write.rs`

**One borrowed walk emits rows dependencies-first.** Nodes are allocations:
a nonempty environment (`Env::addr`; ∅ names no row), a built list, a built
map. An explicit Open/Close stack visits each once; a `HashMap` keyed by
address assigns its id at close. The borrowed image keeps every address alive
([[decisions/260806_depth-proof-env-seam|depth-proof-env-seam]]).

- `Value::Closure { tags, val, env }` — held syntax over its env row, the tag
  spine peeled syntactically into a flat list.
- `Value::Builtin(name)`, `Value::Handle(title)`.
- `Value::Residual { builtin, env, supplied }` — η of `builtin` after
  `supplied = arity − remaining` arguments, over its env row; zero is real.
- `Value::Failure(RawDatum<ErrorFact>)` — a caught failure's witness.
- `Value::Built(row)` — a built list or map, shared as the parent shared it.

**Writing is total but for the cwd.** `Val::Builtin`, `Val::Handle` and
`CompKind::Builtin` (δ) are `serde(skip)`; they occur only at a value
closure's root, never under a tag (`ValClos::variant`), and δ only inside η.
The one refusal is a non-Unicode cwd: `ProtocolFailure::Marshal`, whose
message names the path and the remedy.

### Reading — `read.rs`, `read/ir.rs`

**`marshal::read` parses, bounds depth, decodes structurally, then admits and
constructs in one pass.** Every refusal is a `DecodeFailure` at its JSON path,
wrapped once as `ProtocolFailure::Decode { kind: Parent, operation: "child" }`.

- *Depth.* `nesting` refuses a document deeper than `NESTING_LIMIT` (65 536
  levels; `TooDeep`) iteratively, before any recursive walk. Decoding and
  dropping the raw tree run on a grown stack (`STACK`, 64 MiB).
- *Rows.* Row `i` is built from rows `j < i`; then the root env and handler
  frames from any row. A bad reference is `UnresolvedRow` with a `RowDefect`:
  `Forward`, `Absent` or `WrongKind`. `Env::from_bindings` and the map row
  refuse duplicate names and keys.
- *IR admission* (`admit_val`, `admit_comp`, `admit_module`) rebuilds each
  node through its constructor, recursing into linked modules:
  - byte leaves pass `Bytes::from_base64`; `Define` and binding schemes pass
    `Closed::new` (`OpenScheme`);
  - a `Rec` index lies in its group (`RecIndex`);
  - record and map literal fields, `within` options and `grant` capabilities
    are strictly sorted, since `MapRef::get` binary-searches
    (`UnorderedField`, `DuplicateField`); update fields are distinct;
  - a `Site`'s node ids lie in range (`Site::check`, `DanglingNode`);
  - a `Define`'s schemes name exactly its pattern's names, since exports are
    read off schemes (`SchemeNames`).
- *Closures.* `ValClos::closure` refuses a foot naming what its env row does
  not bind (`OpenClosure`), then folds the tags back.
- *Builtins.* Names resolve through Σ's canonical values, `Signature::native`
  (`UnknownBuiltin`), so `BuiltinEntry::residual`'s identity check holds.
- *Residuals.* `BuiltinEntry::residual` rebuilds the receiver's η suffix when
  `supplied < arity` and the env row binds exactly that entry and its first
  `supplied` arguments; otherwise `InvalidResidual` with a `ResidualDefect`:
  `Saturated`, `EnvironmentSize`, `BuiltinUnbound`, `BuiltinForeign` or
  `ArgumentUnbound`.
- *Handles* arrive withheld (`ValClos::withheld`); observing one fails
  `withheld-handle`. *Failures* validate their `RawDatum<ErrorFact>` and
  rebuild with `ValClos::failure`, so `raise` keeps the typed reason.
- *Handler frames* keep entries, catch-all and `FrameKind`. Each arm must be a
  block or function (`Shape`) and names are distinct, since
  `HandlerEntry::ral_per_name` trusts its thunk.
- *Grant.* The inherited stack is reheld (`rehold`). `RawSpawnGrant`
  validates to `Inherit`, `Base(name)` or `Restrict(record)`, unfrozen;
  `SpawnGrant::narrow_onto` freezes it against the child and pushes one layer
  ([[decisions/260922_a-spawn-is-one-layer|a-spawn-is-one-layer]]).

## Framing — `core/src/io/frame.rs`

**`encode`, `put` and `read_frame` delimit the engine protocol's frames.**
`Marshalled` has no inner length prefix: its carrying frame bounds it, and
in-process marshalling is unframed.

- `fuse` checks `MAX_FRAME_LEN` (256 MiB) before the reader allocates and
  before the writer sends, returning typed actual/limit counts. `encode`
  yields an `Encoded`; `put` fails only on I/O.
- Frames bound length, not depth: `JsonDocument::parse` is iterative,
  encoding runs under `serde_stacker`, recursive decoding under
  `on_json_stack`.
- `FrameReadError` pairs a portable `FrameFailure` with report-only
  `FrameDump` evidence, which never becomes the cause.

## Typed JSON boundaries — `core/src/data/json.rs`

**`from_slice<T: JsonDecode>` returns `JsonInputFailure`: syntax or a typed
`DecodeFailure`.** `JsonDocument` keeps source ranges and every object entry,
so validation sees duplicate keys and numeric spellings.

- `RawFOValue` and `RawDatum<T>` hold unchecked leaves until `validate`. The
  raw graphs of `protocol/json.rs` and `carrier/json.rs` validate before
  returning values. A non-finite float is `DecodeCause::FiniteBits`.
- `serialize_structural` serves only audited raw representations whose
  fallible leaves are already checked; `JsonEncode` returns
  `JsonEncodeFailure`.
- exarch's `record/json.rs` runs `Mirror::check` before `hydrate`.
  `record/log/raw.rs` verifies the seal over the commit's exact source slice
  before validating records; `RecordLineFailure` separates syntax, envelope,
  seal and record failures.
