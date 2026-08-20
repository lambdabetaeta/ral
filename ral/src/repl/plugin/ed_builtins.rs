//! Editor builtins — line editor interface exposed to plugin handlers.
//!
//! Each op (`_ed-get`, `_ed-set`, `_ed-push`, …) is its own builtin so the
//! type checker sees the actual return type, arity is fixed per op, and the
//! `_` prefix hides them from `help`.  Every op requires an active
//! [`PluginContext`], set up by the REPL before dispatching into a plugin
//! handler — outside a handler every op fails with a "no plugin context"
//! error.
//!
//! These builtins live in the `ral` crate rather than in `ral-core`
//! because the editor surface is purely a host concern: core has no
//! knowledge of editor state or plugin handlers.  [`ED_BUILTINS`] is
//! installed into the REPL's own shell at startup
//! (see [`super::super::session::Session::boot`]).

use ral_core::builtins::util::arg0_str;
use ral_core::source::Span as ByteSpan;
use ral_core::syntax::lexer::{Token, lex};
use ral_core::typecheck::builtins::{
    BuiltinTypeRule, closed_record, fun, mk_scheme as scheme, pure, thunk,
};
use ral_core::typecheck::{CompTy, PayloadRoute, Row, Scheme, Ty, Unifier};
use ral_core::types::as_list;
use ral_core::types::{Break, BuiltinBody, BuiltinEntry, Mooring, Settled, as_map, sig};
use ral_core::{Shell, Value};
use std::borrow::Cow;

use super::super::highlight_style::style_ansi;
use super::editor::{HighlightSpan, PluginContext, Span};
use ral_core::text::{byte_to_char, char_to_byte};

fn ctx(shell: &Shell) -> Settled<&PluginContext> {
    shell
        .repl()
        .plugin_context
        .as_ref()
        .and_then(|b| b.downcast_ref::<PluginContext>())
        .ok_or_else(|| sig("editor op: no plugin context (not inside a plugin handler)"))
}

fn ctx_mut(shell: &mut Shell) -> Settled<&mut PluginContext> {
    shell
        .repl_mut()
        .plugin_context
        .as_mut()
        .and_then(|b| b.downcast_mut::<PluginContext>())
        .ok_or_else(|| sig("editor op: no plugin context (not inside a plugin handler)"))
}

fn require_interactive(name: &str, shell: &Shell) -> Settled<()> {
    if !shell.is_interactive() {
        return Err(sig(format!(
            "{name}: not available outside interactive mode"
        )));
    }
    Ok(())
}

/// Split `text` at a character-offset `cursor` into the substrings left and
/// right of it.
fn split_at_cursor(text: &str, cursor: usize) -> (String, String) {
    let left: String = text.chars().take(cursor).collect();
    let right: String = text.chars().skip(cursor).collect();
    (left, right)
}

// ─── State read ──────────────────────────────────────────────────────────────

/// `_ed-get` → `[text: Str, cursor: Int, keymap: Str]`
pub fn builtin_ed_get(_args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-get", shell)?;
    shell.check_editor_read("get")?;
    let pc = ctx(shell)?;
    #[allow(
        clippy::cast_possible_wrap,
        reason = "editor cursor is a buffer char offset, far below i64::MAX"
    )]
    let cursor = pc.editor_state.cursor as i64;
    Ok(Value::map(vec![
        ("text".into(), Value::String(pc.editor_state.text.clone())),
        ("cursor".into(), Value::Int(cursor)),
        (
            "keymap".into(),
            Value::String(pc.editor_state.keymap.clone()),
        ),
    ]))
}

/// `_ed-text` → `Str` — current buffer text.
pub fn builtin_ed_text(_args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-text", shell)?;
    shell.check_editor_read("text")?;
    let pc = ctx(shell)?;
    Ok(Value::String(pc.editor_state.text.clone()))
}

/// `_ed-cursor` → `Int` — current cursor offset (chars).
pub fn builtin_ed_cursor(_args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-cursor", shell)?;
    shell.check_editor_read("cursor")?;
    let pc = ctx(shell)?;
    #[allow(
        clippy::cast_possible_wrap,
        reason = "editor cursor is a buffer char offset, far below i64::MAX"
    )]
    let cursor = pc.editor_state.cursor as i64;
    Ok(Value::Int(cursor))
}

/// `_ed-keymap` → `Str` — current keymap name.
pub fn builtin_ed_keymap(_args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-keymap", shell)?;
    shell.check_editor_read("keymap")?;
    let pc = ctx(shell)?;
    Ok(Value::String(pc.editor_state.keymap.clone()))
}

/// `_ed-lbuffer` → `Str` — text to the left of the cursor.
pub fn builtin_ed_lbuffer(
    _args: &[Value],
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    require_interactive("_ed-lbuffer", shell)?;
    shell.check_editor_read("lbuffer")?;
    let pc = ctx(shell)?;
    let (left, _) = split_at_cursor(&pc.editor_state.text, pc.editor_state.cursor);
    Ok(Value::String(left))
}

// ─── State write ─────────────────────────────────────────────────────────────

/// `_ed-set [text?: Str, cursor?: Int]` — row-polymorphic partial write.
/// Unknown fields are ignored.
pub fn builtin_ed_set(args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-set", shell)?;
    shell.check_editor_write("set")?;
    let map = as_map(&args[0], "_ed-set")?;
    let text = map.get("text").map(std::string::ToString::to_string);
    let cursor = match map.get("cursor") {
        Some(Value::Int(n)) => Some(*n),
        Some(_) => return Err(sig("_ed-set: cursor must be Int")),
        None => None,
    };
    let pc = ctx_mut(shell)?;
    if let Some(text) = text {
        pc.editor_state.text = text;
    }
    if let Some(n) = cursor {
        #[allow(
            clippy::cast_possible_wrap,
            reason = "buffer char count is far below i64::MAX"
        )]
        let max = pc.editor_state.text.chars().count() as i64;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to [0, char_count]; non-negative and within buffer length"
        )]
        let cursor = n.clamp(0, max) as usize;
        pc.editor_state.cursor = cursor;
    }
    Ok(Value::Unit)
}

/// `_ed-set-lbuffer <l>` — replace text left of cursor; right side preserved.
pub fn builtin_ed_set_lbuffer(
    args: &[Value],
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    require_interactive("_ed-set-lbuffer", shell)?;
    shell.check_editor_write("set-lbuffer")?;
    let l = args[0].to_string();
    let pc = ctx_mut(shell)?;
    let (_, right) = split_at_cursor(&pc.editor_state.text, pc.editor_state.cursor);
    let new_cursor = l.chars().count();
    pc.editor_state.text = format!("{l}{right}");
    pc.editor_state.cursor = new_cursor;
    Ok(Value::Unit)
}

/// `_ed-insert <str>` — insert at cursor; cursor advances to end of insertion.
pub fn builtin_ed_insert(args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-insert", shell)?;
    shell.check_editor_write("insert")?;
    let s = args[0].to_string();
    let pc = ctx_mut(shell)?;
    let cursor = pc.editor_state.cursor;
    let (left, right) = split_at_cursor(&pc.editor_state.text, cursor);
    let s_chars = s.chars().count();
    pc.editor_state.text = format!("{left}{s}{right}");
    pc.editor_state.cursor = cursor + s_chars;
    Ok(Value::Unit)
}

/// `_ed-push` — save buffer to stack, clear.
pub fn builtin_ed_push(_args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-push", shell)?;
    shell.check_editor_write("push")?;
    let pc = ctx_mut(shell)?;
    let text = std::mem::take(&mut pc.editor_state.text);
    let cursor = pc.editor_state.cursor;
    pc.editor_state.cursor = 0;
    pc.outputs.pushed_buffer = Some((text, cursor));
    Ok(Value::Unit)
}

/// `_ed-accept` — mark buffer for immediate execution.
pub fn builtin_ed_accept(_args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-accept", shell)?;
    shell.check_editor_write("accept")?;
    let pc = ctx_mut(shell)?;
    pc.outputs.accept_line = true;
    Ok(Value::Unit)
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
        return Ok(tui_result(
            Value::String("_ed-tui: already in TUI mode".into()),
            1,
        ));
    }
    {
        let pc = ctx(shell)?;
        if pc.inputs.in_readline {
            return Ok(tui_result(
                Value::String("_ed-tui: not available inside buffer-change hooks".into()),
                1,
            ));
        }
    }
    let loaned = mooring.lend_terminal();
    // A TUI plugin's own screen output, not a value the program binds: the
    // truncation marker is the whole report a 16 MiB draw deserves.
    let (result, bytes, _overflowed) = ral_core::evaluator::with_capture(shell, |shell| {
        ral_core::builtins::apply(&args[0], &[], &loaned, shell)
    });
    match result {
        Ok(v) => {
            let v = match v {
                Value::Unit => Value::String(decode_captured(&bytes)),
                Value::Bytes(b) => Value::String(decode_captured(&b)),
                other => other,
            };
            Ok(tui_result(v, 0))
        }
        Err(Break::Error(e)) => Ok(tui_result(
            Value::String(e.message.clone()),
            i64::from(e.exit_code()),
        )),
        Err(other) => Err(other),
    }
}

// ─── Queries ─────────────────────────────────────────────────────────────────

/// `_ed-history <prefix> <limit>` — prefix search over history; `limit=0` for unbounded.
pub fn builtin_ed_history(args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-history", shell)?;
    shell.check_editor_read("history")?;
    let prefix = args[0].to_string();
    let limit = match &args[1] {
        #[allow(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "negatives floored to 0 (the unbounded sentinel); count is far below usize::MAX"
        )]
        Value::Int(n) => (*n).max(0) as usize,
        _ => return Err(sig("_ed-history: limit must be Int")),
    };
    let pc = ctx(shell)?;
    let mut results: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in &pc.inputs.history_entries {
        if !prefix.is_empty() && !entry.starts_with(&prefix) {
            continue;
        }
        if seen.insert(entry.clone()) {
            results.push(Value::String(entry.clone()));
            if limit > 0 && results.len() >= limit {
                break;
            }
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
            | Token::Deref(_)
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
pub fn builtin_ed_parse(_args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-parse", shell)?;
    shell.check_editor_read("parse")?;
    let pc = ctx(shell)?;
    let text = &pc.editor_state.text;
    let cursor = pc.editor_state.cursor;

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
    let Ok(tokens) = lex(text) else {
        return Ok(empty());
    };

    let words: Vec<(usize, String)> = tokens
        .iter()
        .filter(|(tok, _)| is_word_token(tok))
        .map(|(tok, span)| (span.start as usize, word_text(text, tok, *span)))
        .collect();

    if words.is_empty() {
        return Ok(empty());
    }

    // Determine which word the cursor is in/after.
    let cursor_byte = char_to_byte(text, cursor);

    let mut current = 0usize;
    let mut offset = 0usize;
    for (idx, (word_start, _)) in words.iter().enumerate() {
        if *word_start <= cursor_byte {
            current = idx;
            offset = *word_start;
        }
    }

    let offset_chars = byte_to_char(text, offset);

    let word_values: Vec<Value> = words.into_iter().map(|(_, w)| Value::String(w)).collect();

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
pub fn builtin_ed_ghost(args: &[Value], _mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-ghost", shell)?;
    shell.check_editor_write("ghost")?;
    let text = arg0_str(args);
    let pc = ctx_mut(shell)?;
    pc.outputs.ghost_text = (!text.is_empty()).then_some(text);
    Ok(Value::Unit)
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
    Ok(Value::String(rendered))
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
    _mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    require_interactive("_ed-highlight", shell)?;
    shell.check_editor_write("highlight")?;
    let spans_val = as_list(&args[0], "_ed-highlight")?;
    if spans_val.is_empty() {
        ctx_mut(shell)?.outputs.highlight_spans.clear();
        return Ok(Value::Unit);
    }
    let text_len = ctx(shell)?.editor_state.text.chars().count();
    let int_field = |v: &Value, field: &'static str| match v {
        Value::Int(n) => Ok(*n),
        _ => Err(sig(format!("highlight span: {field} must be Int"))),
    };
    let mut spans = Vec::with_capacity(spans_val.len());
    for sv in &spans_val {
        let m = as_map(sv, "_ed-highlight span")?;
        let mut start: i64 = 0;
        let mut end: i64 = 0;
        let mut style = String::new();
        for (k, v) in &m {
            match k.as_str() {
                "start" => start = int_field(v, "start")?,
                "end" => end = int_field(v, "end")?,
                "style" => style = v.to_string(),
                _ => {} // row polymorphism
            }
        }
        if style_ansi(&style).is_none() {
            return Err(sig(format!("_ed-highlight: unknown style '{style}'")));
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "floored to 0 then clamped to text_len by Span::clamped; char offsets far below usize::MAX"
        )]
        let (lo, hi) = (start.max(0) as usize, end.max(0) as usize);
        spans.push(HighlightSpan {
            span: Span::clamped(lo, hi, text_len),
            style,
        });
    }
    ctx_mut(shell)?.outputs.highlight_spans = spans;
    Ok(Value::Unit)
}

// ─── Plugin-local state ──────────────────────────────────────────────────────

/// `_ed-state <default> <updater>` — read-modify-write on the plugin's
/// persistent cell.  `default` is used on first call; `updater` is invoked
/// with the current value and its return becomes the new value.
pub fn builtin_ed_state(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    require_interactive("_ed-state", shell)?;
    shell.check_editor_write("state")?;
    let default = &args[0];
    let updater = &args[1];
    let current = {
        let pc = ctx(shell)?;
        if pc.state_default_used {
            pc.state_cell.clone().unwrap_or_else(|| default.clone())
        } else {
            default.clone()
        }
    };
    let new_val = ral_core::builtins::apply(updater, &[current], mooring, shell)?;
    let pc = ctx_mut(shell)?;
    pc.state_cell = Some(new_val.clone());
    pc.state_default_used = true;
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
    let record = Ty::Record(Row::Extend(
        "text".into(),
        Box::new(Ty::String),
        Box::new(Row::Extend(
            "cursor".into(),
            Box::new(Ty::Int),
            Box::new(Row::Var(rho)),
        )),
    ));
    scheme(&[], &[], &[rho], thunk(fun(record, pure(Ty::Unit))))
}

fn scheme_string_to_unit(_u: &mut Unifier) -> Scheme {
    scheme(&[], &[], &[], thunk(fun(Ty::String, pure(Ty::Unit))))
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
        BuiltinTypeRule::Scheme(scheme_ed_get),
        "_ed-get  — return editor state record [text, cursor, keymap].",
        BuiltinBody::Static(builtin_ed_get),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-text"),
        BuiltinTypeRule::Scheme(scheme_string_thunk),
        "_ed-text  — return current buffer text.",
        BuiltinBody::Static(builtin_ed_text),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-cursor"),
        BuiltinTypeRule::Scheme(scheme_int_thunk),
        "_ed-cursor  — return current cursor offset (chars).",
        BuiltinBody::Static(builtin_ed_cursor),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-keymap"),
        BuiltinTypeRule::Scheme(scheme_string_thunk),
        "_ed-keymap  — return current keymap name.",
        BuiltinBody::Static(builtin_ed_keymap),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-lbuffer"),
        BuiltinTypeRule::Scheme(scheme_string_thunk),
        "_ed-lbuffer  — return text to the left of the cursor.",
        BuiltinBody::Static(builtin_ed_lbuffer),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-set"),
        BuiltinTypeRule::Scheme(scheme_ed_set),
        "_ed-set <map>  — partial write of editor state (text and/or cursor); unknown fields ignored.",
        BuiltinBody::Static(builtin_ed_set),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-set-lbuffer"),
        BuiltinTypeRule::Scheme(scheme_string_to_unit),
        "_ed-set-lbuffer <text>  — replace text left of cursor; right side preserved.",
        BuiltinBody::Static(builtin_ed_set_lbuffer),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-insert"),
        BuiltinTypeRule::Scheme(scheme_string_to_unit),
        "_ed-insert <text>  — insert text at cursor; cursor advances past insertion.",
        BuiltinBody::Static(builtin_ed_insert),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-push"),
        BuiltinTypeRule::Scheme(scheme_unit_thunk),
        "_ed-push  — save buffer to stack, clear.",
        BuiltinBody::Static(builtin_ed_push),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-accept"),
        BuiltinTypeRule::Scheme(scheme_unit_thunk),
        "_ed-accept  — mark buffer for immediate execution.",
        BuiltinBody::Static(builtin_ed_accept),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-tui"),
        BuiltinTypeRule::Scheme(scheme_tui),
        "_ed-tui <thunk>  — suspend editor, run thunk, return [output: Str, status: Int]; never raises on body status.",
        BuiltinBody::Static(builtin_ed_tui),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-history"),
        BuiltinTypeRule::Scheme(scheme_history),
        "_ed-history <prefix> <limit>  — prefix search over history; limit=0 for unbounded.",
        BuiltinBody::Static(builtin_ed_history),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-parse"),
        BuiltinTypeRule::Scheme(scheme_parse),
        "_ed-parse  — tokenize buffer at cursor; returns [words, current, offset].",
        BuiltinBody::Static(builtin_ed_parse),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-ghost"),
        BuiltinTypeRule::Scheme(scheme_string_to_unit),
        "_ed-ghost <text>  — set ghost text (empty string clears).",
        BuiltinBody::Static(builtin_ed_ghost),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-highlight"),
        BuiltinTypeRule::Scheme(scheme_highlight),
        "_ed-highlight <spans>  — set highlight spans (empty list clears).",
        BuiltinBody::Static(builtin_ed_highlight),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-clipboard"),
        BuiltinTypeRule::Scheme(scheme_string_to_bool),
        "_ed-clipboard <text>  — OSC 52 system-clipboard write; returns Bool (true on emit, false when host terminal can't).",
        BuiltinBody::Static(builtin_ed_clipboard),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-hyperlink"),
        BuiltinTypeRule::Scheme(scheme_string_string_to_string),
        "_ed-hyperlink <uri> <text>  — wrap text in OSC 8 hyperlink; returns plain text when terminal can't render hyperlinks.",
        BuiltinBody::Static(builtin_ed_hyperlink),
    ),
    BuiltinEntry::new(
        Cow::Borrowed("_ed-state"),
        BuiltinTypeRule::Scheme(scheme_state),
        "_ed-state <default> <updater>  — read-modify-write the plugin's persistent cell.",
        BuiltinBody::Static(builtin_ed_state),
    ),
];

/// Builtins installed into the REPL's own shell at startup
/// (see [`super::super::session::Session::boot`]).
pub static ED_BUILTINS: &[BuiltinEntry] = &ED_BUILTINS_ARR;

#[cfg(test)]
mod tests {
    use super::super::editor::{EditorState, PluginInputs, PluginOutputs};
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
            assert!(
                matches!(entry.type_rule, BuiltinTypeRule::Scheme(..)),
                "no scheme for {:?}",
                entry.name
            );
        }
    }

    /// An interactive shell carrying a plugin context whose buffer holds
    /// `text`, ready to drive `builtin_ed_set`.
    fn shell_with_buffer(text: &str) -> Shell {
        let mut shell = Shell::new(ral_core::io::TerminalState::default());
        shell.set_interactive(true);
        let mut pc = PluginContext {
            inputs: PluginInputs::default(),
            outputs: PluginOutputs::default(),
            editor_state: EditorState::default(),
            state_cell: None,
            state_default_used: false,
        };
        pc.editor_state.text = text.to_string();
        shell.repl_mut().plugin_context = Some(Box::new(pc));
        shell
    }

    fn editor_state(shell: &Shell) -> EditorState {
        ctx(shell).unwrap().editor_state.clone()
    }

    /// The dependency-order regression: setting `text` and `cursor`
    /// together clamps the cursor against the **new** text, not the old
    /// (shorter) buffer.  `OrdMap` visits `"cursor"` before `"text"`, so
    /// a fold over entries would clamp `11` against the empty buffer and
    /// silently lose it.
    #[test]
    fn ed_set_clamps_cursor_against_new_text() {
        let mut shell = shell_with_buffer("");
        let arg = Value::map(vec![
            ("text".into(), Value::String("hello world".into())),
            ("cursor".into(), Value::Int(11)),
        ]);
        builtin_ed_set(&[arg], &Mooring::adrift(), &mut shell).unwrap();
        let st = editor_state(&shell);
        assert_eq!(st.text, "hello world");
        assert_eq!(st.cursor, 11);
    }

    /// An over-long cursor clamps to the new text's character count.
    #[test]
    fn ed_set_clamps_over_long_cursor_to_new_len() {
        let mut shell = shell_with_buffer("");
        let arg = Value::map(vec![
            ("text".into(), Value::String("abc".into())),
            ("cursor".into(), Value::Int(99)),
        ]);
        builtin_ed_set(&[arg], &Mooring::adrift(), &mut shell).unwrap();
        assert_eq!(editor_state(&shell).cursor, 3);
    }

    /// A non-Int cursor errors before any mutation.
    #[test]
    fn ed_set_rejects_non_int_cursor() {
        let mut shell = shell_with_buffer("old");
        let arg = Value::map(vec![
            ("text".into(), Value::String("new".into())),
            ("cursor".into(), Value::String("3".into())),
        ]);
        assert!(builtin_ed_set(&[arg], &Mooring::adrift(), &mut shell).is_err());
        let st = editor_state(&shell);
        assert_eq!(st.text, "old");
    }
}
