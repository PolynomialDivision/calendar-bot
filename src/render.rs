//! Matrix messages for the calendar, laid out for a phone screen: one line
//! per event, time first, title in bold, days as headings.

use std::collections::HashMap;

use chrono::{Duration, NaiveDate, NaiveDateTime};

use mxbot_common::calendar::ics::{DayEntry, EntryTime};

const MAX_TITLE_CHARS: usize = 120;
const MAX_LOCATION_CHARS: usize = 40;
const NOTHING: &str = "🌴 Nothing on the calendar.";

/// Plain text and HTML of one message.
pub type Message = (String, String);

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        return s;
    }
    let cut: String = s.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

/// "Today", "Tomorrow", else "Wed 30 Sep".
fn day_name(day: NaiveDate, today: NaiveDate) -> String {
    match (day - today).num_days() {
        0 => "Today".to_owned(),
        1 => "Tomorrow".to_owned(),
        _ => day.format("%a %-d %b").to_string(),
    }
}

/// Day heading for overviews: "Wed 30 Sep", with "· today"/"· tomorrow".
fn day_heading(day: NaiveDate, today: NaiveDate) -> String {
    let date = day.format("%a %-d %b").to_string();
    match (day - today).num_days() {
        0 => format!("{date} · today"),
        1 => format!("{date} · tomorrow"),
        _ => date,
    }
}

/// How an entry reads on `day` in the one-day view.
fn when_on_day(entry: &DayEntry, day: NaiveDate) -> String {
    match entry.time {
        EntryTime::AllDay => "All day".to_owned(),
        EntryTime::Timed { start, end } => {
            if entry.first_day < day {
                match end {
                    Some(end) if entry.last_day == day => format!("until {}", end.format("%H:%M")),
                    _ => "All day".to_owned(),
                }
            } else {
                match end {
                    Some(end) if !entry.is_multi_day() => {
                        format!("{}–{}", start.format("%H:%M"), end.format("%H:%M"))
                    }
                    _ => start.format("%H:%M").to_string(),
                }
            }
        }
    }
}

/// One bullet line. `suffix` carries multi-day context.
fn entry_line(entry: &DayEntry, when: &str, suffix: Option<String>) -> Message {
    let title = truncate(&entry.summary, MAX_TITLE_CHARS);
    let title = if title.is_empty() {
        "(no title)".to_owned()
    } else {
        title
    };
    let location = entry
        .location
        .as_deref()
        .map(|l| truncate(l, MAX_LOCATION_CHARS))
        .filter(|l| !l.is_empty());
    let suffix = suffix.unwrap_or_default();

    let mut plain = format!("• {when} {title}{suffix}");
    let mut html = format!(
        "• {when} <strong>{}</strong>{}",
        html_escape(&title),
        html_escape(&suffix)
    );
    if let Some(loc) = location {
        plain.push_str(&format!(" · 📍 {loc}"));
        html.push_str(&format!(" · 📍 {}", html_escape(&loc)));
    }
    (plain, html)
}

fn warning_line(failed: usize, total: usize) -> Option<String> {
    (failed > 0).then(|| {
        format!(
            "⚠️ {failed} of {total} calendar{} could not be loaded. This may be incomplete.",
            if total == 1 { "" } else { "s" }
        )
    })
}

fn join(plain: Vec<String>, html: Vec<String>) -> Message {
    (plain.join("\n"), html.join("<br>"))
}

/// The one-day view (daily post, `!today`).
pub fn day_message(
    day: NaiveDate,
    today: NaiveDate,
    entries: &[DayEntry],
    failed: usize,
    total: usize,
) -> Message {
    let title = format!("{} · {}", day_name(day, today), day.format("%a %-d %b"));
    let title = if day == today || (day - today).num_days() == 1 {
        title
    } else {
        day.format("%a %-d %b").to_string()
    };
    let mut plain = Vec::new();
    let mut html = Vec::new();
    if entries.is_empty() {
        plain.push(format!("📅 {title}"));
        plain.push(NOTHING.to_owned());
        html.push(format!("📅 <strong>{}</strong>", html_escape(&title)));
        html.push(NOTHING.to_owned());
    } else {
        plain.push(format!("📅 {title}"));
        html.push(format!("📅 <strong>{}</strong>", html_escape(&title)));
        for entry in entries {
            let suffix = entry.is_multi_day().then(|| {
                let n = (day - entry.first_day).num_days() + 1;
                let of = (entry.last_day - entry.first_day).num_days() + 1;
                format!(" (day {n}/{of})")
            });
            let (p, h) = entry_line(entry, &when_on_day(entry, day), suffix);
            plain.push(p);
            html.push(h);
        }
    }
    if let Some(w) = warning_line(failed, total) {
        plain.push(w.clone());
        html.push(format!("<em>{}</em>", html_escape(&w)));
    }
    join(plain, html)
}

/// Several days grouped under day headings (weekly/monthly post, `!week`,
/// `!month`). A multi-day event is listed once, on its first day in view,
/// with its last day.
pub fn range_message(
    title: &str,
    days: &[(NaiveDate, Vec<DayEntry>)],
    today: NaiveDate,
    failed: usize,
    total: usize,
) -> Message {
    let (plain, html) = range_lines(days, today, |_| None);
    titled(title, plain, html, failed, total)
}

/// How an event of the week changed since the overview was last posted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mark {
    New,
    /// Moved within the week, from this start.
    Moved(NaiveDateTime),
}

/// An event of the last overview that is no longer in the rest of the week.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gone {
    pub summary: String,
    pub starts_at: NaiveDateTime,
    pub all_day: bool,
    /// Its new start when it moved out of the week, `None` when cancelled.
    pub moved_to: Option<NaiveDateTime>,
}

/// The rest of the week again after changes: the overview with new and
/// moved events marked, then what was cancelled or moved away.
pub fn update_message(
    days: &[(NaiveDate, Vec<DayEntry>)],
    today: NaiveDate,
    marks: &HashMap<String, Mark>,
    gone: &[Gone],
    failed: usize,
    total: usize,
) -> Message {
    let (mut plain, mut html) = range_lines(days, today, |entry| marks.get(&entry.key));
    for (cancelled, heading) in [(true, "❌ Cancelled"), (false, "📅 Moved to another week")] {
        let listed: Vec<&Gone> = gone
            .iter()
            .filter(|g| g.moved_to.is_none() == cancelled)
            .collect();
        if listed.is_empty() {
            continue;
        }
        if !plain.is_empty() {
            plain.push(String::new());
            html.push(String::new());
        }
        plain.push(heading.to_owned());
        html.push(format!("<strong>{heading}</strong>"));
        for g in listed {
            let title = truncate(&g.summary, MAX_TITLE_CHARS);
            let title = if title.is_empty() {
                "(no title)".to_owned()
            } else {
                title
            };
            let was = moment(g.starts_at, g.all_day);
            let to = g
                .moved_to
                .map(|t| format!(" → {}", moment(t, g.all_day)))
                .unwrap_or_default();
            plain.push(format!("• {was} {title}{to}"));
            html.push(format!(
                "• {was} <strong>{}</strong>{}",
                html_escape(&title),
                html_escape(&to)
            ));
        }
    }
    let last = days.last().map_or(today, |(d, _)| *d);
    let title = format!(
        "🔄 Calendar update · {} to {}",
        today.format("%-d %b"),
        last.format("%-d %b")
    );
    titled(&title, plain, html, failed, total)
}

/// "Tue 21 Oct 14:00", or without the time for an all-day event.
fn moment(at: NaiveDateTime, all_day: bool) -> String {
    if all_day {
        at.format("%a %-d %b").to_string()
    } else {
        at.format("%a %-d %b %H:%M").to_string()
    }
}

/// The day headings and bullet lines of a range; `mark` flags single
/// entries (the update after changes).
fn range_lines<'a>(
    days: &[(NaiveDate, Vec<DayEntry>)],
    today: NaiveDate,
    mark: impl Fn(&DayEntry) -> Option<&'a Mark>,
) -> (Vec<String>, Vec<String>) {
    let window_start = days.first().map(|(d, _)| *d);
    let mut plain = vec![];
    let mut html = vec![];
    for (day, entries) in days {
        let shown: Vec<&DayEntry> = entries
            .iter()
            .filter(|e| e.first_day == *day || Some(*day) == window_start)
            .collect();
        if shown.is_empty() {
            continue;
        }
        // A blank line between days keeps a long week readable on a phone.
        if !plain.is_empty() {
            plain.push(String::new());
            html.push(String::new());
        }
        let heading = day_heading(*day, today);
        plain.push(format!("🔹 {heading}"));
        html.push(format!("🔹 <strong>{}</strong>", html_escape(&heading)));
        for entry in shown {
            let when = match entry.time {
                EntryTime::AllDay => "All day".to_owned(),
                EntryTime::Timed { .. } if entry.first_day < *day => "All day".to_owned(),
                EntryTime::Timed { start, end } => match end {
                    Some(end) if !entry.is_multi_day() => {
                        format!("{}–{}", start.format("%H:%M"), end.format("%H:%M"))
                    }
                    _ => start.format("%H:%M").to_string(),
                },
            };
            let mut suffix = entry
                .is_multi_day()
                .then(|| format!(" (until {})", entry.last_day.format("%a %-d %b")));
            let when = match mark(entry) {
                None => when,
                Some(Mark::New) => format!("🆕 {when}"),
                Some(Mark::Moved(from)) => {
                    let all_day = matches!(entry.time, EntryTime::AllDay);
                    let was = format!(" (was {})", moment(*from, all_day));
                    suffix = Some(suffix.unwrap_or_default() + was.as_str());
                    format!("📅 {when}")
                }
            };
            let (p, h) = entry_line(entry, &when, suffix);
            plain.push(p);
            html.push(h);
        }
    }
    (plain, html)
}

/// `title` over `plain`/`html` (or "nothing" when they are empty), with the
/// warning about calendars that failed.
fn titled(
    title: &str,
    plain: Vec<String>,
    html: Vec<String>,
    failed: usize,
    total: usize,
) -> Message {
    let any = !plain.is_empty();
    let mut out_plain = vec![];
    let mut out_html = vec![];
    if any {
        out_plain.push(title.to_owned());
        out_html.push(format!("<strong>{}</strong>", html_escape(title)));
        out_plain.extend(plain);
        out_html.extend(html);
    } else {
        out_plain.push(title.to_owned());
        out_plain.push(NOTHING.to_owned());
        out_html.push(format!("<strong>{}</strong>", html_escape(title)));
        out_html.push(NOTHING.to_owned());
    }
    if let Some(w) = warning_line(failed, total) {
        out_plain.push(w.clone());
        out_html.push(format!("<em>{}</em>", html_escape(&w)));
    }
    join(out_plain, out_html)
}

pub fn week_title(start: NaiveDate) -> String {
    let end = start + Duration::days(6);
    format!(
        "📆 Next 7 days · {} to {}",
        start.format("%-d %b"),
        end.format("%-d %b")
    )
}

pub fn month_title(month_first: NaiveDate) -> String {
    format!("🗓️ {}", month_first.format("%B %Y"))
}

/// `!next`: the first event that has not started yet.
pub fn next_message(
    days: &[(NaiveDate, Vec<DayEntry>)],
    now: NaiveDateTime,
    failed: usize,
    total: usize,
) -> Message {
    let today = now.date();
    let next = days
        .iter()
        .flat_map(|(_, entries)| entries)
        .filter(|e| match e.time {
            // All-day events count from their day on; timed ones until they start.
            EntryTime::AllDay => e.first_day >= today,
            EntryTime::Timed { .. } => e.starts_at >= now,
        })
        .min_by_key(|e| e.starts_at);

    let (mut plain, mut html) = match next {
        None => {
            let text = format!(
                "⏭️ Nothing on the calendar in the next {} days. 🌴",
                days.len()
            );
            (vec![text.clone()], vec![text])
        }
        Some(e) => {
            let when = match e.time {
                EntryTime::AllDay => day_name(e.first_day, today),
                EntryTime::Timed { start, .. } => {
                    format!("{} {}", day_name(e.first_day, today), start.format("%H:%M"))
                }
            };
            let days_away = (e.first_day - today).num_days();
            let hint = if days_away > 1 {
                format!(" · in {days_away} days")
            } else {
                String::new()
            };
            let (p, h) = entry_line(e, "", None);
            // entry_line starts with "•  "; reuse its title/location formatting.
            let p = p.trim_start_matches("• ").trim_start().to_owned();
            let h = h.trim_start_matches("• ").trim_start().to_owned();
            (
                vec![format!("⏭️ Next: {p}"), format!("🕒 {when}{hint}")],
                vec![
                    format!("⏭️ Next: {h}"),
                    format!("🕒 {}", html_escape(&format!("{when}{hint}"))),
                ],
            )
        }
    };
    if let Some(w) = warning_line(failed, total) {
        plain.push(w.clone());
        html.push(format!("<em>{}</em>", html_escape(&w)));
    }
    join(plain, html)
}

pub fn help_message() -> Message {
    let plain = "📅 Calendar\n\
                 ☀️ !today: today's events\n\
                 📆 !week: the next 7 days\n\
                 🗓️ !month: the rest of this month\n\
                 ⏭️ !next: the next event"
        .to_owned();
    let html = plain.replace('\n', "<br>");
    (plain, html)
}

pub fn unavailable_message() -> Message {
    let text = "⚠️ I can't reach the calendar right now.\n🔁 Please try again later.".to_owned();
    (text.clone(), text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveTime;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn hm(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn timed(day: NaiveDate, start: NaiveTime, end: Option<NaiveTime>, title: &str) -> DayEntry {
        DayEntry {
            time: EntryTime::Timed { start, end },
            summary: title.to_owned(),
            location: None,
            first_day: day,
            last_day: day,
            starts_at: day.and_time(start),
            key: format!("{title}#{day}"),
        }
    }

    fn all_day(first: NaiveDate, last: NaiveDate, title: &str) -> DayEntry {
        DayEntry {
            time: EntryTime::AllDay,
            summary: title.to_owned(),
            location: None,
            first_day: first,
            last_day: last,
            starts_at: first.and_time(NaiveTime::MIN),
            key: format!("{title}#{first}"),
        }
    }

    #[test]
    fn day_view_is_one_line_per_event_with_times_and_places() {
        let today = date(2026, 9, 25);
        let mut plenum = timed(today, hm(19, 0), Some(hm(21, 0)), "Plenum");
        plenum.location = Some("Scharnweberstraße 38, 10247 Berlin, Deutschland".to_owned());
        let entries = [all_day(today, today, "Holiday"), plenum];

        let (plain, html) = day_message(today, today, &entries, 0, 1);

        assert_eq!(
            plain,
            "📅 Today · Fri 25 Sep\n\
             • All day Holiday\n\
             • 19:00–21:00 Plenum · 📍 Scharnweberstraße 38, 10247 Berlin, Deu…"
        );
        assert!(html.contains("• 19:00–21:00 <strong>Plenum</strong>"));
    }

    #[test]
    fn empty_and_partially_failed_states_are_honest() {
        let today = date(2026, 9, 25);
        let (plain, _) = day_message(today, today, &[], 0, 1);
        assert_eq!(plain, "📅 Today · Fri 25 Sep\n🌴 Nothing on the calendar.");

        let (plain, _) = day_message(today, today, &[], 1, 3);
        assert!(plain.ends_with("⚠️ 1 of 3 calendars could not be loaded. This may be incomplete."));
    }

    #[test]
    fn multi_day_events_show_their_progress_and_end() {
        let trip = all_day(date(2026, 9, 25), date(2026, 9, 27), "Trip");

        let (day_view, _) = day_message(
            date(2026, 9, 26),
            date(2026, 9, 26),
            std::slice::from_ref(&trip),
            0,
            1,
        );
        assert!(day_view.contains("• All day Trip (day 2/3)"));

        let days = vec![
            (date(2026, 9, 25), vec![trip.clone()]),
            (date(2026, 9, 26), vec![trip.clone()]),
            (date(2026, 9, 27), vec![trip]),
        ];
        let (week, _) = range_message("📆 Week", &days, date(2026, 9, 25), 0, 1);
        assert_eq!(
            week, "📆 Week\n🔹 Fri 25 Sep · today\n• All day Trip (until Sun 27 Sep)",
            "listed once, not on every day"
        );
    }

    #[test]
    fn overviews_group_events_under_day_headings() {
        let today = date(2026, 9, 25);
        let days = vec![
            (today, vec![timed(today, hm(19, 0), None, "Plenum")]),
            (date(2026, 9, 26), vec![]),
            (
                date(2026, 9, 27),
                vec![
                    timed(date(2026, 9, 27), hm(10, 0), Some(hm(12, 0)), "Brunch"),
                    timed(date(2026, 9, 27), hm(20, 0), None, "<Movie>"),
                ],
            ),
        ];

        let (plain, html) = range_message(&week_title(today), &days, today, 0, 1);

        assert_eq!(
            plain,
            "📆 Next 7 days · 25 Sep to 1 Oct\n\
             🔹 Fri 25 Sep · today\n\
             • 19:00 Plenum\n\
             \n\
             🔹 Sun 27 Sep\n\
             • 10:00–12:00 Brunch\n\
             • 20:00 <Movie>"
        );
        assert!(html.contains("<strong>Sun 27 Sep</strong>"));
        assert!(html.contains("&lt;Movie&gt;"));
    }

    #[test]
    fn next_skips_started_events_and_says_when() {
        let today = date(2026, 9, 25);
        let days = vec![
            (today, vec![timed(today, hm(9, 0), None, "Standup")]),
            (date(2026, 9, 26), vec![]),
            (
                date(2026, 9, 27),
                vec![timed(date(2026, 9, 27), hm(19, 0), None, "Plenum")],
            ),
        ];

        let (plain, _) = next_message(&days, today.and_time(hm(12, 0)), 0, 1);

        assert_eq!(plain, "⏭️ Next: Plenum\n🕒 Sun 27 Sep 19:00 · in 2 days");
    }

    #[test]
    fn long_titles_are_shortened() {
        let long = "x".repeat(300);
        let today = date(2026, 9, 25);
        let (plain, _) = day_message(today, today, &[timed(today, hm(9, 0), None, &long)], 0, 1);
        assert!(plain.lines().nth(1).unwrap().chars().count() < 140);
        assert!(plain.contains('…'));
    }

    #[test]
    fn update_marks_new_and_moved_events_and_lists_the_gone_ones() {
        let today = date(2026, 10, 20);
        let thu = date(2026, 10, 22);
        let plenum = timed(thu, hm(19, 0), None, "Plenum");
        let film = timed(thu, hm(21, 0), None, "Film <night>");
        let days = vec![
            (today, vec![]),
            (date(2026, 10, 21), vec![]),
            (thu, vec![plenum.clone(), film.clone()]),
        ];
        let marks = HashMap::from([
            (
                plenum.key.clone(),
                Mark::Moved(date(2026, 10, 21).and_time(hm(19, 0))),
            ),
            (film.key.clone(), Mark::New),
        ]);
        let gone = [
            Gone {
                summary: "Party".to_owned(),
                starts_at: date(2026, 10, 24).and_time(hm(20, 0)),
                all_day: false,
                moved_to: None,
            },
            Gone {
                summary: "Flea market".to_owned(),
                starts_at: date(2026, 10, 25).and_time(NaiveTime::MIN),
                all_day: true,
                moved_to: Some(date(2026, 11, 1).and_time(NaiveTime::MIN)),
            },
        ];

        let (plain, html) = update_message(&days, today, &marks, &gone, 0, 1);

        assert_eq!(
            plain,
            "🔄 Calendar update · 20 Oct to 22 Oct\n\
             🔹 Thu 22 Oct\n\
             • 📅 19:00 Plenum (was Wed 21 Oct 19:00)\n\
             • 🆕 21:00 Film <night>\n\
             \n\
             ❌ Cancelled\n\
             • Sat 24 Oct 20:00 Party\n\
             \n\
             📅 Moved to another week\n\
             • Sun 25 Oct Flea market → Sun 1 Nov"
        );
        assert!(html.contains("<strong>Film &lt;night&gt;</strong>"));
    }
}
