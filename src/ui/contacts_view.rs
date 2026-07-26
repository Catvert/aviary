//! Contacts view: filterable list on the left, details and history on the right.

use super::app::AviaryApp;
use super::state::{MainView, SenderHistoryState};
use crate::model::Contact;
use crate::runtime::Cmd;
use gpui::{
    div, prelude::*, px, AvailableSpace, Context, ScrollStrategy, ScrollWheelEvent, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::Input,
    v_flex, v_virtual_list, ActiveTheme, IconName, Sizable, StyledExt,
};
use std::ops::Range;
use std::rc::Rc;

impl AviaryApp {
    pub fn render_contacts(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.ensure_contacts_loaded();
        super::app::sidebar_layout(
            "contacts-panes",
            self.sidebar_resize.clone(),
            self.render_contacts_list(window, cx).into_any_element(),
            self.render_contact_detail(window, cx).into_any_element(),
        )
    }

    fn render_contacts_list(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let contacts = Rc::new(self.visible_contacts());

        // Virtual list: rows are uniform, so measure the first one offscreen
        // to determine item height.
        let row_h = contacts.first().map(|c| {
            let mut el = self.contact_row(c, cx);
            el.layout_as_root(
                gpui::size(AvailableSpace::MinContent, AvailableSpace::MinContent),
                window,
                cx,
            )
            .height
        });
        let sizes: Rc<Vec<gpui::Size<gpui::Pixels>>> =
            Rc::new(vec![
                gpui::size(px(0.), row_h.unwrap_or_default());
                contacts.len()
            ]);

        let empty = contacts.is_empty();
        let has_unfiltered_contacts = !self.contacts.list.is_empty();
        let loading = self
            .active_account_ids()
            .iter()
            .any(|aid| self.contacts.loading_accounts.contains(aid));
        let base_handle = self.scrolls.contacts.handle.base_handle().clone();
        self.scrolls.contacts.motion.advance(&base_handle, window);
        let body = if empty {
            div()
                .flex_1()
                .min_h_0()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(if loading {
                    tr!("contacts-loading")
                } else if has_unfiltered_contacts {
                    tr!("contacts-no-results")
                } else {
                    tr!("contacts-empty")
                })
                .into_any_element()
        } else {
            // Non-scrollable wrapper: an ancestor's wheel listener runs after
            // the virtual list's internal handler (see `ui/motion.rs`).
            div()
                .id("contacts-scroll")
                .flex_1()
                .min_h_0()
                .on_scroll_wheel(cx.listener({
                    let handle = base_handle;
                    move |this, event: &ScrollWheelEvent, window, cx| {
                        if this
                            .scrolls
                            .contacts
                            .motion
                            .on_wheel(&handle, event, window)
                        {
                            cx.notify();
                        }
                    }
                }))
                .child(
                    v_virtual_list(cx.entity(), "contacts-vlist", sizes, {
                        let contacts = contacts.clone();
                        move |this, range: Range<usize>, _window, cx| {
                            range
                                .filter_map(|ix| contacts.get(ix).map(|c| this.contact_row(c, cx)))
                                .collect::<Vec<_>>()
                        }
                    })
                    .track_scroll(&self.scrolls.contacts.handle)
                    .py_2(),
                )
                .into_any_element()
        };

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .justify_between()
                    .items_center()
                    .child(div().font_semibold().text_sm().child(tr!("contacts-title")))
                    .child(
                        Button::new("reload-contacts")
                            .ghost()
                            .xsmall()
                            .icon(super::icons::app_icon("refresh-cw"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reload_contacts();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex().w_full().px_2().pb_2().child(
                    div()
                        .key_context("ContactsSearch")
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(
                            Input::new(&self.contacts_search_input)
                                .flex_1()
                                .min_w_0()
                                .w_full()
                                .cleanable(true)
                                .prefix(gpui_component::Icon::new(IconName::Search).small()),
                        ),
                ),
            )
            .child(body)
            .child(self.render_sidebar_navigation(cx))
    }

    fn visible_contacts(&self) -> Vec<Contact> {
        self.contacts
            .list
            .iter()
            .filter(|contact| contact_matches_query(contact, &self.contacts.query))
            .cloned()
            .collect()
    }

    /// Contact virtual-list row with constant height (used
    /// and for offscreen measurement).
    fn contact_row(&self, c: &Contact, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let selected = self.contacts.selected.as_deref() == Some(c.email.as_str());
        let email = c.email.clone();
        div()
            .px_2()
            .child(
                h_flex()
                    .id(gpui::ElementId::Name(format!("contact-{}", c.email).into()))
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1p5()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .when(selected, |el| el.bg(theme.list_active))
                    .when(!selected, |el| el.hover(|s| s.bg(theme.list_hover)))
                    .child(
                        div()
                            .w(px(28.))
                            .h(px(28.))
                            .rounded_full()
                            .bg(super::util::name_color(&c.email))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_sm()
                            .child(
                                c.name
                                    .chars()
                                    .next()
                                    .or_else(|| c.email.chars().next())
                                    .unwrap_or('?')
                                    .to_uppercase()
                                    .to_string(),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().truncate().child(if c.name.is_empty() {
                                c.email.clone()
                            } else {
                                c.name.clone()
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .text_color(theme.muted_foreground)
                                    .child(c.email.clone()),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.focus_shortcuts(window);
                        this.select_contact(email.clone(), cx);
                    })),
            )
            .into_any_element()
    }

    pub(super) fn ensure_contacts_loaded(&mut self) {
        for aid in self.active_account_ids() {
            self.ensure_contacts_for_account(&aid);
        }
    }

    pub(crate) fn ensure_contacts_for_account(&mut self, aid: &crate::model::AccountId) {
        if self.contacts.by_account.contains_key(aid)
            || !self.contacts.loading_accounts.insert(aid.clone())
        {
            return;
        }
        self.send(Cmd::LoadContacts {
            account_id: aid.clone(),
        });
    }

    pub(super) fn reload_contacts(&mut self) {
        for aid in self.active_account_ids() {
            self.contacts.by_account.remove(&aid);
            self.contacts.loading_accounts.insert(aid.clone());
            self.send(Cmd::LoadContacts { account_id: aid });
        }
        self.refresh_visible_contacts();
    }

    pub(super) fn refresh_visible_contacts(&mut self) {
        let mut seen = std::collections::HashSet::new();
        let mut contacts = Vec::new();
        for aid in self.active_account_ids() {
            if let Some(list) = self.contacts.by_account.get(&aid) {
                for contact in list {
                    if seen.insert(contact.email.to_ascii_lowercase()) {
                        contacts.push(contact.clone());
                    }
                }
            }
        }
        contacts.sort_by_key(|contact| {
            let label = if contact.name.is_empty() {
                &contact.email
            } else {
                &contact.name
            };
            label.to_ascii_lowercase()
        });
        self.contacts.list = contacts;
    }

    fn select_contact(&mut self, email: String, cx: &mut Context<Self>) {
        self.contacts.selected = Some(email.clone());
        let limit = self
            .settings
            .account_or_default(self.current_account_id.as_ref())
            .sender_history_limit;
        self.sender_history = SenderHistoryState::Loading {
            email: email.clone(),
        };
        self.send_for_active(|aid| Cmd::LoadSenderHistory {
            account_id: aid,
            email: email.clone(),
            limit,
        });
        cx.notify();
    }

    pub(super) fn navigate_contacts(
        &mut self,
        movement: super::shortcuts::ListMovement,
        cx: &mut Context<Self>,
    ) {
        let contacts = self.visible_contacts();
        if contacts.is_empty() {
            return;
        }
        let current = self
            .contacts
            .selected
            .as_deref()
            .and_then(|email| contacts.iter().position(|contact| contact.email == email));
        let target = match movement {
            super::shortcuts::ListMovement::Previous => {
                current.map_or(0, |index| index.saturating_sub(1))
            }
            super::shortcuts::ListMovement::Next => {
                current.map_or(0, |index| (index + 1).min(contacts.len() - 1))
            }
            super::shortcuts::ListMovement::First => 0,
            super::shortcuts::ListMovement::Last => contacts.len() - 1,
        };
        let email = contacts[target].email.clone();
        self.scrolls.contacts.motion.cancel();
        self.scrolls
            .contacts
            .handle
            .scroll_to_item(target, ScrollStrategy::Center);
        self.select_contact(email, cx);
    }

    /// Opens Contacts directly on an address from a message.
    /// Details remain useful even when the address is not yet in the address
    /// book; its history is then loaded normally.
    pub(crate) fn show_contact(&mut self, email: String, cx: &mut Context<Self>) {
        self.enter_main_view(MainView::Contacts, cx);
        self.ensure_contacts_loaded();
        self.select_contact(email, cx);
    }

    fn render_contact_detail(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let Some(email) = self.contacts.selected.clone() else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child(tr!("contacts-select"))
                .into_any_element();
        };
        let contact = self
            .contacts
            .list
            .iter()
            .find(|c| c.email == email)
            .cloned();
        let name = contact
            .as_ref()
            .map(|c| c.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| email.clone());

        let history: gpui::AnyElement = match &self.sender_history {
            SenderHistoryState::Loaded { messages, .. } if !messages.is_empty() => {
                let mut rows = v_flex().gap_0p5().p_2();
                for m in messages.clone() {
                    rows = rows.child(self.message_row(&m, false, "contact-history", cx));
                }
                div()
                    .id("contact-history-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(rows)
                    .into_any_element()
            }
            SenderHistoryState::Loading { .. } => div()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(tr!("loading-history"))
                .into_any_element(),
            _ => div()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(tr!("contacts-no-recent-messages"))
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        v_flex()
                            .flex_1()
                            .child(div().text_lg().font_semibold().child(name))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(email.clone()),
                            ),
                    ),
            )
            .child(history)
            .into_any_element()
    }
}

fn contact_matches_query(contact: &Contact, query: &str) -> bool {
    let haystack = format!("{}\n{}", contact.name, contact.email).to_lowercase();
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .all(|term| haystack.contains(&term))
}

#[cfg(test)]
mod tests {
    use super::contact_matches_query;
    use crate::model::Contact;

    fn contact(name: &str, email: &str) -> Contact {
        Contact {
            name: name.to_string(),
            email: email.to_string(),
            score: 0.0,
        }
    }

    #[test]
    fn search_filters_name_and_address_case_insensitively() {
        let accented = contact("Contact-É", "contact-e@example.test");

        assert!(contact_matches_query(&accented, "cONTACT-é"));
        assert!(contact_matches_query(&accented, "é example"));
        assert!(!contact_matches_query(&accented, "absent"));
    }
}
