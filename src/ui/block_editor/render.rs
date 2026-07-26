//! gpui rendering for the block document.

use super::{
    BlockEditor, EbKind, ListBlock, ResizeDrag, TableBlock, TextBlock, TextStyle, CONTEXT,
    IMAGE_DEFAULT_MAX_HEIGHT, IMAGE_MAX_WIDTH,
};
use crate::{
    blocks::BlockKind,
    model::InlineImage,
    ui::{blitz_body, components::block_input::BlockInput as Input, icons},
};
use gpui::{
    canvas, div, img, prelude::*, px, AnyElement, App, Context, ElementId, FontWeight, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Render, ScrollHandle, SharedUri, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    menu::{DropdownMenu, PopupMenuItem},
    scroll::{ScrollableElement, Scrollbar, ScrollbarShow},
    v_flex, ActiveTheme, Sizable, StyledExt, Theme,
};

/// Type sizes for one frame: the window's rem size times the editor's zoom.
/// Every block of a frame is laid out against the same values.
#[derive(Clone, Copy)]
struct BlockMetrics {
    zoom_scale: f32,
    body_size: Pixels,
    body_line_height: Pixels,
}

impl BlockEditor {
    /// Faithful preview of the HTML that will actually be sent. Unlike `TextView`,
    /// Blitz preserves styles, tables, and images in HTML signatures.
    pub fn preview_element(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let html = self.build_html(cx);
        let instance = format!("{}-preview", self.scope);
        blitz_body::preview_html_element(
            &instance,
            &html,
            &self.images,
            self.mail_body_options,
            self.layout_width
                .map(|width| width - 24.0)
                .filter(|width| *width >= 40.0),
            window,
            cx,
        )
    }
    /// A paragraph, heading, quote or code block. Typography goes on the
    /// `Input` itself: its root div fixes the size and would override anything
    /// inherited from a wrapper.
    fn render_text_block(
        &self,
        text: &TextBlock,
        bid: u64,
        theme: &Theme,
        metrics: BlockMetrics,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let BlockMetrics {
            zoom_scale,
            body_size,
            body_line_height,
        } = metrics;
        // Typography must be set on `Input`: its root div fixes the
        // size and overrides inheritance from a wrapper.
        let body: AnyElement = match text.style {
            TextStyle::Paragraph => div()
                .w_full()
                .child(
                    Input::new(&text.input)
                        .appearance(false)
                        .tab_index(self.input_tab_index(&text.input))
                        .text_size(body_size)
                        .line_height(body_line_height)
                        .p_0(),
                )
                .into_any_element(),
            TextStyle::Heading(level) => {
                let (size, weight) = match level {
                    1 => (px(26. * zoom_scale), FontWeight::BOLD),
                    2 => (px(21. * zoom_scale), FontWeight::BOLD),
                    _ => (px(17. * zoom_scale), FontWeight::SEMIBOLD),
                };
                div()
                    .w_full()
                    .child(
                        Input::new(&text.input)
                            .appearance(false)
                            .tab_index(self.input_tab_index(&text.input))
                            .text_size(size)
                            .font_weight(weight)
                            .line_height(px(f32::from(size) * 1.4))
                            .p_0(),
                    )
                    .into_any_element()
            }
            TextStyle::Quote => div()
                .w_full()
                .pl_3()
                .border_l_2()
                .border_color(theme.border)
                .child(
                    Input::new(&text.input)
                        .appearance(false)
                        .tab_index(self.input_tab_index(&text.input))
                        .italic()
                        .text_size(body_size)
                        .line_height(body_line_height)
                        .text_color(theme.muted_foreground)
                        .p_0(),
                )
                .into_any_element(),
            TextStyle::Code => div()
                .w_full()
                .p_2()
                .rounded(theme.radius)
                .bg(theme.muted)
                .child(
                    Input::new(&text.input)
                        .appearance(false)
                        .tab_index(self.input_tab_index(&text.input))
                        .font_family("JetBrains Mono")
                        .text_size(body_size)
                        .line_height(body_line_height)
                        .p_0(),
                )
                .into_any_element(),
        };
        self.wrap_text_actions(body, &text.input, bid, None, cx)
    }

    /// One input per item, with the marker its nesting level calls for.
    fn render_list_block(
        &self,
        list: &ListBlock,
        bid: u64,
        theme: &Theme,
        metrics: BlockMetrics,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let BlockMetrics {
            zoom_scale,
            body_size,
            body_line_height,
        } = metrics;
        let mut column = v_flex().w_full();
        let mut counters: Vec<u32> = Vec::new();
        for (row_index, row) in list.rows.iter().enumerate() {
            let level = row.indent as usize;
            while counters.len() > level + 1 {
                counters.pop();
            }
            while counters.len() < level + 1 {
                counters.push(0);
            }
            counters[level] += 1;
            let marker = if list.ordered {
                format!("{}.", counters[level])
            } else {
                match level % 3 {
                    0 => "•".to_string(),
                    1 => "◦".to_string(),
                    _ => "▪".to_string(),
                }
            };
            let row_element = h_flex()
                .w_full()
                .items_start()
                .gap_1()
                .pl(px(crate::blocks::COMPOSE_LIST_INDENT
                    * zoom_scale
                    * level as f32))
                .child(
                    div()
                        .w(px(20. * zoom_scale))
                        .flex_shrink_0()
                        .text_size(body_size)
                        .line_height(body_line_height)
                        .text_color(theme.foreground)
                        .child(marker),
                )
                .child(
                    div().flex_1().child(
                        Input::new(&row.input)
                            .appearance(false)
                            .tab_index(self.input_tab_index(&row.input))
                            .text_size(body_size)
                            .line_height(body_line_height)
                            .p_0(),
                    ),
                );
            column = column.child(self.wrap_text_actions(
                row_element.into_any_element(),
                &row.input,
                bid,
                Some(row_index),
                cx,
            ));
        }
        column.into_any_element()
    }

    /// One input per cell, plus the row/column toolbar.
    fn render_table_block(
        &self,
        table: &TableBlock,
        bid: u64,
        theme: &Theme,
        metrics: BlockMetrics,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let BlockMetrics {
            zoom_scale,
            body_size,
            body_line_height,
        } = metrics;
        let columns = table.rows.first().map(Vec::len).unwrap_or(1).max(1);
        let mut grid = v_flex()
            .min_w(px(columns as f32 * 120. * zoom_scale))
            .w_full();
        for (row_index, row) in table.rows.iter().enumerate() {
            // No `h_flex()` here: its `align-items` constraint
            // would prevent cells from stretching across the row.
            let mut row_element = div().flex().flex_row().w_full();
            for (column_index, cell) in row.iter().enumerate() {
                let input = Input::new(&cell.input)
                    .appearance(false)
                    .tab_index(self.input_tab_index(&cell.input))
                    .text_size(body_size)
                    .line_height(body_line_height)
                    .w_full()
                    .p_0()
                    .when(row_index == 0, |input| input.font_weight(FontWeight::BOLD));
                let cell = div()
                    .w_full()
                    .h_full()
                    .px(px(8. * zoom_scale))
                    .py(px(6. * zoom_scale))
                    // Match `border-collapse: collapse` in the HTML
                    // preview: draw every shared edge only once.
                    .border_r_1()
                    .border_b_1()
                    .when(row_index == 0, |cell| cell.border_t_1())
                    .when(column_index == 0, |cell| cell.border_l_1())
                    .border_color(theme.border)
                    .when(row_index == 0, |cell| cell.bg(theme.muted))
                    .child(input)
                    .into_any_element();
                row_element = row_element.child(self.wrap_table_cell_actions(
                    cell,
                    &table.rows[row_index][column_index].input,
                    bid,
                    row_index,
                    column_index,
                    cx,
                ));
            }
            grid = grid.child(row_element);
        }
        v_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .child(self.table_toolbar(bid, cx))
            .child(div().w_full().min_w_0().overflow_x_scrollbar().child(grid))
            .into_any_element()
    }

    /// An inline image with its resize handle, or the degraded rendering used
    /// while its bytes are unknown.
    #[allow(clippy::too_many_arguments)]
    fn render_image_block(
        &self,
        cid: &str,
        width: &Option<u32>,
        path: &Option<String>,
        scroll: &ScrollHandle,
        bid: u64,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match path {
            Some(path) => {
                let image = match width {
                    // A user-selected width must remain part of layout so
                    // the ancestor can scroll all the way to the resize
                    // handle. `max_w_full` would clamp it to the viewport.
                    Some(width) => img(SharedUri::from(path.clone()))
                        .w(px(*width as f32))
                        .flex_shrink_0(),
                    None => img(SharedUri::from(path.clone()))
                        .max_w_full()
                        .max_h(px(IMAGE_DEFAULT_MAX_HEIGHT)),
                };
                let row_width = width.map(|width| width as f32 + 10.0);
                let image_overflows =
                    row_width
                        .zip(self.layout_width)
                        .is_some_and(|(image, available)| {
                            image + super::IMAGE_SCROLL_CHROME > available
                        });
                let image_group = format!("blk-img-{bid}");
                let entity = cx.entity();
                let image_row = h_flex()
                    .gap_1()
                    .items_center()
                    .group(image_group.clone())
                    .when_some(row_width, |element, width| {
                        let width = px(width);
                        element.w(width).min_w(width)
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .on_children_prepainted(move |bounds, _window, cx| {
                                if let Some(bounds) = bounds.first() {
                                    let width = f32::from(bounds.size.width);
                                    entity.update(cx, |this, _| {
                                        this.measured.insert(bid, width);
                                    });
                                }
                            })
                            .child(image),
                    )
                    .child(
                        div()
                            .id(ElementId::Name(format!("blk-img-rs-{bid}").into()))
                            .invisible()
                            .group_hover(image_group, |style| style.visible())
                            .w(px(6.))
                            .h(px(36.))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(theme.muted_foreground.opacity(0.6))
                            .cursor_col_resize()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                                    cx.stop_propagation();
                                    this.push_undo(cx);
                                    this.resize = Some(ResizeDrag {
                                        bid,
                                        start_x: event.position.x,
                                        start_w: this.resize_start_width(bid),
                                    });
                                }),
                            ),
                    )
                    .into_any_element();
                let mut image_area = div()
                    .id(ElementId::Name(format!("blk-img-area-{bid}").into()))
                    .w_full()
                    .min_w_0()
                    .overflow_x_scroll()
                    .track_scroll(scroll);
                // Do not let GPUI translate a plain vertical wheel into
                // horizontal movement just because this area only scrolls
                // on the x axis.
                image_area.style().restrict_scroll_to_axis = Some(true);
                let image_scroll = scroll.clone();
                v_flex()
                    .w_full()
                    .min_w_0()
                    .when(image_overflows, |element| {
                        element.on_scroll_wheel(move |event, window, cx| {
                            if event.modifiers.shift {
                                let delta = event.delta.pixel_delta(window.line_height());
                                // Some platforms already expose Shift+wheel
                                // as an x delta, which the restricted image
                                // area has applied. Convert y only when they
                                // leave it vertical.
                                if delta.x == px(0.) && delta.y != px(0.) {
                                    let offset = image_scroll.offset();
                                    let x = (offset.x + delta.y)
                                        .clamp(-image_scroll.max_offset().width, px(0.));
                                    image_scroll.set_offset(gpui::point(x, offset.y));
                                    cx.notify(window.current_view());
                                }
                                cx.stop_propagation();
                            }
                        })
                    })
                    .child(image_area.child(image_row))
                    .when(image_overflows, |element| {
                        element.child(
                            div().relative().w_full().h(px(16.)).child(
                                Scrollbar::horizontal(scroll)
                                    .id(ElementId::Name(format!("blk-img-scroll-{bid}").into()))
                                    .scrollbar_show(ScrollbarShow::Always),
                            ),
                        )
                    })
                    .into_any_element()
            }
            None => div()
                .py_1()
                .px_2()
                .rounded(theme.radius)
                .border_1()
                .border_dashed()
                .border_color(theme.border)
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(if self.pending_remote_images.contains_key(cid) {
                    tr!("compose-inline-image-downloading")
                } else {
                    tr!("compose-inline-image-fallback", { cid: cid })
                })
                .into_any_element(),
        }
    }

    /// The signature block: the fragment as the recipient will see it, under a
    /// header naming it and offering the account's other signatures.
    ///
    /// Read-only on purpose — a signature is edited in Preferences, where it
    /// belongs, and switching is then a replacement rather than a merge of two
    /// half-edited versions.
    fn render_signature_block(
        &self,
        name: &str,
        html: &str,
        bid: u64,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let instance = format!("{}-signature-{bid}", self.scope);
        let choices = self.available_signatures.clone();
        let label = if name.trim().is_empty() {
            tr!("compose-signature-block")
        } else {
            tr!("compose-signature-block-named", { name: name })
        };
        v_flex()
            .w_full()
            .min_w_0()
            .my_1()
            .p_2()
            .gap_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .text_color(theme.muted_foreground)
                            .child(label),
                    )
                    .when(!choices.is_empty(), |header| {
                        let entity = cx.entity();
                        header.child(
                            Button::new(ElementId::Name(format!("signature-picker-{bid}").into()))
                                .ghost()
                                .xsmall()
                                .icon(icons::app_icon("pen-line"))
                                .label(tr!("compose-signature-change"))
                                .dropdown_menu(move |mut menu, _window, _cx| {
                                    for choice in choices.clone() {
                                        let entity = entity.clone();
                                        menu = menu.item(
                                            PopupMenuItem::new(choice.name.clone()).on_click(
                                                move |_, window, cx| {
                                                    entity.update(cx, |this, cx| {
                                                        this.apply_signature(&choice, window, cx);
                                                    });
                                                },
                                            ),
                                        );
                                    }
                                    let entity = entity.clone();
                                    menu.separator().item(
                                        PopupMenuItem::new(tr!("compose-signature-none")).on_click(
                                            move |_, window, cx| {
                                                entity.update(cx, |this, cx| {
                                                    this.clear_signature(window, cx);
                                                });
                                            },
                                        ),
                                    )
                                }),
                        )
                    }),
            )
            .child(blitz_body::html_element(
                &instance,
                html,
                &self.images,
                self.mail_body_options,
                self.layout_width
                    .map(|width| width - 32.0)
                    .filter(|width| *width >= 40.0),
                cx,
            ))
            .into_any_element()
    }

    /// A raw-HTML block, rendered by the same engine as a received body.
    fn render_html_block(
        &self,
        html: &str,
        bid: u64,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let instance = format!("{}-html-{bid}", self.scope);
        v_flex()
            .w_full()
            .min_w_0()
            .my_1()
            .p_2()
            .gap_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(theme.muted_foreground)
                    .child(tr!("compose-html-block")),
            )
            .child(blitz_body::html_element(
                &instance,
                html,
                &self.images,
                self.mail_body_options,
                self.layout_width
                    .map(|width| width - 32.0)
                    .filter(|width| *width >= 40.0),
                cx,
            ))
            .into_any_element()
    }

    /// A quoted original message: collapsible, read-only, full HTML fidelity.
    fn render_original_message_block(
        &self,
        html: &str,
        inline_images: &[InlineImage],
        source_id: &str,
        index: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bid = self.blocks[index].id;
        let instance = format!("{}-original-{source_id}-{bid}", self.scope);
        let quoted_number = self.blocks[..=index]
            .iter()
            .filter(|block| {
                matches!(
                    block.kind,
                    EbKind::Original {
                        kind: BlockKind::OriginalMessage { .. }
                    }
                )
            })
            .count();
        let label = tr!("viewer-quoted-message", { number: quoted_number });
        let entity = cx.entity();
        let toggle = Button::new(ElementId::Name(
            format!("block-original-toggle-{bid}").into(),
        ))
        .ghost()
        .xsmall()
        .icon(icons::app_icon(if self.original_messages_collapsed {
            "chevron-right"
        } else {
            "chevron-down"
        }))
        .label(label)
        .on_click(move |_, _, cx| {
            entity.update(cx, |this, cx| {
                this.original_messages_collapsed = !this.original_messages_collapsed;
                cx.notify();
            });
        });
        v_flex()
            .w_full()
            .min_w_0()
            .my_1()
            .pl_3()
            .border_l_2()
            .border_color(theme.border)
            .child(toggle)
            .when(!self.original_messages_collapsed, |element| {
                element.child(blitz_body::html_element(
                    &instance,
                    html,
                    inline_images,
                    self.mail_body_options,
                    self.layout_width
                        .map(|width| width - 48.0)
                        .filter(|width| *width >= 40.0),
                    cx,
                ))
            })
            .into_any_element()
    }

    fn render_block(
        &self,
        index: usize,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let block = &self.blocks[index];
        let bid = block.id;
        let group = format!("blk-{bid}");
        let ui_scale = f32::from(window.rem_size()) / 16.0;
        let zoom_scale = ui_scale * self.zoom;
        let metrics = BlockMetrics {
            zoom_scale,
            body_size: px(crate::blocks::COMPOSE_BODY_FONT_SIZE * zoom_scale),
            body_line_height: px(crate::blocks::COMPOSE_BODY_LINE_HEIGHT * zoom_scale),
        };

        let content: AnyElement = match &block.kind {
            EbKind::Text(text) => self.render_text_block(text, bid, theme, metrics, cx),
            EbKind::List(list) => self.render_list_block(list, bid, theme, metrics, cx),
            EbKind::Table(table) => self.render_table_block(table, bid, theme, metrics, cx),
            EbKind::Image {
                cid,
                width,
                path,
                scroll,
            } => self.render_image_block(cid, width, path, scroll, bid, theme, cx),
            EbKind::Divider => div()
                .w_full()
                .py_2()
                .child(div().h(px(1.)).w_full().bg(theme.border))
                .into_any_element(),
            EbKind::Original {
                kind: BlockKind::RawHtml { html },
            } => self.render_html_block(html, bid, theme, cx),
            EbKind::Original {
                kind: BlockKind::Signature { name, html, .. },
            } => self.render_signature_block(name, html, bid, theme, cx),
            EbKind::Original {
                kind:
                    BlockKind::OriginalMessage {
                        html,
                        inline_images,
                        source_id,
                    },
            } => {
                self.render_original_message_block(html, inline_images, source_id, index, theme, cx)
            }
            EbKind::Original { .. } => unreachable!("invalid original-message block"),
        };

        let selected = self
            .sel_range()
            .is_some_and(|(lo, hi)| index >= lo && index <= hi);
        let select_on_click = matches!(&block.kind, EbKind::Image { .. } | EbKind::Divider);

        h_flex()
            // A concrete width avoids min-content reflow when the Blitz
            // placeholder is replaced by its raster.
            .when_some(
                self.layout_width.filter(|width| *width >= 40.0),
                |element, width| {
                    let width = px(width.floor());
                    element.w(width).min_w(width)
                },
            )
            .when(self.layout_width.is_none(), |element| element.w_full())
            .items_start()
            .gap_1()
            .group(group.clone())
            .when(selected, |element| {
                element
                    .rounded(theme.radius)
                    .bg(theme.primary.opacity(0.15))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.select_all_armed = None;
                    if event.modifiers.shift {
                        cx.stop_propagation();
                        this.shift_select(bid, window, cx);
                    } else if select_on_click {
                        cx.stop_propagation();
                        this.select_blocks(bid, bid, window, cx);
                    } else {
                        this.drag_anchor = Some(bid);
                        if this.sel.take().is_some() {
                            cx.notify();
                        }
                    }
                }),
            )
            .on_mouse_move(
                cx.listener(move |this, event: &MouseMoveEvent, window, cx| {
                    if event.pressed_button != Some(MouseButton::Left) {
                        return;
                    }
                    let Some(anchor) = this.drag_anchor else {
                        return;
                    };
                    if anchor != bid || this.sel.is_some() {
                        let selection = Some((anchor, bid));
                        if this.sel != selection {
                            this.sel = selection;
                            this.focus_handle.focus(window);
                            cx.notify();
                        }
                    }
                }),
            )
            .child(
                div()
                    .pt(px(2.))
                    .invisible()
                    .group_hover(group, |style| style.visible())
                    .child(self.block_menu(index, cx)),
            )
            .child(div().flex_1().min_w_0().child(content))
            .into_any_element()
    }
}

impl Render for BlockEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Rendering follows every mutation: this is the state that the next
        // keystroke will push onto history.
        self.mirror = self.exact_snapshot(cx);
        self.ensure_spellchecks(cx);
        let theme = cx.theme().clone();
        let width_probe = canvas(
            {
                let entity = cx.entity();
                move |bounds, _window, cx| {
                    let width = f32::from(bounds.size.width).floor();
                    if width >= 40.0 {
                        entity.update(cx, |this, cx| {
                            if this
                                .layout_width
                                .is_none_or(|current| (current - width).abs() >= 1.0)
                            {
                                this.layout_width = Some(width);
                                cx.notify();
                            }
                        });
                    }
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let mut root = v_flex()
            .w_full()
            .min_w_0()
            .relative()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_copy_selection))
            .on_action(cx.listener(Self::on_cut_selection))
            .on_action(cx.listener(Self::on_delete_selection))
            .on_action(cx.listener(Self::on_cancel_selection))
            .on_action(cx.listener(Self::on_select_all_blocks))
            .on_action(cx.listener(Self::on_select_prev))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_focus_selected))
            .on_action(cx.listener(Self::on_undo_blocks))
            .on_action(cx.listener(Self::on_redo_blocks))
            .on_action(cx.listener(Self::on_insert_link))
            .on_action(cx.listener(Self::on_apply_spelling_suggestion))
            .on_action(cx.listener(Self::on_ignore_spelling))
            .on_action(cx.listener(Self::on_add_spelling_to_dictionary))
            .on_action(cx.listener(Self::on_ignore_proofreading_rule))
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, window, cx| {
                    if !event.modifiers.control && !event.modifiers.platform {
                        return;
                    }
                    cx.stop_propagation();
                    if this.adjust_zoom(event, window) {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.drag_anchor = None;
                    this.resize = None;
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                let Some(resize) = this.resize else {
                    return;
                };
                if event.pressed_button != Some(MouseButton::Left) {
                    this.resize = None;
                    return;
                }
                let delta = f32::from(event.position.x) - f32::from(resize.start_x);
                let width = (resize.start_w + delta).clamp(40., IMAGE_MAX_WIDTH) as u32;
                if let Some(block) = this.blocks.iter_mut().find(|block| block.id == resize.bid) {
                    if let EbKind::Image {
                        width: image_width, ..
                    } = &mut block.kind
                    {
                        if *image_width != Some(width) {
                            *image_width = Some(width);
                            cx.notify();
                        }
                    }
                }
            }));
        for index in 0..self.blocks.len() {
            root = root.child(self.render_block(index, &theme, window, cx));
        }
        root.child(width_probe).child(
            div()
                .id("be-tail")
                .w_full()
                .h(px(64.))
                .on_click(cx.listener(|this, _, window, cx| this.focus_tail(window, cx))),
        )
    }
}
