//! Mailbox, Notifications, and Security tabs, including factory reset.

use super::super::app::AviaryApp;
use super::labelled;
use crate::runtime::Cmd;
use gpui::{div, prelude::*, Context};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::Input,
    switch::Switch,
    v_flex, ActiveTheme, Sizable,
};

impl AviaryApp {
    pub(super) fn render_settings_inbox(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = self.settings_ui.as_ref().expect("settings_ui");
        let auto = self
            .settings
            .account_or_default(self.current_account_id.as_ref())
            .auto_refresh_secs;
        let auto_button = |id: &'static str, label: String, secs: u32, cx: &mut Context<Self>| {
            super::choice_button(Button::new(id).small().label(label), auto == secs).on_click(
                cx.listener(move |this, _, _, cx| {
                    if let Some(aid) = this.current_account_id.clone() {
                        this.settings.account_mut(&aid).auto_refresh_secs = secs;
                        this.settings.save();
                        this.mailbox.last_auto_refresh_sent = None;
                        this.sync_auto_refresh();
                    }
                    cx.notify();
                }),
            )
        };
        let cache_limit = self.settings.global.mail_cache_limit_mb;
        let cache_button =
            |id: &'static str, label: String, limit_mb: u64, cx: &mut Context<Self>| {
                super::choice_button(
                    Button::new(id).small().label(label),
                    cache_limit == limit_mb,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.settings.global.mail_cache_limit_mb = limit_mb;
                    this.settings.save();
                    this.send(Cmd::SetMailCacheLimit { limit_mb });
                    cx.notify();
                }))
            };
        let used_mb = self.mail_cache_used_bytes as f64 / (1024.0 * 1024.0);
        let measured_limit_mb = self.mail_cache_limit_bytes as f64 / (1024.0 * 1024.0);
        let group_by_conversation = self.settings.global.group_by_conversation;

        v_flex()
            .child(
                self.section(&tr!("settings-message-list-heading"), cx)
                    .child(
                        Switch::new("group-by-conversation")
                            .checked(group_by_conversation)
                            .label(tr!("settings-group-by-conversation"))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.settings.global.group_by_conversation = *checked;
                                this.settings.save();
                                // The list model changes shape, not just its
                                // styling: it has to be rebuilt and remeasured.
                                this.invalidate_message_list();
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(tr!("settings-group-by-conversation-help")),
                    ),
            )
            .child(
                self.section(&tr!("settings-loading"), cx)
                    .child(labelled(
                        &tr!("settings-inbox-page-size"),
                        &ui.fetch_limit,
                        cx,
                    ))
                    .child(labelled(
                        &tr!("settings-inbox-sender-history-size"),
                        &ui.sender_history_limit,
                        cx,
                    ))
                    .child(
                        Button::new("save-inbox")
                            .primary()
                            .label(tr!("settings-apply"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let (Some(ui), Some(aid)) =
                                    (&this.settings_ui, this.current_account_id.clone())
                                {
                                    if let Ok(v) =
                                        ui.fetch_limit.read(cx).value().trim().parse::<usize>()
                                    {
                                        this.settings.account_mut(&aid).fetch_limit = v.max(1);
                                    }
                                    if let Ok(v) = ui
                                        .sender_history_limit
                                        .read(cx)
                                        .value()
                                        .trim()
                                        .parse::<usize>()
                                    {
                                        this.settings.account_mut(&aid).sender_history_limit =
                                            v.max(1);
                                    }
                                    this.settings.save();
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                self.section(&tr!("settings-auto-refresh-title"), cx).child(
                    h_flex()
                        .gap_2()
                        .child(auto_button(
                            "ar-off",
                            tr!("auto-refresh-disabled").to_string(),
                            0,
                            cx,
                        ))
                        .child(auto_button(
                            "ar-1",
                            tr!("auto-refresh-1m").to_string(),
                            60,
                            cx,
                        ))
                        .child(auto_button(
                            "ar-5",
                            tr!("auto-refresh-5m").to_string(),
                            300,
                            cx,
                        ))
                        .child(auto_button(
                            "ar-15",
                            tr!("auto-refresh-15m").to_string(),
                            900,
                            cx,
                        )),
                ),
            )
            .child(
                self.section(&tr!("settings-cache-title"), cx)
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(tr!("settings-cache-usage", {
                                used: format!("{used_mb:.1}"),
                                limit: format!("{measured_limit_mb:.0}")
                            })),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(cache_button(
                                "cache-100",
                                tr!("size-mb", { value: 100 }).to_string(),
                                100,
                                cx,
                            ))
                            .child(cache_button(
                                "cache-250",
                                tr!("size-mb", { value: 250 }).to_string(),
                                250,
                                cx,
                            ))
                            .child(cache_button(
                                "cache-500",
                                tr!("size-mb", { value: 500 }).to_string(),
                                500,
                                cx,
                            ))
                            .child(cache_button(
                                "cache-1000",
                                tr!("size-gb", { value: 1 }).to_string(),
                                1_000,
                                cx,
                            ))
                            .child(cache_button(
                                "cache-2000",
                                tr!("size-gb", { value: 2 }).to_string(),
                                2_000,
                                cx,
                            )),
                    )
                    .child(
                        Button::new("clear-mail-cache")
                            .label(tr!("settings-cache-clear"))
                            .on_click(cx.listener(|this, _, _, _| {
                                this.send(Cmd::ClearMailCache);
                            })),
                    ),
            )
    }

    pub(super) fn render_settings_notifications(
        &mut self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let g = self.settings.global.clone();
        let ui = self.settings_ui.as_ref().expect("settings_ui");
        v_flex()
            .child(
                self.section(&tr!("settings-notifications-heading"), cx)
                    .child(
                        Switch::new("notif")
                            .checked(g.notifications_enabled)
                            .label(tr!("settings-notifications-new-messages"))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.settings.global.notifications_enabled = *checked;
                                this.settings.save();
                                cx.notify();
                            })),
                    )
                    .child(
                        Switch::new("tray")
                            .checked(g.tray_enabled)
                            .label(tr!("settings-tray-restart-label"))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.settings.global.tray_enabled = *checked;
                                this.settings.save();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                self.section(&tr!("settings-action-delay-heading"), cx)
                    .child(
                        Switch::new("action-delay")
                            .checked(g.action_delay_enabled)
                            .label(tr!("settings-action-delay-enabled"))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.settings.global.action_delay_enabled = *checked;
                                this.settings.save();
                                cx.notify();
                            })),
                    )
                    .when(g.action_delay_enabled, |section| {
                        section
                            .child(labelled(
                                &tr!("settings-action-delay"),
                                &ui.action_delay_secs,
                                cx,
                            ))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(tr!("settings-action-delay-help")),
                            )
                            .child(
                                Button::new("save-action-delay")
                                    .primary()
                                    .label(tr!("settings-apply"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let Some(ui) = &this.settings_ui else {
                                            return;
                                        };
                                        if let Ok(value) = ui
                                            .action_delay_secs
                                            .read(cx)
                                            .value()
                                            .trim()
                                            .parse::<u32>()
                                        {
                                            this.settings.global.action_delay_secs =
                                                value.clamp(1, 300);
                                            this.settings.save();
                                        }
                                        cx.notify();
                                    })),
                            )
                    }),
            )
            .child(
                self.section(&tr!("settings-send-delay-heading"), cx)
                    .child(labelled(
                        &tr!("settings-send-delay"),
                        &ui.send_delay_secs,
                        cx,
                    ))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(tr!("settings-send-delay-help")),
                    )
                    .child(
                        Button::new("save-send-delay")
                            .primary()
                            .label(tr!("settings-apply"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                let Some(ui) = &this.settings_ui else {
                                    return;
                                };
                                if let Ok(value) =
                                    ui.send_delay_secs.read(cx).value().trim().parse::<u32>()
                                {
                                    this.settings.global.send_delay_secs = value.min(300);
                                    this.settings.save();
                                }
                                cx.notify();
                            })),
                    ),
            )
    }

    pub(super) fn render_settings_security(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let g = self.settings.global.clone();
        v_flex()
            .child(
                self.section(&tr!("settings-security-heading"), cx).child(
                    Switch::new("remote-images")
                        .checked(g.show_remote_images)
                        .label(tr!("settings-security-remote-images"))
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.settings.global.show_remote_images = *checked;
                            this.settings.save();
                            cx.notify();
                        })),
                ),
            )
            .child(self.render_blocked_senders_section(cx))
            .child(
                self.section(&tr!("settings-danger-zone"), cx)
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(tr!("settings-factory-reset-explanation-short")),
                    )
                    .child(
                        Button::new("factory-reset")
                            .danger()
                            .label(tr!("settings-factory-reset-button"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                let _ = this;
                                let entity = cx.entity();
                                gpui_component::WindowExt::open_dialog(
                                    window,
                                    cx,
                                    move |dialog, _window, _cx| {
                                        let entity = entity.clone();
                                        dialog
                                            .title(tr!("settings-factory-reset-title"))
                                            .confirm()
                                            .child(
                                                div().child(tr!(
                                                    "settings-factory-reset-confirm-short"
                                                )),
                                            )
                                            .on_ok(move |_, window, cx| {
                                                entity.update(cx, |this, cx| {
                                                    this.perform_factory_reset(window, cx);
                                                });
                                                true
                                            })
                                    },
                                );
                            })),
                    ),
            )
    }

    /// Locally blocked senders: whose mail is moved to junk on arrival.
    ///
    /// Local because the alternative is not: Graph exposes no blocked-senders
    /// list on v1.0 and Gmail's filters live behind `gmail.settings.basic` —
    /// both would force every account to re-authenticate for a feature IMAP
    /// could not have at all. Applying the list here works the same on the
    /// three backends, at the cost of only running while Aviary is open.
    fn render_blocked_senders_section(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let blocked = self.settings.global.blocked_senders.clone();
        let input = self
            .settings_ui
            .as_ref()
            .expect("settings_ui")
            .blocked_sender
            .clone();
        let mut section = self
            .section(&tr!("settings-blocked-senders-heading"), cx)
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr!("settings-blocked-senders-explanation")),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().child(Input::new(&input)))
                    .child(
                        Button::new("blocked-senders-add")
                            .small()
                            .label(tr!("settings-blocked-senders-add"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_blocked_sender_from_input(window, cx);
                            })),
                    ),
            );
        if blocked.is_empty() {
            return section.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr!("settings-blocked-senders-empty")),
            );
        }
        for address in blocked {
            let removed = address.clone();
            section = section.child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().text_sm().child(address.clone()))
                    .child(
                        Button::new(gpui::ElementId::Name(
                            format!("blocked-sender-remove-{address}").into(),
                        ))
                        .ghost()
                        .xsmall()
                        .icon(super::super::icons::app_icon("x"))
                        .tooltip(tr!("settings-blocked-senders-remove"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.settings.global.unblock_sender(&removed);
                            this.settings.save();
                            cx.notify();
                        })),
                    ),
            );
        }
        section
    }

    /// Typing an address that is not one leaves the field alone rather than
    /// storing something that can never match a `From` header.
    fn add_blocked_sender_from_input(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let input = self
            .settings_ui
            .as_ref()
            .expect("settings_ui")
            .blocked_sender
            .clone();
        let value = input.read(cx).value().trim().to_string();
        if !self.settings.global.block_sender(&value) {
            return;
        }
        self.settings.save();
        input.update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
    }

    pub(super) fn perform_factory_reset(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        for a in self.accounts.clone() {
            self.send(Cmd::Logout(a.id));
        }
        if let Err(e) = crate::auth::clear_all_tokens() {
            log::warn!("factory reset: failed to clear tokens: {e:#}");
        }
        self.send(Cmd::ClearMailCache);
        self.send(Cmd::ResetLanguageTool);
        self.settings = super::super::settings::Settings::default();
        super::super::settings::AppSession::remove_file();
        self.reset_working_session(window, cx);
        super::super::set_i18n_language(self.settings.global.language);
        #[cfg(target_os = "linux")]
        if let Some(tray) = &self.tray {
            tray.refresh_i18n();
        }
        self.settings.save();
        self.settings_ui = None;
        cx.refresh_windows();
        cx.notify();
    }
}
