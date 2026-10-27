//! The rootfs has no engine; install it fresh on the session overlay before switching root.
//! Assembly and supervision share the same PID 1 process image.

mod plan;
mod run;

pub(crate) use run::assemble;
