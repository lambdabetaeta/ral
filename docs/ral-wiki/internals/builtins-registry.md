---
verified_at_commit: 9a0a1136
verified_at_date: 2026-08-06
anchors: [builtin_registry, CORE_BUILTINS, WATCH_BUILTIN, BuiltinEntry, fixed_arity, native_value, seed_natives_and_base, coreutils_invoke]
---

# Builtins: structure and registration

A builtin is a Rust atom the shell keeps in-process. `core/src/builtins/` defines
them; the question this page answers is how one declaration stays coherent
across the typechecker and the evaluator.

**One macro binds the facets.** `builtin_registry!` (in `builtins.rs`) declares
each builtin as one row, collapsed into the `CORE_BUILTINS` static
(`&[BuiltinEntry]`):

- names;
- doc line;
- [[internals/type-inference|type rule]] — a command `Sig` or a `Scheme`;
- runtime body.

Because the facets are one entry, they cannot drift apart. Arity is not among
them: `BuiltinEntry::fixed_arity` *derives* it from the type rule — a
signature's argv shape, a scheme's curry-spine depth — so the arity the checker
enforces is the arity the body assumes with nothing to keep in agreement
([[invariants/fixed-arity|fixed-arity]]). Bodies are grouped by concern
(`strings.rs`, `collections.rs`, `predicates.rs`, `fs.rs`, `codecs.rs`,
`concurrency.rs` for `spawn` / `watch`, `modules.rs` for `use`, …).

**The manifest is a boot manifest; it is not a resolution layer.** Installing a
set seeds two places at once, partitioned by the derived arity
(`seed_natives_and_base`, with `native_value` the single classifying door shared
with wire hydration): a fixed-arity entry becomes a `Value::Native` in the
shell's *base scope*, reached as an ordinary binding; an open-argv one becomes a
*base frame* under the handler stack's run frames
([[internals/handler-dispatch|handler-dispatch]]). Dispatch consults no builtin
table ([[decisions/260801_a-name-is-a-value-or-it-is-handled|a-name-is-a-value-or-it-is-handled]]);
the installed table remains as the manifest that the checker's rule side-table,
`help`/`explain`, tab completion, and wire re-linking are read from. The base scope is
reached by lookup after the user scopes and enumerated by no harvest — the
binding harvests walk user scopes only, which is what keeps a pristine native
typed by its `Sig` rule rather than as a binding, and its head a command rather
than an application. `true` and `false` join it as language constants.

**Bundled coreutils are not builtins.** They are exec images, so they live
outside the manifest module: `core/src/uutils.rs` declares the vendored tools via
`declare_coreutils!` as two `cfg`-gated lists — `cross` (always on) and `unix`
(Unix-only) — emitting one `COREUTILS_TOOLS` slice and a `coreutils_invoke`
dispatcher; `RIPGREP_TOOLS` routes `rg` through `ral-ripgrep-core`. A bundled
head resolves to an `ExecImage::BundledTool` and is always born as a
`ral --ral-bundled-tool <tool>` child, so it carries ordinary process semantics
and the same capability chokepoint as a host external
([[decisions/260731_bundled-tools-always-reexec|bundled-tools-always-reexec]]) —
which is what lets ral be a [[invariants/single-binary|single binary]] with no
sibling helpers.

**Host layers register their own, above core.** The [[map/repl|REPL]] adds the
`_ed-*` editor builtins and exarch adds its resident host atoms; both sit *above*
core and core never inspects them
([[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]), and
they seed through the same partition, so a host contributes natives as well as
base frames: the REPL's `jobs`/`fg`/`bg`/`disown` are natives, `detach` a base
frame on a host that arms the policy. Three core-implemented builtins register
this way too — `WATCH_BUILTIN`, `SERVICE_BUILTIN`, and `DETACH_BUILTIN`, each a
public one-entry wrapper over a private body — so a host whose streams are
capture buffers, or whose leases reap ordinary workers, simply lacks the verb it
cannot honour
([[decisions/260617_watch-repl-builtin|watch-repl-builtin]],
[[map/core/builtins|builtins]]).

Why these primitives exist and how the set is shaped is
[[design/builtins|builtins]]; which layer any given capability lands in is
[[design/name-resolution|name-resolution]]. See also map
[[map/core/builtins|builtins]]. `docs/SPEC.md` §14, §16.7.
