//! Accounts tab: connected accounts and Azure/Google configuration.

use super::super::app::AviaryApp;
use super::labelled;
use crate::model::Provider;
use crate::runtime::Cmd;
use gpui::{div, prelude::*, px, Context};
use gpui_component::{
    button::{Button, ButtonGroup, ButtonVariants},
    color_picker::ColorPicker,
    h_flex,
    input::Input,
    menu::{DropdownMenu, PopupMenuItem},
    v_flex, ActiveTheme, Disableable, IconName, Sizable, StyledExt,
};

impl AviaryApp {
    pub(super) fn refresh_account_customizations(&mut self, cx: &mut Context<Self>) {
        self.refresh_compose_account_options(cx);
        self.refresh_event_compose_account_options(cx);
        cx.notify();
    }

    pub(super) fn render_settings_accounts(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let accounts_list = self.render_account_rows(cx);

        // Dropdown contents are cached by element id; include the language so
        // its labels refresh immediately when the locale changes.
        let add_account_variant = usize::from(self.settings.global.language.to_lang_id() == "fr");
        let add_account_entity = cx.entity();
        let add_account_button = Button::new(("settings-add-account", add_account_variant))
            .primary()
            .small()
            .icon(super::super::icons::app_icon("plus"))
            .label(tr!("settings-add-account-heading"))
            .dropdown_menu(move |menu, _window, _cx| {
                let microsoft_entity = add_account_entity.clone();
                let google_entity = add_account_entity.clone();
                let imap_entity = add_account_entity.clone();
                menu.item(PopupMenuItem::new(tr!("settings-add-microsoft")).on_click(
                    move |_, _, cx| {
                        microsoft_entity.update(cx, |this, cx| {
                            this.start_microsoft_login(cx);
                        });
                    },
                ))
                .item(PopupMenuItem::new(tr!("settings-add-google")).on_click(
                    move |_, window, cx| {
                        google_entity.update(cx, |this, cx| {
                            this.start_google_login(window, cx);
                        });
                    },
                ))
                .item(PopupMenuItem::new(tr!("settings-add-imap")).on_click(
                    move |_, window, cx| {
                        imap_entity.update(cx, |this, cx| {
                            this.open_imap_form(window, cx);
                        });
                    },
                ))
            });

        v_flex()
            .child(
                v_flex()
                    .gap_3()
                    .mb_6()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .pb_1()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .flex_1()
                                    .text_lg()
                                    .font_semibold()
                                    .child(tr!("settings-accounts-connected-heading")),
                            )
                            .child(add_account_button),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(tr!("settings-account-default-description")),
                    )
                    .child(accounts_list),
            )
            .child(self.render_provider_config_section(cx))
    }

    fn move_account(&mut self, account_id: &crate::model::AccountId, offset: isize) {
        let mut order: Vec<String> = self
            .ordered_accounts()
            .into_iter()
            .map(|account| account.id.0)
            .collect();
        let Some(index) = order.iter().position(|id| id == &account_id.0) else {
            return;
        };
        let target = index as isize + offset;
        if !(0..order.len() as isize).contains(&target) {
            return;
        }
        order.swap(index, target as usize);
        self.settings.global.account_order = order;
        self.settings.save();
    }

    /// One row per configured account, then the accounts whose tokens could not
    /// be resumed, then the empty state.
    fn render_account_rows(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = self.settings_ui.as_ref().expect("settings_ui");
        let mut accounts_list = v_flex().gap_2();
        let accounts = self.ordered_accounts();
        let account_count = accounts.len();
        let configured_default_account_id = self.settings.global.default_account_id.clone();
        for (index, a) in accounts.into_iter().enumerate() {
            let aid = a.id.clone();
            let is_default = configured_default_account_id.as_deref() == Some(aid.0.as_str());
            let provider_label = match a.provider {
                Provider::Microsoft => tr!("provider-microsoft-365"),
                Provider::Google => tr!("provider-gmail"),
                Provider::Imap => tr!("provider-imap"),
            };
            let move_up_aid = aid.clone();
            let move_down_aid = aid.clone();
            let logout_aid = aid.clone();
            let reset_aid = aid.clone();
            let default_aid = aid.clone();
            let editor = ui
                .account_editors
                .get(&aid)
                .expect("account editor initialized with settings UI");
            let display_name = editor.display_name.clone();
            let color = editor.color.clone();
            let reset_color = color.clone();
            let default_button = Button::new(gpui::ElementId::Name(
                format!("account-default-{}", aid.0).into(),
            ))
            .small()
            .label(if is_default {
                tr!("settings-account-default")
            } else {
                tr!("settings-account-set-default")
            })
            .tooltip(tr!("settings-account-default-description"))
            .disabled(is_default)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.settings.global.default_account_id = Some(default_aid.0.clone());
                this.settings.save();
                cx.notify();
            }));
            let default_button = if is_default {
                default_button.primary()
            } else {
                default_button.outline()
            };
            accounts_list = accounts_list.child(
                v_flex()
                    .gap_3()
                    .p_3()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_3()
                            .items_center()
                            .child(
                                div().w(px(10.)).h(px(10.)).rounded_full().bg(
                                    super::super::util::account_color(
                                        &aid,
                                        self.settings
                                            .accounts
                                            .get(&aid)
                                            .and_then(|s| s.color_override),
                                    ),
                                ),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_center()
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .truncate()
                                                    .font_semibold()
                                                    .child(self.account_label(&a)),
                                            )
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .px_2()
                                                    .py_0p5()
                                                    .rounded_full()
                                                    .border_1()
                                                    .border_color(theme.border)
                                                    .bg(theme.secondary)
                                                    .text_xs()
                                                    .text_color(theme.secondary_foreground)
                                                    .child(provider_label),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .truncate()
                                            .text_color(theme.muted_foreground)
                                            .child(a.email.clone()),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .flex_none()
                                    .gap_2()
                                    .child(
                                        ButtonGroup::new(gpui::ElementId::Name(
                                            format!("account-order-{}", aid.0).into(),
                                        ))
                                        .outline()
                                        .compact()
                                        .child(
                                            Button::new(gpui::ElementId::Name(
                                                format!("account-up-{}", aid.0).into(),
                                            ))
                                            .xsmall()
                                            .disabled(index == 0)
                                            .icon(super::super::icons::app_icon("arrow-up"))
                                            .tooltip(tr!("settings-account-move-up")),
                                        )
                                        .child(
                                            Button::new(gpui::ElementId::Name(
                                                format!("account-down-{}", aid.0).into(),
                                            ))
                                            .xsmall()
                                            .disabled(index + 1 == account_count)
                                            .icon(super::super::icons::app_icon("arrow-down"))
                                            .tooltip(tr!("settings-account-move-down")),
                                        )
                                        .on_click(
                                            cx.listener(
                                                move |this, selected: &Vec<usize>, _, cx| {
                                                    match selected.first() {
                                                        Some(0) => {
                                                            this.move_account(&move_up_aid, -1)
                                                        }
                                                        Some(1) => {
                                                            this.move_account(&move_down_aid, 1)
                                                        }
                                                        _ => return,
                                                    }
                                                    cx.notify();
                                                },
                                            ),
                                        ),
                                    )
                                    .child(
                                        Button::new(gpui::ElementId::Name(
                                            format!("logout-{}", aid.0).into(),
                                        ))
                                        .danger()
                                        .small()
                                        .label(tr!("settings-logout"))
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.send(Cmd::Logout(logout_aid.clone()));
                                                cx.notify();
                                            }),
                                        ),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_3()
                            .items_end()
                            .flex_wrap()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w(px(220.))
                                    .max_w(px(460.))
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(theme.muted_foreground)
                                            .child(tr!("settings-account-display-name-label")),
                                    )
                                    .child(Input::new(&display_name).small().w_full()),
                            )
                            .child(default_button)
                            .child(
                                v_flex().child(
                                    h_flex()
                                        .gap_1()
                                        .items_center()
                                        .child(ColorPicker::new(&color).small())
                                        .child(
                                            Button::new(gpui::ElementId::Name(
                                                format!("account-color-reset-{}", aid.0).into(),
                                            ))
                                            .ghost()
                                            .xsmall()
                                            .icon(super::super::icons::app_icon("undo-2"))
                                            .tooltip(tr!("settings-account-color-reset"))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.settings
                                                    .account_mut(&reset_aid)
                                                    .color_override = None;
                                                this.settings.save();
                                                let default_color =
                                                    super::super::util::account_color(
                                                        &reset_aid, None,
                                                    );
                                                reset_color.update(cx, |state, cx| {
                                                    state.set_value(default_color, window, cx);
                                                });
                                                this.refresh_account_customizations(cx);
                                            })),
                                        ),
                                ),
                            ),
                    ),
            );
        }
        let mut unavailable: Vec<_> = self.unavailable_accounts.clone().into_iter().collect();
        unavailable.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
        for (aid, (provider, error)) in unavailable {
            let provider = match provider {
                Provider::Microsoft => "Microsoft 365",
                Provider::Google => "Google",
                Provider::Imap => "IMAP",
            };
            let logout_id = aid.clone();
            accounts_list = accounts_list.child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .p_3()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.danger)
                    .child(div().w(px(10.)).h(px(10.)).rounded_full().bg(theme.danger))
                    .child(
                        v_flex()
                            .flex_1()
                            .child(div().font_semibold().child(aid.0.clone()))
                            .child(
                                div().text_sm().text_color(theme.muted_foreground).child(
                                    tr!("settings-reconnect-required", { provider: provider }),
                                ),
                            )
                            .child(div().text_xs().text_color(theme.danger).child(error)),
                    )
                    .child(
                        Button::new(gpui::ElementId::Name(
                            format!("logout-unavailable-{}", aid.0).into(),
                        ))
                        .danger()
                        .small()
                        .label(tr!("delete"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.send(Cmd::Logout(logout_id.clone()));
                            cx.notify();
                        })),
                    ),
            );
        }
        if self.accounts.is_empty() && self.unavailable_accounts.is_empty() {
            accounts_list = accounts_list.child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(tr!("settings-accounts-empty")),
            );
        }
        accounts_list
    }

    /// Azure and Google registration overrides, folded away by default: only a
    /// tenant that blocks the bundled applications needs them.
    fn render_provider_config_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = self.settings_ui.as_ref().expect("settings_ui");
        let advanced_expanded = ui.advanced_provider_config_expanded;
        v_flex()
            .mb_4()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .id("settings-advanced-provider-config-toggle")
                    .w_full()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.list_hover))
                    .child(
                        gpui_component::Icon::new(if advanced_expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall()
                        .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child(tr!("settings-advanced")),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(ui) = &mut this.settings_ui {
                            ui.advanced_provider_config_expanded = !advanced_expanded;
                        }
                        cx.notify();
                    })),
            )
            .when(advanced_expanded, |section| {
                section.child(
                    v_flex()
                        .gap_4()
                        .p_3()
                        .border_t_1()
                        .border_color(theme.border)
                        .child(
                            v_flex()
                                .gap_3()
                                .child(div().font_semibold().child(tr!("settings-azure-config")))
                                .child(labelled(
                                    &tr!("settings-azure-client-id"),
                                    &ui.azure_client_id,
                                    cx,
                                ))
                                .child(labelled(
                                    &tr!("settings-azure-tenant"),
                                    &ui.azure_tenant,
                                    cx,
                                )),
                        )
                        .child(
                            v_flex()
                                .gap_3()
                                .pt_4()
                                .border_t_1()
                                .border_color(theme.border)
                                .child(div().font_semibold().child(tr!("settings-google-config")))
                                .child(labelled(
                                    &tr!("settings-google-client-id"),
                                    &ui.google_client_id,
                                    cx,
                                ))
                                .child(labelled(
                                    &tr!("settings-google-client-secret"),
                                    &ui.google_client_secret,
                                    cx,
                                )),
                        )
                        .child(
                            Button::new("save-provider-config")
                                .primary()
                                .label(tr!("settings-save-configuration"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(ui) = &this.settings_ui {
                                        this.settings.global.azure_client_id =
                                            ui.azure_client_id.read(cx).value().trim().to_string();
                                        this.settings.global.azure_tenant =
                                            ui.azure_tenant.read(cx).value().trim().to_string();
                                        this.settings.global.google_client_id =
                                            ui.google_client_id.read(cx).value().trim().to_string();
                                        this.settings.global.google_client_secret = ui
                                            .google_client_secret
                                            .read(cx)
                                            .value()
                                            .trim()
                                            .to_string();
                                        this.settings.save();
                                    }
                                    cx.notify();
                                })),
                        ),
                )
            })
    }
}
