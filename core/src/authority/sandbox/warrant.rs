//! The warrant a launch hands to the confined re-exec of ral that runs its
//! program, and its only owner: the confinement to enter and the image to
//! run, on a descriptor no other process can write.  The descriptors every
//! confined launch hands down are [`handoff`]'s; the re-exec's side is
//! [`trampoline`].
//!
//! The re-exec's argv starts with `ral --warrant <w> <r>`: the warrant's
//! slot, then the report socket's ([`crate::process::report`]). A Linux
//! envelope appends the three payload stream slots, installed only in the
//! trampoline before its inherited-fd sweep. The warrant is
//! NUL-terminated fields, with no length anywhere:
//!
//! ```text
//! ral-warrant/5  <confinement>  file  <real>  <argv0>  <argc>  <arg>…
//! ral-warrant/5  <confinement>  tool  <name>           <argc>  <arg>…
//! ```
//!
//! The decoder accepts only what the encoder emits, byte for byte, so the
//! encoding is canonical and injective.

mod handoff;
mod trampoline;

pub(super) use handoff::{Handoff, Slot, Slots, parcel};
pub(crate) use trampoline::serve_warrant;

use super::launch::Image;
#[cfg(target_os = "linux")]
use super::linux::landlock::Landlocked;
use crate::Role;
use crate::path::{RealPath, shown};
use handoff::{close_inherited, unwritable};
use std::borrow::{Borrow, Cow};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;

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

/// The confinement and command encoded for the child. Construction borrows
/// the image and argument bytes; decoding produces the owned default types.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Warrant<I = Image, A = OsString> {
    confinement: Confinement,
    image: I,
    args: Vec<A>,
}

/// The program, once its confinement is entered: the only thing a received
/// warrant yields, so nothing else can run it.
pub(super) struct Confined {
    image: Image,
    args: Vec<OsString>,
}

impl<'a> Warrant<&'a Image, &'a OsStr> {
    pub(super) fn new(
        confinement: Confinement,
        image: &'a Image,
        args: &'a [Cow<'_, str>],
    ) -> Self {
        Self {
            confinement,
            image,
            args: args.iter().map(|arg| OsStr::new(arg.as_ref())).collect(),
        }
    }
}

impl<I: Borrow<Image>, A: AsRef<OsStr>> Warrant<I, A> {
    /// Encode directly from the borrowed or owned fields into a sealed parcel.
    pub(super) fn parcel(&self) -> Result<OwnedFd, String> {
        parcel("ral-warrant", &self.encode()?)
            .map_err(|e| format!("sandbox: cannot parcel the warrant: {e}"))
    }

    fn encode(&self) -> Result<Vec<u8>, String> {
        let (kind, program, arg0): (&[u8], &[u8], Option<&[u8]>) = match self.image.borrow() {
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
        let bytes = nul_terminated(head.chain(self.args.iter().map(|a| a.as_ref().as_bytes())))?;
        if bytes.len() > CAP {
            return Err(format!(
                "the warrant is {} bytes, over its {CAP}-byte cap: is the command line that long?",
                bytes.len()
            ));
        }
        Ok(bytes)
    }
}

impl Warrant {
    /// The warrant the launch left at `slots.warrant`, its confinement
    /// entered and every inherited descriptor but the report swept: the
    /// program, and nothing before it.
    pub(super) fn confine(slots: Slots) -> Result<Confined, String> {
        let Self {
            confinement,
            image,
            args,
        } = Self::receive(slots.warrant)?;
        #[cfg(target_os = "linux")]
        slots.install_streams()?;
        confinement.enter(slots.report)?;
        Ok(Confined { image, args })
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
        use std::io;

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
            let mut launch = super::super::launch::trampoline(own, handoff, slots, report);
            launch.stdin(crate::process::StdioSpec::Null);
            Ok::<_, crate::error::Error>(launch)
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
}
