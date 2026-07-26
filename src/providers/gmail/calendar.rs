use crate::model::{AccountId, CalendarEvent, CalendarInvitation, InvitationResponse};
use crate::providers::{NewCalendarEvent, OnlineMeetingKind};
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

const CAL_BASE: &str = "https://www.googleapis.com/calendar/v3";

#[derive(Deserialize)]
struct EventList {
    #[serde(default)]
    items: Vec<GEvent>,
}

#[derive(Deserialize)]
struct GEvent {
    id: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    location: String,
    #[serde(default)]
    status: String,
    start: Option<GTime>,
    end: Option<GTime>,
    #[serde(rename = "htmlLink")]
    html_link: Option<String>,
    #[serde(rename = "hangoutLink")]
    hangout_link: Option<String>,
    organizer: Option<GOrganizer>,
    #[serde(default)]
    attendees: Vec<GAttendee>,
}

#[derive(Deserialize)]
struct GTime {
    #[serde(rename = "dateTime")]
    date_time: Option<String>,
    date: Option<String>,
}

#[derive(Deserialize)]
struct GOrganizer {
    #[serde(default)]
    email: String,
    #[serde(rename = "displayName", default)]
    display_name: String,
}

#[derive(Deserialize)]
struct GAttendee {
    #[serde(default, rename = "self")]
    is_self: bool,
    #[serde(default, rename = "responseStatus")]
    response_status: String,
}

fn parse_time(t: &GTime) -> (Option<DateTime<Utc>>, bool) {
    if let Some(dt) = &t.date_time {
        return (
            DateTime::parse_from_rfc3339(dt)
                .ok()
                .map(|d| d.with_timezone(&Utc)),
            false,
        );
    }
    if let Some(date) = &t.date {
        // All-day event — treat the date as midnight UTC.
        let parsed = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|nd| DateTime::<Utc>::from_naive_utc_and_offset(nd, Utc));
        return (parsed, true);
    }
    (None, false)
}

pub async fn create_event(
    client: &reqwest::Client,
    access_token: &str,
    event: &NewCalendarEvent<'_>,
) -> Result<()> {
    let want_meet = matches!(event.online_meeting, Some(OnlineMeetingKind::GoogleMeet));
    let mut params: Vec<(&str, &str)> = Vec::new();
    if want_meet {
        params.push(("conferenceDataVersion", "1"));
    }
    if !event.attendees.is_empty() {
        params.push(("sendUpdates", "all"));
    }
    let qs = if params.is_empty() {
        String::new()
    } else {
        let mut s = String::from("?");
        for (i, (k, v)) in params.iter().enumerate() {
            if i > 0 {
                s.push('&');
            }
            s.push_str(k);
            s.push('=');
            s.push_str(v);
        }
        s
    };
    let url = format!("{CAL_BASE}/calendars/primary/events{qs}");

    let (start_v, end_v) = if event.all_day {
        (
            json!({ "date": event.start.format("%Y-%m-%d").to_string() }),
            // Google all-day end is exclusive; +1 day from the last
            // included day.
            json!({
                "date": (event.end + chrono::Duration::days(1))
                    .format("%Y-%m-%d")
                    .to_string()
            }),
        )
    } else {
        (
            json!({
                "dateTime": event.start.to_rfc3339(),
                "timeZone": "UTC",
            }),
            json!({
                "dateTime": event.end.to_rfc3339(),
                "timeZone": "UTC",
            }),
        )
    };

    let mut payload = json!({
        "summary": event.subject,
        "description": event.description,
        "location": event.location,
        "start": start_v,
        "end": end_v,
    });
    if want_meet {
        let req_id = format!(
            "aviary-{}",
            chrono::Utc::now().timestamp_micros().unsigned_abs()
        );
        payload["conferenceData"] = json!({
            "createRequest": {
                "requestId": req_id,
                "conferenceSolutionKey": { "type": "hangoutsMeet" }
            }
        });
    }
    if !event.attendees.is_empty() {
        let arr: Vec<serde_json::Value> = event
            .attendees
            .iter()
            .map(|email| json!({ "email": email }))
            .collect();
        payload["attendees"] = json!(arr);
    }

    let resp = client
        .post(&url)
        .bearer_auth(access_token)
        .json(&payload)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "google create_event failed").await);
    }
    Ok(())
}

pub(super) async fn invitation_by_ical_uid(
    client: &reqwest::Client,
    access_token: &str,
    i_cal_uid: &str,
) -> Result<Option<CalendarInvitation>> {
    let response = client
        .get(format!("{CAL_BASE}/calendars/primary/events"))
        .bearer_auth(access_token)
        .query(&[("iCalUID", i_cal_uid), ("maxResults", "10")])
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(
            crate::providers::http_error(response, "google invitation lookup failed").await,
        );
    }
    let list: EventList = response.json().await?;
    let Some(event) = list
        .items
        .into_iter()
        .find(|event| event.attendees.iter().any(|attendee| attendee.is_self))
    else {
        return Ok(None);
    };
    if event.status.eq_ignore_ascii_case("cancelled") {
        return Ok(None);
    }
    let (Some((start, all_day)), Some((end, _))) = (
        event.start.as_ref().map(parse_time),
        event.end.as_ref().map(parse_time),
    ) else {
        return Ok(None);
    };
    let (Some(start), Some(mut end)) = (start, end) else {
        return Ok(None);
    };
    if all_day && end > start {
        end -= chrono::Duration::days(1);
    }
    let response = event
        .attendees
        .iter()
        .find(|attendee| attendee.is_self)
        .map(|attendee| attendee.response_status.as_str())
        .unwrap_or_default();
    let response = match response {
        value if value.eq_ignore_ascii_case("accepted") => InvitationResponse::Accepted,
        value if value.eq_ignore_ascii_case("tentative") => InvitationResponse::Tentative,
        value if value.eq_ignore_ascii_case("declined") => InvitationResponse::Declined,
        _ => InvitationResponse::NeedsAction,
    };
    let organizer = event
        .organizer
        .as_ref()
        .map(|organizer| {
            if organizer.display_name.is_empty() {
                organizer.email.clone()
            } else if organizer.email.is_empty() {
                organizer.display_name.clone()
            } else {
                format!("{} <{}>", organizer.display_name, organizer.email)
            }
        })
        .unwrap_or_default();
    Ok(Some(CalendarInvitation {
        event_id: event.id,
        subject: event.summary,
        start,
        end,
        all_day,
        location: event.location,
        organizer,
        response,
    }))
}

pub async fn respond_to_invitation(
    client: &reqwest::Client,
    access_token: &str,
    event_id: &str,
    response: InvitationResponse,
) -> Result<()> {
    let response_status = match response {
        InvitationResponse::Accepted => "accepted",
        InvitationResponse::Tentative => "tentative",
        InvitationResponse::Declined => "declined",
        InvitationResponse::NeedsAction => {
            bail!("{}", crate::tr!("invitation-error-invalid-response"))
        }
    };
    let event_url = format!(
        "{CAL_BASE}/calendars/primary/events/{}",
        urlencoding::encode(event_id)
    );
    let fetched = client
        .get(&event_url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(crate::tr!("invitation-error-network", {
                provider: "Google Calendar",
                error: error
            }))
        })?;
    if !fetched.status().is_success() {
        let status = fetched.status();
        let error = fetched.text().await.unwrap_or_default();
        bail!(
            "{}",
            crate::tr!("invitation-error-provider", {
                provider: "Google Calendar",
                status: status,
                error: error
            })
        );
    }
    let event: serde_json::Value = fetched.json().await?;
    let mut attendees = event
        .get("attendees")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(attendee) = attendees.iter_mut().find(|attendee| {
        attendee
            .get("self")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }) else {
        bail!("{}", crate::tr!("invitation-error-no-attendee"));
    };
    attendee["responseStatus"] = json!(response_status);
    let updated = client
        .patch(event_url)
        .bearer_auth(access_token)
        .query(&[("sendUpdates", "all")])
        .json(&json!({ "attendees": attendees }))
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(crate::tr!("invitation-error-network", {
                provider: "Google Calendar",
                error: error
            }))
        })?;
    if !updated.status().is_success() {
        let status = updated.status();
        let error = updated.text().await.unwrap_or_default();
        bail!(
            "{}",
            crate::tr!("invitation-error-provider", {
                provider: "Google Calendar",
                status: status,
                error: error
            })
        );
    }
    Ok(())
}

pub async fn update_event(
    client: &reqwest::Client,
    access_token: &str,
    event_id: &str,
    event: &NewCalendarEvent<'_>,
) -> Result<()> {
    let url = format!(
        "{CAL_BASE}/calendars/primary/events/{}?sendUpdates=all",
        urlencoding::encode(event_id)
    );
    let (start, end) = if event.all_day {
        (
            json!({ "date": event.start.format("%Y-%m-%d").to_string() }),
            json!({
                "date": (event.end + chrono::Duration::days(1))
                    .format("%Y-%m-%d")
                    .to_string()
            }),
        )
    } else {
        (
            json!({ "dateTime": event.start.to_rfc3339(), "timeZone": "UTC" }),
            json!({ "dateTime": event.end.to_rfc3339(), "timeZone": "UTC" }),
        )
    };
    let payload = json!({
        "summary": event.subject,
        "description": event.description,
        "location": event.location,
        "start": start,
        "end": end,
    });
    let resp = client
        .patch(url)
        .bearer_auth(access_token)
        .json(&payload)
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(crate::tr!("calendar-update-error-network", {
                provider: "Google Calendar",
                error: error
            }))
        })?;
    if !resp.status().is_success() {
        let status = resp.status();
        let error = resp.text().await.unwrap_or_default();
        bail!(
            "{}",
            crate::tr!("calendar-update-error-provider", {
                provider: "Google Calendar",
                status: status,
                error: error
            })
        );
    }
    Ok(())
}

pub async fn delete_event(
    client: &reqwest::Client,
    access_token: &str,
    event_id: &str,
) -> Result<()> {
    let url = format!(
        "{CAL_BASE}/calendars/primary/events/{}?sendUpdates=all",
        urlencoding::encode(event_id)
    );
    let resp = client
        .delete(url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(crate::tr!("calendar-delete-error-network", {
                provider: "Google Calendar",
                error: error
            }))
        })?;
    if !resp.status().is_success() {
        let status = resp.status();
        let error = resp.text().await.unwrap_or_default();
        bail!(
            "{}",
            crate::tr!("calendar-delete-error-provider", {
                provider: "Google Calendar",
                status: status,
                error: error
            })
        );
    }
    Ok(())
}

pub async fn move_event(
    client: &reqwest::Client,
    access_token: &str,
    event_id: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    all_day: bool,
) -> Result<()> {
    let url = format!(
        "{CAL_BASE}/calendars/primary/events/{}?sendUpdates=all",
        urlencoding::encode(event_id)
    );
    let (start, end) = if all_day {
        (
            json!({ "date": start.format("%Y-%m-%d").to_string() }),
            json!({ "date": end.format("%Y-%m-%d").to_string() }),
        )
    } else {
        (
            json!({ "dateTime": start.to_rfc3339(), "timeZone": "UTC" }),
            json!({ "dateTime": end.to_rfc3339(), "timeZone": "UTC" }),
        )
    };
    let resp = client
        .patch(url)
        .bearer_auth(access_token)
        .json(&json!({ "start": start, "end": end }))
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(crate::tr!("calendar-move-error-network", {
                provider: "Google Calendar",
                error: error
            }))
        })?;
    if !resp.status().is_success() {
        let status = resp.status();
        let error = resp.text().await.unwrap_or_default();
        bail!(
            "{}",
            crate::tr!("calendar-move-error-provider", {
            provider: "Google Calendar",
            status: status,
            error: error
            })
        );
    }
    Ok(())
}

pub async fn list_events(
    client: &reqwest::Client,
    access_token: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<CalendarEvent>> {
    let url = format!("{CAL_BASE}/calendars/primary/events");
    let resp = client
        .get(&url)
        .bearer_auth(access_token)
        .query(&[
            ("timeMin", from.to_rfc3339().as_str()),
            ("timeMax", to.to_rfc3339().as_str()),
            ("singleEvents", "true"),
            ("orderBy", "startTime"),
            ("maxResults", "200"),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "google calendar failed").await);
    }
    let list: EventList = resp.json().await?;
    let mut out = Vec::with_capacity(list.items.len());
    for e in list.items {
        let (start, all_day_start) = e.start.as_ref().map(parse_time).unwrap_or((None, false));
        let (end, _) = e.end.as_ref().map(parse_time).unwrap_or((None, false));
        let start = start.unwrap_or_else(Utc::now);
        let end = end.unwrap_or(start);
        let organizer = e
            .organizer
            .as_ref()
            .map(|o| {
                if o.display_name.is_empty() {
                    o.email.clone()
                } else if o.email.is_empty() {
                    o.display_name.clone()
                } else {
                    format!("{} <{}>", o.display_name, o.email)
                }
            })
            .unwrap_or_default();
        out.push(CalendarEvent {
            id: e.id,
            account_id: AccountId::default(),
            read_only: false,
            subject: e.summary,
            start,
            end,
            all_day: all_day_start,
            location: e.location,
            organizer,
            preview: e.description,
            is_cancelled: e.status.eq_ignore_ascii_case("cancelled"),
            online_meeting_url: e.hangout_link,
            web_link: e.html_link,
        });
    }
    Ok(out)
}
