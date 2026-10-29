//! Scheduled wakeups: a five-field cron grammar, a one-shot relative
//! `after <dur>`, and the harness's own one-shot `At`, a wall-clock instant.
//!
//! The grammar is parsed here rather than taken from a `chrono`-based crate
//! that would drag a second datetime tree in beside `jiff`.
//!
//! Every occurrence is a wall-clock instant — a cron's next in the host
//! timezone, an `after`'s counted from when it was armed, an `At` as given.
//! The reaper's clock is monotonic and stops while the machine sleeps, so each
//! entry is armed toward its instant at most [`WALL_CHECK`] at a time, and
//! every wake re-reads the wall clock: after a suspend a fire is at most that
//! late, and a clock step moves it with the clock.

use crate::bus::{AgentAddress, Refused, ScheduleId, WeakAgentAddress};
use crate::clock;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan, Zoned};
use ral_core::process::{Deadline, arm_callback};
use ral_core::sync::LockExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// The longest the reaper sleeps toward a wall-clock instant. Its clock
/// stops while the machine sleeps, so after a wake a fire is late by at most
/// this.
const WALL_CHECK: Duration = Duration::from_mins(1);

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
    /// One-shot at a wall-clock instant: armed by the harness, never parsed.
    At(Timestamp),
}

impl Trigger {
    /// Whether the trigger re-arms after firing.
    pub fn is_recurring(&self) -> bool {
        matches!(self, Self::Cron { .. })
    }

    /// The instant of the next fire after `now`; `None` past a cron's search horizon.
    pub fn next_after(&self, now: Timestamp) -> Option<Timestamp> {
        match self {
            Self::Cron { schedule, .. } => Some(
                schedule
                    .next_after(&now.to_zoned(TimeZone::system()))?
                    .timestamp(),
            ),
            Self::After(d) => now.checked_add(*d).ok(),
            Self::At(t) => Some(*t),
        }
    }

    /// The trigger as text, for the `exarch-schedules` listing and the wakeup.
    pub fn describe(&self) -> String {
        match self {
            Self::Cron { expr, .. } => expr.clone(),
            Self::After(d) => format!("after {}", fmt_duration(*d)),
            Self::At(t) => format!("at {}", clock::local(*t)),
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
    let mult: u64 = match unit.trim() {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        other => return Err(format!("unknown duration unit `{other}` (use s/m/h/d)")),
    };
    let secs = n
        .checked_mul(mult)
        .ok_or_else(|| format!("duration `{s}` is too large"))?;
    if secs == 0 {
        return Err("duration must be greater than zero".into());
    }
    Ok(Duration::from_secs(secs))
}

/// The inverse of [`parse_duration`], in the coarsest unit that divides.
pub(crate) fn fmt_duration(d: Duration) -> String {
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

/// A snapshot row for the `exarch-schedules` listing.
pub struct ScheduleInfo {
    pub label: String,
    pub trigger: String,
    /// Seconds until the next fire; zero once overdue.
    pub next_s: u64,
    pub fires: u64,
}

/// What [`ScheduleRegistry::schedule`] answers: the label the caller named,
/// and the delay to the first fire.
///
/// Model-facing, a schedule is known by its label and nothing else.
#[derive(Debug)]
pub struct ScheduleReceipt {
    pub label: String,
    pub next_in: Duration,
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
    next_id: u64,
    epoch: u64,
    entries: HashMap<ScheduleId, Entry>,
}

struct Entry {
    trigger: Trigger,
    prompt: String,
    label: String,
    fires: u64,
    /// The wall-clock instant of the next fire.
    next: Timestamp,
    /// The next occurrence, armed on the reaper; dropping it disarms.
    deadline: Deadline,
    address: WeakAgentAddress,
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
                epoch: 0,
                entries: HashMap::new(),
            })),
        }
    }

    /// Add a schedule and arm its first occurrence on the reaper.  `label` is
    /// refused if a live schedule already bears it. `address` belongs to the
    /// scheduling agent itself.
    ///
    /// # Errors
    /// A trigger with no next occurrence, or a `label` already borne by a
    /// live schedule.
    pub(crate) fn schedule(
        &self,
        trigger: Trigger,
        prompt: String,
        label: String,
        address: &AgentAddress,
    ) -> Result<ScheduleReceipt, String> {
        let now = Timestamp::now();
        let next = trigger
            .next_after(now)
            .ok_or_else(|| "this trigger has no next occurrence".to_string())?;
        let address = address.downgrade();
        let mut g = self.lock();
        if g.entries.values().any(|e| e.label == label) {
            return Err(format!(
                "label '{label}' is already borne by a live schedule: pick another, or `` exarch-schedules `remove `` it first"
            ));
        }
        let id = ScheduleId::new(g.next_id);
        g.next_id += 1;
        let deadline = self.arm_deadline(id, next);
        g.entries.insert(
            id,
            Entry {
                trigger,
                prompt,
                label: label.clone(),
                fires: 0,
                next,
                deadline,
                address,
            },
        );
        drop(g);
        Ok(ScheduleReceipt {
            label,
            next_in: clock::until(next, now),
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

    /// Whether a schedule owes work, including terminal publication.
    /// At quiescence the attend loop parks while this holds.
    pub fn armed(&self) -> bool {
        !self.lock().entries.is_empty()
    }

    /// Snapshot the live schedules, ordered by id.
    pub fn list(&self) -> Vec<ScheduleInfo> {
        let now = Timestamp::now();
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
                            next_s: clock::until(e.next, now).as_secs(),
                            fires: e.fires,
                        },
                    )
                })
                .collect()
        };
        rows.sort_unstable_by_key(|(id, _)| id.get());
        rows.into_iter().map(|(_, info)| info).collect()
    }

    /// The epoch stamped on wakeups, including ones awaiting publication.
    pub(crate) fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// Disarm every schedule and invalidate wakeups already composed.
    pub fn clear(&self) {
        let mut g = self.lock();
        g.epoch += 1;
        g.entries.clear();
    }

    /// Arm the reaper toward `next`, at most [`WALL_CHECK`] ahead.  Takes no
    /// registry lock, so `fire` may call it under one.
    fn arm_deadline(&self, id: ScheduleId, next: Timestamp) -> Deadline {
        let reg = self.clone();
        let delay = clock::until(next, Timestamp::now()).min(WALL_CHECK);
        arm_callback(delay, move || reg.fire_at(id, Timestamp::now()))
    }

    /// The mailbox transaction orders publication with terminal retirement.
    fn fire_at(&self, id: ScheduleId, now: Timestamp) {
        let address = self
            .lock()
            .entries
            .get(&id)
            .map(|entry| entry.address.clone());
        let Some(address) = address.and_then(|address| address.upgrade()) else {
            self.lock().entries.remove(&id);
            return;
        };
        #[cfg(test)]
        tests::before_fire_transaction();
        let result = address.post_wakeup_with(id, |already_queued| {
            let mut g = self.lock();
            let epoch = g.epoch;
            let entry = g.entries.get_mut(&id)?;
            if now < entry.next {
                entry.deadline = self.arm_deadline(id, entry.next);
                return None;
            }
            let wakeup = (!already_queued).then(|| {
                entry.fires += 1;
                (
                    epoch,
                    format!(
                        "[scheduled '{}' · {}] {}",
                        entry.label,
                        entry.trigger.describe(),
                        entry.prompt,
                    ),
                )
            });
            let next = entry
                .trigger
                .is_recurring()
                .then(|| entry.trigger.next_after(now))
                .flatten();
            if let Some(next) = next {
                entry.next = next;
                entry.deadline = self.arm_deadline(id, next);
            } else {
                g.entries.remove(&id);
            }
            wakeup
        });
        if matches!(result, Err(Refused::Ended)) {
            self.lock().entries.remove(&id);
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
    use crate::agent::Trunk;
    use crate::agent::nursery::Joined;
    use crate::agent::testkit::{host, ral_call, scripted, set_provider, trunk};
    use crate::bus::{AgentMailbox, Drained, Input, Next, Rest, Signal};
    use crate::provider::scripted::{Reply, Script};
    use crate::record::{
        AgentState, ChildStatus, Forensic, JobId, Protocol, Record, Transient, Voice,
    };
    use jiff::civil::date;
    use ral_core::process::{CancelCause, DurableRoot};

    thread_local! {
        static BEFORE_FIRE_TRANSACTION: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::Cell::new(None) };
    }

    pub(super) fn before_fire_transaction() {
        if let Some(hook) = BEFORE_FIRE_TRANSACTION.take() {
            hook();
        }
    }

    fn mailbox() -> AgentMailbox {
        let life = DurableRoot::default().child();
        let record = crate::record::Emitter::none();
        record.bind(life.clone());
        AgentMailbox::new(
            life,
            record,
            crate::agent::Admission::Free,
            crate::provider::Pick::of(&crate::provider::Provider::scripted(
                "test-model",
                crate::provider::scripted::Script::new(),
            )),
        )
    }

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
        let mailbox = mailbox();
        let receipt = reg
            .schedule(
                Trigger::Cron {
                    schedule: sched("0 3 * * *"),
                    expr: "0 3 * * *".into(),
                },
                "run tests".into(),
                "nightly".into(),
                &mailbox.address(),
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
        let mailbox = mailbox();
        reg.schedule(
            Trigger::After(Duration::from_mins(1)),
            "x".into(),
            "nightly".into(),
            &mailbox.address(),
        )
        .unwrap();
        let err = reg
            .schedule(
                Trigger::After(Duration::from_mins(1)),
                "y".into(),
                "nightly".into(),
                &mailbox.address(),
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
    fn unschedule_by_label_removes_the_match_and_is_a_noop_otherwise() {
        let reg = ScheduleRegistry::new();
        let mailbox = mailbox();
        reg.schedule(
            Trigger::After(Duration::from_mins(1)),
            "x".into(),
            "nightly".into(),
            &mailbox.address(),
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
        let mailbox = mailbox();
        reg.schedule(
            Trigger::After(Duration::from_millis(40)),
            "ping".into(),
            "ping".into(),
            &mailbox.address(),
        )
        .unwrap();
        let fired = ral_core::test_helper::eventually(SYNC_TIMEOUT, || {
            (!reg.armed()).then(|| mailbox.drain())
        })
        .expect("a one-shot `after` schedule must fire");
        let [Drained::Input(Input::Wakeup { epoch, text, .. })] = fired.as_slice() else {
            panic!("exactly one tagged wakeup must be delivered");
        };
        assert_eq!(*epoch, reg.epoch());
        assert!(text.contains("ping"), "delivered the prompt: {text}");
        assert!(text.starts_with("[scheduled"), "marked wakeup: {text}");
        assert!(
            reg.list().is_empty(),
            "a one-shot schedule is removed after firing"
        );
    }

    #[test]
    fn a_fire_before_its_instant_rearms_and_posts_nothing() {
        let reg = ScheduleRegistry::new();
        let mailbox = mailbox();
        let at = Timestamp::now()
            .checked_add(Duration::from_hours(1))
            .unwrap();
        reg.schedule(
            Trigger::At(at),
            "later".into(),
            "later".into(),
            &mailbox.address(),
        )
        .unwrap();
        reg.fire_at(ScheduleId::new(0), Timestamp::now());
        assert!(mailbox.drain().is_empty(), "no wakeup is queued");
        let live = reg.list();
        assert_eq!(live.len(), 1, "the entry is still live");
        assert_eq!(live[0].fires, 0, "an early wake counts no fire");
    }

    #[test]
    fn list_reports_the_time_remaining() {
        let reg = ScheduleRegistry::new();
        let mailbox = mailbox();
        reg.schedule(
            Trigger::After(Duration::from_hours(2)),
            "x".into(),
            "x".into(),
            &mailbox.address(),
        )
        .unwrap();
        let next_in = Duration::from_secs(reg.list()[0].next_s);
        assert!(
            next_in > Duration::from_mins(119) && next_in <= Duration::from_hours(2),
            "got {next_in:?}"
        );
    }

    #[test]
    fn clear_drops_every_schedule() {
        let reg = ScheduleRegistry::new();
        let mailbox = mailbox();
        reg.schedule(
            Trigger::Cron {
                schedule: sched("0 3 * * *"),
                expr: "0 3 * * *".into(),
            },
            "x".into(),
            "x".into(),
            &mailbox.address(),
        )
        .unwrap();
        assert_eq!(reg.list().len(), 1);
        let epoch = reg.epoch();
        reg.clear();
        assert!(reg.list().is_empty(), "/clear drops schedules");
        assert_eq!(reg.epoch(), epoch + 1, "/clear invalidates old wakeups");
    }

    const SYNC_TIMEOUT: Duration = Duration::from_secs(5);
    const QUIET: Rest = Rest {
        pending: false,
        schedules: false,
    };

    #[derive(Clone, Copy)]
    enum Terminal {
        After,
        At,
        ExhaustedCron,
    }

    fn terminal(family: Terminal) -> (ScheduleRegistry, AgentMailbox, Timestamp) {
        let registry = ScheduleRegistry::new();
        let mailbox = mailbox();
        let future = Timestamp::now()
            .checked_add(Duration::from_hours(2))
            .unwrap();
        registry
            .schedule(
                Trigger::At(future),
                "last wakeup".into(),
                "last".into(),
                &mailbox.address(),
            )
            .unwrap();
        let now = if matches!(family, Terminal::ExhaustedCron) {
            Timestamp::MAX
        } else {
            Timestamp::now()
        };
        {
            let mut guard = registry.lock();
            let entry = guard.entries.get_mut(&ScheduleId::new(0)).unwrap();
            entry.next = now;
            entry.trigger = match family {
                Terminal::After => Trigger::After(Duration::from_hours(2)),
                Terminal::At => Trigger::At(now),
                Terminal::ExhaustedCron => Trigger::Cron {
                    schedule: sched("* * * * *"),
                    expr: "* * * * *".into(),
                },
            };
            let no_next = entry.trigger.next_after(now).is_none();
            drop(guard);
            if matches!(family, Terminal::ExhaustedCron) {
                assert!(no_next);
            }
        }
        (registry, mailbox, now)
    }

    fn paused_fire(
        registry: &ScheduleRegistry,
        now: Timestamp,
    ) -> (Joined<()>, std::sync::mpsc::Sender<()>) {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let registry = registry.clone();
        let worker = Joined::new(std::thread::spawn(move || {
            BEFORE_FIRE_TRANSACTION.set(Some(Box::new(move || {
                ready_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(SYNC_TIMEOUT)
                    .expect("release the fire transaction");
            })));
            registry.fire_at(ScheduleId::new(0), now);
        }));
        ready_rx
            .recv_timeout(SYNC_TIMEOUT)
            .expect("fire must reach its mailbox transaction");
        (worker, release_tx)
    }

    struct CancelConsumer(AgentAddress);

    impl Drop for CancelConsumer {
        fn drop(&mut self) {
            self.0.end(CancelCause::Cancelled);
        }
    }

    #[test]
    fn every_terminal_family_queues_its_wakeup_before_the_job_can_settle() {
        for family in [Terminal::After, Terminal::At, Terminal::ExhaustedCron] {
            let (registry, mailbox, now) = terminal(family);
            assert!(mailbox.begin());
            let (producer, release) = paused_fire(&registry, now);
            std::thread::scope(|scope| {
                let _cancel = CancelConsumer(mailbox.address());
                let (park_tx, park_rx) = std::sync::mpsc::channel();
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let consumer = {
                    let mailbox = &mailbox;
                    let registry = &registry;
                    scope.spawn(move || {
                        let rest = Rest {
                            pending: false,
                            schedules: registry.armed(),
                        };
                        park_tx.send(rest.schedules).unwrap();
                        assert!(done_tx.send(mailbox.next(rest)).is_ok());
                    })
                };
                assert!(park_rx.recv_timeout(SYNC_TIMEOUT).unwrap());
                assert!(
                    registry.armed(),
                    "the transaction has not retired the entry"
                );
                assert!(matches!(
                    done_rx.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ));
                release.send(()).unwrap();
                let next = done_rx
                    .recv_timeout(SYNC_TIMEOUT)
                    .expect("terminal publication must reach its waiting consumer");
                assert!(matches!(next, Next::Take {
                    input: Input::Wakeup { text, epoch: 0, .. }, ..
                } if text.contains("last wakeup")));
                producer.join().unwrap();
                consumer.join().unwrap();
                assert!(!registry.armed());
                assert!(matches!(mailbox.next(QUIET), Next::Settled { .. }));
            });
        }
    }

    #[test]
    fn clear_before_the_fire_transaction_suppresses_the_old_id_and_keeps_the_new_one() {
        let (registry, mailbox, now) = terminal(Terminal::After);
        let (producer, release) = paused_fire(&registry, now);
        registry.clear();
        mailbox.clear();
        registry
            .schedule(
                Trigger::After(Duration::from_hours(2)),
                "new task".into(),
                "last".into(),
                &mailbox.address(),
            )
            .unwrap();
        release.send(()).unwrap();
        producer.join().unwrap();
        assert!(mailbox.drain().is_empty(), "the cleared ID cannot publish");
        let guard = registry.lock();
        assert_eq!(guard.epoch, 1);
        assert_eq!(guard.entries.len(), 1);
        assert_eq!(guard.entries[&ScheduleId::new(1)].prompt, "new task");
    }

    #[test]
    fn unschedule_before_the_fire_transaction_suppresses_publication() {
        let (registry, mailbox, now) = terminal(Terminal::After);
        let (producer, release) = paused_fire(&registry, now);
        assert!(registry.unschedule("last"));
        assert!(!registry.armed());
        release.send(()).unwrap();
        producer.join().unwrap();
        assert!(mailbox.drain().is_empty());
        assert!(!registry.armed());
    }

    #[test]
    fn unschedule_after_publication_does_not_recall_the_queued_wakeup() {
        let (registry, mailbox, now) = terminal(Terminal::After);
        registry.fire_at(ScheduleId::new(0), now);
        assert!(
            !registry.unschedule("last"),
            "the terminal entry already retired"
        );
        assert!(matches!(mailbox.drain().as_slice(),
            [Drained::Input(Input::Wakeup { text, .. })] if text.contains("last wakeup")
        ));
    }

    #[test]
    fn a_terminal_overlap_retires_without_posting_a_second_wakeup() {
        for family in [Terminal::After, Terminal::At, Terminal::ExhaustedCron] {
            let (registry, mailbox, now) = terminal(family);
            mailbox
                .address()
                .post(Input::Wakeup {
                    id: ScheduleId::new(0),
                    epoch: registry.epoch(),
                    text: "already queued".into(),
                })
                .unwrap();
            registry.fire_at(ScheduleId::new(0), now);
            assert!(!registry.armed());
            assert!(matches!(mailbox.drain().as_slice(),
                [Drained::Input(Input::Wakeup { text, .. })] if text == "already queued"
            ));
        }
    }

    #[test]
    fn racing_terminal_callbacks_publish_only_one_wakeup() {
        let (registry, mailbox, now) = terminal(Terminal::After);
        let (first, release_first) = paused_fire(&registry, now);
        let (second, release_second) = paused_fire(&registry, now);
        release_first.send(()).unwrap();
        release_second.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        assert!(!registry.armed());
        assert!(matches!(
            mailbox.drain().as_slice(),
            [Drained::Input(Input::Wakeup { .. })]
        ));
    }

    #[test]
    fn recurring_publication_stays_armed_and_skips_queued_overlap() {
        let (registry, mailbox, now) = terminal(Terminal::After);
        let now = now.checked_add(Duration::from_hours(2)).unwrap();
        registry
            .lock()
            .entries
            .get_mut(&ScheduleId::new(0))
            .unwrap()
            .trigger = Trigger::Cron {
            schedule: sched("* * * * *"),
            expr: "* * * * *".into(),
        };
        registry.fire_at(ScheduleId::new(0), now);
        assert!(registry.armed());
        let next = registry.lock().entries[&ScheduleId::new(0)].next;
        registry.fire_at(ScheduleId::new(0), next);
        assert_eq!(registry.list()[0].fires, 1);
        assert!(matches!(
            mailbox.drain().as_slice(),
            [Drained::Input(Input::Wakeup { .. })]
        ));
        assert!(registry.armed());
        let next = registry.lock().entries[&ScheduleId::new(0)].next;
        registry.fire_at(ScheduleId::new(0), next);
        assert_eq!(
            registry.list()[0].fires,
            2,
            "taking up input permits another fire"
        );
        assert!(matches!(
            mailbox.drain().as_slice(),
            [Drained::Input(Input::Wakeup { .. })]
        ));
    }

    #[test]
    fn a_recurring_fire_for_an_ended_life_retires_without_publication_or_rearm() {
        let (registry, mailbox, now) = terminal(Terminal::After);
        registry
            .lock()
            .entries
            .get_mut(&ScheduleId::new(0))
            .unwrap()
            .trigger = Trigger::Cron {
            schedule: sched("* * * * *"),
            expr: "* * * * *".into(),
        };
        mailbox.address().end(CancelCause::Cancelled);
        assert!(
            registry.armed(),
            "ending a life leaves timer retirement to fire"
        );
        registry.fire_at(ScheduleId::new(0), now);
        assert!(mailbox.drain().is_empty());
        assert!(!registry.armed(), "the recurring entry is not rearmed");
    }

    #[test]
    fn a_scheduled_job_can_end_before_the_fire_transaction() {
        let (registry, mailbox, now) = terminal(Terminal::After);
        assert!(mailbox.begin());
        let (producer, release) = paused_fire(&registry, now);
        mailbox.address().end(CancelCause::Cancelled);
        assert!(matches!(
            mailbox.next(Rest {
                pending: false,
                schedules: registry.armed(),
            }),
            Next::Ended
        ));
        release.send(()).unwrap();
        producer.join().unwrap();
        assert!(mailbox.drain().is_empty());
        assert!(!registry.armed());
    }

    fn job_records(host: &crate::host::Host) -> Vec<Record> {
        let mut records = Vec::new();
        loop {
            match host
                .feed()
                .recv_timeout(SYNC_TIMEOUT)
                .expect("the job must settle")
            {
                Signal::Commit(_, commit) => {
                    let settled = commit.records().iter().any(|recorded| {
                        matches!(
                            recorded.value(),
                            Record::Forensic(Forensic::JobSettled { .. })
                        )
                    });
                    records.extend(
                        commit
                            .records()
                            .iter()
                            .map(|recorded| recorded.value().clone()),
                    );
                    if settled {
                        return records;
                    }
                }
                Signal::Transient(_, Transient::Died { .. } | Transient::Fault { .. }) => {
                    panic!("the root must stay live until its job settles");
                }
                _ => {}
            }
        }
    }

    fn settled_job(records: &[Record]) -> JobId {
        assert_eq!(
            records
                .iter()
                .filter(|record| { matches!(record, Record::Forensic(Forensic::JobOpened)) })
                .count(),
            1
        );
        records
            .iter()
            .find_map(|record| match record {
                Record::Forensic(Forensic::JobSettled { job, .. }) => Some(*job),
                _ => None,
            })
            .expect("a settled job")
    }

    #[test]
    fn a_conversing_roots_schedule_survives_settlement_and_opens_a_job_at_rest() {
        let mut agent = trunk(Trunk::Converses);
        let registry = agent.body.state.schedules.clone();
        registry
            .schedule(
                Trigger::After(Duration::from_hours(2)),
                "wake from rest".into(),
                "later".into(),
                &agent.mailbox.address(),
            )
            .unwrap();
        set_provider(
            &mut agent,
            scripted(
                "test-model",
                Script::new()
                    .then(Reply::text("first answer"))
                    .then(Reply::text("awake")),
            ),
        );
        agent.handle().steer("first prompt".into()).unwrap();
        let host = host(agent);
        let handle = host
            .fleet()
            .registry
            .get(host.roots()[0])
            .expect("a live root");
        let first = job_records(&host);
        let first_job = settled_job(&first);
        assert!(registry.armed(), "conversation schedules outlive their job");
        assert!(handle.status().job.is_none());
        let next = registry.lock().entries[&ScheduleId::new(0)].next;
        registry.fire_at(ScheduleId::new(0), next);
        let second = job_records(&host);
        assert_ne!(settled_job(&second), first_job);
        assert!(second.iter().any(|record| matches!(record,
            Record::Protocol(Protocol::UserPrompt { voice: Voice::Wakeup, text, .. })
                if text.contains("wake from rest")
        )));
        assert!(!registry.armed());
        assert_eq!(host.end(), [] as [String; 0]);
    }

    fn wait_for_scheduled_rest(host: &crate::host::Host) -> JobId {
        let mut first_job = None;
        loop {
            match host
                .feed()
                .recv_timeout(SYNC_TIMEOUT)
                .expect("the agent must wait for its schedule")
            {
                Signal::Commit(_, commit) => {
                    for recorded in commit.records() {
                        match recorded.value() {
                            Record::Forensic(Forensic::JobOpened) => {
                                assert!(
                                    first_job
                                        .replace(JobId::from(recorded.locus().seq().commit()))
                                        .is_none()
                                );
                            }
                            Record::Forensic(Forensic::Error { .. } | Forensic::Nudge { .. }) => {
                                panic!("an armed schedule must not produce errors or nudges");
                            }
                            Record::Forensic(
                                Forensic::JobSettled { .. } | Forensic::Status { .. },
                            ) => {
                                panic!("an armed schedule must keep the returning job open");
                            }
                            _ => {}
                        }
                    }
                }
                Signal::Transient(_, Transient::State(AgentState::Ready))
                    if first_job.is_some() =>
                {
                    break;
                }
                Signal::Transient(_, Transient::Died { .. } | Transient::Fault { .. }) => {
                    panic!("the scheduled root must stay live")
                }
                _ => {}
            }
        }
        first_job.expect("the human prompt opened a job")
    }

    #[test]
    fn a_returning_agents_schedule_holds_its_job_and_its_report_clears_all_schedules() {
        let mut agent = trunk(Trunk::Returns);
        let registry = agent.body.state.schedules.clone();
        for label in ["wake", "still armed"] {
            registry
                .schedule(
                    Trigger::After(Duration::from_hours(2)),
                    "last wakeup".into(),
                    label.into(),
                    &agent.mailbox.address(),
                )
                .unwrap();
        }
        let script = Script::new()
            .then(Reply::text("waiting for the wakeup"))
            .then(Reply::tool_calls(vec![ral_call(
                "reply",
                "exarch-agents `reply 'awake'",
            )]))
            .then(Reply::tool_calls(vec![ral_call(
                "fresh",
                "exarch-agents `reply 'fresh'",
            )]));
        set_provider(&mut agent, scripted("test-model", script));
        agent.handle().steer("start".into()).unwrap();
        let host = host(agent);
        let handle = host
            .fleet()
            .registry
            .get(host.roots()[0])
            .expect("a live root");
        let first_job = wait_for_scheduled_rest(&host);
        assert_eq!(handle.status().job, Some(first_job));
        assert_eq!(registry.list().len(), 2);
        let next = registry.lock().entries[&ScheduleId::new(0)].next;
        let (producer, release) = paused_fire(&registry, next);
        assert_eq!(handle.status().job, Some(first_job));
        release.send(()).unwrap();
        producer.join().unwrap();
        let records = job_records(&host);
        assert!(
            !records
                .iter()
                .any(|record| matches!(record, Record::Forensic(Forensic::JobOpened)))
        );
        assert!(records.iter().any(|record| matches!(record,
            Record::Forensic(Forensic::JobSettled { job, .. }) if *job == first_job
        )));
        assert!(records.iter().any(|record| matches!(record,
            Record::Forensic(Forensic::Status { status: ChildStatus::Replied(value) })
                if value == &ral_core::data::FOValue::String { value: "awake".into() }
        )));
        assert!(
            !registry.armed(),
            "the report clears even schedules that have not fired"
        );
        assert!(registry.epoch() > 0);
        assert!(records.iter().any(|record| matches!(record,
            Record::Protocol(Protocol::UserPrompt { voice: Voice::Wakeup, text, .. })
                if text.contains("last wakeup")
        )));
        host.port()
            .submit(host.roots()[0], "fresh prompt".into())
            .unwrap();
        let fresh = job_records(&host);
        assert_ne!(settled_job(&fresh), first_job);
        assert!(fresh.iter().any(|record| matches!(record,
            Record::Forensic(Forensic::Status { status: ChildStatus::Replied(value) })
                if value == &ral_core::data::FOValue::String { value: "fresh".into() }
        )));
        assert_eq!(host.end(), [] as [String; 0]);
    }

    #[test]
    fn a_wakeup_from_before_clear_is_declined_without_opening_a_job() {
        let mut agent = trunk(Trunk::Converses);
        let registry = agent.body.state.schedules.clone();
        registry
            .schedule(
                Trigger::After(Duration::from_hours(2)),
                "stale wakeup".into(),
                "old".into(),
                &agent.mailbox.address(),
            )
            .unwrap();
        let next = registry.lock().entries[&ScheduleId::new(0)].next;
        registry.fire_at(ScheduleId::new(0), next);
        let mut composed = agent.mailbox.drain();
        assert_eq!(composed.len(), 1);
        let Some(Drained::Input(old)) = composed.pop() else {
            panic!("a queued wakeup")
        };
        agent.clear().expect("a test engine can clear");
        assert_eq!(registry.epoch(), 1);
        agent.mailbox.address().post(old).unwrap();
        agent.handle().steer("fresh prompt".into()).unwrap();
        set_provider(
            &mut agent,
            scripted(
                "test-model",
                Script::new().then(Reply::text("fresh answer")),
            ),
        );
        let host = host(agent);
        let records = job_records(&host);
        settled_job(&records);
        let prompts: Vec<_> = records
            .iter()
            .filter_map(|record| match record {
                Record::Protocol(Protocol::UserPrompt { text, voice, .. }) => {
                    Some((text.as_str(), *voice))
                }
                _ => None,
            })
            .collect();
        assert_eq!(prompts, [("fresh prompt", Voice::Human)]);
        assert_eq!(host.end(), [] as [String; 0]);
    }
}
