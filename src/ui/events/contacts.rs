//! Reduces address-book and sender-history events.

use super::super::app::AviaryApp;
use super::super::state::SenderHistoryState;
use super::super::util;
use crate::model::{AccountId, Contact, MessageHeader};
use gpui::Context;

impl AviaryApp {
    pub(super) fn on_sender_history(
        &mut self,
        email: String,
        messages: Vec<MessageHeader>,
        next_link: Option<String>,
    ) {
        if self.sender_history.email() == Some(email.as_str()) {
            self.sender_history = SenderHistoryState::Loaded {
                email,
                messages,
                next: next_link,
                loading_more: false,
            };
        }
    }

    pub(super) fn on_sender_history_more(
        &mut self,
        email: String,
        more: Vec<MessageHeader>,
        next_link: Option<String>,
    ) {
        if let SenderHistoryState::Loaded {
            email: current,
            messages,
            next,
            loading_more,
        } = &mut self.sender_history
        {
            if *current == email {
                util::dedup_append(messages, more);
                *next = next_link;
                *loading_more = false;
            }
        }
    }

    pub(super) fn on_sender_history_error(&mut self, email: String, is_load_more: bool) {
        if is_load_more {
            if let SenderHistoryState::Loaded {
                email: current,
                loading_more,
                ..
            } = &mut self.sender_history
            {
                if *current == email {
                    *loading_more = false;
                }
            }
        } else if matches!(
            &self.sender_history,
            SenderHistoryState::Loading { email: current } if *current == email
        ) {
            // Return to Idle so expanding the panel again (or reselecting a
            // contact) can retry the same address.
            self.sender_history = SenderHistoryState::Idle;
        }
    }

    pub(super) fn on_contacts(
        &mut self,
        account_id: AccountId,
        contacts: Vec<Contact>,
        cx: &mut Context<Self>,
    ) {
        self.address_book.merge(&contacts, cx);
        self.contacts.loading_accounts.remove(&account_id);
        self.contacts.by_account.insert(account_id, contacts);
        self.refresh_visible_contacts();
    }
}
