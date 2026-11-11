//! Live enforcement requires a successful confined launch and a control.

use crate::authority::policy::{Capabilities, FsPolicy, GrantStack};
use crate::fact::ExitCode as _;
use crate::path::FrozenPath;
use crate::protocol::Run;
use crate::shell::run::RunReport;
use crate::shell::runtime::End;
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
                panic!(
                    "enforcement probe did not run: {}",
                    diagnostics
                        .render(&crate::shell::runtime::ExitHints::default())
                        .0
                )
            }
        };
        let status = ending.code();
        let captured = captured.expect("an enforcement probe captures its streams");
        let mut stderr = String::from_utf8_lossy(&captured.stderr.bytes).into_owned();
        if let End::Err(error) = ending {
            stderr.push_str(&error.describe());
        }
        Self {
            stdout: String::from_utf8_lossy(&captured.stdout.bytes).into_owned(),
            stderr,
            status: i32::from(status),
        }
    }
}

/// Dispatch under the given stack, capturing both streams.
///
/// # Panics
/// If the host cannot boot or the probe cannot reach evaluation.
pub fn run(grants: GrantStack, source: &str) -> Output {
    Output::from_report(run_report(grants, source))
}

fn run_report(grants: GrantStack, source: &str) -> RunReport {
    let mut shell = crate::shell::boot::boot_shell(
        crate::io::terminal::TerminalState::default(),
        crate::shell::boot::BakedPrelude::runtime(),
        &crate::HostSurface::default(),
        super::process_cwd(),
    )
    .expect("a test host resolves its baseline");
    for layer in grants {
        shell.push_session_capabilities(layer);
    }
    shell.run(Run::captured(
        source,
        SourceOrigin::named("<enforcement probe>"),
    ))
}

fn already_confined(ending: &End) -> bool {
    use crate::fact::{LaunchFailure, Reason, SeatbeltFailure};
    matches!(ending, End::Err(error)
        if matches!(&error.reason, Reason::Launch(cause)
            if matches!(cause.as_ref(), LaunchFailure::Seatbelt(SeatbeltFailure::AlreadyConfined(_)))))
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
            let report = run_report(super::stack([super::layer(caps)]), &source);
            let nested =
                matches!(&report, RunReport::Ran { ending, .. } if already_confined(ending));
            let out = Output::from_report(report);
            let newline = if cfg!(windows) { "\r\n" } else { "\n" };
            if out.status == 0 && out.stdout == format!("{MARKER}{newline}") {
                Ok(Confining(()))
            } else {
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

/// Skip the calling test locally, or fail it when CI requires the sandbox the
/// product itself needs.
///
/// # Panics
/// `RAL_REQUIRE_SANDBOX` is set, so `why` is a failure.
#[track_caller]
pub fn skip(why: impl std::fmt::Display) {
    assert!(
        std::env::var_os("RAL_REQUIRE_SANDBOX").is_none(),
        "confinement required: {why}"
    );
    skip_beyond_baseline(why);
}

/// Skip the calling test for a prerequisite beyond what the product needs, such
/// as a newer kernel ABI than it requires: never a failure, since the product
/// works without it.
#[track_caller]
pub fn skip_beyond_baseline(why: impl std::fmt::Display) {
    let file = std::panic::Location::caller().file();
    let _ = std::io::Write::write_fmt(
        &mut std::io::stderr(),
        format_args!(
            "skip {}: {why}\n",
            std::thread::current().name().unwrap_or(file)
        ),
    );
}

/// [`skip`]s the calling test unless this process can confine.
#[macro_export]
macro_rules! require_confinement {
    () => {{
        match $crate::test_helper::confinement::confinement() {
            Ok(proof) => proof,
            Err(why) => {
                $crate::test_helper::confinement::skip(why);
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

/// Whether this kernel's Landlock is at least ABI `needed`; if not, the calling
/// test must stop. Below the ABI ral itself needs that is a [`skip`]; at or above
/// it, a [`skip_beyond_baseline`].
///
/// # Panics
/// The version query fails, or Landlock is below ral's own minimum and
/// `RAL_REQUIRE_SANDBOX` is set.
#[cfg(target_os = "linux")]
#[track_caller]
pub fn landlock_reaches(needed: u32) -> bool {
    let found = landlock_abi();
    let baseline = crate::authority::sandbox::BASELINE_ABI;
    if found >= needed {
        return true;
    }
    if found < baseline {
        skip(format_args!(
            "ral needs Landlock ABI {baseline}; found ABI {found} (0 is absent)"
        ));
    } else {
        skip_beyond_baseline(format_args!(
            "needs Landlock ABI {needed}; found ABI {found}"
        ));
    }
    false
}

/// [`landlock_reaches`] `$needed`, or return from the calling test.
#[macro_export]
macro_rules! require_landlock {
    ($needed:expr) => {
        #[cfg(target_os = "linux")]
        {
            if !$crate::test_helper::confinement::landlock_reaches($needed) {
                return;
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn already_confined_is_a_seatbelt_fact_not_diagnostic_wording() {
        use crate::error::Error;
        use crate::fact::{
            LaunchFailure, OsFailure, Reason, SeatbeltFailure, SeatbeltFailureAlreadyConfined,
            SeatbeltFailureEntry,
        };
        for detail in [None, Some("an unrelated external diagnostic".into())] {
            let reason = Reason::Launch(Box::new(LaunchFailure::Seatbelt(
                SeatbeltFailure::AlreadyConfined(SeatbeltFailureAlreadyConfined { detail }),
            )));
            assert!(already_confined(&End::Err(Error::new(reason))));
        }
        let wording = "this process is already inside a macOS sandbox profile";
        assert!(!already_confined(&End::Err(Error::new(Reason::raised(
            wording
        )))));
        let reason = Reason::Launch(Box::new(LaunchFailure::Seatbelt(SeatbeltFailure::Entry(
            SeatbeltFailureEntry {
                cause: OsFailure::from(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                detail: Some(wording.into()),
            },
        ))));
        assert!(!already_confined(&End::Err(Error::new(reason))));
    }

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
