//! Decode a capability `ValClos` (the argument of `grant [...]`, a
//! `--capabilities` profile, or a grant value one host authored for another)
//! into a resolved [`GrantLayer`]: the one grant front-end.
//!
//! A dimension the record does not name stays `None` and inherits the
//! surrounding frame, so an attenuation touches only what it lists.  Every
//! malformed shape is a strict error, whoever authored it.
//! Two things are dropped as dead rather than failing the whole profile: a
//! path rooted under a foreign platform's convention, which can never match
//! an access here, and a bare exec name that finds nothing here, neither a
//! file on the `PATH` nor a tool this platform bundles.
//!
//! The runtime cannot tell a record from a map, so both are read, at every
//! depth.  Every list denotes a set: a repeat adds nothing and is no error.

use crate::authority::grant::freeze::{Anchor, FreezeCtx, Place, Selector};
use crate::authority::grant::freeze::{grant_error, invalid_error};
use crate::authority::grant::value::Target;
use crate::authority::grant::{Keyed, Placed, Polar, name_rules};
use crate::authority::policy::{
    EditorVerb, ExecRules, Fragment, FsLayer, Gpu, GrantLayer, GrantPath, Net, Rule, ShellVerb,
};
use crate::data::FOValue;
use crate::error::Error;
use crate::fact::{
    DataStep, Expected, Found, GrantFailure, GrantFailureRefusedKey, InvalidCause, Reason,
    ShapeFailure, TextPreview, UnexpectedPayload, ValueShape,
};

use crate::path::{Excluded, Kind, PathRules, SigilHead, Trace, has_drive, names_a_path_for};
use crate::shell::runtime::{ListRef, MapRef, ValClos, View};
use crate::text::near_names;
use crate::typecheck::table::{
    DIR_TARGET, EDITOR, EXEC_TARGET, Form as TableForm, GPU, Holds, NET, PLACE, SHELL, Table,
    Vocab, code, declared,
};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::str::FromStr;

fn found(value: &ValClos) -> Found {
    match value.view() {
        View::Unit => Found::Unit,
        View::Bool(v) => Found::Bool(v),
        View::Int(v) => Found::Int(v),
        View::Float(v) => Found::Float(v),
        View::String(v) => Found::Text(TextPreview::new(v.as_str())),
        View::Bytes(_) => Found::Bytes,
        View::List(_) => Found::List,
        View::Map(_) => Found::Map,
        View::Variant { label, .. } => Found::Variant(label.to_string()),
        View::Thunk { comp, .. } if comp.lam().is_none() => Found::Block,
        View::Thunk { .. } | View::Builtin(_) => Found::Function,
        View::Handle(_) => Found::Handle,
    }
}

fn shape(value: &ValClos, expected: Expected) -> Error {
    invalid_error(InvalidCause::Shape(ShapeFailure {
        expected,
        found: found(value),
    }))
}

fn at(mut error: Error, step: DataStep) -> Error {
    if let Reason::Invalid(failure) = &mut error.reason {
        failure.path.insert(0, step);
    }
    error
}

fn at_field(error: Error, field: &str) -> Error {
    at(error, DataStep::Field(field.into()))
}
fn at_index(error: Error, index: usize) -> Error {
    at(error, DataStep::Index(index.into()))
}

/// The declared table of a nested grant key, whose words refuse an unknown
/// key at that depth.
fn nested(key: &str) -> &'static Table {
    match declared(TableForm::Grant).holds(key) {
        Some(Holds::Table(table)) => table,
        _ => panic!("`grant` declares `{key}` as a nested table"),
    }
}

// ── Tagged values ─────────────────────────────────────────────────────────

/// A tagged value: its label and payload.
struct Tag<'a> {
    label: &'a str,
    payload: Option<Cow<'a, ValClos>>,
}

impl<'a> Tag<'a> {
    fn of(value: &'a ValClos) -> Option<Self> {
        match value.view() {
            View::Variant { label, payload } => Some(Self { label, payload }),
            _ => None,
        }
    }

    /// The string payload; `vocab` supplies contextual surface advice.
    fn text(&self, vocab: &Vocab) -> Result<&str, Error> {
        match self.payload.as_deref() {
            Some(value) => match value.view() {
                View::String(s) => Ok(s.as_str()),
                _ => Err(at(
                    shape(value, Expected::Shape(ValueShape::String)),
                    DataStep::Payload(self.label.into()),
                )
                .with_hint(vocab.payload)),
            },
            None => Err(
                invalid_error(InvalidCause::MissingPayload(self.label.into()))
                    .with_hint(vocab.payload),
            ),
        }
    }

    fn nullary(&self, vocab: &Vocab) -> Result<(), Error> {
        match self.payload.as_deref() {
            None => Ok(()),
            Some(value) => Err(
                invalid_error(InvalidCause::UnexpectedPayload(UnexpectedPayload {
                    tag: self.label.into(),
                    found: found(value),
                }))
                .with_hint(vocab.payload),
            ),
        }
    }
}

/// Prefix each refusal with the key it came from.
fn within(prefix: &str) -> impl Fn(Error) -> Error + '_ {
    move |e| e.with_context(prefix)
}

fn unknown_tag(vocab: &Vocab, tag: &str) -> Error {
    grant_error(vocab.refuse(tag))
}

fn untagged(vocab: &Vocab, value: &ValClos) -> Error {
    shape(
        value,
        Expected::Tags((vocab.row)().into_iter().map(|(tag, _)| tag).collect()),
    )
    .with_hint(vocab.untagged)
}

fn list_of(value: &ValClos) -> Result<ListRef<'_>, Error> {
    match value.view() {
        View::List(items) => Ok(items),
        _ => Err(shape(value, Expected::Shape(ValueShape::List))),
    }
}

fn table_of<'a>(value: &'a ValClos, empty: &str) -> Result<MapRef<'a>, Error> {
    match value.view() {
        View::Map(map) => Ok(map),
        _ => Err(shape(value, Expected::Shape(ValueShape::Map))
            .with_hint(format!("for a table that admits nothing write `{empty}`"))),
    }
}

// ── Places and selectors ──────────────────────────────────────────────────

/// A place tag and its subpath; `None` for a tag that is not a place.
fn place_of(tag: &Tag) -> Result<Option<Place>, Error> {
    let Some(anchor) = Anchor::of_tag(tag.label) else {
        return Ok(None);
    };
    Ok(Some(anchor.place(tag.text(&PLACE)?)))
}

fn selector_of(tag: &Tag) -> Result<Option<Selector>, Error> {
    let Ok(selector) = tag.label.parse() else {
        return Ok(None);
    };
    tag.nullary(&DIR_TARGET)?;
    Ok(Some(selector))
}

/// One entry of an fs list.
fn decode_place(value: &ValClos) -> Result<Place, Error> {
    let tag = Tag::of(value).ok_or_else(|| untagged(&PLACE, value))?;
    place_of(&tag)?.ok_or_else(|| unknown_tag(&PLACE, tag.label))
}

/// The places of an fs list, each walked as polarity `W` walks.
fn freeze_places<W: Polar>(
    list: ListRef<'_>,
    ctx: &FreezeCtx,
) -> Result<Vec<GrantPath<W::Grant>>, Error> {
    list.iter()
        .enumerate()
        .filter_map(|(index, item)| {
            decode_place(&item)
                .and_then(|place| {
                    ctx.walked::<W>(&place)
                        .and_then(|placed| placed.map(Placed::grant).transpose())
                        .map_err(|e| at(e, DataStep::Payload(place.anchor.tag().into_owned())))
                })
                .map_err(|error| at(error, DataStep::Index(index.into())))
                .transpose()
        })
        .collect()
}

// ── Dimension decoders ────────────────────────────────────────────────────

/// `fs: [read: [...], write: [...], deny: [...]]`, lists of places, where
/// `deny` carves holes inside the `read`/`write` prefix regions.
fn decode_fs(value: &ValClos, err_prefix: &str, ctx: &FreezeCtx) -> Result<FsLayer, Error> {
    let at = |error| within(err_prefix)(error);
    let (mut read, mut write, mut deny) = (Vec::new(), Vec::new(), Vec::new());
    for (sub, paths) in table_of(value, "fs: [read: []]").map_err(at)? {
        let list = |paths| list_of(paths).map_err(|e| at_field(e, sub)).map_err(at);
        match sub {
            "read" => {
                read = freeze_places::<Trace>(list(&paths)?, ctx)
                    .map_err(|e| at_field(e, "read"))
                    .map_err(at)?;
            }
            "write" => {
                write = freeze_places::<Trace>(list(&paths)?, ctx)
                    .map_err(|e| at_field(e, "write"))
                    .map_err(at)?;
            }
            "deny" => {
                deny = freeze_places::<Excluded>(list(&paths)?, ctx)
                    .map_err(|e| at_field(e, "deny"))
                    .map_err(at)?;
            }
            _ => {
                return Err(at(invalid_error(InvalidCause::UnknownField(
                    nested("fs").unknown_key(sub),
                ))));
            }
        }
    }
    Ok(FsLayer::new(read, write, deny))
}

/// A nullary tag of `V`: its label names a `V` and carries no payload.
fn nullary_tag<V: FromStr>(value: &ValClos, vocab: &Vocab) -> Result<V, Error> {
    let tag = Tag::of(value).ok_or_else(|| untagged(vocab, value))?;
    let parsed = tag
        .label
        .parse()
        .map_err(|_| unknown_tag(vocab, tag.label))?;
    tag.nullary(vocab)?;
    Ok(parsed)
}

/// `net` and `gpu` each take their one restriction; omit one to inherit.
fn decode_veto<V: FromStr>(value: &ValClos, vocab: &Vocab, err_prefix: &str) -> Result<V, Error> {
    nullary_tag(value, vocab).map_err(within(err_prefix))
}

/// `editor`/`shell`: a list of verbs, each a nullary tag of `V`.
fn decode_verbs<V: FromStr + Ord>(
    value: &ValClos,
    vocab: &Vocab,
    err_prefix: &str,
) -> Result<BTreeSet<V>, Error> {
    let at = |error| within(err_prefix)(error);
    list_of(value)
        .map_err(at)?
        .iter()
        .enumerate()
        .map(|(index, item)| nullary_tag(&item, vocab).map_err(|e| at_index(e, index)))
        .collect::<Result<_, _>>()
        .map_err(at)
}

// ── Exec targets ──────────────────────────────────────────────────────────

/// A name is a bare command name; a path is a place, which is judged by its
/// own rules.
fn bare_name(name: &str) -> Result<(), Error> {
    if !path_shaped(name) {
        return Ok(());
    }
    Err(grant_error(GrantFailure::BareNameRequired(name.into())))
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
fn file_rules<W: Keyed>(place: &Place, ctx: &FreezeCtx) -> Result<Vec<Rule>, Error> {
    let Some(placed) = ctx.walked::<W>(place)? else {
        return Ok(vec![]);
    };
    if let Placed::Walked(walked) = &placed
        && walked.real.followed().kind() == Some(Kind::Dir)
    {
        return Err(grant_error(GrantFailure::ProgramIsDirectory(Target::File(
            place.clone(),
        ))));
    }
    Ok(vec![W::file(placed.grant()?)?])
}

/// `dir D`: every directory `D`, a place or a selector, resolves to.
fn dir_rules<W: Keyed>(tag: &Tag, ctx: &FreezeCtx) -> Result<Vec<Rule>, Error> {
    let payload = tag.payload.as_deref().ok_or_else(|| {
        invalid_error(InvalidCause::MissingPayload("dir".into())).with_hint(DIR_TARGET.untagged)
    })?;
    let inner = Tag::of(payload).ok_or_else(|| untagged(&DIR_TARGET, payload))?;
    let dirs = if let Some(selector) = selector_of(&inner)? {
        ctx.selector::<W>(selector)?
    } else if let Some(place) = place_of(&inner)? {
        ctx.walked::<W>(&place)
            .and_then(|placed| placed.map(Placed::grant).transpose())
            .map_err(|e| at(e, DataStep::Payload(place.anchor.tag().into_owned())))?
            .into_iter()
            .collect()
    } else {
        return Err(unknown_tag(&DIR_TARGET, inner.label));
    };
    Ok(dirs.into_iter().map(W::dir).collect())
}

/// One entry of an `allow` or `deny` list, as the rules polarity `W` makes
/// of it.
fn decode_target<W: Keyed>(value: &ValClos, ctx: &FreezeCtx) -> Result<Vec<Rule>, Error> {
    let tag = Tag::of(value).ok_or_else(|| untagged(&EXEC_TARGET, value))?;
    match tag.label {
        "name" => {
            let name = tag.text(&EXEC_TARGET)?;
            bare_name(name).map_err(|e| at(e, DataStep::Payload("name".into())))?;
            let path = ctx.var("PATH");
            name_rules::<W>(name, path.as_deref(), cfg!(unix))
                .map_err(|e| at(e, DataStep::Payload("name".into())))
        }
        "dir" => dir_rules::<W>(&tag, ctx).map_err(|e| {
            if tag.payload.is_some() {
                at(e, DataStep::Payload("dir".into()))
            } else {
                e
            }
        }),
        _ => match place_of(&tag)? {
            Some(place) => {
                file_rules::<W>(&place, ctx).map_err(|e| at(e, DataStep::Payload(tag.label.into())))
            }
            None => Err(unknown_tag(&EXEC_TARGET, tag.label)),
        },
    }
}

/// An `allow` or `deny` list, the rules of each target at polarity `W`.
fn decode_targets<W: Keyed>(list: ListRef<'_>, ctx: &FreezeCtx) -> Result<Vec<Rule>, Error> {
    let rules: Vec<_> = list
        .iter()
        .enumerate()
        .map(|(index, item)| {
            decode_target::<W>(&item, ctx).map_err(|error| at(error, DataStep::Index(index.into())))
        })
        .collect::<Result<_, _>>()?;
    Ok(rules.into_iter().flatten().collect())
}

/// `exec: [allow: [...], deny: [...]]`.  Argument restrictions are gone: a
/// target is admitted or denied whatever its arguments.
fn decode_exec(value: &ValClos, err_prefix: &str, ctx: &FreezeCtx) -> Result<ExecRules, Error> {
    let at = |error| within(err_prefix)(error);
    let mut rules = Vec::new();
    for (key, targets) in table_of(value, "exec: [allow: []]").map_err(at)? {
        let decode = match key {
            "allow" => decode_targets::<Trace>,
            "deny" => decode_targets::<Excluded>,
            other => return Err(at(unknown_exec_key(other, &targets))),
        };
        let list = list_of(&targets)
            .map_err(|e| at_field(e, key))
            .map_err(at)?;
        rules.extend(
            decode(list, ctx)
                .map_err(|e| at_field(e, key))
                .map_err(at)?,
        );
    }
    Ok(rules.into_iter().collect())
}

/// Whether `value` has the shape of the old surface's per-command policy:
/// `'allow'`, `'deny'`, or a list of subcommands.
fn is_old_policy(value: &ValClos) -> bool {
    match value.view() {
        View::String(_) => true,
        View::List(items) => items.iter().all(|i| matches!(i.view(), View::String(_))),
        _ => false,
    }
}

fn unknown_exec_key(key: &str, value: &ValClos) -> Error {
    let err = invalid_error(InvalidCause::UnknownField(nested("exec").unknown_key(key)));
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
/// The single `ValClos → GrantLayer` constructor for a record; every spelling
/// is walked against `ctx` right here, so "a `GrantLayer` has every spelling
/// already resolved" holds by construction whoever comes through.  Unknown
/// keys error here and in each dimension's decoder rather than being
/// silently dropped; the keyset is `grant`'s own declared table, so this
/// decoder and the checker cannot drift apart.
///
/// # Errors
/// A [`Error`] naming the key whose shape or value it will not read, or
/// the place that would not resolve absolute or whose walk failed.
pub(crate) fn decode_capability_map(
    value: &ValClos,
    err_prefix: &str,
    ctx: &FreezeCtx,
) -> Result<GrantLayer, Error> {
    let entries = match value.view() {
        View::Map(entries) => Some(entries),
        View::Unit => None,
        _ => {
            return Err(shape(
                value,
                Expected::AnyOf(vec![ValueShape::Map, ValueShape::Unit]),
            )
            .with_context(err_prefix));
        }
    };
    let (mut exec, mut fs, mut net, mut gpu, mut editor, mut shell) =
        (None, None, None, None, None, None);
    for (k, v) in entries.into_iter().flatten() {
        let context = format!("{err_prefix} {k}");
        let decoded = (|| -> Result<(), Error> {
            match k {
                "exec" => exec = Some(decode_exec(&v, &context, ctx)?),
                "fs" => fs = Some(decode_fs(&v, &context, ctx)?),
                "net" => net = Some(decode_veto::<Net>(&v, &NET, &context)?),
                "gpu" => gpu = Some(decode_veto::<Gpu>(&v, &GPU, &context)?),
                "editor" => editor = Some(decode_verbs::<EditorVerb>(&v, &EDITOR, &context)?),
                "shell" => shell = Some(decode_verbs::<ShellVerb>(&v, &SHELL, &context)?),
                other => {
                    let table = declared(TableForm::Grant);
                    return Err(match table.holds(other) {
                        Some(Holds::Refused(cause)) => {
                            grant_error(GrantFailure::RefusedKey(GrantFailureRefusedKey {
                                key: other.into(),
                                cause: *cause,
                            }))
                        }
                        _ => invalid_error(InvalidCause::UnknownField(table.unknown_key(other))),
                    }
                    .with_context(err_prefix));
                }
            }
            Ok(())
        })();
        decoded.map_err(|error| {
            if matches!(k, "exec" | "fs" | "net" | "gpu" | "editor" | "shell") {
                at_field(error, k)
            } else {
                error
            }
        })?;
    }
    Ok(GrantLayer {
        exec,
        fs,
        net,
        gpu,
        editor,
        shell,
    })
}

/// `record`, a grant value that crossed as data, resolved against `ctx`:
/// what the host that enforces it makes of it.
///
/// # Errors
/// As [`decode_capability_map`].
pub fn decode_grant(
    record: &FOValue,
    err_prefix: &str,
    ctx: &FreezeCtx,
) -> Result<GrantLayer, Error> {
    decode_capability_map(&ValClos::from(record.clone()), err_prefix, ctx)
}

/// As [`decode_grant`], read as an extension: an axis it omits states
/// nothing.
///
/// # Errors
/// As [`decode_capability_map`].
pub fn decode_fragment(
    record: &FOValue,
    err_prefix: &str,
    ctx: &FreezeCtx,
) -> Result<Fragment, Error> {
    decode_grant(record, err_prefix, ctx).map(Fragment::of)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::authority::policy::{Baseline, ExecDenial, Program, Withheld};
    use crate::authority::policy::{ExecAllow, ExecDeny, FsAllow, FsOp, Veto};
    use crate::environ::EnvVars;
    use crate::path::utf8;
    #[cfg(unix)]
    use crate::test_helper::{located, stack, trace};

    fn bare(label: &str) -> ValClos {
        ValClos::variant(label, None)
    }

    fn tagged(label: &str, payload: &str) -> ValClos {
        ValClos::variant(label, Some(ValClos::string(payload)))
    }

    fn dir(inner: ValClos) -> ValClos {
        ValClos::variant("dir", Some(inner))
    }

    fn rec(entries: &[(&str, ValClos)]) -> ValClos {
        ValClos::map(
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    fn list(items: Vec<ValClos>) -> ValClos {
        ValClos::list(items)
    }

    fn exec(allow: Vec<ValClos>, deny: Vec<ValClos>) -> ValClos {
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
            EnvVars::from_pairs([("HOME", home)]).unwrap(),
            crate::path::CwdPath::new(cwd.into()).expect("a fixture cwd is absolute"),
        )
    }

    fn decode(value: &ValClos) -> Result<GrantLayer, Error> {
        decode_capability_map(value, "test", &ctx())
    }

    fn rules_of(exec_value: ValClos) -> Result<ExecRules, Error> {
        decode(&rec(&[("exec", exec_value)])).map(|layer| layer.exec().cloned().unwrap())
    }

    /// Read as a fragment, a grant value leaves what it omits to the base:
    /// over the top, an exclusion is every program less it, the fs silent.
    #[test]
    fn a_fragment_over_the_top_is_every_program_less_its_exclusions() {
        let veto = exec(vec![], vec![tagged("name", "ral-no-such-tool")]);
        let record = FOValue::try_from(&rec(&[("exec", veto)])).unwrap();
        let any = |rules: &ExecRules| rules.allows().any(|a| *a == ExecAllow::Any);
        let policy = decode_grant(&record, "test", &ctx()).unwrap();
        assert!(!any(policy.exec().unwrap()), "{policy:?}");
        let fragment = decode_fragment(&record, "test", &ctx()).unwrap();
        let carved = GrantLayer::root().extend(fragment);
        let rules = carved.exec().expect("an exec axis");
        assert!(any(rules) && vetoed(rules, "ral-no-such-tool"), "{rules:?}");
        assert!(carved.fs().is_none());
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
        rules.denies().any(|d| *d == ExecDeny::Name(Veto::of(name)))
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
            .table(FsOp::Read)
            .allows()
            .map(|a| match a {
                FsAllow::Prefix(prefix) => prefix.written.as_str(),
                FsAllow::RunPlace(_) => panic!("a decoded fs list holds no run place"),
            })
            .collect()
    }

    fn refusal(value: &ValClos) -> Error {
        decode(value).unwrap_err()
    }

    fn assert_invalid(error: &Error, path: Vec<DataStep>, cause: InvalidCause) {
        assert_eq!(
            error.reason,
            Reason::Invalid(Box::new(crate::fact::InvalidFailure {
                operation: "grant".into(),
                path,
                cause,
            }))
        );
    }

    fn assert_shape(error: &Error, path: Vec<DataStep>, expected: Expected, found: Found) {
        let description = format!("expected {}, got {}", expected.describe(), found.describe());
        assert!(error.describe().ends_with(&description), "{error}");
        assert_invalid(
            error,
            path,
            InvalidCause::Shape(ShapeFailure { expected, found }),
        );
    }

    fn tags(vocab: &Vocab) -> Expected {
        Expected::Tags((vocab.row)().into_iter().map(|(tag, _)| tag).collect())
    }

    fn exec_refusal(exec_value: ValClos) -> Error {
        refusal(&rec(&[("exec", exec_value)]))
    }

    fn fs_refusal(read: Vec<ValClos>) -> Error {
        refusal(&rec(&[("fs", rec(&[("read", list(read))]))]))
    }

    /// The key path says where the decoder stood: report context, never the
    /// record a handler reads.
    #[test]
    fn the_key_path_is_context_and_not_the_reason() {
        let e = exec_refusal(rec(&[("alow", list(vec![]))]));
        assert!(e.to_string().starts_with("test exec: "), "{e}");
        assert!(!e.describe().contains("test exec"), "{}", e.describe());
        assert_eq!(e.context(), Some("test exec"));
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
                matches!(&deny[..], [Rule::Deny(d)] if **d == ExecDeny::Name(Veto::of(name))),
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
                matches!(&veto[..], [Rule::Deny(d)] if **d == ExecDeny::Name(Veto::of("zz-lost"))),
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
            FreezeCtx::at(EnvVars::from_pairs(env).unwrap(), cwd)
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
        let baseline = Baseline::default();
        let withheld = |at: &std::path::Path| {
            grants
                .admit(
                    &baseline,
                    Program::File {
                        located: Box::new(located(at)),
                        arg0: at.to_path_buf(),
                    },
                )
                .map(drop)
        };
        assert!(withheld(&first.join("git")).is_ok());
        assert!(
            matches!(
                withheld(&later.join("git")),
                Err(ExecDenial::Run(Withheld::MissingAllowance))
            ),
            "a later `PATH` finds another git, which the grant never named"
        );
        assert!(
            withheld(&later.join("zz-lost")).is_err_and(|denied| matches!(
                denied,
                ExecDenial::Run(Withheld::Excluded { scope, .. })
                    if *scope == ExecDeny::Name(Veto::of("zz-lost"))
            ))
        );
    }

    /// `name` and `` `dir `path `` read one environment: an overlay `PATH` is
    /// the `PATH` of both, and the process `PATH` of neither.
    #[cfg(unix)]
    #[test]
    fn a_name_and_the_path_selector_read_the_same_path() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, root) = tree();
        let bin = root.join("opt-tools");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("tool"), "").unwrap();
        std::fs::set_permissions(bin.join("tool"), PermissionsExt::from_mode(0o755)).unwrap();
        let ctx = anchored_at(&root, &root, &[("PATH", &bin)]);
        let layer = decode_capability_map(
            &rec(&[(
                "exec",
                exec(vec![tagged("name", "tool"), dir(bare("path"))], vec![]),
            )]),
            "test",
            &ctx,
        )
        .unwrap();
        let mut allowed = written(layer.exec().unwrap());
        allowed.sort();
        let bin = utf8(&bin).unwrap();
        let mut expected = [format!("dir {bin}"), format!("{bin}/tool")];
        expected.sort();
        assert_eq!(allowed, expected);
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

    /// A target in both lists is an allowance and a veto, which refuses.
    #[test]
    fn a_target_in_both_lists_is_both() {
        let g = rules_of(exec(vec![tagged("name", "ls")], vec![tagged("name", "ls")])).unwrap();
        assert_eq!(allowed_names(&g), BTreeSet::from(["ls"]));
        assert!(vetoed(&g, "ls"), "{g:?}");
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
            assert!(e.to_string().contains("bare command name"), "{name}: {e}");
            assert!(e.reason.advice()[0].contains("`` `absolute /path ``"));
        }
    }

    #[test]
    fn an_unwrapped_directory_place_is_refused_with_the_dir_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        let e = exec_refusal(exec(vec![tagged("absolute", &path)], vec![]));
        assert!(e.to_string().contains("is a directory"), "{e}");
        assert!(e.reason.advice()[0].contains("`dir `absolute"), "dir hint");
    }

    #[test]
    fn dir_takes_a_place_or_a_selector_only() {
        for payload in [
            tagged("name", "git"),
            dir(bare("path")),
            ValClos::string("/x"),
        ] {
            let e = exec_refusal(exec(vec![dir(payload.clone())], vec![]));
            let path = vec![
                DataStep::Field("exec".into()),
                DataStep::Field("allow".into()),
                DataStep::Index(0usize.into()),
                DataStep::Payload("dir".into()),
            ];
            if let Some(tag) = Tag::of(&payload) {
                assert_invalid(&e, path, InvalidCause::Grant(DIR_TARGET.refuse(tag.label)));
                assert!(e.describe().contains("place or a selector"), "{e}");
            } else {
                assert_shape(&e, path, tags(&DIR_TARGET), found(&payload));
                assert_eq!(e.hints(), [Box::<str>::from(DIR_TARGET.untagged)]);
                assert!(e.hints()[0].contains("place or a selector"));
            }
        }
        let e = exec_refusal(exec(vec![bare("dir")], vec![]));
        assert_invalid(
            &e,
            vec![
                DataStep::Field("exec".into()),
                DataStep::Field("allow".into()),
                DataStep::Index(0usize.into()),
            ],
            InvalidCause::MissingPayload("dir".into()),
        );
        assert!(e.describe().contains("tag \"dir\" needs a value"), "{e}");
        assert_eq!(e.hints(), [Box::<str>::from(DIR_TARGET.untagged)]);
        assert!(e.hints()[0].contains("place or a selector"));
    }

    /// Per-command maps, string policies and subcommand lists are gone, and
    /// are never read as an unrestricted allow.
    #[test]
    fn obsolete_exec_policies_are_refused() {
        for value in [
            ValClos::string("allow"),
            list(vec![ValClos::string("git")]),
            rec(&[("git", ValClos::string("allow"))]),
            rec(&[("cargo", list(vec![ValClos::string("build")]))]),
        ] {
            let e = exec_refusal(value.clone());
            if let View::Map(entries) = value.view() {
                let (key, _) = entries.iter().next().unwrap();
                assert_invalid(
                    &e,
                    vec![DataStep::Field("exec".into())],
                    InvalidCause::UnknownField(nested("exec").unknown_key(key)),
                );
                assert!(e.hints()[0].contains("no longer supported"));
            } else {
                assert_shape(
                    &e,
                    vec![DataStep::Field("exec".into())],
                    Expected::Shape(ValueShape::Map),
                    found(&value),
                );
                assert_eq!(
                    e.hints(),
                    [Box::<str>::from(
                        "for a table that admits nothing write `exec: [allow: []]`"
                    )]
                );
            }
            assert_eq!(e.context(), Some("test exec"));
        }
    }

    /// The per-command migration hint is for the old string surface's shapes;
    /// any other value under an unknown key is a plain unknown key.
    #[test]
    fn the_per_command_hint_is_only_for_the_old_policy_shapes() {
        for value in [
            ValClos::string("allow"),
            list(vec![ValClos::string("build")]),
        ] {
            let hint = exec_refusal(rec(&[("git", value)])).hints()[0].to_string();
            assert!(hint.contains("`` allow: [`name git] ``"), "{hint}");
        }
        for value in [list(vec![bare("git")]), ValClos::bool(true), rec(&[])] {
            let e = exec_refusal(rec(&[("only", value)]));
            assert_invalid(
                &e,
                vec![DataStep::Field("exec".into())],
                InvalidCause::UnknownField(nested("exec").unknown_key("only")),
            );
            assert!(e.describe().contains("unknown field \"only\""), "{e}");
            assert!(e.hints().is_empty(), "{:?}", e.hints());
        }
    }

    /// An unknown key is refused before its value is read.
    #[test]
    fn an_unknown_fs_key_is_refused_before_its_places() {
        let e = refusal(&rec(&[(
            "fs",
            rec(&[("raed", list(vec![tagged("cwdd", ".")]))]),
        )]));
        assert_invalid(
            &e,
            vec![DataStep::Field("fs".into())],
            InvalidCause::UnknownField(nested("fs").unknown_key("raed")),
        );
        assert!(e.describe().contains("unknown field \"raed\""), "{e}");
    }

    #[test]
    fn a_nested_table_is_a_record() {
        // an empty map is a record at run time: it restricts to the baseline
        for (value, accepted) in [
            (ValClos::UNIT, false),
            (rec(&[]), true),
            (ValClos::string("x"), false),
        ] {
            assert_eq!(decode(&rec(&[("fs", value)])).is_ok(), accepted);
        }
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
            let e = fs_refusal(vec![item.clone()]);
            let tag = Tag::of(&item).unwrap();
            assert_invalid(
                &e,
                vec![
                    DataStep::Field("fs".into()),
                    DataStep::Field("read".into()),
                    DataStep::Index(0usize.into()),
                ],
                InvalidCause::Grant(PLACE.refuse(tag.label)),
            );
            assert!(e.to_string().contains("is not a place"), "{e}");
            assert!(e.reason.advice()[0].contains("fs lists take places"));
        }
        let e = fs_refusal(vec![ValClos::string("/tmp")]);
        assert_shape(
            &e,
            vec![
                DataStep::Field("fs".into()),
                DataStep::Field("read".into()),
                DataStep::Index(0usize.into()),
            ],
            tags(&PLACE),
            Found::Text(TextPreview::new("/tmp")),
        );
        assert!(e.hints()[0].contains("`absolute /tmp"));
        let m = fs_refusal(vec![tagged("cwdd", "x")]).to_string();
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
        let e = fs_refusal(vec![bare("cwd")]);
        assert_invalid(
            &e,
            vec![
                DataStep::Field("fs".into()),
                DataStep::Field("read".into()),
                DataStep::Index(0usize.into()),
            ],
            InvalidCause::MissingPayload("cwd".into()),
        );
        assert!(e.describe().contains("tag \"cwd\" needs a value"), "{e}");
        assert_eq!(e.hints(), [Box::<str>::from(PLACE.payload)]);
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
        let env = EnvVars::from_pairs(
            std::iter::once(("HOME", home))
                .chain(extra.iter().copied())
                .map(|(k, v)| (k, utf8(v).unwrap())),
        )
        .unwrap();
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
        let baseline = Baseline::default();
        for held in [secret.clone(), secret.join("f")] {
            assert!(
                matches!(
                    grants.read(&baseline, &trace(&held)),
                    Err(Withheld::Excluded { .. })
                ),
                "{held:?}"
            );
        }
        assert!(grants.read(&baseline, &trace(open.join("f"))).is_ok());
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
            assert!(why.to_string().contains(utf8(&looped).unwrap()), "{why}");
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
            assert!(why.to_string().contains("filesystem root"), "{why}");
        }
    }

    /// The root as an allowance is everywhere, on every host: an fs prefix,
    /// an exec `dir`; as an exec file it is a directory, not a program.
    #[test]
    fn an_authored_root_allowance_is_everywhere() {
        let root = tagged("absolute", "/");
        let layer = decode(&rec(&[("fs", rec(&[("read", list(vec![root.clone()]))]))])).unwrap();
        let everywhere = FsAllow::Prefix(GrantPath::everywhere());
        let fs = layer.fs().expect("an fs layer").table(FsOp::Read);
        assert!(fs.allows().any(|a| *a == everywhere), "{fs:?}");
        let rules = rules_of(exec(vec![dir(root.clone())], vec![])).unwrap();
        let everywhere = ExecAllow::Dir(GrantPath::everywhere());
        assert!(rules.allows().any(|a| *a == everywhere), "{rules:?}");
        let why = exec_refusal(exec(vec![root], vec![]));
        assert!(why.to_string().contains("a directory"), "{why}");
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
        assert!(why.to_string().contains("filesystem root"), "{why}");
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
        assert!(why.to_string().contains("outside HOME"), "{why}");

        let grants = stack([read(&stays).expect("a base linked within HOME")]);
        std::fs::remove_file(&stays).unwrap();
        std::os::unix::fs::symlink(&outside, &stays).unwrap();
        let baseline = Baseline::default();
        assert!(grants.read(&baseline, &trace(inside.join("f"))).is_ok());
        assert!(grants.read(&baseline, &trace(outside.join("f"))).is_err());
    }

    // ── net, gpu, editor, shell ──

    #[test]
    fn net_offline_decodes_and_nothing_else_does() {
        let layer = decode(&rec(&[("net", bare("offline"))])).unwrap();
        assert_eq!(layer.axes().net, Some(Net::Offline));
        for value in [ValClos::bool(true), ValClos::bool(false)] {
            let e = refusal(&rec(&[("net", value.clone())]));
            assert_shape(
                &e,
                vec![DataStep::Field("net".into())],
                tags(&NET),
                found(&value),
            );
            assert_eq!(e.context(), Some("test net"));
            assert!(
                e.hints()[0].contains("omit `net` to inherit, or write `` net: `offline ``"),
                "hint"
            );
        }
        let m = refusal(&rec(&[("net", bare("online"))])).to_string();
        assert!(m.contains("`` `online `` is not a net mode"), "{m}");
        let m = refusal(&rec(&[(
            "net",
            ValClos::variant("offline", Some(ValClos::bool(true))),
        )]))
        .to_string();
        assert!(m.contains("takes no value"), "{m}");
    }

    #[test]
    fn verb_lists_decode_as_sets() {
        let axes = decode(&rec(&[
            (
                "editor",
                list(vec![bare("read"), bare("tui"), bare("read")]),
            ),
            ("shell", list(vec![])),
        ]))
        .unwrap()
        .axes();
        assert_eq!(
            axes.editor,
            Some([EditorVerb::Read, EditorVerb::Tui].into())
        );
        assert_eq!(axes.shell, Some(BTreeSet::new()));
    }

    #[test]
    fn the_boolean_record_surface_is_refused() {
        for key in ["editor", "shell"] {
            let e = refusal(&rec(&[(key, rec(&[("read", ValClos::bool(true))]))]));
            assert_shape(
                &e,
                vec![DataStep::Field(key.into())],
                Expected::Shape(ValueShape::List),
                Found::Map,
            );
            assert_eq!(e.context(), Some(format!("test {key}").as_str()));
        }
        let e = refusal(&rec(&[("shell", list(vec![ValClos::string("chdir")]))]));
        assert_shape(
            &e,
            vec![
                DataStep::Field("shell".into()),
                DataStep::Index(0usize.into()),
            ],
            tags(&SHELL),
            Found::Text(TextPreview::new("chdir")),
        );
        assert!(e.hints()[0].contains("`shell` takes the list of verbs"));
        let m = refusal(&rec(&[(
            "editor",
            list(vec![ValClos::variant("read", Some(ValClos::bool(true)))]),
        )]))
        .to_string();
        assert!(m.contains("takes no value"), "{m}");
    }

    #[test]
    fn detach_is_a_shell_verb_not_a_key() {
        let e = refusal(&rec(&[("detach", ValClos::bool(true))]));
        assert_invalid(
            &e,
            vec![],
            InvalidCause::Grant(GrantFailure::RefusedKey(GrantFailureRefusedKey {
                key: "detach".into(),
                cause: crate::fact::RefusedGrantKey::DetachMovedToShell,
            })),
        );
        assert_eq!(e.context(), Some("test"));
        assert!(e.reason.advice()[0].contains("`detach` is a shell verb"));
        assert!(e.reason.advice()[0].contains("`` shell: [`chdir, `detach] ``"));
        assert_eq!(e.hints(), []);
        assert!(e.plain().contains("`` shell: [`chdir, `detach] ``"));
    }

    // ── drift ──

    /// A valid value for each key the grant table declares.
    fn representative(key: &str) -> ValClos {
        match key {
            "exec" => exec(vec![tagged("name", "git")], vec![dir(bare("system"))]),
            "fs" => rec(&[("read", list(vec![tagged("cwd", ".")]))]),
            "net" => bare("offline"),
            "gpu" => bare("withheld"),
            "editor" => list(vec![bare("read")]),
            "shell" => list(vec![bare("chdir")]),
            other => panic!("declared key '{other}' has no representative value here"),
        }
    }

    /// A valid list entry for each key a nested table declares.
    fn entry(table: &str) -> ValClos {
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
        let table = declared(TableForm::Grant);
        for key in table.keys {
            match &key.holds {
                Holds::Refused(cause) => {
                    let e = refusal(&rec(&[(key.label, ValClos::UNIT)]));
                    assert_invalid(
                        &e,
                        vec![],
                        InvalidCause::Grant(GrantFailure::RefusedKey(GrantFailureRefusedKey {
                            key: key.label.into(),
                            cause: *cause,
                        })),
                    );
                    assert_eq!(e.reason.advice(), [cause.advice()]);
                    assert_eq!(e.context(), Some("test"));
                    assert_eq!(e.hints(), []);
                }
                Holds::Table(sub) => {
                    for inner in sub.keys {
                        let value = rec(&[(inner.label, list(vec![entry(key.label)]))]);
                        decode(&rec(&[(key.label, value)]))
                            .unwrap_or_else(|e| panic!("'{} {}': {e}", key.label, inner.label));
                    }
                }
                _ => {
                    let value = rec(&[(key.label, representative(key.label))]);
                    decode(&value).unwrap_or_else(|e| panic!("'{}': {e}", key.label));
                }
            }
        }
    }

    /// A key the table does not declare is refused in that table's words, at
    /// each depth.
    #[test]
    fn an_unknown_key_is_refused_in_its_tables_words_at_each_depth() {
        let table = declared(TableForm::Grant);
        let e = refusal(&rec(&[("bogus", ValClos::UNIT)]));
        assert_invalid(
            &e,
            vec![],
            InvalidCause::UnknownField(table.unknown_key("bogus")),
        );
        assert_eq!(e.context(), Some("test"));
        for key in table.keys {
            if let Holds::Table(sub) = &key.holds {
                let e = refusal(&rec(&[(key.label, rec(&[("bogus", list(vec![]))]))]));
                assert_invalid(
                    &e,
                    vec![DataStep::Field(key.label.into())],
                    InvalidCause::UnknownField(sub.unknown_key("bogus")),
                );
                assert_eq!(e.context(), Some(format!("test {}", key.label).as_str()));
            }
        }
    }
    #[test]
    fn grant_decode_dir_subpath_failure_keeps_both_payload_steps() {
        let error = exec_refusal(exec(vec![dir(tagged("cwd", "/x"))], vec![]));
        assert_invalid(
            &error,
            vec![
                DataStep::Field("exec".into()),
                DataStep::Field("allow".into()),
                DataStep::Index(0usize.into()),
                DataStep::Payload("dir".into()),
                DataStep::Payload("cwd".into()),
            ],
            InvalidCause::Grant(GrantFailure::Subpath(crate::fact::GrantFailureSubpath {
                anchor: Anchor::Cwd,
                raw: "/x".into(),
                cause: crate::fact::RelativeSubpathRefusal::Rooted,
            })),
        );
        assert_eq!(error.context(), Some("test exec"));
        assert!(error.describe().contains("takes a relative path"));
        assert!(error.describe().contains("`absolute /x"));
        assert_eq!(error.hints(), []);
    }

    #[test]
    fn grant_decode_prefixes_only_present_payload_values_once() {
        for (value, cause, payload_path) in [
            (
                ValClos::variant("cwd", Some(ValClos::bool(true))),
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::String),
                    found: Found::Bool(true),
                }),
                vec![DataStep::Payload("cwd".into())],
            ),
            (
                bare("cwd"),
                InvalidCause::MissingPayload("cwd".into()),
                vec![],
            ),
            (
                bare("unknown"),
                InvalidCause::Grant(PLACE.refuse("unknown")),
                vec![],
            ),
        ] {
            let error = fs_refusal(vec![value]);
            let mut path = vec![
                DataStep::Field("fs".into()),
                DataStep::Field("read".into()),
                DataStep::Index(0usize.into()),
            ];
            path.extend(payload_path);
            assert_invalid(&error, path, cause);
        }
        for (inner, cause, payload_path) in [
            (
                ValClos::variant("cwd", Some(ValClos::bool(true))),
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::String),
                    found: Found::Bool(true),
                }),
                vec![DataStep::Payload("cwd".into())],
            ),
            (
                bare("cwd"),
                InvalidCause::MissingPayload("cwd".into()),
                vec![],
            ),
            (
                bare("unknown"),
                InvalidCause::Grant(DIR_TARGET.refuse("unknown")),
                vec![],
            ),
            (
                ValClos::variant("system", Some(ValClos::bool(true))),
                InvalidCause::UnexpectedPayload(UnexpectedPayload {
                    tag: "system".into(),
                    found: Found::Bool(true),
                }),
                vec![],
            ),
        ] {
            let error = exec_refusal(exec(vec![dir(inner)], vec![]));
            let mut path = vec![
                DataStep::Field("exec".into()),
                DataStep::Field("allow".into()),
                DataStep::Index(0usize.into()),
                DataStep::Payload("dir".into()),
            ];
            path.extend(payload_path);
            assert_invalid(&error, path, cause);
        }
        let context = FreezeCtx::at(
            EnvVars::from_pairs([
                ("PATH", ""),
                ("HOME", if cfg!(windows) { r"C:\h" } else { "/h" }),
            ])
            .unwrap(),
            crate::path::CwdPath::new(if cfg!(windows) {
                r"C:\work".into()
            } else {
                "/work".into()
            })
            .unwrap(),
        );
        let error = decode_capability_map(
            &rec(&[("exec", exec(vec![dir(bare("path"))], vec![]))]),
            "test",
            &context,
        )
        .unwrap_err();
        assert_invalid(
            &error,
            vec![
                DataStep::Field("exec".into()),
                DataStep::Field("allow".into()),
                DataStep::Index(0usize.into()),
                DataStep::Payload("dir".into()),
            ],
            InvalidCause::Grant(GrantFailure::EmptyPathSelector),
        );
    }
}
