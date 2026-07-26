//! Deciding what to rasterize, and when.
//!
//! A mail body is often thousands of pixels tall, so a document is painted band
//! by band: [`measure_and_render`] produces the first bands around the visible
//! region, [`maintain_bands`] works out which bands the current scroll offset
//! needs, and [`pump_bands`] asks the document thread for the missing ones as
//! the user scrolls, evicting those far offscreen. [`pump_resources`] plays the
//! same role for remote images that arrive after the first paint.

use super::actor::spawn_doc_thread;
use super::*;

/// Width-probe prepaint: remembers the content origin for mouse-to-document
/// conversion and triggers a render when the actually available panel width
/// changes. The probe is empty and constrained to 100%, unlike fixed-width tiles.
pub(super) fn measure_and_render(
    key: String,
    job: Arc<Job>,
    fallback_width: Option<f32>,
    bounds: gpui::Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let measured_width = f32::from(bounds.size.width).floor();
    let Some(width) = resolve_render_width(measured_width, fallback_width) else {
        return;
    };
    let device_scale = window.scale_factor();
    // Vertical range of the document (logical px, relative to its top) worth
    // rasterizing right now: the window-visible part plus one viewport of
    // margin on each side so scrolling meets already-painted bands.
    let viewport_h = f32::from(window.viewport_size().height).max(200.0);
    let origin_y = f32::from(bounds.origin.y);
    let paint_range = (
        (-origin_y - viewport_h).max(0.0),
        viewport_h - origin_y + viewport_h,
    );
    let (target, evicted) = {
        let owner = window.current_view();
        let entry = cx.default_global::<BlitzCache>().entry_mut(&key);
        entry.origin = Some(bounds.origin);
        entry.owner = Some(owner);
        entry.last_seen = Some(Instant::now());
        let zoom = entry.zoom;
        let evicted = maintain_bands(entry, paint_range, viewport_h, width, device_scale);
        let reusable = entry
            .can_rerender_live(width, device_scale)
            .then(|| entry.live.clone().expect("reusable live document"));
        // A first open has nothing on screen: start immediately. The debounce
        // only smooths resize/zoom bursts over existing content.
        let debounce = if entry.rendered.is_some() {
            RENDER_DEBOUNCE
        } else {
            Duration::ZERO
        };
        let target = entry.request_target(width, device_scale, zoom).map(|gen| {
            let cancellation = entry
                .render_cancellation
                .clone()
                .expect("request_target creates its cancellation signal");
            (gen, cancellation, zoom, reusable, debounce, entry.reader)
        });
        (target, evicted)
    };
    for image in evicted {
        cx.drop_image(image, None);
    }
    pump_bands(key.clone(), cx);
    pump_resources(key.clone(), cx);
    let Some((generation, cancellation, zoom, reusable, debounce, preview_images)) = target else {
        return;
    };

    cx.spawn(async move |cx| {
        if !debounce.is_zero() {
            cx.background_executor().timer(debounce).await;
        }
        let still_current = cx
            .update(|cx| {
                cx.default_global::<BlitzCache>()
                    .entries
                    .get(&key)
                    .is_some_and(|entry| entry.target_generation == generation)
            })
            .unwrap_or(false);
        if !still_current {
            cancellation.cancel();
            return;
        }
        let result = if let Some(live) = reusable {
            let (out_tx, out_rx) = oneshot::channel();
            let sent = live.tx.send(DocCmd::Rerender {
                logical_width: width,
                device_scale,
                zoom,
                visible: paint_range,
                cancellation: cancellation.clone(),
                out: out_tx,
            });
            let result = if sent.is_ok() {
                out_rx
                    .await
                    .unwrap_or_else(|_| Err(render_interrupted_error()))
            } else {
                Err(render_interrupted_error())
            };
            match result {
                Ok(rendered) => RenderAttempt::Reuse {
                    live,
                    result: Ok(rendered),
                },
                Err(err) if err == render_cancelled_error() => RenderAttempt::Reuse {
                    live,
                    result: Err(err),
                },
                Err(_) => {
                    let replacement = match spawn_doc_thread(
                        job.clone(),
                        width,
                        device_scale,
                        zoom,
                        paint_range,
                        cancellation,
                        preview_images,
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => Err(render_interrupted_error()),
                    };
                    RenderAttempt::Replace(replacement)
                }
            }
        } else {
            let result = match spawn_doc_thread(
                job,
                width,
                device_scale,
                zoom,
                paint_range,
                cancellation,
                preview_images,
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(render_interrupted_error()),
            };
            RenderAttempt::Replace(result)
        };
        let _ = cx.update(|cx| {
            let mut images_to_drop = Vec::new();
            let mut owner_to_notify = None;
            match cx.default_global::<BlitzCache>().entries.get_mut(&key) {
                Some(e) if e.target_generation == generation => {
                    e.in_flight = false;
                    e.render_cancellation = None;
                    owner_to_notify = e.owner;
                    match result {
                        RenderAttempt::Replace(Ok((rendered, live))) => {
                            replace_rendered(e, rendered, &mut images_to_drop);
                            e.live = Some(Arc::new(live));
                            e.live_target = Some((width, device_scale, zoom));
                            e.error = None;
                            e.pending.clear();
                            e.pending_bands.clear();
                            e.cursor = None;
                        }
                        RenderAttempt::Reuse {
                            live,
                            result: Ok(rendered),
                        } => {
                            if e.live
                                .as_ref()
                                .is_some_and(|current| Arc::ptr_eq(current, &live))
                            {
                                replace_rendered(e, rendered, &mut images_to_drop);
                                e.live_target = Some((width, device_scale, zoom));
                                e.error = None;
                                e.pending.clear();
                                e.pending_bands.clear();
                                e.cursor = None;
                            } else {
                                images_to_drop.extend(
                                    rendered.tiles.iter().filter_map(|tile| tile.image.clone()),
                                );
                            }
                        }
                        RenderAttempt::Replace(Err(err))
                        | RenderAttempt::Reuse {
                            result: Err(err), ..
                        } => e.error = Some(err),
                    }
                }
                // The entry was evicted or a newer measurement won the race;
                // also release tiles that are no longer useful.
                _ => match result {
                    RenderAttempt::Replace(Ok((rendered, _)))
                    | RenderAttempt::Reuse {
                        result: Ok(rendered),
                        ..
                    } => {
                        images_to_drop
                            .extend(rendered.tiles.iter().filter_map(|tile| tile.image.clone()));
                    }
                    RenderAttempt::Replace(Err(_))
                    | RenderAttempt::Reuse { result: Err(_), .. } => {}
                },
            }
            {
                let cache = cx.default_global::<BlitzCache>();
                cache.touch(&key);
                cache.enforce_budget();
            }
            drop_orphaned_images(cx);
            for image in images_to_drop {
                cx.drop_image(image, None);
            }
            if let Some(owner) = owner_to_notify {
                cx.notify(owner);
            }
        });
    })
    .detach();
}

/// Band bookkeeping run at every probe prepaint: queues missing bands inside
/// the desired range for the band pump, and evicts bands far outside it
/// (returning their images so the caller can free the GPU textures).
pub(super) fn maintain_bands(
    entry: &mut Entry,
    paint_range: (f32, f32),
    viewport_h: f32,
    width: f32,
    device_scale: f32,
) -> Vec<Arc<RenderImage>> {
    let mut evicted = Vec::new();
    // Only meaningful when the displayed tiles belong to the current target;
    // during a resize/zoom the upcoming render brings its own bands.
    if entry.live.is_none()
        || !entry
            .live_target
            .is_some_and(|t| render_targets_match(t, (width, device_scale, entry.zoom)))
    {
        return evicted;
    }
    let Some(rendered) = entry.rendered.clone() else {
        return evicted;
    };
    let count = rendered.tiles.len();
    if count == 0 {
        return evicted;
    }
    let display = f64::from(rendered.display);
    let band_ix = |logical: f32| -> usize {
        ((f64::from(logical.max(0.0)) * display) as u64 / u64::from(TILE_ROWS))
            .min(count as u64 - 1) as usize
    };
    let desired = (band_ix(paint_range.0), band_ix(paint_range.1));
    entry.desired_bands = Some(desired);
    entry.last_paint_range = Some(paint_range);
    for ix in desired.0..=desired.1 {
        if rendered.tiles[ix].image.is_none() && !entry.pending_bands.contains(&ix) {
            entry.pending_bands.push(ix);
        }
    }
    // Keep a wider window than we request so slow back-and-forth scrolling
    // does not oscillate between evicting and repainting the same bands.
    let keep = (
        band_ix(paint_range.0 - 2.0 * viewport_h),
        band_ix(paint_range.1 + 2.0 * viewport_h),
    );
    if rendered
        .tiles
        .iter()
        .enumerate()
        .any(|(ix, tile)| tile.image.is_some() && (ix < keep.0 || ix > keep.1))
    {
        let mut tiles = rendered.tiles.clone();
        for (ix, tile) in tiles.iter_mut().enumerate() {
            if ix < keep.0 || ix > keep.1 {
                if let Some(image) = tile.image.take() {
                    evicted.push(image);
                }
            }
        }
        entry.rendered = Some(Arc::new(Rendered {
            width: rendered.width,
            display: rendered.display,
            tiles,
            truncated: rendered.truncated,
            resources_pending: rendered.resources_pending,
        }));
    }
    evicted
}

/// Band pump: single-flight loop sending queued missing bands to the actor and
/// installing the returned tiles. Stale requests (outside the latest desired
/// range) are dropped at send time.
pub(super) fn pump_bands(key: String, cx: &mut App) {
    {
        let Some(e) = cx.default_global::<BlitzCache>().entries.get_mut(&key) else {
            return;
        };
        if e.band_pump_running || e.live.is_none() || e.pending_bands.is_empty() {
            return;
        }
        e.band_pump_running = true;
    }
    cx.spawn(async move |cx| {
        loop {
            let step = cx.update(|cx| {
                let cache = cx.default_global::<BlitzCache>();
                let e = cache.entries.get_mut(&key)?;
                let mut bands = std::mem::take(&mut e.pending_bands);
                if let Some((lo, hi)) = e.desired_bands {
                    bands.retain(|ix| *ix >= lo && *ix <= hi);
                }
                if bands.is_empty() || e.live.is_none() {
                    e.band_pump_running = false;
                    return None;
                }
                bands.sort_unstable();
                bands.dedup();
                Some((e.live.clone().expect("live document checked above"), bands))
            });
            let (live, bands) = match step {
                Ok(Some(p)) => p,
                _ => break,
            };
            let (out_tx, out_rx) = oneshot::channel();
            let sent = live
                .tx
                .send(DocCmd::PaintBands { bands, out: out_tx })
                .is_ok();
            let outcome = if sent { out_rx.await.ok() } else { None };
            let Some((actor_target, band_tiles)) = outcome else {
                let _ = cx.update(|cx| {
                    if let Some(e) = cx.default_global::<BlitzCache>().entries.get_mut(&key) {
                        e.band_pump_running = false;
                    }
                });
                break;
            };
            let applied = cx.update(|cx| {
                let mut images_to_drop = Vec::new();
                let mut owner_to_notify = None;
                if let Some(e) = cx.default_global::<BlitzCache>().entries.get_mut(&key) {
                    if e.live.as_ref().is_some_and(|l| Arc::ptr_eq(l, &live))
                        && e.live_target
                            .is_some_and(|target| render_targets_match(target, actor_target))
                    {
                        if let Some(r) = &e.rendered {
                            if !band_tiles.is_empty() {
                                let mut tiles = r.tiles.clone();
                                for (ix, image, height) in band_tiles {
                                    // A prepaint may have re-queued this band
                                    // while it was being painted.
                                    e.pending_bands.retain(|queued| *queued != ix);
                                    if let Some(slot) = tiles.get_mut(ix) {
                                        if let Some(old) = slot.image.take() {
                                            images_to_drop.push(old);
                                        }
                                        *slot = Tile {
                                            image: Some(image),
                                            height,
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
                                owner_to_notify = e.owner;
                            }
                        }
                    }
                }
                {
                    let cache = cx.default_global::<BlitzCache>();
                    cache.enforce_budget();
                }
                drop_orphaned_images(cx);
                for image in images_to_drop {
                    cx.drop_image(image, None);
                }
                if let Some(owner) = owner_to_notify {
                    cx.notify(owner);
                }
            });
            if applied.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Resource pump: polls the actor for remote resources that landed after the
/// first paint and swaps in the repainted tiles.
///
/// The first paint no longer waits for remote images, so this is what makes
/// them appear at all. Polling (rather than being woken by the network task) is
/// deliberate: deliveries may only be applied on the actor thread, which is the
/// only place allowed to touch Stylo state, and a poll that finds nothing costs
/// one channel round trip.
///
/// Single-flight per entry, and bounded by [`RESOURCE_PUMP_BUDGET`] so a server
/// that never answers cannot keep it spinning: HTTP requests carry their own
/// timeout, after which the actor reports no resources left in flight.
pub(super) fn pump_resources(key: String, cx: &mut App) {
    {
        let Some(e) = cx.default_global::<BlitzCache>().entries.get_mut(&key) else {
            return;
        };
        let waiting = e
            .rendered
            .as_ref()
            .is_some_and(|rendered| rendered.resources_pending);
        if e.resource_pump_running || e.live.is_none() || !waiting {
            return;
        }
        e.resource_pump_running = true;
    }
    cx.spawn(async move |cx| {
        let started = Instant::now();
        loop {
            cx.background_executor().timer(RESOURCE_POLL).await;
            let step = cx.update(|cx| {
                let cache = cx.default_global::<BlitzCache>();
                let e = cache.entries.get_mut(&key)?;
                let waiting = e
                    .rendered
                    .as_ref()
                    .is_some_and(|rendered| rendered.resources_pending);
                if !waiting || e.live.is_none() || started.elapsed() > RESOURCE_PUMP_BUDGET {
                    e.resource_pump_running = false;
                    return None;
                }
                let visible = e.last_paint_range.unwrap_or((0.0, 0.0));
                Some((
                    e.live.clone().expect("live document checked above"),
                    visible,
                ))
            });
            let (live, visible) = match step {
                Ok(Some(step)) => step,
                _ => break,
            };
            let (out_tx, out_rx) = oneshot::channel();
            let sent = live
                .tx
                .send(DocCmd::ApplyResources {
                    visible,
                    out: out_tx,
                })
                .is_ok();
            let outcome = if sent {
                out_rx.await.ok().flatten()
            } else {
                None
            };
            let applied = cx.update(|cx| {
                let mut images_to_drop = Vec::new();
                let mut owner_to_notify = None;
                if let Some(e) = cx.default_global::<BlitzCache>().entries.get_mut(&key) {
                    if !sent {
                        e.resource_pump_running = false;
                    }
                    if let Some((actor_target, rendered)) = outcome {
                        // Same guards as the band pump: a repaint produced for
                        // a width or zoom the UI has already moved past must
                        // not replace the current tiles.
                        if e.live.as_ref().is_some_and(|l| Arc::ptr_eq(l, &live))
                            && e.live_target
                                .is_some_and(|target| render_targets_match(target, actor_target))
                        {
                            replace_rendered(e, rendered, &mut images_to_drop);
                            e.pending_bands.clear();
                            owner_to_notify = e.owner;
                        } else {
                            images_to_drop.extend(
                                rendered.tiles.iter().filter_map(|tile| tile.image.clone()),
                            );
                        }
                    }
                }
                {
                    let cache = cx.default_global::<BlitzCache>();
                    cache.enforce_budget();
                }
                drop_orphaned_images(cx);
                for image in images_to_drop {
                    cx.drop_image(image, None);
                }
                if let Some(owner) = owner_to_notify {
                    cx.notify(owner);
                }
            });
            if applied.is_err() || !sent {
                break;
            }
        }
        let _ = cx.update(|cx| {
            if let Some(e) = cx.default_global::<BlitzCache>().entries.get_mut(&key) {
                e.resource_pump_running = false;
            }
        });
    })
    .detach();
}

pub(super) fn replace_rendered(
    entry: &mut Entry,
    rendered: Rendered,
    images_to_drop: &mut Vec<Arc<RenderImage>>,
) {
    if let Some(old) = entry.rendered.replace(Arc::new(rendered)) {
        images_to_drop.extend(old.tiles.iter().filter_map(|tile| tile.image.clone()));
    }
}

/// Local measurement retains priority. The resizable panel is used only during
/// initial layout, when an intrinsic-width descendant may temporarily receive 0 px.
pub(super) fn resolve_render_width(measured: f32, panel_fallback: Option<f32>) -> Option<f32> {
    if measured >= 40.0 {
        Some(measured.floor())
    } else {
        panel_fallback
            .filter(|width| *width >= 40.0)
            .map(|width| width.floor())
    }
}
