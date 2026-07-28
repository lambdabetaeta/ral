//! Linux sandbox: the bubblewrap (`bwrap`) argv that confines a process.
//!
//! Two callers build one envelope. `super::reexec` re-execs ral itself here
//! for a grant body, and `super::launch` wraps a single external child;
//! whatever the re-exec'd ral spawns inherits its mount namespace and
//! seccomp filter.  The filter is applied only on x86-64 and aarch64.
//!
//! bwrap has no endpoint filter — `--unshare-net` drops the network
//! namespace whole — so `SandboxProjection::net` is a bit, not a list.

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
    // bwrap has no negative path rule, so each denied path gets an empty
    // tmpfs laid over it after the binds: last mount wins, reads find an
    // empty dir, writes land in throwaway memory.
    //
    // Deliberately not gated on existence.  `--tmpfs DEST` creates its own
    // mount point (unlike `--bind SRC DEST`), and a guard here would leave
    // a deny path that is absent at entry but under a writable bind
    // unmasked — a child could then create and write it.
    //
    // `bind_spec.pinned_dirs` goes unused here on purpose: a `--tmpfs` mask
    // is a mount bound to the denied path, not to the path string that named
    // it at mount time, so a rename of an ancestor carries the mount — and
    // the mask — along with it.  Only the denied path's own name resists
    // rename or removal, with `EBUSY`: the pin macOS renders explicitly,
    // Linux gets for free.
    let mut denied_binds = bind_spec.deny_paths;
    denied_binds.sort();
    denied_binds.dedup();
    for bind in &denied_binds {
        c.args(["--tmpfs", bind]);
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
    use crate::types::{FsPolicy, FsProjection, SandboxProjection};

    #[test]
    fn denied_paths_are_overlaid_after_rw_binds() {
        let dir = std::env::temp_dir().join(format!("ral-bwrap-deny-test-{}", std::process::id()));
        let denied = dir.join(".exarch.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&denied, "capabilities").unwrap();

        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsPolicy {
                read_prefixes: Vec::new(),
                write_prefixes: vec![dir.to_string_lossy().into_owned().into()],
                deny_paths: vec![denied.to_string_lossy().into_owned().into()],
            }),
            net: true,
            exec: crate::types::ExecProjection::default(),
        };
        let cmd = make_command_with_policy(
            "/bin/true",
            &[],
            &policy,
            None,
            super::launch::Ownership::Kept,
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let bind_pos = args
            .windows(3)
            .position(|w| w[0] == "--bind" && w[1] == dir.to_string_lossy());
        let deny_pos = args
            .windows(2)
            .position(|w| w[0] == "--tmpfs" && w[1] == denied.to_string_lossy());

        assert!(bind_pos.is_some(), "rw bind missing: {args:?}");
        assert!(deny_pos.is_some(), "tmpfs deny overlay missing: {args:?}");
        assert!(
            bind_pos.unwrap() < deny_pos.unwrap(),
            "deny overlay must win"
        );
    }

    /// A deny path absent at entry, yet under a writable bind, must still
    /// be masked — or a child could create it.
    #[test]
    fn deny_overlay_is_emitted_for_a_nonexistent_path() {
        let dir =
            std::env::temp_dir().join(format!("ral-bwrap-deny-absent-{}", std::process::id()));
        // Deliberately not created: the deny target must be absent.
        let denied = dir.join("secret-not-yet-created");
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsPolicy {
                read_prefixes: Vec::new(),
                write_prefixes: vec![dir.to_string_lossy().into_owned().into()],
                deny_paths: vec![denied.to_string_lossy().into_owned().into()],
            }),
            net: true,
            exec: crate::types::ExecProjection::default(),
        };
        let cmd = make_command_with_policy(
            "/bin/true",
            &[],
            &policy,
            None,
            super::launch::Ownership::Kept,
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let deny_pos = args
            .windows(2)
            .position(|w| w[0] == "--tmpfs" && w[1] == denied.to_string_lossy());
        assert!(
            deny_pos.is_some(),
            "absent deny path must still be overlaid with --tmpfs: {args:?}"
        );
    }

    /// A kept child's envelope is tied to our death and a surrendered one's
    /// must not be, while every other confinement flag stays identical.
    #[test]
    fn only_the_parent_death_tie_distinguishes_a_surrendered_launch() {
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsPolicy {
                read_prefixes: vec!["/usr".to_string().into()],
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
        let kept = argv(super::launch::Ownership::Kept);
        let surrendered = argv(super::launch::Ownership::Surrendered);

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
