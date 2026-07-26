//! AI tab: active provider and API-specific settings.

use super::super::app::AviaryApp;
use super::labelled;
use crate::ai::AiProvider;
use gpui::{div, prelude::*, Context, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::Input,
    menu::{DropdownMenu, PopupMenuItem},
    v_flex, ActiveTheme, Sizable,
};

fn provider_label(provider: AiProvider) -> gpui::SharedString {
    match provider {
        AiProvider::OpenAi => tr!("ai-provider-openai"),
        AiProvider::Anthropic => tr!("ai-provider-anthropic"),
        AiProvider::Gemini => tr!("ai-provider-gemini"),
        AiProvider::Local => tr!("ai-provider-local"),
    }
}

impl AviaryApp {
    pub(super) fn render_settings_ai(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let ui = self.settings_ui.as_ref().expect("settings_ui");
        let provider_button = self.render_ai_provider_button(cx);

        let fields = self.render_ai_provider_fields(cx);

        let prompt_list = self.render_ai_prompt_list(cx);

        v_flex()
            .child(
                self.section(&tr!("settings-ai-heading"), cx)
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(tr!("settings-ai-description")),
                    )
                    .child(provider_button)
                    .child(fields)
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(tr!("settings-ai-system-prompt")),
                            )
                            .child(Input::new(&ui.ai_system_prompt)),
                    )
                    .child(labelled(
                        &tr!("settings-ai-reader-translation-target"),
                        &ui.ai_reader_translation_target,
                        cx,
                    ))
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(tr!("settings-ai-reader-translation-prompt")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(tr!("settings-ai-reader-translation-description")),
                            )
                            .child(Input::new(&ui.ai_reader_translation_prompt)),
                    ),
            )
            .child(self.render_ai_save_button(cx))
            .child(
                self.section(&tr!("settings-ai-prompts-heading"), cx)
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(tr!("settings-ai-prompts-description")),
                    )
                    .child(prompt_list),
            )
            .child(self.render_ai_prompt_editor(cx))
    }

    fn commit_ai_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ui) = &self.settings_ui else {
            return;
        };
        let name = ui.ai_prompt_name.read(cx).value().trim().to_string();
        let prompt = ui.ai_prompt_body.read(cx).value().trim().to_string();
        if name.is_empty() || prompt.is_empty() {
            self.notify_error(tr!("settings-ai-prompt-required"), window, cx);
            return;
        }
        match ui.editing_ai_prompt {
            Some(id) => {
                if let Some(preset) = self
                    .settings
                    .global
                    .ai
                    .prompts
                    .iter_mut()
                    .find(|preset| preset.id == id)
                {
                    preset.name = name;
                    preset.prompt = prompt;
                }
            }
            None => {
                self.settings.global.ai.prompt_seq += 1;
                self.settings
                    .global
                    .ai
                    .prompts
                    .push(crate::ai::AiPromptPreset {
                        id: self.settings.global.ai.prompt_seq,
                        name,
                        prompt,
                    });
            }
        }
        self.settings.save();
        self.refresh_compose_ai_settings(cx);
        self.clear_ai_prompt_editor(window, cx);
        cx.notify();
    }

    fn clear_ai_prompt_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ui) = &mut self.settings_ui {
            ui.editing_ai_prompt = None;
            ui.ai_prompt_name
                .update(cx, |state, cx| state.set_value("", window, cx));
            ui.ai_prompt_body
                .update(cx, |state, cx| state.set_value("", window, cx));
        }
    }

    /// Provider picker. A switch applies immediately, so composers already
    /// open pick it up on their next stream.
    fn render_ai_provider_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let provider = self.settings.global.ai.provider;
        let entity = cx.entity();
        Button::new("ai-provider")
            .outline()
            .label(provider_label(provider))
            .dropdown_menu(move |mut menu, _, _| {
                for candidate in [
                    AiProvider::OpenAi,
                    AiProvider::Anthropic,
                    AiProvider::Gemini,
                    AiProvider::Local,
                ] {
                    let entity = entity.clone();
                    menu = menu.item(PopupMenuItem::new(provider_label(candidate)).on_click(
                        move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.settings.global.ai.provider = candidate;
                                this.settings.save();
                                this.refresh_compose_ai_settings(cx);
                                cx.notify();
                            });
                        },
                    ));
                }
                menu
            })
    }

    /// Credentials and model of the selected provider only.
    fn render_ai_provider_fields(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = self.settings_ui.as_ref().expect("settings_ui");
        match self.settings.global.ai.provider {
            AiProvider::OpenAi => v_flex()
                .gap_3()
                .child(labelled(
                    &tr!("settings-ai-api-key"),
                    &ui.ai_openai_api_key,
                    cx,
                ))
                .child(labelled(&tr!("settings-ai-model"), &ui.ai_openai_model, cx)),
            AiProvider::Anthropic => v_flex()
                .gap_3()
                .child(labelled(
                    &tr!("settings-ai-api-key"),
                    &ui.ai_anthropic_api_key,
                    cx,
                ))
                .child(labelled(
                    &tr!("settings-ai-model"),
                    &ui.ai_anthropic_model,
                    cx,
                )),
            AiProvider::Gemini => v_flex()
                .gap_3()
                .child(labelled(
                    &tr!("settings-ai-api-key"),
                    &ui.ai_gemini_api_key,
                    cx,
                ))
                .child(labelled(&tr!("settings-ai-model"), &ui.ai_gemini_model, cx)),
            AiProvider::Local => v_flex()
                .gap_3()
                .child(labelled(
                    &tr!("settings-ai-local-url"),
                    &ui.ai_local_base_url,
                    cx,
                ))
                .child(labelled(
                    &tr!("settings-ai-api-key-optional"),
                    &ui.ai_local_api_key,
                    cx,
                ))
                .child(labelled(&tr!("settings-ai-model"), &ui.ai_local_model, cx)),
        }
    }

    /// Saved prompt presets, each with its edit and delete entry.
    fn render_ai_prompt_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut prompt_list = v_flex().gap_2();
        for preset in self.settings.global.ai.prompts.clone() {
            let id = preset.id;
            prompt_list = prompt_list.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .p_2()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().child(preset.name))
                    .child(
                        Button::new(gpui::ElementId::Name(format!("edit-ai-prompt-{id}").into()))
                            .ghost()
                            .xsmall()
                            .label(tr!("templates-edit"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let preset = this
                                    .settings
                                    .global
                                    .ai
                                    .prompts
                                    .iter()
                                    .find(|preset| preset.id == id)
                                    .cloned();
                                if let (Some(preset), Some(ui)) = (preset, &mut this.settings_ui) {
                                    ui.editing_ai_prompt = Some(id);
                                    ui.ai_prompt_name.update(cx, |state, cx| {
                                        state.set_value(preset.name, window, cx)
                                    });
                                    ui.ai_prompt_body.update(cx, |state, cx| {
                                        state.set_value(preset.prompt, window, cx)
                                    });
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(gpui::ElementId::Name(
                            format!("delete-ai-prompt-{id}").into(),
                        ))
                        .danger()
                        .xsmall()
                        .label(tr!("delete"))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.settings
                                    .global
                                    .ai
                                    .prompts
                                    .retain(|preset| preset.id != id);
                                if this
                                    .settings_ui
                                    .as_ref()
                                    .is_some_and(|ui| ui.editing_ai_prompt == Some(id))
                                {
                                    this.clear_ai_prompt_editor(window, cx);
                                }
                                this.settings.save();
                                this.refresh_compose_ai_settings(cx);
                                cx.notify();
                            },
                        )),
                    ),
            );
        }
        if self.settings.global.ai.prompts.is_empty() {
            prompt_list = prompt_list.child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr!("settings-ai-prompts-empty")),
            );
        }
        prompt_list
    }

    /// Writes every provider's credentials at once: the fields of the
    /// providers that are not selected keep their values, so switching back
    /// does not require typing them again.
    fn render_ai_save_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("save-ai-config")
            .primary()
            .label(tr!("settings-save-configuration"))
            .on_click(cx.listener(|this, _, window, cx| {
                if let Some(ui) = &this.settings_ui {
                    let reader_translation_target = ui
                        .ai_reader_translation_target
                        .read(cx)
                        .value()
                        .trim()
                        .to_string();
                    let ai = &mut this.settings.global.ai;
                    ai.openai_api_key = ui.ai_openai_api_key.read(cx).value().trim().to_string();
                    ai.openai_model = ui.ai_openai_model.read(cx).value().trim().to_string();
                    ai.anthropic_api_key =
                        ui.ai_anthropic_api_key.read(cx).value().trim().to_string();
                    ai.anthropic_model = ui.ai_anthropic_model.read(cx).value().trim().to_string();
                    ai.gemini_api_key = ui.ai_gemini_api_key.read(cx).value().trim().to_string();
                    ai.gemini_model = ui.ai_gemini_model.read(cx).value().trim().to_string();
                    ai.local_base_url = ui.ai_local_base_url.read(cx).value().trim().to_string();
                    ai.local_api_key = ui.ai_local_api_key.read(cx).value().trim().to_string();
                    ai.local_model = ui.ai_local_model.read(cx).value().trim().to_string();
                    ai.system_prompt = ui.ai_system_prompt.read(cx).value().trim().to_string();
                    ai.reader_translation_prompt = ui
                        .ai_reader_translation_prompt
                        .read(cx)
                        .value()
                        .trim()
                        .to_string();
                    ai.reader_translation_target = reader_translation_target.clone();
                    this.settings.save();
                    this.refresh_compose_ai_settings(cx);
                    this.viewer_translation.target.update(cx, |state, cx| {
                        state.set_value(reader_translation_target, window, cx);
                    });
                }
                cx.notify();
            }))
    }

    /// The prompt editor, on the preset being edited or on a new one.
    fn render_ai_prompt_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = self.settings_ui.as_ref().expect("settings_ui");
        let editing_prompt = ui.editing_ai_prompt;
        self.section(
            &if editing_prompt.is_some() {
                tr!("settings-ai-prompt-edit-title")
            } else {
                tr!("settings-ai-prompt-new-title")
            },
            cx,
        )
        .child(labelled(
            &tr!("settings-ai-prompt-name"),
            &ui.ai_prompt_name,
            cx,
        ))
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(tr!("settings-ai-prompt-template")),
                )
                .child(Input::new(&ui.ai_prompt_body).min_h(gpui::px(144.))),
        )
        .child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("save-ai-prompt")
                        .primary()
                        .label(tr!("settings-ai-prompt-save"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.commit_ai_prompt(window, cx);
                        })),
                )
                .when(editing_prompt.is_some(), |el| {
                    el.child(
                        Button::new("cancel-ai-prompt")
                            .ghost()
                            .label(tr!("cancel"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.clear_ai_prompt_editor(window, cx);
                                cx.notify();
                            })),
                    )
                }),
        )
    }
}
