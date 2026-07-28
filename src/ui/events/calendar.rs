//! Reduces calendar runtime events into UI state.

use super::super::app::AviaryApp;
use super::super::state::ThreadBodyState;
use crate::model::{AccountId, CalendarEvent, InvitationResponse, Message, MessageRef};
use chrono::{DateTime, Utc};
use gpui::{Context, Window};
use gpui_component::notification::Notification;

impl AviaryApp {
    pub(super) fn on_invitation_responded(
        &mut self,
        account_id: AccountId,
        message_id: String,
        response: InvitationResponse,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let reference = MessageRef {
            account_id: account_id.clone(),
            id: message_id.clone(),
        };
        self.invitation_responses_in_flight.remove(&reference);
        let update = |message: &mut Message| {
            if message.header.account_id == account_id && message.header.id == message_id {
                if let Some(invitation) = &mut message.invitation {
                    invitation.response = response;
                }
            }
        };
        if let Some(message) = self.mailbox.selected_mut() {
            update(message);
        }
        for tab in &mut self.mailbox.open_tabs {
            if let Some(message) = tab.message_mut() {
                update(message);
            }
        }
        for state in self.mailbox.thread_bodies.values_mut() {
            if let ThreadBodyState::Loaded(message) = state {
                update(message);
            }
        }
        self.calendar.force_reload();
        self.toast(
            window,
            cx,
            Notification::success(tr!("invitation-response-saved")),
        );
        cx.notify();
    }

    pub(super) fn on_invitation_response_error(
        &mut self,
        account_id: AccountId,
        message_id: String,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.invitation_responses_in_flight.remove(&MessageRef {
            account_id,
            id: message_id,
        });
        self.toast(window, cx, Notification::error(error));
        cx.notify();
    }

    pub(super) fn on_calendar_events(
        &mut self,
        account_id: AccountId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        events: Vec<CalendarEvent>,
    ) {
        if self.calendar_account_visible(&account_id) {
            self.calendar.on_events(&account_id, from, to, events);
        }
    }

    /// A window failed even after the runtime's transient-failure retries.
    /// Its months go back among the missing chunks (behind the scope's
    /// cooldown) — leaving them marked would display them as event-free until
    /// the next full reload.
    pub(super) fn on_calendar_load_failed(
        &mut self,
        account_id: AccountId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.calendar.on_load_failed(&account_id.0, from, to);
        self.notify_error(error, window, cx);
    }

    pub(super) fn on_calendar_event_saved(
        &mut self,
        request_id: u64,
        updated: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_event_compose(request_id, cx);
        self.calendar.force_reload();
        self.toast(
            window,
            cx,
            Notification::success(if updated {
                tr!("toast-event-updated")
            } else {
                tr!("toast-event-created")
            }),
        );
    }

    pub(super) fn on_calendar_event_save_error(
        &mut self,
        request_id: u64,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.event_compose_error(request_id, error.clone(), cx);
        self.notify_error(error, window, cx);
    }

    pub(super) fn on_calendar_event_deleted(
        &mut self,
        account_id: AccountId,
        event_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.calendar
            .deleting
            .remove(&(account_id.clone(), event_id.clone()));
        self.calendar
            .events
            .retain(|event| event.account_id != account_id || event.id != event_id);
        if self.calendar.selected.as_deref() == Some(event_id.as_str()) {
            self.calendar.selected = None;
        }
        self.calendar.force_reload();
        self.toast(
            window,
            cx,
            Notification::success(tr!("toast-event-deleted")),
        );
    }

    pub(super) fn on_calendar_event_delete_error(
        &mut self,
        account_id: AccountId,
        event_id: String,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.calendar.deleting.remove(&(account_id, event_id));
        self.notify_error(error, window, cx);
    }

    pub(super) fn on_calendar_event_moved(
        &mut self,
        account_id: AccountId,
        event_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.calendar.moving.remove(&(account_id, event_id));
        self.calendar.force_reload();
        self.toast(window, cx, Notification::success(tr!("toast-event-moved")));
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn on_calendar_event_move_error(
        &mut self,
        account_id: AccountId,
        event_id: String,
        previous_start: DateTime<Utc>,
        previous_end: DateTime<Utc>,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.calendar
            .moving
            .remove(&(account_id.clone(), event_id.clone()));
        if let Some(event) = self
            .calendar
            .events
            .iter_mut()
            .find(|event| event.account_id == account_id && event.id == event_id)
        {
            event.start = previous_start;
            event.end = previous_end;
        }
        self.calendar.events.sort_by_key(|event| event.start);
        self.calendar.invalidate_event_layouts();
        self.notify_error(error, window, cx);
    }

    pub(super) fn on_ical_sync_state(
        &mut self,
        subscription_id: String,
        syncing: bool,
        error: Option<String>,
        last_success: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        // A failed sync forgets the feed's chunk marks so its months are
        // refetched (behind the scope's cooldown) instead of staying empty.
        if !syncing && error.is_some() {
            self.calendar
                .on_scope_failed(&format!("ical:{subscription_id}"));
        }
        self.calendar.ical_sync.insert(
            subscription_id,
            super::super::calendar_view::IcalSyncStatus {
                syncing,
                error,
                last_success,
            },
        );
    }

    /// A subscribed feed changed. Its events are merged per month chunk, so the
    /// whole chunk cache is dropped rather than patched — but only if the
    /// subscription is still one the user keeps.
    pub(super) fn on_ical_feed_updated(&mut self, subscription_id: String) {
        if self
            .settings
            .global
            .ical_subscriptions
            .iter()
            .any(|subscription| subscription.id == subscription_id)
        {
            self.calendar.force_reload();
        }
    }
}
