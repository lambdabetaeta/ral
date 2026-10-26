//! Resolve authored policies into the tables a layer holds.

use super::freeze::PolicyError;
use super::paths::Polar;
use crate::authority::policy::{
    Capabilities, ExecAllow, ExecDeny, ExecGrant, ExecKey, ExecRules, Extent, FsLayer, FsPolicy,
    GrantLayer, GrantPath, Rule,
};
use crate::host::EnvVars;
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

/// The rules of one authored key at polarity `W`.
fn key_rules<W: Keyed>(key: &ExecKey, path: Option<&str>) -> Result<Vec<Rule>, PolicyError> {
    Ok(match key {
        ExecKey::Name(name) => name_rules::<W>(name, path, cfg!(unix))?,
        ExecKey::Path(file) => vec![W::file(GrantPath::<W>::walk(file.clone())?)?],
        ExecKey::Dir(dir) => vec![W::dir(GrantPath::<W>::walk(dir.clone())?)],
    })
}

impl ExecRules {
    /// The rules of an authored grant: each key walked as its polarity walks,
    /// a bare name on the `PATH` of `env`.  `every` is [`ExecAllow::Any`].
    ///
    /// # Errors
    /// A spelling that cannot be resolved, named; a root exclusion.
    pub(crate) fn of_grant(grant: &ExecGrant, env: &EnvVars) -> Result<Self, PolicyError> {
        let path = env.get_or_host("PATH");
        let mut rules = Vec::new();
        if grant.every() {
            rules.push(Rule::Allow(ExecAllow::Any));
        }
        for key in grant.allows() {
            rules.extend(key_rules::<Trace>(key, path.as_deref())?);
        }
        for key in grant.denies() {
            rules.extend(key_rules::<Excluded>(key, path.as_deref())?);
        }
        Ok(rules.into_iter().collect())
    }
}

impl FsLayer {
    /// The layer an authored policy makes: each spelling walked as its
    /// polarity walks.
    ///
    /// # Errors
    /// A spelling that cannot be resolved, named; a root exclusion.
    pub(crate) fn of_policy(policy: FsPolicy) -> Result<Self, PolicyError> {
        let FsPolicy {
            read_prefixes,
            write_prefixes,
            deny_paths,
        } = policy;
        fn walk<W: Polar>(paths: Vec<FrozenPath>) -> Result<Vec<GrantPath<W::Grant>>, PolicyError> {
            paths.into_iter().map(GrantPath::<W>::walk).collect()
        }
        Ok(Self::new(
            walk::<Trace>(read_prefixes)?,
            walk::<Trace>(write_prefixes)?,
            walk::<Excluded>(deny_paths)?,
        ))
    }
}

impl GrantLayer {
    /// Resolve every spelling of `caps`; `env` for the `PATH` a `` `name ``
    /// key is found on (every [`FrozenPath`] is already absolute).
    ///
    /// # Errors
    /// A spelling that cannot be resolved, named; a root exclusion.
    pub fn from_capabilities(caps: Capabilities, env: &EnvVars) -> Result<Self, PolicyError> {
        let Capabilities {
            exec,
            fs,
            net,
            gpu,
            editor,
            shell,
        } = caps;
        Ok(Self::new(
            fs.map(FsLayer::of_policy).transpose()?,
            exec.map(|grant| ExecRules::of_grant(&grant, env))
                .transpose()?,
            net,
            gpu,
            editor,
            shell,
        ))
    }
}
