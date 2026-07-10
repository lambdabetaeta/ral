//! Scheduled-wakeup triggers: a five-field cron grammar parsed in-tree and
//! evaluated against the host-local timezone with `jiff`, plus a relative
//! `after <dur>` one-shot.
//!
//! Cron is *calendar* scheduling — "every weekday at 09:00", "nightly at
//! 03:00" — the dominant shape for a resident agent and the lingua franca
//! models emit reliably.  `after` covers "in two hours", which cron cannot
//! express.  The grammar is small and fully specified, so it is parsed here
//! rather than pulling a `chrono`-based cron crate that would drag a second
//! datetime tree in beside the `jiff` already compiled in; evaluation
//! (next-occurrence, timezone, DST) reuses `jiff`.
//!
//! Cron is wall-clock; the reaper is monotonic.  [`Trigger::next_delay`]
//! bridges them: `jiff` computes the next absolute occurrence in the host
//! tz, and the caller arms the reaper with the monotonic delta to it,
//! recomputing on each fire so DST shifts, clock steps, and suspends are
//! absorbed.

use crate::bus::{InboxMsg, Mailbox};
use jiff::civil::DateTime;
use jiff::{ToSpan, Zoned};
use ral_core::process::{Deadline, arm_callback};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// Three-letter month names, lowercased, 1-indexed (Jan = 1).
const MONTHS: &[(&str, u8)] = &[
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];

/// Three-letter weekday names, lowercased, cron-numbered (Sun = 0).
const WEEKDAYS: &[(&str, u8)] = &[
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];

/// A safety cap on the next-occurrence search.  With the whole-day skip
/// below, a valid cron reaches its next fire in at most (days to the first
/// matching date) + one day of minutes, so a century of headroom covers
/// even the rarest legitimate expression (a leap-day-on-a-weekday cron)
/// while bounding a parseable-but-impossible one (e.g. Feb 30) to a quick
/// `None`.
const MAX_STEPS: usize = 366 * 100 + 1_440;

/// A parsed five-field cron expression: one allowed-value bitmask per
/// field, plus whether the day-of-month and day-of-week fields were `*`
/// (which selects the day-matching rule).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minute: u64, // 0..=59
    hour: u64,   // 0..=23
    dom: u64,    // 1..=31
    month: u64,  // 1..=12
    dow: u64,    // 0..=6 (Sun = 0)
    dom_star: bool,
    dow_star: bool,
}

impl CronSchedule {
    /// Parse a standard five-field expression: `minute hour dom month dow`.
    /// Each field is a comma list of `*`, `N`, `a-b`, `*/step`, `a-b/step`,
    /// or `N/step` (N to the field max).  Month and day-of-week also accept
    /// three-letter names; day-of-week accepts `7` for Sunday.
    pub fn parse(expr: &str) -> Result<Self, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "a cron expression has five fields (minute hour day-of-month month day-of-week), got {}",
                fields.len()
            ));
        }
        let minute = parse_field(fields[0], 0, 59, &[])?;
        let hour = parse_field(fields[1], 0, 23, &[])?;
        let dom = parse_field(fields[2], 1, 31, &[])?;
        let month = parse_field(fields[3], 1, 12, MONTHS)?;
        // Day-of-week is parsed over 0..=7 to admit 7 as Sunday, then 7 is
        // folded onto 0 so the matcher reads a single Sunday bit.
        let mut dow = parse_field(fields[4], 0, 7, WEEKDAYS)?;
        if dow & (1 << 7) != 0 {
            dow = (dow & !(1 << 7)) | 1;
        }
        Ok(Self {
            minute,
            hour,
            dom,
            month,
            dow,
            dom_star: fields[2].trim() == "*",
            dow_star: fields[4].trim() == "*",
        })
    }

    /// The next firing instant strictly after `from`, in `from`'s timezone,
    /// or `None` if the expression never fires within the search horizon.
    pub fn next_after(&self, from: &Zoned) -> Option<Zoned> {
        let tz = from.time_zone().clone();
        // Start at the next whole minute strictly after `from`: cron fires on
        // minute boundaries, and a partial current minute is already past.
        let mut cand = from
            .datetime()
            .with()
            .second(0)
            .subsec_nanosecond(0)
            .build()
            .ok()?
            .checked_add(1.minute())
            .ok()?;
        for _ in 0..MAX_STEPS {
            if !self.date_matches(&cand) {
                // No time on a non-matching date can fire: skip the whole
                // day to its successor's 00:00 rather than stepping minutes.
                cand = cand
                    .with()
                    .hour(0)
                    .minute(0)
                    .second(0)
                    .subsec_nanosecond(0)
                    .build()
                    .ok()?
                    .checked_add(1.day())
                    .ok()?;
                continue;
            }
            if self.time_matches(&cand)
                && let Ok(z) = cand.to_zoned(tz.clone())
                && z.timestamp().duration_since(from.timestamp()).as_secs() > 0
            {
                return Some(z);
            }
            cand = cand.checked_add(1.minute()).ok()?;
        }
        None
    }

    fn time_matches(&self, dt: &DateTime) -> bool {
        bit(self.minute, dt.minute()) && bit(self.hour, dt.hour())
    }

    fn date_matches(&self, dt: &DateTime) -> bool {
        if !bit(self.month, dt.month()) {
            return false;
        }
        let dom_ok = bit(self.dom, dt.day());
        let dow_ok = bit(self.dow, dt.weekday().to_sunday_zero_offset());
        // Vixie-cron OR semantics: when both day fields are restricted, a day
        // matches if *either* does; when only one is, only that one; when
        // neither, every day.
        match (self.dom_star, self.dow_star) {
            (true, true) => true,
            (false, true) => dom_ok,
            (true, false) => dow_ok,
            (false, false) => dom_ok || dow_ok,
        }
    }
}

/// Whether bit `v` is set in `mask`, with `v` from `jiff`'s `i8` fields.
fn bit(mask: u64, v: i8) -> bool {
    (0i8..64).contains(&v) && (mask >> v) & 1 == 1
}

/// Parse one cron field into an allowed-value bitmask over `[min, max]`.
fn parse_field(spec: &str, min: u8, max: u8, names: &[(&str, u8)]) -> Result<u64, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("empty cron field".into());
    }
    let mut mask = 0u64;
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u8 = s
                    .parse()
                    .map_err(|_| format!("invalid step `{s}` in cron field `{spec}`"))?;
                if step == 0 {
                    return Err(format!("cron step cannot be zero in `{spec}`"));
                }
                (r, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (resolve(a, min, max, names)?, resolve(b, min, max, names)?)
        } else {
            let v = resolve(range, min, max, names)?;
            // `N/step` means N to the field max; a bare `N` is just N.
            if part.contains('/') { (v, max) } else { (v, v) }
        };
        if lo > hi {
            return Err(format!("cron range `{range}` is descending in `{spec}`"));
        }
        let mut v = lo;
        while v <= hi {
            mask |= 1 << v;
            v += step;
        }
    }
    Ok(mask)
}

/// Resolve one cron atom — a number or a three-letter name — into a value,
/// validating it lies in `[min, max]`.
fn resolve(tok: &str, min: u8, max: u8, names: &[(&str, u8)]) -> Result<u8, String> {
    let tok = tok.trim();
    let v = if let Ok(n) = tok.parse::<u8>() {
        n
    } else {
        let lower = tok.to_ascii_lowercase();
        names
            .iter()
            .find(|(name, _)| *name == lower)
            .map(|(_, n)| *n)
            .ok_or_else(|| format!("unrecognised cron value `{tok}`"))?
    };
    if v < min || v > max {
        return Err(format!("cron value `{tok}` out of range {min}..={max}"));
    }
    Ok(v)
}

/// What makes a wakeup fire: a recurring calendar cron, or a one-shot
/// relative delay.
#[derive(Debug, Clone)]
pub enum Trigger {
    /// A recurring cron, with the source expression kept for display and the
    /// marked render.
    Cron {
        schedule: CronSchedule,
        expr: String,
    },
    /// A one-shot relative delay from arming time.
    After(Duration),
}

impl Trigger {
    /// Whether the trigger re-arms after firing.  Cron recurs; `after` is
    /// one-shot.
    pub fn is_recurring(&self) -> bool {
        matches!(self, Self::Cron { .. })
    }

    /// The monotonic delay from now to the next fire, computed fresh each
    /// call (cron recomputes against the host tz; `after` is fixed).  `None`
    /// for a cron whose next occurrence is beyond the search horizon.
    pub fn next_delay(&self) -> Option<Duration> {
        match self {
            Self::After(d) => Some(*d),
            Self::Cron { schedule, .. } => {
                let now = Zoned::now();
                let next = schedule.next_after(&now)?;
                let secs = next.timestamp().duration_since(now.timestamp()).as_secs();
                Some(Duration::from_secs(secs.max(0) as u64))
            }
        }
    }

    /// The trigger as text, for the `schedules` listing and the marked
    /// wakeup render.
    pub fn describe(&self) -> String {
        match self {
            Self::Cron { expr, .. } => expr.clone(),
            Self::After(d) => format!("after {}", fmt_duration(*d)),
        }
    }
}

/// Parse a relative-delay string for `after`: an integer followed by one of
/// `s`, `m`, `h`, `d` (e.g. `30m`, `2h`).
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| format!("duration `{s}` needs a unit (s/m/h/d), e.g. 30m"))?,
    );
    let n: u64 = num
        .parse()
        .map_err(|_| format!("invalid duration count in `{s}`"))?;
    let secs = match unit.trim() {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        other => return Err(format!("unknown duration unit `{other}` (use s/m/h/d)")),
    };
    if secs == 0 {
        return Err("duration must be greater than zero".into());
    }
    Ok(Duration::from_secs(secs))
}

/// Render a `Duration` back to the compact `after` form for listings.
fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s.is_multiple_of(86_400) {
        format!("{}d", s / 86_400)
    } else if s.is_multiple_of(3600) {
        format!("{}h", s / 3600)
    } else if s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// The id of a live schedule, unique for the session's lifetime (monotonic,
/// never reused across `/clear`).
pub type ScheduleId = u64;

/// A snapshot row for the `schedules` listing.
pub struct ScheduleInfo {
    pub id: ScheduleId,
    pub label: String,
    pub trigger: String,
    /// Seconds until the next fire, or `None` for a cron with no further
    /// occurrence.
    pub next_in: Option<Duration>,
    /// How many times this schedule has fired.
    pub fires: u64,
}

/// A session's live scheduled wakeups.
///
/// Cheap to clone — the inner `Arc`
/// shares the map, so the reaper closure that fires a schedule holds a
/// handle it re-arms and posts through after the arming turn has ended.
#[derive(Clone)]
pub struct ScheduleRegistry {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    /// Monotonic id source; never reset, so a stale fire can never reach a
    /// schedule created after a `/clear`.
    next_id: ScheduleId,
    entries: HashMap<ScheduleId, Entry>,
}

struct Entry {
    trigger: Trigger,
    prompt: String,
    label: String,
    /// Shared with the in-flight wakeup message: set on post, cleared on
    /// drain, read for the overlap-skip rule.
    pending: Arc<AtomicBool>,
    fires: u64,
    /// Holds the next occurrence armed on the reaper; replaced on each fire,
    /// dropped (disarming) when the schedule is removed.
    deadline: Deadline,
}

impl Default for ScheduleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ScheduleRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                next_id: 0,
                entries: HashMap::new(),
            })),
        }
    }

    /// Add a schedule and arm its first occurrence on the reaper.  Returns
    /// the new id, or an error when a cron expression never fires.  `label`
    /// defaults to `sched-{id}`.  `mailbox` is the *owning session's* mailbox
    /// the fired wakeup is posted to — a session schedules only itself.
    pub fn schedule(
        &self,
        trigger: Trigger,
        prompt: String,
        label: Option<String>,
        mailbox: Mailbox,
    ) -> Result<ScheduleId, String> {
        let delay = trigger
            .next_delay()
            .ok_or_else(|| "this trigger has no next occurrence".to_string())?;
        let mut g = self.lock();
        let id = g.next_id;
        g.next_id += 1;
        let label = label.unwrap_or_else(|| format!("sched-{id}"));
        let deadline = self.arm_deadline(id, &mailbox, delay);
        g.entries.insert(
            id,
            Entry {
                trigger,
                prompt,
                label,
                pending: Arc::new(AtomicBool::new(false)),
                fires: 0,
                deadline,
            },
        );
        Ok(id)
    }

    /// Remove one schedule by id; `true` if it existed.  Dropping its entry
    /// disarms its reaper deadline.
    pub fn unschedule(&self, id: ScheduleId) -> bool {
        self.lock().entries.remove(&id).is_some()
    }

    /// Whether any schedule is live — the drive loop's park-or-terminate
    /// input at quiescence: a peer with a live self-schedule parks for its
    /// next wakeup rather than terminating.
    pub fn armed(&self) -> bool {
        !self.lock().entries.is_empty()
    }

    /// Snapshot the live schedules, ordered by id.
    pub fn list(&self) -> Vec<ScheduleInfo> {
        let g = self.lock();
        let mut v: Vec<ScheduleInfo> = g
            .entries
            .iter()
            .map(|(id, e)| ScheduleInfo {
                id: *id,
                label: e.label.clone(),
                trigger: e.trigger.describe(),
                next_in: e.trigger.next_delay(),
                fires: e.fires,
            })
            .collect();
        v.sort_by_key(|s| s.id);
        v
    }

    /// `/clear`: drop every schedule (disarming each reaper deadline).  The
    /// monotonic id source is untouched, so a wakeup armed before the clear
    /// can never fire into a schedule created after it.
    pub fn clear(&self) {
        self.lock().entries.clear();
    }

    /// Arm one occurrence on the reaper: a `Run` deadline whose closure
    /// fires this schedule.  Does not touch the registry lock, so it is safe
    /// to call while holding it.
    fn arm_deadline(&self, id: ScheduleId, mailbox: &Mailbox, delay: Duration) -> Deadline {
        let reg = self.clone();
        let mailbox = mailbox.clone();
        arm_callback(delay, move || reg.fire(id, &mailbox))
    }

    /// The reaper fired this schedule's deadline.  Posts a wakeup (unless a
    /// previous one is still pending — the overlap-skip), then re-arms the
    /// next occurrence (cron) or removes the schedule (one-shot `after`).
    /// Runs on the reaper thread, outside its heap lock, so re-arming here
    /// is safe.  The wakeup is posted only after this registry's guard
    /// drops: a park verdict reads `armed()` under the consumer's inbox
    /// mutex, so the process-wide lock order is inbox → registry (see
    /// `bus`'s module docs) and a push must never run under this lock.
    fn fire(&self, id: ScheduleId, mailbox: &Mailbox) {
        let mut g = self.lock();
        let Some(entry) = g.entries.get_mut(&id) else {
            return; // unscheduled or cleared between arming and firing
        };
        let recurring = entry.trigger.is_recurring();
        // Overlap-skip: post only when no previous wakeup is still unconsumed.
        let msg = if entry.pending.swap(true, Ordering::AcqRel) {
            None
        } else {
            entry.fires += 1;
            Some(InboxMsg::ScheduledWakeup {
                id,
                label: entry.label.clone(),
                trigger: entry.trigger.describe(),
                prompt: entry.prompt.clone(),
                pending: entry.pending.clone(),
            })
        };
        if recurring {
            if let Some(delay) = entry.trigger.next_delay() {
                entry.deadline = self.arm_deadline(id, mailbox, delay);
            } else {
                let _ = entry;
                g.entries.remove(&id);
            }
        } else {
            let _ = entry;
            g.entries.remove(&id);
        }
        drop(g);
        if let Some(msg) = msg {
            mailbox
                .push(msg)
                .expect("ScheduledWakeup is idempotent and never rejects");
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{Inbox, Turn};
    use jiff::civil::date;

    fn sched(expr: &str) -> CronSchedule {
        CronSchedule::parse(expr).unwrap()
    }

    #[test]
    fn rejects_wrong_field_count() {
        assert!(CronSchedule::parse("* * * *").is_err());
        assert!(CronSchedule::parse("0 9 * * 1-5 extra").is_err());
    }

    #[test]
    fn parses_stars_lists_ranges_steps_and_names() {
        // Every minute.
        let s = sched("* * * * *");
        assert!(s.dom_star && s.dow_star);
        // Weekdays at 09:00, names and numbers agree.
        assert_eq!(sched("0 9 * * 1-5"), sched("0 9 * * mon-fri"));
        // Step and list.
        let half = sched("*/30 * * * *");
        assert!(bit(half.minute, 0) && bit(half.minute, 30) && !bit(half.minute, 15));
        let list = sched("0 0,12 * * *");
        assert!(bit(list.hour, 0) && bit(list.hour, 12) && !bit(list.hour, 6));
        // Month names.
        assert_eq!(sched("0 0 1 jan *"), sched("0 0 1 1 *"));
        // 7 folds onto Sunday.
        assert_eq!(sched("0 0 * * 7"), sched("0 0 * * 0"));
    }

    #[test]
    fn rejects_out_of_range_and_bad_steps() {
        assert!(CronSchedule::parse("60 * * * *").is_err());
        assert!(CronSchedule::parse("* 24 * * *").is_err());
        assert!(CronSchedule::parse("* * 32 * *").is_err());
        assert!(CronSchedule::parse("* * * 13 *").is_err());
        assert!(CronSchedule::parse("*/0 * * * *").is_err());
        assert!(CronSchedule::parse("5-1 * * * *").is_err());
    }

    #[test]
    fn next_after_finds_the_following_daily_occurrence() {
        // 2026-06-20 is a Saturday, 10:00 local (UTC for the test tz).
        let from = date(2026, 6, 20)
            .at(10, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap();
        // Nightly at 03:00 → next is 2026-06-21 03:00.
        let next = sched("0 3 * * *").next_after(&from).unwrap();
        assert_eq!(next.year(), 2026);
        assert_eq!(next.month(), 6);
        assert_eq!(next.day(), 21);
        assert_eq!(next.hour(), 3);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn next_after_respects_weekday_restriction() {
        // From Saturday 2026-06-20 10:00, weekdays-at-09:00 fires Monday.
        let from = date(2026, 6, 20)
            .at(10, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap();
        let next = sched("0 9 * * 1-5").next_after(&from).unwrap();
        assert_eq!(next.day(), 22, "Monday 2026-06-22");
        assert_eq!(next.hour(), 9);
    }

    #[test]
    fn next_after_steps_within_the_hour() {
        let from = date(2026, 6, 20)
            .at(10, 5, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap();
        let next = sched("*/30 * * * *").next_after(&from).unwrap();
        assert_eq!(next.hour(), 10);
        assert_eq!(next.minute(), 30);
    }

    #[test]
    fn impossible_cron_returns_none() {
        // Feb 30 never exists.
        let from = date(2026, 1, 1)
            .at(0, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap();
        assert!(sched("0 0 30 2 *").next_after(&from).is_none());
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_mins(30));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_hours(2));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_hours(24));
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
        assert!(parse_duration("2x").is_err());
        assert!(parse_duration("0m").is_err());
        assert!(parse_duration("h").is_err());
    }

    #[test]
    fn describe_round_trips_after() {
        let t = Trigger::After(parse_duration("2h").unwrap());
        assert_eq!(t.describe(), "after 2h");
        assert!(!t.is_recurring());
    }

    #[test]
    fn registry_schedules_lists_and_unschedules() {
        let reg = ScheduleRegistry::new();
        let inbox = Inbox::new();
        let id = reg
            .schedule(
                Trigger::Cron {
                    schedule: sched("0 3 * * *"),
                    expr: "0 3 * * *".into(),
                },
                "run tests".into(),
                Some("nightly".into()),
                inbox.mailbox(),
            )
            .unwrap();
        let live = reg.list();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].label, "nightly");
        assert_eq!(live[0].trigger, "0 3 * * *");
        assert!(reg.unschedule(id), "an existing schedule is removable");
        assert!(reg.list().is_empty());
        assert!(!reg.unschedule(id), "a removed schedule is gone");
    }

    #[test]
    fn after_fires_once_then_is_removed() {
        let reg = ScheduleRegistry::new();
        let inbox = Inbox::new();
        reg.schedule(
            Trigger::After(Duration::from_millis(40)),
            "ping".into(),
            None,
            inbox.mailbox(),
        )
        .unwrap();
        let mut fired = None;
        for _ in 0..200 {
            if let Some(turn) = inbox.drain_turn() {
                fired = Some(turn);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let turn = fired.expect("a one-shot `after` schedule must fire");
        assert!(
            matches!(&turn, Turn::Wakeup(_)),
            "delivered tagged as a wakeup turn, got {turn:?}"
        );
        let text = turn.text();
        assert!(text.contains("ping"), "delivered the prompt: {text}");
        assert!(text.starts_with("[scheduled"), "marked wakeup: {text}");
        assert!(
            reg.list().is_empty(),
            "a one-shot schedule is removed after firing"
        );
    }

    #[test]
    fn clear_drops_every_schedule() {
        let reg = ScheduleRegistry::new();
        let inbox = Inbox::new();
        reg.schedule(
            Trigger::Cron {
                schedule: sched("0 3 * * *"),
                expr: "0 3 * * *".into(),
            },
            "x".into(),
            None,
            inbox.mailbox(),
        )
        .unwrap();
        assert_eq!(reg.list().len(), 1);
        reg.clear();
        assert!(reg.list().is_empty(), "/clear drops schedules");
    }
}
