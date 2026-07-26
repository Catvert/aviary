//! Reduces authentication and account lifecycle events.

use super::super::app::AviaryApp;
use super::super::state::{AuthState, MainView};
use crate::model::{Account, AccountId, Provider};
use crate::runtime::Cmd;
use gpui::{Context, Window};

impl AviaryApp {
    pub(super) fn on_device_code(
        &mut self,
        user_code: String,
        verification_uri: String,
        message: String,
    ) {
        self.auth = AuthState::AwaitingCode {
            user_code,
            verification_uri,
        };
        log::info!("{message}");
    }

    pub(super) fn on_google_auth_opening(&mut self, auth_url: String) {
        self.auth = AuthState::AwaitingGoogle { auth_url };
    }

    pub(super) fn on_account_ready(
        &mut self,
        account: Account,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let account_id = account.id.clone();
        self.unavailable_accounts.remove(&account_id);
        if let Some(existing) = self
            .accounts
            .iter_mut()
            .find(|existing| existing.id == account_id)
        {
            *existing = account;
        } else {
            self.accounts.push(account);
            let order = &mut self.settings.global.account_order;
            if !order.contains(&account_id.0) {
                order.push(account_id.0.clone());
            }
            let order = order.clone();
            self.accounts
                .sort_by_key(|account| order.iter().position(|id| id == &account.id.0));
        }
        self.settings_ui = None;
        self.auth = AuthState::Authenticated;
        let wanted = self.settings.global.last_account_id.as_deref();
        if self.current_account_id.is_none() || wanted == Some(account_id.0.as_str()) {
            self.set_current_account_context(Some(account_id.clone()));
            self.settings.save();
        }
        self.calendar.force_reload();
        self.restore_pending_event_composes(window, cx);
        self.refresh_visible_contacts();
        self.rehydrate_restored_messages(&account_id);
        // Each connected account owns an independent Inbox supervisor. Start
        // it even when the restored application view is Calendar or Settings.
        self.mailbox.last_auto_refresh_sent = None;
        self.sync_auto_refresh();
        if self.view == MainView::Mail {
            // At startup `load_mail_view` runs before any account is ready,
            // so its tag preload finds nobody: (re)load the registry here,
            // otherwise mailbox rows show no tag chips until a message is
            // opened.
            self.ensure_tags_loaded(&account_id);
            self.send(Cmd::LoadFolders {
                account_id: account_id.clone(),
            });
            let account_is_visible = self.mailbox.unified_selected_account.as_ref().map_or_else(
                || self.unified_account_included(&account_id),
                |selected| selected == &account_id,
            );
            if account_is_visible {
                if self.mailbox.search.query.is_empty() {
                    self.request_mailbox_refresh();
                } else {
                    self.send(Cmd::Search {
                        account_id: account_id.clone(),
                        query: self.mailbox.search.query.clone(),
                        scope: self.mail_search_scope(),
                        limit: self.fetch_limit(&account_id),
                    });
                }
            }
        }
    }

    pub(super) fn on_account_restore_failed(
        &mut self,
        account_id: AccountId,
        provider: Provider,
        error: String,
    ) {
        log::warn!("account {account_id} unavailable: {error}");
        self.unavailable_accounts
            .insert(account_id, (provider, error));
    }

    pub(super) fn on_logged_out(&mut self, account_id: AccountId, cx: &mut Context<Self>) {
        let selected_belongs_to_account = self
            .mailbox
            .selected
            .as_ref()
            .is_some_and(|message| message.header.account_id == account_id)
            || self
                .mailbox
                .selected_id
                .as_ref()
                .is_some_and(|selected_id| {
                    self.mailbox.messages.iter().any(|message| {
                        message.id == *selected_id && message.account_id == account_id
                    }) || self
                        .mailbox
                        .search
                        .results
                        .as_ref()
                        .is_some_and(|messages| {
                            messages.iter().any(|message| {
                                message.id == *selected_id && message.account_id == account_id
                            })
                        })
                });

        let mut compose_ids: std::collections::HashSet<u64> = self
            .composes
            .iter()
            .filter_map(|handle| {
                let view = handle.view.upgrade()?;
                view.read(cx)
                    .belongs_to_account(&account_id)
                    .then_some(handle.id)
            })
            .collect();
        compose_ids.extend(self.inline_composes.iter().filter_map(|compose| {
            compose
                .view
                .read(cx)
                .belongs_to_account(&account_id)
                .then_some(compose.id)
        }));

        let mut remaining_composes = Vec::with_capacity(self.composes.len());
        for handle in self.composes.drain(..) {
            if compose_ids.contains(&handle.id) {
                if let Some(compose_window) = handle.window {
                    let _ = compose_window.update(cx, |_, window, _| window.remove_window());
                }
            } else {
                remaining_composes.push(handle);
            }
        }
        self.composes = remaining_composes;
        self.inline_composes
            .retain(|compose| !compose_ids.contains(&compose.id));

        let tab_count = self.mailbox.open_tabs.len();
        self.mailbox.open_tabs.retain(|tab| match tab {
            super::super::state::ViewerTab::Message(message) => {
                message.header.account_id != account_id
            }
            super::super::state::ViewerTab::Loading(reference) => {
                reference.account_id != account_id
            }
            super::super::state::ViewerTab::Compose(compose_id) => {
                !compose_ids.contains(compose_id)
            }
        });
        if self.mailbox.open_tabs.len() != tab_count {
            self.mailbox.active_tab = None;
            self.viewer_translation.result = None;
            self.sender_history = super::super::state::SenderHistoryState::Idle;
        }
        if selected_belongs_to_account {
            self.mailbox.selected = None;
            self.mailbox.selected_id = None;
            self.mailbox.thread = None;
            self.mailbox.thread_bodies.clear();
            self.viewer_translation.result = None;
            self.sender_history = super::super::state::SenderHistoryState::Idle;
        }
        self.mailbox.sent_messages.retain(|_, messages| {
            messages.retain(|message| message.message.header.account_id != account_id);
            !messages.is_empty()
        });
        let remaining_sent_ids: std::collections::HashSet<_> = self
            .mailbox
            .sent_messages
            .values()
            .flatten()
            .map(|message| message.message.header.id.clone())
            .collect();
        self.mailbox
            .expanded_sent_messages
            .retain(|id| remaining_sent_ids.contains(id));
        let orphaned_panels: Vec<u64> = [
            self.inline_reply.as_ref(),
            self.pending_inline_reply.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter(|reply| AviaryApp::reply_panel_belongs_to(reply, &account_id, cx))
        .map(|reply| reply.compose_id)
        .collect();
        for compose_id in orphaned_panels {
            self.close_compose(compose_id, cx);
        }
        if !compose_ids.is_empty() || self.mailbox.open_tabs.len() != tab_count {
            self.session_dirty = true;
        }

        self.accounts.retain(|account| account.id != account_id);
        self.settings_ui = None;
        self.unavailable_accounts.remove(&account_id);
        self.contacts.by_account.remove(&account_id);
        self.contacts.loading_accounts.remove(&account_id);
        self.discard_pending_rehydrate(&account_id);
        self.tags_by_account.remove(&account_id);
        self.tags_loading.remove(&account_id);
        self.kanban.remove_account(&account_id);
        self.mailbox.folders_by_account.remove(&account_id);
        self.mailbox
            .messages
            .retain(|message| message.account_id != account_id);
        if let Some(results) = &mut self.mailbox.search.results {
            results.retain(|message| message.account_id != account_id);
        }
        if self.mailbox.unified_selected_account.as_ref() == Some(&account_id) {
            self.mailbox.unified_selected_account = None;
            self.mailbox.selected_folder_id = None;
        }
        if self.current_account_id.as_ref() == Some(&account_id) {
            let next = self.accounts.first().map(|account| account.id.clone());
            self.set_current_account_context(next);
            self.settings.save();
        }
        if self.settings.global.default_account_id.as_deref() == Some(account_id.0.as_str()) {
            self.settings.global.default_account_id =
                self.accounts.first().map(|account| account.id.0.clone());
            self.settings.save();
        }
        if self.view == MainView::Mail && !self.accounts.is_empty() {
            self.request_mailbox_refresh();
        }
        if self.accounts.is_empty() && self.unavailable_accounts.is_empty() {
            self.auth = AuthState::Idle;
        }
        self.refresh_sender_history_for_displayed();
        self.update_tray_unread();
    }
}
