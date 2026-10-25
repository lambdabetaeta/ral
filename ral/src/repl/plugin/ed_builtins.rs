//! Editor builtins — line editor interface exposed to plugin handlers.
//!
//! Each op (`_ed-get`, `_ed-set`, `_ed-push`, …) is its own builtin so the
//! type checker sees the actual return type, arity is fixed per op, and the
//! `_` prefix hides them from `help`.  Each is a thin engine-side wrapper: it
//! keeps its capability and argument checks, and puts the rest to the host
//! as a `` `repl-editor `` enquiry, which the host answers only while an
//! editor context is installed for the dispatch — inside a plugin handler.

use ral_core::capability::{EditorVerb, Flag};
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::Datum;
use ral_core::ty::Site;
use ral_core::ty::{
    CompTy, Grade, Kind, Scheme, Ty, Typed, closed_record, closed_variant, open_record,
};
use ral_core::typecheck::Unifier;
use ral_core::typecheck::builtins::{fun, graded, mk_scheme as scheme, pure, thunk};
use ral_core::types::{Break, BuiltinBody, BuiltinEntry, Mooring, Settled, sig};
use ral_core::{Shell, Value};
use std::borrow::Cow;
use std::sync::Arc;

use super::super::completion::{CommandAt, parse_at};
use super::super::enquiry::{ClipboardReq, Data, EditorOp, EditorSnapshot, Gated, HighlightReq};
use super::super::highlight_style::style_ansi;

/// Put `op` to the host, which `name` may do under `op`'s grant only.
fn ask(shell: &Shell, mooring: &Mooring, name: &str, op: impl Gated) -> Settled<Value> {
    answer::<Data>(shell, mooring, name, op).map(|Data(v)| v.into())
}

/// `op`'s answer, decoded as a `T`.
fn answer<T: Datum>(shell: &Shell, mooring: &Mooring, name: &str, op: impl Gated) -> Settled<T> {
    shell.check(op.flag(), name)?;
    T::decode(&shell.enquire(mooring, op.into().encode())?)
        .map_err(|why| sig(format!("the host answered outside its shape: {why}")))
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
pub fn builtin_ed_get(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    ask(shell, mooring, "_ed-get", EditorOp::Get)
}

// ─── State write ─────────────────────────────────────────────────────────────

/// `_ed-set`'s request: each field is `` `keep `` or `` `set v ``.
fn set_op(arg: &Value) -> Settled<EditorOp> {
    let map = arg.as_map("_ed-set")?;
    let choice = |key: &str| match map.get(key).as_deref() {
        Some(Value::Variant {
            label,
            payload: None,
        }) if &**label == "keep" => Ok(None),
        Some(Value::Variant {
            label,
            payload: Some(v),
        }) if &**label == "set" => Ok(Some((**v).clone())),
        _ => Err(sig(format!("_ed-set: {key} must be `keep or `set <value>"))),
    };
    let text = choice("text")?
        .map(|v| v.as_str("_ed-set text").map(str::to_owned))
        .transpose()?;
    let cursor = match choice("cursor")? {
        Some(Value::Int(n)) => Some(offset(n)),
        Some(_) => return Err(sig("_ed-set: cursor must be Int")),
        None => None,
    };
    Ok(EditorOp::Set { text, cursor })
}

/// `_ed-set [text: `keep|`set Str, cursor: `keep|`set Int]` — partial write.
pub fn builtin_ed_set(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    ask(shell, mooring, "_ed-set", set_op(&args[0])?)
}

/// `_ed-set-lbuffer <l>` — replace text left of cursor; right side preserved.
pub fn builtin_ed_set_lbuffer(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    ask(
        shell,
        mooring,
        "_ed-set-lbuffer",
        EditorOp::SetLbuffer(args[0].as_str("_ed-set-lbuffer")?.to_owned()),
    )
}

/// `_ed-insert <str>` — insert at cursor; cursor advances to end of insertion.
pub fn builtin_ed_insert(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    ask(
        shell,
        mooring,
        "_ed-insert",
        EditorOp::Insert(args[0].as_str("_ed-insert")?.to_owned()),
    )
}

/// `_ed-push` — save buffer to stack, clear.
pub fn builtin_ed_push(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    ask(shell, mooring, "_ed-push", EditorOp::Push)
}

/// `_ed-accept` — mark buffer for immediate execution.
pub fn builtin_ed_accept(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    ask(shell, mooring, "_ed-accept", EditorOp::Accept)
}

// ─── TUI ─────────────────────────────────────────────────────────────────────

/// Build the `[output: .., status: Int]` record returned by `_ed-tui`.
fn tui_result(output: Value, status: i64) -> Value {
    Value::map(vec![
        ("output".into(), output),
        ("status".into(), Value::Int(status)),
    ])
}

/// Decode captured stdout as lossy UTF-8, stripping a single trailing newline.
fn decode_captured(bytes: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(bytes).into_owned();
    if s.ends_with('\n') {
        s.pop();
    }
    s
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
/// trailing newline stripped — the call displays a terminal selection rather
/// than computing with it.
///
/// The pipeline foreground signal is a derived [`Mooring`]:
/// [`Mooring::lend_terminal`] raises `terminal_access` to `ExplicitLoan`,
/// which the pipeline foreground rule honors, keeping `_ed-tui`'s body in
/// the foreground process group despite the captured stdout pipe.  The
/// loaned mooring dies with the call — nothing to restore.  The loan exists
/// only in a run holding the terminal, once, so `_ed-tui` reports status 1
/// anywhere else or nested.
pub fn builtin_ed_tui(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    shell.check(Flag::Editor(EditorVerb::Tui), "_ed-tui")?;
    let Some(loaned) = mooring.lend_terminal() else {
        return Ok(tui_result(
            Value::string(
                "_ed-tui: this run holds no terminal to lend (a TUI opens from a keybinding handler, and not inside another)",
            ),
            1,
        ));
    };
    // A TUI plugin's own screen output, not a value the program binds: the
    // truncation marker is the whole report a 16 MiB draw deserves.
    let (result, bytes, _overflowed) = ral_core::evaluator::with_capture(shell, |shell| {
        ral_core::evaluator::apply(&args[0], Vec::new(), &loaned, shell)
    });
    match result {
        Ok(_) => Ok(tui_result(Value::string(decode_captured(&bytes)), 0)),
        Err(Break::Error(e)) => Ok(tui_result(
            Value::string(e.message.clone()),
            i64::from(e.code()),
        )),
        Err(other) => Err(other),
    }
}

// ─── Queries ─────────────────────────────────────────────────────────────────

/// `_ed-history <prefix> <limit>` — prefix search over history; `limit=0` for unbounded.
pub fn builtin_ed_history(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let prefix = args[0].as_str("_ed-history")?;
    let limit = match &args[1] {
        Value::Int(n) => offset(*n),
        _ => return Err(sig("_ed-history: limit must be Int")),
    };
    let history: Vec<String> = answer(shell, mooring, "_ed-history", EditorOp::History)?;
    let mut results: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in history {
        if !entry.starts_with(prefix) || !seen.insert(entry.clone()) {
            continue;
        }
        results.push(Value::string(entry));
        if limit > 0 && results.len() >= limit {
            break;
        }
    }
    Ok(Value::list(results))
}

/// `_ed-parse` → `[words: [Str], current: Int, offset: Int]` — the simple
/// command at the cursor.  `words` is empty exactly when the buffer does not lex.
pub fn builtin_ed_parse(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let EditorSnapshot { text, cursor, .. } = answer(shell, mooring, "_ed-parse", EditorOp::Get)?;
    Ok(parse_at(&text, cursor).unwrap_or_default().encode().into())
}

// ─── Output channels ─────────────────────────────────────────────────────────

/// `_ed-ghost <text>` — set ghost text (empty string clears).
pub fn builtin_ed_ghost(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
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
    args: &[Value],
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let uri = args[0].as_str("_ed-hyperlink")?;
    let text = args[1].as_str("_ed-hyperlink")?;
    let rendered = if shell.terminal().ui_hyperlinks_ok() {
        ral_core::io::ansi::osc8_link(uri, text)
    } else {
        text.to_owned()
    };
    Ok(Value::string(rendered))
}

/// `_ed-clipboard <text>` — copy `text` to the system clipboard.
///
/// Returns `Bool`: whether the whole text reached the clipboard, natively when
/// ral runs on this machine and else by OSC 52 (which also needs the
/// `ui_clipboard_write_ok` capability surfaced by `$TERMINAL`).  Gated on
/// `editor.write`.
pub fn builtin_ed_clipboard(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let req = ClipboardReq {
        text: args[0].as_str("_ed-clipboard")?.to_owned(),
        osc52: shell.terminal().ui_clipboard_write_ok(),
    };
    answer(shell, mooring, "_ed-clipboard", req).map(Value::Bool)
}

/// `_ed-highlight <spans>` — set highlight spans (empty list clears).
pub fn builtin_ed_highlight(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let spans = args[0]
        .as_list("_ed-highlight")?
        .iter()
        .map(|v| highlight_req(&v))
        .collect::<Settled<_>>()?;
    ask(shell, mooring, "_ed-highlight", EditorOp::Highlight(spans))
}

/// One `_ed-highlight` span: row-polymorphic, its style checked here.
fn highlight_req(v: &Value) -> Settled<HighlightReq> {
    let int_field = |v: &Value, field: &'static str| match v {
        Value::Int(n) => Ok(offset(*n)),
        _ => Err(sig(format!("highlight span: {field} must be Int"))),
    };
    let m = v.as_map("_ed-highlight span")?;
    let mut span = HighlightReq {
        start: 0,
        end: 0,
        style: String::new(),
    };
    for (k, v) in &m {
        match k {
            "start" => span.start = int_field(&v, "start")?,
            "end" => span.end = int_field(&v, "end")?,
            "style" => span.style = v.to_string(),
            _ => {}
        }
    }
    if style_ansi(&span.style).is_none() {
        return Err(sig(format!(
            "_ed-highlight: unknown style '{}'",
            span.style
        )));
    }
    Ok(span)
}

// ─── Plugin-local state ──────────────────────────────────────────────────────

/// `_ed-state <default> <updater>` — read-modify-write on the plugin's
/// persistent cell.  `default` is used on first call; `updater` is invoked
/// with the current value and its return becomes the new value, which the
/// host keeps, so it must be data.
///
/// A boundary: the cell persists across sessions, so the value the host hands
/// back was decided by whichever program last wrote it, and is admitted against
/// this call's type before the updater runs.  The updater's own result was
/// produced by typed code and is stored as it is.
pub fn builtin_ed_state(
    args: &[Value],
    site: &Arc<Site>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    // Write is checked before the updater runs, not after it.
    shell.check(Flag::Editor(EditorVerb::Write), "_ed-state")?;
    let current = match answer::<Option<Data>>(shell, mooring, "_ed-state", EditorOp::StateGet)? {
        Some(Data(stored)) => {
            let stored = Value::from(stored);
            // The cell outlives every session, so what it holds was decided by
            // whichever program last wrote it.
            site.admit(&stored, shell.fixings()).map_err(|mismatch| {
                mismatch.refusal("_ed-state (the plugin's persistent cell)", shell)
            })?;
            stored
        }
        None => args[0].clone(),
    };
    let new_val = ral_core::evaluator::apply(&args[1], vec![current], mooring, shell)?;
    let data = FOValue::try_from(&new_val).map_err(|e| {
        sig(format!(
            "_ed-state: the updater returned {}, but the state cell holds only data",
            e.leaf
        ))
    })?;
    ask(shell, mooring, "_ed-state", EditorOp::StateSet(Data(data)))?;
    Ok(new_val)
}

// ─── Host registration ───────────────────────────────────────────────────────
//
// Every facet that `ral_core::builtins` exposes is carried by the entry
// that owns the call function.  This is a static host extension: plugins
// remain dynamic source/alias/hook loaders above this surface.

fn scheme_ed_get(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], thunk(pure(EditorSnapshot::ty())))
}

fn scheme_unit_thunk(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], thunk(pure(Ty::Unit)))
}

fn scheme_ed_set(_u: &mut Unifier) -> Scheme {
    let field = |ty| closed_variant(&[("keep", Ty::Unit), ("set", ty)]);
    let record = closed_record(&[("text", field(Ty::String)), ("cursor", field(Ty::Int))]);
    scheme(&[], &[], thunk(fun(record, pure(Ty::Unit))))
}

fn scheme_string_to_bool(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], thunk(fun(Ty::String, pure(Ty::Bool))))
}

fn scheme_string_string_to_string(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        thunk(fun(Ty::String, fun(Ty::String, pure(Ty::String)))),
    )
}

fn scheme_highlight(u: &mut Unifier) -> Scheme {
    let rest = u.fresh_row_var();
    let span = open_record(
        &[("start", Ty::Int), ("end", Ty::Int), ("style", Ty::String)],
        rest,
    );
    scheme(
        &[],
        &[(rest, false)],
        thunk(fun(Ty::list(span), pure(Ty::Unit))),
    )
}

fn scheme_history(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        thunk(fun(Ty::String, fun(Ty::Int, pure(Ty::list(Ty::String))))),
    )
}

fn scheme_parse(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], thunk(pure(CommandAt::ty())))
}

/// `_ed-tui :: ∀ε α. U(F^ε α) → F [output: Str, status: Int]` — absorbs its
/// body, whatever it produces; the output is the record's.
fn scheme_tui(u: &mut Unifier) -> Scheme {
    let (av, ev) = (u.fresh_tyvar(), u.fresh_grade_var());
    let body = CompTy::Return(Grade::Var(ev), Box::new(Ty::Var(av)));
    graded(
        &[ev],
        scheme(
            &[(av, Kind::ANY)],
            &[],
            thunk(fun(
                thunk(body),
                pure(closed_record(&[
                    ("output", Ty::String),
                    ("status", Ty::Int),
                ])),
            )),
        ),
    )
}

fn scheme_state(u: &mut Unifier) -> Scheme {
    let av = u.fresh_tyvar();
    let a = Ty::Var(av);
    scheme(
        &[(av, Kind::DATA)],
        &[],
        thunk(fun(
            a.clone(),
            fun(thunk(fun(a.clone(), pure(a.clone()))), pure(a)),
        )),
    )
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

    /// Every `_ed-*` entry must carry all static facets directly.
    #[test]
    fn every_ed_name_has_all_facets() {
        for entry in ED_BUILTINS {
            assert!(!entry.decl.name.is_empty());
            assert_eq!(
                entry.decl.convention,
                ral_core::types::Convention::Value,
                "the editor surface is applied, not an argv: {:?}",
                entry.decl.name
            );
            assert!(
                !entry.decl.doc.is_empty(),
                "no doc for {:?}",
                entry.decl.name
            );
        }
    }

    /// The perimeter test's plugin half: a row whose result only its own call
    /// determines is a boundary, and `_ed-state` is the editor surface's one.
    #[test]
    fn a_result_only_variable_in_the_editor_surface_is_a_boundary() {
        for entry in ED_BUILTINS.iter().chain(super::super::load::BUILTINS) {
            let scheme = ral_core::test_access::builtin_scheme(entry, &mut Unifier::new());
            assert!(
                !ral_core::test_access::has_result_only_var(&scheme) || entry.decl.is_boundary(),
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

    impl ral_core::types::EnquiryDesk for Cell {
        fn enquire(
            &self,
            req: FOValue,
            _scope: &ral_core::process::Scope,
        ) -> ral_core::types::Settled<FOValue> {
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
    fn run_with_cell(stored: FOValue, run: Run) -> (Result<Value, Break>, usize) {
        let cell = std::sync::Arc::new(Cell {
            stored,
            asked: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut runtime = ral_core::test_helper::core_runtime();
        runtime.install_builtins(ED_BUILTINS);
        let mut shell = ral_core::Shell::root(runtime.into(), ral_core::test_helper::process_cwd());
        shell.set_interactive(true);
        let report = shell.run(ral_core::run::RunRequest {
            desk: Some(cell.clone()),
            ..ral_core::run::RunRequest::from(run)
        });
        let ral_core::run::RunReport::Ran { ending, .. } = report else {
            panic!("the line must check and run");
        };
        (
            ending.into_result(),
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
            panic!("a stale cell must be refused, got {result:?}");
        };
        assert!(
            e.message
                .contains("_ed-state (the plugin's persistent cell)")
                && e.message.contains("the value is text (`'stale'`)"),
            "{}",
            e.message
        );
        assert_eq!(asked, 1, "the read only: the updater must not have run");
    }

    #[test]
    fn a_state_cell_of_the_right_type_reaches_the_updater() {
        let (result, asked) = run_with_cell(FOValue::Int { value: 4 }, line_run(BUMP));
        assert!(matches!(result, Ok(Value::Int(5))), "{result:?}");
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
            e.message.contains("_ed-state requires editor: [`write]"),
            "{}",
            e.message
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
        let Ok(Value::Map(m)) = result else {
            panic!("_ed-tui reports rather than raises, got {result:?}");
        };
        assert!(matches!(m.get("status").as_deref(), Some(Value::Int(1))));
        assert!(
            m.get("output")
                .is_some_and(|o| o.to_string().contains("holds no terminal to lend"))
        );
        assert_eq!(asked, 0);
    }

    /// A non-Int cursor is refused by the builtin, before any request is put.
    #[test]
    fn ed_set_rejects_non_int_cursor() {
        let arg = Value::map(vec![
            (
                "text".into(),
                Value::variant("set", Some(Value::string("new"))),
            ),
            (
                "cursor".into(),
                Value::variant("set", Some(Value::string("3"))),
            ),
        ]);
        assert!(set_op(&arg).is_err());
    }

    /// A negative cursor floors at zero; the host clamps the rest.
    #[test]
    fn ed_set_floors_a_negative_cursor() {
        let arg = Value::map(vec![
            ("text".into(), Value::variant("keep", None)),
            ("cursor".into(), Value::variant("set", Some(Value::Int(-4)))),
        ]);
        assert!(matches!(
            set_op(&arg),
            Ok(EditorOp::Set {
                text: None,
                cursor: Some(0)
            })
        ));
    }
}
