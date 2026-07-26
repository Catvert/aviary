//! Application icons.
//!
//! Icons are Lucide SVG files embedded in `assets/icons/` and resolved by
//! the `AssetSource` in `ui::mod`. `app_icon("mail")` loads
//! `icons/mail.svg`, the same files referenced by
//! composants gpui-component.

use gpui_component::Icon;

/// Icon by Lucide filename without an extension.
pub fn app_icon(name: &str) -> Icon {
    Icon::empty().path(format!("icons/{name}.svg"))
}
