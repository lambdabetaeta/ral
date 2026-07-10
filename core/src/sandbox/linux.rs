//! Linux sandbox using bubblewrap (`bwrap`).
//!
//! A ral subprocess is re-exec'd inside `bwrap` when `--sandbox-projection`
//! is provided on startup (see [`super::early_init`]).  External commands
//! spawned by that child inherit the same mount namespace and seccomp
//! filter.
//!
//! On x86-64 and aarch64 a seccomp-BPF filter is additionally applied via a
//! memfd, blocking dangerous syscalls (`ptrace`, `kexec_load`, `bpf`, etc.)
//! while allowing everything else.
//!
//! Network filtering is all-or-nothing: `--unshare-net` removes the network
//! namespace entirely.  [`SandboxProjection::net`] is therefore a boolean
//! allow/deny bit, not an endpoint list.

use crate::types::{FsProjection, SandboxProjection};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Build a [`Command`] that runs `name` inside a `bwrap` sandbox
/// configured by `policy`.
///
/// Read-only and read-write bind mounts are
/// derived from the policy prefixes, with `deny_paths` overlaid read-only
/// after broad writable binds.
///
/// `chdir` is the in-sandbox working directory: bwrap runs the child in
/// its mount-namespace root unless told otherwise, so a per-command
/// launch must pass the target's logical cwd here.  The grant-body
/// re-exec and profile-dump call sites pass `None` (the re-exec'd ral
/// inherits the launcher cwd and threads logical cwd into its own
/// children).
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:bwrap-launch] Builds the bwrap-wrapped external exec image the model launches under a Linux sandbox projection. The exec card is fused onto this image at command::run, which emits the exec event with the resolved argv and exit status when the spawn/wait completes."
)]
pub fn make_command_with_policy(
    name: &str,
    args: &[String],
    policy: &SandboxProjection,
    chdir: Option<&str>,
) -> Command {
    let mut c = Command::new("bwrap");
    let bind_spec = policy.bind_spec();
    let mut ro_binds = default_ro_binds();
    ro_binds.extend(bind_spec.read_prefixes.iter().cloned());
    // Bind the exe *file itself* when `name` is an absolute path — bwrap
    // must be able to see it to `execvp`.  Default bind prefixes cover
    // /bin, /usr, etc.; anything outside them (Nix store paths,
    // ~/.cargo/bin, ...) needs explicit binding, or the exec fails with
    // ENOENT inside the sandbox.  Bind the file, not its parent: sibling
    // executables and configs still fall under the policy declared by
    // the caller's `fs:` capability.
    if crate::path::is_absolute(name) {
        ro_binds.push(name.to_string());
    }
    ro_binds.sort();
    ro_binds.dedup();
    let mut rw_binds = bind_spec.write_prefixes;
    rw_binds.sort();
    rw_binds.dedup();

    c.args(["--die-with-parent", "--new-session"]);
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
            // No fs attenuation in the stack: pass fs through.  bwrap
            // is only here for the seccomp/--die-with-parent envelope;
            // the grant body should see the host filesystem unchanged.
            // `--dev-bind / /` mount-binds the whole tree including
            // device nodes (`--bind` would skip them).
            c.args(["--dev-bind", "/", "/"]);
        }
    }
    // `deny_paths` carve out fully-forbidden subtrees inside otherwise
    // bound prefixes.  bwrap has no negative path rule, so we overlay
    // an empty tmpfs at each denied path; reads find an empty dir,
    // writes land in throwaway memory.  This is the "last mount wins"
    // analogue of Seatbelt's deny rules — both reads and writes denied,
    // matching the FsPolicy docs.
    //
    // The overlay is NOT gated on the path already existing.  Unlike
    // `--bind SRC DEST`, whose source must exist, `--tmpfs DEST` creates
    // its mount point, so an existence guard here would leave a deny path
    // that is absent at sandbox entry but lies under a writable bind
    // unmasked — a spawned child could then create and write it, a gap
    // Seatbelt does not share (its subpath deny matches by path, not
    // inode).
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

/// Build a raw seccomp-BPF filter (array of `sock_filter` structs) that:
///
///  1. Kills the process if the syscall ABI does not match the expected arch.
///  2. Kills the process for each listed dangerous syscall.
///  3. Allows everything else.
///
/// `bwrap` reads the raw `sock_filter` bytes from the fd given to
/// `--seccomp` and constructs the `sock_fprog` internally before calling
/// `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, ...)`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn build_seccomp_filter() -> Vec<u8> {
    // BPF instruction codes used below.
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
    // Reject the wrong ABI before doing anything else.
    prog.insn(LD_W_ABS, 0, 0, ARCH);
    prog.insn(JEQ_K, 1, 0, AUDIT_ARCH); // jt=1: skip the next kill on match
    prog.insn(RET_K, 0, 0, KILL);
    // For each clause: load nr; if nr == k, fall through to KILL; else skip.
    prog.insn(LD_W_ABS, 0, 0, NR);
    for &nr in denied {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "nr is a libc::SYS_* syscall number: small and non-negative, always within u32")]
        prog.insn(JEQ_K, 0, 1, nr as u32); // jf=1: skip past the kill
        prog.insn(RET_K, 0, 0, KILL);
    }
    prog.insn(RET_K, 0, 0, ALLOW);
    prog.into_bytes()
}

/// Tiny accumulator for `sock_filter`-shaped BPF instructions.
/// Each instruction is `(opcode: u16, jt: u8, jf: u8, k: u32)` packed
/// little-endian, exactly the layout the kernel expects.
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

/// Write the seccomp filter into a memfd, `dup2` it to FD 100, and clear
/// `CLOEXEC` so `bwrap` can read FD 100 after exec.  The `pre_exec`
/// closure runs in the forked child right before `bwrap` is exec'd;
/// `bwrap` then applies the filter to itself and every process it spawns.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn apply_seccomp(cmd: &mut Command, filter: Vec<u8>) {
    const SECCOMP_FD: libc::c_int = 100;
    unsafe {
        cmd.pre_exec(move || {
            let name = c"seccomp".as_ptr();
            #[allow(clippy::cast_possible_truncation, reason = "memfd_create returns a small fd or -1, both within c_int")]
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
                #[allow(clippy::cast_sign_loss, reason = "n > 0 is guaranteed: the n < 0 and n == 0 branches return above")]
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

/// Re-exec the current ral process under `bwrap` with `policy` enforced.
/// Blocks until the child exits, returning its status as a `u8`.
pub(super) fn respawn_under_bwrap(
    exe: &Path,
    args: &[String],
    policy: &SandboxProjection,
) -> Result<u8, String> {
    let mut cmd = make_command_with_policy(exe.to_string_lossy().as_ref(), args, policy, None);
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
    // Bwrap respawn is a one-shot helper that cannot be SIGSTOP'd by
    // user code (its only purpose is to bootstrap the sandboxed ral),
    // so the WUNTRACED ceremony of ChildHandle would be pointless here.
    #[allow(clippy::disallowed_methods)]
    let status = child
        .wait()
        .map_err(|e| format!("ral: failed to enter sandbox: {e}"))?;
    #[allow(clippy::cast_sign_loss, reason = "clamp(0, 255) bounds the value to the u8 range before the cast")]
    Ok(status.code().unwrap_or(1).clamp(0, 255) as u8)
}

/// System paths that are always bind-mounted read-only.  `/etc` is
/// intentionally excluded; only the files needed for dynamic linking, name
/// resolution, user lookup, and toolchain resolution are listed.
fn default_ro_binds() -> Vec<String> {
    [
        "/bin",
        "/usr",
        "/lib",
        "/lib64",
        "/dev",
        "/proc",
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
        // getpwuid / getgrgid sit in libc startup paths; without these,
        // many programs can't even resolve $HOME.
        "/etc/passwd",
        "/etc/group",
        // Debian/Ubuntu toolchain symlinks (cc → gcc-13, etc.).
        "/etc/alternatives",
        // Linuxbrew prefix; analogous to /opt/homebrew on macOS.
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
        let cmd = make_command_with_policy("/bin/true", &[], &policy, None);
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

    /// A deny path that does NOT exist at sandbox entry but lies under a
    /// writable bind must still be overlaid: `--tmpfs DEST` creates its mount
    /// point, so the overlay is unconditional.  Gating it on existence (as
    /// the `--bind` source guard does) left such a path unmasked, and a
    /// spawned child could create and write it under the bound subtree.
    #[test]
    fn deny_overlay_is_emitted_for_a_nonexistent_path() {
        let dir =
            std::env::temp_dir().join(format!("ral-bwrap-deny-absent-{}", std::process::id()));
        // Deliberately NOT created — the point is the deny target is absent.
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
        let cmd = make_command_with_policy("/bin/true", &[], &policy, None);
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
}
