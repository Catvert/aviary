//! Menus attached to a message row.
//!
//! The right-click menu of a row and the move submenu it shares with the bulk
//! toolbar. `MoveScope` is what keeps the two identical: the folder hierarchy is
//! walked the same way for one message and for a selection, and only the leaf
//! action differs.

use super::folders::folder_display_label;
use crate::model::{AccountId, MailFolder, MessageHeader, MessageRef, Provider};
use crate::ui::app::AviaryApp;
use gpui::{Context, Entity, SharedString, Window};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub(super) struct MoveFolderTarget {
    id: String,
    label: SharedString,
    children: Vec<MoveFolderTarget>,
}

/// Builds the provider folder hierarchy for the move menu. Children of the
/// currently selected folder are promoted to the menu root because their
/// parent is not a valid destination. Malformed cycles are kept visible once.
pub(super) fn move_folder_targets(
    folders: &[MailFolder],
    selected_folder_id: Option<&str>,
) -> Vec<MoveFolderTarget> {
    let folders: Vec<_> = folders
        .iter()
        .filter(|folder| selected_folder_id != Some(folder.id.as_str()))
        .cloned()
        .collect();
    let ids: HashSet<_> = folders.iter().map(|folder| folder.id.clone()).collect();
    let mut roots = Vec::new();
    let mut children: HashMap<String, Vec<MailFolder>> = HashMap::new();

    for folder in folders.iter().cloned() {
        match folder.parent_id.as_deref() {
            Some(parent) if parent != folder.id && ids.contains(parent) => {
                children.entry(parent.to_string()).or_default().push(folder);
            }
            _ => roots.push(folder),
        }
    }

    fn append(
        targets: &mut Vec<MoveFolderTarget>,
        folders: &[MailFolder],
        children: &HashMap<String, Vec<MailFolder>>,
        visited: &mut HashSet<String>,
    ) {
        for folder in folders {
            if !visited.insert(folder.id.clone()) {
                continue;
            }
            let mut nested = Vec::new();
            if let Some(child_folders) = children.get(&folder.id) {
                append(&mut nested, child_folders, children, visited);
            }
            targets.push(MoveFolderTarget {
                id: folder.id.clone(),
                label: folder_display_label(folder),
                children: nested,
            });
        }
    }

    let mut targets = Vec::with_capacity(folders.len());
    let mut visited = HashSet::new();
    append(&mut targets, &roots, &children, &mut visited);
    for folder in &folders {
        if !visited.contains(&folder.id) {
            append(
                &mut targets,
                std::slice::from_ref(folder),
                &children,
                &mut visited,
            );
        }
    }
    targets
}

/// Whether [`move_folder_targets`] would yield at least one entry.
///
/// The bulk toolbar only needs that answer — to enable or disable its move
/// button — and asks for it on every frame. Building the whole hierarchy to
/// find out cloned the account's folder list twice for nothing.
pub(super) fn has_move_folder_targets(
    folders: &[MailFolder],
    selected_folder_id: Option<&str>,
) -> bool {
    folders
        .iter()
        .any(|folder| selected_folder_id != Some(folder.id.as_str()))
}

/// What a move entry applies to. The folder hierarchy is walked identically
/// for a row and for the bulk selection; only the leaf action differs.
#[derive(Clone)]
pub(super) enum MoveScope {
    Message {
        account_id: AccountId,
        message_id: String,
    },
    Selection(Vec<MessageRef>),
}

/// Everything the recursive move menu needs beyond the folder targets it is
/// currently listing.
#[derive(Clone)]
pub(super) struct MoveMenu {
    pub(super) entity: Entity<AviaryApp>,
    pub(super) scope: MoveScope,
    pub(super) source_folder_id: Option<String>,
    pub(super) offline: bool,
}

impl MoveMenu {
    /// A leaf entry: moving to `target_folder_id`, undoable like every other
    /// entry point.
    fn item(&self, label: SharedString, target_folder_id: String) -> PopupMenuItem {
        let entity = self.entity.clone();
        let scope = self.scope.clone();
        let source_folder_id = self.source_folder_id.clone();
        PopupMenuItem::new(label)
            .icon(crate::ui::icons::app_icon("folder"))
            .disabled(self.offline)
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| {
                    match &scope {
                        MoveScope::Message {
                            account_id,
                            message_id,
                        } => this.move_message_with_undo(
                            account_id.clone(),
                            message_id,
                            source_folder_id.clone(),
                            target_folder_id.clone(),
                            window,
                            cx,
                        ),
                        MoveScope::Selection(references) => this.bulk_move_messages_with_undo(
                            references.clone(),
                            source_folder_id.clone(),
                            target_folder_id.clone(),
                            window,
                            cx,
                        ),
                    }
                    cx.notify();
                });
            })
    }

    /// Lists `targets`, a folder holding children becoming a submenu whose
    /// first entry moves to the parent itself.
    pub(super) fn add_targets(
        &self,
        mut menu: PopupMenu,
        targets: Vec<MoveFolderTarget>,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        for target in targets {
            if target.children.is_empty() {
                menu = menu.item(self.item(target.label, target.id));
                continue;
            }

            let children = target.children;
            let target_folder_id = target.id;
            let builder = self.clone();
            menu = menu.submenu_with_icon(
                Some(crate::ui::icons::app_icon("folder-open")),
                target.label,
                window,
                cx,
                move |submenu, window, cx| {
                    let submenu = submenu
                        .item(builder.item(tr!("ctx-move-here"), target_folder_id.clone()))
                        .separator();
                    builder.add_targets(submenu, children.clone(), window, cx)
                },
            );
        }
        menu
    }
}

impl AviaryApp {
    /// The row's right-click menu, as the closure `context_menu` expects: the
    /// caller keeps its element, this builds what pops up on it.
    /// `thread` carries the loaded members of the conversation a **collapsed**
    /// row stands for. Everything that would otherwise hit the single message
    /// the row displays — delete, archive, junk, move, read state, pinning — then
    /// applies to the thread, because that is what the row *is*. Opening,
    /// replying, forwarding and quick actions stay on the message: they have
    /// no thread-wide meaning. Tags stay on the message too — a tag is what
    /// feeds the kanban, and tagging a thread would deal one card per message.
    pub(super) fn message_row_menu(
        &self,
        m: &MessageHeader,
        is_read: bool,
        is_pinned: bool,
        thread: Option<&[MessageRef]>,
        cx: &mut Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let entity = cx.entity();
        let aid = m.account_id.clone();
        let mid = m.id.clone();
        // A thread of one loaded message is an ordinary row: acting "on the
        // thread" would be the same action under a more alarming label.
        let thread: Option<Vec<MessageRef>> = thread
            .filter(|members| members.len() > 1)
            .map(<[MessageRef]>::to_vec);
        let thread_count = thread.as_ref().map_or(0, Vec::len);
        let offline = self.offline_accounts.contains(&aid);
        let in_junk = self.viewing_junk_folder(&aid);
        let has_junk_folder = self.junk_folder_available(&aid);
        let sender = m.from.clone();
        let sender_blocked = self.settings.global.sender_is_blocked(&sender);
        let snoozed = self.settings.snoozed_until(&aid, &mid).is_some();
        let source_folder = self.mailbox.selected_folder_id.clone();
        let provider = self
            .account(&aid)
            .map(|account| account.provider)
            .unwrap_or(Provider::Microsoft);
        let current_tags = m.tags.clone();

        // The folder hierarchy, the account's tags and its quick actions are
        // read from the entity when the menu opens rather than captured here:
        // this builder runs for every visible row of every frame, and walking
        // the whole folder tree per row made hovering the list rebuild it
        // dozens of times a second.
        move |mut menu, window, cx| {
            let (move_targets, available_tags, quick_menu_actions) = {
                let this = entity.read(cx);
                let move_targets = this
                    .mailbox
                    .folders_by_account
                    .get(&aid)
                    .map(|folders| move_folder_targets(folders, source_folder.as_deref()))
                    .unwrap_or_default();
                let available_tags = this.tags_by_account.get(&aid).cloned().unwrap_or_default();
                let quick_menu_actions: Vec<_> = this
                    .quick_actions_for(&aid)
                    .iter()
                    .map(|action| (action.clone(), this.quick_action_is_valid(&aid, action)))
                    .collect();
                (move_targets, available_tags, quick_menu_actions)
            };
            {
                let entity = entity.clone();
                let aid = aid.clone();
                let mid = mid.clone();
                menu = menu.item(
                    PopupMenuItem::new(tr!("ctx-open"))
                        .icon(crate::ui::icons::app_icon("mail-open"))
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.open_message(aid.clone(), mid.clone(), cx)
                            });
                        }),
                );
            }
            menu = menu.separator();
            if !quick_menu_actions.is_empty() {
                let entity = entity.clone();
                let aid = aid.clone();
                let mid = mid.clone();
                let quick_menu_actions = quick_menu_actions.clone();
                menu = menu
                    .submenu_with_icon(
                        Some(crate::ui::icons::app_icon("zap")),
                        tr!("quick-actions-menu"),
                        window,
                        cx,
                        move |submenu, _window, _cx| {
                            crate::ui::quick_actions::append_quick_action_menu(
                                submenu,
                                &quick_menu_actions,
                                &entity,
                                &aid,
                                &mid,
                                offline,
                            )
                        },
                    )
                    .separator();
            }
            {
                let entity = entity.clone();
                let aid = aid.clone();
                let mid = mid.clone();
                let available_tags = available_tags.clone();
                let current_tags = current_tags.clone();
                menu = menu.submenu_with_icon(
                    Some(crate::ui::icons::app_icon("tags")),
                    tr!("tags-edit"),
                    window,
                    cx,
                    move |submenu, _window, _cx| {
                        crate::ui::tag_menu::append_tag_menu_items(
                            submenu,
                            &entity,
                            provider,
                            &aid,
                            &mid,
                            &available_tags,
                            &current_tags,
                            offline,
                        )
                    },
                );
            }
            menu = menu.separator();
            {
                let entity = entity.clone();
                let aid = aid.clone();
                let mid = mid.clone();
                menu = menu.item(
                    PopupMenuItem::new(tr!("viewer-reply-all"))
                        .icon(crate::ui::icons::app_icon("reply-all"))
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.pending_forward_id = None;
                                this.pending_reply_id = Some(mid.clone());
                                this.open_message(aid.clone(), mid.clone(), cx);
                            });
                        }),
                );
            }
            {
                let entity = entity.clone();
                let aid = aid.clone();
                let mid = mid.clone();
                menu = menu.item(
                    PopupMenuItem::new(tr!("viewer-forward"))
                        .icon(crate::ui::icons::app_icon("forward"))
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.pending_reply_id = None;
                                this.pending_forward_id = Some(mid.clone());
                                this.open_message(aid.clone(), mid.clone(), cx);
                            });
                        }),
                );
            }
            {
                let entity = entity.clone();
                let aid = aid.clone();
                let mid = mid.clone();
                let thread = thread.clone();
                menu = menu.item(
                    PopupMenuItem::new(match (&thread, is_pinned) {
                        (Some(_), true) => tr!("ctx-unpin-thread"),
                        (Some(_), false) => tr!("ctx-pin-thread"),
                        (None, true) => tr!("messages-unpin"),
                        (None, false) => tr!("messages-pin"),
                    })
                    .icon(crate::ui::icons::app_icon(if is_pinned {
                        "pin-off"
                    } else {
                        "pin"
                    }))
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            match &thread {
                                Some(members) => {
                                    this.set_conversation_pinned(members, !is_pinned);
                                }
                                None => this.set_message_pinned(&aid, &mid, !is_pinned),
                            }
                            cx.notify();
                        });
                    }),
                );
            }
            {
                let entity = entity.clone();
                let aid = aid.clone();
                let mid = mid.clone();
                let thread = thread.clone();
                menu = menu.item(
                    PopupMenuItem::new(match (&thread, is_read) {
                        (Some(_), true) => tr!("ctx-mark-thread-unread", { count: thread_count }),
                        (Some(_), false) => tr!("ctx-mark-thread-read", { count: thread_count }),
                        (None, true) => tr!("ctx-mark-unread"),
                        (None, false) => tr!("ctx-mark-read"),
                    })
                    .icon(crate::ui::icons::app_icon(if is_read {
                        "mail"
                    } else {
                        "mail-open"
                    }))
                    .disabled(offline)
                    .on_click(move |_, window, cx| {
                        entity.update(cx, |this, cx| {
                            match &thread {
                                // Each member carries its own previous state:
                                // a half-read thread has to restore exactly
                                // what it was if the undo window is used.
                                Some(members) => {
                                    let items = this
                                        .message_states_where(|reference| {
                                            members.contains(reference)
                                        })
                                        .into_iter()
                                        .map(|(reference, read, _)| (reference, read))
                                        .collect();
                                    this.bulk_mark_read_undoable(items, !is_read, window, cx);
                                }
                                None => this.mark_read_undoable(
                                    MessageRef {
                                        account_id: aid.clone(),
                                        id: mid.clone(),
                                    },
                                    !is_read,
                                    is_read,
                                    window,
                                    cx,
                                ),
                            }
                            cx.notify();
                        });
                    }),
                );
            }
            // On the thread when the row stands for one: putting off the newest
            // message of a conversation and leaving the rest on screen would
            // not clear anything.
            {
                let entity = entity.clone();
                let targets: Vec<MessageRef> = thread.clone().unwrap_or_else(|| {
                    vec![MessageRef {
                        account_id: aid.clone(),
                        id: mid.clone(),
                    }]
                });
                menu = menu.separator();
                if snoozed {
                    let aid = aid.clone();
                    let mid = mid.clone();
                    menu = menu.item(
                        PopupMenuItem::new(tr!("snooze-cancel"))
                            .icon(crate::ui::icons::app_icon("clock"))
                            .on_click(move |_, window, cx| {
                                entity.update(cx, |this, cx| {
                                    this.unsnooze_message(&aid, &mid, window, cx);
                                });
                            }),
                    );
                } else {
                    menu = menu.submenu_with_icon(
                        Some(crate::ui::icons::app_icon("clock")),
                        match &thread {
                            Some(_) => tr!("snooze-thread-menu", { count: thread_count }),
                            None => tr!("snooze-menu"),
                        },
                        window,
                        cx,
                        move |submenu, _window, _cx| {
                            crate::ui::snooze::append_snooze_menu(
                                submenu, &entity, &targets, offline,
                            )
                        },
                    );
                }
            }
            if !move_targets.is_empty() {
                menu = menu.separator();
                let entity = entity.clone();
                let aid = aid.clone();
                let mid = mid.clone();
                let move_targets = move_targets.clone();
                let source_folder = source_folder.clone();
                let scope = match &thread {
                    Some(members) => MoveScope::Selection(members.clone()),
                    None => MoveScope::Message {
                        account_id: aid.clone(),
                        message_id: mid.clone(),
                    },
                };
                menu = menu.submenu_with_icon(
                    Some(crate::ui::icons::app_icon("folder-open")),
                    match thread {
                        Some(_) => tr!("ctx-move-thread-to", { count: thread_count }),
                        None => tr!("ctx-move-to"),
                    },
                    window,
                    cx,
                    move |submenu, window, cx| {
                        MoveMenu {
                            entity: entity.clone(),
                            scope: scope.clone(),
                            source_folder_id: source_folder.clone(),
                            offline,
                        }
                        .add_targets(
                            submenu,
                            move_targets.clone(),
                            window,
                            cx,
                        )
                    },
                );
            }
            menu = menu.separator();
            {
                let entity = entity.clone();
                let aid = aid.clone();
                let mid = mid.clone();
                let thread = thread.clone();
                menu = menu.item(
                    PopupMenuItem::new(match thread {
                        Some(_) => tr!("ctx-archive-thread", { count: thread_count }),
                        None => tr!("ctx-archive"),
                    })
                    .icon(crate::ui::icons::app_icon("archive"))
                    .disabled(offline)
                    .on_click(move |_, window, cx| {
                        entity.update(cx, |this, cx| {
                            match &thread {
                                Some(members) => this.bulk_archive_messages_with_undo(
                                    members.clone(),
                                    window,
                                    cx,
                                ),
                                None => {
                                    this.archive_message_with_undo(aid.clone(), &mid, window, cx)
                                }
                            }
                            cx.notify();
                        });
                    }),
                );
            }
            if has_junk_folder {
                let entity = entity.clone();
                let aid = aid.clone();
                let mid = mid.clone();
                let thread = thread.clone();
                menu = menu.item(
                    PopupMenuItem::new(match (&thread, in_junk) {
                        (Some(_), true) => tr!("ctx-not-junk-thread", { count: thread_count }),
                        (Some(_), false) => tr!("ctx-junk-thread", { count: thread_count }),
                        (None, true) => tr!("ctx-not-junk"),
                        (None, false) => tr!("ctx-junk"),
                    })
                    .icon(crate::ui::icons::app_icon(if in_junk {
                        "inbox"
                    } else {
                        "alert-circle"
                    }))
                    .disabled(offline)
                    .on_click(move |_, window, cx| {
                        entity.update(cx, |this, cx| {
                            match (&thread, in_junk) {
                                (Some(members), true) => {
                                    this.bulk_mark_not_junk_with_undo(members.clone(), window, cx)
                                }
                                (Some(members), false) => {
                                    this.bulk_mark_junk_with_undo(members.clone(), window, cx)
                                }
                                (None, true) => {
                                    this.mark_not_junk_with_undo(aid.clone(), &mid, window, cx)
                                }
                                (None, false) => {
                                    this.mark_junk_with_undo(aid.clone(), &mid, window, cx)
                                }
                            }
                            cx.notify();
                        });
                    }),
                );
            }
            // Always on the message's own sender, never on the thread: a
            // conversation has several, and taking the newest one's would block
            // whoever happened to reply last.
            if has_junk_folder {
                let entity = entity.clone();
                let sender = sender.clone();
                menu = menu.item(
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
                    .on_click(move |_, window, cx| {
                        entity.update(cx, |this, cx| {
                            this.toggle_sender_block(&sender, window, cx);
                        });
                    }),
                );
            }
            {
                let entity = entity.clone();
                let aid = aid.clone();
                let mid = mid.clone();
                let thread = thread.clone();
                menu = menu.item(
                    PopupMenuItem::new(match thread {
                        Some(_) => tr!("ctx-delete-thread", { count: thread_count }),
                        None => tr!("delete"),
                    })
                    .icon(crate::ui::icons::app_icon("trash-2"))
                    .disabled(offline)
                    .on_click(move |_, window, cx| {
                        entity.update(cx, |this, cx| {
                            match &thread {
                                Some(members) => {
                                    this.bulk_delete_messages_with_undo(
                                        members.clone(),
                                        window,
                                        cx,
                                    );
                                }
                                None => {
                                    this.delete_message_with_undo(aid.clone(), &mid, window, cx)
                                }
                            }
                            cx.notify();
                        });
                    }),
                );
            }
            menu
        }
    }
}

#[cfg(test)]
mod tests {
    use super::move_folder_targets;
    use crate::model::MailFolder;

    fn folder(id: &str, parent_id: Option<&str>) -> MailFolder {
        MailFolder {
            id: id.to_string(),
            display_name: id.to_string(),
            parent_id: parent_id.map(str::to_string),
            well_known_name: None,
            total_item_count: 0,
            unread_item_count: 0,
        }
    }

    #[test]
    fn move_targets_keep_recursive_folder_hierarchy() {
        let folders = [
            folder("root", None),
            folder("child", Some("root")),
            folder("grandchild", Some("child")),
            folder("other", None),
        ];

        let targets = move_folder_targets(&folders, None);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].id, "root");
        assert_eq!(targets[0].children[0].id, "child");
        assert_eq!(targets[0].children[0].children[0].id, "grandchild");
        assert_eq!(targets[1].id, "other");
    }

    #[test]
    fn move_targets_promote_children_of_current_folder() {
        let folders = [
            folder("root", None),
            folder("child", Some("root")),
            folder("grandchild", Some("child")),
        ];

        let targets = move_folder_targets(&folders, Some("root"));
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, "child");
        assert_eq!(targets[0].children[0].id, "grandchild");
    }
}
