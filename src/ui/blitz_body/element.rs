//! The gpui element in front of a rendered document.
//!
//! The entry points ([`element`], [`html_element`], the preview variants) each
//! describe *what* to render; they all end in [`render_element`], which draws
//! the cached tiles, measures the width available with a probe, forwards mouse
//! and keyboard events to [`super::events`], and arms the band and resource
//! pumps of [`super::bands`].
//!
//! Link activation and zoom live here too, because both belong to the element
//! the user is pointing at rather than to the document itself.

use super::bands::measure_and_render;
use super::events::{copy_selection, push_op, push_pointer};
use super::*;

pub(crate) fn element(
    m: &Message,
    options: MailBodyOptions,
    fallback_width: Option<f32>,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    message_element(m, options, fallback_width, false, window, cx)
}

/// Renders one part extracted from a message whose quoted history is displayed
/// separately. Unlike a standalone reader document, the part must stop at its
/// content height so the following conversation card is not pushed to the
/// bottom of Blitz's synthetic browser viewport.
pub(crate) fn fragment_element(
    m: &Message,
    options: MailBodyOptions,
    fallback_width: Option<f32>,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    message_element(m, options, fallback_width, true, window, cx)
}

pub(super) fn message_element(
    m: &Message,
    options: MailBodyOptions,
    fallback_width: Option<f32>,
    fragment: bool,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    let (source, kind): (&str, PrepSource) = match m.format {
        BodyFormat::Markdown => (
            m.raw_body.as_deref().unwrap_or(&m.body),
            if fragment {
                PrepSource::HtmlFragment
            } else {
                PrepSource::Html
            },
        ),
        BodyFormat::Text => (
            &m.body,
            if fragment {
                PrepSource::TextFragment
            } else {
                PrepSource::Text
            },
        ),
    };
    let mail_theme = MailTheme::from_app(cx.theme(), options.force_light_theme);
    let (key, job) = prepared_job(
        &format!("message:{}", m.header.id),
        source,
        kind,
        &m.inline_images,
        options,
        mail_theme,
        cx,
    );
    dispatch_pending_mailto(&key, window, cx);
    render_element(
        key,
        job,
        fallback_width,
        true,
        true,
        Some(zoom_badge_anchor(window)),
        cx,
    )
}

/// Renders a faithful HTML fragment in a location separate from the reader.
///
/// `instance` is part of the cache key: displaying the same message in the
/// reader and an editor quote therefore creates two independent Blitz documents,
/// each with its own origin, focus, and width.
pub(crate) fn html_element(
    instance: &str,
    html: &str,
    images: &[InlineImage],
    options: MailBodyOptions,
    fallback_width: Option<f32>,
    cx: &mut App,
) -> gpui::AnyElement {
    let mail_theme = MailTheme::from_app(cx.theme(), options.force_light_theme);
    let (key, job) = prepared_job(
        instance,
        html,
        PrepSource::HtmlFragment,
        images,
        options,
        mail_theme,
        cx,
    );
    render_element(key, job, fallback_width, false, false, None, cx)
}

/// Zoomable variant intended for a full HTML preview (composer or current
/// message translation).
pub(crate) fn preview_html_element(
    instance: &str,
    html: &str,
    images: &[InlineImage],
    options: MailBodyOptions,
    fallback_width: Option<f32>,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    preview_html_element_with_height(
        instance,
        html,
        images,
        options,
        fallback_width,
        false,
        window,
        cx,
    )
}

/// Zoomable preview for a quoted/current sub-message embedded in the reader.
pub(crate) fn preview_html_fragment_element(
    instance: &str,
    html: &str,
    images: &[InlineImage],
    options: MailBodyOptions,
    fallback_width: Option<f32>,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    preview_html_element_with_height(
        instance,
        html,
        images,
        options,
        fallback_width,
        true,
        window,
        cx,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn preview_html_element_with_height(
    instance: &str,
    html: &str,
    images: &[InlineImage],
    options: MailBodyOptions,
    fallback_width: Option<f32>,
    fragment: bool,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    let mail_theme = MailTheme::from_app(cx.theme(), options.force_light_theme);
    let (key, job) = prepared_job(
        instance,
        html,
        if fragment {
            PrepSource::HtmlFragment
        } else {
            PrepSource::Html
        },
        images,
        options,
        mail_theme,
        cx,
    );
    dispatch_pending_mailto(&key, window, cx);
    render_element(
        key,
        job,
        fallback_width,
        false,
        true,
        Some(zoom_badge_anchor(window)),
        cx,
    )
}

pub(super) fn zoom_badge_anchor(window: &Window) -> Point<Pixels> {
    let viewport = window.viewport_size();
    point(viewport.width - px(16.), viewport.height - px(16.))
}

pub(super) fn render_element(
    key: String,
    job: Arc<Job>,
    fallback_width: Option<f32>,
    reader: bool,
    zoomable: bool,
    zoom_badge_anchor: Option<Point<Pixels>>,
    cx: &mut App,
) -> gpui::AnyElement {
    let theme = cx.theme().clone();
    let focus = {
        let cache = cx.default_global::<BlitzCache>();
        if reader {
            cache.cancel_pending_readers_except(Some(&key));
        }
        let entry = cache.entry_mut(&key);
        entry.reader = reader;
        let existing = entry.focus.clone();
        match existing {
            Some(f) => f,
            None => {
                let f = cx.focus_handle();
                let entry = cx.default_global::<BlitzCache>().entry_mut(&key);
                entry.reader = reader;
                entry.focus = Some(f.clone());
                f
            }
        }
    };

    let (
        rendered,
        error,
        in_flight,
        cursor,
        has_live,
        zoom,
        zoom_badge_visible,
        context_image,
        context_link,
    ) = {
        let e = cx.default_global::<BlitzCache>().entry_mut(&key);
        (
            e.rendered.clone(),
            e.error.clone(),
            e.in_flight,
            e.cursor.unwrap_or(CursorStyle::Arrow),
            e.live.is_some(),
            e.zoom,
            e.zoom_badge_visible,
            e.context_image.clone(),
            e.context_link.clone(),
        )
    };
    cx.default_global::<BlitzCache>().sweep_idle_documents();
    drop_orphaned_images(cx);
    let mut content = v_flex().w_full().min_w_0().items_start();
    if let Some(r) = &rendered {
        for tile in &r.tiles {
            content = content.child(match &tile.image {
                Some(image) => img(image.clone())
                    .object_fit(ObjectFit::Fill)
                    .flex_shrink_0()
                    .w(px(r.width))
                    .h(px(tile.height))
                    .into_any_element(),
                // Band not rasterized yet (or evicted after scrolling away):
                // reserve its exact height so scroll geometry stays stable.
                None => div()
                    .flex_shrink_0()
                    .w(px(r.width))
                    .h(px(tile.height))
                    .into_any_element(),
            });
        }
        if r.truncated {
            content = content.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tr!("viewer-message-truncated")),
            );
        }
    } else if let Some(e) = &error {
        content = content.child(div().text_sm().text_color(theme.danger).child(e.clone()));
    } else if in_flight {
        content = content.child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(tr!("viewer-rendering-message")),
        );
    } else {
        // First pass: the width measurement below will start rendering.
        content = content.child(div().h(px(24.)));
    }

    let width_probe = canvas(
        {
            let key = key.clone();
            move |bounds, window, cx| {
                measure_and_render(key, job, fallback_width, bounds, window, cx)
            }
        },
        |_, _, _, _| {},
    )
    // Same strategy as `gpui-component::ResizablePanel`: a full-size absolute
    // probe receives reliable bounds from the first prepaint. A zero-height
    // canvas may become measurable only after a window event.
    .absolute()
    .size_full();

    let mut wrapper = div()
        .w_full()
        .min_w_0()
        .relative()
        .track_focus(&focus)
        .cursor(cursor);
    if let Some(width) = fallback_width.filter(|width| *width >= 40.0) {
        // Break the initial empty-intrinsic-width -> zero-probe cycle.
        wrapper = wrapper.min_w(px(width.floor()));
    }

    if has_live {
        wrapper = wrapper
            .on_mouse_down(MouseButton::Left, {
                let key = key.clone();
                let focus = focus.clone();
                move |ev: &MouseDownEvent, window, cx| {
                    if window.default_prevented() {
                        return;
                    }
                    focus.focus(window);
                    push_pointer(
                        cx,
                        &key,
                        PointerPhase::Down,
                        ev.position,
                        true,
                        &ev.modifiers,
                    );
                }
            })
            .on_mouse_up(MouseButton::Left, {
                let key = key.clone();
                move |ev: &MouseUpEvent, window, cx| {
                    if window.default_prevented() {
                        return;
                    }
                    push_pointer(
                        cx,
                        &key,
                        PointerPhase::Up,
                        ev.position,
                        false,
                        &ev.modifiers,
                    );
                }
            })
            // Releasing outside the element ends selection dragging so the
            // document is not left in a pressed-button state.
            .on_mouse_up_out(MouseButton::Left, {
                let key = key.clone();
                move |ev: &MouseUpEvent, _, cx| {
                    push_pointer(
                        cx,
                        &key,
                        PointerPhase::Up,
                        ev.position,
                        false,
                        &ev.modifiers,
                    );
                }
            })
            .on_mouse_move({
                let key = key.clone();
                move |ev: &MouseMoveEvent, _, cx| {
                    let dragging = ev.pressed_button == Some(MouseButton::Left);
                    push_pointer(
                        cx,
                        &key,
                        PointerPhase::Move,
                        ev.position,
                        dragging,
                        &ev.modifiers,
                    );
                }
            })
            .on_key_down({
                let key = key.clone();
                move |ev: &KeyDownEvent, _, cx| {
                    let ks = &ev.keystroke;
                    if ks.key == "escape" {
                        push_op(cx, &key, DocOp::ClearSelection);
                    } else if ks.key == "c" && (ks.modifiers.control || ks.modifiers.platform) {
                        copy_selection(key.clone(), cx);
                    }
                }
            });
    }

    if zoomable {
        wrapper = wrapper.on_scroll_wheel({
            let key = key.clone();
            move |event: &ScrollWheelEvent, window, cx| {
                if !event.modifiers.control && !event.modifiers.platform {
                    return;
                }
                cx.stop_propagation();
                let delta = wheel_zoom_delta(event, window);
                if delta != 0.0 {
                    adjust_zoom(cx, &key, delta);
                }
            }
        });
    }

    let zoom_badge = zoom_badge_visible
        .then_some(zoom_badge_anchor)
        .flatten()
        .map(|anchor| {
            let badge_key = key.clone();
            deferred(
                anchored()
                    .anchor(Corner::BottomRight)
                    .position(anchor)
                    .snap_to_window_with_margin(px(12.))
                    .child(
                        div()
                            .id(gpui::ElementId::Name(
                                format!("blitz-zoom-badge-{key}").into(),
                            ))
                            .occlude()
                            .cursor_pointer()
                            .px_3()
                            .py_1p5()
                            .rounded(cx.theme().radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.popover)
                            .text_color(theme.popover_foreground)
                            .text_sm()
                            .font_semibold()
                            .shadow_md()
                            .hover(|style| style.bg(theme.list_hover))
                            .on_mouse_down(MouseButton::Left, |_, window, _| {
                                window.prevent_default();
                            })
                            .on_mouse_up(MouseButton::Left, |_, window, _| {
                                window.prevent_default();
                            })
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                set_zoom(cx, &badge_key, 1.0);
                            })
                            .child(zoom_label(zoom)),
                    ),
            )
            .with_priority(2)
        });
    let link_badge = context_link
        .as_ref()
        .zip(zoom_badge_anchor)
        .map(|(url, anchor)| {
            let position = if zoom_badge_visible {
                point(anchor.x, anchor.y - px(44.))
            } else {
                anchor
            };
            deferred(
                anchored()
                    .anchor(Corner::BottomRight)
                    .position(position)
                    .snap_to_window_with_margin(px(12.))
                    .child(
                        div()
                            .max_w(px(640.))
                            .overflow_hidden()
                            .truncate()
                            .px_3()
                            .py_1p5()
                            .rounded(cx.theme().radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.popover)
                            .text_color(theme.popover_foreground)
                            .text_xs()
                            .shadow_md()
                            .child(url.clone()),
                    ),
            )
            .with_priority(2)
        });

    let wrapper = wrapper
        .child(width_probe)
        .child(content)
        .children(zoom_badge)
        .children(link_badge);
    match (context_link, context_image) {
        (Some(url), _) => wrapper
            .context_menu(move |menu, _, _| link_context_menu(menu, url.clone()))
            .into_any_element(),
        (None, Some(image)) => {
            let image = crate::ui::image_lightbox::ImageAsset::rendered(image);
            wrapper
                .context_menu(move |menu, _, cx| {
                    crate::ui::image_lightbox::actions_menu(menu, image.clone(), cx)
                })
                .into_any_element()
        }
        (None, None) => wrapper.into_any_element(),
    }
}

pub(super) fn link_context_menu(menu: PopupMenu, url: String) -> PopupMenu {
    let url_to_open = url.clone();
    menu.item(
        PopupMenuItem::new(tr!("ctx-open"))
            .icon(crate::ui::icons::app_icon("external-link"))
            .on_click(move |_, window, cx| activate_link(&url_to_open, window, cx)),
    )
    .item(
        PopupMenuItem::new(tr!("copy"))
            .icon(crate::ui::icons::app_icon("copy"))
            .on_click(move |_, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(url.clone()));
            }),
    )
}

pub(super) fn activate_link(raw: &str, window: &mut Window, cx: &mut App) {
    let Some(url) = safe_link(raw) else {
        return;
    };
    let Some(init) = mailto_compose_init(&url) else {
        open_link(&url);
        return;
    };
    let app = cx
        .try_global::<LinkHandler>()
        .and_then(|handler| handler.app.clone())
        .and_then(|app| app.upgrade());
    if let Some(app) = app {
        app.update(cx, |this, cx| {
            this.open_inline_compose(init.clone(), window, cx);
        });
    } else {
        open_link(&url);
    }
}

/// Turns a `mailto:` anchor into the composer it should open, or `None` when
/// the link is not a `mailto:` at all.
///
/// A `mailto:` is never handed to the system opener, even one that names no
/// valid address: Aviary may itself be the registered handler, so delegating
/// would ask the desktop to launch us again — and the honest answer to a
/// malformed mail link is an empty composer, not a round trip.
pub(super) fn mailto_compose_init(raw: &str) -> Option<crate::ui::compose::ComposeInit> {
    let mailto = crate::mailto::parse(raw)?;
    Some(crate::ui::compose::ComposeInit {
        to: mailto.to,
        cc: mailto.cc,
        bcc: mailto.bcc,
        subject: mailto.subject,
        body_md: mailto.body,
        ..crate::ui::compose::ComposeInit::blank()
    })
}

pub(super) fn dispatch_pending_mailto(key: &str, window: &mut Window, cx: &mut App) {
    let pending = cx
        .default_global::<BlitzCache>()
        .entries
        .get_mut(key)
        .map(|entry| std::mem::take(&mut entry.pending_mailto))
        .unwrap_or_default();
    for url in pending {
        window.defer(cx, move |window, cx| activate_link(&url, window, cx));
    }
}

pub(super) fn wheel_zoom_delta(event: &ScrollWheelEvent, window: &Window) -> f32 {
    match event.delta {
        ScrollDelta::Lines(delta) => {
            let axis = if delta.y == 0.0 { delta.x } else { delta.y };
            axis.signum() * ZOOM_LINE_STEP
        }
        ScrollDelta::Pixels(delta) => {
            let axis = if delta.y == px(0.) { delta.x } else { delta.y };
            let line_height = f32::from(window.line_height()).max(1.0);
            (f32::from(axis) / line_height * ZOOM_LINE_STEP).clamp(-0.2, 0.2)
        }
    }
}

pub(super) fn next_zoom(current: f32, delta: f32) -> f32 {
    ((current + delta).clamp(ZOOM_MIN, ZOOM_MAX) * 20.0).round() / 20.0
}

pub(super) fn zoom_label(zoom: f32) -> String {
    format!("{:.0} %", zoom * 100.0)
}

pub(super) fn adjust_zoom(cx: &mut App, key: &str, delta: f32) {
    let zoom = {
        let Some(entry) = cx.default_global::<BlitzCache>().entries.get(key) else {
            return;
        };
        next_zoom(entry.zoom, delta)
    };
    set_zoom(cx, key, zoom);
}

pub(super) fn set_zoom(cx: &mut App, key: &str, zoom: f32) {
    let (owner, generation) = {
        let Some(entry) = cx.default_global::<BlitzCache>().entries.get_mut(key) else {
            return;
        };
        let Some(generation) = entry.update_zoom(zoom) else {
            return;
        };
        (entry.owner, generation)
    };

    let badge_key = key.to_string();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(ZOOM_BADGE_DURATION).await;
        let owner = cx
            .update(|cx| {
                let entry = cx
                    .default_global::<BlitzCache>()
                    .entries
                    .get_mut(&badge_key)?;
                entry.hide_zoom_badge(generation).then_some(entry.owner)
            })
            .ok()
            .flatten()
            .flatten();
        if let Some(owner) = owner {
            let _ = cx.update(|cx| cx.notify(owner));
        }
    })
    .detach();

    if let Some(owner) = owner {
        cx.notify(owner);
    }
}
