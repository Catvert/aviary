use super::{BgAccount, EventDraft, Evt};
use crate::model::InvitationResponse;
use chrono::{DateTime, Utc};
use std::sync::Arc;

pub(super) async fn load_calendar(account: Arc<BgAccount>, from: DateTime<Utc>, to: DateTime<Utc>) {
    let Some(auth) = account.auth_or_report().await else {
        return;
    };
    account.emit(Evt::Status(tr!("calendar-loading").to_string()));
    match account.session(&auth).list_events(from, to).await {
        Ok(mut events) => {
            for ev in &mut events {
                ev.account_id = account.id.clone();
            }
            account.emit(Evt::CalendarEvents {
                account_id: account.id.clone(),
                from,
                to,
                events,
            });
        }
        Err(e) => account.emit(Evt::Error(e.to_string())),
    }
}

pub(super) async fn create_event(account: Arc<BgAccount>, request_id: u64, event: EventDraft) {
    let auth = match account.ensure_auth().await {
        Ok(t) => t,
        Err(e) => {
            account.emit(Evt::EventCreateError {
                request_id,
                account_id: account.id.clone(),
                error: e.to_string(),
            });
            return;
        }
    };
    account.emit(Evt::Status(tr!("status-event-creating").to_string()));
    match account
        .session(&auth)
        .create_event(&event.as_new_event())
        .await
    {
        Ok(()) => {
            account.global.record_recipient_usage(event.attendees).await;
            account.emit(Evt::EventCreated {
                request_id,
                account_id: account.id.clone(),
            });
        }
        Err(e) => account.emit(Evt::EventCreateError {
            request_id,
            account_id: account.id.clone(),
            error: e.to_string(),
        }),
    }
}

pub(super) async fn respond_to_invitation(
    account: Arc<BgAccount>,
    message_id: String,
    event_id: String,
    response: InvitationResponse,
) {
    let auth = match account.ensure_auth().await {
        Ok(tokens) => tokens,
        Err(error) => {
            account.emit(Evt::InvitationResponseError {
                account_id: account.id.clone(),
                message_id,
                error: error.to_string(),
            });
            return;
        }
    };
    account.emit(Evt::Status(tr!("invitation-status-responding").to_string()));
    match account
        .session(&auth)
        .respond_to_invitation(&event_id, response)
        .await
    {
        Ok(()) => {
            match account
                .global
                .cache
                .load_message(account.id.clone(), message_id.clone())
                .await
            {
                Ok(Some(mut message)) => {
                    if let Some(invitation) = &mut message.invitation {
                        invitation.response = response;
                    }
                    account
                        .global
                        .cache
                        .store_message(account.id.clone(), message);
                }
                Ok(None) => {}
                Err(error) => log::warn!("updating cached invitation response: {error:#}"),
            }
            account.emit(Evt::InvitationResponded {
                account_id: account.id.clone(),
                message_id,
                response,
            });
        }
        Err(error) => account.emit(Evt::InvitationResponseError {
            account_id: account.id.clone(),
            message_id,
            error: error.to_string(),
        }),
    }
}

pub(super) async fn update_event(
    account: Arc<BgAccount>,
    request_id: u64,
    event_id: String,
    event: EventDraft,
) {
    let auth = match account.ensure_auth().await {
        Ok(tokens) => tokens,
        Err(error) => {
            account.emit(Evt::CalendarEventUpdateError {
                request_id,
                account_id: account.id.clone(),
                error: error.to_string(),
            });
            return;
        }
    };
    match account
        .session(&auth)
        .update_event(&event_id, &event.as_new_event())
        .await
    {
        Ok(()) => account.emit(Evt::CalendarEventUpdated {
            request_id,
            account_id: account.id.clone(),
        }),
        Err(error) => account.emit(Evt::CalendarEventUpdateError {
            request_id,
            account_id: account.id.clone(),
            error: error.to_string(),
        }),
    }
}

pub(super) async fn delete_event(account: Arc<BgAccount>, event_id: String) {
    let auth = match account.ensure_auth().await {
        Ok(tokens) => tokens,
        Err(error) => {
            account.emit(Evt::CalendarEventDeleteError {
                account_id: account.id.clone(),
                event_id,
                error: error.to_string(),
            });
            return;
        }
    };
    match account.session(&auth).delete_event(&event_id).await {
        Ok(()) => account.emit(Evt::CalendarEventDeleted {
            account_id: account.id.clone(),
            event_id,
        }),
        Err(error) => account.emit(Evt::CalendarEventDeleteError {
            account_id: account.id.clone(),
            event_id,
            error: error.to_string(),
        }),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn move_event(
    account: Arc<BgAccount>,
    event_id: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    previous_start: DateTime<Utc>,
    previous_end: DateTime<Utc>,
    all_day: bool,
) {
    let auth = match account.ensure_auth().await {
        Ok(tokens) => tokens,
        Err(error) => {
            account.emit(Evt::CalendarEventMoveError {
                account_id: account.id.clone(),
                event_id,
                previous_start,
                previous_end,
                error: error.to_string(),
            });
            return;
        }
    };
    match account
        .session(&auth)
        .move_event(&event_id, start, end, all_day)
        .await
    {
        Ok(()) => account.emit(Evt::CalendarEventMoved {
            account_id: account.id.clone(),
            event_id,
        }),
        Err(error) => account.emit(Evt::CalendarEventMoveError {
            account_id: account.id.clone(),
            event_id,
            previous_start,
            previous_end,
            error: error.to_string(),
        }),
    }
}
