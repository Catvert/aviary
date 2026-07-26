//! Everything above the body: subject, actions, addresses, and tags.
//!
//! The reader's chrome reflows below two pane widths (see [`ViewerChrome`]), so
//! the sections are built separately and assembled by
//! [`AviaryApp::render_viewer_header`].

use super::*;

/// What the reader's chrome needs to know before drawing itself: the settings
/// that change what the body looks like, and the two pane widths below which
/// the header reflows.
pub(super) struct ViewerChrome {
    pub(super) mode: BodyViewMode,
    pub(super) options: MailBodyOptions,
    pub(super) offline: bool,
    pub(super) reply_all_primary: bool,
    pub(super) collapse_quoted_messages: bool,
    /// Already pinned in a tab, so the pin button would do nothing.
    pub(super) in_tab: bool,
    /// Actions take a row of their own under the subject.
    pub(super) compact: bool,
    /// Buttons drop their labels and keep only tooltips.
    pub(super) very_compact: bool,
}

impl AviaryApp {
    /// Address chip used in the reader header. Clicking opens address-related
    /// actions without visually overloading the header.
    fn render_viewer_address_pill(
        &self,
        raw: &str,
        id: String,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let raw = raw.trim();
        let email = util::extract_email(raw);
        let label = if let Some(email) = &email {
            let name = util::display_name(raw);
            if name.eq_ignore_ascii_case(email) {
                email.clone()
            } else {
                format!("{name} <{email}>")
            }
        } else {
            raw.to_string()
        };
        let label = if compact {
            compact_address_label(raw, email.as_deref())
        } else {
            label
        };
        let copy_value = email.clone().unwrap_or_else(|| raw.to_string());
        let target = email.clone().unwrap_or_default();
        let has_email = email.is_some();
        let app = cx.entity();

        Button::new(gpui::ElementId::Name(id.into()))
            .outline()
            .rounded(px(999.))
            .xsmall()
            .label(label)
            .tooltip(copy_value.clone())
            .dropdown_menu(move |mut menu, _window, _cx| {
                let address = copy_value.clone();
                menu = menu.item(PopupMenuItem::new(tr!("viewer-address-copy")).on_click(
                    move |_, window, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(address.clone()));
                        window.push_notification(
                            tr!("viewer-address-copied", { address: address.clone() }),
                            cx,
                        );
                    },
                ));
                {
                    let app = app.clone();
                    let address = target.clone();
                    menu = menu.item(
                        PopupMenuItem::new(tr!("viewer-address-new-message"))
                            .disabled(!has_email)
                            .on_click(move |_, window, cx| {
                                app.update(cx, |this, cx| {
                                    this.open_inline_compose(
                                        ComposeInit::with_to(address.clone()),
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    );
                }
                {
                    let app = app.clone();
                    let address = target.clone();
                    menu = menu.item(
                        PopupMenuItem::new(tr!("viewer-address-show-contact"))
                            .disabled(!has_email)
                            .on_click(move |_, _, cx| {
                                app.update(cx, |this, cx| {
                                    this.show_contact(address.clone(), cx);
                                });
                            }),
                    );
                }
                menu
            })
            .into_any_element()
    }

    pub(super) fn render_viewer_address_row(
        &self,
        label: gpui::SharedString,
        addresses: &[String],
        id_prefix: &str,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut pills = h_flex().flex_1().min_w_0().flex_wrap().gap_1().gap_y_1();
        for (index, address) in addresses.iter().enumerate() {
            pills = pills.child(self.render_viewer_address_pill(
                address,
                format!("{id_prefix}-{index}"),
                compact,
                cx,
            ));
        }
        h_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .items_center()
            .child(
                div()
                    .w(px(42.))
                    .flex_none()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(pills)
            .into_any_element()
    }

    pub(super) fn render_view_mode_picker(
        &self,
        mode: BodyViewMode,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        ButtonGroup::new("viewer-body-mode")
            .outline()
            .compact()
            .xsmall()
            .child(
                Button::new("vm-blitz")
                    .icon(crate::ui::icons::app_icon("eye"))
                    .tooltip(tr!("viewer-mode-faithful-short"))
                    .selected(mode == BodyViewMode::Blitz),
            )
            .child(
                Button::new("vm-md")
                    .icon(crate::ui::icons::app_icon("file-text"))
                    .tooltip(tr!("viewer-mode-markdown"))
                    .selected(mode == BodyViewMode::Markdown),
            )
            .child(
                Button::new("vm-src")
                    .icon(crate::ui::icons::app_icon("file-type"))
                    .tooltip(tr!("viewer-mode-source"))
                    .selected(mode == BodyViewMode::Source),
            )
            .on_click(cx.listener(|this, selected: &Vec<usize>, _, cx| {
                let mode = match selected.first() {
                    Some(0) => BodyViewMode::Blitz,
                    Some(1) => BodyViewMode::Markdown,
                    Some(2) => BodyViewMode::Source,
                    _ => return,
                };
                this.settings.global.body_view_mode = mode;
                this.settings.save();
                cx.notify();
            }))
    }

    pub(super) fn render_tag_bar(&self, m: &Message, cx: &mut Context<Self>) -> impl IntoElement {
        let aid = m.header.account_id.clone();
        let provider = self
            .account(&aid)
            .map(|a| a.provider)
            .unwrap_or(Provider::Microsoft);
        let available = self.tags_by_account.get(&aid).cloned().unwrap_or_default();
        let current = m.tags.clone();
        let mid = m.header.id.clone();
        let entity = cx.entity();
        let offline = self.offline_accounts.contains(&aid);

        let mut bar = h_flex().gap_1().items_center().flex_wrap();
        for key in &current {
            let (label, color) = available
                .iter()
                .find(|t| &util::tag_storage_key(provider, t) == key)
                .map(|t| {
                    (
                        t.display_name.clone(),
                        crate::ui::tag_menu::tag_color(&t.display_name, t.color),
                    )
                })
                .unwrap_or_else(|| (key.clone(), util::name_color(key)));
            bar = bar.child(
                div()
                    .px_2()
                    .rounded_full()
                    .text_xs()
                    .bg(color.opacity(0.25))
                    .child(label),
            );
        }
        bar = bar.child(
            Button::new("tag-picker")
                .ghost()
                .xsmall()
                .icon(crate::ui::icons::app_icon("tag"))
                .label(tr!("tags-edit"))
                .dropdown_menu(move |menu, _window, _cx| {
                    crate::ui::tag_menu::append_tag_menu_items(
                        menu, &entity, provider, &aid, &mid, &available, &current, offline,
                    )
                }),
        );
        let message_id = m.header.id.clone();
        let translation_active = self.viewer_translation.open
            || self
                .viewer_translation
                .result
                .as_ref()
                .is_some_and(|translation| {
                    translation.message_id == message_id && translation.visible
                });
        bar = bar.child(
            Button::new("translate-message")
                .ghost()
                .xsmall()
                .selected(translation_active)
                .icon(crate::ui::icons::app_icon("globe"))
                .label(tr!("viewer-translate"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.viewer_translation.open = !this.viewer_translation.open;
                    if let Some(translation) = &mut this.viewer_translation.result {
                        translation.visible =
                            this.viewer_translation.open && translation.message_id == message_id;
                    }
                    cx.notify();
                })),
        );
        bar
    }

    /// Layout and settings facts the chrome derives once per frame, so the
    /// sections below agree on them.
    pub(super) fn viewer_chrome(&self, m: &Message, cx: &App) -> ViewerChrome {
        let width = self
            .viewer_layout_width
            .or_else(|| self.viewer_panel_width(cx));
        ViewerChrome {
            mode: self.settings.global.body_view_mode,
            options: self.settings.global.mail_body_options(),
            offline: self.offline_accounts.contains(&m.header.account_id),
            reply_all_primary: self.settings.global.reply_all_primary,
            collapse_quoted_messages: self.settings.global.collapse_quoted_messages,
            in_tab: self
                .mailbox
                .open_tabs
                .iter()
                .any(|t| t.message().is_some_and(|tm| tm.header.id == m.header.id)),
            compact: width.is_some_and(|width| width < 620.0),
            very_compact: width.is_some_and(|width| width < 420.0),
        }
    }

    /// The pane's chrome: subject and actions, addresses, date and body
    /// controls, tags, and attachments.
    pub(super) fn render_viewer_header(
        &mut self,
        m: &Message,
        chrome: &ViewerChrome,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let compact_header = chrome.compact;
        let very_compact_header = chrome.very_compact;
        let header_top = self.render_viewer_subject_row(m, chrome, cx);
        let address_rows = self.render_viewer_address_block(m, chrome, cx);
        let body_controls = self.render_viewer_body_controls(m, chrome, cx);
        let tag_bar = self.render_tag_bar(m, cx);
        let attachment_panel =
            (!m.attachments.is_empty()).then(|| self.render_attachment_panel(m, cx));
        v_flex()
            .gap_1()
            .px_4()
            .when(very_compact_header, |header| header.px_3())
            .pt_3()
            .pb_2()
            .border_b_1()
            .border_color(theme.border)
            .child(header_top)
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .items_start()
                    .when(compact_header, |details| details.flex_col())
                    .child(address_rows)
                    .child(
                        v_flex()
                            .flex_none()
                            .gap_1()
                            .items_end()
                            .when(compact_header, |side| side.w_full())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(util::full_date(&m.header.received)),
                            )
                            .child(body_controls),
                    ),
            )
            .child(tag_bar)
            .children(attachment_panel)
    }

    fn render_viewer_subject_row(
        &mut self,
        m: &Message,
        chrome: &ViewerChrome,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let compact_header = chrome.compact;
        // Actions have a substantial intrinsic width. If they stay on the same
        // row in a narrow pane, the `.flex_1()` subject is compressed until it
        // displays only a few characters per line.
        // Below this threshold, give the subject a full row and place
        // actions en dessous.
        let subject = div()
            .flex_1()
            .min_w_0()
            .text_lg()
            .font_semibold()
            .line_clamp(2)
            .child(if m.header.subject.is_empty() {
                tr!("no-subject").to_string()
            } else {
                m.header.subject.clone()
            });
        let actions = self.render_viewer_actions(m, chrome, cx);
        if compact_header {
            v_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .child(subject)
                .child(actions)
                .into_any_element()
        } else {
            h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .items_start()
                .child(subject)
                .child(actions)
                .into_any_element()
        }
    }

    /// Reply, forward, quick actions, and the destructive pair. Every button
    /// loses its label in a very narrow pane and keeps only its tooltip.
    fn render_viewer_actions(
        &mut self,
        m: &Message,
        chrome: &ViewerChrome,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let aid = m.header.account_id.clone();
        let mid = m.header.id.clone();
        let offline = chrome.offline;
        let compact_header = chrome.compact;
        let very_compact_header = chrome.very_compact;
        let reply_all_primary = chrome.reply_all_primary;
        let show_remote = chrome.options.show_remote_images;
        let in_tab = chrome.in_tab;
        let in_junk = self.viewing_junk_folder(&aid);
        let has_junk_folder = self.junk_folder_available(&aid);
        let sender = m.header.from.clone();
        let sender_blocked = self.settings.global.sender_is_blocked(&sender);
        let snoozed = self.settings.snoozed_until(&aid, &mid).is_some();
        let entity = cx.entity();
        let quick_action_controls =
            self.render_quick_action_controls(&aid, &mid, "viewer", true, true, cx);
        let reply_actions = ButtonGroup::new("reply-actions")
            .outline()
            .compact()
            .disabled(offline)
            .when(!reply_all_primary, |group| {
                group
                    .child(
                        Button::new("reply")
                            .small()
                            .icon(crate::ui::icons::app_icon("reply"))
                            .when(!very_compact_header, |button| {
                                button.label(tr!("viewer-reply"))
                            })
                            .when(very_compact_header, |button| {
                                button.tooltip(tr!("viewer-reply"))
                            }),
                    )
                    .child(
                        Button::new("reply-all")
                            .small()
                            .icon(crate::ui::icons::app_icon("reply-all"))
                            .tooltip(tr!("viewer-reply-all")),
                    )
            })
            .when(reply_all_primary, |group| {
                group
                    .child(
                        Button::new("reply-all")
                            .small()
                            .icon(crate::ui::icons::app_icon("reply-all"))
                            .when(!very_compact_header, |button| {
                                button.label(tr!("viewer-reply-all"))
                            })
                            .when(very_compact_header, |button| {
                                button.tooltip(tr!("viewer-reply-all"))
                            }),
                    )
                    .child(
                        Button::new("reply")
                            .small()
                            .icon(crate::ui::icons::app_icon("reply"))
                            .tooltip(tr!("viewer-reply")),
                    )
            })
            .on_click(cx.listener({
                let m = m.clone();
                move |this, selected: &Vec<usize>, window, cx| match (
                    reply_all_primary,
                    selected.first(),
                ) {
                    (false, Some(0)) | (true, Some(1)) => {
                        this.start_inline_reply(&m, window, cx);
                    }
                    (false, Some(1)) | (true, Some(0)) => {
                        this.start_inline_reply_all(&m, window, cx);
                    }
                    _ => {}
                }
            }));
        h_flex()
            .gap_2()
            .items_center()
            .flex_wrap()
            .when(compact_header, |el| el.w_full().justify_end())
            .child(reply_actions)
            .child(
                Button::new("forward")
                    .outline()
                    .compact()
                    .small()
                    .disabled(offline)
                    .icon(crate::ui::icons::app_icon("forward"))
                    .when(!very_compact_header, |button| {
                        button.label(tr!("viewer-forward"))
                    })
                    .when(very_compact_header, |button| {
                        button.tooltip(tr!("viewer-forward"))
                    })
                    .on_click(cx.listener({
                        let m = m.clone();
                        move |this, _, window, cx| {
                            this.open_inline_compose(
                                ComposeInit::forward(m.header.account_id.clone(), &m),
                                window,
                                cx,
                            );
                        }
                    })),
            )
            .child(quick_action_controls)
            .child(
                Button::new("message-actions")
                    .outline()
                    .compact()
                    .small()
                    .icon(crate::ui::icons::app_icon("ellipsis"))
                    .tooltip(tr!("viewer-more-actions"))
                    .dropdown_menu({
                        let message = m.clone();
                        let entity = entity.clone();
                        let aid = aid.clone();
                        let mid = mid.clone();
                        let sender = sender.clone();
                        move |menu, window, cx| {
                            let markdown = message.body.clone();
                            let print_message = message.clone();
                            let entity = entity.clone();
                            let block_entity = entity.clone();
                            let snooze_entity = entity.clone();
                            let aid = aid.clone();
                            let mid = mid.clone();
                            let sender = sender.clone();
                            let mut menu = menu;
                            {
                                let targets = vec![crate::model::MessageRef {
                                    account_id: aid.clone(),
                                    id: mid.clone(),
                                }];
                                let unsnooze_aid = aid.clone();
                                let unsnooze_mid = mid.clone();
                                menu = if snoozed {
                                    menu.item(
                                        PopupMenuItem::new(tr!("snooze-cancel"))
                                            .icon(crate::ui::icons::app_icon("clock"))
                                            .on_click(move |_, window, cx| {
                                                snooze_entity.update(cx, |this, cx| {
                                                    this.unsnooze_message(
                                                        &unsnooze_aid,
                                                        &unsnooze_mid,
                                                        window,
                                                        cx,
                                                    );
                                                });
                                            }),
                                    )
                                } else {
                                    menu.submenu_with_icon(
                                        Some(crate::ui::icons::app_icon("clock")),
                                        tr!("snooze-menu"),
                                        window,
                                        cx,
                                        move |submenu, _window, _cx| {
                                            crate::ui::snooze::append_snooze_menu(
                                                submenu,
                                                &snooze_entity,
                                                &targets,
                                                offline,
                                            )
                                        },
                                    )
                                }
                                .separator();
                            }
                            if has_junk_folder {
                                menu = menu
                                    .item(
                                        PopupMenuItem::new(if in_junk {
                                            tr!("ctx-not-junk")
                                        } else {
                                            tr!("ctx-junk")
                                        })
                                        .icon(crate::ui::icons::app_icon(if in_junk {
                                            "inbox"
                                        } else {
                                            "alert-circle"
                                        }))
                                        .disabled(offline)
                                        .on_click(
                                            move |_, window, cx| {
                                                entity.update(cx, |this, cx| {
                                                    if in_junk {
                                                        this.mark_not_junk_with_undo(
                                                            aid.clone(),
                                                            &mid,
                                                            window,
                                                            cx,
                                                        );
                                                    } else {
                                                        this.mark_junk_with_undo(
                                                            aid.clone(),
                                                            &mid,
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                    cx.notify();
                                                });
                                            },
                                        ),
                                    )
                                    .item(
                                        PopupMenuItem::new(if sender_blocked {
                                            tr!("ctx-unblock-sender")
                                        } else {
                                            tr!("ctx-block-sender")
                                        })
                                        .icon(crate::ui::icons::app_icon(if sender_blocked {
                                            "circle-check"
                                        } else {
                                            "circle-x"
                                        }))
                                        .disabled(offline)
                                        .on_click(
                                            move |_, window, cx| {
                                                block_entity.update(cx, |this, cx| {
                                                    this.toggle_sender_block(&sender, window, cx);
                                                });
                                            },
                                        ),
                                    )
                                    .separator();
                            }
                            menu.item(
                                PopupMenuItem::new(tr!("viewer-copy-markdown"))
                                    .icon(crate::ui::icons::app_icon("copy"))
                                    .on_click(move |_, window, cx| {
                                        copy_markdown_to_clipboard(&markdown, window, cx);
                                    }),
                            )
                            .item(
                                PopupMenuItem::new(tr!("viewer-print"))
                                    .icon(crate::ui::icons::app_icon("printer"))
                                    .on_click(move |_, _, _| {
                                        browser::print_message(print_message.clone(), show_remote);
                                    }),
                            )
                        }
                    }),
            )
            .when(!in_tab, |el| {
                el.child(
                    Button::new("pin-tab")
                        .ghost()
                        .small()
                        .icon(crate::ui::icons::app_icon("pin"))
                        .tooltip(tr!("viewer-keep-open"))
                        .on_click(cx.listener({
                            let m = m.clone();
                            move |this, _, _, cx| {
                                this.open_message_tab(m.clone(), cx);
                            }
                        })),
                )
            })
            .child(
                Button::new("archive")
                    .outline()
                    .compact()
                    .small()
                    .disabled(offline)
                    .icon(crate::ui::icons::app_icon("archive"))
                    .tooltip(tr!("viewer-archive"))
                    .on_click(cx.listener({
                        let aid = aid.clone();
                        let mid = mid.clone();
                        move |this, _, window, cx| {
                            this.archive_message_with_undo(aid.clone(), &mid, window, cx);
                            cx.notify();
                        }
                    })),
            )
            .child(
                Button::new("delete")
                    .danger()
                    .small()
                    .disabled(offline)
                    .icon(crate::ui::icons::app_icon("trash-2"))
                    .tooltip(tr!("viewer-delete"))
                    .on_click(cx.listener({
                        let aid = aid.clone();
                        let mid = mid.clone();
                        move |this, _, window, cx| {
                            this.delete_message_with_undo(aid.clone(), &mid, window, cx);
                            cx.notify();
                        }
                    })),
            )
    }

    /// From/To/Cc/Bcc, one row per non-empty field.
    fn render_viewer_address_block(
        &mut self,
        m: &Message,
        chrome: &ViewerChrome,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mid = m.header.id.clone();
        let very_compact_header = chrome.very_compact;
        let from = [m.header.from.clone()];
        let mut address_rows =
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(self.render_viewer_address_row(
                    tr!("compose-from-label"),
                    &from,
                    &format!("viewer-from-{mid}"),
                    very_compact_header,
                    cx,
                ));
        if !m.to.is_empty() {
            address_rows = address_rows.child(self.render_viewer_address_row(
                tr!("compose-to-label"),
                &m.to,
                &format!("viewer-to-{mid}"),
                very_compact_header,
                cx,
            ));
        }
        if !m.cc.is_empty() {
            address_rows = address_rows.child(self.render_viewer_address_row(
                tr!("compose-cc-label"),
                &m.cc,
                &format!("viewer-cc-{mid}"),
                very_compact_header,
                cx,
            ));
        }
        if !m.bcc.is_empty() {
            address_rows = address_rows.child(self.render_viewer_address_row(
                tr!("compose-bcc"),
                &m.bcc,
                &format!("viewer-bcc-{mid}"),
                very_compact_header,
                cx,
            ));
        }
        address_rows
    }

    /// How the body is displayed: view mode, system browser, and the display
    /// options that are also mail-wide settings.
    fn render_viewer_body_controls(
        &mut self,
        m: &Message,
        chrome: &ViewerChrome,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mode = chrome.mode;
        let show_remote = chrome.options.show_remote_images;
        let force_uniform_font_family = chrome.options.force_uniform_font_family;
        let force_uniform_font_size = chrome.options.force_uniform_font_size;
        let collapse_quoted_messages = chrome.collapse_quoted_messages;
        h_flex()
            .gap_1()
            .child(self.render_view_mode_picker(mode, cx))
            .child({
                let message = m.clone();
                Button::new("open-message-in-browser")
                    .ghost()
                    .xsmall()
                    .icon(crate::ui::icons::app_icon("globe"))
                    .tooltip(tr!("open-in-browser"))
                    .on_click(move |_, _, _| {
                        browser::open_message(message.clone(), show_remote);
                    })
            })
            .child({
                let entity = cx.entity();
                Button::new("display-options")
                    .ghost()
                    .xsmall()
                    .selected(
                        show_remote
                            || force_uniform_font_family
                            || force_uniform_font_size
                            || !collapse_quoted_messages,
                    )
                    .icon(crate::ui::icons::app_icon("settings-2"))
                    .tooltip(tr!("viewer-display-options"))
                    .dropdown_menu(move |mut menu, _window, _cx| {
                        {
                            let entity = entity.clone();
                            menu = menu.item(
                                PopupMenuItem::new(tr!("viewer-remote-images"))
                                    .checked(show_remote)
                                    .on_click(move |_, _, cx| {
                                        entity.update(cx, |this, cx| {
                                            this.settings.global.show_remote_images =
                                                !this.settings.global.show_remote_images;
                                            this.settings.save();
                                            cx.notify();
                                        });
                                    }),
                            );
                        }
                        {
                            let entity = entity.clone();
                            menu = menu.item(
                                PopupMenuItem::new(tr!("viewer-uniform-font-family"))
                                    .checked(force_uniform_font_family)
                                    .on_click(move |_, _, cx| {
                                        entity.update(cx, |this, cx| {
                                            this.settings.global.force_uniform_font_family =
                                                !this.settings.global.force_uniform_font_family;
                                            this.settings.save();
                                            cx.notify();
                                        });
                                    }),
                            );
                        }
                        {
                            let entity = entity.clone();
                            menu = menu.item(
                                PopupMenuItem::new(tr!("viewer-uniform-font-size"))
                                    .checked(force_uniform_font_size)
                                    .on_click(move |_, _, cx| {
                                        entity.update(cx, |this, cx| {
                                            this.settings.global.force_uniform_font_size =
                                                !this.settings.global.force_uniform_font_size;
                                            this.settings.save();
                                            cx.notify();
                                        });
                                    }),
                            );
                        }
                        {
                            let entity = entity.clone();
                            menu = menu.item(
                                PopupMenuItem::new(tr!("viewer-collapse-quoted-messages"))
                                    .checked(collapse_quoted_messages)
                                    .on_click(move |_, _, cx| {
                                        entity.update(cx, |this, cx| {
                                            this.settings.global.collapse_quoted_messages =
                                                !this.settings.global.collapse_quoted_messages;
                                            this.settings.save();
                                            cx.notify();
                                        });
                                    }),
                            );
                        }
                        menu
                    })
            })
    }

    /// The provider's "you replied" banner.
    pub(super) fn render_viewer_action_banner(
        &self,
        m: &Message,
        mode: BodyViewMode,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let theme = cx.theme().clone();
        // The "you replied" banner stays only while no card above the
        // message shows our reply (a follow-up from the other party alone
        // does not withdraw it).
        let has_sent_details = self
            .mailbox
            .sent_messages
            .get(&m.header.id)
            .is_some_and(|messages| !messages.is_empty())
            || self
                .thread_newer_messages(m)
                .iter()
                .any(|h| self.is_own_address(&h.account_id, &h.from));

        // Keep the historical provider banner as a fallback. Once Aviary has
        // the exact outgoing message, the richer expandable card replaces it.
        if mode == BodyViewMode::Source || has_sent_details {
            None
        } else {
            m.header.last_action.map(|action| {
                use crate::model::LastAction;
                let (icon, label) = match action {
                    LastAction::Replied => ("reply", tr!("viewer-action-replied")),
                    LastAction::RepliedAll => ("reply-all", tr!("viewer-action-replied-all")),
                    LastAction::Forwarded => ("forward", tr!("viewer-action-forwarded")),
                };
                let text = match &m.header.last_action_at {
                    Some(at) => {
                        tr!("viewer-action-at", { action: label, date: util::full_date(at) })
                    }
                    None => tr!("viewer-action", { action: label }),
                };
                h_flex()
                    .gap_2()
                    .items_center()
                    .px_4()
                    .py_1p5()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.muted)
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(crate::ui::icons::app_icon(icon).xsmall())
                    .child(text)
            })
        }
    }
}
