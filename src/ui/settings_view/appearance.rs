//! Appearance tab: theme, language, body rendering, scale, and sizes.

use super::super::app::AviaryApp;
use super::super::settings::{
    BodyViewMode, LanguageChoice, ThemeColorRole, ThemeMode, ThemePreset,
};
use gpui::{div, prelude::*, px, Context, Window};
use gpui_component::{
    button::Button, color_picker::ColorPicker, h_flex, switch::Switch, v_flex, ActiveTheme,
    Sizable, StyledExt,
};

fn preset_label(preset: ThemePreset) -> gpui::SharedString {
    match preset {
        ThemePreset::Manual => tr!("settings-theme-preset-manual"),
        ThemePreset::OneDark => tr!("settings-theme-preset-one-dark"),
        ThemePreset::OneLight => tr!("settings-theme-preset-one-light"),
        ThemePreset::NordDark => tr!("settings-theme-preset-nord-dark"),
        ThemePreset::NordLight => tr!("settings-theme-preset-nord-light"),
        ThemePreset::Dracula => tr!("settings-theme-preset-dracula"),
        ThemePreset::DraculaAlucard => tr!("settings-theme-preset-dracula-alucard"),
        ThemePreset::GruvboxDark => tr!("settings-theme-preset-gruvbox-dark"),
        ThemePreset::GruvboxLight => tr!("settings-theme-preset-gruvbox-light"),
        ThemePreset::TokyoNight => tr!("settings-theme-preset-tokyo-night"),
        ThemePreset::TokyoDay => tr!("settings-theme-preset-tokyo-day"),
        ThemePreset::CatppuccinMocha => tr!("settings-theme-preset-catppuccin-mocha"),
        ThemePreset::CatppuccinLatte => tr!("settings-theme-preset-catppuccin-latte"),
        ThemePreset::KanagawaWave => tr!("settings-theme-preset-kanagawa-wave"),
        ThemePreset::KanagawaLotus => tr!("settings-theme-preset-kanagawa-lotus"),
        ThemePreset::RosePine => tr!("settings-theme-preset-rose-pine"),
        ThemePreset::RosePineDawn => tr!("settings-theme-preset-rose-pine-dawn"),
        ThemePreset::EverforestDark => tr!("settings-theme-preset-everforest-dark"),
        ThemePreset::EverforestLight => tr!("settings-theme-preset-everforest-light"),
        ThemePreset::SolarizedDark => tr!("settings-theme-preset-solarized-dark"),
        ThemePreset::SolarizedLight => tr!("settings-theme-preset-solarized-light"),
        ThemePreset::GithubDark => tr!("settings-theme-preset-github-dark"),
        ThemePreset::GithubLight => tr!("settings-theme-preset-github-light"),
    }
}

fn color_role_label(role: ThemeColorRole) -> gpui::SharedString {
    match role {
        ThemeColorRole::Background => tr!("settings-theme-color-background"),
        ThemeColorRole::Surface => tr!("settings-theme-color-surface"),
        ThemeColorRole::SurfaceVariant => tr!("settings-theme-color-surface-variant"),
        ThemeColorRole::Foreground => tr!("settings-theme-color-foreground"),
        ThemeColorRole::MutedForeground => tr!("settings-theme-color-muted-foreground"),
        ThemeColorRole::Border => tr!("settings-theme-color-border"),
        ThemeColorRole::Primary => tr!("settings-theme-color-primary"),
        ThemeColorRole::Success => tr!("settings-theme-color-success"),
        ThemeColorRole::Warning => tr!("settings-theme-color-warning"),
        ThemeColorRole::Danger => tr!("settings-theme-color-danger"),
    }
}

impl AviaryApp {
    pub(super) fn render_settings_appearance(
        &mut self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let g = self.settings.global.clone();

        v_flex()
            .child(self.render_appearance_theme_section(cx))
            .child(self.render_appearance_language_section(cx))
            .child(self.render_appearance_body_section(cx))
            .child(self.section(&tr!("settings-ui-scale"), cx).child({
                let mut row = h_flex().gap_2();
                for (label, scale) in [
                    ("90 %", 0.9f32),
                    ("100 %", 1.0),
                    ("110 %", 1.1),
                    ("125 %", 1.25),
                    ("150 %", 1.5),
                ] {
                    row = row.child(
                        super::choice_button(
                            Button::new(gpui::ElementId::Name(format!("scale-{label}").into()))
                                .small()
                                .label(label),
                            (g.ui_scale - scale).abs() < 0.01,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.settings.global.ui_scale = scale;
                                this.settings.save();
                                super::super::theme::apply(&this.settings.global, Some(window), cx);
                                cx.notify();
                            },
                        )),
                    );
                }
                row
            }))
            .child(self.section(&tr!("settings-body-font-size"), cx).child({
                let mut row = h_flex().gap_2();
                for size in [12.0f32, 13.0, 14.0, 16.0, 18.0] {
                    row = row.child(
                        super::choice_button(
                            Button::new(gpui::ElementId::Name(format!("bfs-{size}").into()))
                                .small()
                                .label(format!("{size:.0} px")),
                            (g.body_font_size - size).abs() < 0.1,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.settings.global.body_font_size = size;
                            this.settings.save();
                            cx.notify();
                        })),
                    );
                }
                row
            }))
    }

    fn language_button(
        &self,
        id: &'static str,
        label: String,
        value: LanguageChoice,
        current: LanguageChoice,
        cx: &mut Context<Self>,
    ) -> Button {
        super::choice_button(Button::new(id).small().label(label), current == value).on_click(
            cx.listener(move |this, _, window, cx| {
                this.change_language(value, window, cx);
            }),
        )
    }

    fn apply_theme_preset(
        &mut self,
        preset: ThemePreset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_theme_preset(preset, window, cx);
        self.settings.save();
        super::super::theme::apply(&self.settings.global, Some(window), cx);
        cx.notify();
    }

    fn select_theme_preset(
        &mut self,
        preset: ThemePreset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.global.select_custom_theme_preset(preset);
        self.sync_theme_color_editors(window, cx);
    }

    fn sync_theme_color_editors(&self, window: &mut Window, cx: &mut Context<Self>) {
        let palette = self.settings.global.custom_theme_palette;
        if let Some(ui) = &self.settings_ui {
            for editor in &ui.theme_colors {
                editor.picker.update(cx, |state, cx| {
                    state.set_value(
                        super::super::util::packed_color(palette.color(editor.role)),
                        window,
                        cx,
                    );
                });
            }
        }
    }

    pub(crate) fn select_custom_theme_mode(
        &mut self,
        mode: ThemeMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.global.select_custom_theme_mode(mode);
        self.sync_theme_color_editors(window, cx);
    }

    pub(crate) fn change_custom_theme_mode(
        &mut self,
        mode: ThemeMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_custom_theme_mode(mode, window, cx);
        self.settings.save();
        super::super::theme::apply(&self.settings.global, Some(window), cx);
        cx.notify();
    }

    fn change_language(
        &mut self,
        language: LanguageChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.global.language = language;
        super::super::set_i18n_language(language);
        self.settings.save();
        #[cfg(target_os = "linux")]
        if let Some(tray) = &self.tray {
            tray.refresh_i18n();
        }

        self.search_input.update(cx, |state, cx| {
            state.set_placeholder(tr!("search-hint"), window, cx);
        });
        self.contacts_search_input.update(cx, |state, cx| {
            state.set_placeholder(tr!("contacts-search-hint"), window, cx);
        });
        self.viewer_translation.target.update(cx, |state, cx| {
            state.set_placeholder(tr!("viewer-translation-target-placeholder"), window, cx);
        });
        if let Some(ui) = &self.settings_ui {
            for (input, placeholder) in [
                (
                    &ui.ai_prompt_name,
                    tr!("settings-ai-prompt-name-placeholder"),
                ),
                (
                    &ui.ai_prompt_body,
                    tr!("settings-ai-prompt-body-placeholder"),
                ),
            ] {
                input.update(cx, |state, cx| {
                    state.set_placeholder(placeholder.clone(), window, cx);
                });
            }
        }
        self.refresh_rich_snippet_i18n(window, cx);
        if let Some(form) = &self.imap_form {
            for (input, placeholder) in [
                (&form.email, tr!("login-imap-form-email-hint")),
                (
                    &form.display_name,
                    tr!("login-imap-display-name-placeholder"),
                ),
                (&form.imap_host, tr!("login-imap-form-imap-host-hint")),
                (&form.imap_username, tr!("login-imap-username-placeholder")),
                (&form.smtp_host, tr!("login-imap-form-smtp-host-hint")),
                (&form.smtp_username, tr!("login-smtp-username-placeholder")),
                (&form.password, tr!("login-imap-form-password-hint")),
            ] {
                input.update(cx, |state, cx| {
                    state.set_placeholder(placeholder.clone(), window, cx);
                });
            }
        }
        for compose in self
            .inline_composes
            .iter()
            .map(|compose| compose.view.clone())
            .chain(self.inline_reply.iter().map(|reply| reply.view.clone()))
            .collect::<Vec<_>>()
        {
            compose.update(cx, |compose, cx| compose.refresh_i18n(window, cx));
        }

        let compose_windows = self
            .composes
            .iter()
            .filter_map(|compose| compose.window.map(|window| (window, compose.view.clone())))
            .collect::<Vec<_>>();
        for (handle, view) in compose_windows {
            let _ = handle.update(cx, |_, window, cx| {
                let _ = view.update(cx, |view, cx| view.refresh_i18n(window, cx));
            });
        }

        if let Some(inline) = &self.calendar.inline_compose {
            inline
                .view
                .update(cx, |view, cx| view.refresh_i18n(window, cx));
        }

        let event_windows = self
            .calendar
            .composes
            .iter()
            .filter_map(|compose| compose.window.map(|window| (window, compose.view.clone())))
            .collect::<Vec<_>>();
        for (handle, view) in event_windows {
            let _ = handle.update(cx, |_, window, cx| {
                let _ = view.update(cx, |view, cx| view.refresh_i18n(window, cx));
            });
        }
        cx.refresh_windows();
        cx.notify();
    }

    /// Mode, preset palette, and the custom-palette editors. Each mode keeps
    /// its own variant, so switching never loses what the user tuned.
    fn render_appearance_theme_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let g = self.settings.global.clone();
        let custom_theme = g.uses_custom_theme();
        self.section(&tr!("settings-theme"), cx).child(
            v_flex()
                .gap_4()
                .child(
                    h_flex()
                        .gap_2()
                        .child(self.appearance_choice(
                            "th-light",
                            tr!("settings-theme-light").to_string(),
                            !custom_theme && g.theme_mode == ThemeMode::Light,
                            |a| {
                                a.settings
                                    .global
                                    .select_builtin_theme_mode(ThemeMode::Light);
                            },
                            cx,
                        ))
                        .child(self.appearance_choice(
                            "th-dark",
                            tr!("settings-theme-dark").to_string(),
                            !custom_theme && g.theme_mode == ThemeMode::Dark,
                            |a| {
                                a.settings.global.select_builtin_theme_mode(ThemeMode::Dark);
                            },
                            cx,
                        ))
                        .child(
                            super::choice_button(
                                Button::new("th-custom")
                                    .small()
                                    .label(tr!("settings-theme-custom")),
                                custom_theme,
                            )
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    let mode = this.settings.global.theme_mode;
                                    this.change_custom_theme_mode(mode, window, cx);
                                },
                            )),
                        ),
                )
                .when(custom_theme, |panel| {
                    panel.child(self.render_appearance_palette_editors(cx))
                }),
        )
    }

    /// UI language, applied to every open window as soon as it changes.
    fn render_appearance_language_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let g = self.settings.global.clone();
        self.section(&tr!("settings-language"), cx).child(
            h_flex()
                .gap_2()
                .child(self.language_button(
                    "lang-sys",
                    tr!("settings-language-system").to_string(),
                    LanguageChoice::System,
                    g.language,
                    cx,
                ))
                .child(self.language_button(
                    "lang-fr",
                    tr!("settings-language-french").to_string(),
                    LanguageChoice::French,
                    g.language,
                    cx,
                ))
                .child(self.language_button(
                    "lang-en",
                    tr!("settings-language-english").to_string(),
                    LanguageChoice::English,
                    g.language,
                    cx,
                )),
        )
    }

    /// How received bodies are rendered: view mode, remote images, uniform
    /// typography, quoted-history folding.
    fn render_appearance_body_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let g = self.settings.global.clone();
        self.section(&tr!("settings-message-preview"), cx)
            .child(
                h_flex()
                    .gap_2()
                    .child(self.appearance_choice(
                        "bv-blitz",
                        tr!("viewer-mode-faithful").to_string(),
                        g.body_view_mode == BodyViewMode::Blitz,
                        |a| a.settings.global.body_view_mode = BodyViewMode::Blitz,
                        cx,
                    ))
                    .child(self.appearance_choice(
                        "bv-md",
                        tr!("viewer-mode-markdown").to_string(),
                        g.body_view_mode == BodyViewMode::Markdown,
                        |a| a.settings.global.body_view_mode = BodyViewMode::Markdown,
                        cx,
                    )),
            )
            .child(
                Switch::new("uniform-email-font-family")
                    .checked(g.force_uniform_font_family)
                    .label(tr!("settings-uniform-font-family"))
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.settings.global.force_uniform_font_family = *checked;
                        this.settings.save();
                        cx.notify();
                    })),
            )
            .child(
                Switch::new("uniform-email-font-size")
                    .checked(g.force_uniform_font_size)
                    .label(tr!("settings-uniform-font-size"))
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.settings.global.force_uniform_font_size = *checked;
                        this.settings.save();
                        cx.notify();
                    })),
            )
            .child(
                Switch::new("reply-all-primary")
                    .checked(g.reply_all_primary)
                    .label(tr!("settings-reply-all-primary"))
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.settings.global.reply_all_primary = *checked;
                        this.settings.save();
                        cx.notify();
                    })),
            )
            .child(
                Switch::new("force-light-email-preview")
                    .checked(g.force_light_email_preview)
                    .label(tr!("settings-force-light-email-preview"))
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.settings.global.force_light_email_preview = *checked;
                        this.settings.save();
                        cx.notify();
                    })),
            )
            .child(
                Switch::new("start-maximized")
                    .checked(g.start_maximized)
                    .label(tr!("settings-start-maximized"))
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.settings.global.start_maximized = *checked;
                        this.settings.save();
                        cx.notify();
                    })),
            )
            .child(
                Switch::new("collapse-quoted-messages")
                    .checked(g.collapse_quoted_messages)
                    .label(tr!("settings-collapse-quoted-messages"))
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.settings.global.collapse_quoted_messages = *checked;
                        this.settings.save();
                        cx.notify();
                    })),
            )
    }

    /// One segmented-choice button of this tab: applies the change, persists
    /// it, and re-themes every open window.
    fn appearance_choice(
        &self,
        id: &'static str,
        label: String,
        selected: bool,
        apply: fn(&mut AviaryApp),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        super::choice_button(Button::new(id).small().label(label), selected).on_click(cx.listener(
            move |this, _, window, cx| {
                apply(this);
                this.settings.save();
                super::super::theme::apply(&this.settings.global, Some(window), cx);
                cx.notify();
            },
        ))
    }

    /// The custom-palette editors, shown only when a custom palette is in use.
    /// Light and dark keep separate variants, so switching mode never discards
    /// what was tuned for the other one.
    fn render_appearance_palette_editors(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let g = self.settings.global.clone();
        let theme_color_editors: Vec<_> = self
            .settings_ui
            .as_ref()
            .expect("settings UI initialized")
            .theme_colors
            .iter()
            .map(|editor| (editor.role, editor.picker.clone()))
            .collect();

        v_flex()
            .gap_4()
            .p_3()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary.opacity(0.35))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr!("settings-theme-custom-description")),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(tr!("settings-theme-custom-mode")),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                super::choice_button(
                                    Button::new("th-custom-light")
                                        .small()
                                        .label(tr!("settings-theme-light")),
                                    g.theme_mode == ThemeMode::Light,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.change_custom_theme_mode(ThemeMode::Light, window, cx);
                                    },
                                )),
                            )
                            .child(
                                super::choice_button(
                                    Button::new("th-custom-dark")
                                        .small()
                                        .label(tr!("settings-theme-dark")),
                                    g.theme_mode == ThemeMode::Dark,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.change_custom_theme_mode(ThemeMode::Dark, window, cx);
                                    },
                                )),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_sm().font_semibold().child(match g.theme_mode {
                                ThemeMode::Dark => {
                                    tr!("settings-theme-presets-dark")
                                }
                                ThemeMode::Light => {
                                    tr!("settings-theme-presets-light")
                                }
                            }))
                            .when(g.theme_preset == ThemePreset::Manual, |row| {
                                row.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(preset_label(ThemePreset::Manual)),
                                )
                            }),
                    )
                    .child({
                        let mut row = h_flex().gap_1().gap_y_1().flex_wrap();
                        for &preset in ThemePreset::for_mode(g.theme_mode) {
                            row = row.child(
                                super::choice_button(
                                    Button::new(gpui::ElementId::Name(
                                        format!("theme-preset-{preset:?}").into(),
                                    ))
                                    .xsmall()
                                    .label(preset_label(preset)),
                                    g.theme_preset == preset,
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.apply_theme_preset(preset, window, cx);
                                    },
                                )),
                            );
                        }
                        row
                    }),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(tr!("settings-theme-palette")),
                    )
                    .child(h_flex().gap_3().gap_y_2().flex_wrap().children(
                        theme_color_editors.into_iter().map(|(role, picker)| {
                            div().w(px(205.)).child(
                                ColorPicker::new(&picker)
                                    .small()
                                    .label(color_role_label(role)),
                            )
                        }),
                    )),
            )
    }
}
