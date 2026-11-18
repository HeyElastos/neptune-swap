//! The wallpaper under the glass and its frost: a deep blue field with soft light and slow waves of
//! fine lines, drawn once; then a blurred copy that every glass panel shows the part of behind
//! it (downscale, then three box passes each way).

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use tiny_skia::{
    Color, FillRule, FilterQuality, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, PixmapPaint, Point, RadialGradient,
    Rect, SpreadMode, Stroke, Transform,
};

/// Square, so `image-fit: cover` crops it the same for the wallpaper and the frost at any window
/// shape.
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

/// The curve of one wave: y = y0 + a·sin(k·x + phase) + b·sin(2.3·k·x − 0.7·phase), sampled
/// across the width.
fn wave(y0: f32, a: f32, b: f32, k: f32, phase: f32) -> impl Fn(f32) -> f32 {
    move |x| y0 + a * (k * x + phase).sin() + b * (2.3 * k * x - 0.7 * phase).sin()
}

fn curve(f: &impl Fn(f32) -> f32, close_below: bool) -> Option<tiny_skia::Path> {
    let e = EDGE as f32;
    let mut pb = PathBuilder::new();
    pb.move_to(0.0, f(0.0));
    for i in 1..=120 {
        let x = e * i as f32 / 120.0;
        pb.line_to(x, f(x));
    }
    if close_below {
        pb.line_to(e, e);
        pb.line_to(0.0, e);
        pb.close();
    }
    pb.finish()
}

/// A paint fading in from the left edge and out at the right, `hex` at `alpha` in between.
fn sweep(hex: u32, alpha: f32) -> Paint<'static> {
    let e = EDGE as f32;
    let stops = vec![
        GradientStop::new(0.0, rgba(hex, alpha * 0.15)),
        GradientStop::new(0.45, rgba(hex, alpha)),
        GradientStop::new(1.0, rgba(hex, alpha * 0.25)),
    ];
    let shader = LinearGradient::new(Point::from_xy(0.0, 0.0), Point::from_xy(e, e * 0.2), stops, SpreadMode::Pad, Transform::identity())
        .unwrap_or(tiny_skia::Shader::SolidColor(rgba(hex, alpha)));
    Paint { shader, anti_alias: true, ..Paint::default() }
}

fn wallpaper(light: bool) -> Pixmap {
    let mut pixmap = Pixmap::new(EDGE, EDGE).expect("pixmap");
    let e = EDGE as f32;
    // A deep blue field (never black) with soft light.
    if light {
        pixmap.fill(rgba(0xDDE7F6, 1.0));
        glow(&mut pixmap, e * 0.15, e * 0.05, e * 0.80, 0x9EC0FF, 0.85);
        glow(&mut pixmap, e * 0.98, e * 0.55, e * 0.70, 0x8EE0D6, 0.55);
        glow(&mut pixmap, e * 0.05, e * 0.95, e * 0.70, 0xC9B8FF, 0.60);
    } else {
        pixmap.fill(rgba(0x0B1640, 1.0));
        glow(&mut pixmap, e * 0.10, e * 0.02, e * 0.95, 0x2A55E0, 0.75);
        glow(&mut pixmap, e * 1.00, e * 0.45, e * 0.80, 0x1287C9, 0.50);
        glow(&mut pixmap, e * 0.05, e * 1.00, e * 0.80, 0x5A3FD0, 0.50);
        glow(&mut pixmap, e * 0.55, e * 0.62, e * 0.45, 0x3D7BFF, 0.22);
    }

    // Waves: soft filled swells, then ribbons of fine lines riding over them.
    let line = if light { 0xFFFFFF } else { 0xBFD6FF };
    let k = std::f32::consts::TAU / e;
    let swells = [(0.58f32, 0.07f32, 0.02f32, 0.9f32, 0.6f32), (0.70, 0.06, 0.025, 1.2, 2.1), (0.82, 0.05, 0.02, 0.8, 4.0)];
    for (y, a, b, f, phase) in swells {
        let w = wave(e * y, e * a, e * b, k * f, phase);
        if let Some(path) = curve(&w, true) {
            pixmap.fill_path(&path, &sweep(line, if light { 0.10 } else { 0.06 }), FillRule::Winding, Transform::identity(), None);
        }
    }
    let ribbons = [(0.50f32, 0.09f32, 0.03f32, 0.85f32, 0.3f32, 0.55f32), (0.66, 0.07, 0.03, 1.15, 2.4, 0.40)];
    for (y, a, b, f, phase, alpha) in ribbons {
        for i in 0..28 {
            let t = i as f32 / 27.0;
            // The lines fan out from one edge of the ribbon to the other, brightest at its heart.
            let w = wave(e * (y + t * 0.06), e * a * (1.0 - 0.35 * t), e * b, k * f, phase + t * 0.9);
            let strength = alpha * (1.0 - (2.0 * t - 1.0).abs()).powf(0.7) * 0.35 + 0.02;
            if let Some(path) = curve(&w, false) {
                let stroke = Stroke { width: 1.1, ..Stroke::default() };
                pixmap.stroke_path(&path, &sweep(line, strength), &stroke, Transform::identity(), None);
            }
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
