//! Software decoding of ETC2 texture blocks for hosts whose Vulkan driver cannot sample them.

const MODIFIERS: [[i32; 2]; 8] = [
    [2, 8],
    [5, 17],
    [9, 29],
    [13, 42],
    [18, 60],
    [24, 80],
    [33, 106],
    [47, 183],
];

const DISTANCES: [i32; 8] = [3, 6, 11, 16, 23, 32, 41, 64];

const EAC_MODIFIERS: [[i32; 8]; 16] = [
    [-3, -6, -9, -15, 2, 5, 8, 14],
    [-3, -7, -10, -13, 2, 6, 9, 12],
    [-2, -5, -8, -13, 1, 4, 7, 12],
    [-2, -4, -6, -13, 1, 3, 5, 12],
    [-3, -6, -8, -12, 2, 5, 7, 11],
    [-3, -7, -9, -11, 2, 6, 8, 10],
    [-4, -7, -8, -11, 3, 6, 7, 10],
    [-3, -5, -8, -11, 2, 4, 7, 10],
    [-2, -6, -8, -10, 1, 5, 7, 9],
    [-2, -5, -8, -10, 1, 4, 7, 9],
    [-2, -4, -8, -10, 1, 3, 7, 9],
    [-2, -5, -7, -10, 1, 4, 6, 9],
    [-3, -4, -7, -10, 2, 3, 6, 9],
    [-1, -2, -3, -10, 0, 1, 2, 9],
    [-4, -6, -8, -9, 3, 5, 7, 8],
    [-3, -5, -7, -9, 2, 4, 6, 8],
];

/// The ETC2 block formats Roblox uploads, keyed by their Vulkan format numbers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Etc2Format {
    /// `VK_FORMAT_ETC2_R8G8B8_UNORM_BLOCK` / `_SRGB_BLOCK` (ETC1 data decodes the same way).
    Rgb8,
    /// `VK_FORMAT_ETC2_R8G8B8A8_UNORM_BLOCK` / `_SRGB_BLOCK`.
    Rgba8,
}

impl Etc2Format {
    pub(crate) const fn from_vk(format: i32) -> Option<Self> {
        match format {
            147 | 148 => Some(Self::Rgb8),
            151 | 152 => Some(Self::Rgba8),
            _ => None,
        }
    }

    pub(crate) const fn block_bytes(self) -> usize {
        match self {
            Self::Rgb8 => 8,
            Self::Rgba8 => 16,
        }
    }
}

fn bits(word: u64, high: u32, count: u32) -> u32 {
    ((word >> (high + 1 - count)) & ((1 << count) - 1)) as u32
}

fn extend4(v: u32) -> i32 {
    (v * 17) as i32
}

fn extend5(v: u32) -> i32 {
    ((v << 3) | (v >> 2)) as i32
}

fn extend6(v: u32) -> i32 {
    ((v << 2) | (v >> 4)) as i32
}

fn extend7(v: u32) -> i32 {
    ((v << 1) | (v >> 6)) as i32
}

fn clamp(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

fn shade(color: [i32; 3], delta: i32) -> [u8; 3] {
    color.map(|c| clamp(c + delta))
}

fn pixel_index(word: u64, x: usize, y: usize) -> usize {
    let i = x * 4 + y;
    let msb = (word >> (16 + i)) & 1;
    let lsb = (word >> i) & 1;
    (msb * 2 + lsb) as usize
}

fn signed3(v: u32) -> i32 {
    if v >= 4 {
        v as i32 - 8
    } else {
        v as i32
    }
}

/// Decodes one 8-byte ETC2 RGB block into 4x4 RGB pixels, indexed `[y][x]`.
fn decode_rgb_block(block: &[u8; 8]) -> [[[u8; 3]; 4]; 4] {
    let word = u64::from_be_bytes(*block);

    let differential = bits(word, 33, 1) == 1;
    if differential {
        let r = bits(word, 63, 5) as i32 + signed3(bits(word, 58, 3));
        let g = bits(word, 55, 5) as i32 + signed3(bits(word, 50, 3));
        let b = bits(word, 47, 5) as i32 + signed3(bits(word, 42, 3));
        if !(0..=31).contains(&r) {
            return decode_t_block(word);
        }
        if !(0..=31).contains(&g) {
            return decode_h_block(word);
        }
        if !(0..=31).contains(&b) {
            return decode_planar_block(word);
        }
    }

    let (first, second) = if differential {
        let base = [bits(word, 63, 5), bits(word, 55, 5), bits(word, 47, 5)];
        let delta = [
            signed3(bits(word, 58, 3)),
            signed3(bits(word, 50, 3)),
            signed3(bits(word, 42, 3)),
        ];
        (
            base.map(extend5),
            [0, 1, 2].map(|c| extend5((base[c] as i32 + delta[c]) as u32)),
        )
    } else {
        (
            [bits(word, 63, 4), bits(word, 55, 4), bits(word, 47, 4)].map(extend4),
            [bits(word, 59, 4), bits(word, 51, 4), bits(word, 43, 4)].map(extend4),
        )
    };
    let tables = [bits(word, 39, 3) as usize, bits(word, 36, 3) as usize];
    let flip = bits(word, 32, 1) == 1;

    let mut out = [[[0u8; 3]; 4]; 4];
    for (y, row) in out.iter_mut().enumerate() {
        for (x, pixel) in row.iter_mut().enumerate() {
            let sub = usize::from(if flip { y >= 2 } else { x >= 2 });
            let [small, large] = MODIFIERS[tables[sub]];
            let delta = [small, large, -small, -large][pixel_index(word, x, y)];
            *pixel = shade(if sub == 0 { first } else { second }, delta);
        }
    }
    out
}

fn paint(word: u64, colors: [[u8; 3]; 4]) -> [[[u8; 3]; 4]; 4] {
    let mut out = [[[0u8; 3]; 4]; 4];
    for (y, row) in out.iter_mut().enumerate() {
        for (x, pixel) in row.iter_mut().enumerate() {
            *pixel = colors[pixel_index(word, x, y)];
        }
    }
    out
}

fn decode_t_block(word: u64) -> [[[u8; 3]; 4]; 4] {
    let first = [
        (bits(word, 60, 2) << 2) | bits(word, 57, 2),
        bits(word, 55, 4),
        bits(word, 51, 4),
    ]
    .map(extend4);
    let second = [bits(word, 47, 4), bits(word, 43, 4), bits(word, 39, 4)].map(extend4);
    let d = DISTANCES[((bits(word, 35, 2) << 1) | bits(word, 32, 1)) as usize];
    paint(
        word,
        [
            shade(first, 0),
            shade(second, d),
            shade(second, 0),
            shade(second, -d),
        ],
    )
}

fn decode_h_block(word: u64) -> [[[u8; 3]; 4]; 4] {
    let first4 = [
        bits(word, 62, 4),
        (bits(word, 58, 3) << 1) | bits(word, 52, 1),
        (bits(word, 51, 1) << 3) | bits(word, 49, 3),
    ];
    let second4 = [bits(word, 46, 4), bits(word, 42, 4), bits(word, 38, 4)];
    let packed = |c: [u32; 3]| (c[0] << 8) | (c[1] << 4) | c[2];
    let order = u32::from(packed(first4) >= packed(second4));
    let d = DISTANCES[((bits(word, 34, 1) << 2) | (bits(word, 32, 1) << 1) | order) as usize];
    let (first, second) = (first4.map(extend4), second4.map(extend4));
    paint(
        word,
        [
            shade(first, d),
            shade(first, -d),
            shade(second, d),
            shade(second, -d),
        ],
    )
}

fn decode_planar_block(word: u64) -> [[[u8; 3]; 4]; 4] {
    let origin = [
        extend6(bits(word, 62, 6)),
        extend7((bits(word, 56, 1) << 6) | bits(word, 54, 6)),
        extend6((bits(word, 48, 1) << 5) | (bits(word, 44, 2) << 3) | bits(word, 41, 3)),
    ];
    let horizontal = [
        extend6((bits(word, 38, 5) << 1) | bits(word, 32, 1)),
        extend7(bits(word, 31, 7)),
        extend6(bits(word, 24, 6)),
    ];
    let vertical = [
        extend6(bits(word, 18, 6)),
        extend7(bits(word, 12, 7)),
        extend6(bits(word, 5, 6)),
    ];
    let mut out = [[[0u8; 3]; 4]; 4];
    for (y, row) in out.iter_mut().enumerate() {
        for (x, pixel) in row.iter_mut().enumerate() {
            let (x, y) = (x as i32, y as i32);
            *pixel = [0, 1, 2].map(|c| {
                clamp(
                    (x * (horizontal[c] - origin[c])
                        + y * (vertical[c] - origin[c])
                        + 4 * origin[c]
                        + 2)
                        >> 2,
                )
            });
        }
    }
    out
}

/// Decodes one 8-byte EAC alpha block into 4x4 alpha values, indexed `[y][x]`.
fn decode_alpha_block(block: &[u8; 8]) -> [[u8; 4]; 4] {
    let word = u64::from_be_bytes(*block);
    let base = bits(word, 63, 8) as i32;
    let multiplier = bits(word, 55, 4) as i32;
    let table = EAC_MODIFIERS[bits(word, 51, 4) as usize];
    let mut out = [[0u8; 4]; 4];
    for (y, row) in out.iter_mut().enumerate() {
        for (x, alpha) in row.iter_mut().enumerate() {
            let i = (x * 4 + y) as u32;
            let index = bits(word, 47 - 3 * i, 3) as usize;
            *alpha = clamp(base + table[index] * multiplier);
        }
    }
    out
}

/// Decodes `width` x `height` texels of ETC2 blocks, laid out `blocks_per_row` blocks to a
/// row, into RGBA8 rows of `width * 4` bytes. Blocks missing from `src` leave their texels
/// untouched.
pub(crate) fn decode(
    format: Etc2Format,
    src: &[u8],
    blocks_per_row: usize,
    width: u32,
    height: u32,
    dst: &mut [u8],
) {
    let (width, height) = (width as usize, height as usize);
    let blocks_x = width.div_ceil(4);
    let block_bytes = format.block_bytes();
    let row_bytes = width * 4;

    for by in 0..height.div_ceil(4) {
        for bx in 0..blocks_x {
            let at = (by * blocks_per_row + bx) * block_bytes;
            let Some(block) = src.get(at..at + block_bytes) else {
                return;
            };
            let (alpha, color) = match format {
                Etc2Format::Rgb8 => (None, block),
                Etc2Format::Rgba8 => (
                    Some(decode_alpha_block(
                        block[..8].try_into().expect("8-byte alpha half"),
                    )),
                    &block[8..],
                ),
            };
            let color = decode_rgb_block(color.try_into().expect("8-byte color half"));

            for (dy, row) in color.iter().enumerate() {
                let y = by * 4 + dy;
                if y >= height {
                    break;
                }
                for (dx, rgb) in row.iter().enumerate() {
                    let x = bx * 4 + dx;
                    if x >= width {
                        break;
                    }
                    let out = y * row_bytes + x * 4;
                    let a = alpha.map_or(255, |alpha| alpha[dy][dx]);
                    dst[out..out + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], a]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_block(word: u64) -> [[[u8; 3]; 4]; 4] {
        decode_rgb_block(&word.to_be_bytes())
    }

    #[test]
    fn individual_mode_paints_each_half_with_its_base_color_and_modifier() {
        // R1=0xF R2=0x0, G1=G2=0x8, B1=0x0 B2=0xF, tables 0/0, no flip, every index 0.
        let word = (0xF0u64 << 56) | (0x88 << 48) | (0x0F << 40);
        let px = rgb_block(word);
        assert_eq!(px[0][0], [255, 138, 2]);
        assert_eq!(px[3][1], [255, 138, 2]);
        assert_eq!(px[0][2], [2, 138, 255]);
        assert_eq!(px[3][3], [2, 138, 255]);
    }

    #[test]
    fn flip_bit_splits_the_block_into_top_and_bottom_halves() {
        let word = (0xF0u64 << 56) | (0x88 << 48) | (0x0F << 40) | (1 << 32);
        let px = rgb_block(word);
        assert_eq!(px[1][3], [255, 138, 2]);
        assert_eq!(px[2][0], [2, 138, 255]);
    }

    #[test]
    fn pixel_indices_select_the_four_modifiers_column_by_column() {
        // Grey 0x8, table 7 (47/183). Pixel (0,1) has its lsb set, (0,2) its msb, (0,3) both.
        let word = (0x88u64 << 56)
            | (0x88 << 48)
            | (0x88 << 40)
            | (0b111_111 << 34)
            | (1 << (16 + 2))
            | (1 << (16 + 3))
            | (1 << 1)
            | (1 << 3);
        let px = rgb_block(word);
        assert_eq!(px[0][0], [183; 3]);
        assert_eq!(px[1][0], [255; 3]);
        assert_eq!(px[2][0], [89; 3]);
        assert_eq!(px[3][0], [0; 3]);
    }

    #[test]
    fn differential_mode_adds_the_signed_delta_to_the_second_half() {
        // R1=16 dR=-1, G1=16 dG=+3, B1=0 dB=0.
        let r = (16u64 << 3) | 0b111;
        let g = (16u64 << 3) | 0b011;
        let word = (r << 56) | (g << 48) | (1 << 33);
        let px = rgb_block(word);
        let base = extend5(16) as u8;
        assert_eq!(px[0][0], [base + 2, base + 2, 2]);
        assert_eq!(px[0][3], [extend5(15) as u8 + 2, extend5(19) as u8 + 2, 2]);
    }

    #[test]
    fn red_overflow_selects_t_mode() {
        // R1=31 dR=+3 overflows; T-mode color 1 reads R from bits 60..59 and 57..56 (0xF).
        let word = (0b1111_1011u64 << 56) | (1 << 33);
        assert_eq!(rgb_block(word)[0][0], [255, 0, 0]);
    }

    #[test]
    fn green_overflow_selects_h_mode() {
        // R1=0, G1=31 dG=+1 overflows. Every pixel index 0 paints color 1 plus the distance.
        let word = (0b1111_1001u64 << 48) | (1 << 33);
        let g = bits(word, 55, 5) as i32 + signed3(bits(word, 50, 3));
        assert!(!(0..=31).contains(&g));
        let px = rgb_block(word);
        assert_eq!(px[0][0], px[3][3]);
    }

    #[test]
    fn blue_overflow_selects_planar_mode() {
        // B1=31 dB=+1 overflows. Blue origin = 0b0_11_010 = 26, gradients 0.
        let word = (1u64 << 33) | (0b1111_1001 << 40);
        let px = rgb_block(word);
        let blue = extend6(26);
        assert_eq!(px[0][0], [0, 0, blue as u8]);
        assert_eq!(px[0][1], [0, 0, clamp((-blue + 4 * blue + 2) >> 2)]);
    }

    #[test]
    fn eac_alpha_with_zero_multiplier_is_the_base_value() {
        let word: u64 = 0x80 << 56;
        assert_eq!(decode_alpha_block(&word.to_be_bytes()), [[0x80; 4]; 4]);
    }

    #[test]
    fn eac_alpha_applies_table_times_multiplier_per_pixel() {
        // Base 100, multiplier 2, table 0; pixel (0,0) index 7 (+14), the rest index 0 (-3).
        let word: u64 = (100 << 56) | (2 << 52) | (0b111 << 45);
        let alpha = decode_alpha_block(&word.to_be_bytes());
        assert_eq!(alpha[0][0], 128);
        assert_eq!(alpha[1][0], 94);
    }

    #[test]
    fn decode_writes_rgba_rows_and_clips_partial_blocks() {
        let block = (0xF0u64 << 56).to_be_bytes();
        let src: Vec<u8> = block.iter().copied().cycle().take(8 * 4).collect();
        let mut dst = vec![0u8; 6 * 5 * 4];
        decode(Etc2Format::Rgb8, &src, 2, 6, 5, &mut dst);
        assert_eq!(&dst[..4], &[255, 2, 2, 255]);
        let right_half = (4 * 6 + 2) * 4;
        assert_eq!(&dst[right_half..right_half + 4], &[2, 2, 2, 255]);
        let last = (4 * 6 + 5) * 4;
        assert_eq!(&dst[last..last + 4], &[255, 2, 2, 255]);
    }

    #[test]
    fn decode_skips_padding_blocks_at_the_end_of_each_row() {
        let red = (0xF0u64 << 56).to_be_bytes();
        let blue = (0x0Fu64 << 40).to_be_bytes();
        // Two block rows of three blocks each; only the first block of each row is in range.
        let src: Vec<u8> = [red, blue, blue, blue, blue, blue].concat();
        let mut dst = vec![0u8; 4 * 8 * 4];
        decode(Etc2Format::Rgb8, &src, 3, 4, 8, &mut dst);
        assert_eq!(&dst[..4], &[255, 2, 2, 255]);
        let second_row = 4 * 4 * 4;
        assert_eq!(&dst[second_row..second_row + 4], &[2, 2, 2, 255]);
    }

    #[test]
    fn decode_fills_alpha_from_the_eac_half() {
        let mut block = [0u8; 16];
        block[..8].copy_from_slice(&(0x40u64 << 56).to_be_bytes());
        let mut dst = vec![0u8; 4 * 4 * 4];
        decode(Etc2Format::Rgba8, &block, 1, 4, 4, &mut dst);
        assert!(dst.chunks(4).all(|px| px[3] == 0x40));
    }

    #[test]
    fn vulkan_format_numbers_map_to_block_layouts() {
        assert_eq!(Etc2Format::from_vk(147), Some(Etc2Format::Rgb8));
        assert_eq!(Etc2Format::from_vk(152), Some(Etc2Format::Rgba8));
        assert_eq!(Etc2Format::from_vk(131), None);
        assert_eq!(Etc2Format::Rgba8.block_bytes(), 16);
    }
}
