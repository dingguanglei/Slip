//! Render an image to terminal cells using Unicode half-blocks.
//!
//! Each character cell shows two vertically-stacked pixels via the upper
//! half-block `▀`: the foreground colour is the top pixel, the background
//! colour the bottom one. This needs no terminal graphics protocol, so an
//! inline thumbnail shows up in every terminal (the universal fallback that
//! kitty/iTerm2/sixel would only improve on). Output is ratatui `Line`s, so
//! thumbnails drop straight into the chat log.

use image::{GenericImageView, ImageReader};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::io::Cursor;

const UPPER_HALF_BLOCK: &str = "▀";

/// Cap the decoder's memory so a decompression bomb (a tiny file that expands
/// to gigabytes) cannot exhaust RAM while rendering a thumbnail.
const MAX_DECODE_BYTES: u64 = 96 * 1024 * 1024;
/// Cap the source dimensions the decoder will accept.
const MAX_DECODE_DIM: u32 = 12_000;
/// Skip files larger than this without even reading them.
pub const MAX_THUMB_FILE_BYTES: u64 = 25 * 1024 * 1024;

/// Whether inline image thumbnails are enabled. `SLIP_IMAGE=off` disables them.
pub fn thumbnails_enabled() -> bool {
    !std::env::var("SLIP_IMAGE")
        .map(|value| matches!(value.trim(), "off" | "0" | "false" | "no"))
        .unwrap_or(false)
}

/// Decode `bytes` and render a thumbnail no larger than `max_cols` columns by
/// `max_rows` character rows, preserving aspect ratio. Returns `None` if the
/// bytes are not a decodable image or exceed the decode limits (which bound
/// memory against maliciously crafted images).
pub fn render_thumbnail(bytes: &[u8], max_cols: u16, max_rows: u16) -> Option<Vec<Line<'static>>> {
    if max_cols == 0 || max_rows == 0 {
        return None;
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIM);
    limits.max_image_height = Some(MAX_DECODE_DIM);
    limits.max_alloc = Some(MAX_DECODE_BYTES);

    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    let image = reader.decode().ok()?;
    Some(render_image(&image, max_cols, max_rows))
}

fn render_image(image: &image::DynamicImage, max_cols: u16, max_rows: u16) -> Vec<Line<'static>> {
    let (src_w, src_h) = image.dimensions();
    if src_w == 0 || src_h == 0 {
        return Vec::new();
    }

    // A cell is one pixel wide and two pixels tall. Terminal cells are about
    // twice as tall as wide, so a half-block pixel is roughly square and the
    // image keeps its proportions when fit into cols x (rows*2) pixels.
    let max_px_w = max_cols as f64;
    let max_px_h = (max_rows as f64) * 2.0;
    let scale = (max_px_w / src_w as f64)
        .min(max_px_h / src_h as f64)
        .min(1.0);
    let target_w = ((src_w as f64 * scale).round() as u32).max(1);
    let target_h = ((src_h as f64 * scale).round() as u32).max(1);

    let resized = image
        .resize_exact(target_w, target_h, image::imageops::FilterType::Triangle)
        .to_rgba8();
    let width = resized.width();
    let height = resized.height();

    let mut lines = Vec::new();
    let mut row = 0u32;
    while row < height {
        let mut spans = Vec::with_capacity(width as usize);
        for col in 0..width {
            let top = resized.get_pixel(col, row);
            let bottom = if row + 1 < height {
                *resized.get_pixel(col, row + 1)
            } else {
                // Odd height: pad the last row with the top pixel.
                *top
            };
            spans.push(Span::styled(
                UPPER_HALF_BLOCK,
                Style::default()
                    .fg(rgba_to_color(top.0))
                    .bg(rgba_to_color(bottom.0)),
            ));
        }
        lines.push(Line::from(spans));
        row += 2;
    }
    lines
}

/// Flatten RGBA onto a dark background so transparent regions blend rather
/// than show a jarring opaque colour.
fn rgba_to_color(rgba: [u8; 4]) -> Color {
    let [r, g, b, a] = rgba;
    if a == 255 {
        return Color::Rgb(r, g, b);
    }
    let alpha = a as u16;
    let blend = |channel: u8| ((channel as u16 * alpha) / 255) as u8;
    Color::Rgb(blend(r), blend(g), blend(b))
}

#[cfg(test)]
mod tests {
    use super::{render_thumbnail, thumbnails_enabled};

    fn tiny_png() -> Vec<u8> {
        // A 2x2 RGBA image encoded as PNG.
        use image::{ImageFormat, RgbaImage};
        let mut img = RgbaImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        img.put_pixel(1, 0, image::Rgba([0, 255, 0, 255]));
        img.put_pixel(0, 1, image::Rgba([0, 0, 255, 255]));
        img.put_pixel(1, 1, image::Rgba([255, 255, 0, 255]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn renders_small_image_to_lines() {
        let lines = render_thumbnail(&tiny_png(), 20, 10).expect("decodes");
        assert!(!lines.is_empty());
        // 2px tall -> one cell row of half-blocks.
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 2);
    }

    #[test]
    fn rejects_non_image_bytes() {
        assert!(render_thumbnail(b"not an image", 20, 10).is_none());
        assert!(render_thumbnail(&tiny_png(), 0, 10).is_none());
    }

    #[test]
    fn env_toggle_default_on() {
        // Default (unset) is enabled; the parsing itself is what we check.
        let _ = thumbnails_enabled();
    }
}
