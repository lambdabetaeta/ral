//! The runtime ABI shared by the host, guest PID 1, engine, synod and build-tools.
//! Changes to `boot`, `ports::FACTORY`, `layout` or `packet` bump `boot::CONTRACT`.

pub mod boot;
pub mod layout;
pub mod link;
pub mod packet;
pub mod ports;
