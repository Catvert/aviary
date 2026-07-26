//! The reply panel above the reader's body.
//!
//! It is a regular composer — a [`ComposeView`] on the `Panel` surface — plus
//! the two things that make it a panel rather than a tab: the message whose
//! reader it belongs to, and the reply target it was opened for. Everything
//! else is the composer's own machinery, reached through the same paths as any
//! other composition: sending and its undo window, draft autosave, the AI
//! assistant, templates, attachments, and detaching into a window. All of it is
//! routed back by `compose_id`.
//!
//! Only one panel exists at a time. While an earlier reply is hidden for its
//! send-undo window, a new reply opens in an ordinary reader-pane tab instead,
//! so the two stay independently editable.

use super::super::app::AviaryApp;
use super::super::compose::{ComposeInit, ComposeView};
use super::super::settings::InlineReplySession;
use crate::model::{AccountId, Message};
use gpui::{div, prelude::*, AnyElement, Context, Entity, Subscription, Window};
use gpui_component::ActiveTheme;

pub struct InlineReply {
    pub compose_id: u64,
    /// Currently displayed message: keeps the panel mounted even when the
    /// target is an older message extracted from its quoted body.
    pub message_id: String,
    /// Local target identity, so the same quoted reply is not opened twice.
    pub reply_target_id: String,
    pub view: Entity<ComposeView>,
    _event_subscription: Subscription,
}

impl AviaryApp {
    /// Opens the reply panel above the body of `m`. Initial content (signature,
    /// attribution, and quote) matches the window composer.
    pub fn start_inline_reply(&mut self, m: &Message, window: &mut Window, cx: &mut Context<Self>) {
        let init = ComposeInit::reply(m.header.account_id.clone(), m);
        self.start_inline_reply_with_init(
            m.header.id.clone(),
            m.header.id.clone(),
            init,
            window,
            cx,
        );
    }

    /// Opens a reply including all message recipients.
    pub fn start_inline_reply_all(
        &mut self,
        m: &Message,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let init = self.reply_all_init(m.header.account_id.clone(), m);
        self.start_inline_reply_with_init(
            m.header.id.clone(),
            m.header.id.clone(),
            init,
            window,
            cx,
        );
    }

    /// Replies to an older message extracted from the displayed body's quoted
    /// history. That message has no provider object in the account, as with a
    /// forward, so `reply_to` stays empty and only the historical recipients
    /// are carried over.
    pub(super) fn start_inline_quoted_reply(
        &mut self,
        displayed_message_id: String,
        quoted: &Message,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut init = self.reply_all_init(quoted.header.account_id.clone(), quoted);
        init.reply_to = None;
        self.start_inline_reply_with_init(
            displayed_message_id,
            quoted.header.id.clone(),
            init,
            window,
            cx,
        );
    }

    fn start_inline_reply_with_init(
        &mut self,
        displayed_message_id: String,
        reply_target_id: String,
        init: ComposeInit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pending) = &self.pending_inline_reply {
            if pending.reply_target_id == reply_target_id {
                return;
            }
            // The panel is taken by a reply awaiting its send-undo window.
            self.open_inline_compose(init, window, cx);
            return;
        }
        if self
            .inline_reply
            .as_ref()
            .is_some_and(|reply| reply.reply_target_id == reply_target_id)
        {
            return;
        }
        let (compose_id, view, event_subscription) =
            self.build_reply_panel_compose(init, window, cx);
        self.inline_reply = Some(InlineReply {
            compose_id,
            message_id: displayed_message_id,
            reply_target_id,
            view,
            _event_subscription: event_subscription,
        });
        cx.notify();
    }

    pub(crate) fn inline_reply_session(&self, cx: &gpui::App) -> Option<InlineReplySession> {
        let reply = self.inline_reply.as_ref()?;
        Some(InlineReplySession {
            displayed_message_id: reply.message_id.clone(),
            reply_target_id: reply.reply_target_id.clone(),
            compose: reply.view.read(cx).to_init(cx),
        })
    }

    pub(crate) fn restore_inline_reply_session(
        &mut self,
        session: Option<InlineReplySession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = session else { return };
        self.start_inline_reply_with_init(
            session.displayed_message_id,
            session.reply_target_id,
            session.compose,
            window,
            cx,
        );
    }

    /// Reply panel rendered above the body when the active reply targets the
    /// displayed message. The composer draws itself; what belongs to the reader
    /// is the frame around it and its width.
    pub(super) fn render_inline_reply(
        &mut self,
        m: &Message,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = self
            .inline_reply
            .as_ref()
            .filter(|reply| reply.message_id == m.header.id)
            .map(|reply| reply.view.clone())?;
        let theme = cx.theme().clone();
        Some(
            div()
                .w_full()
                .min_w_0()
                .when_some(self.viewer_layout_width, |el, width| {
                    // `.mx_2()` means 8 px on each side.
                    let width = gpui::px((width - 16.0).max(40.0).floor());
                    el.w(width).min_w(width).max_w(width)
                })
                .mx_2()
                .mt_3()
                .border_1()
                .border_color(theme.border)
                .rounded(theme.radius)
                .bg(theme.popover)
                .child(view)
                .into_any_element(),
        )
    }

    /// Hides the panel for the duration of a send's undo window, keeping its
    /// composer alive so cancelling puts it back untouched.
    pub(crate) fn hide_reply_panel(&mut self, compose_id: u64) -> bool {
        if self
            .inline_reply
            .as_ref()
            .is_some_and(|reply| reply.compose_id == compose_id)
        {
            self.pending_inline_reply = self.inline_reply.take();
            return true;
        }
        false
    }

    /// Puts back a panel hidden by [`Self::hide_reply_panel`].
    pub(crate) fn restore_reply_panel(&mut self, compose_id: u64) -> bool {
        if self
            .pending_inline_reply
            .as_ref()
            .is_some_and(|reply| reply.compose_id == compose_id)
        {
            self.inline_reply = self.pending_inline_reply.take();
            return true;
        }
        self.inline_reply
            .as_ref()
            .is_some_and(|reply| reply.compose_id == compose_id)
    }

    /// Drops the panel, displayed or hidden, when its composer is closed.
    pub(crate) fn discard_reply_panel(&mut self, compose_id: u64) -> bool {
        let displayed = self
            .inline_reply
            .as_ref()
            .is_some_and(|reply| reply.compose_id == compose_id);
        if displayed {
            self.inline_reply = None;
        }
        let hidden = self
            .pending_inline_reply
            .as_ref()
            .is_some_and(|reply| reply.compose_id == compose_id);
        if hidden {
            self.pending_inline_reply = None;
        }
        displayed || hidden
    }

    /// True while this account owns the panel's composer: closing the account
    /// has to take the panel with it.
    pub(crate) fn reply_panel_belongs_to(
        reply: &InlineReply,
        account_id: &AccountId,
        cx: &gpui::App,
    ) -> bool {
        reply.view.read(cx).belongs_to_account(account_id)
    }
}
