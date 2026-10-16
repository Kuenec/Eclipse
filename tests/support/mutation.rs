const INTERESTING_BYTES: [u8; 6] = [0x00, 0x01, 0x7f, 0x80, 0xfe, 0xff];
const INTERESTING_WORDS: [u32; 7] = [0, 1, 0x7fff, 0x8000, 0xffff, 0x7fff_ffff, u32::MAX];
const MAX_EDITS: usize = 4;

pub(crate) struct Mutations {
    state: u64,
}

impl Mutations {
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = self.state;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^ (mixed >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).expect("bounds fit u64"))
            .expect("a value below a usize bound fits usize")
    }

    pub(crate) fn of(&mut self, original: &[u8]) -> Vec<u8> {
        let mut bytes = original.to_vec();
        for _ in 0..=self.below(MAX_EDITS) {
            let at = self.below(bytes.len() + 1);
            match self.below(6) {
                0 if at < bytes.len() => bytes[at] ^= 1 << self.below(8),
                1 if at < bytes.len() => {
                    bytes[at] = INTERESTING_BYTES[self.below(INTERESTING_BYTES.len())];
                }
                2 => bytes.truncate(at),
                3 => bytes.insert(at, self.next().to_le_bytes()[0]),
                4 if at < bytes.len() => {
                    bytes.remove(at);
                }
                _ => {
                    let word = INTERESTING_WORDS[self.below(INTERESTING_WORDS.len())];
                    for (slot, byte) in bytes.iter_mut().skip(at).zip(word.to_le_bytes()) {
                        *slot = byte;
                    }
                }
            }
        }
        bytes
    }
}
