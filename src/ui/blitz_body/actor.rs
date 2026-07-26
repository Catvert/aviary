//! The document's own OS thread.
//!
//! `HtmlDocument` is not `Send`: a rendered document therefore lives on the
//! thread that built it, and every later interaction with it — mouse batches,
//! band painting, late resources, zoom — arrives as a [`DocCmd`] over an mpsc
//! channel. The thread ends when the channel closes, which is what evicting a
//! cache entry does.

use super::events::process_batch;
use super::paint::{
    apply_resources_doc, paint_tile_band, physical_width, raster_scale, render_doc, rerender_doc,
};
use super::*;

/// Starts the actor thread: renders the document, then listens for [`DocCmd`]
/// until the channel closes (eviction or replacement after a width change), at
/// which point the thread exits and releases the document. Zoom rerenders stay
/// on this actor. Because `HtmlDocument` is not `Send`, its entire lifetime
/// occurs on this thread.
pub(super) fn spawn_doc_thread(
    job: Arc<Job>,
    logical_width: f32,
    device_scale: f32,
    zoom: f32,
    initial_range: (f32, f32),
    cancellation: Arc<RenderCancellation>,
    preview_images: bool,
) -> oneshot::Receiver<Result<(Rendered, LiveDoc), String>> {
    let (result_tx, result_rx) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("blitz-doc".into())
        .spawn(move || {
            let mut scale = raster_scale(device_scale, zoom);
            let mut target = (logical_width, device_scale, zoom);
            // gpui rounds image dimensions up when converting to physical
            // pixels. Using the same rounding avoids one-pixel filtered resizing
            // at 125% and 150% scales.
            let mut width_phys = physical_width(logical_width, scale.display);
            let navigation = Arc::new(OpenLinks::default());
            match render_doc(
                &job,
                logical_width,
                width_phys,
                scale,
                navigation.clone(),
                &cancellation,
                initial_range,
            ) {
                Err(e) => {
                    let _ = result_tx.send(Err(e));
                }
                Ok(DocRender {
                    rendered,
                    mut doc,
                    render_h,
                    provider,
                    pending,
                }) => {
                    let mut paint_state = PaintState::new(render_h, job.theme.background_color());
                    paint_state.preview_images = preview_images;
                    paint_state.set_materialized_from(&rendered.tiles);
                    let (tx, rx) = mpsc::channel();
                    if result_tx.send(Ok((rendered, LiveDoc { tx }))).is_err() {
                        return; // receiver gone: nobody holds the channel
                    }
                    while let Ok(cmd) = rx.recv() {
                        match cmd {
                            DocCmd::Batch(batch, out) => {
                                let mut outcome = process_batch(
                                    &mut doc,
                                    batch,
                                    width_phys,
                                    scale,
                                    &mut paint_state,
                                );
                                outcome.mailto_links = navigation.flush(outcome.clicked_image);
                                let _ = out.send((target, outcome));
                            }
                            DocCmd::SelectedContent(out) => {
                                let _ = out.send(selected_content(&doc, &paint_state, &job.images));
                            }
                            DocCmd::PaintBands { bands, out } => {
                                let selected = selected_image_nodes(&doc, &paint_state);
                                let count =
                                    paint_state.render_h.div_ceil(TILE_ROWS).max(1) as usize;
                                let mut painted = Vec::new();
                                for ix in bands {
                                    if ix >= count {
                                        continue;
                                    }
                                    match paint_tile_band(
                                        &mut doc,
                                        width_phys,
                                        scale,
                                        paint_state.render_h,
                                        ix as u32,
                                        job.theme.background_color(),
                                        &selected,
                                    ) {
                                        Ok(band) => {
                                            paint_state.mark_materialized(band.0);
                                            painted.push(band);
                                        }
                                        Err(e) => log::warn!("Blitz band paint: {e}"),
                                    }
                                }
                                let _ = out.send((target, painted));
                            }
                            DocCmd::ApplyResources { visible, out } => {
                                // Nothing delivered since the last poll: answer
                                // without touching the document. `resolve` on a
                                // large newsletter is not free, and this runs
                                // every few hundred milliseconds.
                                if provider.drain_deliveries() == 0 {
                                    let _ = out.send(None);
                                    continue;
                                }
                                let result = apply_resources_doc(
                                    &mut doc,
                                    target.0,
                                    width_phys,
                                    scale,
                                    job.theme.background_color(),
                                    visible,
                                    pending.remote() > 0,
                                );
                                match result {
                                    Ok((rendered, render_h)) => {
                                        paint_state =
                                            PaintState::new(render_h, job.theme.background_color());
                                        paint_state.preview_images = preview_images;
                                        paint_state.set_materialized_from(&rendered.tiles);
                                        let _ = out.send(Some((target, rendered)));
                                    }
                                    Err(e) => {
                                        log::warn!("Blitz late resource paint: {e}");
                                        let _ = out.send(None);
                                    }
                                }
                            }
                            DocCmd::Rerender {
                                logical_width,
                                device_scale,
                                zoom,
                                visible,
                                cancellation,
                                out,
                            } => {
                                let result = rerender_doc(
                                    &mut doc,
                                    (logical_width, device_scale, zoom),
                                    job.theme.background_color(),
                                    &cancellation,
                                    visible,
                                    pending.remote() > 0,
                                );
                                let result = match result {
                                    Ok((rendered, render_h, next_width, next_scale)) => {
                                        width_phys = next_width;
                                        scale = next_scale;
                                        target = (logical_width, device_scale, zoom);
                                        paint_state =
                                            PaintState::new(render_h, job.theme.background_color());
                                        paint_state.preview_images = preview_images;
                                        paint_state.set_materialized_from(&rendered.tiles);
                                        Ok(rendered)
                                    }
                                    Err(err) => Err(err),
                                };
                                let _ = out.send(result);
                            }
                        }
                    }
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("Blitz render thread: {e:#}");
    }
    result_rx
}
