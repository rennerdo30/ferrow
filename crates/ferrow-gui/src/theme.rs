//! Look and feel: the OS UI font and a Fluent-style palette that follows
//! the system light/dark setting and accent colour.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke,
    TextStyle, Visuals,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Family name for the semibold face used in headers.
pub const SEMIBOLD: &str = "ferrow-semibold";
/// Family name for the system icon font (Windows: Segoe Fluent Icons).
pub const ICONS: &str = "ferrow-icons";

/// Whether the system icon font was found.
pub fn has_icon_font(ctx: &egui::Context) -> bool {
    ctx.fonts(|f| f.definitions().font_data.contains_key("icons"))
}

/// Candidate font files per role, in preference order.
fn font_candidates() -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let mut regular = Vec::new();
    let mut semibold = Vec::new();
    let mut fallback = Vec::new();
    if cfg!(windows) {
        let fonts = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("Fonts");
        regular.extend([fonts.join("segoeui.ttf"), fonts.join("arial.ttf")]);
        semibold.extend([fonts.join("seguisb.ttf"), fonts.join("segoeuib.ttf")]);
        fallback.extend([
            fonts.join("seguisym.ttf"),
            fonts.join("YuGothR.ttc"),
            fonts.join("msyh.ttc"),
            fonts.join("malgun.ttf"),
            fonts.join("Nirmala.ttc"),
        ]);
    } else if cfg!(target_os = "macos") {
        let sys = PathBuf::from("/System/Library/Fonts");
        regular.extend([sys.join("SFNS.ttf"), sys.join("Helvetica.ttc")]);
        semibold.extend([sys.join("SFNS.ttf")]);
        fallback.extend([
            sys.join("Hiragino Sans GB.ttc"),
            sys.join("AppleSDGothicNeo.ttc"),
            sys.join("Apple Symbols.ttf"),
        ]);
    } else {
        for dir in ["/usr/share/fonts", "/usr/local/share/fonts"] {
            let d = PathBuf::from(dir);
            regular.extend([
                d.join("truetype/noto/NotoSans-Regular.ttf"),
                d.join("noto/NotoSans-Regular.ttf"),
                d.join("truetype/dejavu/DejaVuSans.ttf"),
                d.join("TTF/DejaVuSans.ttf"),
                d.join("dejavu/DejaVuSans.ttf"),
                d.join("truetype/liberation/LiberationSans-Regular.ttf"),
            ]);
            semibold.extend([
                d.join("truetype/noto/NotoSans-SemiBold.ttf"),
                d.join("noto/NotoSans-SemiBold.ttf"),
                d.join("truetype/dejavu/DejaVuSans-Bold.ttf"),
                d.join("TTF/DejaVuSans-Bold.ttf"),
            ]);
            fallback.extend([
                d.join("opentype/noto/NotoSansCJK-Regular.ttc"),
                d.join("noto-cjk/NotoSansCJK-Regular.ttc"),
            ]);
        }
    }
    (regular, semibold, fallback)
}

fn load(path: &PathBuf) -> Option<Arc<FontData>> {
    let bytes = std::fs::read(path).ok()?;
    Some(Arc::new(FontData::from_owned(bytes)))
}

/// Install the OS UI font. Returns an error when no usable font exists;
/// egui ships no font of its own in this build (licence policy).
pub fn install_fonts(ctx: &egui::Context) -> Result<(), String> {
    let (regular, semibold, fallback) = font_candidates();
    let mut defs = FontDefinitions::empty();
    let regular_data = regular
        .iter()
        .find_map(load)
        .ok_or_else(|| format!("no UI font found (looked for {})", regular[0].display()))?;
    defs.font_data.insert("ui".into(), regular_data);
    let mut chain = vec!["ui".to_owned()];
    for (i, path) in fallback.iter().enumerate() {
        if let Some(data) = load(path) {
            let name = format!("fallback-{i}");
            defs.font_data.insert(name.clone(), data);
            chain.push(name);
        }
    }
    let semibold_name = match semibold.iter().find_map(load) {
        Some(data) => {
            defs.font_data.insert("ui-semibold".into(), data);
            "ui-semibold".to_owned()
        }
        None => "ui".to_owned(),
    };
    let mut semibold_chain = vec![semibold_name];
    semibold_chain.extend(chain.iter().cloned());
    defs.families
        .insert(FontFamily::Proportional, chain.clone());
    defs.families.insert(FontFamily::Monospace, chain);
    defs.families
        .insert(FontFamily::Name(SEMIBOLD.into()), semibold_chain);
    let mut icon_chain = Vec::new();
    if cfg!(windows) {
        let fonts = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("Fonts");
        // Windows 11 ships Segoe Fluent Icons; Windows 10 has MDL2 Assets
        // with the same caption glyph code points.
        if let Some(data) = [fonts.join("SegoeIcons.ttf"), fonts.join("segmdl2.ttf")]
            .iter()
            .find_map(load)
        {
            defs.font_data.insert("icons".into(), data);
            icon_chain.push("icons".to_owned());
        }
    }
    icon_chain.push("ui".to_owned());
    defs.families
        .insert(FontFamily::Name(ICONS.into()), icon_chain);
    ctx.set_fonts(defs);
    Ok(())
}

/// Fluent-style colours for one theme.
#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub accent: Color32,
    /// Accent for text and selection marks on the current background.
    pub accent_text: Color32,
    pub text: Color32,
    pub text_secondary: Color32,
    pub text_tertiary: Color32,
    /// Opaque window background when no OS backdrop is available.
    pub window: Color32,
    /// The content "card" layered on the backdrop.
    pub card: Color32,
    pub card_stroke: Color32,
    pub control: Color32,
    pub control_hover: Color32,
    pub control_stroke: Color32,
    pub row_hover: Color32,
    pub row_selected: Color32,
    pub divider: Color32,
    pub highlight: Color32,
    pub folder: Color32,
    pub folder_dark: Color32,
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

impl Palette {
    pub fn new(dark: bool, accent: Option<[u8; 3]>) -> Palette {
        let base = accent
            .map(|[r, g, b]| Color32::from_rgb(r, g, b))
            .unwrap_or(if dark {
                Color32::from_rgb(0x00, 0x78, 0xD4)
            } else {
                Color32::from_rgb(0x00, 0x5F, 0xB8)
            });
        let a = |c: Color32, alpha: u8| Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha);
        if dark {
            let accent_text = mix(base, Color32::WHITE, 0.45);
            Palette {
                dark,
                accent: base,
                accent_text,
                text: Color32::from_rgb(0xFF, 0xFF, 0xFF),
                text_secondary: Color32::from_rgba_unmultiplied(255, 255, 255, 200),
                text_tertiary: Color32::from_rgba_unmultiplied(255, 255, 255, 140),
                window: Color32::from_rgb(0x20, 0x20, 0x20),
                card: Color32::from_rgba_unmultiplied(0x3A, 0x3A, 0x3A, 0x4C),
                card_stroke: Color32::from_rgba_unmultiplied(0, 0, 0, 0x30),
                control: Color32::from_rgba_unmultiplied(255, 255, 255, 0x0F),
                control_hover: Color32::from_rgba_unmultiplied(255, 255, 255, 0x15),
                control_stroke: Color32::from_rgba_unmultiplied(255, 255, 255, 0x12),
                row_hover: Color32::from_rgba_unmultiplied(255, 255, 255, 0x0F),
                row_selected: Color32::from_rgba_unmultiplied(255, 255, 255, 0x15),
                divider: Color32::from_rgba_unmultiplied(255, 255, 255, 0x15),
                highlight: a(accent_text, 0x40),
                folder: Color32::from_rgb(0xF2, 0xC5, 0x5C),
                folder_dark: Color32::from_rgb(0xD9, 0xA4, 0x30),
            }
        } else {
            Palette {
                dark,
                accent: base,
                accent_text: base,
                text: Color32::from_rgba_unmultiplied(0, 0, 0, 0xE4),
                text_secondary: Color32::from_rgba_unmultiplied(0, 0, 0, 0x9E),
                text_tertiary: Color32::from_rgba_unmultiplied(0, 0, 0, 0x72),
                window: Color32::from_rgb(0xF3, 0xF3, 0xF3),
                card: Color32::from_rgba_unmultiplied(255, 255, 255, 0xB3),
                card_stroke: Color32::from_rgba_unmultiplied(0, 0, 0, 0x0F),
                control: Color32::from_rgba_unmultiplied(255, 255, 255, 0xB3),
                control_hover: Color32::from_rgba_unmultiplied(0xF9, 0xF9, 0xF9, 0x80),
                control_stroke: Color32::from_rgba_unmultiplied(0, 0, 0, 0x13),
                row_hover: Color32::from_rgba_unmultiplied(0, 0, 0, 0x09),
                row_selected: Color32::from_rgba_unmultiplied(0, 0, 0, 0x0F),
                divider: Color32::from_rgba_unmultiplied(0, 0, 0, 0x0F),
                highlight: a(base, 0x30),
                folder: Color32::from_rgb(0xFF, 0xCA, 0x4F),
                folder_dark: Color32::from_rgb(0xE8, 0xA8, 0x23),
            }
        }
    }

    /// egui visuals matching the palette, for menus, tooltips and the
    /// text cursor.
    pub fn visuals(&self) -> Visuals {
        let mut v = if self.dark {
            Visuals::dark()
        } else {
            Visuals::light()
        };
        v.panel_fill = Color32::TRANSPARENT;
        v.window_fill = if self.dark {
            Color32::from_rgb(0x2C, 0x2C, 0x2C)
        } else {
            Color32::from_rgb(0xF9, 0xF9, 0xF9)
        };
        v.window_stroke = Stroke::new(1.0, self.control_stroke);
        v.window_corner_radius = CornerRadius::same(8);
        v.menu_corner_radius = CornerRadius::same(8);
        v.override_text_color = Some(self.text);
        v.selection.bg_fill = self.highlight;
        v.selection.stroke = Stroke::new(1.0, self.text);
        v.text_cursor.stroke = Stroke::new(1.5, self.text);
        // No blinking caret: it would repaint twice a second while idle,
        // costing CPU and remote-desktop bandwidth.
        v.text_cursor.blink = false;
        v.extreme_bg_color = Color32::TRANSPARENT;
        v.hyperlink_color = self.accent_text;
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(4);
            w.fg_stroke = Stroke::new(1.0, self.text);
            w.bg_stroke = Stroke::NONE;
            w.expansion = 0.0;
        }
        v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
        v.widgets.inactive.bg_fill = self.control;
        v.widgets.hovered.weak_bg_fill = self.row_hover;
        v.widgets.hovered.bg_fill = self.control_hover;
        v.widgets.active.weak_bg_fill = self.row_selected;
        v.widgets.active.bg_fill = self.row_selected;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, self.divider);
        v
    }
}

/// Text sizes from the Windows type ramp (Body 14, Caption 12).
pub fn apply_style(ctx: &egui::Context, palette: &Palette) {
    ctx.global_style_mut(|style| {
        style.visuals = palette.visuals();
        style.text_styles = [
            (TextStyle::Small, FontId::proportional(12.0)),
            (TextStyle::Body, FontId::proportional(14.0)),
            (TextStyle::Button, FontId::proportional(14.0)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
            (
                TextStyle::Heading,
                FontId::new(20.0, FontFamily::Name(SEMIBOLD.into())),
            ),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 4.0);
        style.spacing.button_padding = egui::vec2(11.0, 5.0);
        style.spacing.menu_margin = Margin::same(4);
        style.spacing.interact_size.y = 32.0;
        style.interaction.selectable_labels = false;
    });
}
