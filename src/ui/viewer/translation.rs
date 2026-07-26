//! Reading a message in another language.
//!
//! The translation is ephemeral: it streams in from the writing assistant and is
//! shown instead of the body, but neither the cached message nor the provider
//! copy is touched. Chunks arrive through the composer's AI events, keyed by the
//! same request id, which is why the reducers here are reached from
//! `ui::events::compose`.

use super::*;
use gpui::Entity;
use gpui_component::input::InputState;

/// The reader's translation panel.
pub struct ViewerTranslationState {
    /// Target language, edited by the user and kept across messages.
    pub target: Entity<InputState>,
    pub open: bool,
    /// Result for the displayed message, streamed in as it arrives.
    pub result: Option<ViewerTranslation>,
}

/// Ephemeral result of an AI translation in the reader. The cached message and
/// model remain unchanged.
pub struct ViewerTranslation {
    pub request_id: u64,
    pub message_id: String,
    pub html: String,
    pub running: bool,
    pub visible: bool,
}

pub(super) fn translated_body_element(
    message: &Message,
    translation: &ViewerTranslation,
    options: MailBodyOptions,
    fallback_width: Option<f32>,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    if translation.html.is_empty() {
        return div()
            .text_color(cx.theme().muted_foreground)
            .child(if translation.running {
                tr!("viewer-translation-working")
            } else {
                tr!("viewer-translation-empty")
            })
            .into_any_element();
    }

    if !translation.running {
        return crate::ui::blitz_body::preview_html_element(
            &format!("translation:{}", translation.request_id),
            &translation.html,
            &message.inline_images,
            options,
            fallback_width,
            window,
            cx,
        );
    }

    // HTML is incomplete while streaming. A progressive text preview avoids
    // restarting a Blitz thread for every token; the final document switches
    // to Blitz as soon as the response completes.
    let markdown = crate::providers::html::convert_email_html(&translation.html);
    let markdown = if options.show_remote_images {
        markdown
    } else {
        strip_remote_images(&markdown)
    };
    // Keep partial model-produced targets inert while streaming. The final
    // document switches to Blitz, whose navigation provider filters schemes.
    let source = markdown_to_source_preview(&markdown);
    let max_run = source
        .split(|character: char| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(max_run.max(2) + 1);
    let markdown = format!("{fence}text\n{source}\n{fence}");
    let font_size = px(options.font_size);
    let text_style = gpui_component::text::TextViewStyle {
        heading_base_font_size: font_size,
        ..Default::default()
    };
    TextView::markdown(
        gpui::ElementId::Name(format!("translated-body-{}", translation.request_id).into()),
        markdown,
        window,
        cx,
    )
    .style(text_style)
    .selectable(true)
    .into_any_element()
}

fn message_html_for_translation(message: &Message) -> String {
    let html = match message.format {
        BodyFormat::Markdown => message.raw_body.clone().unwrap_or_else(|| {
            let parser =
                pulldown_cmark::Parser::new_ext(&message.body, pulldown_cmark::Options::all());
            let mut html = String::new();
            pulldown_cmark::html::push_html(&mut html, parser);
            html
        }),
        BodyFormat::Text => format!(
            "<div style=\"white-space:pre-wrap\">{}</div>",
            util::escape_html_text(&message.body)
        ),
    };
    // Providers rewrite these references in Markdown intended for TextView.
    // Blitz and the original MIME use the cid: scheme.
    html.replace("bytes://cid-", "cid:")
}

impl AviaryApp {
    pub(super) fn render_viewer_translation_panel(
        &mut self,
        message: &Message,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.viewer_translation.open {
            return None;
        }
        let theme = cx.theme().clone();
        let target = self.viewer_translation.target.clone();
        let current = self
            .viewer_translation
            .result
            .as_ref()
            .filter(|translation| translation.message_id == message.header.id);
        let running = self
            .viewer_translation
            .result
            .as_ref()
            .is_some_and(|translation| translation.running);
        let has_translation = current.is_some_and(|translation| translation.visible);
        let message_to_translate = message.clone();

        Some(
            h_flex()
                .w_full()
                .min_w_0()
                .when_some(self.viewer_layout_width, |element, width| {
                    let width = px(width.floor());
                    element.w(width).min_w(width).max_w(width)
                })
                .gap_2()
                .flex_wrap()
                .items_center()
                .px_4()
                .py_2()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.muted)
                .child(Input::new(&target).w(px(220.)))
                .child(
                    Button::new("start-viewer-translation")
                        .primary()
                        .small()
                        .disabled(running)
                        .label(tr!("viewer-translation-start"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.start_viewer_translation(&message_to_translate, window, cx);
                        })),
                )
                .when(running, |element| {
                    element.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(tr!("viewer-translation-working")),
                    )
                })
                .when(has_translation, |element| {
                    element.child(
                        Button::new("show-original-message")
                            .ghost()
                            .small()
                            .disabled(running)
                            .label(tr!("viewer-translation-original"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(translation) = &mut this.viewer_translation.result {
                                    translation.visible = false;
                                }
                                cx.notify();
                            })),
                    )
                })
                .child(div().flex_1())
                .child(
                    Button::new("close-viewer-translation")
                        .ghost()
                        .xsmall()
                        .icon(crate::ui::icons::app_icon("x"))
                        .tooltip(tr!("viewer-translation-close"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.viewer_translation.open = false;
                            if let Some(translation) = &mut this.viewer_translation.result {
                                translation.visible = false;
                            }
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    fn start_viewer_translation(
        &mut self,
        message: &Message,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let instruction = self
            .viewer_translation
            .target
            .read(cx)
            .value()
            .trim()
            .to_string();
        if instruction.is_empty() {
            self.notify_error(tr!("viewer-translation-target-required"), window, cx);
            return;
        }
        let prompt_template = self
            .settings
            .global
            .ai
            .reader_translation_prompt
            .trim()
            .to_string();
        if prompt_template.is_empty() {
            self.notify_error(tr!("viewer-translation-prompt-required"), window, cx);
            return;
        }
        let system_prompt = prompt_template
            .replace("[[instruction_optional]]", instruction.trim())
            .replace("[[instruction]]", instruction.trim())
            .replace("[[subject]]", &message.header.subject)
            // Untrusted HTML stays in the user message; it
            // must never be injected into system instructions.
            .replace("[[body]]", "");

        let request_id = self.next_editor_id();
        let request = crate::runtime::AiEditRequest {
            compose_id: request_id,
            config: self.settings.global.ai.active_config(),
            // The dedicated prompt replaces the editor's Markdown prompt, and
            // the document to translate travels separately as a user message.
            system_prompt,
            prompt_template: "[[body]]".to_string(),
            instruction,
            subject: message.header.subject.clone(),
            body_markdown: message_html_for_translation(message),
        };
        self.viewer_translation.result = Some(ViewerTranslation {
            request_id,
            message_id: message.header.id.clone(),
            html: String::new(),
            running: true,
            visible: true,
        });
        self.send(Cmd::EditMailWithAi(request));
        cx.notify();
    }

    pub(crate) fn viewer_translation_chunk(&mut self, request_id: u64, delta: &str) -> bool {
        let Some(translation) = self
            .viewer_translation
            .result
            .as_mut()
            .filter(|translation| translation.request_id == request_id)
        else {
            return false;
        };
        translation.html.push_str(delta);
        true
    }

    pub(crate) fn viewer_translation_finished(&mut self, request_id: u64, html: String) -> bool {
        let Some(translation) = self
            .viewer_translation
            .result
            .as_mut()
            .filter(|translation| translation.request_id == request_id)
        else {
            return false;
        };
        translation.html = html;
        translation.running = false;
        true
    }

    pub(crate) fn viewer_translation_error(&mut self, request_id: u64) -> bool {
        let Some(translation) = self
            .viewer_translation
            .result
            .as_mut()
            .filter(|translation| translation.request_id == request_id)
        else {
            return false;
        };
        translation.running = false;
        true
    }
}
