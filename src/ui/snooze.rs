//! Putting a message off until later.
//!
//! A deadline is an Aviary-side state, like pinning: the message never moves,
//! the list hides it until its time comes, and a tick wakes it. Nothing goes to
//! the provider when a message is put off, so there is nothing to retry, to
//! undo or to reconcile — and, decisively, the message id never changes, which
//! an IMAP move could not promise (`UID MOVE` mints a new one, and the
//! COPY+EXPUNGE fallback does not even report it).
//!
//! The presets are the part most likely to be wrong at a boundary (late
//! evening, a Monday, a deadline already past), so the date arithmetic here is
//! free of gpui and tested on its own.

use super::app::AviaryApp;
use crate::model::{AccountId, MessageRef};
use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone, Utc, Weekday};
use gpui::{prelude::*, Context, Window};
use gpui_component::{notification::Notification, WindowExt};

/// How often deadlines are checked. A message put off until tomorrow morning
/// needs no second precision, and a plain tick needs no re-arming when the
/// deadline set — or the system clock — changes under it.
pub(super) const WAKE_TICK: std::time::Duration = std::time::Duration::from_secs(30);

/// Hour "this evening" means.
const EVENING_HOUR: u32 = 18;
/// Hour every morning preset means.
const MORNING_HOUR: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnoozePreset {
    ThisEvening,
    Tomorrow,
    NextWeek,
}

impl SnoozePreset {
    pub(crate) const ALL: [Self; 3] = [Self::ThisEvening, Self::Tomorrow, Self::NextWeek];

    pub(crate) fn label(self) -> gpui::SharedString {
        match self {
            Self::ThisEvening => tr!("snooze-this-evening"),
            Self::Tomorrow => tr!("snooze-tomorrow"),
            Self::NextWeek => tr!("snooze-next-week"),
        }
    }

    /// When this preset falls, or `None` when it no longer makes sense —
    /// "this evening" at eight in the evening would be a deadline in the past,
    /// and an entry that wakes the message immediately is worse than no entry.
    pub(crate) fn deadline(self, now: DateTime<Local>) -> Option<DateTime<Utc>> {
        let local = match self {
            Self::ThisEvening => at_hour(now, EVENING_HOUR)?,
            Self::Tomorrow => at_hour(now + Duration::days(1), MORNING_HOUR)?,
            Self::NextWeek => at_hour(next_monday(now), MORNING_HOUR)?,
        };
        (local > now).then(|| local.with_timezone(&Utc))
    }
}

/// `day` at `hour` o'clock sharp, in the local zone.
///
/// `None` where that instant does not exist or is ambiguous — a spring-forward
/// gap or an autumn repeat. Both are rare and neither is worth guessing at:
/// the preset simply steps aside for the free-form date picker.
fn at_hour(day: DateTime<Local>, hour: u32) -> Option<DateTime<Local>> {
    let time = NaiveTime::from_hms_opt(hour, 0, 0)?;
    Local
        .from_local_datetime(&day.date_naive().and_time(time))
        .single()
}

/// The coming Monday. On a Monday it is the *next* one, seven days out, since
/// "next week" said today must not mean this morning.
fn next_monday(now: DateTime<Local>) -> DateTime<Local> {
    let days = match now.weekday() {
        Weekday::Mon => 7,
        other => 7 - other.num_days_from_monday() as i64,
    };
    now + Duration::days(days)
}

/// How a pending deadline reads on a row: a time today, a weekday this week,
/// a date beyond it. Same principle as the list's own date column — the closer
/// it is, the more precise it has to be.
pub(crate) fn deadline_label(until: DateTime<Utc>, now: DateTime<Local>) -> String {
    let local = until.with_timezone(&Local);
    let locale = super::datefmt::current_locale();
    let days = (local.date_naive() - now.date_naive()).num_days();
    if days == 0 {
        local.format_localized("%H:%M", locale).to_string()
    } else if days == 1 {
        tr!("snooze-tomorrow-at", { time: local.format_localized("%H:%M", locale).to_string() })
            .to_string()
    } else if (2..7).contains(&days) {
        local
            .format_localized("%a %H:%M", locale)
            .to_string()
            .to_lowercase()
    } else {
        local.format_localized("%d/%m", locale).to_string()
    }
}

/// Midnight-free reading of a date picked in the free-form dialog: a day with
/// no time means that morning, not the stroke of midnight that starts it.
pub(crate) fn morning_of(date: chrono::NaiveDate) -> Option<DateTime<Utc>> {
    let time = NaiveTime::from_hms_opt(MORNING_HOUR, 0, 0)?;
    Local
        .from_local_datetime(&date.and_time(time))
        .single()
        .map(|local| local.with_timezone(&Utc))
}

/// Fills a "remind me later" menu for `targets`: the presets that still lie
/// ahead, then the free-form date.
///
/// Shared by the row menu, the reader and the bulk toolbar so the three offer
/// the same deadlines — and so a preset that has passed disappears everywhere
/// at once rather than in whichever of the three was written last.
pub(super) fn append_snooze_menu(
    mut menu: gpui_component::menu::PopupMenu,
    entity: &gpui::Entity<AviaryApp>,
    targets: &[MessageRef],
    offline: bool,
) -> gpui_component::menu::PopupMenu {
    use gpui_component::menu::PopupMenuItem;

    let now = Local::now();
    for preset in SnoozePreset::ALL {
        let Some(deadline) = preset.deadline(now) else {
            continue;
        };
        let entity = entity.clone();
        let targets = targets.to_vec();
        menu = menu.item(
            PopupMenuItem::new(format!(
                "{}  ·  {}",
                preset.label(),
                deadline_label(deadline, now)
            ))
            .disabled(offline)
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| {
                    this.snooze_messages(&targets, deadline, window, cx);
                });
            }),
        );
    }
    let entity = entity.clone();
    let targets = targets.to_vec();
    menu.separator().item(
        PopupMenuItem::new(tr!("snooze-pick-date"))
            .disabled(offline)
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| {
                    this.open_snooze_dialog(targets.clone(), window, cx);
                });
            }),
    )
}

impl AviaryApp {
    /// Free-form deadline: a date, woken in the morning.
    ///
    /// A date and no time, deliberately. An hour picker would be a second field
    /// for a decision that is nearly always "some morning, not today", and the
    /// three presets already cover the times that are actually asked for.
    pub(crate) fn open_snooze_dialog(
        &mut self,
        targets: Vec<MessageRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_component::date_picker::{DatePicker, DatePickerState};

        if targets.is_empty() {
            return;
        }
        let tomorrow = (Local::now() + Duration::days(1)).date_naive();
        let picker = cx.new(|cx| {
            let mut state = DatePickerState::new(window, cx).date_format("%d/%m/%Y");
            state.set_date(tomorrow, window, cx);
            state
        });
        self.snooze_dialog_picker = Some(picker.clone());
        let entity = cx.entity();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let entity = entity.clone();
            let picker = picker.clone();
            let targets = targets.clone();
            dialog
                .title(tr!("snooze-pick-date-title"))
                .confirm()
                .child(DatePicker::new(&picker))
                .on_ok(move |_, window, cx| {
                    let Some(date) = picker.read(cx).date().start() else {
                        return false;
                    };
                    let Some(deadline) = morning_of(date) else {
                        return false;
                    };
                    // A date already gone would wake the message on the next
                    // tick — the dialog stays open instead of pretending.
                    if deadline <= Utc::now() {
                        return false;
                    }
                    entity.update(cx, |this, cx| {
                        this.snooze_messages(&targets, deadline, window, cx);
                        this.snooze_dialog_picker = None;
                    });
                    true
                })
        });
    }

    /// Puts messages off until `until`: they leave every listing on the spot
    /// and come back when their time is up.
    ///
    /// The toast is the feedback and the way back — the messages vanish from
    /// the list, and without being told when they return, that reads as a
    /// deletion. No undo window: nothing was sent, so cancelling is just
    /// putting them back, which the "snoozed" filter allows at any time.
    pub(crate) fn snooze_messages(
        &mut self,
        references: &[MessageRef],
        until: DateTime<Utc>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if references.is_empty() {
            return;
        }
        for reference in references {
            self.settings
                .snooze_message(&reference.account_id, &reference.id, until);
        }
        self.settings.save();
        self.clear_message_selection();
        self.invalidate_message_list();
        let deadline = deadline_label(until, Local::now());
        window.push_notification(
            Notification::info(if references.len() == 1 {
                tr!("snooze-set", { deadline: deadline })
            } else {
                tr!("snooze-set-bulk", { count: references.len(), deadline: deadline })
            }),
            cx,
        );
        cx.notify();
    }

    /// Cancels a deadline, putting the message back where it always was.
    pub(crate) fn unsnooze_message(
        &mut self,
        account_id: &AccountId,
        message_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.settings.unsnooze_message(account_id, message_id) {
            return;
        }
        self.settings.save();
        self.leave_empty_snooze_filter();
        self.invalidate_message_list();
        window.push_notification(Notification::info(tr!("snooze-cancelled")), cx);
        cx.notify();
    }

    /// Wakes whatever has come due. Called on a tick, and once at startup so a
    /// deadline that fell while Aviary was closed is not missed.
    ///
    /// A woken message is marked unread, which is the whole point of putting it
    /// off: it comes back needing attention. Only the ones still loaded — the
    /// read state of a message no longer in any list cannot be read, let alone
    /// changed, and its deadline is dropped either way rather than kept alive
    /// forever against an id the provider may have retired.
    pub(crate) fn wake_due_snoozes(&mut self, cx: &mut Context<Self>) {
        let due = self.settings.take_due_snoozes(Utc::now());
        if due.is_empty() {
            return;
        }
        self.settings.save();
        let woken: Vec<_> = due
            .iter()
            .filter_map(|(account_id, id)| {
                self.mailbox
                    .messages
                    .iter()
                    .find(|header| &header.account_id == account_id && &header.id == id)
                    .cloned()
            })
            .collect();
        let unread: Vec<_> = woken
            .iter()
            .filter(|header| header.is_read)
            .map(|header| MessageRef {
                account_id: header.account_id.clone(),
                id: header.id.clone(),
            })
            .collect();
        if !unread.is_empty() {
            self.bulk_mark_unread_silently(unread, cx);
        }
        if self.settings.global.notifications_enabled {
            for header in &woken {
                crate::notify::new_message(header, self.notification_tx.clone());
            }
        }
        self.leave_empty_snooze_filter();
        self.invalidate_message_list();
        self.update_tray_unread();
        cx.notify();
    }

    /// Drops the "snoozed" filter once nothing is put off any more. Leaving it
    /// on would show an empty list and no reason for it — the messages did not
    /// disappear, they came back to the list the filter is hiding.
    fn leave_empty_snooze_filter(&mut self) {
        let empty = self
            .settings
            .accounts
            .values()
            .all(|account| account.snoozed_messages.is_empty());
        if empty {
            self.mailbox.show_snoozed_only = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    fn local(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("unambiguous local time")
    }

    #[test]
    fn this_evening_is_six_and_disappears_once_it_has_passed() {
        // A Wednesday, mid-morning.
        let morning = local(2026, 7, 15, 9, 30);
        let evening = SnoozePreset::ThisEvening
            .deadline(morning)
            .expect("still ahead");
        assert_eq!(evening.with_timezone(&Local).hour(), EVENING_HOUR);
        assert_eq!(evening.with_timezone(&Local).date_naive().day(), 15);

        assert!(SnoozePreset::ThisEvening
            .deadline(local(2026, 7, 15, 20, 0))
            .is_none());
    }

    #[test]
    fn tomorrow_is_the_next_morning_whatever_the_hour() {
        for hour in [7, 12, 23] {
            let deadline = SnoozePreset::Tomorrow
                .deadline(local(2026, 7, 15, hour, 0))
                .expect("always ahead");
            let deadline = deadline.with_timezone(&Local);
            assert_eq!(deadline.date_naive().day(), 16, "from {hour}h");
            assert_eq!(deadline.hour(), MORNING_HOUR, "from {hour}h");
        }
    }

    /// "Next week" asked on a Monday must not land on the same morning it is
    /// asked — the whole point is to get the message off today's list.
    #[test]
    fn next_week_is_the_coming_monday_and_never_today() {
        // 2026-07-13 is a Monday, 2026-07-15 a Wednesday.
        let from_monday = SnoozePreset::NextWeek
            .deadline(local(2026, 7, 13, 9, 0))
            .expect("ahead");
        assert_eq!(from_monday.with_timezone(&Local).date_naive().day(), 20);

        let from_wednesday = SnoozePreset::NextWeek
            .deadline(local(2026, 7, 15, 9, 0))
            .expect("ahead");
        let from_wednesday = from_wednesday.with_timezone(&Local);
        assert_eq!(from_wednesday.date_naive().day(), 20);
        assert_eq!(from_wednesday.weekday(), Weekday::Mon);
        assert_eq!(from_wednesday.hour(), MORNING_HOUR);
    }

    #[test]
    fn every_preset_is_in_the_future_or_absent() {
        for hour in 0..24 {
            let now = local(2026, 7, 15, hour, 17);
            for preset in SnoozePreset::ALL {
                if let Some(deadline) = preset.deadline(now) {
                    assert!(
                        deadline > now.with_timezone(&Utc),
                        "{preset:?} at {hour}h is not ahead"
                    );
                }
            }
        }
    }

    /// A date typed into the dialog means that morning: midnight would wake
    /// the message during the night before the day the user picked.
    #[test]
    fn a_picked_date_wakes_in_the_morning() {
        let deadline =
            morning_of(chrono::NaiveDate::from_ymd_opt(2026, 8, 3).expect("valid date")).unwrap();
        let deadline = deadline.with_timezone(&Local);
        assert_eq!(deadline.hour(), MORNING_HOUR);
        assert_eq!(deadline.date_naive().day(), 3);
    }
}
