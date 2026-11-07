//! The handoff from a launch to the confined re-exec of ral that runs its
//! program, and its only owner: the [`Warrant`] — the confinement to enter,
//! the image to run — on a descriptor no other process can write, and the
//! descriptors every confined launch hands down, each at the number it
//! already holds ([`Handoff`]).  The re-exec's side is [`trampoline`].
//!
//! The re-exec's whole argv is `ral --warrant <w> <r>`: the warrant's slot,
//! then the report socket's ([`crate::process::report`]).  The warrant is
//! NUL-terminated fields, with no length anywhere:
//!
//! ```text
//! ral-warrant/5  <confinement>  file  <real>  <argv0>  <argc>  <arg>…
//! ral-warrant/5  <confinement>  tool  <name>           <argc>  <arg>…
//! ```
//!
//! The decoder accepts only what the encoder emits, byte for byte, so the
//! encoding is canonical and injective.

mod trampoline;

pub(crate) use trampoline::serve_warrant;

use super::launch::Image;
#[cfg(target_os = "linux")]
use super::linux::landlock::Landlocked;
use crate::Role;
use crate::path::{RealPath, shown};
#[cfg(target_os = "linux")]
use crate::site::{Silent, site};
use libc::c_int;
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// The number a handed-down descriptor holds, the same in parent and child:
/// from [`Self::FIRST`] up, away from stdio. Only a
/// [`Handoff`] makes one, or a child parsing what its parent wrote.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Slot(c_int);

impl Slot {
    /// The floor for copies made by `lift`, not for the child's sweep.
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

/// The trampoline's two slots: its warrant's, and its report's.
#[derive(Clone, Copy, Debug)]
pub(super) struct Slots {
    pub(super) warrant: Slot,
    pub(super) report: Slot,
}

impl Slots {
    /// `--warrant <warrant> <report>`.
    pub(super) fn argv(self) -> [String; 3] {
        [
            Role::Warrant.flag().to_owned(),
            self.warrant.to_string(),
            self.report.to_string(),
        ]
    }

    /// The slots `args`, what follows [`Self::argv`]'s flag, name.
    fn parse(args: &[OsString]) -> Result<Self, String> {
        let [warrant, report] = args else {
            return Err(format!(
                "{} takes two arguments, the descriptors its warrant and its report arrive on: \
                 it is how ral starts its own confined children, never a command to run by hand",
                Role::Warrant.flag()
            ));
        };
        let slot = |arg: &OsString| Slot::parse(&shown(arg));
        Ok(Self {
            warrant: slot(warrant)?,
            report: slot(report)?,
        })
    }
}

const MAGIC: &[u8] = b"ral-warrant/5";
/// Under macOS's default socket-buffer ceiling, far above either OS's `ARG_MAX`.
const CAP: usize = 8 << 20;

// `Confinement`, one type per platform: what the confined re-exec enters
// before it runs anything, spelled as one field of the warrant.  Entering
// sweeps every descriptor above stderr but the report's at `keep`.

/// macOS: the Seatbelt profile the launch compiled.
#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Confinement(pub(super) String);

/// Linux: the Landlock ruleset the launch promised, at the slot it names,
/// or none.
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Confinement(pub(super) Option<Landlocked>);

#[cfg(target_os = "macos")]
impl Confinement {
    fn spell(&self) -> Cow<'_, str> {
        (&self.0).into()
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "Linux's spelling can be malformed, and the decoder reads both alike"
    )]
    fn parse(field: &str) -> Result<Self, String> {
        Ok(Self(field.to_owned()))
    }

    fn enter(&self, keep: Slot) -> Result<(), String> {
        close_inherited(keep)?;
        super::macos::apply_profile(&self.0)
            .map_err(|e| format!("cannot enter the Seatbelt sandbox: {e}"))
    }
}

#[cfg(target_os = "linux")]
impl Confinement {
    fn spell(&self) -> Cow<'_, str> {
        match &self.0 {
            None => "unconfined".into(),
            Some(Landlocked { refer_root, at }) => {
                let refer = if *refer_root { "+refer" } else { "" };
                format!("landlock{refer}@{at}").into()
            }
        }
    }

    fn parse(field: &str) -> Result<Self, String> {
        if field == "unconfined" {
            return Ok(Self(None));
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
        Ok(Self(Some(Landlocked {
            refer_root,
            at: Slot::parse(at)?,
        })))
    }

    fn enter(&self, keep: Slot) -> Result<(), String> {
        self.0.as_ref().map_or(Ok(()), Landlocked::enter)?;
        close_inherited(keep)
    }
}

/// The confinement, and the image to run inside it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Warrant {
    confinement: Confinement,
    image: Image,
    args: Vec<OsString>,
}

/// The program, once its confinement is entered: the only thing a received
/// warrant yields, so nothing else can run it.
pub(super) struct Confined {
    image: Image,
    args: Vec<OsString>,
}

impl Warrant {
    pub(super) fn new(confinement: Confinement, image: &Image, args: &[String]) -> Self {
        Self {
            confinement,
            image: image.clone(),
            args: args.iter().map(OsString::from).collect(),
        }
    }

    /// The warrant the launch left at `slots.warrant`, its confinement
    /// entered and every inherited descriptor but the report swept: the
    /// program, and nothing before it.
    pub(super) fn confine(slots: Slots) -> Result<Confined, String> {
        let Self {
            confinement,
            image,
            args,
        } = Self::receive(slots.warrant)?;
        confinement.enter(slots.report)?;
        Ok(Confined { image, args })
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
        let (kind, program, arg0): (&[u8], &[u8], Option<&[u8]>) = match &self.image {
            Image::File { real, arg0 } => (
                b"file",
                real.as_path().as_os_str().as_bytes(),
                Some(arg0.as_os_str().as_bytes()),
            ),
            Image::Tool(tool) => (b"tool", tool.as_bytes(), None),
        };
        let confinement = self.confinement.spell();
        let argc = self.args.len().to_string();
        let head = [MAGIC, confinement.as_bytes(), kind, program]
            .into_iter()
            .chain(arg0)
            .chain([argc.as_bytes()]);
        let bytes = nul_terminated(head.chain(self.args.iter().map(|a| a.as_bytes())))?;
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
        let confinement = Confinement::parse(text(next("confinement")?)?)?;
        let kind = next("kind")?;
        let program = OsStr::from_bytes(next("program")?);
        let image = match kind {
            b"file" => Image::File {
                real: RealPath::off_the_wire(program).map_err(|e| {
                    format!(
                        "the warrant's file program {} is not a real path, which is absolute \
                         and names no link, `.` or `..`: {e}; refusing PATH lookup",
                        shown(program)
                    )
                })?,
                arg0: OsStr::from_bytes(next("argv[0]")?).into(),
            },
            b"tool" => Image::Tool(crate::uutils::bundled_name(program).ok_or_else(|| {
                format!(
                    "the warrant names an unknown bundled tool, {:?}",
                    shown(program)
                )
            })?),
            other => {
                return Err(format!(
                    "the warrant names an unknown kind of program, {:?}",
                    String::from_utf8_lossy(other)
                ));
            }
        };
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
        let warrant = Self {
            confinement,
            image,
            args,
        };
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
    use std::os::fd::AsFd;
    match crate::process::report::is_socket(fd.as_fd()) {
        Ok(true) => Ok(()),
        Ok(false) => Err("the warrant is not a socket; refusing it".to_string()),
        Err(e) => Err(format!("cannot stat the warrant: {e}")),
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
             {e}; is the open-file limit below that? Check it with `sh -c 'ulimit -n'`, and raise it for the session with the rc's `limits: [open-files: N]`",
            Slot::FIRST
        )
    })?;
    let at = Slot(copy.as_raw_fd());
    Ok((copy, at))
}

/// Close every descriptor above stderr but `keep`, which stays close-on-exec;
/// the warrant role must open nothing before this.
///
/// # Errors
/// A kernel that cannot sweep: the program would hold the payload's copy
/// of the `--info-fd` writer, and with it the parent's spawn, for its whole
/// life.
#[cfg(target_os = "linux")]
pub(super) fn close_inherited(keep: Slot) -> Result<(), String> {
    let at = libc::c_uint::try_from(keep.fd()).expect("a slot is positive");
    close_range(3, at - 1)?;
    close_range(at + 1, libc::c_uint::MAX)?;
    cloexec(keep)
}

#[cfg(target_os = "linux")]
fn close_range(first: libc::c_uint, last: libc::c_uint) -> Result<(), String> {
    // SAFETY: the warrant role owns no descriptors in the range here.
    let swept = unsafe {
        site!(
            Silent::InheritedSweep,
            libc::syscall(libc::SYS_close_range, first, last, 0 as libc::c_uint)
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
        format!(
            "cannot close inherited descriptors above stderr with close_range(2): {e}; refusing to run"
        )
    })
}

/// The report's slot, closed by the program's `execve`.
fn cloexec(keep: Slot) -> Result<(), String> {
    // SAFETY: `F_SETFD` only sets the slot's close-on-exec flag.
    if unsafe { libc::fcntl(keep.fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(format!(
            "cannot set the report socket at fd {keep} close-on-exec: {}; refusing to run",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Close every descriptor above stderr but `keep`, which stays close-on-exec;
/// the warrant role must open nothing before this.
#[cfg(target_os = "macos")]
pub(super) fn close_inherited(keep: Slot) -> Result<(), String> {
    let mut entries = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        64
    ];
    let count = loop {
        let size = c_int::try_from(std::mem::size_of_val(entries.as_slice()))
            .map_err(|e| format!("cannot size the proc_pidinfo(PROC_PIDLISTFDS) buffer: {e}"))?;
        // SAFETY: entries is writable for size bytes; this only lists our descriptors.
        let listed = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDLISTFDS,
                0,
                entries.as_mut_ptr().cast(),
                size,
            )
        };
        if listed <= 0 {
            return Err(format!(
                "proc_pidinfo(PROC_PIDLISTFDS): {}; refusing to run",
                io::Error::last_os_error()
            ));
        }
        let listed = usize::try_from(listed).expect("a positive descriptor-list size fits usize");
        let width = std::mem::size_of::<libc::proc_fdinfo>();
        if !listed.is_multiple_of(width) {
            return Err(format!(
                "proc_pidinfo(PROC_PIDLISTFDS) returned a partial descriptor record ({listed} bytes); refusing to run"
            ));
        }
        if listed < std::mem::size_of_val(entries.as_slice()) {
            break listed / width;
        }
        entries.resize(
            entries.len() * 2,
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0,
            },
        );
    };
    for entry in &entries[..count] {
        if entry.proc_fd <= 2 || entry.proc_fd == keep.fd() {
            continue;
        }
        // SAFETY: the warrant role owns no descriptors above stderr here.
        if unsafe { libc::close(entry.proc_fd) } < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EBADF) {
                return Err(format!(
                    "close({}): {error}; refusing to run",
                    entry.proc_fd
                ));
            }
        }
    }
    cloexec(keep)
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

    fn confinement() -> Confinement {
        Confinement::parse(CONFINEMENT).expect("a valid confinement")
    }

    fn file(real: &[u8], arg0: &[u8], args: &[&[u8]]) -> Warrant {
        Warrant {
            confinement: confinement(),
            image: Image::File {
                real: RealPath::off_the_wire(OsStr::from_bytes(real)).expect("a real path"),
                arg0: OsStr::from_bytes(arg0).into(),
            },
            args: args.iter().map(|a| OsStr::from_bytes(a).into()).collect(),
        }
    }

    fn framed(fields: &[&[u8]]) -> Vec<u8> {
        nul_terminated(fields.iter().copied()).expect("no field holds a NUL")
    }

    #[test]
    fn a_warrant_round_trips_bytes_that_are_not_utf8_and_arguments_of_any_shape() {
        let warrant = file(
            b"/opt/\xffbin/tool",
            b"tools/\xfeb",
            &[b"-c", b"echo a  b\nc", b"", b"\xfe\xff"],
        );
        let bytes = warrant.encode().expect("encodes");
        assert_eq!(Warrant::decode(&bytes).expect("decodes"), warrant);
    }

    #[test]
    fn every_bundled_name_round_trips_to_its_static_spelling() {
        for tool in crate::uutils::bundled_tools() {
            let warrant = Warrant {
                confinement: Confinement::parse(NO_CONFINEMENT).expect("a valid confinement"),
                image: Image::Tool(tool),
                args: Vec::new(),
            };
            assert_eq!(
                Warrant::decode(&warrant.encode().unwrap()).unwrap(),
                warrant
            );
        }
    }

    /// The controls decode, so each refusal is its own defect's.
    #[test]
    fn a_malformed_warrant_is_refused() {
        let conf = confinement().spell().into_owned();
        let conf = conf.as_bytes();
        let big = vec![b'a'; CAP];
        let control = framed(&[MAGIC, conf, b"file", b"/bin/sh", b"sh", b"1", b"x"]);
        let tool = framed(&[MAGIC, conf, b"tool", b"ls", b"0"]);
        for bytes in [&control, &tool] {
            assert!(Warrant::decode(bytes).is_ok(), "the control must decode");
        }
        let file = |program: &[u8], argc: &[u8], args: &[&[u8]]| {
            let head = [MAGIC, conf, b"file", program, b"sh", argc];
            framed(&[&head[..], args].concat())
        };
        for (case, bytes) in [
            ("empty", Vec::new()),
            (
                "another version",
                framed(&[
                    b"ral-warrant/4",
                    conf,
                    b"file",
                    b"/bin/sh",
                    b"sh",
                    b"1",
                    b"x",
                ]),
            ),
            ("cut short", control[..control.len() - 1].to_vec()),
            (
                "an unknown kind",
                framed(&[MAGIC, conf, b"exec", b"/bin/sh", b"sh", b"1", b"x"]),
            ),
            ("an empty file program", file(b"", b"1", &[b"x"])),
            ("a relative file program", file(b"./bin/sh", b"1", &[b"x"])),
            ("a slashless file program", file(b"sh", b"1", &[b"x"])),
            ("a doubled separator", file(b"/bin//sh", b"1", &[b"x"])),
            ("a parent step", file(b"/usr/../bin/sh", b"1", &[b"x"])),
            ("an empty tool", framed(&[MAGIC, conf, b"tool", b"", b"0"])),
            (
                "an unknown tool",
                framed(&[MAGIC, conf, b"tool", b"ral-no-such-tool", b"0"]),
            ),
            (
                "a tool path",
                framed(&[MAGIC, conf, b"tool", b"/bin/ls", b"0"]),
            ),
            (
                "an argument too many",
                file(b"/bin/sh", b"1", &[b"x", b"y"]),
            ),
            ("an argument too few", file(b"/bin/sh", b"2", &[b"x"])),
            ("a signed count", file(b"/bin/sh", b"+1", &[b"x"])),
            ("a padded count", file(b"/bin/sh", b"01", &[b"x"])),
            ("over the cap", file(b"/bin/sh", b"1", &[&big])),
            (
                "a file without its argv[0]",
                framed(&[MAGIC, conf, b"file", b"/bin/sh", b"1", b"x"]),
            ),
            (
                "a file with the interpreter fields of version 4",
                framed(&[
                    MAGIC, conf, b"file", b"/bin/sh", b"sh", b"", b"", b"1", b"x",
                ]),
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
            let bytes = framed(&[MAGIC, field.as_bytes(), b"file", b"/bin/sh", b"sh", b"0"]);
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

    #[cfg(target_os = "macos")]
    #[test]
    fn a_warrant_near_the_socket_ceiling_arrives_or_refuses_before_spawn() {
        use super::super::{ExecProjection, SandboxProjection};
        use crate::test_helper::confinement::{Output, admitted, denied};

        let proof = crate::require_confinement!();
        let mut bytes = [0_u8; 8];
        let mut size = bytes.len();
        // SAFETY: both outputs are writable; there is no new value.
        assert_eq!(
            unsafe {
                libc::sysctlbyname(
                    c"kern.ipc.maxsockbuf".as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    &raw mut size,
                    std::ptr::null_mut(),
                    0,
                )
            },
            0,
            "kern.ipc.maxsockbuf: {}",
            io::Error::last_os_error()
        );
        let ceiling = match size {
            4 => usize::try_from(u32::from_ne_bytes(bytes[..4].try_into().unwrap())).unwrap(),
            8 => usize::try_from(u64::from_ne_bytes(bytes)).unwrap(),
            _ => panic!("kern.ipc.maxsockbuf returned an unexpected {size}-byte integer"),
        };

        let projection = SandboxProjection {
            fs: super::super::FsProjection::Unrestricted,
            exec: ExecProjection::default(),
            net: Some(crate::authority::policy::Net::Offline),
            gpu: None,
        };
        let mut warrant = file(b"/bin/echo", b"echo", &[b"warrant-arrived"]);
        warrant.confinement = Confinement(super::super::macos::build_profile(&projection).unwrap());
        let prepare = |warrant: &Warrant| {
            let mut handoff = Handoff::default();
            let (slots, report) = super::super::launch::issue(warrant, &mut handoff)?;
            let own = super::super::reexec::own().unwrap();
            let mut launch = super::super::launch::trampoline(
                own,
                handoff,
                slots,
                report,
                &crate::process::DurableRoot::default().worker(),
            );
            launch.stdin(crate::process::StdioSpec::null());
            Ok::<_, super::super::Refusal>(launch)
        };
        let ran = |launch: crate::process::Launch| -> Output {
            launch.output().expect("the prepared warrant spawns").into()
        };
        let control = ran(prepare(&warrant).unwrap());
        admitted(&proof, &control, "warrant-arrived\n");
        let original = warrant.encode().unwrap().len();
        let target = ceiling.checked_sub(32 << 10).unwrap();
        assert!(
            target > original + 2,
            "the socket ceiling cannot hold the control warrant"
        );
        warrant.confinement.0.push_str("\n;");
        warrant
            .confinement
            .0
            .extend(std::iter::repeat_n('x', target - original - 2));
        match prepare(&warrant) {
            Ok(launch) => admitted(&proof, &ran(launch), "warrant-arrived\n"),
            Err(error) => {
                let error = error.to_string();
                assert!(
                    error.contains("parcel") || error.contains("byte cap"),
                    "{error}"
                );
                let refused = Output {
                    status: 126,
                    stdout: String::new(),
                    stderr: error,
                };
                denied(&proof, &control, &refused, "warrant-arrived\n");
            }
        }
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
