//! Kanban view: one column per tag, with cards for tagged messages. Uses
//! native gpui drag-and-drop between columns.

use super::app::AviaryApp;
use super::motion::{HoverMotionExt as _, HoverMotionMap, Lerp as _, WheelScrollMotion};
use super::util;
use crate::model::{AccountId, MessageHeader, Tag};
use crate::runtime::Cmd;
use gpui::{
    div, point, prelude::*, px, Context, Hsla, Pixels, Point, ScrollHandle, ScrollWheelEvent,
    Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::Input,
    menu::{DropdownMenu, PopupMenuItem},
    scroll::Scrollbar,
    v_flex, ActiveTheme, IconName, Sizable, StyledExt,
};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

const KANBAN_SCROLLBAR_HEIGHT: Pixels = px(16.);
const KANBAN_CARD_HOVER_DURATION: Duration = Duration::from_millis(140);

pub struct BoardState {
    pub accounts: HashMap<AccountId, AccountBoardState>,
    /// Message displayed in the right drawer as `(account, message)`.
    pub preview: Option<(AccountId, String)>,
    horizontal_scroll: ScrollHandle,
    horizontal_overflow: bool,
    column_scrolls: HashMap<String, KanbanColumnScroll>,
    merged_revision: u64,
    merged_cache: Option<(u64, Vec<AccountId>, Rc<Vec<MergedColumn>>)>,
    card_hover: HoverMotionMap<(AccountId, String)>,
}

impl Default for BoardState {
    fn default() -> Self {
        Self {
            accounts: HashMap::new(),
            preview: None,
            horizontal_scroll: ScrollHandle::default(),
            horizontal_overflow: false,
            column_scrolls: HashMap::new(),
            merged_revision: 0,
            merged_cache: None,
            card_hover: HoverMotionMap::new(KANBAN_CARD_HOVER_DURATION),
        }
    }
}

#[derive(Default)]
pub struct AccountBoardState {
    pub tags: Vec<Tag>,
    pub columns: Vec<TagColumn>,
}

pub struct TagColumn {
    pub tag_id: String,
    pub messages: Vec<MessageHeader>,
    pub loaded: bool,
    pub loading: bool,
}

/// Card drag payload.
#[derive(Clone)]
pub struct CardDrag {
    pub account_id: AccountId,
    pub source_tag_id: String,
    pub message: MessageHeader,
}

#[derive(Clone)]
struct MergedTagTarget {
    account_id: AccountId,
    tag_id: String,
}

#[derive(Clone)]
struct MergedCard {
    source_tag_id: String,
    message: MessageHeader,
}

#[derive(Clone)]
struct MergedColumn {
    key: String,
    title: String,
    targets: Vec<MergedTagTarget>,
    cards: Vec<MergedCard>,
    loaded: bool,
}

struct KanbanColumnScroll {
    handle: ScrollHandle,
    motion: WheelScrollMotion,
}

impl Default for KanbanColumnScroll {
    fn default() -> Self {
        Self {
            handle: ScrollHandle::new(),
            motion: WheelScrollMotion::default(),
        }
    }
}

fn normalized_tag_name(name: &str) -> String {
    name.trim().to_lowercase()
}

fn scroll_board_by_shift_wheel(
    board: &ScrollHandle,
    event: &ScrollWheelEvent,
    window: &Window,
) -> bool {
    if !event.modifiers.shift {
        return false;
    }
    let delta = event.delta.pixel_delta(window.line_height());
    let amount = if delta.y == px(0.) { delta.x } else { delta.y };
    if amount == px(0.) {
        return true;
    }
    let offset = board.offset();
    let x = (offset.x + amount).clamp(-board.max_offset().width, px(0.));
    board.set_offset(point(x, offset.y));
    true
}

/// Transfers Shift + wheel from a column's vertical scroller to the board.
/// The column's native scroll handler has already applied the delta when this
/// ancestor listener runs, so restore it before moving horizontally.
fn transfer_shift_wheel_to_board(
    column: &ScrollHandle,
    board: &ScrollHandle,
    event: &ScrollWheelEvent,
    window: &Window,
) -> bool {
    if !event.modifiers.shift {
        return false;
    }
    let delta = event.delta.pixel_delta(window.line_height());
    let amount = if delta.y == px(0.) { delta.x } else { delta.y };
    if amount == px(0.) {
        return true;
    }

    let column_offset = column.offset();
    column.set_offset(point(column_offset.x, column_offset.y - delta.y));
    scroll_board_by_shift_wheel(board, event, window);
    true
}

impl BoardState {
    fn account_from_columns(cols: &[String]) -> AccountBoardState {
        AccountBoardState {
            tags: Vec::new(),
            columns: cols
                .iter()
                .map(|tag_id| TagColumn {
                    tag_id: tag_id.clone(),
                    messages: Vec::new(),
                    loaded: false,
                    loading: false,
                })
                .collect(),
        }
    }

    pub fn ensure_account(&mut self, account_id: &AccountId, cols: &[String]) {
        if !self.accounts.contains_key(account_id) {
            self.accounts
                .insert(account_id.clone(), Self::account_from_columns(cols));
            self.invalidate_merged();
        }
    }

    pub fn reset_account(&mut self, account_id: &AccountId, cols: &[String]) {
        self.accounts
            .insert(account_id.clone(), Self::account_from_columns(cols));
        self.invalidate_merged();
        self.prune_card_motion();
    }

    pub fn remove_account(&mut self, account_id: &AccountId) {
        self.accounts.remove(account_id);
        self.invalidate_merged();
        self.prune_card_motion();
        if self
            .preview
            .as_ref()
            .is_some_and(|(preview_account, _)| preview_account == account_id)
        {
            self.preview = None;
        }
    }

    pub fn account(&self, account_id: &AccountId) -> Option<&AccountBoardState> {
        self.accounts.get(account_id)
    }

    pub fn account_mut(&mut self, account_id: &AccountId) -> Option<&mut AccountBoardState> {
        self.invalidate_merged();
        self.accounts.get_mut(account_id)
    }

    pub(super) fn invalidate_merged(&mut self) {
        self.merged_revision = self.merged_revision.wrapping_add(1);
        self.merged_cache = None;
    }

    pub fn column_ids(&self, account_id: &AccountId) -> Vec<String> {
        self.account(account_id)
            .map(|board| {
                board
                    .columns
                    .iter()
                    .map(|column| column.tag_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn set_tags(&mut self, account_id: &AccountId, tags: Vec<Tag>) {
        let changed = self.account(account_id).is_some_and(|board| {
            board.tags.len() != tags.len()
                || board.tags.iter().zip(&tags).any(|(left, right)| {
                    left.id != right.id
                        || left.display_name != right.display_name
                        || left.color != right.color
                })
        });
        if changed {
            self.invalidate_merged();
        }
        if changed {
            if let Some(board) = self.accounts.get_mut(account_id) {
                board.tags = tags;
            }
        }
    }

    pub fn add_column(&mut self, account_id: &AccountId, tag_id: &str) {
        let Some(board) = self.account_mut(account_id) else {
            return;
        };
        if !board.columns.iter().any(|column| column.tag_id == tag_id) {
            board.columns.push(TagColumn {
                tag_id: tag_id.to_string(),
                messages: Vec::new(),
                loaded: false,
                loading: false,
            });
        }
    }

    pub fn remove_column(&mut self, account_id: &AccountId, tag_id: &str) {
        if let Some(board) = self.account_mut(account_id) {
            board.columns.retain(|column| column.tag_id != tag_id);
        }
        self.prune_card_motion();
    }

    pub fn replace_tag_id(&mut self, account_id: &AccountId, old_id: &str, new_id: &str) {
        if old_id == new_id {
            return;
        }
        if let Some(board) = self.account_mut(account_id) {
            for column in &mut board.columns {
                if column.tag_id == old_id {
                    column.tag_id = new_id.to_string();
                }
            }
        }
    }

    pub fn set_column_messages(
        &mut self,
        account_id: &AccountId,
        tag_id: &str,
        messages: Vec<MessageHeader>,
    ) {
        let Some(board) = self.account_mut(account_id) else {
            return;
        };
        if let Some(col) = board.columns.iter_mut().find(|c| c.tag_id == tag_id) {
            col.messages = messages;
            col.loaded = true;
            col.loading = false;
        }
        self.prune_card_motion();
    }

    pub fn mark_column_loading(&mut self, account_id: &AccountId, tag_id: &str) {
        let Some(board) = self.account_mut(account_id) else {
            return;
        };
        if let Some(col) = board.columns.iter_mut().find(|c| c.tag_id == tag_id) {
            col.loading = true;
        }
    }

    fn tag(&self, account_id: &AccountId, tag_id: &str) -> Option<&Tag> {
        self.account(account_id)?
            .tags
            .iter()
            .find(|tag| tag.id == tag_id)
    }

    fn column_key(board: &AccountBoardState, column: &TagColumn) -> String {
        board
            .tags
            .iter()
            .find(|tag| tag.id == column.tag_id)
            .map(|tag| normalized_tag_name(&tag.display_name))
            .unwrap_or_else(|| normalized_tag_name(&column.tag_id))
    }

    fn visible_keys(&self) -> HashSet<String> {
        self.accounts
            .values()
            .flat_map(|board| {
                board
                    .columns
                    .iter()
                    .map(|column| Self::column_key(board, column))
            })
            .collect()
    }

    fn available_tag_names(&self) -> Vec<(String, String, Option<u32>)> {
        let visible = self.visible_keys();
        let mut names = HashMap::<String, (String, Option<u32>)>::new();
        for tag in self.accounts.values().flat_map(|board| board.tags.iter()) {
            let key = normalized_tag_name(&tag.display_name);
            if !key.is_empty() && !visible.contains(&key) {
                let entry = names
                    .entry(key)
                    .or_insert_with(|| (tag.display_name.clone(), tag.color));
                if entry.1.is_none() {
                    entry.1 = tag.color;
                }
            }
        }
        let mut names: Vec<_> = names
            .into_iter()
            .map(|(key, (title, color))| (key, title, color))
            .collect();
        names.sort_by_key(|entry| entry.1.to_lowercase());
        names
    }

    fn add_merged_column(&mut self, key: &str) -> Vec<(AccountId, String)> {
        let mut added = Vec::new();
        for (account_id, board) in &mut self.accounts {
            let tag_ids: Vec<_> = board
                .tags
                .iter()
                .filter(|tag| normalized_tag_name(&tag.display_name) == key)
                .map(|tag| tag.id.clone())
                .collect();
            for tag_id in tag_ids {
                if board.columns.iter().any(|column| column.tag_id == tag_id) {
                    continue;
                }
                board.columns.push(TagColumn {
                    tag_id: tag_id.clone(),
                    messages: Vec::new(),
                    loaded: false,
                    loading: true,
                });
                added.push((account_id.clone(), tag_id));
            }
        }
        if !added.is_empty() {
            self.invalidate_merged();
        }
        added
    }

    fn merge_matching_columns(&mut self) -> Vec<(AccountId, String)> {
        let visible = self.visible_keys();
        let mut added = Vec::new();
        for key in visible {
            added.extend(self.add_merged_column(&key));
        }
        added
    }

    fn remove_merged_column(&mut self, key: &str) -> Vec<AccountId> {
        let mut changed = Vec::new();
        for (account_id, board) in &mut self.accounts {
            let matching_ids: HashSet<_> = board
                .tags
                .iter()
                .filter(|tag| normalized_tag_name(&tag.display_name) == key)
                .map(|tag| tag.id.as_str())
                .collect();
            let before = board.columns.len();
            board
                .columns
                .retain(|column| !matching_ids.contains(column.tag_id.as_str()));
            if board.columns.len() != before {
                changed.push(account_id.clone());
            }
        }
        if !changed.is_empty() {
            self.invalidate_merged();
            self.prune_card_motion();
        }
        changed
    }

    fn merged_columns(&mut self, account_order: &[AccountId]) -> Rc<Vec<MergedColumn>> {
        if let Some((revision, order, columns)) = &self.merged_cache {
            if *revision == self.merged_revision && order == account_order {
                return columns.clone();
            }
        }
        let mut merged = Vec::<MergedColumn>::new();
        let mut positions = HashMap::<String, usize>::new();
        let mut seen_targets = Vec::<HashSet<(AccountId, String)>>::new();
        let mut seen_cards = Vec::<HashSet<(AccountId, String)>>::new();
        for account_id in account_order {
            let Some(board) = self.account(account_id) else {
                continue;
            };
            for column in &board.columns {
                let tag = self.tag(account_id, &column.tag_id);
                let title = tag
                    .map(|tag| tag.display_name.clone())
                    .unwrap_or_else(|| column.tag_id.clone());
                let key = normalized_tag_name(&title);
                let index = if let Some(index) = positions.get(&key) {
                    *index
                } else {
                    let index = merged.len();
                    positions.insert(key.clone(), index);
                    merged.push(MergedColumn {
                        key: key.clone(),
                        title,
                        targets: Vec::new(),
                        cards: Vec::new(),
                        loaded: true,
                    });
                    seen_targets.push(HashSet::new());
                    seen_cards.push(HashSet::new());
                    index
                };
                let target = MergedTagTarget {
                    account_id: account_id.clone(),
                    tag_id: column.tag_id.clone(),
                };
                if seen_targets[index].insert((target.account_id.clone(), target.tag_id.clone())) {
                    merged[index].targets.push(target);
                }
                merged[index].loaded &= column.loaded;
                for message in &column.messages {
                    if !seen_cards[index].insert((message.account_id.clone(), message.id.clone())) {
                        continue;
                    }
                    merged[index].cards.push(MergedCard {
                        source_tag_id: column.tag_id.clone(),
                        message: message.clone(),
                    });
                }
            }
        }
        for column in &mut merged {
            column
                .cards
                .sort_by_key(|card| std::cmp::Reverse(card.message.received));
        }
        let merged = Rc::new(merged);
        self.merged_cache = Some((self.merged_revision, account_order.to_vec(), merged.clone()));
        merged
    }

    fn sync_column_scrolls(&mut self) {
        let visible = self.visible_keys();
        self.column_scrolls.retain(|key, _| visible.contains(key));
        for key in visible {
            self.column_scrolls.entry(key).or_default();
        }
    }

    fn prune_card_motion(&mut self) {
        let visible: HashSet<_> = self
            .accounts
            .values()
            .flat_map(|board| board.columns.iter())
            .flat_map(|column| column.messages.iter())
            .map(|message| (message.account_id.clone(), message.id.clone()))
            .collect();
        self.card_hover.retain(|key| visible.contains(key));
    }
}

impl AviaryApp {
    pub fn render_kanban(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if let Some((account_id, message_id)) = self.kanban.preview.clone() {
            let target_is_requested = self.mailbox.active_tab.is_none()
                && self.mailbox.selected_id.as_deref() == Some(message_id.as_str())
                && self.mailbox.selected.as_ref().is_none_or(|message| {
                    message.header.account_id == account_id && message.header.id == message_id
                });
            if !target_is_requested {
                self.open_message(account_id, message_id, cx);
            }
        }
        self.ensure_kanban_loaded();
        self.kanban.card_hover.request_frame(window);
        let account_order: Vec<_> = self
            .ordered_accounts()
            .into_iter()
            .map(|account| account.id)
            .collect();
        let merged_columns = self.kanban.merged_columns(&account_order);
        let panes = super::app::sidebar_layout(
            "kanban-panes",
            self.sidebar_resize.clone(),
            self.render_kanban_sidebar(&merged_columns, cx)
                .into_any_element(),
            self.render_kanban_board(&merged_columns, window, cx)
                .into_any_element(),
        );
        let preview = self.kanban.preview.clone();
        let theme = cx.theme().clone();

        div()
            .relative()
            .size_full()
            .child(panes)
            .children(preview.map(|(_, message_id)| {
                v_flex()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .w(px(620.))
                    .max_w_full()
                    .occlude()
                    .border_l_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .shadow_lg()
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_2()
                            .items_center()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .font_semibold()
                                    .truncate()
                                    .child(tr!("kanban-message-preview")),
                            )
                            .child(
                                Button::new(gpui::ElementId::Name(
                                    format!("kanban-close-preview-{message_id}").into(),
                                ))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip(tr!("kanban-close-preview"))
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.kanban.preview = None;
                                        super::blitz_body::cancel_pending_reader(cx);
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .child(self.render_viewer_pane(window, cx).into_any_element()),
                    )
            }))
    }

    fn render_kanban_board(
        &mut self,
        columns: &[MergedColumn],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        if self.accounts.is_empty() {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child(tr!("kanban-account-required"))
                .into_any_element();
        }

        let horizontal_scroll = self.kanban.horizontal_scroll.clone();
        let app = cx.entity();
        let mut board = h_flex()
            .on_children_prepainted({
                let horizontal_scroll = horizontal_scroll.clone();
                move |_, _, cx| {
                    let horizontal_overflow = horizontal_scroll.max_offset().width > px(0.);
                    app.update(cx, |this, cx| {
                        if this.kanban.horizontal_overflow != horizontal_overflow {
                            this.kanban.horizontal_overflow = horizontal_overflow;
                            cx.notify();
                        }
                    });
                }
            })
            .id("kanban-scroll")
            .w_full()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .gap_3()
            .p_3()
            .items_start()
            .overflow_y_hidden()
            .overflow_x_scroll()
            .track_scroll(&horizontal_scroll);
        // Do not let gpui translate a plain vertical wheel into horizontal
        // movement. Shift + wheel is handled explicitly below and in columns.
        board.style().restrict_scroll_to_axis = Some(true);
        board = board.on_scroll_wheel(cx.listener({
            let horizontal_scroll = horizontal_scroll.clone();
            move |_, event: &ScrollWheelEvent, window, cx| {
                if scroll_board_by_shift_wheel(&horizontal_scroll, event, window) {
                    cx.stop_propagation();
                    cx.notify();
                }
            }
        }));
        for (index, column) in columns.iter().enumerate() {
            board = board.child(self.render_kanban_column(index, column, window, cx));
        }
        if columns.is_empty() {
            board = board.child(
                div()
                    .p_6()
                    .text_color(theme.muted_foreground)
                    .child(tr!("kanban-get-started")),
            );
        }

        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(board)
            .when(self.kanban.horizontal_overflow, |view| {
                view.child(
                    div()
                        .flex_none()
                        .w_full()
                        .h(KANBAN_SCROLLBAR_HEIGHT)
                        .border_t_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .child(Scrollbar::horizontal(&horizontal_scroll)),
                )
            })
            .into_any_element()
    }

    fn render_kanban_sidebar(
        &mut self,
        merged_columns: &[MergedColumn],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let accounts = self.ordered_accounts();
        let actions = h_flex()
            .gap_1()
            .items_center()
            .px_3()
            .pb_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.render_add_column_button("kanban-sidebar-add-column", true, cx)),
            )
            .child(
                Button::new("kanban-refresh")
                    .ghost()
                    .small()
                    .icon(super::icons::app_icon("refresh-cw"))
                    .tooltip(tr!("kanban-reload"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reload_kanban();
                        cx.notify();
                    })),
            );

        let mut columns = v_flex().gap_1().px_2();
        for (index, column) in merged_columns.iter().enumerate() {
            let color = util::name_color(&column.title);
            columns = columns.child(
                h_flex()
                    .id(gpui::ElementId::Name(
                        format!("kanban-sidebar-column-{index}").into(),
                    ))
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_2()
                    .rounded(theme.radius)
                    .hover(|style| style.bg(theme.list_hover))
                    .child(div().w(px(8.)).h(px(24.)).rounded_full().bg(color))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_medium()
                                    .truncate()
                                    .child(column.title.clone()),
                            )
                            .child(div().text_xs().text_color(theme.muted_foreground).child(
                                tr!("kanban-merged-accounts", {
                                    count: column.targets.len()
                                }),
                            )),
                    )
                    .child(
                        div()
                            .min_w(px(24.))
                            .px_1p5()
                            .py_0p5()
                            .rounded_full()
                            .bg(theme.muted)
                            .text_xs()
                            .text_center()
                            .text_color(theme.muted_foreground)
                            .child(column.cards.len().to_string()),
                    ),
            );
        }
        if merged_columns.is_empty() {
            columns = columns.child(
                div()
                    .px_2()
                    .py_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tr!("kanban-get-started")),
            );
        }

        let mut account_legend = v_flex().gap_1().px_3().pb_3();
        for account in &accounts {
            let account_color = util::account_color(
                &account.id,
                self.settings
                    .accounts
                    .get(&account.id)
                    .and_then(|settings| settings.color_override),
            );
            account_legend = account_legend.child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .items_center()
                    .py_0p5()
                    .child(div().w(px(8.)).h(px(8.)).rounded_full().bg(account_color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .truncate()
                            .child(self.account_label(account)),
                    ),
            );
        }

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                v_flex()
                    .gap_1()
                    .px_3()
                    .pt_3()
                    .pb_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(super::icons::app_icon("square-kanban").small())
                            .child(div().text_lg().font_semibold().child(tr!("kanban-title"))),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        tr!("kanban-sidebar-subtitle", {
                            count: accounts.len()
                        }),
                    )),
            )
            .child(actions)
            .child(
                div()
                    .id("kanban-sidebar-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(columns),
            )
            .when(!accounts.is_empty(), |sidebar| {
                sidebar.child(
                    v_flex()
                        .flex_none()
                        .border_t_1()
                        .border_color(theme.border)
                        .pt_2()
                        .child(
                            div()
                                .px_3()
                                .pb_1()
                                .text_xs()
                                .font_semibold()
                                .text_color(theme.muted_foreground)
                                .child(tr!("kanban-accounts")),
                        )
                        .child(account_legend),
                )
            })
            .child(self.render_sidebar_navigation(cx))
    }

    pub(super) fn render_add_column_button(
        &self,
        id: &'static str,
        full_width: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tags = self.kanban.available_tag_names();
        let has_existing_tags = !tags.is_empty();
        let entity = cx.entity();

        Button::new(id)
            .primary()
            .small()
            .when(full_width, |button| button.w_full())
            .icon(if full_width {
                super::icons::app_icon("plus")
            } else {
                super::icons::app_icon("columns-plus")
            })
            .label(if full_width {
                tr!("kanban-column")
            } else {
                tr!("toolbar-new-column")
            })
            .dropdown_menu(move |mut menu, _window, _cx| {
                for (key, title, color) in tags.clone() {
                    let entity = entity.clone();
                    menu = menu.item(super::tag_menu::tag_menu_item(title, color).on_click(
                        move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.add_merged_kanban_column(&key);
                                cx.notify();
                            });
                        },
                    ));
                }
                if has_existing_tags {
                    menu = menu.separator();
                }
                let entity = entity.clone();
                menu.item(
                    PopupMenuItem::new(tr!("tags-create"))
                        .icon(super::icons::app_icon("plus"))
                        .on_click(move |_, window, cx| {
                            entity.update(cx, |this, cx| {
                                this.open_create_merged_tag_dialog(window, cx);
                            });
                        }),
                )
            })
    }

    fn persist_kanban_columns_for(&mut self, account_ids: &[AccountId]) {
        let account_ids: HashSet<_> = account_ids.iter().cloned().collect();
        if account_ids.is_empty() {
            return;
        }
        for account_id in account_ids {
            let columns = self.kanban.column_ids(&account_id);
            self.settings.account_mut(&account_id).kanban_tag_columns = columns;
        }
        self.settings.save();
    }

    fn add_merged_kanban_column(&mut self, key: &str) {
        let added = self.kanban.add_merged_column(key);
        let changed_accounts: Vec<_> = added
            .iter()
            .map(|(account_id, _)| account_id.clone())
            .collect();
        self.persist_kanban_columns_for(&changed_accounts);
        for (account_id, tag_id) in added {
            self.send(Cmd::LoadTagListing {
                account_id,
                tag_id,
                limit: 50,
            });
        }
    }

    pub(super) fn ensure_kanban_loaded(&mut self) {
        let account_ids: Vec<_> = self
            .ordered_accounts()
            .into_iter()
            .map(|account| account.id)
            .collect();
        for aid in &account_ids {
            let columns = self
                .settings
                .account_or_default(Some(aid))
                .kanban_tag_columns;
            self.kanban.ensure_account(aid, &columns);
            if let Some(tags) = self.tags_by_account.get(aid) {
                self.kanban.set_tags(aid, tags.clone());
            } else {
                self.ensure_tags_loaded(aid);
            }
        }

        let merged = self.kanban.merge_matching_columns();
        let changed_accounts: Vec<_> = merged
            .iter()
            .map(|(account_id, _)| account_id.clone())
            .collect();
        if !changed_accounts.is_empty() {
            self.persist_kanban_columns_for(&changed_accounts);
        }
        for (account_id, tag_id) in merged {
            self.send(Cmd::LoadTagListing {
                account_id,
                tag_id,
                limit: 50,
            });
        }

        for aid in account_ids {
            let pending: Vec<String> = self
                .kanban
                .account(&aid)
                .into_iter()
                .flat_map(|board| board.columns.iter())
                .filter(|column| !column.loaded && !column.loading)
                .map(|column| column.tag_id.clone())
                .collect();
            if !pending.is_empty() {
                if let Some(board) = self.kanban.account_mut(&aid) {
                    for column in &mut board.columns {
                        if pending.contains(&column.tag_id) {
                            column.loading = true;
                        }
                    }
                }
            }
            for tag_id in pending {
                self.send(Cmd::LoadTagListing {
                    account_id: aid.clone(),
                    tag_id,
                    limit: 50,
                });
            }
        }
        self.kanban.sync_column_scrolls();
    }

    fn render_kanban_column(
        &mut self,
        ix: usize,
        column: &MergedColumn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let key = column.key.clone();
        let targets = column.targets.clone();
        let scroll_handle = {
            let scroll = self.kanban.column_scrolls.entry(key.clone()).or_default();
            scroll.motion.advance(&scroll.handle, window);
            scroll.handle.clone()
        };
        let horizontal_scroll = self.kanban.horizontal_scroll.clone();

        let mut cards = v_flex().gap_2().p_2().min_h(px(60.));
        for card in &column.cards {
            cards = cards.child(self.render_kanban_card(&card.message, &card.source_tag_id, cx));
        }
        if !column.loaded {
            cards = cards.child(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tr!("loading")),
            );
        } else if column.cards.is_empty() {
            cards = cards.child(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tr!("kanban-empty-column")),
            );
        }

        v_flex()
            .id(gpui::ElementId::Name(format!("kcol-{ix}").into()))
            .w(px(300.))
            .h_full()
            .min_h_0()
            .flex_none()
            .overflow_hidden()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .child(self.render_kanban_column_header(ix, column, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .on_scroll_wheel(cx.listener({
                        let handle = scroll_handle.clone();
                        let key = key.clone();
                        let horizontal_scroll = horizontal_scroll.clone();
                        move |this, event: &ScrollWheelEvent, window, cx| {
                            if transfer_shift_wheel_to_board(
                                &handle,
                                &horizontal_scroll,
                                event,
                                window,
                            ) {
                                if let Some(scroll) = this.kanban.column_scrolls.get_mut(&key) {
                                    scroll.motion.cancel();
                                }
                                cx.stop_propagation();
                                cx.notify();
                            } else {
                                let changed = this.kanban.column_scrolls.get_mut(&key).is_some_and(
                                    |scroll| scroll.motion.on_wheel(&handle, event, window),
                                );
                                if changed {
                                    cx.notify();
                                }
                                // The column owns a plain wheel gesture; do not
                                // also let the board consume it horizontally.
                                cx.stop_propagation();
                            }
                        }
                    }))
                    .child(
                        div()
                            .id(gpui::ElementId::Name(format!("kcol-scroll-{ix}").into()))
                            .size_full()
                            .overflow_x_hidden()
                            .overflow_y_scroll()
                            .track_scroll(&scroll_handle)
                            .child(cards),
                    ),
            )
            .drag_over::<CardDrag>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .on_drop(cx.listener({
                let targets = targets.clone();
                move |this, drag: &CardDrag, window, cx| {
                    let Some(tag_id) = targets
                        .iter()
                        .find(|target| target.account_id == drag.account_id)
                        .map(|target| target.tag_id.clone())
                    else {
                        return;
                    };
                    if drag.source_tag_id == tag_id {
                        return;
                    }
                    // Optimistically move the card locally.
                    let msg = drag.message.clone();
                    if let Some(board) = this.kanban.account_mut(&drag.account_id) {
                        if let Some(src) = board
                            .columns
                            .iter_mut()
                            .find(|column| column.tag_id == drag.source_tag_id)
                        {
                            src.messages.retain(|message| message.id != msg.id);
                        }
                        if let Some(dst) = board
                            .columns
                            .iter_mut()
                            .find(|column| column.tag_id == tag_id)
                        {
                            dst.messages.insert(0, msg.clone());
                        }
                    }
                    this.move_kanban_undoable(
                        vec![
                            Cmd::RemoveTag {
                                account_id: drag.account_id.clone(),
                                message_id: msg.id.clone(),
                                tag_id: drag.source_tag_id.clone(),
                            },
                            Cmd::AddTag {
                                account_id: drag.account_id.clone(),
                                message_id: msg.id.clone(),
                                tag_id: tag_id.clone(),
                            },
                        ],
                        drag.account_id.clone(),
                        msg,
                        drag.source_tag_id.clone(),
                        tag_id,
                        window,
                        cx,
                    );
                    cx.notify();
                }
            }))
    }

    fn open_create_merged_tag_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            gpui_component::input::InputState::new(window, cx)
                .placeholder(tr!("tags-new-name-placeholder"))
        });
        let entity = cx.entity();
        gpui_component::WindowExt::open_dialog(window, cx, move |dialog, _window, _cx| {
            let entity = entity.clone();
            let input = input.clone();
            dialog
                .title(tr!("tags-create-title"))
                .confirm()
                .child(
                    v_flex()
                        .gap_2()
                        .child(div().text_sm().child(tr!("kanban-create-all-accounts")))
                        .child(Input::new(&input)),
                )
                .on_ok(move |_, _window, cx| {
                    let name = input.read(cx).value().trim().to_string();
                    if name.is_empty() {
                        return false;
                    }
                    entity.update(cx, |this, cx| {
                        for account in this.ordered_accounts() {
                            this.send(Cmd::CreateTag {
                                account_id: account.id,
                                name: name.clone(),
                                color: None,
                            });
                        }
                        cx.notify();
                    });
                    true
                })
        });
    }

    fn open_rename_merged_tag_dialog(
        &mut self,
        current: String,
        targets: Vec<MergedTagTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input =
            cx.new(|cx| gpui_component::input::InputState::new(window, cx).default_value(current));
        let entity = cx.entity();
        gpui_component::WindowExt::open_dialog(window, cx, move |dialog, _window, _cx| {
            let entity = entity.clone();
            let input = input.clone();
            let targets = targets.clone();
            dialog
                .title(tr!("tags-rename-title"))
                .confirm()
                .child(Input::new(&input))
                .on_ok(move |_, _window, cx| {
                    let new_name = input.read(cx).value().trim().to_string();
                    if new_name.is_empty() {
                        return false;
                    }
                    entity.update(cx, |this, cx| {
                        for target in &targets {
                            this.send(Cmd::RenameTag {
                                account_id: target.account_id.clone(),
                                id: target.tag_id.clone(),
                                new_name: new_name.clone(),
                            });
                        }
                        cx.notify();
                    });
                    true
                })
        });
    }

    fn render_kanban_card(
        &self,
        m: &MessageHeader,
        tag_id: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let account_color = util::account_color(
            &m.account_id,
            self.settings
                .accounts
                .get(&m.account_id)
                .and_then(|settings| settings.color_override),
        );
        let drag = CardDrag {
            account_id: m.account_id.clone(),
            source_tag_id: tag_id.to_string(),
            message: m.clone(),
        };
        let aid = m.account_id.clone();
        let mid = m.id.clone();
        let motion_key = (aid.clone(), mid.clone());
        let hover = self.kanban.card_hover.value(&motion_key);
        let lift = px(0.).lerp(px(-2.), hover);
        let background = theme.background.lerp(theme.list_hover, hover);
        let border_color = theme.border.lerp(account_color.opacity(0.65), hover);
        div()
            .id(gpui::ElementId::Name(
                format!("card-{}-{}-{}", m.account_id.0, tag_id, m.id).into(),
            ))
            .relative()
            .top(lift)
            .p_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(border_color)
            .bg(background)
            .cursor_pointer()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .truncate()
                    .child(if m.subject.is_empty() {
                        tr!("no-subject").to_string()
                    } else {
                        m.subject.clone()
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .items_center()
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(9.))
                                    .h(px(9.))
                                    .rounded_full()
                                    .bg(account_color),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .text_color(theme.muted_foreground)
                                    .child(util::display_name(&m.from)),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(util::short_date(&m.received)),
                    ),
            )
            .on_drag(drag, move |drag, cursor_offset, _window, cx| {
                cx.new(|_| CardDragPreview {
                    subject: drag.message.subject.clone(),
                    sender: util::display_name(&drag.message.from),
                    received: util::short_date(&drag.message.received),
                    account_color,
                    cursor_offset,
                })
            })
            .with_hover_motion(cx, motion_key, |this| &mut this.kanban.card_hover)
            .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, _, cx| {
                if ev.click_count() >= 2 {
                    this.request_message_tab(aid.clone(), mid.clone(), cx);
                } else {
                    this.kanban.preview = Some((aid.clone(), mid.clone()));
                    this.open_message(aid.clone(), mid.clone(), cx);
                }
            }))
    }

    /// A column's header: its colour, title, card count, and the menu that
    /// renames or removes the tag it stands for.
    fn render_kanban_column_header(
        &self,
        ix: usize,
        column: &MergedColumn,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let color = util::name_color(&column.title);
        let key = column.key.clone();
        let title = column.title.clone();
        let targets = column.targets.clone();
        let entity = cx.entity();
        h_flex()
            .flex_none()
            .gap_2()
            .items_center()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(div().w(px(10.)).h(px(10.)).rounded_full().bg(color))
            .child(
                div()
                    .flex_1()
                    .font_semibold()
                    .text_sm()
                    .truncate()
                    .child(title.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(column.cards.len().to_string()),
            )
            .child(
                Button::new(gpui::ElementId::Name(format!("kcol-menu-{ix}").into()))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Ellipsis)
                    .dropdown_menu({
                        let entity = entity.clone();
                        let key = key.clone();
                        let title = title.clone();
                        let targets = targets.clone();
                        move |mut menu, _window, _cx| {
                            {
                                let entity = entity.clone();
                                let key = key.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(tr!("kanban-hide-column")).on_click(
                                        move |_, _, cx| {
                                            entity.update(cx, |this, cx| {
                                                let changed =
                                                    this.kanban.remove_merged_column(&key);
                                                this.persist_kanban_columns_for(&changed);
                                                cx.notify();
                                            });
                                        },
                                    ),
                                );
                            }
                            {
                                let entity = entity.clone();
                                let title = title.clone();
                                let targets = targets.clone();
                                menu = menu.item(PopupMenuItem::new(tr!("tags-rename")).on_click(
                                    move |_, window, cx| {
                                        entity.update(cx, |this, cx| {
                                            this.open_rename_merged_tag_dialog(
                                                title.clone(),
                                                targets.clone(),
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ));
                            }
                            {
                                let entity = entity.clone();
                                let targets = targets.clone();
                                menu = menu.item(PopupMenuItem::new(tr!("tags-delete")).on_click(
                                    move |_, window, cx| {
                                        entity.update(cx, |this, cx| {
                                            let delay = this.action_delay_secs();
                                            let commands = targets
                                                .iter()
                                                .map(|target| Cmd::DeleteTag {
                                                    account_id: target.account_id.clone(),
                                                    id: target.tag_id.clone(),
                                                })
                                                .collect();
                                            this.send_many_undoable(
                                                commands,
                                                tr!("undo-tag-delete-pending", { seconds: delay }),
                                                tr!("undo-tag-delete-started"),
                                                tr!("undo-tag-delete-cancelled"),
                                                window,
                                                cx,
                                            );
                                            cx.notify();
                                        });
                                    },
                                ));
                            }
                            menu
                        }
                    }),
            )
    }
}

/// Floating preview while dragging a card.
struct CardDragPreview {
    subject: String,
    sender: String,
    received: String,
    account_color: Hsla,
    cursor_offset: Point<Pixels>,
}

impl Render for CardDragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let size = gpui::size(px(284.), px(64.));
        div()
            // As with calendar events, gpui positions the root at
            // `mouse - click offset`. Recenter the preview so it does not
            // jump based on where the card was grabbed.
            .pl(self.cursor_offset.x - size.width / 2.)
            .pt(self.cursor_offset.y - size.height / 2.)
            .child(
                h_flex()
                    .w(size.width)
                    .h(size.height)
                    .gap_2()
                    .items_center()
                    .p_2()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(self.account_color.opacity(0.55))
                    .bg(self.account_color.opacity(0.2))
                    .shadow_md()
                    .text_color(theme.foreground)
                    .child(
                        div()
                            .flex_none()
                            .w(px(4.))
                            .h(px(40.))
                            .rounded_full()
                            .bg(self.account_color),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(div().text_sm().font_semibold().truncate().child(
                                if self.subject.is_empty() {
                                    tr!("no-subject").to_string()
                                } else {
                                    self.subject.clone()
                                },
                            ))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .child(self.sender.clone()),
                                    )
                                    .child(self.received.clone()),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_board(tag_id: &str, name: &str, visible: bool) -> AccountBoardState {
        AccountBoardState {
            tags: vec![Tag {
                id: tag_id.to_string(),
                display_name: name.to_string(),
                color: None,
            }],
            columns: visible
                .then(|| TagColumn {
                    tag_id: tag_id.to_string(),
                    messages: Vec::new(),
                    loaded: true,
                    loading: false,
                })
                .into_iter()
                .collect(),
        }
    }

    #[test]
    fn same_named_tags_share_one_logical_column() {
        let first = AccountId("first".to_string());
        let second = AccountId("second".to_string());
        let mut board = BoardState::default();
        board
            .accounts
            .insert(first.clone(), account_board("tag-a", " Work ", true));
        board
            .accounts
            .insert(second.clone(), account_board("tag-b", "work", true));

        let columns = board.merged_columns(&[first, second]);

        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].targets.len(), 2);
    }

    #[test]
    fn adding_a_logical_column_targets_every_matching_account() {
        let first = AccountId("first".to_string());
        let second = AccountId("second".to_string());
        let mut board = BoardState::default();
        board
            .accounts
            .insert(first, account_board("tag-a", "Suivi", false));
        board
            .accounts
            .insert(second, account_board("tag-b", "SUIVI", false));

        let added = board.add_merged_column("suivi");

        assert_eq!(added.len(), 2);
        assert!(board
            .accounts
            .values()
            .all(|account| account.columns.len() == 1));
    }

    #[test]
    fn available_logical_tag_keeps_a_provider_color() {
        let first = AccountId("first".to_string());
        let second = AccountId("second".to_string());
        let mut first_board = account_board("tag-a", "Work", false);
        first_board.tags[0].color = None;
        let mut second_board = account_board("tag-b", "work", false);
        second_board.tags[0].color = Some(0x12_34_56);
        let mut board = BoardState::default();
        board.accounts.insert(first, first_board);
        board.accounts.insert(second, second_board);

        let available = board.available_tag_names();

        assert_eq!(available.len(), 1);
        assert_eq!(available[0].0, "work");
        assert_eq!(available[0].2, Some(0x12_34_56));
    }

    #[test]
    fn replacing_a_tag_id_keeps_the_visible_column() {
        let account_id = AccountId("account-a".to_string());
        let mut board = BoardState::default();
        board.accounts.insert(
            account_id.clone(),
            account_board("tag-old", "Étiquette A", true),
        );

        board.replace_tag_id(&account_id, "tag-old", "tag-new");

        assert_eq!(board.column_ids(&account_id), vec!["tag-new"]);
        assert!(board
            .account(&account_id)
            .is_some_and(|account| account.columns[0].loaded));
    }
}
