//! Préférences → Étiquettes: per-account tag management. Colors are applied
//! server-side (Outlook category presets / Gmail label palette), so every
//! client sees the change; rename and delete reuse the existing runtime
//! commands. IMAP keywords have no registry: no color, no rename.

use super::super::app::AviaryApp;
use super::super::{icons, util};
use crate::model::{Account, AccountId, Provider, Tag};
use crate::providers::tag_color_palette;
use crate::runtime::Cmd;
use gpui::{div, prelude::*, Context, ElementId, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputState},
    menu::{DropdownMenu, PopupMenuItem},
    v_flex, ActiveTheme, Disableable, Sizable, StyledExt, WindowExt,
};

impl AviaryApp {
    pub(super) fn render_settings_tags(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let accounts = self.ordered_accounts();
        for account in &accounts {
            self.ensure_tags_loaded(&account.id);
        }
        let theme = cx.theme().clone();
        let mut root = self.section(&tr!("settings-tags-title"), cx).child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(tr!("settings-tags-hint")),
        );
        for account in accounts {
            let tags = self
                .tags_by_account
                .get(&account.id)
                .cloned()
                .unwrap_or_default();
            let offline = self.offline_accounts.contains(&account.id);
            let mut section = v_flex()
                .gap_1()
                .mt_2()
                .child(div().text_sm().font_semibold().child(account.email.clone()));
            if tags.is_empty() {
                section = section.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(tr!("tags-picker-empty")),
                );
            }
            for tag in tags {
                section = section.child(self.render_settings_tag_row(&account, tag, offline, cx));
            }
            let aid = account.id.clone();
            section = section.child(
                h_flex().pt_1().child(
                    Button::new(ElementId::Name(format!("tag-add-{}", account.id.0).into()))
                        .outline()
                        .xsmall()
                        .icon(icons::app_icon("plus"))
                        .label(tr!("tags-create"))
                        .disabled(offline)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_create_tag_dialog(aid.clone(), window, cx);
                        })),
                ),
            );
            root = root.child(section);
        }
        root
    }

    fn render_settings_tag_row(
        &self,
        account: &Account,
        tag: Tag,
        offline: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let palette = tag_color_palette(account.provider);
        let color = tag
            .color
            .map(util::packed_color)
            .unwrap_or_else(|| util::name_color(&tag.display_name));
        let aid = account.id.clone();
        let row_key = format!("{}-{}", account.id.0, tag.id);
        let mut row = h_flex()
            .gap_2()
            .items_center()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .child(icons::app_icon("tag").text_color(color).small())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .child(tag.display_name.clone()),
            );
        if !palette.is_empty() {
            let entity = cx.entity();
            let color_aid = aid.clone();
            let tag_id = tag.id.clone();
            row = row.child(
                Button::new(ElementId::Name(format!("tag-color-{row_key}").into()))
                    .ghost()
                    .xsmall()
                    .icon(icons::app_icon("palette"))
                    .tooltip(tr!("tags-color"))
                    .disabled(offline)
                    .dropdown_menu(move |mut menu, _window, _cx| {
                        for &color in palette {
                            let entity = entity.clone();
                            let aid = color_aid.clone();
                            let tag_id = tag_id.clone();
                            menu = menu.item(
                                PopupMenuItem::new(format!("#{color:06X}"))
                                    .icon(
                                        icons::app_icon("tag")
                                            .text_color(util::packed_color(color)),
                                    )
                                    .on_click(move |_, _, cx| {
                                        entity.update(cx, |this, cx| {
                                            this.send(Cmd::SetTagColor {
                                                account_id: aid.clone(),
                                                id: tag_id.clone(),
                                                color,
                                            });
                                            cx.notify();
                                        });
                                    }),
                            );
                        }
                        menu
                    }),
            );
        }
        {
            let rename_aid = aid.clone();
            let tag_id = tag.id.clone();
            let tag_name = tag.display_name.clone();
            row = row.child(
                Button::new(ElementId::Name(format!("tag-rename-{row_key}").into()))
                    .ghost()
                    .xsmall()
                    .icon(icons::app_icon("pencil"))
                    .tooltip(tr!("tags-rename"))
                    .disabled(offline || account.provider == Provider::Imap)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_settings_rename_tag_dialog(
                            rename_aid.clone(),
                            tag_id.clone(),
                            tag_name.clone(),
                            window,
                            cx,
                        );
                    })),
            );
        }
        {
            let delete_aid = aid.clone();
            let tag_id = tag.id.clone();
            let tag_name = tag.display_name.clone();
            row = row.child(
                Button::new(ElementId::Name(format!("tag-delete-{row_key}").into()))
                    .ghost()
                    .xsmall()
                    .icon(icons::app_icon("trash-2").text_color(theme.danger))
                    .tooltip(tr!("tags-delete"))
                    .disabled(offline)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_settings_delete_tag_dialog(
                            delete_aid.clone(),
                            tag_id.clone(),
                            tag_name.clone(),
                            window,
                            cx,
                        );
                    })),
            );
        }
        row
    }

    fn open_settings_rename_tag_dialog(
        &mut self,
        account_id: AccountId,
        tag_id: String,
        current: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current));
        let entity = cx.entity();
        WindowExt::open_dialog(window, cx, move |dialog, _window, _cx| {
            let entity = entity.clone();
            let input = input.clone();
            let account_id = account_id.clone();
            let tag_id = tag_id.clone();
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
                        this.send(Cmd::RenameTag {
                            account_id: account_id.clone(),
                            id: tag_id.clone(),
                            new_name,
                        });
                        cx.notify();
                    });
                    true
                })
        });
    }

    fn open_settings_delete_tag_dialog(
        &mut self,
        account_id: AccountId,
        tag_id: String,
        tag_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entity = cx.entity();
        WindowExt::open_dialog(window, cx, move |dialog, _window, _cx| {
            let entity = entity.clone();
            let account_id = account_id.clone();
            let tag_id = tag_id.clone();
            dialog
                .title(tr!("tags-delete-title"))
                .confirm()
                .child(
                    div()
                        .text_sm()
                        .child(tr!("tags-delete-confirm", { name: tag_name.clone() })),
                )
                .on_ok(move |_, _window, cx| {
                    entity.update(cx, |this, cx| {
                        this.send(Cmd::DeleteTag {
                            account_id: account_id.clone(),
                            id: tag_id.clone(),
                        });
                        cx.notify();
                    });
                    true
                })
        });
    }
}
