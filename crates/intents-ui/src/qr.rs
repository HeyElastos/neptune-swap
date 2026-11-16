//! QR codes for short (EC Hybrid) addresses, drawn into a Slint image.

use qrcode::{Color, EcLevel, QrCode};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Pixels per module, and the quiet zone in modules (the standard asks for 4;
/// the white tile around the image adds to it).
const SCALE: usize = 4;
const QUIET: usize = 2;

/// The QR code of `text`, black on white, or None when it does not fit.
/// Bech32 is case-insensitive; upper case uses QR's denser alphanumeric mode.
pub fn image(text: &str) -> Option<Image> {
    // M where it fits; a Generation address (3,482 characters) only fits at L,
    // in the largest version: a dense code, as the official wallet shows it too.
    let data = text.to_ascii_uppercase();
    let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::M)
        .or_else(|_| QrCode::with_error_correction_level(data.as_bytes(), EcLevel::L))
        .ok()?;
    let modules = code.width();
    let colors = code.to_colors();
    let side = (modules + 2 * QUIET) * SCALE;
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(side as u32, side as u32);
    let pixels = buffer.make_mut_slice();
    for (i, pixel) in pixels.iter_mut().enumerate() {
        let (x, y) = (i % side / SCALE, i / side / SCALE);
        let dark = x >= QUIET
            && y >= QUIET
            && x < modules + QUIET
            && y < modules + QUIET
            && colors[(y - QUIET) * modules + (x - QUIET)] == Color::Dark;
        let v = if dark { 0 } else { 255 };
        *pixel = Rgba8Pixel { r: v, g: v, b: v, a: 255 };
    }
    Some(Image::from_rgba8(buffer))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_short_address_fits_and_an_empty_one_still_encodes() {
        let short = format!("nolgaz1{}", "q".repeat(120));
        let image = super::image(&short).expect("fits");
        assert!(image.size().width > 0 && image.size().width == image.size().height);
        assert!(super::image("").is_some());
    }

    #[test]
    fn a_generation_address_fits_at_low_correction_and_more_does_not() {
        assert!(super::image(&format!("nolgam1{}", "q".repeat(3475))).is_some());
        assert!(super::image(&"q".repeat(9000)).is_none());
    }
}
