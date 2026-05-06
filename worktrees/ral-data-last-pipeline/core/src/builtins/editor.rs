//! `_editor` — line editor interface exposed to plugin handlers.
//!
//! All operations require an active `PluginContext`, set up by the REPL
//! before it dispatches into a plugin handler.  Outside a handler every
//! op fails with a "no plugin context" error.

use crate::types::*;

use super::util::{arg0_str, as_list, as_map, sig};

/// Valid highlight style names.
const STYLES: &[&str] = &[
    "command",
    "builtin",
    "prelude",
    "argument",
    "option",
    "path-exists",
    "path-missing",
    "string",
    "number",
    "comment",
    "error",
    "match",
    "bracket-1",
    "bracket-2",
    "bracket-3",
];

pub fn builtin_editor(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    if !shell.io.interactive {
        return Err(sig("_editor: not available outside interactive mode"));
    }
    let op = arg0_str(args);
    let rest = args.get(1..).unwrap_or(&[]);
    match op.as_str() {
        "get" => editor_get(shell),
        "set" => editor_set(rest, shell),
        "push" => editor_push(shell),
        "accept" => editor_accept(shell),
        "tui" => editor_tui(rest, shell),
        "history" => editor_history(rest, shell),
        "parse" => editor_parse(shell),
        "ghost" => editor_ghost(rest, shell),
        "highlight" => editor_highlight(rest, shell),
        "state" => editor_state(rest, shell),
        _ => Err(sig(format!("_editor: unknown operation '{op}'"))),
    }
}

fn ctx(shell: &Shell) -> Result<&PluginContext, EvalSignal> {
    shell.repl.plugin_context
        .as_ref()
        .ok_or_else(|| sig("_editor: no plugin context (not inside a plugin handler)"))
}

fn ctx_mut(shell: &mut Shell) -> Result<&mut PluginContext, EvalSignal> {
    shell.repl.plugin_context
        .as_mut()
        .ok_or_else(|| sig("_editor: no plugin context (not inside a plugin handler)"))
}

/// `_editor 'get'` → `[text: Str, cursor: Int, keymap: Str]`
fn editor_get(shell: &Shell) -> Result<Value, EvalSignal> {
    shell.check_editor_read("get")?;
    let pc = ctx(shell)?;
    Ok(Value::Map(vec![
        ("text".into(), Value::String(pc.editor_state.text.clone())),
        ("cursor".into(), Value::Int(pc.editor_state.cursor as i64)),
        (
            "keymap".into(),
            Value::String(pc.editor_state.keymap.clone()),
        ),
    ]))
}

/// `_editor 'set' [text: Str, cursor: Int]`
fn editor_set(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    shell.check_editor_write("set")?;
    if args.is_empty() {
        return Err(sig("_editor 'set' requires a record argument"));
    }
    let map = as_map(&args[0], "_editor 'set'")?;
    let pc = ctx_mut(shell)?;
    for (k, v) in &map {
        match k.as_str() {
            "text" => pc.editor_state.text = v.to_string(),
            "cursor" => {
                let n = match v {
                    Value::Int(n) => *n,
                    _ => return Err(sig("_editor 'set': cursor must be Int")),
                };
                let max = pc.editor_state.text.chars().count() as i64;
                pc.editor_state.cursor = n.clamp(0, max) as usize;
            }
            _ => {} // row polymorphism: extra fields ignored
        }
    }
    Ok(Value::Unit)
}

/// `_editor 'push'` — save buffer to stack, clear.
fn editor_push(shell: &mut Shell) -> Result<Value, EvalSignal> {
    shell.check_editor_write("push")?;
    let pc = ctx_mut(shell)?;
    let text = std::mem::take(&mut pc.editor_state.text);
    let cursor = pc.editor_state.cursor;
    pc.editor_state.cursor = 0;
    pc.outputs.pushed_buffer = Some((text, cursor));
    Ok(Value::Unit)
}

/// `_editor 'accept'` — mark buffer for immediate execution.
fn editor_accept(shell: &mut Shell) -> Result<Value, EvalSignal> {
    shell.check_editor_write("accept")?;
    let pc = ctx_mut(shell)?;
    pc.outputs.accept_line = true;
    Ok(Value::Unit)
}

/// `_editor 'tui' {block}` — suspend editor, run block.
///
/// The body's stdout is captured so that a TUI command (e.g. `fzf`) which
/// prints its selection on stdout can have that selection delivered back to
/// the plugin as a String.  The TUI itself draws on /dev/tty via stderr, so
/// capturing stdout does not disrupt the interface.  When the body returns a
/// non-Unit value it wins; otherwise the captured bytes are decoded (trailing
/// newline stripped) and returned as a String.
fn editor_tui(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    shell.check_editor_tui()?;
    if args.is_empty() {
        return Err(sig("_editor 'tui' requires a thunk argument"));
    }
    {
        let pc = ctx(shell)?;
        if pc.in_tui {
            return Err(sig("_editor 'tui': already in TUI mode"));
        }
        if pc.inputs.in_readline {
            return Err(sig(
                "_editor 'tui': not available inside buffer-change hooks",
            ));
        }
    }
    ctx_mut(shell)?.in_tui = true;
    let (result, bytes) =
        crate::evaluator::with_capture(shell, |shell| super::call_value(&args[0], &[], shell));
    if let Some(pc) = shell.repl.plugin_context.as_mut() {
        pc.in_tui = false;
    }
    result.map(|v| match v {
        Value::Unit if !bytes.is_empty() => {
            let mut s = String::from_utf8_lossy(&bytes).into_owned();
            if s.ends_with('\n') {
                s.pop();
            }
            Value::String(s)
        }
        Value::Unit => Value::String(String::new()),
        Value::Bytes(b) => {
            let mut s = String::from_utf8_lossy(&b).into_owned();
            if s.ends_with('\n') {
                s.pop();
            }
            Value::String(s)
        }
        other => other,
    })
}

/// `_editor 'history' <prefix> <limit>` — prefix search over history.
fn editor_history(args: &[Value], shell: &Shell) -> Result<Value, EvalSignal> {
    shell.check_editor_read("history")?;
    let prefix = arg0_str(args);
    let limit = match args.get(1) {
        Some(Value::Int(n)) => *n as usize,
        _ => 0,
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
    Ok(Value::List(results))
}

/// `_editor 'parse'` — tokenize buffer at cursor.
fn editor_parse(shell: &Shell) -> Result<Value, EvalSignal> {
    shell.check_editor_read("parse")?;
    let pc = ctx(shell)?;
    let text = &pc.editor_state.text;
    let cursor = pc.editor_state.cursor;

    if text.is_empty() {
        return Ok(Value::Map(vec![
            ("words".into(), Value::List(vec![])),
            ("current".into(), Value::Int(0)),
            ("offset".into(), Value::Int(0)),
        ]));
    }

    // Simple whitespace tokenizer.  A full parser integration would use
    // ral_core::parse, but for now a word-split with cursor tracking
    // covers the common completion cases.
    let mut words: Vec<(usize, String)> = Vec::new(); // (byte_offset, word)
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        // Handle single-quoted strings (no hash-bump support — completion
        // tokenizer is intentionally minimal).
        if bytes[i] == b'\'' {
            i += 1;
            let mut word = String::new();
            while i < bytes.len() && bytes[i] != b'\'' {
                word.push(bytes[i] as char);
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // closing '
            }
            words.push((start, word));
        } else if bytes[i] == b'"' {
            // Double-quoted: just strip quotes for tokenization
            i += 1;
            let mut word = String::new();
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 1;
                    word.push(bytes[i] as char);
                } else {
                    word.push(bytes[i] as char);
                }
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            } // skip closing "
            words.push((start, word));
        } else {
            // Unquoted token: split on whitespace and shell metacharacters
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && !matches!(bytes[i], b'|' | b';' | b'{' | b'}')
            {
                i += 1;
            }
            words.push((start, text[start..i].to_string()));
        }
    }

    // Determine which word the cursor is in/after.
    // Convert cursor from char index to byte offset for comparison.
    let cursor_byte = text
        .char_indices()
        .nth(cursor)
        .map(|(i, _)| i)
        .unwrap_or(text.len());

    let mut current = 0usize;
    let mut offset = 0usize;
    for (idx, (word_start, _)) in words.iter().enumerate() {
        if *word_start <= cursor_byte {
            current = idx;
            offset = *word_start;
        }
    }

    // Convert offset back to char index
    let offset_chars = text[..offset].chars().count();

    let word_values: Vec<Value> = words
        .iter()
        .map(|(_, w)| Value::String(w.clone()))
        .collect();

    Ok(Value::Map(vec![
        ("words".into(), Value::List(word_values)),
        ("current".into(), Value::Int(current as i64)),
        ("offset".into(), Value::Int(offset_chars as i64)),
    ]))
}

/// `_editor 'ghost' <text>`
fn editor_ghost(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    shell.check_editor_write("ghost")?;
    let text = arg0_str(args);
    let pc = ctx_mut(shell)?;
    pc.outputs.ghost_text = (!text.is_empty()).then_some(text);
    Ok(Value::Unit)
}

/// `_editor 'highlight' [Span]`
fn editor_highlight(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    shell.check_editor_write("highlight")?;
    if args.is_empty() {
        ctx_mut(shell)?.outputs.highlight_spans.clear();
        return Ok(Value::Unit);
    }
    let spans_val = as_list(&args[0], "_editor 'highlight'")?;
    let text_len = ctx(shell)?.editor_state.text.chars().count();
    let int_field = |v: &Value, field: &'static str| match v {
        Value::Int(n) => Ok(*n),
        _ => Err(sig(format!("highlight span: {field} must be Int"))),
    };
    let mut spans = Vec::with_capacity(spans_val.len());
    for sv in &spans_val {
        let m = as_map(sv, "_editor 'highlight' span")?;
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
        if !STYLES.contains(&style.as_str()) {
            return Err(sig(format!("_editor 'highlight': unknown style '{style}'")));
        }
        spans.push(HighlightSpan {
            start: start.clamp(0, text_len as i64) as usize,
            end: end.clamp(0, text_len as i64) as usize,
            style,
        });
    }
    ctx_mut(shell)?.outputs.highlight_spans = spans;
    Ok(Value::Unit)
}

/// `_editor 'state' <default> <updater>`
fn editor_state(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    shell.check_editor_write("state")?;
    if args.len() < 2 {
        return Err(sig(
            "_editor 'state' requires 2 arguments (default, updater)",
        ));
    }
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
    let new_val = super::call_value(updater, &[current], shell)?;
    let pc = ctx_mut(shell)?;
    pc.state_cell = Some(new_val.clone());
    pc.state_default_used = true;
    Ok(new_val)
}
