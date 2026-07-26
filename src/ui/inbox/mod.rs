//! Mail view: three resizable panes: folders,
//! folder tree ([`folders`]), message list ([`messages`]), and reader (`viewer`),
//! plus sender history below the reader.

pub(super) mod folders;
mod message_menu;
pub(super) mod messages;

use super::app::AviaryApp;
use super::state::SenderHistoryState;
use crate::runtime::Cmd;
use gpui::{div, prelude::*, px, Context, Rems, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    resizable::{h_resizable, resizable_panel, v_resizable},
    v_flex, ActiveTheme, Disableable, Sizable, StyledExt,
};

/// Shared minimum height for the folder, message-list, and viewer headers.
pub(super) const MAIL_PANE_HEADER_HEIGHT: Rems = Rems(3.);

impl AviaryApp {
    pub fn render_inbox(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_resizable("inbox-sidebar-panes")
            .with_state(&self.sidebar_resize)
            .child(
                super::app::sidebar_panel()
                    .child(self.render_folders_pane(window, cx).into_any_element()),
            )
            .child(
                resizable_panel().child(
                    h_resizable("inbox-content-panes")
                        .with_state(&self.inbox_resize_h)
                        .child(
                            resizable_panel()
                                .size(px(380.))
                                .size_range(px(260.)..px(640.))
                                .child(self.render_messages_pane(window, cx).into_any_element()),
                        )
                        .child(resizable_panel().child(
                            // Sender history is not relevant while composing:
                            // the composer occupies the entire right pane.
                            if self.active_compose_tab().is_some() {
                                self.render_viewer_pane(window, cx).into_any_element()
                            } else if self.sender_history_expanded {
                                v_resizable("viewer-history")
                                    .with_state(&self.inbox_resize_v)
                                    .child(resizable_panel().child(
                                        self.render_viewer_pane(window, cx).into_any_element(),
                                    ))
                                    .child(
                                        resizable_panel()
                                            .size(px(180.))
                                            .size_range(px(100.)..px(420.))
                                            .child(
                                                self.render_history_pane(window, cx)
                                                    .into_any_element(),
                                            ),
                                    )
                                    .into_any_element()
                            } else {
                                v_flex()
                                    .size_full()
                                    .child(div().flex_1().min_h_0().child(
                                        self.render_viewer_pane(window, cx).into_any_element(),
                                    ))
                                    .child(self.render_history_header(false, cx))
                                    .into_any_element()
                            },
                        )),
                ),
            )
    }

    // ------------------------------------------------------------
    // Sender history
    // ------------------------------------------------------------

    fn render_history_pane(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let body: gpui::AnyElement = match &self.sender_history {
            SenderHistoryState::Idle => div()
                .p_3()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(tr!("history-select-message"))
                .into_any_element(),
            SenderHistoryState::Loading { email } => div()
                .p_3()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(tr!("history-loading-for", { email: email }))
                .into_any_element(),
            SenderHistoryState::Loaded {
                email,
                messages,
                next,
                loading_more,
            } => {
                let mut rows = v_flex().gap_0p5().p_2();
                for m in messages {
                    rows = rows.child(self.message_row(m, false, "sender-history", cx));
                }
                if let Some(next_link) = next.clone() {
                    let email = email.clone();
                    let loading = *loading_more;
                    rows = rows.child(
                        Button::new("history-more")
                            .ghost()
                            .xsmall()
                            .label(if loading {
                                tr!("loading")
                            } else {
                                tr!("load-more-short")
                            })
                            .disabled(loading)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let SenderHistoryState::Loaded { loading_more, .. } =
                                    &mut this.sender_history
                                {
                                    *loading_more = true;
                                }
                                for aid in this.active_account_ids() {
                                    this.send(Cmd::LoadMoreSenderHistory {
                                        account_id: aid,
                                        email: email.clone(),
                                        next_link: next_link.clone(),
                                    });
                                }
                                cx.notify();
                            })),
                    );
                }
                div()
                    .id("history-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(rows)
                    .into_any_element()
            }
        };

        v_flex()
            .size_full()
            .child(self.render_history_header(true, cx))
            .child(div().flex_1().min_h_0().child(body))
    }

    fn render_history_header(&self, expanded: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let title = match self.sender_history.email() {
            Some(email) => tr!("history-title-for", { email: email }),
            None => tr!("history-title"),
        };

        h_flex()
            .id("sender-history-toggle")
            .w_full()
            .gap_1p5()
            .px_3()
            .py_1p5()
            .border_t_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(|s| s.bg(theme.list_hover))
            .child(
                gpui_component::Icon::new(if expanded {
                    gpui_component::IconName::ChevronDown
                } else {
                    gpui_component::IconName::ChevronRight
                })
                .xsmall()
                .text_color(theme.muted_foreground),
            )
            .child(div().font_semibold().text_sm().child(title))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.sender_history_expanded = !expanded;
                if !expanded {
                    this.refresh_sender_history_for_displayed();
                }
                cx.notify();
            }))
            .into_any_element()
    }

    /// Loads history only while its panel is open. A
    /// result already loaded (or loading) for the same sender is retained.
    pub(super) fn refresh_sender_history_for_displayed(&mut self) {
        if !self.sender_history_expanded {
            return;
        }
        let Some(message) = self.displayed_message() else {
            self.sender_history = SenderHistoryState::Idle;
            return;
        };
        let Some(email) = super::util::extract_email(&message.header.from) else {
            self.sender_history = SenderHistoryState::Idle;
            return;
        };
        if self.sender_history.email() == Some(email.as_str()) {
            return;
        }

        let account_id = message.header.account_id.clone();
        let limit = self
            .settings
            .account_or_default(Some(&account_id))
            .sender_history_limit;
        self.sender_history = SenderHistoryState::Loading {
            email: email.clone(),
        };
        self.send(Cmd::LoadSenderHistory {
            account_id,
            email,
            limit,
        });
    }
}

impl SenderHistoryState {
    pub fn email(&self) -> Option<&str> {
        match self {
            Self::Idle => None,
            Self::Loading { email } | Self::Loaded { email, .. } => Some(email),
        }
    }
}
