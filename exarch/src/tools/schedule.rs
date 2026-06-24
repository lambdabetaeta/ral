//! `schedule` / `schedules` / `unschedule` tools — the agent's self-wakeup
//! surface.
//!
//! A wakeup schedules the *agent*, not a worker: it posts a synthetic,
//! marked user turn into the session inbox at the turn boundary, re-engaging
//! the loop with no human present.  The trigger is a cron expression (a
//! recurring calendar occurrence) or `after <dur>` (a one-shot relative
//! delay); the payload is a string prompt the model acts on, not a
//! computation.  Self-scheduling is gated behind `--allow-schedule`: an
//! agent that can wake itself indefinitely holds real authority.

use super::{Tool, invalid_input, u64_field};
use crate::agent::Agent;
use crate::bus::{Emitter, Kind};
use crate::event::ToolResult as SessionToolResult;
use crate::provider::Provider;
use crate::schedule::{CronSchedule, Trigger, parse_duration};
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

/// `schedule` — arm a recurring cron or one-shot `after` wakeup.
pub(super) struct ScheduleTool;

impl Tool for ScheduleTool {
    fn name(&self) -> &'static str {
        "schedule"
    }

    fn desc(&self) -> &'static str {
        "Schedule a wakeup: at the chosen time a marked user turn carrying \
`prompt` is delivered to you, re-engaging you with no human present.  Give \
exactly one trigger — `cron` (a five-field expression in the host's local \
timezone, e.g. `0 9 * * 1-5` for weekdays at 09:00) for a recurring \
calendar occurrence, or `after` (e.g. `30m`, `2h`) for a one-shot relative \
delay.  The payload is a natural-language instruction you will act on when \
woken, not code.  List live ones with `schedules`; remove one with \
`unschedule`.  Requires scheduling authority (off by default)."
    }

    fn schema(&self) -> &'static Value {
        static S: OnceLock<Value> = OnceLock::new();
        S.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The instruction delivered to you when the wakeup fires.",
                    },
                    "cron": {
                        "type": "string",
                        "description": "A five-field cron expression in host-local time \
            (minute hour day-of-month month day-of-week), e.g. `0 3 * * *` for nightly at \
            03:00.  Recurring.  Give this OR `after`, not both.",
                    },
                    "after": {
                        "type": "string",
                        "description": "A one-shot relative delay: an integer and a unit \
            s/m/h/d, e.g. `30m`, `2h`.  Give this OR `cron`, not both.",
                    },
                    "label": {
                        "type": "string",
                        "description": "An optional human label for the schedule, shown by \
            `schedules`.  Defaults to `sched-{id}`.",
                    },
                },
                "required": ["prompt"],
            })
        })
    }

    fn dispatch(
        &self,
        id: String,
        input: Value,
        session: &mut Agent,
        _provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult {
        if !session.schedule_authority {
            let msg = "scheduling is not authorised in this session — start exarch with \
                       --allow-schedule to enable self-wakeups"
                .to_string();
            emit.emit(Kind::ToolCall {
                tool: "schedule",
                cmd: "schedule".into(),
                summary: None,
            });
            emit.emit(Kind::ToolResult(msg.clone()));
            return SessionToolResult { id, content: msg };
        }
        let Some(obj) = input.as_object() else {
            return invalid_input(
                id,
                "schedule",
                "<invalid input>",
                "tool input is not a JSON object",
                emit,
            );
        };
        let prompt = match obj.get("prompt").and_then(Value::as_str) {
            Some(p) => p.to_string(),
            None => {
                return invalid_input(
                    id,
                    "schedule",
                    "<invalid input>",
                    "missing required string field `prompt`",
                    emit,
                );
            }
        };
        let label = obj.get("label").and_then(Value::as_str).map(str::to_string);
        let cron = obj.get("cron").and_then(Value::as_str);
        let after = obj.get("after").and_then(Value::as_str);
        let trigger = match (cron, after) {
            (Some(expr), None) => match CronSchedule::parse(expr) {
                Ok(schedule) => Trigger::Cron {
                    schedule,
                    expr: expr.to_string(),
                },
                Err(e) => return invalid_input(id, "schedule", expr, &e, emit),
            },
            (None, Some(dur)) => match parse_duration(dur) {
                Ok(d) => Trigger::After(d),
                Err(e) => return invalid_input(id, "schedule", dur, &e, emit),
            },
            (Some(_), Some(_)) => {
                return invalid_input(
                    id,
                    "schedule",
                    "<invalid input>",
                    "give exactly one trigger: `cron` or `after`, not both",
                    emit,
                );
            }
            (None, None) => {
                return invalid_input(
                    id,
                    "schedule",
                    "<invalid input>",
                    "give one trigger: `cron` or `after`",
                    emit,
                );
            }
        };
        emit.emit(Kind::ToolCall {
            tool: "schedule",
            cmd: format!("{}: {}", trigger.describe(), prompt),
            summary: label.clone(),
        });
        let mailbox = session.mailbox();
        let content = match session.schedules.schedule(trigger, prompt, label, mailbox) {
            Ok(sid) => format!("scheduled (id {sid})"),
            Err(e) => format!("could not schedule: {e}"),
        };
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
    }
}

/// `schedules` — list the live scheduled wakeups.
pub(super) struct SchedulesTool;

impl Tool for SchedulesTool {
    fn name(&self) -> &'static str {
        "schedules"
    }

    fn desc(&self) -> &'static str {
        "List the live scheduled wakeups — by id, with label, trigger, time \
to the next fire, and how many times each has fired.  Use it to recover \
schedule ids after a context compaction, then `unschedule` to remove one."
    }

    fn schema(&self) -> &'static Value {
        static S: OnceLock<Value> = OnceLock::new();
        S.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {},
                "required": [],
            })
        })
    }

    fn dispatch(
        &self,
        id: String,
        _input: Value,
        session: &mut Agent,
        _provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult {
        emit.emit(Kind::ToolCall {
            tool: "schedules",
            cmd: "schedules".into(),
            summary: None,
        });
        let live = session.schedules.list();
        let content = if live.is_empty() {
            "no live schedules".to_string()
        } else {
            live.iter()
                .map(|s| {
                    let next = s
                        .next_in
                        .map(|d| format!("{}s", d.as_secs()))
                        .unwrap_or_else(|| "never".into());
                    format!(
                        "{}  {}  [{}]  next in {}  fired {}",
                        s.id, s.label, s.trigger, next, s.fires
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
    }
}

/// `unschedule` — remove one scheduled wakeup by id.
pub(super) struct UnscheduleTool;

impl Tool for UnscheduleTool {
    fn name(&self) -> &'static str {
        "unschedule"
    }

    fn desc(&self) -> &'static str {
        "Remove a scheduled wakeup by its id (from `schedules`).  A no-op if \
no schedule has that id."
    }

    fn schema(&self) -> &'static Value {
        static S: OnceLock<Value> = OnceLock::new();
        S.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The id of the schedule to remove, as shown by `schedules`.",
                    },
                },
                "required": ["id"],
            })
        })
    }

    fn dispatch(
        &self,
        id: String,
        input: Value,
        session: &mut Agent,
        _provider: &Arc<Provider>,
        emit: &Emitter,
    ) -> SessionToolResult {
        let sid = match u64_field(&input, "id") {
            Some(n) => n,
            None => {
                return invalid_input(
                    id,
                    "unschedule",
                    "<invalid input>",
                    "missing required integer field `id`",
                    emit,
                );
            }
        };
        emit.emit(Kind::ToolCall {
            tool: "unschedule",
            cmd: format!("unschedule {sid}"),
            summary: None,
        });
        let content = if session.schedules.unschedule(sid) {
            format!("unscheduled {sid}")
        } else {
            format!("no schedule with id {sid}")
        };
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
    }
}
