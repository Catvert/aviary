//! Tests for the faithful reader mode.
//!
//! Kept in its own file: the parent module is the largest in the project and
//! half of it was test code. `use super::*` reaches everything, private items
//! included, so these are the same tests in the same module path.

use super::actor::*;
use super::bands::*;
use super::element::*;
use super::events::*;
use super::paint::*;
use super::*;
use blitz_traits::navigation::{DummyNavigationProvider, NavigationOptions};

const LIGHT_THEME: MailTheme = MailTheme {
    dark: false,
    background: 0xffffffff,
    foreground: 0x111111ff,
    border: 0xd4d4d4ff,
    link: 0x0000eeff,
};
const DARK_THEME: MailTheme = MailTheme {
    dark: true,
    background: 0x282c34ff,
    foreground: 0xabb2bfff,
    border: 0x323740ff,
    link: 0x61afefff,
};
const TEST_SCALE: RasterScale = RasterScale {
    render: 1.0,
    display: 1.0,
};

fn render_test_doc(html: &str) -> (Rendered, HtmlDocument, u32) {
    render_test_doc_with_theme(html, LIGHT_THEME)
}

fn render_test_doc_with_theme(html: &str, theme: MailTheme) -> (Rendered, HtmlDocument, u32) {
    render_test_doc_at_width(html, theme, 600)
}

fn render_test_doc_at_width(
    html: &str,
    theme: MailTheme,
    width: u32,
) -> (Rendered, HtmlDocument, u32) {
    render_test_doc_at_width_with_height(html, theme, width, false)
}

fn render_test_fragment(html: &str) -> (Rendered, HtmlDocument, u32) {
    render_test_doc_at_width_with_height(html, LIGHT_THEME, 600, true)
}

fn render_test_doc_at_width_with_height(
    html: &str,
    theme: MailTheme,
    width: u32,
    intrinsic_height: bool,
) -> (Rendered, HtmlDocument, u32) {
    let job = Job {
        html: html.into(),
        images: Vec::new().into(),
        allow_remote: false,
        force_uniform_font_family: false,
        force_uniform_font_size: false,
        uniform_font_size: 14.0,
        theme,
        intrinsic_height,
    };
    let render = render_doc(
        &job,
        width as f32,
        width,
        TEST_SCALE,
        Arc::new(DummyNavigationProvider),
        &RenderCancellation::default(),
        FULL_RANGE,
    )
    .expect("Blitz render");
    (render.rendered, render.doc, render.render_h)
}

fn test_body_options(
    force_uniform_font_family: bool,
    force_uniform_font_size: bool,
    font_size: f32,
) -> MailBodyOptions {
    MailBodyOptions {
        show_remote_images: false,
        force_uniform_font_family,
        force_uniform_font_size,
        force_light_theme: false,
        font_size,
    }
}

/// Logical range covering any document: tests paint every band.
const FULL_RANGE: (f32, f32) = (0.0, f32::MAX);

fn tile_bytes(r: &Rendered, ix: usize) -> &[u8] {
    r.tiles[ix]
        .image
        .as_ref()
        .expect("painted tile")
        .as_bytes(0)
        .expect("frame 0")
}

#[test]
fn cache_separates_two_instances_of_the_same_html() {
    let html = "<p>Quoted message</p>";
    let reader = cache_key(
        "message:42",
        html,
        &[],
        test_body_options(false, false, 14.0),
        LIGHT_THEME,
    );
    let quote = cache_key(
        "editor:7:quote:42",
        html,
        &[],
        test_body_options(false, false, 14.0),
        LIGHT_THEME,
    );
    let uniform_family = cache_key(
        "message:42",
        html,
        &[],
        test_body_options(true, false, 14.0),
        LIGHT_THEME,
    );
    let uniform_size = cache_key(
        "message:42",
        html,
        &[],
        test_body_options(false, true, 14.0),
        LIGHT_THEME,
    );
    let larger = cache_key(
        "message:42",
        html,
        &[],
        test_body_options(false, true, 18.0),
        LIGHT_THEME,
    );
    let dark = cache_key(
        "message:42",
        html,
        &[],
        test_body_options(false, false, 14.0),
        DARK_THEME,
    );

    assert_ne!(reader, quote);
    assert_ne!(reader, uniform_family);
    assert_ne!(reader, uniform_size);
    assert_ne!(reader, dark, "changing the email theme must rerender");
    assert_ne!(uniform_size, larger);
    assert_eq!(
        reader,
        cache_key(
            "message:42",
            html,
            &[],
            test_body_options(false, false, 18.0),
            LIGHT_THEME,
        ),
        "size should not affect the cache while size normalization is disabled"
    );
    assert_eq!(
        uniform_family,
        cache_key(
            "message:42",
            html,
            &[],
            test_body_options(true, false, 18.0),
            LIGHT_THEME,
        ),
        "size should not affect a family-only override"
    );
}

#[test]
fn repairs_encoded_dynamics_footer_fragments() {
    let html = r#"<html><body><div id="footer">
        <span class="msdynmkt_personalization">&lt;table id=&quot;footer-table&quot;&gt;&lt;tbody&gt;&lt;tr&gt;&lt;td&gt;</span>
        <table id="real-links"><tbody><tr><td>Phishing link</td></tr></tbody></table>
        <span class="other">Ordinary span</span>
        <span class="msdynmkt_personalization">&lt;/td&gt;&lt;/tr&gt;&lt;tr&gt;&lt;td id=&quot;copyright&quot;&gt;&amp;copy; 2026 Organisation de test&lt;/td&gt;&lt;/tr&gt;&lt;/tbody&gt;&lt;/table&gt;</span>
    </div></body></html>"#;

    let repaired = repair_outlook_html(html);
    let document = scraper::Html::parse_document(&repaired);
    let select = |css: &str| scraper::Selector::parse(css).expect("test selector");

    assert_eq!(document.select(&select("#footer-table")).count(), 1);
    assert_eq!(
        document
            .select(&select("#footer-table #real-links"))
            .count(),
        1,
        "the real links table should be nested in the decoded scaffold: {repaired}"
    );
    assert_eq!(
        document
            .select(&select("#copyright"))
            .flat_map(|element| element.text())
            .collect::<String>()
            .trim(),
        "© 2026 Organisation de test"
    );
    assert!(repaired.contains("Ordinary span"));
    assert!(
        !document
            .root_element()
            .text()
            .any(|text| text.contains("<table id=")),
        "structural markup must no longer be painted as text: {repaired}"
    );
}

#[test]
fn removes_tracking_pixels_but_keeps_visible_mail_images() {
    let html = r#"<html><body>
        <img id="one-pixel" src="https://tracker.test/open" width="1" height="1">
        <img id="zero-pixel" src="https://tracker.test/zero" width="0" height="0" data-tracking="">
        <img id="score" src="https://assets.test/score.jpg" width="45" height="43">
    </body></html>"#;

    let repaired = repair_outlook_html(html);
    let document = scraper::Html::parse_document(&repaired);
    let select = |css: &str| scraper::Selector::parse(css).expect("test selector");

    assert_eq!(document.select(&select("#one-pixel")).count(), 0);
    assert_eq!(document.select(&select("#zero-pixel")).count(), 0);
    assert_eq!(document.select(&select("#score")).count(), 1);
}

#[test]
fn repairs_outlook_fragments_around_cid_images() {
    let html = r#"<html><body>
        <div dir="auto">
            Introduction
            <blockquote type="cite"><table><tr><td>
                <div style="overflow:hidden"></div>
            </td></tr></table></blockquote>
        </div>
        <div><img src="cid:header"></div>
        <div role="textbox" aria-label="Message body">
            <blockquote type="cite"><table><tr><td>Hello</td></tr><tr><td>
                <div id="footer-host"><a href="https://example.test" title="Organisation de test"></a></div>
            </td></tr></table></blockquote>
        </div>
        <div><img src="cid:logo"></div>
        <div role="textbox" aria-label="Message body">
            <blockquote type="cite"><table><tr><td>
                <a href="https://example.test" title="Organisation de test"></a>
                <a href="https://social-a.example.test" title="Réseau de test A"></a>
            </td></tr></table></blockquote>
        </div>
        <div><img src="cid:social-a"></div>
        <div role="textbox" aria-label="Message body">
            <blockquote type="cite"><table><tr><td>
                <a href="https://social-a.example.test" title="Réseau de test A"></a>
                <a href="https://social-b.example.test" title="Réseau de test B"></a>
            </td></tr></table></blockquote>
        </div>
        <div><img src="cid:social-b"></div>
        <div role="textbox" aria-label="Message body">
            <blockquote type="cite"><div dir="ltr">
                <table><tr><td>
                    <a href="https://social-b.example.test" title="Réseau de test B"></a>
                </td></tr></table>
                <div>&lt;cmn-mailsimple-logo-example.jpg&gt;</div>
                <div>&lt;cmn-mailsimple-icon-social-a.png&gt;</div>
                <div>&lt;cmn-mailsimple-icon-social-b.png&gt;</div>
            </div></blockquote>
        </div>
    </body></html>"#;

    let repaired = repair_fragmented_outlook_cids(html);
    let document = scraper::Html::parse_document(&repaired);
    let select = |css: &str| scraper::Selector::parse(css).expect("test selector");

    assert_eq!(
        document
            .select(&select(r#"body > div > img[src^="cid:"]"#))
            .count(),
        0,
        "images should no longer remain in sibling divs: {repaired}"
    );
    assert_eq!(
        document
            .select(&select(r#"div[style*="overflow"] > img[src="cid:header"]"#))
            .count(),
        1
    );
    assert_eq!(
        document.select(&select("#footer-host > a > img")).count(),
        3,
        "logo and icons should share the footer: {repaired}"
    );
    assert_eq!(
        document.select(&select(r#"div[role="textbox"]"#)).count(),
        2,
        "textless fragments should be merged"
    );
    assert_eq!(
        document
            .select(&select(r#"div[role="textbox"] table"#))
            .count(),
        1,
        "the empty table in the final fragment should disappear"
    );
    assert!(!repaired.contains("cmn-mailsimple"));
}

#[test]
fn does_not_repair_one_adjacent_cid() {
    let html = r#"<html><body>
        <div role="textbox"><a href="https://example.test"></a></div>
        <div><img src="cid:hero"></div>
    </body></html>"#;
    assert_eq!(repair_fragmented_outlook_cids(html), html);
}

#[test]
fn only_the_latest_render_target_is_current() {
    let mut entry = Entry::default();
    let first = entry.request_target(600.0, 1.0, 1.0).expect("first target");
    let first_cancellation = entry
        .render_cancellation
        .clone()
        .expect("first cancellation");
    assert_eq!(entry.request_target(600.0, 1.0, 1.0), None);

    let latest = entry
        .request_target(601.0, 1.0, 1.0)
        .expect("resized target");
    assert!(first_cancellation.is_cancelled());
    assert_ne!(first, latest);
    assert_ne!(entry.target_generation, first);
    assert_eq!(entry.target_generation, latest);

    let zoomed = entry
        .request_target(601.0, 1.0, 1.25)
        .expect("zoomed target");
    assert_ne!(zoomed, latest, "zoom should trigger a new render");
}

#[test]
fn live_document_is_reused_only_for_zoom_changes() {
    let mut entry = Entry::default();
    let (tx, _rx) = mpsc::channel();
    entry.live = Some(Arc::new(LiveDoc { tx }));
    entry.live_target = Some((600.0, 1.25, 1.0));
    entry.rendered = Some(Arc::new(Rendered {
        width: 600.0,
        display: 1.0,
        tiles: Vec::new(),
        truncated: false,
        resources_pending: false,
    }));

    assert!(entry.can_rerender_live(600.0, 1.25));
    assert!(!entry.can_rerender_live(601.0, 1.25));
    assert!(!entry.can_rerender_live(600.0, 1.5));
}

#[test]
fn wheel_zoom_is_bounded_and_rounded() {
    assert_eq!(next_zoom(1.0, 0.1), 1.1);
    assert_eq!(next_zoom(1.0, -0.1), 0.9);
    assert_eq!(next_zoom(ZOOM_MAX, 0.5), ZOOM_MAX);
    assert_eq!(next_zoom(ZOOM_MIN, -0.5), ZOOM_MIN);
}

#[test]
fn zoom_badge_tracks_latest_change_and_reset() {
    let mut entry = Entry::default();
    let first = entry.update_zoom(1.25).expect("first zoom");
    let latest = entry.update_zoom(1.5).expect("latest zoom");

    assert_eq!(zoom_label(entry.zoom), "150 %");
    assert!(entry.zoom_badge_visible);
    assert!(!entry.hide_zoom_badge(first), "stale timer is ignored");
    assert!(entry.zoom_badge_visible);

    let reset = entry.update_zoom(1.0).expect("reset zoom");
    assert_eq!(zoom_label(entry.zoom), "100 %");
    assert!(!entry.hide_zoom_badge(latest), "pre-reset timer is ignored");
    assert!(entry.hide_zoom_badge(reset));
    assert!(!entry.zoom_badge_visible);
}

#[test]
fn navigation_cancels_only_reader_renders() {
    let mut cache = BlitzCache::default();

    let reader = cache.entry_mut("reader");
    reader.reader = true;
    reader.rendered = Some(Arc::new(Rendered {
        width: 600.0,
        display: 1.0,
        tiles: Vec::new(),
        truncated: false,
        resources_pending: false,
    }));
    reader
        .request_target(601.0, 1.0, 1.0)
        .expect("reader target");
    let reader_cancellation = reader
        .render_cancellation
        .clone()
        .expect("reader cancellation");

    let quote = cache.entry_mut("quote");
    quote.request_target(600.0, 1.0, 1.0).expect("quote target");
    let quote_cancellation = quote
        .render_cancellation
        .clone()
        .expect("quote cancellation");

    cache.cancel_pending_readers_except(None);

    let reader = cache.entries.get("reader").expect("reader cached");
    assert!(reader_cancellation.is_cancelled());
    assert!(!reader.in_flight);
    assert!(reader.rendered.is_some(), "finished tiles stay cached");
    assert_eq!(reader.last_target, None, "the target can be retried");

    let quote = cache.entries.get("quote").expect("quote cached");
    assert!(!quote_cancellation.is_cancelled());
    assert!(quote.in_flight, "editor quotes use an independent slot");
}

fn drag_batch(y_from: f32, y_to: f32) -> Vec<DocOp> {
    let mods = gpui::Modifiers::default();
    vec![
        DocOp::Ui(UiEvent::PointerDown(blitz_pointer(
            10.0,
            y_from,
            MouseEventButtons::Primary,
            &mods,
        ))),
        DocOp::Ui(UiEvent::PointerMove(blitz_pointer(
            140.0,
            y_to,
            MouseEventButtons::Primary,
            &mods,
        ))),
        DocOp::Ui(UiEvent::PointerUp(blitz_pointer(
            140.0,
            y_to,
            MouseEventButtons::empty(),
            &mods,
        ))),
    ]
}

/// Smoke test for the entire pipeline (Stylo -> Taffy -> parley ->
/// vello_cpu) on a document with text and a table, a common email shape.
/// Verifies that tiles and non-white pixels are produced.
#[test]
fn renders_a_simple_document() {
    let (r, _doc, _h) = render_test_doc(
        r#"<html><body>
            <h1 style="color:#123456">Title</h1>
            <table border="1"><tr><td>Left</td><td>Right</td></tr></table>
        </body></html>"#,
    );
    assert_eq!(r.width, 600.0);
    assert!(!r.tiles.is_empty());
    let bytes = tile_bytes(&r, 0);
    assert!(
        bytes
            .chunks_exact(4)
            .any(|p| p[0] != 255 || p[1] != 255 || p[2] != 255),
        "render should not be entirely white"
    );
}

#[test]
fn html_fragment_height_tracks_signature_content() {
    let (_rendered, doc, render_height) = render_test_fragment(
        r#"<html><body style="margin:0">
            <table id="signature" cellspacing="0" cellpadding="0">
                <tr><td style="height:48px">Organisation de test</td></tr>
            </table>
        </body></html>"#,
    );
    let signature = doc
        .as_ref()
        .query_selector("#signature")
        .expect("selector")
        .expect("signature present");
    let signature = doc.as_ref().get_node(signature).expect("signature node");
    let signature_bottom =
        signature.absolute_position(0.0, 0.0).y + signature.final_layout.size.height;

    assert!(
        render_height < 200,
        "an embedded signature must not fill the 800px browser viewport: {render_height}"
    );
    assert!(
        render_height as f32 >= signature_bottom,
        "the intrinsic canvas clips the signature: {render_height} < {signature_bottom}"
    );
}

#[test]
fn outlook_split_fragment_does_not_reserve_a_browser_viewport() {
    // A split occurs before Outlook's transport header, so the current
    // prefix can end with still-open document wrappers. html5ever repairs
    // those wrappers; fragment sizing must then follow the visible content.
    let html = r#"<html><head><style>
            p.MsoNormal { margin:0; font:11pt Aptos,sans-serif }
        </style></head><body><div class="WordSection1">
            <p class="MsoNormal">Message actuel synthétique.</p>
            <table id="signature" width="640" style="border-collapse:collapse">
                <tr><td style="height:48px">Organisation de test</td></tr>
                <tr><td><div id="banner" style="height:120px">Bannière de test</div></td></tr>
            </table>
            <p class="MsoNormal">&nbsp;</p><div>"#;

    let (_full, _full_doc, full_height) = render_test_doc(html);
    let (_fragment, fragment_doc, fragment_height) = render_test_fragment(html);
    let banner = fragment_doc
        .as_ref()
        .query_selector("#banner")
        .expect("selector")
        .expect("banner present");
    let banner = fragment_doc.as_ref().get_node(banner).expect("banner node");
    let banner_bottom = banner.absolute_position(0.0, 0.0).y + banner.final_layout.size.height;

    assert!(
        full_height >= 800,
        "a standalone reader document keeps its browser viewport: {full_height}"
    );
    assert!(
        fragment_height < 400,
        "the split fragment kept a large blank viewport: {fragment_height}"
    );
    assert!(
        fragment_height as f32 >= banner_bottom,
        "fragment sizing clips the last visible block: {fragment_height} < {banner_bottom}"
    );
}

#[test]
fn zoom_reduces_css_viewport_and_reflows_document() {
    let job = Job {
        html: format!(
            r#"<html><body style="margin:0;font-size:18px"><p>{}</p></body></html>"#,
            "A long sentence intended to verify line wrapping. ".repeat(20)
        )
        .into(),
        images: Vec::new().into(),
        allow_remote: false,
        force_uniform_font_family: false,
        force_uniform_font_size: false,
        uniform_font_size: 14.0,
        theme: LIGHT_THEME,
        intrinsic_height: false,
    };
    let cancellation = RenderCancellation::default();
    let DocRender {
        rendered: normal,
        doc: normal_doc,
        ..
    } = render_doc(
        &job,
        600.0,
        600,
        TEST_SCALE,
        Arc::new(DummyNavigationProvider),
        &cancellation,
        FULL_RANGE,
    )
    .expect("normal render");
    let DocRender {
        rendered: zoomed,
        doc: zoomed_doc,
        ..
    } = render_doc(
        &job,
        600.0,
        600,
        RasterScale {
            render: 1.5,
            display: 1.0,
        },
        Arc::new(DummyNavigationProvider),
        &cancellation,
        FULL_RANGE,
    )
    .expect("zoomed render");

    let normal_css_width = normal_doc.as_ref().root_element().final_layout.size.width;
    let zoomed_css_width = zoomed_doc.as_ref().root_element().final_layout.size.width;
    let normal_height: f32 = normal.tiles.iter().map(|tile| tile.height).sum();
    let zoomed_height: f32 = zoomed.tiles.iter().map(|tile| tile.height).sum();

    assert_eq!(
        normal.width, zoomed.width,
        "the panel should not become wider"
    );
    assert!(
        zoomed_css_width < normal_css_width,
        "Blitz should lay out HTML in a narrower CSS viewport"
    );
    assert!(
        zoomed_height > normal_height,
        "the zoomed document should reflow and become taller"
    );
}

/// A late-resource repaint must reproduce the geometry of the paint it
/// replaces. `width` in particular is what the UI compares against its
/// measured width: any drift there reads as "the width changed" and would
/// schedule a full re-render on every poll of the resource pump.
#[test]
fn late_resource_repaint_keeps_the_render_geometry() {
    let (initial, mut doc, initial_h) = render_test_doc(
        "<html><body><p>A body whose images are still downloading.</p></body></html>",
    );

    let (repainted, repainted_h) = apply_resources_doc(
        &mut doc,
        600.0,
        600,
        TEST_SCALE,
        LIGHT_THEME.background_color(),
        FULL_RANGE,
        false,
    )
    .expect("late resource repaint");

    assert_eq!(repainted.width, initial.width);
    assert_eq!(repainted.display, initial.display);
    assert_eq!(repainted_h, initial_h);
    assert_eq!(repainted.tiles.len(), initial.tiles.len());
    // The pump stops on this flag; a repaint reporting stale work would
    // keep it polling for the rest of its budget.
    assert!(!repainted.resources_pending);
}

#[test]
fn reused_zoom_matches_a_fresh_document() {
    let job = Job {
        html: format!(
            r#"<html><head><style>
                body {{ margin: 0; }}
                #responsive {{ height: 20px; background: #123456; }}
                @media (max-width: 450px) {{
                    #responsive {{ height: 40px; background: #654321; }}
                }}
            </style></head><body>
                <div id="responsive"></div><p>{}</p>
            </body></html>"#,
            "A long sentence that must reflow at the zoomed width. ".repeat(20)
        )
        .into(),
        images: Vec::new().into(),
        allow_remote: false,
        force_uniform_font_family: false,
        force_uniform_font_size: false,
        uniform_font_size: 14.0,
        theme: LIGHT_THEME,
        intrinsic_height: false,
    };
    let cancellation = RenderCancellation::default();
    let DocRender {
        doc: mut reused_doc,
        ..
    } = render_doc(
        &job,
        600.0,
        600,
        TEST_SCALE,
        Arc::new(DummyNavigationProvider),
        &cancellation,
        FULL_RANGE,
    )
    .expect("normal render");

    let (reused, reused_h, reused_width, reused_scale) = rerender_doc(
        &mut reused_doc,
        (600.0, 1.0, 1.5),
        LIGHT_THEME.background_color(),
        &cancellation,
        FULL_RANGE,
        false,
    )
    .expect("reused zoom render");
    let DocRender {
        rendered: fresh,
        doc: fresh_doc,
        render_h: fresh_h,
        ..
    } = render_doc(
        &job,
        600.0,
        600,
        RasterScale {
            render: 1.5,
            display: 1.0,
        },
        Arc::new(DummyNavigationProvider),
        &cancellation,
        FULL_RANGE,
    )
    .expect("fresh zoom render");

    assert_eq!(reused_width, 600);
    assert_eq!(reused_scale.render, 1.5);
    assert_eq!(reused_h, fresh_h);
    assert_eq!(
        reused_doc.as_ref().root_element().final_layout,
        fresh_doc.as_ref().root_element().final_layout,
        "reusing the DOM must produce the same zoomed layout"
    );
    assert_eq!(reused.tiles.len(), fresh.tiles.len());
    for (reused_tile, fresh_tile) in reused.tiles.iter().zip(&fresh.tiles) {
        assert_eq!(reused_tile.height, fresh_tile.height);
        assert_eq!(
            reused_tile
                .image
                .as_ref()
                .expect("painted tile")
                .as_bytes(0),
            fresh_tile.image.as_ref().expect("painted tile").as_bytes(0),
            "reused and fresh zooms must rasterize identically"
        );
    }
}

#[test]
fn actor_remains_live_after_reused_zoom() {
    let job = Arc::new(Job {
        html: "<html><body><p>Reusable actor</p></body></html>".into(),
        images: Vec::new().into(),
        allow_remote: false,
        force_uniform_font_family: false,
        force_uniform_font_size: false,
        uniform_font_size: 14.0,
        theme: LIGHT_THEME,
        intrinsic_height: false,
    });
    let cancellation = Arc::new(RenderCancellation::default());
    let initial = futures::executor::block_on(spawn_doc_thread(
        job,
        600.0,
        1.0,
        1.0,
        FULL_RANGE,
        cancellation,
        false,
    ))
    .expect("actor response")
    .expect("initial actor render");
    let (_rendered, live) = initial;

    let cancelled = Arc::new(RenderCancellation::default());
    cancelled.cancel();
    let (cancelled_tx, cancelled_rx) = oneshot::channel();
    live.tx
        .send(DocCmd::Rerender {
            logical_width: 600.0,
            device_scale: 1.0,
            zoom: 1.25,
            visible: FULL_RANGE,
            cancellation: cancelled,
            out: cancelled_tx,
        })
        .expect("send cancelled zoom");
    let cancelled_result =
        futures::executor::block_on(cancelled_rx).expect("cancelled actor response");
    let Err(cancelled_error) = cancelled_result else {
        panic!("cancelled zoom must be skipped");
    };
    assert_eq!(cancelled_error, render_cancelled_error());

    let (zoom_tx, zoom_rx) = oneshot::channel();
    live.tx
        .send(DocCmd::Rerender {
            logical_width: 600.0,
            device_scale: 1.0,
            zoom: 1.5,
            visible: FULL_RANGE,
            cancellation: Arc::new(RenderCancellation::default()),
            out: zoom_tx,
        })
        .expect("send reused zoom");
    let zoomed = futures::executor::block_on(zoom_rx)
        .expect("zoom actor response")
        .expect("reused zoom render");
    assert!(!zoomed.tiles.is_empty());

    let (batch_tx, batch_rx) = oneshot::channel();
    live.tx
        .send(DocCmd::Batch(Vec::new(), batch_tx))
        .expect("actor accepts a post-zoom batch");
    let (actor_target, _outcome) =
        futures::executor::block_on(batch_rx).expect("batch actor response");
    assert!(render_targets_match(actor_target, (600.0, 1.0, 1.5)));

    let (selection_tx, selection_rx) = oneshot::channel();
    live.tx
        .send(DocCmd::SelectedContent(selection_tx))
        .expect("actor still accepts commands");
    assert!(
        futures::executor::block_on(selection_rx)
            .expect("selection actor response")
            .is_none(),
        "no selection was made"
    );
}

#[test]
fn zero_border_does_not_draw_email_tables() {
    let (rendered, doc, _height) = render_test_doc(
        r#"<html><body style="margin:0">
            <table border="0" style="border-collapse:collapse;width:100px;height:40px">
                <tr><td></td><td></td></tr>
            </table>
        </body></html>"#,
    );

    for selector in ["table", "td"] {
        let node = doc
            .as_ref()
            .query_selector(selector)
            .expect("selector")
            .unwrap_or_else(|| panic!("{selector} present"));
        let border = doc.as_ref().get_node(node).unwrap().final_layout.border;
        assert_eq!(
            (border.top, border.right, border.bottom, border.left),
            (0.0, 0.0, 0.0, 0.0),
            "border=0 must suppress the UA border on {selector}"
        );
    }

    let pixels = tile_bytes(&rendered, 0);
    assert!(
        pixels
            .chunks_exact(4)
            .all(|pixel| pixel == [0xff, 0xff, 0xff, 0xff]),
        "a collapsed border=0 table must not paint black grid lines"
    );
}

#[test]
fn nonzero_border_remains_visible_on_collapsed_table() {
    let (rendered, _doc, _height) = render_test_doc(
        r#"<html><body style="margin:0">
            <table border="1" style="border-collapse:collapse;width:100px;height:40px">
                <tr><td></td><td></td></tr>
            </table>
        </body></html>"#,
    );

    let pixels = tile_bytes(&rendered, 0);
    assert!(
        pixels
            .chunks_exact(4)
            .any(|pixel| pixel != [0xff, 0xff, 0xff, 0xff]),
        "a real collapsed table border must remain visible"
    );
}

#[test]
fn physical_width_matches_gpui_rounding() {
    assert_eq!(physical_width(800.0, 1.0), 800);
    assert_eq!(physical_width(801.0, 1.25), 1002);
    assert_eq!(physical_width(801.0, 1.5), 1202);
}

#[test]
fn panel_width_unblocks_initial_zero_width_layout() {
    assert_eq!(resolve_render_width(0.0, Some(1042.0)), Some(1042.0));
    assert_eq!(
        resolve_render_width(900.0, Some(1042.0)),
        Some(900.0),
        "direct measurement regains priority once available"
    );
    assert_eq!(resolve_render_width(0.0, None), None);
}

#[test]
fn render_remains_visible_at_panel_widths() {
    let html = r#"<html><body style="margin:0">
        <div style="height:40px;background:#123456"></div>
        <table width="900"><tr><td>Contenu adaptable</td></tr></table>
    </body></html>"#;

    for width in [240, 319, 480, 719, 960] {
        let (rendered, _doc, _h) = render_test_doc_at_width(html, LIGHT_THEME, width);
        assert_eq!(rendered.width, width as f32);
        assert_eq!(
            &tile_bytes(&rendered, 0)[..4],
            &[0x56, 0x34, 0x12, 0xff],
            "document blank at width {width}"
        );
    }
}

#[test]
fn blitz_context_loads_embedded_fonts() {
    let mut ctx = blitz_font_context();
    let inter = ctx
        .collection
        .family_id("Inter")
        .expect("Inter must be registered in Fontique");
    let mono = ctx
        .collection
        .family_id("JetBrains Mono")
        .expect("JetBrains Mono must be registered in Fontique");
    let emoji = ctx
        .collection
        .family_id("Noto Color Emoji")
        .expect("Noto Color Emoji must be registered in Fontique");

    assert_eq!(
        ctx.collection
            .generic_families(GenericFamily::SansSerif)
            .next(),
        Some(inter)
    );
    assert_eq!(
        ctx.collection
            .generic_families(GenericFamily::Monospace)
            .next(),
        Some(mono)
    );
    assert_eq!(
        ctx.collection.generic_families(GenericFamily::Emoji).next(),
        Some(emoji)
    );
}

#[test]
fn renders_embedded_emoji_with_color_bitmap_glyphs() {
    let (rendered, _doc, _height) = render_test_doc(
        r#"<html><body style="margin:0;font-size:64px;line-height:1">😊</body></html>"#,
    );
    let pixels = tile_bytes(&rendered, 0);
    let colored_pixels = pixels
        .chunks_exact(4)
        .filter(|pixel| {
            pixel[3] != 0 && (pixel[0] != pixel[1] || pixel[1] != pixel[2] || pixel[0] != pixel[2])
        })
        .count();
    let non_white_pixels = pixels
        .chunks_exact(4)
        .filter(|pixel| **pixel != [0xff, 0xff, 0xff, 0xff])
        .count();
    assert!(
        colored_pixels > 100,
        "the smile must contain colored CBDT pixels, not a monochrome outline; \
         colored={colored_pixels}, non-white={non_white_pixels}"
    );
}

#[test]
fn font_family_and_size_normalization_are_independent() {
    fn typography(doc: &HtmlDocument) -> (String, f32) {
        let span = doc
            .as_ref()
            .query_selector("span")
            .expect("selector")
            .expect("span present");
        let styles = doc
            .as_ref()
            .get_node(span)
            .expect("span node")
            .primary_styles()
            .expect("computed styles");
        (
            format!("{:?}", styles.get_font().font_family.families),
            styles.get_font().font_size.computed_size.0.px(),
        )
    }

    let html = r#"<html><body><span style="font-family: 'Courier New' !important; font-size: 30px !important">
        Texte entrant
    </span></body></html>"#;
    let (_rendered, faithful, _height) = render_test_doc(html);
    let normalized = |force_uniform_font_family, force_uniform_font_size| {
        let job = Job {
            html: html.into(),
            images: Vec::new().into(),
            allow_remote: false,
            force_uniform_font_family,
            force_uniform_font_size,
            uniform_font_size: 14.0,
            theme: LIGHT_THEME,
            intrinsic_height: false,
        };
        let DocRender { doc, .. } = render_doc(
            &job,
            600.0,
            600,
            TEST_SCALE,
            Arc::new(DummyNavigationProvider),
            &RenderCancellation::default(),
            FULL_RANGE,
        )
        .expect("normalized Blitz render");
        doc
    };

    let faithful = typography(&faithful);
    let family_only = typography(&normalized(true, false));
    let size_only = typography(&normalized(false, true));
    let both = typography(&normalized(true, true));
    assert!(faithful.0.contains("Courier New"));
    assert!((faithful.1 - 30.0).abs() < 0.01);
    assert!(family_only.0.contains("Inter"));
    assert!((family_only.1 - 30.0).abs() < 0.01);
    assert!(size_only.0.contains("Courier New"));
    assert!((size_only.1 - 14.0).abs() < 0.01);
    assert!(both.0.contains("Inter"));
    assert!((both.1 - 14.0).abs() < 0.01);
}

#[test]
fn dark_render_uses_theme_background() {
    let (r, _doc, _h) = render_test_doc_with_theme(
        "<html><body><p>Message without forced colors</p></body></html>",
        DARK_THEME,
    );
    let bytes = tile_bytes(&r, 0);
    assert_eq!(
        &bytes[..4],
        &[0x34, 0x2c, 0x28, 0xff],
        "the BGRA canvas should use #282c34"
    );
}

#[test]
fn render_passes_prefers_color_scheme_dark() {
    let html = r#"<html><head><style>
        @media (prefers-color-scheme: dark) {
            html, body { background-color: #102030; }
        }
    </style></head><body><p>Dark mode</p></body></html>"#;
    let (r, _doc, _h) = render_test_doc_with_theme(html, DARK_THEME);
    let bytes = tile_bytes(&r, 0);
    assert_eq!(
        &bytes[..4],
        &[0x30, 0x20, 0x10, 0xff],
        "the email dark media query must be active"
    );
}

#[test]
fn blocked_remote_stylesheet_does_not_stall_document() {
    let stylesheets = (0..32)
        .map(|index| {
            format!(r#"<link rel="stylesheet" href="https://example.invalid/mail-{index}.css">"#)
        })
        .collect::<String>();
    let html = format!(
        r#"<html><head>{stylesheets}</head><body style="margin:0">
            <div style="width:40px;height:40px;background:#123456"></div>
        </body></html>"#
    );
    let started = Instant::now();
    let (r, _doc, _h) = render_test_doc(&html);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a burst of blocked stylesheets must not consume the network timeout"
    );
    assert_eq!(
        &tile_bytes(&r, 0)[..4],
        &[0x56, 0x34, 0x12, 0xff],
        "the document must still be painted"
    );
}

#[test]
fn ua_stylesheet_uses_theme_palette() {
    let css = DARK_THEME.css();
    assert!(css.contains("color-scheme: dark"));
    assert!(css.contains("color: #abb2bf"));
    assert!(css.contains("color: #61afef"));
    assert!(css.contains("border-color: #323740"));
    assert!(css.contains("[color=\"black\" i]"));
    assert!(css.contains("color: #abb2bf !important"));
}

#[test]
fn forced_light_palette_is_browser_like() {
    let theme = MailTheme::forced_light();
    assert!(!theme.dark);
    assert_eq!(theme.background, 0xffffffff);
    assert_eq!(theme.foreground, 0x000000ff);
    assert_eq!(theme.link, 0x0563c1ff);
    assert!(theme.css().contains("color-scheme: light"));
}

#[test]
fn dark_render_fixes_inline_outlook_grays_without_overriding_brand() {
    fn first_pixel(html: &str) -> [u8; 4] {
        let (r, _doc, _h) = render_test_doc_with_theme(html, DARK_THEME);
        let bytes = tile_bytes(&r, 0);
        bytes[..4].try_into().expect("pixel BGRA")
    }

    let black = first_pixel(
        r#"<html><body style="margin:0"><span
            style="display:block;width:40px;height:40px;color:black;background-color:currentColor"
        ></span></body></html>"#,
    );
    assert_eq!(
        black,
        [0xbf, 0xb2, 0xab, 0xff],
        "color:black should become the OneDark #abb2bf foreground"
    );

    for gray in ["#2a2a2a", "#707070", "rgb(42, 42, 42)"] {
        let pixel = first_pixel(&format!(
            r#"<html><body style="margin:0"><span
                style="display:block;width:40px;height:40px;color:{gray};background-color:currentColor"
            ></span></body></html>"#,
        ));
        assert_eq!(
            pixel,
            [0xbf, 0xb2, 0xab, 0xff],
            "{gray} should become the OneDark #abb2bf foreground"
        );
    }

    let brand = first_pixel(
        r#"<html><body style="margin:0"><span
            style="display:block;width:40px;height:40px;color:#003B49;background-color:currentColor"
        ></span></body></html>"#,
    );
    assert_eq!(
        brand,
        [0x49, 0x3b, 0x00, 0xff],
        "an explicit brand color should remain unchanged"
    );
}

#[test]
fn dark_adaptation_preserves_dark_backgrounds_and_light_mode() {
    let html = r#"<p style="color:#2a2a2a;background-color:#2a2a2a">
        Text <span style="color:rgba(0,0,0,0)">hidden</span>
    </p>"#;
    let dark = adapt_dark_colors(html, DARK_THEME);
    assert!(dark.contains("color:#abb2bf"));
    assert!(dark.contains("background-color:#2a2a2a"));
    assert!(dark.contains("color:rgba(0,0,0,0)"));
    assert_eq!(adapt_dark_colors(html, LIGHT_THEME), html);
}

#[test]
fn dark_adaptation_converts_white_email_template_backgrounds() {
    let html = r##"<html><body bgcolor="#ffffff"
        style="background-color:#ffffff;color:#919191">
        <style>
            table { background: #fff; }
            .button { background:#EF2637; color:#ffffff; }
        </style>
        <table bgcolor="white"><tr>
            <td style="background-color:rgb(245, 245, 245);color:#404040">Text</td>
            <td class="button" bgcolor="#EF2637">Button</td>
        </tr></table>
    </body></html>"##;

    let dark = adapt_dark_colors(html, DARK_THEME);
    assert!(dark.contains(r##"bgcolor="#282c34""##));
    assert!(dark.contains("background-color:#282c34;color:#abb2bf"));
    assert!(dark.contains("background: #282c34"));
    assert!(dark.contains(r##"bgcolor="#EF2637""##));
    assert!(dark.contains("background:#EF2637; color:#ffffff"));
}

#[test]
fn dark_render_converts_an_email_white_canvas() {
    let (rendered, _doc, _height) = render_test_doc_with_theme(
        r##"<html><body bgcolor="#ffffff" style="margin:0;background:#fff">
            <div style="width:40px;height:40px;background-color:#ffffff"></div>
        </body></html>"##,
        DARK_THEME,
    );
    let bytes = tile_bytes(&rendered, 0);
    assert_eq!(
        &bytes[..4],
        &[0x34, 0x2c, 0x28, 0xff],
        "white template backgrounds should become #282c34"
    );
}

/// Pointer-event selection crosses the entire pipeline: down -> drag -> up,
/// followed by extraction of selected text.
#[test]
fn selects_text_by_dragging() {
    let (_r, mut doc, render_h) =
        render_test_doc("<html><body><p style=\"font-size:16px\">Hello world</p></body></html>");
    let mut st = PaintState::new(render_h, LIGHT_THEME.background_color());
    let outcome = process_batch(&mut doc, drag_batch(18.0, 18.0), 600, TEST_SCALE, &mut st);
    assert!(outcome.update.is_some(), "batch should rerasterize");
    let text = doc.as_ref().get_selected_text();
    let text = text.expect("a selection should exist");
    assert!(
        text.contains("ello") || text.contains("world"),
        "unexpected selected text: {text:?}"
    );
    let content = selected_content(&doc, &st, &[]).expect("HTML selection");
    assert_eq!(content.text, text);
    assert!(
        content.html.contains(text.trim()),
        "HTML should contain exactly the selected text: {:?}",
        content.html
    );
}

#[test]
fn rich_selection_preserves_html_styles_and_cid_image() {
    let html = r#"<html><body><section class="message" style="color:#123456">
        <p id="before"><strong>Before</strong></p>
        <img id="logo" src="cid:logo-test" width="32" height="24">
        <p id="after"><a href="https://example.test">After</a></p>
    </section></body></html>"#;
    let (_rendered, doc, render_h) = render_test_doc(html);
    let before = doc
        .as_ref()
        .query_selector("#before")
        .expect("selector")
        .expect("paragraph before");
    let after = doc
        .as_ref()
        .query_selector("#after")
        .expect("selector")
        .expect("paragraph after");
    let mut state = PaintState::new(render_h, LIGHT_THEME.background_color());
    state.rich_anchor = Some(before);
    state.rich_focus = Some(after);
    state.rich_dragged = true;

    let inline = InlineImage {
        cid: "logo-test".into(),
        mime: "image/png".into(),
        bytes: vec![0x89, b'P', b'N', b'G'],
    };
    let content =
        selected_content(&doc, &state, std::slice::from_ref(&inline)).expect("rich selection");

    assert!(content.html.contains("<section"));
    assert!(content.html.contains("color:#123456"));
    assert!(content.html.contains("<strong>Before</strong>"));
    assert!(content.html.contains("cid:logo-test"));
    assert!(content.html.contains("https://example.test"));
    assert_eq!(content.images, vec![inline]);
}

#[test]
fn one_image_can_be_selected_and_copied() {
    let (_rendered, doc, render_h) = render_test_doc(
        r#"<html><body><img id="logo" src="cid:logo-test" width="32" height="24"></body></html>"#,
    );
    let logo = doc
        .as_ref()
        .query_selector("#logo")
        .expect("selector")
        .expect("image");
    let mut state = PaintState::new(render_h, LIGHT_THEME.background_color());
    state.rich_anchor = Some(logo);
    state.rich_focus = Some(logo);
    state.rich_dragged = true;

    assert_eq!(selected_image_nodes(&doc, &state), vec![logo]);
    let (html, roots) = selected_html(&doc, logo, logo).expect("image fragment");
    assert_eq!(roots, vec![logo]);
    assert!(html.starts_with("<img"));
    assert!(html.contains("cid:logo-test"));
}

#[test]
fn image_outside_horizontal_drag_path_is_not_copied() {
    let (_rendered, doc, render_h) = render_test_doc(
        r#"<html><body><p id="line">Before <img id="logo" src="cid:logo-test" width="32" height="24"> after</p></body></html>"#,
    );
    let line = doc
        .as_ref()
        .query_selector("#line")
        .expect("selector")
        .expect("line");
    let logo = doc
        .as_ref()
        .query_selector("#logo")
        .expect("selector")
        .expect("image");
    let image = doc.as_ref().get_node(logo).expect("image node");
    let position = image.absolute_position(0.0, 0.0);
    let y = position.y + image.final_layout.size.height / 2.0;

    let mut state = PaintState::new(render_h, LIGHT_THEME.background_color());
    state.rich_anchor = Some(line);
    state.rich_focus = Some(line);
    state.rich_anchor_point = Some((position.x - 50.0, y));
    state.rich_focus_point = Some((position.x - 5.0, y));
    state.rich_dragged = true;
    assert!(selected_image_nodes(&doc, &state).is_empty());

    state.rich_focus_point = Some((position.x + image.final_layout.size.width + 5.0, y));
    assert_eq!(selected_image_nodes(&doc, &state), vec![logo]);
}

#[test]
fn html_width_attribute_constrains_loaded_table_image() {
    use base64::Engine as _;
    use std::io::Cursor;

    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        200,
        200,
        image::Rgba([0, 0, 255, 255]),
    ))
    .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
    .expect("encode synthetic icon");
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let html = repair_outlook_html(&format!(
        r#"<html><body><table width="600"><tr><td align="center">
            <a href="https://example.test"><img id="icon" width="27"
                style="display:block" src="data:image/png;base64,{encoded}"></a>
        </td></tr></table></body></html>"#
    ));
    let (_rendered, doc, _height) = render_test_doc(&html);
    let icon = doc
        .as_ref()
        .query_selector("#icon")
        .expect("selector")
        .expect("icon");
    let layout = doc.as_ref().get_node(icon).expect("icon node").final_layout;

    assert_eq!(layout.size.width, 27.0);
    assert_eq!(layout.size.height, 27.0);
}

#[test]
fn inline_block_email_columns_keep_following_sections_in_flow() {
    let html = repair_outlook_html(
        r##"<html><head><style>
            html, body { height: 100% !important; width: 100% !important; margin: 0; padding: 0; }
            table { border-spacing: 0; margin: 0 auto !important; }
            th { padding: 0 !important; font-weight: normal; }
            .columns { display: inline-block; text-align: center; }
        </style></head><body bgcolor="#fafafa">
            <table width="100%"><tr><td style="height:650px">Introductory content</td></tr></table>
            <table id="features" width="100%" bgcolor="#fafafa"><tr>
                <td></td><td bgcolor="#ffffff" style="width:600px">
                    <table width="100%" style="text-align:center"><tr style="font-size:0">
                        <th class="columns" valign="top" style="text-align:left;display:inline-block;min-width:300px;max-width:100%;width:50%;min-width:-webkit-calc(50%);min-width:calc(50%);width:-webkit-calc(230400px - 48000%);width:calc(230400px - 48000%)">
                            <table width="100%" style="border:5px solid #fff"><tr><td id="feature-a" bgcolor="#f5feff" style="padding:10px">
                                <div style="height:40px;width:40px;background:#000e9c"></div>
                                <div style="height:20px"></div><p id="feature-copy">Feature A<br>Details A</p>
                            </td></tr></table>
                        </th>
                        <th class="columns" valign="top" style="text-align:left;display:inline-block;min-width:300px;max-width:100%;width:50%;min-width:-webkit-calc(50%);min-width:calc(50%);width:-webkit-calc(230400px - 48000%);width:calc(230400px - 48000%)">
                            <table width="100%" style="border:5px solid #fff"><tr><td bgcolor="#f5feff" style="padding:10px">
                                <div style="height:40px;width:40px;background:#000e9c"></div>
                                <div style="height:20px"></div><p>Feature B<br>Details B</p>
                            </td></tr></table>
                        </th>
                    </tr></table>
                </td><td></td>
            </tr></table>
            <table id="benefits" width="100%" bgcolor="#fafafa"><tr>
                <td></td><td bgcolor="#f2f2f2" style="width:600px">
                    <table width="100%"><tr><td style="padding:15px">
                        <table width="100%"><tr><td style="font-size:14px;line-height:21px">
                            <p style="font-size:18px;line-height:21px"><strong>Benefits</strong></p>
                            <p style="font-size:14px;line-height:21px">Benefit A<br>Benefit B<br>Benefit C<br>Benefit D<br>Benefit E</p>
                            <p style="font-size:14px;line-height:21px">Synthetic price.</p>
                        </td></tr></table>
                        <table width="100%"><tr><td height="20" style="line-height:20px;height:20px">
                            <img height="20" style="display:block;height:20px" alt="">
                        </td></tr></table>
                        <table width="100%"><tr><td align="center">
                            <table style="display:table"><tr><td id="benefit-action" style="display:inline-block;padding:14px 16px;border-radius:30px">Action</td></tr></table>
                        </td></tr></table>
                    </td></tr></table>
                </td><td></td>
            </tr></table>
            <table id="footer" width="100%" bgcolor="#fafafa"><tr>
                <td></td><td bgcolor="#00185e" style="width:600px;height:100px;color:white">Footer</td><td></td>
            </tr></table>
        </body></html>"##,
    );
    assert!(!html.contains("48000%"), "extreme calc width was retained");
    let (rendered, doc, render_height) = render_test_doc_at_width(&html, LIGHT_THEME, 720);
    let bounds = |selector: &str| {
        let id = doc
            .as_ref()
            .query_selector(selector)
            .expect("selector")
            .unwrap_or_else(|| panic!("{selector} present"));
        let node = doc.as_ref().get_node(id).expect("layout node");
        let position = node.absolute_position(0.0, 0.0);
        (position.y, position.y + node.final_layout.size.height)
    };
    let features = bounds("#features");
    let feature_a = bounds("#feature-a");
    let feature_copy = bounds("#feature-copy");
    let benefits = bounds("#benefits");
    let benefit_action = bounds("#benefit-action");
    let footer = bounds("#footer");
    let root_height = doc.as_ref().root_element().final_layout.size.height;

    assert!(
        feature_a.1 > feature_a.0 && feature_a.1 >= feature_copy.1,
        "feature card collapsed: {feature_a:?}"
    );
    assert!(
        benefits.0 >= features.1,
        "benefits overlap feature grid: {features:?}, {benefits:?}"
    );
    assert!(
        footer.0 >= benefits.1,
        "footer overlaps benefits: {benefits:?}, {footer:?}"
    );
    assert!(
        benefits.1 >= benefit_action.1,
        "benefit action escapes its section: {benefits:?}, {benefit_action:?}"
    );
    assert!(
        render_height as f32 >= footer.1,
        "document canvas clips the footer: {render_height} < {}",
        footer.1
    );
    assert!(
        root_height >= footer.1,
        "fixed-height root clips following sections: {root_height} < {}",
        footer.1
    );
    let footer_y = (footer.0 as u32 + 10).min(render_height - 1);
    let tile_ix = (footer_y / TILE_ROWS) as usize;
    let local_y = (footer_y % TILE_ROWS) as usize;
    let pixel_offset = (local_y * 720 + 360) * 4;
    assert_eq!(
        &tile_bytes(&rendered, tile_ix)[pixel_offset..pixel_offset + 4],
        &[0x5e, 0x18, 0x00, 0xff],
        "footer must be rasterized below the fixed-height root"
    );
}

#[test]
fn ordinary_calc_width_is_preserved() {
    let html = r#"<html><body><div id="regular" style="width:50%;width:calc(100% - 20px)">Content</div></body></html>"#;
    assert_eq!(repair_outlook_html(html), html);
}

/// On a document taller than several tiles, dragging near the top repaints
/// only the first tile (partial repaint).
#[test]
fn top_drag_repaints_only_first_tile() {
    let paragraphs = "<p>Lorem ipsum dolor sit amet consectetur.</p>".repeat(300);
    let html = format!("<html><body>{paragraphs}</body></html>");
    let (r, mut doc, render_h) = render_test_doc(&html);
    assert!(
        r.tiles.len() >= 3,
        "test document should cover at least 3 tiles"
    );

    let mut st = PaintState::new(render_h, LIGHT_THEME.background_color());
    let outcome = process_batch(&mut doc, drag_batch(18.0, 60.0), 600, TEST_SCALE, &mut st);
    let updates = outcome.update.expect("a partial repaint");
    assert_eq!(updates.len(), 1, "only one tile should be touched");
    assert_eq!(updates[0].0, 0, "the first tile");
}

/// A band repaint must preserve rasterized content exactly outside the
/// selection. This is the path used after mouse interactions.
#[test]
fn partial_repaint_preserves_tile_content() {
    let html = r#"<html><body style="margin:0">
        <div style="height:2300px;background:#123456"></div>
        <div style="height:900px;background:#abcdef"></div>
    </body></html>"#;
    let (rendered, mut doc, render_h) = render_test_doc(html);
    assert!(rendered.tiles.len() >= 2);

    let (ix, repainted, height) = paint_tile_band(
        &mut doc,
        600,
        TEST_SCALE,
        render_h,
        1,
        LIGHT_THEME.background_color(),
        &[],
    )
    .expect("partial repaint");

    assert_eq!(ix, 1);
    assert_eq!(height, rendered.tiles[1].height);
    assert_eq!(
        repainted.as_bytes(0),
        rendered.tiles[1]
            .image
            .as_ref()
            .expect("painted tile")
            .as_bytes(0),
        "a mouse-driven repaint must not blank or alter the tile"
    );
}

/// A click with no visible selection before or after repaints nothing.
#[test]
fn simple_click_does_not_repaint() {
    let (_r, mut doc, render_h) =
        render_test_doc("<html><body><p style=\"font-size:16px\">Hello world</p></body></html>");
    let mods = gpui::Modifiers::default();
    let down = DocOp::Ui(UiEvent::PointerDown(blitz_pointer(
        10.0,
        18.0,
        MouseEventButtons::Primary,
        &mods,
    )));
    let up = DocOp::Ui(UiEvent::PointerUp(blitz_pointer(
        10.0,
        18.0,
        MouseEventButtons::empty(),
        &mods,
    )));
    let mut st = PaintState::new(render_h, LIGHT_THEME.background_color());
    let outcome = process_batch(&mut doc, vec![down, up], 600, TEST_SCALE, &mut st);
    assert!(
        outcome.update.is_none(),
        "a click without a selection should not rerasterize"
    );
}

#[test]
fn click_on_image_is_detected_for_navigation_suppression() {
    let bitmap = image::RgbaImage::from_pixel(8, 6, image::Rgba([0x12, 0x34, 0x56, 0xff]));
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(bitmap)
        .write_to(&mut png, image::ImageFormat::Png)
        .expect("encode test image");
    let job = Job {
        html: r#"<html><body><img id="preview" src="cid:preview" width="80" height="60"></body></html>"#
            .into(),
        images: vec![InlineImage {
            cid: "preview".into(),
            mime: "image/png".into(),
            bytes: png.into_inner(),
        }]
        .into(),
        allow_remote: false,
        force_uniform_font_family: false,
        force_uniform_font_size: false,
        uniform_font_size: 14.0,
        theme: LIGHT_THEME,
        intrinsic_height: false,
    };
    let DocRender {
        mut doc, render_h, ..
    } = render_doc(
        &job,
        600.0,
        600,
        TEST_SCALE,
        Arc::new(DummyNavigationProvider),
        &RenderCancellation::default(),
        FULL_RANGE,
    )
    .expect("render image document");
    let node_id = doc
        .as_ref()
        .query_selector("#preview")
        .expect("selector")
        .expect("preview image");
    let node = doc.as_ref().get_node(node_id).expect("preview node");
    let position = node.absolute_position(0.0, 0.0);
    let x = position.x + node.final_layout.size.width / 2.0;
    let y = position.y + node.final_layout.size.height / 2.0;
    let modifiers = gpui::Modifiers::default();
    let down = DocOp::Ui(UiEvent::PointerDown(blitz_pointer(
        x,
        y,
        MouseEventButtons::Primary,
        &modifiers,
    )));
    let up = DocOp::Ui(UiEvent::PointerUp(blitz_pointer(
        x,
        y,
        MouseEventButtons::empty(),
        &modifiers,
    )));
    let mut state = PaintState::new(render_h, LIGHT_THEME.background_color());
    state.preview_images = true;

    let outcome = process_batch(&mut doc, vec![down, up], 600, TEST_SCALE, &mut state);

    assert!(
        matches!(outcome.hovered_image_change, Some(Some(_))),
        "the faithful viewer must expose a context image under the pointer"
    );
    assert!(outcome.clicked_image);
    assert_eq!(outcome.cursor, CursorStyle::Arrow);
}

/// Clicking an anchor (down and up at the same point) must leave through the
/// `NavigationProvider` with the link URL.
#[test]
fn click_on_link_navigates() {
    #[derive(Default)]
    struct RecordLinks(std::sync::Mutex<Vec<String>>);
    impl NavigationProvider for RecordLinks {
        fn navigate_to(&self, options: NavigationOptions) {
            self.0
                .lock()
                .expect("recorded links")
                .push(options.url.to_string());
        }
    }

    let job = Job {
        html: "<html><body><p style=\"font-size:16px\">\
               <a href=\"https://example.com/offer\">Click here</a></p></body></html>"
            .into(),
        images: Vec::new().into(),
        allow_remote: false,
        force_uniform_font_family: false,
        force_uniform_font_size: false,
        uniform_font_size: 14.0,
        theme: LIGHT_THEME,
        intrinsic_height: false,
    };
    let rec = Arc::new(RecordLinks::default());
    let DocRender {
        mut doc, render_h, ..
    } = render_doc(
        &job,
        600.0,
        600,
        TEST_SCALE,
        rec.clone(),
        &RenderCancellation::default(),
        FULL_RANGE,
    )
    .expect("Blitz render");

    let mods = gpui::Modifiers::default();
    let down = DocOp::Ui(UiEvent::PointerDown(blitz_pointer(
        30.0,
        18.0,
        MouseEventButtons::Primary,
        &mods,
    )));
    let up = DocOp::Ui(UiEvent::PointerUp(blitz_pointer(
        30.0,
        18.0,
        MouseEventButtons::empty(),
        &mods,
    )));
    let mut st = PaintState::new(render_h, LIGHT_THEME.background_color());
    let outcome = process_batch(&mut doc, vec![down, up], 600, TEST_SCALE, &mut st);

    let seen = rec.0.lock().expect("recorded links");
    assert!(
        seen.iter().any(|u| u.contains("example.com/offer")),
        "click should trigger navigation; observed: {seen:?}"
    );
    assert_eq!(
        outcome.hovered_link_change.flatten().as_deref(),
        Some("https://example.com/offer"),
        "the context menu should receive the same safe link"
    );
}

#[test]
fn link_context_target_walks_to_safe_parent_anchor() {
    let (_rendered, doc, _height) = render_test_doc(
        r#"<html><body><a href="https://example.test/path"><span id="label">Open</span></a></body></html>"#,
    );
    let label = doc
        .as_ref()
        .query_selector("#label")
        .expect("selector")
        .expect("link label");

    assert_eq!(
        safe_link_from_node(&doc, label).as_deref(),
        Some("https://example.test/path")
    );
}

#[test]
fn link_context_target_rejects_unsafe_schemes() {
    let (_rendered, doc, _height) = render_test_doc(
        r#"<html><body><a href="javascript:alert(1)"><span id="label">Ignore</span></a></body></html>"#,
    );
    let label = doc
        .as_ref()
        .query_selector("#label")
        .expect("selector")
        .expect("link label");

    assert_eq!(safe_link_from_node(&doc, label), None);
    assert_eq!(
        safe_link("mailto:contact-a@example.test").as_deref(),
        Some("mailto:contact-a@example.test")
    );
}

#[test]
fn mailto_prefills_the_whole_composer() {
    let init = mailto_compose_init("mailto:contact-a%40example.test?subject=Hello&body=Bonjour")
        .expect("a mailto link");
    assert_eq!(init.to, "contact-a@example.test");
    // The subject and body used to be dropped here, which made a link built to
    // open a pre-filled message open an empty one instead.
    assert_eq!(init.subject, "Hello");
    assert_eq!(init.body_md, "Bonjour");

    let init = mailto_compose_init(
        "mailto:contact-a%40example.test,contact-b%40example.invalid?cc=contact-c%40example.test",
    )
    .expect("a mailto link");
    assert_eq!(init.to, "contact-a@example.test, contact-b@example.invalid");
    assert_eq!(init.cc, "contact-c@example.test");

    // Still a mail link, so it opens a composer rather than being handed back
    // to the desktop — with nothing filled in, since nothing was addressable.
    let init = mailto_compose_init("mailto:not-an-address").expect("a mailto link");
    assert!(init.to.is_empty());

    assert!(mailto_compose_init("https://example.test/contact-a@example.test").is_none());
}

#[test]
fn wide_table_is_constrained_to_pane_width() {
    // A `width=900` table, common in emails, must not overflow a 600 px
    // pane (`max-width: 100%` from MAIL_UA_CSS).
    let (_r, doc, _h) = render_test_doc(
        r#"<html><body>
            <table width="900" border="1"><tr><td>Left</td><td>Right</td></tr></table>
        </body></html>"#,
    );
    let table = doc
        .as_ref()
        .query_selector("table")
        .expect("selector")
        .expect("table present");
    let w = doc
        .as_ref()
        .get_node(table)
        .unwrap()
        .final_layout
        .size
        .width;
    assert!(
        w <= 600.0,
        "900 px table is not constrained to the pane: {w}"
    );
}

#[test]
fn percentage_height_email_tables_do_not_create_viewport_sized_gaps() {
    let html = repair_outlook_html(
        r#"<table width="100%"><tr><td></td></tr></table>
        <!doctype html><html style="height:100%"><body style="margin:0;height:100%">
            <table class="wrapper" width="100%" height="100%"><tr><td>
                <table width="600" align="center"><tr><td>
                    <table id="masthead" width="100%" height="100%"><tr>
                        <td style="height:60px">Synthetic brand</td>
                    </tr></table>
                    <table id="offer" width="100%"><tr>
                        <td style="height:100px;background:#f90">Synthetic offer</td>
                    </tr></table>
                    <table width="100%"><tr>
                        <td style="height:1200px">Synthetic long content</td>
                    </tr></table>
                </td></tr></table>
            </td></tr></table>
        </body></html>
        <img width="1" height="1" src="https://telemetry.example.test/pixel">"#,
    );
    let (_rendered, doc, _height) = render_test_doc(&html);
    let bounds = |selector: &str| {
        let id = doc
            .as_ref()
            .query_selector(selector)
            .expect("selector")
            .unwrap_or_else(|| panic!("{selector} present"));
        let node = doc.as_ref().get_node(id).expect("layout node");
        let position = node.absolute_position(0.0, 0.0);
        (position.y, position.y + node.final_layout.size.height)
    };
    let masthead = bounds("#masthead");
    let offer = bounds("#offer");

    assert!(
        masthead.1 - masthead.0 < 150.0 && offer.0 < 150.0,
        "percentage-height email tables created a viewport-sized gap: masthead={masthead:?}, offer={offer:?}"
    );
}

#[test]
fn legacy_table_height_grows_to_fit_a_taller_banner() {
    let html = repair_outlook_html(
        r#"<html><body style="margin:0">
        <table id="spacer" width="540" height="30"><tr><td></td></tr></table>
        <table id="banner" width="540" height="120"><tr>
            <td style="font-size:0;line-height:0">
                <div id="art" style="height:240px;background:#123456"></div>
            </td>
        </tr></table>
        <table id="copy" width="540"><tr>
            <td style="height:80px;background:#eeeeee">Synthetic copy</td>
        </tr></table>
    </body></html>"#,
    );
    let (_rendered, doc, _height) = render_test_doc(&html);
    let bounds = |selector: &str| {
        let id = doc
            .as_ref()
            .query_selector(selector)
            .expect("selector")
            .unwrap_or_else(|| panic!("{selector} present"));
        let node = doc.as_ref().get_node(id).expect("layout node");
        let position = node.absolute_position(0.0, 0.0);
        (position.y, position.y + node.final_layout.size.height)
    };
    let spacer = bounds("#spacer");
    let banner = bounds("#banner");
    let art = bounds("#art");
    let copy = bounds("#copy");

    assert!(
        spacer.1 - spacer.0 >= 30.0
            && banner.1 >= art.1
            && copy.0 >= art.1,
        "legacy table height clipped its banner or lost its minimum: spacer={spacer:?}, banner={banner:?}, art={art:?}, copy={copy:?}"
    );
}

#[test]
fn floated_email_tables_form_a_two_column_grid() {
    // Marketing emails commonly build grids from sibling tables floated
    // left/right instead of using a single table row. Keep the structure
    // close to MediaMarkt's category block: a 660 px container, 20 px cell
    // gutters and four 310 px outer tables wrapping 300 px cards.
    let (_r, doc, _h) = render_test_doc_at_width(
        r#"<html><head><style>
            body { margin: 0; }
            .category { padding-bottom: 20px; }
            .card { height: 250px; }
        </style></head><body>
            <table align="center" width="660"><tr><td style="padding:0 20px 20px">
                <table id="category-1" class="category" align="left" width="310"
                    style="float:left"><tr><td><table class="card" width="300"><tr><td>One</td></tr></table></td></tr></table>
                <table id="category-2" class="category" align="right" width="310"
                    style="float:right"><tr><td><table class="card" width="300"><tr><td>Two</td></tr></table></td></tr></table>
                <table id="category-3" class="category" align="left" width="310"
                    style="float:left"><tr><td><table class="card" width="300"><tr><td>Three</td></tr></table></td></tr></table>
                <table id="category-4" class="category" align="right" width="310"
                    style="float:right"><tr><td><table class="card" width="300"><tr><td>Four</td></tr></table></td></tr></table>
            </td></tr></table>
        </body></html>"#,
        LIGHT_THEME,
        800,
    );

    let position = |selector: &str| {
        let id = doc
            .as_ref()
            .query_selector(selector)
            .expect("selector")
            .unwrap_or_else(|| panic!("{selector} present"));
        doc.as_ref()
            .get_node(id)
            .expect("layout node")
            .absolute_position(0.0, 0.0)
    };
    let first = position("#category-1");
    let second = position("#category-2");
    let third = position("#category-3");
    let fourth = position("#category-4");

    assert!(
        (first.y - second.y).abs() < 1.0 && second.x > first.x,
        "first row should contain left and right cards: {first:?}, {second:?}"
    );
    assert!(
        (third.y - fourth.y).abs() < 1.0 && fourth.x > third.x,
        "second row should contain left and right cards: {third:?}, {fourth:?}"
    );
    assert!(
        third.y > first.y,
        "the second pair should wrap below the first: {first:?}, {third:?}"
    );
}

#[test]
fn outlook_row_height_does_not_overlap_following_content() {
    let html = repair_outlook_html(
        r#"<html><body>
            <div>
                <table border="0" cellspacing="3" cellpadding="0" width="1077"
                    style="width:807.75pt"><tbody><tr style="height:1.25pt">
                    <td valign="top" style="padding:.75pt;height:1.25pt">
                        <p><a href="https://example.test"><span>
                            <img id="banner" width="580" height="183" src="cid:banner">
                        </span></a></p>
                        <table border="0" cellspacing="3" cellpadding="0"><tbody><tr>
                            <td valign="top" style="padding:.75pt"><p>&nbsp;</p></td>
                            <td valign="top" style="padding:.75pt"><p>&nbsp;</p></td>
                        </tr></tbody></table>
                    </td>
                </tr></tbody></table>
                <p>&nbsp;</p>
            </div>
            <p>&nbsp;</p>
            <div id="following"><div style="border-top:solid #E1E1E1 1pt;padding:3pt 0 0">
                <p><b>From:</b> Contact A</p>
            </div></div>
        </body></html>"#,
    );
    let (_r, doc, _h) = render_test_doc(&html);
    let node = |selector: &str| {
        let id = doc
            .as_ref()
            .query_selector(selector)
            .expect("selector")
            .expect("node present");
        doc.as_ref().get_node(id).expect("layout node")
    };
    let banner = node("#banner");
    let following = node("#following");
    let banner_bottom = banner.absolute_position(0.0, 0.0).y + banner.final_layout.size.height;
    let following_top = following.absolute_position(0.0, 0.0).y;

    assert!(
        following_top >= banner_bottom,
        "following content overlaps the Outlook banner: {following_top} < {banner_bottom}"
    );
}

#[test]
fn long_text_wraps_inside_its_cell() {
    // An unbreakable word wider than its column must wrap (MAIL_UA_CSS
    // `overflow-wrap`) instead of overflowing the cell.
    let (_r, doc, _h) = render_test_doc(
        r#"<html><body>
            <table style="width:120px;table-layout:fixed"><tr>
                <td>SupercalifragilisticexpialidociousWithoutAnyBreak</td>
            </tr></table>
        </body></html>"#,
    );
    let td = doc
        .as_ref()
        .query_selector("td")
        .expect("selector")
        .expect("td present");
    let l = doc.as_ref().get_node(td).unwrap().final_layout;
    assert!(
        l.size.height > 30.0,
        "text should wrap across multiple lines (h = {})",
        l.size.height
    );
}

#[test]
fn nonbreaking_spaces_preserve_outlook_style_indentation() {
    let (_rendered, doc, _height) = render_test_doc(
        r#"<html><body>
            <p id="plain" style="margin:0;font:16px Arial">Nested item</p>
            <p id="indented" style="margin:0;font:16px Arial">&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;Nested item</p>
            <p id="formatted" style="margin:0;font:16px Arial">
                Ordinary source whitespace
            </p>
        </body></html>"#,
    );
    let inline_metrics = |selector: &str| {
        let id = doc
            .as_ref()
            .query_selector(selector)
            .expect("selector")
            .unwrap_or_else(|| panic!("{selector} present"));
        let layout = doc
            .as_ref()
            .get_node(id)
            .expect("layout node")
            .element_data()
            .and_then(|element| element.inline_layout_data.as_deref())
            .expect("inline layout");
        let advance = layout
            .layout
            .lines()
            .next()
            .expect("first line")
            .metrics()
            .advance;
        (layout.text.clone(), advance)
    };

    let (plain_text, plain_advance) = inline_metrics("#plain");
    let (indented_text, indented_advance) = inline_metrics("#indented");
    let (formatted_text, _formatted_advance) = inline_metrics("#formatted");

    assert_eq!(plain_text, "Nested item");
    assert!(
        indented_text.starts_with("\u{a0}\u{a0}\u{a0}\u{a0}"),
        "leading non-breaking spaces were trimmed: {indented_text:?}"
    );
    assert!(
        indented_advance > plain_advance + 20.0,
        "non-breaking spaces did not create indentation: {indented_advance} <= {plain_advance}"
    );
    assert_eq!(
        formatted_text, "Ordinary source whitespace",
        "ordinary HTML formatting whitespace must still collapse"
    );
}

#[test]
fn wrapped_text_rows_expand_before_the_next_row() {
    let (_rendered, doc, _height) = render_test_doc_at_width(
        r#"<html><head><style>
            body { margin: 0; background: #eee; border-collapse: collapse; }
            table { border-spacing: 0; }
            @media only screen and (max-width: 700px) {
                table { width: 100% !important; padding: 0 !important; }
                .c-block { display: block !important; width: 100% !important; }
            }
        </style></head><body>
            <table width="700" align="center"><tr><td style="padding:0 30px;background:#fff">
                <table id="copy-table" width="100%" style="max-width:700px">
                    <tr><td class="c-block" width="405" align="right" style="padding:29px 0 17px;font:400 14px/25px Arial">
                        Synthetic metadata<br>Reference: 000000
                    </td></tr>
                    <tr><td class="c-block" width="100%" style="width:auto!important;padding:0 0 10px;font:400 30px/36px Arial;text-align:left">
                        Synthetic greeting
                    </td></tr>
                    <tr><td id="row-a" style="border-collapse:collapse;padding:14px 10px 8px 0;font-family:Arial;font-size:11px;font-weight:400;line-height:180%;line-height:22px;text-align:left">
                        <span style="font:400 16px/22px Arial">This synthetic paragraph contains enough ordinary words to wrap onto several visual lines inside the available table cell.<br>
                        A second sentence in the same cell contains additional ordinary words and must increase the height of its table row before the following content is placed.</span>
                    </td></tr>
                    <tr><td id="row-b" style="border-collapse:collapse;padding:14px 10px 0 0;font-family:Arial;font-size:11px;font-weight:400;line-height:180%;line-height:22px;text-align:left">
                        <span style="font:400 16px/22px Arial">A separate synthetic paragraph also contains enough text to wrap over multiple lines and must remain completely below the first paragraph without painting over it.</span>
                    </td></tr>
                    <tr><td id="row-c" style="border-collapse:collapse;padding:14px 10px 0 0;font-family:Arial;font-size:11px;font-weight:400;line-height:180%;line-height:22px;text-align:left">
                        <span style="font:400 16px/22px Arial">Final line</span>
                    </td></tr>
                    <tr><td id="row-d" colspan="2" style="padding:18px 0 26px">
                        <img width="146" src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=" alt="Synthetic signature">
                    </td></tr>
                </table>
            </td></tr>
            <tr><td id="after-rows" style="height:80px;background:#ddd">Following section
                <table width="100%"><tr><td align="center">
                    <img width="320" src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=" alt="Synthetic artwork">
                </td></tr></table>
            </td></tr>
            </table>
        </body></html>"#,
        LIGHT_THEME,
        760,
    );
    let bounds = |selector: &str| {
        let id = doc
            .as_ref()
            .query_selector(selector)
            .expect("selector")
            .unwrap_or_else(|| panic!("{selector} present"));
        let node = doc.as_ref().get_node(id).expect("layout node");
        let position = node.absolute_position(0.0, 0.0);
        (position.y, position.y + node.final_layout.size.height)
    };
    let width = |selector: &str| {
        let id = doc
            .as_ref()
            .query_selector(selector)
            .expect("selector")
            .unwrap_or_else(|| panic!("{selector} present"));
        doc.as_ref()
            .get_node(id)
            .expect("layout node")
            .final_layout
            .size
            .width
    };
    let row_a = bounds("#row-a");
    let row_b = bounds("#row-b");
    let row_c = bounds("#row-c");
    let row_d = bounds("#row-d");
    let after_rows = bounds("#after-rows");
    let copy_width = width("#copy-table");
    let row_a_width = width("#row-a");

    assert!(
        row_a_width >= copy_width - 1.0,
        "a redundant colspan created an empty text column: {row_a_width} < {copy_width}"
    );
    assert!(
        row_a.1 - row_a.0 >= 80.0,
        "the first wrapped cell stayed one line high: {row_a:?}"
    );
    assert!(
        row_b.0 >= row_a.1,
        "the second row overlaps the first: {row_a:?}, {row_b:?}"
    );
    assert!(
        row_c.0 >= row_b.1,
        "the final row overlaps the second: {row_b:?}, {row_c:?}"
    );
    assert!(
        row_d.0 >= row_c.1,
        "the spanning row overlaps the final text row: {row_c:?}, {row_d:?}"
    );
    assert!(
        after_rows.0 >= row_d.1,
        "the section after the nested table overlaps its rows: {row_d:?}, {after_rows:?}"
    );
}

#[test]
fn minified_html5_namespace_keeps_text_after_breaks() {
    let (_rendered, doc, _height) = render_test_doc_at_width(
        r#"<!DOCTYPE html><html lang="en" xmlns="http://www.w3.org/1999/xhtml" xmlns:v="urn:schemas-microsoft-com:vml"><head><style>
            .notice { width: 520px; font: 16px Arial; }
        </style></head><body>
            <p id="notice" class="notice">Dear recipient,<br><br>This synthetic notification contains enough text to exercise line wrapping.<br><br>Thank you for reviewing this test message.<br><br>Best regards,<br><br>Example service team<br></p>
            <div id="after">After notice</div>
        </body></html>"#,
        LIGHT_THEME,
        650,
    );
    let node = |selector: &str| {
        let id = doc
            .as_ref()
            .query_selector(selector)
            .expect("selector")
            .expect("node present");
        doc.as_ref().get_node(id).expect("layout node")
    };
    let notice = node("#notice");
    let after = node("#after");
    let notice_top = notice.absolute_position(0.0, 0.0).y;
    let notice_bottom = notice_top + notice.final_layout.size.height;
    let after_top = after.absolute_position(0.0, 0.0).y;

    assert!(
        notice.final_layout.size.height >= 120.0,
        "forced breaks should contribute to paragraph height (h = {})",
        notice.final_layout.size.height
    );
    assert!(
        after_top >= notice_bottom,
        "content after the notice overlaps it: {after_top} < {notice_bottom}"
    );
}

#[test]
fn account_alert_email_keeps_the_render_actor_alive() {
    let html = repair_outlook_html(
        r##"<!DOCTYPE html><html lang="en"><head>
            <link href="//example.test/font.css" rel="stylesheet" type="text/css" nonce="synthetic">
        </head><body style="margin:0;padding:0" bgcolor="#fff">
            <table width="100%"><tr><td>Account alert for Contact A</td></tr></table>
        </body></html>"##,
    );
    let job = Arc::new(Job {
        html: html.into(),
        images: Vec::new().into(),
        allow_remote: false,
        force_uniform_font_family: false,
        force_uniform_font_size: false,
        uniform_font_size: 14.0,
        theme: LIGHT_THEME,
        intrinsic_height: false,
    });

    futures::executor::block_on(spawn_doc_thread(
        job,
        600.0,
        1.0,
        1.0,
        FULL_RANGE,
        Arc::new(RenderCancellation::default()),
        false,
    ))
    .expect("render actor response")
    .expect("account alert render");
}

#[test]
fn complex_script_text_wraps_without_spaces() {
    let (_rendered, doc, _height) = render_test_doc(
        r#"<html><body style="margin:0">
            <p id="thai" style="width:80px;font-size:16px;line-height:20px;margin:0">
                ภาษาไทยไม่มีการเว้นวรรคระหว่างคำภาษาไทยไม่มีการเว้นวรรคระหว่างคำ
            </p>
        </body></html>"#,
    );
    let paragraph = doc
        .as_ref()
        .query_selector("#thai")
        .expect("selector")
        .expect("Thai paragraph");
    let height = doc
        .as_ref()
        .get_node(paragraph)
        .expect("Thai paragraph node")
        .final_layout
        .size
        .height;
    assert!(
        height >= 40.0,
        "dictionary segmentation should wrap Thai text across lines (height = {height})"
    );
}

#[test]
fn incremental_hover_reflows_and_restores_layout() {
    let html = r#"<html><head><style>
        body { margin: 0; }
        #target { display: block; font-size: 10px; line-height: 10px; }
        #target:hover { font-size: 30px; line-height: 30px; }
    </style></head><body>
        <a id="target" href="https://example.test">Hover me</a>
        <div id="following">Following content</div>
    </body></html>"#;
    let (_rendered, mut doc, height) = render_test_doc_at_width(html, LIGHT_THEME, 300);
    assert!(
        doc.as_ref().incremental_layout(),
        "the faithful renderer should use Blitz incremental layout"
    );

    let position = |doc: &HtmlDocument, selector: &str| {
        let id = doc
            .as_ref()
            .query_selector(selector)
            .expect("selector")
            .unwrap_or_else(|| panic!("{selector} present"));
        doc.as_ref()
            .get_node(id)
            .expect("layout node")
            .absolute_position(0.0, 0.0)
    };
    let initial_y = position(&doc, "#following").y;
    let target = position(&doc, "#target");
    let mut state = PaintState::new(height, LIGHT_THEME.background_color());
    let modifiers = gpui::Modifiers::default();

    process_batch(
        &mut doc,
        vec![DocOp::Ui(UiEvent::PointerMove(blitz_pointer(
            target.x + 1.0,
            target.y + 1.0,
            MouseEventButtons::empty(),
            &modifiers,
        )))],
        300,
        TEST_SCALE,
        &mut state,
    );
    let hovered_y = position(&doc, "#following").y;
    assert!(
        hovered_y >= initial_y + 20.0,
        "the larger :hover text should push following content down: {initial_y} -> {hovered_y}"
    );

    process_batch(
        &mut doc,
        vec![DocOp::Ui(UiEvent::PointerMove(blitz_pointer(
            299.0,
            100.0,
            MouseEventButtons::empty(),
            &modifiers,
        )))],
        300,
        TEST_SCALE,
        &mut state,
    );
    let restored_y = position(&doc, "#following").y;
    assert!(
        (restored_y - initial_y).abs() < 1.0,
        "leaving :hover should restore layout: {initial_y} -> {restored_y}"
    );
}

#[test]
fn decode_data_uri_base64() {
    // Base64 PNG containing only the magic header.
    let uri = "data:image/png;base64,iVBORw0KGgo%3D";
    let bytes = decode_data_uri(uri).expect("decoding");
    assert_eq!(&bytes[..4], &[0x89, b'P', b'N', b'G']);
}

#[test]
fn bands_cover_logical_ranges() {
    // 3 bands of TILE_ROWS at density 1.
    let render_h = TILE_ROWS * 2 + 100;
    assert_eq!(bands_for_logical_range((0.0, 100.0), 1.0, render_h), (0, 0));
    assert_eq!(
        bands_for_logical_range((0.0, TILE_ROWS as f32 + 1.0), 1.0, render_h),
        (0, 1)
    );
    assert_eq!(
        bands_for_logical_range((0.0, f32::MAX), 1.0, render_h),
        (0, 2)
    );
    // At density 2, one logical viewport of TILE_ROWS/2 already fills band 0.
    assert_eq!(
        bands_for_logical_range((0.0, TILE_ROWS as f32 / 2.0 - 1.0), 2.0, render_h),
        (0, 0)
    );
    // A range far below the document clamps to the last band.
    assert_eq!(
        bands_for_logical_range((1.0e9, 2.0e9), 1.0, render_h),
        (2, 2)
    );
}

fn tall_test_html() -> String {
    let paragraphs = "<p>Lorem ipsum dolor sit amet consectetur.</p>".repeat(300);
    format!("<html><body>{paragraphs}</body></html>")
}

/// A viewport-limited initial render only paints the first bands; the rest
/// are placeholders whose heights match a full render.
#[test]
fn initial_render_is_limited_to_the_visible_range() {
    let job = Job {
        html: tall_test_html().into(),
        images: Vec::new().into(),
        allow_remote: false,
        force_uniform_font_family: false,
        force_uniform_font_size: false,
        uniform_font_size: 14.0,
        theme: LIGHT_THEME,
        intrinsic_height: false,
    };
    let DocRender { rendered: lazy, .. } = render_doc(
        &job,
        600.0,
        600,
        TEST_SCALE,
        Arc::new(DummyNavigationProvider),
        &RenderCancellation::default(),
        (0.0, 800.0),
    )
    .expect("lazy render");
    let DocRender { rendered: full, .. } = render_doc(
        &job,
        600.0,
        600,
        TEST_SCALE,
        Arc::new(DummyNavigationProvider),
        &RenderCancellation::default(),
        FULL_RANGE,
    )
    .expect("full render");

    assert!(lazy.tiles.len() >= 3, "test document should span 3+ bands");
    assert_eq!(lazy.tiles.len(), full.tiles.len());
    assert!(lazy.tiles[0].image.is_some(), "visible band is painted");
    assert!(
        lazy.tiles.last().expect("tiles").image.is_none(),
        "offscreen band stays lazy"
    );
    for (lazy_tile, full_tile) in lazy.tiles.iter().zip(&full.tiles) {
        assert_eq!(
            lazy_tile.height, full_tile.height,
            "placeholder heights must match the painted layout"
        );
    }
}

/// The actor paints requested bands identically to an eager render and
/// reports them as materialized afterwards.
#[test]
fn actor_paints_missing_bands_on_demand() {
    let job = Arc::new(Job {
        html: tall_test_html().into(),
        images: Vec::new().into(),
        allow_remote: false,
        force_uniform_font_family: false,
        force_uniform_font_size: false,
        uniform_font_size: 14.0,
        theme: LIGHT_THEME,
        intrinsic_height: false,
    });
    let (lazy, live) = futures::executor::block_on(spawn_doc_thread(
        job.clone(),
        600.0,
        1.0,
        1.0,
        (0.0, 800.0),
        Arc::new(RenderCancellation::default()),
        false,
    ))
    .expect("actor response")
    .expect("initial actor render");
    let last = lazy.tiles.len() - 1;
    assert!(lazy.tiles[last].image.is_none());

    let (out_tx, out_rx) = oneshot::channel();
    live.tx
        .send(DocCmd::PaintBands {
            bands: vec![last, last + 5],
            out: out_tx,
        })
        .expect("send band request");
    let (_target, painted) = futures::executor::block_on(out_rx).expect("band response");
    assert_eq!(painted.len(), 1, "out-of-range bands are skipped");
    assert_eq!(painted[0].0, last);

    let DocRender { rendered: full, .. } = render_doc(
        &job,
        600.0,
        600,
        TEST_SCALE,
        Arc::new(DummyNavigationProvider),
        &RenderCancellation::default(),
        FULL_RANGE,
    )
    .expect("full render");
    assert_eq!(
        painted[0].1.as_bytes(0),
        full.tiles[last]
            .image
            .as_ref()
            .expect("painted tile")
            .as_bytes(0),
        "on-demand band must match the eager raster"
    );
}

/// Selection repaints skip bands the UI never received.
#[test]
fn selection_repaint_skips_lazy_bands() {
    let (rendered, mut doc, render_h) = render_test_doc(&tall_test_html());
    assert!(rendered.tiles.len() >= 3);
    let mut st = PaintState::new(render_h, LIGHT_THEME.background_color());
    st.materialized = vec![false; rendered.tiles.len()];
    st.materialized[0] = true;

    // Drag across the first two bands: only the materialized one repaints.
    let outcome = process_batch(
        &mut doc,
        drag_batch(18.0, TILE_ROWS as f32 + 60.0),
        600,
        TEST_SCALE,
        &mut st,
    );
    let updates = outcome.update.expect("a partial repaint");
    assert_eq!(
        updates.iter().map(|u| u.0).collect::<Vec<_>>(),
        vec![0],
        "only the materialized band should be repainted"
    );
}

fn test_tile(rows: u32) -> Tile {
    let buffer = vec![0u8; (4 * rows * 4) as usize];
    Tile {
        image: Some(tile_image(buffer, 4, rows).expect("tile")),
        height: rows as f32,
    }
}

fn cached_rendered(tiles: Vec<Tile>) -> Arc<Rendered> {
    Arc::new(Rendered {
        width: 600.0,
        display: 1.0,
        tiles,
        truncated: false,
        resources_pending: false,
    })
}

#[test]
fn byte_budget_evicts_oldest_entries_only() {
    let mut cache = BlitzCache::default();
    cache.entry_mut("old").rendered = Some(cached_rendered(vec![test_tile(64)]));
    cache.entry_mut("new").rendered = Some(cached_rendered(vec![test_tile(64)]));
    let per_entry = 4 * 64 * 4;
    assert_eq!(cache.total_bytes(), 2 * per_entry);

    // Both entries fit: nothing evicted.
    cache.enforce_budget_of(2 * per_entry);
    assert!(cache.entries.contains_key("old"));

    // Touching "old" makes it the survivor when the budget shrinks.
    cache.entry_mut("old");
    cache.enforce_budget_of(per_entry);
    assert!(cache.entries.contains_key("old"));
    assert!(!cache.entries.contains_key("new"));
    assert_eq!(
        cache.take_orphaned().len(),
        1,
        "evicted tiles await a GPU drop"
    );

    // The most recent entry survives even an impossible budget.
    cache.enforce_budget_of(0);
    assert!(cache.entries.contains_key("old"));
}

#[test]
fn maintain_bands_queues_missing_and_evicts_far_bands() {
    let viewport_h = TILE_ROWS as f32;
    let mut entry = Entry::default();
    let (tx, _rx) = mpsc::channel();
    entry.live = Some(Arc::new(LiveDoc { tx }));
    entry.live_target = Some((600.0, 1.0, 1.0));
    // 12 bands: band 0 painted (far above), band 11 painted (far below),
    // the visible band 4 missing.
    let mut tiles: Vec<Tile> = (0..12)
        .map(|_| Tile {
            image: None,
            height: TILE_ROWS as f32,
        })
        .collect();
    tiles[0] = test_tile(TILE_ROWS);
    tiles[11] = test_tile(TILE_ROWS);
    entry.rendered = Some(cached_rendered(tiles));

    let paint_range = (4.0 * viewport_h, 5.0 * viewport_h);
    let evicted = maintain_bands(&mut entry, paint_range, viewport_h, 600.0, 1.0);

    assert_eq!(
        entry.pending_bands,
        vec![4, 5],
        "visible missing bands queued"
    );
    assert_eq!(entry.desired_bands, Some((4, 5)));
    assert_eq!(evicted.len(), 2, "both far bands are evicted");
    let rendered = entry.rendered.as_ref().expect("rendered");
    assert!(rendered.tiles[0].image.is_none());
    assert!(rendered.tiles[11].image.is_none());

    // A stale target (mid-resize) leaves everything untouched.
    let mut stale = Entry::default();
    let (tx, _rx) = mpsc::channel();
    stale.live = Some(Arc::new(LiveDoc { tx }));
    stale.live_target = Some((500.0, 1.0, 1.0));
    stale.rendered = Some(cached_rendered(vec![Tile {
        image: None,
        height: TILE_ROWS as f32,
    }]));
    assert!(maintain_bands(&mut stale, (0.0, 100.0), viewport_h, 600.0, 1.0).is_empty());
    assert!(stale.pending_bands.is_empty());
}

#[test]
fn prepared_job_memoizes_and_invalidates() {
    let options = MailBodyOptions {
        show_remote_images: false,
        force_uniform_font_family: false,
        force_uniform_font_size: false,
        force_light_theme: false,
        font_size: 14.0,
    };
    let mut cache = PrepCache::default();
    // Exercise the pure logic without a gpui App: mirror prepared_job's
    // hit condition directly against the cache struct.
    let source = "<table><tr><td>Bonjour</td></tr></table>";
    let html: Arc<str> = repair_outlook_html(source).into();
    let html_hash = hash_html(&html, PrepSource::Html);
    let images: Arc<[InlineImage]> = Vec::new().into();
    let key = cache_key(
        "message:1",
        &html,
        &[],
        test_body_options(false, false, 14.0),
        LIGHT_THEME,
    );
    cache.entries.insert(
        "message:1".into(),
        PrepEntry {
            source: Arc::from(source),
            kind: PrepSource::Html,
            html: html.clone(),
            html_hash,
            images_sig: (0, 0),
            show_remote: false,
            force_uniform_font_family: false,
            force_uniform_font_size: false,
            font_size_bits: options.font_size.to_bits(),
            theme: LIGHT_THEME,
            images: images.clone(),
            key: key.clone(),
            job: Arc::new(Job {
                html: html.clone(),
                images,
                allow_remote: false,
                force_uniform_font_family: false,
                force_uniform_font_size: false,
                uniform_font_size: 14.0,
                theme: LIGHT_THEME,
                intrinsic_height: false,
            }),
        },
    );

    let entry = cache.entries.get("message:1").expect("memo entry");
    let hit = entry.kind == PrepSource::Html
        && entry.show_remote == options.show_remote_images
        && entry.images_sig == images_signature(&[])
        && entry.theme == LIGHT_THEME
        && entry.source.as_ref() == source;
    assert!(hit, "unchanged source and options must hit the memo");

    let new_images = [InlineImage {
        cid: "logo".into(),
        mime: "image/png".into(),
        bytes: vec![1, 2, 3],
    }];
    assert_ne!(
        entry.images_sig,
        images_signature(&new_images),
        "a late-arriving image set must invalidate the memo"
    );
    assert_ne!(
        entry.key,
        cache_key(
            "message:1",
            &html,
            &new_images,
            test_body_options(false, false, 14.0),
            LIGHT_THEME,
        ),
        "rehydrated images must use a fresh Blitz document"
    );
    assert_ne!(
        entry.theme, DARK_THEME,
        "a theme change must invalidate the memo"
    );
    assert!(
        entry.kind == PrepSource::Html && entry.source.as_ref() == source,
        "a theme change must keep the repaired HTML reusable"
    );
}

/// The document behind a message the reader has left is the expensive half of
/// a cache entry; its tiles are the cheap half that keeps the message on
/// screen. Releasing one without the other is the whole point.
#[test]
fn idle_documents_are_released_while_their_tiles_stay() {
    let mut cache = BlitzCache::default();
    let now = Instant::now();
    let idle = Duration::from_secs(15);

    for (key, seen) in [("on-screen", now), ("left-behind", now - idle * 2)] {
        let entry = cache.entry_mut(key);
        let (tx, _rx) = mpsc::channel();
        entry.live = Some(Arc::new(LiveDoc { tx }));
        entry.last_seen = Some(seen);
        entry.last_target = Some((600.0, 1.0, 1.0));
        entry.live_target = Some((600.0, 1.0, 1.0));
        entry.rendered = Some(Arc::new(Rendered {
            width: 600.0,
            display: 1.0,
            tiles: vec![test_tile(TILE_ROWS)],
            truncated: false,
            resources_pending: false,
        }));
    }

    assert_eq!(cache.release_idle_documents(idle, now), 1);

    let displayed = &cache.entries["on-screen"];
    assert!(displayed.live.is_some(), "a visible document must survive");

    let cached = &cache.entries["left-behind"];
    assert!(cached.live.is_none(), "the idle document must be released");
    assert!(
        cached.rendered.is_some(),
        "its tiles must stay so returning to the message is instant"
    );
    // Rasterization is lazy, so the entry has to be renderable again — a
    // retained `last_target` would make `request_target` skip the rebuild and
    // leave unpainted bands as permanent placeholders.
    assert!(cached.last_target.is_none());
    assert!(cached.live_target.is_none());
}

/// A second sweep within the interval must not walk the cache again.
#[test]
fn document_sweeps_are_rate_limited() {
    let mut cache = BlitzCache::default();
    cache.sweep_idle_documents();
    let first = cache.last_sweep.expect("first sweep records its instant");
    cache.sweep_idle_documents();
    assert_eq!(
        cache.last_sweep,
        Some(first),
        "the second sweep must be skipped"
    );
}
