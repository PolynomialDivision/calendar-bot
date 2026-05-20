use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, Weekday};
use futures_util::StreamExt;
use matrix_sdk::{
    Client, Room, RoomState, SessionMeta, SessionTokens,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    encryption::verification::{SasState, Verification, VerificationRequest, VerificationRequestState},
    ruma::{
        OwnedDeviceId, OwnedUserId,
        api::client::filter::FilterDefinition,
        events::{
            key::verification::request::ToDeviceKeyVerificationRequestEvent,
            room::{
                member::StrippedRoomMemberEvent,
                message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
            },
        },
    },
};
use matrix_sdk_base::crypto::CollectStrategy;
use serde::Deserialize;
use tokio::{fs, time::sleep, time::Duration as TokioDuration};
use tracing::{error, info, warn};

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Config {
    calendar: CalendarConfig,
    matrix: MatrixConfig,
    #[serde(default)]
    security: SecurityConfig,
}

#[derive(Deserialize, Clone)]
struct CalendarSource {
    /// Direct ICS URL (GET request).
    ics_url: Option<String>,
    /// CalDAV collection URL (REPORT request — server-side date filtering, handles recurrence).
    caldav_url: Option<String>,
    /// CalDAV account root URL — discovers and fetches all calendars automatically.
    caldav_account_url: Option<String>,
    /// HTTP Basic Auth username (optional).
    username: Option<String>,
    /// HTTP Basic Auth password (optional).
    password: Option<String>,
    /// Calendar slugs to skip when using caldav_account_url (e.g. ["contacts", "birthdays"]).
    #[serde(default)]
    exclude: Vec<String>,
}

fn default_true() -> bool { true }
fn default_weekly_day() -> String { "monday".to_owned() }
fn default_monthly_day() -> u32 { 1 }

fn parse_weekday(s: &str) -> Weekday {
    match s.to_lowercase().as_str() {
        "tuesday"   | "tue" => Weekday::Tue,
        "wednesday" | "wed" => Weekday::Wed,
        "thursday"  | "thu" => Weekday::Thu,
        "friday"    | "fri" => Weekday::Fri,
        "saturday"  | "sat" => Weekday::Sat,
        "sunday"    | "sun" => Weekday::Sun,
        _                   => Weekday::Mon,
    }
}

/// Returns the date for day-of-month `day` in the given year/month,
/// clamping to the last day if the month is shorter (e.g. day 31 in April → 30 Apr).
fn month_day(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).unwrap_or_else(|| {
        let next = if month == 12 {
            NaiveDate::from_ymd_opt(year + 1, 1, 1).unwrap()
        } else {
            NaiveDate::from_ymd_opt(year, month + 1, 1).unwrap()
        };
        next - Duration::days(1)
    })
}

#[derive(Deserialize, Default, Clone)]
struct DailySummaryConfig {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_reminder_time")]
    time: String,
    #[serde(default)]
    post_if_empty: bool,
}

#[derive(Deserialize, Default, Clone)]
struct WeeklySummaryConfig {
    #[serde(default)]
    enabled: bool,
    /// Day of week to post (monday–sunday). Default: monday.
    #[serde(default = "default_weekly_day")]
    day: String,
    #[serde(default = "default_reminder_time")]
    time: String,
    #[serde(default)]
    post_if_empty: bool,
}

#[derive(Deserialize, Default, Clone)]
struct MonthlySummaryConfig {
    #[serde(default)]
    enabled: bool,
    /// Day of month to post (1–31, clamped to last day if month is shorter). Default: 1.
    #[serde(default = "default_monthly_day")]
    day: u32,
    #[serde(default = "default_reminder_time")]
    time: String,
    #[serde(default)]
    post_if_empty: bool,
}

#[derive(Deserialize, Clone)]
struct CalendarConfig {
    /// One or more calendar sources to aggregate.
    sources: Vec<CalendarSource>,
    #[serde(default)]
    daily: DailySummaryConfig,
    #[serde(default)]
    weekly: WeeklySummaryConfig,
    #[serde(default)]
    monthly: MonthlySummaryConfig,
}

fn default_reminder_time() -> String {
    "07:00".to_owned()
}

#[derive(Deserialize)]
struct MatrixConfig {
    homeserver: String,
    user_id: String,
    access_token: String,
    device_id: String,
    recovery_key: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum EncryptionStrategy {
    AllDevices,
    #[default]
    IdentityBased,
    OnlyTrusted,
}

impl From<EncryptionStrategy> for CollectStrategy {
    fn from(s: EncryptionStrategy) -> Self {
        match s {
            EncryptionStrategy::AllDevices => CollectStrategy::AllDevices,
            EncryptionStrategy::IdentityBased => CollectStrategy::IdentityBasedStrategy,
            EncryptionStrategy::OnlyTrusted => CollectStrategy::OnlyTrustedDevices,
        }
    }
}

#[derive(Deserialize, Default)]
struct SecurityConfig {
    #[serde(default)]
    allowed_inviters: Vec<String>,
    #[serde(default)]
    admin_users: Vec<String>,
    #[serde(default)]
    encryption_strategy: EncryptionStrategy,
}

// ── Bot state ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct BotState {
    bot_user_id: OwnedUserId,
    allowed_inviters: HashSet<OwnedUserId>,
    admin_users: HashSet<OwnedUserId>,
    reset_allowed: Arc<Mutex<HashSet<OwnedUserId>>>,
}

// ── ICS parsing ───────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct IcsEvent {
    summary: String,
    dtstart: String,
    dtstart_is_date: bool, // VALUE=DATE or 8-char YYYYMMDD
    location: Option<String>,
    description: Option<String>,
    rrule: Option<String>,
    exdates: Vec<String>,
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

fn parse_ics(ics: &str) -> Vec<IcsEvent> {
    let unfolded = unfold(ics);
    let mut events: Vec<IcsEvent> = Vec::new();
    let mut current: Option<IcsEvent> = None;

    for raw_line in unfolded.lines() {
        let line = raw_line.trim_end_matches('\r');

        if line == "BEGIN:VEVENT" {
            current = Some(IcsEvent::default());
            continue;
        }
        if line == "END:VEVENT" {
            if let Some(ev) = current.take() {
                if !ev.dtstart.is_empty() {
                    events.push(ev);
                }
            }
            continue;
        }

        let Some(ref mut ev) = current else { continue };

        // Split "KEY;PARAMS:VALUE" — the first colon separates key+params from value.
        let Some(colon) = line.find(':') else { continue };
        let key_part = &line[..colon];
        let value = &line[colon + 1..];

        // Split "KEY;PARAMS" into key name and params string.
        let (key_name, params) = key_part
            .find(';')
            .map(|i| (&key_part[..i], &key_part[i + 1..]))
            .unwrap_or((key_part, ""));

        match key_name {
            "SUMMARY" => ev.summary = unescape(value),
            "DTSTART" => {
                ev.dtstart = value.to_owned();
                // VALUE=DATE param, or bare 8-char string = all-day
                ev.dtstart_is_date = params.contains("VALUE=DATE") || value.len() == 8;
            }
            "LOCATION" => ev.location = Some(unescape(value)),
            "DESCRIPTION" => ev.description = Some(unescape(value)),
            "RRULE" => ev.rrule = Some(value.to_owned()),
            "EXDATE" => ev.exdates.push(value[..8.min(value.len())].to_owned()),
            _ => {}
        }
    }

    events
}

/// Parse ICS date string (YYYYMMDD or YYYYMMDDTHHMMSS[Z]) → NaiveDate.
fn parse_dtstart_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim_end_matches('Z');
    NaiveDate::parse_from_str(&s[..8.min(s.len())], "%Y%m%d").ok()
}

/// Parse ICS datetime string (YYYYMMDDTHHMMSS[Z]) → NaiveDateTime (local/float).
fn parse_dtstart_datetime(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim_end_matches('Z');
    if s.len() >= 15 {
        NaiveDateTime::parse_from_str(&s[..15], "%Y%m%dT%H%M%S").ok()
    } else {
        None
    }
}

fn weekday_code(wd: Weekday) -> &'static str {
    match wd {
        Weekday::Mon => "MO",
        Weekday::Tue => "TU",
        Weekday::Wed => "WE",
        Weekday::Thu => "TH",
        Weekday::Fri => "FR",
        Weekday::Sat => "SA",
        Weekday::Sun => "SU",
    }
}

/// Returns true if the recurring event with start date `start` and RRULE `rule`
/// Check if `today` matches a MONTHLY BYDAY entry like "3TH" (3rd Thursday) or "-1FR" (last Friday).
fn monthly_byday_matches(d: &str, today: NaiveDate) -> bool {
    let len = d.len();
    if len < 2 {
        return false;
    }
    // Weekday code is always the last 2 chars (MO TU WE TH FR SA SU).
    let wd_str = &d[len - 2..];
    let prefix = &d[..len - 2];

    if wd_str != weekday_code(today.weekday()) {
        return false;
    }

    let ordinal: i32 = if prefix.is_empty() {
        return true; // no ordinal = any occurrence of this weekday
    } else {
        match prefix.parse::<i32>() {
            Ok(n) => n,
            Err(_) => return false,
        }
    };

    if ordinal > 0 {
        // (day - 1) / 7 + 1 gives the occurrence number within the month (1-based).
        ((today.day() - 1) / 7 + 1) as i32 == ordinal
    } else {
        // Negative ordinal: count from end of month. -1 = last occurrence.
        let next_month = if today.month() == 12 {
            NaiveDate::from_ymd_opt(today.year() + 1, 1, 1).unwrap()
        } else {
            NaiveDate::from_ymd_opt(today.year(), today.month() + 1, 1).unwrap()
        };
        let last_day = next_month - Duration::days(1);
        let days_from_end = (last_day - today).num_days() as i32;
        -((days_from_end / 7) + 1) == ordinal
    }
}

/// has an occurrence on `today`. Handles FREQ=DAILY/WEEKLY/MONTHLY/YEARLY,
/// BYDAY, and UNTIL.
fn rrule_matches(start: NaiveDate, rule: &str, today: NaiveDate) -> bool {
    if start > today {
        return false;
    }

    let mut freq = "";
    let mut byday: Vec<&str> = Vec::new();
    let mut until: Option<NaiveDate> = None;

    for part in rule.split(';') {
        if let Some(v) = part.strip_prefix("FREQ=") {
            freq = v;
        } else if let Some(v) = part.strip_prefix("BYDAY=") {
            byday = v.split(',').collect();
        } else if let Some(v) = part.strip_prefix("UNTIL=") {
            until = NaiveDate::parse_from_str(&v[..8.min(v.len())], "%Y%m%d").ok();
        }
    }

    if let Some(until_date) = until {
        if today > until_date {
            return false;
        }
    }

    match freq {
        "DAILY" => true,
        "WEEKLY" => {
            if byday.is_empty() {
                start.weekday() == today.weekday()
            } else {
                let wd = weekday_code(today.weekday());
                byday.iter().any(|d| d.contains(wd))
            }
        }
        "MONTHLY" => {
            if byday.is_empty() {
                start.day() == today.day()
            } else {
                byday.iter().any(|d| monthly_byday_matches(d, today))
            }
        }
        "YEARLY" => start.month() == today.month() && start.day() == today.day(),
        _ => false,
    }
}

#[derive(Debug)]
enum EventTime {
    AllDay,
    At(NaiveDateTime),
}

#[derive(Debug)]
struct DayEvent {
    time: EventTime,
    summary: String,
    location: Option<String>,
}

impl DayEvent {
    fn sort_key(&self) -> i64 {
        match &self.time {
            EventTime::At(dt) => dt.and_utc().timestamp(),
            EventTime::AllDay => i64::MAX, // all-day events shown last
        }
    }
}

/// Filter parsed ICS events for those occurring on `today`.
fn events_for_today(events: &[IcsEvent], today: NaiveDate) -> Vec<DayEvent> {
    let mut result = Vec::new();

    for ev in events {
        let start_date = match parse_dtstart_date(&ev.dtstart) {
            Some(d) => d,
            None => continue,
        };

        let occurs_today = if let Some(rule) = &ev.rrule {
            // Check recurrence
            let today_str = today.format("%Y%m%d").to_string();
            if ev.exdates.contains(&today_str) {
                continue; // excluded
            }
            rrule_matches(start_date, rule, today)
        } else {
            start_date == today
        };

        if !occurs_today {
            continue;
        }

        let time = if ev.dtstart_is_date {
            EventTime::AllDay
        } else {
            match parse_dtstart_datetime(&ev.dtstart) {
                Some(dt) => EventTime::At(dt),
                None => EventTime::AllDay,
            }
        };

        result.push(DayEvent {
            time,
            summary: ev.summary.clone(),
            location: ev.location.clone(),
        });
    }

    result.sort_by_key(|e| e.sort_key());
    result
}

// ── Calendar fetching ─────────────────────────────────────────────────────────

fn add_auth(req: reqwest::RequestBuilder, src: &CalendarSource) -> reqwest::RequestBuilder {
    if let (Some(u), Some(p)) = (&src.username, &src.password) {
        req.basic_auth(u, Some(p))
    } else {
        req
    }
}

async fn fetch_ics_direct(http: &reqwest::Client, src: &CalendarSource) -> Result<String> {
    let url = src.ics_url.as_deref().context("No ics_url configured")?;
    let req = add_auth(http.get(url), src);
    Ok(req.send().await?.text().await?)
}

async fn fetch_ics_caldav(
    http: &reqwest::Client,
    src: &CalendarSource,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<Vec<String>> {
    let url = src.caldav_url.as_deref().context("No caldav_url configured")?;
    let start = start.format("%Y%m%dT000000Z").to_string();
    let end = end.format("%Y%m%dT000000Z").to_string();

    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8" ?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><C:calendar-data/></D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT">
        <C:time-range start="{start}" end="{end}"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#
    );

    let method = reqwest::Method::from_bytes(b"REPORT").expect("valid method");
    let req = add_auth(
        http.request(method, url)
            .header("Content-Type", "application/xml; charset=utf-8")
            .header("Depth", "1")
            .body(body),
        src,
    );

    let xml = req.send().await?.text().await?;

    let mut blocks = Vec::new();
    let mut pos = 0;
    while let Some(rel) = xml[pos..].find("BEGIN:VCALENDAR") {
        let start_idx = pos + rel;
        if let Some(rel_end) = xml[start_idx..].find("END:VCALENDAR") {
            let end_idx = start_idx + rel_end + "END:VCALENDAR".len();
            blocks.push(xml[start_idx..end_idx].to_owned());
            pos = end_idx;
        } else {
            break;
        }
    }

    Ok(blocks)
}

/// PROPFIND the account root and return all calendar collection URLs found.
async fn discover_calendars(
    http: &reqwest::Client,
    src: &CalendarSource,
) -> Result<Vec<String>> {
    let account_url = src.caldav_account_url.as_deref().context("No caldav_account_url")?;

    let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:resourcetype/>
    <D:displayname/>
  </D:prop>
</D:propfind>"#;

    let method = reqwest::Method::from_bytes(b"PROPFIND").expect("valid method");
    let req = add_auth(
        http.request(method, account_url)
            .header("Content-Type", "application/xml; charset=utf-8")
            .header("Depth", "1")
            .body(body),
        src,
    );

    let xml = req.send().await?.text().await?;

    // Extract href values from responses that contain <C:calendar/> resourcetype.
    // We split by <D:response> blocks and keep those that declare a calendar collection.
    let mut urls = Vec::new();
    let mut pos = 0;
    while let Some(rel) = xml[pos..].find("<D:response>").or_else(|| xml[pos..].find("<d:response>")) {
        let block_start = pos + rel;
        let close_tag = if xml[block_start..].starts_with("<D:") { "</D:response>" } else { "</d:response>" };
        let block_end = xml[block_start..]
            .find(close_tag)
            .map(|i| block_start + i + close_tag.len())
            .unwrap_or(xml.len());
        let block = &xml[block_start..block_end];
        pos = block_end;

        let is_calendar = block.contains(":calendar/>")
            || block.contains(":calendar />")
            || block.contains(":calendar>");
        if !is_calendar {
            continue;
        }

        // Extract the href from this response block.
        let href = block
            .find("<D:href>").or_else(|| block.find("<d:href>"))
            .and_then(|s| {
                let after = &block[s..];
                let val_start = after.find('>').map(|i| i + 1)?;
                let val_end = after[val_start..].find('<').map(|i| val_start + i)?;
                Some(after[val_start..val_end].trim().to_owned())
            });

        let Some(href) = href else { continue };

        // Skip the account root itself (returned as first entry).
        let account_path = reqwest::Url::parse(account_url)
            .ok()
            .map(|u| u.path().trim_end_matches('/').to_owned())
            .unwrap_or_default();
        let href_path = href.trim_end_matches('/');
        if href_path == account_path {
            continue;
        }

        // Check exclude list against the last path segment (calendar slug).
        let slug = href_path.rsplit('/').next().unwrap_or(href_path);
        if src.exclude.iter().any(|ex| ex == slug) {
            info!("Skipping excluded calendar: {slug}");
            continue;
        }

        // Build absolute URL if href is a path.
        let absolute = if href.starts_with("http://") || href.starts_with("https://") {
            href
        } else {
            let base = reqwest::Url::parse(account_url)?;
            base.join(&href)?.to_string()
        };

        urls.push(absolute);
    }

    Ok(urls)
}

/// Fetch raw ICS events from a single source for a date range (no day filtering).
async fn fetch_source_ics_events(
    http: &reqwest::Client,
    src: &CalendarSource,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<Vec<IcsEvent>> {
    if src.caldav_account_url.is_some() {
        let calendar_urls = discover_calendars(http, src).await?;
        info!("Discovered {} calendar(s) from account", calendar_urls.len());
        let mut all: Vec<IcsEvent> = Vec::new();
        for url in &calendar_urls {
            let cal_src = CalendarSource {
                caldav_url: Some(url.clone()),
                caldav_account_url: None,
                ics_url: None,
                username: src.username.clone(),
                password: src.password.clone(),
                exclude: vec![],
            };
            match fetch_ics_caldav(http, &cal_src, start, end).await {
                Ok(blocks) => blocks.iter().for_each(|b| all.extend(parse_ics(b))),
                Err(e) => warn!("Failed to fetch calendar {url}: {e}"),
            }
        }
        Ok(all)
    } else if src.caldav_url.is_some() {
        let blocks = fetch_ics_caldav(http, src, start, end).await?;
        Ok(blocks.iter().flat_map(|b| parse_ics(b)).collect())
    } else {
        let ics = fetch_ics_direct(http, src).await?;
        Ok(parse_ics(&ics))
    }
}

async fn get_todays_events(
    http: &reqwest::Client,
    config: &CalendarConfig,
    today: NaiveDate,
) -> Vec<DayEvent> {
    let mut all: Vec<IcsEvent> = Vec::new();
    for (i, src) in config.sources.iter().enumerate() {
        match fetch_source_ics_events(http, src, today, today + Duration::days(1)).await {
            Ok(events) => {
                info!("Source {}: {} raw event(s)", i + 1, events.len());
                all.extend(events);
            }
            Err(e) => warn!("Source {} failed: {e}", i + 1),
        }
    }
    let mut result = events_for_today(&all, today);
    result.sort_by_key(|e| e.sort_key());
    result
}

/// Returns events grouped by day for a 7-day window starting at `week_start`.
async fn get_week_events(
    http: &reqwest::Client,
    config: &CalendarConfig,
    week_start: NaiveDate,
) -> Vec<(NaiveDate, Vec<DayEvent>)> {
    let week_end = week_start + Duration::days(7);
    let mut all: Vec<IcsEvent> = Vec::new();
    for (i, src) in config.sources.iter().enumerate() {
        match fetch_source_ics_events(http, src, week_start, week_end).await {
            Ok(events) => {
                info!("Weekly source {}: {} raw event(s)", i + 1, events.len());
                all.extend(events);
            }
            Err(e) => warn!("Weekly source {} failed: {e}", i + 1),
        }
    }
    (0..7)
        .map(|d| {
            let day = week_start + Duration::days(d);
            let mut events = events_for_today(&all, day);
            events.sort_by_key(|e| e.sort_key());
            (day, events)
        })
        .collect()
}

// ── Message formatting ────────────────────────────────────────────────────────

fn format_today_message(today: NaiveDate, events: &[DayEvent]) -> (String, String) {
    let day_str = today.format("%a %d %b %Y").to_string();

    if events.is_empty() {
        let plain = format!("📅 No events today ({day_str}).");
        return (plain.clone(), plain);
    }

    let mut plain_lines = vec![format!("📅 {day_str}")];
    let mut html_lines = vec![format!("📅 <strong>{day_str}</strong>")];

    for ev in events {
        let time_str = match &ev.time {
            EventTime::AllDay => "All day".to_owned(),
            EventTime::At(dt) => dt.format("%H:%M").to_string(),
        };
        let loc_plain = ev.location.as_deref().map(|l| format!(" [{l}]")).unwrap_or_default();
        let loc_html = ev.location.as_deref()
            .map(|l| format!(" <em>[{}]</em>", html_escape(l)))
            .unwrap_or_default();
        plain_lines.push(format!("• {} {}{}", time_str, ev.summary, loc_plain));
        html_lines.push(format!("• {} {}{}", time_str, html_escape(&ev.summary), loc_html));
    }

    (plain_lines.join("\n"), html_lines.join("<br>"))
}

fn format_week_message(
    week_start: NaiveDate,
    week_days: &[(NaiveDate, Vec<DayEvent>)],
) -> (String, String) {
    let week_end = week_start + Duration::days(6);
    let header = format!(
        "📆 {} – {}",
        week_start.format("%a %d %b"),
        week_end.format("%a %d %b %Y"),
    );

    let has_events = week_days.iter().any(|(_, evs)| !evs.is_empty());
    if !has_events {
        let plain = format!("{header}: no events.");
        return (plain.clone(), plain);
    }

    let mut plain_lines = vec![format!("{header}")];
    let mut html_lines = vec![format!("<strong>{header}</strong>")];

    for (day, events) in week_days {
        if events.is_empty() {
            continue;
        }
        let day_prefix = day.format("%a %d").to_string();
        for ev in events {
            let time_str = match &ev.time {
                EventTime::AllDay => "All day".to_owned(),
                EventTime::At(dt) => dt.format("%H:%M").to_string(),
            };
            let loc_plain = ev.location.as_deref().map(|l| format!(" [{l}]")).unwrap_or_default();
            let loc_html = ev.location.as_deref()
                .map(|l| format!(" <em>[{}]</em>", html_escape(l)))
                .unwrap_or_default();
            plain_lines.push(format!("{day_prefix} · {} {}{}", time_str, ev.summary, loc_plain));
            html_lines.push(format!(
                "<strong>{day_prefix}</strong> · {} {}{}",
                time_str, html_escape(&ev.summary), loc_html,
            ));
        }
    }

    (plain_lines.join("\n"), html_lines.join("<br>"))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── Scheduler ─────────────────────────────────────────────────────────────────

fn parse_hm(s: &str) -> (u32, u32) {
    let mut parts = s.splitn(2, ':');
    let h = parts.next().and_then(|s| s.parse().ok()).unwrap_or(7);
    let m = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (h, m)
}

async fn check_and_post(_state: &BotState, client: &Client, http: &reqwest::Client, config: &CalendarConfig) {
    let today = Local::now().date_naive();
    let events = get_todays_events(http, config, today).await;

    if events.is_empty() && !config.daily.post_if_empty {
        info!("No events today — staying silent");
        return;
    }

    let (plain, html) = format_today_message(today, &events);
    info!("Posting daily summary ({} event(s)) to Matrix", events.len());

    for room in client.joined_rooms() {
        if let Err(e) = room.send(RoomMessageEventContent::text_html(&plain, &html)).await {
            error!("Failed to send daily summary to {}: {e}", room.room_id());
        }
    }
}

async fn daily_scheduler_loop(
    state: BotState,
    client: Client,
    http: reqwest::Client,
    config: CalendarConfig,
    test_mode: bool,
) {
    if test_mode {
        info!("Test mode: posting daily summary immediately");
        check_and_post(&state, &client, &http, &config).await;
        return;
    }

    let (hour, minute) = parse_hm(&config.daily.time);
    let target = NaiveTime::from_hms_opt(hour, minute, 0).expect("invalid daily time");

    loop {
        let now = Local::now();
        let today = now.date_naive();
        let next_dt = if now.time() < target {
            today.and_time(target)
        } else {
            (today + Duration::days(1)).and_time(target)
        };
        let secs = (next_dt - now.naive_local()).num_seconds().max(0) as u64;
        info!("Next daily summary in {secs}s (at {hour:02}:{minute:02})");
        sleep(TokioDuration::from_secs(secs)).await;
        check_and_post(&state, &client, &http, &config).await;
    }
}

async fn check_and_post_weekly(
    _state: &BotState,
    client: &Client,
    http: &reqwest::Client,
    config: &CalendarConfig,
) {
    let week_start = Local::now().date_naive();
    let week_days = get_week_events(http, config, week_start).await;
    let total: usize = week_days.iter().map(|(_, evs)| evs.len()).sum();

    if total == 0 && !config.weekly.post_if_empty {
        info!("No events this week — staying silent");
        return;
    }

    let (plain, html) = format_week_message(week_start, &week_days);
    info!("Posting weekly summary ({total} event(s)) to Matrix");

    for room in client.joined_rooms() {
        if let Err(e) = room
            .send(RoomMessageEventContent::text_html(&plain, &html))
            .await
        {
            error!("Failed to send weekly summary to {}: {e}", room.room_id());
        }
    }
}

async fn weekly_scheduler_loop(
    state: BotState,
    client: Client,
    http: reqwest::Client,
    config: CalendarConfig,
    test_mode: bool,
) {
    if test_mode {
        info!("Test mode: posting weekly summary immediately");
        check_and_post_weekly(&state, &client, &http, &config).await;
        return;
    }

    let weekday = parse_weekday(&config.weekly.day);
    let (hour, minute) = parse_hm(&config.weekly.time);
    let target_time = NaiveTime::from_hms_opt(hour, minute, 0).expect("invalid weekly time");

    loop {
        let now = Local::now();
        let today = now.date_naive();

        // Days until the next occurrence of the configured weekday.
        let days_until = (weekday.num_days_from_monday() as i64
            - today.weekday().num_days_from_monday() as i64)
            .rem_euclid(7);
        // If today is that weekday but the time has passed, jump to next week.
        let days_until = if days_until == 0 && now.time() >= target_time {
            7
        } else {
            days_until
        };

        let target_date = today + Duration::days(days_until);
        let target_dt = target_date.and_time(target_time);
        let secs = (target_dt - now.naive_local()).num_seconds().max(0) as u64;
        info!(
            "Next weekly summary in {secs}s (on {} at {hour:02}:{minute:02})",
            target_date.format("%A %d %b")
        );
        sleep(TokioDuration::from_secs(secs)).await;
        check_and_post_weekly(&state, &client, &http, &config).await;
    }
}

/// Returns events grouped by day for the full calendar month containing `month_start`.
async fn get_month_events(
    http: &reqwest::Client,
    config: &CalendarConfig,
    month_start: NaiveDate,
) -> Vec<(NaiveDate, Vec<DayEvent>)> {
    let first = NaiveDate::from_ymd_opt(month_start.year(), month_start.month(), 1).unwrap();
    let next_month_first = if first.month() == 12 {
        NaiveDate::from_ymd_opt(first.year() + 1, 1, 1).unwrap()
    } else {
        NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1).unwrap()
    };
    let days_in_month = (next_month_first - first).num_days();

    let mut all: Vec<IcsEvent> = Vec::new();
    for (i, src) in config.sources.iter().enumerate() {
        match fetch_source_ics_events(http, src, first, next_month_first).await {
            Ok(events) => {
                info!("Monthly source {}: {} raw event(s)", i + 1, events.len());
                all.extend(events);
            }
            Err(e) => warn!("Monthly source {} failed: {e}", i + 1),
        }
    }

    (0..days_in_month)
        .map(|d| {
            let day = first + Duration::days(d);
            let mut events = events_for_today(&all, day);
            events.sort_by_key(|e| e.sort_key());
            (day, events)
        })
        .collect()
}

fn format_month_message(
    month_start: NaiveDate,
    month_days: &[(NaiveDate, Vec<DayEvent>)],
) -> (String, String) {
    let header = format!("📅 {}", month_start.format("%B %Y"));

    let has_events = month_days.iter().any(|(_, evs)| !evs.is_empty());
    if !has_events {
        let plain = format!("{header}: no events.");
        return (plain.clone(), plain);
    }

    let mut plain_lines = vec![format!("{header}")];
    let mut html_lines = vec![format!("<strong>{header}</strong>")];

    for (day, events) in month_days {
        if events.is_empty() {
            continue;
        }
        let day_prefix = day.format("%a %d").to_string();
        for ev in events {
            let time_str = match &ev.time {
                EventTime::AllDay => "All day".to_owned(),
                EventTime::At(dt) => dt.format("%H:%M").to_string(),
            };
            let loc_plain = ev.location.as_deref().map(|l| format!(" [{l}]")).unwrap_or_default();
            let loc_html = ev.location.as_deref()
                .map(|l| format!(" <em>[{}]</em>", html_escape(l)))
                .unwrap_or_default();
            plain_lines.push(format!("{day_prefix} · {} {}{}", time_str, ev.summary, loc_plain));
            html_lines.push(format!(
                "<strong>{day_prefix}</strong> · {} {}{}",
                time_str, html_escape(&ev.summary), loc_html,
            ));
        }
    }

    (plain_lines.join("\n"), html_lines.join("<br>"))
}

async fn check_and_post_monthly(
    _state: &BotState,
    client: &Client,
    http: &reqwest::Client,
    config: &CalendarConfig,
) {
    let today = Local::now().date_naive();
    let month_days = get_month_events(http, config, today).await;
    let total: usize = month_days.iter().map(|(_, evs)| evs.len()).sum();

    if total == 0 && !config.monthly.post_if_empty {
        info!("No events this month — staying silent");
        return;
    }

    let (plain, html) = format_month_message(today, &month_days);
    info!("Posting monthly summary ({total} event(s)) to Matrix");

    for room in client.joined_rooms() {
        if let Err(e) = room.send(RoomMessageEventContent::text_html(&plain, &html)).await {
            error!("Failed to send monthly summary to {}: {e}", room.room_id());
        }
    }
}

async fn monthly_scheduler_loop(
    state: BotState,
    client: Client,
    http: reqwest::Client,
    config: CalendarConfig,
    test_mode: bool,
) {
    if test_mode {
        info!("Test mode: posting monthly summary immediately");
        check_and_post_monthly(&state, &client, &http, &config).await;
        return;
    }

    let (hour, minute) = parse_hm(&config.monthly.time);
    let target_time = NaiveTime::from_hms_opt(hour, minute, 0).expect("invalid monthly time");
    let dom = config.monthly.day.clamp(1, 31);

    loop {
        let now = Local::now();
        let today = now.date_naive();

        // Try this month's target date first, then next month if already passed.
        let candidate = month_day(today.year(), today.month(), dom);
        let target_date = if candidate > today || (candidate == today && now.time() < target_time) {
            candidate
        } else {
            let (ny, nm) = if today.month() == 12 {
                (today.year() + 1, 1)
            } else {
                (today.year(), today.month() + 1)
            };
            month_day(ny, nm, dom)
        };

        let target_dt = target_date.and_time(target_time);
        let secs = (target_dt - now.naive_local()).num_seconds().max(0) as u64;
        info!(
            "Next monthly summary in {secs}s (on {} at {hour:02}:{minute:02})",
            target_date.format("%d %b %Y")
        );
        sleep(TokioDuration::from_secs(secs)).await;
        check_and_post_monthly(&state, &client, &http, &config).await;
    }
}

// ── Verification ──────────────────────────────────────────────────────────────

async fn handle_verification_request(client: Client, state: BotState, request: VerificationRequest) {
    let user_id = request.other_user_id();

    let already_verified = client
        .encryption()
        .get_user_devices(user_id)
        .await
        .map(|devices| devices.devices().any(|d| d.is_verified()))
        .unwrap_or(false);

    if already_verified {
        let allowed = state.reset_allowed.lock().await.remove(user_id);
        if !allowed {
            warn!("Rejecting verification from {} — already has a verified device", user_id);
            request.cancel().await.ok();
            return;
        }
        info!("Allowing re-verification for {} (trust was reset by admin)", user_id);
    }

    info!("Accepting verification from {user_id}");
    if let Err(e) = request.accept().await {
        error!("Failed to accept verification: {e}");
        return;
    }

    let mut stream = request.changes();
    while let Some(state) = stream.next().await {
        match state {
            VerificationRequestState::Transitioned { verification } => {
                if let Verification::SasV1(sas) = verification {
                    tokio::spawn(handle_sas(sas));
                    break;
                }
            }
            VerificationRequestState::Done | VerificationRequestState::Cancelled(_) => break,
            _ => {}
        }
    }
}

async fn handle_sas(sas: matrix_sdk::encryption::verification::SasVerification) {
    if let Err(e) = sas.accept().await {
        error!("SAS accept failed: {e}");
        return;
    }
    let mut stream = sas.changes();
    while let Some(state) = stream.next().await {
        match state {
            SasState::KeysExchanged { emojis, .. } => {
                if let Some(e) = emojis {
                    info!(
                        "SAS emojis: {:?}",
                        e.emojis.iter().map(|em| em.description).collect::<Vec<_>>()
                    );
                }
                if let Err(e) = sas.confirm().await {
                    error!("SAS confirm failed: {e}");
                }
            }
            SasState::Done { .. } => {
                info!("SAS verification complete");
                break;
            }
            SasState::Cancelled(info) => {
                warn!("SAS cancelled: {:?}", info.cancel_code());
                break;
            }
            _ => {}
        }
    }
}

async fn bootstrap_cross_signing(client: &Client, user_id: &OwnedUserId) {
    match client.encryption().bootstrap_cross_signing(None).await {
        Ok(()) => info!("Cross-signing bootstrapped for {user_id}"),
        Err(e) => warn!("Cross-signing bootstrap failed: {e}"),
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "calendar_bot=info,matrix_sdk=warn".parse().unwrap()),
        )
        .init();

    let test_mode = std::env::args().any(|a| a == "--test");
    let config_path = std::env::args()
        .find(|a| a.ends_with(".toml"))
        .unwrap_or_else(|| "config.toml".to_owned());

    let config_str = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config: {config_path}"))?;
    let config: Config = toml::from_str(&config_str)?;

    if config.calendar.sources.is_empty() {
        anyhow::bail!("Config [calendar]: add at least one [[calendar.sources]] entry");
    }

    let store_path = PathBuf::from(
        std::env::var("STORE_PATH").unwrap_or_else(|_| "store".to_owned()),
    );
    fs::create_dir_all(&store_path).await?;

    let strategy: CollectStrategy = config.security.encryption_strategy.into();
    let client = Client::builder()
        .homeserver_url(&config.matrix.homeserver)
        .sqlite_store(&store_path, None)
        .with_room_key_recipient_strategy(strategy)
        .build()
        .await?;

    let user_id: OwnedUserId = config.matrix.user_id.parse()?;
    let device_id: OwnedDeviceId = OwnedDeviceId::from(config.matrix.device_id);

    client
        .restore_session(MatrixSession {
            meta: SessionMeta { user_id: user_id.clone(), device_id },
            tokens: SessionTokens {
                access_token: config.matrix.access_token,
                refresh_token: None,
            },
        })
        .await?;
    info!("Session restored as {user_id}");

    if let Some(ref key) = config.matrix.recovery_key {
        match client.encryption().recovery().recover(key).await {
            Ok(()) => info!("Cross-signing keys recovered"),
            Err(e) => warn!("Recovery failed: {e}"),
        }
    }
    bootstrap_cross_signing(&client, &user_id).await;

    let allowed_inviters: HashSet<OwnedUserId> = config
        .security
        .allowed_inviters
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    if allowed_inviters.is_empty() {
        warn!("No allowed_inviters configured — bot accepts invites from anyone");
    } else {
        info!("Allowed inviters: {allowed_inviters:?}");
    }

    let admin_users: HashSet<OwnedUserId> = config
        .security
        .admin_users
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    if admin_users.is_empty() {
        warn!("No admin_users configured — !reset-trust command is disabled");
    } else {
        info!("Admin users: {admin_users:?}");
    }

    let bot_state = BotState {
        bot_user_id: user_id,
        allowed_inviters,
        admin_users,
        reset_allowed: Arc::new(Mutex::new(HashSet::new())),
    };

    // Invite handler
    client.add_event_handler({
        let state = bot_state.clone();
        move |ev: StrippedRoomMemberEvent, room: Room| {
            let state = state.clone();
            async move {
                if ev.state_key != state.bot_user_id {
                    return;
                }
                if !state.allowed_inviters.is_empty() && !state.allowed_inviters.contains(&ev.sender) {
                    warn!("Rejecting invite from {} (not in allowed_inviters)", ev.sender);
                    room.leave().await.ok();
                    return;
                }
                info!("Accepted invite from {} to {}", ev.sender, room.room_id());
                tokio::spawn(async move {
                    let mut delay = 2u64;
                    loop {
                        match room.join().await {
                            Ok(_) => { info!("Joined {}", room.room_id()); break; }
                            Err(err) => {
                                warn!("Join failed for {}: {err}; retry in {delay}s", room.room_id());
                                sleep(TokioDuration::from_secs(delay)).await;
                                delay = (delay * 2).min(3600);
                            }
                        }
                    }
                });
            }
        }
    });

    // To-device verification
    client.add_event_handler({
        let state = bot_state.clone();
        move |ev: ToDeviceKeyVerificationRequestEvent, client: Client| {
            let state = state.clone();
            async move {
                let Some(request) = client
                    .encryption()
                    .get_verification_request(&ev.sender, &ev.content.transaction_id)
                    .await
                else {
                    warn!("Verification request object not found");
                    return;
                };
                tokio::spawn(handle_verification_request(client, state, request));
            }
        }
    });

    // In-room messages: verification requests and !reset-trust command
    client.add_event_handler({
        let state = bot_state.clone();
        move |ev: OriginalSyncRoomMessageEvent, room: Room, client: Client| {
            let state = state.clone();
            async move {
                if ev.sender == state.bot_user_id || room.state() != RoomState::Joined {
                    return;
                }
                if let MessageType::VerificationRequest(_) = &ev.content.msgtype {
                    let Some(request) = client
                        .encryption()
                        .get_verification_request(&ev.sender, &ev.event_id)
                        .await
                    else {
                        return;
                    };
                    tokio::spawn(handle_verification_request(client, state, request));
                    return;
                }
                let MessageType::Text(ref text) = ev.content.msgtype else { return };
                if let Some(target) = text.body.trim().strip_prefix("!reset-trust ") {
                    if state.admin_users.contains(&ev.sender) {
                        match target.trim().parse::<OwnedUserId>() {
                            Ok(target_user) => {
                                state.reset_allowed.lock().await.insert(target_user.clone());
                                info!("Trust reset allowed for {} (by {})", target_user, ev.sender);
                            }
                            Err(_) => warn!("!reset-trust: invalid user ID '{}'", target.trim()),
                        }
                    } else {
                        warn!("!reset-trust from non-admin {} — ignored", ev.sender);
                    }
                }
            }
        }
    });

    let http = reqwest::Client::new();

    if config.calendar.daily.enabled {
        tokio::spawn(daily_scheduler_loop(
            bot_state.clone(),
            client.clone(),
            http.clone(),
            config.calendar.clone(),
            test_mode,
        ));
    }

    if config.calendar.weekly.enabled {
        tokio::spawn(weekly_scheduler_loop(
            bot_state.clone(),
            client.clone(),
            http.clone(),
            config.calendar.clone(),
            test_mode,
        ));
    }

    if config.calendar.monthly.enabled {
        tokio::spawn(monthly_scheduler_loop(
            bot_state.clone(),
            client.clone(),
            http.clone(),
            config.calendar.clone(),
            test_mode,
        ));
    }

    info!("Starting sync...");
    let filter = FilterDefinition::with_lazy_loading();
    client.sync(SyncSettings::default().filter(filter.into())).await?;

    Ok(())
}
