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
use gpui_kit::Context;

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
        let mut snapshots = Vec::new();
        for target in step_targets(&reference, &execution.steps) {
            let Some(header) = self.quick_action_header_for_reference(&target) else {
                continue;
            };
            let steps = || {
                execution
                    .steps
                    .iter()
                    .filter(|step| step.target(&reference.id) == target.id)
            };
            let changes_tags = steps().any(|step| {
                matches!(
                    step,
                    QuickActionStep::RemoveTag { .. } | QuickActionStep::AddTag { .. }
                )
            });
            let changes_read = steps().any(|step| matches!(step, QuickActionStep::MarkRead { .. }));
            let changes_flagged =
                steps().any(|step| matches!(step, QuickActionStep::SetFlag { .. }));
            let body_tags = changes_tags.then(|| {
                self.quick_action_body_tags(&target)
                    .unwrap_or_else(|| header.tags.clone())
            });
            let snapshot = QuickActionMessageSnapshot {
                tags: changes_tags.then(|| header.tags.clone()),
                body_tags,
                read: changes_read.then_some(header.is_read),
                flagged: changes_flagged.then_some(header.is_flagged),
            };
            snapshots.push((target, snapshot));
        }
        // Nothing on screen to act on: a retry of the steps left over after
        // every message they name was already moved away.
        if snapshots.is_empty() {
            return;
        }
        let removals =
            self.apply_quick_action_steps_optimistically(&reference, &execution.steps, cx);
        self.quick_actions.effects.insert(
            execution.execution_id,
            QuickActionOptimisticEffect {
                reference,
                steps: execution.steps.clone(),
                snapshots,
                removals,
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

    /// `reference` is the execution's message; a step naming another member
    /// of the thread applies to that one instead.
    fn apply_quick_action_steps_optimistically(
        &mut self,
        execution_reference: &MessageRef,
        steps: &[QuickActionStep],
        cx: &mut Context<Self>,
    ) -> Vec<OptimisticMessageRemoval> {
        let mut moved = Vec::new();
        for step in steps {
            let target = MessageRef {
                account_id: execution_reference.account_id.clone(),
                id: step.target(&execution_reference.id).to_string(),
            };
            let reference = &target;
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
                QuickActionStep::MarkRead { read, .. } => {
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
                QuickActionStep::Move { .. } => {
                    if !moved.contains(&target) {
                        moved.push(target);
                    }
                }
                QuickActionStep::Forward { .. } | QuickActionStep::Reply { .. } => {}
            }
        }
        if moved.is_empty() {
            Vec::new()
        } else {
            self.remove_quick_action_messages_optimistically(&moved, cx)
        }
    }

    /// Removes every moved message at once, so that the reader moves on to a
    /// neighbor outside the thread rather than to another of its members.
    fn remove_quick_action_messages_optimistically(
        &mut self,
        references: &[MessageRef],
        cx: &mut Context<Self>,
    ) -> Vec<OptimisticMessageRemoval> {
        let displayed = self.displayed_message().and_then(|message| {
            references
                .iter()
                .find(|reference| {
                    message.header.account_id == reference.account_id
                        && message.header.id == reference.id
                })
                .cloned()
        });
        let neighbor = displayed
            .as_ref()
            .and_then(|current| self.message_neighbor_after_bulk_removal(current, references));
        let removals = references
            .iter()
            .map(|reference| self.remove_message_optimistically_ref(reference))
            .collect();
        if displayed.is_some() {
            if let Some(message) = neighbor {
                self.open_message(message.account_id, message.id, cx);
            } else {
                self.cancel_pending_message_open(cx);
            }
        }
        removals
    }

    fn restore_quick_action_effect(
        &mut self,
        effect: QuickActionOptimisticEffect,
        cx: &mut Context<Self>,
    ) {
        for removal in effect.removals.into_iter().rev() {
            self.restore_optimistic_message(removal);
        }
        for (reference, snapshot) in effect.snapshots {
            self.restore_quick_action_snapshot(&reference, snapshot);
        }
        self.update_tray_unread();
        cx.notify();
    }

    fn restore_quick_action_snapshot(
        &mut self,
        reference: &MessageRef,
        snapshot: QuickActionMessageSnapshot,
    ) {
        let QuickActionMessageSnapshot {
            tags,
            body_tags,
            read,
            flagged,
        } = snapshot;
        self.update_header_for(reference, |header| {
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
        self.update_quick_action_bodies(reference, |message| {
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
    }
}

/// The messages `steps` act on, the execution's own first, each once.
fn step_targets(reference: &MessageRef, steps: &[QuickActionStep]) -> Vec<MessageRef> {
    let mut targets = vec![reference.clone()];
    for step in steps {
        let id = step.target(&reference.id);
        if !targets.iter().any(|target| target.id == id) {
            targets.push(MessageRef {
                account_id: reference.account_id.clone(),
                id: id.to_string(),
            });
        }
    }
    targets
}
