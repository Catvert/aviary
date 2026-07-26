use super::{from_label, Client, GraphList, GraphRecipient, BASE};
use crate::model::{AccountId, CalendarEvent, InvitationResponse};
use crate::providers::NewCalendarEvent;
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct GraphEvent {
    id: String,
    subject: Option<String>,
    #[serde(rename = "isAllDay")]
    is_all_day: Option<bool>,
    #[serde(rename = "isCancelled")]
    is_cancelled: Option<bool>,
    #[serde(rename = "bodyPreview")]
    body_preview: Option<String>,
    #[serde(rename = "webLink")]
    web_link: Option<String>,
    start: Option<GraphDateTime>,
    end: Option<GraphDateTime>,
    location: Option<GraphLocation>,
    organizer: Option<GraphRecipient>,
    #[serde(rename = "onlineMeeting")]
    online_meeting: Option<GraphOnlineMeeting>,
}

#[derive(Deserialize)]
struct GraphDateTime {
    #[serde(rename = "dateTime")]
    date_time: String,
}

#[derive(Deserialize)]
struct GraphLocation {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct GraphOnlineMeeting {
    #[serde(rename = "joinUrl")]
    join_url: Option<String>,
}

fn parse_graph_dt(d: &GraphDateTime) -> Option<DateTime<Utc>> {
    let raw = d.date_time.trim_end_matches('Z');
    let naive = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

impl From<GraphEvent> for CalendarEvent {
    fn from(e: GraphEvent) -> Self {
        let start = e
            .start
            .as_ref()
            .and_then(parse_graph_dt)
            .unwrap_or_else(Utc::now);
        let end = e.end.as_ref().and_then(parse_graph_dt).unwrap_or(start);
        let location = e.location.and_then(|l| l.display_name).unwrap_or_default();
        let organizer = from_label(e.organizer);
        let online_meeting_url = e.online_meeting.and_then(|m| m.join_url);
        Self {
            id: e.id,
            account_id: AccountId::default(),
            read_only: false,
            subject: e.subject.unwrap_or_default(),
            start,
            end,
            all_day: e.is_all_day.unwrap_or(false),
            location,
            organizer,
            preview: e.body_preview.unwrap_or_default(),
            is_cancelled: e.is_cancelled.unwrap_or(false),
            online_meeting_url,
            web_link: e.web_link,
        }
    }
}

pub async fn create_event(client: &Client<'_>, event: &NewCalendarEvent<'_>) -> Result<()> {
    let url = format!("{BASE}/me/events");
    let (start_str, end_str) = if event.all_day {
        (
            event.start.format("%Y-%m-%dT00:00:00").to_string(),
            // Graph requires the end date to be the day after the last
            // included day for all-day events.
            (event.end + chrono::Duration::days(1))
                .format("%Y-%m-%dT00:00:00")
                .to_string(),
        )
    } else {
        (
            event.start.format("%Y-%m-%dT%H:%M:%S").to_string(),
            event.end.format("%Y-%m-%dT%H:%M:%S").to_string(),
        )
    };
    let mut payload = json!({
        "subject": event.subject,
        "body": {
            "contentType": "HTML",
            "content": event.description,
        },
        "start": { "dateTime": start_str, "timeZone": "UTC" },
        "end":   { "dateTime": end_str,   "timeZone": "UTC" },
        "isAllDay": event.all_day,
    });
    if !event.location.is_empty() {
        payload["location"] = json!({ "displayName": event.location });
    }
    if matches!(
        event.online_meeting,
        Some(crate::providers::OnlineMeetingKind::Teams)
    ) {
        payload["isOnlineMeeting"] = json!(true);
        payload["onlineMeetingProvider"] = json!("teamsForBusiness");
    }
    if !event.attendees.is_empty() {
        let arr: Vec<serde_json::Value> = event
            .attendees
            .iter()
            .map(|email| {
                json!({
                    "emailAddress": { "address": email },
                    "type": "required"
                })
            })
            .collect();
        payload["attendees"] = json!(arr);
    }
    super::post_json(client, &url, &payload, "create_event").await
}

pub async fn respond_to_invitation(
    client: &Client<'_>,
    event_id: &str,
    response: InvitationResponse,
) -> Result<()> {
    let action = match response {
        InvitationResponse::Accepted => "accept",
        InvitationResponse::Tentative => "tentativelyAccept",
        InvitationResponse::Declined => "decline",
        InvitationResponse::NeedsAction => {
            bail!("{}", crate::tr!("invitation-error-invalid-response"))
        }
    };
    let url = format!(
        "{BASE}/me/events/{}/{action}",
        urlencoding::encode(event_id)
    );
    let payload = json!({
        "comment": "",
        "sendResponse": true,
    });
    let response = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(crate::tr!("invitation-error-network", {
                provider: "Microsoft Graph",
                error: error
            }))
        })?;
    if !response.status().is_success() {
        let status = response.status();
        let error = response.text().await.unwrap_or_default();
        bail!(
            "{}",
            crate::tr!("invitation-error-provider", {
                provider: "Microsoft Graph",
                status: status,
                error: error
            })
        );
    }
    Ok(())
}

pub async fn update_event(
    client: &Client<'_>,
    event_id: &str,
    event: &NewCalendarEvent<'_>,
) -> Result<()> {
    let url = format!("{BASE}/me/events/{}", urlencoding::encode(event_id));
    let (start, end) = if event.all_day {
        (
            event.start.format("%Y-%m-%dT00:00:00").to_string(),
            (event.end + chrono::Duration::days(1))
                .format("%Y-%m-%dT00:00:00")
                .to_string(),
        )
    } else {
        (
            event.start.format("%Y-%m-%dT%H:%M:%S").to_string(),
            event.end.format("%Y-%m-%dT%H:%M:%S").to_string(),
        )
    };
    let payload = json!({
        "subject": event.subject,
        "body": { "contentType": "HTML", "content": event.description },
        "location": { "displayName": event.location },
        "start": { "dateTime": start, "timeZone": "UTC" },
        "end": { "dateTime": end, "timeZone": "UTC" },
        "isAllDay": event.all_day,
    });
    let resp = client
        .patch(url)
        .json(&payload)
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(crate::tr!("calendar-update-error-network", {
                provider: "Microsoft Graph",
                error: error
            }))
        })?;
    if !resp.status().is_success() {
        let status = resp.status();
        let error = resp.text().await.unwrap_or_default();
        bail!(
            "{}",
            crate::tr!("calendar-update-error-provider", {
                provider: "Microsoft Graph",
                status: status,
                error: error
            })
        );
    }
    Ok(())
}

pub async fn delete_event(client: &Client<'_>, event_id: &str) -> Result<()> {
    let url = format!("{BASE}/me/events/{}", urlencoding::encode(event_id));
    let resp = client.delete(url).send().await.map_err(|error| {
        anyhow::anyhow!(crate::tr!("calendar-delete-error-network", {
            provider: "Microsoft Graph",
            error: error
        }))
    })?;
    if !resp.status().is_success() {
        let status = resp.status();
        let error = resp.text().await.unwrap_or_default();
        bail!(
            "{}",
            crate::tr!("calendar-delete-error-provider", {
                provider: "Microsoft Graph",
                status: status,
                error: error
            })
        );
    }
    Ok(())
}

pub async fn move_event(
    client: &Client<'_>,
    event_id: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    all_day: bool,
) -> Result<()> {
    let url = format!("{BASE}/me/events/{}", urlencoding::encode(event_id));
    let (start, end) = if all_day {
        (
            start.format("%Y-%m-%dT00:00:00").to_string(),
            end.format("%Y-%m-%dT00:00:00").to_string(),
        )
    } else {
        (
            start.format("%Y-%m-%dT%H:%M:%S").to_string(),
            end.format("%Y-%m-%dT%H:%M:%S").to_string(),
        )
    };
    let payload = json!({
        "start": { "dateTime": start, "timeZone": "UTC" },
        "end": { "dateTime": end, "timeZone": "UTC" },
        "isAllDay": all_day,
    });
    let resp = client
        .patch(url)
        .json(&payload)
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!(crate::tr!("calendar-move-error-network", {
                provider: "Microsoft Graph",
                error: error
            }))
        })?;
    if !resp.status().is_success() {
        let status = resp.status();
        let error = resp.text().await.unwrap_or_default();
        bail!(
            "{}",
            crate::tr!("calendar-move-error-provider", {
            provider: "Microsoft Graph",
            status: status,
            error: error
            })
        );
    }
    Ok(())
}

pub async fn list_events(
    client: &Client<'_>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<CalendarEvent>> {
    let url = format!("{BASE}/me/calendarView");
    let start = from.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let end = to.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let resp = client
        .get(&url)
        .header("Prefer", "outlook.timezone=\"UTC\"")
        .query(&[
            ("startDateTime", start.as_str()),
            ("endDateTime", end.as_str()),
            ("$top", "200"),
            ("$orderby", "start/dateTime"),
            (
                "$select",
                "id,subject,start,end,isAllDay,isCancelled,bodyPreview,webLink,location,organizer,onlineMeeting",
            ),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph calendar failed").await);
    }
    let list: GraphList<GraphEvent> = resp.json().await?;
    Ok(list.value.into_iter().map(Into::into).collect())
}
