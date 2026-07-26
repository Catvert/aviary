//! Event composer shared by the calendar's inline right-hand pane and a
//! detachable OS window. Runtime replies are routed by `request_id`, just as
//! they are for mail composers.

use super::account_selector::{
    account_identity_label, account_selector, AccountSelectorKind, AccountSelectorOption,
};
use super::addresses::RecipientInput;
use super::app::AviaryApp;
use super::settings::{EventComposeSession, EventComposeSessionMode};
use super::util;
use crate::model::{AccountId, CalendarEvent, Provider};
use crate::runtime::Cmd;
use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Utc};
use gpui::{div, prelude::*, px, Context, Entity, WeakEntity, Window, WindowHandle};
use gpui_component::{
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    date_picker::{DatePicker, DatePickerState},
    h_flex,
    input::{IndentInline, Input, InputState, OutdentInline},
    v_flex, ActiveTheme, Disableable, Root, Sizable, StyledExt,
};
use tokio::sync::mpsc;

pub struct EventComposeHandle {
    pub id: u64,
    pub window: Option<WindowHandle<Root>>,
    pub view: WeakEntity<EventComposeView>,
}

pub struct InlineEventCompose {
    pub id: u64,
    pub view: Entity<EventComposeView>,
    _subscription: gpui::Subscription,
}

pub enum EventComposeEvent {
    Close,
    Detach,
}

impl gpui::EventEmitter<EventComposeEvent> for EventComposeView {}

#[derive(Clone)]
enum EventComposeMode {
    Create,
    Edit { event_id: String },
}

impl EventComposeMode {
    fn is_edit(&self) -> bool {
        matches!(self, Self::Edit { .. })
    }

    fn title(&self) -> String {
        if self.is_edit() {
            tr!("calendar-edit-event-title").to_string()
        } else {
            tr!("calendar-new-event-title").to_string()
        }
    }
}

impl AviaryApp {
    fn calendar_compose_accounts(&self) -> Vec<(AccountSelectorOption, Provider)> {
        self.ordered_accounts()
            .into_iter()
            .filter(|account| account.provider != Provider::Imap)
            .map(|account| {
                let label = account_identity_label(self.account_label(&account), &account.email);
                let color = util::account_color(
                    &account.id,
                    self.settings
                        .accounts
                        .get(&account.id)
                        .and_then(|settings| settings.color_override),
                );
                (
                    AccountSelectorOption {
                        id: account.id,
                        label,
                        color,
                    },
                    account.provider,
                )
            })
            .collect()
    }

    pub(crate) fn refresh_event_compose_account_options(&mut self, cx: &mut gpui::App) {
        let accounts = self.calendar_compose_accounts();
        for handle in &self.calendar.composes {
            let accounts = accounts.clone();
            let _ = handle.view.update(cx, |view, cx| {
                view.accounts = accounts;
                cx.notify();
            });
        }
    }

    /// Open the default inline composer on the calendar's current date.
    pub fn open_event_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_event_compose_for_day(self.calendar.anchor_date(), window, cx);
    }

    /// Open an inline composer for a day clicked in the month grid. If a
    /// draft is already visible, keep it and move its date pickers to the
    /// clicked day instead of stacking another side pane.
    pub fn open_event_compose_for_day(
        &mut self,
        day: NaiveDate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_event_compose_for_range(day, day, window, cx);
    }

    /// Open an inline composer with an inclusive date range. An existing
    /// creation draft is reused; an edit form is closed so selecting cells
    /// always starts a new calendar event.
    pub fn open_event_compose_for_range(
        &mut self,
        first: NaiveDate,
        last: NaiveDate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (start, end) = if first <= last {
            (first, last)
        } else {
            (last, first)
        };
        let inline = self.calendar.inline_compose.as_ref().map(|inline| {
            (
                inline.id,
                inline.view.clone(),
                inline.view.read(cx).mode.is_edit(),
            )
        });
        if let Some((id, view, editing)) = inline {
            if editing {
                self.close_event_compose(id, cx);
            } else {
                view.update(cx, |view, cx| view.set_range(start, end, window, cx));
                self.calendar.anchor = Some(start);
                self.calendar.select_dates(start, end);
                cx.notify();
                return;
            }
        }

        let accounts = self.calendar_compose_accounts();
        if accounts.is_empty() {
            self.calendar.clear_date_selection();
            self.notify_error(tr!("calendar-no-writable-account"), window, cx);
            return;
        }
        self.calendar.anchor = Some(start);
        self.calendar.select_dates(start, end);
        let default_account = self
            .default_creation_account_id()
            .filter(|account_id| {
                accounts
                    .iter()
                    .any(|(account, _)| &account.id == account_id)
            })
            .unwrap_or_else(|| accounts[0].0.id.clone());
        self.ensure_contacts_for_account(&default_account);

        let id = self.next_editor_id();
        let view = cx.new(|cx| {
            EventComposeView::new(
                id,
                self.cmd_tx.clone(),
                accounts,
                default_account,
                start,
                self.address_book.clone(),
                true,
                EventComposeMode::Create,
                None,
                None,
                window,
                cx,
            )
        });
        if end != start {
            view.update(cx, |view, cx| view.set_range(start, end, window, cx));
        }
        self.register_inline_event_compose(id, view, window, cx);
    }

    fn sync_calendar_inline_compose_selection(&mut self, cx: &gpui::App) {
        let selection = self.calendar.inline_compose.as_ref().and_then(|inline| {
            let compose = inline.view.read(cx);
            if compose.mode.is_edit() {
                None
            } else {
                compose.selected_date_range(cx)
            }
        });
        if let Some((start, end)) = selection {
            self.calendar.select_dates(start, end);
        } else {
            self.calendar.clear_date_selection();
        }
    }

    pub fn open_event_edit(
        &mut self,
        event: CalendarEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self
            .calendar
            .inline_compose
            .as_ref()
            .map(|inline| inline.id)
        {
            self.close_event_compose(id, cx);
        }
        let accounts = self.calendar_compose_accounts();
        if !accounts
            .iter()
            .any(|(account, _)| account.id == event.account_id)
        {
            self.notify_error(tr!("calendar-no-writable-account"), window, cx);
            return;
        }
        let id = self.next_editor_id();
        let mode = EventComposeMode::Edit {
            event_id: event.id.clone(),
        };
        let day = if event.all_day {
            event.start.date_naive()
        } else {
            event.start.with_timezone(&Local).date_naive()
        };
        let view = cx.new(|cx| {
            EventComposeView::new(
                id,
                self.cmd_tx.clone(),
                accounts,
                event.account_id.clone(),
                day,
                self.address_book.clone(),
                true,
                mode,
                Some(event),
                None,
                window,
                cx,
            )
        });
        self.calendar.selected = None;
        self.register_inline_event_compose(id, view, window, cx);
    }

    fn register_inline_event_compose(
        &mut self,
        id: u64,
        view: Entity<EventComposeView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (inputs, attendees, dates) = {
            let compose = view.read(cx);
            (
                [
                    compose.subject.clone(),
                    compose.location.clone(),
                    compose.description.clone(),
                    compose.start_time.clone(),
                    compose.end_time.clone(),
                ],
                compose.attendees.clone(),
                [compose.start_date.clone(), compose.end_date.clone()],
            )
        };
        cx.observe(&view, |this, _, cx| {
            this.session_dirty = true;
            this.sync_calendar_inline_compose_selection(cx);
        })
        .detach();
        for input in inputs {
            cx.observe(&input, |this, _, _| this.session_dirty = true)
                .detach();
        }
        cx.observe(&attendees, |this, _, _| this.session_dirty = true)
            .detach();
        for date in dates {
            cx.observe(&date, |this, _, cx| {
                this.session_dirty = true;
                this.sync_calendar_inline_compose_selection(cx);
            })
            .detach();
        }
        let subscription = cx.subscribe_in(&view, window, Self::on_event_compose_event);
        let focus_view = view.clone();
        cx.on_next_frame(window, move |_, window, cx| {
            focus_view.update(cx, |view, cx| view.focus_subject(window, cx));
        });
        self.calendar.composes.push(EventComposeHandle {
            id,
            window: None,
            view: view.downgrade(),
        });
        self.calendar.inline_compose = Some(InlineEventCompose {
            id,
            view,
            _subscription: subscription,
        });
        self.sync_calendar_inline_compose_selection(cx);
        cx.notify();
    }

    fn on_event_compose_event(
        &mut self,
        view: &Entity<EventComposeView>,
        event: &EventComposeEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self
            .calendar
            .inline_compose
            .as_ref()
            .filter(|inline| inline.view == *view)
            .map(|inline| inline.id)
        else {
            return;
        };
        match event {
            EventComposeEvent::Close => self.close_event_compose(id, cx),
            EventComposeEvent::Detach => self.detach_event_compose(id, cx),
        }
    }

    fn detach_event_compose(&mut self, request_id: u64, cx: &mut Context<Self>) {
        if self
            .calendar
            .inline_compose
            .as_ref()
            .is_none_or(|inline| inline.id != request_id)
        {
            return;
        }
        let inline = self
            .calendar
            .inline_compose
            .take()
            .expect("inline event compose checked above");
        let view = inline.view;
        self.calendar.clear_date_selection();
        let title = view.read(cx).mode.title();
        view.update(cx, |view, cx| {
            view.inline = false;
            cx.notify();
        });
        let bounds = gpui::Bounds::centered(None, gpui::size(px(620.), px(650.)), cx);
        let window = cx.open_window(
            gpui::WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Root::new(view, window, cx)),
        );
        if let Ok(window) = window {
            if let Some(handle) = self
                .calendar
                .composes
                .iter_mut()
                .find(|handle| handle.id == request_id)
            {
                handle.window = Some(window);
            }
        } else {
            self.calendar
                .composes
                .retain(|handle| handle.id != request_id);
        }
        cx.notify();
    }

    pub fn close_event_compose(&mut self, request_id: u64, cx: &mut Context<Self>) {
        if self
            .calendar
            .inline_compose
            .as_ref()
            .is_some_and(|inline| inline.id == request_id)
        {
            self.calendar.inline_compose = None;
            self.calendar.clear_date_selection();
        }
        if let Some(index) = self
            .calendar
            .composes
            .iter()
            .position(|handle| handle.id == request_id)
        {
            let handle = self.calendar.composes.remove(index);
            if let Some(window) = handle.window {
                let _ = window.update(cx, |_, window, _| window.remove_window());
            }
        }
        cx.notify();
    }

    pub fn event_compose_error(&mut self, request_id: u64, error: String, cx: &mut Context<Self>) {
        if let Some(handle) = self
            .calendar
            .composes
            .iter()
            .find(|handle| handle.id == request_id)
        {
            let _ = handle.view.update(cx, |view, cx| view.on_error(error, cx));
        }
    }

    pub(crate) fn event_compose_sessions(&self, cx: &gpui::App) -> Vec<EventComposeSession> {
        let mut sessions = self.pending_event_composes.clone();
        sessions.extend(self.calendar.composes.iter().filter_map(|handle| {
            let view = handle.view.upgrade()?;
            view.read(cx).to_session(handle.window.is_some(), cx)
        }));
        sessions
    }

    pub(crate) fn restore_pending_event_composes(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pending_event_composes.is_empty() {
            return;
        }
        let accounts = self.calendar_compose_accounts();
        let mut still_pending = Vec::new();
        for session in std::mem::take(&mut self.pending_event_composes) {
            if !accounts
                .iter()
                .any(|(account, _)| account.id == session.account_id)
                || (!session.detached && self.calendar.inline_compose.is_some())
            {
                still_pending.push(session);
                continue;
            }
            self.ensure_contacts_for_account(&session.account_id);
            let id = self.next_editor_id();
            let detached = session.detached;
            let mode = match &session.mode {
                EventComposeSessionMode::Create => EventComposeMode::Create,
                EventComposeSessionMode::Edit { event_id } => EventComposeMode::Edit {
                    event_id: event_id.clone(),
                },
            };
            let view = cx.new(|cx| {
                EventComposeView::new(
                    id,
                    self.cmd_tx.clone(),
                    accounts.clone(),
                    session.account_id.clone(),
                    session.start_date,
                    self.address_book.clone(),
                    true,
                    mode,
                    None,
                    Some(session),
                    window,
                    cx,
                )
            });
            let existing_inline = detached
                .then(|| self.calendar.inline_compose.take())
                .flatten();
            self.register_inline_event_compose(id, view, window, cx);
            if detached {
                self.detach_event_compose(id, cx);
                self.calendar.inline_compose = existing_inline;
                self.sync_calendar_inline_compose_selection(cx);
            }
        }
        self.pending_event_composes = still_pending;
    }
}

pub struct EventComposeView {
    id: u64,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    accounts: Vec<(AccountSelectorOption, Provider)>,
    account_id: AccountId,
    subject: Entity<InputState>,
    location: Entity<InputState>,
    attendees: Entity<RecipientInput>,
    description: Entity<InputState>,
    start_date: Entity<DatePickerState>,
    start_time: Entity<InputState>,
    end_date: Entity<DatePickerState>,
    end_time: Entity<InputState>,
    all_day: bool,
    online_meeting: bool,
    submitting: bool,
    error: Option<String>,
    inline: bool,
    mode: EventComposeMode,
}

impl EventComposeView {
    fn focus_subject(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.subject
            .update(cx, |subject, cx| subject.focus(window, cx));
    }

    pub fn refresh_i18n(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.inline {
            window.set_window_title(&self.mode.title());
        }
        for (input, placeholder) in [
            (&self.subject, tr!("calendar-event-title-placeholder")),
            (&self.location, tr!("calendar-new-event-location")),
            (&self.description, tr!("calendar-description-placeholder")),
            (&self.start_time, tr!("calendar-new-event-time-hint")),
            (&self.end_time, tr!("calendar-new-event-time-hint")),
        ] {
            input.update(cx, |state, cx| {
                state.set_placeholder(placeholder.clone(), window, cx);
            });
        }
        self.attendees.update(cx, |input, cx| {
            input.set_placeholder(
                tr!("calendar-attendees-placeholder").to_string(),
                window,
                cx,
            );
        });
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        id: u64,
        cmd_tx: mpsc::UnboundedSender<Cmd>,
        accounts: Vec<(AccountSelectorOption, Provider)>,
        account_id: AccountId,
        day: NaiveDate,
        address_book: super::addresses::AddressBook,
        inline: bool,
        mode: EventComposeMode,
        event: Option<CalendarEvent>,
        restored: Option<EventComposeSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let all_day = restored
            .as_ref()
            .map(|session| session.all_day)
            .unwrap_or_else(|| event.as_ref().is_some_and(|event| event.all_day));
        let event_start_day = || {
            event.as_ref().map_or(day, |event| {
                if event.all_day {
                    event.start.date_naive()
                } else {
                    event.start.with_timezone(&Local).date_naive()
                }
            })
        };
        let event_end_day = || {
            event.as_ref().map_or(day, |event| {
                if event.all_day {
                    let inclusive_end = (event.end - Duration::days(1)).date_naive();
                    inclusive_end.max(event.start.date_naive())
                } else {
                    event.end.with_timezone(&Local).date_naive()
                }
            })
        };
        let start_day = restored
            .as_ref()
            .map(|session| session.start_date)
            .unwrap_or_else(event_start_day);
        let end_day = restored
            .as_ref()
            .map(|session| session.end_date)
            .unwrap_or_else(event_end_day);
        let start_time = restored
            .as_ref()
            .map(|session| session.start_time.clone())
            .or_else(|| {
                event.as_ref().map(|event| {
                    event
                        .start
                        .with_timezone(&Local)
                        .format("%H:%M")
                        .to_string()
                })
            })
            .unwrap_or_else(|| "09:00".to_string());
        let end_time = restored
            .as_ref()
            .map(|session| session.end_time.clone())
            .or_else(|| {
                event
                    .as_ref()
                    .map(|event| event.end.with_timezone(&Local).format("%H:%M").to_string())
            })
            .unwrap_or_else(|| "10:00".to_string());
        let subject = restored
            .as_ref()
            .map(|session| session.subject.clone())
            .or_else(|| event.as_ref().map(|event| event.subject.clone()))
            .unwrap_or_default();
        let location = restored
            .as_ref()
            .map(|session| session.location.clone())
            .or_else(|| event.as_ref().map(|event| event.location.clone()))
            .unwrap_or_default();
        let attendees = restored
            .as_ref()
            .map(|session| session.attendees.clone())
            .unwrap_or_default();
        let description = restored
            .as_ref()
            .map(|session| session.description.clone())
            .or_else(|| event.as_ref().map(|event| event.preview.clone()))
            .unwrap_or_default();
        let online_meeting = restored
            .as_ref()
            .is_some_and(|session| session.online_meeting);
        let input =
            |window: &mut Window, cx: &mut Context<Self>, value: &str, placeholder: String| {
                let value = value.to_string();
                cx.new(move |cx| {
                    InputState::new(window, cx)
                        .placeholder(placeholder)
                        .default_value(value)
                })
            };
        let date_picker = |date: NaiveDate, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(move |cx| {
                let mut state = DatePickerState::new(window, cx).date_format("%d/%m/%Y");
                state.set_date(date, window, cx);
                state
            })
        };
        Self {
            id,
            cmd_tx,
            accounts,
            account_id,
            subject: input(
                window,
                cx,
                &subject,
                tr!("calendar-event-title-placeholder").to_string(),
            ),
            location: input(
                window,
                cx,
                &location,
                tr!("calendar-new-event-location").to_string(),
            ),
            attendees: cx.new(|cx| {
                RecipientInput::new(
                    &attendees,
                    tr!("calendar-attendees-placeholder").to_string(),
                    address_book,
                    window,
                    cx,
                )
                .tab_index(60)
            }),
            description: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .rows(6)
                    .placeholder(tr!("calendar-description-placeholder"))
                    .default_value(description.clone())
            }),
            start_date: date_picker(start_day, window, cx),
            start_time: input(
                window,
                cx,
                &start_time,
                tr!("calendar-new-event-time-hint").to_string(),
            ),
            end_date: date_picker(end_day, window, cx),
            end_time: input(
                window,
                cx,
                &end_time,
                tr!("calendar-new-event-time-hint").to_string(),
            ),
            all_day,
            online_meeting,
            submitting: false,
            error: None,
            inline,
            mode,
        }
    }

    fn set_range(
        &mut self,
        start: NaiveDate,
        end: NaiveDate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_date
            .update(cx, |state, cx| state.set_date(start, window, cx));
        self.end_date
            .update(cx, |state, cx| state.set_date(end, window, cx));
        cx.notify();
    }

    fn selected_date_range(&self, cx: &gpui::App) -> Option<(NaiveDate, NaiveDate)> {
        let start = self.start_date.read(cx).date().start()?;
        let end = self.end_date.read(cx).date().start()?;
        Some(if start <= end {
            (start, end)
        } else {
            (end, start)
        })
    }

    fn to_session(&self, detached: bool, cx: &gpui::App) -> Option<EventComposeSession> {
        if self.submitting {
            return None;
        }
        Some(EventComposeSession {
            detached,
            account_id: self.account_id.clone(),
            subject: self.subject.read(cx).value().to_string(),
            location: self.location.read(cx).value().to_string(),
            attendees: self.attendees.read(cx).serialized(cx),
            description: self.description.read(cx).value().to_string(),
            start_date: self.start_date.read(cx).date().start()?,
            start_time: self.start_time.read(cx).value().to_string(),
            end_date: self.end_date.read(cx).date().start()?,
            end_time: self.end_time.read(cx).value().to_string(),
            all_day: self.all_day,
            online_meeting: self.online_meeting,
            mode: match &self.mode {
                EventComposeMode::Create => EventComposeSessionMode::Create,
                EventComposeMode::Edit { event_id } => EventComposeSessionMode::Edit {
                    event_id: event_id.clone(),
                },
            },
        })
    }

    pub fn on_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.submitting = false;
        self.error = Some(error);
        cx.notify();
    }

    fn to_utc(day: NaiveDate, time: &str, all_day: bool) -> Option<DateTime<Utc>> {
        let time = if all_day {
            chrono::NaiveTime::from_hms_opt(0, 0, 0)?
        } else {
            chrono::NaiveTime::parse_from_str(time.trim(), "%H:%M").ok()?
        };
        Local
            .from_local_datetime(&day.and_time(time))
            .earliest()
            .map(|date| date.with_timezone(&Utc))
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let subject = self.subject.read(cx).value().trim().to_string();
        if subject.is_empty() {
            self.error = Some(tr!("calendar-new-event-error-subject").to_string());
            cx.notify();
            return;
        }
        let start_day = self.start_date.read(cx).date().start();
        let end_day = self.end_date.read(cx).date().start();
        let (Some(start_day), Some(end_day)) = (start_day, end_day) else {
            self.error = Some(tr!("calendar-invalid-dates").to_string());
            cx.notify();
            return;
        };
        let start = Self::to_utc(
            start_day,
            self.start_time.read(cx).value().as_ref(),
            self.all_day,
        );
        let end = Self::to_utc(
            end_day,
            self.end_time.read(cx).value().as_ref(),
            self.all_day,
        );
        let (Some(start), Some(end)) = (start, end) else {
            self.error = Some(tr!("calendar-invalid-dates").to_string());
            cx.notify();
            return;
        };
        if (!self.all_day && end <= start) || (self.all_day && end < start) {
            self.error = Some(tr!("calendar-new-event-error-order").to_string());
            cx.notify();
            return;
        }

        let attendees = if self.mode.is_edit() {
            Vec::new()
        } else {
            let attendees = self.attendees.read(cx).bare_addresses(cx);
            if let Some(invalid) = attendees
                .iter()
                .find(|address| !util::is_valid_email(address))
            {
                self.error =
                    Some(tr!("calendar-new-event-error-attendee", { value: invalid }).to_string());
                cx.notify();
                return;
            }
            attendees
        };
        let description = {
            let markdown = self.description.read(cx).value().to_string();
            if markdown.trim().is_empty() {
                String::new()
            } else {
                let blocks = crate::blocks::markdown_to_blocks(&markdown)
                    .into_iter()
                    .enumerate()
                    .map(|(index, kind)| crate::blocks::Block {
                        id: index as u64 + 1,
                        kind,
                    })
                    .collect::<Vec<_>>();
                crate::blocks::build_html_body(&blocks)
            }
        };
        let online_meeting = if self.online_meeting && !self.mode.is_edit() {
            self.accounts
                .iter()
                .find(|(account, _)| account.id == self.account_id)
                .and_then(|(_, provider)| match provider {
                    Provider::Microsoft => Some(crate::providers::OnlineMeetingKind::Teams),
                    Provider::Google => Some(crate::providers::OnlineMeetingKind::GoogleMeet),
                    Provider::Imap => None,
                })
        } else {
            None
        };

        self.submitting = true;
        self.error = None;
        let event = crate::runtime::EventDraft {
            subject,
            description,
            location: self.location.read(cx).value().trim().to_string(),
            start,
            end,
            all_day: self.all_day,
            online_meeting,
            attendees,
        };
        let command = match &self.mode {
            EventComposeMode::Create => Cmd::CreateEvent {
                request_id: self.id,
                account_id: self.account_id.clone(),
                event,
            },
            EventComposeMode::Edit { event_id } => Cmd::UpdateCalendarEvent {
                request_id: self.id,
                account_id: self.account_id.clone(),
                event_id: event_id.clone(),
                event,
            },
        };
        let _ = self.cmd_tx.send(command);
        cx.notify();
    }
}

impl Render for EventComposeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.inline {
            super::theme::apply_window_scale(window, cx);
        }
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let editing = self.mode.is_edit();
        let text_field = move |label: String, input: &Entity<InputState>, tab_index: isize| {
            v_flex()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(div().text_sm().text_color(muted).child(label))
                .child(Input::new(input).tab_index(tab_index))
        };
        let date_field =
            move |label: String, picker: &Entity<DatePickerState>, tab_index: isize| {
                v_flex()
                    .gap_1()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().text_color(muted).child(label))
                    .child(
                        div()
                            .tab_group()
                            .tab_index(tab_index)
                            .tab_stop(false)
                            .child(DatePicker::new(picker).number_of_months(1).small()),
                    )
            };

        let account_options = self
            .accounts
            .iter()
            .map(|(account, _)| account.clone())
            .collect::<Vec<_>>();
        let account_picker = if editing {
            None
        } else {
            account_selector(
                "event-account",
                &account_options,
                Some(&self.account_id),
                AccountSelectorKind::Calendar,
                80,
                cx.entity(),
                |view: &mut Self, account_id, cx| {
                    view.account_id = account_id;
                    cx.notify();
                },
            )
        };

        let header = self.inline.then(|| {
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(theme.border)
                .child(div().flex_1().font_semibold().child(self.mode.title()))
                .child(
                    Button::new("event-detach")
                        .ghost()
                        .xsmall()
                        .tab_index(95)
                        .disabled(self.submitting)
                        .icon(super::icons::app_icon("external-link"))
                        .tooltip(tr!("calendar-event-detach"))
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(EventComposeEvent::Detach))),
                )
                .child(
                    Button::new("event-close")
                        .ghost()
                        .xsmall()
                        .tab_index(96)
                        .disabled(self.submitting)
                        .icon(super::icons::app_icon("x"))
                        .tooltip(tr!("calendar-new-event-cancel"))
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(EventComposeEvent::Close))),
                )
        });

        let form = v_flex()
            .gap_3()
            .p_3()
            .child(text_field(
                tr!("calendar-event-title-label").to_string(),
                &self.subject,
                10,
            ))
            .child(
                h_flex()
                    .gap_3()
                    .flex_wrap()
                    .child(
                        Checkbox::new("event-all-day")
                            .tab_index(20)
                            .checked(self.all_day)
                            .label(tr!("calendar-new-event-all-day"))
                            .on_click(cx.listener(|view, checked: &bool, _, cx| {
                                view.all_day = *checked;
                                cx.notify();
                            })),
                    )
                    .when(!editing, |row| {
                        row.child(
                            Checkbox::new("event-online")
                                .tab_index(21)
                                .checked(self.online_meeting)
                                .label(tr!("calendar-online-meeting-checkbox"))
                                .on_click(cx.listener(|view, checked: &bool, _, cx| {
                                    view.online_meeting = *checked;
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(date_field(
                        tr!("calendar-new-event-start").to_string(),
                        &self.start_date,
                        30,
                    ))
                    .when(!self.all_day, |row| {
                        row.child(text_field(
                            tr!("calendar-time-label").to_string(),
                            &self.start_time,
                            31,
                        ))
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(date_field(
                        tr!("calendar-new-event-end").to_string(),
                        &self.end_date,
                        40,
                    ))
                    .when(!self.all_day, |row| {
                        row.child(text_field(
                            tr!("calendar-time-label").to_string(),
                            &self.end_time,
                            41,
                        ))
                    }),
            )
            .child(text_field(
                tr!("calendar-new-event-location").to_string(),
                &self.location,
                50,
            ))
            .when(!editing, |form| {
                form.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(tr!("calendar-new-event-attendees")),
                        )
                        .child(self.attendees.clone()),
                )
            })
            .child(
                v_flex()
                    .gap_1()
                    .capture_action(cx.listener(|_, _: &IndentInline, window, cx| {
                        cx.stop_propagation();
                        window.focus_next();
                    }))
                    .capture_action(cx.listener(|_, _: &OutdentInline, window, cx| {
                        cx.stop_propagation();
                        window.focus_prev();
                    }))
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(tr!("calendar-new-event-description")),
                    )
                    .child(Input::new(&self.description).tab_index(70).h(px(150.))),
            )
            .when_some(self.error.clone(), |form, error| {
                form.child(
                    div()
                        .p_2()
                        .rounded(theme.radius)
                        .bg(theme.danger.opacity(0.1))
                        .text_sm()
                        .text_color(theme.danger)
                        .child(error),
                )
            });

        v_flex()
            .tab_group()
            .size_full()
            .min_w_0()
            .bg(theme.background)
            .text_color(theme.foreground)
            .children(header)
            .child(
                div()
                    .id("event-compose-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(form),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .children(account_picker),
                    )
                    .child(
                        Button::new("submit-event")
                            .primary()
                            .tab_index(90)
                            .icon(super::icons::app_icon("calendar"))
                            .label(if self.submitting {
                                if editing {
                                    tr!("status-event-updating")
                                } else {
                                    tr!("status-event-creating")
                                }
                            } else if editing {
                                tr!("calendar-event-save")
                            } else {
                                tr!("calendar-new-event-create")
                            })
                            .disabled(self.submitting)
                            .loading(self.submitting)
                            .on_click(cx.listener(|view, _, _, cx| view.submit(cx))),
                    ),
            )
    }
}
