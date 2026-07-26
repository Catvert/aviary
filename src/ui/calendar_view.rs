//! Calendar view: list grouped by day (with an "upcoming days" sub-mode) or a
//! continuously scrolling week grid, event details, detachable inline
//! creation, and moving events between days.

use super::app::AviaryApp;
use super::event_compose::{EventComposeHandle, InlineEventCompose};
use super::settings::CalendarLayout;
use super::util;
use crate::model::{AccountId, CalendarEvent, Provider};
use crate::runtime::Cmd;
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Utc};
use gpui::{
    div, point, prelude::*, px, Context, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels,
    Point, ScrollWheelEvent, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    menu::{ContextMenuExt, PopupMenuItem},
    v_flex, v_virtual_list, ActiveTheme, Disableable, IconName, Selectable, Sizable, StyledExt,
    WindowExt,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

#[derive(Debug, Clone, Default)]
pub struct IcalSyncStatus {
    pub syncing: bool,
    pub error: Option<String>,
    pub last_success: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CalendarRange {
    Day,
    #[default]
    Week,
    Month,
    /// The next `calendar_upcoming_days` days starting today (list layout).
    Upcoming,
}

/// Fallback height of one week row in the continuously scrolling grid, used
/// until the viewport has been measured. Afterwards the height is derived
/// from the viewport so `calendar_grid_weeks` rows fit on screen.
const GRID_ROW_H: f32 = 128.;
/// Lower bound keeping rows usable when many weeks are shown at once.
const MIN_GRID_ROW_H: f32 = 60.;
/// Weeks kept on each side of today's week in the virtualized grid window
/// (about ten years of runway per direction; scrolling clamps at the edges
/// and explicit navigation stays within the window in practice).
const GRID_WEEKS_BEFORE: i64 = 520;
const GRID_WEEKS: usize = GRID_WEEKS_BEFORE as usize * 2 + 1;
/// Extra weeks fetched around the viewport so scrolling rarely waits.
const GRID_PREFETCH_WEEKS: usize = 5;
/// Above this many `(scope, month)` cache entries, chunks and events far from
/// the viewed period are evicted to bound memory during long scrolls.
const MAX_CHUNK_KEYS: usize = 120;
const KEEP_CHUNK_RADIUS: i32 = 24;

/// Sequential month index (`year * 12 + month0`) used as fetch-chunk key.
fn month_chunk(day: NaiveDate) -> i32 {
    day.year() * 12 + day.month0() as i32
}

fn chunk_start(chunk: i32) -> NaiveDate {
    NaiveDate::from_ymd_opt(chunk.div_euclid(12), chunk.rem_euclid(12) as u32 + 1, 1)
        .expect("valid chunk month")
}

/// UTC overlap between a stored event and a fetched window. Zero-length
/// events count as one second so they belong to exactly one window.
fn overlaps_utc(event: &CalendarEvent, from: DateTime<Utc>, to: DateTime<Utc>) -> bool {
    let end = if event.end > event.start {
        event.end
    } else {
        event.start + Duration::seconds(1)
    };
    event.start < to && end > from
}

pub struct CalendarViewState {
    pub events: Vec<CalendarEvent>,
    pub range: CalendarRange,
    pub layout: CalendarLayout,
    pub anchor: Option<NaiveDate>,
    /// Inclusive date range currently targeted by the inline event composer.
    pub date_selection: Option<(NaiveDate, NaiveDate)>,
    /// First cell of an in-progress pointer selection in the month grid.
    pub date_drag_anchor: Option<NaiveDate>,
    pub selected: Option<String>,
    /// Month chunks already requested, keyed by calendar scope (account id or
    /// `ical:{id}`). Cleared by [`Self::force_reload`] to force a refetch.
    loaded_chunks: HashMap<String, HashSet<i32>>,
    pub loading: bool,
    pub composes: Vec<EventComposeHandle>,
    pub inline_compose: Option<InlineEventCompose>,
    pub moving: HashSet<(AccountId, String)>,
    pub deleting: HashSet<(AccountId, String)>,
    pub ical_sync: HashMap<String, IcalSyncStatus>,
    /// Monday of the first week of the virtualized grid window.
    pub(super) grid_origin: NaiveDate,
    /// First week row visible in the grid viewport, captured at prepaint.
    pub(super) grid_first_visible: usize,
    /// Number of week rows covered by the grid viewport.
    pub(super) grid_visible_rows: usize,
    /// Date whose week the grid scrolls to on the next render.
    pub(super) grid_pending_scroll: Option<NaiveDate>,
    /// Week-row height currently in use, derived from the measured viewport
    /// so the configured number of weeks fits on screen.
    pub(super) grid_row_h: f32,
    week_cache: HashMap<(NaiveDate, Vec<String>), Rc<WeekRenderData>>,
}

impl CalendarViewState {
    pub fn new(layout: CalendarLayout) -> Self {
        let today = Local::now().date_naive();
        Self {
            events: Vec::new(),
            range: if layout == CalendarLayout::Calendar {
                CalendarRange::Month
            } else {
                CalendarRange::Week
            },
            layout,
            anchor: None,
            date_selection: None,
            date_drag_anchor: None,
            selected: None,
            loaded_chunks: HashMap::new(),
            loading: false,
            composes: Vec::new(),
            inline_compose: None,
            moving: HashSet::new(),
            deleting: HashSet::new(),
            ical_sync: HashMap::new(),
            grid_origin: start_of_week(today) - Duration::weeks(GRID_WEEKS_BEFORE),
            grid_first_visible: GRID_WEEKS_BEFORE as usize,
            grid_visible_rows: 6,
            grid_pending_scroll: Some(today),
            grid_row_h: GRID_ROW_H,
            week_cache: HashMap::new(),
        }
    }

    /// Merges a fetched window into the cache: fetched windows are the source
    /// of truth for their period, so stored events overlapping it are
    /// replaced. Every backend returns all events *overlapping* the request,
    /// so an event spanning two windows reappears in each response.
    fn merge_events(
        &mut self,
        account_id: &AccountId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        events: Vec<CalendarEvent>,
    ) {
        self.events
            .retain(|e| &e.account_id != account_id || !overlaps_utc(e, from, to));
        self.events.extend(events);
        self.events.sort_by_key(|a| a.start);
        self.week_cache.clear();
        self.loading = false;
    }

    pub fn on_events(
        &mut self,
        account_id: &AccountId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        events: Vec<CalendarEvent>,
    ) {
        self.merge_events(account_id, from, to, events);
    }

    pub fn force_reload(&mut self) {
        self.loaded_chunks.clear();
        self.week_cache.clear();
    }

    pub(super) fn invalidate_event_layouts(&mut self) {
        self.week_cache.clear();
    }

    pub fn on_ical_events(
        &mut self,
        subscription_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        events: Vec<CalendarEvent>,
    ) {
        let account_id = AccountId(format!("ical:{subscription_id}"));
        self.merge_events(&account_id, from, to, events);
    }

    /// Marks a chunk as requested; returns `false` when it already was.
    fn mark_chunk(&mut self, scope: &str, chunk: i32) -> bool {
        if let Some(chunks) = self.loaded_chunks.get_mut(scope) {
            chunks.insert(chunk)
        } else {
            self.loaded_chunks
                .insert(scope.to_string(), HashSet::from([chunk]));
            true
        }
    }

    /// Contiguous runs of months in `first..=last` not yet requested for
    /// `scope`; the returned runs are marked as requested.
    fn missing_runs(&mut self, scope: &str, first: i32, last: i32) -> Vec<(i32, i32)> {
        let mut runs = Vec::new();
        let mut run: Option<(i32, i32)> = None;
        for chunk in first..=last {
            if self.mark_chunk(scope, chunk) {
                run = Some(run.map_or((chunk, chunk), |(start, _)| (start, chunk)));
            } else if let Some(done) = run.take() {
                runs.push(done);
            }
        }
        runs.extend(run);
        runs
    }

    /// Bounds cache memory during long scrolls: keeps only the chunks (and
    /// their events) within [`KEEP_CHUNK_RADIUS`] months of the viewed period.
    fn evict_distant_chunks(&mut self, center: i32) {
        let total: usize = self.loaded_chunks.values().map(HashSet::len).sum();
        if total <= MAX_CHUNK_KEYS {
            return;
        }
        for chunks in self.loaded_chunks.values_mut() {
            chunks.retain(|chunk| (chunk - center).abs() <= KEEP_CHUNK_RADIUS);
        }
        let from = to_utc(chunk_start(center - KEEP_CHUNK_RADIUS));
        let to = to_utc(chunk_start(center + KEEP_CHUNK_RADIUS + 1));
        self.events.retain(|event| overlaps_utc(event, from, to));
        self.week_cache.clear();
    }

    /// Row index of the week containing `date`, clamped to the grid window.
    pub(super) fn week_index(&self, date: NaiveDate) -> usize {
        let days = (start_of_week(date) - self.grid_origin).num_days();
        days.div_euclid(7).clamp(0, GRID_WEEKS as i64 - 1) as usize
    }

    /// Schedules a grid jump; the viewport estimate moves immediately so the
    /// same frame already fetches the target period.
    pub(super) fn grid_scroll_to(&mut self, date: NaiveDate) {
        self.grid_pending_scroll = Some(date);
        self.grid_first_visible = self.week_index(date);
    }

    /// Local date at the center of the grid viewport (Thursday of the middle
    /// visible week, so the title month matches the dominant visible month).
    fn grid_center_date(&self) -> NaiveDate {
        let center = self.grid_first_visible + self.grid_visible_rows / 2;
        let week = center.min(GRID_WEEKS - 1);
        self.grid_origin + Duration::weeks(week as i64) + Duration::days(3)
    }

    pub(super) fn anchor_date(&self) -> NaiveDate {
        self.anchor.unwrap_or_else(|| Local::now().date_naive())
    }

    pub(super) fn select_dates(&mut self, first: NaiveDate, last: NaiveDate) {
        self.date_selection = Some(ordered_date_range(first, last));
    }

    pub(super) fn clear_date_selection(&mut self) {
        self.date_selection = None;
        self.date_drag_anchor = None;
    }

    fn date_is_selected(&self, day: NaiveDate) -> bool {
        self.date_selection
            .is_some_and(|(start, end)| day >= start && day <= end)
    }

    fn range_bounds(&self, upcoming_days: i64) -> (NaiveDate, NaiveDate) {
        let anchor = self.anchor_date();
        match self.range {
            CalendarRange::Day => (anchor, anchor + Duration::days(1)),
            CalendarRange::Week => {
                let start = start_of_week(anchor);
                (start, start + Duration::days(7))
            }
            CalendarRange::Month => {
                let first = anchor.with_day(1).expect("valid day 1");
                let next = if first.month() == 12 {
                    NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)
                } else {
                    NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)
                }
                .expect("valid next month");
                (first, next)
            }
            CalendarRange::Upcoming => {
                let today = Local::now().date_naive();
                (today, today + Duration::days(upcoming_days))
            }
        }
    }
}

impl Default for CalendarViewState {
    fn default() -> Self {
        Self::new(CalendarLayout::default())
    }
}

#[derive(Clone)]
struct EventDrag {
    event: CalendarEvent,
    displayed_day: NaiveDate,
}

fn start_of_week(d: NaiveDate) -> NaiveDate {
    d - Duration::days(d.weekday().num_days_from_monday() as i64)
}

/// First displayed week (a Monday) and number of weeks (4 to 6) of the paged
/// month grid containing `anchor`.
fn paged_month_weeks(anchor: NaiveDate) -> (NaiveDate, i64) {
    let first = anchor.with_day(1).expect("valid day 1");
    let next = if first.month() == 12 {
        NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)
    }
    .expect("valid next month");
    let start = start_of_week(first);
    let weeks = ((next - start).num_days() + 6) / 7;
    (start, weeks)
}

fn ordered_date_range(first: NaiveDate, last: NaiveDate) -> (NaiveDate, NaiveDate) {
    if first <= last {
        (first, last)
    } else {
        (last, first)
    }
}

fn google_maps_search_url(location: &str) -> String {
    format!(
        "https://www.google.com/maps/search/?api=1&query={}",
        urlencoding::encode(location.trim())
    )
}

fn to_utc(d: NaiveDate) -> DateTime<Utc> {
    Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).expect("valid midnight"))
        .single()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).expect("valid midnight")))
}

fn event_day(event: &CalendarEvent) -> NaiveDate {
    if event.all_day {
        event.start.date_naive()
    } else {
        event.start.with_timezone(&Local).date_naive()
    }
}

/// Whether an event occupies any part of a local calendar-date range. RFC
/// 5545 all-day `DTEND` values are exclusive, so an event ending on July 9 is
/// displayed through July 8.
fn event_occurs_in_range(event: &CalendarEvent, from: NaiveDate, to: NaiveDate) -> bool {
    if event.all_day {
        let start = event.start.date_naive();
        let parsed_end = event.end.date_naive();
        let end = if parsed_end > start {
            parsed_end
        } else {
            start + Duration::days(1)
        };
        start < to && end > from
    } else {
        let from = to_utc(from);
        let to = to_utc(to);
        if event.end > event.start {
            event.start < to && event.end > from
        } else {
            event.start >= from && event.start < to
        }
    }
}

fn event_occurs_on_day(event: &CalendarEvent, day: NaiveDate) -> bool {
    event_occurs_in_range(event, day, day + Duration::days(1))
}

fn all_day_last_day(event: &CalendarEvent) -> NaiveDate {
    let start = event.start.date_naive();
    let end = event.end.date_naive();
    if end > start {
        end.pred_opt().unwrap_or(start)
    } else {
        start
    }
}

const MONTH_EVENT_TOP: f32 = 24.;
const MONTH_EVENT_HEIGHT: f32 = 24.;
const MONTH_EVENT_STEP: f32 = 26.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WeekEventLayout {
    event_index: usize,
    start_col: u16,
    span: u16,
    lane: u16,
    continues_before: bool,
    continues_after: bool,
}

struct WeekRenderData {
    events: Vec<CalendarEvent>,
    layouts: Vec<WeekEventLayout>,
}

/// Packs each event into one continuous weekly segment. An event crossing a
/// week boundary gets one segment per week, as in conventional month views.
fn layout_week_events(events: &[CalendarEvent], week_start: NaiveDate) -> Vec<WeekEventLayout> {
    let mut layouts = Vec::new();
    for (event_index, event) in events.iter().enumerate() {
        let occupied: Vec<u16> = (0..7)
            .filter(|offset| {
                event_occurs_on_day(event, week_start + Duration::days(i64::from(*offset)))
            })
            .collect();
        let (Some(start_col), Some(end_col)) = (occupied.first(), occupied.last()) else {
            continue;
        };
        layouts.push(WeekEventLayout {
            event_index,
            start_col: *start_col,
            span: end_col - start_col + 1,
            lane: 0,
            continues_before: event_occurs_on_day(event, week_start - Duration::days(1)),
            continues_after: event_occurs_on_day(event, week_start + Duration::days(7)),
        });
    }
    layouts.sort_by(|left, right| {
        left.start_col
            .cmp(&right.start_col)
            .then_with(|| right.span.cmp(&left.span))
            .then_with(|| {
                events[left.event_index]
                    .id
                    .cmp(&events[right.event_index].id)
            })
    });

    let mut lane_ends = Vec::<u16>::new();
    for layout in &mut layouts {
        let end_col = layout.start_col + layout.span;
        if let Some((lane, occupied_until)) = lane_ends
            .iter_mut()
            .enumerate()
            .find(|(_, occupied_until)| **occupied_until <= layout.start_col)
        {
            layout.lane = lane as u16;
            *occupied_until = end_col;
        } else {
            layout.lane = lane_ends.len() as u16;
            lane_ends.push(end_col);
        }
    }
    layouts
}

impl AviaryApp {
    fn finish_calendar_date_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.calendar.date_drag_anchor.take().is_none() {
            return;
        }
        let Some((start, end)) = self.calendar.date_selection else {
            return;
        };
        self.open_event_compose_for_range(start, end, window, cx);
    }

    /// Configured length of the "upcoming" list sub-mode, sanitized.
    fn upcoming_days(&self) -> i64 {
        i64::from(self.settings.global.calendar_upcoming_days.clamp(1, 365))
    }

    /// Whether the grid scrolls continuously (default) or pages one month at
    /// a time.
    fn grid_infinite(&self) -> bool {
        self.settings.global.calendar_infinite_scroll
    }

    /// Local date range currently on screen: the visible week rows in the
    /// scrolling grid, the full displayed weeks of the anchored month in the
    /// paged grid, or the selected list range.
    fn calendar_visible_bounds(&self) -> (NaiveDate, NaiveDate) {
        match self.calendar.layout {
            CalendarLayout::Calendar if self.grid_infinite() => {
                let first = self.calendar.grid_first_visible.min(GRID_WEEKS - 1);
                let last = (first + self.calendar.grid_visible_rows.max(1)).min(GRID_WEEKS);
                (
                    self.calendar.grid_origin + Duration::weeks(first as i64),
                    self.calendar.grid_origin + Duration::weeks(last as i64),
                )
            }
            CalendarLayout::Calendar => {
                // The paged grid shows whole weeks, so the leading/trailing
                // days of adjacent months are part of the on-screen range.
                let (start, weeks) = paged_month_weeks(self.calendar.anchor_date());
                (start, start + Duration::weeks(weeks))
            }
            CalendarLayout::List => self.calendar.range_bounds(self.upcoming_days()),
        }
    }

    pub(super) fn navigate_calendar_events(
        &mut self,
        movement: super::shortcuts::ListMovement,
        cx: &mut Context<Self>,
    ) {
        let (from, to) = self.calendar_visible_bounds();
        let visible: Vec<_> = self
            .calendar
            .events
            .iter()
            .filter(|event| self.calendar_account_visible(&event.account_id))
            .filter(|event| event_occurs_in_range(event, from, to))
            .collect();
        if visible.is_empty() {
            return;
        }
        let current = self
            .calendar
            .selected
            .as_deref()
            .and_then(|id| visible.iter().position(|event| event.id.as_str() == id));
        let target = match movement {
            super::shortcuts::ListMovement::Previous => {
                current.map_or(0, |index| index.saturating_sub(1))
            }
            super::shortcuts::ListMovement::Next => {
                current.map_or(0, |index| (index + 1).min(visible.len() - 1))
            }
            super::shortcuts::ListMovement::First => 0,
            super::shortcuts::ListMovement::Last => visible.len() - 1,
        };
        self.calendar.selected = Some(visible[target].id.clone());
        cx.notify();
    }

    pub fn render_calendar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.ensure_calendar_loaded();
        let drawer = self
            .calendar
            .inline_compose
            .as_ref()
            .map(|inline| inline.view.clone().into_any_element())
            .or_else(|| self.render_event_detail(cx));
        let panes = super::app::sidebar_layout(
            "calendar-panes",
            self.sidebar_resize.clone(),
            self.render_calendar_sidebar(window, cx).into_any_element(),
            self.render_calendar_content(window, cx).into_any_element(),
        );
        let theme = cx.theme().clone();
        div()
            .relative()
            .size_full()
            .child(panes)
            .children(drawer.map(|content| {
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .w(px(420.))
                    .max_w_full()
                    .occlude()
                    .border_l_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .shadow_lg()
                    .child(content)
            }))
    }

    fn render_calendar_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let anchor = self.calendar.anchor_date();
        let grid = self.calendar.layout == CalendarLayout::Calendar;
        let infinite_grid = grid && self.grid_infinite();
        if !infinite_grid {
            self.scrolls.calendar.motion.cancel();
        }
        let upcoming = !grid && self.calendar.range == CalendarRange::Upcoming;
        let (from, to) = self.calendar_visible_bounds();

        let title = if grid && self.grid_infinite() {
            // The grid scrolls continuously: the title follows the viewport,
            // not the anchor.
            self.calendar
                .grid_center_date()
                .format_localized("%B %Y", super::datefmt::current_locale())
                .to_string()
        } else if grid {
            anchor
                .format_localized("%B %Y", super::datefmt::current_locale())
                .to_string()
        } else {
            match self.calendar.range {
                CalendarRange::Month => anchor
                    .format_localized("%B %Y", super::datefmt::current_locale())
                    .to_string(),
                CalendarRange::Week => {
                    let start = start_of_week(anchor);
                    let end = start + Duration::days(6);
                    tr!("calendar-week-of", {
                        start: start.format_localized("%e %B %Y", super::datefmt::current_locale()),
                        end: end.format_localized("%e %B %Y", super::datefmt::current_locale())
                    })
                    .to_string()
                }
                CalendarRange::Day => anchor
                    .format_localized("%A %e %B %Y", super::datefmt::current_locale())
                    .to_string(),
                CalendarRange::Upcoming => {
                    tr!("calendar-upcoming-title", { count: self.upcoming_days() }).to_string()
                }
            }
        };

        let toolbar = h_flex()
            .gap_2()
            .items_center()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(div().text_lg().font_semibold().child(title))
            .when(self.calendar.loading, |el| {
                el.child(gpui_component::spinner::Spinner::new().small())
            })
            .child(div().flex_1())
            .when(!upcoming, |toolbar| {
                toolbar.child(
                    Button::new("cal-prev")
                        .ghost()
                        .small()
                        .icon(IconName::ChevronLeft)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.shift_calendar(-1);
                            cx.notify();
                        })),
                )
            })
            .child(
                Button::new("cal-today")
                    .ghost()
                    .small()
                    .label(tr!("calendar-today"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.calendar.anchor = None;
                        if this.calendar.layout == CalendarLayout::Calendar && this.grid_infinite()
                        {
                            this.calendar.grid_scroll_to(Local::now().date_naive());
                        }
                        cx.notify();
                    })),
            )
            .when(!upcoming, |toolbar| {
                toolbar.child(
                    Button::new("cal-next")
                        .ghost()
                        .small()
                        .icon(IconName::ChevronRight)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.shift_calendar(1);
                            cx.notify();
                        })),
                )
            })
            .when(self.calendar.layout == CalendarLayout::List, |toolbar| {
                toolbar
                    .child(gpui_component::divider::Divider::vertical())
                    .child(self.range_button(
                        "cal-upcoming",
                        tr!("calendar-upcoming-view").to_string(),
                        CalendarRange::Upcoming,
                        cx,
                    ))
                    .child(self.range_button(
                        "cal-day",
                        tr!("calendar-day-view").to_string(),
                        CalendarRange::Day,
                        cx,
                    ))
                    .child(self.range_button(
                        "cal-week",
                        tr!("calendar-week-view").to_string(),
                        CalendarRange::Week,
                        cx,
                    ))
                    .child(self.range_button(
                        "cal-month",
                        tr!("calendar-month-view").to_string(),
                        CalendarRange::Month,
                        cx,
                    ))
            })
            .child(gpui_component::divider::Divider::vertical())
            .child(
                Button::new("cal-layout-list")
                    .ghost()
                    .small()
                    .icon(super::icons::app_icon("list"))
                    .label(tr!("calendar-list-view"))
                    .selected(self.calendar.layout == CalendarLayout::List)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_calendar_layout(CalendarLayout::List);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("cal-layout-calendar")
                    .ghost()
                    .small()
                    .icon(super::icons::app_icon("grid-3x3"))
                    .label(tr!("calendar-grid-view"))
                    .selected(self.calendar.layout == CalendarLayout::Calendar)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_calendar_layout(CalendarLayout::Calendar);
                        cx.notify();
                    })),
            );

        let body: gpui::AnyElement = if infinite_grid {
            self.render_infinite_grid(window, cx).into_any_element()
        } else if grid {
            self.render_paged_grid(cx).into_any_element()
        } else {
            let visible: Vec<CalendarEvent> = self
                .calendar
                .events
                .iter()
                .filter(|event| self.calendar_account_visible(&event.account_id))
                .filter(|event| event_occurs_in_range(event, from, to))
                .cloned()
                .collect();
            self.render_event_list(&visible, from, to, cx)
                .into_any_element()
        };

        v_flex()
            .size_full()
            .child(toolbar)
            .child(div().flex_1().min_h_0().child(body))
    }

    fn render_calendar_sidebar(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let accounts: Vec<_> = self
            .accounts
            .iter()
            .filter(|account| account.provider != Provider::Imap)
            .cloned()
            .collect();

        let mut account_list = v_flex().gap_0p5().px_2();
        if accounts.is_empty() && self.settings.global.ical_subscriptions.is_empty() {
            account_list = account_list.child(
                div()
                    .px_2()
                    .py_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tr!("calendar-no-calendars")),
            );
        }
        for account in accounts {
            let account_id = account.id.clone();
            let checkbox_id = account.id.clone();
            let label = self.account_label(&account);
            let visible = self.calendar_account_visible(&account.id);
            let color = util::account_color(
                &account.id,
                self.settings
                    .accounts
                    .get(&account.id)
                    .and_then(|settings| settings.color_override),
            );
            account_list = account_list.child(
                h_flex()
                    .id(gpui::ElementId::Name(
                        format!("calendar-account-{}", account.id.0).into(),
                    ))
                    .w_full()
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1p5()
                    .rounded(theme.radius)
                    .hover(|style| style.bg(theme.list_hover))
                    .child(
                        Checkbox::new(gpui::ElementId::Name(
                            format!("calendar-account-visible-{}", checkbox_id.0).into(),
                        ))
                        .xsmall()
                        .checked(visible)
                        .on_click(cx.listener(
                            move |this, checked: &bool, _, cx| {
                                this.set_calendar_account_visible(&account_id, *checked);
                                cx.notify();
                            },
                        )),
                    )
                    .child(div().w(px(8.)).h(px(8.)).rounded_full().bg(color))
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap_0()
                            .child(div().text_sm().truncate().child(label))
                            .child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .text_color(theme.muted_foreground)
                                    .child(account.email),
                            ),
                    ),
            );
        }
        for subscription in self.settings.global.ical_subscriptions.clone() {
            let account_id = subscription.account_id();
            let checkbox_id = account_id.clone();
            let visible = self.calendar_account_visible(&account_id);
            let color = util::packed_color(subscription.color);
            let status = self
                .calendar
                .ical_sync
                .get(&subscription.id)
                .cloned()
                .unwrap_or_default();
            account_list = account_list.child(
                h_flex()
                    .id(gpui::ElementId::Name(
                        format!("calendar-account-{}", account_id.0).into(),
                    ))
                    .w_full()
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1p5()
                    .rounded(theme.radius)
                    .hover(|style| style.bg(theme.list_hover))
                    .child(
                        Checkbox::new(gpui::ElementId::Name(
                            format!("calendar-account-visible-{}", checkbox_id.0).into(),
                        ))
                        .xsmall()
                        .checked(visible)
                        .on_click(cx.listener(
                            move |this, checked: &bool, _, cx| {
                                this.set_calendar_account_visible(&account_id, *checked);
                                cx.notify();
                            },
                        )),
                    )
                    .child(div().w(px(8.)).h(px(8.)).rounded_full().bg(color))
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap_0()
                            .child(div().text_sm().truncate().child(subscription.name))
                            .child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .text_color(if status.error.is_some() {
                                        theme.danger
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .child(if status.syncing {
                                        tr!("settings-ical-syncing")
                                    } else if status.error.is_some() {
                                        tr!("calendar-ical-stale")
                                    } else {
                                        tr!("calendar-ical-read-only")
                                    }),
                            ),
                    ),
            );
        }

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(self.render_mini_calendar(cx))
            .child(
                h_flex()
                    .mt_2()
                    .px_4()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(tr!("calendar-my-calendars")),
                    ),
            )
            .child(
                div()
                    .id("calendar-accounts-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(account_list),
            )
            .child(self.render_sidebar_navigation(cx))
    }

    fn render_mini_calendar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let anchor = self.calendar.anchor_date();
        let first = anchor.with_day(1).expect("valid day 1");
        let start = start_of_week(first);
        let today = Local::now().date_naive();

        let mut calendar = v_flex().gap_1().px_3().pt_3();
        calendar = calendar.child(
            h_flex()
                .items_center()
                .child(
                    div().flex_1().text_sm().font_semibold().child(
                        first
                            .format_localized("%B %Y", super::datefmt::current_locale())
                            .to_string(),
                    ),
                )
                .child(
                    Button::new("mini-calendar-prev")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ChevronLeft)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.shift_calendar_month(-1);
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("mini-calendar-next")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ChevronRight)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.shift_calendar_month(1);
                            cx.notify();
                        })),
                ),
        );

        let mut weekdays = h_flex().gap_0p5();
        for offset in 0..7 {
            let day = start + Duration::days(offset);
            weekdays = weekdays.child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_center()
                    .text_color(theme.muted_foreground)
                    .child(
                        day.format_localized("%a", super::datefmt::current_locale())
                            .to_string(),
                    ),
            );
        }
        calendar = calendar.child(weekdays);

        for week in 0..6 {
            let mut row = h_flex().gap_0p5();
            for weekday in 0..7 {
                let day = start + Duration::days(week * 7 + weekday);
                let in_month = day.month() == first.month();
                row = row.child(
                    div()
                        .id(gpui::ElementId::Name(
                            format!("mini-calendar-day-{day}").into(),
                        ))
                        .flex_1()
                        .h(px(25.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .text_xs()
                        .cursor_pointer()
                        .when(!in_month, |element| element.opacity(0.45))
                        .when(day == today, |element| {
                            element.border_1().border_color(theme.primary)
                        })
                        .when(day == anchor, |element| {
                            element
                                .bg(theme.primary)
                                .text_color(theme.primary_foreground)
                        })
                        .hover(|style| style.bg(theme.list_hover))
                        .child(day.day().to_string())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.calendar.anchor = Some(day);
                            if this.calendar.layout == CalendarLayout::Calendar
                                && this.grid_infinite()
                            {
                                this.calendar.grid_scroll_to(day);
                            }
                            cx.notify();
                        })),
                );
            }
            calendar = calendar.child(row);
        }
        calendar
    }

    fn shift_calendar_month(&mut self, dir: i64) {
        let anchor = self.calendar.anchor_date();
        let month = anchor.month() as i64 + dir;
        let (year, month) = if month < 1 {
            (anchor.year() - 1, 12)
        } else if month > 12 {
            (anchor.year() + 1, 1)
        } else {
            (anchor.year(), month as u32)
        };
        self.calendar.anchor = NaiveDate::from_ymd_opt(year, month, 1);
    }

    pub(super) fn calendar_account_visible(&self, account_id: &AccountId) -> bool {
        let known_account = self
            .accounts
            .iter()
            .any(|account| &account.id == account_id && account.provider != Provider::Imap);
        let known_ical = self
            .settings
            .global
            .ical_subscriptions
            .iter()
            .any(|subscription| subscription.account_id() == *account_id);
        (known_account || known_ical)
            && !self
                .settings
                .global
                .calendar_hidden_account_ids
                .contains(&account_id.0)
    }

    fn calendar_visibility_key(&self) -> Vec<String> {
        let mut accounts: Vec<_> = self
            .accounts
            .iter()
            .filter(|account| self.calendar_account_visible(&account.id))
            .map(|account| account.id.0.clone())
            .chain(
                self.settings
                    .global
                    .ical_subscriptions
                    .iter()
                    .map(|subscription| subscription.account_id())
                    .filter(|account_id| self.calendar_account_visible(account_id))
                    .map(|account_id| account_id.0),
            )
            .collect();
        accounts.sort();
        accounts.dedup();
        accounts
    }

    fn week_render_data(&mut self, week_start: NaiveDate) -> Rc<WeekRenderData> {
        let key = (week_start, self.calendar_visibility_key());
        if let Some(data) = self.calendar.week_cache.get(&key) {
            return data.clone();
        }
        let events: Vec<_> = self
            .calendar
            .events
            .iter()
            .filter(|event| self.calendar_account_visible(&event.account_id))
            .filter(|event| {
                event_occurs_in_range(event, week_start, week_start + Duration::days(7))
            })
            .cloned()
            .collect();
        let layouts = layout_week_events(&events, week_start);
        let data = Rc::new(WeekRenderData { events, layouts });
        self.calendar.week_cache.insert(key, data.clone());
        data
    }

    fn calendar_account_ids(&self) -> Vec<AccountId> {
        self.accounts
            .iter()
            .filter(|account| account.provider != Provider::Imap)
            .filter(|account| self.calendar_account_visible(&account.id))
            .map(|account| account.id.clone())
            .collect()
    }

    fn set_calendar_account_visible(&mut self, account_id: &AccountId, visible: bool) {
        let hidden = &mut self.settings.global.calendar_hidden_account_ids;
        if visible {
            hidden.retain(|id| id != &account_id.0);
        } else if !hidden.contains(&account_id.0) {
            hidden.push(account_id.0.clone());
        }
        self.settings.save();
        self.calendar.force_reload();
        if self.calendar.selected.as_ref().is_some_and(|selected| {
            self.calendar
                .events
                .iter()
                .any(|event| &event.account_id == account_id && &event.id == selected)
        }) {
            self.calendar.selected = None;
        }
    }

    fn set_calendar_layout(&mut self, layout: CalendarLayout) {
        if self.calendar.layout == layout {
            return;
        }
        self.calendar.layout = layout;
        if layout == CalendarLayout::Calendar {
            self.calendar.range = CalendarRange::Month;
            self.calendar.grid_scroll_to(self.calendar.anchor_date());
        }
        self.settings.global.calendar_layout = layout;
        self.settings.save();
    }

    fn range_button(
        &self,
        id: &'static str,
        label: String,
        value: CalendarRange,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .ghost()
            .small()
            .label(label)
            .selected(self.calendar.range == value)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.calendar.range = value;
                cx.notify();
            }))
    }

    fn shift_calendar(&mut self, dir: i64) {
        if self.calendar.layout == CalendarLayout::Calendar && self.grid_infinite() {
            // Page the continuous grid by month, from the viewed month.
            let center = self.calendar.grid_center_date();
            let month = center.month() as i64 + dir;
            let (y, m) = if month < 1 {
                (center.year() - 1, 12)
            } else if month > 12 {
                (center.year() + 1, 1)
            } else {
                (center.year(), month as u32)
            };
            let target = NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(center);
            self.calendar.anchor = Some(target);
            self.calendar.grid_scroll_to(target);
            return;
        }
        if self.calendar.layout == CalendarLayout::Calendar {
            self.shift_calendar_month(dir);
            return;
        }
        let anchor = self.calendar.anchor_date();
        let next = match self.calendar.range {
            CalendarRange::Day => anchor + Duration::days(dir),
            CalendarRange::Week => anchor + Duration::days(7 * dir),
            CalendarRange::Month => {
                let month = anchor.month() as i64 + dir;
                let (y, m) = if month < 1 {
                    (anchor.year() - 1, 12)
                } else if month > 12 {
                    (anchor.year() + 1, 1)
                } else {
                    (anchor.year(), month as u32)
                };
                NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(anchor)
            }
            // Anchored to today; prev/next are hidden.
            CalendarRange::Upcoming => anchor,
        };
        self.calendar.anchor = Some(next);
    }

    /// Requests the month chunks covering the on-screen period (with a
    /// prefetch margin in the scrolling grid) that were not already fetched.
    /// Runs on every calendar render, so it must stay cheap and idempotent.
    pub(super) fn ensure_calendar_loaded(&mut self) {
        let account_ids = self.calendar_account_ids();
        let ical_ids: Vec<String> = self
            .settings
            .global
            .ical_subscriptions
            .iter()
            .filter(|subscription| self.calendar_account_visible(&subscription.account_id()))
            .map(|subscription| subscription.id.clone())
            .collect();
        if account_ids.is_empty() && ical_ids.is_empty() {
            self.calendar.loading = false;
            return;
        }

        let (from, to) = self.calendar_visible_bounds();
        let (from, to) = if self.calendar.layout == CalendarLayout::Calendar && self.grid_infinite()
        {
            (
                from - Duration::weeks(GRID_PREFETCH_WEEKS as i64),
                to + Duration::weeks(GRID_PREFETCH_WEEKS as i64),
            )
        } else {
            (from, to)
        };
        let first_chunk = month_chunk(from);
        let last_chunk = month_chunk(to - Duration::days(1)).max(first_chunk);
        self.calendar
            .evict_distant_chunks((first_chunk + last_chunk) / 2);

        // One request per contiguous run of missing months per calendar.
        let mut sent = false;
        for account_id in account_ids {
            for (start, end) in self
                .calendar
                .missing_runs(&account_id.0, first_chunk, last_chunk)
            {
                sent = true;
                self.send(Cmd::LoadCalendar {
                    account_id: account_id.clone(),
                    from: to_utc(chunk_start(start)),
                    to: to_utc(chunk_start(end + 1)),
                });
            }
        }
        for subscription_id in ical_ids {
            let scope = format!("ical:{subscription_id}");
            for (start, end) in self.calendar.missing_runs(&scope, first_chunk, last_chunk) {
                sent = true;
                self.send(Cmd::LoadIcalCalendar {
                    subscription_id: subscription_id.clone(),
                    from: to_utc(chunk_start(start)),
                    to: to_utc(chunk_start(end + 1)),
                    force_refresh: false,
                });
            }
        }
        if sent {
            self.calendar.loading = true;
        }
    }

    fn render_event_list(
        &self,
        events: &[CalendarEvent],
        from: NaiveDate,
        to: NaiveDate,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut list = v_flex().gap_1().p_3();
        if events.is_empty() {
            list = list.child(
                div()
                    .p_4()
                    .text_color(theme.muted_foreground)
                    .child(tr!("calendar-no-events")),
            );
        }
        let mut current_day = None;
        for event in events {
            let day = event_day(event).max(from).min(to - Duration::days(1));
            if current_day != Some(day) {
                current_day = Some(day);
                list = list.child(
                    div()
                        .mt_2()
                        .text_sm()
                        .font_semibold()
                        .text_color(theme.muted_foreground)
                        .child(
                            day.format_localized("%A %e %B", super::datefmt::current_locale())
                                .to_string(),
                        ),
                );
            }
            list = list.child(self.event_row(event, day, cx));
        }
        div()
            .id("cal-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(list)
    }

    fn event_row(
        &self,
        e: &CalendarEvent,
        occurrence_day: NaiveDate,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let color = self.calendar_event_color(e);
        let time = if e.all_day {
            tr!("calendar-all-day-short").to_string()
        } else {
            let start = e.start.with_timezone(&Local);
            let end = e.end.with_timezone(&Local);
            match (
                start.date_naive() == occurrence_day,
                end.date_naive() == occurrence_day,
            ) {
                (true, true) => format!("{} – {}", start.format("%H:%M"), end.format("%H:%M")),
                (true, false) => format!("{} →", start.format("%H:%M")),
                (false, true) => format!("→ {}", end.format("%H:%M")),
                (false, false) => "↔".to_string(),
            }
        };
        let eid = e.id.clone();
        let event = e.clone();
        let deleting = self
            .calendar
            .deleting
            .contains(&(e.account_id.clone(), e.id.clone()));
        let edit_event = event.clone();
        let delete_event = event.clone();
        let read_only = e.read_only;
        let entity = cx.entity();
        let row = h_flex()
            .id(gpui::ElementId::Name(
                format!("evt-{occurrence_day}-{}", e.id).into(),
            ))
            .gap_2()
            .items_center()
            .px_2()
            .py_1p5()
            .rounded(theme.radius)
            .cursor_pointer()
            .hover(|s| s.bg(theme.list_hover))
            .child(div().w(px(3.)).h(px(28.)).rounded_full().bg(color))
            .child(
                div()
                    .w(px(110.))
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(time),
            )
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .text_sm()
                    .when(e.is_cancelled, |el| el.line_through())
                    .child(e.subject.clone()),
            )
            .when(!e.location.is_empty(), |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .max_w(px(220.))
                        .truncate()
                        .child(e.location.clone()),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.calendar.selected = Some(eid.clone());
                this.focus_shortcuts(window);
                cx.notify();
            }));
        row.context_menu(move |menu, _window, _cx| {
            let edit_entity = entity.clone();
            let delete_entity = entity.clone();
            let edit_event = edit_event.clone();
            let delete_event = delete_event.clone();
            menu.item(
                PopupMenuItem::new(tr!("calendar-event-edit"))
                    .icon(super::icons::app_icon("pencil"))
                    .disabled(read_only)
                    .on_click(move |_, window, cx| {
                        edit_entity.update(cx, |this, cx| {
                            this.open_event_edit(edit_event.clone(), window, cx);
                        });
                    }),
            )
            .item(
                PopupMenuItem::new(tr!("calendar-event-delete"))
                    .icon(super::icons::app_icon("trash-2"))
                    .disabled(deleting || read_only)
                    .on_click(move |_, window, cx| {
                        delete_entity.update(cx, |this, cx| {
                            this.confirm_delete_calendar_event(delete_event.clone(), window, cx);
                        });
                    }),
            )
        })
    }

    /// Places `week` on the *first* visible grid row by writing the scroll
    /// offset directly — exact since rows share one height; gpui clamps the
    /// offset at paint.
    fn grid_jump_to_week(&mut self, week: usize, row_h: f32) {
        // A pending wheel animation would overwrite the jump.
        self.scrolls.calendar.motion.cancel();
        let handle = self.scrolls.calendar.handle.base_handle();
        handle.set_offset(point(handle.offset().x, px(-(week as f32) * row_h)));
    }

    /// Day-name header row shared by both grid variants (weeks start on
    /// Monday).
    fn grid_weekday_header(&self, theme: &gpui_component::theme::Theme) -> impl IntoElement {
        let start = start_of_week(Local::now().date_naive());
        let mut head = h_flex()
            .gap_0()
            .px_2()
            .pb_1()
            .border_b_1()
            .border_color(theme.border);
        for d in 0..7 {
            let day = start + Duration::days(d);
            head = head.child(
                div()
                    .flex_1()
                    .px_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        day.format_localized("%a", super::datefmt::current_locale())
                            .to_string(),
                    ),
            );
        }
        head
    }

    /// Paged month grid, used when the « défilement continu » option is
    /// disabled: the anchored month's weeks fill the pane and ‹/› move one
    /// month at a time.
    fn render_paged_grid(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let today = Local::now().date_naive();
        let (start, weeks) = paged_month_weeks(self.calendar.anchor_date());
        let mut grid = v_flex()
            .size_full()
            .p_2()
            .gap_0()
            .child(self.grid_weekday_header(&theme));
        for week in 0..weeks {
            let week_start = start + Duration::weeks(week);
            grid = grid.child(self.render_week_row(week_start, today, false, cx));
        }
        grid.on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _, window, cx| {
                this.finish_calendar_date_selection(window, cx);
            }),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|this, _, window, cx| {
                this.finish_calendar_date_selection(window, cx);
            }),
        )
    }

    /// Continuously scrolling week grid (classic-Outlook style): a virtual
    /// list of fixed-height week rows over a multi-year window. Only visible
    /// rows are built, and only their months are fetched (see
    /// [`Self::ensure_calendar_loaded`]).
    fn render_infinite_grid(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();

        // Row height so the configured number of weeks fits the viewport
        // (measured on the previous frame; the fallback only covers the very
        // first one). On change — resize or new setting — the first visible
        // week is re-anchored so the view does not drift.
        let viewport = f32::from(
            self.scrolls
                .calendar
                .handle
                .base_handle()
                .bounds()
                .size
                .height,
        );
        let weeks_on_screen = self.settings.global.calendar_grid_weeks.clamp(2, 12) as f32;
        let row_h = if viewport > 1. {
            (viewport / weeks_on_screen).max(MIN_GRID_ROW_H)
        } else {
            self.calendar.grid_row_h
        };
        if (row_h - self.calendar.grid_row_h).abs() > 0.5
            && self.calendar.grid_pending_scroll.is_none()
        {
            self.grid_jump_to_week(self.calendar.grid_first_visible, row_h);
        }
        self.calendar.grid_row_h = row_h;
        if let Some(date) = self.calendar.grid_pending_scroll.take() {
            // Positioned directly (uniform row heights make this exact) so
            // the target week lands on the *first* visible row; the virtual
            // list's deferred scroll only scrolls minimally into view, which
            // would leave a below-viewport target at the bottom.
            self.grid_jump_to_week(self.calendar.week_index(date), row_h);
        }
        let origin = self.calendar.grid_origin;
        let today = Local::now().date_naive();
        let head = self.grid_weekday_header(&theme);

        let sizes: Rc<Vec<gpui::Size<Pixels>>> =
            Rc::new(vec![gpui::size(px(0.), px(row_h)); GRID_WEEKS]);
        let base_handle = self.scrolls.calendar.handle.base_handle().clone();
        self.scrolls.calendar.motion.advance(&base_handle, window);
        let app = cx.entity();

        v_flex().size_full().p_2().gap_0().child(head).child(
            // Non-scrollable wrapper: hosts wheel motion and tracks
            // the visible week window at prepaint to drive fetching and
            // the toolbar title (same pattern as the message list).
            div()
                .on_children_prepainted({
                    let handle = base_handle.clone();
                    move |_, _, cx| {
                        // While the wheel animates, fetch for where the
                        // viewport is heading rather than where it is.
                        let target =
                            -f32::from(app.read(cx).scrolls.calendar.motion.target_y(&handle));
                        let viewport = f32::from(handle.bounds().size.height);
                        let first = (target / row_h).max(0.) as usize;
                        let rows = if viewport > 0. {
                            (viewport / row_h).ceil() as usize + 1
                        } else {
                            6
                        };
                        app.update(cx, |this, cx| {
                            let calendar = &mut this.calendar;
                            if calendar.grid_first_visible != first
                                || calendar.grid_visible_rows != rows
                            {
                                calendar.grid_first_visible = first;
                                calendar.grid_visible_rows = rows;
                                cx.notify();
                            }
                        });
                    }
                })
                .id("calendar-grid-scroll")
                .flex_1()
                .min_h_0()
                .on_scroll_wheel(cx.listener({
                    let handle = base_handle;
                    move |this, event: &ScrollWheelEvent, window, cx| {
                        if this
                            .scrolls
                            .calendar
                            .motion
                            .on_wheel(&handle, event, window)
                        {
                            cx.notify();
                        }
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.finish_calendar_date_selection(window, cx);
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.finish_calendar_date_selection(window, cx);
                    }),
                )
                .child(
                    v_virtual_list(
                        cx.entity(),
                        "calendar-grid",
                        sizes,
                        move |this, range: std::ops::Range<usize>, _window, cx| {
                            range
                                .map(|index| {
                                    let week_start = origin + Duration::weeks(index as i64);
                                    this.render_week_row(week_start, today, true, cx)
                                        .into_any_element()
                                })
                                .collect::<Vec<_>>()
                        },
                    )
                    .track_scroll(&self.scrolls.calendar.handle),
                ),
        )
    }

    /// One fixed-height week row of the scrolling grid: seven day cells plus
    /// the event bars of that week.
    fn render_week_row(
        &mut self,
        week_start: NaiveDate,
        today: NaiveDate,
        fixed_height: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let data = self.week_render_data(week_start);
        {
            let layouts = &data.layouts;
            // Event lanes fully fitting under the day number at the current
            // row height; when some events overflow, the last lane is
            // sacrificed for the "+N more" label. Paged rows flex to fill
            // the pane, so they keep the historical four lanes.
            let chip_lanes = if fixed_height {
                let row_h = self.calendar.grid_row_h;
                let slots = ((((row_h - MONTH_EVENT_TOP - MONTH_EVENT_HEIGHT) / MONTH_EVENT_STEP)
                    .floor() as i32)
                    + 1)
                .max(1) as u16;
                let lane_count = layouts
                    .iter()
                    .map(|layout| layout.lane + 1)
                    .max()
                    .unwrap_or(0);
                if lane_count > slots {
                    slots.saturating_sub(1).max(1)
                } else {
                    slots
                }
            } else {
                4
            };
            let mut row = div()
                .relative()
                .min_w_0()
                .when(fixed_height, |row| {
                    row.w_full()
                        .h(px(self.calendar.grid_row_h))
                        .overflow_hidden()
                })
                .when(!fixed_height, |row| row.flex_1().min_h(px(120.)));
            let cells = self.week_day_cells(week_start, today, cx);
            row = row.child(cells);

            for chip in self.week_event_chips(week_start, &data, chip_lanes, cx) {
                row = row.child(chip);
            }

            for dow in 0..7u16 {
                let hidden = layouts
                    .iter()
                    .filter(|layout| {
                        layout.lane >= chip_lanes
                            && dow >= layout.start_col
                            && dow < layout.start_col + layout.span
                    })
                    .count();
                if hidden > 0 {
                    row = row.child(
                        div()
                            .absolute()
                            .top(px(
                                MONTH_EVENT_TOP + f32::from(chip_lanes) * MONTH_EVENT_STEP
                            ))
                            .left_0()
                            .right_0()
                            .h(px(MONTH_EVENT_HEIGHT))
                            .grid()
                            .grid_cols(7)
                            .child(
                                div()
                                    .col_start(dow as i16 + 1)
                                    .col_end(dow as i16 + 2)
                                    .px_1()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(tr!("calendar-more-events", { count: hidden })),
                            ),
                    );
                }
            }
            row
        }
    }

    fn move_calendar_event(
        &mut self,
        drag: &EventDrag,
        target_day: NaiveDate,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let event = &drag.event;
        if event.read_only {
            return;
        }
        let key = (event.account_id.clone(), event.id.clone());
        if self.calendar.moving.contains(&key) {
            return;
        }
        let local_start = event.start.with_timezone(&Local);
        let source_day = drag.displayed_day;
        if source_day == target_day {
            return;
        }
        let shift = target_day - source_day;

        let new_start = if event.all_day {
            event.start + shift
        } else {
            let shifted_day = event_day(event) + shift;
            let local = Local.from_local_datetime(&shifted_day.and_time(local_start.time()));
            let Some(local) = local.earliest() else {
                return;
            };
            local.with_timezone(&Utc)
        };
        let new_end = new_start + (event.end - event.start);
        let previous_start = event.start;
        let previous_end = event.end;
        if let Some(current) = self
            .calendar
            .events
            .iter_mut()
            .find(|current| current.account_id == event.account_id && current.id == event.id)
        {
            current.start = new_start;
            current.end = new_end;
        }
        self.calendar.events.sort_by_key(|event| event.start);
        self.calendar.invalidate_event_layouts();
        self.calendar.moving.insert(key);
        self.send(Cmd::MoveCalendarEvent {
            account_id: event.account_id.clone(),
            event_id: event.id.clone(),
            start: new_start,
            end: new_end,
            previous_start,
            previous_end,
            all_day: event.all_day,
        });
        cx.notify();
    }

    fn confirm_delete_calendar_event(
        &mut self,
        event: CalendarEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.read_only {
            return;
        }
        let entity = cx.entity();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let entity = entity.clone();
            let account_id = event.account_id.clone();
            let event_id = event.id.clone();
            dialog
                .title(tr!("calendar-delete-event-title"))
                .confirm()
                .child(div().child(tr!("calendar-delete-event-confirm", {
                    name: event.subject.clone()
                })))
                .on_ok(move |_, _, cx| {
                    entity.update(cx, |this, cx| {
                        this.calendar
                            .deleting
                            .insert((account_id.clone(), event_id.clone()));
                        this.send(Cmd::DeleteCalendarEvent {
                            account_id: account_id.clone(),
                            event_id: event_id.clone(),
                        });
                        cx.notify();
                    });
                    true
                })
        });
    }

    fn render_event_detail(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let id = self.calendar.selected.as_ref()?;
        let e = self.calendar.events.iter().find(|e| &e.id == id)?.clone();
        let theme = cx.theme().clone();
        let when = if e.all_day {
            let start = e.start.date_naive();
            let end = all_day_last_day(&e);
            if end > start {
                tr!("calendar-all-day-range", {
                    start: start.format_localized(
                        "%A %e %B %Y",
                        super::datefmt::current_locale()
                    ),
                    end: end.format_localized(
                        "%A %e %B %Y",
                        super::datefmt::current_locale()
                    )
                })
                .to_string()
            } else {
                tr!("calendar-all-day-date", {
                    date: start
                        .format_localized("%A %e %B %Y", super::datefmt::current_locale())
                })
                .to_string()
            }
        } else {
            let start = e.start.with_timezone(&Local);
            let end = e.end.with_timezone(&Local);
            if start.date_naive() == end.date_naive() {
                format!(
                    "{} → {}",
                    start.format_localized("%A %e %B %Y %H:%M", super::datefmt::current_locale()),
                    end.format("%H:%M")
                )
            } else {
                format!(
                    "{} → {}",
                    start.format_localized("%A %e %B %Y %H:%M", super::datefmt::current_locale()),
                    end.format_localized("%A %e %B %Y %H:%M", super::datefmt::current_locale())
                )
            }
        };
        let edit_event = e.clone();
        let delete_event = e.clone();
        let maps_url = (!e.location.trim().is_empty()).then(|| google_maps_search_url(&e.location));
        let deleting = self
            .calendar
            .deleting
            .contains(&(e.account_id.clone(), e.id.clone()));
        let actions = h_flex()
            .w_full()
            .gap_2()
            .gap_y_1()
            .items_center()
            .justify_end()
            .flex_wrap()
            .when(!e.read_only, |element| {
                element
                    .child(
                        Button::new("edit-event-detail")
                            .ghost()
                            .xsmall()
                            .icon(super::icons::app_icon("pencil"))
                            .tooltip(tr!("calendar-event-edit"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_event_edit(edit_event.clone(), window, cx);
                            })),
                    )
                    .child(
                        Button::new("delete-event-detail")
                            .ghost()
                            .xsmall()
                            .icon(super::icons::app_icon("trash-2"))
                            .disabled(deleting)
                            .tooltip(tr!("calendar-event-delete"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_delete_calendar_event(
                                    delete_event.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    )
            })
            .when_some(e.online_meeting_url.clone(), |el, url| {
                el.child(
                    Button::new("join")
                        .primary()
                        .xsmall()
                        .label(tr!("calendar-join"))
                        .icon(IconName::ExternalLink)
                        .on_click(move |_, _, _| {
                            let _ = open::that(&url);
                        }),
                )
            })
            .when_some(e.web_link.clone(), |el, url| {
                el.child(
                    Button::new("open-web")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Globe)
                        .tooltip(tr!("open-in-browser"))
                        .on_click(move |_, _, _| {
                            let _ = open::that(&url);
                        }),
                )
            })
            .child(
                Button::new("close-detail")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.calendar.selected = None;
                        cx.notify();
                    })),
            );
        Some(
            v_flex()
                .id("calendar-event-detail")
                .size_full()
                .overflow_y_scroll()
                .gap_2()
                .p_4()
                .bg(theme.background)
                .child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(
                            div()
                                .w_full()
                                .min_w_0()
                                .font_semibold()
                                .whitespace_normal()
                                .line_clamp(2)
                                .child(e.subject.clone()),
                        )
                        .child(actions),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(when),
                )
                .when_some(maps_url, |el, url| {
                    el.child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                div()
                                    .min_w_0()
                                    .text_sm()
                                    .child(tr!("calendar-location", { value: e.location })),
                            )
                            .child(
                                Button::new("open-event-location-map")
                                    .ghost()
                                    .xsmall()
                                    .icon(super::icons::app_icon("map"))
                                    .tooltip(tr!("calendar-open-google-maps"))
                                    .on_click(move |_, _, _| {
                                        let _ = open::that(&url);
                                    }),
                            ),
                    )
                })
                .when(!e.organizer.is_empty(), |el| {
                    el.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(tr!("calendar-organizer", { value: e.organizer })),
                    )
                })
                .when(!e.preview.is_empty(), |el| {
                    el.child(div().text_sm().child(e.preview.clone()))
                })
                .into_any_element(),
        )
    }

    fn calendar_event_color(&self, event: &CalendarEvent) -> Hsla {
        if let Some(subscription_id) = event.account_id.0.strip_prefix("ical:") {
            if let Some(subscription) = self
                .settings
                .global
                .ical_subscriptions
                .iter()
                .find(|subscription| subscription.id == subscription_id)
            {
                return util::packed_color(subscription.color);
            }
        }
        util::account_color(
            &event.account_id,
            self.settings
                .accounts
                .get(&event.account_id)
                .and_then(|settings| settings.color_override),
        )
    }

    /// The seven day cells of one week row: their number, their background, and
    /// the click and drop targets that go with them.
    fn week_day_cells(
        &self,
        week_start: NaiveDate,
        today: NaiveDate,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme().clone();
        let mut cells = div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .grid()
            .grid_cols(7)
            .gap_0();
        for dow in 0..7 {
            let day = week_start + Duration::days(dow);
            let selected = self.calendar.date_is_selected(day);
            // In the continuous grid, month boundaries are conveyed by an
            // alternating tint and by the month name on each 1st.
            let alternate_month = day.month().is_multiple_of(2);
            let day_label = if day.day() == 1 {
                day.format_localized("%e %b", super::datefmt::current_locale())
                    .to_string()
                    .trim()
                    .to_string()
            } else {
                day.day().to_string()
            };
            let cell = v_flex()
                .id(gpui::ElementId::Name(format!("calendar-day-{day}").into()))
                .col_start(dow as i16 + 1)
                .col_end(dow as i16 + 2)
                .h_full()
                .overflow_hidden()
                .p_1()
                // Cells normally own their left and bottom separators.
                // Yield the shared edge beside/above today so its complete
                // blue outline remains exactly one pixel wide.
                .when(!(dow > 0 && day - Duration::days(1) == today), |element| {
                    element.border_l_1()
                })
                .when(day + Duration::days(7) != today, |element| {
                    element.border_b_1()
                })
                .when(dow == 6, |element| element.border_r_1())
                .border_color(theme.border)
                .cursor_pointer()
                .hover(|style| {
                    style.bg(if selected {
                        theme.primary.opacity(0.24)
                    } else {
                        theme.list_hover.opacity(0.55)
                    })
                })
                .when(alternate_month, |el| el.bg(theme.muted.opacity(0.16)))
                .when(dow >= 5, |el| el.bg(theme.muted.opacity(0.3)))
                .when(day == today, |el| {
                    el.border_1()
                        .border_color(theme.primary)
                        .bg(theme.primary.opacity(0.06))
                })
                .when(selected, |el| el.bg(theme.primary.opacity(0.18)))
                .child(
                    div()
                        .text_xs()
                        .when(day.day() == 1, |el| el.font_semibold())
                        .when(day == today, |el| el.font_bold().text_color(theme.primary))
                        .child(day_label),
                )
                .drag_over::<EventDrag>(|style, _, _, cx| {
                    style
                        .bg(cx.theme().drop_target)
                        .border_color(cx.theme().primary)
                })
                .on_drop(cx.listener(move |this, drag: &EventDrag, window, cx| {
                    this.move_calendar_event(drag, day, window, cx);
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        cx.stop_propagation();
                        this.calendar.anchor = Some(day);
                        this.calendar.date_drag_anchor = Some(day);
                        this.calendar.select_dates(day, day);
                        this.calendar.selected = None;
                        cx.notify();
                    }),
                )
                .on_mouse_move(
                    cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                        if event.pressed_button != Some(MouseButton::Left) {
                            return;
                        }
                        let Some(anchor) = this.calendar.date_drag_anchor else {
                            return;
                        };
                        let range = ordered_date_range(anchor, day);
                        if this.calendar.date_selection != Some(range) {
                            this.calendar.date_selection = Some(range);
                            cx.notify();
                        }
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        let Some(anchor) = this.calendar.date_drag_anchor else {
                            return;
                        };
                        let (start, end) = ordered_date_range(anchor, day);
                        this.calendar.select_dates(start, end);
                        this.finish_calendar_date_selection(window, cx);
                    }),
                );
            cells = cells.child(cell);
        }
        cells
    }

    /// One chip per event segment that fits in the row's lanes, positioned over
    /// the day cells it spans.
    fn week_event_chips(
        &self,
        week_start: NaiveDate,
        data: &WeekRenderData,
        chip_lanes: u16,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let theme = cx.theme().clone();
        let events = &data.events;
        let layouts = &data.layouts;
        let mut chips: Vec<gpui::AnyElement> = Vec::new();
        for layout in layouts.iter().filter(|layout| layout.lane < chip_lanes) {
            let e = &events[layout.event_index];
            let segment_day = week_start + Duration::days(i64::from(layout.start_col));
            let eid = e.id.clone();
            let event = e.clone();
            let drag = EventDrag {
                event: event.clone(),
                displayed_day: segment_day,
            };
            let edit_event = event.clone();
            let delete_event = event.clone();
            let entity = cx.entity();
            let moving = self
                .calendar
                .moving
                .contains(&(e.account_id.clone(), e.id.clone()));
            let deleting = self
                .calendar
                .deleting
                .contains(&(e.account_id.clone(), e.id.clone()));
            let color = self.calendar_event_color(e);
            // Event segments are painted after the cell grid, but a
            // translucent fill still lets shared day borders show
            // through. Flatten the tint onto the calendar background so
            // multi-day bars remain visually continuous across cells.
            let event_background = theme.background.blend(color.opacity(0.35));
            let read_only = e.read_only;
            let title = if e.all_day || event_day(e) != segment_day {
                e.subject.clone()
            } else {
                format!(
                    "{}  {}",
                    e.start.with_timezone(&Local).format("%H:%M"),
                    e.subject
                )
            };
            let preview_title = if e.all_day {
                e.subject.clone()
            } else {
                format!(
                    "{}  {}",
                    e.start.with_timezone(&Local).format("%H:%M"),
                    e.subject
                )
            };
            let chip = div()
                .id(gpui::ElementId::Name(
                    format!("week-{week_start}-{}", e.id).into(),
                ))
                // Do not let clicks on an event reach the date cell
                // underneath and start a second event draft, while still
                // allowing wheel events to reach the calendar scroller.
                .block_mouse_except_scroll()
                .col_start(layout.start_col as i16 + 1)
                .col_end((layout.start_col + layout.span) as i16 + 1)
                .h_full()
                .min_w_0()
                .ml(px(3.))
                .mr(px(3.))
                .px_1p5()
                .py_0p5()
                .rounded_sm()
                .when(theme.shadow, |element| element.shadow_xs())
                .when(layout.continues_before, |element| element.rounded_l_none())
                .when(layout.continues_after, |element| element.rounded_r_none())
                .text_xs()
                .text_color(theme.foreground)
                .truncate()
                .when(!read_only, |element| element.cursor_move())
                .bg(event_background)
                .when(moving, |element| element.opacity(0.55))
                .child(title)
                .when(!read_only, |element| {
                    element.on_drag(drag, move |_, cursor_offset, _, cx| {
                        cx.new(|_| EventDragPreview {
                            title: preview_title.clone(),
                            color,
                            cursor_offset,
                        })
                    })
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.calendar.selected = Some(eid.clone());
                    this.focus_shortcuts(window);
                    cx.notify();
                }));
            let chip = chip.context_menu(move |menu, _window, _cx| {
                let edit_entity = entity.clone();
                let delete_entity = entity.clone();
                let edit_event = edit_event.clone();
                let delete_event = delete_event.clone();
                menu.item(
                    PopupMenuItem::new(tr!("calendar-event-edit"))
                        .icon(super::icons::app_icon("pencil"))
                        .disabled(read_only)
                        .on_click(move |_, window, cx| {
                            edit_entity.update(cx, |this, cx| {
                                this.open_event_edit(edit_event.clone(), window, cx);
                            });
                        }),
                )
                .item(
                    PopupMenuItem::new(tr!("calendar-event-delete"))
                        .icon(super::icons::app_icon("trash-2"))
                        .disabled(deleting || read_only)
                        .on_click(move |_, window, cx| {
                            delete_entity.update(cx, |this, cx| {
                                this.confirm_delete_calendar_event(
                                    delete_event.clone(),
                                    window,
                                    cx,
                                );
                            });
                        }),
                )
            });
            chips.push(
                div()
                    .absolute()
                    .top(px(
                        MONTH_EVENT_TOP + f32::from(layout.lane) * MONTH_EVENT_STEP
                    ))
                    .left_0()
                    .right_0()
                    .h(px(MONTH_EVENT_HEIGHT))
                    .grid()
                    .grid_cols(7)
                    .child(chip)
                    .into_any_element(),
            );
        }
        chips
    }
}

struct EventDragPreview {
    title: String,
    color: Hsla,
    cursor_offset: Point<Pixels>,
}

impl gpui::Render for EventDragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let size = gpui::size(px(220.), px(24.));
        div()
            // gpui places the root at `mouse - click offset`. Centering the
            // visual on that offset prevents a differently sized chip from
            // appearing displaced based on where it was grabbed.
            .pl(self.cursor_offset.x - size.width / 2.)
            .pt(self.cursor_offset.y - size.height / 2.)
            .child(
                div()
                    .w(size.width)
                    .h(size.height)
                    .px_1()
                    .flex()
                    .items_center()
                    .rounded_sm()
                    .bg(self.color.opacity(0.25))
                    .text_color(cx.theme().foreground)
                    .text_xs()
                    .truncate()
                    .shadow_md()
                    .child(self.title.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(start: DateTime<Utc>, end: DateTime<Utc>, all_day: bool) -> CalendarEvent {
        CalendarEvent {
            id: "event".into(),
            account_id: AccountId("account".into()),
            read_only: true,
            subject: "Event".into(),
            start,
            end,
            all_day,
            location: String::new(),
            organizer: String::new(),
            preview: String::new(),
            is_cancelled: false,
            online_meeting_url: None,
            web_link: None,
        }
    }

    #[test]
    fn all_day_range_is_visible_on_every_occupied_day() {
        let event = event(
            Utc.with_ymd_and_hms(2026, 6, 23, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 9, 0, 0, 0).unwrap(),
            true,
        );

        assert!(event_occurs_on_day(
            &event,
            NaiveDate::from_ymd_opt(2026, 6, 23).unwrap()
        ));
        assert!(event_occurs_on_day(
            &event,
            NaiveDate::from_ymd_opt(2026, 7, 8).unwrap()
        ));
        assert!(!event_occurs_on_day(
            &event,
            NaiveDate::from_ymd_opt(2026, 7, 9).unwrap()
        ));
        assert_eq!(
            all_day_last_day(&event),
            NaiveDate::from_ymd_opt(2026, 7, 8).unwrap()
        );

        let events = [event];
        let first_week = layout_week_events(&events, NaiveDate::from_ymd_opt(2026, 6, 22).unwrap());
        let middle_week =
            layout_week_events(&events, NaiveDate::from_ymd_opt(2026, 6, 29).unwrap());
        let last_week = layout_week_events(&events, NaiveDate::from_ymd_opt(2026, 7, 6).unwrap());

        assert_eq!((first_week[0].start_col, first_week[0].span), (1, 6));
        assert!(first_week[0].continues_after);
        assert_eq!((middle_week[0].start_col, middle_week[0].span), (0, 7));
        assert!(middle_week[0].continues_before);
        assert!(middle_week[0].continues_after);
        assert_eq!((last_week[0].start_col, last_week[0].span), (0, 3));
        assert!(last_week[0].continues_before);
        assert!(!last_week[0].continues_after);
    }

    #[test]
    fn timed_event_ending_at_midnight_does_not_fill_the_next_day() {
        let event = event(
            to_utc(NaiveDate::from_ymd_opt(2026, 6, 23).unwrap()) + Duration::hours(20),
            to_utc(NaiveDate::from_ymd_opt(2026, 6, 25).unwrap()),
            false,
        );

        assert!(event_occurs_on_day(
            &event,
            NaiveDate::from_ymd_opt(2026, 6, 24).unwrap()
        ));
        assert!(!event_occurs_on_day(
            &event,
            NaiveDate::from_ymd_opt(2026, 6, 25).unwrap()
        ));
    }

    #[test]
    fn month_chunks_round_trip_and_are_contiguous() {
        let day = NaiveDate::from_ymd_opt(2026, 7, 21).unwrap();
        let chunk = month_chunk(day);
        assert_eq!(
            chunk_start(chunk),
            NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()
        );
        assert_eq!(
            chunk_start(chunk + 1),
            NaiveDate::from_ymd_opt(2026, 8, 1).unwrap()
        );
        assert_eq!(
            month_chunk(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()) + 1,
            month_chunk(NaiveDate::from_ymd_opt(2027, 1, 1).unwrap())
        );
    }

    #[test]
    fn fetched_windows_replace_only_their_overlapping_events() {
        let mut state = CalendarViewState::new(CalendarLayout::List);
        let account = AccountId("account".into());
        let july = Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap();
        let august = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let september = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let spanning = event(
            Utc.with_ymd_and_hms(2026, 7, 30, 10, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 8, 2, 10, 0, 0).unwrap(),
            false,
        );

        state.on_events(&account, july, august, vec![spanning.clone()]);
        // The adjacent window returns the same spanning event again; it must
        // not be duplicated.
        state.on_events(&account, august, september, vec![spanning.clone()]);
        assert_eq!(state.events.len(), 1);

        // A refresh of a window it overlaps without it removes it.
        state.on_events(&account, july, august, Vec::new());
        assert!(state.events.is_empty());
    }

    #[test]
    fn requested_chunks_are_grouped_and_not_requested_twice() {
        let mut state = CalendarViewState::new(CalendarLayout::List);
        assert_eq!(state.missing_runs("account", 10, 13), vec![(10, 13)]);
        assert!(state.missing_runs("account", 10, 13).is_empty());
        // A hole between fetched periods yields one run per gap side.
        assert_eq!(state.missing_runs("account", 8, 15), vec![(8, 9), (14, 15)]);
        // Scopes are independent.
        assert_eq!(state.missing_runs("ical:1", 12, 12), vec![(12, 12)]);
    }

    #[test]
    fn dragged_date_ranges_are_ordered_in_both_directions() {
        let earlier = NaiveDate::from_ymd_opt(2026, 6, 23).unwrap();
        let later = NaiveDate::from_ymd_opt(2026, 7, 9).unwrap();

        assert_eq!(ordered_date_range(earlier, later), (earlier, later));
        assert_eq!(ordered_date_range(later, earlier), (earlier, later));
    }

    #[test]
    fn google_maps_location_is_trimmed_and_url_encoded() {
        assert_eq!(
            google_maps_search_url("  1 rue Exemple, Ville-Test  "),
            "https://www.google.com/maps/search/?api=1&query=1%20rue%20Exemple%2C%20Ville-Test"
        );
    }
}
