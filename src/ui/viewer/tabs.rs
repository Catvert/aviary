//! Reader-pane tabs (new Outlook style): the first chip
//! The selection tab follows the list; the others are pinned messages (kanban
//! double-click or reader pin button) or inline composers (new messages and
//! drafts). The bar appears only when an explicit tab is added to the implicit
//! selection tab.

use super::super::app::AviaryApp;
use super::super::icons;
use super::super::state::{MainView, ViewerTab};
use crate::model::Message;
use gpui::{div, prelude::*, px, Context};
use gpui_component::{h_flex, ActiveTheme, IconName, Sizable};
use std::rc::Rc;

impl AviaryApp {
    // ----------------------------------------------------------------
    // Reader-pane tabs
    // ----------------------------------------------------------------

    /// Tab bar at the top of the reader pane. The selection chip returns
    /// to the message selected in the list. With no other tab, the bar is
    /// visuellement absente.
    pub(super) fn render_message_tabs(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.mailbox.open_tabs.is_empty() {
            return div().into_any_element();
        }

        let theme = cx.theme().clone();
        let active = self.mailbox.active_tab;

        let chip = |el: gpui::Stateful<gpui::Div>, is_active: bool| {
            el.px_2()
                .py_1()
                .rounded(theme.radius)
                .cursor_pointer()
                .text_sm()
                .when(is_active, |el| {
                    el.bg(theme.tab_active)
                        .text_color(theme.tab_active_foreground)
                        .border_1()
                        .border_color(theme.border)
                })
                .when(!is_active, |el| {
                    el.text_color(theme.muted_foreground)
                        .hover(|s| s.bg(theme.list_hover))
                })
        };

        let mut row = h_flex()
            .gap_1()
            .items_center()
            .flex_wrap()
            .px_2()
            .py_1p5()
            .min_h(super::super::inbox::MAIL_PANE_HEADER_HEIGHT)
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.tab_bar);

        row = row.child(
            chip(
                div()
                    .id("msg-tab-selection")
                    .flex()
                    .items_center()
                    .gap_1()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.mailbox.active_tab = None;
                        this.refresh_sender_history_for_displayed();
                        cx.notify();
                    })),
                active.is_none(),
            )
            .child(icons::app_icon("inbox").xsmall())
            .child(tr!("viewer-selection-tab")),
        );

        for (ix, t) in self.mailbox.open_tabs.iter().enumerate() {
            let (icon, title) = match t {
                ViewerTab::Message(m) => (
                    None,
                    if m.header.subject.is_empty() {
                        tr!("no-subject").to_string()
                    } else {
                        m.header.subject.clone()
                    },
                ),
                ViewerTab::Loading(_) => (None, tr!("viewer-message-loading").to_string()),
                ViewerTab::Compose(id) => (
                    Some(icons::app_icon("pencil").xsmall()),
                    self.inline_composes
                        .iter()
                        .find(|c| c.id == *id)
                        .map(|c| c.view.read(cx).tab_title(cx))
                        .unwrap_or_else(|| tr!("compose-title").to_string()),
                ),
            };
            row = row.child(
                chip(
                    div()
                        .id(gpui::ElementId::Name(format!("msg-tab-{ix}").into()))
                        .flex()
                        .items_center()
                        .gap_1()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.mailbox.active_tab = Some(ix);
                            this.refresh_sender_history_for_displayed();
                            cx.notify();
                        })),
                    active == Some(ix),
                )
                .children(icon)
                .child(div().max_w(px(180.)).truncate().child(title))
                .child(
                    div()
                        .id(gpui::ElementId::Name(format!("msg-tab-x-{ix}").into()))
                        .cursor_pointer()
                        .rounded(theme.radius)
                        .hover(|s| s.bg(theme.accent))
                        .child(gpui_component::Icon::new(IconName::Close).xsmall())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_viewer_tab(ix);
                            cx.notify();
                        })),
                ),
            );
        }
        row.into_any_element()
    }

    /// `compose_id` de l'onglet actif s'il s'agit d'un composer.
    pub fn active_compose_tab(&self) -> Option<u64> {
        self.mailbox
            .active_tab
            .and_then(|ix| self.mailbox.open_tabs.get(ix))
            .and_then(|t| t.compose_id())
    }

    /// Opens or activates the message tab and switches to Mail view.
    pub fn open_message_tab(&mut self, message: Message, cx: &mut Context<Self>) {
        self.view = MainView::Mail;
        let id = message.header.id.clone();
        if let Some(ix) = self.mailbox.open_tabs.iter().position(|tab| {
            tab.message_ref().is_some_and(|reference| {
                reference.id == id && reference.account_id == message.header.account_id
            })
        }) {
            self.mailbox.open_tabs[ix] = ViewerTab::Message(Rc::new(message));
            self.mailbox.active_tab = Some(ix);
        } else {
            self.mailbox
                .open_tabs
                .push(ViewerTab::Message(Rc::new(message)));
            self.mailbox.active_tab = Some(self.mailbox.open_tabs.len() - 1);
        }
        self.refresh_sender_history_for_displayed();
        cx.notify();
    }

    /// Adds an inline-composer tab and activates it.
    pub(crate) fn open_compose_tab(&mut self, compose_id: u64) {
        self.view = MainView::Mail;
        self.mailbox.open_tabs.push(ViewerTab::Compose(compose_id));
        self.mailbox.active_tab = Some(self.mailbox.open_tabs.len() - 1);
    }

    /// Removes only a composer's visual representation. The entity remains
    /// alive during the send undo delay.
    pub(crate) fn hide_compose_tab(&mut self, compose_id: u64) {
        let Some(ix) = self
            .mailbox
            .open_tabs
            .iter()
            .position(|tab| tab.compose_id() == Some(compose_id))
        else {
            return;
        };
        self.mailbox.open_tabs.remove(ix);
        self.mailbox.active_tab = match self.mailbox.active_tab {
            Some(active) if active == ix => None,
            Some(active) if active > ix => Some(active - 1),
            other => other,
        };
        self.refresh_sender_history_for_displayed();
    }

    pub(crate) fn restore_compose_tab(&mut self, compose_id: u64) {
        if self
            .mailbox
            .open_tabs
            .iter()
            .any(|tab| tab.compose_id() == Some(compose_id))
            || !self
                .inline_composes
                .iter()
                .any(|compose| compose.id == compose_id)
        {
            return;
        }
        self.open_compose_tab(compose_id);
        self.refresh_sender_history_for_displayed();
    }

    /// Closes a message or composer tab. For a composer, its entity and routing
    /// handle are released, so unsent content is lost.
    pub fn close_viewer_tab(&mut self, ix: usize) {
        if ix >= self.mailbox.open_tabs.len() {
            return;
        }
        let tab = self.mailbox.open_tabs.remove(ix);
        if let Some(id) = tab.compose_id() {
            self.inline_composes.retain(|c| c.id != id);
            self.composes.retain(|c| c.id != id);
        }
        self.mailbox.active_tab = match self.mailbox.active_tab {
            Some(a) if a == ix => None,
            Some(a) if a > ix => Some(a - 1),
            other => other,
        };
        self.refresh_sender_history_for_displayed();
    }
}
