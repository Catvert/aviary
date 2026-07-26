//! Full-window preview and actions for images inside received messages.

use crate::model::Attachment;
use gpui::{
    anchored, div, img, point, prelude::*, px, AnyElement, App, Corner, CursorStyle, FocusHandle,
    Image, ImageFormat, KeyDownEvent, MouseButton, RenderImage, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem},
    notification::Notification,
    IconName, Sizable, WindowExt as _,
};
#[cfg(target_os = "linux")]
use std::process::Stdio;
use std::{io::Cursor, sync::Arc};

/// Everything needed to preview, export, or open one image rasterized by the
/// local faithful/Markdown renderer.
#[derive(Clone)]
pub(crate) struct ImageAsset {
    image: Arc<RenderImage>,
    filename: String,
}

impl ImageAsset {
    pub(crate) fn rendered(image: Arc<RenderImage>) -> Self {
        Self {
            image,
            filename: tr!("viewer-image-default-filename").to_string(),
        }
    }

    fn attachment(&self, bytes: Vec<u8>) -> Attachment {
        let mime = mime_for_bytes(&bytes).to_string();
        Attachment {
            id: String::new(),
            filename: self.filename.clone(),
            mime,
            size: bytes.len() as u64,
            bytes: Some(bytes),
        }
    }
}

#[derive(Default)]
struct ImageLightbox {
    image: Option<ImageAsset>,
    focus: Option<FocusHandle>,
}

impl gpui::Global for ImageLightbox {}

fn open(image: ImageAsset, cx: &mut App) {
    let focus = cx.focus_handle();
    let previous = {
        let lightbox = cx.default_global::<ImageLightbox>();
        lightbox.focus = Some(focus);
        lightbox.image.replace(image)
    };
    drop_rendered(previous, cx);
    cx.refresh_windows();
}

fn drop_rendered(image: Option<ImageAsset>, cx: &mut App) {
    if let Some(image) = image {
        cx.drop_image(image.image, None);
    }
}

fn close(cx: &mut App) {
    let image = cx.default_global::<ImageLightbox>().image.take();
    drop_rendered(image, cx);
    cx.refresh_windows();
}

/// Shared by faithful/Markdown context menus and image previews.
pub(crate) fn actions_menu(menu: PopupMenu, image: ImageAsset, cx: &App) -> PopupMenu {
    actions_menu_with_enlarge(menu, image, true, cx)
}

fn actions_menu_with_enlarge(
    mut menu: PopupMenu,
    image: ImageAsset,
    include_enlarge: bool,
    _cx: &App,
) -> PopupMenu {
    if include_enlarge {
        let image_to_enlarge = image.clone();
        menu = menu.item(
            PopupMenuItem::new(tr!("viewer-image-open-large"))
                .icon(super::icons::app_icon("maximize"))
                .on_click(move |_, _, cx| open(image_to_enlarge.clone(), cx)),
        );
    }
    let image_to_open = image.clone();
    menu = menu.item(
        PopupMenuItem::new(tr!("viewer-image-open-external"))
            .icon(super::icons::app_icon("external-link"))
            .on_click(move |_, window, cx| {
                open_external(image_to_open.clone(), window, cx);
            }),
    );
    let image_to_copy = image.clone();
    menu = menu.item(
        PopupMenuItem::new(tr!("viewer-image-copy"))
            .icon(super::icons::app_icon("copy"))
            .on_click(move |_, window, cx| {
                copy_image(image_to_copy.clone(), window, cx);
            }),
    );
    menu.item(
        PopupMenuItem::new(tr!("viewer-image-save-as"))
            .icon(super::icons::app_icon("download"))
            .on_click(move |_, window, cx| save_as(image.clone(), window, cx)),
    )
}

/// Copy action for image attachments, which already own their local bytes.
pub(crate) fn attachment_copy_item(attachment: Arc<Attachment>) -> PopupMenuItem {
    let unavailable = attachment.bytes.is_none();
    PopupMenuItem::new(tr!("viewer-image-copy"))
        .icon(super::icons::app_icon("copy"))
        .disabled(unavailable)
        .on_click(move |_, window, cx| {
            if let Some(bytes) = attachment.bytes.clone() {
                copy_image_bytes(bytes, window, cx);
            }
        })
}

fn copy_image(image: ImageAsset, window: &mut Window, cx: &mut App) {
    let window_handle = window.window_handle();
    cx.spawn(async move |cx| match materialize_bytes(&image, cx).await {
        Ok(bytes) => {
            let _ = cx.update_window(window_handle, |_, window, cx| {
                copy_image_bytes(bytes, window, cx);
            });
        }
        Err(error) => {
            let _ = cx.update_window(window_handle, |_, window, cx| {
                window.push_notification(
                    Notification::error(tr!("viewer-image-copy-error", { error: error })),
                    cx,
                );
            });
        }
    })
    .detach();
}

fn copy_image_bytes(bytes: Vec<u8>, window: &mut Window, cx: &mut App) {
    let image = match clipboard_image(bytes) {
        Ok(image) => image,
        Err(error) => {
            window.push_notification(
                Notification::error(tr!("viewer-image-copy-error", { error: error })),
                cx,
            );
            return;
        }
    };

    // gpui 0.2.2's Wayland backend only advertises and serves text MIME types,
    // even when its ClipboardItem contains an image. Use wl-copy until that
    // backend can publish image bytes itself. X11, macOS and Windows keep the
    // native gpui clipboard path.
    #[cfg(target_os = "linux")]
    if cx.compositor_name() == "Wayland" {
        copy_image_wayland(image, window, cx);
        return;
    }

    cx.write_to_clipboard(gpui::ClipboardItem::new_image(&image));
    window.push_notification(Notification::success(tr!("viewer-image-copied")), cx);
}

#[cfg(target_os = "linux")]
fn copy_image_wayland(image: Image, window: &mut Window, cx: &mut App) {
    let Some(runtime) = crate::runtime::TOKIO_HANDLE.get() else {
        window.push_notification(
            Notification::error(tr!("viewer-image-copy-error", {
                error: tr!("viewer-image-copy-runtime-unavailable")
            })),
            cx,
        );
        return;
    };
    let task = runtime.spawn(write_wayland_clipboard_image(image));
    let window_handle = window.window_handle();
    cx.spawn(async move |cx| {
        let result = match task.await {
            Ok(result) => result,
            Err(error) => Err(tr!("viewer-image-copy-wayland-failed", {
                error: error.to_string()
            })
            .to_string()),
        };
        let _ = cx.update_window(window_handle, |_, window, cx| match result {
            Ok(()) => {
                window.push_notification(Notification::success(tr!("viewer-image-copied")), cx)
            }
            Err(error) => window.push_notification(
                Notification::error(tr!("viewer-image-copy-error", { error: error })),
                cx,
            ),
        });
    })
    .detach();
}

#[cfg(target_os = "linux")]
async fn write_wayland_clipboard_image(image: Image) -> Result<(), String> {
    use tokio::io::AsyncWriteExt as _;

    let mut command = tokio::process::Command::new("wl-copy");
    command
        .arg("--type")
        .arg(image.format.mime_type())
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            tr!("viewer-image-copy-wayland-helper-missing")
        } else {
            tr!("viewer-image-copy-wayland-failed", {
                error: error.to_string()
            })
        }
    })?;
    let Some(mut stdin) = child.stdin.take() else {
        return Err(tr!("viewer-image-copy-wayland-failed", {
            error: tr!("viewer-image-copy-wayland-stdin")
        })
        .to_string());
    };
    stdin.write_all(&image.bytes).await.map_err(|error| {
        tr!("viewer-image-copy-wayland-failed", {
            error: error.to_string()
        })
    })?;
    drop(stdin);

    let output = child.wait_with_output().await.map_err(|error| {
        tr!("viewer-image-copy-wayland-failed", {
            error: error.to_string()
        })
    })?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = if stderr.trim().is_empty() {
        output.status.to_string()
    } else {
        stderr.trim().to_string()
    };
    Err(tr!("viewer-image-copy-wayland-failed", {
        error: detail
    })
    .to_string())
}

fn clipboard_image(bytes: Vec<u8>) -> Result<Image, String> {
    let format = if looks_like_svg(&bytes) {
        ImageFormat::Svg
    } else {
        match image::guess_format(&bytes) {
            Ok(image::ImageFormat::Png) => ImageFormat::Png,
            Ok(image::ImageFormat::Jpeg) => ImageFormat::Jpeg,
            Ok(image::ImageFormat::WebP) => ImageFormat::Webp,
            Ok(image::ImageFormat::Gif) => ImageFormat::Gif,
            Ok(image::ImageFormat::Bmp) => ImageFormat::Bmp,
            Ok(image::ImageFormat::Tiff) => ImageFormat::Tiff,
            _ => return Err(tr!("viewer-image-copy-unsupported").to_string()),
        }
    };
    Ok(Image::from_bytes(format, bytes))
}

fn open_external(image: ImageAsset, window: &mut Window, cx: &mut App) {
    let window_handle = window.window_handle();
    cx.spawn(async move |cx| match materialize_bytes(&image, cx).await {
        Ok(bytes) => super::attachments::open(image.attachment(bytes)),
        Err(error) => {
            let _ = cx.update_window(window_handle, |_, window, cx| {
                window.push_notification(
                    Notification::error(tr!("viewer-image-open-error", { error: error })),
                    cx,
                );
            });
        }
    })
    .detach();
}

fn save_as(image: ImageAsset, window: &mut Window, cx: &mut App) {
    let directory = super::attachments::download_directory();
    let attachment = image.attachment(Vec::new());
    let suggested_name = super::attachments::suggested_filename(&attachment);
    let destination = cx.prompt_for_new_path(&directory, Some(&suggested_name));
    let window_handle = window.window_handle();

    cx.spawn(async move |cx| {
        let path = match destination.await {
            Ok(Ok(Some(path))) => path,
            Ok(Ok(None)) | Err(_) => return,
            Ok(Err(error)) => {
                let _ = cx.update_window(window_handle, |_, window, cx| {
                    window.push_notification(
                        Notification::error(tr!("viewer-attachments-picker-error", {
                            error: error
                        })),
                        cx,
                    );
                });
                return;
            }
        };

        let bytes = match materialize_bytes(&image, cx).await {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = cx.update_window(window_handle, |_, window, cx| {
                    window.push_notification(
                        Notification::error(tr!("viewer-image-save-error", { error: error })),
                        cx,
                    );
                });
                return;
            }
        };
        let filename = image.filename.clone();
        let attachment = image.attachment(bytes);
        let saved = cx
            .background_executor()
            .spawn(async move { super::attachments::save_as(&path, &attachment) })
            .await;
        let _ = cx.update_window(window_handle, |_, window, cx| match saved {
            Ok(()) => window.push_notification(
                Notification::success(tr!("viewer-image-save-success", {
                    filename: filename
                })),
                cx,
            ),
            Err(error) => window.push_notification(
                Notification::error(tr!("viewer-image-save-error", { error: error })),
                cx,
            ),
        });
    })
    .detach();
}

async fn materialize_bytes(image: &ImageAsset, cx: &gpui::AsyncApp) -> Result<Vec<u8>, String> {
    let rendered = image.image.clone();
    cx.background_executor()
        .spawn(async move { rendered_png(&rendered) })
        .await
}

fn rendered_png(image: &RenderImage) -> Result<Vec<u8>, String> {
    let size = image.size(0);
    let width = size.width.0.max(0) as u32;
    let height = size.height.0.max(0) as u32;
    let mut rgba = image
        .as_bytes(0)
        .ok_or_else(|| tr!("viewer-image-content-unavailable"))?
        .to_vec();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2); // gpui stores BGRA; image expects RGBA.
    }
    let image = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| tr!("viewer-image-content-unavailable"))?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(png)
}

fn mime_for_bytes(bytes: &[u8]) -> &'static str {
    if looks_like_svg(bytes) {
        return "image/svg+xml";
    }
    match image::guess_format(bytes) {
        Ok(image::ImageFormat::Jpeg) => "image/jpeg",
        Ok(image::ImageFormat::Gif) => "image/gif",
        Ok(image::ImageFormat::WebP) => "image/webp",
        _ => "image/png",
    }
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let prefix = &bytes[..bytes.len().min(1024)];
    std::str::from_utf8(prefix).is_ok_and(|text| text.to_ascii_lowercase().contains("<svg"))
}

pub(crate) fn render(window: &mut Window, cx: &mut App) -> Option<AnyElement> {
    let (image, focus) = {
        let lightbox = cx.default_global::<ImageLightbox>();
        (lightbox.image.clone()?, lightbox.focus.clone()?)
    };
    focus.focus(window);

    let viewport = window.viewport_size();
    // Keep a real margin on every side and reserve the upper strip for the
    // action bar. ObjectFit::Contain preserves the whole image at every window
    // size instead of cropping large or portrait images.
    let max_width = (viewport.width - px(64.)).max(px(1.));
    let max_height = (viewport.height - px(112.)).max(px(1.));
    let displayed = img(image.image.clone())
        .object_fit(gpui::ObjectFit::Contain)
        .w_full()
        .h_full();

    let menu_image = image.clone();
    let context_image = image.clone();

    Some(
        anchored()
            .anchor(Corner::TopLeft)
            .position(point(px(0.), px(0.)))
            .snap_to_window()
            .child(
                div()
                    .id("viewer-image-lightbox")
                    .occlude()
                    .w(viewport.width)
                    .h(viewport.height)
                    .flex()
                    .items_center()
                    .justify_center()
                    .pt(px(56.))
                    .pb(px(24.))
                    .px(px(32.))
                    .cursor(CursorStyle::Arrow)
                    .bg(gpui::black().opacity(0.88))
                    .track_focus(&focus)
                    .on_key_down(|event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "escape" {
                            cx.stop_propagation();
                            close(cx);
                        }
                    })
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                        close(cx);
                    })
                    .child(
                        div()
                            .w(max_width)
                            .h(max_height)
                            .flex()
                            .items_center()
                            .justify_center()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .child(displayed)
                            .context_menu(move |menu, _, cx| {
                                actions_menu_with_enlarge(menu, context_image.clone(), false, cx)
                            }),
                    )
                    .child(
                        h_flex()
                            .absolute()
                            .top_3()
                            .right_3()
                            .gap_1()
                            .p_1()
                            .rounded_lg()
                            .text_color(gpui::white())
                            .bg(gpui::black().opacity(0.58))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .child(
                                Button::new("viewer-image-lightbox-actions")
                                    .small()
                                    .ghost()
                                    .icon(IconName::Ellipsis)
                                    .tooltip(tr!("viewer-image-actions"))
                                    .dropdown_menu_with_anchor(
                                        Corner::TopRight,
                                        move |menu, _, cx| {
                                            actions_menu_with_enlarge(
                                                menu,
                                                menu_image.clone(),
                                                false,
                                                cx,
                                            )
                                        },
                                    ),
                            )
                            .child(
                                Button::new("viewer-image-lightbox-close")
                                    .small()
                                    .ghost()
                                    .icon(IconName::Close)
                                    .tooltip(tr!("viewer-image-close"))
                                    .on_click(|_, _, cx| {
                                        cx.stop_propagation();
                                        close(cx);
                                    }),
                            ),
                    ),
            )
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::{clipboard_image, rendered_png};
    use gpui::{ImageFormat, RenderImage};
    use image::{Frame, Rgba, RgbaImage};
    use smallvec::smallvec;

    #[test]
    fn rendered_bgra_is_exported_as_rgba_png() {
        let frame = Frame::new(RgbaImage::from_pixel(
            1,
            1,
            Rgba([30, 20, 10, 255]), // Stored buffer is interpreted as BGRA by gpui.
        ));
        let rendered = RenderImage::new(smallvec![frame]);
        let png = rendered_png(&rendered).expect("PNG export");
        let decoded = image::load_from_memory(&png)
            .expect("decode PNG")
            .to_rgba8();
        let pixel = decoded.get_pixel(0, 0);
        assert_eq!(pixel.0, [10, 20, 30, 255]);
    }

    #[test]
    fn clipboard_image_preserves_supported_encoded_format() {
        let png = png_bytes();
        let copied = clipboard_image(png.clone()).expect("clipboard image");
        assert_eq!(copied.format, ImageFormat::Png);
        assert_eq!(copied.bytes, png);

        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>".to_vec();
        let copied = clipboard_image(svg.clone()).expect("SVG clipboard image");
        assert_eq!(copied.format, ImageFormat::Svg);
        assert_eq!(copied.bytes, svg);
    }

    fn png_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encode PNG");
        bytes
    }
}
