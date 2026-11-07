//! How a spawn's grant reaches a child that has not yet resolved it.
//!
//! A restriction record crosses unfrozen: a `cwd` place in a grant names the
//! cwd of the shell that grant governs — the child's, never the spawn site's —
//! so the freeze belongs where the child's cwd is.  Hence a record here and
//! not a [`GrantLayer`], whose spellings are resolved by construction.

use serde::{Deserialize, Serialize};

use super::decode::decode_grant;
use crate::authority::grant::freeze::FreezeCtx;
use crate::authority::policy::GrantLayer;
use crate::data::FOValue;
use crate::error::Error;
use crate::shell::runtime::Shell;

/// A base tag and the anchor to freeze it against (the child's cwd and
/// scratch), in; one [`GrantLayer`] out.
///
/// Pushed onto the spawned shell's stack rather than folded against what it
/// already carries: the stack itself is the meet, so the child's own ceiling
/// only needs to be resolved, not composed here. Evaluated where the host's
/// own capability vocabulary lives, since core carries no base-tag lexicon. A
/// field of [`crate::engine::EngineInstaller`] rather than a registered hook:
/// an installer is chosen at `Attach`, before a seed is applied or a fork
/// adopted, so the policy can be demanded of every host that dresses an engine
/// instead of left in a slot one of them might forget to fill.
pub type GrantNarrower = fn(&str, &FreezeCtx) -> Result<GrantLayer, Error>;

/// A spawn's grant, as it crosses to a child that has not yet resolved it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SpawnGrant {
    /// The parent's authority verbatim: ⊤, a layer that says nothing.
    Inherit,
    /// A bake-in base, named — core has no lexicon for it, so the host's
    /// narrower resolves it.
    Base(String),
    /// A restriction record, unfrozen: frozen against the child's own cwd,
    /// never the spawn site's.
    Restrict(FOValue),
}

impl SpawnGrant {
    /// The one layer a spawn pushes, frozen against `ctx`.
    ///
    /// # Errors
    /// Whatever `narrow` refuses a base with, or the capability decoder's own
    /// sentence for a restriction record it will not read.
    pub fn layer(&self, narrow: GrantNarrower, ctx: &FreezeCtx) -> Result<GrantLayer, Error> {
        match self {
            Self::Inherit => Ok(GrantLayer::root()),
            Self::Base(name) => narrow(name, ctx),
            Self::Restrict(record) => decode_grant(record, "grant", ctx),
        }
    }

    /// Push this grant's layer onto `shell`, frozen against the shell's own
    /// anchor: the one resolution a sealed seed and an adopted fork share.
    ///
    /// # Errors
    /// Whatever [`Self::layer`] refuses.
    pub(crate) fn narrow_onto(
        &self,
        shell: &mut Shell,
        narrow: GrantNarrower,
    ) -> Result<(), Error> {
        let layer = self.layer(narrow, &shell.freeze_ctx())?;
        shell.push_session_capabilities(layer);
        Ok(())
    }
}

// Unix-only: a frozen `cwd` reads with `\` separators on Windows.
#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::authority::policy::{FsAllow, FsOp};
    use crate::shell::grant::decode_capability_map;
    use crate::shell::runtime::Value;

    /// A narrower no test here may reach: only [`SpawnGrant::Base`] consults
    /// one, and these cases are the other two arms.
    fn unreachable_narrower(_grant: &str, _ctx: &FreezeCtx) -> Result<GrantLayer, Error> {
        panic!("only a Base grant consults the narrower")
    }

    fn cwd(cwd: &str) -> FreezeCtx {
        FreezeCtx::at(
            crate::host::EnvVars::new(),
            crate::path::CwdPath::new(cwd.into()).expect("a fixture cwd is absolute"),
        )
    }

    fn restrict(record: &Value) -> SpawnGrant {
        SpawnGrant::Restrict(FOValue::try_from(record).expect("a first-order grant record"))
    }

    #[test]
    fn inherit_attenuates_nothing() {
        let layer = SpawnGrant::Inherit
            .layer(unreachable_narrower, &cwd("/work/proj"))
            .expect("inherit resolves without a narrower");
        assert!(
            !layer.is_restrictive(),
            "inheriting the parent's authority must push the lattice top"
        );
    }

    #[test]
    fn a_restriction_freezes_against_the_cwd_it_is_given() {
        let record = Value::map(vec![(
            "fs".to_string(),
            Value::map(vec![(
                "read".to_string(),
                Value::list(vec![Value::variant("cwd", Some(Value::string(".")))]),
            )]),
        )]);
        let layer = restrict(&record)
            .layer(unreachable_narrower, &cwd("/work/proj"))
            .expect("a well-formed restriction record");
        let fs = layer.fs().expect("an fs policy");
        let read: Vec<_> = (fs.table(FsOp::Read).allows())
            .map(|allow| match allow {
                FsAllow::Prefix(gp) => gp.written.as_str(),
                FsAllow::RunPlace(_) => panic!("a restriction names no run place"),
            })
            .collect();
        assert_eq!(
            read,
            vec!["/work/proj"],
            "`cwd .` must name the cwd the layer is frozen against, not the process's"
        );
    }

    #[test]
    fn a_malformed_restriction_is_refused_by_the_decoder() {
        let record = Value::map(vec![("net".to_string(), Value::string("yes"))]);
        let ctx = cwd("/work/proj");
        let refusal = restrict(&record)
            .layer(unreachable_narrower, &ctx)
            .expect_err("a net axis that is not `offline`");
        let decoder = decode_capability_map(&record, "grant", &ctx)
            .expect_err("the decoder refuses it first");
        assert!(!decoder.hints().is_empty(), "the refusal carries a hint");
        assert_eq!(refusal.to_string(), decoder.to_string());
        assert_eq!(refusal.hints(), decoder.hints(), "the hint survives");
    }

    #[test]
    fn obsolete_argument_policies_are_refused_off_the_wire() {
        let name_git = Value::list(vec![Value::variant("name", Some(Value::string("git")))]);
        let status = Value::list(vec![Value::string("status")]);
        let records = [
            ("git", Value::map(vec![("git".to_string(), status.clone())])),
            (
                "only",
                Value::map(vec![
                    ("allow".to_string(), name_git),
                    ("only".to_string(), status),
                ]),
            ),
        ];
        for (key, exec) in records {
            let record = Value::map(vec![("exec".to_string(), exec)]);
            let refusal = restrict(&record)
                .layer(unreachable_narrower, &cwd("/work/proj"))
                .expect_err("an argument policy must not decode");
            let said = refusal.describe();
            assert!(said.contains(&format!("unknown key '{key}'")), "{said}");
            assert!(
                (refusal.hints().iter())
                    .any(|h| h.contains("argument restrictions are no longer supported")),
                "{refusal:?}"
            );
        }
    }

    fn offline() -> GrantLayer {
        GrantLayer::new(
            None,
            None,
            Some(crate::authority::policy::Net::Offline),
            None,
            None,
            None,
        )
    }

    /// Only the name `tight` resolves, so the result is the narrower's own.
    fn tight_only(base: &str, _ctx: &FreezeCtx) -> Result<GrantLayer, Error> {
        if base == "tight" {
            Ok(offline())
        } else {
            Err(Error::new(base))
        }
    }

    #[test]
    fn a_base_is_resolved_by_the_narrower_it_is_given() {
        let ctx = cwd("/work/proj");
        let layer = SpawnGrant::Base("tight".into())
            .layer(tight_only, &ctx)
            .expect("the narrower resolves `tight`");
        assert_eq!(layer, offline());
        assert!(layer.is_restrictive());
        let refusal = SpawnGrant::Base("loose".into())
            .layer(tight_only, &ctx)
            .expect_err("the narrower refuses `loose`");
        assert_eq!(
            refusal.describe(),
            "loose",
            "the narrower's refusal is the layer's, verbatim"
        );
    }
}
