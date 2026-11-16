//! The wallpaper under the glass and its frost: Neptune's deep blue with soft light, its rings and
//! a few stars, drawn once; then a blurred copy that every glass panel shows the part of behind
//! it (downscale, then three box passes each way).

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use tiny_skia::{
    Color, FillRule, FilterQuality, GradientStop, Paint, PathBuilder, Pixmap, PixmapPaint, Point, RadialGradient, Rect,
    SpreadMode, Stroke, Transform,
};

/// Square, so `image-fit: cover` crops it the same for the wallpaper and the frost at any window shape.
const EDGE: u32 = 1200;
/// The blurred copy's edge: the frost has no detail to lose and the panel scales it up.
const FROST_EDGE: u32 = 160;
/// Box radius at FROST_EDGE: about a 24 dp frost on a phone.
const FROST_RADIUS: usize = 5;

pub struct Backdrop {
    pub wallpaper: Image,
    pub frost: Image,
}

pub fn render(light: bool) -> Backdrop {
    let wallpaper = wallpaper(light);
    let frost = frost(&wallpaper);
    Backdrop { wallpaper: to_image(&wallpaper), frost: to_image(&frost) }
}

fn rgba(hex: u32, alpha: f32) -> Color {
    let [_, r, g, b] = hex.to_be_bytes();
    Color::from_rgba8(r, g, b, (alpha * 255.0).round() as u8)
}

fn glow(pixmap: &mut Pixmap, x: f32, y: f32, radius: f32, hex: u32, alpha: f32) {
    let stops = vec![GradientStop::new(0.0, rgba(hex, alpha)), GradientStop::new(1.0, rgba(hex, 0.0))];
    let center = Point::from_xy(x, y);
    let Some(shader) = RadialGradient::new(center, center, radius, stops, SpreadMode::Pad, Transform::identity()) else {
        return;
    };
    let paint = Paint { shader, anti_alias: true, ..Paint::default() };
    let full = Rect::from_xywh(0.0, 0.0, EDGE as f32, EDGE as f32).expect("rect");
    pixmap.fill_rect(full, &paint, Transform::identity(), None);
}

fn wallpaper(light: bool) -> Pixmap {
    let mut pixmap = Pixmap::new(EDGE, EDGE).expect("pixmap");
    let e = EDGE as f32;
    if light {
        pixmap.fill(rgba(0xDCE6F4, 1.0));
        glow(&mut pixmap, e * 0.15, e * 0.10, e * 0.75, 0x8FB5FF, 0.85);
        glow(&mut pixmap, e * 0.95, e * 0.55, e * 0.70, 0x7FD6D0, 0.70);
        glow(&mut pixmap, e * 0.70, e * 0.95, e * 0.55, 0xF2C978, 0.55);
    } else {
        pixmap.fill(rgba(0x070C1C, 1.0));
        glow(&mut pixmap, e * 0.18, e * 0.08, e * 0.80, 0x2A4FD6, 0.85);
        glow(&mut pixmap, e * 0.95, e * 0.55, e * 0.70, 0x0E8C99, 0.70);
        glow(&mut pixmap, e * 0.65, e * 1.00, e * 0.60, 0xE7B85A, 0.40);
    }

    // The planet's rings: crisp on the wallpaper, gone to light under the glass.
    let ring = if light { rgba(0xFFFFFF, 0.75) } else { rgba(0xBFD3FF, 0.35) };
    let paint = Paint { shader: tiny_skia::Shader::SolidColor(ring), anti_alias: true, ..Paint::default() };
    let tilt = Transform::from_rotate_at(-18.0, e * 0.55, e * 0.42);
    for (i, width) in [(0, 2.5), (1, 1.2), (2, 4.0)] {
        let rx = e * (0.46 + 0.07 * i as f32);
        let ry = rx * 0.22;
        let Some(oval) = Rect::from_xywh(e * 0.55 - rx, e * 0.42 - ry, 2.0 * rx, 2.0 * ry) else { continue };
        if let Some(path) = PathBuilder::from_oval(oval) {
            let stroke = Stroke { width, ..Stroke::default() };
            pixmap.stroke_path(&path, &paint, &stroke, tilt, None);
        }
    }

    // A few stars, placed by a small fixed sequence so every render is the same.
    let star = if light { rgba(0xFFFFFF, 0.9) } else { rgba(0xFFFFFF, 0.8) };
    let paint = Paint { shader: tiny_skia::Shader::SolidColor(star), anti_alias: true, ..Paint::default() };
    let mut seed: u32 = 0x9E37_79B9;
    for _ in 0..70 {
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f32 / u32::MAX as f32
        };
        let (x, y, r) = (next() * e, next() * e, 0.6 + next() * 1.6);
        if let Some(path) = PathBuilder::from_circle(x, y, r) {
            pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }
    }
    pixmap
}

fn frost(wallpaper: &Pixmap) -> Pixmap {
    let mut small = Pixmap::new(FROST_EDGE, FROST_EDGE).expect("pixmap");
    let scale = FROST_EDGE as f32 / EDGE as f32;
    let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..PixmapPaint::default() };
    small.draw_pixmap(0, 0, wallpaper.as_ref(), &paint, Transform::from_scale(scale, scale), None);
    let (w, h) = (FROST_EDGE as usize, FROST_EDGE as usize);
    let data = small.data_mut();
    for _ in 0..3 {
        box_pass(data, w, h, FROST_RADIUS, true);
        box_pass(data, w, h, FROST_RADIUS, false);
    }
    small
}

/// One box blur pass along rows (`horizontal`) or columns, edges clamped. Works on premultiplied
/// RGBA, which is the right space to blur in.
fn box_pass(px: &mut [u8], w: usize, h: usize, radius: usize, horizontal: bool) {
    let (lines, len) = if horizontal { (h, w) } else { (w, h) };
    let index = |line: usize, i: usize| if horizontal { (line * w + i) * 4 } else { (i * w + line) * 4 };
    let span = (2 * radius + 1) as u32;
    let mut out = vec![0u8; len * 4];
    for line in 0..lines {
        for c in 0..4 {
            let at = |i: isize| px[index(line, i.clamp(0, len as isize - 1) as usize) + c] as u32;
            let mut sum: u32 = (-(radius as isize)..=radius as isize).map(at).sum();
            for i in 0..len {
                out[i * 4 + c] = ((sum + span / 2) / span) as u8;
                sum += at(i as isize + radius as isize + 1);
                sum -= at(i as isize - radius as isize);
            }
        }
        for i in 0..len {
            px[index(line, i)..index(line, i) + 4].copy_from_slice(&out[i * 4..i * 4 + 4]);
        }
    }
}

fn to_image(pixmap: &Pixmap) -> Image {
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(pixmap.data(), pixmap.width(), pixmap.height());
    Image::from_rgba8_premultiplied(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_box_pass_keeps_a_flat_field_flat_and_spreads_a_spike() {
        let (w, h) = (9, 1);
        let mut flat = vec![100u8; w * h * 4];
        box_pass(&mut flat, w, h, 2, true);
        assert!(flat.iter().all(|&v| v == 100));

        let mut spike = vec![0u8; w * h * 4];
        spike[4 * 4] = 250;
        box_pass(&mut spike, w, h, 2, true);
        let reds: Vec<u8> = spike.chunks(4).map(|p| p[0]).collect();
        assert_eq!(reds, vec![0, 0, 50, 50, 50, 50, 50, 0, 0]);
    }

    #[test]
    fn the_frost_is_smoother_than_the_wallpaper() {
        let wall = wallpaper(false);
        let frosted = frost(&wall);
        let roughness = |p: &Pixmap| {
            let d = p.data();
            let w = p.width() as usize * 4;
            (0..d.len() - 4).filter(|i| (i + 4) % w != 0).map(|i| (d[i] as i32 - d[i + 4] as i32).unsigned_abs()).sum::<u32>() as f64
                / d.len() as f64
        };
        let small = {
            let mut s = Pixmap::new(FROST_EDGE, FROST_EDGE).unwrap();
            let k = FROST_EDGE as f32 / EDGE as f32;
            s.draw_pixmap(0, 0, wall.as_ref(), &PixmapPaint::default(), Transform::from_scale(k, k), None);
            s
        };
        assert!(roughness(&frosted) < roughness(&small) / 2.0);
    }
}
