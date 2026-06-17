---
status: active
---

# infer_case is left whole

The typechecker's `infer_case` is a single ~100-line function. Decomposing it
into smaller helpers is "probably wrong": the case-inference logic is one
coherent piece of reasoning, and splitting it scatters a single judgment across
helpers that each make no sense alone, with more surface than substance gained.

This is a standing choice recorded to forestall a well-meaning refactor. Leave
`infer_case` as one function unless its *behaviour* needs to change.

(Recorded 2026-05-30; the decision predates the wiki.)

See also [[map/core|core]]; cf. the authoring rule on finding the missing semantic
type rather than surface helpers (`AGENTS.md`).
