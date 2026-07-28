//! Reader pane: body (Faithful/Markdown/Source), attachments, conversation
//! thread, and tags. Open-message tabs, the reply panel (new Outlook style) and
//! the collapsible cards around the displayed message live in the [`tabs`],
//! [`reply`] and [`cards`] submodules.

mod attachments_panel;
mod browser;
mod cards;
mod header;
mod quoted_body;
mod reply;
mod tabs;
pub(crate) mod translation;

pub use attachments_panel::AttachmentFetches;
pub(crate) use browser::print_message;
pub use reply::InlineReply;
pub use translation::ViewerTranslationState;

use self::translation::translated_body_element;

use super::app::AviaryApp;
use super::attachments;
use super::compose::ComposeInit;
use super::settings::{BodyViewMode, MailBodyOptions};
use super::state::ThreadBodyState;
use super::util;
use crate::model::{
    AccountId, BodyFormat, CalendarInvitation, InvitationResponse, Message, MessageHeader,
    MessageRef, Provider,
};
use crate::runtime::Cmd;
use gpui::{div, prelude::*, px, rems, App, Context, Window};
use gpui_component::{
    button::{Button, ButtonGroup, ButtonVariants},
    h_flex,
    input::Input,
    menu::{ContextMenuExt, DropdownMenu, PopupMenuItem},
    notification::Notification,
    text::TextView,
    tooltip::Tooltip,
    v_flex, ActiveTheme, Disableable, IconName, Selectable, Sizable, StyledExt, WindowExt as _,
};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

/// Removes remote images from a Markdown body when the user does not
/// veut pas charger de contenu distant.
fn strip_remote_images(md: &str) -> String {
    let re = regex::Regex::new(r"!\[[^\]]*\]\((https?://[^)]*)\)").expect("valid regex");
    re.replace_all(md, tr!("remote-image-hidden").as_ref())
        .to_string()
}

/// `gpui-component` 0.5 loses `Break` nodes placed in a paragraph
/// Markdown. Therefore, for rendering only, replaces explicit breaks
/// CommonMark (`deux espaces + LF` ou `\ + LF`) par un `<br>` que son parseur
/// inline HTML can display. Stored and copied Markdown remains unchanged.
fn markdown_for_text_view(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut fence: Option<(char, usize)> = None;

    for segment in md.split_inclusive('\n') {
        let Some(line_with_cr) = segment.strip_suffix('\n') else {
            out.push_str(segment);
            continue;
        };
        let line = line_with_cr.strip_suffix('\r').unwrap_or(line_with_cr);
        let eol = if line_with_cr.ends_with('\r') {
            "\r\n"
        } else {
            "\n"
        };
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let marker = trimmed.chars().next();
        let marker_run = marker
            .filter(|c| matches!(c, '`' | '~'))
            .map(|c| {
                trimmed
                    .chars()
                    .take_while(|candidate| candidate == &c)
                    .count()
            })
            .unwrap_or(0);

        if let Some((open_marker, open_len)) = fence {
            out.push_str(line);
            out.push_str(eol);
            if indent <= 3
                && marker == Some(open_marker)
                && marker_run >= open_len
                && trimmed[marker_run..].trim().is_empty()
            {
                fence = None;
            }
            continue;
        }

        if indent <= 3 && marker_run >= 3 {
            fence = marker.map(|c| (c, marker_run));
            out.push_str(line);
            out.push_str(eol);
            continue;
        }

        // Spaces are significant in an indented code block.
        if line.starts_with("    ") || line.starts_with('\t') {
            out.push_str(line);
            out.push_str(eol);
            continue;
        }

        let trailing_spaces = line.len() - line.trim_end_matches(' ').len();
        if trailing_spaces >= 2 {
            out.push_str(line.trim_end_matches(' '));
            out.push_str("<br>");
        } else {
            let trailing_backslashes = line.chars().rev().take_while(|c| *c == '\\').count();
            if trailing_backslashes % 2 == 1 {
                out.push_str(&line[..line.len() - 1]);
                out.push_str("<br>");
            } else {
                out.push_str(line);
                out.push_str(eol);
            }
        }
    }

    out
}

fn markdown_to_preview_html(md: &str) -> String {
    let parser = pulldown_cmark::Parser::new_ext(md, pulldown_cmark::Options::all());
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    // Provider Markdown uses this internal URI form. Blitz resolves the
    // original MIME content identifiers through its local network provider.
    html.replace("bytes://cid-", "cid:")
}

fn markdown_to_source_preview(md: &str) -> String {
    use pulldown_cmark::Event;

    let mut text = String::new();
    for event in pulldown_cmark::Parser::new_ext(md, pulldown_cmark::Options::all()) {
        match event {
            Event::Text(value) | Event::Code(value) => text.push_str(&value),
            Event::SoftBreak | Event::HardBreak => text.push('\n'),
            Event::End(
                pulldown_cmark::TagEnd::Paragraph
                | pulldown_cmark::TagEnd::Heading(_)
                | pulldown_cmark::TagEnd::Item
                | pulldown_cmark::TagEnd::CodeBlock
                | pulldown_cmark::TagEnd::BlockQuote(_),
            ) => text.push('\n'),
            _ => {}
        }
    }
    text
}

fn copy_markdown_to_clipboard(markdown: &str, window: &mut Window, cx: &mut App) {
    cx.write_to_clipboard(gpui::ClipboardItem::new_string(markdown.to_string()));
    window.push_notification(tr!("viewer-markdown-copied"), cx);
}

/// Keeps address buttons within a very narrow reader header. The complete
/// address remains available in the tooltip and dropdown menu.
fn compact_address_label(raw: &str, email: Option<&str>) -> String {
    const MAX_CHARS: usize = 28;
    let candidate = match email {
        Some(email) => {
            let name = util::display_name(raw);
            if name.eq_ignore_ascii_case(email) {
                email.to_string()
            } else {
                name
            }
        }
        None => raw.to_string(),
    };
    let mut chars = candidate.chars();
    let compact: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{compact}…")
    } else {
        compact
    }
}

/// Message body rendered according to the selected mode and presentation options.
pub fn body_element(
    m: &Message,
    mode: BodyViewMode,
    options: MailBodyOptions,
    fallback_width: Option<f32>,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    body_element_with_height(m, mode, options, fallback_width, false, window, cx)
}

/// A current or quoted part extracted from a larger MIME body. It is embedded
/// between other reader elements and must therefore follow its content height
/// instead of reserving a standalone browser viewport.
fn body_fragment_element(
    m: &Message,
    mode: BodyViewMode,
    options: MailBodyOptions,
    fallback_width: Option<f32>,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    body_element_with_height(m, mode, options, fallback_width, true, window, cx)
}

#[allow(clippy::too_many_arguments)]
fn body_element_with_height(
    m: &Message,
    mode: BodyViewMode,
    options: MailBodyOptions,
    fallback_width: Option<f32>,
    fragment: bool,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    let font_size = px(options.font_size);
    let text_style = gpui_component::text::TextViewStyle {
        heading_base_font_size: font_size,
        ..Default::default()
    };
    match mode {
        BodyViewMode::Blitz => {
            if fragment {
                super::blitz_body::fragment_element(m, options, fallback_width, window, cx)
            } else {
                super::blitz_body::element(m, options, fallback_width, window, cx)
            }
        }
        BodyViewMode::Markdown => {
            let md = if options.show_remote_images {
                m.body.clone()
            } else {
                strip_remote_images(&m.body)
            };
            let md = markdown_for_text_view(&md);
            let html = markdown_to_preview_html(&md);
            let copy_md = m.body.clone();
            let preview = if fragment {
                super::blitz_body::preview_html_fragment_element(
                    &format!("body-markdown:{}", m.header.id),
                    &html,
                    &m.inline_images,
                    options,
                    fallback_width,
                    window,
                    cx,
                )
            } else {
                super::blitz_body::preview_html_element(
                    &format!("body-markdown:{}", m.header.id),
                    &html,
                    &m.inline_images,
                    options,
                    fallback_width,
                    window,
                    cx,
                )
            };
            v_flex()
                .gap_1()
                .text_size(font_size)
                .child(
                    h_flex().justify_end().child(
                        Button::new(gpui::ElementId::Name(
                            format!("copy-md-{}", m.header.id).into(),
                        ))
                        .ghost()
                        .xsmall()
                        .icon(super::icons::app_icon("copy"))
                        .label(tr!("viewer-copy-markdown"))
                        .on_click(move |_, window, cx| {
                            copy_markdown_to_clipboard(&copy_md, window, cx);
                        }),
                    ),
                )
                .child(preview)
                .into_any_element()
        }
        BodyViewMode::Source => {
            let src = m.raw_body.clone().unwrap_or_else(|| m.body.clone());
            // Render as a `TextView` code block to support mouse selection, as
            // in Markdown mode. The closing fence must be longer than any run
            // of backticks in the content.
            let max_run = src
                .split(|c: char| c != '`')
                .map(str::len)
                .max()
                .unwrap_or(0);
            let fence = "`".repeat(max_run.max(2) + 1);
            let md = format!("{fence}html\n{src}\n{fence}");
            let copy_src = src.clone();
            v_flex()
                .gap_1()
                .text_size(font_size)
                .child(
                    h_flex().justify_end().child(
                        Button::new(gpui::ElementId::Name(
                            format!("copy-src-{}", m.header.id).into(),
                        ))
                        .ghost()
                        .xsmall()
                        .icon(super::icons::app_icon("copy"))
                        .label(tr!("viewer-copy-source"))
                        .on_click(move |_, window, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                copy_src.clone(),
                            ));
                            window.push_notification(tr!("viewer-source-copied"), cx);
                        }),
                    ),
                )
                .child(
                    TextView::markdown(
                        gpui::ElementId::Name(format!("body-src-{}", m.header.id).into()),
                        md,
                        window,
                        cx,
                    )
                    .style(text_style)
                    .selectable(true),
                )
                .into_any_element()
        }
    }
}

impl AviaryApp {
    fn render_invitation_panel(
        &self,
        message: &Message,
        invitation: &CalendarInvitation,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let reference = MessageRef::from(message);
        let responding = self.invitation_responses_in_flight.contains(&reference);
        let offline = self.offline_accounts.contains(&message.header.account_id);
        let disabled = responding || offline;
        let date = if invitation.all_day {
            if invitation.start.date_naive() == invitation.end.date_naive() {
                tr!("invitation-all-day", {
                    date: util::short_date(&invitation.start)
                })
            } else {
                tr!("invitation-all-day-range", {
                    start: util::short_date(&invitation.start),
                    end: util::short_date(&invitation.end)
                })
            }
        } else {
            tr!("invitation-time-range", {
                start: util::full_date(&invitation.start),
                end: util::full_date(&invitation.end)
            })
        };
        let status = match invitation.response {
            InvitationResponse::NeedsAction => tr!("invitation-status-needs-action"),
            InvitationResponse::Accepted => tr!("invitation-status-accepted"),
            InvitationResponse::Tentative => tr!("invitation-status-tentative"),
            InvitationResponse::Declined => tr!("invitation-status-declined"),
        };
        let response_button = |suffix: &'static str,
                               label: String,
                               icon: &'static str,
                               response: InvitationResponse| {
            let account_id = message.header.account_id.clone();
            let message_id = message.header.id.clone();
            let event_id = invitation.event_id.clone();
            Button::new(gpui::ElementId::Name(
                format!("invitation-{suffix}-{message_id}").into(),
            ))
            .outline()
            .small()
            .icon(super::icons::app_icon(icon))
            .label(label)
            .selected(invitation.response == response)
            .disabled(disabled || invitation.response == response)
            .on_click(cx.listener(move |this, _, _, cx| {
                let reference = MessageRef {
                    account_id: account_id.clone(),
                    id: message_id.clone(),
                };
                if !this.invitation_responses_in_flight.insert(reference) {
                    return;
                }
                this.send(Cmd::RespondToInvitation {
                    account_id: account_id.clone(),
                    message_id: message_id.clone(),
                    event_id: event_id.clone(),
                    response,
                });
                cx.notify();
            }))
        };

        v_flex()
            .gap_2()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.muted)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(super::icons::app_icon("calendar-days").small())
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(tr!("invitation-heading")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(status),
                            ),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child(invitation.subject.clone()),
            )
            .child(
                v_flex()
                    .gap_1()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(date)
                    .when(!invitation.location.is_empty(), |details| {
                        details.child(tr!("invitation-location", {
                            value: invitation.location.clone()
                        }))
                    })
                    .when(!invitation.organizer.is_empty(), |details| {
                        details.child(tr!("invitation-organizer", {
                            value: invitation.organizer.clone()
                        }))
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(response_button(
                        "accept",
                        tr!("invitation-accept").to_string(),
                        "circle-check",
                        InvitationResponse::Accepted,
                    ))
                    .child(response_button(
                        "tentative",
                        tr!("invitation-tentative").to_string(),
                        "clock",
                        InvitationResponse::Tentative,
                    ))
                    .child(response_button(
                        "decline",
                        tr!("invitation-decline").to_string(),
                        "circle-x",
                        InvitationResponse::Declined,
                    )),
            )
            .when(responding, |panel| {
                panel.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(tr!("invitation-status-responding")),
                )
            })
            .into_any_element()
    }

    /// Width published by the reader panel to the right of the list.
    pub(crate) fn viewer_panel_width(&self, cx: &App) -> Option<f32> {
        self.viewer_layout_width
            .or_else(|| {
                self.inbox_resize_h
                    .read(cx)
                    .sizes()
                    .get(1)
                    .copied()
                    .map(f32::from)
            })
            .filter(|width| *width >= 40.0)
    }

    /// Message displayed in the reader: the active tab when it is a message,
    /// otherwise the list selection. Also `None` when the active tab is a
    /// composer, which is handled earlier during rendering.
    /// Message the reader is showing, as a shared handle.
    ///
    /// Rendering the pane hands it to a dozen sub-elements and runs again for
    /// every frame of a hover or scroll transition; returning a copy meant
    /// duplicating the body, the inline images and the attachment bytes each
    /// time.
    pub(super) fn displayed_message(&self) -> Option<Rc<Message>> {
        match self.mailbox.active_tab {
            Some(ix) => self
                .mailbox
                .open_tabs
                .get(ix)
                .and_then(|t| t.shared_message().cloned()),
            None => self.mailbox.selected.clone(),
        }
    }

    pub fn render_viewer_pane(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let tabs_bar = if self.view == super::state::MainView::Mail {
            self.render_message_tabs(cx)
        } else {
            div().into_any_element()
        };
        // Active composer tab: display the composition in the pane.
        if let Some(id) = self.active_compose_tab() {
            self.scrolls.viewer.motion.cancel();
            if let Some(ic) = self.inline_composes.iter().find(|c| c.id == id) {
                return v_flex()
                    .size_full()
                    .child(tabs_bar)
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .flex_1()
                            .min_h_0()
                            .child(ic.view.clone()),
                    )
                    .into_any_element();
            }
        }
        let Some(m) = self.displayed_message() else {
            self.scrolls.viewer.motion.cancel();
            let active_tab_loading = self
                .mailbox
                .active_tab
                .and_then(|index| self.mailbox.open_tabs.get(index))
                .is_some_and(|tab| tab.is_loading());
            let label = if active_tab_loading
                || (self.mailbox.active_tab.is_none() && self.mailbox.selected_id.is_some())
            {
                tr!("viewer-message-loading")
            } else {
                tr!("viewer-select-message")
            };
            return v_flex()
                .size_full()
                .child(tabs_bar)
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .into_any_element();
        };
        let viewer_scroll_handle = self.scrolls.viewer.handle.clone();
        self.scrolls
            .viewer
            .motion
            .advance(&viewer_scroll_handle, window);

        let chrome = self.viewer_chrome(&m, cx);
        let mode = chrome.mode;
        let body_options = chrome.options;
        let translation_active = self.viewer_translation.open
            || self
                .viewer_translation
                .result
                .as_ref()
                .is_some_and(|translation| {
                    translation.message_id == m.header.id && translation.visible
                });

        let header = self.render_viewer_header(&m, &chrome, cx);

        let translation_panel = self.render_viewer_translation_panel(&m, window, cx);
        let invitation_panel = m
            .invitation
            .as_ref()
            .map(|invitation| self.render_invitation_panel(&m, invitation, cx));
        let action_banner = self.render_viewer_action_banner(&m, mode, cx);

        let reply_panel = self.render_inline_reply(&m, cx);

        let max_w = self.settings.global.preview_max_width;
        // The painted measurement is the reader's final width. `ResizableState`
        // remains a good first-frame fallback but may retain an
        // old split (especially after a layout change) and artificially narrow
        // quoted cards to about 600 px.
        let viewer_panel_width = self.viewer_panel_width(cx).or(self.viewer_layout_width);
        let body_fallback_width = viewer_panel_width
            .map(|width| {
                let content_width = width - 32.0; // `.px_4()`
                if max_w > 1.0 {
                    content_width.min(max_w - 32.0)
                } else {
                    content_width
                }
            })
            .filter(|width| *width >= 40.0);
        let body = div()
            // Without an explicit width, this flex item is sized from
            // content. Blitz needs the width to produce that
            // content: on first display, the cycle resolves to 0 px and only
            // recovers after resizing the panel.
            .w_full()
            .min_w_0()
            .px_4()
            .py_3()
            .when(max_w > 1.0, |el| el.max_w(px(max_w)).mx_auto())
            .child(self.render_message_body(
                &m,
                mode,
                body_options,
                body_fallback_width,
                window,
                cx,
            ));

        let sent_messages = self.render_sent_messages(&m, mode, viewer_panel_width, window, cx);

        let thread = if translation_active {
            Vec::new()
        } else {
            self.render_thread(&m, mode, viewer_panel_width, window, cx)
        };

        let viewer = v_flex()
            .size_full()
            .child(tabs_bar)
            .child(header)
            .children(invitation_panel)
            .children(translation_panel)
            .children(action_banner)
            // The reply panel lives outside the scroll area, so it remains
            // visible even after scrolling far into the body.
            .children(reply_panel)
            .child(
                div()
                    .id("viewer-scroll")
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_x_hidden()
                    .overflow_y_scroll()
                    .track_scroll(&self.scrolls.viewer.handle)
                    // On the same div, GPUI's internal scroll handler runs
                    // before this listener; wheel motion then undoes and
                    // animates its line-sized jump.
                    .on_scroll_wheel(cx.listener({
                        let handle = viewer_scroll_handle;
                        move |this, event: &gpui::ScrollWheelEvent, window, cx| {
                            if this.scrolls.viewer.motion.on_wheel(&handle, event, window) {
                                cx.notify();
                            }
                        }
                    }))
                    .child(
                        // Without `w_full`, the scroll container sizes this
                        // column to min-content and every percent-width card
                        // inside collapses with it (the body only *looks*
                        // right because Blitz draws at a measured pixel
                        // width).
                        v_flex()
                            .w_full()
                            .min_w_0()
                            .children(sent_messages)
                            .child(body)
                            .children(thread),
                    ),
            );

        // Measuring an absolute canvas in the reader created a cycle during
        // initial layout: its provisional width became the minimum width
        // content and corrected itself only after dragging the divider. The
        // wrapper itself is allocated directly by the resizable panel, so read
        // the bounds of its full-screen child during prepaint and publish the
        // measurement after painting.
        let entity = cx.entity();
        div()
            .size_full()
            .on_children_prepainted(move |bounds, _window, cx| {
                let Some(child) = bounds.first() else {
                    return;
                };
                let width = f32::from(child.size.width).floor();
                if width < 40.0 {
                    return;
                }
                let entity = entity.clone();
                cx.defer(move |cx| {
                    entity.update(cx, |this, cx| {
                        if this
                            .viewer_layout_width
                            .is_none_or(|current| (current - width).abs() >= 1.0)
                        {
                            this.viewer_layout_width = Some(width);
                            cx.notify();
                        }
                    });
                });
            })
            .child(viewer)
            .into_any_element()
    }

    // ----------------------------------------------------------------
    // Reader sub-elements
    // ----------------------------------------------------------------

    /// Renders the message's own content followed by quoted history as
    /// collapsible cards. Splitting does not affect Source mode, which must
    /// always show the exact received payload.
    fn render_message_body(
        &mut self,
        message: &Message,
        mode: BodyViewMode,
        options: MailBodyOptions,
        fallback_width: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if let Some(translation) = self
            .viewer_translation
            .result
            .as_ref()
            .filter(|translation| {
                translation.message_id == message.header.id && translation.visible
            })
        {
            return translated_body_element(
                message,
                translation,
                options,
                fallback_width,
                window,
                cx,
            );
        }
        if !self.settings.global.collapse_quoted_messages || mode == BodyViewMode::Source {
            return body_element(message, mode, options, fallback_width, window, cx);
        }
        let Some(split) = quoted_body::split_message(message) else {
            return body_element(message, mode, options, fallback_width, window, cx);
        };

        let theme = cx.theme().clone();
        let offline = self.offline_accounts.contains(&message.header.account_id);
        // In a scrollable container, Taffy may resolve a nested `w_full` as an
        // intrinsic width. The column then becomes
        // as wide as the Blitz tile (sometimes one third of the reader), and
        // cards retain that incorrect width until the next resize. `flex_1` in
        // a full-width row instead allocates the
        // available without using a persisted measurement or percentage.
        let mut content = v_flex().flex_1().min_w_0().gap_2();
        if let Some(current) = split.current {
            let current = current.as_message(message, "current");
            content = content.child(body_fragment_element(
                &current,
                mode,
                options,
                fallback_width,
                window,
                cx,
            ));
        }

        for (index, quoted) in split.quoted.into_iter().enumerate() {
            let key = format!("{}:quoted:{index}", message.header.id);
            let expanded = self.mailbox.expanded_quoted_sections.contains(&key);
            let toggle_key = key.clone();
            let source_id = message.header.id.clone();
            let jump_target = self.quoted_message_target(message, &quoted);
            let label = tr!("viewer-quoted-message", { number: index + 1 });
            let preview = quoted.preview.clone();
            let quoted_date = quoted.received().map(|date| util::short_date(&date));
            let can_reply = quoted.can_reply();
            let header = h_flex()
                .id(gpui::ElementId::Name(format!("quoted-head-{key}").into()))
                .w_full()
                .min_w_0()
                .gap_2()
                .items_center()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.muted)
                .cursor_pointer()
                .hover(|style| style.bg(theme.list_hover))
                .child(
                    gpui_component::Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .xsmall(),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .w_full()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .font_semibold()
                                        .text_sm()
                                        .truncate()
                                        .child(label),
                                )
                                .when_some(quoted_date, |element, date| {
                                    element.child(
                                        div()
                                            .flex_none()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(date),
                                    )
                                })
                                .when_some(jump_target, |row, target| {
                                    row.child(
                                        Button::new(gpui::ElementId::Name(
                                            format!("quoted-jump-{key}").into(),
                                        ))
                                        .ghost()
                                        .xsmall()
                                        .icon(super::icons::app_icon("arrow-right"))
                                        .tooltip(tr!("viewer-go-to-message"))
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.open_message(
                                                    target.account_id.clone(),
                                                    target.id.clone(),
                                                    cx,
                                                );
                                            }),
                                        ),
                                    )
                                })
                                .child(
                                    Button::new(gpui::ElementId::Name(
                                        format!("quoted-reply-{key}").into(),
                                    ))
                                    .ghost()
                                    .xsmall()
                                    .disabled(offline || !can_reply)
                                    .icon(super::icons::app_icon("reply-all"))
                                    .tooltip(if can_reply {
                                        tr!("viewer-reply-all")
                                    } else {
                                        tr!("viewer-quoted-reply-unavailable")
                                    })
                                    .on_click(cx.listener({
                                        let source_id = source_id.clone();
                                        move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            let Some(displayed_id) = this
                                                .displayed_message()
                                                .map(|displayed| displayed.header.id.clone())
                                            else {
                                                return;
                                            };
                                            let Some(target) =
                                                this.quoted_message_for_action(&source_id, index)
                                            else {
                                                return;
                                            };
                                            this.start_inline_quoted_reply(
                                                displayed_id,
                                                &target,
                                                window,
                                                cx,
                                            );
                                        }
                                    })),
                                )
                                .child(
                                    Button::new(gpui::ElementId::Name(
                                        format!("quoted-forward-{key}").into(),
                                    ))
                                    .ghost()
                                    .xsmall()
                                    .disabled(offline)
                                    .icon(super::icons::app_icon("forward"))
                                    .tooltip(tr!("viewer-forward"))
                                    .on_click(cx.listener({
                                        let source_id = source_id.clone();
                                        move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            let Some(target) =
                                                this.quoted_message_for_action(&source_id, index)
                                            else {
                                                return;
                                            };
                                            let mut init = ComposeInit::forward(
                                                target.header.account_id.clone(),
                                                &target,
                                            );
                                            // The quoted fragment has no provider ID of its own.
                                            init.forward_of = None;
                                            this.open_inline_compose(init, window, cx);
                                        }
                                    })),
                                ),
                        )
                        .when(!preview.is_empty(), |element| {
                            element.child(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .max_h(rems(3.75))
                                    .overflow_hidden()
                                    .text_sm()
                                    .line_height(rems(1.25))
                                    .text_color(theme.muted_foreground)
                                    .child(preview),
                            )
                        }),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.mailbox.expanded_quoted_sections.remove(&toggle_key) {
                        this.mailbox
                            .expanded_quoted_sections
                            .insert(toggle_key.clone());
                    }
                    cx.notify();
                }));
            let mut block = v_flex().w_full().min_w_0().child(header);
            if expanded {
                let quoted_message = quoted.as_message(message, &format!("quoted-{index}"));
                block = block.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .px_3()
                        .py_2()
                        .border_l_2()
                        .border_color(theme.border)
                        .child(body_fragment_element(
                            &quoted_message,
                            mode,
                            options,
                            fallback_width
                                .map(|width| width - 24.0)
                                .filter(|width| *width >= 40.0),
                            window,
                            cx,
                        )),
                );
            }
            content = content.child(block);
        }
        h_flex()
            .w_full()
            .min_w_0()
            .items_start()
            .child(content)
            .into_any_element()
    }

    /// Reconstructs a quoted sub-message on demand for an action. Lookup occurs
    /// on click to avoid cloning inline images whenever collapsed headers render.
    fn quoted_message_for_action(&self, source_id: &str, index: usize) -> Option<Message> {
        let source = self
            .mailbox
            .selected
            .as_deref()
            .filter(|message| message.header.id == source_id)
            .or_else(|| {
                self.mailbox
                    .open_tabs
                    .iter()
                    .filter_map(|tab| tab.message())
                    .find(|message| message.header.id == source_id)
            })
            .or_else(|| {
                self.mailbox.thread_bodies.values().find_map(|state| {
                    let ThreadBodyState::Loaded(message) = state else {
                        return None;
                    };
                    (message.header.id == source_id).then_some(message.as_ref())
                })
            })?;
        let split = quoted_body::split_message(source)?;
        split
            .quoted
            .get(index)
            .map(|part| part.as_message(source, &format!("quoted-{index}")))
    }

    /// Resolve an embedded quote to exactly one native message in the loaded
    /// conversation. Ambiguous or metadata-poor quotes deliberately have no
    /// jump action.
    ///
    /// The conversation is deduplicated first: a mail you sent comes back from
    /// both Sent Items and the folder it was delivered to, and two copies of
    /// the one mail would read as an ambiguity and silently drop the button.
    fn quoted_message_target(
        &self,
        source: &Message,
        quoted: &quoted_body::BodyPart,
    ) -> Option<MessageRef> {
        let thread = self.thread_without_duplicates(&source.header);
        let mut matches = thread
            .iter()
            .filter(|header| quoted.matches_header(header))
            .map(|header| MessageRef {
                account_id: header.account_id.clone(),
                id: header.id.clone(),
            });
        let target = matches.next()?;
        matches.next().is_none().then_some(target)
    }

    /// Does `from` resolve to the address of the account it belongs to —
    /// i.e. is this a message we sent ourselves?
    fn is_own_address(&self, account_id: &AccountId, from: &str) -> bool {
        let Some(account) = self.account(account_id) else {
            return false;
        };
        util::extract_email(from)
            .unwrap_or_else(|| from.to_string())
            .eq_ignore_ascii_case(&account.email)
    }

    /// The part of the loaded conversation that happened *after* `source`,
    /// newest first: our own replies and the other party's follow-ups alike.
    /// These are displayed above the viewed message (the server is the
    /// source of truth, so this also covers replies sent before Aviary
    /// tracked snapshots or from another client). Messages already covered
    /// by a local snapshot are excluded; the snapshot card wins since it
    /// carries the exact body offline.
    /// The loaded conversation, with the message on screen and every *other
    /// copy of it* removed, and one entry kept per mail.
    ///
    /// A thread query spans folders — Graph's `$filter` runs over
    /// `/me/messages`, IMAP's SEARCH walks the inbox and Sent — so a mail you
    /// sent comes back twice: once from Sent Items, once from wherever it was
    /// delivered. Those are two provider ids for a single mail, and RFC 5322
    /// says which: the `Message-ID` survives the copy. Filtering on `id`
    /// alone let the reader list the message it was already displaying, as an
    /// apparently separate reply.
    ///
    /// Backends that expose no `Message-ID` (Gmail, whose thread API returns
    /// each mail once anyway) fall back to `id`: two unknowns are never
    /// assumed equal.
    fn thread_without_duplicates(&self, source: &MessageHeader) -> Vec<MessageHeader> {
        let Some((cid, thread)) = &self.mailbox.thread else {
            return Vec::new();
        };
        if source.conversation_id.as_deref() != Some(cid.as_str()) {
            return Vec::new();
        }
        thread_without_duplicates(source, thread)
    }

    fn thread_newer_messages(&self, source: &Message) -> Vec<MessageHeader> {
        let snapshots = self.mailbox.sent_messages.get(&source.header.id);
        let mut newer: Vec<MessageHeader> = self
            .thread_without_duplicates(&source.header)
            .into_iter()
            .filter(|h| {
                h.received > source.header.received
                    && !snapshots.is_some_and(|messages| {
                        messages.iter().any(|sent| snapshot_covers(sent, h))
                    })
            })
            .collect();
        newer.sort_by_key(|h| std::cmp::Reverse(h.received));
        newer
    }
}

/// Strips accumulated reply/forward prefixes ("RE:", "TR :", "Fwd:", …) so
/// subjects can be compared across a conversation.
fn strip_reply_prefixes(mut s: &str) -> &str {
    s = s.trim();
    loop {
        let lower = s.to_ascii_lowercase();
        // "fwd" before "fw": both match the same bytes and only the full
        // tag is followed by the required ':'.
        let Some(tag) = ["fwd", "re", "fw", "tr", "aw"]
            .into_iter()
            .find(|tag| lower.starts_with(tag))
        else {
            break;
        };
        let Some(rest) = s[tag.len()..].trim_start().strip_prefix(':') else {
            break;
        };
        s = rest.trim_start();
    }
    s
}

/// A conversation card sits right above its source message, so its subject
/// line is only worth a row when it says something new: not empty, not a
/// bare "RE: <same subject>", and not a degenerate placeholder like "...".
fn subject_adds_context(subject: &str, source_subject: &str) -> bool {
    let subject = strip_reply_prefixes(subject);
    if subject.chars().all(|c| matches!(c, '.' | '…' | ' ')) {
        return false;
    }
    !subject.eq_ignore_ascii_case(strip_reply_prefixes(source_subject))
}

/// Drops the message on screen — and every *other mailbox copy of it* — from
/// a loaded conversation, then keeps one entry per mail.
///
/// A thread query spans folders: Graph's `$filter` runs over `/me/messages`,
/// IMAP's SEARCH walks the inbox and Sent. A mail you sent therefore comes
/// back twice — once from Sent Items, once from wherever it was delivered.
/// Those are two provider ids for a single mail, and RFC 5322 says which one
/// identifies the mail: the `Message-ID` survives the copy. Filtering on `id`
/// alone let the reader list the message it was already displaying, as an
/// apparently separate reply.
///
/// Backends exposing no `Message-ID` (Gmail, whose thread API returns each
/// mail once anyway) fall back to the provider id: two unknowns are never
/// assumed to be the same mail.
fn thread_without_duplicates(
    source: &MessageHeader,
    thread: &[MessageHeader],
) -> Vec<MessageHeader> {
    let mut seen: HashSet<&str> = HashSet::new();
    thread
        .iter()
        .filter(|header| {
            if header.id == source.id {
                return false;
            }
            match (
                header.internet_message_id.as_deref(),
                source.internet_message_id.as_deref(),
            ) {
                (Some(candidate), Some(displayed)) if candidate == displayed => false,
                (Some(candidate), _) => seen.insert(candidate),
                (None, _) => seen.insert(header.id.as_str()),
            }
        })
        .cloned()
        .collect()
}

/// Does this local snapshot represent thread message `h`? Exact match once
/// the sent copy is resolved (`sent_id`); subject/sender/±5 min heuristic
/// as fallback for id-less snapshots.
fn snapshot_covers(sent: &crate::model::SentMessage, h: &MessageHeader) -> bool {
    if sent.sent_id.as_deref() == Some(h.id.as_str()) {
        return true;
    }
    let snapshot = &sent.message.header;
    let same_sender = util::extract_email(&snapshot.from)
        .unwrap_or_else(|| snapshot.from.clone())
        .eq_ignore_ascii_case(&util::extract_email(&h.from).unwrap_or_else(|| h.from.clone()));
    snapshot.account_id == h.account_id
        && snapshot.subject.eq_ignore_ascii_case(&h.subject)
        && same_sender
        && (snapshot.received - h.received).num_seconds().abs() <= 300
}

/// Deferred opening of a message in a tab after a kanban double-click:
/// requests the body from the runtime; `Evt::MessageOpened` handles the rest.
impl AviaryApp {
    pub fn request_message_tab(
        &mut self,
        account_id: AccountId,
        id: String,
        cx: &mut Context<Self>,
    ) {
        self.pending_kanban_open = Some((account_id.clone(), id.clone()));
        self.send(Cmd::OpenMessage { account_id, id });
        cx.notify();
    }
}

#[cfg(test)]
mod markdown_display_tests {
    use super::{compact_address_label, markdown_for_text_view};
    use crate::providers::html::convert_email_html;

    #[test]
    fn preserves_outlook_header_line_breaks() {
        let html = "<p><b>Van:</b> Contact A &lt;contact-a@example.test&gt;<br>\
                    <b>Verzonden:</b> woensdag 15 juli 2026 13:21<br>\
                    <b>Aan:</b> Contact B &lt;contact-b@example.test&gt;<br>\
                    <b>Onderwerp:</b> RE: Projet de test</p>";
        let md = convert_email_html(html);

        let rendered = markdown_for_text_view(&md);

        assert!(rendered.contains("example.test><br>**Verzonden:**"));
        assert!(rendered.contains("13:21<br>**Aan:**"));
        assert!(rendered.contains("example.test><br>**Onderwerp:**"));
    }

    #[test]
    fn leaves_soft_breaks_and_code_blocks_unchanged() {
        let md = "soft break\nstill the paragraph\n\n```text\ncode  \n```\n\n    indented  \n";

        assert_eq!(markdown_for_text_view(md), md);
    }

    #[test]
    fn supports_backslash_and_crlf_hard_breaks() {
        let md = "first\\\r\nsecond  \r\nthird";

        assert_eq!(markdown_for_text_view(md), "first<br>second<br>third");
    }

    #[test]
    fn compact_address_prefers_the_name_and_bounds_bare_addresses() {
        assert_eq!(
            compact_address_label(
                "Contact A <contact-a@example.test>",
                Some("contact-a@example.test")
            ),
            "Contact A"
        );
        assert_eq!(
            compact_address_label(
                "averylongmailboxname@example.test",
                Some("averylongmailboxname@example.test")
            ),
            "averylongmailboxname@example…"
        );
    }
}

#[cfg(test)]
mod thread_dedup_tests {
    use super::thread_without_duplicates;
    use crate::model::{AccountId, MessageHeader};
    use chrono::{TimeZone, Utc};

    fn header(id: &str, rfc_id: Option<&str>, minute: u32) -> MessageHeader {
        MessageHeader {
            id: id.into(),
            account_id: AccountId("account-a".into()),
            subject: "TR: Contrat".into(),
            from: "Contact A <contact-a@example.test>".into(),
            received: Utc
                .with_ymd_and_hms(2026, 3, 15, 11, minute, 0)
                .single()
                .expect("fixed timestamp"),
            preview: String::new(),
            is_read: true,
            is_flagged: false,
            has_attachments: false,
            tags: Vec::new(),
            last_action: None,
            last_action_at: None,
            conversation_id: Some("conversation-1".into()),
            internet_message_id: rfc_id.map(str::to_string),
        }
    }

    fn ids(headers: &[MessageHeader]) -> Vec<&str> {
        headers.iter().map(|header| header.id.as_str()).collect()
    }

    /// The Sent Items copy of the message being read is a different provider
    /// id but the same mail. Listing it made the reader repeat what was
    /// already on screen, as if it were another reply.
    #[test]
    fn the_sent_copy_of_the_displayed_message_is_not_listed_again() {
        let displayed = header("inbox-copy", Some("forward@example.test"), 43);
        let thread = [
            header("sent-copy", Some("forward@example.test"), 43),
            displayed.clone(),
            header("original", Some("original@example.test"), 19),
        ];

        assert_eq!(
            ids(&thread_without_duplicates(&displayed, &thread)),
            vec!["original"]
        );
    }

    /// Two copies of a mail the user is *not* reading collapse to one row.
    #[test]
    fn two_copies_of_another_mail_collapse_to_one_row() {
        let displayed = header("read", Some("read@example.test"), 50);
        let thread = [
            header("other-inbox", Some("other@example.test"), 43),
            header("other-sent", Some("other@example.test"), 43),
            displayed.clone(),
        ];

        assert_eq!(
            ids(&thread_without_duplicates(&displayed, &thread)),
            vec!["other-inbox"]
        );
    }

    /// Without an RFC id there is no evidence two messages are the same mail.
    /// Two unknowns must stay two rows — Gmail exposes none, and its thread
    /// API returns each mail once anyway.
    #[test]
    fn messages_without_an_rfc_id_are_never_assumed_identical() {
        let displayed = header("read", None, 50);
        let thread = [
            header("reply-a", None, 43),
            header("reply-b", None, 19),
            displayed.clone(),
        ];

        assert_eq!(
            ids(&thread_without_duplicates(&displayed, &thread)),
            vec!["reply-a", "reply-b"]
        );
    }

    /// Distinct forwards sent minutes apart share sender, subject and thread
    /// but not their `Message-ID`: both must stay listed.
    #[test]
    fn distinct_forwards_are_kept_apart() {
        let displayed = header("read", Some("read@example.test"), 50);
        let thread = [
            header("forward-1", Some("forward-1@example.test"), 43),
            header("forward-2", Some("forward-2@example.test"), 44),
            displayed.clone(),
        ];

        assert_eq!(
            ids(&thread_without_duplicates(&displayed, &thread)),
            vec!["forward-1", "forward-2"]
        );
    }
}
