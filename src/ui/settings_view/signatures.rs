//! Signatures tab rendered by the shared rich CRUD component.

use super::super::app::AviaryApp;
use super::rich_snippets::SnippetKind;
use gpui::{prelude::*, Context, Window};

impl AviaryApp {
    pub(super) fn render_settings_signatures(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.render_rich_snippets(SnippetKind::Signature, window, cx)
    }
}
