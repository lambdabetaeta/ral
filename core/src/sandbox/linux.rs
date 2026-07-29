//! Linux sandbox: the bubblewrap (`bwrap`) argv that confines a process.
//!
//! Two callers build one envelope. `super::reexec` re-execs ral itself here
//! for a grant body, and `super::launch` wraps a single external child;
//! whatever the re-exec'd ral spawns inherits its mount namespace and
//! seccomp filter.  The filter is applied only on x86-64 and aarch64.
//!
//! bwrap has no endpoint filter — `--unshare-net` drops the network
//! namespace whole — so `SandboxProjection::net` is a bit, not a list.

use crate::path::PathShape;
use crate::types::{FsProjection, SandboxProjection};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Build the [`Command`] that runs `name` under `bwrap` for `policy`: binds
/// derived from the policy prefixes, `deny_paths` overlaid last.
///
/// `chdir` is the in-sandbox cwd — bwrap starts the child in its
/// mount-namespace root — so a per-command launch passes the target's
/// logical cwd, while the grant-body re-exec and the profile dump pass
/// `None` and let the re-exec'd ral thread cwd into its own children.
///
/// `ownership` decides `--die-with-parent` and nothing else.  The flag is
/// `PR_SET_PDEATHSIG(SIGKILL)` over the whole envelope, so a surrendered
/// (detached) launch must not carry it: the survivor would be killed
/// moments after birth, or never, as the scheduler ordered bwrap's `prctl`
/// against the intermediate's `_exit`.  Confinement is otherwise identical.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:bwrap-launch] Builds the bwrap-wrapped external exec image the model launches under a Linux sandbox projection. The exec card is fused onto this image at command::run, which emits the exec event with the resolved argv and exit status when the spawn/wait completes."
)]
pub(crate) fn make_command_with_policy(
    name: &str,
    args: &[String],
    policy: &SandboxProjection,
    chdir: Option<&str>,
    ownership: super::launch::Ownership,
) -> Command {
    let mut c = Command::new("bwrap");
    let bind_spec = policy.bind_spec();
    let mut ro_binds = default_ro_binds();
    ro_binds.extend(bind_spec.read_prefixes.iter().cloned());
    // bwrap cannot `execvp` what it cannot see, and the default prefixes
    // miss Nix store paths, ~/.cargo/bin and the like — an unbound exe
    // fails with ENOENT inside the sandbox.  Bind the file, not its parent:
    // siblings stay under whatever the caller's `fs:` capability declared.
    if crate::path::is_absolute(name) {
        ro_binds.push(name.to_string());
    }
    ro_binds.sort();
    ro_binds.dedup();
    let mut rw_binds = bind_spec.write_prefixes;
    rw_binds.sort();
    rw_binds.dedup();

    c.arg("--new-session");
    if ownership == super::launch::Ownership::Kept {
        c.arg("--die-with-parent");
    }
    if !policy.net {
        c.arg("--unshare-net");
    }
    if let Some(dir) = chdir {
        c.args(["--chdir", dir]);
    }
    match &policy.fs {
        FsProjection::Restricted(_) => {
            c.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
            for bind in ro_binds {
                if crate::path::exists(&bind) && !rw_binds.iter().any(|w| w == &bind) {
                    c.args(["--ro-bind", &bind, &bind]);
                }
            }
            for bind in &rw_binds {
                if crate::path::exists(bind) {
                    c.args(["--bind", bind, bind]);
                }
            }
        }
        FsProjection::Unrestricted => {
            // Nothing in the stack attenuated fs, so bwrap is here only for
            // the seccomp envelope and the parent-death tie.  `--dev-bind`
            // carries device nodes across; `--bind` would skip them.
            c.args(["--dev-bind", "/", "/"]);
        }
    }
    // Masks go on after every bind: last mount wins.  `pinned_dirs` goes
    // unused because a mask is anchored to the inode, so a renamed ancestor
    // carries it — the pin macOS renders explicitly, Linux gets for free.
    let mut denied_binds = bind_spec.deny_paths;
    denied_binds.sort();
    denied_binds.dedup();
    for bind in &denied_binds {
        DenyMask::over(bind).render(&mut c);
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        let filter = build_seccomp_filter();
        apply_seccomp(&mut c, filter);
        c.args(["--seccomp", "100"]);
    }
    c.arg("--");
    c.arg(name);
    c.args(args);
    c
}

/// The mount that masks one denied path, bwrap having no negative path rule.
/// The target's shape forces which one, and getting it wrong costs the launch
/// rather than the deny — `--tmpfs` over a regular file dies in `mkdir` before
/// the body execs — so [`Self::over`] is the only constructor.
enum DenyMask<'p> {
    /// An empty directory with no permission bits, and the only mask that
    /// brings its own mountpoint, hence the absent case too.
    EmptyDir(&'p str),
    /// A device node bound without `MS_DEV`: unopenable, `EACCES` either way.
    UnopenableNode(&'p str),
    /// Nothing — no mount lands on a symlink; the resolved twin holds it.
    OnItsTarget,
}

impl<'p> DenyMask<'p> {
    fn over(path: &'p str) -> Self {
        match crate::path::shape(path) {
            PathShape::Symlink => Self::OnItsTarget,
            PathShape::NonDir => Self::UnopenableNode(path),
            PathShape::Dir | PathShape::Absent => Self::EmptyDir(path),
        }
    }

    fn render(self, c: &mut Command) {
        match self {
            Self::EmptyDir(path) => {
                c.args(["--perms", "0000", "--tmpfs", path]);
            }
            Self::UnopenableNode(path) => {
                c.args(["--ro-bind", "/dev/null", path]);
            }
            Self::OnItsTarget => {}
        }
    }
}

/// A seccomp-BPF program: kill on an ABI mismatch, kill each denied
/// syscall, allow the rest.  `bwrap` reads these raw `sock_filter` bytes
/// from the `--seccomp` fd and builds the `sock_fprog` itself.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn build_seccomp_filter() -> Vec<u8> {
    const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const RET_K: u16 = 0x06; // BPF_RET | BPF_K
    // SECCOMP_RET_KILL_THREAD; KILL_PROCESS (0x8000_0000) would kill bwrap too.
    const KILL: u32 = 0x0000_0000;
    const ALLOW: u32 = 0x7fff_0000;
    // Offsets into the kernel's seccomp_data struct.
    const NR: u32 = 0;
    const ARCH: u32 = 4;

    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xC000_003E;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xC000_00B7;

    let denied: &[i64] = &[
        libc::SYS_ptrace,
        libc::SYS_kexec_load,
        libc::SYS_perf_event_open,
        libc::SYS_bpf,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_keyctl,
        libc::SYS_add_key,
    ];

    let mut prog = BpfProg::new();
    // Syscall numbers are per-ABI, so the arch check must precede every
    // comparison against `nr` or a foreign ABI renumbers past the denies.
    prog.insn(LD_W_ABS, 0, 0, ARCH);
    prog.insn(JEQ_K, 1, 0, AUDIT_ARCH); // jt=1: skip the next kill on match
    prog.insn(RET_K, 0, 0, KILL);
    prog.insn(LD_W_ABS, 0, 0, NR);
    for &nr in denied {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "nr is a libc::SYS_* syscall number: small and non-negative, always within u32"
        )]
        prog.insn(JEQ_K, 0, 1, nr as u32); // jf=1: skip past the kill
        prog.insn(RET_K, 0, 0, KILL);
    }
    prog.insn(RET_K, 0, 0, ALLOW);
    prog.into_bytes()
}

/// Accumulates `(opcode, jt, jf, k)` instructions packed little-endian,
/// exactly the `sock_filter` layout the kernel expects.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
struct BpfProg(Vec<u8>);

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
impl BpfProg {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn insn(&mut self, code: u16, jt: u8, jf: u8, k: u32) {
        let [c0, c1] = code.to_le_bytes();
        let [k0, k1, k2, k3] = k.to_le_bytes();
        self.0.extend_from_slice(&[c0, c1, jt, jf, k0, k1, k2, k3]);
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

/// Park the filter in a memfd on FD 100 with `CLOEXEC` cleared, so it
/// survives the exec into `bwrap`, which reads it for `--seccomp 100` and
/// applies it to itself and everything it spawns.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn apply_seccomp(cmd: &mut Command, filter: Vec<u8>) {
    const SECCOMP_FD: libc::c_int = 100;
    unsafe {
        cmd.pre_exec(move || {
            let name = c"seccomp".as_ptr();
            #[allow(
                clippy::cast_possible_truncation,
                reason = "memfd_create returns a small fd or -1, both within c_int"
            )]
            let fd = libc::syscall(libc::SYS_memfd_create, name, 0u32) as libc::c_int;
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut written = 0usize;
            while written < filter.len() {
                let n = libc::write(
                    fd,
                    filter[written..].as_ptr().cast::<libc::c_void>(),
                    filter.len() - written,
                );
                if n < 0 {
                    libc::close(fd);
                    return Err(std::io::Error::last_os_error());
                }
                if n == 0 {
                    libc::close(fd);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "seccomp memfd write returned 0",
                    ));
                }
                #[allow(
                    clippy::cast_sign_loss,
                    reason = "n > 0 is guaranteed: the n < 0 and n == 0 branches return above"
                )]
                {
                    written += n as usize;
                }
            }
            if libc::lseek(fd, 0, libc::SEEK_SET) < 0 {
                libc::close(fd);
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(fd, SECCOMP_FD) < 0 {
                libc::close(fd);
                return Err(std::io::Error::last_os_error());
            }
            libc::close(fd);
            libc::fcntl(SECCOMP_FD, libc::F_SETFD, 0i32); // clear CLOEXEC
            Ok(())
        });
    }
}

/// Re-exec this ral process under `bwrap` with `policy` enforced, blocking
/// until it exits.
pub(super) fn respawn_under_bwrap(
    exe: &Path,
    args: &[String],
    policy: &SandboxProjection,
) -> Result<u8, String> {
    // We wait on this one, so its envelope must not outlive an abrupt death.
    let mut cmd = make_command_with_policy(
        exe.to_string_lossy().as_ref(),
        args,
        policy,
        None,
        super::launch::Ownership::Kept,
    );
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "ral: failed to enter sandbox: bwrap not found".to_string()
        } else {
            format!("ral: failed to enter sandbox: {e}")
        }
    })?;
    // A bootstrap helper no user code can name, hence never SIGSTOP, so
    // `ChildHandle`'s WUNTRACED ceremony would buy nothing.
    #[allow(clippy::disallowed_methods)]
    let status = child
        .wait()
        .map_err(|e| format!("ral: failed to enter sandbox: {e}"))?;
    #[allow(
        clippy::cast_sign_loss,
        reason = "clamp(0, 255) bounds the value to the u8 range before the cast"
    )]
    Ok(status.code().unwrap_or(1).clamp(0, 255) as u8)
}

/// System paths always bound read-only.  `/etc` wholesale is excluded —
/// only the files dynamic linking, name resolution, user lookup and
/// toolchain resolution need.
fn default_ro_binds() -> Vec<String> {
    [
        "/bin",
        "/usr",
        "/lib",
        "/lib64",
        // `/dev` and `/proc` are absent: the virtual `--dev`/`--proc` mounts
        // emitted first supply minimal versions, and a real bind here would
        // shadow them.  `/sys` has no bwrap virtual op, so it is bound.
        "/sys",
        "/etc/ld.so.conf",
        "/etc/ld.so.conf.d",
        "/etc/ld.so.cache",
        "/etc/resolv.conf",
        "/etc/nsswitch.conf",
        "/etc/hosts",
        "/etc/ssl",
        "/etc/ca-certificates",
        "/etc/pki",
        // getpwuid/getgrgid sit in libc startup paths: without these many
        // programs cannot even resolve $HOME.
        "/etc/passwd",
        "/etc/group",
        // Debian/Ubuntu toolchain symlinks (cc → gcc-13, etc.).
        "/etc/alternatives",
        // Linuxbrew: a system prefix that happens to live under /home.
        "/home/linuxbrew/.linuxbrew",
    ]
    .iter()
    .filter(|path| crate::path::exists(path))
    .map(|path| (*path).to_string())
    .collect()
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::make_command_with_policy;
    use crate::path::NormalizedPrefix;
    use crate::sandbox::launch::Ownership;
    use crate::types::{FsPolicy, FsProjection, SandboxProjection};
    use std::process::Stdio;

    fn workdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ral-bwrap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create work dir");
        dir
    }

    fn deny_within(dir: &std::path::Path, denied: &[&std::path::Path]) -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsPolicy {
                read_prefixes: Vec::new(),
                write_prefixes: vec![NormalizedPrefix::from_surface(dir)],
                deny_paths: denied.iter().map(NormalizedPrefix::from_surface).collect(),
            }),
            net: true,
            exec: crate::types::ExecProjection::default(),
        }
    }

    fn argv(policy: &SandboxProjection) -> Vec<String> {
        make_command_with_policy("/bin/true", &[], policy, None, Ownership::Kept)
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn position_of(args: &[String], mask: &[&str]) -> Option<usize> {
        args.windows(mask.len()).position(|w| w == mask)
    }

    #[test]
    fn an_existing_file_is_masked_by_an_unopenable_node_after_its_bind() {
        let dir = workdir("deny-file");
        let denied = dir.join(".exarch.toml");
        std::fs::write(&denied, "capabilities").unwrap();

        let policy = deny_within(&dir, &[&denied]);
        let args = argv(&policy);
        let bind = position_of(&args, &["--bind", &dir.to_string_lossy()]);
        let mask = position_of(
            &args,
            &["--ro-bind", "/dev/null", &denied.to_string_lossy()],
        );

        assert!(bind.is_some(), "rw bind missing: {args:?}");
        assert!(
            mask.is_some(),
            "node mask missing for an existing file: {args:?}"
        );
        assert!(bind.unwrap() < mask.unwrap(), "the mask must win");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without `--perms` the mask is writable and a child's write inside it
    /// succeeds into throwaway memory — a lie rather than a deny.
    #[test]
    fn a_directory_is_masked_by_a_tmpfs_with_no_permission_bits() {
        let dir = workdir("deny-dir");
        let denied = dir.join(".git");
        std::fs::create_dir_all(&denied).unwrap();

        let args = argv(&deny_within(&dir, &[&denied]));
        assert!(
            position_of(
                &args,
                &["--perms", "0000", "--tmpfs", &denied.to_string_lossy()]
            )
            .is_some(),
            "a denied directory needs an unwritable tmpfs mask: {args:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A deny absent at entry but under a writable bind must still be masked,
    /// or a child creates it and writes it for real.
    #[test]
    fn an_absent_path_is_masked_so_a_child_cannot_create_it() {
        let dir = workdir("deny-absent");
        // Deliberately not created: the deny target must be absent.
        let denied = dir.join("secret-not-yet-created");

        let args = argv(&deny_within(&dir, &[&denied]));
        assert!(
            position_of(
                &args,
                &["--perms", "0000", "--tmpfs", &denied.to_string_lossy()]
            )
            .is_some(),
            "an absent deny path must still be masked: {args:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Naming a symlink in argv costs the whole launch, so the mask goes on
    /// the resolved twin the projection carries beside it.
    #[test]
    fn a_symlinked_deny_is_masked_at_its_target_and_never_at_the_link() {
        let dir = workdir("deny-link");
        let target = dir.join("id_rsa");
        let link = dir.join("link-to-id_rsa");
        std::fs::write(&target, "PRIVATE KEY").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let args = argv(&deny_within(&dir, &[&link, &target]));
        assert!(
            position_of(
                &args,
                &["--ro-bind", "/dev/null", &target.to_string_lossy()]
            )
            .is_some(),
            "the resolved target must carry the mask: {args:?}"
        );
        assert!(
            !args.iter().any(|a| *a == link.to_string_lossy()),
            "the link's own name must not be mounted over: {args:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn run_confined(policy: &SandboxProjection, script: &str) -> Option<std::process::Output> {
        make_command_with_policy(
            "/bin/sh",
            &["-c".to_string(), script.to_string()],
            policy,
            None,
            Ownership::Kept,
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()
    }

    /// No argv assertion can catch a mask that makes bwrap exit before it
    /// execs anything, so this one spawns the envelope.  It skips where no
    /// envelope can be built at all — bwrap absent, or user namespaces
    /// unavailable — since that proves nothing either way.
    #[test]
    fn a_denied_path_refuses_every_access_while_the_body_still_runs() {
        let dir = workdir("deny-spawn");
        let key = dir.join("id_rsa");
        let git = dir.join(".git");
        let readable = dir.join("README");
        std::fs::write(&key, "PRIVATE-KEY-BYTES").unwrap();
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("config"), "GIT-CONFIG-BYTES").unwrap();
        std::fs::write(&readable, "README-BYTES").unwrap();

        let open = deny_within(&dir, &[]);
        let control = run_confined(&open, "echo READY");
        let launched = control
            .as_ref()
            .is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains("READY"));
        if !launched {
            let why = control.map_or_else(
                || "bwrap not found".to_string(),
                |o| String::from_utf8_lossy(&o.stderr).trim().to_string(),
            );
            eprintln!("skipping: this host cannot build a bwrap envelope: {why}");
            return;
        }

        let script = format!(
            "echo READY\n\
             cat '{key}' 2>/dev/null || echo KEY-READ-REFUSED\n\
             echo pwned > '{key}' 2>/dev/null || echo KEY-WRITE-REFUSED\n\
             cat '{config}' 2>/dev/null || echo GIT-READ-REFUSED\n\
             touch '{planted}' 2>/dev/null || echo GIT-WRITE-REFUSED\n\
             cat '{readable}' 2>/dev/null\n",
            key = key.display(),
            config = git.join("config").display(),
            planted = git.join("planted").display(),
            readable = readable.display(),
        );
        let out = run_confined(&deny_within(&dir, &[&key, &git]), &script).expect("spawn bwrap");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);

        assert!(
            stdout.contains("READY"),
            "the deny masks stopped the envelope from launching, so nothing was confined: {stderr}"
        );
        assert!(
            !stdout.contains("PRIVATE-KEY-BYTES"),
            "a denied file's bytes reached the child: {stdout}"
        );
        assert!(
            !stdout.contains("GIT-CONFIG-BYTES"),
            "a denied directory's contents reached the child: {stdout}"
        );
        for refusal in [
            "KEY-READ-REFUSED",
            "KEY-WRITE-REFUSED",
            "GIT-READ-REFUSED",
            "GIT-WRITE-REFUSED",
        ] {
            assert!(stdout.contains(refusal), "missing {refusal}: {stdout}");
        }
        assert!(
            stdout.contains("README-BYTES"),
            "the grant's own writable prefix stopped being readable: {stdout}"
        );

        assert_eq!(
            std::fs::read_to_string(&key).unwrap(),
            "PRIVATE-KEY-BYTES",
            "the denied file was overwritten on the host"
        );
        assert!(
            !git.join("planted").exists(),
            "a child planted a file inside a denied directory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A kept child's envelope is tied to our death and a surrendered one's
    /// must not be, while every other confinement flag stays identical.
    #[test]
    fn only_the_parent_death_tie_distinguishes_a_surrendered_launch() {
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsPolicy {
                read_prefixes: vec![NormalizedPrefix::from_surface("/usr")],
                write_prefixes: Vec::new(),
                deny_paths: Vec::new(),
            }),
            net: false,
            exec: crate::types::ExecProjection::default(),
        };
        let argv = |ownership| {
            make_command_with_policy("/bin/true", &[], &policy, None, ownership)
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let kept = argv(Ownership::Kept);
        let surrendered = argv(Ownership::Surrendered);

        assert!(
            kept.contains(&"--die-with-parent".to_string()),
            "a child we keep must not outlive us: {kept:?}"
        );
        assert!(
            !surrendered.contains(&"--die-with-parent".to_string()),
            "a survivor must not be killed by our death: {surrendered:?}"
        );
        assert_eq!(
            kept.iter()
                .filter(|a| *a != "--die-with-parent")
                .collect::<Vec<_>>(),
            surrendered.iter().collect::<Vec<_>>(),
            "the two launches must otherwise be confined identically"
        );
    }
}
