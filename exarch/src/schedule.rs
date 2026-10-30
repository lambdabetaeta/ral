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

#[cfg(test)]
use crate::bus::AgentAddress;
use crate::bus::{ScheduleId, Txn, WeakAgentAddress};
use crate::clock;
use crate::record::{Schedule, Unavailable};
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan, Zoned};
use ral_core::process::{Deadline, arm_callback};
use std::collections::HashMap;
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

    /// The source expression of a recurring trigger.
    pub fn cron(&self) -> Option<&str> {
        match self {
            Self::Cron { expr, .. } => Some(expr),
            Self::After(_) | Self::At(_) => None,
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

impl ScheduleInfo {
    pub(crate) fn of(label: &str, schedule: &Schedule, now: Timestamp) -> Self {
        Self {
            label: label.to_string(),
            trigger: schedule.trigger.clone(),
            next_s: Duration::from_millis(schedule.next_unix_ms.saturating_sub(unix_ms(now)))
                .as_secs(),
            fires: schedule.fires,
        }
    }
}

fn unix_ms(at: Timestamp) -> u64 {
    u64::try_from(at.as_millisecond()).unwrap_or_default()
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

/// The owner's timers, one per booked label: a resource made from `Schedules`.
pub struct ScheduleRegistry {
    next_id: u64,
    by_label: HashMap<String, Timer>,
}

/// Replacing a timer drops and disarms its deadline.
struct Timer {
    id: ScheduleId,
    trigger: Trigger,
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
            next_id: 0,
            by_label: HashMap::new(),
        }
    }

    /// Commit the booking before arming its first occurrence.
    ///
    /// # Errors
    /// No next occurrence, a booked label, or a recording failure.
    pub(crate) fn schedule(
        &mut self,
        txn: &mut Txn<'_>,
        trigger: Trigger,
        prompt: String,
        label: String,
    ) -> Result<ScheduleReceipt, String> {
        let now = Timestamp::now();
        let next = trigger
            .next_after(now)
            .ok_or_else(|| "this trigger has no next occurrence".to_string())?;
        let schedule = Schedule {
            prompt,
            trigger: trigger.describe(),
            cron: trigger.cron().map(str::to_string),
            next_unix_ms: unix_ms(next),
            fires: 0,
        };
        if !txn
            .schedule(label.clone(), schedule)
            .map_err(|why| why.to_string())?
        {
            return Err(format!(
                "label '{label}' is already borne by a live schedule: pick another, or `` exarch-schedules `remove `` it first"
            ));
        }
        let id = ScheduleId::new(self.next_id);
        self.next_id += 1;
        let deadline = Self::arm_deadline(txn.weak(), label.clone(), id, unix_ms(next));
        drop(self.by_label.insert(
            label.clone(),
            Timer {
                id,
                trigger,
                deadline,
            },
        ));
        Ok(ScheduleReceipt {
            label,
            next_in: clock::until(next, now),
        })
    }

    /// An absent label is a successful no-op; dropping its timer disarms it.
    pub(crate) fn unschedule(
        &mut self,
        txn: &mut Txn<'_>,
        label: &str,
    ) -> Result<bool, Unavailable> {
        let removed = txn.unschedule(label)?;
        drop(self.by_label.remove(label));
        Ok(removed)
    }

    /// Answer the cell's rings on the owner's thread.
    pub(crate) fn fire(&mut self, txn: &mut Txn<'_>, now: Timestamp) -> Result<(), Unavailable> {
        for (label, id) in txn.rung() {
            let Some(timer) = self.by_label.get_mut(&label).filter(|timer| timer.id == id) else {
                continue;
            };
            let Some(schedule) = txn.schedules().get(&label) else {
                drop(self.by_label.remove(&label));
                continue;
            };
            if unix_ms(now) < schedule.next_unix_ms {
                timer.deadline = Self::arm_deadline(txn.weak(), label, id, schedule.next_unix_ms);
                continue;
            }
            let next = timer
                .trigger
                .is_recurring()
                .then(|| timer.trigger.next_after(now))
                .flatten()
                .map(unix_ms);
            let text = format!(
                "[scheduled '{label}' · {}] {}",
                schedule.trigger, schedule.prompt
            );
            txn.fire(&label, id, next, text)?;
            if let Some(next) = next {
                timer.deadline = Self::arm_deadline(txn.weak(), label, id, next);
            } else {
                drop(self.by_label.remove(&label));
            }
        }
        Ok(())
    }

    /// A commit already retired these schedules, or the agent is ending.
    pub fn clear(&mut self) {
        self.by_label.clear();
    }

    fn arm_deadline(
        address: WeakAgentAddress,
        label: String,
        id: ScheduleId,
        next_unix_ms: u64,
    ) -> Deadline {
        let delay = Duration::from_millis(next_unix_ms.saturating_sub(unix_ms(Timestamp::now())))
            .min(WALL_CHECK);
        arm_callback(delay, move || {
            if let Some(address) = address.upgrade() {
                address.ring(label, id);
            }
        })
    }

    /// Ring the label's timer as its deadline would. A ring is not a fire: the
    /// owner fires once `now` reaches the fold's instant.
    #[cfg(test)]
    pub(crate) fn ring(&self, label: &str, address: &AgentAddress) {
        if let Some(timer) = self.by_label.get(label) {
            address.ring(label.to_string(), timer.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Trunk;
    use crate::agent::testkit::{host, ral_call, scripted, set_provider, trunk};
    use crate::bus::{AgentMailbox, Drained, Input, Next, Rest, Signal};
    use crate::provider::scripted::{Reply, Script};
    use crate::record::{
        AgentId, AgentLog, AgentState, ChildStatus, Control, Forensic, JobId, Record, Transcript,
        Transient, Voice,
    };
    use jiff::civil::date;
    use ral_core::process::{CancelCause, DurableRoot};

    fn mailbox() -> (AgentMailbox, AgentLog) {
        let id = AgentId::new(0);
        let (log, mail, schedules, model) = AgentLog::for_test(id, "test-model").unwrap();
        let life = DurableRoot::default().child();
        let journal = log.record_emitter();
        journal.bind(life.clone());
        (
            AgentMailbox::new(
                id,
                life,
                journal,
                crate::agent::Admission::Free,
                None,
                (mail, schedules, model),
            ),
            log,
        )
    }

    fn book(
        registry: &mut ScheduleRegistry,
        mailbox: &AgentMailbox,
        trigger: Trigger,
        prompt: &str,
        label: &str,
    ) -> Result<ScheduleReceipt, String> {
        mailbox
            .transact(|txn| registry.schedule(txn, trigger, prompt.into(), label.into()))
            .unwrap()
    }

    fn listed(mailbox: &AgentMailbox) -> Vec<ScheduleInfo> {
        mailbox
            .transact(|txn| {
                let now = Timestamp::now();
                txn.schedules()
                    .iter()
                    .map(|(label, schedule)| ScheduleInfo::of(label, schedule, now))
                    .collect()
            })
            .unwrap()
    }

    fn remove(registry: &mut ScheduleRegistry, mailbox: &AgentMailbox, label: &str) -> bool {
        mailbox
            .transact(|txn| registry.unschedule(txn, label))
            .unwrap()
            .unwrap()
    }

    fn fire(registry: &mut ScheduleRegistry, mailbox: &AgentMailbox, now: Timestamp) {
        mailbox
            .transact(|txn| registry.fire(txn, now))
            .unwrap()
            .unwrap();
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
        let mut reg = ScheduleRegistry::new();
        let (mailbox, _log) = mailbox();
        let receipt = book(
            &mut reg,
            &mailbox,
            Trigger::Cron {
                schedule: sched("0 3 * * *"),
                expr: "0 3 * * *".into(),
            },
            "run tests",
            "nightly",
        )
        .unwrap();
        assert_eq!(receipt.label, "nightly");
        let live = listed(&mailbox);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].label, "nightly");
        assert_eq!(live[0].trigger, "0 3 * * *");
        assert!(remove(&mut reg, &mailbox, "nightly"));
        assert!(listed(&mailbox).is_empty());
        assert!(!remove(&mut reg, &mailbox, "nightly"));
    }

    #[test]
    fn schedule_rejects_a_label_already_borne_by_a_live_schedule() {
        let mut reg = ScheduleRegistry::new();
        let (mailbox, _log) = mailbox();
        book(
            &mut reg,
            &mailbox,
            Trigger::After(Duration::from_mins(1)),
            "x",
            "nightly",
        )
        .unwrap();
        let err = book(
            &mut reg,
            &mailbox,
            Trigger::After(Duration::from_mins(1)),
            "y",
            "nightly",
        )
        .expect_err("a duplicate label must be refused");
        assert!(err.contains("nightly"), "must name the label: {err}");
        assert!(err.contains("already borne"), "must name the rule: {err}");
        assert_eq!(listed(&mailbox).len(), 1);
    }

    #[test]
    fn unschedule_by_label_removes_the_match_and_is_a_noop_otherwise() {
        let mut reg = ScheduleRegistry::new();
        let (mailbox, _log) = mailbox();
        book(
            &mut reg,
            &mailbox,
            Trigger::After(Duration::from_mins(1)),
            "x",
            "nightly",
        )
        .unwrap();
        assert!(!remove(&mut reg, &mailbox, "no-such-label"));
        assert!(remove(&mut reg, &mailbox, "nightly"));
        assert!(listed(&mailbox).is_empty());
        assert!(!remove(&mut reg, &mailbox, "nightly"));
    }

    #[test]
    fn after_fires_once_then_is_removed() {
        let mut reg = ScheduleRegistry::new();
        let (mailbox, _log) = mailbox();
        book(
            &mut reg,
            &mailbox,
            Trigger::After(Duration::from_millis(10)),
            "ping",
            "ping",
        )
        .unwrap();
        assert!(matches!(mailbox.next(QUIET), Next::Rung));
        fire(&mut reg, &mailbox, Timestamp::now());
        let fired = mailbox.drain();
        let [Drained::Input(Input::Wakeup { text, .. })] = fired.as_slice() else {
            panic!("exactly one tagged wakeup must be delivered");
        };
        assert!(text.contains("ping"), "delivered the prompt: {text}");
        assert!(text.starts_with("[scheduled"), "marked wakeup: {text}");
        assert!(listed(&mailbox).is_empty());
        assert!(reg.by_label.is_empty());
    }

    #[test]
    fn a_fire_before_its_instant_rearms_and_posts_nothing() {
        let mut reg = ScheduleRegistry::new();
        let (mailbox, _log) = mailbox();
        let at = Timestamp::now()
            .checked_add(Duration::from_hours(1))
            .unwrap();
        book(&mut reg, &mailbox, Trigger::At(at), "later", "later").unwrap();
        reg.ring("later", &mailbox.address());
        fire(&mut reg, &mailbox, Timestamp::now());
        assert!(mailbox.drain().is_empty());
        let live = listed(&mailbox);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].fires, 0);
    }

    #[test]
    fn list_reports_the_time_remaining() {
        let mut reg = ScheduleRegistry::new();
        let (mailbox, _log) = mailbox();
        book(
            &mut reg,
            &mailbox,
            Trigger::After(Duration::from_hours(2)),
            "x",
            "x",
        )
        .unwrap();
        let next_in = Duration::from_secs(listed(&mailbox)[0].next_s);
        assert!(
            next_in > Duration::from_mins(119) && next_in <= Duration::from_hours(2),
            "got {next_in:?}"
        );
    }

    #[test]
    fn clear_drops_every_schedule() {
        let mut reg = ScheduleRegistry::new();
        let (mailbox, _log) = mailbox();
        book(
            &mut reg,
            &mailbox,
            Trigger::Cron {
                schedule: sched("0 3 * * *"),
                expr: "0 3 * * *".into(),
            },
            "x",
            "x",
        )
        .unwrap();
        assert_eq!(listed(&mailbox).len(), 1);
        #[allow(
            clippy::redundant_closure_for_method_calls,
            reason = "the method item is not general enough for transact's higher-ranked lifetime"
        )]
        mailbox.transact(|txn| txn.clear()).unwrap().unwrap();
        reg.clear();
        assert!(listed(&mailbox).is_empty());
        assert!(reg.by_label.is_empty());
    }

    const SYNC_TIMEOUT: Duration = Duration::from_secs(5);
    const QUIET: Rest = Rest { pending: false };

    #[derive(Clone, Copy)]
    enum Terminal {
        After,
        At,
        ExhaustedCron,
    }

    fn terminal(family: Terminal) -> (ScheduleRegistry, AgentMailbox, AgentLog, Timestamp) {
        let mut registry = ScheduleRegistry::new();
        let (mailbox, log) = mailbox();
        let future = Timestamp::now()
            .checked_add(Duration::from_hours(2))
            .unwrap();
        let trigger = match family {
            Terminal::After => Trigger::After(Duration::from_hours(2)),
            Terminal::At => Trigger::At(future),
            Terminal::ExhaustedCron => Trigger::Cron {
                schedule: sched("* * * * *"),
                expr: "* * * * *".into(),
            },
        };
        book(&mut registry, &mailbox, trigger, "last wakeup", "last").unwrap();
        let now = if matches!(family, Terminal::ExhaustedCron) {
            Timestamp::MAX
        } else {
            next_fire(&mailbox)
        };
        if matches!(family, Terminal::ExhaustedCron) {
            assert!(registry.by_label["last"].trigger.next_after(now).is_none());
        }
        (registry, mailbox, log, now)
    }

    fn next_fire(mailbox: &AgentMailbox) -> Timestamp {
        let next = mailbox
            .transact(|txn| txn.schedules().get("last").unwrap().next_unix_ms)
            .unwrap();
        Timestamp::from_millisecond(i64::try_from(next).unwrap()).unwrap()
    }

    #[test]
    fn every_terminal_family_queues_its_wakeup_before_the_job_can_settle() {
        for family in [Terminal::After, Terminal::At, Terminal::ExhaustedCron] {
            let (mut registry, mailbox, _log, now) = terminal(family);
            assert!(mailbox.begin());
            registry.ring("last", &mailbox.address());
            assert!(matches!(mailbox.next(QUIET), Next::Rung));
            fire(&mut registry, &mailbox, now);
            assert!(matches!(mailbox.next(QUIET), Next::Take {
                input: Input::Wakeup { text, .. }, ..
            } if text.contains("last wakeup")));
            assert!(registry.by_label.is_empty());
            assert!(matches!(mailbox.next(QUIET), Next::Settled { .. }));
        }
    }

    #[test]
    fn clear_before_answering_a_ring_suppresses_the_old_id_and_keeps_the_new_one() {
        let (mut registry, mailbox, _log, now) = terminal(Terminal::After);
        registry.ring("last", &mailbox.address());
        #[allow(
            clippy::redundant_closure_for_method_calls,
            reason = "the method item is not general enough for transact's higher-ranked lifetime"
        )]
        mailbox.transact(|txn| txn.clear()).unwrap().unwrap();
        registry.clear();
        book(
            &mut registry,
            &mailbox,
            Trigger::After(Duration::from_hours(2)),
            "new task",
            "last",
        )
        .unwrap();
        fire(&mut registry, &mailbox, now);
        assert!(mailbox.drain().is_empty());
        assert_eq!(registry.by_label.len(), 1);
        assert_eq!(registry.by_label["last"].id, ScheduleId::new(1));
        assert_eq!(listed(&mailbox).len(), 1);
    }

    #[test]
    fn unschedule_before_answering_a_ring_suppresses_publication() {
        let (mut registry, mailbox, _log, now) = terminal(Terminal::After);
        registry.ring("last", &mailbox.address());
        assert!(remove(&mut registry, &mailbox, "last"));
        fire(&mut registry, &mailbox, now);
        assert!(mailbox.drain().is_empty());
        assert!(registry.by_label.is_empty());
    }

    #[test]
    fn unschedule_after_publication_does_not_recall_the_queued_wakeup() {
        let (mut registry, mailbox, _log, now) = terminal(Terminal::After);
        registry.ring("last", &mailbox.address());
        fire(&mut registry, &mailbox, now);
        assert!(!remove(&mut registry, &mailbox, "last"));
        assert!(matches!(mailbox.drain().as_slice(),
            [Drained::Input(Input::Wakeup { text, .. })] if text.contains("last wakeup")
        ));
    }

    #[test]
    fn a_terminal_overlap_retires_without_posting_a_second_wakeup() {
        for family in [Terminal::After, Terminal::At, Terminal::ExhaustedCron] {
            let (mut registry, mailbox, _log, now) = terminal(family);
            mailbox
                .address()
                .post(Input::Wakeup {
                    id: ScheduleId::new(0),
                    text: "already queued".into(),
                })
                .unwrap();
            registry.ring("last", &mailbox.address());
            fire(&mut registry, &mailbox, now);
            assert!(registry.by_label.is_empty());
            assert!(matches!(mailbox.drain().as_slice(),
                [Drained::Input(Input::Wakeup { text, .. })] if text == "already queued"
            ));
        }
    }

    #[test]
    fn repeated_terminal_rings_publish_only_one_wakeup() {
        let (mut registry, mailbox, _log, now) = terminal(Terminal::After);
        registry.ring("last", &mailbox.address());
        registry.ring("last", &mailbox.address());
        fire(&mut registry, &mailbox, now);
        assert!(registry.by_label.is_empty());
        assert!(matches!(
            mailbox.drain().as_slice(),
            [Drained::Input(Input::Wakeup { .. })]
        ));
    }

    #[test]
    fn recurring_publication_stays_armed_and_skips_queued_overlap() {
        let (mut registry, mailbox, _log, _) = terminal(Terminal::ExhaustedCron);
        let now = next_fire(&mailbox);
        registry.ring("last", &mailbox.address());
        fire(&mut registry, &mailbox, now);
        assert!(!registry.by_label.is_empty());
        let next = next_fire(&mailbox);
        registry.ring("last", &mailbox.address());
        fire(&mut registry, &mailbox, next);
        assert_eq!(listed(&mailbox)[0].fires, 1);
        assert!(matches!(
            mailbox.drain().as_slice(),
            [Drained::Input(Input::Wakeup { .. })]
        ));
        assert!(!registry.by_label.is_empty());
        let next = next_fire(&mailbox);
        registry.ring("last", &mailbox.address());
        fire(&mut registry, &mailbox, next);
        assert_eq!(listed(&mailbox)[0].fires, 2);
        assert!(matches!(
            mailbox.drain().as_slice(),
            [Drained::Input(Input::Wakeup { .. })]
        ));
    }

    #[test]
    fn a_recurring_ring_for_an_ended_life_posts_nothing_and_the_owner_drops_its_timer() {
        let (mut registry, mailbox, _log, now) = terminal(Terminal::ExhaustedCron);
        mailbox.address().end(CancelCause::Cancelled);
        registry.ring("last", &mailbox.address());
        fire(&mut registry, &mailbox, now);
        assert!(mailbox.drain().is_empty());
        registry.clear();
        assert!(registry.by_label.is_empty());
    }

    #[test]
    fn a_scheduled_job_can_end_before_the_owner_answers_its_ring() {
        let (mut registry, mailbox, _log, now) = terminal(Terminal::After);
        assert!(mailbox.begin());
        registry.ring("last", &mailbox.address());
        mailbox.address().end(CancelCause::Cancelled);
        assert!(matches!(mailbox.next(QUIET), Next::Ended));
        registry.clear();
        fire(&mut registry, &mailbox, now);
        assert!(mailbox.drain().is_empty());
        assert!(registry.by_label.is_empty());
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
                            Record::Control(Control::JobSettled { .. })
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
                .filter(|record| { matches!(record, Record::Control(Control::JobOpened)) })
                .count(),
            1
        );
        records
            .iter()
            .find_map(|record| match record {
                Record::Control(Control::JobSettled { job, .. }) => Some(*job),
                _ => None,
            })
            .expect("a settled job")
    }

    #[test]
    fn a_conversing_roots_schedule_survives_settlement_and_opens_a_job_at_rest() {
        let mut agent = trunk(Trunk::Converses);
        let address = agent.mailbox.address();
        // `now` rings before the owner starts; the other stays booked.
        for (label, trigger) in [
            ("now", Trigger::At(Timestamp::now())),
            ("kept", Trigger::After(Duration::from_hours(2))),
        ] {
            address
                .transact(|txn| {
                    agent.body.schedules.schedule(
                        txn,
                        trigger,
                        "wake from rest".into(),
                        label.into(),
                    )
                })
                .unwrap()
                .unwrap();
        }
        set_provider(
            &mut agent,
            scripted("test-model", Script::new().then(Reply::text("awake"))),
        );
        let host = host(agent);
        let handle = host
            .fleet()
            .registry
            .get(host.roots()[0])
            .expect("a live root");
        let records = job_records(&host);
        assert!(records.iter().any(|record| matches!(record,
            Record::Transcript(Transcript::UserPrompt { voice: Voice::Wakeup, text, .. })
                if text.contains("wake from rest")
        )));
        assert_eq!(
            handle
                .0
                .address
                .transact(|txn| txn
                    .schedules()
                    .iter()
                    .map(|(label, _)| label.to_string())
                    .collect::<Vec<_>>())
                .unwrap(),
            ["kept"],
            "conversation schedules outlive their job"
        );
        assert!(handle.status().unwrap().job.is_none());
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
                            Record::Control(Control::JobOpened) => {
                                assert!(
                                    first_job
                                        .replace(JobId::from(recorded.locus().seq().commit()))
                                        .is_none()
                                );
                            }
                            Record::Forensic(Forensic::Error { .. } | Forensic::Nudge { .. }) => {
                                panic!("an armed schedule must not produce errors or nudges");
                            }
                            Record::Control(
                                Control::JobSettled { .. } | Control::Status { .. },
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
        let address = agent.mailbox.address();
        for label in ["armed", "still armed"] {
            address
                .transact(|txn| {
                    agent.body.schedules.schedule(
                        txn,
                        Trigger::After(Duration::from_hours(2)),
                        "never due".into(),
                        label.into(),
                    )
                })
                .unwrap()
                .unwrap();
        }
        let script = Script::new()
            .then(Reply::text("waiting"))
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
        assert_eq!(handle.status().unwrap().job, Some(first_job));
        assert_eq!(
            handle
                .0
                .address
                .transact(|txn| txn.schedules().iter().count())
                .unwrap(),
            2
        );
        // A due timer would race the rest; the human's post is the next input.
        host.port()
            .submit(host.roots()[0], "reply now".into())
            .unwrap();
        let records = job_records(&host);
        assert!(
            !records
                .iter()
                .any(|record| matches!(record, Record::Control(Control::JobOpened)))
        );
        assert!(records.iter().any(|record| matches!(record,
            Record::Control(Control::JobSettled { job, .. }) if *job == first_job
        )));
        assert!(records.iter().any(|record| matches!(record,
            Record::Control(Control::Status { status: ChildStatus::Replied(value) })
                if value == &ral_core::data::FOValue::String { value: "awake".into() }
        )));
        assert!(
            handle
                .0
                .address
                .transact(|txn| txn.schedules().is_empty())
                .unwrap(),
            "the report clears even schedules that have not fired"
        );
        host.port()
            .submit(host.roots()[0], "fresh prompt".into())
            .unwrap();
        let fresh = job_records(&host);
        assert_ne!(settled_job(&fresh), first_job);
        assert!(fresh.iter().any(|record| matches!(record,
            Record::Control(Control::Status { status: ChildStatus::Replied(value) })
                if value == &ral_core::data::FOValue::String { value: "fresh".into() }
        )));
        assert_eq!(host.end(), [] as [String; 0]);
    }

    #[test]
    fn a_ring_from_before_clear_posts_nothing_and_opens_no_job() {
        let mut agent = trunk(Trunk::Converses);
        let address = agent.mailbox.address();
        address
            .transact(|txn| {
                agent.body.schedules.schedule(
                    txn,
                    Trigger::After(Duration::from_hours(2)),
                    "stale wakeup".into(),
                    "old".into(),
                )
            })
            .unwrap()
            .unwrap();
        agent.body.schedules.ring("old", &address);
        agent.clear().expect("a test engine can clear");
        address
            .transact(|txn| agent.body.schedules.fire(txn, Timestamp::now()))
            .unwrap()
            .unwrap();
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
                Record::Transcript(Transcript::UserPrompt { text, voice, .. }) => {
                    Some((text.as_str(), *voice))
                }
                _ => None,
            })
            .collect();
        assert_eq!(prompts, [("fresh prompt", Voice::Human)]);
        assert_eq!(host.end(), [] as [String; 0]);
    }

    fn commit_count(log: &AgentLog) -> usize {
        let (_, commits) = log.record_emitter().reader().since(None).unwrap();
        commits.map(Result::unwrap).count()
    }

    #[test]
    fn a_removed_then_readded_label_ignores_its_old_ring_without_a_commit() {
        let (mut registry, mailbox, log, _) = terminal(Terminal::After);
        registry.ring("last", &mailbox.address());
        assert!(remove(&mut registry, &mailbox, "last"));
        book(
            &mut registry,
            &mailbox,
            Trigger::After(Duration::from_hours(2)),
            "replacement",
            "last",
        )
        .unwrap();
        let before = commit_count(&log);
        fire(&mut registry, &mailbox, next_fire(&mailbox));
        assert_eq!(commit_count(&log), before);
        assert_eq!(
            mailbox.address().queued_posts(),
            Vec::<crate::record::PostId>::new()
        );
        assert_eq!(listed(&mailbox)[0].fires, 0);
        registry.ring("last", &mailbox.address());
        fire(&mut registry, &mailbox, next_fire(&mailbox));
        assert!(matches!(mailbox.drain().as_slice(),
            [Drained::Input(Input::Wakeup { text, .. })] if text.contains("replacement")));
    }

    #[test]
    fn rings_retired_by_clear_or_report_are_noops_and_drop_their_timers() {
        for report in [false, true] {
            let (mut registry, mailbox, log, now) = terminal(Terminal::After);
            registry.ring("last", &mailbox.address());
            mailbox
                .transact(|txn| {
                    if report {
                        txn.report(ChildStatus::Stopped, |_| {
                            Input::Human("unused notice".into())
                        })
                    } else {
                        txn.clear()
                    }
                })
                .unwrap()
                .unwrap();
            // Leave the retired timer for fire to reconcile from the cell.
            registry.ring("last", &mailbox.address());
            let before = commit_count(&log);
            fire(&mut registry, &mailbox, now);
            assert_eq!(commit_count(&log), before);
            assert_eq!(
                mailbox.address().queued_posts(),
                Vec::<crate::record::PostId>::new()
            );
            assert!(registry.by_label.is_empty());
            assert!(listed(&mailbox).is_empty());
        }
    }

    #[test]
    fn report_withdraws_a_queued_recurring_or_already_retired_one_shot_wakeup() {
        for family in [Terminal::After, Terminal::ExhaustedCron] {
            let parent = trunk(Trunk::Converses);
            let mut helper = parent.fork_named("scheduled-helper").unwrap();
            let handle = helper.handle();
            let mailbox = &helper.mailbox;
            let body = &mut *helper.body;
            let log = &body.log;
            let registry = &mut body.schedules;
            let trigger = if matches!(family, Terminal::After) {
                Trigger::After(Duration::from_hours(2))
            } else {
                Trigger::Cron {
                    schedule: sched("* * * * *"),
                    expr: "* * * * *".into(),
                }
            };
            book(registry, mailbox, trigger, "withdraw on report", "last").unwrap();
            let now = next_fire(mailbox);
            registry.ring("last", &mailbox.address());
            fire(registry, mailbox, now);
            let queued = mailbox.address().queued_posts();
            assert_eq!(queued.len(), 1);
            assert_eq!(
                registry.by_label.is_empty(),
                matches!(family, Terminal::After)
            );
            let before = commit_count(log);
            handle
                .report(ChildStatus::Replied(ral_core::data::FOValue::Unit))
                .unwrap();
            assert!(matches!(
                parent.mailbox.drain().as_slice(),
                [Drained::Input(Input::Child {
                    status: ChildStatus::Replied(ral_core::data::FOValue::Unit),
                    ..
                })]
            ));
            assert_eq!(commit_count(log), before + 1);
            let (_, commits) = log.record_emitter().reader().since(None).unwrap();
            let last = commits.map(Result::unwrap).last().unwrap();
            assert!(matches!(
                last.records().first.value(),
                Record::Control(Control::Status {
                    status: ChildStatus::Replied(ral_core::data::FOValue::Unit)
                })
            ));
            assert!(last.records().iter().any(|record| matches!(record.value(),
                Record::Control(Control::Withdrawn { posts }) if posts.iter().copied().collect::<Vec<_>>() == queued)));
            assert_eq!(
                last.records()
                    .iter()
                    .filter(|record| matches!(record.value(),
                Record::Control(Control::Unscheduled { label }) if label == "last"))
                    .count(),
                usize::from(matches!(family, Terminal::ExhaustedCron))
            );
            assert_eq!(
                mailbox.address().queued_posts(),
                Vec::<crate::record::PostId>::new()
            );
            registry.clear();
            assert!(mailbox.begin());
            assert!(matches!(
                mailbox.next(QUIET),
                Next::Settled { stopped: false }
            ));
        }
    }

    #[test]
    fn a_ring_blocked_by_the_cell_records_nothing_and_only_the_owner_fires() {
        let (mut registry, mailbox, log, now) = terminal(Terminal::After);
        let address = mailbox.address();
        let before = commit_count(&log);
        let (started, start) = std::sync::mpsc::channel();
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            mailbox
                .transact(|_| {
                    let registry = &registry;
                    let address = &address;
                    scope.spawn(move || {
                        started.send(()).unwrap();
                        registry.ring("last", address);
                        done.send(()).unwrap();
                    });
                    start.recv_timeout(SYNC_TIMEOUT).unwrap();
                    assert!(matches!(
                        finished.recv_timeout(Duration::from_millis(20)),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    ));
                    assert_eq!(commit_count(&log), before);
                })
                .unwrap();
            finished.recv_timeout(SYNC_TIMEOUT).unwrap();
        });
        assert_eq!(commit_count(&log), before);
        assert_eq!(address.queued_posts(), Vec::<crate::record::PostId>::new());
        assert!(matches!(mailbox.next(QUIET), Next::Rung));
        fire(&mut registry, &mailbox, now);
        assert_eq!(commit_count(&log), before + 1);
        assert_eq!(address.queued_posts().len(), 1);
    }

    #[test]
    fn the_real_deadline_callback_only_rings_without_committing_or_posting() {
        let (mailbox, log) = mailbox();
        let mut registry = ScheduleRegistry::new();
        book(
            &mut registry,
            &mailbox,
            Trigger::After(Duration::from_millis(10)),
            "callback",
            "callback",
        )
        .unwrap();
        let before = commit_count(&log);
        let address = mailbox.address();
        let (done, finished) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let next = mailbox.next(QUIET);
            done.send(matches!(next, Next::Rung)).unwrap();
            mailbox
        });
        let rung = finished.recv_timeout(SYNC_TIMEOUT);
        if rung.is_err() {
            address.end(CancelCause::Cancelled);
        }
        let mailbox = worker.join().unwrap();
        assert!(rung.unwrap(), "the callback hands a ring to the owner");
        assert_eq!(commit_count(&log), before);
        assert_eq!(address.queued_posts(), Vec::<crate::record::PostId>::new());
        assert_eq!(listed(&mailbox)[0].fires, 0);
        fire(&mut registry, &mailbox, Timestamp::now());
        assert_eq!(commit_count(&log), before + 1);
        assert_eq!(address.queued_posts().len(), 1);
    }
}
