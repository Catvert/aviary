//! Read-only subscribed iCalendar feeds.
//!
//! Network and filesystem work stays on the runtime thread; RFC 5545 parsing
//! and recurrence expansion are delegated to `calcard` in `spawn_blocking`.

use super::Evt;
use crate::model::{AccountId, CalendarEvent, IcalSubscription};
use anyhow::{anyhow, bail, Context, Result};
use calcard::common::timezone::Tz;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarParameter,
    ICalendarParameterName, ICalendarParameterValue, ICalendarProperty, ICalendarStatus,
    ICalendarValue,
};
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use futures::StreamExt;
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::task::JoinHandle;

const MAX_FEED_BYTES: usize = 10 * 1024 * 1024;
const MAX_OCCURRENCES: usize = 10_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CacheMetadata {
    fetched_at: i64,
    etag: Option<String>,
    last_modified: Option<String>,
}

pub(super) struct IcalManager {
    http: reqwest::Client,
    evt_tx: mpsc::UnboundedSender<Evt>,
    cache_dir: Option<PathBuf>,
    subscriptions: RwLock<HashMap<String, IcalSubscription>>,
    timers: Mutex<HashMap<String, JoinHandle<()>>>,
    /// Downloads are infrequent and small. Serializing them also guarantees
    /// cache metadata and feed bodies cannot race each other.
    download_lock: Mutex<()>,
}

impl IcalManager {
    pub fn new(fallback_http: reqwest::Client, evt_tx: mpsc::UnboundedSender<Evt>) -> Arc<Self> {
        let cache_dir = directories::ProjectDirs::from("be", "acetics", "aviary")
            .map(|dirs| dirs.cache_dir().join("ical"));
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() < 10
                    && attempt.url().scheme() == "https"
                    && attempt.url().username().is_empty()
                    && attempt.url().password().is_none()
                {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .unwrap_or(fallback_http);
        Arc::new(Self {
            http,
            evt_tx,
            cache_dir,
            subscriptions: RwLock::new(HashMap::new()),
            timers: Mutex::new(HashMap::new()),
            download_lock: Mutex::new(()),
        })
    }

    fn emit(&self, event: Evt) {
        let _ = self.evt_tx.send(event);
    }

    pub async fn configure(self: Arc<Self>, subscriptions: Vec<IcalSubscription>) {
        for (_, timer) in self.timers.lock().await.drain() {
            timer.abort();
        }
        *self.subscriptions.write().await = subscriptions
            .iter()
            .cloned()
            .map(|subscription| (subscription.id.clone(), subscription))
            .collect();

        let mut timers = self.timers.lock().await;
        for subscription in subscriptions {
            let Some(seconds) = subscription.refresh.seconds() else {
                continue;
            };
            let id = subscription.id.clone();
            let manager = self.clone();
            let timer = tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(seconds));
                // Tokio's first tick is immediate; range loading performs the
                // initial freshness check, so the timer begins after one full
                // configured interval.
                interval.tick().await;
                loop {
                    interval.tick().await;
                    manager.refresh(&id, false).await;
                }
            });
            timers.insert(subscription.id, timer);
        }
    }

    pub async fn load_range(
        &self,
        subscription_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        force_refresh: bool,
    ) {
        let Some(subscription) = self
            .subscriptions
            .read()
            .await
            .get(subscription_id)
            .cloned()
        else {
            return;
        };

        let cached = self.read_cache(&subscription.id).await;
        if let Some((body, metadata)) = &cached {
            self.emit(Evt::IcalSyncState {
                subscription_id: subscription.id.clone(),
                syncing: false,
                error: None,
                last_success: DateTime::from_timestamp(metadata.fetched_at, 0),
            });
            self.expand_and_emit(subscription.clone(), body.clone(), from, to)
                .await;
        }

        let stale = match &cached {
            None => true,
            Some((_, metadata)) => subscription.refresh.seconds().is_some_and(|seconds| {
                Utc::now().timestamp().saturating_sub(metadata.fetched_at) >= seconds as i64
            }),
        };
        if !force_refresh && !stale {
            return;
        }

        self.emit(Evt::IcalSyncState {
            subscription_id: subscription.id.clone(),
            syncing: true,
            error: None,
            last_success: cached
                .as_ref()
                .and_then(|(_, metadata)| DateTime::from_timestamp(metadata.fetched_at, 0)),
        });
        match self.download(&subscription).await {
            Ok((body, fetched_at)) => {
                self.expand_and_emit(subscription.clone(), body, from, to)
                    .await;
                self.emit(Evt::IcalSyncState {
                    subscription_id: subscription.id,
                    syncing: false,
                    error: None,
                    last_success: Some(fetched_at),
                });
            }
            Err(error) => self.emit(Evt::IcalSyncState {
                subscription_id: subscription.id,
                syncing: false,
                error: Some(error.to_string()),
                last_success: cached
                    .as_ref()
                    .and_then(|(_, metadata)| DateTime::from_timestamp(metadata.fetched_at, 0)),
            }),
        }
    }

    pub async fn refresh(&self, subscription_id: &str, _manual: bool) {
        let Some(subscription) = self
            .subscriptions
            .read()
            .await
            .get(subscription_id)
            .cloned()
        else {
            return;
        };
        let last_success = self
            .read_cache(subscription_id)
            .await
            .and_then(|(_, metadata)| DateTime::from_timestamp(metadata.fetched_at, 0));
        self.emit(Evt::IcalSyncState {
            subscription_id: subscription.id.clone(),
            syncing: true,
            error: None,
            last_success,
        });
        match self.download(&subscription).await {
            Ok((_, fetched_at)) => {
                self.emit(Evt::IcalSyncState {
                    subscription_id: subscription.id.clone(),
                    syncing: false,
                    error: None,
                    last_success: Some(fetched_at),
                });
                self.emit(Evt::IcalFeedUpdated {
                    subscription_id: subscription.id,
                });
            }
            Err(error) => self.emit(Evt::IcalSyncState {
                subscription_id: subscription.id,
                syncing: false,
                error: Some(error.to_string()),
                last_success,
            }),
        }
    }

    pub async fn delete_cache(&self, subscription_id: &str) {
        let Some((body_path, metadata_path)) = self.cache_paths(subscription_id) else {
            return;
        };
        let _ = tokio::task::spawn_blocking(move || {
            for path in [body_path, metadata_path] {
                if let Err(error) = std::fs::remove_file(&path) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        log::warn!("failed to remove iCal cache: {error:#}");
                    }
                }
            }
        })
        .await;
    }

    async fn expand_and_emit(
        &self,
        subscription: IcalSubscription,
        body: String,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) {
        let id = subscription.id.clone();
        match tokio::task::spawn_blocking(move || expand_feed(&subscription, &body, from, to)).await
        {
            Ok(Ok(events)) => self.emit(Evt::IcalEvents {
                subscription_id: id,
                from,
                to,
                events,
            }),
            Ok(Err(error)) => self.emit(Evt::IcalSyncState {
                subscription_id: id,
                syncing: false,
                error: Some(error.to_string()),
                last_success: None,
            }),
            Err(_error) => self.emit(Evt::IcalSyncState {
                subscription_id: id,
                syncing: false,
                error: Some(tr!("ical-error-invalid-feed").to_string()),
                last_success: None,
            }),
        }
    }

    async fn download(&self, subscription: &IcalSubscription) -> Result<(String, DateTime<Utc>)> {
        let _guard = self.download_lock.lock().await;
        let url = normalized_url(&subscription.url)?;
        let cached = self.read_cache(&subscription.id).await;
        let mut request = self
            .http
            .get(url)
            .timeout(std::time::Duration::from_secs(20));
        if let Some((_, metadata)) = &cached {
            if let Some(etag) = &metadata.etag {
                request = request.header(IF_NONE_MATCH, etag);
            }
            if let Some(last_modified) = &metadata.last_modified {
                request = request.header(IF_MODIFIED_SINCE, last_modified);
            }
        }
        let response = request
            .send()
            .await
            .map_err(|_| anyhow!(tr!("ical-error-network")))?;
        if response.url().scheme() != "https" {
            bail!("{}", tr!("ical-error-https"));
        }

        let fetched_at = Utc::now();
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            let Some((body, mut metadata)) = cached else {
                bail!("{}", tr!("ical-error-empty-304"));
            };
            metadata.fetched_at = fetched_at.timestamp();
            self.write_cache(&subscription.id, &body, &metadata).await?;
            return Ok((body, fetched_at));
        }
        let status = response.status();
        if !status.is_success() {
            bail!("{}", tr!("ical-error-http", { status: status.as_u16() }));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_FEED_BYTES as u64)
        {
            bail!("{}", tr!("ical-error-too-large"));
        }
        let headers = response.headers().clone();
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| anyhow!(tr!("ical-error-network")))?;
            if bytes.len().saturating_add(chunk.len()) > MAX_FEED_BYTES {
                bail!("{}", tr!("ical-error-too-large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = String::from_utf8(bytes).context(tr!("ical-error-encoding"))?;
        ICalendar::parse(&body).map_err(|_| anyhow!(tr!("ical-error-invalid-feed")))?;
        let metadata = CacheMetadata {
            fetched_at: fetched_at.timestamp(),
            etag: header_string(&headers, ETAG),
            last_modified: header_string(&headers, LAST_MODIFIED),
        };
        self.write_cache(&subscription.id, &body, &metadata).await?;
        Ok((body, fetched_at))
    }

    fn cache_paths(&self, subscription_id: &str) -> Option<(PathBuf, PathBuf)> {
        let safe_id: String = subscription_id
            .chars()
            .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
            .collect();
        (!safe_id.is_empty()).then(|| {
            let root = self.cache_dir.as_ref()?;
            Some((
                root.join(format!("{safe_id}.ics")),
                root.join(format!("{safe_id}.json")),
            ))
        })?
    }

    async fn read_cache(&self, subscription_id: &str) -> Option<(String, CacheMetadata)> {
        let (body_path, metadata_path) = self.cache_paths(subscription_id)?;
        tokio::task::spawn_blocking(move || {
            let body = std::fs::read_to_string(body_path).ok()?;
            let metadata = serde_json::from_slice(&std::fs::read(metadata_path).ok()?).ok()?;
            Some((body, metadata))
        })
        .await
        .ok()
        .flatten()
    }

    async fn write_cache(
        &self,
        subscription_id: &str,
        body: &str,
        metadata: &CacheMetadata,
    ) -> Result<()> {
        let (body_path, metadata_path) = self
            .cache_paths(subscription_id)
            .context(tr!("ical-error-cache-unavailable"))?;
        let body = body.to_string();
        let metadata = serde_json::to_vec(metadata)?;
        tokio::task::spawn_blocking(move || {
            write_private_atomic(&body_path, body.as_bytes())?;
            write_private_atomic(&metadata_path, &metadata)
        })
        .await??;
        Ok(())
    }
}

fn normalized_url(raw: &str) -> Result<reqwest::Url> {
    let trimmed = raw.trim();
    let normalized = trimmed
        .strip_prefix("webcal://")
        .map(|rest| format!("https://{rest}"))
        .unwrap_or_else(|| trimmed.to_string());
    let url = reqwest::Url::parse(&normalized).context(tr!("ical-error-url"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        bail!("{}", tr!("ical-error-https"));
    }
    Ok(url)
}

fn header_string(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("cache path without parent")?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension("tmp");
    write_private_file(&temporary, bytes)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(unix)]
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)?;
    Ok(())
}

fn expand_feed(
    subscription: &IcalSubscription,
    body: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<CalendarEvent>> {
    let mut calendar =
        ICalendar::parse(body).map_err(|_| anyhow!(tr!("ical-error-invalid-feed")))?;
    normalize_utc_recurrence_timezones(&mut calendar);
    let custom_timezones = custom_timezones(&calendar, to);
    let component_timezones = component_timezones(&calendar, &custom_timezones);
    mask_custom_timezones(&mut calendar, &custom_timezones);
    let default_timezone = iana_time_zone::get_timezone()
        .ok()
        .and_then(|name| name.parse::<chrono_tz::Tz>().ok())
        .map(Tz::Tz)
        .unwrap_or_else(|| Tz::Fixed(Local::now().offset().fix()));
    let expanded = calendar.expand_dates(default_timezone, MAX_OCCURRENCES);
    if !expanded.errors.is_empty() {
        log::warn!(
            "{} iCalendar component(s) could not be expanded for subscription {}",
            expanded.errors.len(),
            subscription.id
        );
    }
    let account_id = AccountId(format!("ical:{}", subscription.id));
    let mut events = Vec::new();
    for occurrence in expanded.events {
        let Some(component) = calendar.component_by_id(occurrence.comp_id) else {
            continue;
        };
        if component.component_type != ICalendarComponentType::VEvent {
            continue;
        }
        let all_day = is_all_day(component);
        let Some(occurrence) = occurrence.try_into_date_time() else {
            continue;
        };
        let (start, end, start_timestamp, end_timestamp) = if all_day {
            let start_date = occurrence.start.naive_local().date();
            let end_date = occurrence.end.naive_local().date().max(start_date);
            let start = utc_midnight(start_date);
            let end = utc_midnight(end_date);
            (start, end, start.timestamp(), end.timestamp())
        } else {
            let zones = component_timezones.get(&occurrence.comp_id);
            let start_timestamp = zones
                .and_then(|zones| zones.start.as_ref())
                .and_then(|tzid| custom_timezones.get(tzid))
                .map_or_else(
                    || occurrence.start.timestamp(),
                    |timezone| timezone.to_utc_timestamp(occurrence.start.naive_local()),
                );
            let mut end_timestamp = if zones.is_some_and(|zones| zones.duration) {
                start_timestamp.saturating_add(
                    occurrence
                        .end
                        .timestamp()
                        .saturating_sub(occurrence.start.timestamp()),
                )
            } else {
                zones
                    .and_then(|zones| zones.end.as_ref())
                    .and_then(|tzid| custom_timezones.get(tzid))
                    .map_or_else(
                        || occurrence.end.timestamp(),
                        |timezone| timezone.to_utc_timestamp(occurrence.end.naive_local()),
                    )
            };
            if !component.has_property(&ICalendarProperty::Dtend)
                && !component.has_property(&ICalendarProperty::Duration)
            {
                // RFC 5545: a DATE-TIME DTSTART without DTEND/DURATION has
                // zero duration. calcard currently expands it to day end.
                end_timestamp = start_timestamp;
            }
            let Some(start) = DateTime::from_timestamp(start_timestamp, 0) else {
                continue;
            };
            let end =
                DateTime::from_timestamp(end_timestamp.max(start_timestamp), 0).unwrap_or(start);
            (start, end, start_timestamp, end_timestamp)
        };
        if !all_day
            && !component.has_property(&ICalendarProperty::Dtend)
            && !component.has_property(&ICalendarProperty::Duration)
        {
            debug_assert_eq!(start_timestamp, end_timestamp);
        }
        let outside_range = if end_timestamp == start_timestamp {
            start_timestamp < from.timestamp() || start_timestamp >= to.timestamp()
        } else {
            start_timestamp >= to.timestamp() || end_timestamp <= from.timestamp()
        };
        if outside_range {
            continue;
        }
        let uid = text_property(component, &ICalendarProperty::Uid)
            .unwrap_or_else(|| format!("component-{}", occurrence.comp_id));
        let organizer = organizer(component);
        events.push(CalendarEvent {
            id: format!("{}:{uid}:{start_timestamp}", subscription.id),
            account_id: account_id.clone(),
            read_only: true,
            subject: text_property(component, &ICalendarProperty::Summary)
                .unwrap_or_else(|| tr!("calendar-no-subject").to_string()),
            start,
            end,
            all_day,
            location: text_property(component, &ICalendarProperty::Location).unwrap_or_default(),
            organizer,
            preview: text_property(component, &ICalendarProperty::Description).unwrap_or_default(),
            is_cancelled: component.status() == Some(&ICalendarStatus::Cancelled),
            online_meeting_url: None,
            web_link: text_property(component, &ICalendarProperty::Url),
        });
    }
    events.sort_by_key(|event| event.start);
    Ok(events)
}

#[derive(Debug)]
struct CustomTimezone {
    initial_offset: i32,
    transitions: Vec<TimezoneTransition>,
}

#[derive(Debug)]
struct TimezoneTransition {
    local_start: NaiveDateTime,
    offset_from: i32,
    offset_to: i32,
}

impl CustomTimezone {
    fn from_component(
        calendar: &ICalendar,
        component: &ICalendarComponent,
        through: DateTime<Utc>,
    ) -> Option<Self> {
        let mut transitions = Vec::new();
        let horizon = (through + Duration::days(370)).naive_utc();
        for component_id in &component.component_ids {
            let Some(observance) = calendar.component_by_id(*component_id) else {
                continue;
            };
            if !matches!(
                observance.component_type,
                ICalendarComponentType::Standard | ICalendarComponentType::Daylight
            ) {
                continue;
            }
            let (Some(offset_from), Some(offset_to)) = (
                offset_property(observance, &ICalendarProperty::Tzoffsetfrom),
                offset_property(observance, &ICalendarProperty::Tzoffsetto),
            ) else {
                continue;
            };
            let mut recurrence = observance.clone();
            recurrence.component_type = ICalendarComponentType::VEvent;
            let expanded = ICalendar {
                components: vec![recurrence],
            }
            .expand_dates(Tz::Floating, MAX_OCCURRENCES);
            if !expanded.errors.is_empty() {
                log::warn!("invalid embedded VTIMEZONE recurrence");
            }
            for occurrence in expanded.events {
                let local_start = occurrence.start.naive_local();
                if local_start > horizon {
                    continue;
                }
                transitions.push(TimezoneTransition {
                    local_start,
                    offset_from,
                    offset_to,
                });
            }
        }
        transitions.sort_by_key(|transition| transition.local_start);
        transitions.dedup_by_key(|transition| transition.local_start);
        let initial_offset = transitions.first()?.offset_from;
        Some(Self {
            initial_offset,
            transitions,
        })
    }

    fn to_utc_timestamp(&self, local: NaiveDateTime) -> i64 {
        let offset = self
            .transitions
            .iter()
            .rev()
            .find(|transition| transition.local_start <= local)
            .map_or(self.initial_offset, |transition| {
                let skipped_seconds = transition.offset_to - transition.offset_from;
                if skipped_seconds > 0
                    && local
                        < transition.local_start + Duration::seconds(i64::from(skipped_seconds))
                {
                    // RFC 5545 resolves a local time in a spring-forward gap
                    // using the offset in force before the transition.
                    transition.offset_from
                } else {
                    transition.offset_to
                }
            });
        local.and_utc().timestamp() - i64::from(offset)
    }
}

#[derive(Debug, Default)]
struct ComponentTimezones {
    start: Option<String>,
    end: Option<String>,
    duration: bool,
}

fn custom_timezones(
    calendar: &ICalendar,
    through: DateTime<Utc>,
) -> HashMap<String, CustomTimezone> {
    calendar
        .timezones()
        .filter_map(|component| {
            let tzid = text_property(component, &ICalendarProperty::Tzid)?;
            CustomTimezone::from_component(calendar, component, through)
                .map(|timezone| (tzid, timezone))
        })
        .collect()
}

fn component_timezones(
    calendar: &ICalendar,
    custom_timezones: &HashMap<String, CustomTimezone>,
) -> HashMap<u32, ComponentTimezones> {
    calendar
        .components
        .iter()
        .enumerate()
        .filter(|(_, component)| component.component_type == ICalendarComponentType::VEvent)
        .filter_map(|(component_id, component)| {
            let start = custom_timezone_parameter(
                component.property(&ICalendarProperty::Dtstart),
                custom_timezones,
            );
            let end = custom_timezone_parameter(
                component.property(&ICalendarProperty::Dtend),
                custom_timezones,
            );
            let duration = start.is_some() && component.has_property(&ICalendarProperty::Duration);
            (start.is_some() || end.is_some()).then_some((
                component_id as u32,
                ComponentTimezones {
                    start,
                    end,
                    duration,
                },
            ))
        })
        .collect()
}

fn custom_timezone_parameter(
    entry: Option<&calcard::icalendar::ICalendarEntry>,
    custom_timezones: &HashMap<String, CustomTimezone>,
) -> Option<String> {
    entry
        .and_then(|entry| entry.parameter(&ICalendarParameterName::Tzid))
        .and_then(ICalendarParameterValue::as_text)
        .filter(|tzid| custom_timezones.contains_key(*tzid))
        .map(ToOwned::to_owned)
}

/// calcard resolves IANA and common Outlook aliases itself. For an arbitrary
/// embedded VTIMEZONE, expansion still needs a concrete `Tz`; UTC preserves
/// the wall clock without introducing host-local DST gaps. We then apply the
/// embedded transition table to each expanded occurrence above.
fn mask_custom_timezones(
    calendar: &mut ICalendar,
    custom_timezones: &HashMap<String, CustomTimezone>,
) {
    for component in &mut calendar.components {
        for entry in &mut component.entries {
            for parameter in &mut entry.params {
                if parameter.name == ICalendarParameterName::Tzid
                    && parameter
                        .value
                        .as_text()
                        .is_some_and(|tzid| custom_timezones.contains_key(tzid))
                {
                    parameter.value = ICalendarParameterValue::Text("Etc/UTC".into());
                }
            }
        }
    }
}

fn offset_property(component: &ICalendarComponent, property: &ICalendarProperty) -> Option<i32> {
    let ICalendarValue::PartialDateTime(offset) = component.property(property)?.values.first()?
    else {
        return None;
    };
    let seconds = i32::from(offset.tz_hour?) * 3600 + i32::from(offset.tz_minute.unwrap_or(0)) * 60;
    Some(if offset.tz_minus { -seconds } else { seconds })
}

fn utc_midnight(date: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("valid midnight"))
}

/// calcard 0.3.x correctly parses a trailing `Z`, but its recurrence iterator
/// chooses the DTSTART `TZID` when materializing later occurrences. Explicit
/// UTC dates normally have no TZID, so supply the equivalent resolver hint.
fn normalize_utc_recurrence_timezones(calendar: &mut ICalendar) {
    for component in &mut calendar.components {
        if !component.is_recurrent() {
            continue;
        }
        let Some(start) = component.property_mut(&ICalendarProperty::Dtstart) else {
            continue;
        };
        let is_utc = matches!(
            start.values.first(),
            Some(ICalendarValue::PartialDateTime(date))
                if date.tz_hour == Some(0) && date.tz_minute == Some(0)
        );
        if is_utc && !start.has_parameter(&ICalendarParameterName::Tzid) {
            start.params.push(ICalendarParameter {
                name: ICalendarParameterName::Tzid,
                value: ICalendarParameterValue::Text("Etc/UTC".into()),
            });
        }
    }
}

fn text_property(component: &ICalendarComponent, property: &ICalendarProperty) -> Option<String> {
    component
        .property(property)
        .and_then(|entry| entry.values.first())
        .and_then(ICalendarValue::as_text)
        .map(ToOwned::to_owned)
}

fn is_all_day(component: &ICalendarComponent) -> bool {
    component
        .property(&ICalendarProperty::Dtstart)
        .and_then(|entry| entry.values.first())
        .is_some_and(
            |value| matches!(value, ICalendarValue::PartialDateTime(date) if !date.has_time()),
        )
}

fn organizer(component: &ICalendarComponent) -> String {
    let Some(entry) = component.property(&ICalendarProperty::Organizer) else {
        return String::new();
    };
    let address = entry.calendar_address().unwrap_or_default();
    let name = entry
        .parameter(&ICalendarParameterName::Cn)
        .and_then(|value| value.as_text())
        .unwrap_or_default();
    match (name.is_empty(), address.is_empty()) {
        (false, false) => format!("{name} <{address}>"),
        (false, true) => name.to_string(),
        (true, false) => address.to_string(),
        (true, true) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IcalRefreshInterval;
    use chrono::{Datelike, TimeZone, Timelike};

    fn subscription() -> IcalSubscription {
        IcalSubscription {
            id: "test".into(),
            name: "Test".into(),
            url: "https://example.com/test.ics".into(),
            color: 0x61afef,
            refresh: IcalRefreshInterval::OneHour,
        }
    }

    #[test]
    fn expands_recurrence_exdates_and_all_day_events() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:daily\r\nSUMMARY:Daily\r\nDTSTART:20260101T090000Z\r\nDTEND:20260101T100000Z\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEXDATE:20260102T090000Z\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:holiday\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20260103\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let from = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2026, 1, 5, 0, 0, 0).unwrap();

        let events = expand_feed(&subscription(), body, from, to).unwrap();

        assert_eq!(events.len(), 3);
        assert_eq!(events.iter().filter(|event| event.all_day).count(), 1);
        assert!(events.iter().all(|event| event.read_only));
        assert!(!events
            .iter()
            .any(|event| event.start.date_naive().day() == 2));
    }

    #[test]
    fn rejects_non_https_urls_but_accepts_webcal_alias() {
        assert!(normalized_url("http://example.com/a.ics").is_err());
        assert!(normalized_url("https://user:password@example.com/a.ics").is_err());
        assert_eq!(
            normalized_url("webcal://example.com/a.ics")
                .unwrap()
                .as_str(),
            "https://example.com/a.ics"
        );
    }

    #[test]
    fn applies_recurrence_overrides() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:series\r\nSUMMARY:Original\r\nDTSTART:20260101T090000Z\r\nDTEND:20260101T100000Z\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:series\r\nRECURRENCE-ID:20260102T090000Z\r\nSUMMARY:Moved\r\nDTSTART:20260102T120000Z\r\nDTEND:20260102T130000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let from = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2026, 1, 5, 0, 0, 0).unwrap();

        let events = expand_feed(&subscription(), body, from, to).unwrap();

        assert_eq!(events.len(), 3);
        let moved = events
            .iter()
            .find(|event| event.subject == "Moved")
            .expect("overridden occurrence");
        assert_eq!(moved.start.hour(), 12);
    }

    #[test]
    fn resolves_outlook_vtimezone_identifiers_with_dst() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Custom-Europe\r\nX-MICROSOFT-CDO-TZID:3\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:winter\r\nSUMMARY:Winter\r\nDTSTART;TZID=Custom-Europe:20260115T090000\r\nDTEND;TZID=Custom-Europe:20260115T100000\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:summer\r\nSUMMARY:Summer\r\nDTSTART;TZID=Custom-Europe:20260715T090000\r\nDTEND;TZID=Custom-Europe:20260715T100000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let from = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();

        let events = expand_feed(&subscription(), body, from, to).unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].start.hour(), 8);
        assert_eq!(events[1].start.hour(), 7);
    }

    #[test]
    fn resolves_arbitrary_embedded_vtimezone_transitions() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Vendor/Brussels\r\nBEGIN:STANDARD\r\nDTSTART:19701025T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r\nEND:STANDARD\r\nBEGIN:DAYLIGHT\r\nDTSTART:19700329T020000\r\nTZOFFSETFROM:+0100\r\nTZOFFSETTO:+0200\r\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r\nEND:DAYLIGHT\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:custom-winter\r\nSUMMARY:Winter\r\nDTSTART;TZID=Vendor/Brussels:20260115T090000\r\nDTEND;TZID=Vendor/Brussels:20260115T100000\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:custom-duration\r\nSUMMARY:Duration\r\nDTSTART;TZID=Vendor/Brussels:20260329T013000\r\nDURATION:PT2H\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:custom-summer\r\nSUMMARY:Summer\r\nDTSTART;TZID=Vendor/Brussels:20260715T090000\r\nDTEND;TZID=Vendor/Brussels:20260715T100000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let from = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();

        let events = expand_feed(&subscription(), body, from, to).unwrap();

        assert_eq!(events.len(), 3);
        assert_eq!(events[0].start.hour(), 8);
        let duration = events
            .iter()
            .find(|event| event.subject == "Duration")
            .expect("duration event");
        assert_eq!(duration.start.hour(), 0);
        assert_eq!(duration.start.minute(), 30);
        assert_eq!(duration.end.hour(), 2);
        assert_eq!(duration.end.minute(), 30);
        assert_eq!(events[2].start.hour(), 7);
    }
}
