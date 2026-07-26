//! Mouse and keyboard interaction with a live document.
//!
//! gpui events are converted into Blitz `UiEvent`s and queued on the UI side
//! ([`push_pointer`], [`push_op`]); [`pump`] sends them to the document thread
//! in batches and applies what comes back — cursor shape, and a repaint only
//! when the batch actually changed a *visible* selection ([`repaint_dirty`],
//! which paints just the tiles the pointer and the selection touched). A single
//! click on a link or on empty space therefore costs no rasterization.
//!
//! Copying is here too: [`copy_selection`] carries text, an HTML fragment and
//! the selected images to the rich clipboard.

use super::paint::{paint_tile_band, tile_image};
use super::*;

pub(super) fn push_pointer(
    cx: &mut App,
    key: &str,
    phase: PointerPhase,
    position: Point<Pixels>,
    primary_down: bool,
    mods: &gpui::Modifiers,
) {
    let op = {
        let Some(e) = cx.default_global::<BlitzCache>().entries.get_mut(key) else {
            return;
        };
        if e.live.is_none() {
            return;
        }
        let Some(origin) = e.origin else { return };
        let zoom = e.zoom.max(ZOOM_MIN);
        let x = f32::from(position.x - origin.x) / zoom;
        let y = f32::from(position.y - origin.y) / zoom;
        let buttons = if primary_down {
            MouseEventButtons::Primary
        } else {
            MouseEventButtons::empty()
        };
        let ev = blitz_pointer(x, y, buttons, mods);
        DocOp::Ui(match phase {
            PointerPhase::Down => UiEvent::PointerDown(ev),
            PointerPhase::Move => UiEvent::PointerMove(ev),
            PointerPhase::Up => UiEvent::PointerUp(ev),
        })
    };
    push_op(cx, key, op);
}

pub(super) fn blitz_pointer(
    x: f32,
    y: f32,
    buttons: MouseEventButtons,
    mods: &gpui::Modifiers,
) -> BlitzPointerEvent {
    BlitzPointerEvent {
        id: BlitzPointerId::Mouse,
        is_primary: true,
        coords: PointerCoords {
            page_x: x,
            page_y: y,
            client_x: x,
            client_y: y,
            screen_x: x,
            screen_y: y,
        },
        button: MouseEventButton::Main,
        buttons,
        mods: kb_mods(mods),
        details: PointerDetails::default(),
        element: EventPoint { x: 0.0, y: 0.0 },
        active_pointers: Arc::new(atomic_refcell::AtomicRefCell::new(Vec::new())),
    }
}

pub(super) fn kb_mods(m: &gpui::Modifiers) -> keyboard_types::Modifiers {
    let mut out = keyboard_types::Modifiers::empty();
    if m.control {
        out |= keyboard_types::Modifiers::CONTROL;
    }
    if m.shift {
        out |= keyboard_types::Modifiers::SHIFT;
    }
    if m.alt {
        out |= keyboard_types::Modifiers::ALT;
    }
    if m.platform {
        out |= keyboard_types::Modifiers::META;
    }
    out
}

pub(super) fn push_op(cx: &mut App, key: &str, op: DocOp) {
    {
        let Some(e) = cx.default_global::<BlitzCache>().entries.get_mut(key) else {
            return;
        };
        if e.live.is_none() {
            return;
        }
        e.pending.push(op);
    }
    pump(key.to_string(), cx);
}

/// Pump: drains `Entry.pending` in batches, processes them on the background
/// executor, and applies cursor/new tiles. Only one instance per entry
/// (`pump_running`); events arriving during a batch naturally coalesce into
/// the next one.
pub(super) fn pump(key: String, cx: &mut App) {
    {
        let Some(e) = cx.default_global::<BlitzCache>().entries.get_mut(&key) else {
            return;
        };
        if e.pump_running || e.live.is_none() || e.pending.is_empty() {
            return;
        }
        e.pump_running = true;
    }
    cx.spawn(async move |cx| {
        loop {
            let step = cx.update(|cx| {
                let cache = cx.default_global::<BlitzCache>();
                let e = cache.entries.get_mut(&key)?;
                if e.pending.is_empty() || e.live.is_none() {
                    e.pump_running = false;
                    return None;
                }
                Some((
                    e.live.clone().expect("live document checked above"),
                    std::mem::take(&mut e.pending),
                ))
            });
            let (live, batch) = match step {
                Ok(Some(p)) => p,
                _ => break,
            };
            let (out_tx, out_rx) = oneshot::channel();
            let sent = live.tx.send(DocCmd::Batch(batch, out_tx)).is_ok();
            let outcome = if sent { out_rx.await.ok() } else { None };
            let Some((actor_target, outcome)) = outcome else {
                // Document thread ended (entry evicted or re-rendered): release
                // the pump so a future event can restart it.
                let _ = cx.update(|cx| {
                    if let Some(e) = cx.default_global::<BlitzCache>().entries.get_mut(&key) {
                        e.pump_running = false;
                    }
                });
                break;
            };
            let applied = cx.update(|cx| {
                let mut changed = false;
                let mut images_to_drop = Vec::new();
                let mut owner_to_notify = None;
                let BatchOutcome {
                    cursor,
                    update,
                    hovered_image_change,
                    hovered_link_change,
                    mailto_links,
                    ..
                } = outcome;
                if let Some(e) = cx.default_global::<BlitzCache>().entries.get_mut(&key) {
                    // Ignore results from a document replaced in the meantime
                    // by a render at another width.
                    if e.live.as_ref().is_some_and(|l| Arc::ptr_eq(l, &live))
                        && e.live_target
                            .is_some_and(|target| render_targets_match(target, actor_target))
                    {
                        changed = e.cursor != Some(cursor);
                        e.cursor = Some(cursor);
                        if let Some(updates) = update {
                            if let Some(r) = &e.rendered {
                                let mut tiles = r.tiles.clone();
                                for (ix, img, h) in updates {
                                    if let Some(slot) = tiles.get_mut(ix) {
                                        if let Some(old) = slot.image.take() {
                                            images_to_drop.push(old);
                                        }
                                        *slot = Tile {
                                            image: Some(img),
                                            height: h,
                                        };
                                    }
                                }
                                e.rendered = Some(Arc::new(Rendered {
                                    width: r.width,
                                    display: r.display,
                                    tiles,
                                    truncated: r.truncated,
                                    resources_pending: r.resources_pending,
                                }));
                                changed = true;
                            }
                        }
                        if changed {
                            owner_to_notify = e.owner;
                        }
                        if let Some(next_image) = hovered_image_change {
                            if let Some(old) = std::mem::replace(&mut e.context_image, next_image) {
                                images_to_drop.push(old);
                            }
                            changed = true;
                            owner_to_notify = e.owner;
                        }
                        if let Some(next_link) = hovered_link_change {
                            e.context_link = next_link;
                            changed = true;
                            owner_to_notify = e.owner;
                        }
                        if !mailto_links.is_empty() {
                            e.pending_mailto.extend(mailto_links);
                            changed = true;
                            owner_to_notify = e.owner;
                        }
                    }
                }
                cx.default_global::<BlitzCache>().enforce_budget();
                drop_orphaned_images(cx);
                for image in images_to_drop {
                    cx.drop_image(image, None);
                }
                // A simple hover without a cursor change does not redraw the UI;
                // repainting on every mouse move would be expensive.
                if changed {
                    if let Some(owner) = owner_to_notify {
                        cx.notify(owner);
                    }
                }
            });
            if applied.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Processes a batch of operations on the live document (actor thread).
/// Re-rasterizes only when a selection is visible before or after the batch
/// (a simple click on a link or empty area repaints nothing), and only for
/// tiles intersecting the touched region; hover costs only a hit test.
pub(super) fn process_batch(
    doc: &mut HtmlDocument,
    batch: Vec<DocOp>,
    width_phys: u32,
    scale: RasterScale,
    st: &mut PaintState,
) -> BatchOutcome {
    let mut interacted = false;
    let mut clicked_image = false;
    for op in batch {
        match op {
            DocOp::Ui(ev) => {
                match &ev {
                    UiEvent::PointerDown(e) if e.buttons.contains(MouseEventButtons::Primary) => {
                        let y = e.coords.page_y;
                        if let Some(ext) = st.sel_extent.take() {
                            st.mark_dirty(ext);
                        }
                        st.mark_dirty((y, y));
                        st.sel_extent = Some((y, y));
                        st.focus_y = Some(y);
                        st.rich_anchor = doc
                            .as_ref()
                            .hit(e.coords.page_x, e.coords.page_y)
                            .map(|hit| hit.node_id);
                        st.rich_focus = st.rich_anchor;
                        st.rich_anchor_point = Some((e.coords.page_x, e.coords.page_y));
                        st.rich_focus_point = st.rich_anchor_point;
                        st.rich_dragged = false;
                        interacted = true;
                    }
                    UiEvent::PointerMove(e) if e.buttons.contains(MouseEventButtons::Primary) => {
                        let y = e.coords.page_y;
                        let prev = st.focus_y.unwrap_or(y);
                        st.mark_dirty((prev.min(y), prev.max(y)));
                        if let Some(ext) = &mut st.sel_extent {
                            ext.0 = ext.0.min(y);
                            ext.1 = ext.1.max(y);
                        }
                        st.focus_y = Some(y);
                        if let Some((anchor_x, anchor_y)) = st.rich_anchor_point {
                            let dx = e.coords.page_x - anchor_x;
                            let dy = e.coords.page_y - anchor_y;
                            if dx * dx + dy * dy >= 4.0 {
                                st.rich_dragged = true;
                                st.rich_focus = doc
                                    .as_ref()
                                    .hit(e.coords.page_x, e.coords.page_y)
                                    .map(|hit| hit.node_id)
                                    .or(st.rich_focus);
                                st.rich_focus_point = Some((e.coords.page_x, e.coords.page_y));
                            }
                        }
                        interacted = true;
                    }
                    UiEvent::PointerUp(e) => {
                        let y = e.coords.page_y;
                        st.mark_dirty((y, y));
                        if st.rich_dragged {
                            st.rich_focus = doc
                                .as_ref()
                                .hit(e.coords.page_x, e.coords.page_y)
                                .map(|hit| hit.node_id)
                                .or(st.rich_focus);
                            st.rich_focus_point = Some((e.coords.page_x, e.coords.page_y));
                        }
                        if st.preview_images && !st.rich_dragged {
                            let pressed_image = st.rich_anchor.and_then(|node_id| {
                                raster_image_from_node(doc, node_id).map(|(node_id, _)| node_id)
                            });
                            clicked_image = raster_image_at(doc, e.coords.page_x, e.coords.page_y)
                                .is_some_and(|(node_id, _)| Some(node_id) == pressed_image);
                        }
                        interacted = true;
                    }
                    _ => {}
                }
                doc.handle_ui_event(ev);
            }
            DocOp::ClearSelection => {
                doc.as_mut().clear_text_selection();
                if let Some(ext) = st.sel_extent.take() {
                    st.mark_dirty(ext);
                }
                st.rich_anchor = None;
                st.rich_focus = None;
                st.rich_anchor_point = None;
                st.rich_focus_point = None;
                st.rich_dragged = false;
                interacted = true;
            }
        }
    }
    doc.as_mut().resolve(0.0);

    let hovered = st
        .preview_images
        .then(|| hovered_raster_image_node(doc))
        .flatten();
    let hovered_image_change = if hovered.map(|(node_id, _)| node_id) != st.hovered_image_node {
        st.hovered_image_node = hovered.map(|(node_id, _)| node_id);
        st.hovered_image = hovered.and_then(|(_, image)| lightbox_render_image(image));
        Some(st.hovered_image.clone())
    } else {
        None
    };
    let hovered_link = doc
        .as_ref()
        .get_hover_node_id()
        .and_then(|node_id| safe_link_from_node(doc, node_id));
    let hovered_link_change = if hovered_link != st.hovered_link {
        st.hovered_link = hovered_link;
        Some(st.hovered_link.clone())
    } else {
        None
    };

    let cursor = match doc.as_ref().get_cursor() {
        Some(CursorIcon::Pointer) => CursorStyle::PointingHand,
        Some(CursorIcon::Text) => CursorStyle::IBeam,
        _ => CursorStyle::Arrow,
    };

    // A highlight can change only when a non-empty selection is visible before
    // or after the batch (a selection reduced to a caret is invisible).
    // Otherwise the accumulated region can be safely discarded: a later drag
    // will mark it again from `focus_y`.
    let has_visible = doc.as_ref().get_selected_text().is_some() || st.rich_dragged;
    let update = if interacted && (has_visible || st.had_visible) {
        match repaint_dirty(doc, width_phys, scale, st) {
            Ok(u) => u,
            Err(e) => {
                log::warn!("repaint Blitz: {e}");
                None
            }
        }
    } else {
        st.dirty = None;
        None
    };
    st.had_visible = has_visible;

    BatchOutcome {
        cursor,
        update,
        clicked_image,
        hovered_image_change,
        hovered_link_change,
        mailto_links: Vec::new(),
    }
}

pub(super) fn safe_link_from_node(doc: &HtmlDocument, mut node_id: usize) -> Option<String> {
    loop {
        let node = doc.as_ref().get_node(node_id)?;
        if let Some(element) = node.element_data() {
            if element.name.local.as_ref() == "a" {
                let href = element
                    .attrs()
                    .iter()
                    .find(|attribute| attribute.name.local.as_ref() == "href")?
                    .value
                    .as_ref();
                return safe_link(href);
            }
        }
        node_id = node.parent?;
    }
}

pub(super) fn raster_image_from_node(
    doc: &HtmlDocument,
    mut node_id: usize,
) -> Option<(usize, &RasterImageData)> {
    loop {
        let node = doc.as_ref().get_node(node_id)?;
        if let Some(image) = node
            .element_data()
            .and_then(|element| element.raster_image_data())
        {
            return Some((node_id, image));
        }
        node_id = node.parent?;
    }
}

pub(super) fn raster_image_at(
    doc: &HtmlDocument,
    x: f32,
    y: f32,
) -> Option<(usize, &RasterImageData)> {
    let node_id = doc.as_ref().hit(x, y)?.node_id;
    raster_image_from_node(doc, node_id)
}

pub(super) fn hovered_raster_image_node(doc: &HtmlDocument) -> Option<(usize, &RasterImageData)> {
    raster_image_from_node(doc, doc.as_ref().get_hover_node_id()?)
}

pub(super) fn lightbox_render_image(image: &RasterImageData) -> Option<Arc<RenderImage>> {
    let mut rgba =
        image::RgbaImage::from_raw(image.width, image.height, image.data.data().to_vec())?;
    if image.width > LIGHTBOX_MAX_DIMENSION || image.height > LIGHTBOX_MAX_DIMENSION {
        let scale = (LIGHTBOX_MAX_DIMENSION as f64 / image.width.max(image.height) as f64).min(1.0);
        let width = (image.width as f64 * scale).round().max(1.0) as u32;
        let height = (image.height as f64 * scale).round().max(1.0) as u32;
        rgba = image::imageops::resize(&rgba, width, height, image::imageops::FilterType::Lanczos3);
    }
    let width = rgba.width();
    let height = rgba.height();
    tile_image(rgba.into_raw(), width, height).ok()
}

/// Repaints the accumulated dirty region: materialized bands intersecting it,
/// or every materialized band when the region is unknown.
pub(super) fn repaint_dirty(
    doc: &mut HtmlDocument,
    width_phys: u32,
    scale: RasterScale,
    st: &mut PaintState,
) -> Result<Option<Vec<BandTile>>, String> {
    let tile_count = st.render_h.div_ceil(TILE_ROWS).max(1);
    let (first, last) = match st.dirty.take() {
        Some((min_y, max_y)) => {
            let lo = (f64::from((min_y - DIRTY_MARGIN_CSS).max(0.0)) * scale.render) as u32;
            let hi = (f64::from((max_y + DIRTY_MARGIN_CSS).max(0.0)) * scale.render).ceil() as u32;
            if lo >= st.render_h {
                return Ok(None);
            }
            let hi = hi.min(st.render_h - 1);
            (lo / TILE_ROWS, (hi / TILE_ROWS).min(tile_count - 1))
        }
        None => (0, tile_count - 1),
    };

    let mut updates = Vec::new();
    let selected_images = selected_image_nodes(doc, st);
    for ix in first..=last {
        if !st
            .materialized
            .get(ix as usize)
            .copied()
            .unwrap_or_default()
        {
            continue;
        }
        updates.push(paint_tile_band(
            doc,
            width_phys,
            scale,
            st.render_h,
            ix,
            st.background,
            &selected_images,
        )?);
    }
    Ok((!updates.is_empty()).then_some(updates))
}

/// Copies the current selection to the clipboard.
pub(super) fn copy_selection(key: String, cx: &mut App) {
    let live = cx
        .default_global::<BlitzCache>()
        .entries
        .get(&key)
        .and_then(|e| e.live.clone());
    let Some(live) = live else { return };
    cx.spawn(async move |cx| {
        let (out_tx, out_rx) = oneshot::channel();
        if live.tx.send(DocCmd::SelectedContent(out_tx)).is_err() {
            return;
        }
        let content = out_rx.await.ok().flatten();
        if let Some(content) = content {
            let _ = cx.update(|cx| {
                crate::ui::rich_clipboard::write(content.text, content.html, content.images, cx);
            });
        }
    })
    .detach();
}

/// Walks up from a layout node (text or anonymous block) to the actual DOM
/// element containing it. Converts a plain-text body to a minimal HTML document;
/// faithful mode only makes sense for HTML, so wrap it for consistency.
pub(super) fn text_to_html(text: &str) -> String {
    let esc = util::escape_html_text(text);
    format!(
        "<html><body><pre style=\"white-space:pre-wrap;font-family:Inter,'Noto Color Emoji',sans-serif;margin:0\">{esc}</pre></body></html>"
    )
}
