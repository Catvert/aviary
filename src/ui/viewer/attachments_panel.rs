//! The reader's attachment panel and the fetches behind it.
//!
//! Listing a message gives Aviary the attachment metadata but not the bytes, so
//! opening or saving one goes through `Cmd::FetchAttachment` and comes back as
//! an event; "download all" fetches what is missing and only then builds the
//! ZIP. Nothing here writes to disk on the UI thread.
//!
//! Not to be confused with [`crate::ui::attachments`], which holds the helpers
//! shared with the composer (MIME inference, saving, the archive itself).

use super::*;

/// Attachment payloads being fetched, and what to do when they land.
///
/// Listing a message gives Aviary the metadata but not the bytes, so opening,
/// saving or zipping an attachment goes through the runtime first and finishes
/// in an event handler.
#[derive(Default)]
pub struct AttachmentFetches {
    /// Requests in flight, keyed by account, message and provider-native
    /// attachment id.
    pub in_flight: HashSet<(AccountId, String, String)>,
    /// A click on a lazy attachment opens it as soon as its bytes arrive.
    pub pending_opens: HashSet<(AccountId, String, String)>,
    /// "Download all" waits for every lazy payload before opening the ZIP
    /// destination picker.
    pub pending_archive: Option<(AccountId, String, Vec<crate::model::Attachment>)>,
    /// Prevents duplicate archive writes while one is already running on the
    /// background executor.
    pub saving_archive: bool,
}

impl AviaryApp {
    fn download_attachments_as_zip(
        &mut self,
        files: Vec<crate::model::Attachment>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.attachments.saving_archive {
            return;
        }
        self.attachments.saving_archive = true;
        cx.notify();
        let directory = attachments::download_directory();
        let suggested_name = tr!("viewer-attachments-archive-name");
        let destination = cx.prompt_for_new_path(&directory, Some(&suggested_name));

        cx.spawn_in(window, async move |this, cx| {
            let path = match destination.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) | Err(_) => {
                    let _ = this.update(cx, |this, cx| {
                        this.attachments.saving_archive = false;
                        cx.notify();
                    });
                    return;
                }
                Ok(Err(error)) => {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.attachments.saving_archive = false;
                        this.notify_error(
                            tr!("viewer-attachments-picker-error", { error: error }),
                            window,
                            cx,
                        );
                    });
                    return;
                }
            };

            let saved = cx
                .background_executor()
                .spawn(async move { attachments::save_all_as_zip(&path, &files) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.attachments.saving_archive = false;
                match saved {
                    Ok(()) => window.push_notification(
                        Notification::success(tr!("viewer-attachments-download-success")),
                        cx,
                    ),
                    Err(error) => this.notify_error(
                        tr!("viewer-attachments-download-error", { error: error }),
                        window,
                        cx,
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn request_attachment(
        &mut self,
        account_id: AccountId,
        message_id: String,
        attachment_id: String,
        open_when_ready: bool,
        cx: &mut Context<Self>,
    ) {
        if attachment_id.is_empty() {
            return;
        }
        let key = (
            account_id.clone(),
            message_id.clone(),
            attachment_id.clone(),
        );
        if open_when_ready {
            self.attachments.pending_opens.insert(key.clone());
        }
        if self.attachments.in_flight.insert(key) {
            self.send(Cmd::FetchAttachment {
                account_id,
                message_id,
                attachment_id,
            });
        }
        cx.notify();
    }

    fn request_attachments_archive(
        &mut self,
        account_id: AccountId,
        message_id: String,
        files: Vec<crate::model::Attachment>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(attachment) = files
            .iter()
            .find(|attachment| attachment.bytes.is_none() && attachment.id.is_empty())
        {
            self.notify_error(
                tr!("viewer-attachment-content-unavailable", {
                    filename: attachment.filename.clone()
                }),
                window,
                cx,
            );
            return;
        }
        let missing: Vec<String> = files
            .iter()
            .filter(|attachment| attachment.bytes.is_none() && !attachment.id.is_empty())
            .map(|attachment| attachment.id.clone())
            .collect();
        if missing.is_empty() {
            self.download_attachments_as_zip(files, window, cx);
            return;
        }
        if self.attachments.saving_archive {
            return;
        }
        self.attachments.saving_archive = true;
        self.attachments.pending_archive = Some((account_id.clone(), message_id.clone(), files));
        for attachment_id in missing {
            self.request_attachment(
                account_id.clone(),
                message_id.clone(),
                attachment_id,
                false,
                cx,
            );
        }
        cx.notify();
    }

    pub(crate) fn on_attachment_fetched(
        &mut self,
        account_id: AccountId,
        message_id: String,
        attachment_id: String,
        bytes: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = (
            account_id.clone(),
            message_id.clone(),
            attachment_id.clone(),
        );
        self.attachments.in_flight.remove(&key);

        let update = |message: &mut Message| {
            if message.header.account_id == account_id && message.header.id == message_id {
                if let Some(attachment) = message
                    .attachments
                    .iter_mut()
                    .find(|attachment| attachment.id == attachment_id)
                {
                    attachment.size = bytes.len() as u64;
                    attachment.bytes = Some(bytes.clone());
                }
            }
        };
        if let Some(message) = self.mailbox.selected_mut() {
            update(message);
        }
        for tab in &mut self.mailbox.open_tabs {
            if let Some(message) = tab.message_mut() {
                update(message);
            }
        }
        for state in self.mailbox.thread_bodies.values_mut() {
            if let ThreadBodyState::Loaded(message) = state {
                update(message);
            }
        }

        if self.attachments.pending_opens.remove(&key) {
            if let Some(attachment) =
                self.attachment_for_message(&account_id, &message_id, &attachment_id)
            {
                attachments::open(attachment);
            }
        }

        let ready_archive = self.attachments.pending_archive.as_mut().and_then(
            |(archive_account, archive_message, files)| {
                if archive_account != &account_id || archive_message != &message_id {
                    return None;
                }
                if let Some(attachment) = files
                    .iter_mut()
                    .find(|attachment| attachment.id == attachment_id)
                {
                    attachment.size = bytes.len() as u64;
                    attachment.bytes = Some(bytes.clone());
                }
                files
                    .iter()
                    .all(|attachment| attachment.bytes.is_some())
                    .then(|| files.clone())
            },
        );
        if let Some(files) = ready_archive {
            self.attachments.pending_archive = None;
            self.attachments.saving_archive = false;
            self.download_attachments_as_zip(files, window, cx);
        }
        cx.notify();
    }

    pub(crate) fn on_attachment_fetch_error(
        &mut self,
        account_id: AccountId,
        message_id: String,
        attachment_id: String,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = (account_id.clone(), message_id.clone(), attachment_id);
        self.attachments.in_flight.remove(&key);
        self.attachments.pending_opens.remove(&key);
        if self.attachments.pending_archive.as_ref().is_some_and(
            |(archive_account, archive_message, _)| {
                archive_account == &account_id && archive_message == &message_id
            },
        ) {
            self.attachments.pending_archive = None;
            self.attachments.saving_archive = false;
        }
        self.notify_error(
            tr!("viewer-attachment-fetch-error", { error: error }),
            window,
            cx,
        );
    }

    fn attachment_for_message(
        &self,
        account_id: &AccountId,
        message_id: &str,
        attachment_id: &str,
    ) -> Option<crate::model::Attachment> {
        self.attachments_for_message(account_id, message_id)?
            .into_iter()
            .find(|attachment| attachment.id == attachment_id)
    }

    fn attachments_for_message(
        &self,
        account_id: &AccountId,
        message_id: &str,
    ) -> Option<Vec<crate::model::Attachment>> {
        let matches = |message: &Message| {
            &message.header.account_id == account_id && message.header.id == message_id
        };
        self.mailbox
            .selected
            .as_deref()
            .filter(|message| matches(message))
            .or_else(|| {
                self.mailbox
                    .open_tabs
                    .iter()
                    .filter_map(|tab| tab.message())
                    .find(|message| matches(message))
            })
            .or_else(|| {
                self.mailbox.thread_bodies.values().find_map(|state| {
                    let ThreadBodyState::Loaded(message) = state else {
                        return None;
                    };
                    matches(message).then_some(message.as_ref())
                })
            })
            .map(|message| message.attachments.clone())
    }

    fn save_attachment_as(
        &mut self,
        attachment: crate::model::Attachment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let directory = attachments::download_directory();
        let suggested_name = attachments::suggested_filename(&attachment);
        let destination = cx.prompt_for_new_path(&directory, Some(&suggested_name));

        cx.spawn_in(window, async move |this, cx| {
            let path = match destination.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.notify_error(
                            tr!("viewer-attachments-picker-error", { error: error }),
                            window,
                            cx,
                        );
                    });
                    return;
                }
            };

            let filename = attachment.filename.clone();
            let saved = cx
                .background_executor()
                .spawn(async move { attachments::save_as(&path, &attachment) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match saved {
                Ok(()) => window.push_notification(
                    Notification::success(tr!("viewer-attachment-save-success", {
                        filename: filename
                    })),
                    cx,
                ),
                Err(error) => this.notify_error(
                    tr!("viewer-attachment-save-error", { error: error }),
                    window,
                    cx,
                ),
            });
        })
        .detach();
    }

    pub(super) fn render_attachment_panel(
        &self,
        m: &Message,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let account_id = m.header.account_id.clone();
        let message_id = m.header.id.clone();
        let panel_width = self
            .viewer_layout_width
            .or_else(|| self.viewer_panel_width(cx))
            .unwrap_or(640.0);
        let columns: usize = if panel_width >= 720.0 {
            3
        } else if panel_width >= 360.0 {
            2
        } else {
            1
        };
        // Percentage widths can collapse to their min-content size inside a
        // gpui scroll container. Give the rows a definite width derived from
        // the measured viewer, then cap it for narrow/transitional frames.
        let cards_width_value = (panel_width - 56.0).max(240.0);
        let cards_width = px(cards_width_value);
        let gaps_width = 8.0 * columns.saturating_sub(1) as f32;
        let card_width = (cards_width_value - gaps_width) / columns as f32;
        // Card padding + icon + gap. A definite text width avoids Taffy
        // resolving the nested flex child to its zero min-content width.
        let card_text_width = px((card_width - 40.0).max(48.0));
        let new_row = || {
            h_flex()
                .w(cards_width)
                .max_w_full()
                .min_w_0()
                .items_start()
                .gap_2()
        };
        let mut rows = v_flex().w(cards_width).max_w_full().min_w_0().gap_2();
        let mut row = new_row();
        let mut row_len = 0;
        for (ix, a) in m.attachments.iter().enumerate() {
            let attachment = Arc::new(a.clone());
            let available = attachment.bytes.is_some();
            let key = (
                account_id.clone(),
                message_id.clone(),
                attachment.id.clone(),
            );
            let loading = self.attachments.in_flight.contains(&key);
            let filename = a.filename.clone();
            let size = attachments::format_size(a.size);
            let tooltip = if loading {
                tr!("viewer-attachment-fetching")
            } else {
                tr!("viewer-attachment-tooltip", {
                    mime: a.mime.clone(),
                    size: attachments::format_size(a.size)
                })
            };
            let open_attachment = Arc::clone(&attachment);
            let context_attachment = Arc::clone(&attachment);
            let fetch_account = account_id.clone();
            let fetch_message = message_id.clone();
            let fetch_attachment = a.id.clone();
            let context_account = account_id.clone();
            let context_message = message_id.clone();
            let context_entity = cx.entity();
            let card = h_flex()
                .id(gpui::ElementId::Name(format!("att-{ix}").into()))
                .flex_1()
                .min_w_0()
                .h(px(52.))
                .overflow_hidden()
                .gap_2()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .when(available, |card| {
                    card.cursor_pointer()
                        .hover(|card| card.bg(theme.list_hover))
                        .on_click(cx.listener(move |_this, _, _window, _cx| {
                            attachments::open(open_attachment.as_ref().clone());
                        }))
                })
                .when(!available && !loading, |card| {
                    card.cursor_pointer()
                        .opacity(0.7)
                        .hover(|card| card.bg(theme.list_hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.request_attachment(
                                fetch_account.clone(),
                                fetch_message.clone(),
                                fetch_attachment.clone(),
                                true,
                                cx,
                            );
                        }))
                })
                .when(loading, |card| card.opacity(0.55))
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .child(
                    crate::ui::icons::app_icon(attachments::icon_name(a))
                        .small()
                        .flex_none(),
                )
                .child(
                    v_flex()
                        .w(card_text_width)
                        .flex_none()
                        .min_w_0()
                        .overflow_hidden()
                        .items_start()
                        .child(
                            div()
                                .w(card_text_width)
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .child(filename),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(size),
                        ),
                )
                .context_menu(move |menu, _window, _cx| {
                    let attachment_to_open = Arc::clone(&context_attachment);
                    let attachment_to_save = Arc::clone(&context_attachment);
                    let save_entity = context_entity.clone();
                    let open_entity = context_entity.clone();
                    let open_account = context_account.clone();
                    let open_message = context_message.clone();
                    let open_id = context_attachment.id.clone();
                    let mut menu = menu.item(
                        PopupMenuItem::new(tr!("viewer-attachment-open"))
                            .icon(crate::ui::icons::app_icon("external-link"))
                            .disabled(loading)
                            .on_click(move |_, _, cx| {
                                if available {
                                    attachments::open(attachment_to_open.as_ref().clone());
                                } else {
                                    open_entity.update(cx, |this, cx| {
                                        this.request_attachment(
                                            open_account.clone(),
                                            open_message.clone(),
                                            open_id.clone(),
                                            true,
                                            cx,
                                        );
                                    });
                                }
                            }),
                    );
                    if available
                        && context_attachment
                            .mime
                            .to_ascii_lowercase()
                            .starts_with("image/")
                    {
                        menu = menu.item(crate::ui::image_lightbox::attachment_copy_item(
                            Arc::clone(&context_attachment),
                        ));
                    }
                    menu.item(
                        PopupMenuItem::new(tr!("viewer-attachment-save-as"))
                            .icon(crate::ui::icons::app_icon("download"))
                            .disabled(!available)
                            .on_click(move |_, window, cx| {
                                save_entity.update(cx, |this, cx| {
                                    this.save_attachment_as(
                                        attachment_to_save.as_ref().clone(),
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    )
                });
            row = row.child(card);
            row_len += 1;
            if row_len == columns {
                rows = rows.child(row);
                row = new_row();
                row_len = 0;
            }
        }
        if row_len > 0 {
            for _ in row_len..columns {
                row = row.child(div().flex_1().min_w_0());
            }
            rows = rows.child(row);
        }

        let files = m.attachments.clone();
        let archive_account = m.header.account_id.clone();
        let archive_message = m.header.id.clone();
        v_flex()
            .mt_2()
            .gap_2()
            .p_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.muted.opacity(0.35))
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .text_sm()
                            .font_semibold()
                            .child(crate::ui::icons::app_icon("paperclip").small())
                            .child(tr!("viewer-attachments-count", {
                                count: m.attachments.len()
                            })),
                    )
                    .child(
                        Button::new("download-all-attachments")
                            .ghost()
                            .small()
                            .icon(crate::ui::icons::app_icon("download"))
                            .label(tr!("viewer-attachments-download-all"))
                            .loading(self.attachments.saving_archive)
                            .disabled(self.attachments.saving_archive)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.request_attachments_archive(
                                    archive_account.clone(),
                                    archive_message.clone(),
                                    files.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    ),
            )
            .child(
                div()
                    .id("viewer-attachments-scroll")
                    .w_full()
                    .max_h(px(150.))
                    .overflow_y_scroll()
                    .pr_1()
                    .child(rows),
            )
    }
}
