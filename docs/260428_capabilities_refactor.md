Diagnosis

  The design is good; the distribution is bad.

  - The semantics of a grant are split across /Users/lambdabetaeta/projects/ral-private/core/src/types/capability.rs:1, /Users/lambdabetaeta/projects/ral-private/core/src/types.rs:1232, and /
    Users/lambdabetaeta/projects/ral-private/core/src/sandbox.rs:1. Data, reduction, path resolution, checking, and OS projection are not co-located.
  - /Users/lambdabetaeta/projects/ral-private/core/src/sandbox/spawn.rs:1 is carrying too much: self-pinning, runner selection, stdio setup, child lifecycle, IPC orchestration, and audit
    rehydration.
  - /Users/lambdabetaeta/projects/ral-private/core/src/sandbox/ipc.rs:1 mixes wire types, framing, transport, request packing, child entry, and evaluation.
  - /Users/lambdabetaeta/projects/ral-private/core/src/sandbox/linux.rs:1 and /Users/lambdabetaeta/projects/ral-private/core/src/sandbox/macos.rs:1 both mix generic policy shaping with backend-
    specific enforcement.
  - src/grant.rs:1 is sensible, but it should stay a thin exarch-specific profile composer, not a second place where sandbox semantics accrete.

  The core mistake is that “what authority means” and “how a platform enforces it” are interleaved.

  Target Shape

  Use one law, many interpreters.

  - Law: pure capability algebra.
  - Judgment: reduce an active stack into one EffectiveGrant.
  - Machine: render the fs/net projection into a platform sandbox.
  - Ritual: re-exec, IPC, and result merge.

  core/src/capability/
    mod.rs
    policy.rs      // Capabilities, ExecPolicy, FsPolicy, ShellPolicy, EditorPolicy
    lattice.rs     // meet/join
    path.rs        // GrantPath, ResolutionMode, canonical/lexical handling
    effective.rs   // EffectiveGrant, SandboxProjection
    check.rs       // exec/fs/shell/editor checks + audit payloads

  core/src/sandbox/
    mod.rs         // tiny public facade
    bootstrap.rs   // early_init, flags, sandbox-active env
    limits.rs      // RLIMIT / Job Object
    runner.rs      // eval_grant orchestration
    reexec.rs      // sandbox-self pinning, internal mode
    ipc/
      wire.rs
      codec.rs
      transport.rs
      child.rs
    backend/
      mod.rs
      linux.rs
      macos.rs
      windows.rs

  exarch/src/policy/
    mod.rs
    base.rs        // baked profiles
    compose.rs     // base ∨ extend, then meets
    profiles.rs    // TOML loading / deny-self carveout

  A few concrete idioms would improve the code immediately:

  - Replace set-like Vec<String> plus sort/dedup with BTreeSet<String> or a small newtype that canonicalizes on construction.
  - Replace Option<Vec<(String, ExecPolicy)>> with Option<BTreeMap<String, ExecPolicy>>.
  - Keep path duality explicit with a type like GrantPath { raw, canonical } instead of recomputing it ad hoc.
  - Keep Dynamic as state, not policy engine. Its methods should delegate to capability::*.

  Plan

  1. Freeze behavior first. Add characterization tests for exec-dir matching, deny-path precedence, lexical-vs-canonical path checks, audit streaming, and self-binary pinning. Current baseline
     is stable: cargo test in exarch passed, and cargo test -p ral-core sandbox passed.
  2. Extract the pure capability domain out of types.rs. Move ExecVerdict, check_fs_op, sandbox_policy, canonical_prefix_pairs, intersect_prefix_pairs, and related helpers behind capability::
     {effective,check,path}. Leave thin delegating methods on Shell/Dynamic so call sites stay stable.
  3. Introduce EffectiveGrant. One constructor, one fold over the stack, no ambient mutation:
     EffectiveGrant::from_dynamic(&Dynamic) -> EffectiveGrant
     EffectiveGrant::sandbox_projection() -> Option<SandboxProjection>
     EffectiveGrant::check_exec(...), check_read(...), check_write(...)
  4. Split sandbox/spawn.rs into three concerns:
     reexec.rs for self-pinning and internal-mode dispatch,
     runner.rs for eval_grant and subprocess choreography,
     stdio.rs or runner.rs-local helpers for pump setup.
     That file should stop knowing about wire details.
  5. Split sandbox/ipc.rs into:
     wire.rs for request/response/frame structs,
     codec.rs for runtime <-> wire conversion,
     transport.rs for framing and IpcChannel,
     child.rs for serve_from_env_fd and eval_request.
     The important rule is: transport should not know Shell, and evaluation should not know frame encoding.
  6. Normalize backend responsibilities. linux.rs and macos.rs should each consume only SandboxProjection. All prefix/path preprocessing should already be done before backend entry. Backend
     files should read like renderers, not mini policy engines.
  7. Clean up exarch last. Move src/grant.rs:1 into a small policy module and keep it strictly about profile composition and “deny the bytes that define me.” Do not let exarch duplicate core
     path semantics.
  8. After the move, simplify names. Favor nouns for data (EffectiveGrant, SandboxProjection, GrantPath) and short verbs for actions (reduce, check, render, enter, round_trip). The API should
     read like a semantics, not a scavenger hunt.

  The end state should let a reader trace grant as a straight line: reduce authority, decide, render, run, merge. That is the shape both Thompson and Plotkin would approve of.

  If you want, I can turn this plan into an implementation sequence and start with the first extraction: capability::{path,effective,check} plus delegation shims.


