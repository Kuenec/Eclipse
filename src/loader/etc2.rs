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

const OPAQUE: u8 = 255;

type Texel = [u8; 4];
type BlockTexels = [[Texel; 4]; 4];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Etc2Format {
    Rgb8,
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

fn shade(color: [i32; 3], delta: i32) -> Texel {
    [
        clamp(color[0] + delta),
        clamp(color[1] + delta),
        clamp(color[2] + delta),
        OPAQUE,
    ]
}

fn pixel_index(word: u64, x: usize, y: usize) -> usize {
    let i = x * 4 + y;
    (((word >> (15 + i)) & 2) | ((word >> i) & 1)) as usize
}

fn signed3(v: u32) -> i32 {
    if v >= 4 {
        v as i32 - 8
    } else {
        v as i32
    }
}

#[derive(Clone, Copy)]
enum Halves {
    LeftRight,
    TopBottom,
}

fn paint(word: u64, halves: Halves, palettes: [[Texel; 4]; 2]) -> BlockTexels {
    std::array::from_fn(|y| {
        std::array::from_fn(|x| {
            let half = match halves {
                Halves::LeftRight => x / 2,
                Halves::TopBottom => y / 2,
            };
            palettes[half][pixel_index(word, x, y)]
        })
    })
}

fn decode_rgb_block(block: &[u8; 8]) -> BlockTexels {
    let word = u64::from_be_bytes(*block);

    let differential = bits(word, 33, 1) == 1;
    let (first, second) = if differential {
        let base = [bits(word, 63, 5), bits(word, 55, 5), bits(word, 47, 5)];
        let delta = [
            signed3(bits(word, 58, 3)),
            signed3(bits(word, 50, 3)),
            signed3(bits(word, 42, 3)),
        ];
        let [r, g, b] = [0, 1, 2].map(|c| base[c] as i32 + delta[c]);
        if !(0..=31).contains(&r) {
            return decode_t_block(word);
        }
        if !(0..=31).contains(&g) {
            return decode_h_block(word);
        }
        if !(0..=31).contains(&b) {
            return decode_planar_block(word);
        }
        (base.map(extend5), [r, g, b].map(|c| extend5(c as u32)))
    } else {
        (
            [bits(word, 63, 4), bits(word, 55, 4), bits(word, 47, 4)].map(extend4),
            [bits(word, 59, 4), bits(word, 51, 4), bits(word, 43, 4)].map(extend4),
        )
    };
    let palette = |color: [i32; 3], table: u32| {
        let [small, large] = MODIFIERS[table as usize];
        [small, large, -small, -large].map(|delta| shade(color, delta))
    };
    let halves = if bits(word, 32, 1) == 1 {
        Halves::TopBottom
    } else {
        Halves::LeftRight
    };
    paint(
        word,
        halves,
        [
            palette(first, bits(word, 39, 3)),
            palette(second, bits(word, 36, 3)),
        ],
    )
}

fn decode_t_block(word: u64) -> BlockTexels {
    let first = [
        (bits(word, 60, 2) << 2) | bits(word, 57, 2),
        bits(word, 55, 4),
        bits(word, 51, 4),
    ]
    .map(extend4);
    let second = [bits(word, 47, 4), bits(word, 43, 4), bits(word, 39, 4)].map(extend4);
    let d = DISTANCES[((bits(word, 35, 2) << 1) | bits(word, 32, 1)) as usize];
    let palette = [
        shade(first, 0),
        shade(second, d),
        shade(second, 0),
        shade(second, -d),
    ];
    paint(word, Halves::LeftRight, [palette; 2])
}

fn decode_h_block(word: u64) -> BlockTexels {
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
    let palette = [
        shade(first, d),
        shade(first, -d),
        shade(second, d),
        shade(second, -d),
    ];
    paint(word, Halves::LeftRight, [palette; 2])
}

fn decode_planar_block(word: u64) -> BlockTexels {
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
    std::array::from_fn(|y| {
        std::array::from_fn(|x| {
            let (x, y) = (x as i32, y as i32);
            let channel = |c: usize| {
                clamp(
                    (x * (horizontal[c] - origin[c])
                        + y * (vertical[c] - origin[c])
                        + 4 * origin[c]
                        + 2)
                        >> 2,
                )
            };
            [channel(0), channel(1), channel(2), OPAQUE]
        })
    })
}

fn decode_alpha_block(block: &[u8; 8]) -> [[u8; 4]; 4] {
    let word = u64::from_be_bytes(*block);
    let base = bits(word, 63, 8) as i32;
    let multiplier = bits(word, 55, 4) as i32;
    let alphas = EAC_MODIFIERS[bits(word, 51, 4) as usize].map(|m| clamp(base + m * multiplier));
    std::array::from_fn(|y| {
        std::array::from_fn(|x| alphas[((word >> (45 - 3 * (x * 4 + y))) & 7) as usize])
    })
}

fn decode_block(format: Etc2Format, block: &[u8]) -> BlockTexels {
    match format {
        Etc2Format::Rgb8 => decode_rgb_block(block.try_into().expect("8-byte color block")),
        Etc2Format::Rgba8 => {
            let (alpha, color) = block.split_at(8);
            let alpha = decode_alpha_block(alpha.try_into().expect("8-byte alpha half"));
            let mut texels = decode_rgb_block(color.try_into().expect("8-byte color half"));
            for (texels, alphas) in texels.iter_mut().zip(alpha) {
                for (texel, alpha) in texels.iter_mut().zip(alphas) {
                    texel[3] = alpha;
                }
            }
            texels
        }
    }
}

pub(crate) fn decode(
    format: Etc2Format,
    src: &[u8],
    blocks_per_row: usize,
    width: u32,
    height: u32,
    dst: &mut [u8],
) {
    let (width, height) = (width as usize, height as usize);
    if width == 0 || blocks_per_row == 0 {
        return;
    }
    let block_bytes = format.block_bytes();
    let row_bytes = width * 4;
    let block_rows = src
        .chunks(blocks_per_row * block_bytes)
        .take(height.div_ceil(4));

    for (block_row, band) in block_rows.zip(dst.chunks_mut(4 * row_bytes)) {
        let blocks = block_row.chunks_exact(block_bytes).take(width.div_ceil(4));
        for (bx, block) in blocks.enumerate() {
            let texels = decode_block(format, block);
            let columns = bx * 16..(bx * 16 + 16).min(row_bytes);
            for (row, texels) in band.chunks_exact_mut(row_bytes).zip(&texels) {
                row[columns.clone()].copy_from_slice(&texels.as_flattened()[..columns.len()]);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn fnv1a(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xCBF2_9CE4_8422_2325, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3)
        })
    }

    fn rgb_block(word: u64) -> [[[u8; 3]; 4]; 4] {
        decode_rgb_block(&word.to_be_bytes()).map(|row| row.map(|[r, g, b, _]| [r, g, b]))
    }

    #[test]
    fn individual_mode_paints_each_half_with_its_base_color_and_modifier() {
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
        let word = (0b1111_1011u64 << 56) | (1 << 33);
        assert_eq!(rgb_block(word)[0][0], [255, 0, 0]);
    }

    #[test]
    fn green_overflow_selects_h_mode() {
        let word = (0b1111_1001u64 << 48) | (1 << 33);
        let g = bits(word, 55, 5) as i32 + signed3(bits(word, 50, 3));
        assert!(!(0..=31).contains(&g));
        let px = rgb_block(word);
        assert_eq!(px[0][0], px[3][3]);
    }

    #[test]
    fn blue_overflow_selects_planar_mode() {
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
    fn noise_in_every_block_mode_decodes_to_the_reference_texels() {
        for (format, texels) in [
            (Etc2Format::Rgb8, 0x3C16_E939_170F_F0CC),
            (Etc2Format::Rgba8, 0x6C6B_5F3D_B2DC_67F0),
        ] {
            let src = noise(32 * 32 * format.block_bytes());
            let mut dst = vec![0u8; 128 * 128 * 4];
            decode(format, &src, 32, 128, 128, &mut dst);
            assert_eq!(fnv1a(&dst), texels, "{format:?}");
        }
    }

    #[test]
    fn an_image_without_texels_or_blocks_decodes_nothing() {
        let block = (0xF0u64 << 56).to_be_bytes();
        let mut dst = [7u8; 16];
        decode(Etc2Format::Rgb8, &block, 1, 0, 4, &mut dst);
        decode(Etc2Format::Rgb8, &block, 0, 4, 4, &mut dst);
        assert_eq!(dst, [7u8; 16]);
    }

    #[test]
    fn vulkan_format_numbers_map_to_block_layouts() {
        assert_eq!(Etc2Format::from_vk(147), Some(Etc2Format::Rgb8));
        assert_eq!(Etc2Format::from_vk(152), Some(Etc2Format::Rgba8));
        assert_eq!(Etc2Format::from_vk(131), None);
        assert_eq!(Etc2Format::Rgba8.block_bytes(), 16);
    }
}
