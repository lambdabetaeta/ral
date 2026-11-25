//! `OpenVMM` boots the guest on the Windows Hypervisor Platform, under the user's own token.
//!
//! The wire formats are platform-neutral, so their golden tests run on any host.

mod proto;
mod relay;
mod ttrpc;

#[cfg(windows)]
mod afunix;
#[cfg(windows)]
mod console;
#[cfg(windows)]
mod hypervisor;
#[cfg(windows)]
mod process;
#[cfg(windows)]
pub(crate) mod win;

#[cfg(windows)]
pub(crate) use hypervisor::{OpenVmm, available};
