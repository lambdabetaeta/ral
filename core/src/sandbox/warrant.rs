//! The handoff from a launch to the confined re-exec of ral that becomes its
//! program, and its only owner: the [`Warrant`] — the confinement to enter,
//! the program to run — on a descriptor no other process can write, and the
//! descriptors every confined launch hands down, each at the number it
//! already holds ([`Handoff`]).
//!
//! The re-exec's whole argv is `ral --warrant <fd>`, the warrant's slot.  The
//! warrant is NUL-terminated fields, with no length anywhere:
//!
//! ```text
//! ral-warrant/3  <confinement>  file  <real>  <argv0>  <excluded>  <argc>  <arg>…
//! ral-warrant/3  <confinement>  tool  <name>                       <argc>  <arg>…
//! ```
//!
//! `<excluded>` names the exclusion covering the launched script's
//! interpreter, empty if none: the parent read the `#!` line once, for the
//! child's diagnosis alone, never as evidence.
//!
//! The decoder accepts only what the encoder emits, byte for byte, so the
//! encoding is canonical and injective.

#[cfg(target_os = "linux")]
use super::linux::landlock::Landlocked;
use super::{ExecProjection, SandboxProjection};
use crate::Role;
use crate::capability::{Admitted, Program, Subject, Withheld};
use crate::path::{Depth, Judged, LeafMode, LexicalPath, Located, Reading, shown, walk};
use crate::process::SpawnFailure;
use crate::site::{Silent, site};
use crate::syntax::quote::quote_word;
use libc::c_int;
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

/// The number a handed-down descriptor holds, the same in parent and child:
/// from [`Self::FIRST`] up, so the child's sweep reaches it.  Only a
/// [`Handoff`] makes one, or a child parsing what its parent wrote.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Slot(c_int);

impl Slot {
    /// Everything from here up is the handoff's: ral hands a confined child
    /// nothing else above stderr.
    pub(super) const FIRST: c_int = 98;

    pub(super) const fn fd(self) -> c_int {
        self.0
    }

    /// The slot `field` spells as [`fmt::Display`] does: plain digits, from
    /// [`Self::FIRST`] up.
    pub(super) fn parse(field: &str) -> Result<Self, String> {
        (field.parse::<c_int>().ok())
            .filter(|&at| at >= Self::FIRST && at.to_string() == field)
            .map(Self)
            .ok_or_else(|| {
                format!(
                    "{field:?} names no handoff descriptor, which is a number from {} up \
                     in plain digits",
                    Self::FIRST
                )
            })
    }

    /// The descriptor the launch left here, now the caller's; `None` where
    /// nothing arrived.
    pub(super) fn take(self) -> Option<OwnedFd> {
        let at = self.fd();
        // SAFETY: `F_GETFD` only asks whether the slot is open.
        (unsafe { libc::fcntl(at, libc::F_GETFD) } >= 0)
            // SAFETY: open, and lent by the launch to this one taker.
            .then(|| unsafe { OwnedFd::from_raw_fd(at) })
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

const MAGIC: &[u8] = b"ral-warrant/3";
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

/// Linux: the Landlock ruleset the launch promised, at the slot it names,
/// or none.
#[cfg(target_os = "linux")]
impl Confinement for Option<Landlocked> {
    fn spell(&self) -> Cow<'_, str> {
        match self {
            None => "unconfined".into(),
            Some(Landlocked { refer_root, at }) => {
                let refer = if *refer_root { "+refer" } else { "" };
                format!("landlock{refer}@{at}").into()
            }
        }
    }

    fn parse(field: &str) -> Result<Self, String> {
        if field == "unconfined" {
            return Ok(None);
        }
        let (kind, at) = field.split_once('@').unwrap_or((field, ""));
        let refer_root = match kind {
            "landlock" => false,
            "landlock+refer" => true,
            _ => {
                return Err(format!(
                    "the warrant's confinement {field:?} is not unconfined, landlock@<fd> \
                     or landlock+refer@<fd>"
                ));
            }
        };
        Ok(Some(Landlocked {
            refer_root,
            at: Slot::parse(at)?,
        }))
    }

    fn enter(&self) -> Result<(), String> {
        self.as_ref().map_or(Ok(()), Landlocked::enter)
    }
}

/// The platform's confinement, where a concrete one is named.
#[cfg(target_os = "macos")]
pub(super) type Native = Seatbelt;
#[cfg(target_os = "linux")]
pub(super) type Native = Option<Landlocked>;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Warrant {
    confinement: Native,
    run: Run,
}

/// The program the re-exec becomes inside its confinement: a host file it
/// `execve`s, by the real path the in-process guard judged and under the
/// spelling the user wrote, or a bundled tool it runs in-process.  A file
/// carries the exclusion that covers its script's interpreter, if any, for the
/// hint after a refused `execve`.
#[derive(Debug, PartialEq, Eq)]
enum Run {
    File {
        real: PathBuf,
        arg0: PathBuf,
        excluded: Option<String>,
        args: Vec<OsString>,
    },
    Tool(String, Vec<OsString>),
}

impl Run {
    fn of(admitted: &Admitted, projection: &SandboxProjection) -> Self {
        let args = admitted.args().iter().map(OsString::from).collect();
        match admitted.program() {
            Program::File { arg0, located } => Self::File {
                real: located.real().as_path().to_path_buf(),
                arg0: arg0.clone(),
                excluded: interpreter_exclusion(located, projection),
                args,
            },
            Program::Tool(tool) => Self::Tool(tool.clone(), args),
        }
    }
}

/// The exclusion, on either axis, that covers the interpreter of the `#!`
/// script `program`, read through the handle that was judged.  For the hint
/// alone: support is the projection's, and nothing here is evidence.
fn interpreter_exclusion(
    program: &Located<Judged<Reading>>,
    projection: &SandboxProjection,
) -> Option<String> {
    let spelled = LexicalPath::absolute(&super::carriers::launched_shebang(program)?)?;
    let walked = walk(&spelled, LeafMode::Follow).ok()?;
    let trace = walked.trace();
    trace.kind()?;
    let at = crate::path::Subject::Access(trace);
    let exec = match &projection.exec {
        ExecProjection::Restricted(rules) => rules.withheld(Subject::Program(at)),
        ExecProjection::Unrestricted => None,
    }
    .and_then(|w| match w {
        Withheld::Excluded { scope, .. } => Some(scope.to_string()),
        Withheld::MissingAllowance => None,
    });
    exec.or_else(|| {
        let fs = projection.fs.rules()?;
        let denied = fs
            .deny
            .iter()
            .find(|p| p.real.matches(at, Depth::Within).is_some());
        denied.map(|p| format!("`absolute {}", quote_word(p.written.as_str())))
    })
}

/// The program, once its confinement is entered: the only thing a received
/// warrant yields, so nothing else can run it.
pub(super) struct Confined(Run);

/// The `arg0` and exclusion fields only a file carries.
type FileFields<'a> = [&'a [u8]; 2];

impl Confined {
    /// Become the program: a host file is `execve`d, which returns only when
    /// the kernel refuses, as the exit code that says so; a bundled tool
    /// runs here.
    pub(super) fn run(self) -> u8 {
        match self.0 {
            Run::File {
                real,
                arg0,
                excluded,
                args,
            } => exec(&real, &arg0, excluded.as_deref(), args),
            Run::Tool(tool, args) => crate::uutils::run(tool.as_ref(), args).unwrap_or_else(|e| {
                crate::terminal::cmd_error("ral", &e.to_string());
                127
            }),
        }
    }
}

/// Become the host program at `real`, which sees `arg0` as its own name;
/// returns only when `execve` refuses.
fn exec(
    real: &std::path::Path,
    arg0: &std::path::Path,
    excluded: Option<&str>,
    args: Vec<OsString>,
) -> u8 {
    let mut cmd = site!(Silent::RespawnSpawn, Command::new(real));
    cmd.arg0(arg0).args(args);
    let err = site!(Silent::RespawnExec, &mut cmd => exec());
    // A kernel refusal of a script is most often its interpreter's, which a
    // script the user can edit cannot carry in with it.
    let hint = (err.kind() == io::ErrorKind::PermissionDenied)
        .then(|| super::carriers::shebang(real))
        .flatten()
        .map(|interp| {
            let interp = quote_word(&crate::path::shown(interp)).into_owned();
            match excluded {
                Some(scope) => format!(
                    "\nhint: its interpreter {interp} is excluded by {}; an allowance \
                     cannot override an exclusion",
                    crate::text::code(scope)
                ),
                None => format!(
                    "\nhint: its interpreter {interp} is not admitted by the grant: a script \
                     you can edit carries no interpreter of its own: add `` `absolute {interp} `` \
                     to the exec grant's allow list"
                ),
            }
        });
    crate::terminal::cmd_error(
        "ral",
        &format!("{}: {err}{}", shown(real), hint.unwrap_or_default()),
    );
    SpawnFailure::from(&err).code()
}

impl Warrant {
    pub(super) fn new(
        confinement: Native,
        admitted: &Admitted,
        projection: &SandboxProjection,
    ) -> Self {
        Self {
            confinement,
            run: Run::of(admitted, projection),
        }
    }

    /// The warrant the launch left at `at`, its confinement entered and the
    /// handoff closed: the program, and nothing before it.
    pub(super) fn confine(at: Slot) -> Result<Confined, String> {
        let Self { confinement, run } = Self::receive(at)?;
        confinement.enter()?;
        close_handoff()?;
        Ok(Confined(run))
    }

    /// This warrant, parcelled for a [`Handoff`].
    pub(super) fn parcel(&self) -> Result<OwnedFd, String> {
        parcel("ral-warrant", &self.encode()?)
            .map_err(|e| format!("sandbox: cannot parcel the warrant: {e}"))
    }

    /// The warrant the launch left at `at`, which this takes and closes.
    fn receive(at: Slot) -> Result<Self, String> {
        let fd = at.take().ok_or_else(|| {
            format!(
                "no warrant at fd {at}: `{}` is how ral starts its own confined children, \
                 never a command to run by hand",
                Role::Warrant.flag()
            )
        })?;
        Self::unparcel(fd)
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
        let (kind, program, file, args): (&[u8], &[u8], Option<FileFields>, _) = match &self.run {
            Run::File {
                real,
                arg0,
                excluded,
                args,
            } => (
                b"file",
                real.as_os_str().as_bytes(),
                Some([
                    arg0.as_os_str().as_bytes(),
                    excluded.as_deref().unwrap_or_default().as_bytes(),
                ]),
                args,
            ),
            Run::Tool(tool, args) => (b"tool", tool.as_bytes(), None, args),
        };
        let confinement = self.confinement.spell();
        let argc = args.len().to_string();
        let head = [MAGIC, confinement.as_bytes(), kind, program]
            .into_iter()
            .chain(file.into_iter().flatten())
            .chain([argc.as_bytes()]);
        let bytes = nul_terminated(head.chain(args.iter().map(|a| a.as_bytes())))?;
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
        let file = (kind == b"file")
            .then(|| Ok::<_, String>((next("argv[0]")?, next("interpreter exclusion")?)))
            .transpose()?;
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
        let run = match (kind, file) {
            (b"file", Some((arg0, excluded))) => Run::File {
                real: OsStr::from_bytes(program).into(),
                arg0: OsStr::from_bytes(arg0).into(),
                excluded: Some(text(excluded)?)
                    .filter(|scope| !scope.is_empty())
                    .map(str::to_owned),
                args,
            },
            (b"tool", None) => Run::Tool(text(program)?.to_owned(), args),
            (other, _) => {
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

/// What a confined launch hands its child: copies lifted to free numbers
/// from [`Slot::FIRST`] up, each crossing at the number it holds here.
/// Nothing is `dup2`'d, so no descriptor the child still needs — `std`'s
/// exec-error pipe, a later hook's — can be displaced.
#[derive(Default)]
pub(super) struct Handoff {
    lent: Vec<OwnedFd>,
    kept: Vec<Slot>,
}

impl Handoff {
    /// Lend the child a copy of `fd`, owned here until the spawn.
    ///
    /// # Errors
    /// A lift the descriptor limit refuses.
    pub(super) fn lend(&mut self, fd: BorrowedFd<'_>) -> Result<Slot, String> {
        let (copy, at) = lift(fd)?;
        self.lent.push(copy);
        Ok(at)
    }

    /// Hand the child a copy of `fd` that the caller owns, to close when its
    /// closing is a signal; it must stay open past the spawn, whose hook
    /// names its number.
    ///
    /// # Errors
    /// As [`Self::lend`].
    #[cfg(target_os = "linux")]
    pub(super) fn keep(&mut self, fd: BorrowedFd<'_>) -> Result<(OwnedFd, Slot), String> {
        let (copy, at) = lift(fd)?;
        self.kept.push(at);
        Ok((copy, at))
    }

    /// Hand `cmd`'s child every slot, through one `pre_exec`.
    pub(super) fn install(self, cmd: &mut Command) {
        let Self { lent, kept } = self;
        // SAFETY: post-fork, `fcntl` alone, which is async-signal-safe, on
        // numbers open here at the fork: the lent copies, which the hook
        // owns, and the kept ones, which their owners hold past the spawn.
        unsafe {
            cmd.pre_exec(move || {
                let lent = lent.iter().map(AsRawFd::as_raw_fd);
                for at in lent.chain(kept.iter().map(|at| at.0)) {
                    if libc::fcntl(at, libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
}

/// A close-on-exec copy of `fd` at the lowest free number from
/// [`Slot::FIRST`] up, and that number.
fn lift(fd: BorrowedFd<'_>) -> Result<(OwnedFd, Slot), String> {
    let copy = rustix::io::fcntl_dupfd_cloexec(fd, Slot::FIRST).map_err(|e| {
        format!(
            "sandbox: cannot copy a descriptor to {} or above for the confined child: \
             {e}; is the open-file limit (`ulimit -n`) below that?",
            Slot::FIRST
        )
    })?;
    let at = Slot(copy.as_raw_fd());
    Ok((copy, at))
}

/// Close every descriptor from [`Slot::FIRST`] up, so the program inherits
/// none of the handoff.
///
/// # Errors
/// A kernel that cannot sweep: the program would hold the payload's copy
/// of the `--info-fd` writer, and with it the parent's spawn, for its whole
/// life.
#[cfg(target_os = "linux")]
pub(super) fn close_handoff() -> Result<(), String> {
    // SAFETY: nothing in this process owns a descriptor from the first slot up.
    let swept = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            Slot::FIRST.unsigned_abs(),
            libc::c_uint::MAX,
            0 as libc::c_uint,
        )
    };
    if swept == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    Err(if e.raw_os_error() == Some(libc::ENOSYS) {
        "this kernel has no close_range(2) (Linux 5.9); the handoff cannot be swept, so the \
         program would inherit it; refusing to run"
            .to_string()
    } else {
        format!("cannot sweep the handoff: {e}; refusing to run")
    })
}

/// macOS's one slot, the warrant's, is consumed by [`Warrant::receive`].
#[cfg(target_os = "macos")]
#[allow(
    clippy::unnecessary_wraps,
    reason = "one signature with Linux's sweep, which can fail"
)]
pub(super) fn close_handoff() -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    const CONFINEMENT: &str = "(version 1)\n(deny default)";
    #[cfg(target_os = "linux")]
    const CONFINEMENT: &str = "landlock@101";

    #[cfg(target_os = "macos")]
    const NO_CONFINEMENT: &str = "";
    #[cfg(target_os = "linux")]
    const NO_CONFINEMENT: &str = "unconfined";

    fn confinement() -> Native {
        Native::parse(CONFINEMENT).expect("a valid confinement")
    }

    fn file(real: &[u8], arg0: &[u8], args: &[&[u8]]) -> Warrant {
        excluding(real, arg0, None, args)
    }

    fn excluding(real: &[u8], arg0: &[u8], excluded: Option<&str>, args: &[&[u8]]) -> Warrant {
        let args = args.iter().map(|a| OsStr::from_bytes(a).into()).collect();
        Warrant {
            confinement: confinement(),
            run: Run::File {
                real: OsStr::from_bytes(real).into(),
                arg0: OsStr::from_bytes(arg0).into(),
                excluded: excluded.map(str::to_owned),
                args,
            },
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
                b"tools/\xfeb",
                &[b"-c", b"echo a  b\nc", b"", b"\xfe\xff"],
            ),
            excluding(b"/bin/sh", b"sh", Some("`absolute /bin/dash"), &[b"x"]),
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
        let control = framed(&[MAGIC, conf, b"file", b"/bin/sh", b"sh", b"", b"1", b"x"]);
        assert!(Warrant::decode(&control).is_ok(), "the control must decode");
        for (case, bytes) in [
            ("empty", Vec::new()),
            (
                "another version",
                framed(&[b"ral-warrant/2", conf, b"file", b"/bin/sh", b"sh", b"0"]),
            ),
            ("cut short", control[..control.len() - 1].to_vec()),
            (
                "an unknown kind",
                framed(&[MAGIC, conf, b"exec", b"/bin/sh", b"1", b"x"]),
            ),
            (
                "an argument too many",
                framed(&[
                    MAGIC, conf, b"file", b"/bin/sh", b"sh", b"", b"1", b"x", b"y",
                ]),
            ),
            (
                "an argument too few",
                framed(&[MAGIC, conf, b"file", b"/bin/sh", b"sh", b"", b"2", b"x"]),
            ),
            (
                "a signed count",
                framed(&[MAGIC, conf, b"file", b"/bin/sh", b"sh", b"", b"+1", b"x"]),
            ),
            (
                "a padded count",
                framed(&[MAGIC, conf, b"file", b"/bin/sh", b"sh", b"", b"01", b"x"]),
            ),
            (
                "over the cap",
                framed(&[MAGIC, conf, b"file", b"/bin/sh", b"sh", b"", b"1", &big]),
            ),
            (
                "a file without its argv[0]",
                framed(&[MAGIC, conf, b"file", b"/bin/sh", b"1", b"x"]),
            ),
            (
                "a file without its exclusion field",
                framed(&[MAGIC, conf, b"file", b"/bin/sh", b"sh", b"1", b"x"]),
            ),
            (
                "a tool with an argv[0]",
                framed(&[MAGIC, conf, b"tool", b"ls", b"ls", b"0"]),
            ),
        ] {
            assert!(Warrant::decode(&bytes).is_err(), "{case} must be refused");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_linux_confinement_has_three_spellings() {
        for (field, ok) in [
            ("unconfined", true),
            ("landlock@101", true),
            ("landlock+refer@101", true),
            ("landlock", false),
            ("landlock@", false),
            ("landlock@97", false),
            ("landlock@+101", false),
            ("landlock@0101", false),
            ("landlock@101@102", false),
            ("unconfined@101", false),
            ("landlock+refer+x@101", false),
            ("Landlock@101", false),
            ("101", false),
            ("", false),
        ] {
            let bytes = framed(&[MAGIC, field.as_bytes(), b"tool", b"ls", b"0"]);
            assert_eq!(Warrant::decode(&bytes).is_ok(), ok, "{field:?}");
        }
    }

    #[test]
    fn an_interior_nul_is_refused_never_truncated() {
        assert!(file(b"/bin/sh", b"sh", &[b"a\0b"]).encode().is_err());
        assert!(file(b"/bin/\0sh", b"sh", &[]).encode().is_err());
        assert!(file(b"/bin/sh", b"s\0h", &[]).encode().is_err());
    }

    #[test]
    fn a_parcel_reads_back_whole_through_the_childs_check() {
        let warrant = file(b"/bin/sh", b"sh", &[b"-c", b"true"]);
        let fd = warrant.parcel().expect("parcels");
        assert_eq!(Warrant::unparcel(fd).expect("reads back"), warrant);
    }

    /// Every free number below the first slot, held as a host with that
    /// many descriptors open holds them: what opens next lands among the
    /// slots.
    fn below_first_held() -> Vec<std::fs::File> {
        std::iter::repeat_with(|| {
            std::fs::File::open("/dev/null").expect("open /dev/null (is `ulimit -n` below 98?)")
        })
        .take_while(|null| null.as_raw_fd() < Slot::FIRST)
        .collect()
    }

    /// A descriptor opened after the handoff and written by a later hook,
    /// as `detach`'s handshake is, crosses whole beside the lent one, with
    /// every number below the slots busy.  A concurrent test freeing a low
    /// number weakens this witness, never its verdict.
    #[test]
    fn a_later_hooks_descriptor_crosses_the_handoff_whole() {
        use std::os::fd::AsFd;
        let held = below_first_held();
        let (lent, mut filler) = crate::process::cloexec_pipe().expect("a pipe");
        filler.write_all(b"lent").expect("fill the pipe");
        drop(filler);
        let mut handoff = Handoff::default();
        let at = handoff.lend(lent.as_fd()).expect("lends");
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", &format!("cat /dev/fd/{at}")]);
        handoff.install(&mut cmd);
        let (mut receipt, handshake) = crate::process::cloexec_pipe().expect("a pipe");
        let fd = handshake.as_raw_fd();
        // SAFETY: post-fork, `write` alone, as `detach`'s hook does.
        unsafe {
            cmd.pre_exec(move || {
                let _ = libc::write(fd, b"!".as_ptr().cast(), 1);
                Ok(())
            });
        }
        let out = crate::process::output(&mut cmd).expect("spawn sh");
        drop((handshake, held));
        let mut byte = [0; 1];
        receipt
            .read_exact(&mut byte)
            .expect("the later hook's write must arrive");
        assert_eq!(&byte, b"!");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "lent", "{out:?}");
    }

    /// `std`'s exec-error pipe opens inside the spawn, after every hook, so
    /// with every number below the slots busy it lands among them: the
    /// handoff leaves it whole, and a program that cannot run is the
    /// spawn's error, not a child that died of an abort.
    #[test]
    fn a_program_that_cannot_run_after_a_handoff_is_the_spawns_error() {
        use std::os::fd::AsFd;
        let held = below_first_held();
        let stderr = io::stderr();
        let mut handoff = Handoff::default();
        handoff.lend(stderr.as_fd()).expect("lends");
        let mut cmd = Command::new("/ral/no/such/program");
        handoff.install(&mut cmd);
        let spawned = crate::process::spawn(&mut cmd).map(|mut child| child.wait());
        drop(held);
        let err = spawned.expect_err("a program that cannot run must not spawn");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
