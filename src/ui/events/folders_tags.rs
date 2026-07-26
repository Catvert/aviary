//! Reduces folder, move, and tag events.

use super::super::app::{AviaryApp, BulkReply};
use super::super::state::{SenderHistoryState, ThreadBodyState};
use super::super::util;
use crate::model::{AccountId, MailFolder, Message, MessageHeader, Tag};
use crate::runtime::{Cmd, MessageMutationKind};
use gpui::{Context, Window};
use gpui_component::notification::Notification;

impl AviaryApp {
    pub(super) fn on_folders(&mut self, account_id: AccountId, folders: Vec<MailFolder>) {
        self.mailbox
            .folders_by_account
            .insert(account_id.clone(), folders.clone());
        if self.current_account_id.as_ref() == Some(&account_id) {
            self.mailbox.folders = folders;
        }
    }

    pub(super) fn on_folder_created(
        &mut self,
        account_id: AccountId,
        folder: MailFolder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mailbox
            .folders_by_account
            .entry(account_id.clone())
            .or_default()
            .push(folder.clone());
        if self.current_account_id.as_ref() == Some(&account_id) {
            self.mailbox.folders.push(folder);
        }
        self.toast(
            window,
            cx,
            Notification::success(tr!("toast-folder-created")),
        );
        self.send(Cmd::LoadFolders { account_id });
    }

    pub(super) fn on_folder_renamed(
        &mut self,
        account_id: AccountId,
        id: String,
        new_id: Option<String>,
        new_name: String,
    ) {
        let effective_id = new_id.as_deref().unwrap_or(&id).to_string();
        let renamed_was_selected = self.mailbox.selected_folder_id.as_deref() == Some(id.as_str())
            && self.mailbox.unified_selected_account.as_ref() == Some(&account_id);
        if let Some(folders) = self.mailbox.folders_by_account.get_mut(&account_id) {
            if let Some(folder) = folders.iter_mut().find(|folder| folder.id == id) {
                folder.display_name = new_name.clone();
                folder.id = effective_id.clone();
            }
            for folder in folders {
                if folder.parent_id.as_deref() == Some(id.as_str()) {
                    folder.parent_id = Some(effective_id.clone());
                }
            }
        }
        if self.current_account_id.as_ref() == Some(&account_id) {
            if let Some(folder) = self
                .mailbox
                .folders
                .iter_mut()
                .find(|folder| folder.id == id)
            {
                folder.display_name = new_name;
                folder.id = effective_id.clone();
            }
            for folder in &mut self.mailbox.folders {
                if folder.parent_id.as_deref() == Some(id.as_str()) {
                    folder.parent_id = Some(effective_id.clone());
                }
            }
        }
        let settings = self.settings.account_mut(&account_id);
        for folder_id in &mut settings.pinned_folder_ids {
            if folder_id == &id {
                *folder_id = effective_id.clone();
            }
        }
        for folder_id in &mut settings.expanded_folder_ids {
            if folder_id == &id {
                *folder_id = effective_id.clone();
            }
        }
        for action in &mut settings.quick_actions {
            if action.move_to_folder_id.as_deref() == Some(id.as_str()) {
                action.move_to_folder_id = Some(effective_id.clone());
            }
        }
        if renamed_was_selected {
            self.mailbox.selected_folder_id = Some(effective_id);
            self.mailbox.last_auto_refresh_sent = None;
        }
        self.settings.save();
        self.send(Cmd::LoadFolders {
            account_id: account_id.clone(),
        });
        if renamed_was_selected {
            self.send_refresh();
            self.sync_auto_refresh();
        }
    }

    pub(super) fn on_folder_deleted(&mut self, account_id: AccountId, id: String) {
        if let Some(folders) = self.mailbox.folders_by_account.get_mut(&account_id) {
            folders.retain(|folder| folder.id != id);
        }
        let account_settings = self.settings.account_mut(&account_id);
        account_settings
            .pinned_folder_ids
            .retain(|folder_id| folder_id != &id);
        account_settings
            .expanded_folder_ids
            .retain(|folder_id| folder_id != &id);
        self.settings.save();
        if self.current_account_id.as_ref() == Some(&account_id) {
            self.mailbox.folders.retain(|folder| folder.id != id);
        }
        let deleted_was_selected = self.mailbox.selected_folder_id.as_deref() == Some(id.as_str())
            && self.mailbox.unified_selected_account.as_ref() == Some(&account_id);
        if deleted_was_selected {
            self.mailbox.selected_folder_id = None;
            self.send_refresh();
        }
        // Deleting a parent can also remove or reparent descendants,
        // depending on the provider. Reload the authoritative tree.
        self.send(Cmd::LoadFolders { account_id });
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn on_message_moved(
        &mut self,
        account_id: AccountId,
        message_id: String,
        target_folder_id: String,
        new_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let reply = self.take_bulk_message_completion(&account_id, &message_id, None);
        // Graph changes a message id when it crosses folders. Keep a local
        // pin attached when the provider reports a replacement id.
        self.replace_pinned_message_id(&account_id, &message_id, new_id.as_deref());
        self.remove_message_everywhere(&message_id);
        if let Some(new_id) = new_id {
            log::debug!("message moved, new id: {new_id}");
        }
        self.update_tray_unread();
        // A batch reloads the folder tree and the listing once, when its last
        // message lands: fifty moves are one gesture, not fifty refreshes.
        let batch_pending = matches!(reply, BulkReply::Pending);
        if !batch_pending {
            if self.mailbox.selected_folder_id.as_deref() == Some(target_folder_id.as_str()) {
                self.send_refresh();
            }
            self.send(Cmd::LoadFolders {
                account_id: account_id.clone(),
            });
        }
        if !reply.is_bulk() {
            self.toast(
                window,
                cx,
                Self::message_mutation_notification(
                    Notification::success(tr!("toast-message-moved")),
                    MessageMutationKind::Move,
                    &account_id,
                    &message_id,
                ),
            );
            return;
        }
        self.report_bulk_completion(reply, window, cx);
    }

    pub(super) fn on_tags(&mut self, account_id: AccountId, tags: Vec<Tag>) {
        self.tags_loading.remove(&account_id);
        self.tags_by_account
            .insert(account_id.clone(), tags.clone());
        let columns = self
            .settings
            .account_or_default(Some(&account_id))
            .kanban_tag_columns;
        self.kanban.ensure_account(&account_id, &columns);
        self.kanban.set_tags(&account_id, tags);
    }

    pub(super) fn on_tag_created(
        &mut self,
        account_id: AccountId,
        tag: Tag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tags = self.tags_by_account.entry(account_id.clone()).or_default();
        tags.push(tag.clone());
        let configured_columns = self
            .settings
            .account_or_default(Some(&account_id))
            .kanban_tag_columns;
        self.kanban.ensure_account(&account_id, &configured_columns);
        self.kanban.set_tags(&account_id, tags.clone());
        self.kanban.add_column(&account_id, &tag.id);
        self.kanban.mark_column_loading(&account_id, &tag.id);
        let columns = self.kanban.column_ids(&account_id);
        self.settings.account_mut(&account_id).kanban_tag_columns = columns;
        self.settings.save();
        self.send(Cmd::LoadTagListing {
            account_id,
            tag_id: tag.id,
            limit: 50,
        });
        self.toast(window, cx, Notification::success(tr!("toast-tag-created")));
    }

    pub(super) fn on_tag_renamed(
        &mut self,
        account_id: AccountId,
        id: String,
        new_id: Option<String>,
        old_message_tag: Option<String>,
        new_name: String,
    ) {
        let effective_id = new_id.unwrap_or_else(|| id.clone());
        if let Some(tags) = self.tags_by_account.get_mut(&account_id) {
            if let Some(tag) = tags.iter_mut().find(|tag| tag.id == id) {
                tag.id.clone_from(&effective_id);
                tag.display_name.clone_from(&new_name);
            }
            self.kanban.replace_tag_id(&account_id, &id, &effective_id);
            self.kanban.set_tags(&account_id, tags.clone());
        }

        if effective_id != id {
            if let Some(filters) = self.mailbox.tag_filters.get_mut(&account_id) {
                if filters.remove(&id) {
                    filters.insert(effective_id.clone());
                }
            }
            self.kanban.replace_tag_id(&account_id, &id, &effective_id);
            self.settings.account_mut(&account_id).kanban_tag_columns =
                self.kanban.column_ids(&account_id);
            for action in &mut self.settings.account_mut(&account_id).quick_actions {
                for tag_id in action.add_tags.iter_mut().chain(&mut action.remove_tags) {
                    if tag_id == &id {
                        tag_id.clone_from(&effective_id);
                    }
                }
            }
            self.settings.save();
        }

        if let Some(old_message_tag) = old_message_tag {
            self.replace_loaded_message_tag(&account_id, &old_message_tag, &new_name);
        }
    }

    fn replace_loaded_message_tag(&mut self, account_id: &AccountId, old_tag: &str, new_tag: &str) {
        for header in &mut self.mailbox.messages {
            replace_header_tag_for(header, account_id, old_tag, new_tag);
        }
        if let Some(messages) = &mut self.mailbox.search.results {
            for header in messages {
                replace_header_tag_for(header, account_id, old_tag, new_tag);
            }
        }
        if let SenderHistoryState::Loaded { messages, .. } = &mut self.sender_history {
            for header in messages {
                replace_header_tag_for(header, account_id, old_tag, new_tag);
            }
        }
        if let Some((_, thread)) = &mut self.mailbox.thread {
            for header in thread {
                replace_header_tag_for(header, account_id, old_tag, new_tag);
            }
        }
        if let Some(message) = self.mailbox.selected_mut() {
            replace_message_tag_for(message, account_id, old_tag, new_tag);
        }
        for tab in &mut self.mailbox.open_tabs {
            if let Some(message) = tab.message_mut() {
                replace_message_tag_for(message, account_id, old_tag, new_tag);
            }
        }
        for state in self.mailbox.thread_bodies.values_mut() {
            if let ThreadBodyState::Loaded(message) = state {
                replace_message_tag_for(message, account_id, old_tag, new_tag);
            }
        }
        for sent in self.mailbox.sent_messages.values_mut().flatten() {
            replace_message_tag_for(&mut sent.message, account_id, old_tag, new_tag);
        }
        if let Some(board) = self.kanban.accounts.get_mut(account_id) {
            for column in &mut board.columns {
                for header in &mut column.messages {
                    replace_header_tag_for(header, account_id, old_tag, new_tag);
                }
            }
        }
        self.invalidate_message_list();
    }

    pub(super) fn on_tag_color_set(&mut self, account_id: AccountId, id: String, color: u32) {
        if let Some(tags) = self.tags_by_account.get_mut(&account_id) {
            if let Some(tag) = tags.iter_mut().find(|tag| tag.id == id) {
                tag.color = Some(color);
            }
            self.kanban.set_tags(&account_id, tags.clone());
        }
    }

    pub(super) fn on_tag_deleted(&mut self, account_id: AccountId, id: String) {
        let remove_account_filter =
            self.mailbox
                .tag_filters
                .get_mut(&account_id)
                .is_some_and(|filters| {
                    filters.remove(&id);
                    filters.is_empty()
                });
        if remove_account_filter {
            self.mailbox.tag_filters.remove(&account_id);
        }
        if let Some(tags) = self.tags_by_account.get_mut(&account_id) {
            tags.retain(|tag| tag.id != id);
            self.kanban.set_tags(&account_id, tags.clone());
            self.kanban.remove_column(&account_id, &id);
            let columns = self.kanban.column_ids(&account_id);
            self.settings.account_mut(&account_id).kanban_tag_columns = columns;
            self.settings.save();
        }
    }

    pub(super) fn on_tag_applied(
        &mut self,
        account_id: AccountId,
        message_id: String,
        tag_id: String,
        added: bool,
    ) {
        let key = self
            .tags_by_account
            .get(&account_id)
            .and_then(|tags| tags.iter().find(|tag| tag.id == tag_id))
            .map(|tag| {
                let provider = self
                    .account(&account_id)
                    .map(|account| account.provider)
                    .unwrap_or_default();
                util::tag_storage_key(provider, tag)
            })
            .unwrap_or_else(|| tag_id.clone());
        self.update_header(&message_id, |header| {
            header.tags.retain(|tag| tag != &key);
            if added {
                header.tags.push(key.clone());
            }
        });
        if let Some(selected) = self.mailbox.selected_mut() {
            if selected.header.id == message_id {
                selected.tags.retain(|tag| tag != &key);
                if added {
                    selected.tags.push(key.clone());
                }
            }
        }
        self.kanban.mark_column_loading(&account_id, &tag_id);
        self.send(Cmd::LoadTagListing {
            account_id,
            tag_id,
            limit: 50,
        });
    }

    pub(super) fn on_tag_apply_error(
        &mut self,
        account_id: AccountId,
        message_id: String,
        tag_id: String,
        added: bool,
    ) {
        log::warn!(
            "tag mutation failed for {message_id}: {} {tag_id}; reloading kanban columns",
            if added { "add" } else { "remove" }
        );
        // A kanban move is a remove + add pair. If either half fails, reload
        // every visible column for that account so partial server success is
        // reconciled instead of leaving the optimistic card in limbo.
        let tag_ids = self.kanban.column_ids(&account_id);
        for tag_id in tag_ids {
            self.kanban.mark_column_loading(&account_id, &tag_id);
            self.send(Cmd::LoadTagListing {
                account_id: account_id.clone(),
                tag_id,
                limit: 50,
            });
        }
    }

    pub(super) fn on_tag_listing(
        &mut self,
        account_id: AccountId,
        tag_id: String,
        messages: Vec<MessageHeader>,
    ) {
        self.kanban
            .set_column_messages(&account_id, &tag_id, messages);
    }
}

fn replace_header_tag_for(
    header: &mut MessageHeader,
    account_id: &AccountId,
    old_tag: &str,
    new_tag: &str,
) {
    if &header.account_id == account_id {
        replace_tag_value(&mut header.tags, old_tag, new_tag);
    }
}

fn replace_message_tag_for(
    message: &mut Message,
    account_id: &AccountId,
    old_tag: &str,
    new_tag: &str,
) {
    if &message.header.account_id != account_id {
        return;
    }
    replace_tag_value(&mut message.header.tags, old_tag, new_tag);
    replace_tag_value(&mut message.tags, old_tag, new_tag);
}

fn replace_tag_value(tags: &mut Vec<String>, old_tag: &str, new_tag: &str) {
    for tag in tags.iter_mut() {
        if tag == old_tag {
            tag.clear();
            tag.push_str(new_tag);
        }
    }
    let mut seen = std::collections::HashSet::new();
    tags.retain(|tag| seen.insert(tag.clone()));
}
