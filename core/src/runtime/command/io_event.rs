//! Builds the structural I/O events that redirect and exec doors push onto a
//! run's surface sink through `Mooring::emit_io`.  Core emits plain
//! [`Value::Map`]s and names no card type, so these constructors are the whole
//! wire contract; the host decodes it in `value_to_io`, in
//! `exarch/src/bus/card/io.rs`.

use crate::syntax::ast::RedirectMode;
use crate::types::Value;

/// How a write door settled.
#[derive(Clone, Copy)]
pub(crate) enum WriteOutcome {
    Committed,
    /// The body broke before commit: an atomic temp is discarded, but a
    /// non-atomic target may be left partly written.
    Aborted,
    /// The open never succeeded, or the atomic rename failed at commit.
    Failed,
}

impl WriteOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::Aborted => "aborted",
            Self::Failed => "failed",
        }
    }
}

fn mode_str(mode: RedirectMode) -> &'static str {
    match mode {
        RedirectMode::Write => "write",
        RedirectMode::Append => "append",
        RedirectMode::StreamWrite => "stream",
        RedirectMode::Read | RedirectMode::HereString => {
            unreachable!("stdin doors never produce write events")
        }
    }
}

/// `{io:"read", path:<path>}` — a `< file` stdin redirect opened.
pub(crate) fn read(path: &str) -> Value {
    Value::map(vec![
        ("io".to_string(), Value::String("read".into())),
        ("path".to_string(), Value::String(path.to_string())),
    ])
}

/// `{io:"write", path, mode, outcome}` — an fd 1/2 file target, settled at
/// frame teardown.  `new_bytes` (a bounded head of what landed) and
/// `old_bytes` (the whole prior content, atomic overwrites only) join it when
/// the caller has them; absent, they are omitted keys rather than nulls.
pub(crate) fn write(
    path: &str,
    mode: RedirectMode,
    outcome: WriteOutcome,
    new_bytes: Option<&[u8]>,
    old_bytes: Option<&[u8]>,
) -> Value {
    let mut fields = vec![
        ("io".to_string(), Value::String("write".into())),
        ("path".to_string(), Value::String(path.to_string())),
        ("mode".to_string(), Value::String(mode_str(mode).into())),
        (
            "outcome".to_string(),
            Value::String(outcome.as_str().into()),
        ),
    ];
    if let Some(b) = new_bytes {
        fields.push(("new_bytes".to_string(), Value::Bytes(b.to_vec())));
    }
    if let Some(b) = old_bytes {
        fields.push(("old_bytes".to_string(), Value::Bytes(b.to_vec())));
    }
    Value::map(fields)
}

/// `{io:"exec", argv:[prog, …args], outcome, status}` — an external or
/// bundled command completed.
pub(crate) fn exec(program: &str, args: &[String], status: i32) -> Value {
    let argv: Vec<Value> = std::iter::once(Value::String(program.to_string()))
        .chain(args.iter().map(|a| Value::String(a.clone())))
        .collect();
    let outcome = if status == 0 { "ok" } else { "bad" };
    Value::map(vec![
        ("io".to_string(), Value::String("exec".into())),
        ("argv".to_string(), Value::list(argv)),
        ("outcome".to_string(), Value::String(outcome.into())),
        ("status".to_string(), Value::Int(i64::from(status))),
    ])
}
