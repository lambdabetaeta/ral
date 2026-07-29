//! Scheduled wakeups: a five-field cron grammar, plus a one-shot relative
//! `after <dur>`.
//!
//! The grammar is parsed here rather than taken from a `chrono`-based crate
//! that would drag a second datetime tree in beside `jiff`.
//!
//! Cron is wall-clock, the reaper monotonic.  Every fire recomputes the next
//! absolute occurrence in the host timezone and arms the reaper with the
//! delta to it, so DST shifts, clock steps, and suspends are absorbed rather
//! than accumulated.

use crate::bus::{Mailbox, Post};
use crate::sync::LockExt;
use jiff::civil::DateTime;
use jiff::{ToSpan, Zoned};
use ral_core::process::{Deadline, arm_callback};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

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

/// Cron numbering, which `date_matches` meets with `to_sunday_zero_offset`.
const WEEKDAYS: &[(&str, u8)] = &[
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];

/// Cap on the next-occurrence search: a century of whole-day skips plus one
/// day of minutes.  Every valid cron fires well inside that, and a
/// parseable-but-impossible one (Feb 30) reaches `None` quickly.
const MAX_STEPS: usize = 366 * 100 + 1_440;

/// A parsed five-field cron: one allowed-value bitmask per field, plus
/// whether each day field was `*`, which selects the day-matching rule.
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
    /// Parse `minute hour dom month dow`, each field a comma list of `*`,
    /// `N`, `a-b`, `*/step`, `a-b/step`, or `N/step` (N to the field max).
    /// Month and day-of-week also accept three-letter names; day-of-week
    /// accepts `7` for Sunday.
    ///
    /// # Errors
    /// Wrong field count, or a malformed field.
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
        // 7 is a second spelling of Sunday: admit it, then fold its bit onto
        // 0 so the matcher reads one.
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
        // Cron fires on minute boundaries; the current partial minute is past.
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
                // No time on a non-matching date can fire, so skip the day
                // rather than its 1,440 minutes — the skip `MAX_STEPS` counts on.
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
        // Vixie-cron OR semantics: with both day fields restricted, either
        // one matching is enough.
        match (self.dom_star, self.dow_star) {
            (true, true) => true,
            (false, true) => dom_ok,
            (true, false) => dow_ok,
            (false, false) => dom_ok || dow_ok,
        }
    }
}

/// Whether bit `v` is set in `mask`; `v` is a `jiff` field, hence the guard.
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
        loop {
            mask |= 1 << v;
            match v.checked_add(step) {
                Some(next) if next <= hi => v = next,
                _ => break,
            }
        }
    }
    Ok(mask)
}

/// Resolve one cron atom — a number or a name — to a value in `[min, max]`.
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

/// What makes a wakeup fire: a recurring cron, or a one-shot delay from
/// arming time.  `expr` is the cron source text, kept for display.
#[derive(Debug, Clone)]
pub enum Trigger {
    Cron {
        schedule: CronSchedule,
        expr: String,
    },
    After(Duration),
}

impl Trigger {
    /// Whether the trigger re-arms after firing.
    pub fn is_recurring(&self) -> bool {
        matches!(self, Self::Cron { .. })
    }

    /// The delay from now to the next fire, recomputed against the host
    /// timezone on every call.  `None` past a cron's search horizon.
    pub fn next_delay(&self) -> Option<Duration> {
        match self {
            Self::After(d) => Some(*d),
            Self::Cron { schedule, .. } => {
                let now = Zoned::now();
                let next = schedule.next_after(&now)?;
                let secs = next.timestamp().duration_since(now.timestamp()).as_secs();
                #[allow(
                    clippy::cast_sign_loss,
                    reason = "max(0) floors the delay to a non-negative seconds count"
                )]
                let secs = secs.max(0) as u64;
                Some(Duration::from_secs(secs))
            }
        }
    }

    /// The trigger as text, for the `schedules` listing and the wakeup.
    pub fn describe(&self) -> String {
        match self {
            Self::Cron { expr, .. } => expr.clone(),
            Self::After(d) => format!("after {}", fmt_duration(*d)),
        }
    }
}

/// Parse a relative delay for `after`: an integer and one of `s`, `m`, `h`,
/// `d` (e.g. `30m`, `2h`).
///
/// # Errors
/// A missing or unknown unit, a non-numeric count, or a zero duration.
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

/// The inverse of [`parse_duration`], in the coarsest unit that divides.
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

/// A schedule's id: monotonic, and never reused, not even across `/clear`.
pub type ScheduleId = u64;

/// A snapshot row for the `schedules` listing.
pub struct ScheduleInfo {
    pub label: String,
    pub trigger: String,
    /// `None` for a cron with no further occurrence.
    pub next_in: Option<Duration>,
    pub fires: u64,
}

/// What [`ScheduleRegistry::schedule`] answers: the resolved label — the
/// caller's own, or the minted default — and the delay to the first fire.
///
/// Model-facing, a schedule is known by its label and nothing else.
#[derive(Debug)]
pub struct ScheduleReceipt {
    pub label: String,
    pub next_in: Duration,
}

/// Whether `label` has the `sched-<digits>` shape minted defaults use.
/// [`ScheduleRegistry::schedule`] refuses a user label of this shape, which
/// is what makes every minted default collision-free by construction.
fn is_reserved_label(label: &str) -> bool {
    label
        .strip_prefix("sched-")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// A session's live scheduled wakeups.  Cheap to clone, and the clone shares
/// the map: the reaper closure holds one, and re-arms and posts through it
/// long after the run that armed it has ended.
#[derive(Clone)]
pub struct ScheduleRegistry {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    /// Never reset, so a fire armed before a `/clear` cannot reach a
    /// schedule created after it.
    next_id: ScheduleId,
    entries: HashMap<ScheduleId, Entry>,
}

struct Entry {
    trigger: Trigger,
    prompt: String,
    label: String,
    /// Shared with the in-flight wakeup: set on post, cleared when the inbox
    /// drains it, read for the overlap-skip.
    pending: Arc<AtomicBool>,
    fires: u64,
    /// The next occurrence, armed on the reaper; dropping it disarms.
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

    /// Add a schedule and arm its first occurrence on the reaper.  `label`
    /// defaults to `sched-{id}`; a supplied one is refused if a live schedule
    /// already bears it, or if it has the reserved shape [`is_reserved_label`]
    /// names.  `mailbox` is the owning session's — a session schedules only
    /// itself.
    ///
    /// # Errors
    /// A trigger with no next occurrence, or a taken or reserved `label`.
    pub fn schedule(
        &self,
        trigger: Trigger,
        prompt: String,
        label: Option<String>,
        mailbox: &Mailbox,
    ) -> Result<ScheduleReceipt, String> {
        if let Some(label) = &label
            && is_reserved_label(label)
        {
            return Err(format!(
                "label '{label}': the sched-<n> form is reserved for default labels — pick another name"
            ));
        }
        let delay = trigger
            .next_delay()
            .ok_or_else(|| "this trigger has no next occurrence".to_string())?;
        let mut g = self.lock();
        if let Some(label) = &label
            && g.entries.values().any(|e| &e.label == label)
        {
            return Err(format!(
                "label '{label}' is already borne by a live schedule — pick another, or unschedule it first"
            ));
        }
        let id = g.next_id;
        g.next_id += 1;
        let label = label.unwrap_or_else(|| format!("sched-{id}"));
        let deadline = self.arm_deadline(id, mailbox, delay);
        g.entries.insert(
            id,
            Entry {
                trigger,
                prompt,
                label: label.clone(),
                pending: Arc::new(AtomicBool::new(false)),
                fires: 0,
                deadline,
            },
        );
        drop(g);
        Ok(ScheduleReceipt {
            label,
            next_in: delay,
        })
    }

    /// Remove one schedule by label; `true` if a live one bore it, and
    /// dropping its entry disarms its deadline.  `false` is no evidence of a
    /// caller mistake — a one-shot may have fired and removed itself since
    /// the label was read — so callers treat it as a successful no-op.
    pub fn unschedule(&self, label: &str) -> bool {
        let mut g = self.lock();
        let Some(id) = g
            .entries
            .iter()
            .find_map(|(id, e)| (e.label == label).then_some(*id))
        else {
            return false;
        };
        g.entries.remove(&id).is_some()
    }

    /// Whether any schedule is live.  At quiescence the attend loop parks for
    /// the next wakeup rather than terminating when this holds.
    pub fn armed(&self) -> bool {
        !self.lock().entries.is_empty()
    }

    /// Snapshot the live schedules, ordered by id.
    pub fn list(&self) -> Vec<ScheduleInfo> {
        let mut rows: Vec<(ScheduleId, ScheduleInfo)> = {
            let g = self.lock();
            g.entries
                .iter()
                .map(|(&id, e)| {
                    (
                        id,
                        ScheduleInfo {
                            label: e.label.clone(),
                            trigger: e.trigger.describe(),
                            next_in: e.trigger.next_delay(),
                            fires: e.fires,
                        },
                    )
                })
                .collect()
        };
        rows.sort_unstable_by_key(|(id, _)| *id);
        rows.into_iter().map(|(_, info)| info).collect()
    }

    /// `/clear`: drop every schedule, disarming each deadline.  The id source
    /// is left alone, for the reason `Inner::next_id` gives.
    pub fn clear(&self) {
        self.lock().entries.clear();
    }

    /// Arm one occurrence on the reaper.  Takes no registry lock, so `fire`
    /// may call it under one.
    fn arm_deadline(&self, id: ScheduleId, mailbox: &Mailbox, delay: Duration) -> Deadline {
        let reg = self.clone();
        let mailbox = mailbox.clone();
        arm_callback(delay, move || reg.fire(id, &mailbox))
    }

    /// The reaper fired this schedule: post a wakeup unless a previous one is
    /// still pending (the overlap-skip), then re-arm, or drop a spent
    /// one-shot.  Runs on the reaper thread outside its heap lock, so
    /// re-arming here is safe; the push waits for this guard to drop, since a
    /// park verdict reads `armed()` under the inbox mutex and the lock order
    /// is inbox → registry.  Composing and pushing straddle that drop, so a
    /// `/clear` can fall between them: the message carries the epoch read at
    /// composition and the inbox's pop-time check settles the two orderings.
    fn fire(&self, id: ScheduleId, mailbox: &Mailbox) {
        let mut g = self.lock();
        let Some(entry) = g.entries.get_mut(&id) else {
            return; // unscheduled or cleared between arming and firing
        };
        let recurring = entry.trigger.is_recurring();
        let msg = if entry.pending.swap(true, Ordering::AcqRel) {
            None
        } else {
            entry.fires += 1;
            Some(Post::ScheduledWakeup {
                id,
                label: entry.label.clone(),
                trigger: entry.trigger.describe(),
                prompt: entry.prompt.clone(),
                pending: entry.pending.clone(),
                epoch: mailbox.epoch(),
            })
        };
        if recurring {
            if let Some(delay) = entry.trigger.next_delay() {
                entry.deadline = self.arm_deadline(id, mailbox, delay);
            } else {
                g.entries.remove(&id);
            }
        } else {
            g.entries.remove(&id);
        }
        drop(g);
        if let Some(msg) = msg {
            mailbox
                .push(msg)
                .expect("ScheduledWakeup is idempotent and never rejects");
        }
    }

    /// Poison-recovering: every operation under it is total, so a panicked
    /// holder leaves the map usable.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock_ignore_poison()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{Inbox, Item};
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
        let s = sched("* * * * *");
        assert!(s.dom_star && s.dow_star);
        assert_eq!(sched("0 9 * * 1-5"), sched("0 9 * * mon-fri"));
        let half = sched("*/30 * * * *");
        assert!(bit(half.minute, 0) && bit(half.minute, 30) && !bit(half.minute, 15));
        let list = sched("0 0,12 * * *");
        assert!(bit(list.hour, 0) && bit(list.hour, 12) && !bit(list.hour, 6));
        assert_eq!(sched("0 0 1 jan *"), sched("0 0 1 1 *"));
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
    fn oversized_step_does_not_overflow() {
        // A step wider than the field must not overflow the u8 increment:
        // the walk sets the low bound, then stops.
        let s = sched("59/200 * * * *");
        assert!(bit(s.minute, 59) && !bit(s.minute, 0));
        let s = sched("*/200 * * * *");
        assert!(bit(s.minute, 0) && !bit(s.minute, 59));
    }

    #[test]
    fn next_after_finds_the_following_daily_occurrence() {
        let from = date(2026, 6, 20)
            .at(10, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap();
        let next = sched("0 3 * * *").next_after(&from).unwrap();
        assert_eq!(next.year(), 2026);
        assert_eq!(next.month(), 6);
        assert_eq!(next.day(), 21);
        assert_eq!(next.hour(), 3);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn next_after_respects_weekday_restriction() {
        // 2026-06-20 is a Saturday, so weekdays-at-09:00 waits for Monday.
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
        // Feb 30 parses, but no date bears it.
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
        let receipt = reg
            .schedule(
                Trigger::Cron {
                    schedule: sched("0 3 * * *"),
                    expr: "0 3 * * *".into(),
                },
                "run tests".into(),
                Some("nightly".into()),
                &inbox.mailbox(),
            )
            .unwrap();
        assert_eq!(receipt.label, "nightly");
        let live = reg.list();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].label, "nightly");
        assert_eq!(live[0].trigger, "0 3 * * *");
        assert!(
            reg.unschedule("nightly"),
            "an existing schedule is removable"
        );
        assert!(reg.list().is_empty());
        assert!(!reg.unschedule("nightly"), "a removed schedule is gone");
    }

    #[test]
    fn schedule_rejects_a_label_already_borne_by_a_live_schedule() {
        let reg = ScheduleRegistry::new();
        let inbox = Inbox::new();
        reg.schedule(
            Trigger::After(Duration::from_mins(1)),
            "x".into(),
            Some("nightly".into()),
            &inbox.mailbox(),
        )
        .unwrap();
        let err = reg
            .schedule(
                Trigger::After(Duration::from_mins(1)),
                "y".into(),
                Some("nightly".into()),
                &inbox.mailbox(),
            )
            .expect_err("a duplicate label must be refused");
        assert!(
            err.contains("nightly"),
            "must name the offending label, got: {err}"
        );
        assert!(
            err.contains("already borne"),
            "must name the rule, got: {err}"
        );
        assert_eq!(
            reg.list().len(),
            1,
            "the duplicate attempt registers nothing"
        );
    }

    #[test]
    fn schedule_rejects_a_user_label_shaped_like_a_default() {
        let reg = ScheduleRegistry::new();
        let inbox = Inbox::new();
        let err = reg
            .schedule(
                Trigger::After(Duration::from_mins(1)),
                "x".into(),
                Some("sched-3".into()),
                &inbox.mailbox(),
            )
            .expect_err("the reserved sched-<n> shape must be refused");
        assert!(
            err.contains("sched-3"),
            "must name the offending label, got: {err}"
        );
        assert!(err.contains("reserved"), "must name the rule, got: {err}");
        assert!(
            reg.list().is_empty(),
            "the refused attempt registers nothing"
        );
    }

    #[test]
    fn unschedule_by_label_removes_the_match_and_is_a_noop_otherwise() {
        let reg = ScheduleRegistry::new();
        let inbox = Inbox::new();
        reg.schedule(
            Trigger::After(Duration::from_mins(1)),
            "x".into(),
            Some("nightly".into()),
            &inbox.mailbox(),
        )
        .unwrap();
        assert!(
            !reg.unschedule("no-such-label"),
            "an unborne label is a no-op"
        );
        assert!(reg.unschedule("nightly"), "an existing label is removable");
        assert!(reg.list().is_empty());
        assert!(
            !reg.unschedule("nightly"),
            "a removed label is a no-op, not an error"
        );
    }

    #[test]
    fn after_fires_once_then_is_removed() {
        let reg = ScheduleRegistry::new();
        let inbox = Inbox::new();
        reg.schedule(
            Trigger::After(Duration::from_millis(40)),
            "ping".into(),
            None,
            &inbox.mailbox(),
        )
        .unwrap();
        let mut fired = None;
        for _ in 0..200 {
            if let Some(item) = inbox.next_item() {
                fired = Some(item);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let item = fired.expect("a one-shot `after` schedule must fire");
        assert!(
            matches!(&item, Item::Wakeup(_)),
            "delivered tagged as a wakeup item, got {item:?}"
        );
        let text = item.text();
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
            &inbox.mailbox(),
        )
        .unwrap();
        assert_eq!(reg.list().len(), 1);
        reg.clear();
        assert!(reg.list().is_empty(), "/clear drops schedules");
    }
}
