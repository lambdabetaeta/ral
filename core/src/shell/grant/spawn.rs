//! How a spawn's grant reaches a child that has not yet resolved it.
//!
//! A restriction record crosses unfrozen: a `cwd` place in a grant names the
//! cwd of the shell that grant governs, the child's and never the spawn site's,
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
    /// A bake-in base, by name; core has no lexicon for it, so the host's
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
    /// typed refusal for a restriction record it will not read.
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

impl crate::data::json::JsonDecode for SpawnGrant {
    fn decode_json(
        value: crate::data::json::JsonRef<'_, '_>,
    ) -> Result<Self, crate::fact::DecodeFailure> {
        crate::data::json::decode_structural::<crate::seed::json::RawSpawnGrant>(value)?.validate()
    }
}

impl crate::data::json::JsonEncode for SpawnGrant {
    fn encode_json(
        &self,
        format: crate::data::json::JsonFormat,
    ) -> Result<Vec<u8>, crate::fact::JsonEncodeFailure> {
        Ok(crate::data::json::serialize_structural(
            &crate::seed::json::RawSpawnGrant::from_value(self),
            format,
        ))
    }
}

// Unix-only: a frozen `cwd` reads with `\` separators on Windows.
#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::authority::policy::{FsAllow, FsOp};
    use crate::environ::EnvVars;
    use crate::fact::{
        ExarchFailure, HostFailure, PolicyFailure, PolicyFailureUnknownBase, Reason,
    };
    use crate::shell::runtime::Value;

    /// A narrower no test here may reach: only [`SpawnGrant::Base`] consults
    /// one, and these cases are the other two arms.
    fn unreachable_narrower(_grant: &str, _ctx: &FreezeCtx) -> Result<GrantLayer, Error> {
        panic!("only a Base grant consults the narrower")
    }

    fn cwd(cwd: &str) -> FreezeCtx {
        FreezeCtx::at(
            EnvVars::new(),
            crate::path::CwdPath::new(cwd.into()).expect("a fixture cwd is absolute"),
        )
    }

    fn restrict(record: &Value) -> SpawnGrant {
        SpawnGrant::Restrict(FOValue::try_from(record).expect("a first-order grant record"))
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

    fn offline() -> GrantLayer {
        GrantLayer {
            net: Some(crate::authority::policy::Net::Offline),
            ..GrantLayer::root()
        }
    }

    /// Only the name `tight` resolves, so the result is the narrower's own.
    fn tight_only(base: &str, _ctx: &FreezeCtx) -> Result<GrantLayer, Error> {
        if base == "tight" {
            Ok(offline())
        } else {
            Err(Error::new(Reason::Host(Box::new(HostFailure::Exarch(
                ExarchFailure::Policy(PolicyFailure::UnknownBase(PolicyFailureUnknownBase {
                    name: base.into(),
                    offered: vec!["tight".into()],
                })),
            ))))
            .with_context("policy base")
            .with_hint("check the selected project policy"))
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
        let original = tight_only("loose", &ctx).unwrap_err();
        assert_eq!(refusal.reason, original.reason);
        assert_eq!(refusal.context(), Some("policy base"));
        assert_eq!(refusal.hints(), original.hints());
    }
}
