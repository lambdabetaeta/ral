//! Editor builtins — line editor interface exposed to plugin handlers.
//!
//! Each op (`_ed-get`, `_ed-set`, `_ed-push`, …) is its own builtin so the
//! type checker sees the actual return type, arity is fixed per op, and the
//! `_` prefix hides them from `help`.  Each is a thin engine-side wrapper: it
//! keeps its capability and argument checks, and puts the rest to the host
//! as a `` `repl-editor `` enquiry, which the host answers only while an
//! editor context is installed for the dispatch — inside a plugin handler.

use ral_core::authority::policy::{EditorVerb, Flag};
use ral_core::builtin::{BuiltinBody, BuiltinEntry};
use ral_core::data::FOValue;
use ral_core::data::datum::Datum;
use ral_core::error::Error;
use ral_core::fact::{
    DataStep, EditorFailure, EditorFailureUnknownStyle, Expected, FailureCode as _, HostFailure,
    InvalidCause, InvalidFailure, ProtocolData, ProtocolFailure, ProtocolFailureDecode, RalFailure,
    Reason, ShapeFailure, UnexpectedPayload, UnknownTag, ValueShape,
};
use ral_core::shell::runtime::{Break, Mooring, Settled};
use ral_core::ty::Site;
use ral_core::ty::{
    Binders, CompTy, Grade, Kind, Ty, Typed, closed_record, closed_variant, open_record,
};
use ral_core::typecheck::builtins::{fun, pure, thunk};
use ral_core::{Shell, VClosRef, ValClos, View};
use std::borrow::Cow;
use std::sync::Arc;

use super::super::captured_line;
use super::super::completion::{CommandAt, parse_at};
use super::super::enquiry::{ClipboardReq, Data, EditorOp, EditorSnapshot, Gated, HighlightReq};
use super::super::highlight_style::{style_ansi, style_names};

/// Put `op` to the host, which `name` may do under `op`'s grant only.
fn ask(shell: &Shell, mooring: &Mooring, name: &str, op: impl Gated) -> Settled<ValClos> {
    answer::<Data>(shell, mooring, name, op).map(|Data(v)| v.into())
}

/// `op`'s answer, decoded as a `T`.
fn answer<T: Datum>(shell: &Shell, mooring: &Mooring, name: &str, op: impl Gated) -> Settled<T> {
    shell.check(op.flag(), name)?;
    Ok(
        T::decode(&shell.enquire(mooring, op.into().encode())?).map_err(|cause| {
            Error::new(Reason::Protocol(Box::new(ProtocolFailure::Decode(
                ProtocolFailureDecode {
                    kind: ProtocolData::Answer,
                    operation: name.into(),
                    cause,
                },
            ))))
        })?,
    )
}

/// A non-negative offset, flooring a negative one at zero.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "floored to 0; char offsets far below usize::MAX"
)]
fn offset(n: i64) -> usize {
    n.max(0) as usize
}

// ─── State read ──────────────────────────────────────────────────────────────

/// `_ed-get` → `[text: Str, cursor: Int, keymap: Str]`
pub fn builtin_ed_get(_args: &[ValClos], mooring: &Mooring, shell: &mut Shell) -> Settled<ValClos> {
    ask(shell, mooring, "_ed-get", EditorOp::Get)
}

// ─── State write ─────────────────────────────────────────────────────────────

fn invalid(operation: &str, path: Vec<DataStep>, cause: InvalidCause) -> Error {
    Error::new(Reason::Invalid(Box::new(InvalidFailure {
        operation: operation.into(),
        path,
        cause,
    })))
}

fn shape(operation: &str, path: Vec<DataStep>, value: VClosRef<'_>, expected: Expected) -> Error {
    invalid(
        operation,
        path,
        InvalidCause::Shape(ShapeFailure {
            expected,
            found: value.found(),
        }),
    )
}

/// `_ed-set`'s request: each field is `` `keep `` or `` `set v ``.
fn set_op(arg: &ValClos) -> Settled<EditorOp> {
    let map = arg.as_map("_ed-set")?;
    let choice = |key: &str| {
        let path = vec![DataStep::Field(key.into())];
        let Some(value) = map.get(key) else {
            return Err(invalid(
                "_ed-set",
                vec![],
                InvalidCause::MissingField(key.into()),
            ));
        };
        match value.view() {
            View::Variant { label, payload } if &**label == "keep" => match payload.view() {
                View::Unit => Ok(None),
                _ => Err(invalid(
                    "_ed-set",
                    path,
                    InvalidCause::UnexpectedPayload(UnexpectedPayload {
                        tag: label.to_string(),
                        found: payload.found(),
                    }),
                )),
            },
            View::Variant { label, payload } if &**label == "set" => match payload.view() {
                View::Unit => Err(invalid(
                    "_ed-set",
                    path,
                    InvalidCause::MissingPayload(label.to_string()),
                )),
                _ => Ok(Some(payload)),
            },
            View::Variant { label, .. } => Err(invalid(
                "_ed-set",
                path,
                InvalidCause::UnknownTag(UnknownTag {
                    tag: label.to_string(),
                    offered: vec!["keep".into(), "set".into()],
                }),
            )),
            _ => Err(shape(
                "_ed-set",
                path,
                value,
                Expected::Tags(vec!["keep".into(), "set".into()]),
            )),
        }
    };
    let text = choice("text")?
        .map(|v| match v.view() {
            View::String(s) => Ok(s.to_string()),
            _ => Err(shape(
                "_ed-set",
                vec![
                    DataStep::Field("text".into()),
                    DataStep::Payload("set".into()),
                ],
                v,
                Expected::Shape(ValueShape::String),
            )),
        })
        .transpose()?;
    let cursor = choice("cursor")?
        .map(|v| match v.view() {
            View::Int(n) => Ok(offset(n)),
            _ => Err(shape(
                "_ed-set",
                vec![
                    DataStep::Field("cursor".into()),
                    DataStep::Payload("set".into()),
                ],
                v,
                Expected::Shape(ValueShape::Int),
            )),
        })
        .transpose()?;
    Ok(EditorOp::Set { text, cursor })
}

/// `_ed-set [text: `keep|`set Str, cursor: `keep|`set Int]` — partial write.
pub fn builtin_ed_set(args: &[ValClos], mooring: &Mooring, shell: &mut Shell) -> Settled<ValClos> {
    ask(shell, mooring, "_ed-set", set_op(&args[0])?)
}

/// `_ed-set-lbuffer <l>` — replace text left of cursor; right side preserved.
pub fn builtin_ed_set_lbuffer(
    args: &[ValClos],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    ask(
        shell,
        mooring,
        "_ed-set-lbuffer",
        EditorOp::SetLbuffer(args[0].as_str("_ed-set-lbuffer")?.to_owned()),
    )
}

/// `_ed-insert <str>` — insert at cursor; cursor advances to end of insertion.
pub fn builtin_ed_insert(
    args: &[ValClos],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    ask(
        shell,
        mooring,
        "_ed-insert",
        EditorOp::Insert(args[0].as_str("_ed-insert")?.to_owned()),
    )
}

/// `_ed-push` — save buffer to stack, clear.
pub fn builtin_ed_push(
    _args: &[ValClos],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    ask(shell, mooring, "_ed-push", EditorOp::Push)
}

/// `_ed-accept` — mark buffer for immediate execution.
pub fn builtin_ed_accept(
    _args: &[ValClos],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    ask(shell, mooring, "_ed-accept", EditorOp::Accept)
}

// ─── TUI ─────────────────────────────────────────────────────────────────────

/// Build the `[output: .., status: Int]` record returned by `_ed-tui`.
fn tui_result(output: ValClos, status: i64) -> ValClos {
    ValClos::map(vec![
        ("output".into(), output),
        ("status".into(), ValClos::int(status)),
    ])
}

/// `_ed-tui {body}` — suspend editor, run body, return `[output: Str, status: Int]`.
///
/// On success: `status: 0`, `output: <the body's captured stdout>`.
/// On error  : `status: <error exit code>`, `output: <error message>`.
///
/// The body's stdout is captured so that a TUI command (e.g. `fzf`) which
/// prints its selection on stdout can have that selection delivered back to
/// the plugin as a String.  The TUI itself draws on /dev/tty via stderr, so
/// capturing stdout does not disrupt the interface.  The body is a command, and
/// the captured bytes are decoded, lossily,
/// one trailing LF or CRLF stripped — the call displays a terminal selection rather
/// than computing with it.
///
/// The pipeline foreground signal is a derived [`Mooring`]:
/// [`Mooring::lend_terminal`] raises `terminal_access` to `ExplicitLoan`,
/// which the pipeline foreground rule honors, keeping `_ed-tui`'s body in
/// the foreground process group despite the captured stdout pipe.  The
/// loaned mooring dies with the call — nothing to restore.  The loan exists
/// only in a run holding the terminal, once, so `_ed-tui` reports status 1
/// anywhere else or nested.
pub fn builtin_ed_tui(args: &[ValClos], mooring: &Mooring, shell: &mut Shell) -> Settled<ValClos> {
    shell.check(Flag::Editor(EditorVerb::Tui), "_ed-tui")?;
    let Some(loaned) = mooring.lend_terminal() else {
        return Ok(tui_result(
            ValClos::string(
                "_ed-tui: this run holds no terminal to lend (a TUI opens from a keybinding handler, and not inside another)",
            ),
            1,
        ));
    };
    // A TUI plugin's own screen output, not a value the program binds: the
    // kept prefix is the whole report a 16 MiB draw deserves.
    let (result, retained) = ral_core::shell::evaluator::with_capture(shell, |shell| {
        ral_core::shell::evaluator::apply(&args[0], Vec::new(), &loaned, shell)
    });
    match result {
        Ok(_) => Ok(tui_result(
            ValClos::string(captured_line(&retained.bytes)),
            0,
        )),
        Err(Break::Error(e)) => Ok(tui_result(
            ValClos::string(e.describe().into_owned()),
            i64::from(e.failure_code().get()),
        )),
        Err(other) => Err(other),
    }
}

// ─── Queries ─────────────────────────────────────────────────────────────────

/// `_ed-history <prefix> <limit>` — prefix search over history; `limit=0` for unbounded.
pub fn builtin_ed_history(
    args: &[ValClos],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    let prefix = args[0].as_str("_ed-history")?;
    let limit = match args[1].view() {
        View::Int(n) => offset(n),
        _ => {
            return Err(shape(
                "_ed-history",
                vec![DataStep::Index(1_usize.into())],
                VClosRef::from(&args[1]),
                Expected::Shape(ValueShape::Int),
            )
            .into());
        }
    };
    let history: Vec<String> = answer(shell, mooring, "_ed-history", EditorOp::History)?;
    let mut results: Vec<ValClos> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in history {
        if !entry.starts_with(prefix) || !seen.insert(entry.clone()) {
            continue;
        }
        results.push(ValClos::string(entry));
        if limit > 0 && results.len() >= limit {
            break;
        }
    }
    Ok(ValClos::list(results))
}

/// `_ed-parse` → `[words: [Str], current: Int, offset: Int]` — the simple
/// command at the cursor.  `words` is empty exactly when the buffer does not lex.
pub fn builtin_ed_parse(
    _args: &[ValClos],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    let EditorSnapshot { text, cursor, .. } = answer(shell, mooring, "_ed-parse", EditorOp::Get)?;
    Ok(parse_at(&text, cursor).unwrap_or_default().encode().into())
}

// ─── Output channels ─────────────────────────────────────────────────────────

/// `_ed-ghost <text>` — set ghost text (empty string clears).
pub fn builtin_ed_ghost(
    args: &[ValClos],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    ask(
        shell,
        mooring,
        "_ed-ghost",
        EditorOp::Ghost(args[0].as_str("_ed-ghost")?.to_owned()),
    )
}

/// `_ed-hyperlink <uri> <text>` — wrap `text` in an OSC 8 hyperlink to
/// `uri` when the host terminal recognises them; otherwise return `text`
/// unchanged.
///
/// Pure formatter — emits nothing.  Plugins decide where the result goes
/// (ghost text, an echo, a highlight message body).  The fallback to
/// plain `text` means the return value is always safe to display: the
/// worst case in a hyperlink-free terminal is an unformatted label.
///
/// No `editor.write` check: this is a string-shaping operation, not a
/// side effect on editor or system state.
pub fn builtin_ed_hyperlink(
    args: &[ValClos],
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    let uri = args[0].as_str("_ed-hyperlink")?;
    let text = args[1].as_str("_ed-hyperlink")?;
    let rendered = if shell.terminal().ui_hyperlinks_ok() {
        ral_core::io::ansi::osc8_link(uri, text)
    } else {
        text.to_owned()
    };
    Ok(ValClos::string(rendered))
}

/// `_ed-clipboard <text>` — copy `text` to the system clipboard.
///
/// Returns `Bool`: whether the whole text reached the clipboard, natively when
/// ral runs on this machine and else by OSC 52 (which also needs the
/// `ui_clipboard_write_ok` capability surfaced by `$TERMINAL`).  Gated on
/// `editor.write`.
pub fn builtin_ed_clipboard(
    args: &[ValClos],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    let req = ClipboardReq {
        text: args[0].as_str("_ed-clipboard")?.to_owned(),
        osc52: shell.terminal().ui_clipboard_write_ok(),
    };
    answer(shell, mooring, "_ed-clipboard", req).map(ValClos::bool)
}

/// `_ed-highlight <spans>` — set highlight spans (empty list clears).
pub fn builtin_ed_highlight(
    args: &[ValClos],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    let spans = args[0]
        .as_list("_ed-highlight")?
        .iter()
        .enumerate()
        .map(|(index, v)| highlight_req(index, v))
        .collect::<Settled<_>>()?;
    ask(shell, mooring, "_ed-highlight", EditorOp::Highlight(spans))
}

/// One `_ed-highlight` span: row-polymorphic, its style checked here.
fn highlight_req(index: usize, v: VClosRef<'_>) -> Settled<HighlightReq> {
    let int_field = |v: VClosRef<'_>, field: &'static str| match v.view() {
        View::Int(n) => Ok(offset(n)),
        _ => Err(shape(
            "_ed-highlight",
            vec![DataStep::Index(index.into()), DataStep::Field(field.into())],
            v,
            Expected::Shape(ValueShape::Int),
        )),
    };
    let View::Map(m) = v.view() else {
        return Err(shape(
            "_ed-highlight",
            vec![DataStep::Index(index.into())],
            v,
            Expected::Shape(ValueShape::Map),
        )
        .into());
    };
    let mut span = HighlightReq {
        start: 0,
        end: 0,
        style: String::new(),
    };
    for (k, v) in m.iter() {
        match k {
            "start" => span.start = int_field(v, "start")?,
            "end" => span.end = int_field(v, "end")?,
            "style" => span.style = v.to_string(),
            _ => {}
        }
    }
    if style_ansi(&span.style).is_none() {
        return Err(Error::new(Reason::Host(Box::new(HostFailure::Ral(
            RalFailure::Editor(EditorFailure::UnknownStyle(EditorFailureUnknownStyle {
                style: span.style,
                offered: style_names().map(str::to_owned).collect(),
            })),
        ))))
        .into());
    }
    Ok(span)
}

// ─── Plugin-local state ──────────────────────────────────────────────────────

/// `_ed-state <default> <updater>` — read-modify-write on the plugin's
/// persistent cell.  `default` is used on first call; `updater` is invoked
/// with the current value and its return becomes the new value, which the
/// host keeps, so it must be data.
///
/// A boundary: the cell lives for the session, so the value the host hands
/// back was decided by whichever run last wrote it, and is admitted against
/// this call's type before the updater runs.  The updater's own result was
/// produced by typed code and is stored as it is.
pub fn builtin_ed_state(
    args: &[ValClos],
    site: &Arc<Site>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<ValClos> {
    // Write is checked before the updater runs, not after it.
    shell.check(Flag::Editor(EditorVerb::Write), "_ed-state")?;
    let current = match answer::<Option<Data>>(shell, mooring, "_ed-state", EditorOp::StateGet)? {
        Some(Data(stored)) => {
            let stored = ValClos::from(stored);
            // The cell outlives each run, so what it holds was decided by
            // whichever run last wrote it.
            site.admit(&stored, shell.fixings())
                .map_err(|mismatch| mismatch.refusal("_ed-state"))?;
            stored
        }
        None => args[0].clone(),
    };
    let new_val = ral_core::shell::evaluator::apply(&args[1], vec![current], mooring, shell)?;
    let data = FOValue::try_from(&new_val)
        .map_err(|e| invalid("_ed-state", vec![], InvalidCause::NonData(e.into())))?;
    ask(shell, mooring, "_ed-state", EditorOp::StateSet(Data(data)))?;
    Ok(new_val)
}

// ─── Host registration ───────────────────────────────────────────────────────
//
// Every facet that `ral_core::shell::builtins` exposes is carried by the entry
// that owns the call function.  This is a static host extension: plugins
// remain host source/handler/hook loaders above this surface.

fn scheme_ed_get(_: &mut Binders) -> Ty {
    thunk(pure(EditorSnapshot::ty()))
}

fn scheme_unit_thunk(_: &mut Binders) -> Ty {
    thunk(pure(Ty::unit()))
}

fn scheme_ed_set(_: &mut Binders) -> Ty {
    let field = |ty| closed_variant(&[("keep", Ty::unit()), ("set", ty)]);
    let record = closed_record(&[("text", field(Ty::string())), ("cursor", field(Ty::int()))]);
    thunk(fun(record, pure(Ty::unit())))
}

fn scheme_string_to_bool(_: &mut Binders) -> Ty {
    thunk(fun(Ty::string(), pure(Ty::bool())))
}

fn scheme_string_string_to_string(_: &mut Binders) -> Ty {
    thunk(fun(Ty::string(), fun(Ty::string(), pure(Ty::string()))))
}

fn scheme_highlight(binders: &mut Binders) -> Ty {
    let span = open_record(
        &[
            ("start", Ty::int()),
            ("end", Ty::int()),
            ("style", Ty::string()),
        ],
        binders.row(false),
    );
    thunk(fun(Ty::list(span), pure(Ty::unit())))
}

fn scheme_history(_: &mut Binders) -> Ty {
    thunk(fun(
        Ty::string(),
        fun(Ty::int(), pure(Ty::list(Ty::string()))),
    ))
}

fn scheme_parse(_: &mut Binders) -> Ty {
    thunk(pure(CommandAt::ty()))
}

/// `_ed-tui :: ∀ε α. U(F^ε α) → F [output: Str, status: Int]` — absorbs its
/// body, whatever it produces; the output is the record's.
fn scheme_tui(binders: &mut Binders) -> Ty {
    let a = Ty::var(binders.ty(Kind::Any));
    let body = CompTy::ret(Grade::Var(binders.grade()), a);
    thunk(fun(
        thunk(body),
        pure(closed_record(&[
            ("output", Ty::string()),
            ("status", Ty::int()),
        ])),
    ))
}

fn scheme_state(binders: &mut Binders) -> Ty {
    let a = Ty::var(binders.ty(Kind::Data));
    thunk(fun(
        a.clone(),
        fun(thunk(fun(a.clone(), pure(a.clone()))), pure(a)),
    ))
}

// A named array, not a promoted temporary: rustc refuses promotion once an
// entry carries `BuiltinEntry`'s interior-mutable arity cache.
static ED_BUILTINS_ARR: [BuiltinEntry; 14] = [
    BuiltinEntry::new(
        Cow::Borrowed("_ed-get"),
        scheme_ed_get,
        "_ed-get  — return editor state record [text, cursor, keymap].",
        BuiltinBody::Static(builtin_ed_get),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-set"),
        scheme_ed_set,
        "_ed-set [text: `keep|`set <Str>, cursor: `keep|`set <Int>]  — partial write of editor state; `keep leaves a field as it is.",
        BuiltinBody::Static(builtin_ed_set),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-set-lbuffer"),
        ral_core::typecheck::builtins::scheme::string_to_unit,
        "_ed-set-lbuffer <text>  — replace text left of cursor; right side preserved.",
        BuiltinBody::Static(builtin_ed_set_lbuffer),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-insert"),
        ral_core::typecheck::builtins::scheme::string_to_unit,
        "_ed-insert <text>  — insert text at cursor; cursor advances past insertion.",
        BuiltinBody::Static(builtin_ed_insert),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-push"),
        scheme_unit_thunk,
        "_ed-push  — save buffer to stack, clear.",
        BuiltinBody::Static(builtin_ed_push),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-accept"),
        scheme_unit_thunk,
        "_ed-accept  — mark buffer for immediate execution.",
        BuiltinBody::Static(builtin_ed_accept),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-tui"),
        scheme_tui,
        "_ed-tui <thunk>  — suspend editor, run thunk, return [output: Str, status: Int]; never raises on body status.",
        BuiltinBody::Static(builtin_ed_tui),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-history"),
        scheme_history,
        "_ed-history <prefix> <limit>  — prefix search over history; limit=0 for unbounded.",
        BuiltinBody::Static(builtin_ed_history),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-parse"),
        scheme_parse,
        "_ed-parse  — the simple command at the cursor as [words, current, offset]: words[current] is the word being typed, offset its start.",
        BuiltinBody::Static(builtin_ed_parse),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-ghost"),
        ral_core::typecheck::builtins::scheme::string_to_unit,
        "_ed-ghost <text>  — set ghost text (empty string clears).",
        BuiltinBody::Static(builtin_ed_ghost),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-highlight"),
        scheme_highlight,
        "_ed-highlight <spans>  — set highlight spans (empty list clears).",
        BuiltinBody::Static(builtin_ed_highlight),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-clipboard"),
        scheme_string_to_bool,
        "_ed-clipboard <text>  — copy to the system clipboard, natively when local, else by OSC 52; returns Bool (true when all of it landed).",
        BuiltinBody::Static(builtin_ed_clipboard),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-hyperlink"),
        scheme_string_string_to_string,
        "_ed-hyperlink <uri> <text>  — wrap text in OSC 8 hyperlink; returns plain text when terminal can't render hyperlinks.",
        BuiltinBody::Static(builtin_ed_hyperlink),
    ),
    BuiltinEntry::boundary(
        Cow::Borrowed("_ed-state"),
        scheme_state,
        "_ed-state <default> <updater>  — read-modify-write the plugin's persistent cell.",
        builtin_ed_state,
    ),
];

/// Builtins installed into the REPL's own shell at startup
/// (see [`super::super::session::Session::boot`]).
pub static ED_BUILTINS: &[BuiltinEntry] = &ED_BUILTINS_ARR;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::exec::line_run;
    use ral_core::{RequestedTerminalAccess, protocol::Run};

    /// The perimeter test's plugin half: a row whose result only its own call
    /// determines is a boundary, and `_ed-state` is the editor surface's one.
    #[test]
    fn a_result_only_variable_in_the_editor_surface_is_a_boundary() {
        for entry in ED_BUILTINS {
            let scheme = ral_core::test_access::builtin_scheme(entry);
            assert!(
                !ral_core::test_access::has_result_only_var(scheme) || entry.decl.is_boundary(),
                "{}: a result only its own call determines, and it is no boundary",
                entry.decl.name
            );
        }
        let boundaries: Vec<&str> = ED_BUILTINS
            .iter()
            .filter(|entry| entry.decl.is_boundary())
            .map(|entry| entry.decl.name.as_ref())
            .collect();
        assert_eq!(boundaries, ["_ed-state"]);
    }

    /// A desk keeping the plugin's cell: it answers `StateGet` with what an
    /// earlier session stored and counts every request it is asked.
    struct Cell {
        stored: FOValue,
        asked: std::sync::atomic::AtomicUsize,
    }

    impl ral_core::shell::runtime::EnquiryDesk for Cell {
        fn enquire(
            &self,
            req: FOValue,
            _scope: &ral_core::process::Scope,
        ) -> ral_core::shell::runtime::Settled<FOValue> {
            use super::super::super::enquiry::Enquiry;
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match Enquiry::decode(&req) {
                Ok(Enquiry::Editor(EditorOp::StateGet)) => {
                    Ok(Some(Data(self.stored.clone())).encode())
                }
                Ok(Enquiry::Editor(EditorOp::StateSet(_))) => Ok(FOValue::Unit),
                other => panic!("the cell answers state reads and writes only, got {other:?}"),
            }
        }
    }

    const BUMP: &str = "_ed-state 0 { |n| return $[$n + 1] }";

    /// Run `run` against a cell holding `stored`; its result and how many
    /// requests the cell was asked.
    fn run_with_cell(stored: FOValue, run: Run) -> (Result<ValClos, Break>, usize) {
        let cell = std::sync::Arc::new(Cell {
            stored,
            asked: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut runtime = ral_core::test_helper::core_runtime();
        runtime.install_builtins(ED_BUILTINS);
        let mut shell = ral_core::Shell::root(runtime.into(), ral_core::test_helper::process_cwd());
        shell.set_interactive(true);
        let report = shell.run(ral_core::shell::run::RunRequest {
            desk: Some(cell.clone()),
            ..ral_core::shell::run::RunRequest::from(run)
        });
        let ral_core::shell::run::RunReport::Ran { ending, .. } = report else {
            panic!("the line must check and run");
        };
        (
            ending.into_settled(),
            cell.asked.load(std::sync::atomic::Ordering::SeqCst),
        )
    }

    /// The cell persists across sessions, so what it holds is admitted against
    /// the type this call uses it at before the updater runs: a cell an
    /// earlier program left at another type is refused, and nothing is written.
    #[test]
    fn a_stale_state_cell_is_refused_before_the_updater_runs() {
        let stale = FOValue::String {
            value: "stale".into(),
        };
        let (result, asked) = run_with_cell(stale, line_run(BUMP));
        let Err(Break::Error(e)) = result else {
            panic!("a stale cell must be refused")
        };
        let Reason::Invalid(failure) = &e.reason else {
            panic!("expected admission failure")
        };
        assert_eq!(failure.operation, "_ed-state");
        assert_eq!(failure.path, Vec::<ral_core::fact::DataStep>::new());
        assert!(
            matches!(&failure.cause, InvalidCause::Admission(ral_core::fact::AdmissionFailure { pointer, cause: ral_core::fact::AdmissionCause::Uses(ral_core::fact::AdmissionUses { found: ral_core::fact::Found::Text(preview), .. }) }) if pointer.is_empty() && preview.preview == "stale" && !preview.truncated)
        );
        assert!(e.witness().is_some());
        assert!(e.rejection().is_none());
        assert_eq!(asked, 1, "the updater must not have run");
    }

    #[test]
    fn a_state_cell_of_the_right_type_reaches_the_updater() {
        let (result, asked) = run_with_cell(FOValue::Int { value: 4 }, line_run(BUMP));
        assert!(
            matches!(result.as_ref().map(ValClos::view), Ok(View::Int(5))),
            "{result:?}"
        );
        assert_eq!(asked, 2, "the read, then the write");
    }

    /// `_ed-state` writes, so the write grant is checked before the host is
    /// asked anything and before the updater runs.
    #[test]
    fn a_state_cell_is_refused_without_the_write_grant() {
        let src = format!("grant [editor: [`read]] {{ {BUMP} }}");
        let (result, asked) = run_with_cell(FOValue::Int { value: 4 }, line_run(&src));
        let Err(Break::Error(e)) = result else {
            panic!("a read-only grant must refuse, got {result:?}");
        };
        assert!(
            e.describe().contains("_ed-state requires editor: [`write]"),
            "{}",
            e.describe()
        );
        assert_eq!(asked, 0, "the host must not have been asked");
    }

    /// A run holding no terminal has none to lend: `_ed-tui` reports status 1
    /// and asks the host nothing.
    #[test]
    fn tui_without_a_held_terminal_reports_status_one() {
        let run = Run {
            terminal: RequestedTerminalAccess::Denied,
            ..line_run("_ed-tui { echo hi }")
        };
        let (result, asked) = run_with_cell(FOValue::Unit, run);
        let value = result.expect("_ed-tui reports rather than raises");
        let m = value.as_map("_ed-tui").expect("a result record");
        assert!(matches!(
            m.get("status").map(VClosRef::view),
            Some(View::Int(1))
        ));
        assert_eq!(asked, 0);
    }

    #[test]
    fn ed_set_reads_a_literal_record_over_its_captures() {
        let mut shell = ral_core::test_helper::core_shell();
        let arg = crate::repl::eval(
            &mut shell,
            "let text = 'edited'; let cursor = 4; return [text: `set $text, cursor: `set $cursor]",
        );
        assert!(matches!(
            set_op(&arg),
            Ok(EditorOp::Set { text: Some(text), cursor: Some(4) }) if text == "edited"
        ));
    }

    /// A negative cursor floors at zero; the host clamps the rest.
    #[test]
    fn ed_set_floors_a_negative_cursor() {
        let arg = ValClos::map(vec![
            ("text".into(), ValClos::variant("keep", None)),
            (
                "cursor".into(),
                ValClos::variant("set", Some(ValClos::int(-4))),
            ),
        ]);
        assert!(matches!(
            set_op(&arg),
            Ok(EditorOp::Set {
                text: None,
                cursor: Some(0)
            })
        ));
    }
    #[test]
    fn editor_answer_decoding_and_existing_refusals_stay_distinct() {
        struct Desk(Settled<FOValue>);
        impl ral_core::shell::runtime::EnquiryDesk for Desk {
            fn enquire(&self, _: FOValue, _: &ral_core::process::Scope) -> Settled<FOValue> {
                self.0.clone()
            }
        }
        fn run(answer: Settled<FOValue>) -> Error {
            let mut runtime = ral_core::test_helper::core_runtime();
            runtime.install_builtins(ED_BUILTINS);
            let mut shell = Shell::root(runtime.into(), ral_core::test_helper::process_cwd());
            let ral_core::shell::run::RunReport::Ran { ending, .. } =
                shell.run(ral_core::shell::run::RunRequest {
                    desk: Some(Arc::new(Desk(answer))),
                    ..ral_core::shell::run::RunRequest::from(line_run("_ed-parse"))
                })
            else {
                panic!("editor query must run")
            };
            let Err(Break::Error(error)) = ending.into_settled() else {
                panic!("expected refusal")
            };
            error
        }
        let malformed = FOValue::Int { value: 9 };
        let error = run(Ok(malformed.clone()));
        assert_eq!(
            error.reason,
            Reason::Protocol(Box::new(ProtocolFailure::Decode(ProtocolFailureDecode {
                kind: ProtocolData::Answer,
                operation: "_ed-parse".into(),
                cause: EditorSnapshot::decode(&malformed).unwrap_err(),
            })))
        );
        assert_eq!(error.hints(), []);
        assert!(error.context().is_none());
        let original = Error::new(Reason::State(Box::new(
            ral_core::fact::StateFailure::NoDesk,
        )))
        .with_context("host transaction")
        .with_hints(["first", "second"]);
        let error = run(Err(original.clone().into()));
        assert_eq!(error.reason, original.reason);
        assert_eq!(error.context(), Some("host transaction"));
        assert_eq!(error.hints(), original.hints());
        assert!(error.rejection().is_none());
    }

    #[test]
    fn editor_shape_failures_keep_field_payload_and_index_paths() {
        let arg = ValClos::map(vec![
            ("text".into(), ValClos::variant("keep", None)),
            (
                "cursor".into(),
                ValClos::variant("set", Some(ValClos::string("3"))),
            ),
        ]);
        let Err(Break::Error(error)) = set_op(&arg) else {
            panic!("expected invalid cursor")
        };
        let Reason::Invalid(failure) = error.reason else {
            panic!("expected shape failure")
        };
        assert_eq!(
            failure.path,
            vec![
                DataStep::Field("cursor".into()),
                DataStep::Payload("set".into())
            ]
        );
        assert_eq!(
            failure.cause,
            InvalidCause::Shape(ShapeFailure {
                expected: Expected::Shape(ValueShape::Int),
                found: ValClos::string("3").found()
            })
        );
        let span = ValClos::map(vec![("end".into(), ValClos::bool(false))]);
        let Err(Break::Error(error)) = highlight_req(2, VClosRef::from(&span)) else {
            panic!("expected invalid end")
        };
        let Reason::Invalid(failure) = error.reason else {
            panic!("expected shape failure")
        };
        assert_eq!(
            failure.path,
            vec![
                DataStep::Index(2_usize.into()),
                DataStep::Field("end".into())
            ]
        );
        assert_eq!(
            failure.cause,
            InvalidCause::Shape(ShapeFailure {
                expected: Expected::Shape(ValueShape::Int),
                found: ral_core::fact::Found::Bool(false)
            })
        );
    }
}
