//! Decode a capability `Value` — the argument of `grant [...]`, or a
//! `--capabilities` profile — into a resolved [`GrantLayer`].
//!
//! A dimension the record does not name stays `None` and inherits the
//! surrounding frame, so an attenuation touches only what it lists.  The
//! author is the live user, so every malformed shape is a strict error.
//! Two things are dropped as dead rather than failing the whole profile: a
//! path rooted under a foreign platform's convention, which can never match
//! an access here, and a bare exec name that finds nothing here, neither a
//! file on the `PATH` nor a tool this platform bundles.
//!
//! The runtime cannot tell a record from a map, so both are read, at every
//! depth.  Every list denotes a set: a repeat adds nothing and is no error.

use super::freeze::{Anchor, FreezeCtx, Place, Placed, PolicyError, Selector};
use crate::capability::Polar;
use crate::capability::{
    EditorVerb, ExecRules, FsLayer, GrantLayer, GrantPath, Keyed, Net, Rule, ShellVerb, name_rules,
};
use crate::path::lex::has_drive;
use crate::path::sigil::SigilHead;
use crate::path::{Excluded, Kind, PathRules, Trace, names_a_path_for};
use crate::syntax::quote::quote_word;
use crate::text::{indefinite, near_names};
use crate::typecheck::contract::{
    DIR_TARGET, EDITOR, EXEC_TARGET, Form as ContractForm, Holds, NET, PLACE, SHELL, Table, Vocab,
    code, declared,
};
use crate::types::{List, Map, Value, settings_map};
use std::collections::BTreeSet;
use std::str::FromStr;

/// The declared table of a nested grant key, whose words refuse an unknown
/// key at that depth.
fn nested(key: &str) -> &'static Table {
    match declared(ContractForm::Grant).holds(key) {
        Some(Holds::Table(table)) => table,
        _ => panic!("`grant` declares `{key}` as a nested table"),
    }
}

// ── Tagged values ─────────────────────────────────────────────────────────

/// A tagged value: its label and payload.
struct Tag<'a> {
    label: &'a str,
    payload: Option<&'a Value>,
}

impl<'a> Tag<'a> {
    fn of(value: &'a Value) -> Option<Self> {
        match value {
            Value::Variant { label, payload } => Some(Self {
                label,
                payload: payload.as_deref(),
            }),
            _ => None,
        }
    }

    /// The string payload, `what` naming what it should hold; `vocab` advises.
    fn text(&self, vocab: &Vocab, what: &str) -> Result<&'a str, PolicyError> {
        let tag = code(&format!("`{}", self.label));
        match self.payload {
            Some(Value::String(s)) => Ok(s.as_str()),
            Some(other) => Err(PolicyError::new(format!(
                "{tag} takes {what}, got {}",
                other.type_name()
            ))
            .with_hint(vocab.payload)),
            None => Err(PolicyError::new(format!(
                "{tag} needs a payload: {what}, as in {}",
                code(&format!("`{} …", self.label))
            ))
            .with_hint(vocab.payload)),
        }
    }

    /// A nullary tag takes no payload, not even `()`.
    fn nullary(&self, vocab: &Vocab) -> Result<(), PolicyError> {
        match self.payload {
            None => Ok(()),
            Some(_) => Err(PolicyError::new(format!(
                "{0} takes no payload; write it bare, {0}",
                code(&format!("`{}", self.label))
            ))
            .with_hint(vocab.payload)),
        }
    }
}

/// Prefix each refusal with the key it came from.
fn within(prefix: &str) -> impl Fn(PolicyError) -> PolicyError + '_ {
    move |e| PolicyError {
        message: format!("{prefix}: {}", e.message),
        ..e
    }
}

fn describe(value: &Value) -> String {
    match value {
        Value::String(s) => format!("the string {}", quote_word(s.as_str())),
        other => other.type_name().to_string(),
    }
}

/// A tag `vocab`'s row lacks, in the words the checker uses for it.
fn unknown_tag(vocab: &Vocab, tag: &str) -> PolicyError {
    let (message, advice) = vocab.refuse(tag);
    let error = PolicyError::new(message);
    match advice {
        Some(advice) => error.with_hint(advice),
        None => error,
    }
}

/// A value that is no tag at all, where `vocab` wants one.
fn untagged(vocab: &Vocab, value: &Value) -> PolicyError {
    PolicyError::new(format!(
        "expected {}, got {}",
        indefinite(vocab.noun),
        describe(value)
    ))
    .with_hint(vocab.untagged)
}

fn list_of(value: &Value, noun: &str) -> Result<List, PolicyError> {
    match value {
        Value::List(items) => Ok(items.clone()),
        other => Err(PolicyError::new(format!(
            "expected a list of {noun}, got {}",
            describe(other)
        ))),
    }
}

/// A nested table is a record; `empty` is how one admitting nothing is written.
fn table_of<'a>(value: &'a Value, empty: &str) -> Result<&'a Map, PolicyError> {
    match value {
        Value::Map(m) => Ok(m),
        other => Err(PolicyError::new(format!(
            "a grant table is a record, got {}; for a table that admits nothing write `{empty}`",
            describe(other)
        ))),
    }
}

// ── Places and selectors ──────────────────────────────────────────────────

/// A place tag and its subpath; `None` for a tag that is not a place.
fn place_of(tag: &Tag) -> Result<Option<Place>, PolicyError> {
    let Some(anchor) = Anchor::of_tag(tag.label) else {
        return Ok(None);
    };
    Ok(Some(anchor.place(tag.text(&PLACE, "a path")?.into())))
}

fn selector_of(tag: &Tag) -> Result<Option<Selector>, PolicyError> {
    let Ok(selector) = tag.label.parse() else {
        return Ok(None);
    };
    tag.nullary(&DIR_TARGET)?;
    Ok(Some(selector))
}

/// One entry of an fs list.
fn decode_place(value: &Value) -> Result<Place, PolicyError> {
    let tag = Tag::of(value).ok_or_else(|| untagged(&PLACE, value))?;
    place_of(&tag)?.ok_or_else(|| unknown_tag(&PLACE, tag.label))
}

/// The places of an fs list, each walked as polarity `W` walks.
fn freeze_places<W: Polar>(
    list: &List,
    ctx: &FreezeCtx,
) -> Result<Vec<GrantPath<W::Grant>>, PolicyError> {
    list.iter()
        .filter_map(|item| {
            decode_place(&item)
                .and_then(|place| ctx.walked::<W>(&place))
                .and_then(|placed| placed.map(Placed::grant).transpose())
                .transpose()
        })
        .collect()
}

// ── Dimension decoders ────────────────────────────────────────────────────

/// `fs: [read: [...], write: [...], deny: [...]]`, lists of places, where
/// `deny` carves holes inside the `read`/`write` prefix regions.
fn decode_fs(value: &Value, err_prefix: &str, ctx: &FreezeCtx) -> Result<FsLayer, PolicyError> {
    let at = within(err_prefix);
    let (mut read, mut write, mut deny) = (Vec::new(), Vec::new(), Vec::new());
    for (sub, paths) in table_of(value, "fs: [read: []]").map_err(&at)? {
        let list = |paths: &Value| list_of(paths, "places").map_err(&at);
        match sub {
            "read" => read = freeze_places::<Trace>(&list(&paths)?, ctx).map_err(&at)?,
            "write" => write = freeze_places::<Trace>(&list(&paths)?, ctx).map_err(&at)?,
            "deny" => deny = freeze_places::<Excluded>(&list(&paths)?, ctx).map_err(&at)?,
            _ => return Err(at(PolicyError::new(nested("fs").unknown_key(sub)))),
        }
    }
    Ok(FsLayer::new(read, write, deny))
}

/// `net` takes `offline`, the one restriction; omit it to inherit.
fn decode_net(value: &Value, err_prefix: &str) -> Result<Net, PolicyError> {
    let at = within(err_prefix);
    let tag = Tag::of(value).ok_or_else(|| at(untagged(&NET, value)))?;
    let net = tag
        .label
        .parse()
        .map_err(|_| at(unknown_tag(&NET, tag.label)))?;
    tag.nullary(&NET).map_err(at)?;
    Ok(net)
}

/// `editor`/`shell`: a list of verbs, each a nullary tag of `V`.
fn decode_verbs<V: FromStr + Ord>(
    value: &Value,
    vocab: &Vocab,
    err_prefix: &str,
) -> Result<BTreeSet<V>, PolicyError> {
    let at = within(err_prefix);
    list_of(value, &format!("{}s", vocab.noun))
        .map_err(&at)?
        .iter()
        .map(|item| {
            let tag = Tag::of(&item).ok_or_else(|| untagged(vocab, &item))?;
            let verb = tag
                .label
                .parse()
                .map_err(|_| unknown_tag(vocab, tag.label))?;
            tag.nullary(vocab).map(|()| verb)
        })
        .collect::<Result<_, _>>()
        .map_err(at)
}

// ── Exec targets ──────────────────────────────────────────────────────────

/// A name is a bare command name; a path is a place, which is judged by its
/// own rules.
fn bare_name(name: &str) -> Result<(), PolicyError> {
    if !path_shaped(name) {
        return Ok(());
    }
    Err(PolicyError::new(format!(
        "{} takes a bare command name, not {}",
        code("`name"),
        quote_word(name)
    ))
    .with_hint(
        "for a path write `` `absolute /path `` or `` `cwd relative/path ``; \
         for a directory of programs, `` `dir `absolute /path ``",
    ))
}

/// Whether `name` reads as a place rather than a command name: empty,
/// drive-headed, a path by [`names_a_path_for`], a dot directory, or
/// sigil-headed.  A name is judged under Windows rules on every host, the
/// stricter reading, so a profile refuses alike everywhere: `a\b` is a bare
/// filename under Posix rules, but a profile that loaded on Linux must not
/// fail on Windows.
fn path_shaped(name: &str) -> bool {
    name.is_empty()
        || has_drive(name)
        || names_a_path_for(name, PathRules::Windows)
        || matches!(name, "." | "..")
        || SigilHead::of_for(name, PathRules::Windows).is_some()
}

/// An unwrapped place names one executable; a directory there is a mistake
/// the author can fix by wrapping it in `dir`.
fn file_rules<W: Keyed>(place: &Place, ctx: &FreezeCtx) -> Result<Vec<Rule>, PolicyError> {
    let Some(placed) = ctx.walked::<W>(place)? else {
        return Ok(vec![]);
    };
    if let Placed::Walked(walked) = &placed
        && walked.real.followed().kind() == Some(Kind::Dir)
    {
        let spelled = format!("`{} {}", place.tag(), quote_word(place.payload()));
        return Err(PolicyError::new(format!(
            "{} is a directory, so as an exec target it names a binary that \
             cannot exist",
            code(&spelled)
        ))
        .with_hint(format!(
            "did you mean {}?",
            code(&format!("`dir {spelled}"))
        )));
    }
    Ok(vec![W::file(placed.grant()?)?])
}

/// `dir D`: every directory `D`, a place or a selector, resolves to.
fn dir_rules<W: Keyed>(tag: &Tag, ctx: &FreezeCtx) -> Result<Vec<Rule>, PolicyError> {
    let payload = tag.payload.ok_or_else(|| {
        PolicyError::new(format!("{} needs a place or a selector", code("`dir")))
            .with_hint(DIR_TARGET.untagged)
    })?;
    let inner = Tag::of(payload).ok_or_else(|| untagged(&DIR_TARGET, payload))?;
    let dirs = if let Some(selector) = selector_of(&inner)? {
        ctx.selector::<W>(selector)?
    } else if let Some(place) = place_of(&inner)? {
        ctx.walked::<W>(&place)?
            .map(Placed::grant)
            .transpose()?
            .into_iter()
            .collect()
    } else {
        return Err(unknown_tag(&DIR_TARGET, inner.label));
    };
    Ok(dirs.into_iter().map(W::dir).collect())
}

/// One entry of an `allow` or `deny` list, as the rules polarity `W` makes
/// of it.
fn decode_target<W: Keyed>(value: &Value, ctx: &FreezeCtx) -> Result<Vec<Rule>, PolicyError> {
    let tag = Tag::of(value).ok_or_else(|| untagged(&EXEC_TARGET, value))?;
    match tag.label {
        "name" => {
            let name = tag.text(&EXEC_TARGET, "a command name")?;
            bare_name(name)?;
            let path = ctx.env().get_or_host("PATH");
            name_rules::<W>(name, path.as_deref(), cfg!(unix))
        }
        "dir" => dir_rules::<W>(&tag, ctx),
        _ => match place_of(&tag)? {
            Some(place) => file_rules::<W>(&place, ctx),
            None => Err(unknown_tag(&EXEC_TARGET, tag.label)),
        },
    }
}

/// An `allow` or `deny` list, the rules of each target at polarity `W`.
fn decode_targets<W: Keyed>(list: &List, ctx: &FreezeCtx) -> Result<Vec<Rule>, PolicyError> {
    let rules: Vec<_> = list
        .iter()
        .map(|item| decode_target::<W>(&item, ctx))
        .collect::<Result<_, _>>()?;
    Ok(rules.into_iter().flatten().collect())
}

/// `exec: [allow: [...], deny: [...]]`.  Argument restrictions are gone: a
/// target is admitted or denied whatever its arguments.
fn decode_exec(value: &Value, err_prefix: &str, ctx: &FreezeCtx) -> Result<ExecRules, PolicyError> {
    let at = within(err_prefix);
    let mut rules = Vec::new();
    for (key, targets) in table_of(value, "exec: [allow: []]").map_err(&at)? {
        let decode = match key {
            "allow" => decode_targets::<Trace>,
            "deny" => decode_targets::<Excluded>,
            other => return Err(at(unknown_exec_key(other, &targets))),
        };
        let list = list_of(&targets, "targets").map_err(&at)?;
        rules.extend(decode(&list, ctx).map_err(&at)?);
    }
    Ok(rules.into_iter().collect())
}

/// Whether `value` has the shape of the old surface's per-command policy:
/// `'allow'`, `'deny'`, or a list of subcommands.
fn is_old_policy(value: &Value) -> bool {
    match value {
        Value::String(_) => true,
        Value::List(items) => items.iter().all(|i| matches!(*i, Value::String(_))),
        _ => false,
    }
}

fn unknown_exec_key(key: &str, value: &Value) -> PolicyError {
    let err = PolicyError::new(nested("exec").unknown_key(key));
    if !is_old_policy(value) || !near_names(key, ["allow", "deny"], 1).is_empty() {
        return err;
    }
    err.with_hint(format!(
        "per-command policies and argument restrictions are no longer supported; \
         write {} or {}",
        code(&format!("allow: [`name {key}]")),
        code(&format!("deny: [`name {key}]")),
    ))
}

// ── Capability walker ─────────────────────────────────────────────────────

/// Walk a capability record into a resolved [`GrantLayer`], walking every
/// place against `ctx`.
///
/// The single `Value → GrantLayer` constructor for a record, shared by the
/// `grant [...] { body }` builtin in `evaluator::scope`, the
/// profile loader in `load::profile`, and [`crate::SpawnGrant`]'s
/// resolver; every spelling is walked against `ctx` right here, so "a
/// `GrantLayer` has every spelling already resolved" holds by construction
/// whoever comes through.  Unknown keys error here and in each dimension's
/// decoder rather than being silently dropped; the keyset is `grant`'s own
/// declared table, so this door and the checker cannot drift apart.
///
/// # Errors
/// A [`PolicyError`] naming the key whose shape or value it will not read, or
/// the place that would not resolve absolute or whose walk failed.
pub fn decode_capability_map(
    value: &Value,
    err_prefix: &str,
    ctx: &FreezeCtx,
) -> Result<GrantLayer, PolicyError> {
    let entries = settings_map(value).ok_or_else(|| {
        PolicyError::new(format!(
            "{err_prefix} expects a record or `()`, got {}",
            value.type_name()
        ))
    })?;
    let (mut exec, mut fs, mut net, mut editor, mut shell) = (None, None, None, None, None);
    for (k, v) in &*entries {
        let at = format!("{err_prefix} {k}");
        match k {
            "exec" => exec = Some(decode_exec(&v, &at, ctx)?),
            "fs" => fs = Some(decode_fs(&v, &at, ctx)?),
            "net" => net = Some(decode_net(&v, &at)?),
            "editor" => editor = Some(decode_verbs::<EditorVerb>(&v, &EDITOR, &at)?),
            "shell" => shell = Some(decode_verbs::<ShellVerb>(&v, &SHELL, &at)?),
            other => {
                let table = declared(ContractForm::Grant);
                return Err(PolicyError::new(match table.holds(other) {
                    Some(Holds::Refused(advice)) => format!("{err_prefix}: {advice}"),
                    _ => format!("{err_prefix}: {}", table.unknown_key(other)),
                }));
            }
        }
    }
    Ok(GrantLayer::new(fs, exec, net, editor, shell))
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs scaffolding: FreezeCtx cwds and tempdir files"
)]
mod tests {
    use super::*;
    use crate::capability::{ExecAllow, ExecDeny, FsAllow, FsOp};
    #[cfg(unix)]
    use crate::capability::{ExecDenial, Floor, Program, Withheld};
    use crate::path::command_name_key;
    use crate::path::utf8;
    #[cfg(unix)]
    use crate::test_helper::{located, stack, trace};

    fn bare(label: &str) -> Value {
        Value::variant(label, None)
    }

    fn tagged(label: &str, payload: &str) -> Value {
        Value::variant(label, Some(Value::string(payload)))
    }

    fn dir(inner: Value) -> Value {
        Value::variant("dir", Some(inner))
    }

    fn rec(entries: &[(&str, Value)]) -> Value {
        Value::Map(
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    fn list(items: Vec<Value>) -> Value {
        Value::list(items)
    }

    fn exec(allow: Vec<Value>, deny: Vec<Value>) -> Value {
        rec(&[("allow", list(allow)), ("deny", list(deny))])
    }

    /// Host-absolute anchors: `/work` is drive-relative on Windows, which
    /// `anchored` refuses.
    fn ctx() -> FreezeCtx {
        let (home, cwd) = if cfg!(windows) {
            (r"C:\h", r"C:\work")
        } else {
            ("/h", "/work")
        };
        FreezeCtx::at(
            std::iter::once(("HOME", home)).collect::<crate::types::EnvVars>(),
            crate::path::CwdPath::new(cwd.into()).expect("a fixture cwd is absolute"),
        )
    }

    fn decode(value: &Value) -> Result<GrantLayer, PolicyError> {
        decode_capability_map(value, "test", &ctx())
    }

    fn rules_of(exec_value: Value) -> Result<ExecRules, PolicyError> {
        decode(&rec(&[("exec", exec_value)])).map(|layer| layer.exec().cloned().unwrap())
    }

    /// The tool and found-file allowances, by the bare name they answer to.
    fn allowed_names(rules: &ExecRules) -> BTreeSet<&str> {
        rules
            .allows()
            .filter_map(|a| match a {
                ExecAllow::Tool(name)
                | ExecAllow::File {
                    found_as: Some(name),
                    ..
                } => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }

    fn vetoed(rules: &ExecRules, name: &str) -> bool {
        rules.denies().any(|d| *d == ExecDeny::Name(name.into()))
    }

    /// The allowed files and directories as written, a directory under `dir`.
    #[cfg(unix)]
    fn written(rules: &ExecRules) -> Vec<String> {
        rules
            .allows()
            .filter_map(|a| match a {
                ExecAllow::Dir(dir) => Some(format!("dir {}", dir.written.as_str())),
                ExecAllow::File { written, .. } => Some(written.as_str().to_string()),
                _ => None,
            })
            .collect()
    }

    /// The read allowances of an fs layer as written.
    #[cfg(unix)]
    fn read_prefixes(layer: &GrantLayer) -> Vec<&str> {
        layer
            .fs()
            .expect("an fs layer")
            .table(&FsOp::Read)
            .allows()
            .map(|a| match a {
                FsAllow::Prefix(prefix) => prefix.written.as_str(),
                FsAllow::RunPlace(_) => panic!("a decoded fs list holds no run place"),
            })
            .collect()
    }

    fn refusal(value: &Value) -> PolicyError {
        decode(value).unwrap_err()
    }

    fn exec_refusal(exec_value: Value) -> PolicyError {
        refusal(&rec(&[("exec", exec_value)]))
    }

    fn fs_refusal(read: Vec<Value>) -> PolicyError {
        refusal(&rec(&[("fs", rec(&[("read", list(read))]))]))
    }

    // ── exec ──

    #[test]
    fn exec_names_decode_to_allowances_and_vetoes_by_their_list() {
        let g = rules_of(exec(
            vec![tagged("name", "ls")],
            vec![tagged("name", "bash")],
        ))
        .unwrap();
        assert_eq!(allowed_names(&g), BTreeSet::from(["ls"]));
        assert!(vetoed(&g, "bash"), "{g:?}");
        assert!(!vetoed(&g, "ls"), "{g:?}");
    }

    /// A name the platform does not bundle and the `PATH` does not find allows
    /// nothing, and keeps its veto: off Unix a Unix-only tool is such a name.
    #[cfg(feature = "coreutils")]
    #[test]
    fn a_dead_name_makes_no_allowance_and_keeps_its_veto() {
        let tmp = tempfile::tempdir().unwrap();
        let path = crate::path::utf8(tmp.path()).unwrap();
        let dead = crate::uutils::COREUTILS_UNIX_ONLY_TOOLS
            .iter()
            .chain(&["zzz-no-such"]);
        for name in dead {
            let allow = name_rules::<Trace>(name, Some(path), false).unwrap();
            assert!(allow.is_empty(), "{name}: {allow:?}");
            let deny = name_rules::<Excluded>(name, Some(path), false).unwrap();
            assert!(
                matches!(&deny[..], [Rule::Deny(d)] if **d == ExecDeny::Name(command_name_key(name))),
                "{name}: {deny:?}"
            );
        }
        let kept = name_rules::<Trace>("ls", Some(path), false).unwrap();
        assert!(
            matches!(&kept[..], [Rule::Allow(ExecAllow::Tool(name))] if name == "ls"),
            "a cross-platform bundled tool survives: {kept:?}"
        );
    }

    /// A bare name decodes to the file the `PATH` finds, or to no allowance and
    /// its veto, whatever `unix_available` says: the platform decides only
    /// what it bundles, and no tool is bundled here.
    #[test]
    fn a_name_is_a_found_file_or_no_rule_with_unix_available_a_parameter() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = crate::test_helper::real(tmp.path()).as_path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let file = bin.join(format!("zz-found{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&file, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = Some(utf8(&bin).unwrap());
        for unix_available in [true, false] {
            let found = name_rules::<Trace>("zz-found", path, unix_available).unwrap();
            assert!(
                matches!(&found[..], [Rule::Allow(ExecAllow::File { found_as: Some(n), .. })] if n == "zz-found"),
                "{unix_available}: {found:?}"
            );
            let lost = name_rules::<Trace>("zz-lost", path, unix_available).unwrap();
            assert!(lost.is_empty(), "{unix_available}: {lost:?}");
            let veto = name_rules::<Excluded>("zz-lost", path, unix_available).unwrap();
            assert!(
                matches!(&veto[..], [Rule::Deny(d)] if **d == ExecDeny::Name(command_name_key("zz-lost"))),
                "{unix_available}: {veto:?}"
            );
        }
    }

    /// A name is resolved where it is decoded, on the `PATH` of the decoding
    /// env: no later `PATH` redirects it, and a name found nowhere keeps its
    /// veto, which holds whatever file bears the name.
    #[cfg(unix)]
    #[test]
    fn a_name_found_at_decode_is_not_redirected_and_a_lost_name_keeps_its_veto() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::test_helper::real(tmp.path()).as_path().to_path_buf();
        let (first, later) = (root.join("first"), root.join("later"));
        for (bin, names) in [(&first, &["git"][..]), (&later, &["git", "zz-lost"][..])] {
            std::fs::create_dir(bin).unwrap();
            for name in names {
                std::fs::write(bin.join(name), "").unwrap();
                std::fs::set_permissions(bin.join(name), PermissionsExt::from_mode(0o755)).unwrap();
            }
        }
        let on = |path: &std::path::Path| {
            let env = [("HOME", "/h"), ("PATH", utf8(path).unwrap())];
            let cwd = crate::path::CwdPath::new(root.clone()).unwrap();
            FreezeCtx::at(env.into_iter().collect::<crate::types::EnvVars>(), cwd)
        };
        let layer = decode_capability_map(
            &rec(&[(
                "exec",
                exec(vec![tagged("name", "git")], vec![tagged("name", "zz-lost")]),
            )]),
            "test",
            &on(&first),
        )
        .unwrap();
        let grants = stack([layer]);
        let floor = Floor::default();
        let refusal = |at: &std::path::Path| {
            grants.refusal(
                &floor,
                &Program::File {
                    located: Box::new(located(at)),
                    arg0: at.to_path_buf(),
                },
            )
        };
        assert!(refusal(&first.join("git")).is_none());
        assert!(
            matches!(
                refusal(&later.join("git")),
                Some(ExecDenial::Run(Withheld::MissingAllowance))
            ),
            "a later `PATH` finds another git, which the grant never named"
        );
        assert!(
            refusal(&later.join("zz-lost")).is_some_and(|denied| matches!(
                denied,
                ExecDenial::Run(Withheld::Excluded { scope, .. })
                    if *scope == ExecDeny::Name(command_name_key("zz-lost"))
            ))
        );
    }

    #[cfg(unix)]
    #[test]
    fn exec_places_decode_to_file_allowances_and_dir_to_dir_allowances() {
        let g = rules_of(exec(
            vec![
                tagged("absolute", "/bin/sh"),
                tagged("cwd", "tools/build"),
                dir(tagged("absolute", "/opt/tools")),
            ],
            vec![],
        ))
        .unwrap();
        assert_eq!(
            written(&g),
            ["dir /opt/tools", "/bin/sh", "/work/tools/build"]
        );
        assert_eq!(g.denies().count(), 0);
    }

    #[test]
    fn exec_dir_system_expands_to_the_tool_roots() {
        let g = rules_of(exec(vec![dir(bare("system"))], vec![])).unwrap();
        assert!(g.allows().count() >= 1);
        assert!(g.allows().all(|a| matches!(a, ExecAllow::Dir(_))));
        assert_eq!(g.denies().count(), 0);
    }

    /// A list is a set: a repeat adds nothing, and refuses nothing.
    #[test]
    fn exec_repeats_are_idempotent() {
        let once = rules_of(exec(vec![tagged("name", "ls")], vec![])).unwrap();
        let twice = rules_of(exec(
            vec![tagged("name", "ls"), tagged("name", "ls")],
            vec![],
        ))
        .unwrap();
        assert_eq!(once, twice);
    }

    /// A target in both lists is an allowance and a veto, which refuses.
    #[test]
    fn a_target_in_both_lists_is_both() {
        let g = rules_of(exec(vec![tagged("name", "ls")], vec![tagged("name", "ls")])).unwrap();
        assert_eq!(allowed_names(&g), BTreeSet::from(["ls"]));
        assert!(vetoed(&g, "ls"), "{g:?}");
    }

    #[test]
    fn an_empty_allow_list_admits_nothing() {
        let g = rules_of(rec(&[("allow", list(vec![]))])).unwrap();
        assert_eq!((g.allows().count(), g.denies().count()), (0, 0));
    }

    #[test]
    fn a_path_shaped_name_is_refused_with_a_place_hint() {
        for name in [
            "./tool",
            "/bin/sh",
            "~",
            "xdg:bin",
            r"xdg:config\x",
            r"~\x",
            "..",
            "",
            "C:tool.exe",
            "c:x",
            "a\\b",
        ] {
            let e = exec_refusal(exec(vec![tagged("name", name)], vec![]));
            assert!(
                e.message.contains("bare command name"),
                "{name}: {}",
                e.message
            );
            assert!(e.hint.unwrap().contains("`` `absolute /path ``"));
        }
    }

    #[test]
    fn a_bare_string_target_offers_name_or_a_place() {
        let e = exec_refusal(exec(vec![Value::string("git")], vec![]));
        let hint = e.hint.unwrap();
        assert!(hint.contains("`name git"), "{hint}");
        assert!(hint.contains("`absolute"), "{hint}");
    }

    #[test]
    fn an_unknown_target_tag_lists_the_targets_and_offers_the_nearest() {
        let e = exec_refusal(exec(vec![tagged("nmae", "git")], vec![]));
        assert!(
            e.message.contains("`` `nmae `` is not an exec target;"),
            "{}",
            e.message
        );
        assert!(
            e.message.ends_with("(did you mean `` `name ``?)"),
            "{}",
            e.message
        );
        assert!(
            e.message.contains("`dir") && e.message.contains("`xdg-bin"),
            "{}",
            e.message
        );
    }

    #[test]
    fn an_unwrapped_selector_is_refused_with_the_dir_hint() {
        for sel in ["path", "system", "homebrew"] {
            let e = exec_refusal(exec(vec![bare(sel)], vec![]));
            let hint = e.hint.unwrap();
            assert!(hint.contains(&format!("write `` `dir `{sel} ``")), "{hint}");
        }
    }

    #[test]
    fn an_unwrapped_directory_place_is_refused_with_the_dir_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        let e = exec_refusal(exec(vec![tagged("absolute", &path)], vec![]));
        assert!(e.message.contains("is a directory"), "{}", e.message);
        assert!(e.hint.unwrap().contains("`dir `absolute"), "dir hint");
    }

    #[test]
    fn dir_takes_a_place_or_a_selector_only() {
        for payload in [
            tagged("name", "git"),
            dir(bare("path")),
            Value::string("/x"),
        ] {
            let e = exec_refusal(exec(vec![dir(payload)], vec![]));
            assert!(e.message.contains("place or a selector"), "{}", e.message);
        }
        let e = exec_refusal(exec(vec![bare("dir")], vec![]));
        assert!(
            e.message.contains("`` `dir `` needs a place or a selector"),
            "{}",
            e.message
        );
    }

    #[test]
    fn nullary_tags_refuse_a_payload_even_unit() {
        let e = exec_refusal(exec(
            vec![dir(Value::variant("system", Some(Value::Unit)))],
            vec![],
        ));
        assert!(e.message.contains("takes no payload"), "{}", e.message);
    }

    /// Per-command maps, string policies and subcommand lists are gone, and
    /// are never read as an unrestricted allow.
    #[test]
    fn obsolete_exec_policies_are_refused() {
        for value in [
            Value::string("allow"),
            list(vec![Value::string("git")]),
            rec(&[("git", Value::string("allow"))]),
            rec(&[("cargo", list(vec![Value::string("build")]))]),
        ] {
            let e = exec_refusal(value.clone());
            let text = format!("{} {}", e.message, e.hint.clone().unwrap_or_default());
            assert!(
                text.contains("record") || text.contains("no longer supported"),
                "{value:?}: {text}"
            );
        }
    }

    #[test]
    fn a_misspelt_exec_key_offers_the_real_ones() {
        let e = exec_refusal(rec(&[("alow", list(vec![]))]));
        assert!(e.message.contains("takes allow, deny"), "{}", e.message);
        assert!(
            e.message.ends_with("(did you mean 'allow'?)"),
            "{}",
            e.message
        );
        assert!(e.hint.is_none());
    }

    /// The per-command migration hint is for the old string surface's shapes;
    /// any other value under an unknown key is a plain unknown key.
    #[test]
    fn the_per_command_hint_is_only_for_the_old_policy_shapes() {
        for value in [Value::string("allow"), list(vec![Value::string("build")])] {
            let hint = exec_refusal(rec(&[("git", value)])).hint.unwrap();
            assert!(hint.contains("`` allow: [`name git] ``"), "{hint}");
        }
        for value in [list(vec![bare("git")]), Value::Bool(true), rec(&[])] {
            let e = exec_refusal(rec(&[("only", value)]));
            assert!(e.message.contains("unknown key 'only'"), "{}", e.message);
            assert!(e.hint.is_none(), "{:?}", e.hint);
        }
    }

    /// An unknown key is refused before its value is read.
    #[test]
    fn an_unknown_fs_key_is_refused_before_its_places() {
        let e = refusal(&rec(&[(
            "fs",
            rec(&[("raed", list(vec![tagged("cwdd", ".")]))]),
        )]));
        assert!(e.message.contains("unknown key 'raed'"), "{}", e.message);
    }

    /// Specific advice replaces the nearest-tag guess, which would contradict it.
    #[test]
    fn a_dir_of_a_name_gives_its_advice_and_no_guess() {
        let e = exec_refusal(exec(vec![dir(tagged("name", "git"))], vec![]));
        assert!(!e.message.contains("did you mean"), "{}", e.message);
        assert!(e.hint.unwrap().contains("`` `name git ``"));
    }

    #[test]
    fn a_nested_table_is_a_record() {
        for value in [Value::Unit, rec(&[]), Value::string("x")] {
            let r = decode(&rec(&[("fs", value.clone())]));
            match value {
                // an empty map is a record at run time: it restricts to the floor
                Value::Map(_) => assert!(r.is_ok()),
                _ => assert!(r.unwrap_err().message.contains("a grant table is a record")),
            }
        }
        let e = exec_refusal(Value::Unit);
        assert!(e.message.contains("exec: [allow: []]"), "{}", e.message);
    }

    // ── fs ──

    #[cfg(unix)]
    #[test]
    fn fs_lists_decode_places_as_sets() {
        let places = vec![
            tagged("cwd", "src"),
            tagged("cwd", "src"),
            tagged("home", ".cache"),
        ];
        let layer = decode(&rec(&[("fs", rec(&[("read", list(places))]))])).unwrap();
        assert_eq!(read_prefixes(&layer), ["/h/.cache", "/work/src"]);
    }

    #[test]
    fn fs_lists_refuse_what_is_not_a_place() {
        for item in [
            tagged("name", "git"),
            dir(tagged("cwd", ".")),
            bare("path"),
            bare("system"),
            bare("homebrew"),
        ] {
            let e = fs_refusal(vec![item]);
            assert!(e.message.contains("is not a place"), "{}", e.message);
            assert!(e.hint.unwrap().contains("fs lists take places"));
        }
        let e = fs_refusal(vec![Value::string("/tmp")]);
        assert!(e.message.contains("expected a place"), "{}", e.message);
        assert!(e.hint.unwrap().contains("`absolute /tmp"));
        let m = fs_refusal(vec![tagged("cwdd", "x")]).message;
        assert!(
            m.contains(
                "`` `cwdd `` is not a place; a place is one of `` `absolute ``, `` `cwd ``, "
            ),
            "{m}"
        );
        assert!(
            m.ends_with("`` `xdg-bin `` (did you mean `` `cwd ``?)"),
            "{m}"
        );
        let m = fs_refusal(vec![bare("cwd")]).message;
        assert!(m.contains("`` `cwd `` needs a payload"), "{m}");
    }

    #[test]
    fn an_empty_fs_list_keeps_only_the_floor() {
        let layer = decode(&rec(&[("fs", rec(&[("read", list(vec![]))]))])).unwrap();
        assert_eq!(layer.fs(), Some(&FsLayer::default()));
    }

    #[test]
    fn a_dead_place_is_dropped_not_refused() {
        let foreign = if cfg!(windows) { "/x" } else { r"C:\x" };
        let layer = decode(&rec(&[(
            "fs",
            rec(&[("read", list(vec![tagged("absolute", foreign)]))]),
        )]))
        .unwrap();
        assert_eq!(layer.fs(), Some(&FsLayer::default()));
    }

    /// `/tmp` is a Unix place: Windows has no such root, so it is dropped.
    #[cfg(windows)]
    #[test]
    fn a_unix_tmp_is_dropped_on_windows() {
        let layer = decode(&rec(&[(
            "fs",
            rec(&[("read", list(vec![tagged("absolute", "/tmp")]))]),
        )]))
        .unwrap();
        assert_eq!(layer.fs(), Some(&FsLayer::default()));
    }

    /// A freeze anchor at the real `home` and `root`, binding `extra` too.
    #[cfg(unix)]
    fn anchored_at(
        home: &std::path::Path,
        root: &std::path::Path,
        extra: &[(&str, &std::path::Path)],
    ) -> FreezeCtx {
        let env = std::iter::once(("HOME", home))
            .chain(extra.iter().copied())
            .map(|(k, v)| (k, utf8(v).unwrap()))
            .collect::<crate::types::EnvVars>();
        FreezeCtx::at(env, crate::path::CwdPath::new(root.to_path_buf()).unwrap())
    }

    /// A tempdir and the real path of its root.
    #[cfg(unix)]
    fn tree() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::test_helper::real(tmp.path()).as_path().to_path_buf();
        (tmp, root)
    }

    /// A deny through a directory the user cannot enter is resolved at its
    /// position, and holds what lies beneath it.
    #[cfg(unix)]
    #[test]
    fn a_deny_through_an_unreadable_directory_resolves_positionally_and_holds_beneath() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            eprintln!("skip: root enters every directory");
            return;
        }
        let (_tmp, root) = tree();
        let (locked, open) = (root.join("locked"), root.join("open"));
        std::fs::create_dir(&locked).unwrap();
        std::fs::create_dir(&open).unwrap();
        let secret = locked.join("secret");
        std::fs::set_permissions(&locked, PermissionsExt::from_mode(0o000)).unwrap();
        let decoded = decode_capability_map(
            &rec(&[(
                "fs",
                rec(&[
                    ("read", list(vec![tagged("absolute", utf8(&root).unwrap())])),
                    (
                        "deny",
                        list(vec![tagged("absolute", utf8(&secret).unwrap())]),
                    ),
                ]),
            )]),
            "test",
            &anchored_at(&root, &root, &[]),
        );
        std::fs::set_permissions(&locked, PermissionsExt::from_mode(0o700)).unwrap();
        let grants = stack([decoded.expect("an unenterable directory is no refusal")]);
        let floor = Floor::default();
        for held in [secret.clone(), secret.join("f")] {
            assert!(
                matches!(
                    grants.read(&floor, &trace(&held)),
                    Err(Withheld::Excluded { .. })
                ),
                "{held:?}"
            );
        }
        assert!(grants.read(&floor, &trace(open.join("f"))).is_ok());
    }

    /// A spelling whose walk fails, neither absent nor unenterable, refuses
    /// the layer and names the spelling, whichever axis wrote it.
    #[cfg(unix)]
    #[test]
    fn a_spelling_whose_walk_fails_refuses_the_layer_naming_it() {
        let (_tmp, root) = tree();
        let (a, b) = (root.join("a"), root.join("b"));
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        let looped = a.join("x");
        let place = tagged("absolute", utf8(&looped).unwrap());
        for value in [
            rec(&[("fs", rec(&[("read", list(vec![place.clone()]))]))]),
            rec(&[("fs", rec(&[("deny", list(vec![place.clone()]))]))]),
            rec(&[("exec", exec(vec![dir(place)], vec![]))]),
        ] {
            let why = decode_capability_map(&value, "test", &anchored_at(&root, &root, &[]))
                .expect_err("a link cycle cannot be walked");
            assert!(
                why.message.contains(utf8(&looped).unwrap()),
                "{}",
                why.message
            );
        }
    }

    /// An exclusion of the root is refused where it is decoded, whichever axis
    /// writes it.
    #[test]
    fn a_root_exclusion_is_refused_at_decode() {
        let root = tagged("absolute", "/");
        for value in [
            rec(&[("fs", rec(&[("deny", list(vec![root.clone()]))]))]),
            rec(&[("exec", exec(vec![], vec![dir(root)]))]),
        ] {
            let why = refusal(&value);
            assert!(why.message.contains("filesystem root"), "{}", why.message);
        }
    }

    /// The root as an allowance is everywhere, on every host: an fs prefix,
    /// an exec `dir`; as an exec file it is a directory, not a program.
    #[test]
    fn an_authored_root_allowance_is_everywhere() {
        let root = tagged("absolute", "/");
        let layer = decode(&rec(&[("fs", rec(&[("read", list(vec![root.clone()]))]))])).unwrap();
        let everywhere = FsAllow::Prefix(GrantPath::everywhere());
        let fs = layer.fs().expect("an fs layer").table(&FsOp::Read);
        assert!(fs.allows().any(|a| *a == everywhere), "{fs:?}");
        let rules = rules_of(exec(vec![dir(root.clone())], vec![])).unwrap();
        let everywhere = ExecAllow::Dir(GrantPath::everywhere());
        assert!(rules.allows().any(|a| *a == everywhere), "{rules:?}");
        let why = exec_refusal(exec(vec![root], vec![]));
        assert!(why.message.contains("a directory"), "{}", why.message);
    }

    /// So is a link that leads to it.
    #[cfg(unix)]
    #[test]
    fn a_link_to_the_root_is_refused_as_an_exclusion() {
        let (_tmp, root) = tree();
        std::os::unix::fs::symlink("/", root.join("alias")).unwrap();
        let alias = tagged("absolute", utf8(&root.join("alias")).unwrap());
        let why = decode_capability_map(
            &rec(&[("fs", rec(&[("deny", list(vec![alias]))]))]),
            "test",
            &anchored_at(&root, &root, &[]),
        )
        .expect_err("a link to the root excludes every path");
        assert!(why.message.contains("filesystem root"), "{}", why.message);
    }

    /// `xdg-*` judges its base by the walk the layer keeps: a base linked out
    /// of the home is refused, one linked within it is held where it led, and
    /// stays there when the link is retargeted after.
    #[cfg(unix)]
    #[test]
    fn xdg_refuses_a_base_linked_out_of_home_as_it_was_at_decode() {
        let (_tmp, root) = tree();
        let (home, outside) = (root.join("home"), root.join("outside"));
        let inside = home.join("inside");
        for dir in [&inside, &outside] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let (leaves, stays) = (home.join("leaves"), home.join("stays"));
        std::os::unix::fs::symlink(&outside, &leaves).unwrap();
        std::os::unix::fs::symlink(&inside, &stays).unwrap();
        let read = |base: &std::path::Path| {
            decode_capability_map(
                &rec(&[(
                    "fs",
                    rec(&[("read", list(vec![tagged("xdg-config", ".")]))]),
                )]),
                "test",
                &anchored_at(&home, &root, &[("XDG_CONFIG_HOME", base)]),
            )
        };
        let why = read(&leaves).expect_err("a base linked out of HOME");
        assert!(why.message.contains("outside HOME"), "{}", why.message);

        let grants = stack([read(&stays).expect("a base linked within HOME")]);
        std::fs::remove_file(&stays).unwrap();
        std::os::unix::fs::symlink(&outside, &stays).unwrap();
        let floor = Floor::default();
        assert!(grants.read(&floor, &trace(inside.join("f"))).is_ok());
        assert!(grants.read(&floor, &trace(outside.join("f"))).is_err());
    }

    // ── net, editor, shell ──

    #[test]
    fn net_offline_decodes_and_nothing_else_does() {
        let layer = decode(&rec(&[("net", bare("offline"))])).unwrap();
        assert_eq!(layer.summary().unwrap().net, Some(Net::Offline));
        for value in [Value::Bool(true), Value::Bool(false)] {
            let e = refusal(&rec(&[("net", value)]));
            assert!(e.message.contains("expected a net mode"), "{}", e.message);
            assert!(
                e.hint
                    .unwrap()
                    .contains("omit `net` to inherit, or write `` net: `offline ``"),
                "hint"
            );
        }
        let m = refusal(&rec(&[("net", bare("online"))])).message;
        assert!(m.contains("`` `online `` is not a net mode"), "{m}");
        let m = refusal(&rec(&[(
            "net",
            Value::variant("offline", Some(Value::Unit)),
        )]))
        .message;
        assert!(m.contains("takes no payload"), "{m}");
    }

    #[test]
    fn verb_lists_decode_as_sets() {
        let summary = decode(&rec(&[
            (
                "editor",
                list(vec![bare("read"), bare("tui"), bare("read")]),
            ),
            ("shell", list(vec![])),
        ]))
        .unwrap()
        .summary()
        .unwrap();
        assert_eq!(
            summary.editor,
            Some([EditorVerb::Read, EditorVerb::Tui].into())
        );
        assert_eq!(summary.shell, Some(BTreeSet::new()));
    }

    #[test]
    fn an_unknown_verb_lists_the_offered_ones() {
        let m = refusal(&rec(&[("shell", list(vec![bare("chdri")]))])).message;
        assert!(m.contains("`` `chdri `` is not a verb"), "{m}");
        assert!(
            m.contains("`` `chdir ``, `` `detach ``") && m.contains("did you mean `` `chdir ``?"),
            "{m}"
        );
        let m = refusal(&rec(&[("editor", list(vec![bare("exec")]))])).message;
        assert!(m.contains("`` `read ``, `` `write ``, `` `tui ``"), "{m}");
    }

    #[test]
    fn the_boolean_record_surface_is_refused() {
        for key in ["editor", "shell"] {
            let m = refusal(&rec(&[(key, rec(&[("read", Value::Bool(true))]))])).message;
            assert!(m.contains("expected a list of"), "{m}");
        }
        let e = refusal(&rec(&[("shell", list(vec![Value::string("chdir")]))]));
        assert!(e.message.contains("expected a verb"), "{}", e.message);
        assert!(e.hint.unwrap().contains("`shell` takes the list of verbs"));
        let m = refusal(&rec(&[(
            "editor",
            list(vec![Value::variant("read", Some(Value::Unit))]),
        )]))
        .message;
        assert!(m.contains("takes no payload"), "{m}");
    }

    #[test]
    fn detach_is_a_shell_verb_not_a_key() {
        let e = refusal(&rec(&[("detach", Value::Bool(true))]));
        assert!(
            e.message.contains("`detach` is a shell verb"),
            "{}",
            e.message
        );
        assert!(
            e.message.contains("`` shell: [`chdir, `detach] ``"),
            "{}",
            e.message
        );
    }

    // ── drift ──

    /// A valid value for each key the grant table declares.
    fn representative(key: &str) -> Value {
        match key {
            "exec" => exec(vec![tagged("name", "git")], vec![dir(bare("system"))]),
            "fs" => rec(&[("read", list(vec![tagged("cwd", ".")]))]),
            "net" => bare("offline"),
            "editor" => list(vec![bare("read")]),
            "shell" => list(vec![bare("chdir")]),
            other => panic!("declared key '{other}' has no representative value here"),
        }
    }

    /// A valid list entry for each key a nested table declares.
    fn entry(table: &str) -> Value {
        match table {
            "exec" => tagged("name", "git"),
            "fs" => tagged("cwd", "."),
            other => panic!("nested table '{other}' has no representative entry here"),
        }
    }

    /// Every key `Form::Grant` declares, at every depth, reaches its own arm
    /// and decodes a valid value; a refused key errors in its table's advice.
    #[test]
    fn every_declared_grant_key_is_handled_by_decode_capability_map() {
        let table = declared(ContractForm::Grant);
        for key in table.keys {
            match &key.holds {
                Holds::Refused(advice) => {
                    let e = refusal(&rec(&[(key.label, Value::Unit)]));
                    assert_eq!(e.message, format!("test: {advice}"));
                }
                Holds::Table(sub) => {
                    for inner in sub.keys {
                        let value = rec(&[(inner.label, list(vec![entry(key.label)]))]);
                        decode(&rec(&[(key.label, value)])).unwrap_or_else(|e| {
                            panic!("'{} {}': {}", key.label, inner.label, e.message)
                        });
                    }
                }
                _ => {
                    let value = rec(&[(key.label, representative(key.label))]);
                    decode(&value).unwrap_or_else(|e| panic!("'{}': {}", key.label, e.message));
                }
            }
        }
    }

    /// A key the table does not declare is refused in that table's words, at
    /// each depth.
    #[test]
    fn an_unknown_key_is_refused_in_its_tables_words_at_each_depth() {
        let table = declared(ContractForm::Grant);
        let e = refusal(&rec(&[("bogus", Value::Unit)]));
        assert_eq!(e.message, format!("test: {}", table.unknown_key("bogus")));
        for key in table.keys {
            if let Holds::Table(sub) = &key.holds {
                let e = refusal(&rec(&[(key.label, rec(&[("bogus", list(vec![]))]))]));
                assert_eq!(
                    e.message,
                    format!("test {}: {}", key.label, sub.unknown_key("bogus"))
                );
            }
        }
    }
}
