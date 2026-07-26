//! Composition state every `ComposeView` holds, whichever surface it is
//! displayed on: recipients, the block editor, attachments, templates and the
//! transient send/draft/AI flags.

use super::addresses::RecipientInput;
use super::block_editor::BlockEditor;
use super::motion::WheelScrollMotion;
use crate::model::{Attachment, Signature, Template};
use gpui::{Entity, ScrollHandle};
use gpui_component::input::InputState;

pub(super) struct ComposerCoreInit {
    pub to: Entity<RecipientInput>,
    pub cc: Entity<RecipientInput>,
    pub bcc: Entity<RecipientInput>,
    pub show_cc: bool,
    pub show_bcc: bool,
    pub editor: Entity<BlockEditor>,
    pub files: Vec<Attachment>,
    pub templates: Vec<Template>,
    /// Signatures of every mailbox, filtered by the sending account when the
    /// picker is built — the composer can change account after opening.
    pub signatures: Vec<Signature>,
    pub draft_id: Option<String>,
    pub reply_to: Option<String>,
    pub reply_all: bool,
    pub ai_prompt: Entity<InputState>,
}

pub struct ComposerCore {
    pub to: Entity<RecipientInput>,
    pub cc: Entity<RecipientInput>,
    pub bcc: Entity<RecipientInput>,
    pub show_cc: bool,
    pub show_bcc: bool,
    /// Body edited as WYSIWYG blocks; the editor owns inline images.
    pub editor: Entity<BlockEditor>,
    pub preview: bool,
    pub files: Vec<Attachment>,
    pub templates: Vec<Template>,
    /// Signatures of every mailbox; see `ComposerCoreInit`.
    pub signatures: Vec<Signature>,
    pub draft_id: Option<String>,
    pub sending: bool,
    pub error: Option<String>,
    pub reply_to: Option<String>,
    pub reply_all: bool,
    pub ai_prompt: Entity<InputState>,
    pub ai_expanded: bool,
    pub ai_running: bool,
    pub ai_stream: String,
    pub ai_scroll_handle: ScrollHandle,
    /// Shared body/preview scroll state for full composers and inline replies.
    pub editor_scroll_handle: ScrollHandle,
    pub editor_scroll_motion: WheelScrollMotion,
}

impl ComposerCore {
    pub fn new(init: ComposerCoreInit) -> Self {
        Self {
            to: init.to,
            cc: init.cc,
            bcc: init.bcc,
            show_cc: init.show_cc,
            show_bcc: init.show_bcc,
            editor: init.editor,
            preview: false,
            files: init.files,
            templates: init.templates,
            signatures: init.signatures,
            draft_id: init.draft_id,
            sending: false,
            error: None,
            reply_to: init.reply_to,
            reply_all: init.reply_all,
            ai_prompt: init.ai_prompt,
            ai_expanded: false,
            ai_running: false,
            ai_stream: String::new(),
            ai_scroll_handle: ScrollHandle::new(),
            editor_scroll_handle: ScrollHandle::new(),
            editor_scroll_motion: WheelScrollMotion::default(),
        }
    }
}
