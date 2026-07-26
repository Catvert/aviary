//! Application themes.
//!
//! Two built-in themes (light/dark) and a customizable palette with presets
//! are available.

use super::settings::{CustomThemePalette, GlobalSettings, ThemeMode};
use gpui::{px, App, Hsla, Rgba, Window};
use gpui_component::{Colorize, Theme, ThemeColor};

fn c(hex: u32) -> Hsla {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

fn interactive(color: u32, dark: bool, amount: f32) -> Hsla {
    if dark {
        c(color).lighten(amount)
    } else {
        c(color).darken(amount)
    }
}

fn foreground_on(color: u32) -> Hsla {
    let r = ((color >> 16) & 0xff) as f32 / 255.0;
    let g = ((color >> 8) & 0xff) as f32 / 255.0;
    let b = (color & 0xff) as f32 / 255.0;
    if 0.2126 * r + 0.7152 * g + 0.0722 * b > 0.58 {
        c(0x17191d)
    } else {
        c(0xffffff)
    }
}

fn apply_custom_palette(t: &mut ThemeColor, p: CustomThemePalette, dark: bool) {
    let on_primary = foreground_on(p.primary);
    let on_success = foreground_on(p.success);
    let on_warning = foreground_on(p.warning);
    let on_danger = foreground_on(p.danger);
    let hover_surface = interactive(p.surface_variant, dark, 0.05);
    let active_surface = interactive(p.surface_variant, dark, 0.1);

    t.background = c(p.background);
    t.foreground = c(p.foreground);
    t.border = c(p.border);
    t.input = c(p.border);
    t.ring = c(p.primary);
    t.caret = c(p.foreground);
    t.selection = Hsla {
        a: 0.32,
        ..c(p.primary)
    };

    t.primary = c(p.primary);
    t.primary_foreground = on_primary;
    t.primary_hover = interactive(p.primary, dark, 0.07);
    t.primary_active = interactive(p.primary, dark, 0.13);
    t.secondary = c(p.surface_variant);
    t.secondary_foreground = c(p.foreground);
    t.secondary_hover = hover_surface;
    t.secondary_active = active_surface;

    t.danger = c(p.danger);
    t.danger_foreground = on_danger;
    t.danger_hover = interactive(p.danger, dark, 0.07);
    t.danger_active = interactive(p.danger, dark, 0.13);
    t.warning = c(p.warning);
    t.warning_foreground = on_warning;
    t.warning_hover = interactive(p.warning, dark, 0.07);
    t.warning_active = interactive(p.warning, dark, 0.13);
    t.success = c(p.success);
    t.success_foreground = on_success;
    t.success_hover = interactive(p.success, dark, 0.07);
    t.success_active = interactive(p.success, dark, 0.13);
    t.info = c(p.primary);
    t.info_foreground = on_primary;
    t.info_hover = interactive(p.primary, dark, 0.07);
    t.info_active = interactive(p.primary, dark, 0.13);

    t.muted = c(p.surface_variant);
    t.muted_foreground = c(p.muted_foreground);
    t.accent = hover_surface;
    t.accent_foreground = c(p.foreground);
    t.popover = c(p.surface);
    t.popover_foreground = c(p.foreground);
    t.group_box = c(p.surface);
    t.group_box_foreground = c(p.foreground);
    t.accordion = c(p.surface);
    t.accordion_hover = hover_surface;

    t.list = c(p.surface);
    t.list_active = active_surface;
    t.list_active_border = c(p.primary);
    t.list_even = c(p.background);
    t.list_hover = hover_surface;
    t.list_head = c(p.surface);

    t.sidebar = c(p.surface);
    t.sidebar_foreground = c(p.foreground);
    t.sidebar_accent = hover_surface;
    t.sidebar_accent_foreground = c(p.foreground);
    t.sidebar_border = c(p.border);
    t.sidebar_primary = c(p.primary);
    t.sidebar_primary_foreground = on_primary;

    t.tab_bar = c(p.surface);
    t.tab_bar_segmented = c(p.surface_variant);
    t.tab = gpui::transparent_black();
    t.tab_active = c(p.background);
    t.tab_foreground = c(p.muted_foreground);
    t.tab_active_foreground = c(p.foreground);

    t.title_bar = c(p.surface);
    t.title_bar_border = c(p.border);
    t.window_border = c(p.border);

    t.table = c(p.surface);
    t.table_active = active_surface;
    t.table_active_border = c(p.primary);
    t.table_hover = hover_surface;
    t.table_head = c(p.surface);
    t.table_head_foreground = c(p.muted_foreground);
    t.table_even = c(p.background);
    t.table_row_border = c(p.border);

    t.scrollbar = gpui::transparent_black();
    t.scrollbar_thumb = c(p.surface_variant);
    t.scrollbar_thumb_hover = hover_surface;
    t.skeleton = c(p.surface_variant);
    t.switch = c(p.surface_variant);
    t.switch_thumb = c(p.foreground);
    t.slider_bar = c(p.surface_variant);
    t.slider_thumb = c(p.primary);

    t.link = c(p.primary);
    t.link_hover = interactive(p.primary, dark, 0.07);
    t.link_active = interactive(p.primary, dark, 0.13);
    t.drag_border = c(p.primary);
    t.drop_target = Hsla {
        a: 0.28,
        ..c(p.primary)
    };
    t.progress_bar = c(p.primary);
    t.tiles = c(p.surface);

    t.red = c(p.danger);
    t.red_light = interactive(p.danger, dark, 0.12);
    t.green = c(p.success);
    t.green_light = interactive(p.success, dark, 0.12);
    t.blue = c(p.primary);
    t.blue_light = interactive(p.primary, dark, 0.12);
    t.yellow = c(p.warning);
    t.yellow_light = interactive(p.warning, dark, 0.12);
}

/// Current interface scale, published by `apply` and read by each window while
/// rendering (secondary windows do not have access to settings).
struct UiScale(f32);

impl gpui::Global for UiScale {}

/// Base rem size in gpui; all `text_*` sizes in
/// derive from it, making `set_rem_size` the global interface zoom.
const BASE_REM: f32 = 16.0;

/// Applies the interface scale to a window. Call at the beginning of each
/// window's `render` (the assignment is trivial, with no relayout
/// while the value remains unchanged).
pub fn apply_window_scale(window: &mut Window, cx: &App) {
    let scale = cx
        .try_global::<UiScale>()
        .map(|s| s.0)
        .unwrap_or(1.0)
        .clamp(0.5, 2.0);
    window.set_rem_size(px(BASE_REM * scale));
}

/// Applies the theme selected in settings. Call at startup and whenever the
/// mode, variant, or scale changes.
pub fn apply(global: &GlobalSettings, window: Option<&mut Window>, cx: &mut App) {
    cx.set_global(UiScale(global.ui_scale));
    let mode = match global.theme_mode {
        ThemeMode::Dark => gpui_component::ThemeMode::Dark,
        ThemeMode::Light => gpui_component::ThemeMode::Light,
    };
    Theme::change(mode, window, cx);

    let theme = Theme::global_mut(cx);
    theme.font_family = "Inter".into();
    theme.mono_font_family = "JetBrains Mono".into();
    theme.font_size = px(14.);

    if global.uses_custom_theme() {
        apply_custom_palette(
            &mut theme.colors,
            global.custom_theme_palette,
            global.theme_mode == ThemeMode::Dark,
        );
    }

    cx.refresh_windows();
}
