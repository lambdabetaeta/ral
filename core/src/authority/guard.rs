//! In-process enforcement: judge actions against a grant stack and explain
//! refusals. The shell records denials at its boundary.

mod enforce;

pub(crate) use enforce::{
    Denial, Refusal, check_device_name, check_exec, check_flag, check_fs_exact, deny_head,
    fs_denial,
};
