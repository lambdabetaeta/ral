//! Admit structural IR leaves while rebuilding the runtime tree.

use super::super::wire::RawData;
use crate::data::Bytes;
use crate::data::json::on_json_stack;
use crate::fact::{DecodeCause, DecodeFailure, RecIndex, SchemeNames};
use crate::ir::{
    Argv, CaseArm, Comp, CompKind, DefineSchemes, Exec, Explicit, Fields, Key, MapEntryV, Module,
    Name, Node, Pattern, Phrase, Prim, Redirects, StderrTarget, StdinSource, Toplevel, Val,
    ValListElem,
};
use crate::source::Spanned;
use crate::ty::{Closed, Scheme, Site};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;

pub(super) fn admit_val(value: &Val<Explicit, RawData>) -> Result<Val, DecodeFailure> {
    on_json_stack(|| match value {
        Val::Unit => Ok(Val::Unit),
        Val::String(value) => Ok(Val::String(value.clone())),
        Val::Int(value) => Ok(Val::Int(*value)),
        Val::Float(value) => Ok(Val::Float(*value)),
        Val::Bool(value) => Ok(Val::Bool(*value)),
        Val::Variable(name) => Ok(Val::Variable(name.clone())),
        Val::Thunk(node) => admit_comp(node.shape())
            .map(|comp| Val::thunk(Arc::new(comp)))
            .map_err(|error| error.payload("Thunk")),
        Val::List(node) => node
            .shape()
            .iter()
            .enumerate()
            .map(|(index, value)| {
                admit_spanned_val(value).map_err(|error| error.index(index.into()))
            })
            .collect::<Result<Box<[_]>, _>>()
            .map(Val::list)
            .map_err(|error| error.payload("List")),
        Val::Record(node) => admit_sorted_fields(node.shape())
            .map(Val::record)
            .map_err(|error| error.payload("Record")),
        Val::Map(node) => admit_sorted_fields(node.shape())
            .map(Val::map)
            .map_err(|error| error.payload("Map")),
        Val::Variant { label, payload } => Ok(Val::Variant {
            label: label.clone(),
            payload: Box::new(
                admit_val(payload).map_err(|error| error.field("payload").payload("Variant"))?,
            ),
        }),
        Val::Bytes(value) => Bytes::from_base64(value)
            .map(Val::Bytes)
            .map_err(|error| error.payload("Bytes")),
        Val::Builtin(entry) => Ok(Val::Builtin(Arc::clone(entry))),
        Val::Handle(handle) => Ok(Val::Handle(handle.clone())),
    })
}

fn admit_spanned_val(
    value: &Spanned<Val<Explicit, RawData>>,
) -> Result<Spanned<Val>, DecodeFailure> {
    on_json_stack(|| {
        admit_val(&value.item)
            .map(|item| Spanned::with_span(value.span, item))
            .map_err(|error| error.field("item"))
    })
}

type RawFields = [(Name, Spanned<Val<Explicit, RawData>>)];

/// The fields of a record or map, which `MapRef::get` binary-searches.
fn admit_sorted_fields(fields: &RawFields) -> Result<Fields, DecodeFailure> {
    let keys = fields.iter().map(|(key, _)| key);
    for (index, (before, key)) in keys.clone().zip(keys.skip(1)).enumerate() {
        let cause = match before.cmp(key) {
            Ordering::Less => continue,
            Ordering::Equal => DecodeCause::DuplicateField(key.to_string()),
            Ordering::Greater => DecodeCause::UnorderedField(key.to_string()),
        };
        return Err(DecodeFailure::new(cause)
            .index(0usize.into())
            .index((index + 1).into()));
    }
    admit_fields(fields)
}

/// The fields of a record update: written order, each key once.
fn admit_update_fields(fields: &RawFields) -> Result<Fields, DecodeFailure> {
    let mut seen = HashSet::with_capacity(fields.len());
    for (index, (key, _)) in fields.iter().enumerate() {
        if !seen.insert(key) {
            return Err(
                DecodeFailure::new(DecodeCause::DuplicateField(key.to_string()))
                    .index(0usize.into())
                    .index(index.into()),
            );
        }
    }
    admit_fields(fields)
}

fn admit_fields(fields: &RawFields) -> Result<Fields, DecodeFailure> {
    on_json_stack(|| {
        fields
            .iter()
            .enumerate()
            .map(|(index, (name, value))| {
                admit_spanned_val(value)
                    .map(|value| (name.clone(), value))
                    .map_err(|error| error.index(1usize.into()).index(index.into()))
            })
            .collect()
    })
}

fn admit_comp(comp: &Comp<Explicit, RawData>) -> Result<Comp, DecodeFailure> {
    on_json_stack(|| {
        admit_kind(&comp.item)
            .map(|item| Spanned::with_span(comp.span, item))
            .map_err(|error| error.field("item"))
    })
}

fn admit_kind(kind: &CompKind<Explicit, RawData>) -> Result<CompKind, DecodeFailure> {
    on_json_stack(|| match kind {
        CompKind::Return(value) => admit_val(value)
            .map(CompKind::Return)
            .map_err(|error| error.payload("Return")),
        CompKind::Force(value) => admit_val(value)
            .map(CompKind::Force)
            .map_err(|error| error.payload("Force")),
        CompKind::Lam { param, body } => Ok(CompKind::Lam {
            param: param.clone(),
            body: Arc::new(admit_comp(body).map_err(|error| error.field("body").payload("Lam"))?),
        }),
        CompKind::App {
            head,
            args,
            hole: (),
        } => Ok(CompKind::App {
            head: Arc::new(admit_comp(head).map_err(|error| error.field("head").payload("App"))?),
            args: args
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    admit_spanned_val(value).map_err(|error| error.index(index.into()))
                })
                .collect::<Result<_, _>>()
                .map_err(|error| error.field("args").payload("App"))?,
            hole: (),
        }),
        CompKind::Bind {
            comp,
            hole: (),
            pattern,
            rest,
        } => Ok(CompKind::Bind {
            comp: Arc::new(admit_comp(comp).map_err(|error| error.field("comp").payload("Bind"))?),
            hole: (),
            pattern: Arc::clone(pattern),
            rest: Arc::new(admit_comp(rest).map_err(|error| error.field("rest").payload("Bind"))?),
        }),
        CompKind::Seq { first, rest } => Ok(CompKind::Seq {
            first: Arc::new(
                admit_comp(first).map_err(|error| error.field("first").payload("Seq"))?,
            ),
            rest: Arc::new(admit_comp(rest).map_err(|error| error.field("rest").payload("Seq"))?),
        }),
        CompKind::Rec { group, index } => {
            if *index >= group.shape().len() {
                return Err(DecodeFailure::new(DecodeCause::RecIndex(RecIndex {
                    index: (*index).into(),
                    members: group.shape().len().into(),
                }))
                .field("index")
                .payload("Rec"));
            }
            Ok(CompKind::Rec {
                group: Node::new(
                    group
                        .shape()
                        .iter()
                        .enumerate()
                        .map(|(index, (name, comp))| {
                            admit_comp(comp)
                                .map(|comp| (name.clone(), Arc::new(comp)))
                                .map_err(|error| error.index(1usize.into()).index(index.into()))
                        })
                        .collect::<Result<Box<[_]>, _>>()
                        .map_err(|error| error.field("group").payload("Rec"))?,
                ),
                index: *index,
            })
        }
        CompKind::If { cond, then, else_ } => Ok(CompKind::If {
            cond: admit_spanned_val(cond).map_err(|error| error.field("cond").payload("If"))?,
            then: admit_spanned_val(then).map_err(|error| error.field("then").payload("If"))?,
            else_: admit_spanned_val(else_).map_err(|error| error.field("else_").payload("If"))?,
        }),
        CompKind::Case { scrutinee, arms } => Ok(CompKind::Case {
            scrutinee: admit_spanned_val(scrutinee)
                .map_err(|error| error.field("scrutinee").payload("Case"))?,
            arms: arms
                .iter()
                .enumerate()
                .map(|(index, arm)| admit_arm(arm).map_err(|error| error.index(index.into())))
                .collect::<Result<_, _>>()
                .map_err(|error| error.field("arms").payload("Case"))?,
        }),
        CompKind::Try { body, handler } => Ok(CompKind::Try {
            body: admit_val(body).map_err(|error| error.field("body").payload("Try"))?,
            handler: admit_val(handler).map_err(|error| error.field("handler").payload("Try"))?,
        }),
        CompKind::Guard {
            acquire,
            hole: (),
            body,
            release,
        } => Ok(CompKind::Guard {
            acquire: admit_val(acquire).map_err(|error| error.field("acquire").payload("Guard"))?,
            hole: (),
            body: admit_val(body).map_err(|error| error.field("body").payload("Guard"))?,
            release: admit_val(release).map_err(|error| error.field("release").payload("Guard"))?,
        }),
        CompKind::Capture(body) => admit_comp(body)
            .map(|body| CompKind::Capture(Arc::new(body)))
            .map_err(|error| error.payload("Capture")),
        CompKind::Prim(prim) => admit_prim(prim)
            .map(CompKind::Prim)
            .map_err(|error| error.payload("Prim")),
        CompKind::Exec(exec) => admit_exec(exec)
            .map(CompKind::Exec)
            .map_err(|error| error.payload("Exec")),
        CompKind::Pipeline { stages } => Ok(CompKind::Pipeline {
            stages: stages
                .iter()
                .enumerate()
                .map(|(index, comp)| {
                    admit_comp(comp)
                        .map(Arc::new)
                        .map_err(|error| error.index(index.into()))
                })
                .collect::<Result<_, _>>()
                .map_err(|error| error.field("stages").payload("Pipeline"))?,
        }),
        CompKind::Redirect { body, redirects } => Ok(CompKind::Redirect {
            body: Arc::new(
                admit_comp(body).map_err(|error| error.field("body").payload("Redirect"))?,
            ),
            redirects: admit_redirects(redirects)
                .map_err(|error| error.field("redirects").payload("Redirect"))?,
        }),
        CompKind::Within { opts, body } => Ok(CompKind::Within {
            opts: Node::new(
                admit_sorted_fields(opts.shape())
                    .map_err(|error| error.field("opts").payload("Within"))?,
            ),
            body: admit_val(body).map_err(|error| error.field("body").payload("Within"))?,
        }),
        CompKind::Grant { caps, body } => Ok(CompKind::Grant {
            caps: Node::new(
                admit_sorted_fields(caps.shape())
                    .map_err(|error| error.field("caps").payload("Grant"))?,
            ),
            body: admit_val(body).map_err(|error| error.field("body").payload("Grant"))?,
        }),
        CompKind::Spawn { title, body } => Ok(CompKind::Spawn {
            title: title
                .as_ref()
                .map(admit_spanned_val)
                .transpose()
                .map_err(|error| error.field("title").payload("Spawn"))?,
            body: admit_val(body).map_err(|error| error.field("body").payload("Spawn"))?,
        }),
        CompKind::Gather { body } => Ok(CompKind::Gather {
            body: admit_val(body).map_err(|error| error.field("body").payload("Gather"))?,
        }),
        CompKind::Audit { body } => Ok(CompKind::Audit {
            body: admit_val(body).map_err(|error| error.field("body").payload("Audit"))?,
        }),
        CompKind::Tilde(path) => Ok(CompKind::Tilde(path.clone())),
        CompKind::Use(module) => admit_module(module)
            .map(|module| CompKind::Use(Arc::new(module)))
            .map_err(|error| error.payload("Use")),
        CompKind::Builtin { builtin, args } => Ok(CompKind::Builtin {
            builtin: builtin.clone(),
            args: args.clone(),
        }),
        &CompKind::BindingCall { hole, .. } => match hole {},
    })
}

fn admit_arm(arm: &CaseArm<Explicit, RawData>) -> Result<CaseArm, DecodeFailure> {
    on_json_stack(|| {
        let CaseArm { tag, body } = arm;
        Ok(CaseArm {
            tag: tag.clone(),
            body: admit_spanned_val(body).map_err(|error| error.field("body"))?,
        })
    })
}

fn admit_exec(exec: &Exec<Explicit, RawData>) -> Result<Exec, DecodeFailure> {
    on_json_stack(|| {
        let Exec {
            head,
            argv,
            redirects,
            site,
        } = exec;
        Ok(Exec {
            head: head.clone(),
            argv: admit_argv(argv).map_err(|error| error.field("argv"))?,
            redirects: admit_redirects(redirects).map_err(|error| error.field("redirects"))?,
            site: site
                .as_ref()
                .map(admit_site)
                .transpose()
                .map_err(|error| error.field("site"))?,
        })
    })
}

/// A decoded site names only its own nodes, which a door indexes unchecked.
fn admit_site(site: &Arc<Site>) -> Result<Arc<Site>, DecodeFailure> {
    site.check().map(|()| Arc::clone(site))
}

fn admit_argv(argv: &Argv<Explicit, RawData>) -> Result<Argv, DecodeFailure> {
    on_json_stack(|| {
        argv.iter()
            .enumerate()
            .map(|(index, elem)| admit_list_elem(elem).map_err(|error| error.index(index.into())))
            .collect()
    })
}

fn admit_prim(prim: &Prim<Explicit, RawData>) -> Result<Prim, DecodeFailure> {
    on_json_stack(|| match prim {
        Prim::Binary(op, lhs, rhs) => Ok(Prim::Binary(
            *op,
            admit_val(lhs).map_err(|error| error.index(1usize.into()).payload("Binary"))?,
            admit_val(rhs).map_err(|error| error.index(2usize.into()).payload("Binary"))?,
        )),
        Prim::Negate(value) => admit_val(value)
            .map(Prim::Negate)
            .map_err(|error| error.payload("Negate")),
        Prim::Not(value) => admit_val(value)
            .map(Prim::Not)
            .map_err(|error| error.payload("Not")),
        Prim::Render(value) => admit_val(value)
            .map(Prim::Render)
            .map_err(|error| error.payload("Render")),
        Prim::Concat(parts) => parts
            .iter()
            .enumerate()
            .map(|(index, value)| admit_val(value).map_err(|error| error.index(index.into())))
            .collect::<Result<_, _>>()
            .map(Prim::Concat)
            .map_err(|error| error.payload("Concat")),
        Prim::Decode(value) => admit_val(value)
            .map(Prim::Decode)
            .map_err(|error| error.payload("Decode")),
        Prim::Index { target, key } => Ok(Prim::Index {
            target: admit_val(target).map_err(|error| error.field("target").payload("Index"))?,
            key: admit_key(&key.item)
                .map(|item| Spanned::with_span(key.span, item))
                .map_err(|error| error.field("item").field("key").payload("Index"))?,
        }),
        Prim::Update { base, fields } => Ok(Prim::Update {
            base: admit_spanned_val(base).map_err(|error| error.field("base").payload("Update"))?,
            fields: admit_update_fields(fields)
                .map_err(|error| error.field("fields").payload("Update"))?,
        }),
        Prim::List(elems) => elems
            .iter()
            .enumerate()
            .map(|(index, elem)| admit_list_elem(elem).map_err(|error| error.index(index.into())))
            .collect::<Result<_, _>>()
            .map(Prim::List)
            .map_err(|error| error.payload("List")),
        Prim::Map(entries) => entries
            .iter()
            .enumerate()
            .map(|(index, entry)| admit_map_entry(entry).map_err(|error| error.index(index.into())))
            .collect::<Result<_, _>>()
            .map(Prim::Map)
            .map_err(|error| error.payload("Map")),
    })
}

fn admit_key(key: &Key<Explicit, RawData>) -> Result<Key, DecodeFailure> {
    on_json_stack(|| match key {
        Key::Label(label) => Ok(Key::Label(label.clone())),
        Key::Position(index) => Ok(Key::Position(*index)),
        Key::Data(value) => admit_val(value)
            .map(Key::Data)
            .map_err(|error| error.payload("Data")),
    })
}

fn admit_list_elem(elem: &ValListElem<Explicit, RawData>) -> Result<ValListElem, DecodeFailure> {
    on_json_stack(|| match elem {
        ValListElem::Single(value) => admit_spanned_val(value)
            .map(ValListElem::Single)
            .map_err(|error| error.payload("Single")),
        ValListElem::Spread(value) => admit_spanned_val(value)
            .map(ValListElem::Spread)
            .map_err(|error| error.payload("Spread")),
    })
}

fn admit_map_entry(entry: &MapEntryV<Explicit, RawData>) -> Result<MapEntryV, DecodeFailure> {
    on_json_stack(|| match entry {
        MapEntryV::Entry(key, value) => Ok(MapEntryV::Entry(
            admit_spanned_val(key).map_err(|error| error.index(0usize.into()).payload("Entry"))?,
            admit_spanned_val(value)
                .map_err(|error| error.index(1usize.into()).payload("Entry"))?,
        )),
        MapEntryV::Spread(value) => admit_spanned_val(value)
            .map(MapEntryV::Spread)
            .map_err(|error| error.payload("Spread")),
    })
}

fn admit_redirects(
    redirects: &Redirects<Val<Explicit, RawData>>,
) -> Result<Redirects<Val>, DecodeFailure> {
    on_json_stack(|| {
        let Redirects {
            stdin,
            stdout,
            stderr,
        } = redirects;
        Ok(Redirects {
            stdin: match stdin {
                Some(StdinSource::File(value)) => Some(StdinSource::File(
                    admit_val(value).map_err(|error| error.payload("File").field("stdin"))?,
                )),
                Some(StdinSource::Here(value)) => Some(StdinSource::Here(
                    admit_val(value).map_err(|error| error.payload("Here").field("stdin"))?,
                )),
                None => None,
            },
            stdout: match stdout {
                Some((mode, value)) => Some((
                    *mode,
                    admit_val(value).map_err(|error| error.index(1usize.into()).field("stdout"))?,
                )),
                None => None,
            },
            stderr: match stderr {
                Some(StderrTarget::File(mode, value)) => Some(StderrTarget::File(
                    *mode,
                    admit_val(value).map_err(|error| {
                        error.index(1usize.into()).payload("File").field("stderr")
                    })?,
                )),
                Some(StderrTarget::Stdout) => Some(StderrTarget::Stdout),
                None => None,
            },
        })
    })
}

fn admit_module(module: &Module<RawData>) -> Result<Module, DecodeFailure> {
    on_json_stack(|| {
        let Module { top, origin } = module;
        Ok(Module {
            top: admit_top(top).map_err(|error| error.field("top"))?,
            origin: origin.clone(),
        })
    })
}

fn admit_top(top: &Toplevel<RawData>) -> Result<Toplevel, DecodeFailure> {
    on_json_stack(|| {
        let Toplevel { phrases, admits } = top;
        Ok(Toplevel {
            phrases: phrases
                .iter()
                .enumerate()
                .map(|(index, phrase)| {
                    admit_phrase(&phrase.item)
                        .map(|item| Spanned::with_span(phrase.span, item))
                        .map_err(|error| error.field("item").index(index.into()))
                })
                .collect::<Result<_, _>>()
                .map_err(|error| error.field("phrases"))?,
            admits: admits
                .iter()
                .enumerate()
                .map(|(index, (name, site))| {
                    admit_site(site)
                        .map(|site| (name.clone(), site))
                        .map_err(|error| error.index(1usize.into()).index(index.into()))
                })
                .collect::<Result<_, _>>()
                .map_err(|error| error.field("admits"))?,
        })
    })
}

fn admit_phrase(
    phrase: &Phrase<DefineSchemes<Arc<Scheme>>, Explicit, RawData>,
) -> Result<Phrase, DecodeFailure> {
    on_json_stack(|| match phrase {
        Phrase::Define {
            pattern,
            comp,
            hole: (),
            schemes,
        } => {
            bind_exactly(pattern, schemes)
                .map_err(|error| error.field("schemes").payload("Define"))?;
            Ok(Phrase::Define {
                pattern: Arc::clone(pattern),
                comp: Arc::new(
                    admit_comp(comp).map_err(|error| error.field("comp").payload("Define"))?,
                ),
                hole: (),
                schemes: schemes
                    .iter()
                    .enumerate()
                    .map(|(index, (name, scheme))| {
                        Closed::new(Arc::clone(scheme))
                            .map(|scheme| (name.clone(), scheme))
                            .map_err(|open| {
                                DecodeFailure::new(DecodeCause::OpenScheme(open))
                                    .index(1usize.into())
                                    .index(index.into())
                            })
                    })
                    .collect::<Result<_, _>>()
                    .map_err(|error| error.field("schemes").payload("Define"))?,
            })
        }
        Phrase::Run(comp) => admit_comp(comp)
            .map(|comp| Phrase::Run(Arc::new(comp)))
            .map_err(|error| error.payload("Run")),
    })
}

/// A module's exports are read off its schemes and looked up in the
/// environment its patterns bind.
fn bind_exactly(
    pattern: &Pattern,
    schemes: &DefineSchemes<Arc<Scheme>>,
) -> Result<(), DecodeFailure> {
    let bound: Vec<String> = pattern.names().iter().map(ToString::to_string).collect();
    let schemed: Vec<String> = schemes.iter().map(|(name, _)| name.clone()).collect();
    let sorted = |names: &[String]| {
        let mut names = names.to_vec();
        names.sort_unstable();
        names
    };
    if sorted(&bound) == sorted(&schemed) {
        Ok(())
    } else {
        Err(DecodeFailure::new(DecodeCause::SchemeNames(SchemeNames {
            bound,
            schemed,
        })))
    }
}
