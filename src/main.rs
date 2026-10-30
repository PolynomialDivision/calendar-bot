use mxbot_common::calendar::{caldav, ics};
mod render;
mod update;

use anyhow::{Context, Result};
#[cfg(test)]
use chrono::NaiveTime;
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Utc, Weekday};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use mxbot_common::{
    clock::Zone,
    config::{MatrixConfig, SecurityConfig, TimeOfDay},
    matrix_sdk::{
        deserialized_responses::EncryptionInfo,
        ruma::events::room::message::{
            MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
        },
        Room, RoomState,
    },
    persist,
    schedule::{self, Delivery, Job, JsonLedger, Outcome, Rule, Slot},
    settings::Settings,
    supervise::{supervise_ticking, Alive},
    Bot,
};
use serde::{Deserialize, Serialize};
use tokio::{time::sleep, time::Duration as TokioDuration};
use tracing::{error, info, warn};

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Config {
    calendar: CalendarConfig,
    matrix: MatrixConfig,
    #[serde(default)]
    security: SecurityConfig,
}

/// One `[[calendar.sources]]` entry (mxbot-common's, the fields as before).
type CalendarSource = caldav::Source;

fn default_true() -> bool {
    true
}
fn default_weekly_day() -> String {
    "monday".to_owned()
}
fn default_monthly_day() -> u32 {
    1
}

/// An English weekday, full or short, any case; None for anything else.
fn weekday(s: &str) -> Option<Weekday> {
    Some(match s.trim().to_lowercase().as_str() {
        "monday" | "mon" => Weekday::Mon,
        "tuesday" | "tue" => Weekday::Tue,
        "wednesday" | "wed" => Weekday::Wed,
        "thursday" | "thu" => Weekday::Thu,
        "friday" | "fri" => Weekday::Fri,
        "saturday" | "sat" => Weekday::Sat,
        "sunday" | "sun" => Weekday::Sun,
        _ => return None,
    })
}

/// The weekly overview's day. The config's is checked at the start; one set later that is no
/// weekday counts as Monday (logged).
fn parse_weekday(s: &str) -> Weekday {
    weekday(s).unwrap_or_else(|| {
        warn!(day = %s, "Not a weekday: the weekly overview goes out on Monday");
        Weekday::Mon
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
    /// `TZ` (as compose sets it), else Europe/Berlin: the summaries' times and days.
    zone: Zone,
    posted: PostedStore,
    week: WeekStore,
}

// ── Posted markers ────────────────────────────────────────────────────────────

/// The scheduled run each summary last handled, as the date of that run,
/// persisted in `store/posted.json` (`{"daily": "2026-10-12", …}`): the
/// schedule's ledger. Without it a restart around the scheduled minute would
/// lose the post for good: the fresh process only looks ahead to the next
/// run. A date counts for its whole day, week or month, so moving the time
/// after a post does not post that period again. The file keeps the shape
/// older versions read; one that does not parse is moved aside.
#[derive(Clone)]
struct PostedStore {
    ledger: JsonLedger,
}

impl PostedStore {
    async fn load(path: PathBuf) -> Result<Self> {
        Ok(Self {
            ledger: JsonLedger::open_dated(path).await?,
        })
    }

    fn get(&self, kind: Summary) -> Option<NaiveDate> {
        self.ledger.recorded(kind.entry())?.parse().ok()
    }

    /// Remember that the run of `kind` dated `date` was handled (the
    /// schedule's runner does this itself).
    #[cfg(test)]
    async fn mark(&self, kind: Summary, date: NaiveDate) {
        if let Err(e) = self.ledger.record(kind.entry(), date.to_string()).await {
            warn!("Could not save posted.json: {e:#}");
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
    /// One save at a time, in order: the weekly overview and the update
    /// check may both set the snapshot.
    writer: Arc<tokio::sync::Mutex<()>>,
}

impl WeekStore {
    async fn load(path: PathBuf) -> Result<Self> {
        let week: Option<update::WeekSnapshot> = persist::load_json_or_default(&path).await?;
        Ok(Self {
            path,
            inner: Arc::new(Mutex::new(week)),
            writer: Arc::default(),
        })
    }

    fn get(&self) -> Option<update::WeekSnapshot> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    async fn set(&self, week: update::WeekSnapshot) {
        let _writing = self.writer.lock().await;
        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = Some(week.clone());
        if let Err(e) = persist::save_json_atomic(&self.path, &Some(week)).await {
            warn!("Could not save {}: {e:#}", self.path.display());
        }
    }
}

/// How long one post keeps retrying a homeserver that does not answer
/// (a restart, an outage of minutes) before the run is tried again later.
const SEND_FOR: TokioDuration = TokioDuration::from_secs(10 * 60);

/// One post: the day it is about, the key of its transaction IDs (it names
/// the run: summary and slot, so a run tried again after a crash or a failed
/// send gets no second copy where it arrived), and until when to retry.
struct Run {
    today: NaiveDate,
    key: String,
    deadline: Instant,
}

impl BotState {
    /// Post into the broadcast rooms with transaction IDs from `key`,
    /// retrying transient failures until `deadline`; failures are logged.
    async fn post(
        &self,
        what: &str,
        plain: &str,
        html: &str,
        key: &str,
        deadline: Instant,
    ) -> schedule::Delivered {
        let delivered = schedule::post(
            self.bot.broadcast_rooms(),
            RoomMessageEventContent::text_html(plain, html),
            key,
            deadline,
        )
        .await;
        for (room, e) in &delivered.failed {
            error!("Failed to send {what} to {room}: {e}");
        }
        delivered
    }
}

// ── Calendar fetching ─────────────────────────────────────────────────────────

// The fetching itself is mxbot-common's (calendar::caldav, moved there 2026-10-22).

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
        match caldav::fetch_source_ics_events(&state.http, src, from, until).await {
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
/// would be a claim we can't make. The run then counts as not handled, as it
/// does when the post reached no room (or failed in one for a reason that
/// may pass), so it is tried again after [`RETRY_AFTER`].
async fn post_scheduled(
    state: &BotState,
    what: &str,
    loaded: &Loaded,
    post_if_empty: bool,
    run: &Run,
    message: impl FnOnce() -> render::Message,
) -> Outcome {
    if loaded.all_failed() {
        error!("Skipping {what}: no calendar source could be loaded");
        return Outcome::RetryIn(RETRY_AFTER);
    }
    let count = loaded.event_count();
    if count == 0 && !post_if_empty {
        info!("No events for the {what}, staying silent");
        return Outcome::Skipped;
    }
    let (plain, html) = message();
    info!("Posting {what} ({count} event(s)) to Matrix");
    state
        .post(what, &plain, &html, &run.key, run.deadline)
        .await
        .outcome(RETRY_AFTER)
}

// The next and last runs are the shared schedule's (mxbot_common::schedule::Rule);
// these keep the tests that pin them.

/// The next time `time` occurs strictly after `now` (today or tomorrow).
#[cfg(test)]
fn next_daily_run(now: NaiveDateTime, time: NaiveTime) -> NaiveDateTime {
    next_local(&Rule::daily(time_of_day(time)), now)
}

/// The next `weekday` at `time` after `now`; a week later if that moment
/// has already passed today.
#[cfg(test)]
fn next_weekly_run(now: NaiveDateTime, weekday: Weekday, time: NaiveTime) -> NaiveDateTime {
    let rule = Rule::Weekly {
        day: weekday,
        at: time_of_day(time),
    };
    next_local(&rule, now)
}

/// The next day-of-month `day` (1–31, clamped to short months) at `time`
/// after `now`: this month if still ahead, otherwise next month.
#[cfg(test)]
fn next_monthly_run(now: NaiveDateTime, day: u32, time: NaiveTime) -> NaiveDateTime {
    let rule = Rule::Monthly {
        day,
        at: time_of_day(time),
    };
    next_local(&rule, now)
}

/// The most recent day-of-month `day` at `time` that is not after `now`.
#[cfg(test)]
fn last_monthly_run(now: NaiveDateTime, day: u32, time: NaiveTime) -> NaiveDateTime {
    let rule = Rule::Monthly {
        day,
        at: time_of_day(time),
    };
    rule.latest_local(now)
        .expect("a monthly rule always has a run")
}

#[cfg(test)]
fn time_of_day(t: NaiveTime) -> TimeOfDay {
    TimeOfDay::new(t.hour(), t.minute())
}

#[cfg(test)]
fn next_local(rule: &Rule, now: NaiveDateTime) -> NaiveDateTime {
    rule.next_local(now).expect("every summary rule has a time")
}

/// Whole seconds from `now` until `target`, rounded up so a timer never
/// fires just short of the moment it waits for.
#[cfg(test)]
fn secs_until(now: NaiveDateTime, target: NaiveDateTime) -> u64 {
    let millis = (target - now).num_milliseconds().max(0);
    ((millis + 999) / 1000) as u64
}

/// How long to wait before retrying a run whose calendars were all
/// unreachable, or whose post did not get through.
const RETRY_AFTER: TokioDuration = TokioDuration::from_secs(15 * 60);

/// The longest a summary's run takes: its calendar loads (30 s timeouts)
/// and a post retried for [`SEND_FOR`].
const LONGEST_RUN: TokioDuration = TokioDuration::from_secs(20 * 60);

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

    /// Its key in `store/posted.json`.
    fn entry(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
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

    /// When the summary is due, in local time.
    fn rule(self, schedule: &Schedule) -> Rule {
        match self {
            Self::Daily => Rule::daily(schedule.daily_time),
            Self::Weekly => Rule::Weekly {
                day: parse_weekday(&schedule.weekly_day),
                at: schedule.weekly_time,
            },
            Self::Monthly => Rule::Monthly {
                day: schedule.monthly_day,
                at: schedule.monthly_time,
            },
        }
    }

    /// The first scheduled run strictly after `now`.
    #[cfg(test)]
    fn next_run(self, now: NaiveDateTime, schedule: &Schedule) -> NaiveDateTime {
        next_local(&self.rule(schedule), now)
    }

    /// The most recent scheduled run at or before `now`.
    fn last_run(self, now: NaiveDateTime, schedule: &Schedule) -> NaiveDateTime {
        self.rule(schedule)
            .latest_local(now)
            .expect("every summary rule has a time")
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

    /// The summary as a scheduled job: due at its time in `zone`, caught up
    /// within [`Summary::catch_up_window`], recorded in `store/posted.json`
    /// once handled. Settings changes apply at once.
    fn job(self, settings: &Settings<Schedule>, zone: Zone) -> Job {
        let schedule = settings.clone();
        let catch_up = self.catch_up_window().to_std().expect("a positive window");
        Job::new(
            self.name(),
            zone,
            Delivery::AtLeastOnce,
            catch_up,
            move || {
                let schedule = schedule.get();
                self.enabled(&schedule).then(|| self.rule(&schedule))
            },
        )
        .entry(self.entry())
        .wake_on(settings.store().subscribe())
    }

    /// The post of `slot`: today's overview, its sends retried for
    /// [`SEND_FOR`] but not past the catch-up window.
    fn run(self, state: &BotState, slot: &Slot) -> Run {
        let window_end = slot.at + self.catch_up_window();
        let left = (window_end - Utc::now()).to_std().unwrap_or_default();
        Run {
            today: state.zone.now().date_naive(),
            // The slot's date, not its period: a weekly day moved after the post runs
            // again in the same week (see `JsonLedger::open_dated`), and with the week's
            // key the homeserver would answer with the first post.
            key: format!("calendar-bot:{}:{}", self.entry(), slot.local.date()),
            deadline: Instant::now() + SEND_FOR.min(left),
        }
    }
}

/// The scheduled run that is due now: the most recent one, unless it was
/// already handled (`posted` holds its date) or lies too far back to be
/// worth catching up on.
#[cfg(test)]
fn due_run(
    kind: Summary,
    now: NaiveDateTime,
    schedule: &Schedule,
    posted: Option<NaiveDate>,
) -> Option<NaiveDateTime> {
    // In UTC the wall clock is the instant: the rule's own catch-up check.
    let utc = Zone(mxbot_common::clock::Tz::UTC);
    let window = kind.catch_up_window().to_std().unwrap();
    let slot = kind.rule(schedule).due(utc, now.and_utc(), window)?;
    (posted != Some(slot.local.date())).then_some(slot.local)
}

/// Load and post the overview of `kind` as of `run.today`; whether the run
/// was handled (posted, or silent on purpose) or is to be tried again.
async fn check_and_post(
    state: &BotState,
    kind: Summary,
    post_if_empty: bool,
    run: &Run,
) -> Outcome {
    let today = run.today;
    let what = kind.name();
    match kind {
        Summary::Daily => {
            let loaded = load(state, today, 1).await;
            post_scheduled(state, what, &loaded, post_if_empty, run, || {
                render::day_message(today, today, &loaded.days[0].1, loaded.failed, loaded.total)
            })
            .await
        }
        Summary::Weekly => {
            let loaded = load(state, today, 7).await;
            let outcome = post_scheduled(state, what, &loaded, post_if_empty, run, || {
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
            if !matches!(outcome, Outcome::RetryIn(_)) {
                let week = update::WeekSnapshot::of(
                    &loaded.days,
                    today + Duration::days(6),
                    state.zone.now().naive_local(),
                    loaded.failed == 0,
                );
                state.week.set(week).await;
            }
            outcome
        }
        Summary::Monthly => {
            // From today to the end of the month: days already past are no
            // use in an overview (and on the default day 1 this is the whole
            // month).
            let (first, days) = rest_of_month(today);
            let loaded = load(state, today, days).await;
            post_scheduled(state, what, &loaded, post_if_empty, run, || {
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
/// and again after [`RETRY_AFTER`] when the calendars or the homeserver were
/// unreachable. Ticks `alive` at least every 15 minutes.
async fn scheduler_loop(state: BotState, kind: Summary, alive: Alive) {
    let job = kind.job(&state.schedule, state.zone);
    let ledger = state.posted.ledger.clone();
    schedule::run(job, ledger, alive, move |slot: Slot| {
        let state = state.clone();
        async move {
            let run = kind.run(&state, &slot);
            let post_if_empty = kind.post_if_empty(&state.schedule.get());
            check_and_post(&state, kind, post_if_empty, &run).await
        }
    })
    .await
}

/// `--test`: post `kind` once, now, if it is enabled (outside the ledger).
async fn post_now(state: BotState, kind: Summary) {
    let schedule = state.schedule.get();
    if !kind.enabled(&schedule) {
        return;
    }
    info!("Test mode: posting {} immediately", kind.name());
    let run = Run {
        today: state.zone.now().date_naive(),
        key: format!(
            "calendar-bot:{}:test:{}",
            kind.entry(),
            Utc::now().timestamp_millis()
        ),
        deadline: Instant::now() + SEND_FOR,
    };
    check_and_post(&state, kind, kind.post_if_empty(&schedule), &run).await;
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

/// The weekly run whose overview is out without a snapshot of it (posted by
/// a bot from before week updates, or the store lost it), while its week
/// still runs: then the calendar as it is now becomes its state, and later
/// changes count. What changed before that is not known.
fn missing_snapshot(
    now: NaiveDateTime,
    schedule: &Schedule,
    posted: Option<NaiveDate>,
) -> Option<NaiveDateTime> {
    let run = Summary::Weekly.last_run(now, schedule);
    (schedule.weekly_enabled
        && schedule.weekly_updates
        && posted == Some(run.date())
        && now.date() <= run.date() + Duration::days(6))
    .then_some(run)
}

/// One check: load the rest of the week (and the weeks after it, to tell
/// moved from cancelled), compare it with the last overview and post the
/// rest of the week again when something changed.
async fn check_week_update(state: &BotState) {
    let now = state.zone.now().naive_local();
    let Some(before) = state.week.get() else {
        let posted = state.posted.get(Summary::Weekly);
        let Some(run) = missing_snapshot(now, &state.schedule.get(), posted) else {
            return;
        };
        let until = run.date() + Duration::days(6);
        let loaded = load(state, now.date(), (until - now.date()).num_days() + 1).await;
        if loaded.failed == 0 {
            info!("Week update: no snapshot of this week's overview, taking the calendar as it is now");
            let week = update::WeekSnapshot::of(&loaded.days, until, run, true);
            state.week.set(week).await;
        }
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
    // The key names this update: the overview it follows and what it says.
    // Not sent everywhere: the next check tries again, and rooms that have
    // it get no second copy.
    let key = format!(
        "calendar-bot:week-update:{}:{}:{plain}",
        before.until, before.posted_at
    );
    let delivered = state
        .post(
            "week update",
            &plain,
            &html,
            &key,
            Instant::now() + SEND_FOR,
        )
        .await;
    if let Outcome::RetryIn(_) = delivered.outcome(RETRY_AFTER) {
        warn!("Week update not delivered everywhere, trying again at the next check");
        return;
    }
    state
        .week
        .set(update::WeekSnapshot::of(week, before.until, now, true))
        .await;
}

/// Check the rest of the week every [`UPDATE_CHECK`], ticking `alive` once per check.
async fn update_loop(state: BotState, alive: Alive) {
    loop {
        alive.tick();
        check_week_update(&state).await;
        sleep(TokioDuration::from_secs(UPDATE_CHECK.num_seconds() as u64)).await;
    }
}

#[cfg(test)]
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
    let now = state.zone.now().naive_local();
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
    let mut config: Config =
        mxbot_common::config::load_toml(&mxbot_common::config::config_path_from_args())?;

    if config.calendar.sources.is_empty() {
        anyhow::bail!("Config [calendar]: add at least one [[calendar.sources]] entry");
    }
    // A source's password may come from the environment (password_env, e.g. CALDAV_PASSWORD:
    // the app password the deployment writes next to the bot); a missing one stops the start.
    for (i, src) in config.calendar.sources.iter_mut().enumerate() {
        src.resolve_password_env()
            .with_context(|| format!("[[calendar.sources]] {}", i + 1))?;
    }
    if weekday(&config.calendar.weekly.day).is_none() {
        anyhow::bail!(
            "Config [calendar.weekly] day = {:?}: not a weekday (monday … sunday)",
            config.calendar.weekly.day
        );
    }

    let zone = Zone::resolve(None)?;
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
        zone,
        posted: PostedStore::load(store_path.join("posted.json")).await?,
        week: WeekStore::load(store_path.join("week.json")).await?,
    };

    install_message_handler(&state);

    for kind in [Summary::Daily, Summary::Weekly, Summary::Monthly] {
        // Test mode posts once and ends; otherwise a loop that ends restarts the bot.
        if test_mode {
            tokio::spawn(post_now(state.clone(), kind));
        } else {
            let within = kind
                .job(&state.schedule, state.zone)
                .stall_after(LONGEST_RUN);
            supervise_ticking(kind.name(), within, |alive| {
                scheduler_loop(state.clone(), kind, alive)
            });
        }
    }
    if !test_mode {
        // Two hours: three missed checks of UPDATE_CHECK, plus their loads.
        supervise_ticking(
            "week update",
            TokioDuration::from_secs(2 * 60 * 60),
            |alive| update_loop(state.clone(), alive),
        );
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
                if let Err(e) = mxbot_common::send::send_with_retry(
                    &room,
                    RoomMessageEventContent::text_html(plain, html),
                )
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
    fn weekday_names_and_nothing_else() {
        assert_eq!(weekday("Friday"), Some(Weekday::Fri));
        assert_eq!(weekday(" sun "), Some(Weekday::Sun));
        assert_eq!(weekday("monday"), Some(Weekday::Mon));
        assert_eq!(weekday("nonsense"), None);
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
    fn a_posted_week_without_a_snapshot_gets_one_while_it_runs() {
        let mut schedule = schedule("monday", TimeOfDay::new(7, 0));
        let monday = Some(date(2026, 10, 19));
        let run = at(2026, 10, 19, 7, 0);

        assert_eq!(
            missing_snapshot(at(2026, 10, 21, 21, 20), &schedule, monday),
            Some(run)
        );
        assert_eq!(
            missing_snapshot(at(2026, 10, 25, 23, 0), &schedule, monday),
            Some(run)
        );
        // Not posted (yet), or the week is over: nothing to take.
        assert_eq!(
            missing_snapshot(at(2026, 10, 21, 12, 0), &schedule, None),
            None
        );
        assert_eq!(
            missing_snapshot(at(2026, 10, 26, 6, 0), &schedule, Some(date(2026, 10, 12))),
            None
        );
        schedule.weekly_updates = false;
        assert_eq!(
            missing_snapshot(at(2026, 10, 21, 12, 0), &schedule, monday),
            None
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

    // ── The shared schedule, as calendar-bot uses it ─────────────────────────

    use mxbot_common::schedule::Runner;

    const BERLIN: Zone = Zone::DEFAULT;

    fn utc(s: &str) -> chrono::DateTime<Utc> {
        s.parse().unwrap()
    }

    /// A store directory with `posted.json` holding `posted` (if any), the
    /// summary settings, and that directory.
    async fn store(posted: Option<&str>) -> (PostedStore, Settings<Schedule>, tempfile_dir::Dir) {
        let dir = tempfile_dir::Dir::new("schedule");
        if let Some(posted) = posted {
            std::fs::write(dir.path.join("posted.json"), posted).unwrap();
        }
        let posted = PostedStore::load(dir.path.join("posted.json"))
            .await
            .unwrap();
        let settings = Settings::load(
            dir.path.join("settings.json"),
            schedule("monday", TimeOfDay::new(7, 0)),
        )
        .await
        .unwrap();
        (posted, settings, dir)
    }

    /// One round of `kind`'s job at `now`, its work answering `outcome`;
    /// returns when it wants to look again and whether the work ran.
    async fn round(
        runner: &mut Runner,
        posted: &PostedStore,
        now: &str,
        outcome: Outcome,
    ) -> (Option<chrono::DateTime<Utc>>, bool) {
        let mut ran = false;
        let wake = runner
            .round(&posted.ledger, utc(now), &mut |_slot: Slot| {
                ran = true;
                async move { outcome }
            })
            .await;
        (wake, ran)
    }

    #[tokio::test]
    async fn summaries_are_due_at_local_time_across_both_dst_changes() {
        // Handled the day before each change; the next post is 07:00 local.
        let (posted, settings, _dir) = store(Some(r#"{"daily":"2026-03-28"}"#)).await;
        let mut daily = Runner::new(Summary::Daily.job(&settings, BERLIN));
        // Saturday 23:00 CET: 07:00 CEST is 05:00 UTC, 7 hours on (not 8).
        let (wake, ran) = round(&mut daily, &posted, "2026-03-28T22:00:00Z", Outcome::Done).await;
        assert_eq!(wake, Some(utc("2026-03-29T05:00:00Z")));
        assert!(!ran);
        let (_, ran) = round(&mut daily, &posted, "2026-03-29T05:00:00Z", Outcome::Done).await;
        assert!(ran);
        assert_eq!(posted.get(Summary::Daily), Some(date(2026, 3, 29)));

        let (posted, settings, _dir) = store(Some(r#"{"daily":"2026-10-24"}"#)).await;
        let mut daily = Runner::new(Summary::Daily.job(&settings, BERLIN));
        // Saturday midnight CEST: 07:00 CET is 06:00 UTC, 8 hours on (not 7).
        let (wake, _) = round(&mut daily, &posted, "2026-10-24T22:00:00Z", Outcome::Done).await;
        assert_eq!(wake, Some(utc("2026-10-25T06:00:00Z")));
        // An hour early (where a naive sleep woke) nothing is due yet.
        let (_, ran) = round(&mut daily, &posted, "2026-10-25T05:00:00Z", Outcome::Done).await;
        assert!(!ran);
        let (_, ran) = round(&mut daily, &posted, "2026-10-25T06:00:00Z", Outcome::Done).await;
        assert!(ran);
    }

    #[tokio::test]
    async fn a_post_that_reached_no_room_is_tried_again_not_recorded() {
        let (posted, settings, _dir) = store(None).await;
        let mut daily = Runner::new(Summary::Daily.job(&settings, BERLIN));
        // Every send failed (here: for a reason that is not known to pass).
        let failed = schedule::Delivered {
            sent: Vec::new(),
            failed: vec![(
                "!room:house.org".try_into().unwrap(),
                mxbot_common::matrix_sdk::Error::AuthenticationRequired,
            )],
        };
        let outcome = failed.outcome(RETRY_AFTER);
        assert_eq!(outcome, Outcome::RetryIn(RETRY_AFTER));

        let (wake, ran) = round(&mut daily, &posted, "2026-10-12T05:00:00Z", outcome).await;
        assert!(ran);
        assert_eq!(wake, Some(utc("2026-10-12T05:00:00Z")));
        assert_eq!(posted.get(Summary::Daily), None, "not marked as posted");
        let (wake, ran) = round(&mut daily, &posted, "2026-10-12T05:01:00Z", Outcome::Done).await;
        assert!(!ran);
        assert_eq!(wake, Some(utc("2026-10-12T05:15:00Z")));
        let (_, ran) = round(&mut daily, &posted, "2026-10-12T05:15:00Z", Outcome::Done).await;
        assert!(ran);
        assert_eq!(posted.get(Summary::Daily), Some(date(2026, 10, 12)));
    }

    #[tokio::test]
    async fn daily_and_weekly_at_the_same_minute_both_stay_in_posted_json() {
        let (posted, settings, dir) = store(None).await;
        let mut daily = Runner::new(Summary::Daily.job(&settings, BERLIN));
        let mut weekly = Runner::new(Summary::Weekly.job(&settings, BERLIN));
        // Ten Mondays at 07:00 (05:00 UTC in summer time).
        for week in 0..10 {
            let monday = date(2026, 6, 1) + Duration::days(7 * week);
            let now = format!("{monday}T05:00:00Z");
            let (a, b) = tokio::join!(
                round(&mut daily, &posted, &now, Outcome::Done),
                round(&mut weekly, &posted, &now, Outcome::Done)
            );
            assert!(a.1 && b.1);
            let reloaded = PostedStore::load(dir.path.join("posted.json"))
                .await
                .unwrap();
            assert_eq!(reloaded.get(Summary::Daily), Some(monday));
            assert_eq!(reloaded.get(Summary::Weekly), Some(monday));
        }
    }

    #[tokio::test]
    async fn a_restart_catches_up_but_never_posts_what_the_old_version_posted() {
        // Written by the version before the shared schedule, Monday 07:00.
        let old = r#"{"daily":"2026-10-12","weekly":"2026-10-12","monthly":null}"#;
        let (posted, settings, _dir) = store(Some(old)).await;
        let restart = "2026-10-12T09:55:00Z";
        for kind in [Summary::Daily, Summary::Weekly] {
            let mut runner = Runner::new(kind.job(&settings, BERLIN));
            let (_, ran) = round(&mut runner, &posted, restart, Outcome::Done).await;
            assert!(!ran, "{kind:?} posted twice");
        }
        // The 1st's monthly overview was never handled and is 11 days old.
        let mut monthly = Runner::new(Summary::Monthly.job(&settings, BERLIN));
        let (_, ran) = round(&mut monthly, &posted, restart, Outcome::Done).await;
        assert!(ran);
        assert_eq!(posted.get(Summary::Monthly), Some(date(2026, 10, 1)));

        // Down over Monday 07:00: caught up at the restart.
        let (posted, settings, _dir) = store(Some(r#"{"daily":"2026-10-11"}"#)).await;
        let mut daily = Runner::new(Summary::Daily.job(&settings, BERLIN));
        let (_, ran) = round(&mut daily, &posted, restart, Outcome::Done).await;
        assert!(ran);
        assert_eq!(posted.get(Summary::Daily), Some(date(2026, 10, 12)));
    }

    #[tokio::test]
    async fn moving_the_weekly_day_after_the_overview_posts_on_the_new_day() {
        // Posted Monday; moved to Wednesday: posted again then (an overlap,
        // as before the shared schedule), not skipped for the rest of the week.
        let (posted, settings, _dir) = store(Some(r#"{"weekly":"2026-10-12"}"#)).await;
        settings
            .store()
            .set("weekly_day", "wednesday")
            .await
            .unwrap();
        let mut weekly = Runner::new(Summary::Weekly.job(&settings, BERLIN));
        let (_, ran) = round(&mut weekly, &posted, "2026-10-14T05:00:00Z", Outcome::Done).await;
        assert!(ran);
        assert_eq!(posted.get(Summary::Weekly), Some(date(2026, 10, 14)));
        let (wake, ran) = round(&mut weekly, &posted, "2026-10-14T05:01:00Z", Outcome::Done).await;
        assert!(!ran);
        assert_eq!(wake, Some(utc("2026-10-21T05:00:00Z")));

        // Moving the time within the posting day does not post it again.
        settings.store().set("weekly_time", "09:00").await.unwrap();
        let (_, ran) = round(&mut weekly, &posted, "2026-10-14T07:00:00Z", Outcome::Done).await;
        assert!(!ran);
    }

    #[tokio::test]
    async fn a_weekly_moved_to_sunday_in_the_posted_week_still_posts_on_sunday() {
        // Monday 2026-10-26 posted the overview up to Sunday 11-01; on Tuesday
        // the day becomes Sunday. Sunday 11-01 is in the same ISO week, and
        // must post, or 11-02..11-07 get no overview.
        let (posted, settings, _dir) = store(None).await;
        let mut weekly = Runner::new(Summary::Weekly.job(&settings, BERLIN));
        // 07:00 CET is 06:00 UTC after the autumn change.
        let (_, ran) = round(&mut weekly, &posted, "2026-10-26T06:00:00Z", Outcome::Done).await;
        assert!(ran);
        settings.store().set("weekly_day", "sunday").await.unwrap();
        // Tuesday: Sunday 10-25's run is still within the window and
        // caught up (as before the shared schedule).
        let (_, ran) = round(&mut weekly, &posted, "2026-10-27T10:00:00Z", Outcome::Done).await;
        assert!(ran);
        assert_eq!(posted.get(Summary::Weekly), Some(date(2026, 10, 25)));
        let (wake, ran) = round(&mut weekly, &posted, "2026-10-27T10:01:00Z", Outcome::Done).await;
        assert!(!ran);
        assert_eq!(wake, Some(utc("2026-11-01T06:00:00Z")));
        let (_, ran) = round(&mut weekly, &posted, "2026-11-01T06:00:00Z", Outcome::Done).await;
        assert!(ran);
        assert_eq!(posted.get(Summary::Weekly), Some(date(2026, 11, 1)));
        // A new process the same Sunday posts nothing more.
        let mut fresh = Runner::new(Summary::Weekly.job(&settings, BERLIN));
        let (_, ran) = round(&mut fresh, &posted, "2026-11-01T08:00:00Z", Outcome::Done).await;
        assert!(!ran);
    }

    /// A temporary directory, removed when dropped.
    mod tempfile_dir {
        pub struct Dir {
            pub path: std::path::PathBuf,
        }

        impl Dir {
            pub fn new(what: &str) -> Self {
                let path = std::env::temp_dir().join(format!(
                    "calendar-bot-{what}-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self { path }
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }
}
