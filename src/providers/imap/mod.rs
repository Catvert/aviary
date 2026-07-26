//! IMAP/SMTP provider. Stateful TCP/TLS to the IMAP server for read paths,
//! and lettre-over-SMTP for sending. Folders/UIDs/STORE/SEARCH map onto
//! Aviary's domain types in the same way `gmail` and `graph` do, with a
//! few inevitable IMAP-specific quirks:
//!
//! - Threading: IMAP has no native conversation id. We derive one — the
//!   thread's root `Message-ID`, which RFC 5322 puts first in `References`
//!   — from each message's own headers, so every page of a listing agrees
//!   without sharing state. The thread is rebuilt on demand by SEARCH-ing
//!   the inbox and Sent for messages carrying that id.
//! - Folder ids: we normally use the mailbox name. When several physical
//!   mailboxes have the same special role (Drafts/Sent/Junk/Trash), the list
//!   exposes one local virtual id and merges their counters and messages.
//! - Message ids: we use the IMAP UID + folder, encoded as `"<folder>/<uid>"`,
//!   so we can later FETCH/STORE/EXPUNGE without remembering which mailbox
//!   the user came from.
//!
//! Calendar and contacts are not supported (they live in CalDAV/CardDAV);
//! the dispatch in `providers::mod` short-circuits those calls to empty
//! lists, so we don't expose them here.

mod connect;
mod messages;
mod send;
mod tags;

pub use connect::close_session;

pub use messages::{
    create_folder, delete_folder, delete_message, fetch_attachment, fetch_messages_page, get_me,
    get_message, list_folder_messages, list_folder_messages_page, list_folders, list_from_sender,
    list_thread, mark_read, move_message, note_last_action, rename_folder, search, set_flag,
    sync_folder_messages,
};
pub use send::{delete_draft, save_draft, send_mail, send_reply};
pub use tags::{
    add_tag_to_message, create_tag, delete_tag, list_messages_tagged, list_tags,
    remove_tag_from_message, rename_tag,
};
