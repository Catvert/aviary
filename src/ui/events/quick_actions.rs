//! Progress of a running quick action.
//!
//! A quick action is a recipe of several steps handed to the outbox as one
//! execution, whose visible effects the UI already applied. These reducers
//! reconcile that optimistic state with what actually happened, and — when it
//! stopped part-way — offer to resume from the step that failed.
//!
//! Every notification carries the same `("quick-action", execution_id)` key, so
//! the started/failed/completed messages of one execution replace each other
//! instead of stacking up.

use super::super::app::{compact_error, AviaryApp};
use super::super::quick_actions::QuickActionNotification;
use crate::model::AccountId;
use crate::runtime::QuickActionExecution;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::notification::Notification;
use gpui_kit::{prelude::FluentBuilder, Context, Window};

impl AviaryApp {
    pub(super) fn on_quick_action_completed(
        &mut self,
        execution_id: u64,
        action_name: String,
        message_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::debug!("quick action {execution_id} completed for {message_id}");
        self.complete_optimistic_quick_action(execution_id);
        self.toast(
            window,
            cx,
            Notification::success(tr!("quick-actions-completed", { name: action_name }))
                .id1::<QuickActionNotification>(("quick-action", execution_id)),
        );
    }

    /// The undo window elapsed and the execution left for the provider: the
    /// optimistic effect is no longer cancellable, but it is kept until the
    /// execution ends — `complete_optimistic_quick_action` drops it on
    /// success, `fail_optimistic_quick_action` still needs it to roll back
    /// the steps that did not go through.
    pub(super) fn on_quick_action_started(
        &mut self,
        execution_id: u64,
        action_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toast(
            window,
            cx,
            Notification::info(tr!("quick-actions-started", { name: action_name }))
                .id1::<QuickActionNotification>(("quick-action", execution_id)),
        );
    }

    /// The durable row was removed before it came due: nothing reached the
    /// provider, so the optimistic mail-view change is put back.
    pub(super) fn on_quick_action_cancelled(
        &mut self,
        execution_id: u64,
        action_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_optimistic_quick_action(execution_id, cx);
        self.toast(
            window,
            cx,
            Notification::success(tr!("quick-actions-cancelled", { name: action_name }))
                .id1::<QuickActionNotification>(("quick-action", execution_id)),
        );
    }

    /// A step failed. `remaining` holds what was not run, so the notification
    /// can offer to pick the recipe back up where it stopped.
    pub(super) fn on_quick_action_failed(
        &mut self,
        account_id: AccountId,
        remaining: QuickActionExecution,
        completed_steps: usize,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let execution_id = remaining.execution_id;
        let action_name = remaining.action_name.clone();
        let message_id = remaining.message_id.clone();
        log::error!(
            "quick action {execution_id} for {message_id} failed after {completed_steps} steps"
        );
        self.fail_optimistic_quick_action(execution_id, completed_steps, cx);
        let app = cx.entity();
        self.toast(
            window,
            cx,
            Notification::error(tr!("quick-actions-failed", {
                name: action_name,
                error: compact_error(&error)
            }))
            .id1::<QuickActionNotification>(("quick-action", execution_id))
            .autohide(false)
            .action(move |_, _, _| {
                let app = app.clone();
                let account_id = account_id.clone();
                let remaining = remaining.clone();
                Button::new("retry-quick-action")
                    .ghost()
                    .label(tr!("quick-actions-retry-remaining"))
                    .on_click(move |_, window, cx| {
                        app.update(cx, |this, cx| {
                            this.retry_quick_action(
                                account_id.clone(),
                                remaining.clone(),
                                window,
                                cx,
                            );
                        });
                    })
            }),
        );
    }

    /// The process died while a send of this recipe was in flight. The provider
    /// may have delivered it, so nothing is replayed on its own: the user is
    /// told, and only the steps after the send can be resumed.
    pub(super) fn on_quick_action_send_uncertain(
        &mut self,
        account_id: AccountId,
        remaining: QuickActionExecution,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let execution_id = remaining.execution_id;
        let action_name = remaining.action_name.clone();
        let message_id = remaining.message_id.clone();
        log::error!("quick action {execution_id} for {message_id} has an uncertain send result");
        let can_continue = !remaining.steps.is_empty();
        let app = cx.entity();
        self.toast(
            window,
            cx,
            Notification::warning(tr!("quick-actions-send-uncertain", { name: action_name }))
                .id1::<QuickActionNotification>(("quick-action", execution_id))
                .autohide(false)
                .when(can_continue, |notification| {
                    notification.action(move |_, _, _| {
                        let app = app.clone();
                        let account_id = account_id.clone();
                        let remaining = remaining.clone();
                        Button::new("continue-quick-action-triage")
                            .ghost()
                            .label(tr!("quick-actions-continue-triage"))
                            .on_click(move |_, window, cx| {
                                app.update(cx, |this, cx| {
                                    this.retry_quick_action(
                                        account_id.clone(),
                                        remaining.clone(),
                                        window,
                                        cx,
                                    );
                                });
                            })
                    })
                }),
        );
    }

    /// A step changed read or flagged state provider-side. Only the fields the
    /// step touched are carried, so `None` means "left alone", not "false".
    pub(super) fn on_quick_action_message_state(
        &mut self,
        account_id: AccountId,
        message_id: String,
        read: Option<bool>,
        flagged: Option<bool>,
    ) {
        let reference = crate::model::MessageRef {
            account_id,
            id: message_id,
        };
        self.update_header_for(&reference, |header| {
            if let Some(read) = read {
                header.is_read = read;
            }
            if let Some(flagged) = flagged {
                header.is_flagged = flagged;
            }
        });
        if let Some(message) = self.mailbox.selected_mut().filter(|message| {
            message.header.account_id == reference.account_id && message.header.id == reference.id
        }) {
            if let Some(read) = read {
                message.header.is_read = read;
            }
            if let Some(flagged) = flagged {
                message.header.is_flagged = flagged;
            }
        }
    }
}
