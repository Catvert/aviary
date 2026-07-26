//! Main-window keyboard shortcuts.
//!
//! Vim commands are active only when enabled in settings and no field, rich
//! editor, or modal panel has focus. Composition shortcuts (`Ctrl/Cmd+Enter`,
//! `Ctrl/Cmd+S`) are registered here but handled by `ComposeView`, whose
//! keyboard context is the most specific one — including for the reply panel,
//! which is a composer like any other.

use super::app::AviaryApp;
use super::compose::ComposeInit;
use super::state::{AuthState, MainView};
use crate::model::{MessageHeader, MessageRef};
use gpui::{actions, App, Context, KeyBinding, KeyContext, Window};

actions!(
    aviary_shortcuts,
    [
        NewMessage,
        Refresh,
        FocusSearch,
        BlurSearch,
        ShowMail,
        ShowCalendar,
        ShowKanban,
        ShowContacts,
        ShowSettings,
        PreviousView,
        NextView,
        PreviousItem,
        NextItem,
        FirstItem,
        LastItem,
        ReplyMessage,
        ReplyAll,
        ForwardMessage,
        OpenQuickActions,
        PrintMessage,
        SelectAllMessages,
        ClearMessageSelection,
        ArchiveMessage,
        DeleteMessage,
        ToggleFlag,
        MarkUnread,
        CloseCurrent,
        SendCompose,
        SaveDraft
    ]
);

const SAFE: &str = "Aviary && !Dialog && !Sheet && !PopupMenu && !Popover";
const MAIL_SAFE: &str = "Aviary && !Compose && !Dialog && !Sheet && !PopupMenu && !Popover";
const LIST_SAFE: &str =
    "Aviary && !Input && !BlockEditor && !Compose && !Dialog && !Sheet && !PopupMenu && !Popover";
const VIM_SAFE: &str = "Aviary && vim_mode == enabled && !Input && !BlockEditor && !Compose && !Dialog && !Sheet && !PopupMenu && !Popover";

/// Registers the keymap once. Context predicates decide at
/// each frame whether Vim commands are actually available.
pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        // Global commands, independent of Vim mode.
        KeyBinding::new("secondary-n", NewMessage, Some(SAFE)),
        KeyBinding::new("f5", Refresh, Some(SAFE)),
        KeyBinding::new("secondary-f", FocusSearch, Some(SAFE)),
        KeyBinding::new("secondary-1", ShowMail, Some(SAFE)),
        KeyBinding::new("secondary-2", ShowCalendar, Some(SAFE)),
        KeyBinding::new("secondary-3", ShowKanban, Some(SAFE)),
        KeyBinding::new("secondary-4", ShowContacts, Some(SAFE)),
        KeyBinding::new("secondary-5", ShowSettings, Some(SAFE)),
        KeyBinding::new("secondary-,", ShowSettings, Some(SAFE)),
        KeyBinding::new("secondary-w", CloseCurrent, Some(SAFE)),
        KeyBinding::new("secondary-r", ReplyMessage, Some(MAIL_SAFE)),
        KeyBinding::new("secondary-shift-r", ReplyAll, Some(MAIL_SAFE)),
        KeyBinding::new("secondary-shift-f", ForwardMessage, Some(MAIL_SAFE)),
        KeyBinding::new("secondary-p", PrintMessage, Some(MAIL_SAFE)),
        KeyBinding::new("up", PreviousItem, Some(LIST_SAFE)),
        KeyBinding::new("down", NextItem, Some(LIST_SAFE)),
        KeyBinding::new("home", FirstItem, Some(LIST_SAFE)),
        KeyBinding::new("end", LastItem, Some(LIST_SAFE)),
        KeyBinding::new("secondary-a", SelectAllMessages, Some(LIST_SAFE)),
        KeyBinding::new("escape", ClearMessageSelection, Some(LIST_SAFE)),
        // Archiving removes the message from the current view like deleting
        // does, so it follows `delete`'s context rather than the reply/print
        // one: never while a text field or the block editor has focus.
        KeyBinding::new("secondary-e", ArchiveMessage, Some(LIST_SAFE)),
        KeyBinding::new("delete", DeleteMessage, Some(LIST_SAFE)),
        // Escape returns control to command mode after a keyboard search.
        // These rules, added after the Input rule, return control to command
        // mode from Mail and Contacts search fields.
        KeyBinding::new("escape", BlurSearch, Some("MailSearch > Input")),
        KeyBinding::new("escape", BlurSearch, Some("ContactsSearch > Input")),
        // Optional Vim commands. `g ...` sequences can coexist:
        // gpui waits for the second key before choosing the action.
        KeyBinding::new("j", NextItem, Some(VIM_SAFE)),
        KeyBinding::new("k", PreviousItem, Some(VIM_SAFE)),
        KeyBinding::new("g g", FirstItem, Some(VIM_SAFE)),
        KeyBinding::new("shift-g", LastItem, Some(VIM_SAFE)),
        KeyBinding::new("h", PreviousView, Some(VIM_SAFE)),
        KeyBinding::new("l", NextView, Some(VIM_SAFE)),
        KeyBinding::new("g m", ShowMail, Some(VIM_SAFE)),
        KeyBinding::new("g c", ShowCalendar, Some(VIM_SAFE)),
        KeyBinding::new("g k", ShowKanban, Some(VIM_SAFE)),
        KeyBinding::new("g a", ShowContacts, Some(VIM_SAFE)),
        KeyBinding::new("g p", ShowSettings, Some(VIM_SAFE)),
        KeyBinding::new("c", NewMessage, Some(VIM_SAFE)),
        KeyBinding::new("/", FocusSearch, Some(VIM_SAFE)),
        KeyBinding::new("r", ReplyMessage, Some(VIM_SAFE)),
        KeyBinding::new("shift-r", ReplyAll, Some(VIM_SAFE)),
        KeyBinding::new("f", ForwardMessage, Some(VIM_SAFE)),
        KeyBinding::new("a", OpenQuickActions, Some(VIM_SAFE)),
        KeyBinding::new("e", ArchiveMessage, Some(VIM_SAFE)),
        KeyBinding::new("d", DeleteMessage, Some(VIM_SAFE)),
        KeyBinding::new("s", ToggleFlag, Some(VIM_SAFE)),
        KeyBinding::new("u", MarkUnread, Some(VIM_SAFE)),
        KeyBinding::new("q", CloseCurrent, Some(VIM_SAFE)),
        // Composer and inline reply: descendant variants take precedence over
        // `Input` so Ctrl/Cmd+Enter does not insert a line.
        KeyBinding::new("secondary-enter", SendCompose, Some("Compose")),
        KeyBinding::new(
            "secondary-enter",
            SendCompose,
            Some("Compose > BlockEditor"),
        ),
        KeyBinding::new("secondary-enter", SendCompose, Some("Compose > Input")),
        KeyBinding::new("secondary-s", SaveDraft, Some("Compose")),
    ]);
}

pub(crate) fn main_context(vim_enabled: bool) -> KeyContext {
    let mut context = KeyContext::default();
    context.add("Aviary");
    context.set("vim_mode", if vim_enabled { "enabled" } else { "disabled" });
    context
}

fn authenticated(this: &AviaryApp) -> bool {
    matches!(this.auth, AuthState::Authenticated)
}

fn current_header(this: &AviaryApp) -> Option<MessageHeader> {
    if this.view != MainView::Mail || this.active_compose_tab().is_some() {
        return None;
    }
    if let Some(message) = this.displayed_message() {
        return Some(message.header.clone());
    }
    let selected = this.mailbox.selected_id.as_deref()?;
    this.mailbox
        .search
        .results
        .as_ref()
        .into_iter()
        .flatten()
        .chain(this.mailbox.messages.iter())
        .find(|message| message.id == selected)
        .cloned()
}

pub(crate) fn new_message(
    this: &mut AviaryApp,
    _: &NewMessage,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    if authenticated(this) {
        this.open_inline_compose(ComposeInit::blank(), window, cx);
    }
}

pub(crate) fn refresh(
    this: &mut AviaryApp,
    _: &Refresh,
    _: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    if !authenticated(this) {
        return;
    }
    match this.view {
        MainView::Mail => this.send_refresh(),
        MainView::Calendar => this.calendar.force_reload(),
        MainView::Kanban => this.reload_kanban(),
        MainView::Contacts => this.reload_contacts(),
        MainView::Settings => {}
    }
    cx.notify();
}

pub(crate) fn focus_search(
    this: &mut AviaryApp,
    _: &FocusSearch,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    if !authenticated(this) {
        return;
    }
    if this.view == MainView::Contacts {
        this.contacts_search_input
            .update(cx, |state, cx| state.focus(window, cx));
    } else {
        this.enter_main_view(MainView::Mail, cx);
        this.search_input
            .update(cx, |state, cx| state.focus(window, cx));
    }
}

pub(crate) fn blur_search(
    this: &mut AviaryApp,
    _: &BlurSearch,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    this.focus_shortcuts(window);
    cx.notify();
}

fn show_view(
    this: &mut AviaryApp,
    view: MainView,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    if view == MainView::Settings || authenticated(this) {
        this.enter_main_view(view, cx);
        this.focus_shortcuts(window);
    }
}

macro_rules! view_handler {
    ($name:ident, $action:ty, $view:expr) => {
        pub(crate) fn $name(
            this: &mut AviaryApp,
            _: &$action,
            window: &mut Window,
            cx: &mut Context<AviaryApp>,
        ) {
            show_view(this, $view, window, cx);
        }
    };
}

view_handler!(show_mail, ShowMail, MainView::Mail);
view_handler!(show_calendar, ShowCalendar, MainView::Calendar);
view_handler!(show_kanban, ShowKanban, MainView::Kanban);
view_handler!(show_contacts, ShowContacts, MainView::Contacts);
view_handler!(show_settings, ShowSettings, MainView::Settings);

fn cycle_view(
    this: &mut AviaryApp,
    delta: isize,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    let views = [
        MainView::Mail,
        MainView::Calendar,
        MainView::Kanban,
        MainView::Contacts,
        MainView::Settings,
    ];
    let current = views
        .iter()
        .position(|view| *view == this.view)
        .unwrap_or(0);
    let target = (current as isize + delta).rem_euclid(views.len() as isize) as usize;
    show_view(this, views[target], window, cx);
}

pub(crate) fn previous_view(
    this: &mut AviaryApp,
    _: &PreviousView,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    cycle_view(this, -1, window, cx);
}

pub(crate) fn next_view(
    this: &mut AviaryApp,
    _: &NextView,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    cycle_view(this, 1, window, cx);
}

#[derive(Clone, Copy)]
pub(super) enum ListMovement {
    Previous,
    Next,
    First,
    Last,
}

fn move_selection(this: &mut AviaryApp, movement: ListMovement, cx: &mut Context<AviaryApp>) {
    match this.view {
        MainView::Mail => this.navigate_messages(movement, cx),
        MainView::Calendar => this.navigate_calendar_events(movement, cx),
        MainView::Contacts => this.navigate_contacts(movement, cx),
        MainView::Kanban | MainView::Settings => {}
    }
}

macro_rules! movement_handler {
    ($name:ident, $action:ty, $movement:expr) => {
        pub(crate) fn $name(
            this: &mut AviaryApp,
            _: &$action,
            _: &mut Window,
            cx: &mut Context<AviaryApp>,
        ) {
            move_selection(this, $movement, cx);
        }
    };
}

movement_handler!(previous_item, PreviousItem, ListMovement::Previous);
movement_handler!(next_item, NextItem, ListMovement::Next);
movement_handler!(first_item, FirstItem, ListMovement::First);
movement_handler!(last_item, LastItem, ListMovement::Last);

/// The unmodified reply shortcut follows the configured primary action. When
/// reply-all is primary, swap the two shortcuts so the modified binding still
/// provides the alternative action.
fn shortcut_uses_reply_all(explicit_reply_all: bool, reply_all_primary: bool) -> bool {
    explicit_reply_all ^ reply_all_primary
}

fn start_shortcut_reply(
    this: &mut AviaryApp,
    explicit_reply_all: bool,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    if this.view != MainView::Mail || this.active_compose_tab().is_some() {
        return;
    }
    let Some(message) = this.displayed_message() else {
        return;
    };
    if this.offline_accounts.contains(&message.header.account_id) {
        return;
    }
    if shortcut_uses_reply_all(explicit_reply_all, this.settings.global.reply_all_primary) {
        this.start_inline_reply_all(&message, window, cx);
    } else {
        this.start_inline_reply(&message, window, cx);
    }
}

pub(crate) fn reply_message(
    this: &mut AviaryApp,
    _: &ReplyMessage,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    start_shortcut_reply(this, false, window, cx);
}

pub(crate) fn reply_all(
    this: &mut AviaryApp,
    _: &ReplyAll,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    start_shortcut_reply(this, true, window, cx);
}

pub(crate) fn forward_message(
    this: &mut AviaryApp,
    _: &ForwardMessage,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    if this.view != MainView::Mail || this.active_compose_tab().is_some() {
        return;
    }
    let Some(message) = this.displayed_message() else {
        return;
    };
    if !this.offline_accounts.contains(&message.header.account_id) {
        this.open_inline_compose(
            ComposeInit::forward(message.header.account_id.clone(), &message),
            window,
            cx,
        );
    }
}

pub(crate) fn open_quick_actions(
    this: &mut AviaryApp,
    _: &OpenQuickActions,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    if this.view != MainView::Mail || this.active_compose_tab().is_some() {
        return;
    }
    let Some(message) = this.displayed_message() else {
        return;
    };
    this.open_quick_action_menu(MessageRef::from(message.as_ref()), "viewer", window, cx);
}

pub(crate) fn print_message(
    this: &mut AviaryApp,
    _: &PrintMessage,
    _: &mut Window,
    _: &mut Context<AviaryApp>,
) {
    if this.view != MainView::Mail || this.active_compose_tab().is_some() {
        return;
    }
    if let Some(message) = this.displayed_message() {
        super::viewer::print_message((*message).clone(), this.settings.global.show_remote_images);
    }
}

pub(crate) fn archive_message(
    this: &mut AviaryApp,
    _: &ArchiveMessage,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    let selected = this.selected_message_headers();
    if !selected.is_empty() {
        if selected
            .iter()
            .any(|header| this.offline_accounts.contains(&header.account_id))
        {
            return;
        }
        let references: Vec<_> = selected
            .iter()
            .map(|header| MessageRef {
                account_id: header.account_id.clone(),
                id: header.id.clone(),
            })
            .collect();
        this.bulk_archive_messages_with_undo(references, window, cx);
        cx.notify();
        return;
    }
    let Some(header) = current_header(this) else {
        return;
    };
    if this.offline_accounts.contains(&header.account_id) {
        return;
    }
    this.archive_message_with_undo(header.account_id, &header.id, window, cx);
    cx.notify();
}

pub(crate) fn delete_message(
    this: &mut AviaryApp,
    _: &DeleteMessage,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    let selected = this.selected_message_headers();
    if !selected.is_empty() {
        if selected
            .iter()
            .any(|header| this.offline_accounts.contains(&header.account_id))
        {
            return;
        }
        let references: Vec<_> = selected
            .iter()
            .map(|header| MessageRef {
                account_id: header.account_id.clone(),
                id: header.id.clone(),
            })
            .collect();
        this.bulk_delete_messages_with_undo(references, window, cx);
        cx.notify();
        return;
    }
    let Some(header) = current_header(this) else {
        return;
    };
    if this.offline_accounts.contains(&header.account_id) {
        return;
    }
    this.delete_message_with_undo(header.account_id, &header.id, window, cx);
    cx.notify();
}

pub(crate) fn toggle_flag(
    this: &mut AviaryApp,
    _: &ToggleFlag,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    let selected = this.selected_message_headers();
    if !selected.is_empty() {
        let should_flag = selected.iter().any(|header| !header.is_flagged);
        let items = selected
            .into_iter()
            .map(|header| {
                (
                    MessageRef {
                        account_id: header.account_id,
                        id: header.id,
                    },
                    header.is_flagged,
                )
            })
            .collect();
        this.bulk_set_flag_undoable(items, should_flag, window, cx);
        cx.notify();
        return;
    }
    let Some(header) = current_header(this) else {
        return;
    };
    if this.offline_accounts.contains(&header.account_id) {
        return;
    }
    this.set_flag_undoable(
        MessageRef {
            account_id: header.account_id,
            id: header.id,
        },
        !header.is_flagged,
        header.is_flagged,
        window,
        cx,
    );
    cx.notify();
}

pub(crate) fn mark_unread(
    this: &mut AviaryApp,
    _: &MarkUnread,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    let selected = this.selected_message_headers();
    if !selected.is_empty() {
        let items = selected
            .into_iter()
            .map(|header| {
                (
                    MessageRef {
                        account_id: header.account_id,
                        id: header.id,
                    },
                    header.is_read,
                )
            })
            .collect();
        this.bulk_mark_read_undoable(items, false, window, cx);
        cx.notify();
        return;
    }
    let Some(header) = current_header(this) else {
        return;
    };
    if !header.is_read || this.offline_accounts.contains(&header.account_id) {
        return;
    }
    this.mark_read_undoable(
        MessageRef {
            account_id: header.account_id,
            id: header.id,
        },
        false,
        true,
        window,
        cx,
    );
    cx.notify();
}

pub(crate) fn select_all_messages(
    this: &mut AviaryApp,
    _: &SelectAllMessages,
    _: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    if this.view == MainView::Mail {
        this.select_all_visible_messages();
        cx.notify();
    }
}

pub(crate) fn clear_message_selection(
    this: &mut AviaryApp,
    _: &ClearMessageSelection,
    _: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    if this.view == MainView::Mail && !this.mailbox.selected_messages.is_empty() {
        this.clear_message_selection();
        cx.notify();
    }
}

pub(crate) fn close_current(
    this: &mut AviaryApp,
    _: &CloseCurrent,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) {
    if this.view != MainView::Mail {
        return;
    }
    let visible_reply = this
        .displayed_message()
        .zip(this.inline_reply.as_ref())
        .filter(|(message, reply)| message.header.id == reply.message_id)
        .map(|(_, reply)| reply.compose_id);
    if let Some(compose_id) = visible_reply {
        this.close_compose(compose_id, cx);
        this.focus_shortcuts(window);
        cx.notify();
        return;
    }
    let Some(index) = this.mailbox.active_tab else {
        return;
    };
    // `close_viewer_tab` checks the index and also releases the entity for the
    // composer inline.
    this.close_viewer_tab(index);
    this.focus_shortcuts(window);
    cx.notify();
}

#[cfg(test)]
mod tests {
    use super::{main_context, shortcut_uses_reply_all, LIST_SAFE, MAIL_SAFE, SAFE, VIM_SAFE};
    use gpui::{KeyBindingContextPredicate, KeyContext, Keystroke};

    #[test]
    fn built_in_shortcut_syntaxes_are_valid() {
        for keystroke in [
            "a",
            "secondary-,",
            "secondary-enter",
            "secondary-shift-f",
            "g",
            "/",
            "shift-g",
        ] {
            Keystroke::parse(keystroke).expect("valid built-in keystroke");
        }
        for context in [
            SAFE,
            MAIL_SAFE,
            LIST_SAFE,
            VIM_SAFE,
            "MailSearch > Input",
            "ContactsSearch > Input",
            "Compose > Input",
            "Compose > BlockEditor",
        ] {
            KeyBindingContextPredicate::parse(context).expect("valid built-in key context");
        }
    }

    #[test]
    fn vim_mode_never_captures_input_or_dialogs() {
        let predicate = KeyBindingContextPredicate::parse(VIM_SAFE).expect("vim context");
        let enabled = main_context(true);
        let disabled = main_context(false);
        assert!(predicate.depth_of(std::slice::from_ref(&enabled)).is_some());
        assert!(predicate.depth_of(&[disabled]).is_none());

        // The reply panel is a `Compose` surface like any other composer, so
        // that one name covers every place text is entered in the reader.
        for child_name in ["Input", "BlockEditor", "Compose", "Dialog"] {
            let child = KeyContext::parse(child_name).expect("child context");
            assert!(
                predicate.depth_of(&[enabled.clone(), child]).is_none(),
                "Vim must be disabled inside {child_name}"
            );
        }
    }

    #[test]
    fn reply_shortcuts_follow_the_configured_primary_action() {
        assert!(!shortcut_uses_reply_all(false, false));
        assert!(shortcut_uses_reply_all(true, false));
        assert!(shortcut_uses_reply_all(false, true));
        assert!(!shortcut_uses_reply_all(true, true));
    }
}
