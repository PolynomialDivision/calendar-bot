mod ics;
mod render;
mod update;

use anyhow::{Context, Result};
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Weekday};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use mxbot_common::{
    config::{MatrixConfig, SecurityConfig, TimeOfDay},
    matrix_sdk::{
        deserialized_responses::EncryptionInfo,
        ruma::events::room::message::{
            MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
        },
        Room, RoomState,
    },
    persist,
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
    /// Post the rest of the week again when events in it are added, moved
    /// or cancelled after the overview. Default: on.
    #[serde(default = "default_true")]
    updates: bool,
}

impl Default for WeeklySummaryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            day: default_weekly_day(),
            time: default_reminder_time(),
            post_if_empty: false,
            updates: true,
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
    weekly_updates: bool,
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
            weekly_updates: config.weekly.updates,
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
    posted: PostedStore,
    week: WeekStore,
}

// ── Posted markers ────────────────────────────────────────────────────────────

/// The scheduled run each summary last handled, as the date of that run,
/// persisted in `store/posted.json`. Without it a restart around the
/// scheduled minute would lose the post for good: the fresh process only
/// looks ahead to the next run.
#[derive(Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Posted {
    daily: Option<NaiveDate>,
    weekly: Option<NaiveDate>,
    monthly: Option<NaiveDate>,
}

impl Posted {
    fn get(&self, kind: Summary) -> Option<NaiveDate> {
        match kind {
            Summary::Daily => self.daily,
            Summary::Weekly => self.weekly,
            Summary::Monthly => self.monthly,
        }
    }

    fn set(&mut self, kind: Summary, date: NaiveDate) {
        let slot = match kind {
            Summary::Daily => &mut self.daily,
            Summary::Weekly => &mut self.weekly,
            Summary::Monthly => &mut self.monthly,
        };
        *slot = Some(date);
    }
}

#[derive(Clone)]
struct PostedStore {
    path: PathBuf,
    inner: Arc<Mutex<Posted>>,
}

impl PostedStore {
    async fn load(path: PathBuf) -> Result<Self> {
        let posted: Posted = persist::load_json_or_default(&path).await?;
        Ok(Self {
            path,
            inner: Arc::new(Mutex::new(posted)),
        })
    }

    fn get(&self, kind: Summary) -> Option<NaiveDate> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(kind)
    }

    /// Remember that the run of `kind` dated `date` was handled. A failed
    /// write only costs a possible repeat after the next restart, so it is
    /// logged rather than propagated.
    async fn mark(&self, kind: Summary, date: NaiveDate) {
        let snapshot = {
            let mut posted = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            posted.set(kind, date);
            *posted
        };
        if let Err(e) = persist::save_json_atomic(&self.path, &snapshot).await {
            warn!("Could not save {}: {e:#}", self.path.display());
        }
    }
}

/// The snapshot of the last weekly overview (`store/week.json`), what
/// [`update_loop`] compares with. A failed write is logged: the next check
/// would then report the same changes once more.
#[derive(Clone)]
struct WeekStore {
    path: PathBuf,
    inner: Arc<Mutex<Option<update::WeekSnapshot>>>,
}

impl WeekStore {
    async fn load(path: PathBuf) -> Result<Self> {
        let week: Option<update::WeekSnapshot> = persist::load_json_or_default(&path).await?;
        Ok(Self {
            path,
            inner: Arc::new(Mutex::new(week)),
        })
    }

    fn get(&self) -> Option<update::WeekSnapshot> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    async fn set(&self, week: update::WeekSnapshot) {
        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = Some(week.clone());
        if let Err(e) = persist::save_json_atomic(&self.path, &Some(week)).await {
            warn!("Could not save {}: {e:#}", self.path.display());
        }
    }
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

/// Sleep for `secs`, or until the schedule settings change.
async fn sleep_until_or_changed(secs: u64, changes: &mut watch::Receiver<u64>) {
    tokio::select! {
        _ = sleep(TokioDuration::from_secs(secs)) => {}
        _ = changes.changed() => {}
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
/// would be a claim we can't make. Returns whether the run counts as
/// handled, which it does not in that last case, so it can be retried.
async fn post_scheduled(
    state: &BotState,
    what: &str,
    loaded: &Loaded,
    post_if_empty: bool,
    message: impl FnOnce() -> render::Message,
) -> bool {
    if loaded.all_failed() {
        error!("Skipping {what}: no calendar source could be loaded");
        return false;
    }
    let count = loaded.event_count();
    if count == 0 && !post_if_empty {
        info!("No events for the {what}, staying silent");
        return true;
    }
    let (plain, html) = message();
    info!("Posting {what} ({count} event(s)) to Matrix");
    state.broadcast(what, &plain, &html).await;
    true
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

/// The most recent day-of-month `day` at `time` that is not after `now`.
fn last_monthly_run(now: NaiveDateTime, day: u32, time: NaiveTime) -> NaiveDateTime {
    let today = now.date();
    let day = day.clamp(1, 31);
    let candidate = month_day(today.year(), today.month(), day).and_time(time);
    if candidate <= now {
        candidate
    } else if today.month() == 1 {
        month_day(today.year() - 1, 12, day).and_time(time)
    } else {
        month_day(today.year(), today.month() - 1, day).and_time(time)
    }
}

fn naive_time(t: TimeOfDay) -> NaiveTime {
    NaiveTime::from_hms_opt(t.hour, t.minute, 0).expect("TimeOfDay is validated")
}

/// Whole seconds from `now` until `target`, rounded up so a timer never
/// fires just short of the moment it waits for.
fn secs_until(now: NaiveDateTime, target: NaiveDateTime) -> u64 {
    let millis = (target - now).num_milliseconds().max(0);
    ((millis + 999) / 1000) as u64
}

/// How long to wait before retrying a run whose calendars were all
/// unreachable.
const RETRY_AFTER: Duration = Duration::minutes(15);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Summary {
    Daily,
    Weekly,
    Monthly,
}

impl Summary {
    fn name(self) -> &'static str {
        match self {
            Self::Daily => "daily summary",
            Self::Weekly => "weekly summary",
            Self::Monthly => "monthly summary",
        }
    }

    fn enabled(self, schedule: &Schedule) -> bool {
        match self {
            Self::Daily => schedule.daily_enabled,
            Self::Weekly => schedule.weekly_enabled,
            Self::Monthly => schedule.monthly_enabled,
        }
    }

    fn post_if_empty(self, schedule: &Schedule) -> bool {
        match self {
            Self::Daily => schedule.daily_post_if_empty,
            Self::Weekly => schedule.weekly_post_if_empty,
            Self::Monthly => schedule.monthly_post_if_empty,
        }
    }

    fn time(self, schedule: &Schedule) -> NaiveTime {
        naive_time(match self {
            Self::Daily => schedule.daily_time,
            Self::Weekly => schedule.weekly_time,
            Self::Monthly => schedule.monthly_time,
        })
    }

    /// The first scheduled run strictly after `now`.
    fn next_run(self, now: NaiveDateTime, schedule: &Schedule) -> NaiveDateTime {
        let time = self.time(schedule);
        match self {
            Self::Daily => next_daily_run(now, time),
            Self::Weekly => next_weekly_run(now, parse_weekday(&schedule.weekly_day), time),
            Self::Monthly => next_monthly_run(now, schedule.monthly_day, time),
        }
    }

    /// The most recent scheduled run at or before `now`.
    fn last_run(self, now: NaiveDateTime, schedule: &Schedule) -> NaiveDateTime {
        let time = self.time(schedule);
        match self {
            Self::Daily => next_daily_run(now, time) - Duration::days(1),
            Self::Weekly => {
                next_weekly_run(now, parse_weekday(&schedule.weekly_day), time) - Duration::days(7)
            }
            Self::Monthly => last_monthly_run(now, schedule.monthly_day, time),
        }
    }

    /// How long after its scheduled moment a missed run is still worth
    /// posting: half the period. Later than that the overview is mostly
    /// about days already gone, and the next regular run is closer anyway.
    fn catch_up_window(self) -> Duration {
        match self {
            Self::Daily => Duration::hours(12),
            Self::Weekly => Duration::hours(84),
            Self::Monthly => Duration::days(15),
        }
    }

    fn describe_run(self, run: NaiveDateTime, schedule: &Schedule) -> String {
        let time = match self {
            Self::Daily => schedule.daily_time,
            Self::Weekly => schedule.weekly_time,
            Self::Monthly => schedule.monthly_time,
        };
        match self {
            Self::Daily => format!("at {time}"),
            Self::Weekly => format!("on {} at {time}", run.format("%A %d %b")),
            Self::Monthly => format!("on {} at {time}", run.format("%d %b %Y")),
        }
    }
}

/// The scheduled run that is due now: the most recent one, unless it was
/// already handled (`posted` holds its date) or lies too far back to be
/// worth catching up on.
fn due_run(
    kind: Summary,
    now: NaiveDateTime,
    schedule: &Schedule,
    posted: Option<NaiveDate>,
) -> Option<NaiveDateTime> {
    let last = kind.last_run(now, schedule);
    if posted == Some(last.date()) || now - last > kind.catch_up_window() {
        return None;
    }
    Some(last)
}

/// Load and post the overview of `kind` as of today; `true` when the run
/// was handled (posted, or silent on purpose).
async fn check_and_post(state: &BotState, kind: Summary, post_if_empty: bool) -> bool {
    let today = Local::now().date_naive();
    let what = kind.name();
    match kind {
        Summary::Daily => {
            let loaded = load(state, today, 1).await;
            post_scheduled(state, what, &loaded, post_if_empty, || {
                render::day_message(today, today, &loaded.days[0].1, loaded.failed, loaded.total)
            })
            .await
        }
        Summary::Weekly => {
            let loaded = load(state, today, 7).await;
            let handled = post_scheduled(state, what, &loaded, post_if_empty, || {
                render::range_message(
                    &render::week_title(today),
                    &loaded.days,
                    today,
                    loaded.failed,
                    loaded.total,
                )
            })
            .await;
            // Also after a silent (empty) week: an event added later is news.
            if handled {
                let week = update::WeekSnapshot::of(
                    &loaded.days,
                    today + Duration::days(6),
                    Local::now().naive_local(),
                    loaded.failed == 0,
                );
                state.week.set(week).await;
            }
            handled
        }
        Summary::Monthly => {
            // From today to the end of the month: days already past are no
            // use in an overview (and on the default day 1 this is the whole
            // month).
            let (first, days) = rest_of_month(today);
            let loaded = load(state, today, days).await;
            post_scheduled(state, what, &loaded, post_if_empty, || {
                render::range_message(
                    &render::month_title(first),
                    &loaded.days,
                    today,
                    loaded.failed,
                    loaded.total,
                )
            })
            .await
        }
    }
}

/// Post `kind` whenever a run is due: at the scheduled moment, right after
/// a (re)start that fell into the catch-up window of a run nobody handled,
/// and again after [`RETRY_AFTER`] when the calendars were unreachable.
async fn scheduler_loop(state: BotState, kind: Summary, test_mode: bool) {
    let name = kind.name();
    if test_mode {
        let schedule = state.schedule.get();
        if kind.enabled(&schedule) {
            info!("Test mode: posting {name} immediately");
            check_and_post(&state, kind, kind.post_if_empty(&schedule)).await;
        }
        return;
    }

    let mut changes = state.schedule.store().subscribe();
    loop {
        let schedule = state.schedule.get();
        if !kind.enabled(&schedule) {
            info!("{} disabled", capitalize(name));
            if changes.changed().await.is_err() {
                return;
            }
            continue;
        }
        let now = Local::now().naive_local();
        let mut wake_at = kind.next_run(now, &schedule);
        let mut retrying = false;
        if let Some(due) = due_run(kind, now, &schedule, state.posted.get(kind)) {
            let late = now - due;
            if late > Duration::minutes(1) {
                info!(
                    "Catching up the {name} scheduled {} ({}h {}m ago)",
                    kind.describe_run(due, &schedule),
                    late.num_hours(),
                    late.num_minutes() % 60
                );
            }
            if check_and_post(&state, kind, kind.post_if_empty(&schedule)).await {
                state.posted.mark(kind, due.date()).await;
                continue;
            }
            let retry_at = Local::now().naive_local() + RETRY_AFTER;
            if retry_at < wake_at {
                warn!("Retrying the {name} in {} min", RETRY_AFTER.num_minutes());
                wake_at = retry_at;
                retrying = true;
            }
        }
        let secs = secs_until(Local::now().naive_local(), wake_at);
        if !retrying {
            info!(
                "Next {name} in {secs}s ({})",
                kind.describe_run(wake_at, &schedule)
            );
        }
        sleep_until_or_changed(secs, &mut changes).await;
    }
}

/// How often [`update_loop`] looks for changes in the rest of the week.
const UPDATE_CHECK: Duration = Duration::minutes(30);
/// The least time between two posts of the week (overview or update).
const UPDATE_GAP: Duration = Duration::minutes(60);
/// No updates from 22:00 to 07:00; changes of the night come in the morning.
const QUIET_FROM: u32 = 22;
const QUIET_UNTIL: u32 = 7;

/// Why the update check does nothing now, or `None` when it should load.
fn update_wait(
    now: NaiveDateTime,
    schedule: &Schedule,
    week: Option<&update::WeekSnapshot>,
) -> Option<&'static str> {
    let hour = now.hour();
    let Some(week) = week else {
        return Some("no weekly overview yet");
    };
    if !schedule.weekly_enabled || !schedule.weekly_updates {
        Some("off")
    } else if now.date() > week.until {
        Some("the week of the last overview is over")
    } else if !(QUIET_UNTIL..QUIET_FROM).contains(&hour) {
        Some("quiet hours")
    } else if week.complete && now - week.posted_at < UPDATE_GAP {
        Some("posted less than an hour ago")
    } else {
        None
    }
}

/// One check: load the rest of the week (and the weeks after it, to tell
/// moved from cancelled), compare it with the last overview and post the
/// rest of the week again when something changed.
async fn check_week_update(state: &BotState) {
    let now = Local::now().naive_local();
    let Some(before) = state.week.get() else {
        return;
    };
    if let Some(why) = update_wait(now, &state.schedule.get(), Some(&before)) {
        info!("Week update: not now ({why})");
        return;
    }
    let today = now.date();
    let loaded = load(state, today, NEXT_LOOKAHEAD_DAYS).await;
    // A missing calendar would look like cancelled events.
    if loaded.failed > 0 {
        warn!(
            "Week update skipped: {} of {} calendar(s) failed",
            loaded.failed, loaded.total
        );
        return;
    }
    let in_week = ((before.until - today).num_days() + 1) as usize;
    let (week, later) = loaded.days.split_at(in_week.min(loaded.days.len()));
    if !before.complete {
        info!("Week update: the last overview missed a calendar, taking this load as its state");
        state
            .week
            .set(update::WeekSnapshot::of(
                week,
                before.until,
                before.posted_at,
                true,
            ))
            .await;
        return;
    }
    let found = update::changes(&before, week, later, now);
    if found.is_empty() {
        info!("Week update: no changes");
        return;
    }
    info!(
        "Posting week update ({} changed, {} gone)",
        found.marks.len(),
        found.gone.len()
    );
    let (plain, html) = render::update_message(
        week,
        today,
        &found.marks,
        &found.gone,
        loaded.failed,
        loaded.total,
    );
    state.broadcast("week update", &plain, &html).await;
    state
        .week
        .set(update::WeekSnapshot::of(week, before.until, now, true))
        .await;
}

/// Check the rest of the week every [`UPDATE_CHECK`].
async fn update_loop(state: BotState) {
    loop {
        check_week_update(&state).await;
        sleep(TokioDuration::from_secs(UPDATE_CHECK.num_seconds() as u64)).await;
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
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

// ── Main ──────────────────────────────────────────────────────────────────────

const ADMIN_HELP: &str = "⚙️ Settings (!admin settings):\n\
daily_*, weekly_*, monthly_*\n\
with enabled, time, day, post_if_empty\n\
weekly_updates: post the rest of the week again after changes\n\
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
        posted: PostedStore::load(store_path.join("posted.json")).await?,
        week: WeekStore::load(store_path.join("week.json")).await?,
    };

    install_message_handler(&state);

    for kind in [Summary::Daily, Summary::Weekly, Summary::Monthly] {
        tokio::spawn(scheduler_loop(state.clone(), kind, test_mode));
    }
    if !test_mode {
        tokio::spawn(update_loop(state.clone()));
    }

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

    fn schedule(day: &str, time: TimeOfDay) -> Schedule {
        Schedule {
            daily_enabled: true,
            daily_time: time,
            daily_post_if_empty: false,
            weekly_enabled: true,
            weekly_day: day.to_owned(),
            weekly_time: time,
            weekly_post_if_empty: false,
            weekly_updates: true,
            monthly_enabled: true,
            monthly_day: 1,
            monthly_time: time,
            monthly_post_if_empty: false,
        }
    }

    #[test]
    fn last_run_is_the_most_recent_scheduled_moment() {
        let s = schedule("monday", TimeOfDay::new(7, 0));
        // Monday 12 Oct 2026, before and after 07:00.
        let before = at(2026, 10, 12, 6, 59);
        let after = at(2026, 10, 12, 11, 55);
        assert_eq!(Summary::Daily.last_run(before, &s), at(2026, 10, 11, 7, 0));
        assert_eq!(Summary::Daily.last_run(after, &s), at(2026, 10, 12, 7, 0));
        assert_eq!(Summary::Weekly.last_run(before, &s), at(2026, 10, 5, 7, 0));
        assert_eq!(Summary::Weekly.last_run(after, &s), at(2026, 10, 12, 7, 0));
        assert_eq!(Summary::Monthly.last_run(after, &s), at(2026, 10, 1, 7, 0));
        // Exactly on time counts as this run, not the previous one.
        assert_eq!(
            Summary::Daily.last_run(at(2026, 10, 12, 7, 0), &s),
            at(2026, 10, 12, 7, 0)
        );
    }

    #[test]
    fn last_monthly_run_crosses_months_and_years() {
        assert_eq!(
            last_monthly_run(at(2026, 1, 1, 6, 0), 1, hm(7, 0)),
            at(2025, 12, 1, 7, 0)
        );
        assert_eq!(
            last_monthly_run(at(2026, 3, 10, 6, 0), 31, hm(7, 0)),
            at(2026, 2, 28, 7, 0)
        );
        assert_eq!(
            last_monthly_run(at(2026, 3, 31, 7, 0), 31, hm(7, 0)),
            at(2026, 3, 31, 7, 0)
        );
    }

    #[test]
    fn due_run_catches_up_a_missed_post_while_it_is_still_useful() {
        let s = schedule("monday", TimeOfDay::new(7, 0));
        let monday_run = at(2026, 10, 12, 7, 0);
        // Restarted at 11:55 on Monday, nothing posted yet: both are due.
        let restart = at(2026, 10, 12, 11, 55);
        assert_eq!(due_run(Summary::Daily, restart, &s, None), Some(monday_run));
        assert_eq!(
            due_run(Summary::Weekly, restart, &s, None),
            Some(monday_run)
        );
        // Already handled today: nothing to do.
        let today = date(2026, 10, 12);
        assert_eq!(due_run(Summary::Daily, restart, &s, Some(today)), None);
        assert_eq!(due_run(Summary::Weekly, restart, &s, Some(today)), None);
        // Last week's marker does not cover this week.
        let last_week = date(2026, 10, 5);
        assert_eq!(
            due_run(Summary::Weekly, restart, &s, Some(last_week)),
            Some(monday_run)
        );
        // Too late: a day overview at 19:30 is history, the week one on
        // Thursday evening too, but Thursday noon still makes it.
        assert_eq!(
            due_run(Summary::Daily, at(2026, 10, 12, 19, 30), &s, None),
            None
        );
        assert_eq!(
            due_run(Summary::Weekly, at(2026, 10, 15, 19, 30), &s, None),
            None
        );
        assert_eq!(
            due_run(Summary::Weekly, at(2026, 10, 15, 12, 0), &s, None),
            Some(monday_run)
        );
        // The regular moment itself is due as well.
        assert_eq!(
            due_run(Summary::Daily, monday_run, &s, None),
            Some(monday_run)
        );
    }

    #[test]
    fn next_run_is_always_after_last_run_by_one_period() {
        let s = schedule("friday", TimeOfDay::new(18, 30));
        let now = at(2026, 10, 12, 11, 55);
        assert_eq!(
            Summary::Weekly.next_run(now, &s) - Summary::Weekly.last_run(now, &s),
            Duration::days(7)
        );
        assert_eq!(
            Summary::Daily.next_run(now, &s) - Summary::Daily.last_run(now, &s),
            Duration::days(1)
        );
        assert_eq!(Summary::Monthly.next_run(now, &s), at(2026, 11, 1, 18, 30));
        assert_eq!(Summary::Monthly.last_run(now, &s), at(2026, 10, 1, 18, 30));
    }

    #[test]
    fn seconds_until_round_up_and_never_go_negative() {
        let now = at(2026, 10, 12, 7, 0);
        assert_eq!(secs_until(now, now), 0);
        assert_eq!(secs_until(now, now - Duration::seconds(5)), 0);
        assert_eq!(secs_until(now, now + Duration::milliseconds(1500)), 2);
        assert_eq!(secs_until(now, now + Duration::seconds(60)), 60);
    }

    #[tokio::test]
    async fn posted_markers_survive_a_reload() {
        let dir = std::env::temp_dir().join(format!(
            "calendar-bot-posted-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("posted.json");

        let store = PostedStore::load(path.clone()).await.unwrap();
        assert_eq!(store.get(Summary::Daily), None);
        store.mark(Summary::Daily, date(2026, 10, 12)).await;
        store.mark(Summary::Weekly, date(2026, 10, 12)).await;
        assert_eq!(store.get(Summary::Weekly), Some(date(2026, 10, 12)));
        assert_eq!(store.get(Summary::Monthly), None);

        let reloaded = PostedStore::load(path).await.unwrap();
        assert_eq!(reloaded.get(Summary::Daily), Some(date(2026, 10, 12)));
        assert_eq!(reloaded.get(Summary::Weekly), Some(date(2026, 10, 12)));
        assert_eq!(reloaded.get(Summary::Monthly), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn the_week_snapshot_survives_a_reload() {
        let dir = std::env::temp_dir().join(format!(
            "calendar-bot-week-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("week.json");
        let store = WeekStore::load(path.clone()).await.unwrap();
        assert_eq!(store.get(), None);

        let week = update::WeekSnapshot::of(&[], date(2026, 10, 25), at(2026, 10, 19, 7, 0), false);
        store.set(week.clone()).await;
        assert_eq!(WeekStore::load(path).await.unwrap().get(), Some(week));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn week_updates_wait_for_the_day_the_gap_and_the_switch() {
        let mut schedule = schedule("monday", TimeOfDay::new(7, 0));
        schedule.weekly_enabled = true;
        schedule.weekly_updates = true;
        let week = update::WeekSnapshot::of(&[], date(2026, 10, 25), at(2026, 10, 19, 7, 0), true);

        assert_eq!(
            update_wait(at(2026, 10, 19, 7, 30), &schedule, Some(&week)),
            Some("posted less than an hour ago")
        );
        assert_eq!(
            update_wait(at(2026, 10, 19, 8, 0), &schedule, Some(&week)),
            None
        );
        assert_eq!(
            update_wait(at(2026, 10, 21, 21, 59), &schedule, Some(&week)),
            None
        );
        assert_eq!(
            update_wait(at(2026, 10, 21, 22, 0), &schedule, Some(&week)),
            Some("quiet hours")
        );
        assert_eq!(
            update_wait(at(2026, 10, 22, 6, 59), &schedule, Some(&week)),
            Some("quiet hours")
        );
        assert_eq!(
            update_wait(at(2026, 10, 25, 12, 0), &schedule, Some(&week)),
            None
        );
        assert_eq!(
            update_wait(at(2026, 10, 26, 12, 0), &schedule, Some(&week)),
            Some("the week of the last overview is over")
        );
        assert_eq!(
            update_wait(at(2026, 10, 21, 12, 0), &schedule, None),
            Some("no weekly overview yet")
        );

        // A snapshot that missed a calendar is completed at once.
        let partial = update::WeekSnapshot {
            complete: false,
            ..week.clone()
        };
        assert_eq!(
            update_wait(at(2026, 10, 19, 7, 30), &schedule, Some(&partial)),
            None
        );

        schedule.weekly_updates = false;
        assert_eq!(
            update_wait(at(2026, 10, 21, 12, 0), &schedule, Some(&week)),
            Some("off")
        );
    }

    #[test]
    fn disabled_log_line_keeps_its_old_wording() {
        assert_eq!(capitalize(Summary::Monthly.name()), "Monthly summary");
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
        assert!(schedule.weekly_updates);
        assert!(!schedule.monthly_enabled);
    }
}
