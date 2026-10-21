//! iCalendar parsing and expansion into per-day entries.
//!
//! Covers what real CalDAV servers (Nextcloud) and ICS feeds send: time
//! zones (`TZID`, UTC, floating, dates), `DTEND`/`DURATION`, recurrence rules
//! with `INTERVAL`/`COUNT`/`UNTIL`/`BYDAY`/`BYMONTHDAY`/`BYMONTH`/`BYSETPOS`,
//! `EXDATE` lists, moved or cancelled single occurrences (`RECURRENCE-ID`,
//! `STATUS:CANCELLED`) and multi-day events.

use std::collections::{HashMap, HashSet};

use chrono::{
    Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc, Weekday,
};

// ── Parsed events ─────────────────────────────────────────────────────────────

/// A start or end, converted to the bot's local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IcsTime {
    Date(NaiveDate),
    DateTime(NaiveDateTime),
}

impl IcsTime {
    pub fn date(self) -> NaiveDate {
        match self {
            IcsTime::Date(d) => d,
            IcsTime::DateTime(dt) => dt.date(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct IcsEvent {
    pub uid: Option<String>,
    pub summary: String,
    pub location: Option<String>,
    pub start: IcsTime,
    pub end: Option<IcsTime>,
    pub duration: Option<Duration>,
    pub rrule: Option<String>,
    /// Local dates of excluded occurrences.
    pub exdates: Vec<NaiveDate>,
    /// Set on a single occurrence that replaces (moves or cancels) the
    /// occurrence of its series originally due on this local date.
    pub recurrence_id: Option<NaiveDate>,
    pub cancelled: bool,
}

/// RFC 5545 §3.1: unfold long lines (continuation lines start with space/tab).
fn unfold(ics: &str) -> String {
    ics.replace("\r\n ", "")
        .replace("\r\n\t", "")
        .replace("\n ", "")
        .replace("\n\t", "")
}

/// Unescape ICS text values (commas, semicolons, newlines, backslashes).
fn unescape(s: &str) -> String {
    s.replace("\\n", "\n")
        .replace("\\N", "\n")
        .replace("\\,", ",")
        .replace("\\;", ";")
        .replace("\\\\", "\\")
}

/// Value of `NAME=value` among `;`-separated property parameters.
fn param<'a>(params: &'a str, name: &str) -> Option<&'a str> {
    params.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.eq_ignore_ascii_case(name).then(|| v.trim_matches('"'))
    })
}

/// A DATE / DATE-TIME value in local time: UTC (`…Z`) and `TZID` values are
/// converted, floating times are taken as local.
fn parse_time(value: &str, params: &str) -> Option<IcsTime> {
    let value = value.trim();
    let is_date = param(params, "VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE"))
        || (value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit()));
    if is_date {
        return NaiveDate::parse_from_str(value.get(..8)?, "%Y%m%d")
            .ok()
            .map(IcsTime::Date);
    }
    let naive = NaiveDateTime::parse_from_str(value.get(..15)?, "%Y%m%dT%H%M%S").ok()?;
    let local = if value.ends_with('Z') {
        Utc.from_utc_datetime(&naive)
            .with_timezone(&Local)
            .naive_local()
    } else if let Some(tz) = param(params, "TZID").and_then(|t| t.parse::<chrono_tz::Tz>().ok()) {
        tz.from_local_datetime(&naive)
            .earliest()
            .map(|dt| dt.with_timezone(&Local).naive_local())
            .unwrap_or(naive)
    } else {
        // Floating time, or a TZID we don't know (e.g. Windows zone names).
        naive
    };
    Some(IcsTime::DateTime(local))
}

/// ISO 8601 durations as used in ICS: `P1D`, `PT1H30M`, `P1W`, `-PT15M`.
fn parse_duration(s: &str) -> Option<Duration> {
    let (sign, s) = match s.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let s = s.strip_prefix('P')?;
    let mut total = Duration::zero();
    let mut number = String::new();
    let mut in_time = false;
    for c in s.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => number.push(c),
            unit => {
                let n: i64 = number.parse().ok()?;
                number.clear();
                total += match (unit, in_time) {
                    ('W', _) => Duration::weeks(n),
                    ('D', _) => Duration::days(n),
                    ('H', true) => Duration::hours(n),
                    ('M', true) => Duration::minutes(n),
                    ('S', true) => Duration::seconds(n),
                    _ => return None,
                };
            }
        }
    }
    Some(total * sign)
}

pub fn parse_ics(ics: &str) -> Vec<IcsEvent> {
    let unfolded = unfold(ics);
    let mut events = Vec::new();
    // Properties of the VEVENT being read; nested components (VALARM) are
    // skipped so an alarm's own properties never overwrite the event's.
    let mut current: Option<Vec<(String, String, String)>> = None;
    let mut nested = 0usize;

    for raw_line in unfolded.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line == "BEGIN:VEVENT" {
            current = Some(Vec::new());
            nested = 0;
            continue;
        }
        if line == "END:VEVENT" {
            if let Some(props) = current.take() {
                events.extend(build_event(&props));
            }
            continue;
        }
        let Some(props) = current.as_mut() else {
            continue;
        };
        if line.starts_with("BEGIN:") {
            nested += 1;
            continue;
        }
        if line.starts_with("END:") {
            nested = nested.saturating_sub(1);
            continue;
        }
        if nested > 0 {
            continue;
        }
        let Some((key_part, value)) = line.split_once(':') else {
            continue;
        };
        let (name, params) = key_part.split_once(';').unwrap_or((key_part, ""));
        props.push((
            name.to_ascii_uppercase(),
            params.to_owned(),
            value.to_owned(),
        ));
    }
    events
}

fn build_event(props: &[(String, String, String)]) -> Option<IcsEvent> {
    let get = |name: &str| props.iter().find(|(n, _, _)| n == name);
    let (_, start_params, start_value) = get("DTSTART")?;
    let start = parse_time(start_value, start_params)?;
    let mut exdates = Vec::new();
    for (_, params, value) in props.iter().filter(|(n, _, _)| n == "EXDATE") {
        exdates.extend(
            value
                .split(',')
                .filter_map(|v| parse_time(v, params))
                .map(IcsTime::date),
        );
    }
    Some(IcsEvent {
        uid: get("UID").map(|(_, _, v)| v.trim().to_owned()),
        summary: get("SUMMARY")
            .map(|(_, _, v)| unescape(v).trim().to_owned())
            .unwrap_or_default(),
        location: get("LOCATION")
            .map(|(_, _, v)| unescape(v).trim().to_owned())
            .filter(|l| !l.is_empty()),
        start,
        end: get("DTEND").and_then(|(_, p, v)| parse_time(v, p)),
        duration: get("DURATION").and_then(|(_, _, v)| parse_duration(v.trim())),
        rrule: get("RRULE").map(|(_, _, v)| v.trim().to_owned()),
        exdates,
        recurrence_id: get("RECURRENCE-ID")
            .and_then(|(_, p, v)| parse_time(v, p))
            .map(IcsTime::date),
        cancelled: get("STATUS")
            .is_some_and(|(_, _, v)| v.trim().eq_ignore_ascii_case("CANCELLED")),
    })
}

// ── Recurrence rules ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Debug, Clone)]
struct Rule {
    freq: Freq,
    interval: i64,
    count: Option<usize>,
    until: Option<IcsTime>,
    /// (ordinal, weekday): `3TH` → (Some(3), Thu), `-1FR` → (Some(-1), Fri).
    by_day: Vec<(Option<i32>, Weekday)>,
    by_month_day: Vec<i32>,
    by_month: Vec<u32>,
    by_set_pos: Vec<i32>,
}

fn parse_weekday_code(code: &str) -> Option<Weekday> {
    Some(match code {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

fn parse_rule(rule: &str) -> Option<Rule> {
    let mut parsed = Rule {
        freq: Freq::Daily,
        interval: 1,
        count: None,
        until: None,
        by_day: Vec::new(),
        by_month_day: Vec::new(),
        by_month: Vec::new(),
        by_set_pos: Vec::new(),
    };
    let mut freq = None;
    let list =
        |v: &str| -> Vec<i32> { v.split(',').filter_map(|x| x.trim().parse().ok()).collect() };
    for part in rule.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_uppercase().as_str() {
            "FREQ" => {
                freq = Some(match value.to_ascii_uppercase().as_str() {
                    "DAILY" => Freq::Daily,
                    "WEEKLY" => Freq::Weekly,
                    "MONTHLY" => Freq::Monthly,
                    "YEARLY" => Freq::Yearly,
                    // HOURLY and finer: not meaningful for a day calendar.
                    _ => return None,
                })
            }
            "INTERVAL" => parsed.interval = value.parse().ok().filter(|&n| n > 0)?,
            "COUNT" => parsed.count = value.parse().ok(),
            "UNTIL" => parsed.until = parse_time(value, ""),
            "BYDAY" => {
                parsed.by_day = value
                    .split(',')
                    .filter_map(|d| {
                        let d = d.trim();
                        let (ord, code) = d.split_at(d.len().checked_sub(2)?);
                        let ord = if ord.is_empty() {
                            None
                        } else {
                            Some(ord.parse().ok()?)
                        };
                        Some((ord, parse_weekday_code(code)?))
                    })
                    .collect()
            }
            "BYMONTHDAY" => parsed.by_month_day = list(value),
            "BYMONTH" => parsed.by_month = list(value).into_iter().map(|m| m as u32).collect(),
            "BYSETPOS" => parsed.by_set_pos = list(value),
            _ => {}
        }
    }
    parsed.freq = freq?;
    Some(parsed)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    next.and_then(|n| n.pred_opt()).map_or(28, |d| d.day())
}

/// Whether `d` is the `ordinal`-th such weekday of its month (negative:
/// counted from the end).
fn nth_weekday_of_month(d: NaiveDate, ordinal: i32) -> bool {
    if ordinal > 0 {
        ((d.day() - 1) / 7 + 1) as i32 == ordinal
    } else {
        let from_end = days_in_month(d.year(), d.month()) - d.day();
        -((from_end / 7) as i32 + 1) == ordinal
    }
}

fn month_day_matches(d: NaiveDate, wanted: i32) -> bool {
    if wanted > 0 {
        d.day() as i32 == wanted
    } else {
        (days_in_month(d.year(), d.month()) as i32 + 1 + wanted) == d.day() as i32
    }
}

impl Rule {
    /// Whether `d` falls in a period the INTERVAL selects.
    fn in_period(&self, start: NaiveDate, d: NaiveDate) -> bool {
        let n = self.interval;
        match self.freq {
            Freq::Daily => (d - start).num_days() % n == 0,
            Freq::Weekly => {
                let week =
                    |x: NaiveDate| x - Duration::days(x.weekday().num_days_from_monday() as i64);
                ((week(d) - week(start)).num_days() / 7) % n == 0
            }
            Freq::Monthly => {
                let months =
                    (d.year() - start.year()) as i64 * 12 + d.month() as i64 - start.month() as i64;
                months % n == 0
            }
            Freq::Yearly => (d.year() - start.year()) as i64 % n == 0,
        }
    }

    /// Whether `d` is a candidate day inside its period (before BYSETPOS).
    fn day_matches(&self, start: NaiveDate, d: NaiveDate) -> bool {
        if !self.by_month.is_empty() && !self.by_month.contains(&d.month()) {
            return false;
        }
        if self.freq == Freq::Yearly && self.by_month.is_empty() && d.month() != start.month() {
            return false;
        }
        let weekday_ok = |ordinal_in_month: bool| {
            self.by_day.iter().any(|&(ord, wd)| {
                wd == d.weekday()
                    && match ord {
                        Some(o) if ordinal_in_month => nth_weekday_of_month(d, o),
                        _ => true,
                    }
            })
        };
        let month_day_ok = || self.by_month_day.iter().any(|&m| month_day_matches(d, m));
        match self.freq {
            Freq::Daily => {
                (self.by_day.is_empty() || weekday_ok(false))
                    && (self.by_month_day.is_empty() || month_day_ok())
            }
            Freq::Weekly => {
                if self.by_day.is_empty() {
                    d.weekday() == start.weekday()
                } else {
                    weekday_ok(false)
                }
            }
            Freq::Monthly | Freq::Yearly => {
                match (self.by_day.is_empty(), self.by_month_day.is_empty()) {
                    (true, true) => d.day() == start.day(),
                    (false, true) => weekday_ok(true),
                    (true, false) => month_day_ok(),
                    (false, false) => weekday_ok(true) && month_day_ok(),
                }
            }
        }
    }

    /// BYSETPOS: keep only the n-th candidates of `d`'s month/period.
    fn set_pos_matches(&self, start: NaiveDate, d: NaiveDate) -> bool {
        if self.by_set_pos.is_empty() {
            return true;
        }
        // The set is a month for MONTHLY (and YEARLY with BYMONTH), a week
        // for WEEKLY (the common uses).
        let period: Vec<NaiveDate> = match self.freq {
            Freq::Weekly => {
                let monday = d - Duration::days(d.weekday().num_days_from_monday() as i64);
                (0..7).map(|i| monday + Duration::days(i)).collect()
            }
            _ => (1..=days_in_month(d.year(), d.month()))
                .filter_map(|day| NaiveDate::from_ymd_opt(d.year(), d.month(), day))
                .collect(),
        };
        let candidates: Vec<NaiveDate> = period
            .into_iter()
            .filter(|&c| c >= start && self.day_matches(start, c))
            .collect();
        let Some(index) = candidates.iter().position(|&c| c == d) else {
            return false;
        };
        let len = candidates.len() as i32;
        self.by_set_pos.iter().any(|&pos| {
            let wanted = if pos > 0 { pos - 1 } else { len + pos };
            wanted == index as i32
        })
    }
}

/// Start dates of a series from `start` up to and including `up_to`, in
/// order, honouring UNTIL (compared as a moment: "…T235959Z" can end a
/// series before a morning occurrence on the next local day) and COUNT
/// (which counts before EXDATE removal, as RFC 5545 specifies).
fn occurrence_dates(start: IcsTime, rule: &Rule, up_to: NaiveDate) -> Vec<NaiveDate> {
    let time_of_day = match start {
        IcsTime::DateTime(dt) => dt.time(),
        IcsTime::Date(_) => NaiveTime::MIN,
    };
    let within_until = |d: NaiveDate| match rule.until {
        None => true,
        Some(IcsTime::Date(u)) => d <= u,
        Some(IcsTime::DateTime(u)) => d.and_time(time_of_day) <= u,
    };
    let start = start.date();
    let mut out = Vec::new();
    let mut produced = 0usize;
    let mut d = start;
    while d <= up_to && within_until(d) {
        if rule.in_period(start, d) && rule.day_matches(start, d) && rule.set_pos_matches(start, d)
        {
            produced += 1;
            if rule.count.is_some_and(|c| produced > c) {
                break;
            }
            out.push(d);
        }
        d += Duration::days(1);
    }
    out
}

// ── Expansion into days ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryTime {
    AllDay,
    Timed {
        start: NaiveTime,
        end: Option<NaiveTime>,
    },
}

/// One event as it appears on one day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayEntry {
    pub time: EntryTime,
    pub summary: String,
    pub location: Option<String>,
    /// First and last day the occurrence covers (equal for single-day events).
    pub first_day: NaiveDate,
    pub last_day: NaiveDate,
    /// Start of the occurrence, for "what's next".
    pub starts_at: NaiveDateTime,
    /// Which occurrence this is, stable when it is moved: the UID of a single
    /// event, the UID and original date of one of a series. Without a UID
    /// the title and start, so such an event can only appear or vanish.
    pub key: String,
}

impl DayEntry {
    pub fn is_multi_day(&self) -> bool {
        self.last_day > self.first_day
    }

    fn sort_key(&self, day: NaiveDate) -> (u8, NaiveTime) {
        match self.time {
            // Events already running (all-day, or continuing from earlier
            // days) frame the day, so they come first, as in calendar apps.
            _ if self.first_day < day => (0, NaiveTime::MIN),
            EntryTime::AllDay => (0, NaiveTime::MIN),
            EntryTime::Timed { start, .. } => (1, start),
        }
    }
}

/// Every event occurring on each of the `days` days starting at `from`,
/// sorted within the day. Occurrences listed by several calendars (same UID
/// and start) appear once.
pub fn expand(events: &[IcsEvent], from: NaiveDate, days: i64) -> Vec<(NaiveDate, Vec<DayEntry>)> {
    let to = from + Duration::days(days - 1);
    // Occurrences replaced by a separate RECURRENCE-ID event, per series.
    let mut overridden: HashMap<&str, HashSet<NaiveDate>> = HashMap::new();
    for ev in events {
        if let (Some(uid), Some(rid)) = (ev.uid.as_deref(), ev.recurrence_id) {
            overridden.entry(uid).or_default().insert(rid);
        }
    }

    let mut seen: HashSet<(String, NaiveDateTime)> = HashSet::new();
    let mut by_day: Vec<(NaiveDate, Vec<DayEntry>)> = (0..days)
        .map(|i| (from + Duration::days(i), Vec::new()))
        .collect();

    for ev in events {
        if ev.cancelled {
            continue;
        }
        let span = span_of(ev);
        // Reach back far enough to catch multi-day occurrences that started
        // before the window and are still running.
        let starts: Vec<NaiveDate> = match (&ev.rrule, ev.recurrence_id) {
            (Some(rule), None) => {
                let Some(rule) = parse_rule(rule) else {
                    continue;
                };
                let skip = ev.uid.as_deref().and_then(|uid| overridden.get(uid));
                occurrence_dates(ev.start, &rule, to)
                    .into_iter()
                    .filter(|d| !ev.exdates.contains(d))
                    .filter(|d| !skip.is_some_and(|s| s.contains(d)))
                    .collect()
            }
            _ => vec![ev.start.date()],
        };

        for start_date in starts {
            let (first_day, last_day, time, starts_at) = occurrence_layout(ev, start_date, span);
            if last_day < from || first_day > to {
                continue;
            }
            let key = (
                ev.uid.clone().unwrap_or_else(|| ev.summary.clone()),
                starts_at,
            );
            if !seen.insert(key) {
                continue;
            }
            let key = match (ev.uid.as_deref(), &ev.rrule, ev.recurrence_id) {
                (Some(uid), _, Some(rid)) => format!("{uid}#{rid}"),
                (Some(uid), Some(_), None) => format!("{uid}#{start_date}"),
                (Some(uid), None, None) => uid.to_owned(),
                (None, _, _) => format!("{}#{starts_at}", ev.summary),
            };
            let entry = DayEntry {
                time,
                summary: ev.summary.clone(),
                location: ev.location.clone(),
                first_day,
                last_day,
                starts_at,
                key,
            };
            for (day, entries) in &mut by_day {
                if *day >= first_day && *day <= last_day {
                    entries.push(entry.clone());
                }
            }
        }
    }

    for (day, entries) in &mut by_day {
        let day = *day;
        entries.sort_by(|a, b| {
            a.sort_key(day)
                .cmp(&b.sort_key(day))
                .then_with(|| a.summary.cmp(&b.summary))
        });
    }
    by_day
}

/// How long one occurrence lasts, from DTEND or DURATION.
fn span_of(ev: &IcsEvent) -> Duration {
    match (ev.start, ev.end) {
        (IcsTime::Date(s), Some(IcsTime::Date(e))) => e - s,
        (IcsTime::DateTime(s), Some(IcsTime::DateTime(e))) => e - s,
        _ => ev.duration.unwrap_or_else(|| match ev.start {
            IcsTime::Date(_) => Duration::days(1),
            IcsTime::DateTime(_) => Duration::zero(),
        }),
    }
}

fn occurrence_layout(
    ev: &IcsEvent,
    start_date: NaiveDate,
    span: Duration,
) -> (NaiveDate, NaiveDate, EntryTime, NaiveDateTime) {
    match ev.start {
        IcsTime::Date(_) => {
            // All-day DTEND is exclusive: a one-day event ends the next day.
            let days = span.num_days().max(1);
            (
                start_date,
                start_date + Duration::days(days - 1),
                EntryTime::AllDay,
                start_date.and_time(NaiveTime::MIN),
            )
        }
        IcsTime::DateTime(dt) => {
            let starts_at = start_date.and_time(dt.time());
            let ends_at = starts_at + span.max(Duration::zero());
            // An event ending exactly at midnight does not occupy the next day.
            let last_day = if ends_at > starts_at && ends_at.time() == NaiveTime::MIN {
                ends_at.date() - Duration::days(1)
            } else {
                ends_at.date()
            };
            let end = (span > Duration::zero()).then(|| ends_at.time());
            (
                start_date,
                last_day.max(start_date),
                EntryTime::Timed {
                    start: dt.time(),
                    end,
                },
                starts_at,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn vevent(lines: &str) -> String {
        format!("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\n{lines}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n")
    }

    fn days_with(events: &[IcsEvent], from: NaiveDate, days: i64) -> Vec<NaiveDate> {
        expand(events, from, days)
            .into_iter()
            .filter(|(_, e)| !e.is_empty())
            .map(|(d, _)| d)
            .collect()
    }

    fn rule_days(dtstart: &str, rrule: &str, from: NaiveDate, days: i64) -> Vec<NaiveDate> {
        let ics = vevent(&format!(
            "UID:x\r\nSUMMARY:S\r\nDTSTART:{dtstart}\r\nRRULE:{rrule}"
        ));
        days_with(&parse_ics(&ics), from, days)
    }

    #[test]
    fn bysetpos_picks_the_third_thursday_not_every_thursday() {
        // From the live Nextcloud data: FREQ=MONTHLY;BYDAY=TH;BYSETPOS=3.
        let days = rule_days(
            "20260917T190000",
            "FREQ=MONTHLY;BYDAY=TH;BYSETPOS=3",
            date(2026, 10, 1),
            61,
        );
        assert_eq!(days, vec![date(2026, 10, 15), date(2026, 11, 19)]);
    }

    #[test]
    fn ordinal_byday_and_fifth_sunday() {
        let first_sunday = rule_days(
            "20231029T190000",
            "FREQ=MONTHLY;BYDAY=1SU",
            date(2026, 10, 1),
            61,
        );
        assert_eq!(first_sunday, vec![date(2026, 10, 4), date(2026, 11, 1)]);

        // Only months that have a fifth Sunday: Nov 2026 (29th), not Oct.
        let fifth = rule_days(
            "20231224T190000",
            "FREQ=MONTHLY;BYDAY=5SU",
            date(2026, 10, 1),
            61,
        );
        assert_eq!(fifth, vec![date(2026, 11, 29)]);

        let last_friday = rule_days(
            "20260101T100000",
            "FREQ=MONTHLY;BYDAY=-1FR",
            date(2026, 9, 1),
            30,
        );
        assert_eq!(last_friday, vec![date(2026, 9, 25)]);
    }

    #[test]
    fn interval_count_and_until_are_honoured() {
        // Every other Monday.
        let biweekly = rule_days(
            "20260907T180000",
            "FREQ=WEEKLY;INTERVAL=2",
            date(2026, 9, 1),
            30,
        );
        assert_eq!(biweekly, vec![date(2026, 9, 7), date(2026, 9, 21)]);

        // Tue+Thu, every other week, starting on a Tuesday.
        let pair = rule_days(
            "20260908T180000",
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU,TH",
            date(2026, 9, 7),
            21,
        );
        assert_eq!(
            pair,
            vec![
                date(2026, 9, 8),
                date(2026, 9, 10),
                date(2026, 9, 22),
                date(2026, 9, 24)
            ]
        );

        let three = rule_days(
            "20260901T090000",
            "FREQ=DAILY;COUNT=3",
            date(2026, 9, 1),
            10,
        );
        assert_eq!(
            three,
            vec![date(2026, 9, 1), date(2026, 9, 2), date(2026, 9, 3)]
        );

        let until = rule_days(
            "20260901T090000",
            "FREQ=DAILY;UNTIL=20260902T235959Z",
            date(2026, 9, 1),
            10,
        );
        // UNTIL is a moment: in a zone east of UTC "23:59:59Z" is already
        // the next local day, yet before that day's 09:00 occurrence.
        let until_local = Utc
            .with_ymd_and_hms(2026, 9, 2, 23, 59, 59)
            .unwrap()
            .with_timezone(&Local)
            .naive_local();
        let expected: Vec<NaiveDate> = (1..=3)
            .map(|d| date(2026, 9, d))
            .filter(|d| d.and_hms_opt(9, 0, 0).unwrap() <= until_local)
            .collect();
        assert_eq!(until, expected);
        let until_date = rule_days(
            "20260901",
            "FREQ=DAILY;UNTIL=20260902",
            date(2026, 9, 1),
            10,
        );
        assert_eq!(until_date, vec![date(2026, 9, 1), date(2026, 9, 2)]);

        let monthday = rule_days(
            "20260131T090000",
            "FREQ=MONTHLY;BYMONTHDAY=-1",
            date(2026, 2, 1),
            60,
        );
        assert_eq!(monthday, vec![date(2026, 2, 28), date(2026, 3, 31)]);

        let yearly = rule_days("20200229", "FREQ=YEARLY", date(2026, 1, 1), 365);
        assert!(yearly.is_empty(), "no 29 Feb in 2026");
    }

    #[test]
    fn moved_and_cancelled_occurrences_replace_the_series_date() {
        // Series every Sunday; the 11 Oct occurrence moved to Monday 12 Oct,
        // the 18 Oct occurrence cancelled.
        let ics = "BEGIN:VCALENDAR\r\n\
            BEGIN:VEVENT\r\nUID:s\r\nSUMMARY:Plenum\r\nDTSTART;TZID=Europe/Berlin:20261004T190000\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:s\r\nSUMMARY:Plenum\r\nRECURRENCE-ID;TZID=Europe/Berlin:20261011T190000\r\nDTSTART;TZID=Europe/Berlin:20261012T190000\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:s\r\nSUMMARY:Plenum\r\nRECURRENCE-ID;TZID=Europe/Berlin:20261018T190000\r\nDTSTART;TZID=Europe/Berlin:20261018T190000\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\n\
            END:VCALENDAR\r\n";

        let days = days_with(&parse_ics(ics), date(2026, 10, 4), 21);

        assert_eq!(days, vec![date(2026, 10, 4), date(2026, 10, 12)]);

        // The moved occurrence keeps its key (the series date it replaces),
        // so the week update reports it as moved, not as new and cancelled.
        let keys: Vec<String> = expand(&parse_ics(ics), date(2026, 10, 4), 21)
            .into_iter()
            .flat_map(|(_, entries)| entries)
            .map(|e| e.key)
            .collect();
        assert_eq!(keys, ["s#2026-10-04", "s#2026-10-11"]);
        let single = vevent("UID:one\r\nSUMMARY:Party\r\nDTSTART:20261012T200000");
        assert_eq!(
            expand(&parse_ics(&single), date(2026, 10, 12), 1)[0].1[0].key,
            "one"
        );
    }

    #[test]
    fn exdate_lists_and_cancelled_events() {
        let ics = vevent(
            "UID:d\r\nSUMMARY:Standup\r\nDTSTART:20260901T090000\r\nRRULE:FREQ=DAILY;COUNT=5\r\n\
             EXDATE:20260902T090000,20260904T090000",
        );
        assert_eq!(
            days_with(&parse_ics(&ics), date(2026, 9, 1), 7),
            vec![date(2026, 9, 1), date(2026, 9, 3), date(2026, 9, 5)]
        );

        let cancelled =
            vevent("UID:c\r\nSUMMARY:Off\r\nDTSTART:20260901T090000\r\nSTATUS:CANCELLED");
        assert!(days_with(&parse_ics(&cancelled), date(2026, 9, 1), 3).is_empty());
    }

    #[test]
    fn multi_day_events_cover_every_day_they_span() {
        // All-day DTEND is exclusive: 25–27 Sep.
        let trip = vevent(
            "UID:t\r\nSUMMARY:Trip\r\nDTSTART;VALUE=DATE:20260925\r\nDTEND;VALUE=DATE:20260928",
        );
        assert_eq!(
            days_with(&parse_ics(&trip), date(2026, 9, 26), 5),
            vec![date(2026, 9, 26), date(2026, 9, 27)]
        );

        // A party from 22:00 to 02:00 touches two days; one ending at
        // midnight only one.
        let night =
            vevent("UID:n\r\nSUMMARY:Party\r\nDTSTART:20260925T220000\r\nDTEND:20260926T020000");
        assert_eq!(days_with(&parse_ics(&night), date(2026, 9, 25), 3).len(), 2);
        let midnight = vevent("UID:m\r\nSUMMARY:Talk\r\nDTSTART:20260925T220000\r\nDURATION:PT2H");
        assert_eq!(
            days_with(&parse_ics(&midnight), date(2026, 9, 25), 3),
            vec![date(2026, 9, 25)]
        );
    }

    #[test]
    fn times_are_converted_to_local_and_end_times_kept() {
        let ics =
            vevent("UID:u\r\nSUMMARY:Call\r\nDTSTART:20260925T170000Z\r\nDTEND:20260925T180000Z");
        let events = parse_ics(&ics);
        let expected = Utc
            .with_ymd_and_hms(2026, 9, 25, 17, 0, 0)
            .unwrap()
            .with_timezone(&Local)
            .naive_local();
        assert_eq!(events[0].start, IcsTime::DateTime(expected));

        let day = expand(&events, expected.date(), 1);
        match &day[0].1[0].time {
            EntryTime::Timed { start, end } => {
                assert_eq!(*start, expected.time());
                assert_eq!(*end, Some((expected + Duration::hours(1)).time()));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn alarms_do_not_overwrite_event_properties_and_duplicates_collapse() {
        let ics = "BEGIN:VCALENDAR\r\n\
            BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Real\r\nDTSTART:20260925T100000\r\n\
            BEGIN:VALARM\r\nDESCRIPTION:Reminder\r\nSUMMARY:Alarm text\r\nEND:VALARM\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Real\r\nDTSTART:20260925T100000\r\nEND:VEVENT\r\n\
            END:VCALENDAR\r\n";
        let events = parse_ics(ics);
        assert_eq!(events[0].summary, "Real");

        let day = expand(&events, date(2026, 9, 25), 1);
        assert_eq!(day[0].1.len(), 1, "same UID and start from two calendars");
    }

    #[test]
    fn all_day_and_running_events_sort_before_timed_ones() {
        let ics = "BEGIN:VCALENDAR\r\n\
            BEGIN:VEVENT\r\nUID:1\r\nSUMMARY:Breakfast\r\nDTSTART:20260925T080000\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:2\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20260925\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:3\r\nSUMMARY:Plenum\r\nDTSTART:20260925T190000\r\nEND:VEVENT\r\n\
            END:VCALENDAR\r\n";
        let day = expand(&parse_ics(ics), date(2026, 9, 25), 1);
        let order: Vec<&str> = day[0].1.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(order, ["Holiday", "Breakfast", "Plenum"]);
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("PT1H30M"), Some(Duration::minutes(90)));
        assert_eq!(parse_duration("P2D"), Some(Duration::days(2)));
        assert_eq!(parse_duration("P1W"), Some(Duration::weeks(1)));
        assert_eq!(parse_duration("-PT15M"), Some(Duration::minutes(-15)));
        assert_eq!(parse_duration("nonsense"), None);
    }
}
