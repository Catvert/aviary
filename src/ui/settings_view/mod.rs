//! Settings view, with one file per tab: accounts, signatures, templates,
//! appearance, mailbox, and notifications/security. This module owns
//! `SettingsUi`, left-pane navigation, and shared helpers.

mod accounts;
mod ai;
mod appearance;
mod calendars;
mod correction;
mod keyboard;
mod misc;
mod quick_actions;
mod rich_snippets;
mod signatures;
mod tags;
mod templates;

use super::addresses::{AddressBook, RecipientInput};
use super::app::AviaryApp;
use super::block_editor::BlockEditor;
use super::settings::ThemeColorRole;
use crate::model::{AccountId, IcalRefreshInterval};
use gpui::{div, prelude::*, px, Context, Entity, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    color_picker::{ColorPickerEvent, ColorPickerState},
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu, PopupMenuItem},
    v_flex, ActiveTheme, IconName, Selectable, Sizable, StyledExt,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SettingsTab {
    #[default]
    Comptes,
    Signatures,
    Modeles,
    Calendriers,
    Ia,
    Correction,
    Apparence,
    Clavier,
    Inbox,
    ActionsRapides,
    Etiquettes,
    Notifications,
    Securite,
    Logs,
}

/// Settings input entities, created on first open.
pub struct SettingsUi {
    pub tab: SettingsTab,
    pub advanced_provider_config_expanded: bool,
    pub azure_client_id: Entity<InputState>,
    pub azure_tenant: Entity<InputState>,
    pub google_client_id: Entity<InputState>,
    pub google_client_secret: Entity<InputState>,
    pub ai_openai_api_key: Entity<InputState>,
    pub ai_openai_model: Entity<InputState>,
    pub ai_anthropic_api_key: Entity<InputState>,
    pub ai_anthropic_model: Entity<InputState>,
    pub ai_gemini_api_key: Entity<InputState>,
    pub ai_gemini_model: Entity<InputState>,
    pub ai_local_base_url: Entity<InputState>,
    pub ai_local_api_key: Entity<InputState>,
    pub ai_local_model: Entity<InputState>,
    pub ai_system_prompt: Entity<InputState>,
    pub ai_reader_translation_target: Entity<InputState>,
    pub ai_reader_translation_prompt: Entity<InputState>,
    pub ai_prompt_name: Entity<InputState>,
    pub ai_prompt_body: Entity<InputState>,
    pub languagetool_java_path: Entity<InputState>,
    pub languagetool_directory: Entity<InputState>,
    pub languagetool_url: Entity<InputState>,
    pub editing_ai_prompt: Option<u64>,
    pub ical_name: Entity<InputState>,
    pub ical_url: Entity<InputState>,
    pub ical_color: Entity<ColorPickerState>,
    pub ical_refresh: IcalRefreshInterval,
    pub editing_ical_id: Option<String>,
    pub fetch_limit: Entity<InputState>,
    pub sender_history_limit: Entity<InputState>,
    pub calendar_upcoming_days: Entity<InputState>,
    pub calendar_grid_weeks: Entity<InputState>,
    pub action_delay_secs: Entity<InputState>,
    pub send_delay_secs: Entity<InputState>,
    pub blocked_sender: Entity<InputState>,
    theme_colors: Vec<ThemeColorEditorState>,
    account_editors: HashMap<AccountId, AccountEditorState>,
    signature: RichSnippetEditorState,
    template: RichSnippetEditorState,
    quick_action: QuickActionEditorState,
}

/// Settings choices that behave like radio buttons use the accent variant for
/// the active value. The component's default secondary selected state is too
/// subtle against dark surfaces.
fn choice_button(button: Button, selected: bool) -> Button {
    button
        .selected(selected)
        .when(selected, |button| button.primary())
}

struct AccountEditorState {
    display_name: Entity<InputState>,
    color: Entity<ColorPickerState>,
}

struct ThemeColorEditorState {
    role: ThemeColorRole,
    picker: Entity<ColorPickerState>,
}

struct RichSnippetEditorState {
    name: Entity<InputState>,
    editor: Entity<BlockEditor>,
    is_default: bool,
    editing_id: Option<i64>,
}

struct QuickActionEditorState {
    editing_id: Option<i64>,
    name: Entity<InputState>,
    to: Entity<RecipientInput>,
    cc: Entity<RecipientInput>,
    bcc: Entity<RecipientInput>,
    forward_enabled: bool,
    note: Entity<BlockEditor>,
    reply_enabled: bool,
    reply_all: bool,
    reply_body: Entity<BlockEditor>,
    color: Entity<ColorPickerState>,
    icon: super::settings::QuickActionIcon,
    favorite: bool,
    add_tags: std::collections::HashSet<String>,
    remove_tags: std::collections::HashSet<String>,
    mark_read: Option<bool>,
    set_flagged: Option<bool>,
    move_to_folder_id: Option<String>,
}

impl QuickActionEditorState {
    fn new(
        options: super::settings::MailBodyOptions,
        address_book: AddressBook,
        window: &mut Window,
        cx: &mut Context<AviaryApp>,
    ) -> Self {
        Self {
            editing_id: None,
            name: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr!("quick-actions-name-placeholder"))
            }),
            to: cx.new(|cx| {
                RecipientInput::new(
                    "",
                    tr!("compose-to-placeholder").to_string(),
                    address_book.clone(),
                    window,
                    cx,
                )
                .tab_index(10)
            }),
            cc: cx.new(|cx| {
                RecipientInput::new(
                    "",
                    tr!("compose-cc-placeholder").to_string(),
                    address_book.clone(),
                    window,
                    cx,
                )
                .tab_index(20)
            }),
            bcc: cx.new(|cx| {
                RecipientInput::new(
                    "",
                    tr!("compose-bcc-placeholder").to_string(),
                    address_book,
                    window,
                    cx,
                )
                .tab_index(30)
            }),
            forward_enabled: false,
            note: cx.new(|cx| {
                BlockEditor::new(
                    Vec::new(),
                    Vec::new(),
                    false,
                    options,
                    &tr!("quick-actions-note-placeholder"),
                    window,
                    cx,
                )
            }),
            reply_enabled: false,
            reply_all: false,
            reply_body: cx.new(|cx| {
                BlockEditor::new(
                    Vec::new(),
                    Vec::new(),
                    false,
                    options,
                    &tr!("quick-actions-reply-placeholder"),
                    window,
                    cx,
                )
            }),
            color: cx.new(|cx| {
                ColorPickerState::new(window, cx).default_value(super::util::packed_color(0xE5A50A))
            }),
            icon: super::settings::QuickActionIcon::Zap,
            favorite: false,
            add_tags: Default::default(),
            remove_tags: Default::default(),
            mark_read: None,
            set_flagged: None,
            move_to_folder_id: None,
        }
    }
}

impl RichSnippetEditorState {
    fn new(
        name_placeholder: gpui::SharedString,
        body_placeholder: gpui::SharedString,
        options: super::settings::MailBodyOptions,
        window: &mut Window,
        cx: &mut Context<AviaryApp>,
    ) -> Self {
        Self {
            name: cx.new(|cx| InputState::new(window, cx).placeholder(name_placeholder)),
            editor: cx.new(|cx| {
                BlockEditor::new(
                    Vec::new(),
                    Vec::new(),
                    false,
                    options,
                    &body_placeholder,
                    window,
                    cx,
                )
            }),
            is_default: false,
            editing_id: None,
        }
    }
}

impl SettingsUi {
    fn new(app: &AviaryApp, window: &mut Window, cx: &mut Context<AviaryApp>) -> Self {
        let g = &app.settings.global;
        let restored_tab = app.last_settings_tab;
        let acc = app
            .settings
            .account_or_default(app.current_account_id.as_ref());
        let mail_body_options = g.mail_body_options();
        let mk = |window: &mut Window, cx: &mut Context<AviaryApp>, value: String| {
            cx.new(|cx| InputState::new(window, cx).default_value(value))
        };
        let mut account_editors = HashMap::new();
        for account in &app.accounts {
            let account_id = account.id.clone();
            let account_settings = app.settings.account_or_default(Some(&account_id));
            let fallback_name = if account.display_name.is_empty() {
                account.email.clone()
            } else {
                account.display_name.clone()
            };
            let display_name = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(fallback_name)
                    .default_value(account_settings.display_name_override)
            });
            let name_account_id = account_id.clone();
            cx.subscribe_in(
                &display_name,
                window,
                move |this, input, event: &InputEvent, _, cx| {
                    if !matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                        return;
                    }
                    let value = input.read(cx).value().trim().to_string();
                    if this
                        .settings
                        .accounts
                        .get(&name_account_id)
                        .is_some_and(|settings| settings.display_name_override == value)
                    {
                        return;
                    }
                    this.settings
                        .account_mut(&name_account_id)
                        .display_name_override = value;
                    this.settings.save();
                    this.refresh_account_customizations(cx);
                },
            )
            .detach();

            let effective_color = super::util::account_color(
                &account_id,
                app.settings
                    .accounts
                    .get(&account_id)
                    .and_then(|settings| settings.color_override),
            );
            let color =
                cx.new(|cx| ColorPickerState::new(window, cx).default_value(effective_color));
            let color_account_id = account_id.clone();
            cx.subscribe_in(
                &color,
                window,
                move |this, _, event: &ColorPickerEvent, _, cx| {
                    let ColorPickerEvent::Change(color) = event;
                    let color = color.map(super::util::color_to_packed);
                    if this
                        .settings
                        .accounts
                        .get(&color_account_id)
                        .and_then(|settings| settings.color_override)
                        == color
                    {
                        return;
                    }
                    this.settings.account_mut(&color_account_id).color_override = color;
                    this.settings.save();
                    this.refresh_account_customizations(cx);
                },
            )
            .detach();
            account_editors.insert(
                account_id,
                AccountEditorState {
                    display_name,
                    color,
                },
            );
        }
        let mut theme_colors = Vec::with_capacity(ThemeColorRole::ALL.len());
        for role in ThemeColorRole::ALL {
            let picker = cx.new(|cx| {
                ColorPickerState::new(window, cx).default_value(super::util::packed_color(
                    g.custom_theme_palette.color(role),
                ))
            });
            cx.subscribe_in(
                &picker,
                window,
                move |this, _, event: &ColorPickerEvent, window, cx| {
                    let ColorPickerEvent::Change(color) = event;
                    let Some(color) = color else { return };
                    this.settings
                        .global
                        .edit_custom_theme_color(role, super::util::color_to_packed(*color));
                    this.settings.save();
                    super::theme::apply(&this.settings.global, Some(window), cx);
                    cx.notify();
                },
            )
            .detach();
            theme_colors.push(ThemeColorEditorState { role, picker });
        }
        Self {
            tab: restored_tab,
            advanced_provider_config_expanded: false,
            azure_client_id: mk(window, cx, g.azure_client_id.clone()),
            azure_tenant: mk(window, cx, g.azure_tenant.clone()),
            google_client_id: mk(window, cx, g.google_client_id.clone()),
            google_client_secret: mk(window, cx, g.google_client_secret.clone()),
            ai_openai_api_key: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .default_value(g.ai.openai_api_key.clone())
            }),
            ai_openai_model: mk(window, cx, g.ai.openai_model.clone()),
            ai_anthropic_api_key: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .default_value(g.ai.anthropic_api_key.clone())
            }),
            ai_anthropic_model: mk(window, cx, g.ai.anthropic_model.clone()),
            ai_gemini_api_key: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .default_value(g.ai.gemini_api_key.clone())
            }),
            ai_gemini_model: mk(window, cx, g.ai.gemini_model.clone()),
            ai_local_base_url: mk(window, cx, g.ai.local_base_url.clone()),
            ai_local_api_key: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .default_value(g.ai.local_api_key.clone())
            }),
            ai_local_model: mk(window, cx, g.ai.local_model.clone()),
            ai_system_prompt: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .rows(6)
                    .default_value(g.ai.system_prompt.clone())
            }),
            ai_reader_translation_target: mk(window, cx, g.ai.reader_translation_target.clone()),
            ai_reader_translation_prompt: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .rows(8)
                    .default_value(g.ai.reader_translation_prompt.clone())
            }),
            ai_prompt_name: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr!("settings-ai-prompt-name-placeholder"))
            }),
            ai_prompt_body: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .rows(8)
                    .placeholder(tr!("settings-ai-prompt-body-placeholder"))
            }),
            editing_ai_prompt: None,
            languagetool_java_path: mk(window, cx, g.languagetool.java_path.clone()),
            languagetool_directory: mk(window, cx, g.languagetool.existing_directory.clone()),
            languagetool_url: mk(window, cx, g.languagetool.external_url.clone()),
            ical_name: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr!("settings-ical-name-placeholder"))
            }),
            ical_url: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr!("settings-ical-url-placeholder"))
            }),
            ical_color: cx.new(|cx| {
                ColorPickerState::new(window, cx).default_value(super::util::account_color(
                    &AccountId("ical-new".into()),
                    None,
                ))
            }),
            ical_refresh: IcalRefreshInterval::OneHour,
            editing_ical_id: None,
            fetch_limit: mk(window, cx, acc.fetch_limit.to_string()),
            sender_history_limit: mk(window, cx, acc.sender_history_limit.to_string()),
            calendar_upcoming_days: mk(window, cx, g.calendar_upcoming_days.to_string()),
            calendar_grid_weeks: mk(window, cx, g.calendar_grid_weeks.to_string()),
            action_delay_secs: mk(window, cx, g.action_delay_secs.to_string()),
            send_delay_secs: mk(window, cx, g.send_delay_secs.to_string()),
            blocked_sender: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr!("settings-blocked-senders-placeholder"))
            }),
            theme_colors,
            account_editors,
            signature: RichSnippetEditorState::new(
                tr!("settings-signatures-name-hint"),
                tr!("signatures-body-placeholder"),
                mail_body_options,
                window,
                cx,
            ),
            template: RichSnippetEditorState::new(
                tr!("templates-name-placeholder"),
                tr!("templates-body-placeholder"),
                mail_body_options,
                window,
                cx,
            ),
            quick_action: QuickActionEditorState::new(
                mail_body_options,
                app.address_book.clone(),
                window,
                cx,
            ),
        }
    }
}

impl AviaryApp {
    /// Explicit mailbox selector for account-specific settings. Changing it
    /// rebuilds the forms so contents from two mailboxes are never mixed.
    fn render_settings_account_selector(
        &self,
        tab: SettingsTab,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let current = self.current_account_id.clone();
        let accounts: Vec<_> = self
            .ordered_accounts()
            .into_iter()
            .map(|account| {
                let name = self.account_label(&account);
                let label = if name == account.email {
                    account.email.clone()
                } else {
                    format!("{name} — {}", account.email)
                };
                let selected = current.as_ref() == Some(&account.id);
                (account.id, label, selected)
            })
            .collect();
        let selected_label = accounts
            .iter()
            .find(|(_, _, selected)| *selected)
            .map(|(_, label, _)| label.clone())
            .unwrap_or_else(|| tr!("settings-mailbox-selector-placeholder").to_string());
        let id = match tab {
            SettingsTab::Signatures => "signatures-mailbox-selector",
            SettingsTab::Modeles => "templates-mailbox-selector",
            _ => "settings-mailbox-selector",
        };
        let entity = cx.entity();

        v_flex()
            .gap_1()
            .mb_3()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child(tr!("settings-mailbox-selector-label")),
            )
            .child(
                Button::new(id)
                    .outline()
                    .w_full()
                    .icon(IconName::Inbox)
                    .label(selected_label)
                    .dropdown_menu(move |mut menu, _window, _cx| {
                        for (account_id, label, selected) in accounts.clone() {
                            let entity = entity.clone();
                            menu = menu.item(PopupMenuItem::new(label).checked(selected).on_click(
                                move |_, window, cx| {
                                    entity.update(cx, |this, cx| {
                                        this.set_current_account_context(Some(account_id.clone()));
                                        this.settings.save();
                                        this.ensure_settings_ui(window, cx);
                                        if let Some(ui) = &mut this.settings_ui {
                                            ui.tab = tab;
                                        }
                                        cx.notify();
                                    });
                                },
                            ));
                        }
                        menu
                    }),
            )
            .into_any_element()
    }

    fn ensure_settings_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_ui.is_none() {
            self.settings_ui = Some(SettingsUi::new(self, window, cx));
            self.send(crate::runtime::Cmd::GetMailCacheStats);
        }
    }

    pub(crate) fn open_logs_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_settings_ui(window, cx);
        if let Some(ui) = &mut self.settings_ui {
            ui.tab = SettingsTab::Logs;
        }
        self.enter_main_view(super::state::MainView::Settings, cx);
    }

    pub fn render_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.ensure_settings_ui(window, cx);
        let tab = self
            .settings_ui
            .as_ref()
            .expect("settings UI initialized")
            .tab;

        let content: gpui::AnyElement = match tab {
            SettingsTab::Comptes => self.render_settings_accounts(cx).into_any_element(),
            SettingsTab::Signatures => self
                .render_settings_signatures(window, cx)
                .into_any_element(),
            SettingsTab::Modeles => self
                .render_settings_templates(window, cx)
                .into_any_element(),
            SettingsTab::Calendriers => self
                .render_settings_calendars(window, cx)
                .into_any_element(),
            SettingsTab::Ia => self.render_settings_ai(window, cx).into_any_element(),
            SettingsTab::Correction => self
                .render_settings_correction(window, cx)
                .into_any_element(),
            SettingsTab::Apparence => self.render_settings_appearance(cx).into_any_element(),
            SettingsTab::Clavier => self.render_settings_keyboard(cx).into_any_element(),
            SettingsTab::Inbox => self.render_settings_inbox(cx).into_any_element(),
            SettingsTab::ActionsRapides => self
                .render_settings_quick_actions(window, cx)
                .into_any_element(),
            SettingsTab::Etiquettes => self.render_settings_tags(cx).into_any_element(),
            SettingsTab::Notifications => self.render_settings_notifications(cx).into_any_element(),
            SettingsTab::Securite => self.render_settings_security(cx).into_any_element(),
            SettingsTab::Logs => self.render_logs_settings(cx).into_any_element(),
        };

        super::app::sidebar_layout(
            "settings-panes",
            self.sidebar_resize.clone(),
            self.render_settings_sidebar(tab, cx).into_any_element(),
            div()
                .id("settings-scroll")
                .size_full()
                .overflow_y_scroll()
                .child(
                    div()
                        .w_full()
                        .p_4()
                        .max_w(if tab == SettingsTab::Logs {
                            px(1200.)
                        } else {
                            px(760.)
                        })
                        .child(content),
                )
                .into_any_element(),
        )
    }

    fn render_settings_sidebar(
        &mut self,
        tab: SettingsTab,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                div()
                    .px_3()
                    .py_3()
                    .text_lg()
                    .font_semibold()
                    .child(tr!("settings-title")),
            )
            .child(
                v_flex()
                    .id("settings-tabs-scroll")
                    .flex_1()
                    .min_h_0()
                    .gap_1()
                    .px_2()
                    .overflow_y_scroll()
                    .child(self.settings_tab_button(
                        "settings-accounts",
                        tr!("settings-tab-accounts").to_string(),
                        "users",
                        SettingsTab::Comptes,
                        tab,
                        cx,
                    ))
                    .child(self.settings_tab_button(
                        "settings-signatures",
                        tr!("settings-tab-signatures").to_string(),
                        "pencil",
                        SettingsTab::Signatures,
                        tab,
                        cx,
                    ))
                    .child(self.settings_tab_button(
                        "settings-templates",
                        tr!("settings-tab-templates").to_string(),
                        "book-open",
                        SettingsTab::Modeles,
                        tab,
                        cx,
                    ))
                    .child(self.settings_tab_button(
                        "settings-calendars",
                        tr!("settings-tab-calendars").to_string(),
                        "calendar-days",
                        SettingsTab::Calendriers,
                        tab,
                        cx,
                    ))
                    .child(self.settings_tab_button(
                        "settings-ai",
                        tr!("settings-tab-ai").to_string(),
                        "bot",
                        SettingsTab::Ia,
                        tab,
                        cx,
                    ))
                    .child(self.settings_tab_button(
                        "settings-correction",
                        tr!("settings-tab-correction").to_string(),
                        "check-check",
                        SettingsTab::Correction,
                        tab,
                        cx,
                    ))
                    .child(self.settings_tab_button(
                        "settings-appearance",
                        tr!("settings-tab-appearance").to_string(),
                        "palette",
                        SettingsTab::Apparence,
                        tab,
                        cx,
                    ))
                    .child(self.settings_tab_button(
                        "settings-keyboard",
                        tr!("settings-tab-keyboard").to_string(),
                        "a-large-small",
                        SettingsTab::Clavier,
                        tab,
                        cx,
                    ))
                    .child(self.settings_tab_button(
                        "settings-inbox",
                        tr!("settings-tab-inbox-fr").to_string(),
                        "inbox",
                        SettingsTab::Inbox,
                        tab,
                        cx,
                    ))
                    .child(self.settings_tab_button(
                        "settings-quick-actions",
                        tr!("settings-tab-quick-actions").to_string(),
                        "zap",
                        SettingsTab::ActionsRapides,
                        tab,
                        cx,
                    ))
                    .child(self.settings_tab_button(
                        "settings-tags",
                        tr!("settings-tab-tags").to_string(),
                        "tags",
                        SettingsTab::Etiquettes,
                        tab,
                        cx,
                    ))
                    .child(self.settings_tab_button(
                        "settings-notifications",
                        tr!("settings-tab-notifications").to_string(),
                        "bell",
                        SettingsTab::Notifications,
                        tab,
                        cx,
                    ))
                    .child(self.settings_tab_button(
                        "settings-security",
                        tr!("settings-tab-security").to_string(),
                        "check-check",
                        SettingsTab::Securite,
                        tab,
                        cx,
                    ))
                    .child(self.settings_tab_button(
                        "settings-logs",
                        tr!("logs-title").to_string(),
                        "square-terminal",
                        SettingsTab::Logs,
                        tab,
                        cx,
                    )),
            )
            .child(self.render_sidebar_navigation(cx))
    }

    fn settings_tab_button(
        &self,
        id: &'static str,
        label: String,
        icon: &'static str,
        target: SettingsTab,
        selected: SettingsTab,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .ghost()
            .small()
            .w_full()
            .justify_start()
            .icon(super::icons::app_icon(icon))
            .label(label)
            .selected(selected == target)
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(ui) = &mut this.settings_ui {
                    ui.tab = target;
                }
                this.focus_shortcuts(window);
                cx.notify();
            }))
    }

    fn section(&self, title: &str, cx: &Context<Self>) -> gpui::Div {
        v_flex().gap_3().mb_6().child(
            div()
                .text_lg()
                .font_semibold()
                .pb_1()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(title.to_string()),
        )
    }
}

fn labelled(label: &str, input: &Entity<InputState>, cx: &Context<AviaryApp>) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(label.to_string()),
        )
        .child(Input::new(input))
}
