//! Linux sandbox: the bubblewrap (`bwrap`) argv that confines one external
//! child.  bwrap is never the payload: its monitor clones a namespace init
//! which forks the payload, whose `setsid` (`--new-session`) makes its pid the
//! group of everything inside — the group ral addresses, read over
//! `--info-fd`; the monitor leads an inert group nobody signals.  The
//! envelope is the child's whole world on every projection: own ipc, uts and
//! cgroup namespaces, a pid namespace with a fresh `/proc` where the host can
//! build one ([`HostEnvelope`]), a seccomp blocklist on x86-64 and aarch64.
//! bwrap has no endpoint filter — `--unshare-net` drops the network namespace
//! whole — so `SandboxProjection::net` is a bit, not a list.

mod host;

pub(crate) use host::HostEnvelope;

use crate::path::{PathShape, Rendered, render_paths};
use crate::types::{FsProjection, SandboxProjection};
use std::os::unix::process::CommandExt;
use std::process::Command;

/// The bubblewrap binary, resolved on `PATH`: every confined launch here execs
/// it, so a host without it can enforce no projection at all.
pub(super) const BWRAP: &str = "bwrap";

/// Build the [`Command`] that runs `name` under `bwrap` for `policy`: binds
/// derived from the policy prefixes, `deny_paths` overlaid last.
///
/// Every name is taken from `policy.rendered()`, so a rule lands on each
/// spelling the kernel might present rather than on the one the grant author
/// happened to write — a deny naming a symlink masks the target the resolved
/// twin names.  `policy.exec` has no counterpart here: bwrap filters mounts
/// and syscalls, not exec by path, so the in-ral gate is the only check.
///
/// `chdir` is the in-sandbox cwd — bwrap starts the child in its
/// mount-namespace root — so a per-command launch passes the target's
/// logical cwd; the profile dump, which spawns nothing, passes `None`.
///
/// `ownership` decides two ties between the session and the envelope:
/// death (`--die-with-parent`) and address (`--info-fd`, returned alongside
/// this `Command`).  A surrendered (detached) launch carries
/// neither: the survivor must not be killed by our death, and there is no
/// session left here to address once we stop watching it.  Confinement
/// itself — the mounts, the seccomp filter — is otherwise identical.
///
/// The render is pure in `host`, so a test can assert an argv for a host it
/// is not running on.
///
/// The second element of the return is `Kept`'s `--info-fd` pipe, whose
/// write end the caller must keep until it has spawned — see [`InfoFd`].
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:bwrap-launch] Builds the bwrap-wrapped external exec image the model launches under a Linux sandbox projection. `finish_command` builds the exec observation for this image, wrapping the whole dispatch, with the resolved argv and exit status when the spawn/wait completes."
)]
pub(crate) fn make_command_with_policy(
    name: &str,
    args: &[String],
    policy: &SandboxProjection,
    chdir: Option<&str>,
    ownership: super::launch::Ownership,
    host: HostEnvelope,
) -> Result<(Command, Option<InfoFd>), String> {
    let rendered = policy.rendered()?;
    let mut c = Command::new(BWRAP);
    // Empty when fs is `Unrestricted`: there the envelope binds `/` wholesale
    // below rather than per prefix.
    let rules = rendered.fs.rules().cloned().unwrap_or_default();
    let mut ro_binds = render_paths(&default_ro_binds())?;
    ro_binds.extend(rules.read_prefixes);
    // bwrap cannot `execvp` what it cannot see, and the default prefixes
    // miss Nix store paths, ~/.cargo/bin and the like — an unbound exe
    // fails with ENOENT inside the sandbox.  Bind the file, not its parent:
    // siblings stay under whatever the caller's `fs:` capability declared.
    if crate::path::is_absolute(name) {
        ro_binds.extend(render_paths(&[name])?);
    }
    ro_binds.sort();
    ro_binds.dedup();
    let mut rw_binds = rules.write_prefixes;
    rw_binds.sort();
    rw_binds.dedup();
    // A name absent on the host binds nothing, so drop it here rather than at
    // each use: what remains is the envelope's mounts.
    ro_binds.retain(|bind| crate::path::exists(bind.as_str()));
    rw_binds.retain(|bind| crate::path::exists(bind.as_str()));

    c.arg("--new-session");
    c.args(["--unshare-ipc", "--unshare-uts", "--unshare-cgroup-try"]);
    if host.private_pids {
        c.arg("--unshare-pid");
    }
    let info_fd = if ownership == super::launch::Ownership::Kept {
        c.arg("--die-with-parent");
        Some(open_info_fd(&mut c)?)
    } else {
        None
    };
    if !policy.net {
        c.arg("--unshare-net");
    }
    if let Some(dir) = chdir {
        c.args(["--chdir", dir]);
    }
    match &rendered.fs {
        FsProjection::Restricted(_) => {
            c.args(["--proc", "/proc"]);
            render_dev(&mut c, host);
            c.args(["--tmpfs", "/tmp"]);
            for bind in &ro_binds {
                if !rw_binds.contains(bind) {
                    c.args(["--ro-bind", bind.as_str(), bind.as_str()]);
                }
            }
            for bind in &rw_binds {
                c.args(["--bind", bind.as_str(), bind.as_str()]);
            }
        }
        FsProjection::Unrestricted => {
            // `--bind` would skip device nodes.  `/proc` goes over the root:
            // a fresh table under `--unshare-pid`, the host's bound without,
            // so `/proc/self` is the payload's own either way.
            c.args(["--dev-bind", "/", "/"]);
            c.args(["--proc", "/proc"]);
        }
    }
    // Masks go on after every bind: last mount wins.  `pinned_dirs` goes
    // unused because a mask is anchored to the inode, so a renamed ancestor
    // carries it — the pin macOS renders explicitly, Linux gets for free.
    let mut denied_binds = rules.deny_paths;
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
    Ok((c, info_fd))
}

/// The envelope's `/dev`: bwrap's `--dev`, or its shape by hand where the
/// host refuses a fresh devpts.
///
/// By hand, `/dev/pts` is the host's, so a pty opened inside is not the
/// envelope's own.  An absent node is dropped: binding one costs the launch.
fn render_dev(c: &mut Command, host: HostEnvelope) {
    if host.virtual_dev {
        c.args(["--dev", "/dev"]);
        return;
    }
    c.args(["--tmpfs", "/dev"]);
    for node in [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
        "/dev/pts",
    ] {
        if crate::path::exists(node) {
            c.args(["--dev-bind", node, node]);
        }
    }
    c.args(["--symlink", "pts/ptmx", "/dev/ptmx"]);
    c.args(["--symlink", "/proc/self/fd", "/dev/fd"]);
    for (fd, name) in ["/dev/stdin", "/dev/stdout", "/dev/stderr"]
        .into_iter()
        .enumerate()
    {
        c.args(["--symlink", &format!("/proc/self/fd/{fd}"), name]);
    }
    // After `/dev`'s tmpfs, or it is buried.
    c.args(["--tmpfs", "/dev/shm"]);
}

/// Fixed fd bwrap's `--info-fd` document travels on, chosen next to the
/// seccomp filter's 100.
const INFO_FD: libc::c_int = 101;

/// Both ends of a `Kept` launch's `--info-fd` pipe.  Our copy of the write
/// end must outlive the fork that gives bwrap its own, then go: a read
/// reaches EOF only once every write end is closed.
pub(super) struct InfoFd {
    reader: os_pipe::PipeReader,
    writer: os_pipe::PipeWriter,
}

impl InfoFd {
    /// The payload's process group — its pid, `--new-session` having made it
    /// a leader — from bwrap's `child-pid`.  `None` if bwrap died before
    /// writing it; never a block, since EOF follows bwrap's own close.
    pub(super) fn payload_pgid(self) -> Option<crate::process::Pgid> {
        #[derive(serde::Deserialize)]
        struct Info {
            #[serde(rename = "child-pid")]
            child_pid: i32,
        }
        let Self { reader, writer } = self;
        drop(writer);
        let info: Info = serde_json::from_reader(reader).ok()?;
        crate::process::Pgid::from_raw(info.child_pid)
    }
}

/// Open the `--info-fd` pipe for a `Kept` launch and register the write
/// end at [`INFO_FD`], `CLOEXEC` cleared so it survives into `bwrap` —
/// the `apply_seccomp` pattern.  `--info-fd` itself is appended here so a
/// caller cannot pass one without the other.
///
/// The `pre_exec` closure captures only the raw fd, so the returned write
/// end must still be open in the parent at fork time for `dup2` to find it.
fn open_info_fd(c: &mut Command) -> Result<InfoFd, String> {
    let (reader, writer) = crate::process::cloexec_pipe().map_err(|e| e.to_string())?;
    let write_fd = std::os::fd::AsRawFd::as_raw_fd(&writer);
    unsafe {
        c.pre_exec(move || {
            if libc::dup2(write_fd, INFO_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(INFO_FD, libc::F_GETFD);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(INFO_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    c.args(["--info-fd", &INFO_FD.to_string()]);
    Ok(InfoFd { reader, writer })
}

/// The mount that masks one denied path, bwrap having no negative path rule.
/// The target's shape forces which one, and getting it wrong costs the launch
/// rather than the deny — `--tmpfs` over a regular file dies in `mkdir` before
/// the body execs — so [`Self::over`] is the only constructor.
///
/// Every mask goes over a name that already exists: a mount bwrap must first
/// `mkdir` is not available to us, for the reason [`Self::LeftAbsent`] gives.
enum DenyMask<'p> {
    /// An empty directory with no permission bits, which refuse the owner as
    /// much as anyone.  The bits are the whole of it: the tmpfs is the
    /// sandboxed uid's own, so a child that deliberately `chmod`s them back
    /// has scratch memory at that name — never the denied directory, and
    /// never the host.  An immutable mode needs a read-only mount, and bwrap
    /// gives that only for a bind, from a source this process would hold.
    EmptyDir(&'p str),
    /// A device node bound without `MS_DEV`: unopenable, `EACCES` either way.
    UnopenableNode(&'p str),
    /// Nothing — no mount lands on a symlink; the resolved twin holds it.
    OnItsTarget,
    /// Nothing — every mask bwrap could lay on a name that does not exist it
    /// must `mkdir` a mountpoint for first: `EROFS` and a dead envelope under
    /// a read-only bind, and under a writable one a mkdir on the host, the
    /// deny creating the very name it forbids (`deny: ['cwd:/.env']` leaving
    /// an `.env` directory in the user's tree).  So the name is left alone: a
    /// read-only bind refuses the only access an absent name has, and under a
    /// writable one the in-process gate stays its single enforcer.
    LeftAbsent,
}

impl<'p> DenyMask<'p> {
    fn over(path: &'p Rendered) -> Self {
        match crate::path::shape(path.as_str()) {
            PathShape::Symlink => Self::OnItsTarget,
            PathShape::NonDir => Self::UnopenableNode(path.as_str()),
            PathShape::Dir => Self::EmptyDir(path.as_str()),
            PathShape::Absent => Self::LeftAbsent,
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
            Self::OnItsTarget | Self::LeftAbsent => {}
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
    #[cfg(target_arch = "x86_64")]
    const JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
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
    // x32 shares AUDIT_ARCH_X86_64 but sets bit 30 in the syscall number,
    // so it must be rejected before any equality compare below is trusted.
    #[cfg(target_arch = "x86_64")]
    {
        prog.insn(JGE_K, 0, 1, 0x4000_0000); // jf=1: skip past the kill
        prog.insn(RET_K, 0, 0, KILL);
    }
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

/// System paths always bound read-only.  `/etc` wholesale is excluded —
/// only the files dynamic linking, name resolution, user lookup and
/// toolchain resolution need.
fn default_ro_binds() -> Vec<String> {
    [
        "/bin",
        "/usr",
        "/lib",
        "/lib64",
        // `/dev` and `/proc` are absent: the mounts emitted first supply
        // minimal versions of both, and a real bind here would shadow them.
        // `/sys` has no bwrap virtual op, so it is bound.
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
    use super::{HostEnvelope, make_command_with_policy};
    use crate::sandbox::launch::Ownership;
    use crate::types::{FsProjection, FsRules, SandboxProjection};
    use std::process::Stdio;

    /// A bare Linux host.
    const WHOLE: HostEnvelope = HostEnvelope {
        private_pids: true,
        virtual_dev: true,
    };

    fn workdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ral-bwrap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create work dir");
        dir
    }

    fn deny_within(dir: &std::path::Path, denied: &[&std::path::Path]) -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                write_prefixes: vec![dir.to_string_lossy().into_owned()],
                deny_paths: denied
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
                ..FsRules::default()
            }),
            net: true,
            exec: crate::types::ExecProjection::default(),
        }
    }

    fn unrestricted() -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Unrestricted,
            net: true,
            exec: crate::types::ExecProjection::default(),
        }
    }

    fn argv_on(host: HostEnvelope, policy: &SandboxProjection, ownership: Ownership) -> Vec<String> {
        make_command_with_policy("/bin/true", &[], policy, None, ownership, host)
            .expect("ASCII paths render")
            .0
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn argv(policy: &SandboxProjection) -> Vec<String> {
        argv_on(WHOLE, policy, Ownership::Kept)
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

    /// One rule under either bind, for two reasons that meet in it: a mask is
    /// a mount, a mount over an absent name needs a mountpoint bwrap must
    /// `mkdir`, and that `mkdir` fails with `EROFS` under a read-only bind —
    /// killing the envelope, as `xdg:config/gcloud` denied on a host with no
    /// gcloud once did — while under a writable identity bind it succeeds on
    /// the host, and the deny creates the name it forbids.
    #[test]
    fn an_absent_deny_is_never_mounted_over() {
        let dir = workdir("deny-absent");
        let config = dir.join("config");
        std::fs::create_dir_all(&config).unwrap();
        // Deliberately not created: the deny target must be absent.
        let under_write = dir.join("secret-not-yet-created");
        let under_read = config.join("gcloud");

        let read_only = SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read_prefixes: vec![config.to_string_lossy().into_owned()],
                deny_paths: vec![under_read.to_string_lossy().into_owned()],
                ..FsRules::default()
            }),
            net: true,
            exec: crate::types::ExecProjection::default(),
        };
        for (bind, denied, policy) in [
            ("writable", &under_write, deny_within(&dir, &[&under_write])),
            ("read-only", &under_read, read_only),
        ] {
            let args = argv(&policy);
            assert!(
                !args.contains(&denied.to_string_lossy().into_owned()),
                "no mount may land on an absent deny beneath a {bind} bind: {args:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Naming a symlink in argv costs the whole launch, so the mask goes on
    /// the resolved twin — which the backend derives at render time, the
    /// projection naming only the link.
    #[test]
    fn a_symlinked_deny_is_masked_at_its_target_and_never_at_the_link() {
        let dir = workdir("deny-link");
        let target = dir.join("id_rsa");
        let link = dir.join("link-to-id_rsa");
        std::fs::write(&target, "PRIVATE KEY").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let args = argv(&deny_within(&dir, &[&link]));
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

    fn run_confined(
        host: HostEnvelope,
        policy: &SandboxProjection,
        script: &str,
    ) -> Option<std::process::Output> {
        let (mut cmd, info_fd) = make_command_with_policy(
            "/bin/sh",
            &["-c".to_string(), script.to_string()],
            policy,
            None,
            Ownership::Kept,
            host,
        )
        .expect("ASCII paths render");
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd.output().ok();
        // Kept open across the fork inside `output()` for `pre_exec`'s `dup2`.
        drop(info_fd);
        out
    }

    /// Whether this host can build a bwrap envelope at all.  Where it cannot
    /// — bwrap absent, or user namespaces unavailable — a spawning test
    /// proves nothing either way and says so on the way out.
    fn envelope_launches(policy: &SandboxProjection) -> bool {
        let control = run_confined(HostEnvelope::probe(), policy, "echo READY");
        if control
            .as_ref()
            .is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains("READY"))
        {
            return true;
        }
        let why = control.map_or_else(
            || "bwrap not found".to_string(),
            |o| String::from_utf8_lossy(&o.stderr).trim().to_string(),
        );
        eprintln!("skipping: this host cannot build a bwrap envelope: {why}");
        false
    }

    /// The whole regression: `xdg:config` readable, `xdg:config/gcloud`
    /// denied, and no gcloud ever installed — the mask over that absent name
    /// had bwrap `mkdir` a mountpoint on a read-only bind, and its `EROFS`
    /// killed every external command under the grant.  The argv was
    /// well-formed throughout, so only a spawn catches it.
    #[test]
    fn an_absent_deny_under_a_read_only_bind_still_lets_the_body_run() {
        let dir = workdir("deny-absent-spawn");
        let config = dir.join("config");
        let work = dir.join("work");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let denied = config.join("gcloud");

        let policy = |deny_paths: Vec<String>| SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read_prefixes: vec![config.to_string_lossy().into_owned()],
                write_prefixes: vec![work.to_string_lossy().into_owned()],
                deny_paths,
                ..FsRules::default()
            }),
            net: true,
            exec: crate::types::ExecProjection::default(),
        };
        if !envelope_launches(&policy(vec![])) {
            return;
        }

        let script = format!(
            "echo READY\n\
             mkdir -p '{denied}' 2>/dev/null || echo DENY-CREATE-REFUSED\n",
            denied = denied.display(),
        );
        let out = run_confined(
            HostEnvelope::probe(),
            &policy(vec![denied.to_string_lossy().into_owned()]),
            &script,
        )
        .expect("spawn bwrap");
        let stdout = String::from_utf8_lossy(&out.stdout);

        assert!(
            stdout.contains("READY"),
            "an absent deny stopped the envelope from launching: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("DENY-CREATE-REFUSED"),
            "the read-only bind must still refuse creating the denied name: {stdout}"
        );
        assert!(
            !denied.exists(),
            "the denied name was created on the host: {}",
            denied.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Building an envelope must leave the host tree exactly as it found it.
    /// Masking an absent deny under a *writable* identity bind had bwrap
    /// `mkdir` its mountpoint straight onto the host: one launch under
    /// `deny: ['cwd:/.env']` and the user's project held an `.env` directory
    /// their own tools could then never write.  Only a spawn sees it — the
    /// argv was well-formed, and the mount was correct inside the namespace.
    #[test]
    fn building_an_envelope_never_creates_a_denied_name_on_the_host() {
        let dir = workdir("deny-absent-spawn-rw");
        let denied = dir.join("not-yet");

        if !envelope_launches(&deny_within(&dir, &[])) {
            return;
        }
        let out = run_confined(
            HostEnvelope::probe(),
            &deny_within(&dir, &[&denied]),
            "echo READY",
        )
        .expect("spawn bwrap");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("READY"),
            "an absent deny stopped the envelope from launching: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !denied.exists(),
            "building the envelope created the denied name on the host: {}",
            denied.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No argv assertion can catch a mask that makes bwrap exit before it
    /// execs anything, so this one spawns the envelope too.
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

        if !envelope_launches(&deny_within(&dir, &[])) {
            return;
        }
        let script = format!(
            "echo READY\n\
             cat '{key}' 2>/dev/null || echo KEY-READ-REFUSED\n\
             echo pwned > '{key}' 2>/dev/null || echo KEY-WRITE-REFUSED\n\
             cat '{config}' 2>/dev/null || echo GIT-READ-REFUSED\n\
             touch '{planted}' 2>/dev/null || echo GIT-WRITE-REFUSED\n\
             chmod 700 '{git}' 2>/dev/null; touch '{planted}' 2>/dev/null\n\
             cat '{readable}' 2>/dev/null\n",
            key = key.display(),
            config = git.join("config").display(),
            git = git.display(),
            planted = git.join("planted").display(),
            readable = readable.display(),
        );
        let out = run_confined(
            HostEnvelope::probe(),
            &deny_within(&dir, &[&key, &git]),
            &script,
        )
        .expect("spawn bwrap");
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

        // The script's last move is the mask's known limit: a child that
        // chmods the tmpfs it owns can write inside it.  What must hold is
        // that none of it reaches the host, which the two assertions below
        // are — the deny is over the real directory, not over the memory a
        // child spends on believing otherwise.
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

    /// The fallback is the same `/dev`, and only where the host refuses one.
    #[test]
    fn a_host_that_refuses_dev_gets_the_same_shape_by_hand() {
        let dir = workdir("dev-render");
        let policy = deny_within(&dir, &[]);

        let mounted = argv(&policy);
        assert!(
            position_of(&mounted, &["--dev", "/dev"]).is_some(),
            "a host that mounts --dev must be given it: {mounted:?}"
        );

        let by_hand = argv_on(
            HostEnvelope {
                virtual_dev: false,
                ..WHOLE
            },
            &policy,
            Ownership::Kept,
        );
        assert!(
            position_of(&by_hand, &["--dev", "/dev"]).is_none(),
            "the refused mount must not be asked for: {by_hand:?}"
        );
        let dev = position_of(&by_hand, &["--tmpfs", "/dev"]).expect("a /dev to fill");
        let shm = position_of(&by_hand, &["--tmpfs", "/dev/shm"]).expect("a fresh /dev/shm");
        assert!(dev < shm, "/dev's tmpfs would bury /dev/shm's: {by_hand:?}");
        for piece in [
            ["--dev-bind", "/dev/null", "/dev/null"],
            ["--dev-bind", "/dev/urandom", "/dev/urandom"],
            ["--symlink", "pts/ptmx", "/dev/ptmx"],
            ["--symlink", "/proc/self/fd/1", "/dev/stdout"],
        ] {
            assert!(
                position_of(&by_hand, &piece).is_some(),
                "the by-hand /dev is missing {piece:?}: {by_hand:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Forced rather than probed: where `--dev` mounts, nothing else would
    /// exercise this arm.
    #[test]
    fn the_by_hand_dev_serves_a_confined_body() {
        let dir = workdir("dev-by-hand");
        let policy = deny_within(&dir, &[]);
        if !envelope_launches(&policy) {
            return;
        }

        let out = run_confined(
            HostEnvelope {
                virtual_dev: false,
                ..HostEnvelope::probe()
            },
            &policy,
            "echo x > /dev/null && echo NULL-OK\n\
             head -c 1 /dev/urandom > /dev/null && echo URANDOM-OK\n\
             : > /dev/shm/probe && echo SHM-OK\n\
             exec 3<>/dev/ptmx && echo PTMX-OK\n",
        )
        .expect("spawn bwrap");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);

        for ok in ["NULL-OK", "URANDOM-OK", "SHM-OK", "PTMX-OK"] {
            assert!(
                stdout.contains(ok),
                "the by-hand /dev is missing {ok}: {stdout} {stderr}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The namespaces key on there being an envelope, the pid one on the host fact.
    #[test]
    fn every_envelope_owns_its_namespaces_and_the_pid_one_where_the_host_allows() {
        let restricted = deny_within(&workdir("namespaces"), &[]);
        let masked = HostEnvelope {
            private_pids: false,
            ..WHOLE
        };
        for (host, private_pids) in [(WHOLE, true), (masked, false)] {
            for policy in [&unrestricted(), &restricted] {
                for ownership in [Ownership::Kept, Ownership::Surrendered] {
                    let args = argv_on(host, policy, ownership);
                    for flag in ["--unshare-ipc", "--unshare-uts", "--unshare-cgroup-try"] {
                        assert!(
                            args.iter().any(|a| a == flag),
                            "{flag} must be on every envelope: {args:?}"
                        );
                    }
                    assert_eq!(
                        args.iter().any(|a| a == "--unshare-pid"),
                        private_pids,
                        "--unshare-pid must follow the host fact alone: {args:?}"
                    );
                }
            }
        }
    }

    /// The bound root carries the host's `/proc`; the fresh mount must land after it.
    #[test]
    fn an_unrestricted_envelope_mounts_proc_over_the_bound_root() {
        let args = argv(&unrestricted());
        let root = position_of(&args, &["--dev-bind", "/", "/"]).expect("the root bind");
        let proc_ = position_of(&args, &["--proc", "/proc"]).expect("a /proc mount");
        assert!(root < proc_, "/proc must be mounted over the bound root: {args:?}");
    }

    /// A kept child's envelope is tied to our death and a surrendered one's
    /// must not be, while every other confinement flag stays identical.
    #[test]
    fn only_the_two_ownership_ties_distinguish_a_surrendered_launch() {
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read_prefixes: vec!["/usr".to_string()],
                ..FsRules::default()
            }),
            net: false,
            exec: crate::types::ExecProjection::default(),
        };
        let kept = argv_on(WHOLE, &policy, Ownership::Kept);
        let surrendered = argv_on(WHOLE, &policy, Ownership::Surrendered);

        assert!(
            kept.contains(&"--die-with-parent".to_string()),
            "a child we keep must not outlive us: {kept:?}"
        );
        assert!(
            kept.windows(2).any(|w| w == ["--info-fd", "101"]),
            "a kept launch must carry an `--info-fd` naming its payload's session: {kept:?}"
        );
        assert!(
            !surrendered.contains(&"--die-with-parent".to_string()),
            "a survivor must not be killed by our death: {surrendered:?}"
        );
        assert!(
            !surrendered.contains(&"--info-fd".to_string()),
            "a surrendered launch has no session left here to address: {surrendered:?}"
        );
        let ownership_ties = ["--die-with-parent", "--info-fd", "101"];
        assert_eq!(
            kept.iter()
                .filter(|a| !ownership_ties.contains(&a.as_str()))
                .collect::<Vec<_>>(),
            surrendered.iter().collect::<Vec<_>>(),
            "the two launches must otherwise be confined identically"
        );
    }

    /// T3 (design doc §6): the grace signal must reach the confined payload's
    /// own session, not bwrap's mortal monitor.  Spawns through the same
    /// `Launch` + `RunningChild` path a real command takes, so the `--info-fd`
    /// leader in `Launch::spawn` is exercised end to end, then cancels with
    /// `Explicit` and checks the trap ran well inside `TEARDOWN_GRACE`.
    ///
    /// The trap's `exit 0` makes the outcome a plain exit; the file is the
    /// witness that the signal arrived.  `Unrestricted`: the ladder keys on
    /// there being an envelope, not on which axis was attenuated.
    #[test]
    fn a_confined_payload_gets_its_grace_signal_not_the_monitors() {
        use crate::process::{CancelCause, CancelScope, PgidPolicy};
        use crate::runtime::command::{ExternalPlumbing, RunningChild};
        use crate::sandbox::LaunchTarget;

        let policy = unrestricted();
        if !envelope_launches(&policy) {
            return;
        }

        let dir = workdir("receipt-grace");
        let flag = dir.join("grace");
        let script = format!(
            "trap 'echo GRACE > {} ; exit 0' TERM\nsleep 30\n",
            flag.display()
        );

        let shell = crate::types::Shell::default();
        let scope = CancelScope::root();
        let mut launch = crate::sandbox::sandboxed_command(
            &policy,
            LaunchTarget::Host { program: "/bin/sh" },
            &["-c".to_string(), script],
            Ownership::Kept,
            &shell,
            &scope,
        )
        .expect("build confined launch");

        let (child, pgid, jail) = launch
            .spawn(PgidPolicy::NewLeader)
            .expect("spawn confined sh");

        let running = RunningChild::assemble_with_owner(
            child,
            "sh".to_string(),
            ExternalPlumbing {
                stdout_pump: None,
                stderr_pump: None,
            },
            pgid,
            scope.clone(),
            jail,
        );

        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            scope.cancel(CancelCause::Explicit);
        });

        let t0 = std::time::Instant::now();
        let waited = running.wait();
        let elapsed = t0.elapsed();
        let (outcome, sent) = (waited.outcome, waited.sent);
        waited.settle();
        canceller.join().expect("canceller thread");

        assert!(
            elapsed.as_secs() < 5,
            "teardown must not fall back to the payload's own 30 s sleep: took {elapsed:?}"
        );
        assert_eq!(
            sent,
            Some(CancelCause::Explicit),
            "the cancel must have reached this wait"
        );
        assert_eq!(
            outcome,
            crate::process::WaitOutcome::Exited(0),
            "the trap's own exit must be reported, not a SIGKILL of an \
             unreached payload"
        );
        assert_eq!(
            std::fs::read_to_string(&flag).unwrap_or_default().trim(),
            "GRACE",
            "the trap must have run: the grace signal reached the payload \
             itself, not just bwrap's monitor"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn field<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
        stdout.lines().find_map(|line| line.strip_prefix(key))
    }

    /// A whole line: the script's own text echoes back in `/proc/1/cmdline`.
    fn emitted(stdout: &str, marker: &str) -> bool {
        stdout.lines().any(|line| line == marker)
    }

    /// Where the host cannot build the namespace, the shared table is
    /// asserted rather than skipped, so a masked host cannot read as a pass.
    #[test]
    fn no_host_pid_is_nameable_inside_the_envelope() {
        let host = HostEnvelope::probe();
        let restricted = deny_within(&workdir("pidns"), &[]);
        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn a host-side sleeper");
        let script = format!(
            "echo READY\n\
             kill -0 {me} 2>/dev/null && echo HOST-PID-VISIBLE\n\
             kill -TERM {sleeper} 2>/dev/null && echo SLEEPER-SIGNALLED\n\
             echo PIDS=$(ls /proc | grep -c '^[0-9]')\n\
             echo INIT=$(tr '\\0' ' ' < /proc/1/cmdline)\n",
            me = std::process::id(),
            sleeper = sleeper.id(),
        );
        for policy in [&unrestricted(), &restricted] {
            if !envelope_launches(policy) {
                continue;
            }
            let out = run_confined(host, policy, &script).expect("spawn bwrap");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains("READY"),
                "the envelope did not launch: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            if host.private_pids {
                assert!(
                    !emitted(&stdout, "HOST-PID-VISIBLE") && !emitted(&stdout, "SLEEPER-SIGNALLED"),
                    "a host pid was nameable inside the envelope: {stdout}"
                );
                assert!(
                    sleeper.try_wait().expect("poll the sleeper").is_none(),
                    "the host-side sleeper was killed from inside the envelope"
                );
                let pids: u32 = field(&stdout, "PIDS=").and_then(|n| n.parse().ok()).expect("a pid count");
                assert!(pids <= 8, "the table inside must be the envelope's own: {stdout}");
                assert!(
                    field(&stdout, "INIT=").is_some_and(|init| init.starts_with("bwrap")),
                    "pid 1 inside must be bwrap's init: {stdout}"
                );
            } else {
                assert!(
                    emitted(&stdout, "HOST-PID-VISIBLE"),
                    "the datum says the table is shared, yet the test's own pid was not nameable: {stdout}"
                );
            }
        }
        let _ = sleeper.kill();
        let _ = sleeper.wait();
    }

    /// A bind of the host's `/proc` under a pid namespace would make
    /// `/proc/self` somebody else's.
    #[test]
    fn proc_self_is_the_payloads_own_on_every_projection() {
        let host = HostEnvelope::probe();
        let restricted = deny_within(&workdir("procself"), &[]);
        let shell = std::fs::canonicalize("/bin/sh").expect("resolve /bin/sh");
        // `read` is a builtin, so `/proc/self` is the shell's; `cat`'s would be its own.
        let script = "echo READY\n\
                      echo SELF=$$\n\
                      read pid _ < /proc/self/stat; echo STAT=$pid\n\
                      echo EXE=$(readlink /proc/$$/exe)\n";
        for policy in [&unrestricted(), &restricted] {
            if !envelope_launches(policy) {
                continue;
            }
            let out = run_confined(host, policy, script).expect("spawn bwrap");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains("READY"),
                "the envelope did not launch: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let own: u32 = field(&stdout, "SELF=").and_then(|n| n.parse().ok()).expect("the shell's pid");
            assert_eq!(
                field(&stdout, "STAT="),
                Some(own.to_string().as_str()),
                "/proc/self must be the reader's own entry: {stdout}"
            );
            assert_eq!(
                field(&stdout, "EXE=").map(std::path::Path::new),
                Some(shell.as_path()),
                "/proc/<pid>/exe must name the shell's own binary: {stdout}"
            );
            if host.private_pids {
                assert!(own < 64, "the shell's pid must be namespace-local: {stdout}");
            }
        }
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn seccomp_filter_rejects_x32_high_bit() {
        // 0x35/0x06 mirror JGE_K/RET_K; k is read from bytes 4..8 of each insn.
        let bytes = super::build_seccomp_filter();
        let insns: Vec<&[u8]> = bytes.chunks(8).collect();
        let has_x32_guard = insns.windows(2).any(|pair| {
            let (insn, next) = (pair[0], pair[1]);
            u16::from_le_bytes([insn[0], insn[1]]) == 0x35
                && u32::from_le_bytes([insn[4], insn[5], insn[6], insn[7]]) == 0x4000_0000
                && u16::from_le_bytes([next[0], next[1]]) == 0x06
                && u32::from_le_bytes([next[4], next[5], next[6], next[7]]) == 0
        });
        #[cfg(target_arch = "x86_64")]
        assert!(
            has_x32_guard,
            "x86-64 must reject the x32 high bit before any equality compare"
        );
        #[cfg(target_arch = "aarch64")]
        assert!(
            !has_x32_guard,
            "aarch64 has no x32 ABI; filter must be unchanged"
        );
    }
}
