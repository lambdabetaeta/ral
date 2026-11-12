---
verified_at_commit: c0905f5e
verified_at_date: 2026-11-12
anchors: [builtin_registry, CORE_BUILTINS, WATCH_BUILTIN, BuiltinEntry, BuiltinEntry::eta, ValClos::builtin, ValClos::residual, CompKind::Builtin, fixed_arity, install_set, coreutils_invoke, Convention, Arm::ty, handlers_ty, Decl::scheme]
---

# Builtins: structure and registration

A builtin is a Rust atom the shell keeps in-process. `core/src/shell/builtins/` defines
them; the question this page answers is how one declaration stays coherent
across the typechecker and the evaluator.

**One macro binds the facets.** `builtin_registry!` (in `shell/builtins.rs`) declares
each builtin as one row, collapsed into the `CORE_BUILTINS` static
(`&[BuiltinEntry]`):

- names;
- doc line;
- [[internals/type-inference|type rule]] — a scheme factory, and nothing else;
- an optional diagnostic facet — what a misuse this verb has a name for earns:
  a decoder's argument;
- runtime body.

Because the facets are one entry, they cannot drift apart. Arity is not among
them: `Decl::fixed_arity` (the entry's checker-facing half) *derives* it from the
value row's type rule — the curry depth of the closed scheme — so the arity
the checker enforces is the arity the body assumes with nothing to keep in
agreement ([[invariants/fixed-arity|fixed-arity]]). The cached `Spine` also
records whether the result settles at `Unit`; it feeds diagnostics and the
evaluator's application gate. An argv row has no type rule or application
arity: `Convention::Argv` fixes `[String] → Command`, using the same `Arm::ty`
as named handlers. `Convention::Value(rule)` alone carries a scheme factory.

The diagnostic facet is deliberately not a typing rule. It says what a wrong
call *means* for this verb, which no polytype can state — that an argument
written to a decoder is a misreading of where the bytes come from.
A caught record passed to `fail` instead earns generic shape guidance from
`shape_hint`, recognising the closed `ErrorRecord` shape by field labels and
types, including `site`, regardless of row order. It points to `raise $e` or
`$e[message]`.
Everything a type can state is in the scheme — including whether a row writes,
which is its result grade (`Command`), not a facet ([[design/capture|grades and capture]]). `Decl::scheme` freshens
ordinary result grades; command, boundary and already graded results retain
their declared grades. Bodies are grouped by concern
(`strings.rs`, `collections.rs`, `predicates.rs`, `fs.rs`, `codecs.rs`,
`concurrency/` for the eliminators, `first-settled`, `workers`, `worker` and
`watch` (`spawn` and `gather` are control operators, `par` prelude code), `misc.rs` for
the ones with no cluster — `fail`, `exit`, `surface`, `warn`, `ask` — …).

**The core boundary table has five entries:** `from-json`, `from-jsonl`,
`from-json-at`, `from-csv` and `env`. Their bodies receive the checked call's
site. `use` belongs to syntax and the shell IR: elaboration links a checked
module, whose public schemes type the import. No manifest entry, boundary
site or runtime builtin implements it.

**The two halves are the two argument conventions, and the choice is visible in
the surface.** `echo` is a base frame because a diagnostic-or-payload line takes
an argv; `warn` is a table entry because it takes one `String`. So `$warn` is a
value and `$echo` is not, and `warn` curries where `echo` spreads
([[decisions/260819_diagnostics-are-a-builtin|diagnostics-are-a-builtin]]).

**The manifest is a boot manifest; it is not a resolution layer.** It is
authored as two, so installing a set is two installs rather than one sorting
(`Runtime::install_set`): a table entry becomes a `ValClos::builtin` in
Σ, reached by ordinary lookup after ρ and the frozen prelude; a base-frame
row becomes a *base frame* under the handler stack's run frames ([[internals/handler-dispatch|handler-dispatch]], [[design/exec|exec]]).
Dispatch consults no builtin table ([[design/builtins|builtins]]);
the installed table remains as the manifest that the checker's rule side-table,
`help`/`explain`, tab completion, and wire re-linking read. Binding harvests
enumerate ρ and the prelude, not the builtin tier; the checker reads an
unshadowed builtin's scheme from its manifest. `true` and `false` join Σ
as language constants.

**A builtin's value form is `⟨Val::Builtin(entry),∅⟩`.** The public
`ValClos::builtin` serves installation, hosts and wire hydration.
`BuiltinEntry::eta` caches `λa₀.…λaₙ₋₁.δ(b; a⃗)`; forcing binds `b` to the
entry itself. β binds arguments and δ reads them from ρ, preserving the
entry's identity rather than re-dispatching its name. An exact-arity call
invokes the body directly. Under-application boxes the remaining λ at a
value demand; its capture holds supplied arguments. The residual crosses
as a thunk; a builtin constant crosses by name. Boundary partials retain
their checked site in inference-expanded `Exec` blocks.
`ValClos::residual` supports display and hook arity and prevents handing δ
to handler or hook checking
([[decisions/261112_runtime-values-are-value-closures|runtime-values-are-value-closures]]).

**Bundled coreutils are not builtins.** They are exec images, so they live
outside the manifest module: `core/src/uutils.rs` declares the vendored tools via
`declare_coreutils!` as two `cfg`-gated lists — `cross` (always on) and `unix`
(Unix-only) — emitting one `COREUTILS_TOOLS` slice and a `coreutils_invoke`
dispatcher; `RIPGREP_TOOLS` routes `rg` through `ral-ripgrep-core`. A bundled
head resolves to a `Program::Tool` and is always born as a
`ral --ral-bundled-tool <tool>` child, so it carries ordinary process semantics
and the same capability chokepoint as a host external
([[decisions/260731_bundled-tools-always-reexec|bundled-tools-always-reexec]]) —
which is what lets ral be a [[invariants/single-binary|single binary]] with no
sibling helpers.

**Host layers register their own, above core.** The [[map/repl|REPL]] adds the
`_ed-*` editor builtins and exarch adds its resident host atoms; both sit *above*
core and core never inspects them
([[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]), and
they seed through the same two installs, so a host contributes natives as well
as base frames: the REPL's `_ed-*` family and the host-installed `detach` are
natives. `detach` takes a description, a program name and an argument list,
with a fixed arity of three; a binding, not a handler, stubs it. Two
core-implemented entries register this way too — `WATCH_BUILTIN` and
`DETACH_BUILTIN`, each a public
one-entry wrapper over a private body — so a host whose streams are capture
buffers can omit `watch`, and a host that offers no survivors can omit `detach`
([[decisions/260617_watch-repl-builtin|watch-repl-builtin]],
[[map/core/builtins|builtins]]).

Plugins load only from the rc's `plugins:` map at startup, not through
builtin values. Their handlers live for the session; adding a plugin requires
a new session.

Why these primitives exist and how the set is shaped is
[[design/builtins|builtins]]; which layer any given capability lands in is
[[design/name-resolution|name-resolution]]. See also map
[[map/core/builtins|builtins]]. `docs/SPEC.md` §14, §16.7.
