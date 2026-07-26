//! Layout and rasterization of one mail document.
//!
//! Everything here runs on the document's own thread (spawned by
//! `spawn_doc_thread`):
//! `HtmlDocument` is not `Send`, so it is created, laid out, painted and
//! dropped without ever crossing a thread boundary. The functions come in two
//! groups: those that produce or refresh a document ([`render_doc`],
//! [`rerender_doc`], [`apply_resources_doc`]) and those that turn its layout
//! into gpui images, band by band ([`paint_tiles`], [`paint_tile_band`],
//! [`tile_image`]).

use super::*;

/// Typography context specific to Blitz.
///
/// gpui and Blitz use independent text engines: loading a font through
/// `App::text_system` does not make it visible to Parley. Keep system-font
/// discovery (required to honor families requested by emails) and place
/// Aviary's fonts before their corresponding generic families.
///
/// Building a `FontContext` from scratch redoes the system-font scan and the
/// embedded registrations for every document. A prototype is built once and
/// cloned per document: fontique clones share the system-font store (`Arc`)
/// and a shared `SourceCache` loads each font file only once process-wide.
pub(super) fn blitz_font_context() -> FontContext {
    static PROTO: OnceLock<Mutex<FontContext>> = OnceLock::new();
    let proto = PROTO.get_or_init(|| Mutex::new(build_blitz_font_context()));
    let guard = proto.lock().expect("font context prototype");
    FontContext {
        collection: guard.collection.clone(),
        source_cache: guard.source_cache.clone(),
    }
}

pub(super) fn build_blitz_font_context() -> FontContext {
    fn register(ctx: &mut FontContext, bytes: &'static [u8]) -> Vec<fontique::FamilyId> {
        ctx.collection
            .register_fonts(Blob::new(Arc::new(bytes) as _), None)
            .into_iter()
            .map(|(family, _)| family)
            .collect()
    }

    let mut ctx = FontContext {
        collection: fontique::Collection::default(),
        source_cache: fontique::SourceCache::new_shared(),
    };
    let mut inter = register(&mut ctx, crate::ui::INTER_FONT);
    inter.extend(register(&mut ctx, crate::ui::INTER_BOLD_FONT));
    inter.sort_unstable();
    inter.dedup();

    let mono = register(&mut ctx, crate::ui::JETBRAINS_MONO_FONT);
    // Vello CPU/Glifo supports the CBDT/PNG strikes embedded in Noto Color
    // Emoji. Sharing the same font as gpui keeps composer previews and faithful
    // message rendering consistent with the editable inputs.
    let emoji = register(&mut ctx, crate::ui::NOTO_COLOR_EMOJI_FONT);

    for generic in [GenericFamily::SansSerif, GenericFamily::SystemUi] {
        ctx.collection
            .append_generic_families(generic, inter.iter().copied());
    }
    ctx.collection
        .append_generic_families(GenericFamily::Monospace, mono.into_iter());
    ctx.collection
        .append_generic_families(GenericFamily::Emoji, emoji.into_iter());
    ctx
}

pub(super) fn raster_scale(device_scale: f32, zoom: f32) -> RasterScale {
    let device_scale = f64::from(device_scale.max(0.5));
    RasterScale {
        render: device_scale * f64::from(zoom.clamp(ZOOM_MIN, ZOOM_MAX)),
        display: device_scale,
    }
}

pub(super) fn physical_width(logical_width: f32, scale: f64) -> u32 {
    (f64::from(logical_width) * scale).ceil().max(16.0) as u32
}

/// Keeps minified HTML5 email documents on Blitz's HTML parser path.
///
/// blitz-html beta.1 detects XHTML by searching the entire first line after a
/// `<!DOCTYPE`. A minified HTML5 document whose `<html>` element carries the
/// customary XHTML namespace therefore gets parsed as XML. Email markup still
/// uses HTML void elements such as `<br>` (rather than `<br />`), so XML parsing
/// nests the rest of the paragraph below the first break and makes it vanish.
/// Isolating a plain HTML5 doctype on its own line preserves the document while
/// avoiding that overly broad upstream heuristic. Real XHTML doctypes contain
/// more than the single `html` token and are intentionally left untouched.
pub(super) fn disambiguate_html5_doctype(html: &str) -> Cow<'_, str> {
    let Some(rest) = html.strip_prefix("<!DOCTYPE") else {
        return Cow::Borrowed(html);
    };
    let Some(relative_end) = rest.find('>') else {
        return Cow::Borrowed(html);
    };
    if !rest[..relative_end].trim().eq_ignore_ascii_case("html") {
        return Cow::Borrowed(html);
    }

    let doctype_end = "<!DOCTYPE".len() + relative_end;
    let first_line_end = html.find(['\r', '\n']).unwrap_or(html.len());
    if doctype_end >= first_line_end
        || !html[..first_line_end].contains("XHTML") && !html[..first_line_end].contains("xhtml")
    {
        return Cow::Borrowed(html);
    }

    let mut normalized = String::with_capacity(html.len() + 1);
    normalized.push_str(&html[..=doctype_end]);
    normalized.push('\n');
    normalized.push_str(&html[doctype_end + 1..]);
    Cow::Owned(normalized)
}

/// Re-lays out and re-rasterizes a live document after late remote resources
/// were handed to it, without reparsing anything.
///
/// A newly arrived image usually changes the document height (the layout ran
/// with no intrinsic size for it), so the whole tile table is rebuilt exactly
/// as the first paint built it. Only bands around `visible` are rasterized;
/// the band pump fills the rest on scroll.
///
/// The text selection is dropped, like every other full repaint of this
/// document. Resources land within seconds of opening a message, so this is
/// almost never observable — but selecting text in a body whose images are
/// still loading will lose the selection when they land.
pub(super) fn apply_resources_doc(
    doc: &mut HtmlDocument,
    logical_width: f32,
    width_phys: u32,
    scale: RasterScale,
    background: Color,
    visible: (f32, f32),
    still_pending: bool,
) -> Result<(Rendered, u32), String> {
    doc.as_mut().resolve(0.0);
    let (tiles, truncated, render_h) =
        paint_tiles(doc, width_phys, scale, background, &[], None, visible)?;
    Ok((
        Rendered {
            // Taken from the actor's target rather than derived back from
            // `width_phys`: `physical_width` rounds up, so recomputing it would
            // drift by a fraction of a pixel and make the UI believe the width
            // changed — which would schedule a full re-render, on every poll.
            width: logical_width,
            display: scale.display as f32,
            tiles,
            truncated,
            resources_pending: still_pending,
        },
        render_h,
    ))
}

/// Reuses a fully loaded live document for a pure zoom change. Parsing, fonts,
/// images and links remain installed, while Blitz invalidates the scale-sensitive
/// inline contexts and incrementally resolves the new viewport. Every physical
/// pixel changes with zoom, so previous tiles are all discarded; only bands
/// around `visible` are rasterized immediately.
pub(super) fn rerender_doc(
    doc: &mut HtmlDocument,
    target: RenderTarget,
    background: Color,
    cancellation: &RenderCancellation,
    visible: (f32, f32),
    still_pending: bool,
) -> Result<(Rendered, u32, u32, RasterScale), String> {
    let (logical_width, device_scale, zoom) = target;
    // Commands waiting behind a newer zoom are discarded before mutating the
    // actor document. Once painting starts it completes coherently; vello_cpu's
    // raster call cannot be interrupted safely anyway.
    cancellation.check()?;

    let scale = raster_scale(device_scale, zoom);
    let width_phys = physical_width(logical_width, scale.display);
    let viewport_h_phys = (800.0 * scale.render) as u32;
    {
        let inner = doc.as_mut();
        inner.clear_text_selection();
        let mut viewport = inner.viewport_mut();
        viewport.window_size = (width_phys, viewport_h_phys);
        viewport.set_hidpi_scale(scale.display as f32);
        viewport.set_zoom((scale.render / scale.display) as f32);
    }
    doc.as_mut().resolve(0.0);

    let (tiles, truncated, render_h) =
        paint_tiles(doc, width_phys, scale, background, &[], None, visible)?;
    Ok((
        Rendered {
            width: logical_width,
            display: scale.display as f32,
            tiles,
            truncated,
            // A zoom does not stop pending downloads; keep the pump armed.
            resources_pending: still_pending,
        },
        render_h,
        width_phys,
        scale,
    ))
}

pub(super) fn render_doc(
    job: &Job,
    logical_width: f32,
    width_phys: u32,
    scale: RasterScale,
    nav: Arc<dyn NavigationProvider>,
    cancellation: &RenderCancellation,
    initial_range: (f32, f32),
) -> Result<DocRender, String> {
    let started = Instant::now();
    cancellation.check()?;
    let viewport_h_phys = (800.0 * scale.render) as u32;

    let pending = Arc::new(NetPending::default());
    let provider = Arc::new(MailNet::new(
        job.images
            .iter()
            .map(|image| (image.cid.clone(), image.bytes.clone()))
            .collect(),
        job.allow_remote,
        pending.clone(),
    ));

    let theme_css = job.theme.css();
    let mut ua_stylesheets = vec![blitz_dom::DEFAULT_CSS.to_string(), MAIL_UA_CSS.to_string()];
    if job.intrinsic_height {
        ua_stylesheets.push(FRAGMENT_UA_CSS.to_string());
    }
    if job.force_uniform_font_family || job.force_uniform_font_size {
        ua_stylesheets.push(uniform_typography_ua_css(
            job.force_uniform_font_family,
            job.force_uniform_font_size,
            job.uniform_font_size,
        ));
    }
    ua_stylesheets.push(theme_css);
    let themed_html = adapt_dark_colors(&job.html, job.theme);
    let html = disambiguate_html5_doctype(&themed_html);
    let document_started = Instant::now();
    let mut doc = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            font_ctx: Some(blitz_font_context()),
            base_url: Some(MAIL_DOCUMENT_BASE_URL.to_string()),
            net_provider: Some(provider.clone()),
            navigation_provider: Some(nav),
            // Replaces the default value (DEFAULT_CSS only): the fixes stylesheet
            // must come later to win at equal specificity.
            ua_stylesheets: Some(ua_stylesheets),
            viewport: Some(Viewport::new(
                width_phys,
                viewport_h_phys,
                scale.render as f32,
                job.theme.scheme(),
            )),
            ..Default::default()
        },
    );
    let document_elapsed = document_started.elapsed();
    cancellation.check()?;

    // Style/layout, then wait for the resources that must be on screen at the
    // first frame (they arrive only through the NetProvider; each `resolve`
    // applies resources already received and may trigger new ones, for example
    // CSS -> images).
    //
    // Remote images are deliberately *not* waited on: a single tracking pixel
    // on an unreachable host used to hold the whole body hostage for the seven
    // seconds of its HTTP timeout, for a document that costs tens of
    // milliseconds to lay out. They keep downloading in the background and the
    // resource pump folds them in through `DocCmd::ApplyResources`.
    //
    // Two things still block: locally-resolved resources (`cid:`, `data:`,
    // blocked remotes), which are already in hand and would otherwise be
    // missing from the first frame, and Blitz's own critical resources —
    // `<head>` stylesheets, which decide the layout and would make an early
    // paint show unstyled content.
    let resolve_started = Instant::now();
    let mut resolve_cpu = Duration::ZERO;
    let mut resource_callback_cpu = Duration::ZERO;
    let mut resource_callbacks = 0usize;
    let mut resolve_passes = 0usize;
    let mut resource_wait_cycles = 0usize;
    loop {
        cancellation.check()?;
        let callbacks_started = Instant::now();
        resource_callbacks += provider.drain_deliveries();
        resource_callback_cpu += callbacks_started.elapsed();
        cancellation.check()?;
        let pass_started = Instant::now();
        doc.as_mut().resolve(0.0);
        resolve_cpu += pass_started.elapsed();
        resolve_passes += 1;
        cancellation.check()?;
        let blocking_done = pending.local() == 0 && !doc.as_ref().has_pending_critical_resources();
        if blocking_done || resolve_started.elapsed() > NET_TIMEOUT {
            break;
        }
        // A local callback, or a network request that completed during
        // `resolve`, can be consumed immediately without paying a 25 ms poll.
        if provider.has_deliveries() {
            continue;
        }
        resource_wait_cycles += 1;
        std::thread::sleep(Duration::from_millis(25));
    }
    cancellation.check()?;
    let resolve_elapsed = resolve_started.elapsed();
    let resource_wait_elapsed = resolve_elapsed
        .saturating_sub(resolve_cpu)
        .saturating_sub(resource_callback_cpu);

    let paint_started = Instant::now();
    let (tiles, truncated, render_h) = paint_tiles(
        &mut doc,
        width_phys,
        scale,
        job.theme.background_color(),
        &[],
        Some(cancellation),
        initial_range,
    )?;
    let paint_elapsed = paint_started.elapsed();
    let materialized_tiles = tiles.iter().filter(|tile| tile.image.is_some()).count();
    let remote_in_flight = pending.remote();
    log::debug!(
        "Blitz email rendered in {} ms \
         (document_ms={}, resolve_ms={}, resolve_cpu_ms={}, resource_wait_ms={}, \
         resource_callback_ms={}, resource_callbacks={}, resolve_passes={}, \
         resource_wait_cycles={}, paint_ms={}, html_bytes={}, inline_images={}, \
         remote_images={}, deferred_remote={}, width_px={}, height_px={}, \
         materialized_tiles={})",
        started.elapsed().as_millis(),
        document_elapsed.as_millis(),
        resolve_elapsed.as_millis(),
        resolve_cpu.as_millis(),
        resource_wait_elapsed.as_millis(),
        resource_callback_cpu.as_millis(),
        resource_callbacks,
        resolve_passes,
        resource_wait_cycles,
        paint_elapsed.as_millis(),
        job.html.len(),
        job.images.len(),
        job.allow_remote,
        remote_in_flight,
        width_phys,
        render_h,
        materialized_tiles
    );

    Ok(DocRender {
        rendered: Rendered {
            width: logical_width,
            display: scale.display as f32,
            tiles,
            truncated,
            resources_pending: remote_in_flight > 0,
        },
        doc,
        render_h,
        provider,
        pending,
    })
}

/// Band indices (inclusive) covering a logical vertical range of the document.
pub(super) fn bands_for_logical_range(
    range: (f32, f32),
    display: f64,
    render_h: u32,
) -> (u32, u32) {
    let count = render_h.div_ceil(TILE_ROWS).max(1);
    let band = |logical: f32| -> u32 {
        ((f64::from(logical.max(0.0)) * display) as u64 / u64::from(TILE_ROWS))
            .min(u64::from(count - 1)) as u32
    };
    (band(range.0), band(range.1.max(range.0)))
}

/// Rasterizes the resolved document into BGRA tiles ready for gpui, band by
/// band: peak memory stays at one band and cancellation can land between
/// bands. Only bands covering `range_logical` are painted — the rest of the
/// tile table carries heights only and is filled lazily by the band pump.
/// Also returns the physical render height for partial repaints.
pub(super) fn paint_tiles(
    doc: &mut HtmlDocument,
    width_phys: u32,
    scale: RasterScale,
    background: Color,
    selected_images: &[usize],
    cancellation: Option<&RenderCancellation>,
    range_logical: (f32, f32),
) -> Result<(Tiles, bool, u32), String> {
    if let Some(cancellation) = cancellation {
        cancellation.check()?;
    }
    let root = doc.as_ref().root_element();
    let layout_h_phys = f64::from(root.final_layout.size.height) * scale.render;
    // Newsletter styles commonly force `html, body { height: 100% !important; }`.
    // In that case the root layout remains one viewport tall even though its
    // descendants continue below it. Blitz records that real painted extent in
    // device pixels as the root's scrollable overflow, so use its bottom edge
    // when sizing our vertically tiled canvas.
    let overflow_h_phys = root.scrollable_overflow.y1.max(0.0);
    let full_phys_h = layout_h_phys.max(overflow_h_phys).ceil().max(32.0) as u64;
    let truncated = full_phys_h > u64::from(MAX_PHYS_HEIGHT);
    let render_h = full_phys_h.min(u64::from(MAX_PHYS_HEIGHT)) as u32;

    let mut tiles: Tiles = Vec::new();
    let mut row = 0u32;
    while row < render_h {
        let rows = (render_h - row).min(TILE_ROWS);
        tiles.push(Tile {
            image: None,
            height: rows as f32 / scale.display as f32,
        });
        row += rows;
    }

    let (first, last) = bands_for_logical_range(range_logical, scale.display, render_h);
    for ix in first..=last {
        if let Some(cancellation) = cancellation {
            cancellation.check()?;
        }
        let (slot, image, height) = paint_tile_band(
            doc,
            width_phys,
            scale,
            render_h,
            ix,
            background,
            selected_images,
        )?;
        tiles[slot] = Tile {
            image: Some(image),
            height,
        };
    }

    Ok((tiles, truncated, render_h))
}

/// Repaints one tile band. Blitz's painter subtracts viewport scroll and culls
/// nodes outside the clip: setting scroll to the top of the band renders exactly
/// that tile at the cost of only its visible content. Scroll is reset afterward
/// because event hit tests assume an unscrolled document.
pub(super) fn paint_tile_band(
    doc: &mut HtmlDocument,
    width_phys: u32,
    scale: RasterScale,
    render_h: u32,
    tile_ix: u32,
    background: Color,
    selected_images: &[usize],
) -> Result<(usize, Arc<RenderImage>, f32), String> {
    let start_row = tile_ix * TILE_ROWS;
    let rows = (render_h - start_row).min(TILE_ROWS);
    doc.as_mut().set_viewport_scroll(blitz_dom::Point {
        x: 0.0,
        y: f64::from(start_row) / scale.render,
    });
    let buffer = render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| {
            scene.fill(
                Fill::NonZero,
                Default::default(),
                background,
                Default::default(),
                &Rect::new(0.0, 0.0, f64::from(width_phys), f64::from(rows)),
            );
            paint_scene(scene, doc.as_mut(), scale.render, width_phys, rows, 0, 0);
            paint_image_selection(scene, doc, selected_images, scale.render, start_row);
        },
        width_phys,
        rows,
    );
    doc.as_mut().set_viewport_scroll(blitz_dom::Point::ZERO);

    Ok((
        tile_ix as usize,
        tile_image(buffer, width_phys, rows)?,
        rows as f32 / scale.display as f32,
    ))
}

/// Highlight for images included in the rich selection. Blitz already paints
/// selected text; this layer exposes the non-text portion of the same drag,
/// including an image-only selection.
pub(super) fn paint_image_selection(
    scene: &mut impl anyrender::PaintScene,
    doc: &HtmlDocument,
    selected_images: &[usize],
    scale: f64,
    tile_start_row: u32,
) {
    let color = Color::from_rgba8(59, 130, 246, 88);
    for id in selected_images {
        let Some(node) = doc.get_node(*id) else {
            continue;
        };
        let position = node.absolute_position(0.0, 0.0);
        let layout = node.final_layout;
        let x = f64::from(position.x) * scale;
        let y = f64::from(position.y) * scale - f64::from(tile_start_row);
        let width = f64::from(layout.size.width) * scale;
        let height = f64::from(layout.size.height) * scale;
        if width <= 0.0 || height <= 0.0 {
            continue;
        }
        scene.fill(
            Fill::NonZero,
            Default::default(),
            color,
            Default::default(),
            &Rect::new(x, y, x + width, y + height),
        );
    }
}

/// Buffer RGBA → `RenderImage` BGRA (l'ordre attendu par gpui).
pub(super) fn tile_image(
    mut buffer: Vec<u8>,
    width_phys: u32,
    rows: u32,
) -> Result<Arc<RenderImage>, String> {
    for pixel in buffer.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let tile = image::RgbaImage::from_raw(width_phys, rows, buffer)
        .ok_or_else(|| "invalid render buffer".to_string())?;
    Ok(Arc::new(RenderImage::new(SmallVec::from_elem(
        image::Frame::new(tile),
        1,
    ))))
}
