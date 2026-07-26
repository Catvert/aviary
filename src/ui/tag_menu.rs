//! Shared builder for the tag-assignment menu. The reader's tag bar
//! dropdown and the mailbox row context submenu both call
//! [`append_tag_menu_items`] so they stay identical: colored tag icon,
//! checkmark on the right, offline state, and the "create tag" entry.

use super::app::AviaryApp;
use super::{icons, util};
use crate::model::{AccountId, Provider, Tag};
use crate::runtime::Cmd;
use gpui::{Entity, Hsla, Styled};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use gpui_component::Side;

/// Provider color when available, otherwise Aviary's stable name-derived
/// color. Pills, menus, and filter controls all use this same resolution.
pub(crate) fn tag_color(label: &str, color: Option<u32>) -> Hsla {
    color
        .map(util::packed_color)
        .unwrap_or_else(|| util::name_color(label))
}

/// Standard visual used anywhere a menu represents a tag.
pub(crate) fn tag_menu_item(label: String, color: Option<u32>) -> PopupMenuItem {
    let effective_color = tag_color(&label, color);
    PopupMenuItem::new(label).icon(icons::app_icon("tag").text_color(effective_color))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn append_tag_menu_items(
    mut menu: PopupMenu,
    entity: &Entity<AviaryApp>,
    provider: Provider,
    account_id: &AccountId,
    message_id: &str,
    available: &[Tag],
    current: &[String],
    offline: bool,
) -> PopupMenu {
    for tag in available {
        let key = util::tag_storage_key(provider, tag);
        let has_tag = current.contains(&key);
        let entity = entity.clone();
        let aid = account_id.clone();
        let mid = message_id.to_string();
        let tag_id = tag.id.clone();
        menu = menu.item(
            tag_menu_item(tag.display_name.clone(), tag.color)
                .checked(has_tag)
                .disabled(offline)
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        let command = if has_tag {
                            Cmd::RemoveTag {
                                account_id: aid.clone(),
                                message_id: mid.clone(),
                                tag_id: tag_id.clone(),
                            }
                        } else {
                            Cmd::AddTag {
                                account_id: aid.clone(),
                                message_id: mid.clone(),
                                tag_id: tag_id.clone(),
                            }
                        };
                        this.set_tag_undoable(command, &aid, &mid, &tag_id, !has_tag, window, cx);
                        cx.notify();
                    });
                }),
        );
    }
    if !available.is_empty() {
        menu = menu.separator();
    }
    let entity = entity.clone();
    let aid = account_id.clone();
    menu.item(
        PopupMenuItem::new(tr!("tags-create"))
            .icon(icons::app_icon("plus"))
            .disabled(offline)
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| {
                    this.open_create_tag_dialog(aid.clone(), window, cx);
                });
            }),
    )
    .check_side(Side::Right)
}
