---
status: proposed
---

# Bundled tools are executable images

**A bundled coreutils/diffutils/ripgrep command is an *executable image*, not a
pipeline-helper job.** ral should keep an inline placement only for the clean
terminal case where the process already has the right ambient state; every other
bundled invocation is a child process whose image is ral itself plus a hidden
bundled-tool sentinel. The stage helper remains reserved for evaluating ral
computations.

## Context

The bundled tools are linked into the ral binary and reached through
`uutils_invoke`. Their upstream entrypoints are ordinary process programs with
library-shaped signatures:

- **They read and write process-global state.** They observe libc/std handles,
  `std::env`, `current_dir`, and uucore's process-global exit-code cell. ral's
  own model keeps cwd/env on `Shell`, not on the host process, so copying that
  state into the process is only sound inside an isolated child.
- **Fd/env/cwd guards are not thread-local.** A guard that saves and restores
  fd 0/1/2, cwd, or environment is correct in a single-job helper process. In a
  parent with `spawn`, `watch`, tests, or any other worker thread, the same guard
  lets siblings observe a transient global mutation.
- **Windows process creation is expensive.** The design should not turn a plain
  `ls` at an interactive prompt into a self-reexec on every call when the
  process already has the right cwd/env and direct terminal handles.
- **Pipelines already have a process boundary.** A byte pipeline stage is an OS
  process in a process group / Windows Job Object. A bundled byte stage needs the
  same lifecycle, cancellation, stdio wiring, and sandbox treatment as an
  external stage; it does not need an evaluator report frame.
- **Value edges still require evaluation.** A bundled head on a value edge must
  enter the evaluator so data-last application can happen. That remains a ral
  helper stage as recorded in [[decisions/260610_value-edge-locality|value-edge-locality]].

The pressure is therefore two-sided: preserve the cheap inline path that makes
Windows tolerable, but remove every path that installs shell state into the
parent process to make a library call look like an exec.

## Decision

Name the command image as part of the spawn plan:

```rust
enum ExecImage {
    Host(PathBuf),
    BundledTool { tool: String },
}

struct SpawnPlan {
    shown: String,
    image: ExecImage,
    args: Vec<String>,
}
```

The exact type names may move, but the representation is the decision: resolving
a command yields either a host executable path or a bundled tool image. The rest
of the runtime consumes the image without asking again whether the command is
bundled.

### Two placements

- **Inline placement is an optimisation, not the model.** It is admitted only
  when all observable process state is already correct: direct terminal/stdout
  and stderr, no redirects, no capture/audit tee, no env overrides, no
  `within [dir: …]`, no logical-cwd/process-cwd mismatch, and no pipeline
  staging. A small process-local mutex serialises `uutils_invoke` in this path
  so the uucore exit-code cell cannot interleave across threads. That mutex is
  not an authority mechanism and must never be used to admit fd/env/cwd mutation.
- **Child placement is the model.** A bundled tool that misses the inline gate is
  spawned as ral itself:

  ```text
  ral --ral-bundled-tool <tool> <args...>
  ```

  The parent uses the ordinary command/pipeline spawn path: `apply_env` installs
  `Shell` cwd/env/PWD/OLDPWD on the child, stdio routing gives the child real fd
  0/1/2 handles, the sandbox self-path pins the executable under confinement,
  and `RunningChild` owns wait, cancellation, pipe drain, and status.

### Hidden bundled-tool entrypoint

The hidden entrypoint is a multicall mode, not a pipeline helper:

1. validate `<tool>` against `is_uutils_tool`;
2. if a start-gate fd/handle is present, block until the pipeline launcher
   releases it;
3. call `uutils_invoke(tool, argv)` inside the child;
4. combine the direct return with uucore's exit-code cell;
5. exit with the tool status.

It reads no `StageJob`, sends no `ChildEvalResponse`, and reconstructs no
`Shell`. Its inherited process environment, cwd, stdio, process group / job
object, and sandbox are already the execution context.

### Pipeline launch

Pipeline resolution keeps the value-edge rule and changes only the byte-stage
image:

- **byte-only bundled stage** → direct process stage with
  `ExecImage::BundledTool`;
- **bundled stage touching a value edge** → `HelperEval`, because data-last
  application is evaluator work;
- **ordinary external stage** → `ExecImage::Host` as today;
- **ral computation stage** → `HelperEval`.

A foreground pipeline's launch gate is orthogonal to evaluation. Direct bundled
children receive the same gate as direct external or helper children and block in
the hidden entrypoint before invoking the tool. The gate says "all stages have
joined the process group and the terminal handoff is settled"; it does not imply
there is a ral evaluator inside the child.

### Standalone dispatch

Standalone command dispatch follows the same image split:

- if the command is `ExecImage::BundledTool` and the inline gate accepts it,
  call `uutils_invoke` in-process under the inline mutex;
- otherwise spawn the image through the ordinary external runner.

This deletes the synthetic one-stage pipeline used only to force a helper
process for capture, audit, env, or cwd. Those cases become ordinary child
process execution of the bundled image.

### Audit

Audit remains a property of the parent command boundary.

- Non-byte-capturing audit records a command node in the parent, like a direct
  external stage.
- Byte-capturing audit should be realised by generic sink tee/pump plumbing.
  The child writes bytes; the parent captures bytes while pumping or routing
  them. A bundled tool does not get a bespoke report protocol merely because it
  is linked into ral.

## Consequences

- The pipeline helper protocol has one job again: evaluate a ral computation in
  a child. It no longer has a bundled-tool arm, `UutilsSnapshot`, or a
  helper-only process-state guard.
- Every path that needs env/cwd/redirect/capture isolation gets an OS process
  boundary. The parent process does not install temporary env, cwd, or stdio
  state to run a bundled tool.
- Windows keeps the important optimisation: a clean terminal `ls` remains
  in-process. Pipelines and capture fallbacks already require a child boundary;
  this design makes that boundary ordinary rather than evaluator-shaped.
- Cancellation and job control become boring. A child-placed bundled tool is a
  `RunningChild`, so Ctrl-C, deadlines, process groups / Job Objects, pipe
  drain, and stop/foreground rules are the same as for host executables.
- Sandboxing stays compositional. The parent renders the child with
  `sandbox::self_command`, so confined bundled tools run under the same pinned
  self-reexec discipline as helper children.
- Tests should exercise the process boundary. Unit tests may cover the pure
  classifier and the hidden entrypoint's argv/status logic, but tests for env,
  cwd, redirection, capture, pipeline, or audit must spawn the child path rather
  than calling the global-mutating runner inside libtest.

## Alternatives considered

- **Always self-reexec bundled tools.** Rejected. It is the cleanest state story,
  but it throws away the Windows-sensitive inline optimisation for the common
  clean-terminal case.
- **Keep the uutils stage-helper arm.** Rejected. It treats a byte command as an
  evaluator job, carries a shell snapshot solely to reinstall process state, and
  makes direct tests call a subprocess-only runner in the parent.
- **Guard the existing in-process paths with a global mutex.** Rejected as the
  main design. A mutex can protect uucore's exit-code cell, but it does not make
  fd/env/cwd mutation semantically thread-local. Use it only for the admitted
  inline placement where those mutations are absent.
- **Use host coreutils on Unix and bundled tools only on Windows.** Rejected.
  ral's bundled-first rule gives one surface across platforms and keeps sandbox
  policy tied to ral's own command resolution rather than to whatever happens to
  be on PATH.
- **Keep a Windows worker process pool or daemon.** Rejected for this cut. A
  daemon would need handle passing for stdio, per-job cwd/env/sandbox state,
  process-group or Job Object cancellation semantics, panic/reset handling for
  uucore globals, and a story for grants. That is more protocol than the current
  helper arm, not less.

## Open questions

- **Exact sentinel spelling.** `--ral-bundled-tool` is the working name. It
  should live beside the existing helper sentinels and be consumed before the
  test harness or host frontend sees it.
- **Generic byte-capturing audit.** If any current audit capture depends on the
  helper report rather than on sink plumbing, that should be fixed at the sink
  layer and shared with host external stages.
- **Ripgrep and diffutils argv shape.** The hidden entrypoint must preserve each
  shim's existing argv convention: ripgrep wants user args after dropping
  argv[0]; diffutils/coreutils expect the tool name in argv slot 0.

See also [[decisions/260610_value-edge-locality|value-edge-locality]],
[[decisions/260610_child-eval-unification|child-eval-unification]],
[[internals/pipeline-execution|pipeline-execution]], [[map/core/runtime|map: runtime]],
[[map/core/builtins|map: builtins]], and [[map/core/io-process|map: io-process]].
