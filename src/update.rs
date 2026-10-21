//! The weekly overview again when the rest of its week changed: what it
//! showed is kept as a snapshot (`store/week.json`), later loads are compared
//! with it, and new, moved and cancelled events are reported.

use std::collections::{BTreeMap, HashMap};

use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};

use crate::ics::{DayEntry, EntryTime};
use crate::render::{Gone, Mark};

/// One event as the last overview showed it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub summary: String,
    pub starts_at: NaiveDateTime,
    pub last_day: NaiveDate,
    pub all_day: bool,
}

/// What the last overview of the week showed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WeekSnapshot {
    /// The last day the overview covers.
    pub until: NaiveDate,
    pub posted_at: NaiveDateTime,
    /// Every calendar loaded. Otherwise the next complete load only replaces
    /// the snapshot: the events of a calendar that was missing are not new.
    pub complete: bool,
    pub events: BTreeMap<String, Seen>,
}

impl WeekSnapshot {
    pub fn of(
        days: &[(NaiveDate, Vec<DayEntry>)],
        until: NaiveDate,
        posted_at: NaiveDateTime,
        complete: bool,
    ) -> Self {
        let events = days
            .iter()
            .flat_map(|(_, entries)| entries)
            .map(|e| {
                (
                    e.key.clone(),
                    Seen {
                        summary: e.summary.clone(),
                        starts_at: e.starts_at,
                        last_day: e.last_day,
                        all_day: matches!(e.time, EntryTime::AllDay),
                    },
                )
            })
            .collect();
        Self {
            until,
            posted_at,
            complete,
            events,
        }
    }
}

/// What changed in the rest of the week.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub marks: HashMap<String, Mark>,
    pub gone: Vec<Gone>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.marks.is_empty() && self.gone.is_empty()
    }
}

/// Whether an event still lies ahead at `now`: all-day events count for
/// the whole day, timed ones until they start (or while they run over
/// several days).
fn ahead(starts_at: NaiveDateTime, last_day: NaiveDate, all_day: bool, now: NaiveDateTime) -> bool {
    let today = now.date();
    last_day > today || (last_day == today && (all_day || starts_at >= now))
}

/// Compare the snapshot with `week` (today to the snapshot's last day) and
/// `later` (the days after it, to tell moved-away events from cancelled
/// ones). Events already past at `now` are left out on both sides: adding a
/// note of this morning in the afternoon is no news.
pub fn changes(
    before: &WeekSnapshot,
    week: &[(NaiveDate, Vec<DayEntry>)],
    later: &[(NaiveDate, Vec<DayEntry>)],
    now: NaiveDateTime,
) -> Changes {
    let mut out = Changes::default();
    let mut in_week: HashMap<&str, &DayEntry> = HashMap::new();
    for entry in week.iter().flat_map(|(_, entries)| entries) {
        in_week.entry(&entry.key).or_insert(entry);
    }
    for (key, entry) in &in_week {
        let all_day = matches!(entry.time, EntryTime::AllDay);
        if !ahead(entry.starts_at, entry.last_day, all_day, now) {
            continue;
        }
        match before.events.get(*key) {
            None => {
                out.marks.insert((*key).to_owned(), Mark::New);
            }
            Some(seen) if seen.starts_at != entry.starts_at => {
                out.marks
                    .insert((*key).to_owned(), Mark::Moved(seen.starts_at));
            }
            Some(_) => {}
        }
    }
    let mut later_start: HashMap<&str, NaiveDateTime> = HashMap::new();
    for entry in later.iter().flat_map(|(_, entries)| entries) {
        later_start.entry(&entry.key).or_insert(entry.starts_at);
    }
    for (key, seen) in &before.events {
        if in_week.contains_key(key.as_str())
            || !ahead(seen.starts_at, seen.last_day, seen.all_day, now)
        {
            continue;
        }
        out.gone.push(Gone {
            summary: seen.summary.clone(),
            starts_at: seen.starts_at,
            all_day: seen.all_day,
            moved_to: later_start.get(key.as_str()).copied(),
        });
    }
    out.gone
        .sort_by(|a, b| (a.starts_at, &a.summary).cmp(&(b.starts_at, &b.summary)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveTime;

    fn date(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    fn at(d: u32, h: u32) -> NaiveDateTime {
        date(d).and_time(NaiveTime::from_hms_opt(h, 0, 0).unwrap())
    }

    fn timed(key: &str, title: &str, starts_at: NaiveDateTime) -> DayEntry {
        DayEntry {
            time: EntryTime::Timed {
                start: starts_at.time(),
                end: None,
            },
            summary: title.to_owned(),
            location: None,
            first_day: starts_at.date(),
            last_day: starts_at.date(),
            starts_at,
            key: key.to_owned(),
        }
    }

    /// `entries` laid out on the days `from`..=`to`.
    fn days(from: u32, to: u32, entries: &[DayEntry]) -> Vec<(NaiveDate, Vec<DayEntry>)> {
        (from..=to)
            .map(|d| {
                let on: Vec<DayEntry> = entries
                    .iter()
                    .filter(|e| e.first_day <= date(d) && e.last_day >= date(d))
                    .cloned()
                    .collect();
                (date(d), on)
            })
            .collect()
    }

    #[test]
    fn new_moved_cancelled_and_moved_away_events_are_told_apart() {
        // Monday 19 Oct: the overview showed four events.
        let posted = [
            timed("plenum", "Plenum", at(21, 19)),
            timed("cleaning", "Cleaning", at(22, 10)),
            timed("party", "Party", at(24, 20)),
            timed("repair", "Repair", at(23, 9)),
        ];
        let before = WeekSnapshot::of(&days(19, 25, &posted), date(25), at(19, 7), true);

        // Tuesday: the plenum moved to Thursday, the party was called off,
        // the repair moved to next week, a film night was added.
        let now = at(20, 12);
        let week = [
            timed("plenum", "Plenum", at(22, 19)),
            timed("cleaning", "Cleaning", at(22, 10)),
            timed("film", "Film night", at(25, 20)),
        ];
        let later = [timed("repair", "Repair", at(28, 9))];
        let found = changes(&before, &days(20, 25, &week), &days(26, 30, &later), now);

        assert_eq!(found.marks.len(), 2);
        assert_eq!(found.marks["plenum"], Mark::Moved(at(21, 19)));
        assert_eq!(found.marks["film"], Mark::New);
        assert_eq!(
            found.gone,
            vec![
                Gone {
                    summary: "Repair".to_owned(),
                    starts_at: at(23, 9),
                    all_day: false,
                    moved_to: Some(at(28, 9)),
                },
                Gone {
                    summary: "Party".to_owned(),
                    starts_at: at(24, 20),
                    all_day: false,
                    moved_to: None,
                },
            ]
        );
    }

    #[test]
    fn past_events_and_unchanged_weeks_are_no_news() {
        let posted = [timed("plenum", "Plenum", at(21, 19))];
        let before = WeekSnapshot::of(&days(19, 25, &posted), date(25), at(19, 7), true);
        let now = at(22, 12);

        // The plenum is over (gone from the load, which starts today), and
        // someone added this morning's meeting afterwards.
        let week = [timed("standup", "Standup", at(22, 9))];
        let found = changes(&before, &days(22, 25, &week), &[], now);
        assert!(found.is_empty(), "{found:?}");

        let unchanged = changes(&before, &days(21, 25, &posted), &[], at(21, 8));
        assert!(unchanged.is_empty(), "{unchanged:?}");
    }
}
