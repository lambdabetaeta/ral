//! External process execution.
//!
//! Spawning a child, wiring stdin/stdout/stderr per the call-site redirects,
//! pumping the result back into the shell's sink, translating exit codes.
//! Kept separate from `evaluator.rs` because none of this is CBPV semantics —
//! it's plumbing between the evaluator and the operating system.

use crate::ast::RedirectMode;
use crate::io::Sink;
use crate::ir::ExecName;
use crate::types::*;
use crate::path::tilde::expand_tilde_path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// A redirect whose target has been evaluated to a concrete file path or fd.
/// The evaluator walks the AST's `ValRedirectTarget` and hands one of these
/// to `exec_external` and to `apply_redirects` (for builtins).
pub(crate) enum EvalRedirect {
    File(String),
    Fd(u32),
}

/// Capability witness that fd 0 of the parent process is safe to inherit
/// into a spawned child's stdin in the current context.
///
/// Constructible only via the named issuers below.  Each issuer documents
/// the discipline that justifies the inheritance: a standalone foreground
/// external owns the tty itself; a pure-external pipeline pgid will own
/// the tty via `claim_foreground`; a non-tty parent fd 0 cannot SIGTTIN
/// anyone.
///
/// Mixed pipelines must NOT mint a permit — handing terminal stdin to an
/// external in a backgrounded pgid would SIGTTIN the child the moment it
/// reads, and ral's pump would then wait forever.  The audit's high #2.
pub struct TtyInputPermit {
    _private: (),
}

impl TtyInputPermit {
    /// Issued for a standalone external job: the spawned process either
    /// becomes the foreground pgid leader (and so owns the tty) or runs
    /// non-interactively where SIGTTIN is moot.
    pub(crate) fn for_standalone_external() -> Self {
        Self { _private: () }
    }

    /// Issued for the first stage of a pure-external pipeline: the
    /// pipeline's own pgid will be foregrounded, so its members can read
    /// from the tty without SIGTTIN.
    pub(crate) fn for_pure_external_pipeline() -> Self {
        Self { _private: () }
    }

    /// Issued when fd 0 of the parent is not a controlling terminal —
    /// inheritance cannot SIGTTIN anyone.  Caller must have just observed
    /// `terminal.startup_stdin_tty == false`.
    pub(crate) fn for_non_tty_stdin() -> Self {
        Self { _private: () }
    }
}

/// How a spawned child's stdin will be wired.
///
/// `Inherit` requires a [`TtyInputPermit`] so that grep'ing for the route
/// makes the discipline visible: the permit is the auditable place where
/// "yes, I checked, this is safe to inherit" lives.  Mixed pipelines
/// produce `Null` instead of `Inherit` for stages with no upstream pipe.
pub enum StdinRoute {
    Inherit(TtyInputPermit),
    Pipe(os_pipe::PipeReader),
    File(std::fs::File),
    Null,
}

impl StdinRoute {
    pub fn into_stdio(self) -> Stdio {
        match self {
            StdinRoute::Inherit(_) => Stdio::inherit(),
            StdinRoute::Pipe(r) => Stdio::from(r),
            StdinRoute::File(f) => Stdio::from(f),
            StdinRoute::Null => Stdio::null(),
        }
    }
}

/// Routing decisions for a child process's stdout / stderr, derived once
/// from the call-site redirects.  `stderr_to_stdout` captures the `2>&1` fd
/// dup; the file fields carry their `RedirectMode` so `open_file` can pick
/// create-vs-append.
///
/// Stdin is *not* on the plan: `<file` is opened upstream and parked in
/// `shell.io.stdin` by [`install_stdin_redirect`], so every dispatch arm
/// (builtin, external, pipeline stage) reads input through the same `Source`
/// channel.  See the module docstring on the cached-tty bug class.
struct RedirectPlan {
    stdout_file: Option<(String, RedirectMode)>,
    stderr_file: Option<(String, RedirectMode)>,
    stderr_to_stdout: bool,
}

/// Coerce `>` to streaming for stderr — atomic semantics make no sense for
/// diagnostic output.  All other modes pass through unchanged.
fn stderr_mode(mode: &RedirectMode) -> RedirectMode {
    match mode {
        RedirectMode::Write => RedirectMode::StreamWrite,
        other => *other,
    }
}

fn classify_redirects(redirects: &[(u32, RedirectMode, EvalRedirect)]) -> RedirectPlan {
    let mut plan = RedirectPlan {
        stdout_file: None,
        stderr_file: None,
        stderr_to_stdout: false,
    };
    for (fd, mode, target) in redirects {
        match target {
            EvalRedirect::Fd(target_fd) => {
                if *fd == 2 && *target_fd == 1 {
                    plan.stderr_to_stdout = true;
                }
            }
            EvalRedirect::File(filename) => match fd {
                // fd 0 is handled by install_stdin_redirect upstream.
                1 => plan.stdout_file = Some((filename.clone(), *mode)),
                2 => plan.stderr_file = Some((filename.clone(), *mode)),
                _ => {}
            },
        }
    }
    plan
}

/// Translate a `std::io::Error` into an `EvalSignal::Error` with the same
/// NotFound / PermissionDenied / fallback mapping used everywhere external
/// I/O can fail.  `status` is the exit code to carry on the error (1 for
/// file ops, 127 for missing executables).  `not_found` lets the spawn site
/// substitute `compat::not_found_hint` for the plain "no such file" message.
fn io_error(ctx: &str, e: std::io::Error, status: i32, not_found: Option<String>) -> EvalSignal {
    let msg = match e.kind() {
        std::io::ErrorKind::NotFound => {
            not_found.unwrap_or_else(|| format!("{ctx}: no such file or directory"))
        }
        std::io::ErrorKind::PermissionDenied => format!("{ctx}: permission denied"),
        _ => format!("{ctx}: {e}"),
    };
    EvalSignal::Error(Error::new(msg, status))
}

/// Proof token: the command head has been resolved against PATH and is
/// known to exist (or is a path literal the OS will adjudicate at spawn).
///
/// The constitutional point is the borrow: `validate_argv` requires
/// `&Identified`, so any shape-of-args rejection cannot run before
/// existence has been settled.  A future contributor adding a new
/// argv-shape validator gets the ordering for free; one who tries to
/// inline shape rejection ahead of identification literally won't compile.
struct Identified {
    shown: String,
    resolved: String,
}

/// Establish identity: render the head, walk PATH for bare names, and
/// fail fast with 127 when the name is bare and missing.  Path / tilde
/// literals reach the OS unchanged — kernel-level ENOENT at spawn time
/// is a perfectly good diagnostic for those.  Builtins, aliases, and
/// grant-denied heads are filtered upstream by `classify_dispatch`, so
/// any Bare name that didn't resolve here really is unfindable.
fn identify(name: &ExecName, shell: &Shell) -> Result<Identified, EvalSignal> {
    let shown = render_exec_name(name, shell);
    let resolved = resolve_in_path(name, shell);
    if let ExecName::Bare(_) = name
        && resolved == shown
    {
        return Err(EvalSignal::Error(
            Error::new(crate::compat::not_found_hint(&shown), 127),
        ));
    }
    Ok(Identified { shown, resolved })
}

/// Reject argv shapes the external boundary cannot accept.
///
/// Takes `&Identified` rather than `&str` so the diagnostic priority
/// ("does this command exist?" beats "is this argument the right
/// shape?") becomes a borrow-check obligation: shape rejection cannot
/// fire ahead of existence resolution.
fn reject_exec_arg(id: &Identified, arg: &Value, shell: &Shell) -> Option<EvalSignal> {
    let cmd = id.shown.as_str();
    match arg {
        Value::List(_) | Value::Map(_) | Value::Thunk { .. } | Value::Handle(_) => {
            Some(shell.err_hint(
                format!(
                    "cannot pass {} to external command '{cmd}'",
                    arg.type_name()
                ),
                format!("use '...' to spread a list into arguments: {cmd} ...$var"),
                1,
            ))
        }
        Value::Bytes(_) => Some(shell.err_hint(
            format!("cannot pass Bytes as argument to external command '{cmd}'"),
            "pipe binary data via stdin with to-bytes, or decode to string first",
            1,
        )),
        _ => None,
    }
}

/// Stringify `args`, refusing any shape the external boundary cannot
/// accept.  `&Identified` is the borrow that proves identity has been
/// settled — without it, this function cannot be called.
fn validate_argv(
    id: &Identified,
    args: &[Value],
    shell: &Shell,
) -> Result<Vec<String>, EvalSignal> {
    for arg in args {
        if let Some(sig) = reject_exec_arg(id, arg, shell) {
            return Err(sig);
        }
    }
    Ok(args.iter().map(|v| v.to_string()).collect())
}

/// Choose the stdin route for a single-command external job.
///
/// All non-terminal sources (pipeline pipe, `<file` redirect already opened
/// by [`install_stdin_redirect`]) flow through `shell.io.stdin`; this routine
/// just consumes whatever sits there.  The fall-through inherit case is gated
/// on a `TtyInputPermit`: inheriting fd 0 when the parent's fd 0 is the
/// controlling tty is only safe when this child will hold the foreground
/// itself (`for_standalone_external`) or when fd 0 is not actually a tty
/// (`for_non_tty_stdin`).  Both are issued here.
fn wire_stdin(shell: &mut Shell) -> StdinRoute {
    match shell.io.stdin.take_reader() {
        Some(crate::io::SourceReader::Pipe(r)) => StdinRoute::Pipe(r),
        Some(crate::io::SourceReader::File(f)) => StdinRoute::File(f),
        None => {
            let permit = if shell.io.terminal.startup_stdin_tty {
                TtyInputPermit::for_standalone_external()
            } else {
                TtyInputPermit::for_non_tty_stdin()
            };
            StdinRoute::Inherit(permit)
        }
    }
}

/// Wire the child's stdout to a redirect file when one is set.
///
/// Returns the atomic-commit token (`Some` for `>` to a regular file) plus an
/// optional `Stdio` handle to assign to the child's stderr — populated only on
/// Windows when the redirect plan has `2>&1`, since Windows lacks `pre_exec`
/// and the dup must be wired pre-spawn from a clone of the same handle.
fn wire_stdout_file(
    command: &mut Command,
    plan: &RedirectPlan,
    shell: &mut Shell,
) -> Result<(Option<AtomicCommit>, Option<Stdio>), EvalSignal> {
    let Some((path, mode)) = &plan.stdout_file else {
        return Ok((None, None));
    };
    let (file, commit) = open_file(path, mode, shell)?;
    #[cfg(windows)]
    let stderr_dup = if plan.stderr_to_stdout {
        Some(Stdio::from(file.try_clone().map_err(pipe_err)?))
    } else {
        None
    };
    #[cfg(not(windows))]
    let stderr_dup = None;
    command.stdout(Stdio::from(file));
    Ok((commit, stderr_dup))
}

/// Set up the child's stderr according to plan and audit mode.
///
/// `2>&1` is realised differently per platform:
///   * Unix: a `pre_exec` `dup2(STDOUT, STDERR)` runs after the kernel has
///     wired the child's fd 1, so stderr inherits whatever stdout was pointed
///     at — pipe, file, or inherited fd.
///   * Windows: there is no `pre_exec`, so the dup must happen pre-spawn by
///     cloning the same handle that's about to be assigned as stdout.
///     `stdout_file_dup` carries that clone for the file-redirect case;
///     for sink-driven stdout we re-clone the writer from `shell.io.stdout`,
///     and for the inherit-tty case we duplicate the parent's `STDOUT_HANDLE`.
///
/// An explicit `2>file` always wins over auditing; auditing captures stderr
/// only when no other destination is set.
///
/// Returns true when stderr is piped and a pump thread or reader must drain
/// `child.stderr` after spawn (callers handle auditing and sink paths
/// separately).
fn wire_stderr(
    command: &mut Command,
    plan: &RedirectPlan,
    auditing: bool,
    inherit_tty: bool,
    stdout_file_dup: Option<Stdio>,
    shell: &mut Shell,
) -> Result<bool, EvalSignal> {
    if plan.stderr_to_stdout {
        #[cfg(unix)]
        {
            let _ = (inherit_tty, stdout_file_dup);
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    if libc::dup2(libc::STDOUT_FILENO, libc::STDERR_FILENO) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            let stdio = if let Some(dup) = stdout_file_dup {
                dup
            } else if inherit_tty {
                use std::os::windows::io::AsHandle;
                let owned = std::io::stdout()
                    .as_handle()
                    .try_clone_to_owned()
                    .map_err(pipe_err)?;
                Stdio::from(owned)
            } else {
                match &shell.io.stdout {
                    crate::io::Sink::Pipe(w) => Stdio::from(w.try_clone().map_err(pipe_err)?),
                    // Buffer / Tee / LineFramed sinks pump child.stdout via an
                    // anonymous pipe allocated by `Stdio::piped()`; we cannot
                    // reach into that pipe pre-spawn to share it with stderr.
                    // Fall back to inherit so diagnostics still surface.
                    _ => Stdio::inherit(),
                }
            };
            command.stderr(stdio);
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (inherit_tty, stdout_file_dup);
            command.stderr(Stdio::inherit());
        }
        Ok(false)
    } else if let Some((path, mode)) = &plan.stderr_file {
        let (file, _) = open_file(path, &stderr_mode(mode), shell)?;
        command.stderr(Stdio::from(file));
        Ok(false)
    } else if auditing || !matches!(shell.io.stderr, crate::io::Sink::Stderr) {
        // Auditing or non-default stderr sink (e.g. §13.3 replay's Sink::Buffer).
        // Pipe the child's stderr; the caller pumps it.
        command.stderr(Stdio::piped());
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Announce the running command via the terminal title (OSC 0).
fn announce_command_title(cmd: &str, shell: &Shell) {
    if shell.io.interactive && shell.io.terminal.ui_title_ok() {
        use std::io::Write;
        let _ = std::io::stdout().write_all(format!("\x1b]0;{cmd}\x07").as_bytes());
        let _ = std::io::stdout().flush();
    }
}

/// Compute the exit code from an `ExitStatus`, including Unix signal exits.
///
/// On Unix a process killed by signal `s` is reported as `128 + s` — the
/// same convention used by bash and POSIX sh.  On non-Unix `unwrap_or(1)`
/// is the best we can do.
pub(crate) fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            status.signal().map(|s| 128 + s).unwrap_or(1)
        }
        #[cfg(not(unix))]
        {
            1
        }
    })
}

/// Map a spawn `io::Error` for command `name` into an `EvalSignal`.
///
/// Uses `compat::not_found_hint` for `NotFound` so the error message mentions
/// PATH issues and platform conventions.  Exit status 127 matches the POSIX
/// convention for "command not found".
pub(crate) fn spawn_error(name: &str, e: std::io::Error) -> EvalSignal {
    EvalSignal::Error(Error::new(
        match e.kind() {
            std::io::ErrorKind::NotFound => crate::compat::not_found_hint(name),
            std::io::ErrorKind::PermissionDenied => format!("{name}: permission denied"),
            _ => format!("{name}: {e}"),
        },
        127,
    ))
}

/// Wrap an I/O error from pipe creation/cloning into an `EvalSignal`.
fn pipe_err(e: std::io::Error) -> EvalSignal {
    EvalSignal::Error(Error::new(format!("pipe: {e}"), 1))
}

/// Wrap `base` in a Tee that mirrors bytes into a private audit buffer
/// when `auditing` is on; otherwise pass through unchanged.  Used by
/// every external-pump path (standalone exec, pipeline external stage)
/// so the audit-capture invariant lives in one place.
pub(crate) fn audit_tee(base: Sink, auditing: bool) -> (Sink, Option<Arc<Mutex<Vec<u8>>>>) {
    if auditing {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Sink::Tee(Box::new(Sink::Buffer(buf.clone())), Box::new(base));
        (sink, Some(buf))
    } else {
        (base, None)
    }
}

/// Spawn an auxiliary thread that captures the leading 64 KiB of the child's
/// stderr and drains the rest to `io::sink()`.
///
/// Capturing on a dedicated thread is what makes it safe to wait on `stdout`
/// (or its pump) before reaping: a child that fills its stderr pipe before
/// closing stdout would otherwise deadlock against the pump.  The bounded
/// prefix keeps the exec-tree JSON compact regardless of how noisy the child
/// is.
pub(crate) fn spawn_stderr_reader(
    child: &mut std::process::Child,
) -> Option<std::thread::JoinHandle<Vec<u8>>> {
    child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            let _ = stderr.by_ref().take(65536).read_to_end(&mut buf);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            buf
        })
    })
}

/// Resolve a bare command name against the ral environment's `PATH`.
///
/// `within [shell: [PATH: …]]` overrides the search directory list for every
/// command inside the block.  Without this walk, `Command::new` hands the
/// bare name to `posix_spawnp(3)`, which searches the *parent* process's
/// PATH — ignoring the override.  Resolving here and passing the full path
/// to `Command::new` makes the ral environment authoritative.
///
/// Names already containing `/` are paths and returned unchanged.  If no
/// match is found the bare name is returned so the OS can produce its
/// normal "not found" error.
pub(crate) fn resolve_in_path(name: &ExecName, shell: &Shell) -> String {
    let rendered = render_exec_name(name, shell);
    if let ExecName::Bare(_) = name {
        let fallback;
        let path = match shell.dynamic.env_vars().get("PATH") {
            Some(p) => p.as_str(),
            None => {
                fallback = std::env::var("PATH").unwrap_or_default();
                &fallback
            }
        };
        if let Some(resolved) = crate::path::resolve_in_path(&rendered, path) {
            return resolved;
        }
    }
    rendered
}

pub(crate) fn exec_policy_names(name: &ExecName, shell: &Shell, resolved: &str) -> Vec<String> {
    let rendered = render_exec_name(name, shell);
    let mut names = Vec::new();
    let mut include_rendered = true;
    if matches!(name, ExecName::Bare(_)) {
        let baseline = std::env::var("PATH")
            .ok()
            .and_then(|path| crate::path::resolve_in_path(&rendered, &path));
        if baseline.as_deref() != Some(resolved) && resolved != rendered {
            include_rendered = false;
        }
    }
    if include_rendered {
        names.push(rendered);
    }
    if resolved != names.last().map(String::as_str).unwrap_or_default() {
        names.push(resolved.into());
    }
    // Path-style execs (`./configure`, `bin/run`) render and resolve to a
    // relative string, so the `exec_dirs` matcher — which requires
    // absolute paths — never sees them.  Push the cwd-joined absolute
    // form as an extra candidate so a directory grant covering the
    // working tree can admit them.
    if matches!(name, ExecName::Path(_))
        && let Some(last) = names.last()
        && let Some(abs) = absolutize_relative(last, shell)
        && !names.iter().any(|n| n == &abs)
    {
        names.push(abs);
    }
    names
}

/// Lexically resolve a relative path against the shell's effective
/// cwd, collapsing `.` and `..`.  Returns `None` when `s` is already
/// absolute.
fn absolutize_relative(s: &str, shell: &Shell) -> Option<String> {
    if std::path::Path::new(s).is_absolute() {
        return None;
    }
    let cwd = shell.dynamic.cwd.as_deref();
    Some(crate::path::resolve_path(cwd, s).to_string_lossy().into_owned())
}

/// A fully-resolved external command, ready to be turned into a `Command`.
///
/// Produced by [`resolve_command`].  Holds the display name, the PATH-
/// resolved executable, and the stringified argv.  Both single-command exec
/// and pipeline-stage launch consume this struct so the resolution rules
/// (PATH lookup, grant policy names, `check_exec_args`, argv rejection of
/// list/map/thunk/handle/Bytes) live in exactly one place.
pub(crate) struct ResolvedCommand {
    pub(crate) shown: String,
    pub(crate) resolved: String,
    pub(crate) args: Vec<String>,
}

/// Resolve a command name and pre-evaluated values into a [`ResolvedCommand`].
///
/// The work splits into four phases whose ordering is structurally
/// enforced — each phase consumes the previous phase's output token,
/// so they cannot run out of order:
///
///   1. **identity**: PATH lookup; bare-not-found → 127.  Yields
///      [`Identified`].
///   2. **argv shape**: `validate_argv` rejects list/map/thunk/handle/Bytes
///      args and stringifies the rest.  Requires `&Identified`, which is
///      how the diagnostic priority "does this command exist?" beats
///      "is this argument the right shape?" without relying on review.
///   3. **grant policy**: `exec_policy_names` + `check_exec_args` against
///      the active grant lattice.
///   4. **spawn target rewrite**: bundled uutils tools become a re-exec
///      of `current_exe()` with the multicall flag prepended; everything
///      else spawns the resolved path directly.  Display, diagnostics,
///      and grant-policy keys still use the tool name, not the helper
///      path.
///
/// Returns the same diagnostic shape both call sites used to produce
/// independently — there is now one source of truth.
pub(crate) fn resolve_command(
    name: &ExecName,
    args: &[Value],
    shell: &mut Shell,
) -> Result<ResolvedCommand, EvalSignal> {
    // Phase 1 — identity: existence and 127 land here.  The returned
    // `Identified` is the borrow token for everything downstream; any
    // shape / policy / spawn-target step requires it, so the ordering
    // (existence ⇒ shape ⇒ policy ⇒ spawn rewrite) is a type-system
    // obligation, not a code-review note.
    let id = identify(name, shell)?;
    // Phase 2 — argv shape: rejected only after identity is known.
    let arg_strs = validate_argv(&id, args, shell)?;
    // Phase 3 — grant policy: the grant lattice gets to see the shown
    // name, the resolved path, and the stringified argv.
    let policy_names = exec_policy_names(name, shell, &id.resolved);
    let policy_name_refs: Vec<&str> = policy_names.iter().map(String::as_str).collect();
    shell.check_exec_args(&id.shown, &policy_name_refs, &arg_strs)?;
    // Phase 4 — spawn target: bundled uutils tools get rewritten to a
    // re-exec of ourselves with the multicall flag; everything else
    // spawns the resolved path directly.  Display/policy/argv-shape are
    // already settled, so this only moves *where* spawn lands.
    Ok(rewrite_spawn_target(name, id, arg_strs))
}

/// Apply the bundled-uutils helper substitution if applicable: when the
/// bare name is a uutils tool, swap the resolved exec for `current_exe()`
/// and prepend `[--ral-uutils-helper, name]` to argv so the spawn enters
/// a fresh ral that dispatches the bundled uucore tool.  Display name
/// and argv ordering for diagnostics are unchanged.
fn rewrite_spawn_target(
    name: &ExecName,
    id: Identified,
    arg_strs: Vec<String>,
) -> ResolvedCommand {
    if let ExecName::Bare(bare) = name
        && crate::builtins::uutils::is_uutils_tool(bare)
        && let Ok(self_exe) = std::env::current_exe()
    {
        let mut helper_args = Vec::with_capacity(2 + arg_strs.len());
        helper_args.push(crate::builtins::uutils::HELPER_FLAG.into());
        helper_args.push(bare.clone());
        helper_args.extend(arg_strs);
        return ResolvedCommand {
            shown: id.shown,
            resolved: self_exe.to_string_lossy().into_owned(),
            args: helper_args,
        };
    }
    ResolvedCommand {
        shown: id.shown,
        resolved: id.resolved,
        args: arg_strs,
    }
}

/// Build a `Command` from a `ResolvedCommand` and apply the shell's
/// scoped env vars + cwd.  Stdio routing and `pre_exec` hooks remain the
/// caller's responsibility — those vary between single-command and pipeline
/// contexts.
pub(crate) fn build_command(rc: &ResolvedCommand, shell: &Shell) -> Command {
    let mut cmd = crate::sandbox::make_command(&rc.resolved, &rc.args, shell);
    apply_env(&mut cmd, shell);
    cmd
}

/// Spawn an external child the canonical way: install the canonical
/// `pre_exec` (apply pgid, reset child signals) via
/// `signal::spawn_with_pgid`, mirror `setpgid` in the parent, then
/// apply sandbox child limits if any capability grant is active.
///
/// One funnel for both standalone exec (`exec_external`) and pipeline
/// stages (`launch_external_stage` via `PipelineGroup::spawn`) so the
/// post-spawn boilerplate cannot drift.
///
/// Returns the child plus its leader pgid: `Some` when `pgid` is
/// `NewLeader` or `Join`, `None` for `Inherit`.
pub(crate) fn spawn_external(
    cmd: &mut Command,
    pgid: crate::signal::PgidPolicy,
    shell: &Shell,
) -> std::io::Result<(std::process::Child, Option<crate::signal::Pgid>)> {
    let (child, leader) = crate::signal::spawn_with_pgid(cmd, pgid)?;
    if shell.has_active_capabilities() {
        crate::sandbox::apply_child_limits(&child);
    }
    Ok((child, leader))
}

/// Build the canonical "command exited with status N" error, complete
/// with hint lookup.  Both `exec_external` and the pipeline collector
/// produce diagnostics of this exact shape; centralising the
/// construction keeps the wording, status, hint integration, and
/// `SourceLoc` use in a single place.
pub(crate) fn external_exit_error(
    cmd_name: &str,
    code: i32,
    loc: crate::diagnostic::SourceLoc,
    shell: &Shell,
) -> Error {
    let hint = shell.exit_hints.lookup(cmd_name, code);
    let mut err = Error::new(format!("{cmd_name}: exited with status {code}"), code).at_loc(loc);
    err.hint = hint;
    err
}

/// A spawned external child plus the threads draining its piped
/// stdout/stderr.  The shared core of standalone exec and pipeline
/// external stages.
///
/// Lifecycle is a small typestate:
///
/// ```text
///     RunningChild ──wait──► WaitedChild ──drain──► ChildCaptures
///         │  Drop                │  Drop
///         ▼                      ▼
///     SIGKILL+reap           join+drop
/// ```
///
/// `wait` consumes `RunningChild` and yields a `WaitedChild` that
/// carries the `ExitStatus`.  `drain` is only callable on `WaitedChild`,
/// so the bug class "drain before wait" (which would let drainers block
/// on a still-open pipe) is unwritable: there's no `RunningChild::drain`.
/// Likewise "wait twice" is unwritable: `wait` consumes self, so a
/// second call has nothing to consume.
///
/// Holding the pgid (not just the pid) means abort-path SIGKILL takes
/// out descendants too — `/bin/sh -c 'sleep 999'` doesn't leave the
/// `sleep` alive when its parent goes away.  The Option around `child`
/// is the disarm latch for `Drop`: `wait` takes the child to call
/// `wait_handling_stop` and never puts it back, so the abort path's
/// kill/reap logic short-circuits if `wait` already ran.
pub(crate) struct RunningChild {
    pub child: Option<std::process::Child>,
    pub pgid: Option<crate::signal::Pgid>,
    pub pump: Option<std::thread::JoinHandle<()>>,
    /// Bounded reader for piped stderr — only present when auditing.
    /// Mutually exclusive with `stderr_pump`.
    pub stderr_reader: Option<std::thread::JoinHandle<Vec<u8>>>,
    /// Pump thread draining piped stderr into a non-default
    /// `Sink::stderr`.  Mutually exclusive with `stderr_reader`.
    pub stderr_pump: Option<std::thread::JoinHandle<()>>,
    pub audit_capture: Option<Arc<Mutex<Vec<u8>>>>,
    /// Display name used in wait-error messages.
    pub name: String,
}

/// A `RunningChild` whose `wait_handling_stop` returned successfully.
/// Holds the `ExitStatus` plus the still-running drainer threads (which
/// see EOF as soon as the child's write ends close at termination, but
/// haven't necessarily been joined yet).
///
/// Construction is gated by [`RunningChild::wait`]; there is no other
/// path.  All atomic-commit / status-interpretation / capture-collection
/// steps therefore have a borrow-check proof of "child has been observed
/// dead" before they run.
pub(crate) struct WaitedChild {
    pub status: std::process::ExitStatus,
    pump: Option<std::thread::JoinHandle<()>>,
    stderr_reader: Option<std::thread::JoinHandle<Vec<u8>>>,
    stderr_pump: Option<std::thread::JoinHandle<()>>,
    audit_capture: Option<Arc<Mutex<Vec<u8>>>>,
}

/// Bytes drained out of a finished child.  `stdout` is the audit-tee
/// buffer (empty when no audit/capture was wired); `stderr` is whatever
/// `stderr_reader` collected.
pub(crate) struct ChildCaptures {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Disposition of a spawned child's stderr.  Mutually-exclusive: once a
/// caller decides "pipe stderr for the audit tree", the pump-to-sink
/// branch cannot also fire on the same fd, and vice versa.  Inherited /
/// file / `2>&1` stderr leaves nothing for ral to drain — that's the
/// `Inherited` arm.
pub(crate) enum StderrCapture {
    /// stderr is wired directly (file, 2>&1, or default inherit) — nothing
    /// for ral to read after spawn.
    Inherited,
    /// stderr is piped; a bounded reader will collect the leading 64 KiB
    /// for the exec tree.  Auditing-only.
    AuditReader,
    /// stderr is piped; pump into this sink (a clone of `shell.io.stderr`
    /// when the caller has installed a non-default destination).
    SinkPump(Sink),
}

/// Post-spawn plumbing for an external child: how stdout is drained, how
/// stderr is captured, and whether stdout bytes are tee'd into an audit
/// buffer.  Both standalone exec and pipeline external stages compute
/// one of these and hand it to [`RunningChild::assemble`], so the
/// `Drop` / wait / drain rules of `RunningChild` cannot be re-derived
/// at each call site.
pub(crate) struct ExternalPlumbing {
    /// Base sink for child stdout when piped.  `None` means stdout was
    /// inherited or wired through a direct OS pipe (next pipeline stage's
    /// stdin) and no pump is needed.
    pub stdout_pump: Option<Sink>,
    /// Stderr disposition — see [`StderrCapture`].
    pub stderr: StderrCapture,
    /// Whether the active context is recording an audit tree.  When `true`
    /// and `stdout_pump` is `Some(base)`, [`audit_tee`] wraps `base` in a
    /// private capture buffer that the audit node will read after wait.
    pub auditing: bool,
}

impl RunningChild {
    /// Assemble a `RunningChild` from a freshly-spawned child plus a
    /// per-call plumbing plan.  Single source of truth for the
    /// stderr-reader / stderr-pump / stdout-pump / audit-tee triple, used
    /// by both [`exec_external`] and the pipeline external stage launcher.
    pub(crate) fn assemble(
        mut child: std::process::Child,
        pgid: Option<crate::signal::Pgid>,
        name: String,
        plumbing: ExternalPlumbing,
    ) -> Self {
        let ExternalPlumbing {
            stdout_pump,
            stderr,
            auditing,
        } = plumbing;

        let mut stderr_reader = None;
        let mut stderr_pump = None;
        match stderr {
            StderrCapture::Inherited => {}
            StderrCapture::AuditReader => stderr_reader = spawn_stderr_reader(&mut child),
            StderrCapture::SinkPump(sink) => {
                stderr_pump = child.stderr.take().map(|s| sink.pump(s));
            }
        }

        let (pump, audit_capture) = match stdout_pump {
            Some(base) => {
                let (sink, cap) = audit_tee(base, auditing);
                (child.stdout.take().map(|s| sink.pump(s)), cap)
            }
            None => (None, None),
        };

        Self {
            child: Some(child),
            pgid,
            pump,
            stderr_reader,
            stderr_pump,
            audit_capture,
            name,
        }
    }
}

impl RunningChild {
    /// Wait for the child to terminate via `wait_handling_stop` (which
    /// SIGKILLs the pgid on SIGTSTP so the waiter cannot hang).
    /// Consumes `self`: a second call has nothing to consume, and the
    /// returned `WaitedChild` is the only handle to the drainer threads
    /// from this point on.  On error the `RunningChild` is dropped and
    /// `Drop` runs the SIGKILL+reap+join sequence.
    pub fn wait(mut self) -> Result<WaitedChild, EvalSignal> {
        // Take the child out so Drop's kill/reap sequence is disarmed
        // on the success path.  Construction always sets `Some(_)`.
        let mut child = self.child.take().expect("RunningChild has no child");
        let status = crate::signal::wait_handling_stop(&mut child, self.pgid)
            .map_err(|e| EvalSignal::Error(Error::new(format!("{}: {e}", self.name), 1)))?;
        Ok(WaitedChild {
            status,
            pump: self.pump.take(),
            stderr_reader: self.stderr_reader.take(),
            stderr_pump: self.stderr_pump.take(),
            audit_capture: self.audit_capture.take(),
        })
    }
}

impl WaitedChild {
    /// Join the drainer threads and collect their captured bytes.
    /// Consumes `self`: drainers can be joined exactly once.  Safe by
    /// construction: a `WaitedChild` only exists once the child has been
    /// observed dead, so drainers see pipe-EOF promptly.
    pub fn drain(mut self) -> ChildCaptures {
        if let Some(jh) = self.pump.take() {
            let _ = jh.join();
        }
        let stderr = self
            .stderr_reader
            .take()
            .and_then(|jh| jh.join().ok())
            .unwrap_or_default();
        if let Some(jh) = self.stderr_pump.take() {
            let _ = jh.join();
        }
        let stdout = self
            .audit_capture
            .take()
            .and_then(|b| b.lock().ok().map(|g| g.clone()))
            .unwrap_or_default();
        ChildCaptures { stdout, stderr }
    }
}

impl Drop for RunningChild {
    /// Abort-path cleanup: SIGKILL the pgid (or fall back to the
    /// direct PID), join the drainers, reap.  No-op when `wait`
    /// already took the child — that's the success path's disarm.
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(unix)]
        match self.pgid {
            Some(crate::signal::Pgid(p)) => unsafe {
                libc::kill(-p, libc::SIGKILL);
            },
            None => {
                let _ = child.kill();
            }
        }
        #[cfg(not(unix))]
        let _ = child.kill();
        if let Some(jh) = self.pump.take() {
            let _ = jh.join();
        }
        if let Some(jh) = self.stderr_reader.take() {
            let _ = jh.join();
        }
        if let Some(jh) = self.stderr_pump.take() {
            let _ = jh.join();
        }
        let _ = child.wait();
    }
}

pub(crate) fn render_exec_name(name: &ExecName, shell: &Shell) -> String {
    let home = shell
        .dynamic
        .env_vars()
        .get("HOME")
        .cloned()
        .unwrap_or_else(|| std::env::var("HOME").unwrap_or_default());
    match name {
        ExecName::Bare(name) => name.clone(),
        ExecName::Path(path) => path.clone(),
        ExecName::TildePath(path) => {
            expand_tilde_path(path.user.as_deref(), path.suffix.as_deref(), &home)
        }
    }
}

pub(crate) fn exec_external(
    cmd: &ExecName,
    args: &[Value],
    redirects: &[(u32, RedirectMode, EvalRedirect)],
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    let rc = resolve_command(cmd, args, shell)?;
    let cmd_name = rc.shown.clone();
    let mut command = build_command(&rc, shell);

    let plan = classify_redirects(redirects);
    command.stdin(wire_stdin(shell).into_stdio());
    let (mut atomic_commit, stdout_file_dup) = wire_stdout_file(&mut command, &plan, shell)?;

    let auditing = shell.audit.tree.is_some();
    // When the shell's stdout is a real TTY, inherit it so the child can
    // detect the TTY and enable colour output, pager selection, etc.
    // The exec tree (`auditing`) captures stderr per-node but not stdout,
    // so its presence does not prevent TTY inheritance — audit/try output
    // goes to stderr, not stdout.
    // `Sink::External` (the REPL's rustyline printer) also targets the real
    // fd 1 — for foreground commands the prompt is not drawn, so a direct
    // dup is safe and is the only way `ls`/`grep`/pagers see a TTY.
    let inherit_tty = plan.stdout_file.is_none()
        && shell.io.terminal.startup_stdout_tty
        && matches!(
            shell.io.stdout,
            crate::io::Sink::Terminal | crate::io::Sink::External(_)
        );

    // Wire stdout from the sink before stderr: the Windows `2>&1` arm of
    // `wire_stderr` reads `shell.io.stdout` to clone a writer, and on Unix
    // the `pre_exec` dup2 needs a real stdout fd in place pre-fork.
    let stdout_plan = if plan.stdout_file.is_none() {
        let p = shell.io.stdout.child_stdout(inherit_tty).map_err(pipe_err)?;
        command.stdout(p.stdio);
        p.pump
    } else {
        None
    };
    let needs_pump = stdout_plan.is_some();

    let stderr_piped = wire_stderr(
        &mut command,
        &plan,
        auditing,
        inherit_tty,
        stdout_file_dup,
        shell,
    )?;

    // Debug builds: trace I/O wiring decisions for external commands.
    #[cfg(debug_assertions)]
    {
        let sink = match &shell.io.stdout {
            crate::io::Sink::Terminal => "Terminal",
            crate::io::Sink::External(_) => "External",
            crate::io::Sink::Pipe(_) => "Pipe",
            _ => "Other",
        };
        crate::dbg_trace!(
            "exec",
            "cmd={cmd_name} resolved={} tty=[in:{} out:{} err:{}] \
             sink={sink} audit={auditing} inherit={inherit_tty} pump={needs_pump} \
             interactive={} fg_job={}",
            rc.resolved,
            shell.io.terminal.startup_stdin_tty,
            shell.io.terminal.startup_stdout_tty,
            shell.io.terminal.startup_stderr_tty,
            shell.io.interactive,
            shell.io.interactive && shell.io.terminal.startup_stdin_tty && !needs_pump,
        );
    }

    announce_command_title(&cmd_name, shell);

    // Interactive foreground job: put the child in its own process group and
    // hand it the terminal, matching what PipelineGroup does for multi-stage
    // pipelines.  Without this a child shell that calls setpgid(0,0) at
    // startup (e.g. ral launching ral) would never receive the terminal and
    // would spin forever in claim_terminal().
    // Only real terminal-output jobs become foreground.  An external invoked
    // inside an internal pipeline stage has `shell.io.stdout = Sink::Pipe(...)`;
    // capture buffers, line-framed `watch` blocks, audit tees, and stderr
    // redirects also must not take the tty.  Without the positive whitelist,
    // such a child would call `tcsetpgrp` from a thread, putting ral into a
    // background pgroup whose next read of the tty raises EIO (SIGTTIN is
    // ignored, see `repl.rs`).
    // Foreground claim requires *both* the runtime conditions (interactive
    // shell, tty stdin, terminal stdout, no shell pump) AND an explicit
    // permit from the caller's `JobControl`.  An internal pipeline stage
    // thread runs with `JobControl::pipeline_thread`, so even if its
    // shell.io appears to satisfy the runtime conditions it cannot take
    // foreground — the orchestrator owns that decision.
    #[cfg(unix)]
    let want_fg = shell.io.job_control.may_foreground()
        && shell.io.interactive
        && shell.io.terminal.startup_stdin_tty
        && !needs_pump
        && matches!(
            shell.io.stdout,
            crate::io::Sink::Terminal | crate::io::Sink::External(_)
        );
    // Every spawned external child gets:
    //   * default disposition for SIGINT / SIGQUIT / SIGTSTP / SIGTTIN /
    //     SIGTTOU / SIGPIPE — universal, not foreground-only;
    //   * a pgid policy: NewLeader if this child is taking foreground,
    //     Inherit otherwise (it shares ral's pgid like any background
    //     child).
    //
    // `spawn_with_pgid` is the funnel: it installs the canonical
    // pre_exec (apply pgid + reset signals), spawns, mirrors `setpgid`
    // in the parent to close the race, and hands back the leader pgid.
    #[cfg(unix)]
    let pgid_policy = if want_fg {
        crate::signal::PgidPolicy::NewLeader
    } else {
        crate::signal::PgidPolicy::Inherit
    };
    #[cfg(not(unix))]
    let pgid_policy = crate::signal::PgidPolicy::Inherit;

    let (child, wait_pgid) = spawn_external(&mut command, pgid_policy, shell).map_err(|e| {
        io_error(
            &cmd_name,
            e,
            127,
            Some(crate::compat::not_found_hint(&cmd_name)),
        )
    })?;

    // Hand the terminal to the child via a `ForegroundGuard`: the guard's
    // `Drop` restores ral's pgid no matter how this function returns.  The
    // shell ignores SIGTTIN, so a missed restore would put the next REPL
    // read into EIO — making the restore RAII-managed is the only reliable
    // way to plug every early-return path between here and `child.wait()`.
    #[cfg(unix)]
    let _fg_guard = if want_fg {
        let child_pid = child.id() as libc::pid_t;
        crate::signal::ForegroundGuard::try_acquire(child_pid, shell)
    } else {
        None
    };

    // Resolve the stderr disposition into the unified `StderrCapture`
    // enum — a bounded reader for the audit tree when auditing is on and
    // stderr was actually piped, a pump into a non-default `shell.io.stderr`
    // sink otherwise, or `Inherited` (file / 2>&1 / default fd 2).
    let stderr_capture = if auditing && !plan.stderr_to_stdout && plan.stderr_file.is_none() {
        StderrCapture::AuditReader
    } else if !auditing && stderr_piped {
        StderrCapture::SinkPump(shell.io.stderr.try_clone().map_err(pipe_err)?)
    } else {
        StderrCapture::Inherited
    };

    // Hand ownership of the in-flight resources to a `RunningChild`
    // through the shared assembly funnel.  From here on, any error path
    // triggers its `Drop` which SIGKILLs the pgid and reaps — no manual
    // cleanup needed.
    let running = RunningChild::assemble(
        child,
        wait_pgid,
        cmd_name.clone(),
        ExternalPlumbing {
            stdout_pump: stdout_plan,
            stderr: stderr_capture,
            auditing,
        },
    );

    // Wait consumes RunningChild; the resulting `WaitedChild` is the
    // only handle that lets us read `status` or `drain` captures.  All
    // post-wait work below thus carries a borrow-check proof of
    // "child has been observed dead".
    let waited = running.wait()?;
    let status = waited.status;

    // Atomic-redirect commit: child completed (any exit code, but not killed).
    // status.code().is_some() is true on normal exit; None for signal-killed,
    // in which case dropping `atomic_commit` removes the tmp file.
    if status.code().is_some()
        && let Some(commit) = atomic_commit.take()
    {
        commit
            .commit()
            .map_err(|e| EvalSignal::Error(Error::new(format!("atomic write: {e}"), 1)))?;
    }

    // Foreground is restored by `_fg_guard`'s `Drop` when this function
    // returns — see the binding above.

    let captures = waited.drain();
    if !captures.stderr.is_empty() {
        shell.audit.captured_stderr = captures.stderr;
    }
    if !captures.stdout.is_empty() {
        shell.audit.captured_stdout = captures.stdout;
    }
    let code = exit_code(status);
    shell.control.last_status = code;
    if code == 0 {
        Ok(Value::Unit)
    } else {
        Err(EvalSignal::Error(external_exit_error(
            &cmd_name,
            code,
            crate::diagnostic::SourceLoc {
                file: String::new(),
                line: shell.location.line,
                col: shell.location.col,
                len: 0,
            },
            shell,
        )))
    }
}

/// State to undo `apply_redirects`: dup'd backup fds plus pending atomic commits.
pub(crate) struct RedirectGuard {
    saved: Vec<(u32, i32)>,
    commits: Vec<AtomicCommit>,
}

/// Read+File redirects to fd 0 are owned by `shell.io.stdin` (set up by
/// [`install_stdin_redirect`]); `apply_redirects` and `wire_stdin` must agree
/// to leave them alone.  This predicate is the single point where that rule
/// is named.
fn is_stdin_file_redirect(r: &(u32, RedirectMode, EvalRedirect)) -> bool {
    matches!(r, (0, RedirectMode::Read, EvalRedirect::File(_)))
}

/// Open `<file` redirects to fd 0 and park the file in `shell.io.stdin`.
///
/// Returns a [`StdinRedirectGuard`] whose `restore` puts back whatever Source
/// was previously installed (a pipeline pipe, an outer redirect, or
/// Terminal).  When several `<file` redirects target fd 0, the last one wins
/// — same as POSIX shells.  No-op when no such redirect is present, in which
/// case `shell.io.stdin` is left untouched.
///
/// Routing `<file` through `Source` rather than `dup2` is what keeps the
/// cached `startup_stdin_tty` from lying to downstream consumers (codecs,
/// `lines`): they consult the cache only when `Source` is `Terminal`, and
/// `Terminal` truly does mean "fall through to the inherited fd 0".
pub(crate) fn install_stdin_redirect(
    redirects: &[(u32, RedirectMode, EvalRedirect)],
    shell: &mut Shell,
) -> Result<StdinRedirectGuard, EvalSignal> {
    let Some(path) = redirects
        .iter()
        .rev()
        .find_map(|r| match r {
            (0, RedirectMode::Read, EvalRedirect::File(p)) => Some(p),
            _ => None,
        })
    else {
        return Ok(StdinRedirectGuard::Untouched);
    };
    shell.check_fs_read(path)?;
    let resolved = shell.resolve_path(path);
    let f = std::fs::File::open(&resolved).map_err(|e| io_error(path, e, 1, None))?;
    let prior = std::mem::replace(&mut shell.io.stdin, crate::io::Source::File(f));
    Ok(StdinRedirectGuard::Installed(prior))
}

/// Restore-on-exit token for [`install_stdin_redirect`].
pub(crate) enum StdinRedirectGuard {
    Untouched,
    Installed(crate::io::Source),
}

impl StdinRedirectGuard {
    pub(crate) fn restore(self, shell: &mut Shell) {
        if let StdinRedirectGuard::Installed(prior) = self {
            shell.io.stdin = prior;
        }
    }
}

/// Apply fd redirects (for builtins that print via stdout/stderr directly).
/// Returns a guard whose `commits` must be fired by `restore_redirects` after
/// the builtin runs successfully.
///
/// Read+File redirects to fd 0 are skipped: they are owned by
/// [`install_stdin_redirect`], not by the dup2 path.
#[cfg(unix)]
pub(crate) fn apply_redirects(
    redirects: &[(u32, RedirectMode, EvalRedirect)],
    shell: &mut Shell,
) -> Result<RedirectGuard, EvalSignal> {
    let mut saved = Vec::new();
    let mut commits = Vec::new();
    for r in redirects {
        if is_stdin_file_redirect(r) {
            continue;
        }
        let (fd, mode, target) = r;
        match target {
            EvalRedirect::File(path) => {
                let effective_mode = if *fd == 2 { stderr_mode(mode) } else { *mode };
                let (file, commit) = open_file(path, &effective_mode, shell)?;
                if let Some(c) = commit {
                    commits.push(c);
                }
                use std::os::unix::io::IntoRawFd;
                let raw = file.into_raw_fd();
                let backup = unsafe { libc::dup(*fd as i32) };
                if backup >= 0 {
                    saved.push((*fd, backup));
                }
                unsafe {
                    libc::dup2(raw, *fd as i32);
                    libc::close(raw);
                }
            }
            EvalRedirect::Fd(target_fd) => {
                let backup = unsafe { libc::dup(*fd as i32) };
                if backup >= 0 {
                    saved.push((*fd, backup));
                }
                unsafe {
                    libc::dup2(*target_fd as i32, *fd as i32);
                }
            }
        }
    }
    Ok(RedirectGuard { saved, commits })
}

#[cfg(not(unix))]
pub(crate) fn apply_redirects(
    _redirects: &[(u32, RedirectMode, EvalRedirect)],
    _env: &Shell,
) -> Result<RedirectGuard, EvalSignal> {
    Ok(RedirectGuard { saved: vec![], commits: vec![] })
}

/// Restore saved fds.  Always run after a redirected builtin returns,
/// regardless of success — otherwise the shell's stdout/stderr stays
/// broken.  Returns the guard's pending atomic commits for the caller
/// to fire (on success) or drop (on failure).
pub(crate) fn restore_redirects(guard: RedirectGuard) -> Vec<AtomicCommit> {
    let RedirectGuard { saved, commits } = guard;
    #[cfg(unix)]
    for (fd, backup) in saved.into_iter().rev() {
        unsafe {
            libc::dup2(backup, fd as i32);
            libc::close(backup);
        }
    }
    #[cfg(not(unix))]
    let _ = saved;
    commits
}

/// Fire pending atomic commits.  Call only on the success path; dropping
/// the Vec on the error path discards each commit's tmp file.
pub(crate) fn commit_atomics(commits: Vec<AtomicCommit>) -> Result<(), EvalSignal> {
    for commit in commits {
        commit
            .commit()
            .map_err(|e| EvalSignal::Error(Error::new(format!("atomic write: {e}"), 1)))?;
    }
    Ok(())
}

/// Pending tmp-to-target rename for an atomic `>` redirect.
///
/// Drop removes the tmp; `commit` fsyncs and renames it into place.
///
/// # The atomic-write recipe, and what each step buys you
///
/// 1. **Resolve symlinks via `canonicalize`.**  Without this, writing
///    to a symlink would replace the link itself with a regular file
///    — silently breaking anyone else who held the link as a path.
///    For new files (no canonical form yet) we fall through to the
///    literal path.  Done in [`open_atomic`].
///
/// 2. **Place the tmp file in the same directory as the target.**
///    `rename(2)` is only atomic within a single filesystem; cross-fs
///    rename returns `EXDEV` and the recipe fails outright.  Using
///    `/tmp` for the staging file would break on every machine where
///    `/tmp` is a separate mount (which is most of them, post-tmpfs).
///    Done in [`open_atomic`].
///
/// 3. **Use a cryptographically random tmp name.**  A predictable
///    name (e.g. `target.tmp`) races with concurrent writers and
///    invites symlink-attack-shaped surprises in shared dirs.
///    `tempfile::Builder` picks the name with `O_EXCL` semantics and
///    RAII-deletes the file if we error out before persisting, so we
///    never leak `.tmp` detritus.  Done in [`open_atomic`].
///
/// 4. **Write the contents, then flush them to disk before rename.**
///    Skip this and a power loss in the window between rename and
///    background data flush leaves a renamed-but-zero-length file:
///    the directory entry was committed, the data blocks weren't.
///    This is the ext4 `data=ordered` bug from 2009 that ate config
///    files all over the Linux desktop (see "Further reading").
///    Step 4a (write) is the redirect's writer; step 4b (flush) is
///    the `sync_all` in [`AtomicCommit::commit`].
///
/// 5. **Copy the target's existing mode onto the tmp before rename
///    (or default to 0644 for new files).**  Skip this and a
///    previously-0600 sensitive file silently becomes 0600 with the
///    *new* owner-only contents — or, on platforms where the default
///    differs, becomes world-readable.  Either way the resulting
///    permissions don't match what the user had before.  Done in
///    [`open_atomic`].
///
/// 6. **Atomic `rename(tmp, target)`.**  Skip this in favour of
///    "truncate target, write" and you reintroduce exactly the bug
///    we're fixing: a crash or `^C` mid-write leaves a half-written
///    file with no recovery path.  Skip it for "copy then unlink"
///    and there's a window where the target doesn't exist; readers
///    racing the swap see `ENOENT`.  Done in [`AtomicCommit::commit`].
///
/// 7. **Best-effort flush the parent directory to disk.**  Skip this
///    and a kernel panic right after the rename can roll back the
///    directory entry: the data is on disk under the tmp name, but
///    on next boot the rename appears never to have happened.
///    Errors here don't roll back the rename, so we ignore them;
///    on platforms that don't support directory-level flush (Windows)
///    the open itself fails and we fall through silently.  Done in
///    [`AtomicCommit::commit`].
///
/// # Things this deliberately does *not* handle
///
/// - **Hardlink fan-out breaks.**  Atomic rename creates a fresh
///   inode; any other names that pointed at the old inode keep its
///   old contents.  Every editor accepts this tradeoff — preserving
///   the inode would mean truncate-and-write, which is non-atomic.
///   Pick one.
/// - **Owner/group, xattrs, ACLs, SELinux contexts** are not copied
///   over.  Kernel default-inheritance handles the common case;
///   explicit copying belongs in a separate "preserve" path if it's
///   ever needed.
/// - **Concurrent writers** race normally — atomic rename gives
///   crash safety, not mutual exclusion.  Last writer wins.
///
/// # New failure mode vs. plain `fs::write`
///
/// Atomic rename requires write permission on the *parent
/// directory*, not just the target file.  Callers that previously
/// relied on file-only write perms will now see `EACCES` here.
/// This is the standard tradeoff for crash safety.
///
/// # Platform notes
///
/// On Linux/macOS the recipe is exact: `rename(2)` swaps the inode
/// even if other processes have the target open, and the open
/// handles keep reading the old inode until close.
///
/// On Windows, `tempfile::persist` calls
/// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, which is atomic on NTFS
/// in the no-observable-intermediate-state sense — but it *fails*
/// (rather than silently swapping) if another process has the target
/// open with a sharing mode that excludes deletion.  Live readers
/// (editors, tail-followers, AV scanners) therefore turn what would
/// be a silent success on Linux into a hard error on Windows.
/// Mode preservation (step 5) and directory-level flush (step 7) are
/// no-ops on Windows: the former because the POSIX permission
/// bitfield doesn't apply to NTFS DACLs, the latter because NTFS
/// commits directory-entry changes via its own journal and there's
/// no API to force it from a regular file handle.
///
/// # Further reading
///
/// - [`rename(2)`](https://man7.org/linux/man-pages/man2/rename.2.html)
///   — atomicity guarantees and the same-filesystem constraint.
/// - [`fsync(2)`](https://man7.org/linux/man-pages/man2/fsync.2.html)
///   — what gets flushed; why the directory-level flush matters.
/// - [Theodore Ts'o, "Don't fear the fsync!"](https://lwn.net/Articles/322823/)
///   — the ext4 `data=ordered` zero-length-after-rename saga that
///   forced flush-before-rename into common practice.
/// - [Pillai et al., "All File Systems Are Not Created Equal"](https://www.usenix.org/conference/osdi14/technical-sessions/presentation/pillai)
///   — academic survey showing how often application code gets
///   crash-safe updates wrong.  This recipe passes their checks.
/// - [Dan Luu, "Files are hard"](https://danluu.com/file-consistency/)
///   — accessible overview of the surprising failure modes.
pub(crate) struct AtomicCommit {
    tmp: tempfile::NamedTempFile,
    target: std::path::PathBuf,
}

impl AtomicCommit {
    pub fn commit(self) -> std::io::Result<()> {
        // (4b) Durably commit data blocks before any directory entry change.
        self.tmp.as_file().sync_all()?;
        // (6) Atomic rename.  `tempfile::persist` calls `rename(2)`; on
        // success the tmp's RAII cleanup is disarmed automatically.
        self.tmp.persist(&self.target).map(|_| ()).map_err(|e| e.error)?;
        // (7) Best-effort directory-level flush.  Errors don't unwind
        // the rename, so we eat them; platforms without directory-level
        // flush (Windows) just fail to open the dir as a regular file.
        if let Some(parent) = self.target.parent().filter(|p| !p.as_os_str().is_empty())
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

/// True for non-existent paths or regular files.  Non-regular files (TTYs,
/// /dev/null, named pipes) get streaming semantics — atomicity is meaningless.
fn atomic_eligible(path: &std::path::Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => meta.file_type().is_file(),
        Err(_) => true,
    }
}

fn open_atomic(
    path: &str,
    target: std::path::PathBuf,
) -> Result<(std::fs::File, AtomicCommit), EvalSignal> {
    // (1) Symlink resolution.  `canonicalise_strict` errors on
    // non-existent paths — for a fresh file the literal target is
    // the right thing.
    let target = crate::path::canon::canonicalise_strict(&target).unwrap_or(target);
    // (2) Same-directory tmp.  `Path::parent` returns `Some("")` for
    // a bare filename; treat empty as "current directory" so the
    // tempfile call doesn't choke.
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    // (3) Random tmp name with RAII-cleanup on early return.  Dot
    // prefix hides it from `ls`; `O_EXCL` semantics inside.
    let tmp = tempfile::Builder::new()
        .prefix(".")
        .suffix(".ral-write.tmp")
        .tempfile_in(parent)
        .map_err(|e| io_error(path, e, 1, None))?;
    // tempfile defaults to 0600 for security; redirects expect umask-style
    // permissions.  Preserve the existing target's mode if it exists; else
    // use 0644.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&target)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or(0o644);
        let mut perms = tmp
            .as_file()
            .metadata()
            .map_err(|e| io_error(path, e, 1, None))?
            .permissions();
        perms.set_mode(mode);
        tmp.as_file()
            .set_permissions(perms)
            .map_err(|e| io_error(path, e, 1, None))?;
    }
    let file = tmp
        .as_file()
        .try_clone()
        .map_err(|e| io_error(path, e, 1, None))?;
    Ok((file, AtomicCommit { tmp, target }))
}

/// Open a redirect target.  Returns the file plus an optional atomic-commit
/// token for `>` redirects to regular files.  Callers must call `.commit()` on
/// the token after the writer completes; dropping it removes the tmp file.
///
/// `>` is atomic for regular files (tmp + fsync + rename), streaming for
/// non-regular targets.  `>~` is always streaming.  `>>` is append.
///
/// Relative paths are resolved against the shell's scoped cwd so that
/// `within [dir: ...]` redirects target the right directory even from
/// builtins, where the host process cwd is not changed.
pub(crate) fn open_file(
    path: &str,
    mode: &RedirectMode,
    shell: &mut Shell,
) -> Result<(std::fs::File, Option<AtomicCommit>), EvalSignal> {
    let resolved = match mode {
        RedirectMode::Read => {
            shell.check_fs_read(path)?;
            shell.resolve_path(path)
        }
        _ => {
            shell.check_fs_write(path)?;
            shell.resolve_path(path)
        }
    };
    match mode {
        RedirectMode::Append => std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&resolved)
            .map(|f| (f, None))
            .map_err(|e| io_error(path, e, 1, None)),
        RedirectMode::StreamWrite => std::fs::File::create(&resolved)
            .map(|f| (f, None))
            .map_err(|e| io_error(path, e, 1, None)),
        RedirectMode::Read => std::fs::File::open(&resolved)
            .map(|f| (f, None))
            .map_err(|e| io_error(path, e, 1, None)),
        RedirectMode::Write => {
            if atomic_eligible(&resolved) {
                let (file, commit) = open_atomic(path, resolved)?;
                Ok((file, Some(commit)))
            } else {
                std::fs::File::create(&resolved)
                    .map(|f| (f, None))
                    .map_err(|e| io_error(path, e, 1, None))
            }
        }
    }
}

/// Propagate `shell.dynamic.env_vars`, working directory, and (under an active grant)
/// strip dynamic-loader overrides before spawning.  Used by `exec_external`
/// and by the pipeline stage builder.
pub fn apply_env(cmd: &mut Command, shell: &Shell) {
    for (k, v) in shell.dynamic.env_vars() {
        cmd.env(k, v);
    }
    if let Some(cwd) = &shell.dynamic.cwd {
        cmd.current_dir(cwd);
    }
    if shell.has_active_capabilities() {
        for var in &["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"] {
            cmd.env_remove(var);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Shell;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn expand_tilde_uses_env_home() {
        let mut shell = Shell::default();
        shell.dynamic.set_env_var("HOME", "/tmp/home");
        assert_eq!(
            render_exec_name(
                &ExecName::TildePath(crate::path::tilde::TildePath {
                    user: None,
                    suffix: Some("/.local/bin/claude".into()),
                }),
                &shell
            ),
            "/tmp/home/.local/bin/claude"
        );
    }

    #[test]
    fn expand_non_tilde_command_name_is_unchanged() {
        let shell = Shell::default();
        assert_eq!(
            render_exec_name(&ExecName::Bare("/usr/local/bin/claude".into()), &shell),
            "/usr/local/bin/claude"
        );
    }

    #[test]
    fn render_path_command_name_is_unchanged() {
        let shell = Shell::default();
        assert_eq!(
            render_exec_name(&ExecName::Path("./bin/claude".into()), &shell),
            "./bin/claude"
        );
    }

    #[test]
    fn policy_names_absolutize_relative_path_execs() {
        // ./configure is denied by exec_dirs (which require absolute
        // paths) unless we also surface the cwd-joined form.
        let mut shell = Shell::default();
        shell.dynamic.cwd = Some("/tmp/jq_src/jq-1.7".into());

        let name = ExecName::Path("./configure".into());
        let resolved = resolve_in_path(&name, &shell);
        let names = exec_policy_names(&name, &shell, &resolved);

        assert_eq!(
            names,
            vec![
                "./configure".to_string(),
                "/tmp/jq_src/jq-1.7/configure".to_string(),
            ],
        );
    }

    #[test]
    fn policy_names_leave_absolute_path_execs_alone() {
        // Already-absolute paths should not duplicate.
        let shell = Shell::default();
        let name = ExecName::Path("/usr/local/bin/configure".into());
        let resolved = resolve_in_path(&name, &shell);
        let names = exec_policy_names(&name, &shell, &resolved);
        assert_eq!(names, vec!["/usr/local/bin/configure".to_string()]);
    }

    #[test]
    fn policy_names_drop_bare_name_when_scoped_path_changes_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("git");
        std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&tool).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tool, perms).unwrap();

        let mut shell = Shell::default();
        shell.dynamic.set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let resolved = resolve_in_path(&ExecName::Bare("git".into()), &shell);
        let names = exec_policy_names(&ExecName::Bare("git".into()), &shell, &resolved);

        assert_eq!(names, vec![resolved]);
    }
}
