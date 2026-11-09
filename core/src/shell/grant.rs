//! Decode grants and freeze them against the shell's own anchor.

mod decode;
mod spawn;

pub(crate) use decode::decode_capability_map;
pub use decode::{decode_fragment, decode_grant};
pub use spawn::{GrantNarrower, SpawnGrant};

use crate::authority::grant::freeze::FreezeCtx;
use crate::authority::policy::GrantLayer;
use crate::data::FOValue;
use crate::environ::EnvVars;
use crate::error::Error;
use crate::shell::runtime::Shell;

impl Shell {
    /// The environment, logical cwd and session scratch every grant freezes against.
    pub(crate) fn freeze_ctx(&self) -> FreezeCtx {
        self.freeze_ctx_with_env(self.context.env_overrides().clone())
    }

    fn freeze_ctx_with_env(&self, env: EnvVars) -> FreezeCtx {
        FreezeCtx::at(env, self.context.cwd().clone()).with_scratch(self.scratch())
    }

    /// `record`, a grant value that crossed as data, resolved against this
    /// shell's own anchor.
    ///
    /// # Errors
    /// As [`decode_grant`].
    pub fn decode_grant(&self, record: &FOValue, err_prefix: &str) -> Result<GrantLayer, Error> {
        decode_grant(record, err_prefix, &self.freeze_ctx())
    }

    /// Decode against an effective environment overlay without installing it.
    /// `env` replaces the shell's overlay snapshot; host fallback is unchanged.
    /// The cwd and scratch remain this shell's, as in [`Self::decode_grant`].
    /// A root recipe can thus resolve its ceiling before seeding its bindings.
    ///
    /// # Errors
    /// As [`decode_grant`].
    pub fn decode_grant_with_env(
        &self,
        record: &FOValue,
        err_prefix: &str,
        env: EnvVars,
    ) -> Result<GrantLayer, Error> {
        decode_grant(record, err_prefix, &self.freeze_ctx_with_env(env))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use crate::authority::grant::freeze::Anchor;
    use crate::path::{CwdPath, fold_dots};

    #[test]
    fn a_shells_context_carries_the_scratch_its_front_end_set() {
        let scratch = std::env::temp_dir().join("a-shells-scratch");
        let mut shell = crate::test_helper::core_shell();
        shell.set_scratch(scratch.to_str().unwrap());
        let path = shell
            .freeze_ctx()
            .place(&Anchor::Scratch.place("."))
            .unwrap()
            .unwrap();
        assert_eq!(path.as_str(), fold_dots(&scratch).to_string_lossy());
        let bare = crate::test_helper::core_shell();
        let err = bare
            .freeze_ctx()
            .place(&Anchor::Scratch.place("."))
            .unwrap_err();
        assert!(err.describe().contains("no scratch directory"), "{err:?}");
    }

    #[test]
    fn a_shells_context_snapshots_its_overlay_and_logical_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let home = root.join("home");
        let cwd = root.join("cwd");
        let mut shell = crate::test_helper::core_shell();
        shell.set_env_var("HOME", home.to_str().unwrap()).unwrap();
        shell.seed_cwd(CwdPath::new(cwd.clone()).unwrap());
        let ctx = shell.freeze_ctx();
        shell.set_env_var("HOME", root.to_str().unwrap()).unwrap();
        shell.seed_cwd(CwdPath::new(root.to_path_buf()).unwrap());
        for (place, expected) in [
            (Anchor::Home.place("."), home),
            (Anchor::Cwd.place("."), cwd),
        ] {
            let path = ctx.place(&place).unwrap().unwrap();
            assert_eq!(path.as_str(), fold_dots(&expected).to_string_lossy());
        }
    }
}
