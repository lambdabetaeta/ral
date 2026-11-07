//! The trampoline, `ral --warrant <w> <r>`: the confined child's side of a
//! warrant.  It takes the report socket at `r` and the warrant at `w`, enters
//! the confinement, runs the image, and says on the report what became of
//! it.  Nothing runs unconfined, and nothing is printed: the parent words
//! what it is told.  Only a call naming no report socket, made by hand, is
//! refused on stderr.
//!
//! macOS becomes the image.  Linux runs it as this process's child inside the
//! bwrap envelope and supervises it, reporting the exact end bwrap would
//! fold into an exit status.

use super::super::launch::Image;
use super::{Confined, Slots, Warrant};
use crate::path::RealPath;
use crate::process::report::Witness;
use crate::site::{Silent, site};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
#[cfg(target_os = "linux")]
use {libc::c_int, rustix::process::Pid};

/// This process's exit when its program never ran.  The parent reads the
/// report instead.
const UNSTARTED: u8 = 126;

/// Serve `ral --warrant <w> <r>`: `args` must be those two slots.
pub(crate) fn serve_warrant(args: &[OsString]) -> u8 {
    match received(args) {
        Ok((slots, witness)) => match Warrant::confine(slots) {
            Ok(confined) => confined.run(witness),
            Err(why) => {
                witness.refused(&why);
                UNSTARTED
            }
        },
        Err(e) => {
            crate::io::terminal::cmd_error("ral", &e);
            UNSTARTED
        }
    }
}

/// The slots `args` name, and the report socket at the second.
fn received(args: &[OsString]) -> Result<(Slots, Witness), String> {
    let slots = Slots::parse(args)?;
    let at = slots.report;
    let fd = at
        .take()
        .ok_or_else(|| format!("no report socket at fd {at}"))?;
    let witness = Witness::new(fd).ok_or_else(|| format!("fd {at} is not ral's report socket"))?;
    Ok((slots, witness))
}

/// The host file `real`, run under the spelling `arg0` it was named by.
fn program(real: &RealPath, arg0: &Path, args: Vec<OsString>) -> Command {
    let mut program = site!(Silent::RespawnSpawn, Command::new(real.as_path()));
    program.arg0(arg0).args(args);
    program
}

#[cfg(target_os = "macos")]
impl Confined {
    /// Become the image.  A host file is `execve`d, closing the
    /// close-on-exec report, which the parent reads as its start.  A bundled
    /// tool runs here once the report is closed; an `execve` of its own keeps
    /// this pid, which the parent waits on.
    fn run(self, witness: Witness) -> u8 {
        match self.image {
            Image::File { real, arg0 } => {
                let mut program = program(&real, &arg0, self.args);
                let e = site!(Silent::RespawnExec, &mut program => exec());
                witness.unstarted(&e);
                UNSTARTED
            }
            Image::Tool(tool) => {
                drop(witness);
                crate::uutils::serve(tool.as_ref(), self.args)
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Confined {
    /// Run the image as this process's child and [`supervise`] it: a file is
    /// spawned, and a bundled tool forked, since a tool may itself `execve`
    /// (`env`) where the supervisor never does.
    fn run(self, witness: Witness) -> u8 {
        let caught = outlive_group_signals();
        let supervisor = rustix::process::getpid();
        let started = match self.image {
            Image::File { real, arg0 } => {
                let mut program = program(&real, &arg0, self.args);
                // SAFETY: `die_with` makes only async-signal-safe syscalls.
                unsafe {
                    program.pre_exec(move || die_with(supervisor));
                }
                crate::process::spawn(&mut program).map(|child| Pid::from_child(&child))
            }
            Image::Tool(tool) => {
                // SAFETY: the warrant role starts no thread, so the child may
                // run anything.
                match unsafe { crate::process::fork() } {
                    Ok(crate::process::Forked::Child) => {
                        drop(witness);
                        tool_child(tool, self.args, &caught, supervisor)
                    }
                    Ok(crate::process::Forked::Parent(pid)) => Ok(pid),
                    Err(e) => Err(e),
                }
            }
        };
        match started {
            Ok(pid) => supervise(witness, pid),
            Err(e) => {
                witness.unstarted(&e);
                UNSTARTED
            }
        }
    }
}

/// Tell the start of `pid`, the program, and, waited for, its exact end;
/// exit as it did.  What the payload's group is sent this process outlives,
/// but `SIGKILL`, whose silence the parent reads as such.
#[cfg(target_os = "linux")]
fn supervise(witness: Witness, pid: Pid) -> u8 {
    use std::os::unix::process::ExitStatusExt;
    let _ = crate::process::install(libc::SIGPIPE, crate::process::Disposition::Ignore);
    let running = witness.started();
    let Ok(ended) = crate::process::reap(pid) else {
        // Unreachable: the program is this process's own child.
        return UNSTARTED;
    };
    running.ended(ended);
    ended
        .code()
        .or_else(|| ended.signal().map(|signal| 128 + signal))
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(u8::MAX)
}

/// The forked tool's whole life, begun as a spawned program begins: no
/// signal blocked, each one `caught` here at its default, and death with its
/// supervisor.  It never returns to the trampoline's frames, by a panic
/// either.
#[cfg(target_os = "linux")]
fn tool_child(tool: &'static str, args: Vec<OsString>, caught: &[c_int], supervisor: Pid) -> ! {
    use std::io::Write;
    let defaulted = caught.iter().all(|&signal| {
        crate::process::install(signal, crate::process::Disposition::Default).is_ok()
    });
    let begun = defaulted
        && nix::sys::signal::SigSet::empty().thread_set_mask().is_ok()
        && die_with(supervisor).is_ok();
    let code = if begun {
        // 101: `std`'s exit for a panicking `main`.
        std::panic::catch_unwind(move || crate::uutils::serve(tool.as_ref(), args)).unwrap_or(101)
    } else {
        UNSTARTED
    };
    let _ = (std::io::stdout().flush(), std::io::stderr().flush());
    // SAFETY: `_exit` runs no atexit hook over the trampoline's state.
    unsafe { libc::_exit(i32::from(code)) }
}

/// Be killed when `supervisor`, this process's parent, dies.
#[cfg(target_os = "linux")]
fn die_with(supervisor: Pid) -> std::io::Result<()> {
    rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))?;
    // A supervisor gone before the prctl leaves no death to hear.
    if rustix::process::getppid() != Some(supervisor) {
        return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
    }
    Ok(())
}

/// A no-op handler for each group signal not ignored, which resets to its
/// default at the program's `execve`; a signal ignored stays so, for the
/// program to inherit.  The signals given the handler.
#[cfg(target_os = "linux")]
fn outlive_group_signals() -> Vec<c_int> {
    extern "C" fn heard(_: c_int) {}
    [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT]
        .into_iter()
        .filter(|&signal| {
            !crate::process::signal::ignored(signal)
                && crate::process::signal::catch(signal, heard).is_ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Refused before any descriptor is touched, so it is safe in-process:
    /// the trampoline takes two arguments, both slots.
    #[test]
    fn a_warrant_flag_naming_no_slot_runs_nothing() {
        for args in [
            &[][..],
            &["sh"],
            &["101"],
            &["97", "101"],
            &["+101", "102"],
            &["0101", "102"],
            &["101", "sh"],
            &["101", "102", "sh"],
        ] {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            assert_eq!(serve_warrant(&args), UNSTARTED, "{args:?}");
        }
    }
}
