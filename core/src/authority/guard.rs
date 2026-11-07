//! In-process enforcement: judge actions against a grant stack and explain
//! refusals. The shell records denials at its boundary.

mod enforce;

pub(crate) use enforce::{
    Denial, check_device_name, check_exec, check_exec_name, check_flag, fs_denial,
};
