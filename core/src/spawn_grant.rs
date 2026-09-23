//! How a spawn's grant reaches a child that has not yet resolved it.
//!
//! A restriction record crosses unfrozen: a `cwd:` sigil in a grant names the
//! cwd of the shell that grant governs — the child's, never the spawn site's —
//! so the freeze belongs where the child's cwd is.  Hence a record here and
//! not a [`Capabilities`], whose paths are resolved by construction.

use serde::{Deserialize, Serialize};

use crate::serial::FOValue;
use crate::types::{Capabilities, Shell};

/// A base tag and the cwd in, one [`Capabilities`] layer out.
///
/// Pushed onto the spawned shell's stack rather than folded against what it
/// already carries: the stack itself is the meet, so the child's own ceiling
/// only needs to be resolved, not composed here. Evaluated where the host's
/// own capability vocabulary lives, since core carries no base-tag lexicon. A
/// field of [`crate::engine::EngineInstaller`] rather than a registered hook:
/// an installer is chosen at `Attach`, before a seed is applied or a fork
/// adopted, so the policy can be demanded of every host that dresses an engine
/// instead of left in a slot one of them might forget to fill.
pub type GrantNarrower = fn(&str, &std::path::Path) -> Result<Capabilities, String>;

/// A spawn's grant, as it crosses to a child that has not yet resolved it.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// The one layer a spawn pushes, frozen against `cwd`.
    ///
    /// # Errors
    /// Whatever `narrow` refuses a base with, or the capability decoder's own
    /// sentence for a restriction record it will not read.
    pub fn layer(
        &self,
        narrow: GrantNarrower,
        cwd: &std::path::Path,
        home: Option<&str>,
    ) -> Result<Capabilities, String> {
        use crate::path::sigil::FreezeCtx;
        use crate::types::Value;

        match self {
            Self::Inherit => Ok(Capabilities::root()),
            Self::Base(name) => narrow(name, cwd),
            Self::Restrict(record) => crate::capability::decode_capability_map(
                &Value::from(record.clone()),
                "grant",
                &FreezeCtx { home, cwd },
            )
            .map_err(|e| match e.hint {
                Some(hint) => format!("{}\nhint: {hint}", e.message),
                None => e.message,
            }),
        }
    }

    /// Push this grant's layer onto `shell`, frozen against the shell's own
    /// cwd and home: the one resolution a hatched seed and an adopted fork
    /// share.
    ///
    /// # Errors
    /// Whatever [`Self::layer`] refuses.
    pub(crate) fn narrow_onto(
        &self,
        shell: &mut Shell,
        narrow: GrantNarrower,
    ) -> Result<(), String> {
        let layer = self.layer(narrow, &shell.cwd(), shell.context.home().as_deref())?;
        shell.push_session_capabilities(layer);
        Ok(())
    }
}

// Unix-only: a frozen `cwd:` reads with `\` separators on Windows.
#[cfg(all(test, unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] literal cwds, named rather than resolved"
)]
mod tests {
    use super::*;
    use crate::path::NormalizedPrefix;
    use crate::types::Value;
    use std::path::Path;

    /// A narrower no test here may reach: only [`SpawnGrant::Base`] consults
    /// one, and these cases are the other two arms.
    fn unreachable_narrower(_grant: &str, _cwd: &Path) -> Result<Capabilities, String> {
        panic!("only a Base grant consults the narrower")
    }

    fn restrict(record: &Value) -> SpawnGrant {
        SpawnGrant::Restrict(FOValue::try_from(record).expect("a first-order grant record"))
    }

    #[test]
    fn inherit_attenuates_nothing() {
        let layer = SpawnGrant::Inherit
            .layer(unreachable_narrower, Path::new("/work/proj"), None)
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
                Value::list(vec![Value::String("cwd:".to_string())]),
            )]),
        )]);
        let layer = restrict(&record)
            .layer(unreachable_narrower, Path::new("/work/proj"), None)
            .expect("a well-formed restriction record");
        let read = layer.fs.expect("an fs policy").read_prefixes;
        assert_eq!(
            read.iter()
                .map(NormalizedPrefix::as_str)
                .collect::<Vec<_>>(),
            vec!["/work/proj"],
            "`cwd:` must name the cwd the layer is frozen against, not the process's"
        );
    }

    #[test]
    fn a_malformed_restriction_is_refused_by_the_decoder() {
        let record = Value::map(vec![("net".to_string(), Value::String("yes".to_string()))]);
        let refusal = restrict(&record)
            .layer(unreachable_narrower, Path::new("/work/proj"), None)
            .expect_err("a net axis that is not a Bool");
        assert_eq!(refusal, "grant net: expected a Bool, got String");
    }
}
