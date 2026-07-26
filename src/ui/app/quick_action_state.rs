//! Optimistic bookkeeping for quick actions.
//!
//! A quick action is a recipe of several steps handed to the outbox as one
//! execution. The UI applies its visible effects (tags, read/flagged state,
//! removal from the listing) as soon as it is scheduled, and keeps what it
//! took away in a `QuickActionOptimisticEffect` so a cancellation or a failure
//! part-way through can restore the message exactly.

use crate::model::{AccountId, Message, MessageHeader, MessageRef};
use crate::runtime::{QuickActionExecution, QuickActionStep};
use crate::ui::app::OptimisticMessageRemoval;
use crate::ui::app::{AviaryApp, QuickActionMessageSnapshot, QuickActionOptimisticEffect};
use crate::ui::state::{SenderHistoryState, ThreadBodyState};
use gpui::Context;

impl AviaryApp {
    /// Apply a quick action containing only reversible mailbox mutations
    /// immediately, while its durable runtime operation is still inside the
    /// undo window.
    pub(crate) fn begin_optimistic_quick_action(
        &mut self,
        account_id: &AccountId,
        execution: &QuickActionExecution,
        cx: &mut Context<Self>,
    ) {
        if execution.steps.iter().any(|step| {
            matches!(
                step,
                QuickActionStep::Forward { .. } | QuickActionStep::Reply { .. }
            )
        }) {
            return;
        }
        let reference = MessageRef {
            account_id: account_id.clone(),
            id: execution.message_id.clone(),
        };
        let Some(header) = self.quick_action_header_for_reference(&reference) else {
            return;
        };
        let changes_tags = execution.steps.iter().any(|step| {
            matches!(
                step,
                QuickActionStep::RemoveTag { .. } | QuickActionStep::AddTag { .. }
            )
        });
        let changes_read = execution
            .steps
            .iter()
            .any(|step| matches!(step, QuickActionStep::MarkRead { .. }));
        let changes_flagged = execution
            .steps
            .iter()
            .any(|step| matches!(step, QuickActionStep::SetFlag { .. }));
        let body_tags = changes_tags.then(|| {
            self.quick_action_body_tags(&reference)
                .unwrap_or_else(|| header.tags.clone())
        });
        let snapshot = QuickActionMessageSnapshot {
            tags: changes_tags.then(|| header.tags.clone()),
            body_tags,
            read: changes_read.then_some(header.is_read),
            flagged: changes_flagged.then_some(header.is_flagged),
        };
        let removal =
            self.apply_quick_action_steps_optimistically(&reference, &execution.steps, cx);
        self.quick_actions.effects.insert(
            execution.execution_id,
            QuickActionOptimisticEffect {
                reference,
                steps: execution.steps.clone(),
                snapshot,
                removal,
            },
        );
    }

    pub(crate) fn complete_optimistic_quick_action(&mut self, execution_id: u64) {
        self.quick_actions.effects.remove(&execution_id);
    }

    pub(crate) fn cancel_optimistic_quick_action(
        &mut self,
        execution_id: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(effect) = self.quick_actions.effects.remove(&execution_id) else {
            return;
        };
        self.restore_quick_action_effect(effect, cx);
    }

    pub(crate) fn fail_optimistic_quick_action(
        &mut self,
        execution_id: u64,
        completed_steps: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(effect) = self.quick_actions.effects.remove(&execution_id) else {
            return;
        };
        let completed = effect
            .steps
            .iter()
            .take(completed_steps)
            .cloned()
            .collect::<Vec<_>>();
        let reference = effect.reference.clone();
        self.restore_quick_action_effect(effect, cx);
        // Preserve mutations that the runtime checkpointed before the failed
        // step, while rolling back the failed step and everything after it.
        self.apply_quick_action_steps_optimistically(&reference, &completed, cx);
    }

    fn quick_action_header_for_reference(&self, reference: &MessageRef) -> Option<MessageHeader> {
        self.mailbox
            .messages
            .iter()
            .chain(self.mailbox.search.results.iter().flatten())
            .find(|header| header.account_id == reference.account_id && header.id == reference.id)
            .cloned()
            .or_else(|| match &self.sender_history {
                SenderHistoryState::Loaded { messages, .. } => messages
                    .iter()
                    .find(|header| {
                        header.account_id == reference.account_id && header.id == reference.id
                    })
                    .cloned(),
                _ => None,
            })
            .or_else(|| {
                self.displayed_message()
                    .filter(|message| {
                        message.header.account_id == reference.account_id
                            && message.header.id == reference.id
                    })
                    .map(|message| message.header.clone())
            })
    }

    fn quick_action_body_tags(&self, reference: &MessageRef) -> Option<Vec<String>> {
        self.displayed_message()
            .filter(|message| {
                message.header.account_id == reference.account_id
                    && message.header.id == reference.id
            })
            .map(|message| message.tags.clone())
    }

    fn quick_action_tag_key(&self, reference: &MessageRef, tag_id: &str) -> String {
        self.tags_by_account
            .get(&reference.account_id)
            .and_then(|tags| tags.iter().find(|tag| tag.id == tag_id))
            .map(|tag| {
                let provider = self
                    .account(&reference.account_id)
                    .map(|account| account.provider)
                    .unwrap_or_default();
                crate::ui::util::tag_storage_key(provider, tag)
            })
            .unwrap_or_else(|| tag_id.to_string())
    }

    fn update_quick_action_bodies(
        &mut self,
        reference: &MessageRef,
        mut update: impl FnMut(&mut Message),
    ) {
        if let Some(message) = self.mailbox.selected_mut().filter(|message| {
            message.header.account_id == reference.account_id && message.header.id == reference.id
        }) {
            update(message);
        }
        for tab in &mut self.mailbox.open_tabs {
            if let Some(message) = tab.message_mut().filter(|message| {
                message.header.account_id == reference.account_id
                    && message.header.id == reference.id
            }) {
                update(message);
            }
        }
        for state in self.mailbox.thread_bodies.values_mut() {
            if let ThreadBodyState::Loaded(message) = state {
                if message.header.account_id == reference.account_id
                    && message.header.id == reference.id
                {
                    update(message);
                }
            }
        }
    }

    fn apply_quick_action_steps_optimistically(
        &mut self,
        reference: &MessageRef,
        steps: &[QuickActionStep],
        cx: &mut Context<Self>,
    ) -> Option<OptimisticMessageRemoval> {
        for step in steps {
            match step {
                QuickActionStep::RemoveTag { tag_id } | QuickActionStep::AddTag { tag_id } => {
                    let added = matches!(step, QuickActionStep::AddTag { .. });
                    let key = self.quick_action_tag_key(reference, tag_id);
                    self.update_header_for(reference, |header| {
                        header.tags.retain(|tag| tag != &key);
                        if added {
                            header.tags.push(key.clone());
                        }
                    });
                    self.update_quick_action_bodies(reference, |message| {
                        message.header.tags.retain(|tag| tag != &key);
                        message.tags.retain(|tag| tag != &key);
                        if added {
                            message.header.tags.push(key.clone());
                            message.tags.push(key.clone());
                        }
                    });
                }
                QuickActionStep::MarkRead { read } => {
                    self.update_header_for(reference, |header| header.is_read = *read);
                    self.update_quick_action_bodies(reference, |message| {
                        message.header.is_read = *read;
                    });
                }
                QuickActionStep::SetFlag { flagged } => {
                    self.update_header_for(reference, |header| header.is_flagged = *flagged);
                    self.update_quick_action_bodies(reference, |message| {
                        message.header.is_flagged = *flagged;
                    });
                }
                QuickActionStep::Forward { .. }
                | QuickActionStep::Reply { .. }
                | QuickActionStep::Move { .. } => {}
            }
        }
        if steps
            .iter()
            .any(|step| matches!(step, QuickActionStep::Move { .. }))
        {
            Some(self.remove_quick_action_message_optimistically(reference, cx))
        } else {
            None
        }
    }

    fn remove_quick_action_message_optimistically(
        &mut self,
        reference: &MessageRef,
        cx: &mut Context<Self>,
    ) -> OptimisticMessageRemoval {
        let was_displayed = self.displayed_message().is_some_and(|message| {
            message.header.account_id == reference.account_id && message.header.id == reference.id
        });
        let neighbor = was_displayed
            .then(|| {
                self.message_neighbor_after_bulk_removal(reference, std::slice::from_ref(reference))
            })
            .flatten();
        let removal = self.remove_message_optimistically_ref(reference);
        if was_displayed {
            if let Some(message) = neighbor {
                self.open_message(message.account_id, message.id, cx);
            } else {
                self.cancel_pending_message_open(cx);
            }
        }
        removal
    }

    fn restore_quick_action_effect(
        &mut self,
        effect: QuickActionOptimisticEffect,
        cx: &mut Context<Self>,
    ) {
        if let Some(removal) = effect.removal {
            self.restore_optimistic_message(removal);
        }
        let reference = effect.reference;
        let tags = effect.snapshot.tags;
        let body_tags = effect.snapshot.body_tags;
        let read = effect.snapshot.read;
        let flagged = effect.snapshot.flagged;
        self.update_header_for(&reference, |header| {
            if let Some(tags) = &tags {
                header.tags.clone_from(tags);
            }
            if let Some(read) = read {
                header.is_read = read;
            }
            if let Some(flagged) = flagged {
                header.is_flagged = flagged;
            }
        });
        self.update_quick_action_bodies(&reference, |message| {
            if let Some(tags) = &tags {
                message.header.tags.clone_from(tags);
            }
            if let Some(body_tags) = &body_tags {
                message.tags.clone_from(body_tags);
            }
            if let Some(read) = read {
                message.header.is_read = read;
            }
            if let Some(flagged) = flagged {
                message.header.is_flagged = flagged;
            }
        });
        self.update_tray_unread();
        cx.notify();
    }
}
