//! External read-only iCalendar subscriptions.

use super::super::app::AviaryApp;
use crate::model::{AccountId, IcalRefreshInterval, IcalSubscription};
use crate::runtime::Cmd;
use gpui::{div, prelude::*, Context, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    color_picker::ColorPicker,
    h_flex,
    input::Input,
    menu::{DropdownMenu, PopupMenuItem},
    switch::Switch,
    v_flex, ActiveTheme, Disableable, Sizable, StyledExt, WindowExt,
};

impl AviaryApp {
    pub(super) fn render_settings_calendars(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = self.settings_ui.as_ref().expect("settings_ui");
        let editing = ui.editing_ical_id.is_some();
        let selected_refresh = ui.ical_refresh;
        let subscriptions = self.render_ical_subscriptions(cx);

        let refresh_label = ical_refresh_label(selected_refresh);
        let entity = cx.entity();
        v_flex()
            .gap_4()
            .child(self.render_calendar_grid_section(cx))
            .child(self.render_calendar_upcoming_section(cx))
            .child(
                self.section(&tr!("settings-ical-title"), cx)
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(tr!("settings-ical-description")),
                    )
                    .child(subscriptions),
            )
            .child(
                self.section(
                    &if editing {
                        tr!("settings-ical-edit-title")
                    } else {
                        tr!("settings-ical-add-title")
                    },
                    cx,
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(div().text_sm().child(tr!("settings-ical-name")))
                        .child(Input::new(&ui.ical_name).w_full()),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(div().text_sm().child(tr!("settings-ical-url")))
                        .child(Input::new(&ui.ical_url).w_full())
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(tr!("settings-ical-url-help")),
                        ),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .items_end()
                        .child(
                            v_flex()
                                .gap_1()
                                .child(div().text_sm().child(tr!("settings-ical-color")))
                                .child(ColorPicker::new(&ui.ical_color).small()),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .gap_1()
                                .child(div().text_sm().child(tr!("settings-ical-frequency")))
                                .child(
                                    Button::new("ical-frequency")
                                        .outline()
                                        .w_full()
                                        .label(refresh_label)
                                        .dropdown_menu(move |mut menu, _, _| {
                                            for refresh in IcalRefreshInterval::ALL {
                                                let refresh = *refresh;
                                                let entity = entity.clone();
                                                menu = menu.item(
                                                    PopupMenuItem::new(ical_refresh_label(refresh))
                                                        .checked(refresh == selected_refresh)
                                                        .on_click(move |_, _, cx| {
                                                            entity.update(cx, |this, cx| {
                                                                if let Some(ui) =
                                                                    &mut this.settings_ui
                                                                {
                                                                    ui.ical_refresh = refresh;
                                                                }
                                                                cx.notify();
                                                            });
                                                        }),
                                                );
                                            }
                                            menu
                                        }),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("save-ical-subscription")
                                .primary()
                                .label(if editing {
                                    tr!("save")
                                } else {
                                    tr!("settings-ical-add")
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.commit_ical_subscription(window, cx);
                                })),
                        )
                        .when(editing, |element| {
                            element.child(
                                Button::new("cancel-ical-edit")
                                    .ghost()
                                    .label(tr!("cancel"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.clear_ical_editor(window, cx);
                                        cx.notify();
                                    })),
                            )
                        }),
                ),
            )
    }

    fn commit_ical_subscription(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ui) = &self.settings_ui else {
            return;
        };
        let name = ui.ical_name.read(cx).value().trim().to_string();
        let raw_url = ui.ical_url.read(cx).value().trim().to_string();
        let Some(url) = normalize_ical_url(&raw_url) else {
            self.notify_error(tr!("settings-ical-invalid-url"), window, cx);
            return;
        };
        if name.is_empty() {
            self.notify_error(tr!("settings-ical-name-required"), window, cx);
            return;
        }
        let color = ui
            .ical_color
            .read(cx)
            .value()
            .map(super::super::util::color_to_packed)
            .unwrap_or(0x61afef);
        let refresh = ui.ical_refresh;
        let editing_id = ui.editing_ical_id.clone();
        let id = if let Some(id) = editing_id {
            if let Some(subscription) = self
                .settings
                .global
                .ical_subscriptions
                .iter_mut()
                .find(|subscription| subscription.id == id)
            {
                subscription.name = name;
                subscription.url = url;
                subscription.color = color;
                subscription.refresh = refresh;
            }
            id
        } else {
            self.settings.global.ical_subscription_seq += 1;
            let id = self.settings.global.ical_subscription_seq.to_string();
            self.settings
                .global
                .ical_subscriptions
                .push(IcalSubscription {
                    id: id.clone(),
                    name,
                    url,
                    color,
                    refresh,
                });
            id
        };
        self.settings.save();
        self.sync_ical_subscriptions();
        self.send(Cmd::RefreshIcalSubscription {
            subscription_id: id,
        });
        self.calendar.force_reload();
        self.clear_ical_editor(window, cx);
        cx.notify();
    }

    fn edit_ical_subscription(
        &mut self,
        subscription: &IcalSubscription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ui) = &mut self.settings_ui else {
            return;
        };
        ui.editing_ical_id = Some(subscription.id.clone());
        ui.ical_refresh = subscription.refresh;
        ui.ical_name.update(cx, |input, cx| {
            input.set_value(&subscription.name, window, cx)
        });
        ui.ical_url.update(cx, |input, cx| {
            input.set_value(&subscription.url, window, cx)
        });
        ui.ical_color.update(cx, |picker, cx| {
            picker.set_value(
                super::super::util::packed_color(subscription.color),
                window,
                cx,
            )
        });
        cx.notify();
    }

    fn clear_ical_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ui) = &mut self.settings_ui else {
            return;
        };
        ui.editing_ical_id = None;
        ui.ical_refresh = IcalRefreshInterval::OneHour;
        ui.ical_name
            .update(cx, |input, cx| input.set_value("", window, cx));
        ui.ical_url
            .update(cx, |input, cx| input.set_value("", window, cx));
        ui.ical_color.update(cx, |picker, cx| {
            picker.set_value(
                super::super::util::account_color(&AccountId("ical-new".into()), None),
                window,
                cx,
            )
        });
    }

    fn confirm_delete_ical_subscription(
        &mut self,
        subscription_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entity = cx.entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let entity = entity.clone();
            let subscription_id = subscription_id.clone();
            dialog
                .title(tr!("settings-ical-delete-title"))
                .confirm()
                .child(div().child(tr!("settings-ical-delete-confirm")))
                .on_ok(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        let was_editing = this.settings_ui.as_ref().is_some_and(|ui| {
                            ui.editing_ical_id.as_ref() == Some(&subscription_id)
                        });
                        this.settings
                            .global
                            .ical_subscriptions
                            .retain(|subscription| subscription.id != subscription_id);
                        let account_id = AccountId(format!("ical:{subscription_id}"));
                        this.settings
                            .global
                            .calendar_hidden_account_ids
                            .retain(|id| id != &account_id.0);
                        this.calendar
                            .events
                            .retain(|event| event.account_id != account_id);
                        this.calendar.ical_sync.remove(&subscription_id);
                        this.settings.save();
                        this.sync_ical_subscriptions();
                        this.send(Cmd::DeleteIcalSubscriptionCache {
                            subscription_id: subscription_id.clone(),
                        });
                        if was_editing {
                            this.clear_ical_editor(window, cx);
                        }
                        this.calendar.force_reload();
                        cx.notify();
                    });
                    true
                })
        });
    }

    pub(super) fn sync_ical_subscriptions(&self) {
        self.send(Cmd::ConfigureIcalSubscriptions(
            self.settings.global.ical_subscriptions.clone(),
        ));
    }

    /// The subscribed iCal feeds, with their sync state and per-feed actions.
    fn render_ical_subscriptions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut subscriptions = v_flex().gap_2();

        if self.settings.global.ical_subscriptions.is_empty() {
            subscriptions = subscriptions.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tr!("settings-ical-empty")),
            );
        }
        for subscription in self.settings.global.ical_subscriptions.clone() {
            let id = subscription.id.clone();
            let edit = subscription.clone();
            let refresh_id = id.clone();
            let delete_id = id.clone();
            let status = self
                .calendar
                .ical_sync
                .get(&id)
                .cloned()
                .unwrap_or_default();
            let endpoint = reqwest::Url::parse(&subscription.url)
                .ok()
                .and_then(|url| url.host_str().map(ToOwned::to_owned))
                .unwrap_or_else(|| tr!("settings-ical-private-url").to_string());
            let state = if status.syncing {
                tr!("settings-ical-syncing")
            } else if let Some(ref error) = status.error {
                tr!("settings-ical-error", { error: error })
            } else if let Some(last_success) = status.last_success {
                tr!("settings-ical-last-sync", {
                    date: super::super::util::full_date(&last_success)
                })
            } else {
                tr!("settings-ical-not-synced")
            };
            subscriptions = subscriptions.child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                div()
                                    .w(gpui::px(10.))
                                    .h(gpui::px(10.))
                                    .rounded_full()
                                    .bg(super::super::util::packed_color(subscription.color)),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .font_semibold()
                                            .truncate()
                                            .child(subscription.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(theme.muted_foreground)
                                            .truncate()
                                            .child(endpoint),
                                    ),
                            )
                            .child(
                                Button::new(gpui::ElementId::Name(
                                    format!("ical-refresh-{id}").into(),
                                ))
                                .ghost()
                                .small()
                                .icon(super::super::icons::app_icon("refresh-cw"))
                                .disabled(status.syncing)
                                .tooltip(tr!("settings-ical-refresh-now"))
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.send(Cmd::RefreshIcalSubscription {
                                            subscription_id: refresh_id.clone(),
                                        });
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                Button::new(gpui::ElementId::Name(
                                    format!("ical-edit-{id}").into(),
                                ))
                                .ghost()
                                .small()
                                .icon(super::super::icons::app_icon("pencil"))
                                .tooltip(tr!("edit"))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.edit_ical_subscription(&edit, window, cx);
                                    },
                                )),
                            )
                            .child(
                                Button::new(gpui::ElementId::Name(
                                    format!("ical-delete-{id}").into(),
                                ))
                                .ghost()
                                .danger()
                                .small()
                                .icon(super::super::icons::app_icon("trash-2"))
                                .tooltip(tr!("delete"))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.confirm_delete_ical_subscription(
                                            delete_id.clone(),
                                            window,
                                            cx,
                                        );
                                    },
                                )),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if status.error.is_some() {
                                theme.danger
                            } else {
                                theme.muted_foreground
                            })
                            .child(state),
                    ),
            );
        }
        subscriptions
    }

    /// Week grid: continuous scrolling and how many weeks a screen holds.
    fn render_calendar_grid_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = self.settings_ui.as_ref().expect("settings_ui");
        self.section(&tr!("settings-calendar-grid-heading"), cx)
            .child(
                Switch::new("calendar-infinite-scroll")
                    .checked(self.settings.global.calendar_infinite_scroll)
                    .label(tr!("settings-calendar-infinite-scroll"))
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.settings.global.calendar_infinite_scroll = *checked;
                        this.settings.save();
                        if *checked {
                            // Re-enter the continuous grid on the
                            // month that was being viewed.
                            this.calendar.grid_scroll_to(this.calendar.anchor_date());
                        }
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tr!("settings-calendar-grid-help")),
            )
            .child(super::labelled(
                &tr!("settings-calendar-grid-weeks"),
                &ui.calendar_grid_weeks,
                cx,
            ))
            .child(
                Button::new("save-calendar-grid-weeks")
                    .primary()
                    .label(tr!("settings-apply"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let Some(ui) = &this.settings_ui else {
                            return;
                        };
                        if let Ok(value) = ui
                            .calendar_grid_weeks
                            .read(cx)
                            .value()
                            .trim()
                            .parse::<u32>()
                        {
                            this.settings.global.calendar_grid_weeks = value.clamp(2, 12);
                            this.settings.save();
                        }
                        cx.notify();
                    })),
            )
    }

    /// How far ahead the \u00ab \u00c0 venir \u00bb sub-mode looks.
    fn render_calendar_upcoming_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = self.settings_ui.as_ref().expect("settings_ui");
        self.section(&tr!("settings-calendar-upcoming-heading"), cx)
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tr!("settings-calendar-upcoming-help")),
            )
            .child(super::labelled(
                &tr!("settings-calendar-upcoming-days"),
                &ui.calendar_upcoming_days,
                cx,
            ))
            .child(
                Button::new("save-calendar-upcoming-days")
                    .primary()
                    .label(tr!("settings-apply"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let Some(ui) = &this.settings_ui else {
                            return;
                        };
                        if let Ok(value) = ui
                            .calendar_upcoming_days
                            .read(cx)
                            .value()
                            .trim()
                            .parse::<u32>()
                        {
                            this.settings.global.calendar_upcoming_days = value.clamp(1, 365);
                            this.settings.save();
                        }
                        cx.notify();
                    })),
            )
    }
}

fn normalize_ical_url(raw: &str) -> Option<String> {
    let normalized = raw
        .trim()
        .strip_prefix("webcal://")
        .map(|rest| format!("https://{rest}"))
        .unwrap_or_else(|| raw.trim().to_string());
    let url = reqwest::Url::parse(&normalized).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.to_string())
}

fn ical_refresh_label(refresh: IcalRefreshInterval) -> gpui::SharedString {
    match refresh {
        IcalRefreshInterval::Manual => tr!("settings-ical-frequency-manual"),
        IcalRefreshInterval::FifteenMinutes => tr!("settings-ical-frequency-15m"),
        IcalRefreshInterval::ThirtyMinutes => tr!("settings-ical-frequency-30m"),
        IcalRefreshInterval::OneHour => tr!("settings-ical-frequency-1h"),
        IcalRefreshInterval::SixHours => tr!("settings-ical-frequency-6h"),
        IcalRefreshInterval::OneDay => tr!("settings-ical-frequency-1d"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_webcal_and_rejects_insecure_urls() {
        assert_eq!(
            normalize_ical_url("webcal://example.com/calendar.ics").as_deref(),
            Some("https://example.com/calendar.ics")
        );
        assert!(normalize_ical_url("http://example.com/calendar.ics").is_none());
        assert!(normalize_ical_url("https://user:secret@example.com/calendar.ics").is_none());
    }
}
