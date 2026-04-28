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
use crate::util::expand_tilde_path;
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

fn reject_exec_arg(cmd: &str, arg: &Value, shell: &Shell) -> Option<EvalSignal> {
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
/// Returns the atomic-commit token (`Some` for `>` to a regular file).
fn wire_stdout_file(
    command: &mut Command,
    plan: &RedirectPlan,
    shell: &mut Shell,
) -> Result<Option<AtomicCommit>, EvalSignal> {
    let Some((path, mode)) = &plan.stdout_file else {
        return Ok(None);
    };
    let (file, commit) = open_file(path, mode, shell)?;
    command.stdout(Stdio::from(file));
    Ok(commit)
}

/// Set up the child's stderr according to plan and audit mode.
///
/// On Unix, `2>&1` is realised via `pre_exec` dup2 so the child inherits the
/// actual stdout fd after we've wired stdout.  On non-Unix we fall back to
/// `Stdio::inherit()`, which is less faithful but keeps the tree running.
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
    shell: &mut Shell,
) -> Result<bool, EvalSignal> {
    if plan.stderr_to_stdout {
        // 2>&1: Unix dup2's stderr from stdout post-fork; non-Unix inherits.
        #[cfg(unix)]
        {
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
        #[cfg(not(unix))]
        command.stderr(Stdio::inherit());
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

/// Build the sink that receives the child's stdout.  Under auditing we tee
/// into a private buffer so the exec tree can record what the user also saw.
#[allow(clippy::type_complexity)]
pub(crate) fn build_pump_sink(
    shell: &Shell,
    auditing: bool,
) -> Result<(Sink, Option<Arc<Mutex<Vec<u8>>>>), EvalSignal> {
    if auditing {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Sink::Tee(
            Box::new(Sink::Buffer(buf.clone())),
            Box::new(shell.io.stdout.try_clone().map_err(pipe_err)?),
        );
        Ok((sink, Some(buf)))
    } else {
        Ok((shell.io.stdout.try_clone().map_err(pipe_err)?, None))
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
        let path = match shell.dynamic.env_vars.get("PATH") {
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
    names
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
/// Performs every check that must precede `Command::new`:
///   * tilde / PATH rendering for diagnostics;
///   * `reject_exec_arg` rejection of list/map/thunk/handle/Bytes args;
///   * stringification of the remaining values;
///   * `resolve_in_path` against the shell-scoped PATH;
///   * grant policy name expansion + `check_exec_args`.
///
/// Returns the same diagnostic shape both call sites used to produce
/// independently — there is now one source of truth.
pub(crate) fn resolve_command(
    name: &ExecName,
    args: &[Value],
    shell: &mut Shell,
) -> Result<ResolvedCommand, EvalSignal> {
    let shown = render_exec_name(name, shell);
    for arg in args {
        if let Some(sig) = reject_exec_arg(&shown, arg, shell) {
            return Err(sig);
        }
    }
    let arg_strs: Vec<String> = args.iter().map(|v| v.to_string()).collect();
    let resolved = resolve_in_path(name, shell);
    let policy_names = exec_policy_names(name, shell, &resolved);
    let policy_name_refs: Vec<&str> = policy_names.iter().map(String::as_str).collect();
    shell.check_exec_args(&shown, &policy_name_refs, &arg_strs)?;
    Ok(ResolvedCommand {
        shown,
        resolved,
        args: arg_strs,
    })
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

pub(crate) fn render_exec_name(name: &ExecName, shell: &Shell) -> String {
    let home = shell
        .dynamic
        .env_vars
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
    let mut atomic_commit = wire_stdout_file(&mut command, &plan, shell)?;

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

    let stderr_piped = wire_stderr(&mut command, &plan, auditing, shell)?;

    let needs_pump = if plan.stdout_file.is_none() {
        shell.io
            .stdout
            .wire_command_stdout(&mut command, inherit_tty)
            .map_err(pipe_err)?
    } else {
        false
    };

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
    #[cfg(unix)]
    {
        let pgid_policy = if want_fg {
            crate::signal::PgidPolicy::NewLeader
        } else {
            crate::signal::PgidPolicy::Inherit
        };
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(move || {
                pgid_policy.apply();
                crate::signal::reset_child_signals();
                Ok(())
            });
        }
    }

    let mut child = command.spawn().map_err(|e| {
        io_error(
            &cmd_name,
            e,
            127,
            Some(crate::compat::not_found_hint(&cmd_name)),
        )
    })?;
    if shell.has_active_capabilities() {
        crate::sandbox::apply_child_limits(&child);
    }

    // Hand the terminal to the child via a `ForegroundGuard`: the guard's
    // `Drop` restores ral's pgid no matter how this function returns.  The
    // shell ignores SIGTTIN, so a missed restore would put the next REPL
    // read into EIO — making the restore RAII-managed is the only reliable
    // way to plug every early-return path between here and `child.wait()`.
    //
    // `wait_pgid` is the same value: a foreground job is its own pgid leader,
    // and we need it later for `wait_handling_stop` to SIGKILL the group if
    // the child gets SIGTSTP'd (Ctrl-Z).
    #[cfg(unix)]
    let (_fg_guard, wait_pgid) = if want_fg {
        let child_pid = child.id() as libc::pid_t;
        unsafe { libc::setpgid(child_pid, child_pid) }; // parent-side race guard
        (
            crate::signal::ForegroundGuard::try_acquire(child_pid, shell),
            Some(crate::signal::Pgid(child_pid)),
        )
    } else {
        (None, None)
    };
    #[cfg(not(unix))]
    let wait_pgid: Option<crate::signal::Pgid> = None;

    // Auditing captures stderr into a bounded reader for the exec tree.
    // Otherwise, if stderr is piped (because shell.io.stderr is non-default),
    // spawn a pump that drains into shell.io.stderr.
    let stderr_reader = if auditing && !plan.stderr_to_stdout && plan.stderr_file.is_none() {
        spawn_stderr_reader(&mut child)
    } else {
        None
    };
    let stderr_pump = if !auditing && stderr_piped {
        let sink = shell.io.stderr.try_clone().map_err(pipe_err)?;
        child.stderr.take().map(|stderr| sink.pump(stderr))
    } else {
        None
    };

    // Spawn a pump thread when the shell must read the child's piped stdout.
    // Route bytes into shell.io.stdout (which may be a capture buffer, a pipe,
    // or a tee).  We wait for the child first (to let it exit), then join
    // the pump (which sees EOF once the child's write-end is closed).
    let (pump_handle, audit_buf) = if needs_pump {
        let (sink, audit_buf) = build_pump_sink(shell, auditing)?;
        (
            child.stdout.take().map(|stdout| sink.pump(stdout)),
            audit_buf,
        )
    } else {
        (None, None)
    };

    let status = crate::signal::wait_handling_stop(&mut child, wait_pgid)
        .map_err(|e| EvalSignal::Error(Error::new(format!("{cmd_name}: {e}"), 1)))?;

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

    if let Some(reader) = stderr_reader
        && let Ok(buf) = reader.join()
        && !buf.is_empty()
    {
        shell.audit.captured_stderr = buf;
    }
    if let Some(jh) = pump_handle {
        let _ = jh.join();
    }
    if let Some(jh) = stderr_pump {
        let _ = jh.join();
    }
    if let Some(buf) = audit_buf {
        shell.audit.captured_stdout = buf
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
    }
    let code = exit_code(status);
    shell.control.last_status = code;
    if code == 0 {
        Ok(Value::Unit)
    } else {
        let hint = shell.exit_hints.lookup(&cmd_name, code);
        let mut err = Error::new(format!("{cmd_name}: exited with status {code}"), code)
            .at(shell.location.line, shell.location.col);
        err.hint = hint;
        Err(EvalSignal::Error(err))
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
pub(crate) struct AtomicCommit {
    tmp: tempfile::NamedTempFile,
    target: std::path::PathBuf,
}

impl AtomicCommit {
    pub fn commit(self) -> std::io::Result<()> {
        self.tmp.as_file().sync_all()?;
        self.tmp.persist(&self.target).map(|_| ()).map_err(|e| e.error)
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
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
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
    for (k, v) in &shell.dynamic.env_vars {
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
        shell.dynamic
            .env_vars
            .insert("HOME".into(), "/tmp/home".into());
        assert_eq!(
            render_exec_name(
                &ExecName::TildePath(crate::util::TildePath {
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
    fn policy_names_drop_bare_name_when_scoped_path_changes_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("git");
        std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&tool).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tool, perms).unwrap();

        let mut shell = Shell::default();
        shell.dynamic
            .env_vars
            .insert("PATH".into(), dir.path().to_string_lossy().into_owned());

        let resolved = resolve_in_path(&ExecName::Bare("git".into()), &shell);
        let names = exec_policy_names(&ExecName::Bare("git".into()), &shell, &resolved);

        assert_eq!(names, vec![resolved]);
    }
}
