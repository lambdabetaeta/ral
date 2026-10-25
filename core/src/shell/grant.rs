//! Decode grants and freeze them against the shell's own anchor.

mod decode;
mod spawn;

pub use decode::decode_capability_map;
pub use spawn::{GrantNarrower, SpawnGrant};

use crate::authority::grant::freeze::{FreezeCtx, PolicyError};
use crate::shell::runtime::{Break, Error, Shell};

impl From<PolicyError> for Break {
    fn from(e: PolicyError) -> Self {
        let err = Error::new(e.message);
        Self::Error(match e.hint {
            Some(hint) => err.with_hint(hint),
            None => err,
        })
    }
}

impl Shell {
    /// The environment, logical cwd and session scratch every grant freezes against.
    pub(crate) fn freeze_ctx(&self) -> FreezeCtx {
        FreezeCtx::at(
            self.context.env_overrides().clone(),
            self.context.cwd().clone(),
        )
        .with_scratch(self.scratch())
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::authority::grant::freeze::Place;
    use crate::path::CwdPath;
    use crate::path::lex::fold_dots;

    #[test]
    fn a_shells_context_carries_the_scratch_its_front_end_set() {
        let scratch = std::env::temp_dir().join("a-shells-scratch");
        let mut shell = crate::test_helper::core_shell();
        shell.set_scratch(scratch.to_str().unwrap());
        let path = shell
            .freeze_ctx()
            .place(&Place::Scratch(".".into()))
            .unwrap()
            .unwrap();
        assert_eq!(path.as_str(), fold_dots(&scratch).to_string_lossy());
        let bare = crate::test_helper::core_shell();
        let err = bare
            .freeze_ctx()
            .place(&Place::Scratch(".".into()))
            .unwrap_err();
        assert!(err.message.contains("no scratch directory"), "{err:?}");
    }

    #[test]
    fn a_shells_context_snapshots_its_overlay_and_logical_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let home = root.join("home");
        let cwd = root.join("cwd");
        let mut shell = crate::test_helper::core_shell();
        shell.set_env_var("HOME", home.to_str().unwrap());
        shell.seed_cwd(CwdPath::new(cwd.clone()).unwrap());
        let ctx = shell.freeze_ctx();
        shell.set_env_var("HOME", root.to_str().unwrap());
        shell.seed_cwd(CwdPath::new(root.to_path_buf()).unwrap());
        for (place, expected) in [
            (Place::Home(".".into()), home),
            (Place::Cwd(".".into()), cwd),
        ] {
            let path = ctx.place(&place).unwrap().unwrap();
            assert_eq!(path.as_str(), fold_dots(&expected).to_string_lossy());
        }
    }

    #[test]
    fn a_policy_refusal_keeps_its_message_and_hint_at_the_shell_boundary() {
        for hint in [None, Some("an alternative".to_owned())] {
            let error = PolicyError {
                message: "a refusal".into(),
                hint: hint.clone(),
            };
            let Break::Error(error) = Break::from(error) else {
                panic!("a policy refusal is never an escape")
            };
            assert_eq!(error.message, "a refusal");
            assert_eq!(error.hint.as_deref(), hint.as_deref());
        }
    }
}
