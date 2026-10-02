use freetype::bitmap::PixelMode;
use freetype::face::LoadFlag;
use freetype::{Face, Library, Matrix, Vector};
use harfrust::{Shaper, ShaperData};
use read_fonts::tables::cmap::{CmapSubtable, EncodingRecord, PlatformId};
use read_fonts::tables::os2::SelectionFlags;
use read_fonts::{FontRef, ReadError, TableProvider};
use std::sync::{Arc, OnceLock};

const MAX_GLYPH_DIMENSION: u32 = 4096;
const WINDOWS_UNICODE_BMP_ENCODING: u16 = 1;
const WINDOWS_UNICODE_FULL_ENCODING: u16 = 10;
const TYPO_METRICS_OS2_VERSION: u16 = 4;
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
    shaping: OnceLock<ShaperData>,
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
                let (units_per_em, metrics) = strike_font_metrics(&data, index)
                    .map_err(|error| format!("colour bitmap font tables: {error}"))?;
                (
                    metrics.ascent,
                    metrics.descent,
                    metrics.ascent - metrics.descent + metrics.line_gap,
                    units_per_em,
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
            shaping: OnceLock::new(),
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

    pub(crate) fn shaper(&self) -> Option<Shaper<'_>> {
        let font = FontRef::from_index(&self.data, self.index).ok()?;
        let shaping = self.shaping.get_or_init(|| ShaperData::new(&font));
        Some(shaping.shaper(&font).build())
    }

    pub(crate) fn glyph_index(&self, character: char) -> Option<u32> {
        nominal_glyph(&self.data, self.index, character)
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

fn nominal_glyph(data: &[u8], index: u32, character: char) -> Option<u32> {
    let font = FontRef::from_index(data, index).ok()?;
    let cmap = font.cmap().ok()?;
    let subtables = cmap.offset_data();
    cmap.encoding_records()
        .iter()
        .filter_map(|record| {
            let subtable = record.subtable(subtables).ok()?;
            maps_unicode(record, &subtable).then_some(subtable)
        })
        .find_map(|subtable| {
            subtable
                .map_codepoint(character)
                .map(|glyph| glyph.to_u32())
                .filter(|&glyph| glyph != 0)
        })
}

fn maps_unicode(record: &EncodingRecord, subtable: &CmapSubtable<'_>) -> bool {
    match (record.platform_id(), record.encoding_id()) {
        (PlatformId::Unicode, _) | (PlatformId::Windows, WINDOWS_UNICODE_BMP_ENCODING) => true,
        (PlatformId::Windows, WINDOWS_UNICODE_FULL_ENCODING) => matches!(
            subtable,
            CmapSubtable::Format12(_) | CmapSubtable::Format13(_)
        ),
        _ => false,
    }
}

struct VerticalMetrics {
    ascent: f32,
    descent: f32,
    line_gap: f32,
}

fn strike_font_metrics(data: &[u8], index: u32) -> Result<(f32, VerticalMetrics), ReadError> {
    let font = FontRef::from_index(data, index)?;
    let units_per_em = f32::from(font.head()?.units_per_em());
    Ok((units_per_em, vertical_metrics(&font)?))
}

fn vertical_metrics(font: &FontRef<'_>) -> Result<VerticalMetrics, ReadError> {
    let hhea = font.hhea()?;
    let hhea = VerticalMetrics {
        ascent: f32::from(hhea.ascender().to_i16()),
        descent: f32::from(hhea.descender().to_i16()),
        line_gap: f32::from(hhea.line_gap().to_i16()),
    };
    let Ok(os2) = font.os2() else {
        return Ok(hhea);
    };
    let typo = VerticalMetrics {
        ascent: f32::from(os2.s_typo_ascender()),
        descent: f32::from(os2.s_typo_descender()),
        line_gap: f32::from(os2.s_typo_line_gap()),
    };
    if os2.version() >= TYPO_METRICS_OS2_VERSION
        && os2
            .fs_selection()
            .contains(SelectionFlags::USE_TYPO_METRICS)
    {
        return Ok(typo);
    }
    let nonzero_or = |value: f32, fallback: f32| if value == 0.0 { fallback } else { value };
    Ok(VerticalMetrics {
        ascent: nonzero_or(
            hhea.ascent,
            nonzero_or(typo.ascent, f32::from(os2.us_win_ascent())),
        ),
        descent: nonzero_or(
            hhea.descent,
            nonzero_or(typo.descent, -f32::from(os2.us_win_descent())),
        ),
        line_gap: if hhea.ascent != 0.0 && hhea.descent != 0.0 {
            hhea.line_gap
        } else if typo.ascent != 0.0 || typo.descent != 0.0 {
            typo.line_gap
        } else {
            0.0
        },
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sfnt(mut tables: Vec<(&[u8; 4], Vec<u8>)>) -> Vec<u8> {
        tables.sort_by_key(|(tag, _)| **tag);
        let count = tables.len() as u16;
        let mut font = 0x0001_0000u32.to_be_bytes().to_vec();
        font.extend(count.to_be_bytes());
        font.extend([0u8; 6]);
        let mut offset = font.len() + tables.len() * 16;
        for (tag, table) in &tables {
            font.extend_from_slice(*tag);
            font.extend(0u32.to_be_bytes());
            font.extend((offset as u32).to_be_bytes());
            font.extend((table.len() as u32).to_be_bytes());
            offset += table.len();
        }
        for (_, table) in tables {
            font.extend(table);
        }
        font
    }

    fn words(values: &[i32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|&value| (value as u16).to_be_bytes())
            .collect()
    }

    fn head(units_per_em: i32) -> Vec<u8> {
        let mut table = words(&[1, 0, 0, 0, 0, 0, 0x5F0F, 0x3CF5, 0, units_per_em]);
        table.resize(54, 0);
        table
    }

    fn hhea([ascent, descent, line_gap]: [i32; 3]) -> Vec<u8> {
        let mut table = words(&[1, 0, ascent, descent, line_gap]);
        table.resize(36, 0);
        table
    }

    fn os2(fs_selection: u16, typo: [i32; 3], win: [i32; 2]) -> Vec<u8> {
        let mut table = words(&[4]);
        table.resize(62, 0);
        table.extend(fs_selection.to_be_bytes());
        table.extend(words(&[0, 0, typo[0], typo[1], typo[2], win[0], win[1]]));
        table.resize(96, 0);
        table
    }

    fn metrics(tables: Vec<(&[u8; 4], Vec<u8>)>) -> (f32, f32, f32) {
        let (units_per_em, metrics) =
            strike_font_metrics(&sfnt(tables), 0).expect("metrics tables parse");
        assert_eq!(units_per_em, 2048.0);
        (metrics.ascent, metrics.descent, metrics.line_gap)
    }

    #[test]
    fn colour_strike_metrics_fall_back_from_hhea_to_typo_to_win_like_freetype() {
        let font = |hhea_metrics: [i32; 3], selection: Option<u16>| {
            let mut tables = vec![(b"head", head(2048)), (b"hhea", hhea(hhea_metrics))];
            if let Some(selection) = selection {
                tables.push((b"OS/2", os2(selection, [700, -300, 50], [900, 250])));
            }
            metrics(tables)
        };
        assert_eq!(font([800, -200, 90], None), (800.0, -200.0, 90.0));
        assert_eq!(font([800, -200, 90], Some(0)), (800.0, -200.0, 90.0));
        assert_eq!(
            font(
                [800, -200, 90],
                Some(SelectionFlags::USE_TYPO_METRICS.bits())
            ),
            (700.0, -300.0, 50.0)
        );
        assert_eq!(font([0, 0, 90], Some(0)), (700.0, -300.0, 50.0));
        assert_eq!(
            metrics(vec![
                (b"head", head(2048)),
                (b"hhea", hhea([0, 0, 90])),
                (b"OS/2", os2(0, [0, 0, 50], [900, 250])),
            ]),
            (900.0, -250.0, 0.0)
        );
    }

    fn format4(mappings: &[(u16, u16)]) -> Vec<u8> {
        let mut segments: Vec<(u16, u16)> = mappings.to_vec();
        segments.push((0xFFFF, 0));
        let count = segments.len() as i32;
        let mut table = words(&[4, 16 + count * 8, 0, count * 2, 0, 0, 0]);
        table.extend(segments.iter().flat_map(|&(code, _)| code.to_be_bytes()));
        table.extend([0, 0]);
        table.extend(segments.iter().flat_map(|&(code, _)| code.to_be_bytes()));
        table.extend(
            segments
                .iter()
                .flat_map(|&(code, glyph)| glyph.wrapping_sub(code).to_be_bytes()),
        );
        table.extend(segments.iter().flat_map(|_| [0, 0]));
        table
    }

    fn format0(mappings: &[(u8, u8)]) -> Vec<u8> {
        let mut table = words(&[0, 262, 0]);
        let mut glyphs = [0u8; 256];
        for &(code, glyph) in mappings {
            glyphs[usize::from(code)] = glyph;
        }
        table.extend(glyphs);
        table
    }

    #[test]
    fn only_unicode_cmap_subtables_map_characters_and_notdef_maps_nothing() {
        let subtables = [
            (1, 0, format0(&[(b'B', 7)])),
            (3, 1, format4(&[(0x0000, 0), (u16::from(b'A'), 5)])),
            (3, 10, format4(&[(u16::from(b'C'), 9)])),
        ];
        let mut cmap = words(&[0, subtables.len() as i32]);
        let mut offset = 4 + subtables.len() * 8;
        for (platform, encoding, subtable) in &subtables {
            cmap.extend(words(&[*platform, *encoding]));
            cmap.extend((offset as u32).to_be_bytes());
            offset += subtable.len();
        }
        for (_, _, subtable) in subtables {
            cmap.extend(subtable);
        }
        let font = sfnt(vec![(b"cmap", cmap)]);
        assert_eq!(nominal_glyph(&font, 0, 'A'), Some(5));
        assert_eq!(nominal_glyph(&font, 0, '\0'), None);
        assert_eq!(nominal_glyph(&font, 0, 'B'), None);
        assert_eq!(nominal_glyph(&font, 0, 'C'), None);
        assert_eq!(nominal_glyph(&font, 1, 'A'), None);
    }
}
