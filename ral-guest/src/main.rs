//! PID 1 refusal precedes every side effect.

#![allow(
    clippy::disallowed_methods,
    reason = "a guest init program: its whole job is the syscalls the bans route, and it links no ral shell"
)]
#![allow(
    clippy::disallowed_macros,
    reason = "[silent:guest-main] the guest init program has no ral shell to print through"
)]

#[cfg(target_os = "linux")]
mod assemble;
#[cfg(target_os = "linux")]
mod engine;
#[cfg(target_os = "linux")]
mod init;
#[cfg(target_os = "linux")]
mod mount;
#[cfg(target_os = "linux")]
mod net;
#[cfg(target_os = "linux")]
mod pump;
#[cfg(target_os = "linux")]
mod reap;
#[cfg(target_os = "linux")]
mod sysctl;
#[cfg(target_os = "linux")]
mod vsock;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    let Err(refusal) = run();
    eprintln!("ral-guest: {refusal}");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
fn run() -> Result<std::convert::Infallible, String> {
    refuse_unless_init()?;
    assemble::assemble()?;
    init::serve()
}

#[cfg(target_os = "linux")]
fn refuse_unless_init() -> Result<(), String> {
    let pid = rustix::process::getpid();
    if pid.is_init() {
        Ok(())
    } else {
        Err(format!(
            "ral-guest is the init process of a ral guest, not a command. It is what runs as \
             pid 1 inside the virtual machine synod and exarch boot; here it is pid {}, where \
             it would mount over your filesystems and set your machine's clock from a kernel \
             command line. Nothing was done.",
            pid.as_raw_nonzero()
        ))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn a_non_init_process_is_refused_with_its_pid() {
        let refusal = super::refuse_unless_init().expect_err("a test process is not pid 1");
        assert!(refusal.contains("pid"), "{refusal}");
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("ral-guest: the init process of a Linux guest; this is not one.");
    std::process::ExitCode::FAILURE
}
