//! `Observe(Register)` — a read of the shell's store, in computation
//! position: what `$CWD`, `$ENV`, and a `~`-path are.
//!
//! Reaches the five pseudo-variables through [`Shell::pseudo_var`] by name,
//! exactly as [`Shell::lookup_value_name`] does for a lexical-miss fallback —
//! the two readers are not yet folded into one because that needs
//! `types/shell/mod.rs`, which this parcel does not own. W2g's job.

use crate::ir::Register;
use crate::path::tilde::{Unexpandable, expand_tilde_path};
use crate::types::{Error, Shell, Value};

/// A read of the store: the five pseudo-variables, total by
/// [`Shell::pseudo_var`], and a `~`-path awaiting `HOME`.
pub(crate) fn observe(reg: &Register, shell: &Shell) -> Result<Value, Error> {
    match reg {
        Register::Env => Ok(shell.pseudo_var("ENV").expect("ENV is a total pseudo-var")),
        Register::Args => Ok(shell
            .pseudo_var("ARGS")
            .expect("ARGS is a total pseudo-var")),
        Register::Nproc => Ok(shell
            .pseudo_var("NPROC")
            .expect("NPROC is a total pseudo-var")),
        Register::Cwd => Ok(shell.pseudo_var("CWD").expect("CWD is a total pseudo-var")),
        Register::User => Ok(shell
            .pseudo_var("USER")
            .expect("USER is a total pseudo-var")),
        Register::Tilde(path) => {
            let home = shell.mobile.context.home();
            expand_tilde_path(
                path.user.as_deref(),
                path.suffix.as_deref(),
                home.as_deref(),
            )
            .map(Value::String)
            .map_err(|cause| {
                shell.err_hint(
                    format!("cannot resolve {}: {}", path.to_literal(), cause.why()),
                    match cause {
                        Unexpandable::HomeUnknown => "set HOME, or spell out an explicit path",
                        Unexpandable::ForeignUser => {
                            "use bare ~ for the current user, or spell out an explicit path"
                        }
                    },
                    1,
                )
            })
        }
    }
}
