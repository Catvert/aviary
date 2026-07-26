//! Gmail provider: targets Gmail v1 + Google Calendar v3 + People API v1.
//!
//! Layout mirrors `providers::graph` so each runtime function has a 1:1
//! counterpart. Dispatch happens in `providers::mod`. Translating Gmail's
//! label model onto Aviary's folder/flag conventions is centralized here:
//! - `INBOX` / `SENT` / `DRAFT` / `TRASH` / `SPAM` are surfaced as
//!   well-known folders, with their Graph aliases (`inbox`, `sentitems`,
//!   `drafts`, `deleteditems`, `junkemail`).
//! - Read state ↔ presence/absence of `UNREAD`.
//! - Star ↔ presence/absence of `STARRED`.
//! - `conversationId` ↔ `threadId`.

use anyhow::Result;

pub(super) const BASE: &str = "https://gmail.googleapis.com/gmail/v1";

mod accounts;
mod calendar;
mod labels;
mod messages;
mod people;
mod send;
mod tags;

pub use accounts::get_me;
pub use calendar::{
    create_event, delete_event, list_events, move_event, respond_to_invitation, update_event,
};
pub use labels::{create_folder, delete_folder, list_folders, rename_folder};
pub use messages::{
    delete_message, fetch_attachment, fetch_messages_page, get_message, list_folder_messages,
    list_folder_messages_page, list_from_sender, list_thread, mark_read, move_message, search,
    set_flag, sync_folder_messages,
};
pub use people::list_people;
pub use send::{delete_draft, save_draft, send_mail, send_reply};
pub use tags::{
    add_tag_to_message, create_tag, delete_tag, list_messages_tagged, list_tags,
    remove_tag_from_message, rename_tag, set_tag_color, LABEL_PALETTE,
};

pub(super) async fn check_status(
    resp: reqwest::Response,
    label: &str,
) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    Err(crate::providers::http_error(resp, &format!("gmail {label} failed")).await)
}
