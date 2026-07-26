//! Shared CRUD editor for signatures and message templates.

use super::super::app::AviaryApp;
use super::super::block_editor::BlockEditor;
use super::super::settings::MailBodyOptions;
use super::{labelled, SettingsTab, SettingsUi};
use crate::blocks::{BlockKind, TEMPLATE_CURSOR_PLACEHOLDER};
use crate::model::{AccountId, InlineImage};
use gpui::{div, prelude::*, px, Context, Entity, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState},
    v_flex, ActiveTheme, IconName, Sizable, WindowExt,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SnippetKind {
    Signature,
    Template,
}

#[derive(Clone)]
struct SnippetItem {
    id: i64,
    name: String,
    is_default: bool,
    kinds: Vec<BlockKind>,
    images: Vec<InlineImage>,
}

impl SnippetKind {
    fn tab(self) -> SettingsTab {
        match self {
            Self::Signature => SettingsTab::Signatures,
            Self::Template => SettingsTab::Modeles,
        }
    }

    fn body_placeholder(self) -> gpui::SharedString {
        match self {
            Self::Signature => tr!("signatures-body-placeholder"),
            Self::Template => tr!("templates-body-placeholder"),
        }
    }

    fn name_placeholder(self) -> gpui::SharedString {
        match self {
            Self::Signature => tr!("settings-signatures-name-hint"),
            Self::Template => tr!("templates-name-placeholder"),
        }
    }

    fn no_account_label(self) -> String {
        match self {
            Self::Signature => tr!("signatures-no-account-prompt").to_string(),
            Self::Template => tr!("templates-no-account").to_string(),
        }
    }

    fn section_title(self) -> String {
        match self {
            Self::Signature => tr!("settings-tab-signatures").to_string(),
            Self::Template => tr!("settings-tab-templates").to_string(),
        }
    }

    fn empty_label(self) -> String {
        match self {
            Self::Signature => tr!("signatures-empty-for-account").to_string(),
            Self::Template => tr!("templates-empty-for-account").to_string(),
        }
    }

    fn edit_label(self) -> String {
        match self {
            Self::Signature => tr!("settings-signatures-edit").to_string(),
            Self::Template => tr!("templates-edit").to_string(),
        }
    }

    fn delete_label(self) -> String {
        match self {
            Self::Signature => tr!("settings-signatures-remove").to_string(),
            Self::Template => tr!("delete").to_string(),
        }
    }

    fn form_title(self, editing: bool) -> gpui::SharedString {
        match (self, editing) {
            (Self::Signature, true) => tr!("signatures-edit-title"),
            (Self::Signature, false) => tr!("signatures-new-title"),
            (Self::Template, true) => tr!("templates-edit-title"),
            (Self::Template, false) => tr!("templates-new-title"),
        }
    }

    fn body_label(self) -> String {
        match self {
            Self::Signature => tr!("signatures-rich-body-label").to_string(),
            Self::Template => tr!("templates-rich-body-label").to_string(),
        }
    }

    fn html_help(self) -> String {
        match self {
            Self::Signature => tr!("signatures-html-paste-help").to_string(),
            Self::Template => tr!("templates-html-paste-help").to_string(),
        }
    }

    fn default_label(self) -> String {
        match self {
            Self::Signature => tr!("signatures-set-default-checkbox").to_string(),
            Self::Template => tr!("templates-set-default-checkbox").to_string(),
        }
    }

    fn save_label(self) -> String {
        match self {
            Self::Signature => tr!("signatures-save-full").to_string(),
            Self::Template => tr!("templates-save-full").to_string(),
        }
    }

    fn html_placeholder(self) -> String {
        match self {
            Self::Signature => tr!("signatures-html-placeholder").to_string(),
            Self::Template => tr!("templates-html-placeholder").to_string(),
        }
    }

    fn html_dialog_title(self) -> String {
        match self {
            Self::Signature => tr!("signatures-html-dialog-title").to_string(),
            Self::Template => tr!("templates-html-dialog-title").to_string(),
        }
    }

    fn name_required_error(self) -> String {
        match self {
            Self::Signature => tr!("signatures-name-required").to_string(),
            Self::Template => tr!("templates-name-required").to_string(),
        }
    }

    fn id_prefix(self) -> &'static str {
        match self {
            Self::Signature => "signature",
            Self::Template => "template",
        }
    }

    fn editor_id(self) -> &'static str {
        match self {
            Self::Signature => "signature-editor-scroll",
            Self::Template => "template-editor-scroll",
        }
    }

    fn image_button_id(self) -> &'static str {
        match self {
            Self::Signature => "signature-insert-image",
            Self::Template => "template-insert-image",
        }
    }

    fn html_button_id(self) -> &'static str {
        match self {
            Self::Signature => "signature-insert-html",
            Self::Template => "template-insert-html",
        }
    }

    fn cursor_button_id(self) -> &'static str {
        match self {
            Self::Signature => "signature-insert-cursor",
            Self::Template => "template-insert-cursor",
        }
    }

    fn default_checkbox_id(self) -> &'static str {
        match self {
            Self::Signature => "sig-default",
            Self::Template => "template-default",
        }
    }

    fn save_button_id(self) -> &'static str {
        match self {
            Self::Signature => "save-sig",
            Self::Template => "save-tmpl",
        }
    }

    fn cancel_button_id(self) -> &'static str {
        match self {
            Self::Signature => "cancel-sig",
            Self::Template => "cancel-tmpl",
        }
    }
}

impl SettingsUi {
    fn snippet(&self, kind: SnippetKind) -> &super::RichSnippetEditorState {
        match kind {
            SnippetKind::Signature => &self.signature,
            SnippetKind::Template => &self.template,
        }
    }

    fn snippet_mut(&mut self, kind: SnippetKind) -> &mut super::RichSnippetEditorState {
        match kind {
            SnippetKind::Signature => &mut self.signature,
            SnippetKind::Template => &mut self.template,
        }
    }
}

fn make_editor(
    kind: SnippetKind,
    kinds: Vec<BlockKind>,
    images: Vec<InlineImage>,
    options: MailBodyOptions,
    window: &mut Window,
    cx: &mut Context<AviaryApp>,
) -> Entity<BlockEditor> {
    cx.new(|cx| {
        if kind == SnippetKind::Template {
            BlockEditor::new_template_editor(
                kinds,
                images,
                options,
                &kind.body_placeholder(),
                window,
                cx,
            )
        } else {
            BlockEditor::new(
                kinds,
                images,
                false,
                options,
                &kind.body_placeholder(),
                window,
                cx,
            )
        }
    })
}

impl AviaryApp {
    fn snippet_items(&self, kind: SnippetKind, account_id: &AccountId) -> Vec<SnippetItem> {
        let settings = self.settings.account_or_default(Some(account_id));
        match kind {
            SnippetKind::Signature => settings
                .signatures
                .into_iter()
                .map(|snippet| SnippetItem {
                    id: snippet.id,
                    name: snippet.name,
                    is_default: snippet.is_default,
                    kinds: snippet.blocks.into_iter().map(|block| block.kind).collect(),
                    images: snippet.images,
                })
                .collect(),
            SnippetKind::Template => settings
                .templates
                .into_iter()
                .map(|snippet| SnippetItem {
                    id: snippet.id,
                    name: snippet.name,
                    is_default: snippet.is_default,
                    kinds: snippet.blocks.into_iter().map(|block| block.kind).collect(),
                    images: snippet.images,
                })
                .collect(),
        }
    }

    pub(super) fn render_rich_snippets(
        &mut self,
        kind: SnippetKind,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let account_selector = self.render_settings_account_selector(kind.tab(), cx);
        let Some(account_id) = self.current_account_id.clone() else {
            return v_flex()
                .child(account_selector)
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(kind.no_account_label()),
                )
                .into_any_element();
        };
        let items = self.snippet_items(kind, &account_id);
        let (editing, is_default, name, editor) = {
            let state = self
                .settings_ui
                .as_ref()
                .expect("settings_ui")
                .snippet(kind);
            (
                state.editing_id,
                state.is_default,
                state.name.clone(),
                state.editor.clone(),
            )
        };

        let mut list = v_flex().gap_2();
        for item in items.clone() {
            let id = item.id;
            let edit_kind = kind;
            let delete_kind = kind;
            let delete_account_id = account_id.clone();
            list = list.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .p_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(div().flex_1().child(item.name))
                    .when(item.is_default, |element| {
                        element.child(
                            div()
                                .text_xs()
                                .text_color(theme.primary)
                                .child(tr!("settings-account-default")),
                        )
                    })
                    .child(
                        Button::new(gpui::ElementId::Name(
                            format!("edit-{}-{id}", kind.id_prefix()).into(),
                        ))
                        .ghost()
                        .xsmall()
                        .label(kind.edit_label())
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.edit_rich_snippet(edit_kind, id, window, cx);
                            },
                        )),
                    )
                    .child(
                        Button::new(gpui::ElementId::Name(
                            format!("delete-{}-{id}", kind.id_prefix()).into(),
                        ))
                        .danger()
                        .xsmall()
                        .label(kind.delete_label())
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.delete_rich_snippet(
                                    delete_kind,
                                    &delete_account_id,
                                    id,
                                    window,
                                    cx,
                                );
                            },
                        )),
                    ),
            );
        }
        if items.is_empty() {
            list = list.child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(kind.empty_label()),
            );
        }

        let image_editor = editor.clone();
        let cursor_editor = editor.clone();
        v_flex()
            .child(account_selector)
            .child(self.section(&kind.section_title(), cx).child(list))
            .child(
                self.section(&kind.form_title(editing.is_some()), cx)
                    .child(labelled(&tr!("folders-name-label"), &name, cx))
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                h_flex()
                                    .w_full()
                                    .items_center()
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_sm()
                                            .text_color(theme.muted_foreground)
                                            .child(kind.body_label()),
                                    )
                                    .when(kind == SnippetKind::Template, |element| {
                                        element.child(
                                            Button::new(kind.cursor_button_id())
                                                .ghost()
                                                .small()
                                                .icon(IconName::Plus)
                                                .label(tr!("templates-add-cursor"))
                                                .on_click(move |_, window, cx| {
                                                    cursor_editor.update(cx, |editor, cx| {
                                                        editor.insert_template_cursor(window, cx)
                                                    });
                                                }),
                                        )
                                    })
                                    .child(
                                        Button::new(kind.image_button_id())
                                            .ghost()
                                            .small()
                                            .icon(super::super::icons::app_icon("image"))
                                            .label(tr!("signatures-add-image"))
                                            .on_click(move |_, window, cx| {
                                                image_editor.update(cx, |editor, cx| {
                                                    editor.prompt_insert_image(window, cx)
                                                });
                                            }),
                                    )
                                    .child(
                                        Button::new(kind.html_button_id())
                                            .ghost()
                                            .small()
                                            .icon(IconName::SquareTerminal)
                                            .label(tr!("signatures-add-html"))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.prompt_rich_snippet_html(kind, window, cx);
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .id(kind.editor_id())
                                    .h(px(if kind == SnippetKind::Template {
                                        320.
                                    } else {
                                        280.
                                    }))
                                    .w_full()
                                    .overflow_y_scroll()
                                    .p_2()
                                    .rounded(theme.radius)
                                    .border_1()
                                    .border_color(theme.border)
                                    .child(editor),
                            )
                            .when(kind == SnippetKind::Template, |element| {
                                element.child(
                                    div().text_xs().text_color(theme.muted_foreground).child(
                                        tr!("templates-cursor-help", {
                                            marker: TEMPLATE_CURSOR_PLACEHOLDER
                                        }),
                                    ),
                                )
                            })
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(kind.html_help()),
                            ),
                    )
                    .child(
                        Checkbox::new(kind.default_checkbox_id())
                            .checked(is_default)
                            .label(kind.default_label())
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                if let Some(ui) = &mut this.settings_ui {
                                    ui.snippet_mut(kind).is_default = *checked;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(kind.save_button_id())
                                    .primary()
                                    .label(kind.save_label())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.commit_rich_snippet(kind, window, cx);
                                    })),
                            )
                            .when(editing.is_some(), |element| {
                                element.child(
                                    Button::new(kind.cancel_button_id())
                                        .ghost()
                                        .label(tr!("cancel"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.clear_rich_snippet_form(kind, window, cx);
                                        })),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    fn edit_rich_snippet(
        &mut self,
        kind: SnippetKind,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(account_id) = self.current_account_id.clone() else {
            return;
        };
        let Some(item) = self
            .snippet_items(kind, &account_id)
            .into_iter()
            .find(|item| item.id == id)
        else {
            return;
        };
        let editor = make_editor(
            kind,
            item.kinds,
            item.images,
            self.settings.global.mail_body_options(),
            window,
            cx,
        );
        if let Some(ui) = &mut self.settings_ui {
            let state = ui.snippet_mut(kind);
            state.editing_id = Some(id);
            state.is_default = item.is_default;
            state.editor = editor;
            state
                .name
                .update(cx, |input, cx| input.set_value(item.name, window, cx));
        }
        cx.notify();
    }

    fn delete_rich_snippet(
        &mut self,
        kind: SnippetKind,
        account_id: &AccountId,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings = self.settings.account_mut(account_id);
        match kind {
            SnippetKind::Signature => settings.signatures.retain(|snippet| snippet.id != id),
            SnippetKind::Template => settings.templates.retain(|snippet| snippet.id != id),
        }
        self.settings.save();
        if kind == SnippetKind::Signature {
            self.refresh_compose_signatures(cx);
        }
        let editing_deleted = self
            .settings_ui
            .as_ref()
            .is_some_and(|ui| ui.snippet(kind).editing_id == Some(id));
        if editing_deleted {
            self.clear_rich_snippet_form(kind, window, cx);
        }
        cx.notify();
    }

    fn prompt_rich_snippet_html(
        &mut self,
        kind: SnippetKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self
            .settings_ui
            .as_ref()
            .map(|ui| ui.snippet(kind).editor.clone())
        else {
            return;
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .rows(12)
                .placeholder(kind.html_placeholder())
        });
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let input = input.clone();
            let editor = editor.clone();
            dialog
                .title(kind.html_dialog_title())
                .confirm()
                .child(Input::new(&input).h(px(260.)))
                .on_ok(move |_, window, cx| {
                    let html = input.read(cx).value().trim().to_string();
                    if html.is_empty() {
                        return false;
                    }
                    editor.update(cx, |editor, cx| editor.insert_html(html, window, cx));
                    true
                })
        });
    }

    fn clear_rich_snippet_form(
        &mut self,
        kind: SnippetKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = make_editor(
            kind,
            Vec::new(),
            Vec::new(),
            self.settings.global.mail_body_options(),
            window,
            cx,
        );
        if let Some(ui) = &mut self.settings_ui {
            let state = ui.snippet_mut(kind);
            state.editing_id = None;
            state.is_default = false;
            state.editor = editor;
            state
                .name
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
    }

    fn commit_rich_snippet(
        &mut self,
        kind: SnippetKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(account_id) = self.current_account_id.clone() else {
            return;
        };
        let Some(ui) = &self.settings_ui else {
            return;
        };
        let state = ui.snippet(kind);
        let name = state.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.notify_error(kind.name_required_error(), window, cx);
            return;
        }
        let blocks = state.editor.read(cx).to_blocks(cx);
        let images = state.editor.read(cx).images().to_vec();
        let editing = state.editing_id;
        let is_default = state.is_default;
        let settings = self.settings.account_mut(&account_id);

        match kind {
            SnippetKind::Signature => {
                if is_default {
                    for snippet in &mut settings.signatures {
                        snippet.is_default = false;
                    }
                }
                if let Some(id) = editing {
                    if let Some(snippet) = settings
                        .signatures
                        .iter_mut()
                        .find(|snippet| snippet.id == id)
                    {
                        snippet.name = name;
                        snippet.blocks = blocks;
                        snippet.images = images;
                        snippet.is_default = is_default;
                    }
                } else {
                    settings.signature_seq += 1;
                    let id = settings.signature_seq;
                    let position = settings.signatures.len() as i64;
                    settings.signatures.push(crate::model::Signature {
                        id,
                        account_id,
                        name,
                        is_default,
                        position,
                        blocks,
                        images,
                    });
                }
            }
            SnippetKind::Template => {
                if is_default {
                    for snippet in &mut settings.templates {
                        snippet.is_default = false;
                    }
                }
                if let Some(id) = editing {
                    if let Some(snippet) = settings
                        .templates
                        .iter_mut()
                        .find(|snippet| snippet.id == id)
                    {
                        snippet.name = name;
                        snippet.blocks = blocks;
                        snippet.images = images;
                        snippet.is_default = is_default;
                    }
                } else {
                    settings.template_seq += 1;
                    let id = settings.template_seq;
                    let position = settings.templates.len() as i64;
                    settings.templates.push(crate::model::Template {
                        id,
                        account_id,
                        name,
                        is_default,
                        position,
                        blocks,
                        images,
                    });
                }
            }
        }
        self.settings.save();
        if kind == SnippetKind::Signature {
            self.refresh_compose_signatures(cx);
        }
        self.clear_rich_snippet_form(kind, window, cx);
    }

    pub(super) fn refresh_rich_snippet_i18n(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ui) = &self.settings_ui else {
            return;
        };
        for kind in [SnippetKind::Signature, SnippetKind::Template] {
            let state = ui.snippet(kind);
            state.name.update(cx, |input, cx| {
                input.set_placeholder(kind.name_placeholder(), window, cx);
            });
            state.editor.update(cx, |editor, cx| {
                editor.set_placeholder(kind.body_placeholder(), window, cx);
            });
        }
    }
}
