//! Offscreen drawing for the 480x480 LCD: tiny-skia for shapes and images,
//! cosmic-text for text, jpeg-encoder for the wire format.

use std::path::Path;

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, Weight};
use tiny_skia::{
    Color, ColorU8, FilterQuality, LineCap, Paint, PathBuilder, Pixmap, PixmapPaint, PremultipliedColorU8,
    Stroke, Transform,
};

pub const SIZE: u32 = 480;
const JPEG_QUALITY: u8 = 85;

pub const fn rgb(r: u8, g: u8, b: u8) -> ColorU8 {
    ColorU8::from_rgba(r, g, b, 255)
}

pub fn new_canvas(background: ColorU8) -> Pixmap {
    let mut pixmap = Pixmap::new(SIZE, SIZE).expect("non-zero size");
    let c = background;
    pixmap.fill(Color::from_rgba8(c.red(), c.green(), c.blue(), c.alpha()));
    pixmap
}

/// Encodes the canvas as the baseline JPEG the device expects.
pub fn to_jpeg(pixmap: &Pixmap) -> Vec<u8> {
    // Canvas content is opaque, so premultiplied RGBA equals straight RGBA.
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY)
        .encode(pixmap.data(), SIZE as u16, SIZE as u16, jpeg_encoder::ColorType::Rgba)
        .expect("in-memory encode");
    out
}

/// Loads any image file, center-crops it to a square and scales it to the LCD size.
pub fn load_image(path: &Path) -> image::ImageResult<Pixmap> {
    Ok(fit_to_canvas(image::open(path)?, image::imageops::FilterType::Lanczos3))
}

/// Center-crops to a square, scales to the LCD size and flattens transparency onto black.
pub fn fit_to_canvas(img: image::DynamicImage, filter: image::imageops::FilterType) -> Pixmap {
    // Cropping first means the resize only touches pixels that end up visible.
    let side = img.width().min(img.height());
    let (x, y) = ((img.width() - side) / 2, (img.height() - side) / 2);
    let img = img.crop_imm(x, y, side, side);
    let img = if side == SIZE { img } else { img.resize_exact(SIZE, SIZE, filter) }.into_rgba8();
    let mut pixmap = new_canvas(rgb(0, 0, 0));
    for (src, dst) in img.pixels().zip(pixmap.pixels_mut()) {
        let [r, g, b, a] = src.0;
        let s = |c: u8| (c as u16 * a as u16 / 255) as u8;
        *dst = PremultipliedColorU8::from_rgba(s(r), s(g), s(b), 255).expect("opaque");
    }
    pixmap
}

/// Rotates the canvas clockwise about its center. Exposed corners take the
/// color of the source's top-left pixel so they blend with the background.
pub fn rotate(pixmap: &Pixmap, degrees: f32) -> Pixmap {
    if degrees.rem_euclid(360.0) == 0.0 {
        return pixmap.clone();
    }
    let corner = pixmap.pixels()[0].demultiply();
    let mut out = new_canvas(corner);
    let center = SIZE as f32 / 2.0;
    let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..PixmapPaint::default() };
    out.draw_pixmap(0, 0, pixmap.as_ref(), &paint, Transform::from_rotate_at(degrees, center, center), None);
    out
}

/// Strokes an arc around (cx, cy). Angles are degrees clockwise from 12 o'clock.
pub fn stroke_arc(pixmap: &mut Pixmap, cx: f32, cy: f32, r: f32, start: f32, sweep: f32, width: f32, color: ColorU8) {
    if sweep.abs() < 0.1 {
        return;
    }
    let steps = (sweep.abs() / 2.0).ceil().max(2.0) as usize;
    let mut pb = PathBuilder::new();
    for i in 0..=steps {
        let a = (start + sweep * i as f32 / steps as f32).to_radians();
        let (x, y) = (cx + r * a.sin(), cy - r * a.cos());
        if i == 0 { pb.move_to(x, y) } else { pb.line_to(x, y) }
    }
    let Some(path) = pb.finish() else { return };
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.red(), color.green(), color.blue(), color.alpha());
    let stroke = Stroke { width, line_cap: LineCap::Round, ..Stroke::default() };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

/// Text rendering state. Building it scans system fonts, so create it once.
pub struct TextRenderer {
    fonts: FontSystem,
    cache: SwashCache,
}

pub struct TextStyle<'a> {
    pub size: f32,
    pub weight: Weight,
    pub family: Family<'a>,
    pub color: ColorU8,
}

impl Default for TextStyle<'_> {
    fn default() -> Self {
        TextStyle { size: 32.0, weight: Weight::NORMAL, family: Family::Name("Pretendard"), color: rgb(255, 255, 255) }
    }
}

impl TextRenderer {
    /// Uses only the bundled Pretendard, the same face as the settings window. Skipping the
    /// system font scan also makes this quick and small.
    pub fn new() -> TextRenderer {
        const FACES: [&[u8]; 3] = [
            include_bytes!("../ui/fonts/Pretendard-Regular.otf"),
            include_bytes!("../ui/fonts/Pretendard-SemiBold.otf"),
            include_bytes!("../ui/fonts/Pretendard-Bold.otf"),
        ];
        let mut db = cosmic_text::fontdb::Database::new();
        for face in FACES {
            db.load_font_source(cosmic_text::fontdb::Source::Binary(std::sync::Arc::new(face)));
        }
        TextRenderer { fonts: FontSystem::new_with_locale_and_db("ko-KR".into(), db), cache: SwashCache::new() }
    }

    /// Draws one line of text horizontally centered on `cx`, vertically centered on `cy`.
    pub fn draw_centered(&mut self, pixmap: &mut Pixmap, text: &str, cx: f32, cy: f32, style: &TextStyle) {
        let metrics = Metrics::new(style.size, style.size * 1.2);
        let mut buffer = Buffer::new(&mut self.fonts, metrics);
        let attrs = Attrs::new().family(style.family).weight(style.weight);
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);
        let width = buffer.layout_runs().map(|run| run.line_w).fold(0.0, f32::max);
        let left = (cx - width / 2.0).round() as i32;
        let top = (cy - metrics.line_height / 2.0).round() as i32;

        let c = style.color;
        let color = cosmic_text::Color::rgba(c.red(), c.green(), c.blue(), c.alpha());
        buffer.draw(&mut self.fonts, &mut self.cache, color, |x, y, w, h, glyph| {
            for py in 0..h as i32 {
                for px in 0..w as i32 {
                    blend(pixmap, left + x + px, top + y + py, glyph);
                }
            }
        });
    }
}

/// Source-over blend of a straight-alpha color into one pixel.
fn blend(pixmap: &mut Pixmap, x: i32, y: i32, color: cosmic_text::Color) {
    if x < 0 || y < 0 || x >= SIZE as i32 || y >= SIZE as i32 || color.a() == 0 {
        return;
    }
    let i = (y as u32 * SIZE + x as u32) as usize;
    let dst = pixmap.pixels()[i];
    let a = color.a() as u32;
    let mix = |s: u8, d: u8| ((s as u32 * a + d as u32 * (255 - a)) / 255) as u8;
    pixmap.pixels_mut()[i] = PremultipliedColorU8::from_rgba(
        mix(color.r(), dst.red()),
        mix(color.g(), dst.green()),
        mix(color.b(), dst.blue()),
        255,
    )
    .expect("opaque");
}
