//! Window chrome: the top bar (tab bar, account switcher, global actions), the
//! sidebar navigation, and the root `Render` impl that assembles the current
//! view and re-emits the `Root` notification, dialog and sheet layers.

use crate::ui::app::AviaryApp;
use crate::ui::compose::ComposeInit;
use crate::ui::settings::ThemeMode;
use crate::ui::state::{AuthState, MainView};
use gpui::{div, prelude::*, Context, Render, Window};
use gpui_component::{
    button::{Button, ButtonGroup, ButtonVariants},
    h_flex,
    menu::{DropdownMenu, PopupMenuItem},
    ActiveTheme, IconName, Root, Selectable, Sizable,
};

impl AviaryApp {
    pub(crate) fn render_topbar(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let authenticated = matches!(self.auth, AuthState::Authenticated);
        let is_dark = self.settings.global.theme_mode == ThemeMode::Dark;
        // The popup is cached by ElementId. Include the state of
        // connection and language force reconstruction when either changes.
        let app_menu_variant = usize::from(authenticated) * 2
            + usize::from(self.settings.global.language.to_lang_id() == "fr");
        let app_menu_entity = cx.entity();
        let app_menu = Button::new(("app-menu", app_menu_variant))
            .ghost()
            .small()
            .compact()
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .child(crate::ui::icons::app_icon("aviary").large())
                    .child(tr!("menu-aviary")),
            )
            .dropdown_menu(move |menu, _window, _cx| {
                let new_message_entity = app_menu_entity.clone();
                let refresh_entity = app_menu_entity.clone();
                let preferences_entity = app_menu_entity.clone();
                let reset_session_entity = app_menu_entity.clone();
                let quit_entity = app_menu_entity.clone();

                menu.item(
                    PopupMenuItem::new(tr!("menu-new-message"))
                        .icon(crate::ui::icons::app_icon("pencil"))
                        .disabled(!authenticated)
                        .on_click(move |_, window, cx| {
                            new_message_entity.update(cx, |this, cx| {
                                this.open_inline_compose(ComposeInit::blank(), window, cx);
                            });
                        }),
                )
                .item(
                    PopupMenuItem::new(tr!("menu-refresh"))
                        .icon(crate::ui::icons::app_icon("refresh-cw"))
                        .disabled(!authenticated)
                        .on_click(move |_, _, cx| {
                            refresh_entity.update(cx, |this, cx| {
                                match this.view {
                                    MainView::Mail => this.send_refresh(),
                                    MainView::Calendar => this.calendar.force_reload(),
                                    MainView::Kanban => this.reload_kanban(),
                                    MainView::Contacts => this.reload_contacts(),
                                    MainView::Settings => {}
                                }
                                cx.notify();
                            });
                        }),
                )
                .item(PopupMenuItem::separator())
                .item(
                    PopupMenuItem::new(tr!("menu-preferences"))
                        .icon(IconName::Settings)
                        .on_click(move |_, window, cx| {
                            preferences_entity.update(cx, |this, cx| {
                                this.enter_main_view(MainView::Settings, cx);
                                this.focus_shortcuts(window);
                            });
                        }),
                )
                .item(PopupMenuItem::separator())
                .item(
                    PopupMenuItem::new(tr!("menu-reset-session"))
                        .icon(crate::ui::icons::app_icon("refresh-cw"))
                        .on_click(move |_, window, cx| {
                            reset_session_entity.update(cx, |this, cx| {
                                this.confirm_reset_session(window, cx);
                            });
                        }),
                )
                .item(PopupMenuItem::separator())
                .item(
                    PopupMenuItem::new(tr!("menu-quit"))
                        .icon(crate::ui::icons::app_icon("log-out"))
                        .on_click(move |_, _, cx| {
                            quit_entity.update(cx, |this, cx| {
                                #[cfg(target_os = "linux")]
                                if let Some(tray) = this.tray.take() {
                                    tray.shutdown();
                                }
                                cx.quit();
                            });
                        }),
                )
            });

        let primary_action = authenticated.then(|| match self.view {
            MainView::Mail => Button::new("new-mail")
                .primary()
                .small()
                .icon(crate::ui::icons::app_icon("pencil"))
                .label(tr!("toolbar-new-message"))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_inline_compose(ComposeInit::blank(), window, cx);
                }))
                .into_any_element(),
            MainView::Calendar => Button::new("new-calendar-event")
                .primary()
                .small()
                .icon(crate::ui::icons::app_icon("calendar"))
                .label(tr!("calendar-new-event"))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_event_compose(window, cx);
                }))
                .into_any_element(),
            MainView::Kanban => {
                if self.accounts.is_empty() {
                    div().into_any_element()
                } else {
                    self.render_add_column_button("kanban-topbar-add-column", false, cx)
                        .into_any_element()
                }
            }
            MainView::Contacts => {
                let recipient = self.contacts.selected.clone();
                Button::new("new-contact-mail")
                    .primary()
                    .small()
                    .icon(crate::ui::icons::app_icon("pencil"))
                    .label(tr!("toolbar-new-message"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let init = recipient
                            .clone()
                            .map(ComposeInit::with_to)
                            .unwrap_or_else(ComposeInit::blank);
                        this.open_inline_compose(init, window, cx);
                    }))
                    .into_any_element()
            }
            MainView::Settings => div().into_any_element(),
        });

        gpui_component::TitleBar::new()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(app_menu)
                    .when(authenticated, |el| {
                        el.child(
                            Button::new("refresh")
                                .ghost()
                                .small()
                                .icon(crate::ui::icons::app_icon("refresh-cw"))
                                .tooltip(tr!("toolbar-refresh"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    match this.view {
                                        MainView::Mail => this.send_refresh(),
                                        MainView::Calendar => this.calendar.force_reload(),
                                        MainView::Kanban => this.reload_kanban(),
                                        MainView::Contacts => this.reload_contacts(),
                                        MainView::Settings => {}
                                    }
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when(!authenticated, |el| {
                        el.child(
                            Button::new("open-logs-auth")
                                .ghost()
                                .small()
                                .icon(IconName::SquareTerminal)
                                .tooltip(tr!("logs-open"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_logs_settings(window, cx);
                                })),
                        )
                    })
                    .children(primary_action)
                    .child(
                        Button::new("theme-toggle")
                            .ghost()
                            .small()
                            .icon(if is_dark {
                                IconName::Sun
                            } else {
                                IconName::Moon
                            })
                            .tooltip(if is_dark {
                                tr!("tooltip-toggle-theme-light")
                            } else {
                                tr!("tooltip-toggle-theme-dark")
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                let mode = if this.settings.global.theme_mode == ThemeMode::Dark {
                                    ThemeMode::Light
                                } else {
                                    ThemeMode::Dark
                                };
                                if this.settings.global.custom_theme_enabled {
                                    this.select_custom_theme_mode(mode, window, cx);
                                } else {
                                    this.settings.global.select_builtin_theme_mode(mode);
                                }
                                crate::ui::theme::apply(&this.settings.global, Some(window), cx);
                                cx.notify();
                                // Paint the new palette before touching the
                                // settings file. A slow filesystem must not
                                // make the title-bar click look ignored.
                                cx.on_next_frame(window, |this, _, _| {
                                    this.settings.save();
                                });
                            })),
                    ),
            )
    }

    /// Compact main navigation placed at the bottom of the left pane of
    /// each view, as in Outlook.
    pub(crate) fn render_sidebar_navigation(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        h_flex()
            .w_full()
            .items_center()
            .justify_center()
            .px_2()
            .py_1p5()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(
                ButtonGroup::new("sidebar-navigation")
                    .outline()
                    .compact()
                    .child(
                        Button::new("nav-mail")
                            .icon(IconName::Inbox)
                            .tooltip(tr!("nav-mail"))
                            .selected(self.view == MainView::Mail),
                    )
                    .child(
                        Button::new("nav-calendar")
                            .icon(IconName::Calendar)
                            .tooltip(tr!("calendar-title"))
                            .selected(self.view == MainView::Calendar),
                    )
                    .child(
                        Button::new("nav-kanban")
                            .icon(crate::ui::icons::app_icon("square-kanban"))
                            .tooltip(tr!("kanban-title"))
                            .selected(self.view == MainView::Kanban),
                    )
                    .child(
                        Button::new("nav-contacts")
                            .icon(crate::ui::icons::app_icon("users"))
                            .tooltip(tr!("contacts-title"))
                            .selected(self.view == MainView::Contacts),
                    )
                    .child(
                        Button::new("nav-settings")
                            .icon(IconName::Settings)
                            .tooltip(tr!("settings-title"))
                            .selected(self.view == MainView::Settings),
                    )
                    .on_click(cx.listener(|this, selected: &Vec<usize>, window, cx| {
                        let Some(index) = selected.first() else {
                            return;
                        };
                        let view = match index {
                            0 => MainView::Mail,
                            1 => MainView::Calendar,
                            2 => MainView::Kanban,
                            3 => MainView::Contacts,
                            _ => MainView::Settings,
                        };
                        this.enter_main_view(view, cx);
                        this.focus_shortcuts(window);
                    })),
            )
    }
}

impl Render for AviaryApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::ui::theme::apply_window_scale(window, cx);
        self.message_row_hover.request_frame(window);
        let authenticated = matches!(self.auth, AuthState::Authenticated);
        let show_auth = self.imap_form.is_some()
            || self.auth.is_in_progress()
            || (!authenticated && self.view != MainView::Settings);
        let content = if show_auth {
            self.render_auth_view(window, cx).into_any_element()
        } else {
            match self.view {
                MainView::Mail => self.render_inbox(window, cx).into_any_element(),
                MainView::Calendar => self.render_calendar(window, cx).into_any_element(),
                MainView::Kanban => self.render_kanban(window, cx).into_any_element(),
                MainView::Contacts => self.render_contacts(window, cx).into_any_element(),
                MainView::Settings => self.render_settings(window, cx).into_any_element(),
            }
        };
        gpui_component::v_flex()
            .key_context(crate::ui::shortcuts::main_context(
                self.settings.global.vim_keybindings,
            ))
            .track_focus(&self.shortcut_focus)
            .on_action(cx.listener(crate::ui::shortcuts::new_message))
            .on_action(cx.listener(crate::ui::shortcuts::refresh))
            .on_action(cx.listener(crate::ui::shortcuts::focus_search))
            .on_action(cx.listener(crate::ui::shortcuts::blur_search))
            .on_action(cx.listener(crate::ui::shortcuts::show_mail))
            .on_action(cx.listener(crate::ui::shortcuts::show_calendar))
            .on_action(cx.listener(crate::ui::shortcuts::show_kanban))
            .on_action(cx.listener(crate::ui::shortcuts::show_contacts))
            .on_action(cx.listener(crate::ui::shortcuts::show_settings))
            .on_action(cx.listener(crate::ui::shortcuts::previous_view))
            .on_action(cx.listener(crate::ui::shortcuts::next_view))
            .on_action(cx.listener(crate::ui::shortcuts::previous_item))
            .on_action(cx.listener(crate::ui::shortcuts::next_item))
            .on_action(cx.listener(crate::ui::shortcuts::first_item))
            .on_action(cx.listener(crate::ui::shortcuts::last_item))
            .on_action(cx.listener(crate::ui::shortcuts::reply_message))
            .on_action(cx.listener(crate::ui::shortcuts::reply_all))
            .on_action(cx.listener(crate::ui::shortcuts::forward_message))
            .on_action(cx.listener(crate::ui::shortcuts::open_quick_actions))
            .on_action(cx.listener(crate::ui::shortcuts::print_message))
            .on_action(cx.listener(crate::ui::shortcuts::select_all_messages))
            .on_action(cx.listener(crate::ui::shortcuts::clear_message_selection))
            .on_action(cx.listener(crate::ui::shortcuts::archive_message))
            .on_action(cx.listener(crate::ui::shortcuts::delete_message))
            .on_action(cx.listener(crate::ui::shortcuts::toggle_flag))
            .on_action(cx.listener(crate::ui::shortcuts::mark_unread))
            .on_action(cx.listener(crate::ui::shortcuts::close_current))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_topbar(window, cx))
            .child(div().flex_1().min_h_0().child(content))
            .children(self.notification_layer.clone())
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_sheet_layer(window, cx))
            .children(crate::ui::image_lightbox::render(window, cx))
    }
}
