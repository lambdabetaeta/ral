//! Structural I/O event values pushed onto a turn's `surface` sink at
//! every redirect read/write door and every exec completion door.
//!
//! Core emits plain [`Value::Map`]s here; a host (exarch) decodes them
//! into cards.  Core names no card type — these constructors fix only
//! the wire shape:
//!
//! ```text
//! read:  {io:"read",  path:<string>}
//! write: {io:"write", path:<string>, mode:<"write"|"append"|"stream">,
//!         outcome:<"committed"|"aborted"|"failed">,
//!         new_bytes:<bytes?>, old_bytes:<bytes?>}
//! exec:  {io:"exec",  argv:<list<string>>, outcome:<"ok"|"bad">, status:<int>}
//! ```

use crate::syntax::ast::RedirectMode;
use crate::types::Value;

/// The settled outcome of a write door: how the logical write op ended.
#[derive(Clone, Copy)]
pub(crate) enum WriteOutcome {
    /// Body ran to completion and (for atomic `>`) the rename succeeded.
    Committed,
    /// The body returned `Err` before commit — the logical write op did
    /// not complete (atomic temp discarded; non-atomic may be partial).
    Aborted,
    /// The open itself failed, or the atomic rename failed at commit.
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

/// `RedirectMode` -> the `mode` field string.  The stdin modes (`Read`,
/// `HereString`) have no write door, so they are not representable here.
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

/// `{io:"write", path:<path>, mode:…, outcome:…, new_bytes:<bytes|null>,
/// old_bytes:<bytes|null>}` — an fd 1/2 file write target, settled at frame
/// teardown.  `new_bytes` is a bounded prefix of the committed content — the
/// head the host previews in the write card, not the whole file.  `old_bytes`
/// is the pre-existing target's whole content, present only when the write
/// was atomic and overwrote an existing file no larger than the same bound on
/// either side — the host diffs `old_bytes`/`new_bytes` into a diff card
/// instead of a plain preview when both are present and differ.
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

/// `{io:"exec", argv:[prog, …args], outcome:…, status:…}` — an external
/// or bundled command completed.  `outcome` is `"ok"` iff `status == 0`.
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
