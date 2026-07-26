//! Folder browser: collapsible trees, favorites, selection of accounts in the
//! unified inbox, and root/child mutations.

use super::super::app::AviaryApp;
use super::super::compose::ComposeInit;
use super::super::util;
use crate::model::{AccountId, MailFolder};
use crate::runtime::Cmd;
use gpui::{div, prelude::*, px, App, Context, ScrollWheelEvent, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState},
    menu::{ContextMenuExt, DropdownMenu, PopupMenuItem},
    v_flex, v_virtual_list, ActiveTheme, IconName, Sizable, StyledExt, WindowExt,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

#[derive(Clone)]
struct FolderTreeRow {
    folder: MailFolder,
    depth: usize,
    has_children: bool,
}

/// One line of the virtualized sidebar: the favorites header, a favorite, an
/// account header, or a folder of that account's tree.
///
/// Entries carry identities rather than rendered data — the row renderer reads
/// the current selection, unread count and offline state itself, exactly as the
/// message list does.
enum FolderEntry {
    FavoritesHeader {
        count: usize,
        collapsed: bool,
    },
    Favorite {
        account_id: AccountId,
        folder_id: String,
    },
    Account {
        account_id: AccountId,
        /// Separated from the block above by a rule, which adds to its height.
        separated: bool,
        collapsed: bool,
    },
    Folder {
        account_id: AccountId,
        folder_id: String,
        depth: usize,
        has_children: bool,
        collapsed: bool,
    },
}

/// Everything about an entry that changes its measured height.
///
/// Heights are measured once per variant and reused for every entry sharing
/// one, so a visual difference missing from this key silently offsets the whole
/// sidebar. Indentation and labels do not change a row's height; the account
/// header's separator rule and a folder's branch button do.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
enum FolderEntryVariant {
    FavoritesHeader,
    Row { has_children: bool },
    Account { separated: bool },
}

impl FolderEntry {
    fn variant(&self) -> FolderEntryVariant {
        match self {
            Self::FavoritesHeader { .. } => FolderEntryVariant::FavoritesHeader,
            Self::Favorite { .. } => FolderEntryVariant::Row {
                has_children: false,
            },
            Self::Account { separated, .. } => FolderEntryVariant::Account {
                separated: *separated,
            },
            Self::Folder { has_children, .. } => FolderEntryVariant::Row {
                has_children: *has_children,
            },
        }
    }
}

/// Measured heights of the sidebar's entry variants.
///
/// Only the measurement is cached, not the entries: the tree is cheap to
/// rebuild and depends on half a dozen settings, so caching it would mean an
/// invalidation to remember at every one of them. Heights depend on the UI
/// scale alone.
pub(crate) struct FolderListMetrics {
    ui_scale: u32,
    heights: HashMap<FolderEntryVariant, gpui::Pixels>,
}

/// Localized label for a well-known folder.
pub(super) fn folder_display_label(f: &MailFolder) -> gpui::SharedString {
    match f.well_known_name.as_deref() {
        Some("inbox") => tr!("folder-inbox"),
        Some("category-personal") => tr!("folder-category-primary"),
        Some("category-social") => tr!("folder-category-social"),
        Some("category-promotions") => tr!("folder-category-promotions"),
        Some("category-updates") => tr!("folder-category-updates"),
        Some("category-forums") => tr!("folder-category-forums"),
        Some("sentitems") => tr!("folder-sent"),
        Some("drafts") => tr!("folder-drafts"),
        Some("deleteditems") => tr!("folder-deleted"),
        Some("junkemail") => tr!("folder-junk"),
        Some("archive") => tr!("folder-archive"),
        Some("outbox") => tr!("folder-outbox"),
        _ => f.display_name.clone().into(),
    }
}

fn folder_icon(f: &MailFolder) -> gpui_component::Icon {
    let name = match f.well_known_name.as_deref() {
        Some("inbox") => "inbox",
        Some("category-personal") => "inbox",
        Some("category-social") => "users",
        Some("category-promotions") => "tag",
        Some("category-updates") => "bell",
        Some("category-forums") => "folder",
        Some("sentitems") => "send",
        Some("drafts") => "pencil",
        Some("deleteditems") => "trash-2",
        Some("junkemail") => "alert-circle",
        Some("archive") => "archive",
        _ => "folder",
    };
    crate::ui::icons::app_icon(name)
}

fn folder_sort_key(f: &MailFolder) -> (u8, String) {
    let rank = match f.well_known_name.as_deref() {
        Some("inbox") => 0,
        Some("category-personal") => 1,
        Some("category-social") => 2,
        Some("category-promotions") => 3,
        Some("category-updates") => 4,
        Some("category-forums") => 5,
        Some("drafts") => 6,
        Some("sentitems") => 7,
        Some("archive") => 8,
        Some("junkemail") => 9,
        Some("deleteditems") => 10,
        _ => 20,
    };
    (rank, f.display_name.to_lowercase())
}

fn folder_tree(folders: &[MailFolder], expanded: &[String]) -> Vec<FolderTreeRow> {
    let ids: HashSet<&str> = folders.iter().map(|folder| folder.id.as_str()).collect();
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
    roots.sort_by_key(folder_sort_key);
    for siblings in children.values_mut() {
        siblings.sort_by_key(folder_sort_key);
    }

    fn append(
        rows: &mut Vec<FolderTreeRow>,
        siblings: &[MailFolder],
        children: &HashMap<String, Vec<MailFolder>>,
        expanded: &[String],
        depth: usize,
        visited: &mut HashSet<String>,
    ) {
        for folder in siblings {
            if !visited.insert(folder.id.clone()) {
                continue;
            }
            let nested = children.get(&folder.id);
            rows.push(FolderTreeRow {
                folder: folder.clone(),
                depth,
                has_children: nested.is_some_and(|folders| !folders.is_empty()),
            });
            if expanded.contains(&folder.id) {
                if let Some(nested) = nested {
                    append(rows, nested, children, expanded, depth + 1, visited);
                }
            } else if let Some(nested) = nested {
                mark_descendants_visited(nested, children, visited);
            }
        }
    }

    fn mark_descendants_visited(
        siblings: &[MailFolder],
        children: &HashMap<String, Vec<MailFolder>>,
        visited: &mut HashSet<String>,
    ) {
        for folder in siblings {
            if !visited.insert(folder.id.clone()) {
                continue;
            }
            if let Some(nested) = children.get(&folder.id) {
                mark_descendants_visited(nested, children, visited);
            }
        }
    }

    let mut rows = Vec::with_capacity(folders.len());
    let mut visited = HashSet::new();
    append(&mut rows, &roots, &children, expanded, 0, &mut visited);
    // A malformed provider cycle must not make folders disappear.
    let mut orphans: Vec<_> = folders
        .iter()
        .filter(|folder| !visited.contains(&folder.id))
        .cloned()
        .collect();
    orphans.sort_by_key(folder_sort_key);
    append(&mut rows, &orphans, &children, expanded, 0, &mut visited);
    rows
}

impl AviaryApp {
    /// Flattens favorites and every account tree into the sidebar's rows.
    fn build_folder_entries(&self) -> Vec<FolderEntry> {
        let mut entries = Vec::new();
        let favorites = self.favorite_folders();
        let has_favorites = !favorites.is_empty();
        if has_favorites {
            let collapsed = self.settings.global.favorite_folders_collapsed;
            entries.push(FolderEntry::FavoritesHeader {
                count: favorites.len(),
                collapsed,
            });
            if !collapsed {
                entries.extend(favorites.into_iter().map(|(account_id, folder_id)| {
                    FolderEntry::Favorite {
                        account_id,
                        folder_id,
                    }
                }));
            }
        }

        for (index, account) in self.ordered_accounts().into_iter().enumerate() {
            let account_id = account.id;
            let collapsed = !self
                .settings
                .global
                .expanded_folder_account_ids
                .contains(&account_id.0);
            entries.push(FolderEntry::Account {
                account_id: account_id.clone(),
                separated: has_favorites || index > 0,
                collapsed,
            });
            if collapsed {
                continue;
            }
            let Some(folders) = self.mailbox.folders_by_account.get(&account_id) else {
                continue;
            };
            let expanded = &self
                .settings
                .account_or_default(Some(&account_id))
                .expanded_folder_ids;
            entries.extend(folder_tree(folders, expanded).into_iter().map(|row| {
                FolderEntry::Folder {
                    account_id: account_id.clone(),
                    depth: row.depth,
                    has_children: row.has_children,
                    collapsed: !expanded.contains(&row.folder.id),
                    folder_id: row.folder.id,
                }
            }));
        }
        entries
    }

    /// Heights of the entry variants present in `entries`, measured offscreen
    /// once per UI scale.
    fn folder_entry_sizes(
        &mut self,
        entries: &[FolderEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Rc<Vec<gpui::Size<gpui::Pixels>>> {
        let ui_scale = self.settings.global.ui_scale.to_bits();
        if self
            .folder_list_metrics
            .as_ref()
            .is_none_or(|metrics| metrics.ui_scale != ui_scale)
        {
            self.folder_list_metrics = Some(FolderListMetrics {
                ui_scale,
                heights: HashMap::new(),
            });
        }

        let available = gpui::size(
            gpui::AvailableSpace::MinContent,
            gpui::AvailableSpace::MinContent,
        );
        for entry in entries {
            let variant = entry.variant();
            if self
                .folder_list_metrics
                .as_ref()
                .is_some_and(|metrics| metrics.heights.contains_key(&variant))
            {
                continue;
            }
            let mut element = self.folder_list_item(entry, cx);
            let height = element.layout_as_root(available, window, cx).height;
            if let Some(metrics) = self.folder_list_metrics.as_mut() {
                metrics.heights.insert(variant, height);
            }
        }

        let heights = self
            .folder_list_metrics
            .as_ref()
            .map(|metrics| &metrics.heights);
        Rc::new(
            entries
                .iter()
                .map(|entry| {
                    let height = heights
                        .and_then(|heights| heights.get(&entry.variant()).copied())
                        .unwrap_or_default();
                    gpui::size(px(0.), height)
                })
                .collect(),
        )
    }

    /// Renders one sidebar entry. Also used for offscreen measurement, so each
    /// variant must keep a constant height.
    ///
    /// The wrapper carries every vertical gap the pane used to get from the
    /// column's `gap` and from the headers' top margins: `layout_as_root`
    /// measures a node's own box, so spacing expressed as a margin would be
    /// absent from the height the virtual list reserves.
    fn folder_list_item(&self, entry: &FolderEntry, cx: &mut Context<Self>) -> gpui::AnyElement {
        let leading_gap = matches!(
            entry,
            FolderEntry::FavoritesHeader { .. }
                | FolderEntry::Account {
                    separated: true,
                    ..
                }
        );
        let inner = self.folder_list_item_inner(entry, cx);
        div()
            .w_full()
            .pb_0p5()
            .when(leading_gap, |el| el.pt_2())
            .child(inner)
            .into_any_element()
    }

    fn folder_list_item_inner(
        &self,
        entry: &FolderEntry,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match entry {
            FolderEntry::FavoritesHeader { count, collapsed } => self
                .folder_section_header(
                    "favorite-folders",
                    tr!("folders-favorites").to_string(),
                    *count,
                    *collapsed,
                    true,
                    cx,
                )
                .into_any_element(),
            FolderEntry::Favorite {
                account_id,
                folder_id,
            } => self.render_favorite_row(account_id, folder_id, cx),
            FolderEntry::Account {
                account_id,
                separated,
                collapsed,
            } => self.render_account_header(account_id, *separated, *collapsed, cx),
            FolderEntry::Folder {
                account_id,
                folder_id,
                depth,
                has_children,
                collapsed,
            } => self.render_folder_tree_row(
                account_id,
                folder_id,
                *depth,
                *has_children,
                *collapsed,
                cx,
            ),
        }
    }

    pub(super) fn render_folders_pane(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entries = Rc::new(self.build_folder_entries());
        let sizes = self.folder_entry_sizes(&entries, window, cx);
        let scroll_handle = self.scrolls.folders.handle.base_handle().clone();
        self.scrolls.folders.motion.advance(&scroll_handle, window);

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .w_full()
                    .min_h(super::MAIL_PANE_HEADER_HEIGHT)
                    .px_2()
                    .py_1p5()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(self.folder_row_generic(
                        "all-inboxes",
                        crate::ui::icons::app_icon("mails"),
                        tr!("folders-all-inboxes").to_string(),
                        None,
                        self.mailbox.unified_selected_account.is_none(),
                        cx.listener(|this, _, _, cx| this.select_folder(None, None, cx)),
                        cx,
                    )),
            )
            .child(
                // Non-scrollable wrapper: its wheel listener runs after the
                // virtual list's internal scroll handler (see `ui/motion.rs`).
                div()
                    .id("folders-scroll")
                    .flex_1()
                    .min_h_0()
                    .on_scroll_wheel(cx.listener({
                        let handle = scroll_handle;
                        move |this, event: &ScrollWheelEvent, window, cx| {
                            if this.scrolls.folders.motion.on_wheel(&handle, event, window) {
                                cx.notify();
                            }
                        }
                    }))
                    .child(
                        v_virtual_list(cx.entity(), "folders-vlist", sizes, {
                            let entries = entries.clone();
                            move |this, range: Range<usize>, _window, cx| {
                                range
                                    .filter_map(|ix| {
                                        entries
                                            .get(ix)
                                            .map(|entry| this.folder_list_item(entry, cx))
                                    })
                                    .collect::<Vec<_>>()
                            }
                        })
                        .track_scroll(&self.scrolls.folders.handle)
                        .px_2()
                        .py_2(),
                    ),
            )
            .child(self.render_sidebar_navigation(cx))
    }

    /// Pinned folders, as `(account, folder)` identities. The row renderer
    /// looks the account and folder up again, so the sidebar never carries
    /// copies of them between frames.
    fn favorite_folders(&self) -> Vec<(AccountId, String)> {
        let mut favorites = Vec::new();
        for account in self.ordered_accounts() {
            let pinned = &self
                .settings
                .account_or_default(Some(&account.id))
                .pinned_folder_ids;
            let Some(folders) = self.mailbox.folders_by_account.get(&account.id) else {
                continue;
            };
            for folder_id in pinned {
                if folders.iter().any(|folder| &folder.id == folder_id) {
                    favorites.push((account.id.clone(), folder_id.clone()));
                }
            }
        }
        favorites
    }

    fn folder_by_id(&self, account_id: &AccountId, folder_id: &str) -> Option<&MailFolder> {
        self.mailbox
            .folders_by_account
            .get(account_id)?
            .iter()
            .find(|folder| folder.id == folder_id)
    }

    /// Whether this folder is the one the listing currently shows. The inbox
    /// also answers yes when no folder is selected, since that is what the
    /// account's default listing displays.
    fn folder_is_selected(&self, account_id: &AccountId, folder: &MailFolder) -> bool {
        self.mailbox.unified_selected_account.as_ref() == Some(account_id)
            && (self.mailbox.selected_folder_id.as_deref() == Some(folder.id.as_str())
                || (folder.well_known_name.as_deref() == Some("inbox")
                    && self.mailbox.selected_folder_id.is_none()))
    }

    fn render_favorite_row(
        &self,
        account_id: &AccountId,
        folder_id: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (Some(account), Some(folder)) = (
            self.account(account_id).cloned(),
            self.folder_by_id(account_id, folder_id).cloned(),
        ) else {
            return div().into_any_element();
        };
        let selected = self.folder_is_selected(account_id, &folder);
        let label: gpui::SharedString = format!(
            "{} · {}",
            folder_display_label(&folder),
            self.account_label(&account)
        )
        .into();
        let click_aid = account_id.clone();
        let target_folder =
            (folder.well_known_name.as_deref() != Some("inbox")).then(|| folder.id.clone());
        self.folder_row(
            account_id,
            &folder,
            label,
            0,
            false,
            false,
            selected,
            cx.listener(move |this, _, _, cx| {
                this.select_folder(Some(click_aid.clone()), target_folder.clone(), cx)
            }),
            cx,
        )
        .into_any_element()
    }

    fn render_folder_tree_row(
        &self,
        account_id: &AccountId,
        folder_id: &str,
        depth: usize,
        has_children: bool,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(folder) = self.folder_by_id(account_id, folder_id).cloned() else {
            return div().into_any_element();
        };
        let selected = self.folder_is_selected(account_id, &folder);
        let click_account = account_id.clone();
        let target_folder =
            (folder.well_known_name.as_deref() != Some("inbox")).then(|| folder.id.clone());
        self.folder_row(
            account_id,
            &folder,
            folder_display_label(&folder),
            depth,
            has_children,
            collapsed,
            selected,
            cx.listener(move |this, _, _, cx| {
                this.select_folder(Some(click_account.clone()), target_folder.clone(), cx)
            }),
            cx,
        )
        .into_any_element()
    }

    fn render_account_header(
        &self,
        account_id: &AccountId,
        separated: bool,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(account) = self.account(account_id).cloned() else {
            return div().into_any_element();
        };
        let aid = account.id.clone();
        let included = self.unified_account_included(&aid);
        let offline = self.offline_accounts.contains(&aid);
        let theme = cx.theme().clone();
        let account_label = self.account_label(&account);
        let account_email = account.email.clone();
        let color = util::account_color(
            &aid,
            self.settings
                .accounts
                .get(&aid)
                .and_then(|settings| settings.color_override),
        );
        let entity = cx.entity();
        let checkbox_aid = aid.clone();
        let collapse_aid = aid.clone();
        let label_toggle_aid = aid.clone();
        let create_aid = aid.clone();
        let compose_aid = aid.clone();
        let actions_entity = cx.entity();
        h_flex()
            .id(gpui::ElementId::Name(
                format!("folder-account-{}", aid.0).into(),
            ))
            .gap_1()
            .items_center()
            .px_1()
            .when(separated, |el| {
                el.pt_2().border_t_1().border_color(theme.border)
            })
            .child(
                Button::new(gpui::ElementId::Name(
                    format!("folder-account-collapse-{}", aid.0).into(),
                ))
                .ghost()
                .xsmall()
                .icon(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .tooltip(tr!("folders-tree-toggle"))
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| {
                        this.toggle_folder_account_collapsed(&collapse_aid);
                        cx.notify();
                    });
                }),
            )
            .child(div().w(px(8.)).h(px(8.)).mr_1().rounded_full().bg(color))
            .child(
                v_flex()
                    .id(gpui::ElementId::Name(
                        format!("folder-account-select-{}", aid.0).into(),
                    ))
                    .flex_1()
                    .min_w_0()
                    .gap_0()
                    .cursor_pointer()
                    .when(!included, |el| el.opacity(0.55))
                    .child(
                        div()
                            .w_full()
                            .text_sm()
                            .font_semibold()
                            .line_height(px(17.))
                            .truncate()
                            .child(account_label),
                    )
                    .child(
                        div()
                            .w_full()
                            .text_xs()
                            .line_height(px(13.))
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(account_email),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_folder_account_collapsed(&label_toggle_aid);
                        cx.notify();
                    })),
            )
            .child(
                Button::new(gpui::ElementId::Name(
                    format!("folder-account-create-{}", aid.0).into(),
                ))
                .ghost()
                .xsmall()
                .icon(IconName::Plus)
                .tooltip(tr!("folders-account-actions"))
                .dropdown_menu(move |menu, _window, _cx| {
                    let compose_entity = actions_entity.clone();
                    let folder_entity = actions_entity.clone();
                    let compose_aid = compose_aid.clone();
                    let create_aid = create_aid.clone();
                    menu.item(
                        PopupMenuItem::new(tr!("menu-new-message"))
                            .icon(super::super::icons::app_icon("pencil"))
                            .on_click(move |_, window, cx| {
                                compose_entity.update(cx, |this, cx| {
                                    this.open_inline_compose(
                                        ComposeInit {
                                            from_account_id: Some(compose_aid.clone()),
                                            ..ComposeInit::default()
                                        },
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    )
                    .item(
                        PopupMenuItem::new(tr!("folders-new"))
                            .icon(super::super::icons::app_icon("folder"))
                            .disabled(offline)
                            .on_click(move |_, window, cx| {
                                folder_entity.update(cx, |this, cx| {
                                    this.open_folder_dialog(create_aid.clone(), None, window, cx);
                                });
                            }),
                    )
                }),
            )
            .child(
                Checkbox::new(gpui::ElementId::Name(
                    format!("folder-account-visible-{}", aid.0).into(),
                ))
                .xsmall()
                .checked(included)
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    this.set_unified_account_included(&checkbox_aid, *checked, cx);
                })),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn folder_row(
        &self,
        account_id: &AccountId,
        folder: &MailFolder,
        label: gpui::SharedString,
        depth: usize,
        has_children: bool,
        collapsed: bool,
        selected: bool,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let unread = (folder.unread_item_count > 0).then_some(folder.unread_item_count);
        let entity = cx.entity();
        let aid = account_id.clone();
        let folder_id = folder.id.clone();
        let folder_name = folder.display_name.clone();
        let deletable = folder.well_known_name.is_none();
        let can_create_child = !folder
            .well_known_name
            .as_deref()
            .is_some_and(|name| name.starts_with("category-"));
        let offline = self.offline_accounts.contains(account_id);
        let pinned = self
            .settings
            .account_or_default(Some(account_id))
            .pinned_folder_ids
            .contains(&folder.id);

        let branch = if has_children {
            let branch_entity = entity.clone();
            let branch_aid = aid.clone();
            let branch_id = folder_id.clone();
            Button::new(gpui::ElementId::Name(
                format!("folder-branch-{}-{}", aid.0, folder.id).into(),
            ))
            .ghost()
            .xsmall()
            .icon(if collapsed {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .tooltip(tr!("folders-tree-toggle"))
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                branch_entity.update(cx, |this, cx| {
                    this.toggle_folder_collapsed(&branch_aid, &branch_id);
                    cx.notify();
                });
            })
            .into_any_element()
        } else {
            div().w(px(20.)).h(px(20.)).into_any_element()
        };

        let row = div()
            .id(gpui::ElementId::Name(
                format!("folder-{}-{}", account_id.0, folder.id).into(),
            ))
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .pl(px(4. + depth as f32 * 14.))
            .pr_2()
            .py_1()
            .rounded(theme.radius)
            .cursor_pointer()
            .when(selected, |el| {
                el.bg(theme.sidebar_accent)
                    .text_color(theme.sidebar_accent_foreground)
            })
            .hover(|style| style.bg(theme.sidebar_accent.opacity(0.6)))
            .child(branch)
            .child(folder_icon(folder).small())
            .child(div().flex_1().min_w_0().truncate().text_sm().child(label))
            .when(pinned, |el| {
                el.child(
                    crate::ui::icons::app_icon("pin")
                        .xsmall()
                        .text_color(theme.warning),
                )
            })
            .when_some(unread, |el, count| {
                el.child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .text_color(theme.primary)
                        .child(count.to_string()),
                )
            })
            .on_click(on_click);

        let context_aid = aid.clone();
        let context_folder_id = folder_id.clone();
        let context_menu = row.context_menu(move |menu, _window, _cx| {
            let pin_entity = entity.clone();
            let pin_aid = context_aid.clone();
            let pin_id = context_folder_id.clone();
            let child_entity = entity.clone();
            let child_aid = context_aid.clone();
            let child_id = context_folder_id.clone();
            let mut menu = menu
                .item(
                    PopupMenuItem::new(if pinned {
                        tr!("folders-remove-favorite")
                    } else {
                        tr!("folders-add-favorite")
                    })
                    .icon(crate::ui::icons::app_icon(if pinned {
                        "pin-off"
                    } else {
                        "pin"
                    }))
                    .on_click(move |_, _, cx| {
                        pin_entity.update(cx, |this, cx| {
                            this.set_folder_pinned(&pin_aid, &pin_id, !pinned);
                            cx.notify();
                        });
                    }),
                )
                .item(
                    PopupMenuItem::new(tr!("folders-new-child"))
                        .icon(crate::ui::icons::app_icon("folder"))
                        .disabled(offline || !can_create_child)
                        .on_click(move |_, window, cx| {
                            child_entity.update(cx, |this, cx| {
                                this.open_folder_dialog(
                                    child_aid.clone(),
                                    Some(child_id.clone()),
                                    window,
                                    cx,
                                );
                            });
                        }),
                );
            if deletable {
                let rename_entity = entity.clone();
                let rename_aid = context_aid.clone();
                let rename_id = context_folder_id.clone();
                let rename_name = folder_name.clone();
                let delete_entity = entity.clone();
                let delete_aid = context_aid.clone();
                let delete_id = context_folder_id.clone();
                let delete_name = folder_name.clone();
                menu = menu
                    .item(
                        PopupMenuItem::new(tr!("folders-rename"))
                            .icon(crate::ui::icons::app_icon("pencil"))
                            .disabled(offline)
                            .on_click(move |_, window, cx| {
                                rename_entity.update(cx, |this, cx| {
                                    this.open_rename_folder_dialog(
                                        rename_aid.clone(),
                                        rename_id.clone(),
                                        rename_name.clone(),
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    )
                    .item(
                        PopupMenuItem::new(tr!("folders-delete"))
                            .icon(crate::ui::icons::app_icon("trash-2"))
                            .disabled(offline)
                            .on_click(move |_, window, cx| {
                                delete_entity.update(cx, |this, cx| {
                                    this.confirm_delete_folder(
                                        delete_aid.clone(),
                                        delete_id.clone(),
                                        delete_name.clone(),
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    );
            }
            menu
        });
        div().w_full().child(context_menu)
    }

    #[allow(clippy::too_many_arguments)]
    fn folder_row_generic(
        &self,
        id: impl Into<gpui::ElementId>,
        icon: gpui_component::Icon,
        label: String,
        unread: Option<u32>,
        selected: bool,
        on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = cx.theme().clone();
        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .cursor_pointer()
            .when(selected, |el| {
                el.bg(theme.sidebar_accent)
                    .text_color(theme.sidebar_accent_foreground)
            })
            .hover(|style| style.bg(theme.sidebar_accent.opacity(0.6)))
            .child(icon.small())
            .child(div().flex_1().truncate().text_sm().child(label))
            .when_some(unread, |el, count| {
                el.child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .text_color(theme.primary)
                        .child(count.to_string()),
                )
            })
            .on_click(on_click)
    }

    fn folder_section_header(
        &self,
        id: &str,
        label: String,
        count: usize,
        collapsed: bool,
        favorite: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .id(gpui::ElementId::Name(format!("folder-section-{id}").into()))
            .px_2()
            .py_1()
            .flex()
            .items_center()
            .gap_1p5()
            .rounded(theme.radius)
            .cursor_pointer()
            .hover(|style| style.bg(theme.list_hover))
            .child(
                gpui_component::Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(theme.muted_foreground),
            )
            .when(favorite, |el| {
                el.child(
                    crate::ui::icons::app_icon("pin")
                        .xsmall()
                        .text_color(theme.warning),
                )
            })
            .child(div().flex_1().text_sm().font_semibold().child(label))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(count.to_string()),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.settings.global.favorite_folders_collapsed =
                    !this.settings.global.favorite_folders_collapsed;
                this.settings.save();
                cx.notify();
            }))
    }

    fn set_unified_account_included(
        &mut self,
        account_id: &AccountId,
        included: bool,
        cx: &mut Context<Self>,
    ) {
        let hidden = &mut self.settings.global.unified_hidden_account_ids;
        if included {
            hidden.retain(|id| id != &account_id.0);
            if !self.mailbox.folders_by_account.contains_key(account_id) {
                self.send(Cmd::LoadFolders {
                    account_id: account_id.clone(),
                });
            }
            self.ensure_tags_loaded(account_id);
        } else if !hidden.contains(&account_id.0) {
            hidden.push(account_id.0.clone());
        }
        self.settings.save();
        self.calendar.force_reload();
        self.refresh_visible_contacts();
        if self.mailbox.unified_selected_account.is_none() {
            self.select_folder(None, None, cx);
        } else {
            cx.notify();
        }
    }

    fn toggle_folder_account_collapsed(&mut self, account_id: &AccountId) {
        let expanded = &mut self.settings.global.expanded_folder_account_ids;
        if expanded.contains(&account_id.0) {
            expanded.retain(|id| id != &account_id.0);
        } else {
            expanded.push(account_id.0.clone());
        }
        self.settings.save();
    }

    fn toggle_folder_collapsed(&mut self, account_id: &AccountId, folder_id: &str) {
        let expanded = &mut self.settings.account_mut(account_id).expanded_folder_ids;
        if expanded.iter().any(|id| id == folder_id) {
            expanded.retain(|id| id != folder_id);
        } else {
            expanded.push(folder_id.to_string());
        }
        self.settings.save();
    }

    fn set_folder_pinned(&mut self, account_id: &AccountId, folder_id: &str, pinned: bool) {
        let ids = &mut self.settings.account_mut(account_id).pinned_folder_ids;
        if pinned {
            if !ids.iter().any(|id| id == folder_id) {
                ids.push(folder_id.to_string());
            }
        } else {
            ids.retain(|id| id != folder_id);
        }
        self.settings.save();
    }

    fn open_folder_dialog(
        &mut self,
        account_id: AccountId,
        parent_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("folders-name-hint")));
        self.folder_dialog_input = Some(input.clone());
        let entity = cx.entity();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let entity = entity.clone();
            let input = input.clone();
            let account_id = account_id.clone();
            let parent_id = parent_id.clone();
            dialog
                .title(if parent_id.is_some() {
                    tr!("folders-new-child-title")
                } else {
                    tr!("folders-new-title")
                })
                .confirm()
                .child(Input::new(&input))
                .on_ok(move |_, _window, cx| {
                    let name = input.read(cx).value().trim().to_string();
                    if name.is_empty() {
                        return false;
                    }
                    entity.update(cx, |this, cx| {
                        this.send(Cmd::CreateFolder {
                            account_id: account_id.clone(),
                            name,
                            parent_id: parent_id.clone(),
                        });
                        cx.notify();
                    });
                    true
                })
        });
    }

    fn open_rename_folder_dialog(
        &mut self,
        account_id: AccountId,
        id: String,
        current: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current));
        self.folder_dialog_input = Some(input.clone());
        let entity = cx.entity();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let entity = entity.clone();
            let input = input.clone();
            let account_id = account_id.clone();
            let id = id.clone();
            dialog
                .title(tr!("folders-rename-title"))
                .confirm()
                .child(Input::new(&input))
                .on_ok(move |_, _window, cx| {
                    let new_name = input.read(cx).value().trim().to_string();
                    if new_name.is_empty() {
                        return false;
                    }
                    entity.update(cx, |this, cx| {
                        this.send(Cmd::RenameFolder {
                            account_id: account_id.clone(),
                            id: id.clone(),
                            new_name,
                        });
                        cx.notify();
                    });
                    true
                })
        });
    }

    fn confirm_delete_folder(
        &mut self,
        account_id: AccountId,
        id: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entity = cx.entity();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let entity = entity.clone();
            let account_id = account_id.clone();
            let id = id.clone();
            dialog
                .title(tr!("folders-delete-title"))
                .confirm()
                .child(div().child(tr!("folders-delete-confirm-short", { name: name.clone() })))
                .on_ok(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        let delay = this.action_delay_secs();
                        this.send_undoable(
                            Cmd::DeleteFolder {
                                account_id: account_id.clone(),
                                id: id.clone(),
                            },
                            tr!("undo-folder-delete-pending", { seconds: delay }),
                            tr!("undo-folder-delete-started"),
                            tr!("undo-folder-delete-cancelled"),
                            window,
                            cx,
                        );
                        cx.notify();
                    });
                    true
                })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::folder_tree;
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
    fn tree_keeps_depth_and_hides_collapsed_descendants() {
        let folders = vec![
            folder("root", None),
            folder("child", Some("root")),
            folder("grandchild", Some("child")),
        ];
        let rows = folder_tree(&folders, &["root".to_string(), "child".to_string()]);
        assert_eq!(
            rows.iter()
                .map(|row| (row.folder.id.as_str(), row.depth, row.has_children))
                .collect::<Vec<_>>(),
            vec![
                ("root", 0, true),
                ("child", 1, true),
                ("grandchild", 2, false)
            ]
        );

        let rows = folder_tree(&folders, &[]);
        assert_eq!(
            rows.iter()
                .map(|row| row.folder.id.as_str())
                .collect::<Vec<_>>(),
            vec!["root"]
        );
    }

    #[test]
    fn tree_surfaces_missing_parents_as_roots() {
        let rows = folder_tree(&[folder("orphan", Some("missing"))], &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].folder.id, "orphan");
        assert_eq!(rows[0].depth, 0);
    }
}
