//! In-memory application log: level filters, copy, and clear actions.

use super::app::AviaryApp;
use crate::logging::LogEntry;
use gpui::{div, prelude::*, px, ClipboardItem, Context};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex, v_flex, ActiveTheme, IconName, Selectable, Sizable, StyledExt,
};
use log::{Level, LevelFilter};

fn visible(entry: &LogEntry, filter: LevelFilter) -> bool {
    match filter {
        LevelFilter::Off => false,
        LevelFilter::Error => entry.level == Level::Error,
        LevelFilter::Warn => matches!(entry.level, Level::Error | Level::Warn),
        LevelFilter::Info => matches!(entry.level, Level::Error | Level::Warn | Level::Info),
        LevelFilter::Debug => entry.level != Level::Trace,
        LevelFilter::Trace => true,
    }
}

fn level_label(level: Level) -> gpui::SharedString {
    match level {
        Level::Error => tr!("logs-level-error"),
        Level::Warn => tr!("logs-level-warn"),
        Level::Info => tr!("logs-level-info"),
        Level::Debug => tr!("logs-level-debug"),
        Level::Trace => tr!("logs-level-trace"),
    }
}

fn format_entry(entry: &LogEntry) -> String {
    format!(
        "{} {:<6} {} — {}",
        entry.timestamp,
        level_label(entry.level),
        entry.target,
        entry.message
    )
}

impl AviaryApp {
    pub(super) fn render_logs_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let all_entries = crate::logging::entries();
        let entries: Vec<LogEntry> = all_entries
            .into_iter()
            .filter(|entry| visible(entry, self.log_filter))
            .collect();
        let count = entries.len();
        let filter_button =
            |id: &'static str, label: String, filter: LevelFilter, cx: &mut Context<Self>| {
                Button::new(id)
                    .small()
                    .label(label)
                    .selected(self.log_filter == filter)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.log_filter = filter;
                        cx.notify();
                    }))
            };
        let copied_entries = entries.clone();

        let mut rows = v_flex().w_full();
        if entries.is_empty() {
            rows = rows.child(
                div()
                    .p_6()
                    .text_color(theme.muted_foreground)
                    .child(tr!("logs-empty")),
            );
        } else {
            for (index, entry) in entries.iter().rev().enumerate() {
                let level_color = match entry.level {
                    Level::Error => theme.danger,
                    Level::Warn => theme.warning,
                    Level::Info => theme.info,
                    Level::Debug | Level::Trace => theme.muted_foreground,
                };
                let copy_text = format_entry(entry);
                rows = rows.child(
                    h_flex()
                        .w_full()
                        .items_start()
                        .gap_3()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .font_family("JetBrains Mono")
                        .text_xs()
                        .child(
                            div()
                                .w(px(92.))
                                .flex_shrink_0()
                                .text_color(theme.muted_foreground)
                                .child(entry.timestamp.clone()),
                        )
                        .child(
                            div()
                                .w(px(62.))
                                .flex_shrink_0()
                                .font_semibold()
                                .text_color(level_color)
                                .child(level_label(entry.level)),
                        )
                        .child(
                            div()
                                .w(px(190.))
                                .flex_shrink_0()
                                .truncate()
                                .text_color(theme.muted_foreground)
                                .child(entry.target.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_normal()
                                .child(entry.message.clone()),
                        )
                        .child(
                            Button::new(gpui::ElementId::Name(
                                format!("logs-copy-entry-{index}").into(),
                            ))
                            .ghost()
                            .xsmall()
                            .flex_shrink_0()
                            .icon(IconName::Copy)
                            .tooltip(tr!("logs-copy-entry"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                            }),
                        ),
                );
            }
        }

        v_flex()
            .w_full()
            .gap_3()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .flex_wrap()
                    .child(div().text_lg().font_semibold().child(tr!("logs-title")))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(tr!("logs-count", { count: count })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("logs-copy")
                            .ghost()
                            .small()
                            .icon(IconName::Copy)
                            .label(tr!("logs-copy"))
                            .on_click(move |_, _, cx| {
                                let text = copied_entries
                                    .iter()
                                    .map(format_entry)
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                            }),
                    )
                    .child(
                        Button::new("logs-clear")
                            .ghost()
                            .small()
                            .icon(IconName::Delete)
                            .label(tr!("logs-clear"))
                            .on_click(cx.listener(|_, _, _, cx| {
                                crate::logging::clear();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .child(filter_button(
                        "logs-error",
                        tr!("logs-filter-errors").to_string(),
                        LevelFilter::Error,
                        cx,
                    ))
                    .child(filter_button(
                        "logs-warn",
                        tr!("logs-filter-warnings").to_string(),
                        LevelFilter::Warn,
                        cx,
                    ))
                    .child(filter_button(
                        "logs-info",
                        tr!("logs-level-info").to_string(),
                        LevelFilter::Info,
                        cx,
                    ))
                    .child(filter_button(
                        "logs-debug",
                        tr!("logs-level-debug").to_string(),
                        LevelFilter::Debug,
                        cx,
                    )),
            )
            .child(
                div()
                    .id("logs-entries")
                    .w_full()
                    .border_1()
                    .border_color(theme.border)
                    .rounded(theme.radius)
                    .overflow_hidden()
                    .child(rows),
            )
    }
}
