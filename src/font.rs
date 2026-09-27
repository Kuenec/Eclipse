use freetype::bitmap::PixelMode;
use freetype::face::LoadFlag;
use freetype::{Face, Library, Matrix, Vector};
use std::sync::Arc;

const MAX_GLYPH_DIMENSION: u32 = 4096;
const LOAD_METRICS: LoadFlag = LoadFlag::from_bits_retain(
    LoadFlag::NO_HINTING.bits() | LoadFlag::NO_BITMAP.bits() | LoadFlag::TARGET_NORMAL.bits(),
);
const LOAD_BITMAP: LoadFlag =
    LoadFlag::from_bits_retain(LOAD_METRICS.bits() | LoadFlag::RENDER.bits());
const LOAD_OUTLINE_IMAGE: LoadFlag =
    LoadFlag::from_bits_retain(LOAD_BITMAP.bits() | LoadFlag::COLOR.bits());
const LOAD_STRIKE_IMAGE: LoadFlag = LoadFlag::from_bits_retain(
    LoadFlag::NO_HINTING.bits()
        | LoadFlag::TARGET_NORMAL.bits()
        | LoadFlag::RENDER.bits()
        | LoadFlag::COLOR.bits(),
);

type MemoryFace = Face<Arc<[u8]>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GlyphSource {
    Outlines,
    ColorStrikes,
}

pub(crate) struct RasterFont {
    data: Arc<[u8]>,
    index: u32,
    source: GlyphSource,
    color: bool,
    units_per_em: f32,
    ascent_per_em: f32,
    descent_per_em: f32,
    ppem_per_height: f32,
    ascent_per_height: f32,
    line_gap_per_height: f32,
}

impl RasterFont {
    pub(crate) fn open(data: Arc<[u8]>, index: u32) -> Result<Self, String> {
        let face = load_face(data.clone(), index)?;
        let source = if face.is_scalable() {
            GlyphSource::Outlines
        } else if face.has_fixed_sizes() && face.has_color() {
            GlyphSource::ColorStrikes
        } else {
            return Err("font has no scalable outline or colour bitmap strikes".to_string());
        };
        let (ascent, descent, line_height, em_size) = match source {
            GlyphSource::Outlines => (
                f32::from(face.ascender()),
                f32::from(face.descender()),
                f32::from(face.height()),
                f32::from(face.em_size()),
            ),
            GlyphSource::ColorStrikes => {
                let table = rustybuzz::ttf_parser::Face::parse(&data, index)
                    .map_err(|error| format!("colour bitmap font tables: {error}"))?;
                let ascent = f32::from(table.ascender());
                let descent = f32::from(table.descender());
                (
                    ascent,
                    descent,
                    ascent - descent + f32::from(table.line_gap()),
                    f32::from(table.units_per_em()),
                )
            }
        };
        let height = ascent - descent;
        if em_size <= 0.0 || !height.is_finite() || height <= 0.0 {
            return Err("font has invalid vertical metrics".to_string());
        }
        Ok(Self {
            color: face.has_color(),
            data,
            index,
            source,
            units_per_em: em_size,
            ascent_per_em: ascent / em_size,
            descent_per_em: -descent / em_size,
            ppem_per_height: em_size / height,
            ascent_per_height: ascent / height,
            line_gap_per_height: (line_height - height) / height,
        })
    }

    pub(crate) fn data(&self) -> &[u8] {
        &self.data
    }

    pub(crate) fn index(&self) -> u32 {
        self.index
    }

    pub(crate) fn glyph_source(&self) -> GlyphSource {
        self.source
    }

    pub(crate) fn has_color(&self) -> bool {
        self.color
    }

    pub(crate) fn units_per_em(&self) -> f32 {
        self.units_per_em
    }

    pub(crate) fn ascent_per_em(&self) -> f32 {
        self.ascent_per_em
    }

    pub(crate) fn descent_per_em(&self) -> f32 {
        self.descent_per_em
    }

    pub(crate) fn em_per_height(&self) -> f32 {
        self.ppem_per_height
    }

    pub(crate) fn scaled(&self, height: f32) -> Option<ScaledFont> {
        let height = if height.is_finite() && height > 0.0 {
            height.min(MAX_GLYPH_DIMENSION as f32)
        } else {
            1.0
        };
        let mut scaled = self.at_em(height * self.ppem_per_height)?;
        scaled.height = height;
        scaled.ascent = height * self.ascent_per_height;
        scaled.line_gap = height * self.line_gap_per_height;
        Some(scaled)
    }

    pub(crate) fn at_em(&self, ppem: f32) -> Option<ScaledFont> {
        let ppem = if ppem.is_finite() && ppem > 0.0 {
            ppem.min(MAX_GLYPH_DIMENSION as f32)
        } else {
            1.0
        };
        let mut face = load_face(self.data.clone(), self.index).ok()?;
        let strike_scale = match self.source {
            GlyphSource::Outlines => {
                let char_height = (ppem * 64.0)
                    .round()
                    .clamp(64.0, (MAX_GLYPH_DIMENSION * 64) as f32)
                    as isize;
                face.set_char_size(0, char_height, 72, 72).ok()?;
                1.0
            }
            GlyphSource::ColorStrikes => {
                let (strike, strike_ppem) = nearest_strike(&face, ppem)?;
                select_strike(&mut face, strike)?;
                ppem / strike_ppem
            }
        };
        let height = ppem / self.ppem_per_height;
        Some(ScaledFont {
            face,
            source: self.source,
            strike_scale,
            height,
            ascent: height * self.ascent_per_height,
            line_gap: height * self.line_gap_per_height,
        })
    }
}

fn nearest_strike(face: &MemoryFace, ppem: f32) -> Option<(i32, f32)> {
    let raw = face.raw();
    let count = usize::try_from(raw.num_fixed_sizes).ok()?;
    if count == 0 || raw.available_sizes.is_null() {
        return None;
    }
    let sizes = unsafe { std::slice::from_raw_parts(raw.available_sizes, count) };
    let strikes = sizes
        .iter()
        .enumerate()
        .map(|(index, size)| (index as i32, size.y_ppem as f32 / 64.0))
        .filter(|&(_, strike_ppem)| strike_ppem > 0.0);
    let larger = strikes
        .clone()
        .filter(|&(_, strike_ppem)| strike_ppem >= ppem)
        .min_by(|a, b| a.1.total_cmp(&b.1));
    larger.or_else(|| strikes.max_by(|a, b| a.1.total_cmp(&b.1)))
}

fn select_strike(face: &mut MemoryFace, strike: i32) -> Option<()> {
    let raw: *mut freetype::ffi::FT_FaceRec = face.raw_mut();
    let error = unsafe { freetype::ffi::FT_Select_Size(raw, strike) };
    (error == freetype::ffi::FT_Err_Ok).then_some(())
}

fn load_face(data: Arc<[u8]>, index: u32) -> Result<MemoryFace, String> {
    let library = Library::init().map_err(|error| format!("{error:?}"))?;
    library
        .new_memory_face2(data, index as isize)
        .map_err(|error| format!("{error:?}"))
}

pub(crate) enum GlyphPixels {
    Coverage(Vec<u8>),
    Color(Vec<[u8; 4]>),
}

pub(crate) struct GlyphImage {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: GlyphPixels,
}

pub(crate) struct ScaledFont {
    face: MemoryFace,
    source: GlyphSource,
    strike_scale: f32,
    height: f32,
    ascent: f32,
    line_gap: f32,
}

impl ScaledFont {
    pub(crate) fn ascent(&self) -> f32 {
        self.ascent
    }

    pub(crate) fn height(&self) -> f32 {
        self.height
    }

    pub(crate) fn line_gap(&self) -> f32 {
        self.line_gap
    }

    pub(crate) fn advance(&mut self, character: char) -> f32 {
        self.reset_transform();
        if self
            .face
            .load_char(character as usize, LOAD_METRICS)
            .is_err()
        {
            return 0.0;
        }
        self.face.glyph().linear_hori_advance() as f32 / 65536.0
    }

    pub(crate) fn glyph(&mut self, character: char) -> Option<RasterGlyph<'_>> {
        self.glyph_at(character, 0.0, 0.0)
    }

    pub(crate) fn glyph_at(
        &mut self,
        character: char,
        position_x: f32,
        position_y: f32,
    ) -> Option<RasterGlyph<'_>> {
        if !position_x.is_finite() || !position_y.is_finite() {
            return None;
        }
        self.set_subpixel_transform(position_x, position_y);
        self.face.load_char(character as usize, LOAD_BITMAP).ok()?;
        let slot = self.face.glyph();
        let bitmap = slot.bitmap();
        let width = u32::try_from(bitmap.width()).ok()?;
        let height = u32::try_from(bitmap.rows()).ok()?;
        let bitmap_left = slot.bitmap_left();
        let bitmap_top = slot.bitmap_top();
        if width == 0 || height == 0 || width > MAX_GLYPH_DIMENSION || height > MAX_GLYPH_DIMENSION
        {
            return None;
        }
        Some(RasterGlyph {
            font: self,
            placement: GlyphPlacement {
                left: position_x.trunc() as i32 + bitmap_left,
                top: position_y.trunc() as i32 - bitmap_top,
                width,
                height,
            },
        })
    }

    pub(crate) fn render(
        &mut self,
        glyph_id: u32,
        position_x: f32,
        position_y: f32,
    ) -> Option<GlyphImage> {
        if !position_x.is_finite() || !position_y.is_finite() {
            return None;
        }
        match self.source {
            GlyphSource::Outlines => {
                self.set_subpixel_transform(position_x, position_y);
                self.face.load_glyph(glyph_id, LOAD_OUTLINE_IMAGE).ok()?;
                let (left, top) = self.slot_bearing();
                let (width, height, pixels) = self.slot_pixels()?;
                Some(GlyphImage {
                    left: position_x.trunc() as i32 + left,
                    top: position_y.trunc() as i32 - top,
                    width,
                    height,
                    pixels,
                })
            }
            GlyphSource::ColorStrikes => {
                self.reset_transform();
                self.face.load_glyph(glyph_id, LOAD_STRIKE_IMAGE).ok()?;
                let (left, top) = self.slot_bearing();
                let (width, height, pixels) = self.slot_pixels()?;
                let scale = self.strike_scale;
                let (width, height, pixels) = resample(width, height, &pixels, scale)?;
                Some(GlyphImage {
                    left: position_x.round() as i32 + (left as f32 * scale).round() as i32,
                    top: position_y.round() as i32 - (top as f32 * scale).round() as i32,
                    width,
                    height,
                    pixels,
                })
            }
        }
    }

    fn slot_bearing(&self) -> (i32, i32) {
        let slot = self.face.glyph();
        (slot.bitmap_left(), slot.bitmap_top())
    }

    fn slot_pixels(&self) -> Option<(u32, u32, GlyphPixels)> {
        let bitmap = self.face.glyph().bitmap();
        let width = u32::try_from(bitmap.width()).ok()?;
        let height = u32::try_from(bitmap.rows()).ok()?;
        if width == 0 || height == 0 || width > MAX_GLYPH_DIMENSION || height > MAX_GLYPH_DIMENSION
        {
            return None;
        }
        let stride = bitmap.pitch().unsigned_abs() as usize;
        let buffer = bitmap.buffer();
        let row_start = |y: usize| {
            let source_y = if bitmap.pitch() >= 0 {
                y
            } else {
                height as usize - 1 - y
            };
            source_y * stride
        };
        let (w, h) = (width as usize, height as usize);
        let pixels = match bitmap.pixel_mode().ok()? {
            PixelMode::Gray => {
                let maximum = u32::from(bitmap.raw().num_grays.saturating_sub(1).max(1) as u16);
                let mut coverage = Vec::with_capacity(w * h);
                for y in 0..h {
                    let row = buffer.get(row_start(y)..row_start(y) + w)?;
                    coverage
                        .extend(row.iter().map(|&value| {
                            ((u32::from(value) * 255 + maximum / 2) / maximum) as u8
                        }));
                }
                GlyphPixels::Coverage(coverage)
            }
            PixelMode::Bgra => {
                let mut color = Vec::with_capacity(w * h);
                for y in 0..h {
                    let row = buffer.get(row_start(y)..row_start(y) + w * 4)?;
                    color.extend(
                        row.as_chunks::<4>()
                            .0
                            .iter()
                            .map(|&[b, g, r, a]| [r, g, b, a]),
                    );
                }
                GlyphPixels::Color(color)
            }
            _ => return None,
        };
        Some((width, height, pixels))
    }

    fn reset_transform(&self) {
        let mut matrix = identity_matrix();
        let mut delta = Vector { x: 0, y: 0 };
        self.face.set_transform(&mut matrix, &mut delta);
    }

    fn set_subpixel_transform(&self, position_x: f32, position_y: f32) {
        let mut matrix = identity_matrix();
        let mut delta = Vector {
            x: (position_x.fract() * 64.0).round() as _,
            y: (-position_y.fract() * 64.0).round() as _,
        };
        self.face.set_transform(&mut matrix, &mut delta);
    }
}

fn resample(
    width: u32,
    height: u32,
    pixels: &GlyphPixels,
    scale: f32,
) -> Option<(u32, u32, GlyphPixels)> {
    let target =
        |length: u32| ((length as f32 * scale).round() as u32).clamp(1, MAX_GLYPH_DIMENSION);
    let (target_width, target_height) = (target(width), target(height));
    let area = |length: u32, target: u32, index: u32| {
        let start = index as f32 * length as f32 / target as f32;
        let end = (index + 1) as f32 * length as f32 / target as f32;
        (start, end)
    };
    let mut sums: Vec<[f32; 4]> = vec![[0.0; 4]; (target_width * target_height) as usize];
    for ty in 0..target_height {
        let (y0, y1) = area(height, target_height, ty);
        for tx in 0..target_width {
            let (x0, x1) = area(width, target_width, tx);
            let mut sum = [0.0f32; 4];
            let mut weight = 0.0f32;
            for sy in y0.floor() as u32..(y1.ceil() as u32).min(height) {
                let wy = (y1.min(sy as f32 + 1.0) - y0.max(sy as f32)).max(0.0);
                for sx in x0.floor() as u32..(x1.ceil() as u32).min(width) {
                    let wx = (x1.min(sx as f32 + 1.0) - x0.max(sx as f32)).max(0.0);
                    let w = wx * wy;
                    let index = (sy * width + sx) as usize;
                    let texel = match pixels {
                        GlyphPixels::Coverage(values) => [0.0, 0.0, 0.0, f32::from(values[index])],
                        GlyphPixels::Color(values) => values[index].map(f32::from),
                    };
                    for (total, value) in sum.iter_mut().zip(texel) {
                        *total += value * w;
                    }
                    weight += w;
                }
            }
            if weight > 0.0 {
                sums[(ty * target_width + tx) as usize] = sum.map(|total| total / weight);
            }
        }
    }
    let to_byte = |value: f32| value.round().clamp(0.0, 255.0) as u8;
    let pixels = match pixels {
        GlyphPixels::Coverage(_) => {
            GlyphPixels::Coverage(sums.iter().map(|texel| to_byte(texel[3])).collect())
        }
        GlyphPixels::Color(_) => {
            GlyphPixels::Color(sums.iter().map(|texel| texel.map(to_byte)).collect())
        }
    };
    Some((target_width, target_height, pixels))
}

fn identity_matrix() -> Matrix {
    Matrix {
        xx: 0x1_0000,
        xy: 0,
        yx: 0,
        yy: 0x1_0000,
    }
}

#[derive(Clone, Copy)]
pub(crate) struct GlyphPlacement {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

pub(crate) struct RasterGlyph<'a> {
    font: &'a mut ScaledFont,
    placement: GlyphPlacement,
}

impl RasterGlyph<'_> {
    pub(crate) fn placement(&self) -> GlyphPlacement {
        self.placement
    }

    pub(crate) fn draw(&self, mut draw_pixel: impl FnMut(u32, u32, f32)) -> bool {
        let bitmap = self.font.face.glyph().bitmap();
        if bitmap.pixel_mode().ok() != Some(PixelMode::Gray) {
            return false;
        }
        let width = self.placement.width as usize;
        let height = self.placement.height as usize;
        let stride = bitmap.pitch().unsigned_abs() as usize;
        let buffer = bitmap.buffer();
        let maximum = f32::from(bitmap.raw().num_grays.saturating_sub(1).max(1));
        for y in 0..height {
            let source_y = if bitmap.pitch() >= 0 {
                y
            } else {
                height - 1 - y
            };
            let row = source_y.saturating_mul(stride);
            for x in 0..width {
                let Some(&coverage) = buffer.get(row.saturating_add(x)) else {
                    return false;
                };
                draw_pixel(x as u32, y as u32, f32::from(coverage) / maximum);
            }
        }
        true
    }
}
