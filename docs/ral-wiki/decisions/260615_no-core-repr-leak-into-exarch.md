---
status: active
---

# Core's representation stays private; the host reads it through accessors

**A host embedding `ral-core` ([[design/exarch-architecture|exarch-architecture]])
reads core's capability and shell state through *behavioural accessors*, never by
destructuring core's internal representation — so core owns the shape of its types
and may refactor them without breaking the host.** The embedding seam
([[decisions/260610_host-embedding-api|host-embedding-api]]) is a function
boundary; this is its data-boundary corollary.

The trigger was making `ExecPolicy::Subcommands` the *set* it always was. A grant's
subcommand allowlist is a set, but it was stored as an order-sensitive
`Vec<String>` — decoded in declaration order, sorted by `meet`/`join` — so the
documented idempotence law `a.meet(a) == a` was false on real decoded data. The
fix is the missing type: `Subcommands(BTreeSet<String>)`, sorted and deduped by
construction, so `Eq`/`meet`/`join` agree and the law holds with no explicit
canonicalisation. That change broke `exarch/src/prompt.rs`, which matched
`ExecPolicy`'s variants and called `.join` on the container — a private
representation choice had silently become a published commitment to the host.

The boundary that resolves it:

- **`ExecPolicy` exposes what the host needs, not its shape.** `admit_label(name)
  -> Option<String>` renders an admitted command's legend — `git`,
  `cargo[build,test]`, or `None` for `Deny`; `is_denied() -> bool` reports the
  lattice bottom. exarch's prompt builder calls these, so `prompt.rs` no longer
  imports `ExecPolicy` or names a `Vec`/`BTreeSet`. Only a `String` and a `bool`
  cross the seam.
- **Core owns the representation; the host is oblivious.** The `Vec → BTreeSet`
  switch (refining the `Subcommands(Vec<String>)` named in
  [[decisions/260602_exec-authority-partitioned|exec-authority-partitioned]]) is
  invisible to every host, because no host ever held the container.
- **Core owns the canonical rendering of its own authority element.** The legend
  format `name[a,b]` belongs to core — it is the textual form of an exec rule, not
  a prompt-layout choice — so the host arranges core-provided renderings rather
  than re-deriving them from core's shape.
- **This is the data-shape analogue of the check chokepoint.** Authority is
  *judged* only through `capability::check_*(&Context, …)`
  ([[decisions/260605_witness-collapse|witness-collapse]]); authority is *read*
  only through accessors. [[design/capability-carriers|capability-carriers]]
  already holds that a consumer receives authority in the form it needs — the
  host's form is a rendered label, not core's lattice element.

Why a boundary and not convenience: destructuring core's variants inside exarch
couples the host to every representation choice core makes, turning each internal
type into an API the host pins. The accessor keeps the seam narrow — core
refactors freely, the host stays oblivious — and the only thing that ever crosses
is the host's own currency, a rendered `String` or a `bool`.

See also [[design/grant|grant]], [[map/exarch|exarch]] (prompt assembly),
[[map/core/capabilities|capabilities]].
