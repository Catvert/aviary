//! Working-session persistence: the snapshot the session actor writes to
//! `session.json`, and its counterpart on startup.
//!
//! Persisted messages are thinned (`slim_message`) because their inline images
//! and attachment bytes already live in the SQLite cache; they are rehydrated
//! through `Cmd::LoadThreadMessage` once their account is ready.

use crate::model::{AccountId, Message, MessageRef, SentMessage};
use crate::runtime::Cmd;
use crate::ui::app::{AviaryApp, PendingSentRestore, SESSION_SENT_SNAPSHOT_CAP};
use crate::ui::settings::{
    AppSession, CalendarSession, ContactsSession, MailboxSession, SentMessageSession,
    SessionViewerTab,
};
use crate::ui::state::{ContactsState, MailboxState, MainView, SenderHistoryState};
use gpui::{div, prelude::*, App, Context, Window};
use gpui_component::{resizable::ResizableState, WindowExt};
use std::collections::{HashMap, HashSet};

/// Lightweight session copy of reply/forward cards. Their message bodies and
/// binary payloads are cached in SQLite and only stable ids remain in JSON.
fn session_sent_messages(
    sent: &HashMap<String, Vec<SentMessage>>,
) -> HashMap<String, Vec<SentMessageSession>> {
    let mut all: Vec<(&String, &SentMessage)> = sent
        .iter()
        .flat_map(|(related_to, snapshots)| {
            snapshots.iter().map(move |snapshot| (related_to, snapshot))
        })
        .collect();
    all.sort_by_key(|(_, snapshot)| std::cmp::Reverse(snapshot.message.header.received));
    all.truncate(SESSION_SENT_SNAPSHOT_CAP);
    let mut references: HashMap<String, Vec<SentMessageSession>> = HashMap::new();
    // `rev()` restores chronological order within each key.
    for (related_to, snapshot) in all.into_iter().rev() {
        references
            .entry(related_to.clone())
            .or_default()
            .push(SentMessageSession {
                action: snapshot.action,
                message: MessageRef::from(snapshot.message.as_ref()),
                sent_id: snapshot.sent_id.clone(),
                internet_message_id: snapshot.internet_message_id.clone(),
            });
    }
    references
}

impl AviaryApp {
    fn session_snapshot(&self, cx: &App) -> AppSession {
        let mut tabs = Vec::with_capacity(self.mailbox.open_tabs.len());
        let mut persisted_active_tab = None;
        let mut inline_compose_ids = HashSet::new();
        for (index, tab) in self.mailbox.open_tabs.iter().enumerate() {
            let persisted = match tab {
                crate::ui::state::ViewerTab::Message(message) => Some(SessionViewerTab::Message(
                    MessageRef::from(message.as_ref()),
                )),
                crate::ui::state::ViewerTab::Loading(reference) => {
                    Some(SessionViewerTab::Message(reference.clone()))
                }
                crate::ui::state::ViewerTab::Compose(id) => {
                    inline_compose_ids.insert(*id);
                    self.inline_composes
                        .iter()
                        .find(|compose| compose.id == *id)
                        .map(|compose| {
                            SessionViewerTab::Compose(Box::new(compose.view.read(cx).to_init(cx)))
                        })
                }
            };
            if let Some(persisted) = persisted {
                if self.mailbox.active_tab == Some(index) {
                    persisted_active_tab = Some(tabs.len());
                }
                tabs.push(persisted);
            }
        }

        // A composer can temporarily have no tab during delayed-send undo or
        // while its durable outbox entry is in flight. Persist both states:
        // `ComposeInit::pending_send` keeps the latter read-only and its stable
        // id lets the eventual acknowledgement close the restored editor.
        for compose in &self.inline_composes {
            if inline_compose_ids.contains(&compose.id) {
                continue;
            }
            tabs.push(SessionViewerTab::Compose(Box::new(
                compose.view.read(cx).to_init(cx),
            )));
        }

        let detached_composes = self
            .composes
            .iter()
            .filter(|handle| handle.window.is_some())
            .filter_map(|handle| handle.view.upgrade())
            .map(|view| view.read(cx).to_init(cx))
            .collect();

        let settings_tab = self
            .settings_ui
            .as_ref()
            .map(|ui| ui.tab)
            .unwrap_or(self.last_settings_tab);
        let mut sent_messages = session_sent_messages(&self.mailbox.sent_messages);
        for pending in &self.pending_sent_restore {
            let entries = sent_messages.entry(pending.related_to.clone()).or_default();
            let position = pending.position.min(entries.len());
            entries.insert(position, pending.session.clone());
        }

        AppSession {
            main_view: self.view,
            mailbox: MailboxSession {
                selected_folder_id: self.mailbox.selected_folder_id.clone(),
                unified_selected_account: self.mailbox.unified_selected_account.clone(),
                selected_message: self
                    .mailbox
                    .selected
                    .as_deref()
                    .map(MessageRef::from)
                    .or_else(|| {
                        self.mailbox.selected_id.as_ref().and_then(|id| {
                            self.pending_rehydrate
                                .iter()
                                .find(|reference| &reference.id == id)
                                .cloned()
                        })
                    }),
                search_query: self.search_input.read(cx).value().to_string(),
                search_history: self.mailbox.search.history.clone(),
                search_scope: self.mailbox.search.scope,
                search_sort: self.mailbox.search.sort,
                show_flagged_only: self.mailbox.show_flagged_only,
                tag_filters: self.mailbox.tag_filters.clone(),
                expanded_quoted_sections: self.mailbox.expanded_quoted_sections.clone(),
                sent_messages,
                expanded_sent_messages: self.mailbox.expanded_sent_messages.clone(),
                collapsed_message_sections: self.mailbox.collapsed_message_sections.clone(),
                expanded_conversations: self.mailbox.expanded_conversations.clone(),
                tabs,
                active_tab: persisted_active_tab,
            },
            sender_history_expanded: self.sender_history_expanded,
            contacts: ContactsSession {
                selected: self.contacts.selected.clone(),
                query: self.contacts.query.clone(),
            },
            calendar: CalendarSession {
                range: self.calendar.range,
                anchor: self.calendar.anchor,
                selected: self.calendar.selected.clone(),
            },
            kanban_preview: self.kanban.preview.clone(),
            settings_tab,
            inline_reply: self.inline_reply_session(cx),
            detached_composes,
            event_composes: self.event_compose_sessions(cx),
        }
    }

    pub(crate) fn persist_session(&mut self, cx: &App) {
        if let Some(ui) = &self.settings_ui {
            self.last_settings_tab = ui.tab;
        }
        self.session_store.save(self.session_snapshot(cx));
        self.session_dirty = false;
    }

    /// Clears all state that could otherwise be captured again immediately
    /// after a factory reset, including drafts living in detached windows.
    pub(crate) fn reset_working_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for handle in std::mem::take(&mut self.composes) {
            if let Some(compose_window) = handle.window {
                let _ = compose_window.update(cx, |_, window, _| window.remove_window());
            }
        }
        self.inline_composes.clear();
        self.inline_reply = None;
        self.pending_inline_reply = None;
        self.viewer_translation.result = None;
        self.viewer_translation.open = false;
        self.pending_kanban_open = None;
        self.pending_notification_open = None;
        self.pending_reply_id = None;
        self.pending_forward_id = None;

        for handle in std::mem::take(&mut self.calendar.composes) {
            if let Some(event_window) = handle.window {
                let _ = event_window.update(cx, |_, window, _| window.remove_window());
            }
        }
        self.pending_event_composes.clear();
        self.mailbox = MailboxState::default();
        self.sender_history = SenderHistoryState::default();
        self.sender_history_expanded = false;
        self.contacts = ContactsState::default();
        self.calendar =
            crate::ui::calendar_view::CalendarViewState::new(self.settings.global.calendar_layout);
        self.kanban = crate::ui::kanban_view::BoardState::default();
        self.view = MainView::Mail;
        self.scrolls = crate::ui::app::Scrolls::default();
        self.mail_search_scroll =
            crate::ui::components::overlay_popover::OverlayPopoverScroll::default();
        self.viewer_layout_width = None;
        self.sidebar_resize = cx.new(|_| ResizableState::default());
        self.inbox_resize_h = cx.new(|_| ResizableState::default());
        self.inbox_resize_v = cx.new(|_| ResizableState::default());
        self.settings_ui = None;
        self.imap_form = None;
        self.folder_dialog_input = None;
        self.search_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.contacts_search_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.session_store.save(AppSession::default());
        self.session_dirty = false;
        self.last_settings_tab = Default::default();
        self.pending_rehydrate.clear();
        self.pending_sent_restore.clear();
    }

    pub(crate) fn confirm_reset_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entity = cx.entity();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let entity = entity.clone();
            dialog
                .title(tr!("reset-session-title"))
                .confirm()
                .child(div().child(tr!("reset-session-confirm")))
                .on_ok(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        this.reset_working_session(window, cx);
                        this.load_mail_view();
                        this.settings.save();
                        cx.notify();
                    });
                    true
                })
        });
    }

    pub(super) fn restore_session_editors(
        &mut self,
        session: AppSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let wanted_view = session.main_view;
        let selected_message = session.mailbox.selected_message.clone();
        for tab in session.mailbox.tabs {
            match tab {
                SessionViewerTab::Message(reference) => self
                    .mailbox
                    .open_tabs
                    .push(crate::ui::state::ViewerTab::Loading(reference)),
                SessionViewerTab::Compose(init) => self.open_inline_compose(*init, window, cx),
            }
        }
        self.mailbox.active_tab = session
            .mailbox
            .active_tab
            .filter(|index| *index < self.mailbox.open_tabs.len());
        self.restore_inline_reply_session(session.inline_reply, window, cx);
        self.view = wanted_view;

        // The JSON session contains only identities. Complete records are
        // loaded from SQLite immediately; cache misses wait for the provider
        // account to become ready below.
        let mut rehydrate = Vec::new();
        if let Some(reference) = selected_message {
            rehydrate.push(reference);
        }
        for tab in &self.mailbox.open_tabs {
            if let crate::ui::state::ViewerTab::Loading(reference) = tab {
                rehydrate.push(reference.clone());
            }
        }
        self.pending_sent_restore = session
            .mailbox
            .sent_messages
            .into_iter()
            .flat_map(|(related_to, messages)| {
                messages
                    .into_iter()
                    .enumerate()
                    .map(move |(position, session)| PendingSentRestore {
                        related_to: related_to.clone(),
                        position,
                        session,
                    })
            })
            .collect();
        rehydrate.extend(
            self.pending_sent_restore
                .iter()
                .map(|pending| pending.session.message.clone()),
        );
        let mut seen = HashSet::new();
        rehydrate.retain(|reference| seen.insert(reference.clone()));
        self.pending_rehydrate = rehydrate;
        for reference in &self.pending_rehydrate {
            self.send(Cmd::LoadCachedMessage {
                account_id: reference.account_id.clone(),
                id: reference.id.clone(),
            });
        }

        if !session.detached_composes.is_empty() {
            let detached = session.detached_composes;
            cx.on_next_frame(window, move |this, window, cx| {
                for init in detached {
                    this.open_compose_window(init, window, cx);
                }
            });
        }
    }

    /// Sends a cache-first fetch (`LoadThreadMessage`) for every referenced
    /// session message belonging to an account that just became ready.
    pub(crate) fn rehydrate_restored_messages(&mut self, account_id: &AccountId) {
        let ids: Vec<_> = self
            .pending_rehydrate
            .iter()
            .filter(|reference| &reference.account_id == account_id)
            .map(|reference| reference.id.clone())
            .collect();
        for id in ids {
            self.send(Cmd::LoadThreadMessage {
                account_id: account_id.clone(),
                id,
            });
        }
    }

    /// Completes any session references satisfied by a cache/provider result.
    pub(crate) fn finish_session_rehydrate(&mut self, message: &Message) {
        let reference = MessageRef::from(message);
        self.pending_rehydrate
            .retain(|pending| pending != &reference);

        let mut remaining = Vec::with_capacity(self.pending_sent_restore.len());
        for pending in self.pending_sent_restore.drain(..) {
            if pending.session.message == reference {
                let restored = SentMessage {
                    related_to: pending.related_to.clone(),
                    action: pending.session.action,
                    message: Box::new(message.clone()),
                    sent_id: pending.session.sent_id,
                    internet_message_id: pending.session.internet_message_id,
                };
                let entries = self
                    .mailbox
                    .sent_messages
                    .entry(pending.related_to)
                    .or_default();
                entries.insert(pending.position.min(entries.len()), restored);
            } else {
                remaining.push(pending);
            }
        }
        self.pending_sent_restore = remaining;
    }

    pub(crate) fn awaits_session_message(&self, account_id: &AccountId, id: &str) -> bool {
        self.pending_rehydrate
            .iter()
            .any(|reference| &reference.account_id == account_id && reference.id == id)
    }

    pub(crate) fn discard_pending_rehydrate(&mut self, account_id: &AccountId) {
        self.pending_rehydrate
            .retain(|reference| &reference.account_id != account_id);
        self.pending_sent_restore
            .retain(|pending| &pending.session.message.account_id != account_id);
    }
}
