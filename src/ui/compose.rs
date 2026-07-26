//! New-message/reply/forward composer, with one OS window per composition.
//! The body is edited in the WYSIWYG block editor
//! (`ui/block_editor.rs`) ; l'envoi passe par `build_html_body`, producteur
//! sole producer of outgoing HTML.

use super::account_selector::{account_selector, AccountSelectorKind, AccountSelectorOption};
use super::addresses::{AddressBook, RecipientInput};
use super::app::AviaryApp;
use super::attachments;
use super::block_editor::{BlockEditor, SignatureChoice};
use super::composer_core::{ComposerCore, ComposerCoreInit};
use super::settings::MailBodyOptions;
use super::util;
use crate::ai::{AiPromptPreset, AiSettings};
use crate::blocks::BlockKind;
use crate::model::{AccountId, Attachment, InlineImage, Message, Signature, Template};
use crate::proofreading::LanguageToolSettings;
use crate::runtime::Cmd;
use gpui::{
    div, prelude::*, px, Context, Entity, Focusable as _, Subscription, WeakEntity, Window,
    WindowHandle,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::{IndentInline, Input, InputState},
    menu::{DropdownMenu, PopupMenuItem},
    v_flex, ActiveTheme, Disableable, Root, Selectable, Sizable, StyledExt,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

pub struct ComposeHandle {
    pub id: u64,
    /// `None` for the inline composer embedded in the reader pane.
    pub window: Option<WindowHandle<Root>>,
    pub view: WeakEntity<ComposeView>,
    /// Keeps a detached composer alive after its window is optimistically
    /// closed and until undo, success, or failure decides its fate.
    pending_view: Option<Entity<ComposeView>>,
    _event_subscription: Option<Subscription>,
    _preference_subscription: gpui::Subscription,
}

/// Composer embedded in the main window's reader pane, which is the default
/// mode for a new message, like inline reply.
pub struct InlineCompose {
    pub id: u64,
    pub view: Entity<ComposeView>,
    _sub: gpui::Subscription,
}

/// Events emitted by an inline `ComposeView` to `AviaryApp`.
pub enum ComposeEvent {
    /// Discard the inline composer.
    Close,
    /// Continue in a separate window.
    Detach,
    /// Hand the send to `AviaryApp::schedule_action`, shared by inline and
    /// detached composers. A zero delay dispatches it immediately.
    ScheduleSend,
    /// Remember the selected visibility for optional recipients.
    RecipientVisibilityChanged { show_cc: bool, show_bcc: bool },
}

impl gpui::EventEmitter<ComposeEvent> for ComposeView {}

/// Where a composer is displayed. It is the same composer everywhere — same
/// editor, same send, draft, AI and template paths — but the three surfaces do
/// not have the same room, so the chrome differs:
///
/// - `Window`: its own OS window, titled by the window manager.
/// - `Tab`: a reader-pane tab, with a banner naming the composition and
///   offering to close it or detach it into a window.
/// - `Panel`: the reply panel above the message being read. The subject is
///   fixed (it is the conversation's) and shown as a title, and the body is
///   capped so the panel never swallows the reader.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ComposeSurface {
    Window,
    Tab,
    Panel,
}

impl ComposeSurface {
    /// Whether the composer draws its own close/detach banner.
    fn has_banner(self) -> bool {
        matches!(self, Self::Tab | Self::Panel)
    }

    /// A panel sits inside the reader's scroll area and sizes itself to its
    /// content; the other two fill what they are given.
    fn fills_available_space(self) -> bool {
        !matches!(self, Self::Panel)
    }
}

/// Contenu initial d'un composer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComposeInit {
    /// Stable local identity, persisted so a queued send can be routed back to
    /// the same editor after an application restart.
    pub compose_id: Option<u64>,
    /// The content is already in the durable outbox and must remain read-only
    /// until the runtime acknowledges or rejects it.
    pub pending_send: bool,
    pub from_account_id: Option<AccountId>,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body_md: String,
    /// Faithful document form when some blocks cannot make a
    /// aller-retour par Markdown (`OriginalMessage` lors d'un transfert).
    pub(crate) body_kinds: Option<Vec<BlockKind>>,
    pub reply_to: Option<String>,
    /// This reply includes all recipients from the original message.
    pub reply_all: bool,
    /// The composer represents a forward. Unlike a reply,
    /// sending remains a new message (`reply_to = None`); this marker
    /// is used only for composer labels.
    pub is_forward: bool,
    /// ID of a forward's original message, passed to `Cmd::SendMail` so the
    /// runtime marks the original as forwarded after sending.
    pub forward_of: Option<String>,
    pub draft_id: Option<String>,
    pub inline_images: Vec<InlineImage>,
    pub files: Vec<Attachment>,
    /// Do not add the default signature because the body already contains it,
    /// as when an inline reply is continued in a window.
    pub skip_signature: bool,
    /// A default template already supplies the beginning of the message, so
    /// the input paragraph normally added before a signature or quote would
    /// produce an unwanted blank line.
    pub(crate) default_template_applied: bool,
}

impl ComposeInit {
    pub fn blank() -> Self {
        Self::default()
    }

    pub fn with_to(to: String) -> Self {
        Self {
            to,
            ..Self::default()
        }
    }

    pub(crate) fn needs_leading_blank(&self) -> bool {
        let has_initial_body = self
            .body_kinds
            .as_ref()
            .is_some_and(|kinds| !kinds.is_empty())
            || !self.body_md.trim().is_empty();
        self.draft_id.is_none()
            && !self.skip_signature
            && !self.default_template_applied
            && has_initial_body
    }

    pub(crate) fn recipient_visibility(
        &self,
        preferred_cc: bool,
        preferred_bcc: bool,
    ) -> (bool, bool) {
        let show_bcc = preferred_bcc || !self.bcc.trim().is_empty();
        let show_cc = preferred_cc || show_bcc || !self.cc.trim().is_empty();
        (show_cc, show_bcc)
    }

    pub fn reply(account_id: AccountId, m: &Message) -> Self {
        let attribution = {
            let when = m
                .header
                .received
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string();
            tr!("reply-on-wrote", { date: when, sender: m.header.from.clone() })
        };
        let original_html = m
            .raw_body
            .clone()
            .unwrap_or_else(|| plain_text_html(&m.body));
        Self {
            from_account_id: Some(account_id),
            to: util::extract_email(&m.header.from).unwrap_or_else(|| m.header.from.clone()),
            subject: make_reply_subject(&m.header.subject),
            body_kinds: Some(vec![
                BlockKind::Paragraph(attribution.to_string()),
                BlockKind::OriginalMessage {
                    html: original_html,
                    inline_images: m.inline_images.clone(),
                    source_id: m.header.id.clone(),
                },
            ]),
            reply_to: Some(m.header.id.clone()),
            inline_images: m.inline_images.clone(),
            ..Self::default()
        }
    }

    pub fn reply_all(account_id: AccountId, account_email: Option<&str>, m: &Message) -> Self {
        let mut init = Self::reply(account_id, m);
        let own_address = account_email.map(normalized_address);
        let sender_is_own = own_address
            .as_deref()
            .is_some_and(|own| own == normalized_address(&m.header.from));
        let mut to = Vec::new();
        let mut cc = Vec::new();

        if sender_is_own {
            for recipient in &m.to {
                push_unique_recipient(&mut to, recipient, own_address.as_deref(), &[]);
            }
        } else {
            push_unique_recipient(&mut to, &m.header.from, own_address.as_deref(), &[]);
        }

        let cc_sources = if sender_is_own {
            m.cc.iter().collect::<Vec<_>>()
        } else {
            m.to.iter().chain(&m.cc).collect::<Vec<_>>()
        };
        for recipient in cc_sources {
            push_unique_recipient(&mut cc, recipient, own_address.as_deref(), &to);
        }

        // A missing account identity or unusual message must not produce a
        // reply without recipients.
        if to.is_empty() {
            to.push(init.to.clone());
        }
        init.to = to.join(", ");
        init.cc = cc.join(", ");
        init.reply_all = true;
        init
    }

    pub fn forward(account_id: AccountId, m: &Message) -> Self {
        let when = m
            .header
            .received
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        let mut headers = tr!("forward-header-from-date", {
            sender: m.header.from.clone(),
            date: when
        })
        .to_string();
        if !m.to.is_empty() {
            headers.push_str(&format!(
                "\n{}",
                tr!("forward-header-to", { recipients: m.to.join(", ") })
            ));
        }
        if !m.cc.is_empty() {
            headers.push_str(&format!(
                "\n{}",
                tr!("forward-header-cc", { recipients: m.cc.join(", ") })
            ));
        }
        headers.push_str(&format!(
            "\n{}",
            tr!("forward-header-subject", { subject: m.header.subject.clone() })
        ));

        // `body` is the Markdown conversion intended for the reader. A forward
        // must start from received HTML to preserve tables,
        // signatures, styles, and exact CID-image positions.
        let original_html = m
            .raw_body
            .clone()
            .unwrap_or_else(|| plain_text_html(&m.body));
        let body_kinds = vec![
            BlockKind::Divider,
            BlockKind::Paragraph(format!("**{}**", tr!("forwarded-message"))),
            BlockKind::Paragraph(headers.to_string()),
            BlockKind::OriginalMessage {
                html: original_html,
                inline_images: m.inline_images.clone(),
                source_id: m.header.id.clone(),
            },
        ];
        Self {
            from_account_id: Some(account_id),
            subject: make_forward_subject(&m.header.subject),
            body_kinds: Some(body_kinds),
            is_forward: true,
            forward_of: Some(m.header.id.clone()),
            inline_images: m.inline_images.clone(),
            files: m.attachments.clone(),
            ..Self::default()
        }
    }

    pub fn draft(account_id: AccountId, m: Message) -> Self {
        // A draft is reopened from the markup, not from `body` — the reader's
        // Markdown conversion is lossy by design (layout tables unwrapped,
        // class names dropped, `cid:` rewritten), and what it drops is exactly
        // what the editor needs to rebuild the document: the signature block,
        // the quoted original, tables, image widths. Plain-text drafts have no
        // markup and keep the Markdown path.
        let body_kinds = m
            .raw_body
            .as_deref()
            .map(|html| {
                crate::blocks::html_to_blocks(
                    html,
                    &crate::blocks::HtmlImport {
                        inline_images: &m.inline_images,
                        source_id: &m.header.id,
                    },
                )
            })
            .filter(|kinds| !kinds.is_empty());
        Self {
            from_account_id: Some(account_id),
            to: m.to.join(", "),
            cc: m.cc.join(", "),
            bcc: m.bcc.join(", "),
            subject: m.header.subject.clone(),
            body_md: if body_kinds.is_some() {
                String::new()
            } else {
                m.body.clone()
            },
            body_kinds,
            draft_id: m.draft_id.clone(),
            inline_images: m.inline_images.clone(),
            files: m.attachments.clone(),
            ..Self::default()
        }
    }
}

fn make_reply_subject(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.to_lowercase().starts_with("re:") {
        trimmed.to_string()
    } else {
        tr!("reply-subject", { subject: trimmed }).to_string()
    }
}

fn normalized_address(raw: &str) -> String {
    util::extract_email(raw)
        .unwrap_or_else(|| raw.trim().to_string())
        .to_ascii_lowercase()
}

fn push_unique_recipient(
    recipients: &mut Vec<String>,
    candidate: &str,
    own_address: Option<&str>,
    excluded: &[String],
) {
    let normalized = normalized_address(candidate);
    if normalized.is_empty()
        || own_address.is_some_and(|own| own == normalized)
        || excluded
            .iter()
            .any(|recipient| normalized_address(recipient) == normalized)
        || recipients
            .iter()
            .any(|recipient| normalized_address(recipient) == normalized)
    {
        return;
    }
    recipients.push(candidate.to_string());
}

fn make_forward_subject(s: &str) -> String {
    let trimmed = s.trim();
    let lower = trimmed.to_lowercase();
    if ["tr:", "fw:", "fwd:"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        trimmed.to_string()
    } else {
        tr!("forward-subject", { subject: trimmed }).to_string()
    }
}

fn compose_kind_title(is_draft: bool, is_reply: bool, is_forward: bool) -> String {
    if is_draft {
        tr!("compose-draft-title").to_string()
    } else if is_reply {
        tr!("viewer-reply").to_string()
    } else if is_forward {
        tr!("viewer-forward").to_string()
    } else {
        tr!("compose-title").to_string()
    }
}

#[derive(Clone, Copy)]
enum ComposeInitialFocus {
    To,
    Subject,
    Body,
}

impl ComposeInitialFocus {
    fn for_init(init: &ComposeInit) -> Self {
        if init.to.trim().is_empty() {
            Self::To
        } else if init.subject.trim().is_empty() {
            Self::Subject
        } else {
            Self::Body
        }
    }
}

fn plain_text_html(text: &str) -> String {
    let escaped = util::escape_html_text(text);
    format!(r#"<pre style="white-space:pre-wrap">{escaped}</pre>"#)
}

/// Shared icon button for saving a draft.
pub(super) fn compose_save_draft_button(id: impl Into<gpui::ElementId>) -> Button {
    Button::new(id)
        .ghost()
        .icon(super::icons::app_icon("save"))
        .tooltip(tr!("compose-save-draft-full"))
}

/// Shared edit/preview toggle represented by the pencil.
pub(super) fn compose_preview_toggle(id: impl Into<gpui::ElementId>, preview: bool) -> Button {
    Button::new(id)
        .xsmall()
        .ghost()
        .flex_none()
        .icon(super::icons::app_icon("pencil"))
        .selected(!preview)
        .tooltip(if preview {
            tr!("compose-edit")
        } else {
            tr!("compose-preview")
        })
}

pub struct ComposeView {
    pub id: u64,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    accounts: Vec<AccountSelectorOption>,
    from_account_id: Option<AccountId>,
    core: ComposerCore,
    subject: Entity<InputState>,
    pending_send: Option<Cmd>,
    outbox_queued: bool,
    /// Account that owns the provider identifiers `reply_to` or
    /// `forward_of`. Another sender sends the content as a new message.
    origin_account_id: Option<AccountId>,
    is_forward: bool,
    forward_of: Option<String>,
    draft_save_in_flight: bool,
    autosave_pending_fingerprint: Option<u64>,
    last_autosave_fingerprint: u64,
    ai_settings: AiSettings,
    surface: ComposeSurface,
    /// Fixed subject of a `Panel` composer: a reply keeps the conversation's,
    /// so it is displayed rather than edited.
    fixed_subject: String,
}

impl std::ops::Deref for ComposeView {
    type Target = ComposerCore;

    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

/// The signature as the draft carries it: rendered once, named, and tied back
/// to the signature it came from so the block's picker can swap it.
pub(crate) fn signature_block_kind(signature: &Signature) -> BlockKind {
    BlockKind::Signature {
        signature_id: Some(signature.id),
        name: signature.name.clone(),
        html: crate::blocks::build_html_body(&signature.blocks),
    }
}

/// Matches the signature blocks rebuilt from a draft's HTML against the
/// mailbox's signatures, so each one gets its name — and its id back when the
/// provider dropped the attribute carrying it.
///
/// Free function: the matching is pure, and it is the part worth testing.
pub(crate) fn name_signature_blocks(kinds: &mut [BlockKind], signatures: &[Signature]) {
    for kind in kinds.iter_mut() {
        let BlockKind::Signature {
            signature_id,
            name,
            html,
        } = kind
        else {
            continue;
        };
        if !name.is_empty() {
            continue;
        }
        let matched = signature_id
            .and_then(|id| signatures.iter().find(|signature| signature.id == id))
            .or_else(|| {
                let text = crate::blocks::html_text(html);
                if text.is_empty() {
                    return None;
                }
                signatures.iter().find(|signature| {
                    crate::blocks::html_text(&crate::blocks::build_html_body(&signature.blocks))
                        == text
                })
            });
        match matched {
            Some(signature) => {
                *signature_id = Some(signature.id);
                *name = signature.name.clone();
            }
            None => {
                // Unknown, but still a signature: the picker can swap it for
                // one of this mailbox's, which is what it is there for.
                *signature_id = None;
                *name = tr!("compose-signature-imported").to_string();
            }
        }
    }
}

/// What the signature block's picker offers for one mailbox.
pub(crate) fn signature_choices(
    signatures: &[Signature],
    account_id: Option<&AccountId>,
) -> Vec<SignatureChoice> {
    signatures
        .iter()
        .filter(|signature| account_id == Some(&signature.account_id))
        .map(|signature| SignatureChoice {
            id: signature.id,
            name: signature.name.clone(),
            html: crate::blocks::build_html_body(&signature.blocks),
            images: signature.images.clone(),
        })
        .collect()
}

impl ComposeView {
    pub(crate) fn belongs_to_account(&self, account_id: &AccountId) -> bool {
        self.from_account_id.as_ref() == Some(account_id)
            || self.origin_account_id.as_ref() == Some(account_id)
    }
}

impl std::ops::DerefMut for ComposeView {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.core
    }
}

impl ComposeView {
    fn focus_to(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.to.update(cx, |to, cx| to.focus(window, cx));
    }

    fn focus_cc(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cc.update(cx, |cc, cx| cc.focus(window, cx));
    }

    fn focus_bcc(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.bcc.update(cx, |bcc, cx| bcc.focus(window, cx));
    }

    fn toggle_cc(&mut self, cx: &mut Context<Self>) {
        self.show_cc = !self.show_cc;
        if !self.show_cc {
            self.show_bcc = false;
        }
        cx.emit(ComposeEvent::RecipientVisibilityChanged {
            show_cc: self.show_cc,
            show_bcc: self.show_bcc,
        });
        cx.notify();
    }

    fn toggle_bcc(&mut self, cx: &mut Context<Self>) {
        self.show_bcc = !self.show_bcc;
        cx.emit(ComposeEvent::RecipientVisibilityChanged {
            show_cc: self.show_cc,
            show_bcc: self.show_bcc,
        });
        cx.notify();
    }

    fn focus_body(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.focus_first(window, cx));
    }

    fn focus_body_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_template_cursor(window, cx) {
            self.focus_body(window, cx);
        }
    }

    fn focus_subject(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.subject
            .update(cx, |subject, cx| subject.focus(window, cx));
    }

    fn focus_template_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.focus_template_cursor(window, cx))
    }

    fn apply_initial_focus(
        &mut self,
        target: ComposeInitialFocus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            ComposeInitialFocus::To => self.focus_to(window, cx),
            // A panel's subject is fixed and not rendered, so there is nothing
            // to focus there; the body is the next thing the user would type in.
            ComposeInitialFocus::Subject if self.surface == ComposeSurface::Panel => {
                if !self.focus_template_cursor(window, cx) {
                    self.focus_body(window, cx);
                }
            }
            ComposeInitialFocus::Subject => self.focus_subject(window, cx),
            ComposeInitialFocus::Body => {
                if !self.focus_template_cursor(window, cx) {
                    self.focus_body(window, cx);
                }
            }
        }
    }

    pub fn refresh_i18n(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.surface == ComposeSurface::Window {
            window.set_window_title(&compose_kind_title(
                self.draft_id.is_some(),
                self.reply_to.is_some(),
                self.is_forward,
            ));
        }
        self.to.update(cx, |input, cx| {
            input.set_placeholder(tr!("compose-to-placeholder").to_string(), window, cx);
        });
        self.cc.update(cx, |input, cx| {
            input.set_placeholder(tr!("compose-cc-placeholder").to_string(), window, cx);
        });
        self.bcc.update(cx, |input, cx| {
            input.set_placeholder(tr!("compose-bcc-placeholder").to_string(), window, cx);
        });
        self.subject.update(cx, |state, cx| {
            state.set_placeholder(tr!("compose-subject-hint"), window, cx);
        });
        self.editor.update(cx, |editor, cx| {
            editor.set_placeholder(tr!("compose-body-placeholder"), window, cx);
        });
        self.ai_prompt.update(cx, |state, cx| {
            state.set_placeholder(tr!("compose-ai-prompt-placeholder"), window, cx);
        });
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        id: u64,
        init: ComposeInit,
        cmd_tx: mpsc::UnboundedSender<Cmd>,
        accounts: Vec<AccountSelectorOption>,
        address_book: AddressBook,
        templates: Vec<Template>,
        signatures: Vec<Signature>,
        ai_settings: AiSettings,
        proofreading_settings: LanguageToolSettings,
        mail_body_options: MailBodyOptions,
        preferred_recipient_visibility: (bool, bool),
        editor_width_hint: Option<f32>,
        surface: ComposeSurface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pending_send = init.pending_send;
        let (show_cc, show_bcc) = init.recipient_visibility(
            preferred_recipient_visibility.0,
            preferred_recipient_visibility.1,
        );
        let to = cx.new(|cx| {
            RecipientInput::new(
                &init.to,
                tr!("compose-to-placeholder").to_string(),
                address_book.clone(),
                window,
                cx,
            )
            .tab_index(10)
        });
        let cc = cx.new(|cx| {
            RecipientInput::new(
                &init.cc,
                tr!("compose-cc-placeholder").to_string(),
                address_book.clone(),
                window,
                cx,
            )
            .tab_index(20)
        });
        let bcc = cx.new(|cx| {
            RecipientInput::new(
                &init.bcc,
                tr!("compose-bcc-placeholder").to_string(),
                address_book.clone(),
                window,
                cx,
            )
            .tab_index(30)
        });
        let subject = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr!("compose-subject-hint"))
                .default_value(init.subject.clone())
        });
        let ai_prompt = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tr!("compose-ai-prompt-placeholder"))
        });
        // A reply or signature without a template pre-fills the body: keep an
        // empty paragraph at the top so the user can type above quoted content.
        // A template already provides this opening; a draft or detached reply
        // also restores the body unchanged.
        let lead_blank = init.needs_leading_blank();
        let proofreading_tx = cmd_tx.clone();
        let mention_address_book = address_book;
        let mention_recipient = to.clone();
        let placeholder = match surface {
            ComposeSurface::Panel => tr!("reply-body-placeholder"),
            _ => tr!("compose-body-placeholder"),
        };
        let editor = cx.new(|cx| {
            let editor = if let Some(kinds) = init.body_kinds.clone() {
                BlockEditor::new(
                    kinds,
                    init.inline_images.clone(),
                    lead_blank,
                    mail_body_options,
                    &placeholder,
                    window,
                    cx,
                )
            } else {
                BlockEditor::from_markdown(
                    &init.body_md,
                    init.inline_images.clone(),
                    lead_blank,
                    mail_body_options,
                    &placeholder,
                    window,
                    cx,
                )
            };
            let editor = editor
                .with_layout_width_hint(editor_width_hint)
                .with_runtime(proofreading_tx)
                .with_proofreading(proofreading_settings)
                .with_contact_mentions(mention_address_book, mention_recipient, cx);
            // The panel is short: the quoted history it carries starts folded.
            if surface == ComposeSurface::Panel {
                editor.collapse_original_messages()
            } else {
                editor
            }
        });
        for recipients in [&to, &cc, &bcc] {
            cx.observe(recipients, |_, _, cx| cx.notify()).detach();
        }
        cx.observe(&subject, |_, _, cx| cx.notify()).detach();
        cx.observe(&editor, |_, _, cx| cx.notify()).detach();
        let from = init
            .from_account_id
            .clone()
            .or_else(|| accounts.first().map(|account| account.id.clone()));
        let origin_account_id = (init.reply_to.is_some() || init.forward_of.is_some())
            .then(|| init.from_account_id.clone())
            .flatten();
        let mut view = Self {
            id,
            cmd_tx,
            accounts,
            from_account_id: from,
            core: ComposerCore::new(ComposerCoreInit {
                to,
                cc,
                bcc,
                show_cc,
                show_bcc,
                editor,
                files: init.files,
                templates,
                signatures,
                draft_id: init.draft_id,
                reply_to: init.reply_to,
                reply_all: init.reply_all,
                ai_prompt,
            }),
            subject,
            pending_send: None,
            outbox_queued: pending_send,
            origin_account_id,
            is_forward: init.is_forward,
            forward_of: init.forward_of,
            draft_save_in_flight: false,
            autosave_pending_fingerprint: None,
            last_autosave_fingerprint: 0,
            ai_settings,
            surface,
            fixed_subject: init.subject,
        };
        view.sending = pending_send;
        view.refresh_signature_choices(cx);
        view.last_autosave_fingerprint = view.draft_fingerprint(cx);
        view
    }

    /// Hands the block editor the sending account's signatures, so the
    /// signature block can offer them. Called again whenever the sending
    /// account changes: another mailbox has other signatures.
    fn refresh_signature_choices(&self, cx: &mut Context<Self>) {
        let choices = signature_choices(&self.signatures, self.from_account_id.as_ref());
        self.editor.update(cx, |editor, cx| {
            editor.set_available_signatures(choices, cx)
        });
    }

    /// Applies `signature_id` to the draft: replaces the signature block when
    /// there is one, otherwise inserts it at the cursor, the way a template is
    /// inserted.
    fn apply_signature(&mut self, signature_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(signature) = self.signatures.iter().find(|signature| {
            signature.id == signature_id
                && self.from_account_id.as_ref() == Some(&signature.account_id)
        }) else {
            return;
        };
        let choice = SignatureChoice {
            id: signature.id,
            name: signature.name.clone(),
            html: crate::blocks::build_html_body(&signature.blocks),
            images: signature.images.clone(),
        };
        self.editor
            .update(cx, |editor, cx| editor.apply_signature(&choice, window, cx));
        cx.notify();
    }

    fn clear_signature(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.clear_signature(window, cx));
        cx.notify();
    }

    /// Tab title: the subject being entered, or a label based on composition type.
    pub fn tab_title(&self, cx: &gpui::App) -> String {
        let subject = self.subject.read(cx).value().trim().to_string();
        if !subject.is_empty() {
            return subject;
        }
        compose_kind_title(
            self.draft_id.is_some(),
            self.reply_to.is_some(),
            self.is_forward,
        )
    }

    fn window_title(&self) -> String {
        compose_kind_title(
            self.draft_id.is_some(),
            self.reply_to.is_some(),
            self.is_forward,
        )
    }

    /// Captures current state again as `ComposeInit`, used to detach an inline
    /// composer into a window.
    pub fn to_init(&self, cx: &gpui::App) -> ComposeInit {
        let editor = self.editor.read(cx);
        ComposeInit {
            compose_id: Some(self.id),
            pending_send: self.sending,
            from_account_id: self.from_account_id.clone(),
            to: self.to.read(cx).serialized(cx),
            cc: self.cc.read(cx).serialized(cx),
            bcc: self.bcc.read(cx).serialized(cx),
            subject: self.subject.read(cx).value().to_string(),
            body_md: editor.to_markdown(cx),
            body_kinds: Some(editor.to_kinds(cx)),
            reply_to: self.reply_to.clone(),
            reply_all: self.reply_all,
            is_forward: self.is_forward,
            forward_of: self.forward_of.clone(),
            draft_id: self.draft_id.clone(),
            inline_images: editor.images().to_vec(),
            files: self.files.clone(),
            skip_signature: true,
            default_template_applied: false,
        }
    }

    fn draft_fingerprint(&self, cx: &gpui::App) -> u64 {
        let mut init = self.to_init(cx);
        // The provider id changes after the first save but does not represent
        // user content and must not immediately trigger a second autosave.
        init.draft_id = None;
        draft_fingerprint_for_init(&init)
    }

    /// Inserts a template's blocks after the focused block, or at the end, and
    /// imports its inline images.
    fn insert_template(&mut self, tid: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tmpl) = self
            .templates
            .iter()
            .find(|template| {
                template.id == tid && self.from_account_id.as_ref() == Some(&template.account_id)
            })
            .cloned()
        else {
            return;
        };
        let kinds = tmpl.blocks.iter().map(|b| b.kind.clone()).collect();
        self.editor.update(cx, |editor, cx| {
            editor.insert_template(kinds, tmpl.images.clone(), window, cx);
        });
        cx.notify();
    }

    fn payload(&self, cx: &Context<Self>) -> (Vec<String>, Vec<String>, Vec<String>, String) {
        let to = self.to.read(cx).bare_addresses(cx);
        let cc = self.cc.read(cx).bare_addresses(cx);
        let bcc = self.bcc.read(cx).bare_addresses(cx);
        let subject = self.subject.read(cx).value().trim().to_string();
        (to, cc, bcc, subject)
    }

    fn trigger_ai(&mut self, preset: AiPromptPreset, cx: &mut Context<Self>) {
        let instruction = self.ai_prompt.read(cx).value().trim().to_string();
        if preset.requires_instruction() && instruction.is_empty() {
            self.error = Some(tr!("compose-ai-instruction-required").to_string());
            cx.notify();
            return;
        }
        let body_markdown = self.editor.read(cx).ai_markdown(cx);
        self.ai_running = true;
        self.ai_stream.clear();
        self.error = None;
        let _ = self
            .cmd_tx
            .send(Cmd::EditMailWithAi(crate::runtime::AiEditRequest {
                compose_id: self.id,
                config: self.ai_settings.active_config(),
                system_prompt: self.ai_settings.system_prompt.clone(),
                prompt_template: preset.prompt,
                instruction,
                subject: self.subject.read(cx).value().trim().to_string(),
                body_markdown,
            }));
        cx.notify();
    }

    pub fn on_ai_chunk(&mut self, delta: String, cx: &mut Context<Self>) {
        self.ai_stream.push_str(&delta);
        self.ai_scroll_handle.scroll_to_bottom();
        cx.notify();
    }

    pub fn on_ai_finished(
        &mut self,
        markdown: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ai_running = false;
        self.ai_stream = markdown.clone();
        self.ai_scroll_handle.scroll_to_bottom();
        self.error = None;
        self.preview = false;
        self.editor.update(cx, |editor, cx| {
            editor.apply_ai_markdown(&markdown, window, cx);
        });
        cx.notify();
    }

    pub fn on_ai_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.ai_running = false;
        self.error = Some(error);
        cx.notify();
    }

    fn set_ai_settings(&mut self, settings: AiSettings) {
        self.ai_settings = settings;
    }

    fn trigger_send(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(from) = self.from_account_id.clone() else {
            self.error = Some(tr!("compose-error-no-active-account").to_string());
            cx.notify();
            return;
        };
        let (to, cc, bcc, subject) = self.payload(cx);
        if to.is_empty() {
            self.error = Some(tr!("compose-error-recipient-required").to_string());
            cx.notify();
            return;
        }
        if subject.is_empty() {
            self.error = Some(tr!("compose-error-subject-required").to_string());
            cx.notify();
            return;
        }
        self.sending = true;
        self.outbox_queued = false;
        self.error = None;
        let editor = self.editor.read(cx);
        let (body, attachments) = editor.build_outgoing(cx);
        let uses_origin = self.origin_account_id.as_ref() == Some(&from);
        let command = Cmd::SendMail {
            account_id: from,
            compose_id: self.id,
            reply_to: uses_origin.then(|| self.reply_to.clone()).flatten(),
            reply_all: uses_origin && self.reply_all,
            forward_of: uses_origin.then(|| self.forward_of.clone()).flatten(),
            draft_id: self.draft_id.clone(),
            mail: crate::runtime::OutgoingMail {
                to,
                cc,
                bcc,
                subject,
                body,
                body_is_html: true,
                attachments,
                files: self.files.clone(),
            },
        };

        self.pending_send = Some(command);
        cx.emit(ComposeEvent::ScheduleSend);
        cx.notify();
    }

    pub(crate) fn take_pending_send(&mut self) -> Option<Cmd> {
        self.pending_send.take()
    }

    pub(crate) fn cancel_pending_send(&mut self, cx: &mut Context<Self>) {
        self.pending_send = None;
        self.sending = false;
        self.outbox_queued = false;
        cx.notify();
    }

    fn trigger_save_draft(&mut self, cx: &mut Context<Self>) {
        let Some(from) = self.from_account_id.clone() else {
            self.error = Some(tr!("compose-error-no-active-account").to_string());
            cx.notify();
            return;
        };
        let (to, cc, bcc, subject) = self.payload(cx);
        self.sending = true;
        self.outbox_queued = false;
        self.error = None;
        let editor = self.editor.read(cx);
        let (body, attachments) = editor.build_outgoing(cx);
        let _ = self.cmd_tx.send(Cmd::SaveDraft {
            account_id: from,
            compose_id: self.id,
            replace_id: self.draft_id.clone(),
            mail: crate::runtime::OutgoingMail {
                to,
                cc,
                bcc,
                subject,
                body,
                body_is_html: true,
                attachments,
                files: self.files.clone(),
            },
            autosave: false,
        });
        cx.notify();
    }

    /// Debounced by the application-level timer. The local session continues
    /// to save every 400 ms; this method mirrors changed content to the
    /// provider without blocking the editor or displaying repetitive toasts.
    pub(crate) fn maybe_autosave_draft(&mut self, cx: &mut Context<Self>) {
        if self.sending || self.ai_running || self.draft_save_in_flight {
            return;
        }
        let Some(from) = self.from_account_id.clone() else {
            return;
        };
        let fingerprint = self.draft_fingerprint(cx);
        if fingerprint == self.last_autosave_fingerprint {
            return;
        }
        let (to, cc, bcc, subject) = self.payload(cx);
        let editor = self.editor.read(cx);
        let (body, attachments) = editor.build_outgoing(cx);
        self.draft_save_in_flight = true;
        self.autosave_pending_fingerprint = Some(fingerprint);
        let _ = self.cmd_tx.send(Cmd::SaveDraft {
            account_id: from,
            compose_id: self.id,
            replace_id: self.draft_id.clone(),
            mail: crate::runtime::OutgoingMail {
                to,
                cc,
                bcc,
                subject,
                body,
                body_is_html: true,
                attachments,
                files: self.files.clone(),
            },
            autosave: true,
        });
    }

    fn pick_attachment(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    this.attach_paths(paths, cx);
                });
            }
        })
        .detach();
    }

    fn attach_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.sending || paths.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let attachments = cx
                .background_executor()
                .spawn(async move {
                    paths
                        .into_iter()
                        .map(|path| attachment_from_path(&path))
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.sending {
                    return;
                }
                let mut first_error = None;
                for attachment in attachments {
                    match attachment {
                        Ok(attachment) => this.files.push(attachment),
                        Err(error) if first_error.is_none() => first_error = Some(error),
                        Err(_) => {}
                    }
                }
                if let Some(error) = first_error {
                    this.error = Some(error);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn on_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.sending = false;
        self.pending_send = None;
        self.outbox_queued = false;
        self.error = Some(error);
        cx.notify();
    }

    pub fn on_outbox_queued(&mut self, cx: &mut Context<Self>) {
        self.outbox_queued = true;
        cx.notify();
    }

    pub fn on_draft_saved(
        &mut self,
        account_id: &AccountId,
        draft_id: Option<String>,
        autosave: bool,
        cx: &mut Context<Self>,
    ) {
        if self.from_account_id.as_ref() != Some(account_id) {
            self.draft_save_in_flight = false;
            self.autosave_pending_fingerprint = None;
            return;
        }
        if autosave {
            self.draft_save_in_flight = false;
        } else {
            self.sending = false;
            self.outbox_queued = false;
        }
        if draft_id.is_some() {
            self.draft_id = draft_id;
        }
        self.last_autosave_fingerprint = self
            .autosave_pending_fingerprint
            .take()
            .unwrap_or_else(|| self.draft_fingerprint(cx));
        cx.notify();
    }

    pub fn on_draft_error(
        &mut self,
        account_id: &AccountId,
        error: String,
        autosave: bool,
        cx: &mut Context<Self>,
    ) {
        if self.from_account_id.as_ref() != Some(account_id) {
            self.draft_save_in_flight = false;
            self.autosave_pending_fingerprint = None;
            return;
        }
        if autosave {
            self.draft_save_in_flight = false;
            self.autosave_pending_fingerprint = None;
            self.error = Some(error);
        } else {
            self.on_error(error, cx);
            return;
        }
        cx.notify();
    }
}

pub(crate) fn draft_fingerprint_for_init(init: &ComposeInit) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut init = init.clone();
    init.draft_id = None;
    init.pending_send = false;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Ok(bytes) = serde_json::to_vec(&init) {
        bytes.hash(&mut hasher);
    }
    hasher.finish()
}

impl ComposeView {
    /// Everything above the body: the hosted-surface banner, the recipient
    /// rows, the subject, the toolbar, the writing assistant, and the
    /// attachment chips.
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let panel = self.surface == ComposeSurface::Panel;
        let inline_bar = self.render_banner(cx);
        let toolbar = self.render_toolbar(cx);
        let ai_panel = self.render_ai_panel(cx);
        let file_chips = (!self.files.is_empty()).then(|| self.render_attachment_chips(cx));
        v_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .p_3()
            .when(!panel, |element| {
                element.border_b_1().border_color(theme.border)
            })
            .children(inline_bar)
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(self.to.clone()))
                    .child(
                        Button::new("toggle-cc")
                            .ghost()
                            .xsmall()
                            .flex_none()
                            .tab_index(15)
                            .label(tr!("compose-add-cc"))
                            .selected(self.show_cc)
                            .disabled(self.sending)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_cc(cx);
                            })),
                    ),
            )
            .when(self.show_cc, |el| {
                el.child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(div().flex_1().min_w_0().child(self.cc.clone()))
                        .child(
                            Button::new("toggle-bcc")
                                .ghost()
                                .xsmall()
                                .flex_none()
                                .tab_index(25)
                                .label(tr!("compose-add-bcc"))
                                .selected(self.show_bcc)
                                .disabled(self.sending)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.toggle_bcc(cx);
                                })),
                        ),
                )
            })
            .when(self.show_bcc, |el| {
                el.child(div().w_full().min_w_0().child(self.bcc.clone()))
            })
            .when(!panel, |element| {
                element.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .child(Input::new(&self.subject).tab_index(40).w_full()),
                )
            })
            .child(toolbar)
            .children(ai_panel)
            .children(file_chips)
    }

    /// Title bar of a composer hosted by the main window — a reader-pane tab or
    /// the reply panel — with its detach and close buttons. A window composer
    /// has the OS chrome instead.
    fn render_banner(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.surface.has_banner() {
            // The panel replies inside a conversation, so its title is that
            // conversation's subject rather than the kind of composition.
            let title = if self.surface == ComposeSurface::Panel {
                self.fixed_subject.clone()
            } else {
                compose_kind_title(
                    self.draft_id.is_some(),
                    self.reply_to.is_some(),
                    self.is_forward,
                )
            };
            Some(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_semibold()
                            .text_sm()
                            .child(title),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .child(
                                Button::new("ic-detach")
                                    .ghost()
                                    .xsmall()
                                    .tab_index(95)
                                    .disabled(self.sending)
                                    .icon(super::icons::app_icon("external-link"))
                                    .tooltip(tr!("compose-open-window"))
                                    .on_click(
                                        cx.listener(|_, _, _, cx| cx.emit(ComposeEvent::Detach)),
                                    ),
                            )
                            .child(
                                Button::new("ic-close")
                                    .ghost()
                                    .xsmall()
                                    .tab_index(96)
                                    .disabled(self.sending)
                                    .icon(super::icons::app_icon("x"))
                                    .tooltip(tr!("compose-discard"))
                                    .on_click(
                                        cx.listener(|_, _, _, cx| cx.emit(ComposeEvent::Close)),
                                    ),
                            ),
                    ),
            )
        } else {
            None
        }
    }

    /// Composition tools: attachments, writing assistant, proofreading,
    /// formatting, images, templates, and the preview toggle.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let available_templates: Vec<(i64, String)> = self
            .templates
            .iter()
            .filter(|template| self.from_account_id.as_ref() == Some(&template.account_id))
            .map(|template| (template.id, template.name.clone()))
            .collect();
        let available_signatures: Vec<(i64, String)> = self
            .signatures
            .iter()
            .filter(|signature| self.from_account_id.as_ref() == Some(&signature.account_id))
            .map(|signature| (signature.id, signature.name.clone()))
            .collect();

        h_flex()
            .w_full()
            .min_w_0()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                h_flex()
                    .tab_group()
                    .tab_index(60)
                    .tab_stop(false)
                    .min_w_0()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        Button::new("attach")
                            .xsmall()
                            .ghost()
                            .icon(super::icons::app_icon("paperclip"))
                            .label(tr!("compose-attach"))
                            .tooltip(tr!("compose-attach-drop-tip"))
                            .disabled(self.sending)
                            .on_click(cx.listener(|this, _, _, cx| this.pick_attachment(cx))),
                    )
                    .child(
                        Button::new("toggle-ai")
                            .xsmall()
                            .ghost()
                            .icon(super::icons::app_icon("bot"))
                            .label(tr!("compose-ai"))
                            .selected(self.ai_expanded)
                            .disabled(self.sending)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.ai_expanded = !this.ai_expanded;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("check-proofreading")
                            .xsmall()
                            .ghost()
                            .icon(super::icons::app_icon("check-check"))
                            .label(tr!("proofreading-check-now"))
                            .disabled(self.sending || self.preview)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.editor.update(cx, |editor, cx| {
                                    editor.check_all_now(cx);
                                });
                            })),
                    )
                    .child(BlockEditor::format_toolbar(
                        "compose-format-actions",
                        self.editor.clone(),
                        self.sending || self.preview,
                    ))
                    .child(
                        Button::new("insert-image")
                            .xsmall()
                            .ghost()
                            .icon(super::icons::app_icon("image"))
                            .label(tr!("compose-add-image"))
                            .disabled(self.sending)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.editor.update(cx, |editor, cx| {
                                    editor.prompt_insert_image(window, cx);
                                });
                            })),
                    )
                    .when(!available_signatures.is_empty(), |el| {
                        let signatures = available_signatures.clone();
                        let entity = cx.entity();
                        el.child(
                            Button::new("signatures")
                                .xsmall()
                                .ghost()
                                .icon(super::icons::app_icon("pen-line"))
                                .label(tr!("compose-signature"))
                                .dropdown_menu(move |mut menu, _window, _cx| {
                                    for (sid, name) in signatures.clone() {
                                        let entity = entity.clone();
                                        menu = menu.item(PopupMenuItem::new(name).on_click(
                                            move |_, window, cx| {
                                                entity.update(cx, |this, cx| {
                                                    this.apply_signature(sid, window, cx);
                                                });
                                            },
                                        ));
                                    }
                                    let entity = entity.clone();
                                    menu.separator().item(
                                        PopupMenuItem::new(tr!("compose-signature-none")).on_click(
                                            move |_, window, cx| {
                                                entity.update(cx, |this, cx| {
                                                    this.clear_signature(window, cx);
                                                });
                                            },
                                        ),
                                    )
                                }),
                        )
                    })
                    .when(!available_templates.is_empty(), |el| {
                        let templates = available_templates.clone();
                        let entity = cx.entity();
                        el.child(
                            Button::new("templates")
                                .xsmall()
                                .ghost()
                                .icon(super::icons::app_icon("book-open"))
                                .label(tr!("settings-tab-templates"))
                                .dropdown_menu(move |mut menu, _window, _cx| {
                                    for (tid, name) in templates.clone() {
                                        let entity = entity.clone();
                                        menu = menu.item(PopupMenuItem::new(name).on_click(
                                            move |_, window, cx| {
                                                entity.update(cx, |this, cx| {
                                                    this.insert_template(tid, window, cx);
                                                });
                                            },
                                        ));
                                    }
                                    menu
                                }),
                        )
                    }),
            )
            .child(
                compose_preview_toggle("toggle-preview", self.preview)
                    .tab_index(61)
                    .disabled(self.sending)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.preview = !this.preview;
                        cx.notify();
                    })),
            )
    }

    /// Writing-assistant panel, folded until the toolbar's button opens it.
    fn render_ai_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let theme = cx.theme().clone();
        self.ai_expanded.then(|| {
            let mut actions = h_flex().gap_1().flex_wrap();
            for preset in self.ai_settings.prompts.clone() {
                let id = preset.id;
                actions = actions.child(
                    Button::new(gpui::ElementId::Name(format!("ai-prompt-{id}").into()))
                        .xsmall()
                        .label(preset.name.clone())
                        .disabled(self.ai_running || self.sending)
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.trigger_ai(preset.clone(), cx)),
                        ),
                );
            }
            actions = actions.when(self.ai_running, |el| {
                el.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(tr!("compose-ai-working")),
                )
            });
            v_flex()
                .tab_group()
                .tab_index(65)
                .tab_stop(false)
                .w_full()
                .gap_2()
                .p_2()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .child(Input::new(&self.ai_prompt).w_full())
                .child(actions)
                .when(!self.ai_stream.is_empty(), |el| {
                    el.child(
                        div()
                            .id("compose-ai-stream")
                            .w_full()
                            .max_h(px(180.))
                            .overflow_y_scroll()
                            .track_scroll(&self.ai_scroll_handle)
                            .p_2()
                            .rounded(theme.radius)
                            .bg(theme.muted)
                            .child(div().text_sm().child(self.ai_stream.clone())),
                    )
                })
        })
    }

    /// One chip per attachment, each carrying its own open/remove menu.
    fn render_attachment_chips(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut file_chips = h_flex()
            .tab_group()
            .tab_index(66)
            .tab_stop(false)
            .gap_1()
            .flex_wrap();
        for (ix, f) in self.files.iter().enumerate() {
            let attachment = f.clone();
            let attachment_to_open = attachment.clone();
            let entity = cx.entity();
            let filename = f.filename.clone();
            let size = attachments::format_size(f.size);
            file_chips = file_chips.child(
                Button::new(gpui::ElementId::Name(format!("file-{ix}").into()))
                    .outline()
                    .small()
                    .h_auto()
                    .max_w(px(280.))
                    .py_1()
                    .px_2()
                    .icon(super::icons::app_icon(attachments::icon_name(f)))
                    .child(
                        v_flex()
                            .min_w_0()
                            .items_start()
                            .child(div().w_full().truncate().text_sm().child(filename))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(size),
                            ),
                    )
                    .tooltip(tr!("compose-attachment-mime-tip", {
                        mime: f.mime.clone()
                    }))
                    .dropdown_menu(move |menu, _window, _cx| {
                        let remove_entity = entity.clone();
                        let attachment_to_open = attachment_to_open.clone();
                        let attachment_to_remove = attachment.clone();
                        menu.item(
                            PopupMenuItem::new(tr!("viewer-attachment-open"))
                                .icon(super::icons::app_icon("external-link"))
                                .on_click(move |_, _, _| {
                                    attachments::open(attachment_to_open.clone());
                                }),
                        )
                        .item(PopupMenuItem::separator())
                        .item(
                            PopupMenuItem::new(tr!("compose-attachment-remove"))
                                .icon(super::icons::app_icon("trash-2"))
                                .on_click(move |_, _, cx| {
                                    remove_entity.update(cx, |this, cx| {
                                        if this.files.get(ix).is_some_and(|file| {
                                            file.filename == attachment_to_remove.filename
                                                && file.size == attachment_to_remove.size
                                                && file.mime == attachment_to_remove.mime
                                        }) {
                                            this.files.remove(ix);
                                        } else if let Some(current_ix) =
                                            this.files.iter().position(|file| {
                                                file.filename == attachment_to_remove.filename
                                                    && file.size == attachment_to_remove.size
                                                    && file.mime == attachment_to_remove.mime
                                            })
                                        {
                                            this.files.remove(current_ix);
                                        }
                                        cx.notify();
                                    });
                                }),
                        )
                    }),
            );
        }
        file_chips
    }

    /// The block editor, or its preview, in the scroll container the surface
    /// asks for: the panel stays bounded inside the reader, the other surfaces
    /// take the height they are given.
    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let panel = self.surface == ComposeSurface::Panel;
        let editor_scroll_handle = self.editor_scroll_handle.clone();
        self.editor_scroll_motion
            .advance(&editor_scroll_handle, window);
        let body_area: gpui::AnyElement = if self.preview {
            let preview = self
                .editor
                .update(cx, |editor, cx| editor.preview_element(window, cx));
            let scroll_handle = editor_scroll_handle.clone();
            div()
                .id("preview-scroll")
                .tab_group()
                .tab_index(50)
                .tab_stop(false)
                .when(panel, |element| {
                    element.w_full().min_h(px(96.)).max_h(px(340.))
                })
                .when(!panel, |element| element.size_full())
                .overflow_y_scroll()
                .track_scroll(&editor_scroll_handle)
                .on_scroll_wheel(cx.listener(
                    move |this, event: &gpui::ScrollWheelEvent, window, cx| {
                        if this
                            .editor_scroll_motion
                            .on_wheel(&scroll_handle, event, window)
                        {
                            cx.notify();
                        }
                    },
                ))
                .p_3()
                .child(preview)
                .into_any_element()
        } else {
            let scroll_handle = editor_scroll_handle.clone();
            div()
                .id("compose-editor-scroll")
                .tab_group()
                .tab_index(50)
                .tab_stop(false)
                .when(panel, |element| {
                    element.w_full().min_h(px(96.)).max_h(px(340.))
                })
                .when(!panel, |element| element.size_full())
                .min_w_0()
                .overflow_y_scroll()
                .when(panel, |element| {
                    element
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.border)
                        .p_1()
                })
                .track_scroll(&editor_scroll_handle)
                .on_scroll_wheel(cx.listener(
                    move |this, event: &gpui::ScrollWheelEvent, window, cx| {
                        if this
                            .editor_scroll_motion
                            .on_wheel(&scroll_handle, event, window)
                        {
                            cx.notify();
                        }
                    },
                ))
                .child(self.editor.clone())
                .into_any_element()
        };

        div()
            .min_w_0()
            .when(panel, |element| element.w_full().px_3())
            .when(!panel, |element| element.flex_1().min_h_0().p_2())
            .child(body_area)
    }

    /// Error line, sender picker, and the draft/send buttons.
    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let panel = self.surface == ComposeSurface::Panel;
        let from_picker = account_selector(
            "from",
            &self.accounts,
            self.from_account_id.as_ref(),
            AccountSelectorKind::Sender,
            70,
            cx.entity(),
            |this: &mut Self, account_id, cx| {
                if this.from_account_id.as_ref() != Some(&account_id) {
                    this.draft_id = None;
                }
                this.from_account_id = Some(account_id);
                // The signature already in the draft is left alone — it may
                // have been chosen deliberately — but the picker now offers
                // the new mailbox's.
                this.refresh_signature_choices(cx);
                cx.notify();
            },
        );

        v_flex()
            .gap_2()
            .when(panel, |element| element.px_3().pb_3())
            .when(!panel, |element| {
                element.p_3().border_t_1().border_color(theme.border)
            })
            .when_some(self.error.clone(), |el, err| {
                el.child(div().text_sm().text_color(theme.danger).child(err))
            })
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(h_flex().flex_1().min_w_0().children(from_picker))
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_2()
                            .child(
                                compose_save_draft_button("save-draft")
                                    .when(panel, |button| button.small())
                                    .tab_index(80)
                                    .disabled(self.sending || self.ai_running)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.trigger_save_draft(cx)),
                                    ),
                            )
                            .child(
                                Button::new("send")
                                    .primary()
                                    .when(panel, |button| button.small())
                                    .tab_index(90)
                                    .icon(super::icons::app_icon("send"))
                                    .label(if self.outbox_queued {
                                        tr!("compose-outbox-waiting")
                                    } else if self.sending {
                                        tr!("compose-sending")
                                    } else {
                                        tr!("compose-send")
                                    })
                                    .disabled(self.sending || self.ai_running)
                                    .loading(self.sending && !self.outbox_queued)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.trigger_send(window, cx)
                                    })),
                            ),
                    ),
            )
    }
}

impl Render for ComposeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        super::theme::apply_window_scale(window, cx);
        let theme = cx.theme().clone();
        let panel = self.surface == ComposeSurface::Panel;

        v_flex()
            .tab_group()
            .key_context("Compose")
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                // Tab walks the header fields in order and then enters the
                // body. A panel has no subject field, so it is skipped.
                let after_recipients =
                    |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                        if this.surface == ComposeSurface::Panel {
                            this.focus_body_entry(window, cx);
                        } else {
                            this.focus_subject(window, cx);
                        }
                    };
                if this.to.read(cx).is_focused(window, cx) {
                    cx.stop_propagation();
                    if this.show_cc {
                        this.focus_cc(window, cx);
                    } else {
                        after_recipients(this, window, cx);
                    }
                    return;
                }
                if this.show_cc && this.cc.read(cx).is_focused(window, cx) {
                    cx.stop_propagation();
                    if this.show_bcc {
                        this.focus_bcc(window, cx);
                    } else {
                        after_recipients(this, window, cx);
                    }
                    return;
                }
                if this.show_bcc && this.bcc.read(cx).is_focused(window, cx) {
                    cx.stop_propagation();
                    after_recipients(this, window, cx);
                    return;
                }
                if this.surface == ComposeSurface::Panel {
                    return;
                }
                let subject_focused = this.subject.read(cx).focus_handle(cx).is_focused(window);
                if this.preview || !subject_focused {
                    return;
                }
                cx.stop_propagation();
                this.focus_body_entry(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &super::shortcuts::SendCompose, window, cx| {
                    if !this.sending && !this.ai_running {
                        this.trigger_send(window, cx);
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &super::shortcuts::SaveDraft, _, cx| {
                if !this.sending && !this.ai_running {
                    this.trigger_save_draft(cx);
                }
            }))
            .when(self.surface.fills_available_space(), |element| {
                element.size_full().bg(theme.background)
            })
            // The reader's body can be thousands of pixels tall: the panel must
            // never shrink in its favour, or it ends up with no height at all.
            .when(panel, |element| element.w_full().min_w_0().flex_none())
            .text_color(theme.foreground)
            .child(self.render_header(cx))
            .child(self.render_body(window, cx))
            .child(self.render_footer(cx))
            .when(!self.sending, |el| {
                el.drag_over::<gpui::ExternalPaths>(|style, _, _, cx| {
                    style
                        .bg(cx.theme().drop_target)
                        .border_color(cx.theme().primary)
                })
                .on_drop(cx.listener(
                    |this, paths: &gpui::ExternalPaths, _, cx| {
                        this.attach_paths(paths.paths().to_vec(), cx);
                    },
                ))
            })
    }
}

fn attachment_from_path(path: &Path) -> Result<Attachment, String> {
    if !path.is_file() {
        return Err(tr!("compose-attachment-file-only").to_string());
    }
    let bytes = std::fs::read(path).map_err(|error| tr!("file-read-error", { error: error }))?;
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| tr!("generic-file").to_string());
    Ok(Attachment {
        id: String::new(),
        size: bytes.len() as u64,
        filename,
        mime: attachments::mime_for_path(path),
        bytes: Some(bytes),
    })
}

impl AviaryApp {
    pub(crate) fn persist_compose_recipient_visibility(&mut self, show_cc: bool, show_bcc: bool) {
        if self.settings.global.compose_show_cc == show_cc
            && self.settings.global.compose_show_bcc == show_bcc
        {
            return;
        }
        self.settings.global.compose_show_cc = show_cc;
        self.settings.global.compose_show_bcc = show_bcc;
        self.settings.save();
    }

    fn on_compose_preference_event(
        &mut self,
        _view: Entity<ComposeView>,
        event: &ComposeEvent,
        _cx: &mut Context<Self>,
    ) {
        if let ComposeEvent::RecipientVisibilityChanged { show_cc, show_bcc } = event {
            self.persist_compose_recipient_visibility(*show_cc, *show_bcc);
        }
    }

    /// Accounts offered as senders, with the same label in all composition modes.
    pub(crate) fn compose_account_options(&self) -> Vec<AccountSelectorOption> {
        self.ordered_accounts()
            .iter()
            .map(|account| {
                let name = self.account_label(account);
                let label = super::account_selector::account_identity_label(name, &account.email);
                let color = util::account_color(
                    &account.id,
                    self.settings
                        .accounts
                        .get(&account.id)
                        .and_then(|settings| settings.color_override),
                );
                AccountSelectorOption {
                    id: account.id.clone(),
                    label,
                    color,
                }
            })
            .collect()
    }

    pub(crate) fn refresh_compose_account_options(&mut self, cx: &mut gpui::App) {
        let accounts = self.compose_account_options();
        for handle in &self.composes {
            let accounts = accounts.clone();
            let _ = handle.view.update(cx, |view, cx| {
                view.accounts = accounts;
                cx.notify();
            });
        }
    }

    /// Pushes the mailboxes' signatures to every open composer, so a signature
    /// added or renamed in Preferences shows up in their pickers without
    /// reopening them. The signature already in a draft is left as it is —
    /// it was rendered when it was inserted, and a draft must not change
    /// under the user.
    pub(crate) fn refresh_compose_signatures(&self, cx: &mut gpui::App) {
        let signatures: Vec<Signature> = self
            .ordered_accounts()
            .into_iter()
            .flat_map(|account| {
                self.settings
                    .account_or_default(Some(&account.id))
                    .signatures
            })
            .collect();
        for handle in &self.composes {
            let signatures = signatures.clone();
            let _ = handle.view.update(cx, |view, cx| {
                view.signatures = signatures;
                view.refresh_signature_choices(cx);
                cx.notify();
            });
        }
    }

    pub(crate) fn refresh_compose_ai_settings(&self, cx: &mut gpui::App) {
        let settings = self.settings.global.ai.clone();
        for handle in &self.composes {
            let settings = settings.clone();
            let _ = handle
                .view
                .update(cx, |view, _| view.set_ai_settings(settings));
        }
    }

    pub(crate) fn reply_all_init(&self, account_id: AccountId, message: &Message) -> ComposeInit {
        let account_email = self
            .account(&account_id)
            .map(|account| account.email.as_str());
        ComposeInit::reply_all(account_id, account_email, message)
    }

    /// Prefixes the body with the sending account's default signature
    /// (`init.from_account_id`) and imports its inline images. Used by the
    /// composer, whichever surface it opens on.
    /// The signature goes in as **one** block, not as its own blocks poured
    /// into the draft: dissolved, nothing said where it began or ended, so it
    /// could neither be named nor swapped — and an imported HTML signature
    /// showed up as an anonymous "HTML fragment".
    pub(crate) fn apply_default_signature(&self, init: &mut ComposeInit) {
        let Some(aid) = &init.from_account_id else {
            return;
        };
        let acc = self.settings.account_or_default(Some(aid));
        if let Some(sig) = acc.signatures.iter().find(|s| s.is_default) {
            if !sig.blocks.is_empty() {
                let mut with_signature = vec![signature_block_kind(sig)];
                if let Some(body_kinds) = &mut init.body_kinds {
                    with_signature.append(body_kinds);
                } else {
                    with_signature.extend(crate::blocks::markdown_to_blocks(&init.body_md));
                    init.body_md.clear();
                }
                init.body_kinds = Some(with_signature);
            }
            for img in &sig.images {
                if !init.inline_images.iter().any(|i| i.cid == img.cid) {
                    init.inline_images.push(img.clone());
                }
            }
        }
    }

    /// Gives the signature blocks of a reopened draft their name back.
    ///
    /// Only the id travels in the mail (`data-aviary-signature-id`): the name
    /// is the user's own wording and has no business leaving the machine. A
    /// provider that strips unknown attributes takes the id with it, so the
    /// fragment's visible text is matched against the mailbox's signatures as
    /// a fallback — the same words are what the user would call the same
    /// signature, whereas the markup may have been reformatted on the way.
    /// Failing both, the block keeps its content and says it was imported: it
    /// can still be swapped for a known signature, which is the point.
    pub(crate) fn resolve_signature_blocks(&self, init: &mut ComposeInit) {
        let Some(kinds) = init.body_kinds.as_mut() else {
            return;
        };
        let unnamed = kinds
            .iter()
            .any(|kind| matches!(kind, BlockKind::Signature { name, .. } if name.is_empty()));
        if !unnamed {
            return;
        }
        let signatures = self
            .settings
            .account_or_default(init.from_account_id.as_ref())
            .signatures;
        name_signature_blocks(kinds, &signatures);
    }

    /// Prefixes the body with the sending mailbox's default template. The
    /// signature has already been added, so the template appears above it and
    /// above a reply quote.
    pub(crate) fn apply_default_template(&self, init: &mut ComposeInit) {
        let Some(aid) = &init.from_account_id else {
            return;
        };
        let acc = self.settings.account_or_default(Some(aid));
        let Some(template) = acc.templates.iter().find(|template| template.is_default) else {
            return;
        };
        if template.blocks.is_empty() {
            return;
        }

        init.default_template_applied = true;
        let mut with_template = template
            .blocks
            .iter()
            .map(|block| block.kind.clone())
            .collect::<Vec<_>>();
        if let Some(body_kinds) = &mut init.body_kinds {
            with_template.append(body_kinds);
        } else {
            with_template.extend(crate::blocks::markdown_to_blocks(&init.body_md));
            init.body_md.clear();
        }
        init.body_kinds = Some(with_template);
        for image in &template.images {
            if !init.inline_images.iter().any(|item| item.cid == image.cid) {
                init.inline_images.push(image.clone());
            }
        }
    }

    /// Shared preparation for both composer modes: ID, sending account,
    /// default signature, address book, and templates.
    fn prepare_compose(
        &mut self,
        init: &mut ComposeInit,
    ) -> (
        u64,
        Vec<AccountSelectorOption>,
        AddressBook,
        Vec<Template>,
        Vec<Signature>,
    ) {
        let id = init
            .compose_id
            .take()
            .unwrap_or_else(|| self.next_editor_id());
        self.compose_seq = self.compose_seq.max(id);
        // Default signature for the sending account, added to the body.
        let from = init
            .from_account_id
            .clone()
            .or_else(|| self.mail_creation_account_id());
        init.from_account_id = from.clone();
        if init.draft_id.is_none() && !init.skip_signature {
            self.apply_default_signature(init);
            self.apply_default_template(init);
        }
        self.resolve_signature_blocks(init);
        let accounts = self.compose_account_options();
        // Recipient history can seed the shared address book before provider
        // contacts have loaded, so emptiness is not a valid readiness check.
        // The per-account maps deduplicate requests that are already loaded or
        // in flight.
        self.ensure_contacts_loaded();
        let address_book = self.address_book.clone();
        // The composer can change sending account after opening, so it keeps
        // templates for every mailbox and filters the menu by
        // `from_account_id` during rendering.
        let templates = self
            .ordered_accounts()
            .into_iter()
            .flat_map(|account| {
                self.settings
                    .account_or_default(Some(&account.id))
                    .templates
            })
            .collect();
        let signatures = self
            .ordered_accounts()
            .into_iter()
            .flat_map(|account| {
                self.settings
                    .account_or_default(Some(&account.id))
                    .signatures
            })
            .collect();
        (id, accounts, address_book, templates, signatures)
    }

    /// Creates a composer on a surface hosted by the main window — a reader-pane
    /// tab or the reply panel — and registers it for the reply routing every
    /// composer shares (`MailSent`, `DraftSaved`, AI streams, outbox
    /// acknowledgements, all keyed by `compose_id`).
    ///
    /// The caller owns the surface: it keeps the returned entity and the
    /// `ComposeEvent` subscription, and decides where the composer is drawn.
    /// `editor_width_hint` is what is left of the pane after that surface's own
    /// margins, which the block editor needs to lay out inline images.
    fn build_hosted_compose(
        &mut self,
        mut init: ComposeInit,
        surface: ComposeSurface,
        editor_width_hint: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (u64, Entity<ComposeView>, Subscription) {
        let initial_focus = ComposeInitialFocus::for_init(&init);
        let (id, accounts, address_book, templates, signatures) = self.prepare_compose(&mut init);
        let cmd_tx = self.cmd_tx.clone();
        let ai_settings = self.settings.global.ai.clone();
        let proofreading_settings = self.settings.global.languagetool.clone();
        let mail_body_options = self.settings.global.mail_body_options();
        let preferred_recipient_visibility = (
            self.settings.global.compose_show_cc,
            self.settings.global.compose_show_bcc,
        );
        let view = cx.new(|cx| {
            ComposeView::new(
                id,
                init,
                cmd_tx,
                accounts,
                address_book,
                templates,
                signatures,
                ai_settings,
                proofreading_settings,
                mail_body_options,
                preferred_recipient_visibility,
                editor_width_hint,
                surface,
                window,
                cx,
            )
        });
        let event_subscription = cx.subscribe_in(&view, window, Self::on_compose_event);
        cx.observe(&view, |this, _, _| this.session_dirty = true)
            .detach();
        let preference_subscription = cx.subscribe(&view, Self::on_compose_preference_event);
        let focus_view = view.clone();
        cx.on_next_frame(window, move |_, window, cx| {
            focus_view.update(cx, |view, cx| {
                view.apply_initial_focus(initial_focus, window, cx);
            });
        });
        self.composes.push(ComposeHandle {
            id,
            window: None,
            view: view.downgrade(),
            pending_view: None,
            _event_subscription: None,
            _preference_subscription: preference_subscription,
        });
        (id, view, event_subscription)
    }

    /// Opens an inline composer in a new reader-pane tab, the default mode.
    /// Multiple compositions may coexist, each in its own tab.
    pub fn open_inline_compose(
        &mut self,
        init: ComposeInit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor_width_hint = self
            .viewer_panel_width(cx)
            .map(|width| width - 16.0)
            .filter(|width| *width >= 40.0);
        let (id, view, sub) =
            self.build_hosted_compose(init, ComposeSurface::Tab, editor_width_hint, window, cx);
        self.inline_composes.push(InlineCompose {
            id,
            view,
            _sub: sub,
        });
        self.open_compose_tab(id);
        cx.notify();
    }

    /// Creates the reply panel's composer. The panel is not a tab: `reply.rs`
    /// keeps it in `inline_reply` and the reader draws it above the body.
    pub(super) fn build_reply_panel_compose(
        &mut self,
        init: ComposeInit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (u64, Entity<ComposeView>, Subscription) {
        let editor_width_hint = self
            .viewer_panel_width(cx)
            // Panel margins, padding, and editor-area border.
            .map(|width| width - 66.0)
            .filter(|width| *width >= 40.0);
        self.build_hosted_compose(init, ComposeSurface::Panel, editor_width_hint, window, cx)
    }

    /// Close or detach an inline composer from the pane banner.
    fn on_compose_event(
        &mut self,
        view: &Entity<ComposeView>,
        ev: &ComposeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let inline_id = self
            .inline_composes
            .iter()
            .find(|c| c.view == *view)
            .map(|c| c.id)
            .or_else(|| {
                self.inline_reply
                    .as_ref()
                    .filter(|reply| reply.view == *view)
                    .map(|reply| reply.compose_id)
            });
        let id = inline_id.or_else(|| {
            self.composes.iter().find_map(|handle| {
                handle
                    .view
                    .upgrade()
                    .is_some_and(|candidate| candidate == *view)
                    .then_some(handle.id)
            })
        });
        let Some(id) = id else {
            return;
        };
        match ev {
            ComposeEvent::Close => {
                self.close_compose(id, cx);
            }
            ComposeEvent::Detach => {
                let init = view.read(cx).to_init(cx);
                self.close_compose(id, cx);
                self.open_compose_window(init, window, cx);
            }
            ComposeEvent::ScheduleSend => {
                let command = view.update(cx, |view, _| view.take_pending_send());
                if let Some(command) = command {
                    if self.hide_reply_panel(id) {
                        // The panel is gone from the reader until the undo
                        // window elapses or the user cancels.
                    } else if inline_id.is_some() {
                        self.hide_compose_tab(id);
                    } else {
                        self.hide_compose_window(id, cx);
                    }
                    self.send_compose_undoable(command, id, window, cx);
                }
            }
            ComposeEvent::RecipientVisibilityChanged { .. } => {}
        }
    }

    pub fn open_compose_window(
        &mut self,
        mut init: ComposeInit,
        main_window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial_focus = ComposeInitialFocus::for_init(&init);
        let (id, accounts, address_book, templates, signatures) = self.prepare_compose(&mut init);
        let cmd_tx = self.cmd_tx.clone();
        let ai_settings = self.settings.global.ai.clone();
        let proofreading_settings = self.settings.global.languagetool.clone();
        let mail_body_options = self.settings.global.mail_body_options();
        let preferred_recipient_visibility = (
            self.settings.global.compose_show_cc,
            self.settings.global.compose_show_bcc,
        );
        let title = compose_kind_title(
            init.draft_id.is_some(),
            init.reply_to.is_some(),
            init.is_forward,
        );
        let bounds = gpui::Bounds::centered(None, gpui::size(px(720.), px(640.)), cx);
        let mut view_slot: Option<Entity<ComposeView>> = None;
        let handle = cx.open_window(
            gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let editor_width_hint = Some(f32::from(window.viewport_size().width) - 16.0)
                    .filter(|width| *width >= 40.0);
                let view = cx.new(|cx| {
                    ComposeView::new(
                        id,
                        init,
                        cmd_tx,
                        accounts,
                        address_book,
                        templates,
                        signatures,
                        ai_settings,
                        proofreading_settings,
                        mail_body_options,
                        preferred_recipient_visibility,
                        editor_width_hint,
                        ComposeSurface::Window,
                        window,
                        cx,
                    )
                });
                let focus_view = view.clone();
                window.on_next_frame(move |window, cx| {
                    focus_view.update(cx, |view, cx| {
                        view.apply_initial_focus(initial_focus, window, cx);
                    });
                });
                view_slot = Some(view.clone());
                cx.new(|cx| Root::new(view, window, cx))
            },
        );
        if let (Ok(window), Some(view)) = (handle, view_slot) {
            cx.observe(&view, |this, _, _| this.session_dirty = true)
                .detach();
            let event_subscription = cx.subscribe_in(&view, main_window, Self::on_compose_event);
            let preference_subscription = cx.subscribe(&view, Self::on_compose_preference_event);
            self.composes.push(ComposeHandle {
                id,
                window: Some(window),
                view: view.downgrade(),
                pending_view: None,
                _event_subscription: Some(event_subscription),
                _preference_subscription: preference_subscription,
            });
        }
        cx.notify();
    }

    /// Optimistically removes a detached composer window while retaining its
    /// entity so undo or a provider failure can restore the exact draft.
    fn hide_compose_window(&mut self, compose_id: u64, cx: &mut Context<Self>) {
        let Some(handle) = self
            .composes
            .iter_mut()
            .find(|handle| handle.id == compose_id)
        else {
            return;
        };
        let Some(window) = handle.window.take() else {
            return;
        };
        handle.pending_view = handle.view.upgrade();
        let _ = window.update(cx, |_, window, _| window.remove_window());
    }

    /// Restores whichever optimistic surface owns this composer: a reader
    /// tab for inline composition, or a new OS window around the retained
    /// entity for detached composition.
    pub(crate) fn restore_compose_surface(&mut self, compose_id: u64, cx: &mut Context<Self>) {
        if self.restore_reply_panel(compose_id) {
            return;
        }
        if self
            .inline_composes
            .iter()
            .any(|compose| compose.id == compose_id)
        {
            self.restore_compose_tab(compose_id);
            return;
        }

        let Some(index) = self
            .composes
            .iter()
            .position(|handle| handle.id == compose_id && handle.window.is_none())
        else {
            return;
        };
        let Some(view) = self.composes[index]
            .pending_view
            .clone()
            .or_else(|| self.composes[index].view.upgrade())
        else {
            return;
        };
        let title = view.read(cx).window_title();
        let bounds = gpui::Bounds::centered(None, gpui::size(px(720.), px(640.)), cx);
        let restored = cx.open_window(
            gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            {
                let view = view.clone();
                move |window, cx| cx.new(|cx| Root::new(view, window, cx))
            },
        );
        if let Ok(window) = restored {
            self.composes[index].window = Some(window);
            self.composes[index].pending_view = None;
        }
    }

    pub fn close_compose(&mut self, compose_id: u64, cx: &mut Context<Self>) {
        // Reply panel: dropping it releases its composer as closing a tab does.
        if self.discard_reply_panel(compose_id) {
            self.composes.retain(|handle| handle.id != compose_id);
            self.session_dirty = true;
            cx.notify();
            return;
        }
        // Inline composer: close its tab, also releasing the entity and handle.
        if let Some(ix) = self
            .mailbox
            .open_tabs
            .iter()
            .position(|t| t.compose_id() == Some(compose_id))
        {
            self.close_viewer_tab(ix);
            cx.notify();
            return;
        }
        // An optimistic composer no longer has a visible surface, but its
        // entity remains alive so undo or an error can restore it.
        self.inline_composes.retain(|c| c.id != compose_id);
        if let Some(ix) = self.composes.iter().position(|c| c.id == compose_id) {
            let handle = self.composes.remove(ix);
            if let Some(win) = handle.window {
                let _ = win.update(cx, |_, window, _| {
                    window.remove_window();
                });
            }
        }
        cx.notify();
    }

    pub fn compose_error(&mut self, compose_id: u64, error: String, cx: &mut Context<Self>) {
        // If optimistic sending fails, make the content editable again.
        self.restore_compose_surface(compose_id, cx);
        if let Some(handle) = self.composes.iter().find(|c| c.id == compose_id) {
            let _ = handle.view.update(cx, |view, cx| view.on_error(error, cx));
        }
    }

    pub fn compose_outbox_queued(&mut self, compose_id: u64, cx: &mut Context<Self>) {
        self.restore_compose_surface(compose_id, cx);
        if let Some(handle) = self.composes.iter().find(|c| c.id == compose_id) {
            let _ = handle.view.update(cx, |view, cx| view.on_outbox_queued(cx));
        }
    }

    pub fn compose_draft_saved(
        &mut self,
        account_id: &AccountId,
        compose_id: u64,
        draft_id: Option<String>,
        autosave: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.composes.iter().find(|c| c.id == compose_id) {
            let _ = handle.view.update(cx, |view, cx| {
                view.on_draft_saved(account_id, draft_id, autosave, cx)
            });
        }
    }

    pub fn compose_draft_error(
        &mut self,
        account_id: &AccountId,
        compose_id: u64,
        error: String,
        autosave: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.composes.iter().find(|c| c.id == compose_id) {
            let _ = handle.view.update(cx, |view, cx| {
                view.on_draft_error(account_id, error, autosave, cx)
            });
        }
    }

    pub fn compose_ai_chunk(
        &mut self,
        compose_id: u64,
        delta: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(handle) = self.composes.iter().find(|handle| handle.id == compose_id) else {
            return false;
        };
        let _ = handle
            .view
            .update(cx, |view, cx| view.on_ai_chunk(delta, cx));
        true
    }

    pub fn compose_ai_finished(
        &mut self,
        compose_id: u64,
        markdown: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(handle) = self.composes.iter().find(|handle| handle.id == compose_id) else {
            return false;
        };
        let _ = handle
            .view
            .update(cx, |view, cx| view.on_ai_finished(markdown, window, cx));
        true
    }

    pub fn compose_ai_error(
        &mut self,
        compose_id: u64,
        error: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(handle) = self.composes.iter().find(|handle| handle.id == compose_id) else {
            return false;
        };
        let _ = handle
            .view
            .update(cx, |view, cx| view.on_ai_error(error, cx));
        true
    }
}

#[cfg(test)]
mod focus_tests {
    use super::{ComposeInit, ComposeInitialFocus};

    #[test]
    fn new_message_starts_at_first_missing_field() {
        let blank = ComposeInit::blank();
        assert!(matches!(
            ComposeInitialFocus::for_init(&blank),
            ComposeInitialFocus::To
        ));

        let with_recipient = ComposeInit::with_to("destinataire@example.com".to_string());
        assert!(matches!(
            ComposeInitialFocus::for_init(&with_recipient),
            ComposeInitialFocus::Subject
        ));

        let complete = ComposeInit {
            to: "destinataire@example.com".to_string(),
            subject: "Subject".to_string(),
            ..ComposeInit::blank()
        };
        assert!(matches!(
            ComposeInitialFocus::for_init(&complete),
            ComposeInitialFocus::Body
        ));
    }

    #[test]
    fn optional_recipients_follow_preference_and_content() {
        let blank = ComposeInit::blank();
        assert_eq!(blank.recipient_visibility(false, false), (false, false));
        assert_eq!(blank.recipient_visibility(true, false), (true, false));
        assert_eq!(blank.recipient_visibility(false, true), (true, true));

        let with_cc = ComposeInit {
            cc: "copie@example.com".to_string(),
            ..ComposeInit::blank()
        };
        assert_eq!(with_cc.recipient_visibility(false, false), (true, false));

        let with_bcc = ComposeInit {
            bcc: "cache@example.com".to_string(),
            ..ComposeInit::blank()
        };
        assert_eq!(with_bcc.recipient_visibility(false, false), (true, true));
    }
}

#[cfg(test)]
mod signature_block_tests {
    use super::*;
    use crate::blocks::Block;

    fn signature(id: i64, name: &str, text: &str) -> Signature {
        Signature {
            id,
            account_id: AccountId::default(),
            name: name.to_string(),
            is_default: false,
            position: 0,
            blocks: vec![Block {
                id: 1,
                kind: BlockKind::Paragraph(text.to_string()),
            }],
            images: Vec::new(),
        }
    }

    fn rendered(signature: &Signature) -> String {
        crate::blocks::build_html_body(&signature.blocks)
    }

    #[test]
    fn a_reopened_signature_block_gets_its_name_from_its_id() {
        let signatures = vec![signature(4, "Pro", "Contact A")];
        let mut kinds = vec![BlockKind::Signature {
            signature_id: Some(4),
            name: String::new(),
            html: rendered(&signatures[0]),
        }];

        name_signature_blocks(&mut kinds, &signatures);

        assert!(
            matches!(&kinds[0], BlockKind::Signature { name, .. } if name == "Pro"),
            "{kinds:?}"
        );
    }

    /// A provider that strips unknown attributes takes the id with it; the
    /// words are then what identifies the signature.
    #[test]
    fn a_signature_stripped_of_its_id_is_recognised_by_its_text() {
        let signatures = vec![
            signature(4, "Pro", "Contact A"),
            signature(5, "Perso", "Contact B"),
        ];
        let mut kinds = vec![BlockKind::Signature {
            signature_id: None,
            name: String::new(),
            // Reformatted by the provider, same words.
            html: "<div><p style=\"margin:0\">Contact B</p></div>".to_string(),
        }];

        name_signature_blocks(&mut kinds, &signatures);

        assert!(
            matches!(
                &kinds[0],
                BlockKind::Signature { signature_id, name, .. }
                    if *signature_id == Some(5) && name == "Perso"
            ),
            "{kinds:?}"
        );
    }

    /// A signature written in another client stays a signature block: unknown
    /// here, but still swappable for one of the mailbox's own.
    #[test]
    fn an_unknown_signature_keeps_its_content_and_says_it_was_imported() {
        let signatures = vec![signature(4, "Pro", "Contact A")];
        let mut kinds = vec![BlockKind::Signature {
            signature_id: Some(99),
            name: String::new(),
            html: "<p>Organisation de test</p>".to_string(),
        }];

        name_signature_blocks(&mut kinds, &signatures);

        let BlockKind::Signature {
            signature_id,
            name,
            html,
        } = &kinds[0]
        else {
            panic!("signature block expected: {kinds:?}");
        };
        assert_eq!(*signature_id, None);
        assert!(!name.is_empty());
        assert!(html.contains("Organisation de test"));
    }

    /// A draft opened, edited and reopened must not be renamed on the way:
    /// only blocks with no name are resolved.
    #[test]
    fn a_named_block_is_left_alone() {
        let mut kinds = vec![BlockKind::Signature {
            signature_id: Some(4),
            name: "Pro".to_string(),
            html: "<p>Contact A</p>".to_string(),
        }];

        name_signature_blocks(&mut kinds, &[]);

        assert!(
            matches!(
                &kinds[0],
                BlockKind::Signature { signature_id, name, .. }
                    if *signature_id == Some(4) && name == "Pro"
            ),
            "{kinds:?}"
        );
    }
}
