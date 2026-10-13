//! Decode a capability `Value` — the argument of `grant [...]`, or a
//! `--capabilities` profile — into a frozen [`Capabilities`].
//!
//! A dimension the record does not name stays `None` and inherits the
//! surrounding frame, so an attenuation touches only what it lists.  The
//! author is the live user, so every malformed shape is a strict error.
//! The one exception is a path rooted under a foreign platform's
//! convention: it can never match an access here, so it is dropped as a
//! dead grant rather than failing the whole profile — the treatment
//! `exarch::policy::base::drop_dead_exec_grants` gives a bundled tool
//! Windows cannot back.
//!
//! The runtime cannot tell a record from a map, so both are read, at every
//! depth.  Every list denotes a set: a repeat adds nothing and is no error.

use super::freeze::{Anchor, FreezeCtx, Place, PolicyError, Selector, has_drive};
use crate::capability::{
    Capabilities, EditorVerb, Evidence, ExecGrant, ExecKey, FsPolicy, Net, ShellVerb,
};
use crate::syntax::quote::quote_word;
use crate::text::{indefinite, near_names};
use crate::typecheck::contract::{
    DIR_TARGET, EDITOR, EXEC_TARGET, Form as ContractForm, Holds, NET, PLACE, SHELL, Table, Vocab,
    declared,
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
        let label = self.label;
        match self.payload {
            Some(Value::String(s)) => Ok(s.as_str()),
            Some(other) => Err(PolicyError::new(format!(
                "`{label} takes {what}, got {}",
                other.type_name()
            ))
            .with_hint(vocab.payload)),
            None => Err(PolicyError::new(format!(
                "`{label} needs a payload: {what}, as in `{label} …`"
            ))
            .with_hint(vocab.payload)),
        }
    }

    /// A nullary tag takes no payload, not even `()`.
    fn nullary(&self, vocab: &Vocab) -> Result<(), PolicyError> {
        match self.payload {
            None => Ok(()),
            Some(_) => Err(PolicyError::new(format!(
                "`{0} takes no payload; write it bare, `{0}",
                self.label
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

fn freeze_places(
    list: &List,
    ctx: &FreezeCtx,
) -> Result<BTreeSet<crate::path::FrozenPath>, PolicyError> {
    list.iter()
        .filter_map(|item| {
            decode_place(&item)
                .and_then(|place| ctx.place(&place))
                .transpose()
        })
        .collect()
}

// ── Dimension decoders ────────────────────────────────────────────────────

/// `fs: [read: [...], write: [...], deny: [...]]`, lists of places, where
/// `deny` carves holes inside the `read`/`write` prefix regions.
fn decode_fs(value: &Value, err_prefix: &str, ctx: &FreezeCtx) -> Result<FsPolicy, PolicyError> {
    let at = within(err_prefix);
    let mut fp = FsPolicy::default();
    for (sub, paths) in table_of(value, "fs: [read: []]").map_err(&at)? {
        let list = list_of(&paths, "places").map_err(&at)?;
        let frozen = freeze_places(&list, ctx)
            .map_err(&at)?
            .into_iter()
            .collect();
        match sub {
            "read" => fp.read_prefixes = frozen,
            "write" => fp.write_prefixes = frozen,
            "deny" => fp.deny_paths = frozen,
            _ => {
                return Err(at(PolicyError::new(nested("fs").unknown_key(sub))));
            }
        }
    }
    Ok(fp)
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
    let path_shaped = name.is_empty()
        || has_drive(name)
        || name.contains(['/', '\\'])
        || matches!(name, "." | "..")
        || name.starts_with('~')
        || name.starts_with("xdg:");
    if !path_shaped {
        return Ok(());
    }
    Err(PolicyError::new(format!(
        "`name takes a bare command name, not {}",
        quote_word(name)
    ))
    .with_hint(
        "for a path write `absolute /path` or `cwd relative/path`; \
                 for a directory of programs, `dir `absolute /path`",
    ))
}

/// An unwrapped place names one executable; a directory there is a mistake
/// the author can fix by wrapping it in `dir`.
fn file_key(place: &Place, ctx: &FreezeCtx) -> Result<Vec<ExecKey>, PolicyError> {
    let Some(frozen) = ctx.place(place)? else {
        return Ok(vec![]);
    };
    if crate::path::is_dir(frozen.as_str()) {
        let spelled = format!("{} {}", place.tag(), quote_word(place.payload()));
        return Err(PolicyError::new(format!(
            "`{spelled}` is a directory, so as an exec target it names a binary that \
             cannot exist"
        ))
        .with_hint(format!("did you mean `dir `{spelled}`?")));
    }
    Ok(vec![ExecKey::Path(frozen)])
}

/// `dir D`: every directory `D`, a place or a selector, resolves to.
fn dir_keys(tag: &Tag, ctx: &FreezeCtx) -> Result<Vec<ExecKey>, PolicyError> {
    let payload = tag.payload.ok_or_else(|| {
        PolicyError::new("`dir needs a place or a selector").with_hint(DIR_TARGET.untagged)
    })?;
    let inner = Tag::of(payload).ok_or_else(|| untagged(&DIR_TARGET, payload))?;
    let dirs = if let Some(selector) = selector_of(&inner)? {
        ctx.selector(selector)?
    } else if let Some(place) = place_of(&inner)? {
        ctx.place(&place)?.into_iter().collect()
    } else {
        return Err(unknown_tag(&DIR_TARGET, inner.label));
    };
    Ok(dirs.into_iter().map(ExecKey::Dir).collect())
}

/// One entry of an `allow` or `deny` list.
fn decode_target(value: &Value, ctx: &FreezeCtx) -> Result<Vec<ExecKey>, PolicyError> {
    let tag = Tag::of(value).ok_or_else(|| untagged(&EXEC_TARGET, value))?;
    match tag.label {
        "name" => {
            let name = tag.text(&EXEC_TARGET, "a command name")?;
            bare_name(name)?;
            Ok(vec![ExecKey::Name(name.into())])
        }
        "dir" => dir_keys(&tag, ctx),
        _ => match place_of(&tag)? {
            Some(place) => file_key(&place, ctx),
            None => Err(unknown_tag(&EXEC_TARGET, tag.label)),
        },
    }
}

/// `exec: [allow: [...], deny: [...]]`.  Argument restrictions are gone: a
/// target is admitted or denied whatever its arguments.
fn decode_exec(value: &Value, err_prefix: &str, ctx: &FreezeCtx) -> Result<ExecGrant, PolicyError> {
    let at = within(err_prefix);
    let mut pairs = Vec::new();
    for (key, targets) in table_of(value, "exec: [allow: []]").map_err(&at)? {
        let evidence = match key {
            "allow" => Evidence::ALLOW,
            "deny" => Evidence::DENY,
            other => return Err(at(unknown_exec_key(other))),
        };
        let list = list_of(&targets, "targets").map_err(&at)?;
        for item in &list {
            let keys = decode_target(&item, ctx).map_err(&at)?;
            pairs.extend(keys.into_iter().map(|k| (k, evidence)));
        }
    }
    Ok(ExecGrant::from_iter(pairs))
}

fn unknown_exec_key(key: &str) -> PolicyError {
    let msg = nested("exec").unknown_key(key);
    if near_names(key, ["allow", "deny"], 1).is_empty() {
        PolicyError::new(msg).with_hint(format!(
            "per-command policies and argument restrictions are no longer supported; \
             write `allow: [`name {key}]` or `deny: [`name {key}]`"
        ))
    } else {
        PolicyError::new(msg)
    }
}

// ── Capability walker ─────────────────────────────────────────────────────

/// Walk a capability record into a frozen [`Capabilities`], resolving every
/// place against `ctx`.
///
/// The single `Value → Capabilities` constructor, shared by the
/// `grant [...] { body }` builtin in `evaluator::scope`, the
/// capability-file loader in `capability::load`, and [`crate::SpawnGrant`]'s
/// resolver; every path is frozen against `ctx` right here, so "a
/// `Capabilities` has every path already resolved" holds by construction
/// whoever comes through.  Unknown keys error here and in each dimension's
/// decoder rather than being silently dropped; the keyset is `grant`'s own
/// declared table, so this door and the checker cannot drift apart.
///
/// # Errors
/// A [`PolicyError`] naming the key whose shape or value it will not read, or
/// the place that would not resolve absolute.
pub fn decode_capability_map(
    value: &Value,
    err_prefix: &str,
    ctx: &FreezeCtx,
) -> Result<Capabilities, PolicyError> {
    let entries = settings_map(value).ok_or_else(|| {
        PolicyError::new(format!(
            "{err_prefix} expects a record or `()`, got {}",
            value.type_name()
        ))
    })?;
    let mut caps = Capabilities::default();
    for (k, v) in &*entries {
        let at = format!("{err_prefix} {k}");
        match k {
            "exec" => caps.exec = Some(decode_exec(&v, &at, ctx)?),
            "fs" => caps.fs = Some(decode_fs(&v, &at, ctx)?),
            "net" => caps.net = Some(decode_net(&v, &at)?),
            "editor" => caps.editor = Some(decode_verbs::<EditorVerb>(&v, &EDITOR, &at)?),
            "shell" => caps.shell = Some(decode_verbs::<ShellVerb>(&v, &SHELL, &at)?),
            other => {
                let table = declared(ContractForm::Grant);
                let err = PolicyError::new(format!("{err_prefix}: {}", table.unknown_key(other)));
                return Err(match other {
                    "detach" => err.with_hint("`detach` is a shell verb now: list it in `shell`, as in `shell: [`chdir, `detach]`"),
                    _ => err,
                });
            }
        }
    }
    Ok(caps)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs scaffolding: FreezeCtx cwds and tempdir files"
)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::path::FrozenPath;

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
        FreezeCtx {
            home: Some(home.into()),
            cwd: cwd.into(),
        }
    }

    fn decode(value: &Value) -> Result<Capabilities, PolicyError> {
        decode_capability_map(value, "test", &ctx())
    }

    fn grant_of(exec_value: Value) -> Result<ExecGrant, PolicyError> {
        decode(&rec(&[("exec", exec_value)])).map(|c| c.exec.unwrap())
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
    fn exec_names_decode_to_name_keys_with_their_list_evidence() {
        let g = grant_of(exec(
            vec![tagged("name", "git")],
            vec![tagged("name", "bash")],
        ))
        .unwrap();
        assert_eq!(g.get(&ExecKey::Name("git".into())), Some(&Evidence::ALLOW));
        assert_eq!(g.get(&ExecKey::Name("bash".into())), Some(&Evidence::DENY));
    }

    #[cfg(unix)]
    #[test]
    fn exec_places_decode_to_path_keys_and_dir_to_dir_keys() {
        let g = grant_of(exec(
            vec![
                tagged("absolute", "/bin/sh"),
                tagged("cwd", "tools/build"),
                dir(tagged("absolute", "/opt/tools")),
            ],
            vec![],
        ))
        .unwrap();
        let key = |k: fn(FrozenPath) -> ExecKey, p: &str| k(FrozenPath::from_surface(p));
        assert_eq!(
            g.get(&key(ExecKey::Path, "/bin/sh")),
            Some(&Evidence::ALLOW)
        );
        assert_eq!(
            g.get(&key(ExecKey::Path, "/work/tools/build")),
            Some(&Evidence::ALLOW)
        );
        assert_eq!(
            g.get(&key(ExecKey::Dir, "/opt/tools")),
            Some(&Evidence::ALLOW)
        );
    }

    #[test]
    fn exec_dir_system_expands_to_the_tool_roots() {
        let g = grant_of(exec(vec![dir(bare("system"))], vec![])).unwrap();
        assert!(g.iter().count() >= 1);
        assert!(
            g.iter()
                .all(|(k, v)| matches!(k, ExecKey::Dir(_)) && *v == Evidence::ALLOW)
        );
    }

    /// A list is a set: a repeat adds nothing, and refuses nothing.
    #[test]
    fn exec_repeats_are_idempotent() {
        let once = grant_of(exec(vec![tagged("name", "git")], vec![])).unwrap();
        let twice = grant_of(exec(
            vec![tagged("name", "git"), tagged("name", "git")],
            vec![],
        ))
        .unwrap();
        assert_eq!(once, twice);
    }

    /// A target in both lists is Both, which `↓` refuses.
    #[test]
    fn a_target_in_both_lists_is_both() {
        let g = grant_of(exec(
            vec![tagged("name", "git")],
            vec![tagged("name", "git")],
        ))
        .unwrap();
        assert_eq!(g.get(&ExecKey::Name("git".into())), Some(&Evidence::BOTH));
    }

    #[test]
    fn an_empty_allow_list_admits_nothing() {
        let g = grant_of(rec(&[("allow", list(vec![]))])).unwrap();
        assert_eq!(g.iter().count(), 0);
    }

    #[test]
    fn a_path_shaped_name_is_refused_with_a_place_hint() {
        for name in [
            "./tool",
            "/bin/sh",
            "a\\b",
            "~",
            "xdg:bin",
            "..",
            "",
            "C:tool.exe",
            "c:x",
        ] {
            let e = exec_refusal(exec(vec![tagged("name", name)], vec![]));
            assert!(
                e.message.contains("bare command name"),
                "{name}: {}",
                e.message
            );
            assert!(e.hint.unwrap().contains("`absolute /path`"));
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
            e.message.contains("`nmae is not an exec target;"),
            "{}",
            e.message
        );
        assert!(
            e.message.ends_with("(did you mean `name?)"),
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
        for sel in ["path", "system"] {
            let e = exec_refusal(exec(vec![bare(sel)], vec![]));
            let hint = e.hint.unwrap();
            assert!(hint.contains(&format!("write `dir `{sel}")), "{hint}");
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
            e.message.contains("`dir needs a place or a selector"),
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
            rec(&[("git", Value::Bool(true))]),
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
        assert!(e.hint.is_none());
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
        let caps = decode(&rec(&[("fs", rec(&[("read", list(places))]))])).unwrap();
        let read: Vec<_> = caps
            .fs
            .unwrap()
            .read_prefixes
            .iter()
            .map(|p| p.as_str().to_string())
            .collect();
        assert_eq!(read, ["/h/.cache", "/work/src"]);
    }

    #[test]
    fn fs_lists_refuse_what_is_not_a_place() {
        for item in [
            tagged("name", "git"),
            dir(tagged("cwd", ".")),
            bare("path"),
            bare("system"),
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
            m.contains("`cwdd is not a place; a place is one of `absolute, `cwd, "),
            "{m}"
        );
        assert!(m.ends_with("`xdg-bin (did you mean `cwd?)"), "{m}");
        let m = fs_refusal(vec![bare("cwd")]).message;
        assert!(m.contains("`cwd needs a payload"), "{m}");
    }

    #[test]
    fn an_empty_fs_list_keeps_only_the_floor() {
        let caps = decode(&rec(&[("fs", rec(&[("read", list(vec![]))]))])).unwrap();
        assert_eq!(caps.fs, Some(FsPolicy::default()));
    }

    #[test]
    fn a_dead_place_is_dropped_not_refused() {
        let foreign = if cfg!(windows) { "/x" } else { r"C:\x" };
        let caps = decode(&rec(&[(
            "fs",
            rec(&[("read", list(vec![tagged("absolute", foreign)]))]),
        )]))
        .unwrap();
        assert_eq!(caps.fs, Some(FsPolicy::default()));
    }

    // ── net, editor, shell ──

    #[test]
    fn net_offline_decodes_and_nothing_else_does() {
        let caps = decode(&rec(&[("net", bare("offline"))])).unwrap();
        assert_eq!(caps.net, Some(Net::Offline));
        for value in [Value::Bool(true), Value::Bool(false)] {
            let e = refusal(&rec(&[("net", value)]));
            assert!(e.message.contains("expected a net mode"), "{}", e.message);
            assert!(
                e.hint
                    .unwrap()
                    .contains("omit `net` to inherit, or write net: `offline"),
                "hint"
            );
        }
        let m = refusal(&rec(&[("net", bare("online"))])).message;
        assert!(m.contains("`online is not a net mode"), "{m}");
        let m = refusal(&rec(&[(
            "net",
            Value::variant("offline", Some(Value::Unit)),
        )]))
        .message;
        assert!(m.contains("takes no payload"), "{m}");
    }

    #[test]
    fn verb_lists_decode_as_sets() {
        let caps = decode(&rec(&[
            (
                "editor",
                list(vec![bare("read"), bare("tui"), bare("read")]),
            ),
            ("shell", list(vec![])),
        ]))
        .unwrap();
        assert_eq!(
            caps.editor,
            Some([EditorVerb::Read, EditorVerb::Tui].into())
        );
        assert_eq!(caps.shell, Some(BTreeSet::new()));
    }

    #[test]
    fn an_unknown_verb_lists_the_offered_ones() {
        let m = refusal(&rec(&[("shell", list(vec![bare("chdri")]))])).message;
        assert!(m.contains("`chdri is not a verb"), "{m}");
        assert!(
            m.contains("`chdir, `detach") && m.contains("did you mean `chdir?"),
            "{m}"
        );
        let m = refusal(&rec(&[("editor", list(vec![bare("exec")]))])).message;
        assert!(m.contains("`read, `write, `tui"), "{m}");
    }

    #[test]
    fn the_boolean_record_surface_is_refused() {
        for key in ["editor", "shell"] {
            let m = refusal(&rec(&[(key, rec(&[("read", Value::Bool(true))]))])).message;
            assert!(m.contains("expected a list of"), "{m}");
        }
        let e = refusal(&rec(&[("shell", list(vec![Value::string("chdir")]))]));
        assert!(e.message.contains("expected a verb"), "{}", e.message);
        assert!(e.hint.unwrap().contains("`shell takes the list of verbs"));
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
        assert!(e.message.contains("unknown key 'detach'"), "{}", e.message);
        assert!(e.hint.unwrap().contains("`shell: [`chdir, `detach]`"));
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
