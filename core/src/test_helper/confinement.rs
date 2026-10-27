//! Live enforcement requires a successful confined launch and a control.

use crate::authority::policy::{Capabilities, FsPolicy, GrantStack};
use crate::path::FrozenPath;
use crate::protocol::Run;
use crate::shell::run::{Ending, RunReport};
use crate::source::SourceOrigin;
use std::sync::OnceLock;

/// Evidence that this process can launch a confined program.
#[derive(Clone, Debug)]
pub struct Confining(());

/// Why a confined launch could not be established.
#[derive(Clone, Debug)]
pub struct Unconfined {
    pub status: i32,
    pub stderr: String,
    pub reason: String,
}

impl std::fmt::Display for Unconfined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (status {}): {}",
            self.reason, self.status, self.stderr
        )
    }
}

/// Captured process or shell output.
#[derive(Debug)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub status: i32,
}

impl From<std::process::Output> for Output {
    fn from(out: std::process::Output) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            status: out.status.code().unwrap_or(1),
        }
    }
}

impl Output {
    /// Capture a completed evaluation.
    ///
    /// # Panics
    /// If evaluation did not run or its streams were not captured.
    pub fn from_report(report: RunReport) -> Self {
        let (ending, captured) = match report {
            RunReport::Ran {
                ending, captured, ..
            } => (ending, captured),
            RunReport::Static { diagnostics } => {
                panic!("enforcement probe did not run: {}", diagnostics.render().0)
            }
            RunReport::Faulted { error } => panic!("enforcement probe faulted: {}", error.message),
        };
        let status = ending.status();
        let captured = captured.expect("an enforcement probe captures its streams");
        let mut stderr = String::from_utf8_lossy(&captured.stderr).into_owned();
        if let Ending::Raised { error, .. } = ending {
            stderr.push_str(&error.message);
        }
        Self {
            stdout: String::from_utf8_lossy(&captured.stdout).into_owned(),
            stderr,
            status,
        }
    }
}

/// Dispatch under the given stack, capturing both streams.
///
/// # Panics
/// If the host cannot boot or the probe cannot reach evaluation.
pub fn run(caps: GrantStack, source: &str) -> Output {
    let mut shell = crate::shell::boot::boot_shell(
        crate::io::terminal::TerminalState::default(),
        crate::shell::boot::BakedPrelude::runtime(),
        &crate::HostSurface::default(),
        super::process_cwd(),
    )
    .expect("a test host resolves its baseline");
    Output::from_report(shell.run(Run {
        caps,
        ..Run::captured(source, SourceOrigin::named("<enforcement probe>"))
    }))
}

/// Called only by `require_confinement!`.
///
/// # Errors
/// The confined echo did not return its marker.
///
/// # Panics
/// If the test host or its temporary directory cannot resolve.
pub fn confinement() -> Result<Confining, Unconfined> {
    static PROOF: OnceLock<Result<Confining, Unconfined>> = OnceLock::new();
    PROOF
        .get_or_init(|| {
            const MARKER: &str = "ral-confinement-probe";
            let dir = tempfile::tempdir().expect("create a confinement probe directory");
            let tmp = FrozenPath::freeze(dir.path()).expect("an absolute temporary directory");
            let caps = Capabilities {
                fs: Some(FsPolicy {
                    read_prefixes: vec![tmp],
                    write_prefixes: Vec::new(),
                    deny_paths: Vec::new(),
                }),
                ..Capabilities::default()
            };
            #[cfg(not(windows))]
            let command = format!("/bin/echo {MARKER}");
            #[cfg(windows)]
            let command = format!("cmd /d /c 'echo {MARKER}'");
            let source = format!(
                "within [dir: '{}'] {{ {command} }}",
                crate::path::shown(dir.path())
            );
            let out = run(super::stack([super::layer(caps)]), &source);
            let newline = if cfg!(windows) { "\r\n" } else { "\n" };
            if out.status == 0 && out.stdout == format!("{MARKER}{newline}") {
                Ok(Confining(()))
            } else {
                #[cfg(target_os = "macos")]
                let nested = out.stderr.lines().any(|line| {
                    line.strip_prefix("ral: cannot enter the Seatbelt sandbox: ")
                        .is_some_and(|reason| {
                            reason.starts_with(crate::authority::sandbox::ALREADY_PROFILED)
                        })
                });
                #[cfg(not(target_os = "macos"))]
                let nested = false;
                Err(Unconfined {
                    status: out.status,
                    stderr: out.stderr,
                    reason: if nested {
                        "the runner is already inside a Seatbelt profile".into()
                    } else {
                        format!(
                            "fs-confined {command} did not return exactly {MARKER:?}; stdout {:?}",
                            out.stdout
                        )
                    },
                })
            }
        })
        .clone()
}

/// The command ran and produced its marker.
///
/// # Panics
/// The command failed, or its nonempty marker is missing.
#[track_caller]
pub fn admitted(_: &Confining, out: &Output, marker: &str) {
    assert!(
        !marker.is_empty(),
        "an enforcement control needs a nonempty marker"
    );
    assert_eq!(out.status, 0, "control failed: {out:?}");
    assert!(
        out.stdout.contains(marker),
        "control marker {marker:?} missing: {out:?}"
    );
}

/// The control runs; the denied operation cannot produce its marker.
///
/// # Panics
/// The control failed, or the denied output contains its marker.
#[track_caller]
pub fn denied(proof: &Confining, control: &Output, out: &Output, marker: &str) {
    admitted(proof, control, marker);
    assert!(
        !out.stdout.contains(marker),
        "denied operation produced {marker:?}: {out:?}"
    );
}

/// Skip locally, or fail when CI requires confinement.
#[macro_export]
macro_rules! require_confinement {
    () => {{
        match $crate::test_helper::confinement::confinement() {
            Ok(proof) => proof,
            Err(why) => {
                assert!(
                    ::std::env::var_os("RAL_REQUIRE_SANDBOX").is_none(),
                    "confinement required: {why}"
                );
                let _ = ::std::io::Write::write_fmt(
                    &mut ::std::io::stderr(),
                    format_args!(
                        "skip {}: {why}\n",
                        ::std::thread::current().name().unwrap_or(module_path!())
                    ),
                );
                return;
            }
        }
    }};
}

pub mod probe;

/// The kernel's Landlock ABI, or zero when Landlock is absent.
///
/// # Panics
/// The version query fails for a reason other than absent Landlock.
#[cfg(target_os = "linux")]
pub fn landlock_abi() -> u32 {
    crate::authority::sandbox::probe_abi().unwrap_or_else(|errno| {
        panic!(
            "cannot query the Landlock ABI: {}",
            std::io::Error::from_raw_os_error(errno)
        )
    })
}

/// An older kernel skips only the assertion needing its newer ABI.
#[macro_export]
macro_rules! require_landlock {
    ($needed:expr) => {
        #[cfg(target_os = "linux")]
        {
            let needed = $needed;
            let found = $crate::test_helper::confinement::landlock_abi();
            if found < needed {
                let _ = ::std::io::Write::write_fmt(
                    &mut ::std::io::stderr(),
                    format_args!(
                        "skip {}: needs Landlock ABI {needed}; found ABI {found}\n",
                        ::std::thread::current().name().unwrap_or(module_path!())
                    ),
                );
                return;
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(status: i32, stdout: &str) -> Output {
        Output {
            status,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    #[test]
    #[should_panic(expected = "control marker")]
    fn an_exit_code_cannot_prove_admission() {
        admitted(&Confining(()), &output(0, ""), "ok");
    }

    #[test]
    #[should_panic(expected = "control failed")]
    fn a_broken_control_cannot_prove_denial() {
        denied(&Confining(()), &output(126, "ok"), &output(126, ""), "ok");
    }

    #[test]
    #[should_panic(expected = "denied operation produced")]
    fn a_failed_status_cannot_hide_a_leak() {
        denied(&Confining(()), &output(0, "ok"), &output(1, "ok"), "ok");
    }

    #[test]
    fn an_empty_mask_denies_bytes_even_when_cat_succeeds() {
        denied(
            &Confining(()),
            &output(0, "secret"),
            &output(0, ""),
            "secret",
        );
    }
}
