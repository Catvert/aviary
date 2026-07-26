//! Optional LanguageTool configuration and managed-install controls.

use super::super::app::AviaryApp;
use super::labelled;
use crate::proofreading::{
    LanguageToolCoverage, LanguageToolLocalSource, LanguageToolMode, LanguageToolState,
};
use crate::runtime::Cmd;
use gpui::{div, prelude::*, Context, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    switch::Switch,
    v_flex, ActiveTheme, Disableable, Sizable, StyledExt,
};

fn status_label(state: LanguageToolState) -> gpui::SharedString {
    match state {
        LanguageToolState::Disabled => tr!("languagetool-status-disabled"),
        LanguageToolState::NotInstalled => tr!("languagetool-status-not-installed"),
        LanguageToolState::Stopped => tr!("languagetool-status-stopped"),
        LanguageToolState::Starting => tr!("languagetool-status-starting"),
        LanguageToolState::Ready => tr!("languagetool-status-ready"),
        LanguageToolState::Installing => tr!("languagetool-status-installing"),
        LanguageToolState::Error => tr!("languagetool-status-error"),
    }
}

fn is_remote_url(value: &str) -> bool {
    reqwest::Url::parse(value.trim())
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host != "localhost" && host != "127.0.0.1" && host != "::1")
}

impl AviaryApp {
    pub(super) fn render_settings_correction(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .gap_4()
            .child(self.render_languagetool_section(cx))
            .child(self.render_languagetool_status_section(cx))
    }

    fn save_languagetool_inputs(&mut self, cx: &mut Context<Self>) {
        self.store_languagetool_inputs(cx);
        self.configure_languagetool(cx);
    }

    fn store_languagetool_inputs(
        &mut self,
        cx: &mut Context<Self>,
    ) -> crate::proofreading::LanguageToolSettings {
        let Some(ui) = &self.settings_ui else {
            return self.settings.global.languagetool.clone();
        };
        self.settings.global.languagetool.java_path = ui
            .languagetool_java_path
            .read(cx)
            .value()
            .trim()
            .to_string();
        self.settings.global.languagetool.existing_directory = ui
            .languagetool_directory
            .read(cx)
            .value()
            .trim()
            .to_string();
        self.settings.global.languagetool.external_url =
            ui.languagetool_url.read(cx).value().trim().to_string();
        self.settings.save();
        self.settings.global.languagetool.clone()
    }

    fn pick_languagetool_path(&mut self, java: bool, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: java,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                if let Some(path) = paths.into_iter().next() {
                    let value = path.to_string_lossy().to_string();
                    let _ = this.update_in(cx, |this, window, cx| {
                        let Some(ui) = &this.settings_ui else { return };
                        let input = if java {
                            &ui.languagetool_java_path
                        } else {
                            &ui.languagetool_directory
                        };
                        input.update(cx, |state, cx| state.set_value(value, window, cx));
                    });
                }
            }
        })
        .detach();
    }

    /// LanguageTool: mode, distribution source, and coverage.
    fn render_languagetool_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.settings.global.languagetool.clone();
        let status = self.languagetool_status.clone();
        let ui = self.settings_ui.as_ref().expect("settings_ui");
        let url = ui.languagetool_url.clone();
        let mode_button =
            |id: &'static str, label: String, mode: LanguageToolMode, cx: &mut Context<Self>| {
                super::choice_button(Button::new(id).small().label(label), settings.mode == mode)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.global.languagetool.mode = mode;
                        this.settings.save();
                        this.configure_languagetool(cx);
                    }))
            };
        let coverage_button = |id: &'static str,
                               label: String,
                               coverage: LanguageToolCoverage,
                               cx: &mut Context<Self>| {
            super::choice_button(
                Button::new(id).small().label(label),
                settings.coverage == coverage,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.settings.global.languagetool.coverage = coverage;
                this.settings.save();
                this.configure_languagetool(cx);
            }))
        };

        let local_options = self.render_languagetool_local_options(cx);
        self.section(&tr!("languagetool-heading"), cx)
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr!("languagetool-description")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(mode_button(
                        "lt-disabled",
                        tr!("languagetool-mode-disabled").to_string(),
                        LanguageToolMode::Disabled,
                        cx,
                    ))
                    .child(mode_button(
                        "lt-local",
                        tr!("languagetool-mode-local").to_string(),
                        LanguageToolMode::LocalManaged,
                        cx,
                    ))
                    .child(mode_button(
                        "lt-external",
                        tr!("languagetool-mode-external").to_string(),
                        LanguageToolMode::ExternalUrl,
                        cx,
                    )),
            )
            .when(settings.mode == LanguageToolMode::LocalManaged, |section| {
                section.child(local_options)
            })
            .when(settings.mode == LanguageToolMode::ExternalUrl, |section| {
                section
                    .child(labelled(&tr!("languagetool-external-url"), &url, cx))
                    .when(is_remote_url(&settings.external_url), |section| {
                        section.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().warning)
                                .child(tr!("languagetool-privacy-warning")),
                        )
                    })
            })
            .when(settings.mode != LanguageToolMode::Disabled, |section| {
                section
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(coverage_button(
                                "lt-grammar",
                                tr!("languagetool-coverage-grammar").to_string(),
                                LanguageToolCoverage::GrammarOnly,
                                cx,
                            ))
                            .child(coverage_button(
                                "lt-spelling-grammar",
                                tr!("languagetool-coverage-all").to_string(),
                                LanguageToolCoverage::SpellingAndGrammar,
                                cx,
                            )),
                    )
                    .child(
                        Switch::new("lt-automatic")
                            .checked(settings.automatic_check)
                            .label(tr!("languagetool-automatic"))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.settings.global.languagetool.automatic_check = *checked;
                                this.settings.save();
                                this.configure_languagetool(cx);
                            })),
                    )
            })
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new("lt-apply")
                            .primary()
                            .label(tr!("settings-apply"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.save_languagetool_inputs(cx);
                            })),
                    )
                    .child(
                        Button::new("lt-test")
                            .label(tr!("languagetool-test"))
                            .disabled(settings.mode == LanguageToolMode::Disabled)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let settings = this.store_languagetool_inputs(cx);
                                this.apply_languagetool_editor_settings(settings.clone(), cx);
                                this.send(Cmd::TestLanguageTool(settings));
                            })),
                    )
                    .when(
                        settings.mode == LanguageToolMode::LocalManaged
                            && settings.local_source == LanguageToolLocalSource::Downloaded,
                        |buttons| {
                            buttons
                                .child(
                                    Button::new("lt-install")
                                        .label(tr!("languagetool-install"))
                                        .disabled(status.state == LanguageToolState::Installing)
                                        .on_click(cx.listener(|this, _, _, _| {
                                            this.send(Cmd::InstallLanguageTool);
                                        })),
                                )
                                .child(
                                    Button::new("lt-uninstall")
                                        .danger()
                                        .label(tr!("languagetool-uninstall"))
                                        .on_click(cx.listener(|this, _, _, _| {
                                            this.send(Cmd::UninstallLanguageTool);
                                        })),
                                )
                        },
                    ),
            )
    }

    /// Install state of the managed distribution and its controls.
    fn render_languagetool_status_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.languagetool_status.clone();
        let mut status_content = v_flex().gap_2().child(
            h_flex()
                .gap_2()
                .child(div().font_semibold().child(status_label(status.state)))
                .when_some(status.version.clone(), |row, version| {
                    row.child(
                        div()
                            .text_sm()
                            .child(tr!("languagetool-version", { version: version })),
                    )
                }),
        );
        if let Some(progress) = status.progress {
            status_content =
                status_content.child(div().text_sm().child(tr!("languagetool-progress", {
                    percent: format!("{:.0}", progress * 100.0)
                })));
        }
        if let Some(detail) = status.detail {
            status_content = status_content.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(tr!("languagetool-error-detail", { error: detail })),
            );
        }

        self.section(&tr!("languagetool-status-heading"), cx)
            .child(status_content)
    }

    /// Managed-install options: where the distribution comes from, and the
    /// Java runtime and directory it needs.
    fn render_languagetool_local_options(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.settings.global.languagetool.clone();
        let ui = self.settings_ui.as_ref().expect("settings_ui");
        let java_path = ui.languagetool_java_path.clone();
        let directory = ui.languagetool_directory.clone();
        let source_button = |id: &'static str,
                             label: String,
                             source: LanguageToolLocalSource,
                             cx: &mut Context<Self>| {
            super::choice_button(
                Button::new(id).small().label(label),
                settings.local_source == source,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.settings.global.languagetool.local_source = source;
                this.settings.save();
                this.configure_languagetool(cx);
            }))
        };
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(source_button(
                        "lt-source-downloaded",
                        tr!("languagetool-source-downloaded").to_string(),
                        LanguageToolLocalSource::Downloaded,
                        cx,
                    ))
                    .child(source_button(
                        "lt-source-existing",
                        tr!("languagetool-source-existing").to_string(),
                        LanguageToolLocalSource::ExistingDirectory,
                        cx,
                    )),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(labelled(&tr!("languagetool-java-path"), &java_path, cx))
                    .child(
                        Button::new("lt-pick-java")
                            .small()
                            .label(tr!("languagetool-choose-java"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.pick_languagetool_path(true, window, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(tr!("languagetool-java-help")),
                    ),
            )
            .when(
                settings.local_source == LanguageToolLocalSource::ExistingDirectory,
                |content| {
                    content.child(
                        v_flex()
                            .gap_2()
                            .child(labelled(
                                &tr!("languagetool-existing-directory"),
                                &directory,
                                cx,
                            ))
                            .child(
                                Button::new("lt-pick-directory")
                                    .small()
                                    .label(tr!("languagetool-choose-directory"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.pick_languagetool_path(false, window, cx);
                                    })),
                            ),
                    )
                },
            )
    }
}
