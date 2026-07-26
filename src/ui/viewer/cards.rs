//! The collapsible cards the reader draws around the displayed message.
//!
//! Above it, in reverse chronological order, come the messages that answered
//! it: our own replies and forwards, and the other party's follow-ups. Below it
//! comes the rest of the conversation as a flat list.
//!
//! A card's body has two possible origins, and that is the only real difference
//! between the cards above the message: Aviary either already holds the message
//! (the local snapshot of something we just sent) or knows only its header, in
//! which case expanding it fetches the body through `thread_bodies`. Both are
//! [`CardBody`], and [`AviaryApp::render_sent_card`] renders either.

use super::super::app::AviaryApp;
use super::super::state::ThreadBodyState;
use super::super::util;
use super::{quoted_body, snapshot_covers, subject_adds_context};
use crate::model::{AccountId, LastAction, Message, MessageRef};
use crate::runtime::Cmd;
use crate::ui::settings::{BodyViewMode, MailBodyOptions};
use gpui::{div, prelude::*, rems, AnyElement, Context, ElementId, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex, v_flex, ActiveTheme, Icon, IconName, Sizable, StyledExt,
};
use std::collections::HashSet;

/// Where an expanded card takes its body from.
enum CardBody {
    /// A message Aviary already holds: the local snapshot of a reply or forward
    /// we sent. Expansion is remembered by id in `expanded_sent_messages`, and
    /// may carry the deferred lookup of the provider's own Sent-items copy —
    /// which survives restarts, where the send-time `FetchSentCopy` is long
    /// gone.
    Held {
        message: Box<Message>,
        resolve: Option<ResolveSentCopy>,
    },
    /// A message known by header only. Expanding it loads the body through the
    /// regular lazy-thread machinery, which also makes its recipients and
    /// attachments available.
    Fetched {
        account_id: AccountId,
        state: Option<ThreadBodyState>,
    },
}

/// The deferred lookup of a snapshot's provider copy, kept as data rather than
/// as a built `Cmd`: the header's click listener may fire more than once, and
/// `Cmd` is not `Clone`.
#[derive(Clone)]
struct ResolveSentCopy {
    account_id: AccountId,
    related_to: String,
    snapshot_id: String,
    sent_id: Option<String>,
    internet_message_id: Option<String>,
}

impl ResolveSentCopy {
    fn command(&self) -> Cmd {
        Cmd::FetchSentCopy {
            account_id: self.account_id.clone(),
            related_to: self.related_to.clone(),
            snapshot_id: self.snapshot_id.clone(),
            sent_id: self.sent_id.clone(),
            internet_message_id: self.internet_message_id.clone(),
        }
    }
}

/// One card above the displayed message.
struct SentCard {
    /// Element-id prefix. Distinct per card kind so two cards about the same
    /// message never collide.
    id_prefix: &'static str,
    message_id: String,
    icon: &'static str,
    label: String,
    received: chrono::DateTime<chrono::Utc>,
    /// Shown only when it adds something to the displayed message's subject.
    subject: Option<String>,
    /// Collapsed preview; empty when there is nothing worth showing.
    preview: String,
    /// `None` while a snapshot's provider copy is still unresolved: there is no
    /// message to jump to yet.
    jump_target: Option<MessageRef>,
    expanded: bool,
    body: CardBody,
}

impl AviaryApp {
    /// The cards above the displayed message, newest first, ending just above
    /// it. Local snapshots and promoted thread messages are interleaved by
    /// date, since both describe the same conversation moving forward.
    pub(super) fn render_sent_messages(
        &mut self,
        source: &Message,
        mode: BodyViewMode,
        viewer_panel_width: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if mode == BodyViewMode::Source {
            return Vec::new();
        }
        let snapshots = self
            .mailbox
            .sent_messages
            .get(&source.header.id)
            .cloned()
            .unwrap_or_default();
        let promoted = self.thread_newer_messages(source);
        let options = self.settings.global.mail_body_options();
        let mut cards = Vec::with_capacity(snapshots.len() + promoted.len());

        for sent in snapshots {
            // A snapshot still waiting for its Sent-items copy retries the
            // lookup when the user expands it. That covers restarts, where the
            // send-time `FetchSentCopy` is long gone.
            let resolve = sent.needs_resolution().then(|| ResolveSentCopy {
                account_id: sent.message.header.account_id.clone(),
                related_to: source.header.id.clone(),
                snapshot_id: sent.message.header.id.clone(),
                sent_id: sent.sent_id.clone(),
                internet_message_id: sent.internet_message_id.clone(),
            });
            let jump_target = if sent.is_snapshot() {
                sent.sent_id.clone()
            } else {
                Some(sent.message.header.id.clone())
            }
            .map(|id| MessageRef {
                account_id: sent.message.header.account_id.clone(),
                id,
            });
            let (icon, label) = match sent.action {
                LastAction::Replied => ("reply", tr!("viewer-sent-reply")),
                LastAction::RepliedAll => ("reply-all", tr!("viewer-sent-reply-all")),
                LastAction::Forwarded => ("forward", tr!("viewer-sent-forward")),
            };
            let message = sent.message;
            let message_id = message.header.id.clone();
            cards.push(SentCard {
                id_prefix: "sent-head",
                icon,
                label: label.to_string(),
                received: message.header.received,
                subject: subject_adds_context(&message.header.subject, &source.header.subject)
                    .then(|| message.header.subject.clone()),
                preview: quoted_body::markdown_preview(&message.body),
                jump_target,
                expanded: self.mailbox.expanded_sent_messages.contains(&message_id),
                message_id,
                body: CardBody::Held { message, resolve },
            });
        }

        for header in promoted {
            let (icon, label) = if self.is_own_address(&header.account_id, &header.from) {
                // Forwards never share the source's conversation, so our own
                // promoted message is a reply; only the reply/reply-all nuance
                // can be taken from the recorded action.
                match source.header.last_action {
                    Some(LastAction::RepliedAll) => ("reply-all", tr!("viewer-sent-reply-all")),
                    _ => ("reply", tr!("viewer-sent-reply")),
                }
            } else {
                (
                    "mail",
                    tr!("viewer-thread-reply-from", { name: util::display_name(&header.from) }),
                )
            };
            let state = self.mailbox.thread_bodies.get(&header.id).cloned();
            cards.push(SentCard {
                id_prefix: "sent-thr",
                icon,
                label: label.to_string(),
                received: header.received,
                subject: subject_adds_context(&header.subject, &source.header.subject)
                    .then(|| header.subject.clone()),
                preview: util::clean_preview(&header.preview),
                jump_target: Some(MessageRef {
                    account_id: header.account_id.clone(),
                    id: header.id.clone(),
                }),
                expanded: state.is_some(),
                message_id: header.id.clone(),
                body: CardBody::Fetched {
                    account_id: header.account_id,
                    state,
                },
            });
        }

        cards.sort_by_key(|card| std::cmp::Reverse(card.received));
        cards
            .into_iter()
            .map(|card| self.render_sent_card(card, mode, options, viewer_panel_width, window, cx))
            .collect()
    }

    fn render_sent_card(
        &mut self,
        card: SentCard,
        mode: BodyViewMode,
        options: MailBodyOptions,
        viewer_panel_width: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let SentCard {
            id_prefix,
            message_id,
            icon,
            label,
            received,
            subject,
            preview,
            jump_target,
            expanded,
            body,
        } = card;
        // A held message already carries its recipients; a fetched one only
        // learns them once expanded, and shows them in its details instead.
        let recipients = match &body {
            CardBody::Held { message, .. } => Some(message.to.join(", "))
                .filter(|recipients| !recipients.is_empty())
                .map(|recipients| tr!("viewer-sent-to", { recipients: recipients })),
            CardBody::Fetched { .. } => None,
        };

        let header = h_flex()
            .id(ElementId::Name(format!("{id_prefix}-{message_id}").into()))
            .w_full()
            .min_w_0()
            .gap_2()
            .items_center()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.muted)
            .cursor_pointer()
            .hover(|style| style.bg(theme.list_hover))
            .child(expand_chevron(expanded))
            .child(
                v_flex()
                    .flex_1()
                    .w_full()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .child(
                                h_flex()
                                    .h_5()
                                    .flex_none()
                                    .items_center()
                                    .child(super::super::icons::app_icon(icon).xsmall()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_semibold()
                                    .text_sm()
                                    .truncate()
                                    .child(label),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(util::short_date(&received)),
                            )
                            .children(jump_target.map(|target| {
                                jump_button(format!("{id_prefix}-jump-{message_id}"), target, cx)
                            })),
                    )
                    .children(subject.map(|subject| {
                        div()
                            .w_full()
                            .min_w_0()
                            .pl_5()
                            .truncate()
                            .text_sm()
                            .child(subject)
                    }))
                    .children(recipients.map(|recipients| {
                        div()
                            .w_full()
                            .min_w_0()
                            .pl_5()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(recipients)
                    }))
                    .when(!expanded && !preview.is_empty(), |element| {
                        element.child(
                            div()
                                .w_full()
                                .min_w_0()
                                .pl_5()
                                .line_clamp(2)
                                .text_sm()
                                .line_height(rems(1.25))
                                .text_color(theme.muted_foreground)
                                .child(preview),
                        )
                    }),
            )
            .on_click(cx.listener({
                let message_id = message_id.clone();
                let toggle = match &body {
                    CardBody::Held { resolve, .. } => Toggle::Held(resolve.clone()),
                    CardBody::Fetched { account_id, .. } => Toggle::Fetched(account_id.clone()),
                };
                move |this, _, _, cx| {
                    match &toggle {
                        Toggle::Held(resolve) => {
                            if !this.mailbox.expanded_sent_messages.remove(&message_id) {
                                this.mailbox
                                    .expanded_sent_messages
                                    .insert(message_id.clone());
                                if let Some(resolve) = resolve {
                                    this.send(resolve.command());
                                }
                            }
                        }
                        Toggle::Fetched(account_id) => {
                            this.toggle_thread_body(account_id, &message_id)
                        }
                    }
                    cx.notify();
                }
            }));

        let mut card = v_flex().w_full().min_w_0().child(header);
        if expanded {
            let details = match body {
                CardBody::Held { message, .. } => Some(self.render_card_details(
                    &message,
                    true,
                    mode,
                    options,
                    viewer_panel_width,
                    window,
                    cx,
                )),
                CardBody::Fetched { state, .. } => match state {
                    Some(ThreadBodyState::Loaded(message)) => Some(self.render_card_details(
                        &message,
                        false,
                        mode,
                        options,
                        viewer_panel_width,
                        window,
                        cx,
                    )),
                    Some(state) => lazy_body_status(&state, cx)
                        .map(|status| div().px_3().py_2().child(status).into_any_element()),
                    None => None,
                },
            };
            card = card.children(details);
        }
        div()
            .w_full()
            .min_w_0()
            .px_4()
            .pt_3()
            .child(card)
            .into_any_element()
    }

    /// The indented block under an expanded card: the address lines its header
    /// does not already show, its attachments, then the body itself.
    #[allow(clippy::too_many_arguments)]
    fn render_card_details(
        &mut self,
        message: &Message,
        header_shows_recipients: bool,
        mode: BodyViewMode,
        options: MailBodyOptions,
        viewer_panel_width: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let mut details = v_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .px_3()
            .py_2()
            .border_l_2()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground);
        let recipients = message.to.join(", ");
        if !header_shows_recipients && !recipients.is_empty() {
            details = details.child(tr!("viewer-sent-to", { recipients: recipients }));
        }
        let cc = message.cc.join(", ");
        if !cc.is_empty() {
            details = details.child(tr!("viewer-sent-cc", { recipients: cc }));
        }
        let bcc = message.bcc.join(", ");
        if !bcc.is_empty() {
            details = details.child(tr!("viewer-sent-bcc", { recipients: bcc }));
        }
        let attachments = message
            .attachments
            .iter()
            .map(|attachment| attachment.filename.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if !attachments.is_empty() {
            details = details.child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(super::super::icons::app_icon("paperclip").xsmall())
                    .child(tr!("viewer-sent-attachments", { files: attachments })),
            );
        }
        details
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .pt_2()
                    .text_color(theme.foreground)
                    .child(self.render_message_body(
                        message,
                        mode,
                        options,
                        // `.px_3()` on the details plus `.px_4()` on the card.
                        inner_body_width(viewer_panel_width, 56.0),
                        window,
                        cx,
                    )),
            )
            .into_any_element()
    }

    /// The rest of the conversation, below the displayed message: a flat list
    /// of one-line rows, each expanding into its body on demand.
    pub(super) fn render_thread(
        &mut self,
        m: &Message,
        mode: BodyViewMode,
        viewer_panel_width: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if mode == BodyViewMode::Source {
            return Vec::new();
        }
        // The loaded thread corresponds to the selection; `thread_without_
        // duplicates` also checks it belongs to the displayed message's
        // conversation, and drops the other mailbox copies of that message.
        let local_sent = self.mailbox.sent_messages.get(&m.header.id);
        // Thread messages promoted to cards above the source
        // (`thread_newer_messages`) leave the plain conversation list.
        let promoted: HashSet<String> = self
            .thread_newer_messages(m)
            .into_iter()
            .map(|h| h.id)
            .collect();
        let others: Vec<_> = self
            .thread_without_duplicates(&m.header)
            .into_iter()
            .filter(|h| {
                !promoted.contains(&h.id)
                    && !local_sent.is_some_and(|messages| {
                        messages.iter().any(|sent| snapshot_covers(sent, h))
                    })
            })
            .collect();
        if others.is_empty() {
            return Vec::new();
        }
        let theme = cx.theme().clone();
        let options = self.settings.global.mail_body_options();
        let mut out: Vec<AnyElement> = vec![div()
            .px_4()
            .pt_2()
            .pb_1()
            .text_sm()
            .font_semibold()
            .text_color(theme.muted_foreground)
            .child(tr!("conversation-count", { count: others.len() }))
            .into_any_element()];

        for h in others {
            let state = self.mailbox.thread_bodies.get(&h.id).cloned();
            let expanded = state.is_some();
            let account_id = h.account_id.clone();
            let id = h.id.clone();
            out.push(
                h_flex()
                    .id(ElementId::Name(format!("thr-{}", h.id).into()))
                    .gap_2()
                    .items_center()
                    .px_4()
                    .py_1p5()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.list_hover))
                    .child(expand_chevron(expanded))
                    .child(div().flex_1().truncate().text_sm().child(format!(
                        "{} — {}",
                        util::display_name(&h.from),
                        h.subject
                    )))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(util::short_date(&h.received)),
                    )
                    .child(jump_button(
                        format!("thr-jump-{}", h.id),
                        MessageRef {
                            account_id: account_id.clone(),
                            id: id.clone(),
                        },
                        cx,
                    ))
                    .on_click(cx.listener({
                        let account_id = account_id.clone();
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.toggle_thread_body(&account_id, &id);
                            cx.notify();
                        }
                    }))
                    .into_any_element(),
            );

            let Some(state) = state else { continue };
            match state {
                ThreadBodyState::Loaded(message) => out.push(
                    div()
                        .w_full()
                        .min_w_0()
                        .px_8()
                        .py_2()
                        .border_l_2()
                        .border_color(theme.border)
                        .child(self.render_message_body(
                            &message,
                            mode,
                            options,
                            inner_body_width(viewer_panel_width, 64.0), // `.px_8()`
                            window,
                            cx,
                        ))
                        .into_any_element(),
                ),
                state => out.extend(
                    lazy_body_status(&state, cx)
                        .map(|status| div().px_8().py_2().child(status).into_any_element()),
                ),
            }
        }
        out
    }

    /// Expands a thread message by requesting its body, or collapses it. The
    /// presence of an entry in `thread_bodies` *is* the expanded state, so
    /// collapsing simply drops it.
    fn toggle_thread_body(&mut self, account_id: &AccountId, id: &str) {
        if self.mailbox.thread_bodies.remove(id).is_none() {
            self.mailbox
                .thread_bodies
                .insert(id.to_string(), ThreadBodyState::Loading);
            self.send(Cmd::LoadThreadMessage {
                account_id: account_id.clone(),
                id: id.to_string(),
            });
        }
    }
}

/// Shared toggle intent, so the click listener does not have to borrow the card
/// body it is rendered from.
enum Toggle {
    Held(Option<ResolveSentCopy>),
    Fetched(AccountId),
}

fn expand_chevron(expanded: bool) -> Icon {
    Icon::new(if expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .xsmall()
}

/// Opens the message in the reader rather than expanding it in place.
fn jump_button(
    id: impl Into<String>,
    target: MessageRef,
    cx: &mut Context<AviaryApp>,
) -> impl IntoElement {
    Button::new(ElementId::Name(id.into().into()))
        .ghost()
        .xsmall()
        .icon(super::super::icons::app_icon("arrow-right"))
        .tooltip(tr!("viewer-go-to-message"))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.open_message(target.account_id.clone(), target.id.clone(), cx);
        }))
}

/// What an expanded card shows while its body is still being fetched, or when
/// the fetch failed. `Loaded` is the caller's business, and so is the padding —
/// a card and a thread row do not indent their bodies the same way.
fn lazy_body_status(state: &ThreadBodyState, cx: &mut Context<AviaryApp>) -> Option<AnyElement> {
    let theme = cx.theme();
    let (text, color) = match state {
        ThreadBodyState::Loading => (tr!("loading"), theme.muted_foreground),
        ThreadBodyState::Error(error) => (error.clone().into(), theme.danger),
        ThreadBodyState::Loaded(_) => return None,
    };
    Some(
        div()
            .text_sm()
            .text_color(color)
            .child(text)
            .into_any_element(),
    )
}

/// Blitz lays a body out at an explicit width; an indented card has to hand it
/// what is left after its own padding, and nothing at all if that is too narrow
/// to be meaningful.
fn inner_body_width(viewer_panel_width: Option<f32>, padding: f32) -> Option<f32> {
    viewer_panel_width
        .map(|width| width - padding)
        .filter(|width| *width >= 40.0)
}
