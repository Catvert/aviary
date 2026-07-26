//! Interface graphique gpui.
//!
//! The UI communicates with the runtime through `Cmd`/`Evt` channels and uses gpui
//! (Zed's framework) with gpui-component. The main window hosts `AviaryApp`;
//! detached composers and messages use actual OS windows.

mod account_selector;
mod addresses;
mod app;
mod attachments;
mod auth_view;
mod blitz_body;
mod block_editor;
mod calendar_view;
mod components;
mod compose;
mod composer_core;
mod contacts_view;
mod datefmt;
mod event_compose;
mod events;
mod icons;
mod image_lightbox;
mod inbox;
mod inline_images;
mod kanban_view;
mod logs_view;
mod memory;
mod motion;
mod quick_actions;
mod rich_clipboard;
mod session_store;
mod settings;
mod settings_view;
mod shortcuts;
mod snooze;
mod spellcheck;
mod state;
mod tag_menu;
mod theme;
// `crate::mailto` reuses the address parsing here rather than growing a second,
// subtly different one.
pub(crate) mod util;
mod viewer;

use crate::single_instance::ExternalRequest;
use gpui::{px, size, App, AppContext, Application, Bounds, WindowBounds, WindowOptions};
use gpui_component::Root;
use rust_embed::RustEmbed;

pub use settings::Settings;

#[cfg(test)]
const I18N_EN: &str = include_str!("../../assets/i18n/en.json");
#[cfg(test)]
const I18N_FR: &str = include_str!("../../assets/i18n/fr.json");

/// Fonts shared by the gpui text engine and Blitz renderer.
/// Registering them in gpui does not automatically expose them to
/// Parley/Fontique, which maintains its own collection (see `blitz_body`).
const INTER_FONT: &[u8] = include_bytes!("../../assets/fonts/Inter.ttf");
const INTER_BOLD_FONT: &[u8] = include_bytes!("../../assets/fonts/Inter-Bold.ttf");
const JETBRAINS_MONO_FONT: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono.ttf");
/// Color bitmap font used by gpui. On Linux gpui recognizes its
/// `NotoColorEmoji` PostScript name and switches to the RGBA glyph pipeline.
const NOTO_COLOR_EMOJI_FONT: &[u8] = include_bytes!("../../assets/fonts/NotoColorEmoji.ttf");

/// Assets embedded in the binary and served to gpui through `AssetSource`.
/// gpui-component resolves its icons under `icons/*.svg`.
#[derive(RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*.svg"]
#[include = "fonts/**/*.ttf"]
struct Assets;

impl gpui::AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        // Message inline `cid:` images, registered when displayed
        // (voir `inline_images`).
        if let Some(bytes) = inline_images::load(path) {
            return Ok(Some(std::borrow::Cow::Owned(bytes)));
        }
        Ok(Self::get(path).map(|f| f.data))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<gpui::SharedString>> {
        Ok(Self::iter()
            .filter(|p| p.starts_with(path))
            .map(|p| p.to_string().into())
            .collect())
    }
}

fn install_i18n(language: settings::LanguageChoice) {
    set_i18n_language(language);
}

pub(super) fn set_i18n_language(language: settings::LanguageChoice) {
    let locale = language.to_lang_id();
    // Each `rust-i18n` catalog owns its locale state. Keep Aviary and the
    // component library in sync so built-in menus follow the live preference.
    rust_i18n::set_locale(locale);
    gpui_component::set_locale(locale);
}

fn install_fonts(cx: &mut App) {
    let fonts = [
        INTER_FONT,
        INTER_BOLD_FONT,
        JETBRAINS_MONO_FONT,
        NOTO_COLOR_EMOJI_FONT,
    ];
    if let Err(e) = cx
        .text_system()
        .add_fonts(fonts.into_iter().map(std::borrow::Cow::Borrowed).collect())
    {
        log::warn!("failed to load embedded fonts: {e:#}");
    }
}

/// `external_requests` carries what other invocations of the binary asked for —
/// a `mailto:` URL clicked on the desktop, or a plain "come to the front". It
/// already holds this process's own launch request (see `single_instance`), so
/// the first one is served exactly like the ones that arrive later.
pub fn run(external_requests: tokio::sync::mpsc::UnboundedReceiver<ExternalRequest>) {
    let mut settings = Settings::load();
    install_i18n(settings.global.language);
    settings.global.ai.ensure_prompt_defaults();
    settings.save();

    Application::new().with_assets(Assets).run(move |cx| {
        // Intercept inline `aviary-cid/...` images (see inline_images).
        let default_client = cx.http_client();
        cx.set_http_client(std::sync::Arc::new(inline_images::CidHttpClient::new(
            default_client,
        )));
        gpui_component::init(cx);
        components::block_input::init(cx);
        addresses::init(cx);
        block_editor::init(cx);
        spellcheck::warm_up();
        shortcuts::init(cx);
        install_fonts(cx);
        theme::apply(&settings.global, None, cx);

        let bounds = Bounds::centered(None, size(px(1100.), px(700.)), cx);
        cx.activate(true);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui_component::TitleBar::title_bar_options()),
                app_id: Some("aviary".into()),
                ..Default::default()
            },
            |window, cx| {
                let main = cx
                    .new(|cx| app::AviaryApp::new(settings.clone(), external_requests, window, cx));
                let root = cx.new(|cx| Root::new(main.clone(), window, cx));
                let notification_source = root.read(cx).notification.clone();
                main.update(cx, |app, cx| {
                    app.install_notification_layer(notification_source, cx);
                });
                root
            },
        )
        .expect("failed to open the main window");
    });
}

#[cfg(test)]
mod i18n_tests {
    use super::{I18N_EN, I18N_FR};
    use std::collections::{BTreeMap, BTreeSet};

    fn catalog(source: &str) -> BTreeMap<String, String> {
        serde_json::from_str(source).expect("valid rust-i18n JSON catalog")
    }

    fn placeholders(value: &str) -> BTreeSet<&str> {
        value
            .split("%{")
            .skip(1)
            .filter_map(|tail| tail.split_once('}').map(|(name, _)| name.trim()))
            .filter(|name| !name.is_empty())
            .collect()
    }

    #[test]
    fn translation_catalogs_have_matching_keys_and_placeholders() {
        let en = catalog(I18N_EN);
        let fr = catalog(I18N_FR);
        let en_keys: BTreeSet<_> = en.keys().collect();
        let fr_keys: BTreeSet<_> = fr.keys().collect();
        assert_eq!(en_keys, fr_keys, "translation catalog keys differ");

        for key in en_keys {
            assert_eq!(
                placeholders(&en[key]),
                placeholders(&fr[key]),
                "placeholder mismatch for translation key {key}"
            );
        }
    }

    #[test]
    fn default_ai_prompts_keep_their_multiline_templates() {
        let fr = catalog(I18N_FR);
        let prompt = &fr["settings-ai-default-prompt-generate"];
        assert!(prompt.contains("\n\nInstructions :\n[[instruction]]"));
        assert!(prompt.contains("\n\nObjet (contexte uniquement) :\n[[subject]]"));
        assert!(prompt.ends_with("[[body]]"));

        let reader_prompt = &fr["settings-ai-default-reader-translation-prompt"];
        assert!(reader_prompt.contains("\n\nLangue cible :\n[[instruction]]"));
        assert!(reader_prompt.contains("\n\nObjet informatif :\n[[subject]]"));
        assert!(!reader_prompt.contains("[[body]]"));
        assert_eq!(
            fr["settings-ai-default-reader-translation-target"],
            "français"
        );
    }

    #[test]
    fn rust_i18n_embeds_and_interpolates_both_locales() {
        assert_eq!(
            rust_i18n::t!("status-connected", locale = "en"),
            "Connected"
        );
        assert_eq!(rust_i18n::t!("status-connected", locale = "fr"), "Connecté");
        assert_eq!(
            rust_i18n::t!("status-connected-as", locale = "fr", label = "Camille"),
            "Connecté · Camille"
        );
    }
}
