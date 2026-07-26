//! Chip-based recipient fields and address completion for composer To/Cc
//! fields and calendar attendees.
//!
//! The completion popup is owned by Aviary and filters the shared address
//! book against the current input. An address is converted to a chip with
//! Enter, a comma, a semicolon, or when focus is lost. Clicking a chip returns
//! it to the input for editing; the x button removes it.

use crate::model::Contact;
use crate::runtime::RecipientUsage;
use gpui::{
    actions, div, prelude::*, px, App, Context, ElementId, Entity, Focusable as _, KeyBinding,
    MouseButton, Pixels, WeakEntity, Window,
};
use gpui_component::{
    avatar::Avatar,
    button::{Button, ButtonVariants},
    h_flex,
    input::{Backspace, Enter, Escape, Input, InputEvent, InputState, MoveDown, MoveUp},
    v_flex, ActiveTheme, Sizable, StyledExt,
};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use super::components::overlay_popover::{OverlayPopover, OverlayPopoverScroll};

/// Maximum number of displayed suggestions.
const MAX_SUGGESTIONS: usize = 8;
const RECIPIENT_CONTEXT: &str = "RecipientInput";

actions!(recipient_input, [ShowAddressCompletions]);

/// Registers the explicit contact-picker shortcut once at application startup.
pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "ctrl-space",
        ShowAddressCompletions,
        Some("RecipientInput > Input"),
    )]);
}

#[derive(Default)]
struct AddressBookState {
    contacts: Vec<Contact>,
    usage: HashMap<String, RecipientUsage>,
    usage_only: HashSet<String>,
    subscribers: Vec<WeakEntity<RecipientInput>>,
}

/// Address book shared with `AviaryApp`, which populates it as
/// `Evt::Contacts` across all accounts) and providers for open composers.
/// `Rc<RefCell>` is sufficient because everything lives on the UI thread.
#[derive(Default, Clone)]
pub struct AddressBook(Rc<RefCell<AddressBookState>>);

impl AddressBook {
    fn subscribe(&self, subscriber: WeakEntity<RecipientInput>) {
        let mut book = self.0.borrow_mut();
        if !book.subscribers.contains(&subscriber) {
            book.subscribers.push(subscriber);
        }
    }

    fn notify_subscribers(&self, cx: &mut App) {
        let subscribers = std::mem::take(&mut self.0.borrow_mut().subscribers);
        let mut alive = Vec::with_capacity(subscribers.len());
        for subscriber in subscribers {
            if subscriber
                .update(cx, |input, cx| {
                    if input.suggestions_open {
                        input.refresh_suggestions(false, cx);
                    }
                })
                .is_ok()
            {
                alive.push(subscriber);
            }
        }
        self.0.borrow_mut().subscribers = alive;
    }

    /// Adds contacts missing from the address book, deduplicated by address.
    pub fn merge(&self, contacts: &[Contact], cx: &mut App) {
        let mut book = self.0.borrow_mut();
        for c in contacts {
            if c.email.is_empty() {
                continue;
            }
            let key = c.email.to_lowercase();
            if let Some(existing) = book
                .contacts
                .iter_mut()
                .find(|existing| existing.email.eq_ignore_ascii_case(&c.email))
            {
                if existing.name.trim().is_empty() && !c.name.trim().is_empty() {
                    existing.name.clone_from(&c.name);
                }
                existing.score = existing.score.max(c.score);
            } else {
                book.contacts.push(c.clone());
            }
            book.usage_only.remove(&key);
        }
        drop(book);
        self.notify_subscribers(cx);
    }

    pub fn merge_usage(&self, entries: &[RecipientUsage], cx: &mut App) {
        let mut book = self.0.borrow_mut();
        for entry in entries {
            book.usage.insert(entry.email.to_lowercase(), entry.clone());
            if !book
                .contacts
                .iter()
                .any(|contact| contact.email.eq_ignore_ascii_case(&entry.email))
            {
                book.contacts.push(Contact {
                    name: String::new(),
                    email: entry.email.clone(),
                    score: 0.,
                });
                book.usage_only.insert(entry.email.to_lowercase());
            }
        }
        drop(book);
        self.notify_subscribers(cx);
    }

    pub fn clear_usage(&self, cx: &mut App) {
        let mut book = self.0.borrow_mut();
        let usage_only = std::mem::take(&mut book.usage_only);
        book.contacts
            .retain(|contact| !usage_only.contains(&contact.email.to_lowercase()));
        book.usage.clear();
        drop(book);
        self.notify_subscribers(cx);
    }

    /// Returns contact suggestions ranked with the same usage-aware ordering
    /// as recipient completion. The caller decides when an empty query should
    /// display results; mail search uses it only once text has been entered.
    pub fn search(&self, query: &str, limit: usize) -> Vec<Contact> {
        let needle = query.trim().to_lowercase();
        let book = self.0.borrow();
        let mut matches: Vec<&Contact> = book
            .contacts
            .iter()
            .filter(|contact| {
                needle.is_empty()
                    || contact.name.to_lowercase().contains(&needle)
                    || contact.email.to_lowercase().contains(&needle)
            })
            .collect();
        matches.sort_by(|a, b| compare_contacts(a, b, &needle, &book.usage));
        matches.into_iter().take(limit).cloned().collect()
    }

    /// Ranges of contact mentions inserted by the composer completion.
    /// Matching the exact display spelling keeps byte offsets stable for gpui
    /// while still restoring mention styling when a draft is reopened.
    pub(crate) fn mention_ranges(&self, value: &str) -> Vec<Range<usize>> {
        let book = self.0.borrow();
        let mut ranges = Vec::new();
        for contact in &book.contacts {
            let mention = format!("@{}", mention_name(contact));
            for (start, _) in value.match_indices(&mention) {
                let end = start + mention.len();
                let ends_at_boundary = value[end..]
                    .chars()
                    .next()
                    .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_');
                if ends_at_boundary {
                    ranges.push(start..end);
                }
            }
        }
        ranges.sort_by_key(|range| (range.start, std::cmp::Reverse(range.end)));
        ranges.dedup();
        ranges
    }
}

/// Outlook-style recipient field: validated addresses are
/// rendered as chips, while the final input contains only the current
/// being entered. The full form (`Name <address>`) is retained for faithful
/// re-editing and draft serialization.
pub struct RecipientInput {
    recipients: Vec<String>,
    input: Entity<InputState>,
    address_book: AddressBook,
    suggestions: Vec<Contact>,
    suggestions_open: bool,
    suggestions_explicit: bool,
    selected_suggestion: usize,
    suggestions_scroll: OverlayPopoverScroll,
    field_height: Pixels,
    tab_index: isize,
}

impl RecipientInput {
    pub fn new(
        initial: &str,
        placeholder: String,
        address_book: AddressBook,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        // Bubble raw text changes to the chip field itself. Parent composers
        // observe this entity for session autosave; without this bridge, text
        // not yet committed into a recipient chip would be missed.
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        cx.subscribe_in(
            &input,
            window,
            |this, state, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.commit_delimited(state, window, cx);
                    this.refresh_suggestions(false, cx);
                }
                InputEvent::PressEnter { .. } => {
                    this.commit_current(state, window, cx);
                }
                InputEvent::Blur => {
                    this.commit_current(state, window, cx);
                    this.close_suggestions();
                    cx.notify();
                }
                InputEvent::Focus => cx.notify(),
            },
        )
        .detach();
        address_book.subscribe(cx.weak_entity());

        Self {
            recipients: super::util::parse_addresses(initial),
            input,
            address_book,
            suggestions: Vec::new(),
            suggestions_open: false,
            suggestions_explicit: false,
            selected_suggestion: 0,
            suggestions_scroll: OverlayPopoverScroll::default(),
            field_height: px(32.),
            tab_index: 0,
        }
    }

    pub fn tab_index(mut self, tab_index: isize) -> Self {
        self.tab_index = tab_index;
        self
    }

    pub fn set_placeholder(
        &mut self,
        placeholder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input.update(cx, |state, cx| {
            state.set_placeholder(placeholder, window, cx);
        });
    }

    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |state, cx| state.focus(window, cx));
    }

    pub fn is_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        self.input.focus_handle(cx).is_focused(window)
    }

    /// All displayed forms, including input not yet converted into a chip.
    /// Useful when detaching a composer or saving a
    /// draft without losing text under the cursor.
    pub fn serialized(&self, cx: &gpui::App) -> String {
        self.values(cx).join(", ")
    }

    /// Bare addresses expected by providers.
    pub fn bare_addresses(&self, cx: &gpui::App) -> Vec<String> {
        super::util::parse_bare_addresses(&self.serialized(cx))
    }

    /// Adds a mentioned contact to the recipient chips unless the same
    /// address is already present (including unfinished input text).
    pub(crate) fn add_mentioned_contact(&mut self, contact: &Contact, cx: &mut Context<Self>) {
        if self
            .bare_addresses(cx)
            .iter()
            .any(|address| address.eq_ignore_ascii_case(&contact.email))
        {
            return;
        }
        self.recipients.push(insert_text(contact));
        cx.notify();
    }

    fn values(&self, cx: &gpui::App) -> Vec<String> {
        let mut values = self.recipients.clone();
        values.extend(super::util::parse_addresses(
            self.input.read(cx).value().as_ref(),
        ));
        values
    }

    fn commit_delimited(
        &mut self,
        state: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = state.read(cx).value().to_string();
        let (completed, remainder) = split_completed(&value);
        if completed.is_empty() {
            return;
        }
        self.recipients.extend(completed);
        state.update(cx, |state, cx| state.set_value(remainder, window, cx));
        self.close_suggestions();
        cx.notify();
    }

    fn commit_current(
        &mut self,
        state: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = state.read(cx).value().to_string();
        let completed = super::util::parse_addresses(&value);
        if completed.is_empty() {
            return;
        }
        self.recipients.extend(completed);
        state.update(cx, |state, cx| state.set_value("", window, cx));
        self.close_suggestions();
        cx.notify();
    }

    fn edit(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.recipients.len() {
            return;
        }
        let recipient = self.recipients.remove(index);
        self.close_suggestions();
        self.input.update(cx, |state, cx| {
            state.set_value(recipient, window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    fn remove(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.recipients.len() {
            self.recipients.remove(index);
            cx.notify();
        }
    }

    fn edit_last_on_empty_backspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.input.read(cx).value().is_empty() {
            return;
        }
        let Some(recipient) = self.recipients.pop() else {
            return;
        };
        cx.stop_propagation();
        self.close_suggestions();
        self.input.update(cx, |state, cx| {
            state.set_value(recipient, window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    fn close_suggestions(&mut self) {
        self.suggestions_open = false;
        self.suggestions_explicit = false;
        self.selected_suggestion = 0;
        self.suggestions_scroll.reset();
    }

    fn refresh_suggestions(&mut self, explicit: bool, cx: &mut Context<Self>) {
        if explicit {
            self.suggestions_explicit = true;
        }
        let value = self.input.read(cx).value().to_string();
        let (_, token) = current_token(&value);
        if token.chars().count() < 2 && !self.suggestions_explicit {
            self.close_suggestions();
            return;
        }

        self.suggestions = self.address_book.search(token, MAX_SUGGESTIONS);
        self.suggestions_open = true;
        self.selected_suggestion = self
            .selected_suggestion
            .min(self.suggestions.len().saturating_sub(1));
        cx.notify();
    }

    fn move_suggestion(&mut self, direction: isize, cx: &mut Context<Self>) {
        if !self.suggestions_open {
            return;
        }
        cx.stop_propagation();
        if self.suggestions.is_empty() {
            return;
        }
        self.selected_suggestion = if direction < 0 {
            self.selected_suggestion
                .checked_sub(1)
                .unwrap_or(self.suggestions.len() - 1)
        } else {
            (self.selected_suggestion + 1) % self.suggestions.len()
        };
        cx.notify();
    }

    fn accept_suggestion(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(contact) = self.suggestions.get(index).cloned() else {
            return false;
        };
        let value = insert_text(&contact);
        self.input
            .update(cx, |state, cx| state.set_value(value, window, cx));
        let input = self.input.clone();
        self.commit_current(&input, window, cx);
        true
    }

    fn accept_selected_suggestion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.suggestions_open {
            return;
        }
        if self.accept_suggestion(self.selected_suggestion, window, cx) {
            cx.stop_propagation();
        }
    }

    fn escape_suggestions(&mut self, cx: &mut Context<Self>) {
        if self.suggestions_open {
            self.close_suggestions();
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn show_completions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |state, cx| {
            state.focus(window, cx);
        });
        self.refresh_suggestions(true, cx);
        cx.stop_propagation();
    }
}

impl Render for RecipientInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let entity_id = self.input.entity_id();
        let recipient_entity = cx.entity();
        let focused = self.input.focus_handle(cx).is_focused(window);
        let mut field = h_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .gap_y_1()
            .flex_wrap()
            .items_center()
            .px_1()
            .py_0p5()
            .rounded(theme.radius)
            .border_1()
            .border_color(if focused { theme.ring } else { theme.input })
            .bg(theme.background)
            .when(theme.shadow, |field| field.shadow_xs());

        for (index, recipient) in self.recipients.iter().cloned().enumerate() {
            let edit_id = ElementId::Name(format!("recipient-edit-{entity_id}-{index}").into());
            let remove_id = ElementId::Name(format!("recipient-remove-{entity_id}-{index}").into());
            let edit_tooltip = tr!("recipient-edit-tooltip", { recipient: recipient.clone() });
            let remove_tooltip = tr!("recipient-remove-tooltip", { recipient: recipient.clone() });
            field = field.child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap_0p5()
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.secondary)
                    .pl_0p5()
                    .pr_0p5()
                    .child(
                        Button::new(edit_id)
                            .xsmall()
                            .tab_index((index * 2 + 1) as isize)
                            .ghost()
                            .label(recipient)
                            .tooltip(edit_tooltip)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit(index, window, cx);
                            })),
                    )
                    .child(
                        Button::new(remove_id)
                            .xsmall()
                            .tab_index((index * 2 + 2) as isize)
                            .ghost()
                            .icon(super::icons::app_icon("x"))
                            .tooltip(remove_tooltip)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove(index, cx);
                            })),
                    ),
            );
        }

        // The input remains a direct flex child, so its internal width
        // calculation is identical with or without chips. Only its chrome is
        // disabled because the shared container now draws the field.
        let editor = Input::new(&self.input)
            .small()
            .tab_index(0)
            .appearance(false)
            .flex_grow()
            .flex_shrink()
            .flex_basis(px(160.))
            .min_w(px(120.));
        let picker_id = ElementId::Name(format!("recipient-picker-{entity_id}").into());

        let suggestions = self.suggestions.clone();
        let selected_suggestion = self.selected_suggestion;
        let suggestions_open = self.suggestions_open;
        let suggestion_top = self.field_height + px(4.);

        field = field
            .child(editor)
            .child(
                Button::new(picker_id)
                    .xsmall()
                    .ghost()
                    .icon(super::icons::app_icon("users"))
                    .tooltip(tr!("recipient-show-contacts-tooltip"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_completions(window, cx);
                    })),
            )
            .on_children_prepainted(move |bounds, _, cx| {
                let top = bounds
                    .iter()
                    .map(|bounds| f32::from(bounds.top()))
                    .reduce(f32::min);
                let bottom = bounds
                    .iter()
                    .map(|bounds| f32::from(bounds.bottom()))
                    .reduce(f32::max);
                if let (Some(top), Some(bottom)) = (top, bottom) {
                    recipient_entity.update(cx, |this, _| {
                        this.field_height = px(bottom - top + 4.);
                    });
                }
            });

        let mut root = div()
            .id(("recipient-input", entity_id))
            .relative()
            .w_full()
            .key_context(RECIPIENT_CONTEXT)
            .tab_group()
            .tab_index(self.tab_index)
            .tab_stop(false)
            .capture_action(cx.listener(|this, _: &Backspace, window, cx| {
                this.edit_last_on_empty_backspace(window, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                this.move_suggestion(-1, cx);
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                this.move_suggestion(1, cx);
            }))
            .capture_action(cx.listener(|this, _: &Enter, window, cx| {
                this.accept_selected_suggestion(window, cx);
            }))
            .capture_action(cx.listener(|this, _: &Escape, _, cx| {
                this.escape_suggestions(cx);
            }))
            .capture_action(cx.listener(|this, _: &ShowAddressCompletions, window, cx| {
                this.show_completions(window, cx);
            }))
            .child(field);

        if suggestions_open {
            let mut panel = OverlayPopover::new(
                ("recipient-suggestions-scroll", entity_id),
                px(0.),
                suggestion_top,
                px(440.),
                px(360.),
                self.suggestions_scroll.clone(),
            )
            .constrain_width();

            if suggestions.is_empty() {
                panel = panel.child(
                    div()
                        .px_2()
                        .py_2()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(tr!("search-menu-no-contacts")),
                );
            } else {
                for (index, contact) in suggestions.into_iter().enumerate() {
                    let label = if contact.name.trim().is_empty() {
                        contact.email.clone()
                    } else {
                        contact.name
                    };
                    let email = contact.email;
                    let show_email = !label.eq_ignore_ascii_case(&email);
                    panel = panel.child(
                        h_flex()
                            .id(ElementId::Name(
                                format!("recipient-suggestion-{entity_id}-{index}").into(),
                            ))
                            .w_full()
                            .min_w_0()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1p5()
                            .rounded_sm()
                            .cursor_pointer()
                            .when(index == selected_suggestion, |row| {
                                row.bg(theme.list_active)
                            })
                            .when(index != selected_suggestion, |row| {
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
                                                .child(email),
                                        )
                                    }),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    this.accept_suggestion(index, window, cx);
                                    cx.stop_propagation();
                                }),
                            ),
                    );
                }
            }

            root = root.child(panel);
        }

        root
    }
}

/// Extracts all entries ending with a comma or semicolon
/// and retains the final fragment in the input.
fn split_completed(value: &str) -> (Vec<String>, String) {
    let Some(last_separator) = value.rfind([',', ';']) else {
        return (Vec::new(), value.to_string());
    };
    let completed = super::util::parse_addresses(&value[..=last_separator]);
    let remainder = value[last_separator + 1..].trim_start().to_string();
    (completed, remainder)
}

/// Token being typed: text between the latest comma and the cursor. Returns
/// its starting byte offset and the token without leading whitespace.
fn current_token(before_cursor: &str) -> (usize, &str) {
    let start = before_cursor.rfind(',').map(|i| i + 1).unwrap_or(0);
    let raw = &before_cursor[start..];
    let token = raw.trim_start();
    (start + (raw.len() - token.len()), token)
}

/// Form inserted into the field: `Name <address>`, or the address alone if the
/// contact has no distinct name. Commas and angle brackets in the name are
/// neutralized so they do not break the field's comma separation.
fn insert_text(c: &Contact) -> String {
    let name = c.name.replace([',', '<', '>'], " ");
    let name = name.trim();
    if name.is_empty() || name.eq_ignore_ascii_case(&c.email) {
        c.email.clone()
    } else {
        format!("{name} <{}>", c.email)
    }
}

pub(crate) fn mention_name(contact: &Contact) -> String {
    let name = contact.name.replace(['\n', '\r', '@'], " ");
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() || name.eq_ignore_ascii_case(&contact.email) {
        contact.email.clone()
    } else {
        name
    }
}

fn match_rank(contact: &Contact, needle: &str) -> u8 {
    if needle.is_empty() {
        return 0;
    }
    let name = contact.name.to_lowercase();
    let email = contact.email.to_lowercase();
    if name == needle || email == needle {
        0
    } else if name.starts_with(needle) || email.starts_with(needle) {
        1
    } else if name.split_whitespace().any(|word| word.starts_with(needle)) {
        2
    } else {
        3
    }
}

fn compare_contacts(
    a: &Contact,
    b: &Contact,
    needle: &str,
    usage: &HashMap<String, RecipientUsage>,
) -> Ordering {
    let usage_for = |contact: &Contact| {
        usage
            .get(&contact.email.to_lowercase())
            .map(|entry| (entry.use_count, entry.last_used))
            .unwrap_or_default()
    };
    let a_usage = usage_for(a);
    let b_usage = usage_for(b);
    b_usage
        .0
        .cmp(&a_usage.0)
        .then_with(|| match_rank(a, needle).cmp(&match_rank(b, needle)))
        .then_with(|| b_usage.1.cmp(&a_usage.1))
        .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal))
        .then_with(|| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.email.to_lowercase().cmp(&b.email.to_lowercase()))
        })
}

#[cfg(test)]
mod tests {
    use super::{compare_contacts, mention_name, split_completed, AddressBook, AddressBookState};
    use crate::model::Contact;
    use crate::runtime::RecipientUsage;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    #[test]
    fn completed_recipient_is_split_from_current_input() {
        let (completed, remainder) =
            split_completed("Contact A <contact-a@example.test>, contact-b@example.test");
        assert_eq!(completed, ["Contact A <contact-a@example.test>"]);
        assert_eq!(remainder, "contact-b@example.test");
    }

    #[test]
    fn paste_commits_every_recipient_before_the_last_fragment() {
        let (completed, remainder) = split_completed("a@example.com; b@example.com, c@");
        assert_eq!(completed, ["a@example.com", "b@example.com"]);
        assert_eq!(remainder, "c@");
    }

    #[test]
    fn input_without_separator_stays_editable() {
        let (completed, remainder) = split_completed("contact-a@example.test");
        assert!(completed.is_empty());
        assert_eq!(remainder, "contact-a@example.test");
    }

    #[test]
    fn recipient_usage_frequency_precedes_provider_score_and_search_quality() {
        let frequent = Contact {
            name: "Organisation Contact".into(),
            email: "team@example.test".into(),
            score: 0.1,
        };
        let provider_favorite = Contact {
            name: "Contact A".into(),
            email: "contact-a@example.test".into(),
            score: 1.0,
        };
        let usage = HashMap::from([(
            frequent.email.clone(),
            RecipientUsage {
                email: frequent.email.clone(),
                use_count: 4,
                last_used: 10,
            },
        )]);

        assert!(compare_contacts(&frequent, &provider_favorite, "", &usage).is_lt());
        // Frequency remains the primary key while filtering, as long as both
        // contacts matched the query before reaching the comparator.
        assert!(compare_contacts(&frequent, &provider_favorite, "contact", &usage).is_lt());
    }

    #[test]
    fn mention_name_is_safe_for_inline_insertion() {
        let contact = Contact {
            name: "Contact\n@A".into(),
            email: "contact-a@example.test".into(),
            score: 0.,
        };
        assert_eq!(mention_name(&contact), "Contact A");
    }

    #[test]
    fn mention_ranges_restore_styling_from_the_address_book() {
        let contact = Contact {
            name: "Contact A".into(),
            email: "contact-a@example.test".into(),
            score: 0.,
        };
        let book = AddressBook(Rc::new(RefCell::new(AddressBookState {
            contacts: vec![contact],
            ..AddressBookState::default()
        })));

        assert_eq!(
            book.mention_ranges("Bonjour @Contact A, bienvenue."),
            vec![8..18]
        );
        assert!(book
            .mention_ranges("Bonjour @Contact Additionnel")
            .is_empty());
    }
}
