//! Message-list pane: search, rows reused by sender history and Contacts, and
//! pagination.

use super::super::app::AviaryApp;
use super::super::motion::{HoverMotionExt as _, Lerp as _};
use super::super::state::MainView;
use super::super::util;
use super::message_menu::{move_folder_targets, MoveMenu, MoveScope};
use crate::model::{AccountId, Contact, MessageHeader, MessageRef, Provider};
use crate::runtime::Cmd;
use crate::ui::settings::{MailSearchScope, MailSearchSort};
use chrono::{Local, NaiveDate};
use gpui::{
    div, prelude::*, px, App, AvailableSpace, Context, MouseButton, ScrollStrategy,
    ScrollWheelEvent, Window,
};
use gpui_component::{
    avatar::Avatar,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    input::{Input, MoveDown, MoveUp},
    menu::{ContextMenuExt, DropdownMenu, PopupMenuItem},
    spinner::Spinner,
    v_flex, v_virtual_list, ActiveTheme, Disableable, IconName, Selectable, Sizable, StyledExt,
};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use super::super::components::overlay_popover::OverlayPopover;

/// Starts fetching the next page before the user reaches the exact bottom of
/// the list, keeping scrolling fluid.
const LOAD_MORE_THRESHOLD: gpui::Pixels = px(320.);
/// Keep the shortcut out of the way while the first couple of rows are still
/// visible; after that, returning to the list's beginning becomes useful.
const SCROLL_TO_TOP_THRESHOLD: gpui::Pixels = px(180.);
const SEARCH_CONTACT_SUGGESTIONS: usize = 4;
const SEARCH_HISTORY_SUGGESTIONS: usize = 4;

struct MailSearchSuggestions {
    query_empty: bool,
    contacts: Vec<Contact>,
    history: Vec<String>,
}

impl MailSearchSuggestions {
    fn len(&self) -> usize {
        self.contacts.len() + self.history.len()
    }

    fn query_at(&self, index: usize) -> Option<String> {
        self.contacts
            .get(index)
            .map(|contact| contact.email.clone())
            .or_else(|| {
                self.history
                    .get(index.checked_sub(self.contacts.len())?)
                    .cloned()
            })
    }
}

/// A conversation shown as one row.
///
/// The row *is* the thread's newest message — clicking it opens that message,
/// as Gmail does — and the chevron is what expands the thread in place, as
/// Outlook does. Actions that would otherwise hit a single message (select,
/// pin) apply to the whole thread instead.
#[derive(Clone)]
struct GroupRow {
    /// `(account, conversation)`: thread ids are only comparable inside one
    /// account, and the inbox may be unified.
    key: (AccountId, String),
    /// Members currently loaded in the list, newest first. Bulk actions must
    /// stay within these — acting on messages that are not on screen would be
    /// unexplainable.
    members: Vec<MessageRef>,
    /// Size of the thread as best known: the cache usually knows more than
    /// the page on screen (see `Evt::ConversationTotals`).
    total: usize,
    expanded: bool,
    /// A thread reads as unread as soon as one of its messages does, and as
    /// pinned as soon as one of them is. Both are settled while the member
    /// headers are at hand, so rendering a row never has to look them up
    /// again in the message list.
    has_unread: bool,
    pinned: bool,
}

/// Entry in the virtual message list: the section-to-row tree
/// is flattened into a sequence of known-height items.
enum MsgEntry {
    Header {
        key: String,
        label: String,
        count: usize,
        pinned: bool,
        collapsed: bool,
    },
    Row {
        reference: MessageRef,
        time_only: bool,
        pinned: bool,
        section_end: bool,
        /// Indented member of an expanded conversation.
        in_group: bool,
    },
    Group {
        reference: MessageRef,
        group: GroupRow,
        time_only: bool,
        pinned: bool,
        section_end: bool,
    },
}

/// Everything about an entry that can change its measured height.
///
/// Heights are measured once per variant and reused for every entry sharing
/// one, so a visual difference missing from this key silently offsets the
/// whole list. Deriving it from the very fields the renderer branches on is
/// what keeps the two in step.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
enum MsgEntryVariant {
    Header {
        pinned: bool,
        collapsed: bool,
    },
    Row {
        pinned: bool,
        section_end: bool,
        in_group: bool,
    },
    Group {
        pinned: bool,
        section_end: bool,
    },
}

impl MsgEntry {
    fn variant(&self) -> MsgEntryVariant {
        match self {
            Self::Header {
                pinned, collapsed, ..
            } => MsgEntryVariant::Header {
                pinned: *pinned,
                collapsed: *collapsed,
            },
            Self::Row {
                pinned,
                section_end,
                in_group,
                ..
            } => MsgEntryVariant::Row {
                pinned: *pinned,
                section_end: *section_end,
                in_group: *in_group,
            },
            Self::Group {
                pinned,
                section_end,
                ..
            } => MsgEntryVariant::Group {
                pinned: *pinned,
                section_end: *section_end,
            },
        }
    }

    /// Closes the pinned section's frame on the last entry it contains,
    /// whatever kind that entry turned out to be.
    fn mark_section_end(&mut self) {
        match self {
            Self::Row { section_end, .. } | Self::Group { section_end, .. } => *section_end = true,
            Self::Header { .. } => {}
        }
    }
}

/// One row of the list before sections and virtualization: either a lone
/// message or a conversation standing in for several.
enum MessageListItem<'a> {
    Single(&'a MessageHeader),
    Group {
        /// Newest first; `[0]` dates the item and decides its section.
        members: Vec<&'a MessageHeader>,
        row: GroupRow,
    },
}

impl<'a> MessageListItem<'a> {
    /// Message that dates the item and stands in for it in the list.
    fn representative(&self) -> &'a MessageHeader {
        match self {
            Self::Single(message) => message,
            Self::Group { members, .. } => members[0],
        }
    }
}

/// Folds a flat, newest-first header list into conversations, in place.
///
/// A thread takes the position — and therefore the day section — of its
/// newest member, which is simply the first one encountered. Threads reduced
/// to a single loaded message stay ordinary rows: a chevron over nothing and
/// a counter reading "1" would turn the whole mailbox into false groups.
///
/// The key is `(account, conversation)` throughout: thread ids come from
/// three different providers and are only comparable inside one account, so a
/// bare id could merge two unrelated exchanges in a unified inbox.
fn group_message_items<'a>(
    headers: &[&'a MessageHeader],
    expanded: &HashSet<(AccountId, String)>,
    totals: &HashMap<(AccountId, String), usize>,
    is_pinned: &dyn Fn(&MessageHeader) -> bool,
) -> Vec<MessageListItem<'a>> {
    let mut members_by_key: HashMap<(AccountId, String), Vec<&'a MessageHeader>> = HashMap::new();
    for message in headers.iter().copied() {
        let Some(conversation_id) = message.conversation_id.as_ref() else {
            continue;
        };
        members_by_key
            .entry((message.account_id.clone(), conversation_id.clone()))
            .or_default()
            .push(message);
    }

    let mut placed: HashSet<(AccountId, String)> = HashSet::new();
    let mut items = Vec::with_capacity(headers.len());
    for message in headers.iter().copied() {
        let key = message
            .conversation_id
            .as_ref()
            .map(|conversation_id| (message.account_id.clone(), conversation_id.clone()));
        let Some(key) = key else {
            items.push(MessageListItem::Single(message));
            continue;
        };
        let members = members_by_key.get(&key).expect("indexed above");
        if members.len() < 2 {
            items.push(MessageListItem::Single(message));
            continue;
        }
        // Only the newest member opens the group; the others were folded into
        // it and produce no item of their own.
        if !placed.insert(key.clone()) {
            continue;
        }
        let total = totals.get(&key).copied().unwrap_or(0).max(members.len());
        items.push(MessageListItem::Group {
            members: members.clone(),
            row: GroupRow {
                expanded: expanded.contains(&key),
                members: members
                    .iter()
                    .map(|message| MessageRef {
                        account_id: message.account_id.clone(),
                        id: message.id.clone(),
                    })
                    .collect(),
                has_unread: members.iter().any(|message| !message.is_read),
                pinned: members.iter().any(|message| is_pinned(message)),
                total,
                key,
            },
        });
    }
    items
}

/// Flattens grouped items into the virtual list's sections and rows.
///
/// **Every message must produce exactly one row.** An expanded group already
/// shows its newest message in its own summary row, so the indented members
/// below start at the *second* one. Listing it twice is not merely redundant:
/// both rows would carry the same gpui `ElementId`, and the duplicate row
/// stops reacting to hover and clicks.
fn build_list_entries(
    items: &[MessageListItem<'_>],
    collapsed_sections: &HashSet<String>,
    is_pinned: &dyn Fn(&MessageHeader) -> bool,
) -> Vec<MsgEntry> {
    let mut entries = Vec::new();
    let push_section = |entries: &mut Vec<MsgEntry>,
                        key: String,
                        label: String,
                        items: Vec<&MessageListItem<'_>>,
                        pinned: bool,
                        collapsed: bool| {
        entries.push(MsgEntry::Header {
            key,
            label,
            count: items.len(),
            pinned,
            collapsed,
        });
        if collapsed {
            return;
        }
        let first = entries.len();
        let reference_of = |message: &MessageHeader| MessageRef {
            account_id: message.account_id.clone(),
            id: message.id.clone(),
        };
        for item in items {
            match item {
                MessageListItem::Single(message) => entries.push(MsgEntry::Row {
                    reference: reference_of(message),
                    time_only: !pinned,
                    pinned,
                    section_end: false,
                    in_group: false,
                }),
                MessageListItem::Group { members, row } => {
                    entries.push(MsgEntry::Group {
                        reference: reference_of(members[0]),
                        group: row.clone(),
                        time_only: !pinned,
                        pinned,
                        section_end: false,
                    });
                    if row.expanded {
                        // `skip(1)`: the summary row above *is* members[0].
                        entries.extend(members.iter().skip(1).map(|message| MsgEntry::Row {
                            reference: reference_of(message),
                            time_only: !pinned,
                            pinned,
                            section_end: false,
                            in_group: true,
                        }));
                    }
                }
            }
        }
        // The pinned frame closes on whatever row ends up last, which
        // expanding a group changes.
        if let Some(last) = entries[first..].last_mut() {
            last.mark_section_end();
        }
    };

    // Pinning applies to the thread: any pinned member keeps the whole
    // conversation at the top, including replies that arrive afterwards.
    let item_is_pinned = |item: &MessageListItem<'_>| match item {
        MessageListItem::Single(message) => is_pinned(message),
        MessageListItem::Group { row, .. } => row.pinned,
    };

    let pinned: Vec<_> = items.iter().filter(|item| item_is_pinned(item)).collect();
    if !pinned.is_empty() {
        let key = "pinned".to_string();
        let collapsed = collapsed_sections.contains(&key);
        push_section(
            &mut entries,
            key,
            tr!("messages-pinned-section").to_string(),
            pinned,
            true,
            collapsed,
        );
    }
    let mut by_day: BTreeMap<Reverse<NaiveDate>, Vec<&MessageListItem<'_>>> = BTreeMap::new();
    for item in items.iter().filter(|item| !item_is_pinned(item)) {
        let day = item
            .representative()
            .received
            .with_timezone(&Local)
            .date_naive();
        by_day.entry(Reverse(day)).or_default().push(item);
    }
    for (Reverse(day), items) in by_day {
        let key = format!("day:{day}");
        let collapsed = collapsed_sections.contains(&key);
        push_section(
            &mut entries,
            key,
            util::message_day_label(day),
            items,
            false,
            collapsed,
        );
    }
    entries
}

#[derive(PartialEq, Eq)]
struct MessageListCacheKey {
    revision: u64,
    ui_scale: u32,
    language: super::super::settings::LanguageChoice,
}

pub(crate) struct MessageListCache {
    key: MessageListCacheKey,
    entries: Rc<Vec<MsgEntry>>,
    sizes: Rc<Vec<gpui::Size<gpui::Pixels>>>,
}

fn neighbor_index_after_removal(len: usize, removed: usize) -> Option<usize> {
    if removed + 1 < len {
        Some(removed + 1)
    } else {
        removed.checked_sub(1)
    }
}

impl AviaryApp {
    fn mail_search_suggestions(&self, cx: &App) -> MailSearchSuggestions {
        let draft = self.search_input.read(cx).value().trim().to_string();
        let query_empty = draft.is_empty();
        let contacts = if query_empty {
            Vec::new()
        } else {
            self.address_book.search(&draft, SEARCH_CONTACT_SUGGESTIONS)
        };
        let history: Vec<String> = self
            .mailbox
            .search
            .history
            .iter()
            .take(SEARCH_HISTORY_SUGGESTIONS)
            .cloned()
            .collect();

        MailSearchSuggestions {
            query_empty,
            contacts,
            history,
        }
    }

    pub(crate) fn selected_mail_search_query(&self, cx: &App) -> Option<String> {
        if !self.mailbox.search.menu_open {
            return None;
        }
        let selected = self.mailbox.search.menu_selection?;
        self.mail_search_suggestions(cx).query_at(selected)
    }

    fn move_mail_search_selection(&mut self, down: bool, cx: &mut Context<Self>) {
        if !self.mailbox.search.menu_open {
            return;
        }
        let len = self.mail_search_suggestions(cx).len();
        if len == 0 {
            return;
        }
        self.mailbox.search.menu_selection = Some(match self.mailbox.search.menu_selection {
            None if down => 0,
            None => len - 1,
            Some(index) if down => (index + 1) % len,
            Some(index) => (index + len - 1) % len,
        });
        cx.stop_propagation();
        cx.notify();
    }

    fn render_mail_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let suggestions = self.mail_search_suggestions(cx);
        let MailSearchSuggestions {
            query_empty,
            contacts,
            history,
        } = suggestions;
        let selected_index = self.mailbox.search.menu_selection;
        // A full click arrives after the input's Blur event has removed this
        // deferred panel. Suggestion rows therefore accept on left mouse-down.
        let trigger = Input::new(&self.search_input)
            .flex_1()
            .min_w_0()
            .w_full()
            .cleanable(true)
            .prefix(gpui_component::Icon::new(IconName::Search).small());
        let contact_count = contacts.len();
        let mut panel = OverlayPopover::new(
            "mail-search-suggestions-scroll",
            px(0.),
            px(36.),
            px(360.),
            px(430.),
            self.mail_search_scroll.clone(),
        )
        .constrain_width()
        .vertical_padding();

        panel = self.add_search_contacts(panel, query_empty, contacts, selected_index, cx);
        panel = self.add_search_history(panel, history, contact_count, selected_index, cx);
        panel = self.add_search_operator_hints(panel, query_empty, cx);

        div()
            .id("mail-search-suggestions")
            .relative()
            .flex_1()
            .min_w_0()
            .w_full()
            .child(trigger)
            .when(self.mailbox.search.menu_open, |root| root.child(panel))
    }

    /// Messages actually navigable in the list after filters are applied.
    /// Collapsed sections deliberately provide no
    /// cible clavier.
    fn filtered_message_headers(&self) -> Vec<&MessageHeader> {
        let tag_filter_keys: HashMap<_, Vec<String>> = self
            .mailbox
            .tag_filters
            .iter()
            .map(|(account_id, tag_ids)| {
                let provider = self
                    .account(account_id)
                    .map(|account| account.provider)
                    .unwrap_or_default();
                let known_tags = self.tags_by_account.get(account_id);
                let keys = tag_ids
                    .iter()
                    .map(|tag_id| {
                        known_tags
                            .and_then(|tags| tags.iter().find(|tag| &tag.id == tag_id))
                            .map(|tag| util::tag_storage_key(provider, tag))
                            .unwrap_or_else(|| tag_id.clone())
                    })
                    .collect();
                (account_id.clone(), keys)
            })
            .collect();
        let flagged_only = self.mailbox.show_flagged_only;
        let snoozed_only = self.mailbox.show_snoozed_only;
        let filtering_tags = !self.mailbox.tag_filters.is_empty();
        self.mailbox
            .search
            .results
            .as_ref()
            .unwrap_or(&self.mailbox.messages)
            .iter()
            // A message put off until later is hidden from every listing —
            // that is what putting it off means — and the "snoozed" filter is
            // the one place it shows, which is also where its deadline reads.
            .filter(|message| {
                self.settings
                    .snoozed_until(&message.account_id, &message.id)
                    .is_some()
                    == snoozed_only
            })
            .filter(|message| !flagged_only || message.is_flagged)
            .filter(|message| {
                !filtering_tags
                    || tag_filter_keys
                        .get(&message.account_id)
                        .is_some_and(|keys| keys.iter().all(|key| message.tags.contains(key)))
            })
            .collect()
    }

    fn visible_message_references(&self) -> Vec<MessageRef> {
        self.filtered_message_headers()
            .into_iter()
            .map(|message| MessageRef {
                account_id: message.account_id.clone(),
                id: message.id.clone(),
            })
            .collect()
    }

    pub(crate) fn clear_message_selection(&mut self) {
        self.mailbox.selected_messages.clear();
        self.mailbox.selection_anchor = None;
    }

    fn toggle_message_selection(&mut self, reference: MessageRef, checked: bool) {
        if checked {
            self.mailbox.selected_messages.insert(reference.clone());
            self.mailbox.selection_anchor = Some(reference);
        } else {
            self.mailbox.selected_messages.remove(&reference);
            if self.mailbox.selection_anchor.as_ref() == Some(&reference) {
                self.mailbox.selection_anchor = None;
            }
        }
    }

    /// Expands or collapses a conversation group, and rebuilds the list: the
    /// members appearing or disappearing changes its entry count.
    fn toggle_conversation_expanded(&mut self, key: (AccountId, String)) {
        if !self.mailbox.expanded_conversations.insert(key.clone()) {
            self.mailbox.expanded_conversations.remove(&key);
        }
        self.invalidate_message_list();
    }

    /// Selects or clears a conversation's **loaded** members. Selecting the
    /// whole thread would queue actions against messages the user cannot see.
    fn toggle_conversation_selection(&mut self, members: &[MessageRef], checked: bool) {
        for member in members {
            self.toggle_message_selection(member.clone(), checked);
        }
    }

    /// Message references in the order the list actually shows them.
    ///
    /// Grouping moves a thread's older messages up under its newest one, so
    /// the flat header order no longer matches the screen. A Shift+click
    /// range has to follow what the user sees, otherwise it covers rows that
    /// were never between the two clicks. A collapsed group contributes its
    /// members here: selecting it selects them.
    fn ordered_visible_message_references(&self) -> Vec<MessageRef> {
        let mut references = Vec::new();
        for entry in self.current_message_list_entries().iter() {
            match entry {
                MsgEntry::Header { .. } => {}
                MsgEntry::Row { reference, .. } => references.push(reference.clone()),
                // A collapsed group stands in for its whole thread; an
                // expanded one only for its newest message, the rest arriving
                // as indented `Row`s just after.
                MsgEntry::Group {
                    reference, group, ..
                } => {
                    if group.expanded {
                        references.push(reference.clone());
                    } else {
                        references.extend(group.members.iter().cloned());
                    }
                }
            }
        }
        references
    }

    fn select_message_range(&mut self, target: MessageRef) {
        let visible = self.ordered_visible_message_references();
        let Some(target_index) = visible.iter().position(|reference| reference == &target) else {
            return;
        };
        let anchor_index = self
            .mailbox
            .selection_anchor
            .as_ref()
            .and_then(|anchor| visible.iter().position(|reference| reference == anchor))
            .unwrap_or(target_index);
        let (start, end) = if anchor_index <= target_index {
            (anchor_index, target_index)
        } else {
            (target_index, anchor_index)
        };
        self.mailbox
            .selected_messages
            .extend(visible[start..=end].iter().cloned());
        if self.mailbox.selection_anchor.is_none() {
            self.mailbox.selection_anchor = Some(target);
        }
    }

    pub(crate) fn select_all_visible_messages(&mut self) {
        let visible = self.visible_message_references();
        self.mailbox
            .selected_messages
            .extend(visible.iter().cloned());
        self.mailbox.selection_anchor = visible.first().cloned();
    }

    /// Reference plus read/flagged state of every selected message.
    ///
    /// The bulk toolbar rebuilds its buttons on every frame and needs nothing
    /// else; copying whole headers there made merely looking at a large
    /// selection cost a few thousand string allocations per frame.
    fn selected_message_states(&self) -> Vec<(MessageRef, bool, bool)> {
        self.message_states_where(|reference| self.mailbox.selected_messages.contains(reference))
    }

    /// Same, for an arbitrary set of messages — a conversation's members when
    /// its collapsed row's menu acts on the thread. Listing order, and what is
    /// no longer loaded is skipped: an action can only reach what is on screen.
    pub(crate) fn message_states_where(
        &self,
        wanted: impl Fn(&MessageRef) -> bool,
    ) -> Vec<(MessageRef, bool, bool)> {
        self.filtered_message_headers()
            .into_iter()
            .filter_map(|message| {
                let reference = MessageRef {
                    account_id: message.account_id.clone(),
                    id: message.id.clone(),
                };
                wanted(&reference).then_some((reference, message.is_read, message.is_flagged))
            })
            .collect()
    }

    pub(crate) fn selected_message_headers(&self) -> Vec<MessageHeader> {
        self.filtered_message_headers()
            .into_iter()
            .filter(|message| {
                self.mailbox.selected_messages.contains(&MessageRef {
                    account_id: message.account_id.clone(),
                    id: message.id.clone(),
                })
            })
            .cloned()
            .collect()
    }

    /// Actual navigation order, with pinned messages first and collapsed
    /// sections excluded. Automatic opening after deletion must follow the
    /// same order as j/k and the arrow keys.
    fn navigable_message_targets(&self) -> Vec<(usize, &MessageHeader)> {
        // Resolving each entry through the linear `message_header_for_reference`
        // would scan the whole list once per row; a folder loaded a few pages
        // deep then costs a quadratic walk on every j/k press.
        let by_reference = self.message_headers_by_reference();
        let mut targets: Vec<(usize, &MessageHeader)> = Vec::new();
        for (index, entry) in self.current_message_list_entries().iter().enumerate() {
            // Every entry stands for exactly one message — an expanded group's
            // indented rows start after its summary — so j/k simply walks them.
            let reference = match entry {
                MsgEntry::Header { .. } => continue,
                MsgEntry::Row { reference, .. } | MsgEntry::Group { reference, .. } => reference,
            };
            if let Some(header) = by_reference
                .get(&(&reference.account_id, reference.id.as_str()))
                .copied()
            {
                targets.push((index, header));
            }
        }
        targets
    }

    pub(crate) fn message_neighbor_after_removal(&self, id: &str) -> Option<MessageHeader> {
        let targets = self.navigable_message_targets();
        let removed = targets.iter().position(|(_, message)| message.id == id)?;
        let neighbor = neighbor_index_after_removal(targets.len(), removed)?;
        Some(targets[neighbor].1.clone())
    }

    pub(crate) fn message_neighbor_after_bulk_removal(
        &self,
        current: &MessageRef,
        removed: &[MessageRef],
    ) -> Option<MessageHeader> {
        let targets = self.navigable_message_targets();
        let current_index = targets.iter().position(|(_, message)| {
            message.account_id == current.account_id && message.id == current.id
        })?;
        targets
            .iter()
            .skip(current_index + 1)
            .chain(targets[..current_index].iter().rev())
            .map(|(_, message)| *message)
            .find(|message| {
                !removed.iter().any(|reference| {
                    reference.account_id == message.account_id && reference.id == message.id
                })
            })
            .cloned()
    }

    pub(crate) fn navigate_messages(
        &mut self,
        movement: super::super::shortcuts::ListMovement,
        cx: &mut Context<Self>,
    ) {
        let targets = self.navigable_message_targets();

        if targets.is_empty() {
            return;
        }
        let current_id = self
            .displayed_message()
            .map(|message| message.header.id.clone())
            .or_else(|| self.mailbox.selected_id.clone());
        let current = current_id
            .as_deref()
            .and_then(|id| targets.iter().position(|(_, message)| message.id == id));
        let target = match movement {
            super::super::shortcuts::ListMovement::Previous => {
                current.map_or(0, |index| index.saturating_sub(1))
            }
            super::super::shortcuts::ListMovement::Next => {
                current.map_or(0, |index| (index + 1).min(targets.len() - 1))
            }
            super::super::shortcuts::ListMovement::First => 0,
            super::super::shortcuts::ListMovement::Last => targets.len() - 1,
        };
        let (entry_index, account_id, message_id) = {
            let (entry_index, message) = targets[target];
            (entry_index, message.account_id.clone(), message.id.clone())
        };
        self.scrolls.messages.motion.cancel();
        self.scrolls
            .messages
            .handle
            .scroll_to_item(entry_index, ScrollStrategy::Center);
        self.open_message_debounced(account_id, message_id, cx);
    }

    // ------------------------------------------------------------
    // Message list
    // ------------------------------------------------------------

    /// Whether the message list currently groups by conversation.
    ///
    /// Search stays flat on purpose: a search looks for one message, and
    /// folding the hits into threads would hide the very row that matched.
    pub(crate) fn conversation_grouping_active(&self) -> bool {
        self.settings.global.group_by_conversation && self.mailbox.search.results.is_none()
    }

    fn message_list_items<'a>(&self, headers: &[&'a MessageHeader]) -> Vec<MessageListItem<'a>> {
        if !self.conversation_grouping_active() {
            return headers
                .iter()
                .copied()
                .map(MessageListItem::Single)
                .collect();
        }
        group_message_items(
            headers,
            &self.mailbox.expanded_conversations,
            &self.mailbox.conversation_totals,
            &|message| self.is_message_pinned(message),
        )
    }

    fn build_message_list_entries(&self) -> Vec<MsgEntry> {
        let headers = self.filtered_message_headers();
        let items = self.message_list_items(&headers);
        build_list_entries(
            &items,
            &self.mailbox.collapsed_message_sections,
            &|message| self.is_message_pinned(message),
        )
    }

    fn message_list_cache_key(&self) -> MessageListCacheKey {
        MessageListCacheKey {
            revision: self.message_list_revision,
            ui_scale: self.settings.global.ui_scale.to_bits(),
            language: self.settings.global.language,
        }
    }

    /// Entries the list is currently showing, taken from the render cache when
    /// it is still valid.
    ///
    /// Keyboard navigation and range selection need the same flattened model
    /// the last frame built, and rebuilding it means regrouping every loaded
    /// header. The cache is filled by [`Self::message_list_model`] on the
    /// render that precedes any input, so this almost always hits; rebuilding
    /// remains the correct answer when it does not.
    /// The loaded members of the collapsed conversation this message heads in
    /// the list, when that thread still holds something unread.
    ///
    /// Opening such a row is opening the thread: the row carries the thread's
    /// unread mark and its counter, so reading only its newest message would
    /// leave it bold with nothing left to read on screen. An expanded thread
    /// shows its members as their own rows — opening one is then about that
    /// message alone, and this returns `None`.
    pub(crate) fn collapsed_conversation_members(
        &self,
        account_id: &AccountId,
        message_id: &str,
    ) -> Option<Vec<MessageRef>> {
        if !self.conversation_grouping_active() {
            return None;
        }
        self.current_message_list_entries()
            .iter()
            .find_map(|entry| match entry {
                MsgEntry::Group {
                    reference, group, ..
                } if !group.expanded
                    && group.has_unread
                    && reference.account_id == *account_id
                    && reference.id == message_id =>
                {
                    Some(group.members.clone())
                }
                _ => None,
            })
    }

    fn current_message_list_entries(&self) -> Rc<Vec<MsgEntry>> {
        let key = self.message_list_cache_key();
        match self
            .message_list_cache
            .as_ref()
            .filter(|cache| cache.key == key)
        {
            Some(cache) => cache.entries.clone(),
            None => Rc::new(self.build_message_list_entries()),
        }
    }

    /// Index over the headers [`Self::message_header_for_reference`] searches,
    /// for callers that resolve more than a handful of references.
    fn message_headers_by_reference(&self) -> HashMap<(&AccountId, &str), &MessageHeader> {
        let mut index = HashMap::new();
        for message in self
            .mailbox
            .search
            .results
            .as_ref()
            .into_iter()
            .flatten()
            .chain(self.mailbox.messages.iter())
        {
            // First match wins, as the linear lookup does: search results
            // shadow the folder listing.
            index
                .entry((&message.account_id, message.id.as_str()))
                .or_insert(message);
        }
        index
    }

    fn message_list_model(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Rc<Vec<MsgEntry>>, Rc<Vec<gpui::Size<gpui::Pixels>>>) {
        let key = self.message_list_cache_key();
        if self
            .message_list_cache
            .as_ref()
            .is_some_and(|cache| cache.key == key)
        {
            let cache = self.message_list_cache.as_ref().expect("cache checked");
            return (cache.entries.clone(), cache.sizes.clone());
        }

        let entries = self.build_message_list_entries();
        // Item heights are stable for a given UI scale. Measure one
        // representative per visual variant only when the model changes —
        // keyed by `MsgEntryVariant`, so a new variant measures itself
        // instead of silently borrowing another one's height.
        let available = gpui::size(AvailableSpace::MinContent, AvailableSpace::MinContent);
        let mut measured: HashMap<MsgEntryVariant, gpui::Pixels> = HashMap::new();
        for entry in &entries {
            measured.entry(entry.variant()).or_insert_with(|| {
                let mut element = self.message_list_item(entry, true, cx);
                element.layout_as_root(available, window, cx).height
            });
        }
        let sizes: Rc<Vec<gpui::Size<gpui::Pixels>>> = Rc::new(
            entries
                .iter()
                .map(|entry| {
                    let height = measured.get(&entry.variant()).copied().unwrap_or_default();
                    gpui::size(px(0.), height)
                })
                .collect(),
        );
        let entries = Rc::new(entries);
        self.message_list_cache = Some(MessageListCache {
            key,
            entries: entries.clone(),
            sizes: sizes.clone(),
        });
        (entries, sizes)
    }

    fn render_bulk_message_toolbar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let states = self.selected_message_states();
        let references: Vec<_> = states
            .iter()
            .map(|(reference, _, _)| reference.clone())
            .collect();
        let count = references.len();
        // `states` already holds the selected messages that are *visible*, so
        // matching the visible count is the same answer as comparing the two
        // sets — without building a second list of references for it.
        let all_visible_selected = count > 0 && count == self.filtered_message_headers().len();
        let accounts: HashSet<_> = references
            .iter()
            .map(|reference| reference.account_id.clone())
            .collect();
        let single_account = (accounts.len() == 1)
            .then(|| accounts.iter().next().cloned())
            .flatten();
        let offline = references
            .iter()
            .any(|reference| self.offline_accounts.contains(&reference.account_id));
        let source_folder_id = self.mailbox.selected_folder_id.clone();
        // Only whether the move button is usable is needed here; the hierarchy
        // itself is walked when the menu opens.
        let has_move_targets = single_account
            .as_ref()
            .and_then(|account_id| self.mailbox.folders_by_account.get(account_id))
            .is_some_and(|folders| {
                super::message_menu::has_move_folder_targets(folders, source_folder_id.as_deref())
            });

        h_flex()
            .w_full()
            .min_h(super::MAIL_PANE_HEADER_HEIGHT)
            .px_2()
            .py_1p5()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("bulk-select-all-wrapper")
                    .flex_none()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        Checkbox::new("bulk-select-all")
                            .xsmall()
                            .checked(all_visible_selected)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                if *checked {
                                    this.select_all_visible_messages();
                                } else {
                                    this.clear_message_selection();
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_semibold()
                    .child(tr!("bulk-selected", { count: count })),
            )
            .children(self.bulk_state_actions(states, offline, cx))
            .children(self.bulk_destination_actions(
                references,
                has_move_targets,
                source_folder_id,
                single_account,
                offline,
                cx,
            ))
            .into_any_element()
    }

    pub(super) fn render_messages_pane(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let searching = self.mailbox.search.results.is_some();
        let flagged_only = self.mailbox.show_flagged_only;
        let filtering_tags = !self.mailbox.tag_filters.is_empty();
        // Walking every loaded header to prune the selection costs two string
        // clones per message, on a render that also runs for each frame of a
        // hover or scroll transition. With no selection there is nothing to
        // prune, which is the usual case.
        if !self.mailbox.selected_messages.is_empty() {
            let visible_references: HashSet<_> =
                self.visible_message_references().into_iter().collect();
            self.mailbox
                .selected_messages
                .retain(|reference| visible_references.contains(reference));
        }
        let bulk_selection_active = !self.mailbox.selected_messages.is_empty();

        let (entries, sizes) = self.message_list_model(window, cx);
        let initial_loading = !searching && entries.is_empty() && !self.mailbox.messages_loaded;

        let base_handle = self.scrolls.messages.handle.base_handle().clone();
        self.scrolls.messages.motion.advance(&base_handle, window);

        v_flex()
            .size_full()
            .when(!bulk_selection_active, |el| {
                el.child(self.render_message_filters(cx))
            })
            .when(bulk_selection_active, |el| {
                el.child(self.render_bulk_message_toolbar(cx))
            })
            .when(searching, |el| {
                el.child(
                    div().px_2().py_1().child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap_1p5()
                            .px_2()
                            .py_1()
                            .rounded(cx.theme().radius)
                            .border_1()
                            .border_color(cx.theme().primary)
                            .bg(cx.theme().primary.opacity(0.08))
                            .child(
                                gpui_component::Icon::new(IconName::Search)
                                    .xsmall()
                                    .flex_none()
                                    .text_color(cx.theme().primary),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .font_semibold()
                                    .child(tr!("search-results", { query: self.mailbox.search.query.clone() })),
                            )
                            .child(
                                Button::new("clear-search")
                                    .ghost()
                                    .xsmall()
                                    .flex_none()
                                    .label(tr!("search-clear"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.clear_mail_search();
                                        this.search_input.update(cx, |s, cx| {
                                            s.set_value("", window, cx);
                                        });
                                        cx.notify();
                                    })),
                            ),
                    ),
                )
            })
            .when(initial_loading, |el| {
                el.child(
                    h_flex()
                        .w_full()
                        .p_4()
                        .gap_2()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(Spinner::new().small())
                        .child(tr!("status-loading-messages")),
                )
            })
            .when(entries.is_empty() && !initial_loading, |el| {
                el.child(
                    div()
                        .p_4()
                        .text_color(cx.theme().muted_foreground)
                        .text_sm()
                        .child(if filtering_tags {
                            tr!("messages-empty-tags")
                        } else if flagged_only && searching {
                            tr!("messages-empty-flagged-results")
                        } else if flagged_only {
                            tr!("messages-empty-flagged")
                        } else if searching {
                            tr!("messages-empty-results")
                        } else {
                            tr!("messages-empty")
                        }),
                )
            })
            .when(!entries.is_empty(), |el| {
                el.child(self.render_message_list(entries.clone(), sizes.clone(), cx))
            })
            .when(!searching && self.mailbox.pagination.loading_more, |el| {
                el.child(
                    div()
                        .w_full()
                        .py_2()
                        .text_center()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(tr!("loading")),
                )
            })
    }

    fn load_more_messages(&mut self, cx: &mut Context<Self>) {
        let message_count = self.mailbox.messages.len();
        if self.mailbox.search.results.is_some()
            || !self.mailbox.pagination.has_more
            || self.mailbox.pagination.loading_more
            || self.mailbox.pagination.last_request_len == Some(message_count)
        {
            return;
        }

        self.mailbox.pagination.loading_more = true;
        self.mailbox.pagination.last_request_len = Some(message_count);
        if self.uses_unified_pagination() {
            self.send(Cmd::LoadMoreUnified {
                request_id: self.mailbox.pagination.unified_request_id,
            });
            cx.notify();
            return;
        }
        let folder = self.mailbox.selected_folder_id.clone();
        for aid in self.active_account_ids() {
            let limit = self.fetch_limit(&aid);
            self.send(Cmd::LoadMore {
                account_id: aid,
                folder_id: folder.clone(),
                skip: message_count,
                limit,
            });
        }
        cx.notify();
    }

    /// Renders a virtual-list entry (section header or message row). Also used
    /// for offscreen height measurement, so rendering must remain a constant
    /// height for each entry type.
    fn message_header_for_reference(&self, reference: &MessageRef) -> Option<&MessageHeader> {
        self.mailbox
            .search
            .results
            .as_ref()
            .into_iter()
            .flatten()
            .chain(self.mailbox.messages.iter())
            .find(|message| {
                message.account_id == reference.account_id && message.id == reference.id
            })
    }

    fn message_list_item(
        &self,
        entry: &MsgEntry,
        show_account: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match entry {
            MsgEntry::Header {
                key,
                label,
                count,
                pinned,
                collapsed,
            } => self.message_section_header(key, label.clone(), *count, *pinned, *collapsed, cx),
            MsgEntry::Row {
                reference,
                time_only,
                pinned,
                section_end,
                in_group,
            } => self.message_list_row(
                reference,
                None,
                *time_only,
                *pinned,
                *section_end,
                *in_group,
                show_account,
                cx,
            ),
            MsgEntry::Group {
                reference,
                group,
                time_only,
                pinned,
                section_end,
            } => self.message_list_row(
                reference,
                Some(group),
                *time_only,
                *pinned,
                *section_end,
                false,
                show_account,
                cx,
            ),
        }
    }

    /// Frame shared by every list row: the pinned section's border, the
    /// indent of a conversation member, and the row itself.
    #[allow(clippy::too_many_arguments)]
    fn message_list_row(
        &self,
        reference: &MessageRef,
        group: Option<&GroupRow>,
        time_only: bool,
        pinned: bool,
        section_end: bool,
        in_group: bool,
        show_account: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(header) = self.message_header_for_reference(reference) else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        div()
            .px_2()
            .when(pinned && section_end, |el| el.pb_0p5())
            .child(
                div()
                    .relative()
                    .pt_0p5()
                    .when(pinned, |el| {
                        el.bg(theme.warning.opacity(0.1))
                            .px_0p5()
                            .border_l_1()
                            .border_r_1()
                            .border_color(theme.warning)
                    })
                    .when(pinned && section_end, |el| {
                        el.rounded_bl(theme.radius)
                            .rounded_br(theme.radius)
                            .pb_0p5()
                            .border_b_1()
                            .border_color(theme.warning)
                    })
                    .child(
                        div()
                            // Members of an expanded thread are set in from
                            // the group they belong to, with a rule joining
                            // them so the nesting reads at a glance.
                            .when(in_group, |el| {
                                el.pl_4().border_l_2().border_color(theme.border).ml_2p5()
                            })
                            .child(self.message_row_inner(
                                header,
                                show_account,
                                time_only,
                                "mailbox",
                                group,
                                cx,
                            )),
                    ),
            )
            .into_any_element()
    }

    fn message_section_header(
        &self,
        key: &str,
        label: String,
        count: usize,
        pinned: bool,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let section_key = key.to_string();

        div()
            .px_2()
            .pt_2()
            .when(pinned && collapsed, |el| el.pb_0p5())
            .child(
                h_flex()
                    .id(gpui::ElementId::Name(
                        format!("message-section-{key}").into(),
                    ))
                    .gap_1p5()
                    .items_center()
                    .relative()
                    .px_2()
                    .py_1p5()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .text_sm()
                    .font_semibold()
                    .when(!pinned, |el| {
                        el.hover(|s| s.bg(theme.list_hover)).child(
                            div()
                                .absolute()
                                .top(px(-5.))
                                .left_0()
                                .right_0()
                                .h(px(1.))
                                .bg(theme.border),
                        )
                    })
                    .when(pinned, |el| {
                        el.bg(theme.warning.opacity(0.1))
                            .border_t_1()
                            .border_l_1()
                            .border_r_1()
                            .border_color(theme.warning)
                            .hover(|s| s.bg(theme.warning.opacity(0.16)))
                    })
                    .when(pinned && collapsed, |el| el.border_b_1())
                    .when(pinned && !collapsed, |el| {
                        el.rounded_bl(px(0.)).rounded_br(px(0.))
                    })
                    .child(
                        gpui_component::Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .xsmall()
                        .text_color(theme.muted_foreground),
                    )
                    .when(pinned, |el| {
                        el.child(
                            crate::ui::icons::app_icon("pin")
                                .xsmall()
                                .text_color(theme.warning),
                        )
                    })
                    .child(div().flex_1().child(label))
                    .child(
                        div()
                            .text_xs()
                            .font_normal()
                            .text_color(theme.muted_foreground)
                            .child(count.to_string()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this
                            .mailbox
                            .collapsed_message_sections
                            .insert(section_key.clone())
                        {
                            this.mailbox.collapsed_message_sections.remove(&section_key);
                        }
                        this.invalidate_message_list();
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    pub(crate) fn message_row(
        &self,
        m: &MessageHeader,
        show_account: bool,
        context_scope: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Sender history, Contacts and the kanban reuse this row and stay
        // flat: `None` is what keeps grouping out of them.
        self.message_row_inner(m, show_account, false, context_scope, None, cx)
    }

    fn message_row_inner(
        &self,
        m: &MessageHeader,
        show_account: bool,
        time_only: bool,
        context_scope: &'static str,
        group: Option<&GroupRow>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let aid = m.account_id.clone();
        let mid = m.id.clone();
        let motion_key = (context_scope, aid.clone(), mid.clone());
        let hover = self.message_row_hover.value(&motion_key);
        let reference = MessageRef {
            account_id: aid.clone(),
            id: mid.clone(),
        };
        // A collapsed group stands in for its members, so it has to look
        // selected when any of them is the one being read.
        let selected = match group {
            Some(group) => group.members.iter().any(|member| {
                member.account_id == aid
                    && self.mailbox.selected_id.as_deref() == Some(member.id.as_str())
            }),
            None => self.mailbox.selected_id.as_deref() == Some(m.id.as_str()),
        };
        let snoozed_until = self.settings.snoozed_until(&aid, &mid);
        let bulk_selectable = context_scope == "mailbox";
        let bulk_selection_active = bulk_selectable && !self.mailbox.selected_messages.is_empty();
        // Checking a conversation checks its loaded members; the box only
        // reads as checked once every one of them is.
        let bulk_selected = bulk_selection_active
            && match group {
                Some(group) => group
                    .members
                    .iter()
                    .all(|member| self.mailbox.selected_messages.contains(member)),
                None => self.mailbox.selected_messages.contains(&reference),
            };
        let is_read = match group {
            Some(group) => !group.has_unread,
            None => m.is_read,
        };
        let is_flagged = m.is_flagged;
        let is_pinned = match group {
            Some(group) => group.pinned,
            None => self.is_message_pinned(m),
        };
        let group_members = group.map(|group| group.members.clone());
        // Pinned mailbox rows already live inside a warning-colored section.
        // Everywhere else, give a starred message the same visual treatment
        // without adding layout-affecting borders to the virtualized row.
        let highlight_flagged = is_flagged && (!is_pinned || context_scope != "mailbox");
        let row_background = if highlight_flagged {
            theme
                .warning
                .opacity(0.1)
                .lerp(theme.warning.opacity(0.16), hover)
        } else {
            theme.list_hover.opacity(0.).lerp(theme.list_hover, hover)
        };
        let entity = cx.entity();

        let account_dot = show_account.then(|| {
            let color = util::account_color(
                &aid,
                self.settings
                    .accounts
                    .get(&aid)
                    .and_then(|s| s.color_override),
            );
            div().w(px(7.)).h(px(7.)).rounded_full().bg(color)
        });

        let tags = self.row_tag_pills(m, context_scope, cx);
        let quick_actions = (!bulk_selection_active && !self.quick_actions_for(&aid).is_empty())
            .then(|| {
                self.render_quick_action_controls(
                    &aid,
                    &mid,
                    context_scope,
                    hover > 0.15,
                    false,
                    cx,
                )
            });

        let row = div()
            .id(gpui::ElementId::Name(
                format!("msg-{context_scope}-{}-{}", aid.0, m.id).into(),
            ))
            .flex()
            .flex_col()
            .relative()
            .gap_0p5()
            .px_2()
            .py_1p5()
            .rounded(theme.radius)
            .cursor_pointer()
            .bg(row_background)
            .when(selected || bulk_selected, |el| el.bg(theme.list_active))
            .when(highlight_flagged, |el| {
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .left_0()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.warning),
                )
            })
            .when(!is_read, |el| {
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_1()
                        .bottom_1()
                        .w(px(2.))
                        .rounded(px(1.))
                        .bg(theme.primary),
                )
            })
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    // The chevron is the only way to expand a thread in
                    // place: clicking anywhere else on the row opens its
                    // newest message.
                    .when_some(group.cloned(), |row, group| {
                        let expanded = group.expanded;
                        row.child(
                            div()
                                .id(gpui::ElementId::Name(
                                    format!("conversation-toggle-{}-{}", aid.0, group.key.1).into(),
                                ))
                                .flex_none()
                                .cursor_pointer()
                                .child(
                                    gpui_component::Icon::new(if expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .xsmall()
                                    .text_color(theme.muted_foreground),
                                )
                                .on_click({
                                    let entity = entity.clone();
                                    let key = group.key.clone();
                                    move |_, _, cx| {
                                        cx.stop_propagation();
                                        entity.update(cx, |this, cx| {
                                            this.toggle_conversation_expanded(key.clone());
                                            cx.notify();
                                        });
                                    }
                                }),
                        )
                    })
                    .when(bulk_selection_active, |row| {
                        row.child(
                            div()
                                .id(gpui::ElementId::Name(
                                    format!("message-select-wrapper-{}-{}", aid.0, m.id).into(),
                                ))
                                .flex_none()
                                .on_click(|_, _, cx| cx.stop_propagation())
                                .child(
                                    Checkbox::new(gpui::ElementId::Name(
                                        format!("message-select-{}-{}", aid.0, m.id).into(),
                                    ))
                                    .xsmall()
                                    .checked(bulk_selected)
                                    .on_click({
                                        let entity = entity.clone();
                                        let reference = reference.clone();
                                        let group_members = group_members.clone();
                                        move |checked: &bool, _, cx| {
                                            entity.update(cx, |this, cx| {
                                                match &group_members {
                                                    Some(members) => this
                                                        .toggle_conversation_selection(
                                                            members, *checked,
                                                        ),
                                                    None => this.toggle_message_selection(
                                                        reference.clone(),
                                                        *checked,
                                                    ),
                                                }
                                                cx.notify();
                                            });
                                        }
                                    }),
                                ),
                        )
                    })
                    .children(account_dot)
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_sm()
                            .when(!is_read, |el| el.font_semibold())
                            .child(util::display_name(&m.from)),
                    )
                    // Reply/forward arrow, as in Outlook.
                    .when_some(m.last_action, |el, action| {
                        el.child(
                            crate::ui::icons::app_icon(match action {
                                crate::model::LastAction::Forwarded => "forward",
                                crate::model::LastAction::Replied
                                | crate::model::LastAction::RepliedAll => "reply",
                            })
                            .xsmall()
                            .text_color(theme.muted_foreground),
                        )
                    })
                    .when(m.has_attachments, |el| {
                        el.child(crate::ui::icons::app_icon("paperclip").xsmall())
                    })
                    // A pending deadline replaces the received date rather than
                    // sitting next to it: the row's height is keyed by
                    // `MsgEntryVariant`, and one more element in this column
                    // would be a visual variant nothing measures. It only ever
                    // shows under the "snoozed" filter anyway — everywhere else
                    // a put-off message is not in the list at all — where when
                    // it comes back is the column worth reading.
                    .child(
                        div()
                            .text_xs()
                            .text_color(if snoozed_until.is_some() {
                                theme.warning
                            } else {
                                theme.muted_foreground
                            })
                            .child(match snoozed_until {
                                Some(until) => {
                                    crate::ui::snooze::deadline_label(until, chrono::Local::now())
                                }
                                None if time_only => util::short_time(&m.received),
                                None => util::short_date(&m.received),
                            }),
                    )
                    .children(quick_actions)
                    .child(
                        div()
                            .id(gpui::ElementId::Name(format!("pin-top-{}", m.id).into()))
                            .cursor_pointer()
                            .child(
                                crate::ui::icons::app_icon(if is_pinned {
                                    "pin"
                                } else {
                                    "pin-off"
                                })
                                .xsmall()
                                .text_color(if is_pinned {
                                    theme.warning
                                } else {
                                    theme.muted_foreground
                                }),
                            )
                            .on_click({
                                let entity = entity.clone();
                                let aid = aid.clone();
                                let mid = mid.clone();
                                let group_members = group_members.clone();
                                move |_, _, cx| {
                                    cx.stop_propagation();
                                    entity.update(cx, |this, cx| {
                                        match &group_members {
                                            Some(members) => {
                                                this.set_conversation_pinned(members, !is_pinned)
                                            }
                                            None => this.set_message_pinned(&aid, &mid, !is_pinned),
                                        }
                                        cx.notify();
                                    });
                                }
                            }),
                    )
                    .child(
                        div()
                            .id(gpui::ElementId::Name(format!("star-{}", m.id).into()))
                            .cursor_pointer()
                            .child(
                                gpui_component::Icon::new(if is_flagged {
                                    IconName::Star
                                } else {
                                    IconName::StarOff
                                })
                                .xsmall()
                                .text_color(if is_flagged {
                                    theme.warning
                                } else {
                                    theme.muted_foreground
                                }),
                            )
                            .on_click({
                                let entity = entity.clone();
                                let aid = aid.clone();
                                let mid = mid.clone();
                                move |_, window, cx| {
                                    cx.stop_propagation();
                                    entity.update(cx, |this, cx| {
                                        if this.offline_accounts.contains(&aid) {
                                            return;
                                        }
                                        this.set_flag_undoable(
                                            MessageRef {
                                                account_id: aid.clone(),
                                                id: mid.clone(),
                                            },
                                            !is_flagged,
                                            is_flagged,
                                            window,
                                            cx,
                                        );
                                        cx.notify();
                                    });
                                }
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .truncate()
                            .when(!is_read, |el| el.font_semibold())
                            .child(if m.subject.is_empty() {
                                tr!("no-subject").to_string()
                            } else {
                                m.subject.clone()
                            }),
                    )
                    // Thread size. It counts what the *cache* knows, not the
                    // page on screen, so it does not creep upward as the user
                    // scrolls into older messages of the same thread.
                    .when_some(group.map(|group| group.total), |el, total| {
                        el.child(
                            div()
                                .flex_none()
                                .px_1p5()
                                .rounded_full()
                                .bg(theme.secondary)
                                .text_xs()
                                .text_color(theme.secondary_foreground)
                                .child(total.to_string()),
                        )
                    }),
            )
            .child(
                h_flex().gap_1().items_center().children(tags).child(
                    div()
                        .flex_1()
                        .text_xs()
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(util::clean_preview(&m.preview)),
                ),
            )
            .with_hover_motion(cx, motion_key, |this| &mut this.message_row_hover)
            .on_click({
                let entity = entity.clone();
                let aid = aid.clone();
                let mid = mid.clone();
                let reference = reference.clone();
                move |event: &gpui::ClickEvent, window, cx| {
                    entity.update(cx, |this, cx| {
                        this.focus_shortcuts(window);
                        let modifiers = event.modifiers();
                        if bulk_selectable && modifiers.shift {
                            this.select_message_range(reference.clone());
                            cx.notify();
                            return;
                        }
                        if bulk_selectable && modifiers.secondary() {
                            let checked = !this.mailbox.selected_messages.contains(&reference);
                            this.toggle_message_selection(reference.clone(), checked);
                            cx.notify();
                            return;
                        }
                        this.clear_message_selection();
                        this.mailbox.selection_anchor = Some(reference.clone());
                        this.open_message(aid.clone(), mid.clone(), cx);
                    });
                }
            });

        // A collapsed group's menu acts on the thread; expanded, its members
        // are rows of their own and the summary row is about its own message.
        let menu_thread = group
            .filter(|group| !group.expanded)
            .map(|group| group.members.as_slice());
        let context_menu =
            row.context_menu(self.message_row_menu(m, is_read, is_pinned, menu_thread, cx));

        // `ContextMenuExt` uses the constant internal ID `context-menu`.
        // Without a row-identified ancestor, gpui reuses the same element state
        // for several messages: all open popovers overlap, making the shadow
        // extremely heavy and slowing hover. This host makes the global ID
        // path unique.
        div()
            .id(gpui::ElementId::Name(
                format!("msg-context-host-{context_scope}-{}", m.id).into(),
            ))
            .w_full()
            .child(context_menu)
    }

    fn row_tag_pills(
        &self,
        m: &MessageHeader,
        context_scope: &'static str,
        cx: &Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        if m.tags.is_empty() {
            return Vec::new();
        }
        let entity = cx.entity();
        let account_id = m.account_id.clone();
        let filter_count: usize = self
            .mailbox
            .tag_filters
            .values()
            .map(|filters| filters.len())
            .sum();
        let provider = self
            .account(&m.account_id)
            .map(|a| a.provider)
            .unwrap_or(Provider::Microsoft);
        let tags = self.tags_by_account.get(&m.account_id);
        m.tags
            .iter()
            .take(3)
            .map(|key| {
                let (label, color, tag_id) = tags
                    .and_then(|list| {
                        list.iter()
                            .find(|t| &util::tag_storage_key(provider, t) == key)
                    })
                    .map(|t| {
                        (
                            t.display_name.clone(),
                            super::super::tag_menu::tag_color(&t.display_name, t.color),
                            t.id.clone(),
                        )
                    })
                    .unwrap_or_else(|| (key.clone(), util::name_color(key), key.clone()));
                let selected = self
                    .mailbox
                    .tag_filters
                    .get(&account_id)
                    .is_some_and(|filters| filters.contains(&tag_id));
                let only_filter = selected && filter_count == 1;
                let entity = entity.clone();
                let account_id = account_id.clone();
                div()
                    .id(gpui::ElementId::Name(
                        format!("tag-filter-{context_scope}-{}-{tag_id}", m.id).into(),
                    ))
                    .px_1p5()
                    .rounded_full()
                    .text_xs()
                    .cursor_pointer()
                    .bg(color.opacity(if selected { 0.45 } else { 0.25 }))
                    .when(selected, |pill| pill.border_1().border_color(color))
                    .text_color(cx.theme().foreground)
                    .child(label)
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        entity.update(cx, |this, cx| {
                            this.mailbox.tag_filters.clear();
                            if !only_filter {
                                this.mailbox
                                    .tag_filters
                                    .entry(account_id.clone())
                                    .or_default()
                                    .insert(tag_id.clone());
                            }
                            this.invalidate_message_list();
                            this.enter_main_view(MainView::Mail, cx);
                        });
                    })
                    .into_any_element()
            })
            .collect()
    }

    /// Contact suggestions of the search menu: the address book filtered by
    /// what has been typed, ranked by how often each recipient is used.
    fn add_search_contacts(
        &self,
        mut panel: OverlayPopover,
        query_empty: bool,
        contacts: Vec<Contact>,
        selected_index: Option<usize>,
        cx: &mut Context<Self>,
    ) -> OverlayPopover {
        let theme = cx.theme().clone();
        let content_app = cx.entity();
        panel = panel.child(
            h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .px_3()
                .py_1p5()
                .text_xs()
                .font_semibold()
                .text_color(theme.muted_foreground)
                .child(
                    gpui_component::Icon::new(IconName::User)
                        .xsmall()
                        .text_color(theme.muted_foreground),
                )
                .child(tr!("search-menu-contacts")),
        );

        if query_empty {
            panel = panel.child(
                div()
                    .px_3()
                    .pb_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tr!("search-menu-contact-hint")),
            );
        } else if contacts.is_empty() {
            panel = panel.child(
                div()
                    .px_3()
                    .pb_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tr!("search-menu-no-contacts")),
            );
        } else {
            for (index, contact) in contacts.iter().cloned().enumerate() {
                let selected = selected_index == Some(index);
                let Contact { name, email, .. } = contact;
                let label = if name.trim().is_empty() {
                    email.clone()
                } else {
                    name
                };
                let show_email = !label.eq_ignore_ascii_case(&email);
                let search_query = email.clone();
                let row_app = content_app.clone();
                panel = panel.child(
                    h_flex()
                        .id(("mail-search-contact", index))
                        .w_full()
                        .min_w_0()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .cursor_pointer()
                        .when(selected, |row| row.bg(theme.list_active))
                        .when(!selected, |row| {
                            row.hover(|style| style.bg(theme.list_hover))
                        })
                        .child(Avatar::new().name(label.clone()).small())
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().truncate().text_sm().font_medium().child(label))
                                .when(show_email, |el| {
                                    el.child(
                                        div()
                                            .truncate()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(email.clone()),
                                    )
                                }),
                        )
                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                            row_app.update(cx, |this, cx| {
                                this.choose_search_suggestion(search_query.clone(), window, cx);
                            });
                            cx.stop_propagation();
                        }),
                );
            }
        }
        panel
    }

    /// Recent searches, newest first, with the entry that clears them.
    fn add_search_history(
        &self,
        mut panel: OverlayPopover,
        history: Vec<String>,
        contact_count: usize,
        selected_index: Option<usize>,
        cx: &mut Context<Self>,
    ) -> OverlayPopover {
        let theme = cx.theme().clone();
        let content_app = cx.entity();
        let clear_app = content_app.clone();
        panel = panel.child(
            h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .mt_1()
                .px_3()
                .pt_2()
                .pb_1()
                .border_t_1()
                .border_color(theme.border)
                .text_xs()
                .font_semibold()
                .text_color(theme.muted_foreground)
                .child(
                    crate::ui::icons::app_icon("clock")
                        .xsmall()
                        .text_color(theme.muted_foreground),
                )
                .child(tr!("search-menu-history"))
                .child(div().flex_1())
                .when(!history.is_empty(), |el| {
                    el.child(
                        Button::new("clear-mail-search-history")
                            .ghost()
                            .xsmall()
                            .label(tr!("search-menu-history-clear"))
                            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                                clear_app.update(cx, |this, cx| {
                                    this.mailbox.search.history.clear();
                                    this.mailbox.search.menu_selection = None;
                                    cx.notify();
                                });
                                cx.stop_propagation();
                            }),
                    )
                }),
        );

        if history.is_empty() {
            panel = panel.child(
                div()
                    .px_3()
                    .pb_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tr!("search-menu-history-empty")),
            );
        } else {
            for (index, query) in history.iter().cloned().enumerate() {
                let selected = selected_index == Some(contact_count + index);
                let row_app = content_app.clone();
                let search_query = query.clone();
                panel = panel.child(
                    h_flex()
                        .id(("mail-search-history", index))
                        .w_full()
                        .min_w_0()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .cursor_pointer()
                        .when(selected, |row| row.bg(theme.list_active))
                        .when(!selected, |row| {
                            row.hover(|style| style.bg(theme.list_hover))
                        })
                        .child(
                            crate::ui::icons::app_icon("clock")
                                .small()
                                .text_color(theme.muted_foreground),
                        )
                        .child(div().flex_1().min_w_0().truncate().text_sm().child(query))
                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                            row_app.update(cx, |this, cx| {
                                this.choose_search_suggestion(search_query.clone(), window, cx);
                            });
                            cx.stop_propagation();
                        }),
                );
            }
        }

        // Scope belongs in the panel rather than beside the field: it is a
        // habit, set once, and a permanent control would crowd a pane that is
        // often narrow.
        {
            let current = self.mailbox.search.scope;
            let folder_label = self
                .mailbox
                .selected_folder_id
                .as_deref()
                .and_then(|id| {
                    self.mailbox
                        .folders_by_account
                        .values()
                        .flatten()
                        .find(|folder| folder.id == id)
                        .map(super::folders::folder_display_label)
                })
                .unwrap_or_else(|| tr!("folder-inbox"));
            panel = panel.child(
                v_flex()
                    .w_full()
                    .border_t_1()
                    .border_color(theme.border)
                    .pt_1p5()
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .font_semibold()
                            .text_color(theme.muted_foreground)
                            .child(
                                gpui_component::Icon::new(IconName::Folder)
                                    .xsmall()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(tr!("search-menu-scope")),
                    )
                    .child(
                        h_flex().w_full().px_3().pb_1().gap_1().children(
                            [
                                (
                                    MailSearchSort::Relevance,
                                    tr!("search-sort-relevance"),
                                    "search-sort-relevance",
                                ),
                                (
                                    MailSearchSort::Date,
                                    tr!("search-sort-date"),
                                    "search-sort-date",
                                ),
                            ]
                            .map(|(sort, label, id)| {
                                let current_sort = self.mailbox.search.sort;
                                Button::new(id)
                                    .xsmall()
                                    .label(label)
                                    .when(current_sort == sort, |button| button.primary())
                                    .when(current_sort != sort, |button| button.ghost())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.set_mail_search_sort(sort, window, cx);
                                    }))
                            }),
                        ),
                    )
                    .child(
                        h_flex().w_full().px_3().pb_2().gap_1().children(
                            [
                                (
                                    MailSearchScope::Everywhere,
                                    tr!("search-scope-everywhere"),
                                    "search-scope-everywhere",
                                ),
                                (
                                    MailSearchScope::Folder,
                                    tr!("search-scope-folder", { folder: folder_label.clone() }),
                                    "search-scope-folder",
                                ),
                            ]
                            .map(|(scope, label, id)| {
                                Button::new(id)
                                    .xsmall()
                                    .label(label)
                                    .when(current == scope, |button| button.primary())
                                    .when(current != scope, |button| button.ghost())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.set_mail_search_scope(scope, window, cx);
                                    }))
                            }),
                        ),
                    ),
            );
        }

        // Operators are invisible unless they are shown somewhere. The hint
        // only occupies the panel while the field is empty, so it never gets
        panel
    }

    /// Operator reminder, shown only while nothing has been typed: an operator
    /// nobody knows about is an operator nobody uses.
    fn add_search_operator_hints(
        &self,
        mut panel: OverlayPopover,
        query_empty: bool,
        cx: &mut Context<Self>,
    ) -> OverlayPopover {
        let theme = cx.theme().clone();
        // Operators are invisible unless they are shown somewhere. The hint
        // only occupies the panel while the field is empty, so it never gets
        // in the way of contacts or history once typing starts.
        if query_empty {
            panel = panel.child(
                v_flex()
                    .w_full()
                    .border_t_1()
                    .border_color(theme.border)
                    .pt_1p5()
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .font_semibold()
                            .text_color(theme.muted_foreground)
                            .child(
                                gpui_component::Icon::new(IconName::Search)
                                    .xsmall()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(tr!("search-menu-operators")),
                    )
                    .child(
                        div()
                            .px_3()
                            .pb_2()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(tr!("search-menu-operators-hint")),
                    ),
            );
        }
        panel
    }

    /// Read and flag state of the selection. Each button applies to every
    /// selected message through the same undoable path a single row uses.
    /// `states` carries `(reference, is_read, is_flagged)` for each selected
    /// message — the only three fields these four buttons read.
    fn bulk_state_actions(
        &self,
        states: Vec<(MessageRef, bool, bool)>,
        offline: bool,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        vec![
            Button::new("bulk-mark-read")
                .ghost()
                .xsmall()
                .icon(crate::ui::icons::app_icon("mail-open"))
                .tooltip(tr!("bulk-mark-read"))
                .disabled(offline)
                .on_click({
                    let states = states.clone();
                    cx.listener(move |this, _, window, cx| {
                        let items = states
                            .iter()
                            .map(|(reference, read, _)| (reference.clone(), *read))
                            .collect();
                        this.bulk_mark_read_undoable(items, true, window, cx);
                        cx.notify();
                    })
                })
                .into_any_element(),
            Button::new("bulk-mark-unread")
                .ghost()
                .xsmall()
                .icon(crate::ui::icons::app_icon("mail"))
                .tooltip(tr!("bulk-mark-unread"))
                .disabled(offline)
                .on_click({
                    let states = states.clone();
                    cx.listener(move |this, _, window, cx| {
                        let items = states
                            .iter()
                            .map(|(reference, read, _)| (reference.clone(), *read))
                            .collect();
                        this.bulk_mark_read_undoable(items, false, window, cx);
                        cx.notify();
                    })
                })
                .into_any_element(),
            Button::new("bulk-flag")
                .ghost()
                .xsmall()
                .icon(IconName::Star)
                .tooltip(tr!("bulk-flag"))
                .disabled(offline)
                .on_click({
                    let states = states.clone();
                    cx.listener(move |this, _, window, cx| {
                        let items = states
                            .iter()
                            .map(|(reference, _, flagged)| (reference.clone(), *flagged))
                            .collect();
                        this.bulk_set_flag_undoable(items, true, window, cx);
                        cx.notify();
                    })
                })
                .into_any_element(),
            Button::new("bulk-unflag")
                .ghost()
                .xsmall()
                .icon(IconName::StarOff)
                .tooltip(tr!("bulk-unflag"))
                .disabled(offline)
                .on_click({
                    let states = states.clone();
                    cx.listener(move |this, _, window, cx| {
                        let items = states
                            .iter()
                            .map(|(reference, _, flagged)| (reference.clone(), *flagged))
                            .collect();
                        this.bulk_set_flag_undoable(items, false, window, cx);
                        cx.notify();
                    })
                })
                .into_any_element(),
        ]
    }

    /// Where the selection goes: another folder, the archive, the junk folder,
    /// the bin — plus the entry that drops the selection itself.
    fn bulk_destination_actions(
        &self,
        references: Vec<MessageRef>,
        has_move_targets: bool,
        source_folder_id: Option<String>,
        single_account: Option<AccountId>,
        offline: bool,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let theme = cx.theme().clone();
        let entity = cx.entity();
        // One folder is on screen at a time, so a selection spanning accounts
        // (unified inbox, search) is never *in* a junk folder: any() reads the
        // displayed folder without needing the selection to agree on an account.
        let in_junk = references
            .iter()
            .any(|reference| self.viewing_junk_folder(&reference.account_id));
        // Every account, not any: a button that is known to fail for part of
        // the selection is worse than no button.
        let has_junk_folder = references
            .iter()
            .all(|reference| self.junk_folder_available(&reference.account_id));
        let mut actions: Vec<gpui::AnyElement> = vec![
            Button::new("bulk-move")
                .ghost()
                .xsmall()
                .icon(crate::ui::icons::app_icon("folder-open"))
                .tooltip(if single_account.is_none() {
                    tr!("bulk-move-one-account")
                } else {
                    tr!("ctx-move-to")
                })
                .disabled(offline || single_account.is_none() || !has_move_targets)
                .dropdown_menu({
                    let entity = entity.clone();
                    let references = references.clone();
                    let account_id = single_account.clone();
                    move |menu, window, cx| {
                        // Walked here rather than captured: the toolbar is
                        // rebuilt on every frame while a selection is active.
                        let targets = account_id
                            .as_ref()
                            .and_then(|account_id| {
                                entity
                                    .read(cx)
                                    .mailbox
                                    .folders_by_account
                                    .get(account_id)
                                    .map(|folders| {
                                        move_folder_targets(folders, source_folder_id.as_deref())
                                    })
                            })
                            .unwrap_or_default();
                        MoveMenu {
                            entity: entity.clone(),
                            scope: MoveScope::Selection(references.clone()),
                            source_folder_id: source_folder_id.clone(),
                            offline,
                        }
                        .add_targets(menu, targets, window, cx)
                    }
                })
                .into_any_element(),
            Button::new("bulk-archive")
                .ghost()
                .xsmall()
                .icon(crate::ui::icons::app_icon("archive"))
                .tooltip(tr!("bulk-archive"))
                .disabled(offline)
                .on_click({
                    let references = references.clone();
                    cx.listener(move |this, _, window, cx| {
                        this.bulk_archive_messages_with_undo(references.clone(), window, cx);
                        cx.notify();
                    })
                })
                .into_any_element(),
            Button::new("bulk-snooze")
                .ghost()
                .xsmall()
                .icon(crate::ui::icons::app_icon("clock"))
                .tooltip(tr!("bulk-snooze"))
                .disabled(offline)
                .dropdown_menu({
                    let entity = entity.clone();
                    let references = references.clone();
                    move |menu, _window, _cx| {
                        crate::ui::snooze::append_snooze_menu(menu, &entity, &references, offline)
                    }
                })
                .into_any_element(),
            Button::new("bulk-delete")
                .ghost()
                .xsmall()
                .icon(crate::ui::icons::app_icon("trash-2").text_color(theme.danger))
                .tooltip(tr!("bulk-delete"))
                .disabled(offline)
                .on_click({
                    let references = references.clone();
                    cx.listener(move |this, _, window, cx| {
                        this.bulk_delete_messages_with_undo(references.clone(), window, cx);
                        cx.notify();
                    })
                })
                .into_any_element(),
            Button::new("bulk-clear")
                .ghost()
                .xsmall()
                .icon(IconName::Close)
                .tooltip(tr!("bulk-clear"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.clear_message_selection();
                    cx.notify();
                }))
                .into_any_element(),
        ];
        if has_junk_folder {
            // Between archiving and deleting, which is where it belongs: it is
            // the third thing one does with a message one does not want.
            actions.insert(
                2,
                Button::new("bulk-junk")
                    .ghost()
                    .xsmall()
                    .icon(crate::ui::icons::app_icon(if in_junk {
                        "inbox"
                    } else {
                        "alert-circle"
                    }))
                    .tooltip(if in_junk {
                        tr!("bulk-not-junk")
                    } else {
                        tr!("bulk-junk")
                    })
                    .disabled(offline)
                    .on_click({
                        let references = references.clone();
                        cx.listener(move |this, _, window, cx| {
                            if in_junk {
                                this.bulk_mark_not_junk_with_undo(references.clone(), window, cx);
                            } else {
                                this.bulk_mark_junk_with_undo(references.clone(), window, cx);
                            }
                            cx.notify();
                        })
                    })
                    .into_any_element(),
            );
        }
        actions
    }

    /// Search field and list filters: unread, flagged, tags, and the
    /// conversation-grouping toggle.
    fn render_message_filters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let flagged_only = self.mailbox.show_flagged_only;
        let snoozed_only = self.mailbox.show_snoozed_only;
        let has_snoozed = self
            .settings
            .accounts
            .values()
            .any(|account| !account.snoozed_messages.is_empty());
        let filtering_tags = !self.mailbox.tag_filters.is_empty();
        let tag_filter_count: usize = self
            .mailbox
            .tag_filters
            .values()
            .map(|selected| selected.len())
            .sum();
        let tag_menus: Vec<_> = self
            .active_account_ids()
            .into_iter()
            .map(|account_id| {
                let account_label = self
                    .account(&account_id)
                    .map(|account| self.account_label(account))
                    .unwrap_or_else(|| account_id.0.clone());
                let tags = self
                    .tags_by_account
                    .get(&account_id)
                    .cloned()
                    .unwrap_or_default();
                (account_id, account_label, tags)
            })
            .collect();
        let selected_tag_color = tag_menus.iter().find_map(|(account_id, _, tags)| {
            let selected = self.mailbox.tag_filters.get(account_id)?;
            let tag = tags.iter().find(|tag| selected.contains(&tag.id))?;
            Some(super::super::tag_menu::tag_color(
                &tag.display_name,
                tag.color,
            ))
        });
        let mut tag_filter_icon = crate::ui::icons::app_icon("tag");
        if let Some(color) = selected_tag_color {
            tag_filter_icon = tag_filter_icon.text_color(color);
        }
        let offline = self
            .active_account_ids()
            .iter()
            .any(|account_id| self.offline_accounts.contains(account_id));
        h_flex()
            .w_full()
            .min_h(super::MAIL_PANE_HEADER_HEIGHT)
            .px_2()
            .py_1p5()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .when(offline, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().warning)
                        .child(tr!("offline")),
                )
            })
            .child(
                div()
                    .key_context("MailSearch")
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .capture_action(cx.listener(|this, _: &MoveUp, _window, cx| {
                        this.move_mail_search_selection(false, cx);
                    }))
                    .capture_action(cx.listener(|this, _: &MoveDown, _window, cx| {
                        this.move_mail_search_selection(true, cx);
                    }))
                    .child(self.render_mail_search(cx)),
            )
            .child(
                Button::new("filter-tags")
                    .ghost()
                    .small()
                    .selected(filtering_tags)
                    .icon(tag_filter_icon)
                    .tooltip(if tag_filter_count == 0 {
                        tr!("tags-filter")
                    } else {
                        tr!("tags-filter-active", { count: tag_filter_count })
                    })
                    .dropdown_menu({
                        let entity = cx.entity();
                        let selected = self.mailbox.tag_filters.clone();
                        move |mut menu, _window, _cx| {
                            let several_accounts = tag_menus.len() > 1;
                            let mut has_tags = false;
                            for (account_id, account_label, tags) in tag_menus.clone() {
                                if tags.is_empty() {
                                    continue;
                                }
                                if several_accounts {
                                    if has_tags {
                                        menu = menu.separator();
                                    }
                                    menu =
                                        menu.item(PopupMenuItem::new(account_label).disabled(true));
                                }
                                for tag in tags {
                                    has_tags = true;
                                    let checked = selected
                                        .get(&account_id)
                                        .is_some_and(|ids| ids.contains(&tag.id));
                                    let entity = entity.clone();
                                    let account_id = account_id.clone();
                                    let tag_id = tag.id.clone();
                                    menu = menu.item(
                                        super::super::tag_menu::tag_menu_item(
                                            tag.display_name,
                                            tag.color,
                                        )
                                        .checked(checked)
                                        .on_click(
                                            move |_, _, cx| {
                                                entity.update(cx, |this, cx| {
                                                    let filters = this
                                                        .mailbox
                                                        .tag_filters
                                                        .entry(account_id.clone())
                                                        .or_default();
                                                    if !filters.insert(tag_id.clone()) {
                                                        filters.remove(&tag_id);
                                                    }
                                                    if filters.is_empty() {
                                                        this.mailbox
                                                            .tag_filters
                                                            .remove(&account_id);
                                                    }
                                                    this.invalidate_message_list();
                                                    cx.notify();
                                                });
                                            },
                                        ),
                                    );
                                }
                            }
                            if !has_tags {
                                menu =
                                    menu.item(PopupMenuItem::new(tr!("tags-none")).disabled(true));
                            }
                            if tag_filter_count > 0 {
                                let entity = entity.clone();
                                menu = menu.separator().item(
                                    PopupMenuItem::new(tr!("tags-filter-clear")).on_click(
                                        move |_, _, cx| {
                                            entity.update(cx, |this, cx| {
                                                this.mailbox.tag_filters.clear();
                                                this.invalidate_message_list();
                                                cx.notify();
                                            });
                                        },
                                    ),
                                );
                            }
                            menu.check_side(gpui_component::Side::Right)
                        }
                    }),
            )
            .child(
                Button::new("filter-flagged")
                    .ghost()
                    .small()
                    .selected(flagged_only)
                    .icon(IconName::Star)
                    .tooltip(tr!("messages-flagged-only"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.mailbox.show_flagged_only = !this.mailbox.show_flagged_only;
                        this.invalidate_message_list();
                        cx.notify();
                    })),
            )
            // Only offered once something is actually put off: an always-on
            // filter for an empty set would be a permanent invitation into a
            // blank list. It stays while the filter is on, though — the last
            // message waking up must not take the way out with it.
            .when(has_snoozed || snoozed_only, |el| {
                el.child(
                    Button::new("filter-snoozed")
                        .ghost()
                        .small()
                        .selected(snoozed_only)
                        .icon(crate::ui::icons::app_icon("clock"))
                        .tooltip(tr!("messages-snoozed-only"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.mailbox.show_snoozed_only = !this.mailbox.show_snoozed_only;
                            this.invalidate_message_list();
                            cx.notify();
                        })),
                )
            })
    }

    /// The virtualized list itself, plus the wheel smoothing and the prepaint
    /// that preloads the next page.
    fn render_message_list(
        &self,
        entries: std::rc::Rc<Vec<MsgEntry>>,
        sizes: std::rc::Rc<Vec<gpui::Size<gpui::Pixels>>>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let show_account = true;
        let base_handle_for_top = self.scrolls.messages.handle.base_handle().clone();
        let show_scroll_to_top =
            self.scrolls.messages.motion.target_y(&base_handle_for_top) < -SCROLL_TO_TOP_THRESHOLD;
        let base_handle = self.scrolls.messages.handle.base_handle().clone();
        let app = cx.entity();
        // Non-scrollable wrapper: its wheel listener runs after
        // the virtual list's internal scroll handler (see
        // `ui/motion.rs`), and
        // prepaint is used to preload more messages.
        div()
            .relative()
            .on_children_prepainted({
                let handle = base_handle.clone();
                move |_, _, cx| {
                    // During wheel animation, the offset
                    // actual position lags behind the target, so
                    // preload relative to where it is heading.
                    let remaining = handle.max_offset().height
                        + app.read(cx).scrolls.messages.motion.target_y(&handle);
                    if remaining <= LOAD_MORE_THRESHOLD {
                        app.update(cx, |this, cx| this.load_more_messages(cx));
                    }
                }
            })
            .id("messages-scroll")
            .flex_1()
            .min_h_0()
            .on_scroll_wheel(cx.listener({
                let handle = base_handle;
                move |this, event: &ScrollWheelEvent, window, cx| {
                    // Pixel deltas (touchpads) do not use the
                    // wheel tween, but still need a fresh render
                    // when the floating shortcut crosses its
                    // visibility threshold.
                    this.scrolls
                        .messages
                        .motion
                        .on_wheel(&handle, event, window);
                    cx.notify();
                }
            }))
            .child(
                v_virtual_list(cx.entity(), "messages-vlist", sizes, {
                    let entries = entries.clone();
                    move |this, range: Range<usize>, _window, cx| {
                        range
                            .filter_map(|ix| {
                                entries
                                    .get(ix)
                                    .map(|entry| this.message_list_item(entry, show_account, cx))
                            })
                            .collect::<Vec<_>>()
                    }
                })
                .track_scroll(&self.scrolls.messages.handle)
                .pb_2(),
            )
            .when(show_scroll_to_top, |el| {
                el.child(
                    Button::new("messages-scroll-to-top")
                        .xsmall()
                        .absolute()
                        .top_2()
                        .right_3()
                        .rounded(px(999.))
                        .shadow_md()
                        .icon(crate::ui::icons::app_icon("arrow-up"))
                        .tooltip(tr!("messages-scroll-to-top"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.scrolls.messages.motion.cancel();
                            let handle = this.scrolls.messages.handle.base_handle().clone();
                            let offset = handle.offset();
                            handle.set_offset(gpui::point(offset.x, px(0.)));
                            cx.notify();
                        })),
                )
            })
    }
}

#[cfg(test)]
mod navigation_tests {
    use super::neighbor_index_after_removal;

    #[test]
    fn deletion_selects_next_then_previous_at_end_of_list() {
        assert_eq!(neighbor_index_after_removal(3, 0), Some(1));
        assert_eq!(neighbor_index_after_removal(3, 1), Some(2));
        assert_eq!(neighbor_index_after_removal(3, 2), Some(1));
        assert_eq!(neighbor_index_after_removal(1, 0), None);
    }
}

#[cfg(test)]
mod grouping_tests {
    use super::{group_message_items, MessageListItem};
    use crate::model::{AccountId, MessageHeader};
    use chrono::{TimeZone, Utc};
    use std::collections::{HashMap, HashSet};

    pub(super) fn threaded_pair() -> (MessageHeader, MessageHeader) {
        (
            message("account-a", "thread-new", Some("conversation-1"), 40),
            message("account-a", "thread-old", Some("conversation-1"), 30),
        )
    }

    pub(super) fn message(
        account: &str,
        id: &str,
        conversation: Option<&str>,
        minute: u32,
    ) -> MessageHeader {
        MessageHeader {
            id: id.into(),
            account_id: AccountId(account.into()),
            subject: "Contrat".into(),
            from: "Contact A <contact-a@example.test>".into(),
            received: Utc
                .with_ymd_and_hms(2026, 3, 15, 12, minute, 0)
                .single()
                .expect("fixed timestamp"),
            preview: String::new(),
            is_read: true,
            is_flagged: false,
            has_attachments: false,
            tags: Vec::new(),
            last_action: None,
            last_action_at: None,
            conversation_id: conversation.map(str::to_string),
            internet_message_id: None,
        }
    }

    /// Headers reach the list newest-first; grouping must preserve that order
    /// and leave each thread where its newest message stood.
    fn group(messages: &[MessageHeader]) -> Vec<MessageListItem<'_>> {
        let headers: Vec<&MessageHeader> = messages.iter().collect();
        group_message_items(&headers, &HashSet::new(), &HashMap::new(), &|_| false)
    }

    fn ids(items: &[MessageListItem<'_>]) -> Vec<Vec<String>> {
        items
            .iter()
            .map(|item| match item {
                MessageListItem::Single(message) => vec![message.id.clone()],
                MessageListItem::Group { members, .. } => {
                    members.iter().map(|member| member.id.clone()).collect()
                }
            })
            .collect()
    }

    /// A thread collapses onto the row its newest message occupied, and the
    /// messages around it keep their positions.
    #[test]
    fn a_thread_takes_the_place_of_its_newest_message() {
        let messages = [
            message("account-a", "loose-1", None, 50),
            message("account-a", "thread-new", Some("conversation-1"), 40),
            message("account-a", "loose-2", None, 30),
            message("account-a", "thread-old", Some("conversation-1"), 20),
        ];

        assert_eq!(
            ids(&group(&messages)),
            vec![
                vec!["loose-1".to_string()],
                vec!["thread-new".to_string(), "thread-old".to_string()],
                vec!["loose-2".to_string()],
            ]
        );
    }

    /// One loaded message is not a conversation: a chevron over nothing and a
    /// counter reading "1" would turn the whole mailbox into false groups.
    #[test]
    fn a_lone_message_stays_an_ordinary_row() {
        let messages = [
            message("account-a", "alone", Some("conversation-1"), 40),
            message("account-a", "no-thread", None, 30),
        ];

        let items = group(&messages);
        assert!(items
            .iter()
            .all(|item| matches!(item, MessageListItem::Single(_))));
    }

    /// Thread ids come from three unrelated providers. In a unified inbox a
    /// bare id would merge two accounts' exchanges into one row.
    #[test]
    fn identical_thread_ids_from_two_accounts_do_not_merge() {
        let messages = [
            message("account-a", "a-1", Some("conversation-1"), 40),
            message("account-b", "b-1", Some("conversation-1"), 30),
        ];

        let items = group(&messages);
        assert_eq!(items.len(), 2);
        assert!(items
            .iter()
            .all(|item| matches!(item, MessageListItem::Single(_))));
    }

    /// The counter comes from the cache, which knows messages the loaded page
    /// does not — but never reports fewer than what is on screen.
    #[test]
    fn the_counter_prefers_the_cache_without_ever_undercounting() {
        let messages = [
            message("account-a", "thread-new", Some("conversation-1"), 40),
            message("account-a", "thread-old", Some("conversation-1"), 30),
        ];
        let headers: Vec<&MessageHeader> = messages.iter().collect();
        let key = (AccountId("account-a".into()), "conversation-1".to_string());

        let totals = HashMap::from([(key.clone(), 12)]);
        let items = group_message_items(&headers, &HashSet::new(), &totals, &|_| false);
        let MessageListItem::Group { row, .. } = &items[0] else {
            panic!("expected a group");
        };
        assert_eq!(row.total, 12);

        // A stale count below the loaded page must not hide loaded rows.
        let totals = HashMap::from([(key, 1)]);
        let items = group_message_items(&headers, &HashSet::new(), &totals, &|_| false);
        let MessageListItem::Group { row, .. } = &items[0] else {
            panic!("expected a group");
        };
        assert_eq!(row.total, 2);
    }

    /// Unread and pinned are thread-level states: one member is enough. This
    /// is what puts a whole conversation in the pinned section (P1-2 Q4).
    #[test]
    fn unread_and_pinned_propagate_from_any_member() {
        let mut messages = [
            message("account-a", "thread-new", Some("conversation-1"), 40),
            message("account-a", "thread-old", Some("conversation-1"), 30),
        ];
        messages[1].is_read = false;
        let headers: Vec<&MessageHeader> = messages.iter().collect();

        let items = group_message_items(&headers, &HashSet::new(), &HashMap::new(), &|message| {
            message.id == "thread-old"
        });
        let MessageListItem::Group { row, .. } = &items[0] else {
            panic!("expected a group");
        };
        assert!(row.has_unread, "an unread reply marks the whole thread");
        assert!(row.pinned, "a pinned member pins the whole thread");
    }
}

#[cfg(test)]
mod entry_tests {
    use super::grouping_tests::{message, threaded_pair};
    use super::{build_list_entries, group_message_items, MsgEntry};
    use crate::model::{AccountId, MessageHeader, MessageRef};
    use std::collections::{HashMap, HashSet};

    fn entries(messages: &[MessageHeader], expanded: bool) -> Vec<MsgEntry> {
        let headers: Vec<&MessageHeader> = messages.iter().collect();
        let expanded: HashSet<(AccountId, String)> = if expanded {
            headers
                .iter()
                .filter_map(|header| {
                    Some((header.account_id.clone(), header.conversation_id.clone()?))
                })
                .collect()
        } else {
            HashSet::new()
        };
        let items = group_message_items(&headers, &expanded, &HashMap::new(), &|_| false);
        build_list_entries(&items, &HashSet::new(), &|_| false)
    }

    fn rendered_references(entries: &[MsgEntry]) -> Vec<MessageRef> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                MsgEntry::Header { .. } => None,
                MsgEntry::Row { reference, .. } | MsgEntry::Group { reference, .. } => {
                    Some(reference.clone())
                }
            })
            .collect()
    }

    /// Every message must render **once**. Two rows for the same message share
    /// a gpui `ElementId`, and the duplicate stops responding to hover and
    /// clicks — an expanded group's summary row already *is* its newest
    /// message, so the indented members start at the second one.
    #[test]
    fn an_expanded_group_never_renders_its_newest_message_twice() {
        let messages = [
            threaded_pair().0,
            threaded_pair().1,
            message("account-a", "loose", None, 10),
        ];

        for expanded in [false, true] {
            let entries = entries(&messages, expanded);
            let references = rendered_references(&entries);
            let unique: HashSet<&MessageRef> = references.iter().collect();
            assert_eq!(
                references.len(),
                unique.len(),
                "duplicate row while expanded={expanded}: {references:?}"
            );
        }
    }

    /// Expanding must reveal the older messages, not merely restyle the row.
    #[test]
    fn expanding_adds_exactly_the_older_members() {
        let messages = [threaded_pair().0, threaded_pair().1];

        let collapsed = rendered_references(&entries(&messages, false));
        let expanded = rendered_references(&entries(&messages, true));

        assert_eq!(collapsed.len(), 1, "a collapsed thread is one row");
        assert_eq!(expanded.len(), 2, "expanding reveals the older message");
        assert_eq!(expanded[0].id, "thread-new");
        assert_eq!(expanded[1].id, "thread-old");
    }
}
