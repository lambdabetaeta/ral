---
generated_at_commit: 19d53bb
generated_at_date: 2026-07-28
covers_paths: [core/src/serial.rs, core/src/subprocess.rs, core/src/subprocess_codec.rs]
---

# Map: core / transport

The wire layer that carries a shell across a process boundary. When a pipeline
stage runs in a re-exec'd helper, the [[internals/evaluator-machine|Mobile half]]
of the shell — a computation, its captured closure, the relevant parent state —
is serialised to JSON, framed, and reconstituted on the other side of a re-exec
of this [[invariants/single-binary|same binary]]. (A [[design/grant|grant]] does
not ride this wire: its body evaluates locally, and external children are
confined per-command — see
[[decisions/260617_sandbox-external-children|sandbox-external-children]].
Distinct from all of this is the crate-root `core/src/transport.rs`, the
transport-parametric *host seam* — the frame algebra between a front-end and
the engine, with `engine.rs` and the `wire.rs` socket channel —
[[decisions/260628_host-seam-transport-parametric|host-seam-transport-parametric]].)

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
- *kinds* ride as serde enums — `WireExecNode.kind` is `ExecNodeKind`, not a
  string — and *floats* ride by IEEE-754 bits (`f64::to_bits`/`from_bits` in the
  serde mirror), total and exact where JSON's number coerces NaN/±∞ to `null`.

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
  environment with shared frames cannot unfold into an O(2^N) tree.
- `from_runtime` walks a `Value`/`Env` into its serial form against the intern
  context; the inverse rebuilds runtime values from the snapshot.

All three hand-written walks match their value type exhaustively, so a new
`Value`/`SerialValue` variant fails the build at each walk rather than being
silently treated as handle-free or dependency-free:

- `from_runtime` — the serialisation walk;
- `value_carries_handle` — the handle-sanitiser;
- `collect_scope_deps` — the dependency collector.

## Mobile envelope — `core/src/subprocess.rs`

`serial.rs` owns value and closure transport; this module owns the surrounding
*mobile envelope* — the wire mirror of the `Mobile` bundle that crosses an
evaluation boundary. Each `Wire*` type mirrors one subtree of the runtime tree
and its conversions compose strictly (a parent's `from_X` calls its children's,
never reaching past them):

- `WireMobile` / `WireContext` — the top;
- `WireExecNode` — the [[design/audit|audit]] tree fragment, living beside the
  request it rides in `core/src/child_eval.rs` ([[map/core/runtime|runtime]]);
- `WireHandlerFrame` — a [[internals/handler-dispatch|handler stack]] frame,
  carrying each alias arm's scheme so a re-exec'd helper stage does not strip it
  ([[decisions/260603_session-scheme-continuity|session-scheme-continuity]]);
- `WireControl`.

`install_shell_mobile` reinstates a received mobile bundle into a child `Shell`.
`reexec_child_shell` is the one constructor the
[[internals/pipeline-execution|pipeline-stage helper]] — the sole re-exec'd
eval path — builds its shell through (`Shell::new` + the host's `HostSurface`
reinstalled via the child-shell-extension hook + `install_shell_mobile`), so it
cannot drop the host builtins. All
conversions share the `InternCtx` from `serial.rs`.

## Framing codec — `core/src/subprocess_codec.rs`

`write_frame` / `read_frame` are length-prefixed JSON frames (a `u32` length
followed by the `serde_json` body). One codec carries the
[[internals/pipeline-execution|pipeline-stage helper]]'s request/response frames
— the single re-exec'd eval protocol — and the host seam's front-end⇄engine
`WireChannel` frames (`core/src/wire.rs`).

This layer is the mechanism behind the Mobile/Local split that the
[[internals/evaluator-machine|evaluator machine]] describes and that the
pipeline-stage helper relies on for out-of-process stage evaluation.
