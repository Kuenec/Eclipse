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

type MemoryFace = Face<Arc<[u8]>>;

pub(crate) struct RasterFont {
    data: Arc<[u8]>,
    ppem_per_height: f32,
    ascent_per_height: f32,
    line_gap_per_height: f32,
}

impl RasterFont {
    pub(crate) fn try_from_vec(data: Vec<u8>) -> Result<Self, String> {
        let data: Arc<[u8]> = data.into();
        let face = load_face(data.clone())?;
        if !face.is_scalable() {
            return Err("font has no scalable outline".to_string());
        }
        let ascent = f32::from(face.ascender());
        let descent = f32::from(face.descender());
        let height = ascent - descent;
        let em_size = f32::from(face.em_size());
        if em_size <= 0.0 || !height.is_finite() || height <= 0.0 {
            return Err("font has invalid vertical metrics".to_string());
        }
        Ok(Self {
            data,
            ppem_per_height: em_size / height,
            ascent_per_height: ascent / height,
            line_gap_per_height: (f32::from(face.height()) - height) / height,
        })
    }

    pub(crate) fn scaled(&self, height: f32) -> Option<ScaledFont> {
        let height = if height.is_finite() && height > 0.0 {
            height.min(MAX_GLYPH_DIMENSION as f32)
        } else {
            1.0
        };
        let face = load_face(self.data.clone()).ok()?;
        let char_height = (height * self.ppem_per_height * 64.0)
            .round()
            .clamp(64.0, (MAX_GLYPH_DIMENSION * 64) as f32) as isize;
        face.set_char_size(0, char_height, 72, 72).ok()?;
        Some(ScaledFont {
            face,
            height,
            ascent: height * self.ascent_per_height,
            line_gap: height * self.line_gap_per_height,
        })
    }
}

fn load_face(data: Arc<[u8]>) -> Result<MemoryFace, String> {
    let library = Library::init().map_err(|error| format!("{error:?}"))?;
    library
        .new_memory_face2(data, 0)
        .map_err(|error| format!("{error:?}"))
}

pub(crate) struct ScaledFont {
    face: MemoryFace,
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
