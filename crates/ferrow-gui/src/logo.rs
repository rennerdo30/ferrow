//! The Ferrow logo, pre-rendered from `docs/assets/logo.svg` at the
//! sizes the window needs, so it stays crisp without an SVG renderer at
//! run time. Regenerate the PNGs when the SVG changes.

use eframe::egui;

/// (pixel size, PNG bytes), smallest first.
static SIZES: &[(u32, &[u8])] = &[
    (18, include_bytes!("../assets/logo-18.png")),
    (23, include_bytes!("../assets/logo-23.png")),
    (27, include_bytes!("../assets/logo-27.png")),
    (32, include_bytes!("../assets/logo-32.png")),
    (36, include_bytes!("../assets/logo-36.png")),
    (45, include_bytes!("../assets/logo-45.png")),
    (64, include_bytes!("../assets/logo-64.png")),
    (256, include_bytes!("../assets/logo-256.png")),
];

/// Decode a PNG to straight (non-premultiplied) RGBA.
fn decode(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    Some((info.width, info.height, buf))
}

/// Index of the smallest pre-rendered size at least `pixels` wide.
fn pick(pixels: f32) -> usize {
    SIZES
        .iter()
        .position(|(s, _)| *s as f32 >= pixels)
        .unwrap_or(SIZES.len() - 1)
}

/// Window and taskbar icon.
pub fn window_icon() -> egui::IconData {
    let (width, height, rgba) = decode(SIZES[pick(256.0)].1).unwrap_or((1, 1, vec![0; 4]));
    egui::IconData {
        rgba,
        width,
        height,
    }
}

/// Logo texture for the title row, sized for `logical` points at the
/// current display scale. Cached per pixel size.
pub struct TitleLogo {
    texture: Option<(u32, egui::TextureHandle)>,
}

impl TitleLogo {
    pub fn new() -> TitleLogo {
        TitleLogo { texture: None }
    }

    pub fn texture(&mut self, ctx: &egui::Context, logical: f32) -> Option<&egui::TextureHandle> {
        let pixels = (logical * ctx.pixels_per_point()).ceil();
        let (size, bytes) = SIZES[pick(pixels)];
        if self.texture.as_ref().map(|(s, _)| *s) != Some(size) {
            let (w, h, rgba) = decode(bytes)?;
            let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
            let handle = ctx.load_texture("ferrow-logo", image, egui::TextureOptions::LINEAR);
            self.texture = Some((size, handle));
        }
        self.texture.as_ref().map(|(_, t)| t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_size_decodes_at_its_size() {
        for (size, bytes) in SIZES {
            let (w, h, rgba) = decode(bytes).expect("decodes");
            assert_eq!((w, h), (*size, *size));
            assert_eq!(rgba.len(), (*size * *size * 4) as usize);
        }
    }

    #[test]
    fn picks_the_next_larger_size() {
        assert_eq!(pick(18.0), 0);
        assert_eq!(pick(19.0), 1);
        assert_eq!(pick(9999.0), SIZES.len() - 1);
    }
}
