//! The handoff from a launch to the confined re-exec of ral that becomes its
//! program, and its only owner: the [`Warrant`] — the confinement to enter,
//! the program to run — on a descriptor no other process can write, and the
//! fixed descriptors every confined launch hands down.
//!
//! The re-exec's whole argv is `ral --warrant`.  The warrant is NUL-terminated
//! fields, with no length anywhere:
//!
//! ```text
//! ral-warrant/1  <confinement>  file|tool  <program>  <argc>  <arg>…
//! ```
//!
//! The decoder accepts only what the encoder emits, byte for byte, so the
//! encoding is canonical and injective.

use crate::capability::{Admitted, Program};
use crate::process::{CommandFailure, SpawnFailure};
use crate::types::Status;
use libc::c_int;
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

/// The descriptors a confined launch hands its child, each at a fixed slot.
/// Every one lies in `98..200` but the admits, which run on from its end: ral
/// hands a confined child nothing else above stderr, so the range is the
/// handoff's by construction, and [`close_handoff`] empties it before the
/// program starts.
const RESERVED: std::ops::Range<c_int> = 98..200;
/// bwrap's `--args`.
#[cfg(target_os = "linux")]
pub(super) const ARGS_FD: c_int = 98;
pub(super) const WARRANT_FD: c_int = 99;
/// bwrap's `--info-fd`.
#[cfg(target_os = "linux")]
pub(super) const INFO_FD: c_int = 100;
/// bwrap's `--add-seccomp-fd`s, one slot each.
#[cfg(target_os = "linux")]
pub(super) const SECCOMP_FD_BASE: c_int = 101;
/// The Landlock exec admits, one slot each.
#[cfg(target_os = "linux")]
pub(super) const ADMIT_FD_BASE: c_int = RESERVED.end;

const MAGIC: &[u8] = b"ral-warrant/1";
/// Under macOS's default socket-buffer ceiling, far above either OS's `ARG_MAX`.
const CAP: usize = 8 << 20;

/// What the confined re-exec enters before it runs anything, spelled as one
/// field of the warrant.
pub(super) trait Confinement: Sized {
    fn spell(&self) -> Cow<'_, str>;
    fn parse(field: &str) -> Result<Self, String>;
    fn enter(&self) -> Result<(), String>;
}

/// macOS: the Seatbelt profile the launch compiled.
#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Seatbelt(pub(super) String);

#[cfg(target_os = "macos")]
impl Confinement for Seatbelt {
    fn spell(&self) -> Cow<'_, str> {
        (&self.0).into()
    }

    fn parse(field: &str) -> Result<Self, String> {
        Ok(Self(field.to_owned()))
    }

    fn enter(&self) -> Result<(), String> {
        super::macos::apply_profile(&self.0)
            .map_err(|e| format!("cannot enter the Seatbelt sandbox: {e}"))
    }
}

/// Linux: how many Landlock exec admits the re-exec inherits from
/// `ADMIT_FD_BASE`, or that exec stays unconfined.
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ExecAdmits {
    Unconfined,
    Inherited(usize),
}

#[cfg(target_os = "linux")]
const UNCONFINED: &str = "unconfined";

#[cfg(target_os = "linux")]
impl Confinement for ExecAdmits {
    fn spell(&self) -> Cow<'_, str> {
        match self {
            Self::Unconfined => UNCONFINED.into(),
            Self::Inherited(n) => n.to_string().into(),
        }
    }

    fn parse(field: &str) -> Result<Self, String> {
        if field == UNCONFINED {
            return Ok(Self::Unconfined);
        }
        field
            .parse()
            .map(Self::Inherited)
            .map_err(|e| format!("the warrant's admit count {field:?}: {e}"))
    }

    fn enter(&self) -> Result<(), String> {
        super::linux::landlock::enter(self)
    }
}

/// The platform's confinement, where a concrete one is named.
#[cfg(target_os = "macos")]
pub(super) type Native = Seatbelt;
#[cfg(target_os = "linux")]
pub(super) type Native = ExecAdmits;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Warrant {
    confinement: Native,
    run: Run,
}

/// The program the re-exec becomes inside its confinement: a host file it
/// `execve`s, by the path the in-process guard judged, or a bundled tool it
/// runs in-process.
#[derive(Debug, PartialEq, Eq)]
enum Run {
    File(PathBuf, Vec<OsString>),
    Tool(String, Vec<OsString>),
}

impl From<&Admitted> for Run {
    fn from(admitted: &Admitted) -> Self {
        let args = admitted.args().iter().map(OsString::from).collect();
        match admitted.program() {
            Program::File { path, .. } => Self::File(path.clone(), args),
            Program::Tool(tool) => Self::Tool(tool.clone(), args),
        }
    }
}

/// The program, once its confinement is entered: the only thing a received
/// warrant yields, so nothing else can run it.
pub(super) struct Confined(Run);

impl Confined {
    /// Become the program: a host file is `execve`d, which returns only when
    /// the kernel refuses, as the exit code that says so; a bundled tool
    /// runs here.
    pub(super) fn run(self) -> u8 {
        match self.0 {
            Run::File(path, args) => exec(&path, args),
            Run::Tool(tool, args) => crate::runtime::pipeline::helper::run_bundled(&tool, args),
        }
    }
}

/// Become the host program at `path`; returns only when `execve` refuses.
fn exec(path: &std::path::Path, args: Vec<OsString>) -> u8 {
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:respawn-spawn] sandbox respawn handoff: builds the Command for the confined re-exec; the surface card fired before this handoff, so the exec itself raises no card."
    )]
    let mut cmd = Command::new(path);
    cmd.args(args);
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:respawn-exec] sandbox respawn handoff: `exec` replaces this process image with the confined target; the surface card fired before this handoff, so the exec itself raises no card."
    )]
    let err = cmd.exec();
    // A kernel refusal of a script is most often its interpreter's, which a
    // script the user can edit cannot carry in with it.
    let hint = (err.kind() == io::ErrorKind::PermissionDenied)
        .then(|| super::carriers::shebang(path))
        .flatten()
        .map(|interp| {
            let interp = interp.display();
            format!(
                "\nhint: its interpreter {interp} is not admitted by the grant: a script you \
                 can edit carries no interpreter of its own — add '{interp}': 'allow' to the \
                 exec grant"
            )
        });
    crate::diagnostic::cmd_error(
        "ral",
        &format!("{}: {err}{}", path.display(), hint.unwrap_or_default()),
    );
    let failure = SpawnFailure::from(&err);
    u8::try_from(Status::Process(CommandFailure::Spawn(failure)).code()).unwrap_or(u8::MAX)
}

impl Warrant {
    pub(super) fn new(confinement: Native, admitted: &Admitted) -> Self {
        Self {
            confinement,
            run: admitted.into(),
        }
    }

    /// The warrant the launch left at [`WARRANT_FD`], its confinement
    /// entered and the handoff closed: the program, and nothing before it.
    pub(super) fn confine() -> Result<Confined, String> {
        let Self { confinement, run } = Self::receive()?;
        confinement.enter()?;
        close_handoff();
        Ok(Confined(run))
    }

    /// This warrant, parcelled for [`WARRANT_FD`].
    pub(super) fn parcel(&self) -> Result<OwnedFd, String> {
        parcel("ral-warrant", &self.encode()?)
            .map_err(|e| format!("sandbox: cannot parcel the warrant: {e}"))
    }

    /// The warrant the launch left at [`WARRANT_FD`], which this takes and
    /// closes.
    fn receive() -> Result<Self, String> {
        // SAFETY: `F_GETFD` only asks whether the slot is open.
        if unsafe { libc::fcntl(WARRANT_FD, libc::F_GETFD) } < 0 {
            return Err(format!(
                "no warrant at fd {WARRANT_FD}: `{}` is how ral starts its own confined \
                 children, never a command to run by hand",
                super::WARRANT_FLAG
            ));
        }
        // SAFETY: open, and installed by the launch for this read alone.
        Self::unparcel(unsafe { OwnedFd::from_raw_fd(WARRANT_FD) })
    }

    /// The warrant behind `fd`, once its channel proves unwritable.
    fn unparcel(fd: OwnedFd) -> Result<Self, String> {
        unwritable(&fd)?;
        let mut bytes = Vec::new();
        std::fs::File::from(fd)
            .take((CAP + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("cannot read the warrant: {e}"))?;
        Self::decode(&bytes)
    }

    fn encode(&self) -> Result<Vec<u8>, String> {
        let (kind, program, args): (&[u8], &[u8], _) = match &self.run {
            Run::File(path, args) => (b"file", path.as_os_str().as_bytes(), args),
            Run::Tool(tool, args) => (b"tool", tool.as_bytes(), args),
        };
        let confinement = self.confinement.spell();
        let argc = args.len().to_string();
        let head = [
            MAGIC,
            confinement.as_bytes(),
            kind,
            program,
            argc.as_bytes(),
        ];
        let bytes = nul_terminated(head.into_iter().chain(args.iter().map(|a| a.as_bytes())))?;
        if bytes.len() > CAP {
            return Err(format!(
                "the warrant is {} bytes, over its {CAP}-byte cap: is the command line that long?",
                bytes.len()
            ));
        }
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let body = bytes
            .strip_suffix(b"\0")
            .ok_or("the warrant does not end in NUL: it was cut short")?;
        let mut fields = body.split(|&b| b == 0);
        if fields.next() != Some(MAGIC) {
            return Err(format!("not a {} warrant", String::from_utf8_lossy(MAGIC)));
        }
        let mut next = |what: &str| {
            fields
                .next()
                .ok_or_else(|| format!("the warrant ends before its {what}"))
        };
        let confinement = Native::parse(text(next("confinement")?)?)?;
        let kind = next("kind")?;
        let program = next("program")?;
        let argc: usize = text(next("argument count")?)?
            .parse()
            .map_err(|e| format!("the warrant's argument count: {e}"))?;
        let args: Vec<OsString> = fields.map(|arg| OsStr::from_bytes(arg).into()).collect();
        if args.len() != argc {
            return Err(format!(
                "the warrant counts {argc} arguments but carries {}",
                args.len()
            ));
        }
        let run = match kind {
            b"file" => Run::File(OsStr::from_bytes(program).into(), args),
            b"tool" => Run::Tool(text(program)?.to_owned(), args),
            other => {
                return Err(format!(
                    "the warrant names an unknown kind of program, {:?}",
                    String::from_utf8_lossy(other)
                ));
            }
        };
        let warrant = Self { confinement, run };
        if warrant.encode()? != bytes {
            return Err("the warrant is not in canonical form".to_string());
        }
        Ok(warrant)
    }
}

fn text(field: &[u8]) -> Result<&str, String> {
    std::str::from_utf8(field).map_err(|e| format!("a warrant field is not UTF-8: {e}"))
}

/// `fields`, each NUL-terminated: the warrant's framing, and bwrap's
/// `--args`.  A field holding a NUL is refused, never truncated.
pub(super) fn nul_terminated<'a>(
    fields: impl IntoIterator<Item = &'a [u8]>,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for field in fields {
        if field.contains(&0) {
            return Err(format!(
                "{:?} holds a NUL byte, which would end it early",
                String::from_utf8_lossy(field)
            ));
        }
        out.extend_from_slice(field);
        out.push(0);
    }
    Ok(out)
}

#[cfg(target_os = "linux")]
const SEALS: rustix::fs::SealFlags = rustix::fs::SealFlags::SHRINK
    .union(rustix::fs::SealFlags::GROW)
    .union(rustix::fs::SealFlags::WRITE)
    .union(rustix::fs::SealFlags::SEAL);

/// `bytes` behind a descriptor no other process can write.  Linux: a sealed
/// memfd, since a same-uid process may reopen any descriptor of ours through
/// `/proc` — sealed, the copy it gets is as read-only as ours.  Reopenable
/// until sealed, so the bytes are read back and checked once nothing can
/// change them; the memfd is left at offset 0, where bwrap reads.
#[cfg(target_os = "linux")]
pub(super) fn parcel(name: &str, bytes: &[u8]) -> io::Result<OwnedFd> {
    use rustix::fs::{MemfdFlags, fcntl_add_seals, memfd_create};
    use std::io::Seek;
    let mut file = std::fs::File::from(memfd_create(
        name,
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?);
    file.write_all(bytes)?;
    fcntl_add_seals(&file, SEALS)?;
    file.rewind()?;
    let mut sealed = Vec::with_capacity(bytes.len());
    (&file)
        .take(bytes.len() as u64 + 1)
        .read_to_end(&mut sealed)?;
    if sealed != bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the parcel changed before it was sealed: did another process open ral's \
             descriptors through /proc?",
        ));
    }
    file.rewind()?;
    Ok(file.into())
}

/// macOS, which has no `/proc`: the read end of a socketpair whose write end
/// is filled and closed here, before any child exists.  Whole or not at all —
/// a socket too small for `bytes` is an error, never a partial parcel.
#[cfg(target_os = "macos")]
pub(super) fn parcel(_name: &str, bytes: &[u8]) -> io::Result<OwnedFd> {
    use rustix::net::sockopt::{set_socket_recv_buffer_size, set_socket_send_buffer_size};
    const SLACK: usize = 64 << 10;
    let (reader, mut writer) = crate::process::cloexec_socketpair()?;
    let unfit = |e: io::Error| {
        io::Error::new(
            e.kind(),
            format!("a {}-byte parcel does not fit a socket: {e}", bytes.len()),
        )
    };
    let room = bytes.len() + SLACK;
    set_socket_send_buffer_size(&writer, room).map_err(|e| unfit(e.into()))?;
    set_socket_recv_buffer_size(&reader, room).map_err(|e| unfit(e.into()))?;
    writer.set_nonblocking(true)?;
    writer.write_all(bytes).map_err(unfit)?;
    Ok(reader.into())
}

/// The child's half of [`parcel`]'s promise, checked before a byte is read.
#[cfg(target_os = "linux")]
fn unwritable(fd: &OwnedFd) -> Result<(), String> {
    let seals = rustix::fs::fcntl_get_seals(fd)
        .map_err(|e| format!("the warrant is not a sealed memfd: {e}"))?;
    if !seals.contains(SEALS) {
        return Err("the warrant's memfd is not sealed against writes; refusing it".to_string());
    }
    rustix::fs::seek(fd, rustix::fs::SeekFrom::Start(0))
        .map(drop)
        .map_err(|e| format!("cannot rewind the warrant: {e}"))
}

#[cfg(target_os = "macos")]
fn unwritable(fd: &OwnedFd) -> Result<(), String> {
    use rustix::fs::{FileType, fstat};
    let stat = fstat(fd).map_err(|e| format!("cannot stat the warrant: {e}"))?;
    if FileType::from_raw_mode(stat.st_mode) == FileType::Socket {
        Ok(())
    } else {
        Err("the warrant is not a socket; refusing it".to_string())
    }
}

/// Hand `cmd`'s child each source at its target slot, through one
/// `pre_exec`.
///
/// Every source is first lifted above every target, so no `dup2` can land on
/// a source not yet moved, in any order.  The hook owns the lent copies; the
/// kept one's comes back instead — a peer whose last copy here closing is
/// its reader's EOF, which only the caller can time.
///
/// # Errors
/// A lift the descriptor limit refuses.
pub(super) fn inherit(
    cmd: &mut Command,
    lent: &[(BorrowedFd<'_>, c_int)],
    kept: Option<(BorrowedFd<'_>, c_int)>,
) -> Result<Option<OwnedFd>, String> {
    let above = lent
        .iter()
        .chain(&kept)
        .map(|&(_, at)| at + 1)
        .max()
        .unwrap_or(0);
    let lift = |&(fd, at): &(BorrowedFd<'_>, c_int)| {
        rustix::io::fcntl_dupfd_cloexec(fd, above)
            .map(|copy| (copy, at))
            .map_err(|e| {
                format!(
                    "sandbox: cannot move a descriptor above {above} for the confined child: \
                     {e}; is the open-file limit (`ulimit -n`) below that?"
                )
            })
    };
    let lent = lent.iter().map(lift).collect::<Result<Vec<_>, _>>()?;
    let kept = kept.as_ref().map(lift).transpose()?;
    let kept_raw = kept.as_ref().map(|(fd, at)| (fd.as_raw_fd(), *at));
    // SAFETY: post-fork, `dup2` alone, which is async-signal-safe.  It clears
    // `CLOEXEC` on the copy it makes; each lifted source keeps its own.
    unsafe {
        cmd.pre_exec(move || {
            let lent = lent.iter().map(|(fd, at)| (fd.as_raw_fd(), *at));
            for (fd, at) in lent.chain(kept_raw) {
                if libc::dup2(fd, at) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(kept.map(|(fd, _)| fd))
}

/// Close whatever the handoff left in [`RESERVED`], so the program inherits
/// none of it.
pub(super) fn close_handoff() {
    for fd in RESERVED {
        // SAFETY: nothing in this process owns a reserved slot; most are
        // already closed, and `EBADF` is ignored.
        unsafe { libc::close(fd) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    const CONFINEMENT: &str = "(version 1)\n(deny default)";
    #[cfg(target_os = "linux")]
    const CONFINEMENT: &str = "3";

    #[cfg(target_os = "macos")]
    const NO_CONFINEMENT: &str = "";
    #[cfg(target_os = "linux")]
    const NO_CONFINEMENT: &str = UNCONFINED;

    fn confinement() -> Native {
        Native::parse(CONFINEMENT).expect("a valid confinement")
    }

    fn file(path: &[u8], args: &[&[u8]]) -> Warrant {
        let args = args.iter().map(|a| OsStr::from_bytes(a).into()).collect();
        Warrant {
            confinement: confinement(),
            run: Run::File(OsStr::from_bytes(path).into(), args),
        }
    }

    fn framed(fields: &[&[u8]]) -> Vec<u8> {
        nul_terminated(fields.iter().copied()).expect("no field holds a NUL")
    }

    #[test]
    fn a_warrant_round_trips_bytes_that_are_not_utf8_and_arguments_of_any_shape() {
        for warrant in [
            file(
                b"/opt/\xffbin/tool",
                &[b"-c", b"echo a  b\nc", b"", b"\xfe\xff"],
            ),
            Warrant {
                confinement: Native::parse(NO_CONFINEMENT).expect("a valid confinement"),
                run: Run::Tool("ls".to_string(), Vec::new()),
            },
        ] {
            let bytes = warrant.encode().expect("encodes");
            assert_eq!(Warrant::decode(&bytes).expect("decodes"), warrant);
        }
    }

    /// The control decodes, so each refusal is its own defect's.
    #[test]
    fn a_malformed_warrant_is_refused() {
        let conf = confinement().spell().into_owned();
        let conf = conf.as_bytes();
        let big = vec![b'a'; CAP];
        let control = framed(&[MAGIC, conf, b"file", b"/bin/sh", b"1", b"x"]);
        assert!(Warrant::decode(&control).is_ok(), "the control must decode");
        for (case, bytes) in [
            ("empty", Vec::new()),
            (
                "another version",
                framed(&[b"ral-warrant/2", conf, b"file", b"/bin/sh", b"0"]),
            ),
            ("cut short", control[..control.len() - 1].to_vec()),
            (
                "an unknown kind",
                framed(&[MAGIC, conf, b"exec", b"/bin/sh", b"1", b"x"]),
            ),
            (
                "an argument too many",
                framed(&[MAGIC, conf, b"file", b"/bin/sh", b"1", b"x", b"y"]),
            ),
            (
                "an argument too few",
                framed(&[MAGIC, conf, b"file", b"/bin/sh", b"2", b"x"]),
            ),
            (
                "a signed count",
                framed(&[MAGIC, conf, b"file", b"/bin/sh", b"+1", b"x"]),
            ),
            (
                "a padded count",
                framed(&[MAGIC, conf, b"file", b"/bin/sh", b"01", b"x"]),
            ),
            (
                "over the cap",
                framed(&[MAGIC, conf, b"file", b"/bin/sh", b"1", &big]),
            ),
        ] {
            assert!(Warrant::decode(&bytes).is_err(), "{case} must be refused");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_admit_count_is_canonical_decimal_or_unconfined() {
        for (field, ok) in [
            ("unconfined", true),
            ("0", true),
            ("12", true),
            ("012", false),
            ("+1", false),
            ("", false),
        ] {
            let bytes = framed(&[MAGIC, field.as_bytes(), b"tool", b"ls", b"0"]);
            assert_eq!(Warrant::decode(&bytes).is_ok(), ok, "{field:?}");
        }
    }

    #[test]
    fn an_interior_nul_is_refused_never_truncated() {
        assert!(file(b"/bin/sh", &[b"a\0b"]).encode().is_err());
        assert!(file(b"/bin/\0sh", &[]).encode().is_err());
    }

    #[test]
    fn a_parcel_reads_back_whole_through_the_childs_check() {
        let warrant = file(b"/bin/sh", &[b"-c", b"true"]);
        let fd = warrant.parcel().expect("parcels");
        assert_eq!(Warrant::unparcel(fd).expect("reads back"), warrant);
    }
}
