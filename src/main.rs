mod ics;
mod render;

use anyhow::{Context, Result};
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, Weekday};
use mxbot_common::{
    config::{MatrixConfig, SecurityConfig, TimeOfDay},
    matrix_sdk::{
        deserialized_responses::EncryptionInfo,
        ruma::events::room::message::{
            MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
        },
        Room, RoomState,
    },
    settings::Settings,
    Bot,
};
use serde::{Deserialize, Serialize};
use tokio::{sync::watch, time::sleep, time::Duration as TokioDuration};
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
    /// CalDAV collection URL (REPORT request, server-side date filtering, handles recurrence).
    caldav_url: Option<String>,
    /// CalDAV account root URL: discovers and fetches all calendars automatically.
    caldav_account_url: Option<String>,
    /// HTTP Basic Auth username (optional).
    username: Option<String>,
    /// HTTP Basic Auth password (optional).
    password: Option<String>,
    /// Calendar slugs to skip when using caldav_account_url (e.g. ["contacts", "birthdays"]).
    #[serde(default)]
    exclude: Vec<String>,
}

fn default_true() -> bool {
    true
}
fn default_weekly_day() -> String {
    "monday".to_owned()
}
fn default_monthly_day() -> u32 {
    1
}

fn parse_weekday(s: &str) -> Weekday {
    match s.to_lowercase().as_str() {
        "tuesday" | "tue" => Weekday::Tue,
        "wednesday" | "wed" => Weekday::Wed,
        "thursday" | "thu" => Weekday::Thu,
        "friday" | "fri" => Weekday::Fri,
        "saturday" | "sat" => Weekday::Sat,
        "sunday" | "sun" => Weekday::Sun,
        _ => Weekday::Mon,
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

#[derive(Deserialize, Clone)]
struct DailySummaryConfig {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_reminder_time")]
    time: TimeOfDay,
    #[serde(default)]
    post_if_empty: bool,
}

impl Default for DailySummaryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            time: default_reminder_time(),
            post_if_empty: false,
        }
    }
}

#[derive(Deserialize, Clone)]
struct WeeklySummaryConfig {
    #[serde(default)]
    enabled: bool,
    /// Day of week to post (monday–sunday). Default: monday.
    #[serde(default = "default_weekly_day")]
    day: String,
    #[serde(default = "default_reminder_time")]
    time: TimeOfDay,
    #[serde(default)]
    post_if_empty: bool,
}

impl Default for WeeklySummaryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            day: default_weekly_day(),
            time: default_reminder_time(),
            post_if_empty: false,
        }
    }
}

#[derive(Deserialize, Clone)]
struct MonthlySummaryConfig {
    #[serde(default)]
    enabled: bool,
    /// Day of month to post (1–31, clamped to last day if month is shorter). Default: 1.
    #[serde(default = "default_monthly_day")]
    day: u32,
    #[serde(default = "default_reminder_time")]
    time: TimeOfDay,
    #[serde(default)]
    post_if_empty: bool,
}

impl Default for MonthlySummaryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            day: default_monthly_day(),
            time: default_reminder_time(),
            post_if_empty: false,
        }
    }
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

fn default_reminder_time() -> TimeOfDay {
    TimeOfDay::new(7, 0)
}

// ── Runtime settings ──────────────────────────────────────────────────────────

/// Summary schedule, adjustable by admins (`!admin set`) and persisted in
/// `store/settings.json`. The `[calendar.*]` config tables are the defaults.
#[derive(Serialize, Deserialize, Clone)]
struct Schedule {
    daily_enabled: bool,
    daily_time: TimeOfDay,
    daily_post_if_empty: bool,
    weekly_enabled: bool,
    weekly_day: String,
    weekly_time: TimeOfDay,
    weekly_post_if_empty: bool,
    monthly_enabled: bool,
    monthly_day: u32,
    monthly_time: TimeOfDay,
    monthly_post_if_empty: bool,
}

impl Schedule {
    fn from_config(config: &CalendarConfig) -> Self {
        Self {
            daily_enabled: config.daily.enabled,
            daily_time: config.daily.time,
            daily_post_if_empty: config.daily.post_if_empty,
            weekly_enabled: config.weekly.enabled,
            weekly_day: config.weekly.day.clone(),
            weekly_time: config.weekly.time,
            weekly_post_if_empty: config.weekly.post_if_empty,
            monthly_enabled: config.monthly.enabled,
            monthly_day: config.monthly.day,
            monthly_time: config.monthly.time,
            monthly_post_if_empty: config.monthly.post_if_empty,
        }
    }
}

// ── Bot state ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct BotState {
    bot: Bot,
    http: reqwest::Client,
    config: CalendarConfig,
    schedule: Settings<Schedule>,
}

impl BotState {
    async fn broadcast(&self, what: &str, plain: &str, html: &str) {
        for room in self.bot.broadcast_rooms() {
            if let Err(e) = room
                .send(RoomMessageEventContent::text_html(plain, html))
                .await
            {
                error!("Failed to send {what} to {}: {e}", room.room_id());
            }
        }
    }
}

/// Sleep until `target`, or until the schedule settings change (`false`).
async fn sleep_until_or_changed(secs: u64, changes: &mut watch::Receiver<u64>) -> bool {
    tokio::select! {
        _ = sleep(TokioDuration::from_secs(secs)) => true,
        _ = changes.changed() => false,
    }
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
    Ok(req.send().await?.error_for_status()?.text().await?)
}

/// Undo XML escaping of `calendar-data` inside a CalDAV response, so "&amp;"
/// in a title reads "&" (and "&#13;" line endings become real ones).
fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest.find(';').filter(|&end| end <= 10).and_then(|end| {
            let entity = &rest[1..end];
            let ch = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            };
            ch.map(|c| (c, end))
        });
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

async fn fetch_ics_caldav(
    http: &reqwest::Client,
    src: &CalendarSource,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<Vec<String>> {
    let url = src
        .caldav_url
        .as_deref()
        .context("No caldav_url configured")?;
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

    let xml = xml_unescape(&req.send().await?.error_for_status()?.text().await?);

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
async fn discover_calendars(http: &reqwest::Client, src: &CalendarSource) -> Result<Vec<String>> {
    let account_url = src
        .caldav_account_url
        .as_deref()
        .context("No caldav_account_url")?;

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

    let xml = req.send().await?.error_for_status()?.text().await?;

    // Extract href values from responses that contain <C:calendar/> resourcetype.
    // We split by <D:response> blocks and keep those that declare a calendar collection.
    let mut urls = Vec::new();
    let mut pos = 0;
    while let Some(rel) = xml[pos..]
        .find("<D:response>")
        .or_else(|| xml[pos..].find("<d:response>"))
    {
        let block_start = pos + rel;
        let close_tag = if xml[block_start..].starts_with("<D:") {
            "</D:response>"
        } else {
            "</d:response>"
        };
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
            .find("<D:href>")
            .or_else(|| block.find("<d:href>"))
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
) -> Result<Vec<ics::IcsEvent>> {
    if src.caldav_account_url.is_some() {
        let calendar_urls = discover_calendars(http, src).await?;
        info!(
            "Discovered {} calendar(s) from account",
            calendar_urls.len()
        );
        let mut all: Vec<ics::IcsEvent> = Vec::new();
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
                Ok(blocks) => blocks.iter().for_each(|b| all.extend(ics::parse_ics(b))),
                Err(e) => warn!("Failed to fetch calendar {url}: {e}"),
            }
        }
        Ok(all)
    } else if src.caldav_url.is_some() {
        let blocks = fetch_ics_caldav(http, src, start, end).await?;
        Ok(blocks.iter().flat_map(|b| ics::parse_ics(b)).collect())
    } else {
        let ics = fetch_ics_direct(http, src).await?;
        Ok(ics::parse_ics(&ics))
    }
}

/// Events of `days` days from `from`, plus how many sources failed.
struct Loaded {
    days: Vec<(NaiveDate, Vec<ics::DayEntry>)>,
    failed: usize,
    total: usize,
}

impl Loaded {
    fn all_failed(&self) -> bool {
        self.total > 0 && self.failed == self.total
    }

    fn event_count(&self) -> usize {
        self.days.iter().map(|(_, e)| e.len()).sum()
    }
}

async fn load(state: &BotState, from: NaiveDate, days: i64) -> Loaded {
    let until = from + Duration::days(days);
    let mut all = Vec::new();
    let mut failed = 0;
    for (i, src) in state.config.sources.iter().enumerate() {
        match fetch_source_ics_events(&state.http, src, from, until).await {
            Ok(events) => {
                info!("Source {}: {} raw event(s)", i + 1, events.len());
                all.extend(events);
            }
            Err(e) => {
                error!("Source {} failed: {e:#}", i + 1);
                failed += 1;
            }
        }
    }
    Loaded {
        days: ics::expand(&all, from, days),
        failed,
        total: state.config.sources.len(),
    }
}

// ── Scheduler ─────────────────────────────────────────────────────────────────

/// Post a scheduled overview, unless it would be empty and `post_if_empty`
/// is off. When no calendar could be reached, nothing is posted: "no events"
/// would be a claim we can't make.
async fn post_scheduled(
    state: &BotState,
    what: &str,
    loaded: &Loaded,
    post_if_empty: bool,
    message: impl FnOnce() -> render::Message,
) {
    if loaded.all_failed() {
        error!("Skipping {what}: no calendar source could be loaded");
        return;
    }
    let count = loaded.event_count();
    if count == 0 && !post_if_empty {
        info!("No events for the {what}, staying silent");
        return;
    }
    let (plain, html) = message();
    info!("Posting {what} ({count} event(s)) to Matrix");
    state.broadcast(what, &plain, &html).await;
}

async fn check_and_post(state: &BotState, post_if_empty: bool) {
    let today = Local::now().date_naive();
    let loaded = load(state, today, 1).await;
    post_scheduled(state, "daily summary", &loaded, post_if_empty, || {
        render::day_message(today, today, &loaded.days[0].1, loaded.failed, loaded.total)
    })
    .await;
}

/// The next time `time` occurs strictly after `now` (today or tomorrow).
fn next_daily_run(now: NaiveDateTime, time: NaiveTime) -> NaiveDateTime {
    let today = now.date();
    if now.time() < time {
        today.and_time(time)
    } else {
        (today + Duration::days(1)).and_time(time)
    }
}

/// The next `weekday` at `time` after `now`; a week later if that moment
/// has already passed today.
fn next_weekly_run(now: NaiveDateTime, weekday: Weekday, time: NaiveTime) -> NaiveDateTime {
    let today = now.date();
    let days_until = (weekday.num_days_from_monday() as i64
        - today.weekday().num_days_from_monday() as i64)
        .rem_euclid(7);
    let days_until = if days_until == 0 && now.time() >= time {
        7
    } else {
        days_until
    };
    (today + Duration::days(days_until)).and_time(time)
}

/// The next day-of-month `day` (1–31, clamped to short months) at `time`
/// after `now`: this month if still ahead, otherwise next month.
fn next_monthly_run(now: NaiveDateTime, day: u32, time: NaiveTime) -> NaiveDateTime {
    let today = now.date();
    let day = day.clamp(1, 31);
    let candidate = month_day(today.year(), today.month(), day);
    let date = if candidate > today || (candidate == today && now.time() < time) {
        candidate
    } else if today.month() == 12 {
        month_day(today.year() + 1, 1, day)
    } else {
        month_day(today.year(), today.month() + 1, day)
    };
    date.and_time(time)
}

fn naive_time(t: TimeOfDay) -> NaiveTime {
    NaiveTime::from_hms_opt(t.hour, t.minute, 0).expect("TimeOfDay is validated")
}

async fn daily_scheduler_loop(state: BotState, test_mode: bool) {
    if test_mode {
        let schedule = state.schedule.get();
        if schedule.daily_enabled {
            info!("Test mode: posting daily summary immediately");
            check_and_post(&state, schedule.daily_post_if_empty).await;
        }
        return;
    }

    let mut changes = state.schedule.store().subscribe();
    loop {
        let schedule = state.schedule.get();
        if !schedule.daily_enabled {
            info!("Daily summary disabled");
            if changes.changed().await.is_err() {
                return;
            }
            continue;
        }
        let now = Local::now().naive_local();
        let next_dt = next_daily_run(now, naive_time(schedule.daily_time));
        let secs = (next_dt - now).num_seconds().max(0) as u64;
        info!("Next daily summary in {secs}s (at {})", schedule.daily_time);
        if sleep_until_or_changed(secs, &mut changes).await {
            check_and_post(&state, schedule.daily_post_if_empty).await;
        }
    }
}

async fn check_and_post_weekly(state: &BotState, post_if_empty: bool) {
    let today = Local::now().date_naive();
    let loaded = load(state, today, 7).await;
    post_scheduled(state, "weekly summary", &loaded, post_if_empty, || {
        render::range_message(
            &render::week_title(today),
            &loaded.days,
            today,
            loaded.failed,
            loaded.total,
        )
    })
    .await;
}

async fn weekly_scheduler_loop(state: BotState, test_mode: bool) {
    if test_mode {
        let schedule = state.schedule.get();
        if schedule.weekly_enabled {
            info!("Test mode: posting weekly summary immediately");
            check_and_post_weekly(&state, schedule.weekly_post_if_empty).await;
        }
        return;
    }

    let mut changes = state.schedule.store().subscribe();
    loop {
        let schedule = state.schedule.get();
        if !schedule.weekly_enabled {
            info!("Weekly summary disabled");
            if changes.changed().await.is_err() {
                return;
            }
            continue;
        }
        let now = Local::now().naive_local();
        let target_dt = next_weekly_run(
            now,
            parse_weekday(&schedule.weekly_day),
            naive_time(schedule.weekly_time),
        );
        let secs = (target_dt - now).num_seconds().max(0) as u64;
        info!(
            "Next weekly summary in {secs}s (on {} at {})",
            target_dt.format("%A %d %b"),
            schedule.weekly_time
        );
        if sleep_until_or_changed(secs, &mut changes).await {
            check_and_post_weekly(&state, schedule.weekly_post_if_empty).await;
        }
    }
}

/// First day of `day`'s month, and how many days remain in it from `day`.
fn rest_of_month(day: NaiveDate) -> (NaiveDate, i64) {
    let first = NaiveDate::from_ymd_opt(day.year(), day.month(), 1).expect("valid month");
    let next = if day.month() == 12 {
        NaiveDate::from_ymd_opt(day.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(day.year(), day.month() + 1, 1)
    }
    .expect("valid month");
    (first, (next - day).num_days())
}

async fn check_and_post_monthly(state: &BotState, post_if_empty: bool) {
    // From today to the end of the month: days already past are no use in
    // an overview (and on the default day 1 this is the whole month).
    let today = Local::now().date_naive();
    let (first, days) = rest_of_month(today);
    let loaded = load(state, today, days).await;
    post_scheduled(state, "monthly summary", &loaded, post_if_empty, || {
        render::range_message(
            &render::month_title(first),
            &loaded.days,
            today,
            loaded.failed,
            loaded.total,
        )
    })
    .await;
}

// ── Commands ──────────────────────────────────────────────────────────────────

/// How far `!next` looks ahead.
const NEXT_LOOKAHEAD_DAYS: i64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Today,
    Week,
    Month,
    Next,
    Help,
}

fn parse_command(body: &str) -> Option<Command> {
    let word = body.split_whitespace().next()?.to_ascii_lowercase();
    Some(match word.as_str() {
        "!today" => Command::Today,
        "!week" => Command::Week,
        "!month" => Command::Month,
        "!next" => Command::Next,
        "!calendar" | "!cal" => Command::Help,
        _ => return None,
    })
}

async fn command_reply(state: &BotState, command: Command) -> render::Message {
    let now = Local::now().naive_local();
    let today = now.date();
    let (from, days) = match command {
        Command::Help => return render::help_message(),
        Command::Today => (today, 1),
        Command::Week => (today, 7),
        Command::Month => (today, rest_of_month(today).1),
        Command::Next => (today, NEXT_LOOKAHEAD_DAYS),
    };
    let loaded = load(state, from, days).await;
    if loaded.all_failed() {
        return render::unavailable_message();
    }
    let (failed, total) = (loaded.failed, loaded.total);
    match command {
        Command::Today => render::day_message(today, today, &loaded.days[0].1, failed, total),
        Command::Week => render::range_message(
            &render::week_title(today),
            &loaded.days,
            today,
            failed,
            total,
        ),
        Command::Month => render::range_message(
            &render::month_title(rest_of_month(today).0),
            &loaded.days,
            today,
            failed,
            total,
        ),
        Command::Next => render::next_message(&loaded.days, now, failed, total),
        Command::Help => unreachable!("answered above"),
    }
}

async fn monthly_scheduler_loop(state: BotState, test_mode: bool) {
    if test_mode {
        let schedule = state.schedule.get();
        if schedule.monthly_enabled {
            info!("Test mode: posting monthly summary immediately");
            check_and_post_monthly(&state, schedule.monthly_post_if_empty).await;
        }
        return;
    }

    let mut changes = state.schedule.store().subscribe();
    loop {
        let schedule = state.schedule.get();
        if !schedule.monthly_enabled {
            info!("Monthly summary disabled");
            if changes.changed().await.is_err() {
                return;
            }
            continue;
        }
        let now = Local::now().naive_local();
        let target_dt =
            next_monthly_run(now, schedule.monthly_day, naive_time(schedule.monthly_time));
        let secs = (target_dt - now).num_seconds().max(0) as u64;
        info!(
            "Next monthly summary in {secs}s (on {} at {})",
            target_dt.format("%d %b %Y"),
            schedule.monthly_time
        );
        if sleep_until_or_changed(secs, &mut changes).await {
            check_and_post_monthly(&state, schedule.monthly_post_if_empty).await;
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

const ADMIN_HELP: &str = "⚙️ Settings (!admin settings):\n\
daily_*, weekly_*, monthly_*\n\
with enabled, time, day, post_if_empty\n\
\n\
👥 For everyone:\n\
!today · !week · !month · !next · !calendar";

#[tokio::main]
async fn main() -> Result<()> {
    mxbot_common::logging::init("calendar_bot");

    let test_mode = std::env::args().any(|a| a == "--test");
    let config: Config =
        mxbot_common::config::load_toml(&mxbot_common::config::config_path_from_args())?;

    if config.calendar.sources.is_empty() {
        anyhow::bail!("Config [calendar]: add at least one [[calendar.sources]] entry");
    }

    let store_path = mxbot_common::config::store_path_from_env();
    let schedule = Settings::load(
        store_path.join("settings.json"),
        Schedule::from_config(&config.calendar),
    )
    .await?;

    let bot = Bot::builder("calendar-bot", env!("CARGO_PKG_VERSION"))
        .store_path(&store_path)
        .settings(schedule.store().clone())
        .admin_help(ADMIN_HELP)
        .start(&config.matrix, &config.security)
        .await?;

    let state = BotState {
        bot: bot.clone(),
        http: reqwest::Client::builder()
            .user_agent(concat!("calendar-bot/", env!("CARGO_PKG_VERSION")))
            // A hung calendar server must not stall a summary indefinitely.
            .timeout(std::time::Duration::from_secs(30))
            .build()?,
        config: config.calendar,
        schedule,
    };

    install_message_handler(&state);

    tokio::spawn(daily_scheduler_loop(state.clone(), test_mode));
    tokio::spawn(weekly_scheduler_loop(state.clone(), test_mode));
    tokio::spawn(monthly_scheduler_loop(state, test_mode));

    info!("Starting sync...");
    bot.initial_sync().await;
    bot.run().await
}

fn install_message_handler(state: &BotState) {
    let state = state.clone();
    state.bot.client.clone().add_event_handler(
        move |ev: OriginalSyncRoomMessageEvent, room: Room, encryption: Option<EncryptionInfo>| {
            let state = state.clone();
            async move {
                if room.state() != RoomState::Joined || ev.sender == state.bot.user_id {
                    return;
                }
                if state
                    .bot
                    .admin
                    .handle(&room, &ev, encryption.as_ref())
                    .await
                    == mxbot_common::admin::Dispatch::Handled
                {
                    return;
                }
                let MessageType::Text(text) = &ev.content.msgtype else {
                    return;
                };
                let Some(command) = parse_command(&text.body) else {
                    return;
                };
                let (plain, html) = command_reply(&state, command).await;
                if let Err(e) = room
                    .send(RoomMessageEventContent::text_html(plain, html))
                    .await
                {
                    error!("Failed to answer {command:?}: {e}");
                }
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        date(y, m, d).and_hms_opt(h, min, 0).unwrap()
    }

    fn hm(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn daily_run_is_today_until_the_time_passes() {
        assert_eq!(
            next_daily_run(at(2026, 9, 25, 6, 59), hm(7, 0)),
            at(2026, 9, 25, 7, 0)
        );
        assert_eq!(
            next_daily_run(at(2026, 9, 25, 7, 0), hm(7, 0)),
            at(2026, 9, 26, 7, 0)
        );
    }

    #[test]
    fn weekly_run_targets_the_next_matching_weekday() {
        // 2026-09-25 is a Friday.
        assert_eq!(
            next_weekly_run(at(2026, 9, 25, 12, 0), Weekday::Mon, hm(7, 0)),
            at(2026, 9, 28, 7, 0)
        );
        assert_eq!(
            next_weekly_run(at(2026, 9, 25, 6, 0), Weekday::Fri, hm(7, 0)),
            at(2026, 9, 25, 7, 0)
        );
        assert_eq!(
            next_weekly_run(at(2026, 9, 25, 8, 0), Weekday::Fri, hm(7, 0)),
            at(2026, 10, 2, 7, 0)
        );
    }

    #[test]
    fn monthly_run_clamps_to_short_months_and_rolls_over_the_year() {
        assert_eq!(
            next_monthly_run(at(2026, 4, 2, 8, 0), 31, hm(7, 0)),
            at(2026, 4, 30, 7, 0)
        );
        assert_eq!(
            next_monthly_run(at(2026, 2, 1, 6, 0), 1, hm(7, 0)),
            at(2026, 2, 1, 7, 0)
        );
        assert_eq!(
            next_monthly_run(at(2026, 12, 15, 8, 0), 1, hm(7, 0)),
            at(2027, 1, 1, 7, 0)
        );
        assert_eq!(
            next_monthly_run(at(2026, 1, 31, 8, 0), 31, hm(7, 0)),
            at(2026, 2, 28, 7, 0)
        );
    }

    #[test]
    fn weekday_names_default_to_monday() {
        assert_eq!(parse_weekday("Friday"), Weekday::Fri);
        assert_eq!(parse_weekday("sun"), Weekday::Sun);
        assert_eq!(parse_weekday("nonsense"), Weekday::Mon);
    }

    #[test]
    fn ics_events_are_parsed_filtered_and_sorted() {
        let ics = "BEGIN:VCALENDAR\r\n\
            BEGIN:VEVENT\r\nUID:1\r\nSUMMARY:Plenum\\, groß\r\nDTSTART:20260925T190000\r\nLOCATION:Kü\r\n che\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:2\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20260925\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:3\r\nSUMMARY:Standup\r\nDTSTART:20260901T090000\r\nRRULE:FREQ=DAILY\r\nEXDATE:20260925T090000\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nUID:4\r\nSUMMARY:Breakfast\r\nDTSTART:20260925T080000\r\nEND:VEVENT\r\n\
            END:VCALENDAR\r\n";
        let events = ics::parse_ics(ics);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].summary, "Plenum, groß");
        assert_eq!(events[0].location.as_deref(), Some("Küche"));

        let days = ics::expand(&events, date(2026, 9, 25), 1);
        let summaries: Vec<&str> = days[0].1.iter().map(|e| e.summary.as_str()).collect();
        // All-day first, then timed by time; the excluded recurrence is gone.
        assert_eq!(summaries, ["Holiday", "Breakfast", "Plenum, groß"]);
    }

    #[test]
    fn caldav_xml_escaping_is_undone() {
        assert_eq!(
            xml_unescape("SUMMARY:Tom &amp; Jerry &lt;3&#13;\nLOCATION:&#x4B;üche"),
            "SUMMARY:Tom & Jerry <3\r\nLOCATION:Küche"
        );
        assert_eq!(xml_unescape("a & b; c"), "a & b; c");
    }

    #[test]
    fn commands_are_recognised() {
        assert_eq!(parse_command("!today"), Some(Command::Today));
        assert_eq!(parse_command("  !WEEK please"), Some(Command::Week));
        assert_eq!(parse_command("!cal"), Some(Command::Help));
        assert_eq!(parse_command("!next"), Some(Command::Next));
        assert_eq!(parse_command("today"), None);
        assert_eq!(parse_command("!todayish"), None);
    }

    #[test]
    fn rest_of_month_counts_from_today() {
        assert_eq!(rest_of_month(date(2026, 9, 1)), (date(2026, 9, 1), 30));
        assert_eq!(rest_of_month(date(2026, 9, 25)), (date(2026, 9, 1), 6));
        assert_eq!(rest_of_month(date(2026, 12, 31)), (date(2026, 12, 1), 1));
    }

    #[test]
    fn runtime_schedule_defaults_come_from_the_config() {
        let config: CalendarConfig = toml::from_str(
            "sources = []\n[weekly]\nenabled = true\nday = \"friday\"\ntime = \"18:30\"",
        )
        .unwrap();
        let schedule = Schedule::from_config(&config);
        assert!(schedule.daily_enabled);
        assert_eq!(schedule.daily_time, TimeOfDay::new(7, 0));
        assert!(schedule.weekly_enabled);
        assert_eq!(schedule.weekly_day, "friday");
        assert_eq!(schedule.weekly_time, TimeOfDay::new(18, 30));
        assert!(!schedule.monthly_enabled);
    }
}
