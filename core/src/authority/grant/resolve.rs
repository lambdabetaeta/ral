//! How a walked exec target becomes the rules a layer holds.

use super::freeze::PolicyError;
use super::paths::Polar;
use crate::authority::policy::{ExecAllow, ExecDeny, Extent, GrantPath, Rule};
use crate::path::{self, Excluded, FrozenPath, Trace, command_name_key};

/// How a walked key becomes a rule, by the polarity that walked it: an
/// allowance keeps the position, an exclusion the holds.
pub(crate) trait Keyed: Polar {
    /// # Errors
    /// A file key naming the root (the root is a directory, not a program),
    /// or what the polarity refuses of the root.
    fn file(path: GrantPath<Self::Grant>) -> Result<Rule, PolicyError>;
    fn dir(path: GrantPath<Self::Grant>) -> Rule;
    /// A bare name: the tool bundled under it and the file the `PATH` found,
    /// never the root; for an exclusion the veto on the name everywhere.
    ///
    /// # Errors
    /// What the polarity refuses of the found file.
    fn name(
        name: &str,
        bundled: bool,
        found: Option<GrantPath<Self>>,
    ) -> Result<Vec<Rule>, PolicyError>;
}

impl Keyed for Trace {
    fn file(path: GrantPath<Extent>) -> Result<Rule, PolicyError> {
        match path.real {
            Extent::Under { position: real, .. } => Ok(Rule::Allow(ExecAllow::File {
                real,
                written: path.written,
                found_as: None,
            })),
            Extent::Everywhere => Err(PolicyError::root_program(&path.written)),
        }
    }

    fn dir(path: GrantPath<Extent>) -> Rule {
        Rule::Allow(ExecAllow::Dir(path))
    }

    fn name(
        name: &str,
        bundled: bool,
        found: Option<GrantPath<Self>>,
    ) -> Result<Vec<Rule>, PolicyError> {
        let tool = bundled.then(|| Rule::Allow(ExecAllow::Tool(name.into())));
        let file = found.map(|path| {
            Rule::Allow(ExecAllow::File {
                real: path.real.real().clone(),
                written: path.written,
                found_as: Some(name.into()),
            })
        });
        Ok(tool.into_iter().chain(file).collect())
    }
}

impl Keyed for Excluded {
    fn file(path: GrantPath<Self>) -> Result<Rule, PolicyError> {
        Ok(Rule::Deny(Box::new(ExecDeny::File(path))))
    }

    fn dir(path: GrantPath<Self>) -> Rule {
        Rule::Deny(Box::new(ExecDeny::Dir(path)))
    }

    fn name(name: &str, _: bool, found: Option<GrantPath<Self>>) -> Result<Vec<Rule>, PolicyError> {
        let file = found
            .map(Self::grant)
            .transpose()?
            .map(|path| Rule::Deny(Box::new(ExecDeny::File(path))));
        let veto = Rule::Deny(Box::new(ExecDeny::Name(command_name_key(name))));
        Ok(file.into_iter().chain([veto]).collect())
    }
}

/// The rules a bare `name` makes on `path`: the bundled tool, where this
/// platform bundles it (`unix_available` says whether it counts the Unix-only
/// ones), and the file the `PATH` finds, walked as its polarity walks; a name
/// found nowhere makes no allowance and keeps its veto.  The `PATH` is read
/// on its absolute entries: a relative entry is the environment's, not a
/// grant's.
///
/// # Errors
/// A found file whose walk fails, named.
pub(crate) fn name_rules<W: Keyed>(
    name: &str,
    path: Option<&str>,
    unix_available: bool,
) -> Result<Vec<Rule>, PolicyError> {
    let found = path::locate(name, path, None)
        .and_then(|hit| FrozenPath::freeze(&hit).ok())
        .map(GrantPath::<W>::walked)
        .transpose()?;
    W::name(name, crate::uutils::bundled(name, unix_available), found)
}
