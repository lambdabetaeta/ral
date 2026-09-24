//! Editor builtins — line editor interface exposed to plugin handlers.
//!
//! Each op (`_ed-get`, `_ed-set`, `_ed-push`, …) is its own builtin so the
//! type checker sees the actual return type, arity is fixed per op, and the
//! `_` prefix hides them from `help`.  Each is a thin engine-side door: it
//! keeps its capability and argument checks, and puts the rest to the host
//! as a `` `repl-editor `` enquiry, which the host answers only while an
//! editor context is installed for the dispatch — inside a plugin handler.

use ral_core::builtins::util::arg0_str;
use ral_core::serial::FOValue;
use ral_core::serial::datum::Datum;
use ral_core::source::Span as ByteSpan;
use ral_core::syntax::lexer::{Token, lex};
use ral_core::typecheck::builtins::{
    closed_record, fun, mk_scheme as scheme, open_record, pure, thunk,
};
use ral_core::typecheck::{CompTy, PayloadRoute, Scheme, Ty, Unifier};
use ral_core::types::as_list;
use ral_core::types::{Break, BuiltinBody, BuiltinEntry, Mooring, Settled, as_map, sig};
use ral_core::{Shell, Value};
use std::borrow::Cow;

use super::super::enquiry::{Data, EditorOp, EditorSnapshot, Enquiry, HighlightReq};
use super::super::highlight_style::style_ansi;
use super::editor::split_at_cursor;
use ral_core::text::{byte_to_char, char_to_byte};

/// Put `op` to the host's editor context.
fn ask(shell: &Shell, mooring: &Mooring, op: EditorOp) -> Settled<FOValue> {
    Ok(shell.enquire(mooring, Enquiry::Editor(op).encode())?)
}

/// `op`'s answer, decoded as a `T`.
fn answer<T: Datum>(shell: &Shell, mooring: &Mooring, op: EditorOp) -> Settled<T> {
    T::decode(&ask(shell, mooring, op)?).map_err(|why| {
        sig(format!(
            "editor op: the host answered outside its shape: {why}"
        ))
    })
}

fn snapshot(shell: &Shell, mooring: &Mooring) -> Settled<EditorSnapshot> {
    answer(shell, mooring, EditorOp::Get)
}

/// Write `op`, answering `()`.
fn write(shell: &Shell, mooring: &Mooring, op: EditorOp) -> Settled<Value> {
    ask(shell, mooring, op).map(|_| Value::Unit)
}

fn require_interactive(name: &str, shell: &Shell) -> Settled<()> {
    if !shell.is_interactive() {
        return Err(sig(format!(
            "{name}: not available outside interactive mode"
        )));
    }
    Ok(())
}

#[allow(
    clippy::cast_possible_wrap,
    reason = "editor cursor is a buffer char offset, far below i64::MAX"
)]
fn int(n: usize) -> Value {
    Value::Int(n as i64)
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
    require_interactive("_ed-get", shell)?;
    shell.check_editor_read("get")?;
    let st = snapshot(shell, mooring)?;
    Ok(Value::map(vec![
        ("text".into(), Value::string(st.text)),
        ("cursor".into(), int(st.cursor)),
        ("keymap".into(), Value::string(st.keymap)),
    ]))
}

/// `_ed-text` → `Str` — current buffer text.
pub fn builtin_ed_text(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-text", shell)?;
    shell.check_editor_read("text")?;
    Ok(Value::string(snapshot(shell, mooring)?.text))
}

/// `_ed-cursor` → `Int` — current cursor offset (chars).
pub fn builtin_ed_cursor(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-cursor", shell)?;
    shell.check_editor_read("cursor")?;
    Ok(int(snapshot(shell, mooring)?.cursor))
}

/// `_ed-keymap` → `Str` — current keymap name.
pub fn builtin_ed_keymap(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-keymap", shell)?;
    shell.check_editor_read("keymap")?;
    Ok(Value::string(snapshot(shell, mooring)?.keymap))
}

/// `_ed-lbuffer` → `Str` — text to the left of the cursor.
pub fn builtin_ed_lbuffer(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-lbuffer", shell)?;
    shell.check_editor_read("lbuffer")?;
    let st = snapshot(shell, mooring)?;
    Ok(Value::string(split_at_cursor(&st.text, st.cursor).0))
}

// ─── State write ─────────────────────────────────────────────────────────────

/// `_ed-set`'s request: row-polymorphic, so unknown fields are ignored.
fn set_op(arg: &Value) -> Settled<EditorOp> {
    let map = as_map(arg, "_ed-set")?;
    let cursor = match map.get("cursor") {
        Some(Value::Int(n)) => Some(offset(*n)),
        Some(_) => return Err(sig("_ed-set: cursor must be Int")),
        None => None,
    };
    Ok(EditorOp::Set {
        text: map.get("text").map(std::string::ToString::to_string),
        cursor,
    })
}

/// `_ed-set [text?: Str, cursor?: Int]` — row-polymorphic partial write.
pub fn builtin_ed_set(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-set", shell)?;
    shell.check_editor_write("set")?;
    write(shell, mooring, set_op(&args[0])?)
}

/// `_ed-set-lbuffer <l>` — replace text left of cursor; right side preserved.
pub fn builtin_ed_set_lbuffer(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    require_interactive("_ed-set-lbuffer", shell)?;
    shell.check_editor_write("set-lbuffer")?;
    write(shell, mooring, EditorOp::SetLbuffer(args[0].to_string()))
}

/// `_ed-insert <str>` — insert at cursor; cursor advances to end of insertion.
pub fn builtin_ed_insert(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-insert", shell)?;
    shell.check_editor_write("insert")?;
    write(shell, mooring, EditorOp::Insert(args[0].to_string()))
}

/// `_ed-push` — save buffer to stack, clear.
pub fn builtin_ed_push(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-push", shell)?;
    shell.check_editor_write("push")?;
    write(shell, mooring, EditorOp::Push)
}

/// `_ed-accept` — mark buffer for immediate execution.
pub fn builtin_ed_accept(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-accept", shell)?;
    shell.check_editor_write("accept")?;
    write(shell, mooring, EditorOp::Accept)
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
/// On success: `status: 0`, `output: <body's return value or captured stdout>`.
/// On error  : `status: <error exit code>`, `output: <error message>`.
///
/// The body's stdout is captured so that a TUI command (e.g. `fzf`) which
/// prints its selection on stdout can have that selection delivered back to
/// the plugin as a String.  The TUI itself draws on /dev/tty via stderr, so
/// capturing stdout does not disrupt the interface.  When the body returns a
/// non-Unit value it wins; otherwise the captured bytes are decoded
/// (trailing newline stripped).
///
/// The pipeline foreground signal is a derived [`Mooring`]:
/// [`Mooring::lend_terminal`] raises `terminal_access` to `ExplicitLoan`,
/// which the pipeline foreground rule honors, keeping `_ed-tui`'s body in
/// the foreground process group despite the captured stdout pipe.  The
/// loaned mooring dies with the call — nothing to restore.
/// [`Mooring::in_terminal_loan`] is the re-entrancy guard.
pub fn builtin_ed_tui(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-tui", shell)?;
    shell.check_editor_tui()?;
    if mooring.in_terminal_loan() {
        return Ok(tui_result(Value::string("_ed-tui: already in TUI mode"), 1));
    }
    if snapshot(shell, mooring)?.in_readline {
        return Ok(tui_result(
            Value::string("_ed-tui: not available inside buffer-change hooks"),
            1,
        ));
    }
    let loaned = mooring.lend_terminal();
    // A TUI plugin's own screen output, not a value the program binds: the
    // truncation marker is the whole report a 16 MiB draw deserves.
    let (result, bytes, _overflowed) = ral_core::evaluator::with_capture(shell, |shell| {
        ral_core::builtins::apply(&args[0], Vec::new(), &loaned, shell)
    });
    match result {
        Ok(v) => {
            let v = match v {
                Value::Unit => Value::string(decode_captured(&bytes)),
                Value::Bytes(b) => Value::string(decode_captured(&b)),
                other => other,
            };
            Ok(tui_result(v, 0))
        }
        Err(Break::Error(e)) => Ok(tui_result(
            Value::string(e.message.clone()),
            i64::from(e.exit_code()),
        )),
        Err(other) => Err(other),
    }
}

// ─── Queries ─────────────────────────────────────────────────────────────────

/// `_ed-history <prefix> <limit>` — prefix search over history; `limit=0` for unbounded.
pub fn builtin_ed_history(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-history", shell)?;
    shell.check_editor_read("history")?;
    let prefix = args[0].to_string();
    let limit = match &args[1] {
        Value::Int(n) => offset(*n),
        _ => return Err(sig("_ed-history: limit must be Int")),
    };
    let history: Vec<String> = answer(shell, mooring, EditorOp::History)?;
    let mut results: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in history {
        if !entry.starts_with(&prefix) || !seen.insert(entry.clone()) {
            continue;
        }
        results.push(Value::string(entry));
        if limit > 0 && results.len() >= limit {
            break;
        }
    }
    Ok(Value::list(results))
}

/// True for token kinds that carry word-like content (command names,
/// arguments, variable references) as opposed to pure syntax (pipes,
/// braces, separators) that only delimit them.
fn is_word_token(tok: &Token) -> bool {
    matches!(
        tok,
        Token::Word(_)
            | Token::SingleQuoted(_)
            | Token::DoubleQuoted(_)
            | Token::Tag(_)
            | Token::Variable(_)
            | Token::Expr(_)
    )
}

/// The text of a word-bearing token.  Single-quoted bodies come straight
/// from the token (already unescaped, hash-bumping and all); every other
/// kind is read back out of `text` via the token's own byte span, stripping
/// the surrounding quotes for double-quoted strings.
///
/// Escapes are asymmetric between the two quote styles: a single-quoted body
/// is the token's unescaped value, but a double-quoted body is returned raw
/// from the source span — its `\n`, `\"`, `$…` escapes verbatim, not
/// interpreted.  Plugins tokenizing the buffer see double-quoted text exactly
/// as typed; unescaping it is theirs to do if they need the runtime value.
fn word_text(text: &str, tok: &Token, span: ByteSpan) -> String {
    if let Token::SingleQuoted(s) = tok {
        return s.clone();
    }
    let start = span.start as usize;
    let end = span.end as usize;
    match tok {
        Token::DoubleQuoted(_) => text[start + 1..end.saturating_sub(1).max(start + 1)].to_string(),
        _ => text[start..end].to_string(),
    }
}

/// `_ed-parse` → `[words: [Str], current: Int, offset: Int]` — tokenize buffer at cursor.
pub fn builtin_ed_parse(_args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-parse", shell)?;
    shell.check_editor_read("parse")?;
    let EditorSnapshot { text, cursor, .. } = snapshot(shell, mooring)?;

    let empty = || {
        Value::map(vec![
            ("words".into(), Value::list(vec![])),
            ("current".into(), Value::Int(0)),
            ("offset".into(), Value::Int(0)),
        ])
    };

    if text.is_empty() {
        return Ok(empty());
    }

    // A buffer that doesn't lex is mid-typing (an open quote, an open
    // brace, …) rather than a well-formed command line; there is nothing
    // sound to tokenize yet, so report no words rather than guessing.
    let Ok(tokens) = lex(&text) else {
        return Ok(empty());
    };

    let words: Vec<(usize, String)> = tokens
        .iter()
        .filter(|(tok, _)| is_word_token(tok))
        .map(|(tok, span)| (span.start as usize, word_text(&text, tok, *span)))
        .collect();

    if words.is_empty() {
        return Ok(empty());
    }

    // Determine which word the cursor is in/after.
    let cursor_byte = char_to_byte(&text, cursor);

    let mut current = 0usize;
    let mut offset = 0usize;
    for (idx, (word_start, _)) in words.iter().enumerate() {
        if *word_start <= cursor_byte {
            current = idx;
            offset = *word_start;
        }
    }

    let offset_chars = byte_to_char(&text, offset);

    let word_values: Vec<Value> = words.into_iter().map(|(_, w)| Value::string(w)).collect();

    #[allow(clippy::cast_possible_wrap, reason = "word index, far below i64::MAX")]
    let current_i = current as i64;
    #[allow(clippy::cast_possible_wrap, reason = "word index, far below i64::MAX")]
    let offset_i = offset_chars as i64;
    Ok(Value::map(vec![
        ("words".into(), Value::list(word_values)),
        ("current".into(), Value::Int(current_i)),
        ("offset".into(), Value::Int(offset_i)),
    ]))
}

// ─── Output channels ─────────────────────────────────────────────────────────

/// `_ed-ghost <text>` — set ghost text (empty string clears).
pub fn builtin_ed_ghost(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-ghost", shell)?;
    shell.check_editor_write("ghost")?;
    write(shell, mooring, EditorOp::Ghost(arg0_str(args)))
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
    require_interactive("_ed-hyperlink", shell)?;
    let uri = args[0].to_string();
    let text = args[1].to_string();
    let rendered = if shell.terminal().ui_hyperlinks_ok() {
        ral_core::ansi::osc8_link(&uri, &text)
    } else {
        text
    };
    Ok(Value::string(rendered))
}

/// `_ed-clipboard <text>` — ask the host terminal to write `text` to the
/// system clipboard via OSC 52.
///
/// Returns `Bool`: `true` when the sequence was emitted, `false` when the
/// terminal isn't known to accept OSC 52 (so a plugin can fall back to
/// `pbcopy` / `xclip` / `wl-copy`).  Gated on `editor.write` and the
/// `ui_clipboard_write_ok` capability surfaced by `$TERMINAL`.
///
/// We emit directly to stdout because OSC 52 is zero-width: it neither
/// moves the cursor nor writes visible bytes, so it does not corrupt the
/// active rustyline display.  This matches the `write_terminal_title`
/// precedent.  IO errors are swallowed — a failed copy is recoverable
/// and shouldn't tear down the plugin handler.
pub fn builtin_ed_clipboard(
    args: &[Value],
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    require_interactive("_ed-clipboard", shell)?;
    shell.check_editor_write("clipboard")?;

    if !shell.terminal().ui_clipboard_write_ok() {
        return Ok(Value::Bool(false));
    }

    use base64::Engine;
    use std::io::Write;
    let payload = base64::engine::general_purpose::STANDARD.encode(arg0_str(args).as_bytes());
    let sequence = ral_core::ansi::osc52_copy(&payload);
    let _ = std::io::stdout().write_all(sequence.as_bytes());
    let _ = std::io::stdout().flush();
    Ok(Value::Bool(true))
}

/// `_ed-highlight <spans>` — set highlight spans (empty list clears).
pub fn builtin_ed_highlight(
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    require_interactive("_ed-highlight", shell)?;
    shell.check_editor_write("highlight")?;
    let spans = as_list(&args[0], "_ed-highlight")?
        .iter()
        .map(highlight_req)
        .collect::<Settled<_>>()?;
    write(shell, mooring, EditorOp::Highlight(spans))
}

/// One `_ed-highlight` span: row-polymorphic, its style checked here.
fn highlight_req(v: &Value) -> Settled<HighlightReq> {
    let int_field = |v: &Value, field: &'static str| match v {
        Value::Int(n) => Ok(offset(*n)),
        _ => Err(sig(format!("highlight span: {field} must be Int"))),
    };
    let m = as_map(v, "_ed-highlight span")?;
    let mut span = HighlightReq {
        start: 0,
        end: 0,
        style: String::new(),
    };
    for (k, v) in &m {
        match k.as_str() {
            "start" => span.start = int_field(v, "start")?,
            "end" => span.end = int_field(v, "end")?,
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
pub fn builtin_ed_state(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-state", shell)?;
    shell.check_editor_write("state")?;
    let current = answer::<Option<Data>>(shell, mooring, EditorOp::StateGet)?
        .map_or_else(|| args[0].clone(), |Data(v)| Value::from(v));
    let new_val = ral_core::builtins::apply(&args[1], vec![current], mooring, shell)?;
    let data = FOValue::try_from(&new_val).map_err(|e| {
        sig(format!(
            "_ed-state: the updater returned {}, but the state cell holds only data",
            e.leaf
        ))
    })?;
    write(shell, mooring, EditorOp::StateSet(Data(data)))?;
    Ok(new_val)
}

// ─── Host registration ───────────────────────────────────────────────────────
//
// Every facet that `ral_core::builtins` exposes is carried by the entry
// that owns the call function.  This is a static host extension: plugins
// remain dynamic source/alias/hook loaders above this surface.

fn scheme_ed_get(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(pure(closed_record(&[
            ("text", Ty::String),
            ("cursor", Ty::Int),
            ("keymap", Ty::String),
        ]))),
    )
}

fn scheme_string_thunk(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(pure(Ty::String)))
}

fn scheme_int_thunk(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(pure(Ty::Int)))
}

fn scheme_unit_thunk(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(pure(Ty::Unit)))
}

fn scheme_ed_set(u: &mut Unifier) -> Scheme {
    let rho = u.fresh_row_var();
    let record = open_record(&[("text", Ty::String), ("cursor", Ty::Int)], rho);
    scheme(&[], &[], &[rho], thunk(fun(record, pure(Ty::Unit))))
}

fn scheme_string_to_bool(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(fun(Ty::String, pure(Ty::Bool))))
}

fn scheme_string_string_to_string(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(Ty::String, fun(Ty::String, pure(Ty::String)))),
    )
}

fn scheme_highlight(u: &mut Unifier) -> Scheme {
    let av = u.fresh_tyvar();
    scheme(
        &[av],
        &[],
        &[],
        thunk(fun(Ty::List(Box::new(Ty::Var(av))), pure(Ty::Unit))),
    )
}

fn scheme_history(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(fun(
            Ty::String,
            fun(Ty::Int, pure(Ty::List(Box::new(Ty::String)))),
        )),
    )
}

fn scheme_parse(_u: &mut Unifier) -> Scheme {
    scheme(
        &[],
        &[],
        &[],
        thunk(pure(closed_record(&[
            ("words", Ty::List(Box::new(Ty::String))),
            ("current", Ty::Int),
            ("offset", Ty::Int),
        ]))),
    )
}

fn scheme_tui(u: &mut Unifier) -> Scheme {
    let av = u.fresh_tyvar();
    let rv = u.fresh_routevar();
    scheme(
        &[av],
        &[rv],
        &[],
        thunk(fun(
            thunk(CompTy::Return(PayloadRoute::Var(rv), Box::new(Ty::Var(av)))),
            pure(closed_record(&[
                ("output", Ty::String),
                ("status", Ty::Int),
            ])),
        )),
    )
}

fn scheme_state(u: &mut Unifier) -> Scheme {
    let av = u.fresh_tyvar();
    let a = Ty::Var(av);
    scheme(
        &[av],
        &[],
        &[],
        thunk(fun(
            a.clone(),
            fun(thunk(fun(a.clone(), pure(a.clone()))), pure(a)),
        )),
    )
}

// A named array, not a promoted temporary: rustc refuses promotion once an
// entry carries `BuiltinEntry`'s interior-mutable arity cache.
static ED_BUILTINS_ARR: [BuiltinEntry; 18] = [
    BuiltinEntry::new(
        Cow::Borrowed("_ed-get"),
        scheme_ed_get,
        "_ed-get  — return editor state record [text, cursor, keymap].",
        BuiltinBody::Static(builtin_ed_get),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-text"),
        scheme_string_thunk,
        "_ed-text  — return current buffer text.",
        BuiltinBody::Static(builtin_ed_text),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-cursor"),
        scheme_int_thunk,
        "_ed-cursor  — return current cursor offset (chars).",
        BuiltinBody::Static(builtin_ed_cursor),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-keymap"),
        scheme_string_thunk,
        "_ed-keymap  — return current keymap name.",
        BuiltinBody::Static(builtin_ed_keymap),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-lbuffer"),
        scheme_string_thunk,
        "_ed-lbuffer  — return text to the left of the cursor.",
        BuiltinBody::Static(builtin_ed_lbuffer),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-set"),
        scheme_ed_set,
        "_ed-set <map>  — partial write of editor state (text and/or cursor); unknown fields ignored.",
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
        "_ed-parse  — tokenize buffer at cursor; returns [words, current, offset].",
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
        "_ed-clipboard <text>  — OSC 52 system-clipboard write; returns Bool (true on emit, false when host terminal can't).",
        BuiltinBody::Static(builtin_ed_clipboard),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-hyperlink"),
        scheme_string_string_to_string,
        "_ed-hyperlink <uri> <text>  — wrap text in OSC 8 hyperlink; returns plain text when terminal can't render hyperlinks.",
        BuiltinBody::Static(builtin_ed_hyperlink),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-state"),
        scheme_state,
        "_ed-state <default> <updater>  — read-modify-write the plugin's persistent cell.",
        BuiltinBody::Static(builtin_ed_state),
    ),
];

/// Builtins installed into the REPL's own shell at startup
/// (see [`super::super::session::Session::boot`]).
pub static ED_BUILTINS: &[BuiltinEntry] = &ED_BUILTINS_ARR;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `_ed-*` entry must carry all static facets directly.
    #[test]
    fn every_ed_name_has_all_facets() {
        for entry in ED_BUILTINS {
            assert!(!entry.name.is_empty());
            assert_eq!(
                entry.convention,
                ral_core::types::Convention::Value,
                "the editor surface is applied, not an argv: {:?}",
                entry.name
            );
            assert!(!entry.doc.is_empty(), "no doc for {:?}", entry.name);
        }
    }

    /// A non-Int cursor is refused at the door, before any request is put.
    #[test]
    fn ed_set_rejects_non_int_cursor() {
        let arg = Value::map(vec![
            ("text".into(), Value::string("new")),
            ("cursor".into(), Value::string("3")),
        ]);
        assert!(set_op(&arg).is_err());
    }

    /// A negative cursor floors at zero; the host clamps the rest.
    #[test]
    fn ed_set_floors_a_negative_cursor() {
        let arg = Value::map(vec![("cursor".into(), Value::Int(-4))]);
        assert!(matches!(
            set_op(&arg),
            Ok(EditorOp::Set {
                text: None,
                cursor: Some(0)
            })
        ));
    }
}
