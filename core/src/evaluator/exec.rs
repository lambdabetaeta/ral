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

/// Routing decisions for a child process's standard streams, derived once
/// from the call-site redirects.  `stderr_to_stdout` captures the `2>&1` fd
/// dup; the file fields carry their `RedirectMode` so `open_file` can pick
/// create-vs-append.
struct RedirectPlan {
    stdin_file: Option<(String, RedirectMode)>,
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
        stdin_file: None,
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
                0 => plan.stdin_file = Some((filename.clone(), *mode)),
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

fn wire_stdin(command: &mut Command, plan: &RedirectPlan, shell: &mut Shell) -> Result<(), EvalSignal> {
    if let Some((path, _)) = &plan.stdin_file {
        shell.check_fs_read(path)?;
        let f = std::fs::File::open(path).map_err(|e| io_error(path, e, 1, None))?;
        command.stdin(Stdio::from(f));
    } else if let Some(reader) = shell.io.stdin.take_pipe() {
        // Pipeline stage: feed piped data as stdin.
        command.stdin(Stdio::from(reader));
    }
    Ok(())
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

/// Spawn an auxiliary thread that drains the child's stderr into a bounded
/// buffer.  Only used under auditing; stderr is truncated at 64 KiB to keep
/// the exec-tree JSON compact.
pub(crate) fn spawn_stderr_reader(
    child: &mut std::process::Child,
) -> Option<std::thread::JoinHandle<Vec<u8>>> {
    child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            if buf.len() > 65536 {
                buf.truncate(65536);
            }
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
    let cmd_name = render_exec_name(cmd, shell);
    for arg in args {
        if let Some(sig) = reject_exec_arg(&cmd_name, arg, shell) {
            return Err(sig);
        }
    }
    let arg_strs: Vec<String> = args.iter().map(|v| v.to_string()).collect();
    let resolved = resolve_in_path(cmd, shell);
    let policy_names = exec_policy_names(cmd, shell, &resolved);
    let policy_name_refs: Vec<&str> = policy_names.iter().map(String::as_str).collect();
    shell.check_exec_args(&cmd_name, &policy_name_refs, &arg_strs)?;
    let mut command = crate::sandbox::make_command(&resolved, &arg_strs, shell);
    apply_env(&mut command, shell);

    let plan = classify_redirects(redirects);
    wire_stdin(&mut command, &plan, shell)?;
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
        && shell.io.terminal.stdout_tty
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
            "cmd={cmd_name} resolved={resolved} tty=[in:{} out:{} err:{}] \
             sink={sink} audit={auditing} inherit={inherit_tty} pump={needs_pump} \
             interactive={} fg_job={}",
            shell.io.terminal.stdin_tty,
            shell.io.terminal.stdout_tty,
            shell.io.terminal.stderr_tty,
            shell.io.interactive,
            shell.io.interactive && shell.io.terminal.stdin_tty && !needs_pump,
        );
    }

    announce_command_title(&cmd_name, shell);

    // Interactive foreground job: put the child in its own process group and
    // hand it the terminal, matching what PipelineGroup does for multi-stage
    // pipelines.  Without this a child shell that calls setpgid(0,0) at
    // startup (e.g. ral launching ral) would never receive the terminal and
    // would spin forever in claim_terminal().
    #[cfg(unix)]
    let fg_job = shell.io.interactive && shell.io.terminal.stdin_tty && !needs_pump;
    #[cfg(unix)]
    if fg_job {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                libc::setpgid(0, 0);
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                libc::signal(libc::SIGTSTP, libc::SIG_DFL);
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
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

    // Give the terminal to the child's process group; reclaim after wait().
    #[cfg(unix)]
    if fg_job {
        let child_pid = child.id() as libc::pid_t;
        unsafe {
            libc::setpgid(child_pid, child_pid); // parent-side race guard
            libc::tcsetpgrp(libc::STDIN_FILENO, child_pid);
        }
    }

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

    let status = child
        .wait()
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

    // Restore terminal foreground to the shell's own process group.
    #[cfg(unix)]
    if fg_job {
        unsafe {
            libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
        }
    }

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

/// Apply fd redirects (for builtins that print via stdout/stderr directly).
/// Returns a guard whose `commits` must be fired by `restore_redirects` after
/// the builtin runs successfully.
#[cfg(unix)]
pub(crate) fn apply_redirects(
    redirects: &[(u32, RedirectMode, EvalRedirect)],
    shell: &mut Shell,
) -> Result<RedirectGuard, EvalSignal> {
    let mut saved = Vec::new();
    let mut commits = Vec::new();
    for (fd, mode, target) in redirects {
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
