//! Block menus and formatting/table toolbars.

use super::{BlockEditor, EbKind, InlineFormat, StyleTarget};
use crate::ui::icons;
use gpui::{prelude::*, AnyElement, Context, ElementId, Entity};
use gpui_component::{
    button::{Button, ButtonGroup, ButtonVariants},
    menu::{DropdownMenu as _, PopupMenuItem},
    Disableable, Sizable,
};

impl BlockEditor {
    pub(super) fn block_menu(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let block = &self.blocks[index];
        let bid = block.id;
        let is_textual = matches!(block.kind, EbKind::Text(_) | EbKind::List(_));
        let entity = cx.entity();
        Button::new(ElementId::Name(format!("blk-menu-{bid}").into()))
            .ghost()
            .xsmall()
            .icon(icons::app_icon("ellipsis-vertical"))
            .dropdown_menu(move |mut menu, _window, _cx| {
                if is_textual {
                    let styles: [(String, StyleTarget); 8] = [
                        (
                            tr!("compose-block-text").to_string(),
                            StyleTarget::Paragraph,
                        ),
                        (tr!("compose-block-h1").to_string(), StyleTarget::Heading(1)),
                        (tr!("compose-block-h2").to_string(), StyleTarget::Heading(2)),
                        (tr!("compose-block-h3").to_string(), StyleTarget::Heading(3)),
                        (tr!("compose-block-quote").to_string(), StyleTarget::Quote),
                        (tr!("compose-block-code").to_string(), StyleTarget::Code),
                        (
                            tr!("compose-block-list-bullets").to_string(),
                            StyleTarget::Bullets,
                        ),
                        (
                            tr!("compose-block-list-numbers").to_string(),
                            StyleTarget::Numbered,
                        ),
                    ];
                    for (label, target) in styles {
                        let entity = entity.clone();
                        menu =
                            menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                                entity.update(cx, |this, cx| {
                                    this.set_style(bid, target, window, cx);
                                });
                            }));
                    }
                    menu = menu.item(PopupMenuItem::separator());
                }
                let up = entity.clone();
                menu = menu.item(PopupMenuItem::new(tr!("compose-block-move-up")).on_click(
                    move |_, _, cx| {
                        up.update(cx, |this, cx| this.move_block(bid, -1, cx));
                    },
                ));
                let down = entity.clone();
                menu = menu.item(PopupMenuItem::new(tr!("compose-block-move-down")).on_click(
                    move |_, _, cx| {
                        down.update(cx, |this, cx| this.move_block(bid, 1, cx));
                    },
                ));
                let insert = entity.clone();
                menu = menu.item(
                    PopupMenuItem::new(tr!("compose-insert-paragraph-below")).on_click(
                        move |_, window, cx| {
                            insert.update(cx, |this, cx| {
                                this.insert_paragraph_after(bid, window, cx);
                            });
                        },
                    ),
                );
                let insert_image = entity.clone();
                menu = menu.item(
                    PopupMenuItem::new(tr!("compose-insert-image-below")).on_click(
                        move |_, window, cx| {
                            insert_image.update(cx, |this, cx| {
                                if let Some(index) = this.block_ix(bid) {
                                    this.prompt_insert_image_at(index + 1, window, cx);
                                }
                            });
                        },
                    ),
                );
                let delete = entity.clone();
                menu = menu.item(PopupMenuItem::new(tr!("compose-block-delete")).on_click(
                    move |_, window, cx| {
                        delete.update(cx, |this, cx| this.delete_block(bid, window, cx));
                    },
                ));
                menu
            })
            .into_any_element()
    }

    pub(crate) fn format_toolbar(
        id: &'static str,
        editor: Entity<Self>,
        disabled: bool,
    ) -> AnyElement {
        let group_id: ElementId = (id, editor.entity_id()).into();
        ButtonGroup::new(group_id.clone())
            .outline()
            .compact()
            .xsmall()
            .disabled(disabled)
            .child(
                Button::new((group_id.clone(), "bold"))
                    .icon(icons::app_icon("bold"))
                    .tooltip(tr!("compose-format-bold")),
            )
            .child(
                Button::new((group_id.clone(), "italic"))
                    .icon(icons::app_icon("italic"))
                    .tooltip(tr!("compose-format-italic")),
            )
            .child(
                Button::new((group_id.clone(), "underline"))
                    .icon(icons::app_icon("underline"))
                    .tooltip(tr!("compose-format-underline")),
            )
            .child(
                Button::new((group_id.clone(), "link"))
                    .icon(icons::app_icon("link"))
                    .tooltip(tr!("compose-link-insert")),
            )
            .child(
                Button::new((group_id, "table"))
                    .icon(icons::app_icon("table"))
                    .tooltip(tr!("compose-table-insert")),
            )
            .on_click(move |selected: &Vec<usize>, window, cx| {
                editor.update(cx, |this, cx| match selected.first().copied() {
                    Some(0) => this.apply_inline_format(InlineFormat::Bold, window, cx),
                    Some(1) => this.apply_inline_format(InlineFormat::Italic, window, cx),
                    Some(2) => this.apply_inline_format(InlineFormat::Underline, window, cx),
                    Some(3) => this.open_link_dialog(window, cx),
                    Some(4) => this.insert_table(2, 2, window, cx),
                    _ => {}
                });
            })
            .into_any_element()
    }

    pub(super) fn table_toolbar(&self, bid: u64, cx: &mut Context<Self>) -> AnyElement {
        ButtonGroup::new(ElementId::Name(
            format!("{}-table-actions-{bid}", self.scope).into(),
        ))
        .outline()
        .compact()
        .xsmall()
        .child(
            Button::new(ElementId::Name(
                format!("{}-table-row-add-{bid}", self.scope).into(),
            ))
            .icon(icons::app_icon("rows-plus"))
            .tooltip(tr!("compose-table-add-row")),
        )
        .child(
            Button::new(ElementId::Name(
                format!("{}-table-row-remove-{bid}", self.scope).into(),
            ))
            .icon(icons::app_icon("rows-minus"))
            .tooltip(tr!("compose-table-remove-row")),
        )
        .child(
            Button::new(ElementId::Name(
                format!("{}-table-column-add-{bid}", self.scope).into(),
            ))
            .icon(icons::app_icon("columns-plus"))
            .tooltip(tr!("compose-table-add-column")),
        )
        .child(
            Button::new(ElementId::Name(
                format!("{}-table-column-remove-{bid}", self.scope).into(),
            ))
            .icon(icons::app_icon("columns-minus"))
            .tooltip(tr!("compose-table-remove-column")),
        )
        .on_click(cx.listener(move |this, selected: &Vec<usize>, window, cx| {
            match selected.first().copied() {
                Some(0) => this.table_add_row(bid, window, cx),
                Some(1) => this.table_remove_row(bid, window, cx),
                Some(2) => this.table_add_column(bid, window, cx),
                Some(3) => this.table_remove_column(bid, window, cx),
                _ => {}
            }
        }))
        .into_any_element()
    }
}
