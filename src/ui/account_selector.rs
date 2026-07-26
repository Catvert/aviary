use crate::model::AccountId;
use gpui::{prelude::*, px, Context, ElementId, Entity, Hsla};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    menu::{DropdownMenu, PopupMenuItem},
    Sizable,
};
use std::rc::Rc;

#[derive(Clone)]
pub(super) struct AccountSelectorOption {
    pub id: AccountId,
    pub label: String,
    pub color: Hsla,
}

/// Selector context: only the label prefix changes, while the
/// component and menu remain identical in all composers.
#[derive(Clone, Copy)]
pub(super) enum AccountSelectorKind {
    Sender,
    Calendar,
}

impl AccountSelectorKind {
    fn label(self, account: String) -> String {
        match self {
            Self::Sender => tr!("compose-from-account", { account: account }).to_string(),
            Self::Calendar => tr!("calendar-event-account", { account: account }).to_string(),
        }
    }
}

/// Shared identity label: the name remains readable, with the address used to
/// distinguish two accounts with the same name.
pub(super) fn account_identity_label(name: String, email: &str) -> String {
    if email.is_empty() || name.eq_ignore_ascii_case(email) {
        name
    } else {
        format!("{name} <{email}>")
    }
}

/// Account selector shared by the mail composer, inline reply, and event
/// composer. The caller retains the logic applied to the selection.
pub(super) fn account_selector<T, F>(
    id: impl Into<ElementId>,
    accounts: &[AccountSelectorOption],
    selected: Option<&AccountId>,
    kind: AccountSelectorKind,
    tab_index: isize,
    entity: Entity<T>,
    on_select: F,
) -> Option<gpui::AnyElement>
where
    T: 'static,
    F: Fn(&mut T, AccountId, &mut Context<T>) + 'static,
{
    if accounts.len() <= 1 {
        return None;
    }

    let selected_label = selected
        .and_then(|selected| accounts.iter().find(|account| &account.id == selected))
        .or_else(|| accounts.first())
        .cloned()
        .expect("a multi-account selector has at least one account");
    let accounts = accounts.to_vec();
    let on_select = Rc::new(on_select);
    let button_label = kind.label(selected_label.label);

    Some(
        Button::new(id)
            .ghost()
            .small()
            .tab_index(tab_index)
            .flex_initial()
            .min_w_0()
            .max_w_full()
            .overflow_hidden()
            .tooltip(button_label.clone())
            .child(
                gpui::div()
                    .flex_none()
                    .w(px(8.))
                    .h(px(8.))
                    .rounded_full()
                    .bg(selected_label.color),
            )
            .child(
                gpui::div()
                    .min_w_0()
                    .max_w_full()
                    .truncate()
                    .child(button_label),
            )
            .dropdown_menu(move |mut menu, _, _| {
                for account in accounts.clone() {
                    let entity = entity.clone();
                    let on_select = Rc::clone(&on_select);
                    let account_id = account.id;
                    let label = account.label;
                    let color = account.color;
                    menu = menu.item(
                        PopupMenuItem::element(move |_, _| {
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .gap_2()
                                .items_center()
                                .child(
                                    gpui::div()
                                        .flex_none()
                                        .w(px(8.))
                                        .h(px(8.))
                                        .rounded_full()
                                        .bg(color),
                                )
                                .child(gpui::div().min_w_0().truncate().child(label.clone()))
                        })
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |view, cx| {
                                on_select(view, account_id.clone(), cx);
                            });
                        }),
                    );
                }
                menu
            })
            .into_any_element(),
    )
}
